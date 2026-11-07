import hashlib
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest
from unittest import mock

SCRIPT = Path(__file__).with_name("collect-licenses.py")
SPEC = importlib.util.spec_from_file_location("collect_licenses", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class LicenseTests(unittest.TestCase):
    def test_full_runtime_notice_is_preserved_beside_cargo_and_java_notices(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            package = root / "package"
            package.mkdir()
            cargo_text = "Complete Cargo dependency notice\nCopyright teighto\n"
            (package / "LICENSE").write_bytes(cargo_text.encode())
            gson_text = "Complete Java dependency notice\nAll conditions retained.\n"
            musl_text = "Full musl runtime copyright\nCopyright © runtime authors\nFinal attribution and license condition.\n"
            cache = root / "cache"
            cache.mkdir()
            for text in (gson_text, musl_text):
                (cache / (hashlib.sha256(text.encode()).hexdigest() + ".txt")).write_bytes(text.encode())
            metadata = {"packages": [{"name": "fixture", "version": "1.0.0", "source": "registry", "license": "MIT", "manifest_path": str(package / "Cargo.toml")}]}
            gson = ("teighto/fixture", "1" * 40, "LICENSE", hashlib.sha256(gson_text.encode()).hexdigest())
            musl = ("https://git.musl-libc.org/cgit/musl/plain/COPYRIGHT?id=" + "2" * 40, hashlib.sha256(musl_text.encode()).hexdigest())
            with mock.patch.object(MODULE, "GSON", gson), mock.patch.object(MODULE, "MUSL_RUNTIME", musl, create=True):
                text, count = MODULE.generate(metadata, cache)
            self.assertIn(cargo_text, text)
            self.assertIn(gson_text, text)
            self.assertIn(musl_text, text)
            self.assertEqual(text.count(musl_text), 1)
            self.assertEqual(count, 3)

    def test_runtime_notice_download_rejects_wrong_digest_and_preserves_cached_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            cache = Path(temporary)
            expected = hashlib.sha256(b"expected license").hexdigest()
            cached = cache / (expected + ".txt")
            cached.write_bytes(b"invalid cached copy")
            response = io.BytesIO(b"different downloaded license")
            response.geturl = lambda: "https://git.musl-libc.org/cgit/musl/plain/COPYRIGHT?id=" + "2" * 40
            with mock.patch.object(MODULE.urllib.request, "urlopen", return_value=response):
                with self.assertRaises(ValueError):
                    MODULE.fetched_url(response.geturl(), expected, cache)
            self.assertEqual(cached.read_bytes(), b"invalid cached copy")

    def test_runtime_notice_refuses_insecure_redirect_and_oversize_download(self):
        for destination, payload in [("http://untrusted.invalid/license", b"license"), ("https://git.musl-libc.org/license", b"x" * (4 * 1024 * 1024 + 1))]:
            with self.subTest(destination=destination), tempfile.TemporaryDirectory() as temporary:
                response = io.BytesIO(payload)
                response.geturl = lambda: destination
                with mock.patch.object(MODULE.urllib.request, "urlopen", return_value=response):
                    with self.assertRaises(ValueError):
                        MODULE.fetched_url("https://git.musl-libc.org/license", "0" * 64, Path(temporary))
                self.assertEqual(list(Path(temporary).iterdir()), [])


if __name__ == "__main__":
    unittest.main()
