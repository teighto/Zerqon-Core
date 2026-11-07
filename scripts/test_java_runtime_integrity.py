import hashlib
import importlib.util
import io
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest import mock
import zipfile

SPEC = importlib.util.spec_from_file_location("runtime_integrity", Path(__file__).with_name("java-runtime-integrity.py"))
runtime = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = runtime
SPEC.loader.exec_module(runtime)


class RuntimeIntegrityTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)

    def asset(self, path, platform):
        data = path.read_bytes()
        return runtime.Asset(platform, path.name, len(data), hashlib.sha256(data).hexdigest())

    def zip(self, entries):
        path = self.root / "fixture.zip"
        with zipfile.ZipFile(path, "w") as archive:
            for name, data, mode in entries:
                info = zipfile.ZipInfo(runtime.ARCHIVE_ROOT + "/" + name)
                info.filename = runtime.ARCHIVE_ROOT + "/" + name
                info.external_attr = mode << 16
                archive.writestr(info, data)
        return path

    def tar(self, entries):
        path = self.root / "fixture.tar.gz"
        with tarfile.open(path, "w:gz", encoding="utf-8") as archive:
            for name, data, mode, target in entries:
                info = tarfile.TarInfo(runtime.ARCHIVE_ROOT + "/" + name)
                info.mode = mode
                if target is not None:
                    info.type = tarfile.SYMTYPE
                    info.linkname = target
                    archive.addfile(info)
                else:
                    info.size = len(data)
                    archive.addfile(info, io.BytesIO(data))
        return path

    @staticmethod
    def file(path, data, executable=False):
        return {"path": path, "size": len(data), "sha256": hashlib.sha256(data).hexdigest(), "executable": executable}

    def test_verified_zip_records_every_regular_file_in_sorted_order(self):
        path = self.zip([("lib/modules", b"modules", 0o100644), ("bin/java.exe", b"binary", 0o100755)])
        asset = self.asset(path, "windows-x64")
        self.assertEqual(runtime.generate_inventory(path, asset), {
            "schema": 1, "release": runtime.RELEASE, "platform": asset.platform,
            "archive_root": runtime.ARCHIVE_ROOT, "archive_name": asset.name,
            "archive_size": asset.size, "archive_sha256": asset.sha256,
            "files": [self.file("bin/java.exe", b"binary"), self.file("lib/modules", b"modules")],
        })

    def test_archive_size_and_hash_are_checked_before_zip_parsing(self):
        path = self.zip([("bin/java.exe", b"binary", 0o100644)])
        asset = self.asset(path, "windows-x64")
        for size, digest in [(asset.size + 1, asset.sha256), (asset.size, "0" * 64)]:
            with self.subTest(size=size), mock.patch.object(runtime.zipfile, "ZipFile", side_effect=AssertionError("parsed unverified bytes")):
                with self.assertRaises(ValueError):
                    runtime.generate_inventory(path, runtime.Asset(asset.platform, asset.name, size, digest))

    def test_parser_uses_the_verified_snapshot_if_source_changes_after_hashing(self):
        path = self.zip([("bin/java.exe", b"original", 0o100644)])
        asset = self.asset(path, "windows-x64")
        original_parser = runtime.read_zip
        def mutate_then_parse(source, inventory):
            path.write_bytes(b"changed after verification")
            original_parser(source, inventory)
        with mock.patch.object(runtime, "read_zip", side_effect=mutate_then_parse):
            result = runtime.generate_inventory(path, asset)
        self.assertEqual(result["files"], [self.file("bin/java.exe", b"original")])

    def test_tar_legal_link_chains_are_regular_copies_with_normalized_permissions(self):
        path = self.tar([
            ("legal/two/LICENSE", b"", 0o777, "../one/LICENSE"),
            ("bin/java", b"binary", 0o751, None),
            ("legal/one/LICENSE", b"", 0o777, "../base/LICENSE"),
            ("legal/base/LICENSE", b"license", 0o755, None),
        ])
        result = runtime.generate_inventory(path, self.asset(path, "linux-x64"))
        self.assertEqual(result.get("files"), [
            self.file("bin/java", b"binary", True), self.file("legal/base/LICENSE", b"license", True),
            self.file("legal/one/LICENSE", b"license"), self.file("legal/two/LICENSE", b"license"),
        ])

    def test_zip_rejects_unsafe_names_collisions_and_links(self):
        cases = [
            [("../escape", b"x", 0o100644)], [("bin/CON.txt", b"x", 0o100644)],
            [("bin/back\\slash", b"x", 0o100644)], [("bin/colon:name", b"x", 0o100644)],
            [("bin/escape.jar ", b"x", 0o100644)], [("bin/a\x01", b"x", 0o100644)],
            [("bin/a\x00hidden", b"x", 0o100644)],
            [("bin/java.exe", b"x", 0o120777)],
            [("bin/Java.exe", b"x", 0o100644), ("bin/java.exe", b"y", 0o100644)],
            [("bin", b"x", 0o100644), ("bin/java.exe", b"y", 0o100644)],
            [("bin/java.exe", b"y", 0o100644), ("bin", b"x", 0o100644)],
        ]
        for entries in cases:
            with self.subTest(entries=entries):
                path = self.zip(entries)
                with self.assertRaises(ValueError): runtime.generate_inventory(path, self.asset(path, "windows-x64"))

    def test_tar_rejects_escaping_missing_cyclic_and_nonlegal_links(self):
        for entries in [
            [("bin/java", b"", 0o777, "../legal/LICENSE")],
            [("legal/one/LICENSE", b"", 0o777, "../../outside")],
            [("legal/one/LICENSE", b"", 0o777, "/etc/passwd")],
            [("legal/one/LICENSE", b"", 0o777, "missing")],
            [("legal/one/LICENSE", b"", 0o777, "LICENSE")],
        ]:
            with self.subTest(entries=entries):
                path = self.tar(entries)
                with self.assertRaises(ValueError): runtime.generate_inventory(path, self.asset(path, "linux-x64"))

    def test_limits_apply_to_regular_files_and_materialized_link_copies(self):
        path = self.tar([("legal/base/LICENSE", b"1234", 0o644, None), ("legal/one/LICENSE", b"", 0o777, "../base/LICENSE")])
        for limit, value in [("MAX_FILE", 3), ("MAX_UNPACKED", 7), ("MAX_ENTRIES", 1)]:
            with self.subTest(limit=limit), mock.patch.object(runtime, limit, value, create=True):
                with self.assertRaises(ValueError): runtime.generate_inventory(path, self.asset(path, "linux-x64"))

    def test_materialized_link_bytes_count_toward_installed_total(self):
        inventory = runtime.Inventory("linux-x64")
        inventory.file("legal/base/LICENSE", io.BytesIO(b"1234"), 4, 0o644)
        inventory.links["legal/copy/LICENSE"] = "legal/base/LICENSE"
        with mock.patch.object(runtime, "MAX_UNPACKED", 7):
            with self.assertRaises(ValueError): inventory.resolve_links()

    def test_non_utf8_zip_names_are_not_reinterpreted_as_platform_encoding(self):
        path = self.zip([("bin/B", b"data", 0o100644)])
        original = (runtime.ARCHIVE_ROOT + "/bin/B").encode()
        path.write_bytes(path.read_bytes().replace(original, original[:-1] + b"\x82"))
        with self.assertRaises(ValueError): runtime.generate_inventory(path, self.asset(path, "windows-x64"))

    def test_tar_hardlinks_and_special_files_are_rejected(self):
        for kind in (tarfile.LNKTYPE, tarfile.FIFOTYPE, tarfile.CHRTYPE, tarfile.CONTTYPE):
            path = self.root / "special.tar.gz"
            with tarfile.open(path, "w:gz") as archive:
                info = tarfile.TarInfo(runtime.ARCHIVE_ROOT + "/legal/special")
                info.type = kind
                info.linkname = runtime.ARCHIVE_ROOT + "/legal/missing"
                archive.addfile(info)
            with self.subTest(kind=kind), self.assertRaises(ValueError):
                runtime.generate_inventory(path, self.asset(path, "linux-x64"))

    def test_check_mode_rejects_changed_output_without_overwriting_it(self):
        path = self.root / "manifest.json"
        content = runtime.encoded_manifest({"files": []})
        runtime.publish(path, content, False)
        runtime.publish(path, content, True)
        path.write_bytes(b"preserve changed manifest")
        with self.assertRaises(ValueError): runtime.publish(path, content, True)
        self.assertEqual(path.read_bytes(), b"preserve changed manifest")
        with mock.patch.object(runtime, "MAX_MANIFEST", 1):
            with self.assertRaises(ValueError): runtime.encoded_manifest({"files": []})


if __name__ == "__main__":
    unittest.main()
