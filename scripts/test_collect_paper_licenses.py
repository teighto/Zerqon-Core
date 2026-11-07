import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import zipfile

SPEC = importlib.util.spec_from_file_location("paper_licenses", Path(__file__).with_name("collect-paper-licenses.py"))
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class PaperLicenseTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.cache = self.root / "cache"
        self.cache.mkdir()

    def fixture(self, entries, source=None):
        path = self.root / "library.jar"
        with zipfile.ZipFile(path, "w") as jar:
            for name, content in entries.items():
                jar.writestr(name, content)
        dependency = {"artifacts": [{"file": "library.jar", "url": "https://repo.maven.apache.org/library.jar", "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}]}
        licenses = {"sources": {}, "artifacts": {"library.jar": {"license": "See upstream texts", "sources": []}}}
        if source:
            digest = hashlib.sha256(source).hexdigest()
            (self.cache / (digest + ".txt")).write_bytes(source)
            licenses["sources"]["library"] = {"source": "https://github.com/example/library", "documents": [{"url": "https://raw.githubusercontent.com/example/library/" + "1" * 40 + "/LICENSE", "sha256": digest}]}
            licenses["artifacts"]["library.jar"]["sources"] = ["library"]
        return dependency, licenses

    def test_preserves_real_bundled_license_and_notice_not_class_files(self):
        dependency, licenses = self.fixture({"META-INF/LICENSE.txt": "Copyright Upstream\nPermission to redistribute", "META-INF/NOTICE": "Keep this attribution", "example/NoticeType.class": b"\xca\xfe\xba\xbe"})
        result = MODULE.collect(dependency, licenses, self.root, self.cache)
        self.assertIn("Copyright Upstream\nPermission to redistribute", result)
        self.assertIn("Keep this attribution", result)
        self.assertNotIn("NoticeType.class", result)

    def test_class_file_does_not_satisfy_missing_license(self):
        dependency, licenses = self.fixture({"META-INF/LICENSE.class": b"\xca\xfe\xba\xbe", "example/NoticeType.class": b"binary"})
        with self.assertRaises(ValueError):
            MODULE.collect(dependency, licenses, self.root, self.cache)

    def test_pinned_fallback_is_used_and_artifact_is_verified(self):
        dependency, licenses = self.fixture({"Main.class": b"bytecode"}, b"MIT License\nCopyright Trusted Upstream")
        self.assertIn("Copyright Trusted Upstream", MODULE.collect(dependency, licenses, self.root, self.cache))
        (self.root / "library.jar").write_bytes(b"tampered")
        with self.assertRaises(ValueError):
            MODULE.collect(dependency, licenses, self.root, self.cache)

    def test_unmapped_dependency_is_an_error_even_when_jar_has_license(self):
        dependency, licenses = self.fixture({"LICENSE": "Valid license"})
        licenses["artifacts"] = {}
        with self.assertRaises(ValueError):
            MODULE.collect(dependency, licenses, self.root, self.cache)

    def test_unsafe_jar_entry_names_are_not_read(self):
        dependency, licenses = self.fixture({"../LICENSE": "Outside"})
        with self.assertRaises(ValueError):
            MODULE.collect(dependency, licenses, self.root, self.cache)


if __name__ == "__main__":
    unittest.main()
