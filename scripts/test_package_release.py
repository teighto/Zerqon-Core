import hashlib
import importlib.util
import json
from pathlib import Path
import struct
import tempfile
import unittest
import zipfile

SCRIPT = Path(__file__).with_name("package-release.py")
SPEC = importlib.util.spec_from_file_location("package_release", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def elf_fixture(interpreter=False, dynamic_tag=None):
    segments = 1 + int(interpreter) + int(dynamic_tag is not None)
    offset = 64 + segments * 56
    contents = b""
    extra = []
    if interpreter:
        payload = b"/lib64/ld-linux-x86-64.so.2\0"
        extra.append(struct.pack("<IIQQQQQQ", 3, 4, offset, 0x1000 + offset, 0, len(payload), len(payload), 1))
        contents += payload
        offset += len(payload)
    if dynamic_tag is not None:
        payload = struct.pack("<qQqQ", dynamic_tag, 1, 0, 0)
        extra.append(struct.pack("<IIQQQQQQ", 2, 6, offset, 0x1000 + offset, 0, len(payload), len(payload), 8))
        contents += payload
        offset += len(payload)
    ident = b"\x7fELF\x02\x01\x01" + bytes(9)
    header = struct.pack("<16sHHIQQQIHHHHHH", ident, 3, 62, 1, 0x1000, 64, 0, 0, 64, 56, segments, 0, 0, 0)
    load = struct.pack("<IIQQQQQQ", 1, 5, 0, 0x1000, 0, offset, offset, 4096)
    return header + load + b"".join(extra) + contents


class PackageTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.notices = self.root / "target/notices.txt"
        files = [
            "target/release/zerqon.exe", "target/release/zerqon",
            "target/x86_64-unknown-linux-musl/release/zerqon",
            "libraries/zerqon-java-api-0.0.1.jar", "libraries/zerqon-java-host-0.0.1.jar",
            "libraries/gson-2.13.2.jar", "dist/hello-java-plugin-0.0.1.jar",
            "target/wasm32-unknown-unknown/release/zerqon_hello_plugin.wasm",
            "README.md", "LICENSE", "docs/CONFIGURATION.md",
            "docs/RUST_PLUGINS.md", "docs/JAVA_PLUGINS.md", "docs/JAVA_COMPATIBILITY.md", "docs/ROADMAP.md", "docs/images/architecture.png", "docs/images/architecture.svg",
            "docs/images/performance.png", "docs/images/performance.svg", "docs/images/plugins.png", "docs/images/plugins.svg",
            "docs/benchmarks/ordinary-chunks.json",
            "docs/PROFILING.md", "docs/WORLD_IMPORT.md", "docs/WORLD_STORAGE.md", "crates/zerqon-profiler/NOTICE.md", "tools/paper-bridge/licenses.lock.json",
            "libraries/paper/zerqon-paper-bridge-0.0.1.jar", "libraries/paper/paper-api-test.jar", "libraries/paper/adventure-test.jar",
            "scripts/prepare-data.py", "target/notices.txt", "target/release-notices/PAPER-LICENSES.txt",
        ]
        for name in files:
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("fixture:" + name, encoding="utf-8")
        (self.root / "target/x86_64-unknown-linux-musl/release/zerqon").write_bytes(elf_fixture())
        (self.root / "Cargo.toml").write_text('[workspace.package]\nversion="0.0.1"\n', encoding="utf-8")
        self.paper_files = ["paper-api-test.jar", "adventure-test.jar"]
        lock = {"artifacts": [{"file": name, "url": "https://repo.papermc.io/" + name, "sha256": hashlib.sha256((self.root / "libraries/paper" / name).read_bytes()).hexdigest()} for name in self.paper_files]}
        (self.root / "tools/paper-bridge/dependencies.lock.json").write_text(json.dumps(lock), encoding="utf-8")

    def test_archive_contains_only_allowlisted_files_and_verified_hashes(self):
        for name in (".env", ".cache/vanilla/server.jar", "world/player.dat", "plugins/secret.jar", "dist/extra.zip", "libraries/other.jar", "libraries/paper/unlocked.jar", "dist/zerqon.jar"):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("private")
        archive_path = MODULE.package(self.root, self.root / "output", "windows-x86_64", self.notices)
        with zipfile.ZipFile(archive_path) as archive:
            names = set(archive.namelist())
            self.assertIn("zerqon.exe", names)
            self.assertIn("libraries/gson-2.13.2.jar", names)
            self.assertIn("examples/rust-plugins/hello.wasm", names)
            self.assertIn("THIRD-PARTY-NOTICES.txt", names)
            self.assertNotIn("zerqon.jar", names)
            self.assertIn("libraries/paper/zerqon-paper-bridge-0.0.1.jar", names)
            self.assertIn("libraries/paper/paper-api-test.jar", names)
            self.assertIn("libraries/paper/adventure-test.jar", names)
            self.assertIn("tools/paper-bridge/dependencies.lock.json", names)
            self.assertIn("tools/paper-bridge/licenses.lock.json", names)
            self.assertIn("PAPER-LICENSES.txt", names)
            self.assertIn("docs/PROFILING.md", names)
            self.assertIn("docs/benchmarks/ordinary-chunks.json", names)
            self.assertNotIn("libraries/paper/unlocked.jar", names)
            self.assertFalse(any(name.startswith((".cache/", "world/", "plugins/")) or name.endswith("extra.zip") for name in names))
            manifest = json.loads(archive.read("BUILD-INFO.json"))
            self.assertEqual(manifest["version"], "0.0.1")
            self.assertEqual(manifest["release_channel"], "development")
            for line in archive.read("SHA256SUMS").decode().splitlines():
                digest, name = line.split("  ", 1)
                self.assertEqual(hashlib.sha256(archive.read(name)).hexdigest(), digest)
        self.assertTrue(archive_path.with_suffix(".zip.sha256").is_file())

    def test_linux_native_file_is_executable_in_archive(self):
        archive_path = MODULE.package(self.root, self.root / "output", "linux-x86_64", self.notices)
        with zipfile.ZipFile(archive_path) as archive:
            self.assertTrue((archive.getinfo("zerqon").external_attr >> 16) & 0o111)
            self.assertNotIn("zerqon.exe", archive.namelist())

    def test_linux_uses_only_the_musl_artifact_when_a_stale_host_binary_exists(self):
        (self.root / "target/release/zerqon").write_bytes(b"stale GNU binary")
        archive_path = MODULE.package(self.root, self.root / "output", "linux-x86_64", self.notices)
        with zipfile.ZipFile(archive_path) as archive:
            self.assertEqual(archive.read("zerqon"), elf_fixture())
            self.assertEqual(json.loads(archive.read("BUILD-INFO.json"))["native_target"], "x86_64-unknown-linux-musl")

    def test_missing_musl_artifact_does_not_fall_back_to_host_binary(self):
        (self.root / "target/x86_64-unknown-linux-musl/release/zerqon").unlink()
        with self.assertRaises(FileNotFoundError):
            MODULE.package(self.root, self.root / "output", "linux-x86_64", self.notices)
        self.assertFalse(list((self.root / "output").glob("*.zip")))

    def test_linux_package_rejects_interpreter_dependencies_and_version_requirements(self):
        path = self.root / "target/x86_64-unknown-linux-musl/release/zerqon"
        for fixture in [elf_fixture(interpreter=True), elf_fixture(dynamic_tag=1), elf_fixture(dynamic_tag=0x6ffffffe), elf_fixture(dynamic_tag=0x6fffffff)]:
            with self.subTest(fixture=fixture.hex()):
                path.write_bytes(fixture)
                with self.assertRaises(ValueError):
                    MODULE.package(self.root, self.root / "output", "linux-x86_64", self.notices)
                self.assertFalse(list((self.root / "output").glob("*.zip")))

    def test_linux_package_rejects_invalid_architecture_and_truncated_headers(self):
        valid = elf_fixture()
        wrong_arch = bytearray(valid)
        struct.pack_into("<H", wrong_arch, 18, 183)
        outside_table = bytearray(valid)
        struct.pack_into("<Q", outside_table, 32, 2**63)
        missing_entry = bytearray(valid)
        struct.pack_into("<Q", missing_entry, 24, 0)
        path = self.root / "target/x86_64-unknown-linux-musl/release/zerqon"
        for fixture in [b"not ELF", valid[:63], valid[:-1], wrong_arch, outside_table, missing_entry]:
            with self.subTest(fixture=bytes(fixture).hex()):
                path.write_bytes(fixture)
                with self.assertRaises(ValueError):
                    MODULE.package(self.root, self.root / "output", "linux-x86_64", self.notices)
                self.assertFalse(list((self.root / "output").glob("*.zip")))

    def test_missing_required_binary_does_not_publish_partial_archive(self):
        (self.root / "target/release/zerqon.exe").unlink()
        with self.assertRaises((FileNotFoundError, ValueError)):
            MODULE.package(self.root, self.root / "output", "windows-x86_64", self.notices)
        self.assertFalse(list((self.root / "output").glob("*.zip")))

    def test_symlinked_required_file_is_not_packaged(self):
        target = self.root / "target/release/zerqon.exe"
        target.unlink()
        try:
            target.symlink_to(self.root / "LICENSE")
        except OSError:
            self.skipTest("symlink creation not permitted")
        with self.assertRaises(ValueError):
            MODULE.package(self.root, self.root / "output", "windows-x86_64", self.notices)

    def test_tampered_paper_dependency_is_rejected_before_publication(self):
        (self.root / "libraries/paper/paper-api-test.jar").write_bytes(b"tampered")
        with self.assertRaises(ValueError):
            MODULE.package(self.root, self.root / "output", "windows-x86_64", self.notices)
        self.assertFalse(list((self.root / "output").glob("*.zip")))


if __name__ == "__main__":
    unittest.main()
