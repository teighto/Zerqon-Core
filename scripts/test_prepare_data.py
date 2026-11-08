import importlib.util
import contextlib
import concurrent.futures
import hashlib
import io
import json
import os
import shutil
import subprocess
import tempfile
import threading
import unittest
import warnings
import zipfile
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("prepare_data", Path(__file__).with_name("prepare-data.py"))
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)


class DataPreparationTests(unittest.TestCase):
    def legacy_cache(self, root):
        reports = root / "generated/reports"
        reports.mkdir(parents=True)
        values = {
            "packets.json": {"handshake": {"serverbound": {"minecraft:intention": {"protocol_id": 0}}}, "login": {"clientbound": {"minecraft:login_finished": {"protocol_id": 2}}}, "play": {"clientbound": {"minecraft:login": {"protocol_id": 50}}}},
            "blocks.json": {"minecraft:air": {"states": [{"id": 0, "default": True}]}},
            "registries.json": {"minecraft:item": {"entries": {"minecraft:stone": {"protocol_id": 1}}}, "minecraft:entity_type": {"entries": {"minecraft:player": {"protocol_id": 1}}}, "minecraft:data_component_type": {"entries": {"minecraft:max_stack_size": {"protocol_id": 0}}}},
            "minecraft/components/item/stone.json": {"components": {"minecraft:max_stack_size": 64}},
        }
        for name, value in values.items():
            path = reports / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps(value), encoding="utf-8")
        marker = json.dumps({"version": "26.3", "protocol": 777, "server_sha1": prepare.SERVER_SHA1, "world_version": 5023, "java_major": 25}).encode()
        (root / "prepared.json").write_bytes(marker)
        return marker

    def test_malformed_cached_item_defaults_are_not_reused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            marker = json.loads(self.legacy_cache(root))
            self.assertIsNotNone(prepare.reusable_report_paths(root, marker))
            (root / "generated/reports/minecraft/components/item/stone.json").write_text('{"components":{"minecraft:max_stack_size":0}}', encoding="utf-8")
            self.assertIsNone(prepare.reusable_report_paths(root, marker))

    def test_damageable_stackable_defaults_are_not_reused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            marker = json.loads(self.legacy_cache(root))
            defaults = root / "generated/reports/minecraft/components/item/stone.json"
            defaults.write_text('{"components":{"minecraft:max_stack_size":64,"minecraft:max_damage":1}}', encoding="utf-8")
            self.assertIsNone(prepare.reusable_report_paths(root, marker))

    def test_invalid_registry_ids_and_supported_defaults_are_not_reused(self):
        cases = [
            ("minecraft:item", "minecraft:stone", {"protocol_id": True}),
            ("minecraft:item", "minecraft:stone", {"protocol_id": 1.0}),
            ("minecraft:item", "minecraft:stone", {"protocol_id": 65536}),
            ("minecraft:item", "minecraft:extra", {"protocol_id": 1}),
            ("minecraft:data_component_type", "minecraft:extra", {"protocol_id": 0}),
            ("minecraft:data_component_type", "minecraft:bad//path", {"protocol_id": 2}),
            ("minecraft:data_component_type", "minecraft:max_stack_size", {"protocol_id": -1}),
        ]
        for registry, key, entry in cases:
            with self.subTest(registry=registry, key=key, entry=entry), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                marker = json.loads(self.legacy_cache(root))
                path = root / "generated/reports/registries.json"
                value = json.loads(path.read_bytes())
                value[registry]["entries"][key] = entry
                path.write_text(json.dumps(value), encoding="utf-8")
                if registry == "minecraft:item" and key == "minecraft:extra":
                    (path.parent / "minecraft/components/item/extra.json").write_text('{"components":{}}', encoding="utf-8")
                self.assertIsNone(prepare.reusable_report_paths(root, marker))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            marker = json.loads(self.legacy_cache(root))
            path = root / "generated/reports/registries.json"
            value = json.loads(path.read_bytes())
            value["minecraft:data_component_type"]["entries"] = {}
            path.write_text(json.dumps(value), encoding="utf-8")
            self.assertIsNone(prepare.reusable_report_paths(root, marker))

    def test_duplicate_json_members_are_not_silently_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "report.json"
            path.write_bytes(b'{"protocol_id":1,"protocol_id":2}')
            with self.assertRaises(ValueError):
                prepare.read_json(path, 1024)

    def test_report_encodings_rejected_by_native_are_not_reused(self):
        for encoding in ("utf-16", "utf-8-sig"):
            with self.subTest(encoding=encoding), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                marker = json.loads(self.legacy_cache(root))
                path = root / "generated/reports/minecraft/components/item/stone.json"
                path.write_text('{"components":{"minecraft:max_stack_size":64}}', encoding=encoding)
                self.assertIsNone(prepare.reusable_report_paths(root, marker))

    def test_invalid_json_scalars_and_depth_are_not_reused_in_unknown_components(self):
        for raw in (b'"\\ud800"', b'1e999', b'9' * 400, b'[' * 128 + b'0' + b']' * 128):
            with self.subTest(raw=raw[:16]), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                marker = json.loads(self.legacy_cache(root))
                path = root / "generated/reports/minecraft/components/item/stone.json"
                path.write_bytes(b'{"components":{"minecraft:max_stack_size":64,"unknown:test":' + raw + b'}}')
                self.assertIsNone(prepare.reusable_report_paths(root, marker))

    def test_atomic_copy_missing_source_does_not_leak_temp_or_change_destination(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            destination = root / "destination"
            destination.write_bytes(b"original")
            with self.assertRaises(FileNotFoundError):
                prepare.atomic_copy(root / "missing", destination)
            self.assertEqual(sorted(path.name for path in root.iterdir()), ["destination"])
            self.assertEqual(destination.read_bytes(), b"original")

    def test_atomic_copy_keeps_live_destination_intact_until_verified_publication(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.write_bytes(b"replacement")
            destination = root / "destination"
            destination.write_bytes(b"original")

            def interrupted_copy(original, target):
                target.write(original.read(3))
                target.flush()
                self.assertEqual(destination.read_bytes(), b"original")
                raise OSError("copy interrupted")

            with patch.object(prepare.shutil, "copyfileobj", side_effect=interrupted_copy):
                with self.assertRaisesRegex(OSError, "copy interrupted"):
                    prepare.atomic_copy(source, destination)
            self.assertEqual(destination.read_bytes(), b"original")
            with self.assertRaisesRegex(ValueError, "checksum"):
                prepare.atomic_copy(source, destination, expected_sha1=hashlib.sha1(b"different").hexdigest())
            self.assertEqual(destination.read_bytes(), b"original")
            self.assertEqual(sorted(path.name for path in root.iterdir()), ["destination", "source"])
            prepare.atomic_copy(source, destination, expected_sha1=hashlib.sha1(b"replacement").hexdigest())
            self.assertEqual(destination.read_bytes(), b"replacement")

    def test_failed_preparation_releases_the_cache_lock(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch.object(prepare, "prepare_locked", side_effect=OSError("preparation failed")):
                with self.assertRaisesRegex(OSError, "preparation failed"):
                    prepare.prepare(root, "unused-java", None)
            with patch.object(prepare, "PREPARE_LOCK_TIMEOUT", 0.03), prepare.prepare_lock(root):
                self.assertEqual(sorted(path.name for path in root.iterdir()), ["prepare.lock"])

    def test_invalid_cached_jar_is_preserved_with_or_without_supplied_archive(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            supplied = root / "supplied.jar"
            supplied.write_bytes(b"verified supplied")
            cached = root / "server.jar"
            cached.write_bytes(b"preserve invalid cached")
            with patch.object(prepare, "SERVER_SHA1", hashlib.sha1(supplied.read_bytes()).hexdigest()), patch.object(prepare.urllib.request, "urlopen", side_effect=AssertionError("must not download")):
                for source in (None, supplied):
                    with self.subTest(source=source), self.assertRaisesRegex(ValueError, "preserve/remove"):
                        prepare.prepare(root, "unused-java", source)
                    self.assertEqual(cached.read_bytes(), b"preserve invalid cached")

    def test_prepare_waits_for_cache_lock_before_touching_existing_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = self.make_jar(root, [("data/minecraft/recipe/test.json", b'{"type":"minecraft:crafting_shapeless"}')])
            source.rename(root / "supplied.jar")
            original = b"existing invalid archive must be preserved"
            (root / "server.jar").write_bytes(original)
            with patch.object(prepare, "SERVER_SHA1", hashlib.sha1((root / "supplied.jar").read_bytes()).hexdigest()), patch.object(prepare, "PREPARE_LOCK_TIMEOUT", 0.03, create=True):
                marker = self.legacy_cache(root)
                with (root / "prepare.lock").open("a+b") as lock:
                    if os.name == "nt":
                        import msvcrt
                        msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
                    else:
                        import fcntl
                        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    try:
                        with self.assertRaises(TimeoutError):
                            prepare.prepare(root, "nonexistent-java-runtime", root / "supplied.jar")
                        self.assertEqual((root / "server.jar").read_bytes(), original)
                        self.assertEqual((root / "prepared.json").read_bytes(), marker)
                    finally:
                        if os.name == "nt":
                            lock.seek(0)
                            msvcrt.locking(lock.fileno(), msvcrt.LK_UNLCK, 1)
                        else:
                            fcntl.flock(lock, fcntl.LOCK_UN)

    def test_failed_download_preserves_unrelated_download_file(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "server.download").write_bytes(b"other process data")
            with patch.object(prepare.urllib.request, "urlopen", return_value=io.BytesIO(b"wrong checksum")):
                with self.assertRaises(ValueError):
                    prepare.fetch_server(root / "server.jar")
            self.assertEqual((root / "server.download").read_bytes(), b"other process data")
            self.assertEqual(sorted(path.name for path in root.iterdir()), ["server.download"])

    def test_concurrent_downloads_use_independent_verified_temporary_files(self):
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "server.jar"
            raw = b"verified fixture archive"
            verified = prepare.verified_file
            barrier = threading.Barrier(2)

            def verify_then_wait(path, expected):
                result = verified(path, expected)
                if path != destination and result:
                    barrier.wait(timeout=3)
                return result

            with patch.object(prepare, "SERVER_SHA1", hashlib.sha1(raw).hexdigest()), patch.object(prepare, "verified_file", side_effect=verify_then_wait), patch.object(prepare.urllib.request, "urlopen", side_effect=lambda *args, **kwargs: io.BytesIO(raw)):
                with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                    futures = [pool.submit(prepare.fetch_server, destination) for _ in range(2)]
                    for future in futures:
                        future.result(timeout=5)
            self.assertEqual(destination.read_bytes(), raw)
            self.assertEqual(list(destination.parent.iterdir()), [destination])

    @unittest.skipUnless(os.name == "nt", "Windows denies replacement while another reader holds the file")
    def test_download_reuses_only_a_verified_concurrent_publisher_with_an_open_file(self):
        raw = b"verified fixture archive"
        verified = prepare.verified_file
        for published in [raw, b"unverified concurrent archive"]:
            with self.subTest(published=published), tempfile.TemporaryDirectory() as directory, contextlib.ExitStack() as readers:
                destination = Path(directory) / "server.jar"

                def publish_before_replace(path, expected):
                    result = verified(path, expected)
                    if path != destination and result:
                        destination.write_bytes(published)
                        readers.enter_context(destination.open("rb"))
                    return result

                with patch.object(prepare, "SERVER_SHA1", hashlib.sha1(raw).hexdigest()), patch.object(prepare, "verified_file", side_effect=publish_before_replace), patch.object(prepare.urllib.request, "urlopen", return_value=io.BytesIO(raw)):
                    if published == raw:
                        prepare.fetch_server(destination)
                    else:
                        with self.assertRaises((PermissionError, ValueError)):
                            prepare.fetch_server(destination)
                self.assertEqual(destination.read_bytes(), published)
                self.assertEqual(list(destination.parent.iterdir()), [destination])

    def test_download_publication_failure_without_a_verified_winner_is_reported(self):
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "server.jar"
            raw = b"verified fixture archive"
            with patch.object(prepare, "SERVER_SHA1", hashlib.sha1(raw).hexdigest()), patch.object(prepare.urllib.request, "urlopen", return_value=io.BytesIO(raw)), patch.object(prepare.os, "replace", side_effect=PermissionError("publication denied")):
                with self.assertRaisesRegex(PermissionError, "publication denied"):
                    prepare.fetch_server(destination)
            self.assertFalse(destination.exists())
            self.assertEqual(list(destination.parent.iterdir()), [])

    @unittest.skipUnless(shutil.which("rustc"), "Rust compiler needed for native lock interoperability")
    def test_python_preparation_and_rust_file_lock_exclude_each_other(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "lock_probe.rs"
            source.write_text('''use std::{fs::{File, TryLockError}, io::{self, Write}};
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let file = File::options().read(true).write(true).create(true).truncate(false).open(&args[2]).unwrap();
    match file.try_lock() {
        Ok(()) => {},
        Err(TryLockError::WouldBlock) => std::process::exit(3),
        Err(error) => panic!("{error:?}"),
    }
    if args[1] == "hold" {
        println!("locked"); io::stdout().flush().unwrap();
        let mut release = String::new(); io::stdin().read_line(&mut release).unwrap();
    }
    file.unlock().unwrap();
}
''', encoding="utf-8")
            executable = root / ("lock_probe.exe" if os.name == "nt" else "lock_probe")
            options = {"creationflags": 0x08000000} if os.name == "nt" else {}
            subprocess.run([shutil.which("rustc"), "--edition=2024", str(source), "-o", str(executable)], capture_output=True, check=True, timeout=30, **options)
            cache = root / "cache"
            cache.mkdir()
            jar = cache / "server.jar"
            jar.write_bytes(b"untouched")
            lock_path = cache / "prepare.lock"
            owner = subprocess.Popen([str(executable), "hold", str(lock_path)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, **options)
            try:
                self.assertEqual(owner.stdout.readline(), "locked\n")
                with patch.object(prepare, "PREPARE_LOCK_TIMEOUT", 0.03), self.assertRaises(TimeoutError):
                    prepare.prepare(cache, "unused-java", root / "not-yet-read.jar")
                self.assertEqual(jar.read_bytes(), b"untouched")
            finally:
                owner.communicate("release\n", timeout=5)
            self.assertEqual(owner.returncode, 0)

            def native_contender(*unused):
                result = subprocess.run([str(executable), "try", str(lock_path)], capture_output=True, timeout=5, **options)
                self.assertEqual(result.returncode, 3, result.stderr)
                raise OSError("staged preparation failure")

            with patch.object(prepare, "prepare_locked", side_effect=native_contender):
                with self.assertRaisesRegex(OSError, "staged preparation failure"):
                    prepare.prepare(cache, "unused-java", None)
            result = subprocess.run([str(executable), "try", str(lock_path)], capture_output=True, timeout=5, **options)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(jar.read_bytes(), b"untouched")

    def make_jar(self, root, entries):
        nested = io.BytesIO()
        with warnings.catch_warnings(), zipfile.ZipFile(nested, "w") as archive:
            warnings.simplefilter("ignore", UserWarning)
            for name, content in entries:
                archive.writestr(name, content)
        jar = root / "server.jar"
        with zipfile.ZipFile(jar, "w") as archive:
            archive.writestr("version.json", json.dumps({"id": "26.3", "protocol_version": 777, "world_version": 5023, "java_version": 25}))
            archive.writestr("META-INF/versions/26.3/server-26.3.jar", nested.getvalue())
        return jar

    def test_recipes_are_extracted_as_raw_assets_without_registry_sync(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            raw = b'{"type":"minecraft:crafting_shapeless","ingredients":["minecraft:oak_log"],"result":{"id":"minecraft:oak_planks","count":4}}'
            jar = self.make_jar(root, [("data/minecraft/recipe/oak_planks.json", raw)])
            prepare.extract_data(jar, root / "data")
            self.assertEqual((root / "data/minecraft/recipe/oak_planks.json").read_bytes(), raw)
            self.assertNotIn("recipe", prepare.REGISTRIES)

    def test_invalid_recipe_archive_members_are_rejected(self):
        good = b'{"type":"minecraft:crafting_shapeless"}'
        cases = [
            [("data/minecraft/recipe/test.json", b"[1,2]")],
            [("data/minecraft/recipe/test.json", b"{\"type\":12}")],
            [("data/minecraft/recipe/test.json", b"{" )],
            [("data/minecraft/recipe/test.json", good)] * 2,
            [("data/minecraft/recipe/test.json", b" " * (1024 * 1024 + 1))],
            [("data/minecraft/recipe/../test.json", good)],
            [("data/minecraft/recipe/con.json", good)],
            [("data/minecraft/recipe/UPPER.json", good)],
        ]
        for entries in cases:
            with self.subTest(entries=[name for name, _ in entries]), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                jar = self.make_jar(root, entries)
                with self.assertRaises(ValueError):
                    prepare.extract_data(jar, root / "data")

    def test_legacy_cache_recipes_are_repaired_without_java(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            raw = b'{"type":"minecraft:crafting_shapeless"}'
            jar = self.make_jar(root, [("data/minecraft/recipe/test.json", raw)])
            with patch.object(prepare, "SERVER_SHA1", hashlib.sha1(jar.read_bytes()).hexdigest()), patch.object(prepare.subprocess, "run", side_effect=AssertionError("Java must not run")) as java:
                self.legacy_cache(root)
                with contextlib.redirect_stdout(io.StringIO()):
                    prepare.prepare(root, "nonexistent-java-runtime", None)
                java.assert_not_called()
                self.assertEqual((root / "data/minecraft/recipe/test.json").read_bytes(), raw)
                marker = json.loads((root / "prepared.json").read_bytes())
                self.assertEqual(marker["recipes"], 1)
                self.assertEqual(marker["recipes_sha256"], prepare.recipe_identity(root / "data")["recipes_sha256"])

    def test_failed_recipe_repair_keeps_old_cache_unchanged(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            raw = b'{"type":"minecraft:crafting_shapeless"}'
            jar = self.make_jar(root, [("data/minecraft/recipe/test.json", raw), ("data/minecraft/recipe/broken.json", b"{")])
            with patch.object(prepare, "SERVER_SHA1", hashlib.sha1(jar.read_bytes()).hexdigest()), patch.object(prepare.subprocess, "run", side_effect=AssertionError("Java must not run")):
                marker = self.legacy_cache(root)
                (root / "data").mkdir()
                (root / "data/old").write_bytes(b"unchanged")
                with self.assertRaises(ValueError):
                    prepare.prepare(root, "nonexistent-java-runtime", None)
                self.assertEqual((root / "prepared.json").read_bytes(), marker)
                self.assertEqual(list((root / "data").iterdir()), [root / "data/old"])
                self.assertEqual((root / "data/old").read_bytes(), b"unchanged")

    def test_unexpected_recipe_is_preserved_before_installation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            jar = self.make_jar(root, [("data/minecraft/recipe/test.json", b'{"type":"minecraft:crafting_shapeless"}')])
            with patch.object(prepare, "SERVER_SHA1", hashlib.sha1(jar.read_bytes()).hexdigest()), patch.object(prepare.subprocess, "run", side_effect=AssertionError("Java must not run")):
                marker = self.legacy_cache(root)
                extra = root / "data/minecraft/recipe/unexpected.json"
                extra.parent.mkdir(parents=True)
                extra.write_bytes(b"user content")
                with self.assertRaisesRegex(ValueError, "Unexpected cached recipe"):
                    prepare.prepare(root, "nonexistent-java-runtime", None)
                self.assertEqual((root / "prepared.json").read_bytes(), marker)
                self.assertEqual(extra.read_bytes(), b"user content")
                self.assertFalse((extra.parent / "test.json").exists())

    def test_nested_tags_expand_in_order_without_duplicate_ids(self):
        definitions = {"minecraft:base": ["minecraft:stone"], "minecraft:all": ["#minecraft:base", "minecraft:dirt", "minecraft:stone", {"id": "minecraft:missing", "required": False}]}
        self.assertEqual(prepare.resolve_tags(definitions, {"minecraft:stone": 1, "minecraft:dirt": 2}), {"minecraft:base": [1], "minecraft:all": [1, 2]})

    def test_required_missing_tag_members_are_rejected(self):
        with self.assertRaises(ValueError):
            prepare.resolve_tags({"minecraft:all": ["minecraft:missing"]}, {})

    def test_tag_cycles_are_rejected(self):
        with self.assertRaises(ValueError):
            prepare.resolve_tags({"minecraft:a": ["#minecraft:b"], "minecraft:b": ["#minecraft:a"]}, {})

    def test_cached_download_requires_matching_content(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "server.jar"
            path.write_bytes(b"abc")
            self.assertTrue(prepare.verified_file(path, "a9993e364706816aba3e25717850c26c9cd0d89d"))
            path.write_bytes(b"corrupt")
            self.assertFalse(prepare.verified_file(path, "a9993e364706816aba3e25717850c26c9cd0d89d"))

    def test_registry_entries_preserve_file_identifier_order_and_json(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for namespace, name in [("minecraft", "x"), ("minecraft", "x-y"), ("other", "x")]:
                path = root / namespace / "dimension_type" / f"{name}.json"
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(json.dumps({"height": 384, "has_skylight": True}), encoding="utf-8")
            entries = prepare.registry_entries(root, "minecraft:dimension_type")
            self.assertEqual([entry["id"] for entry in entries], ["minecraft:x-y", "minecraft:x", "other:x"])
            self.assertEqual(entries[0]["data"], {"height": 384, "has_skylight": True})


if __name__ == "__main__":
    unittest.main()
