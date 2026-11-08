import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
import urllib.error
from unittest import mock
import zipfile

SPEC = importlib.util.spec_from_file_location("paper_licenses", Path(__file__).with_name("collect-paper-licenses.py"))
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class PaperLicenseTests(unittest.TestCase):
    URL = "https://raw.githubusercontent.com/teighto/fixture/" + "1" * 40 + "/LICENSE"
    PAYLOAD = b"MIT License\nCopyright teighto\nComplete attribution and conditions.\n"

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.cache = self.root / "cache"
        self.cache.mkdir()
        for operation in ("socket.create_connection", "socket.getaddrinfo"):
            guard = mock.patch(operation, side_effect=AssertionError("license tests must not access the network"))
            guard.start()
            self.addCleanup(guard.stop)

    @staticmethod
    def response(payload, url):
        response = io.BytesIO(payload)
        response.geturl = lambda: url
        return response

    def document(self, payload=None, url=None):
        return {
            "url": self.URL if url is None else url,
            "sha256": hashlib.sha256(self.PAYLOAD if payload is None else payload).hexdigest(),
        }

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

    def test_transient_reset_retries_then_preserves_complete_pinned_paper_document(self):
        document = self.document()
        response = self.response(self.PAYLOAD, self.URL)
        failure = urllib.error.URLError(ConnectionResetError(10054, "connection reset by peer"))
        with mock.patch.object(MODULE.urllib.request, "urlopen", side_effect=[failure, response]) as opened, mock.patch("time.sleep"):
            source, text = MODULE.fetch_document(document, self.cache)
        self.assertEqual(opened.call_count, 2)
        self.assertEqual((source, text), (self.URL, self.PAYLOAD.decode("utf-8")))
        self.assertEqual((self.cache / (document["sha256"] + ".txt")).read_bytes(), self.PAYLOAD)
        self.assertTrue(response.closed)

    def test_paper_source_policy_is_checked_before_transport_without_retry(self):
        sources = [
            "http://raw.githubusercontent.com/teighto/fixture/" + "1" * 40 + "/LICENSE",
            "https://untrusted.invalid/LICENSE",
            "https://raw.githubusercontent.com/teighto/fixture/main/LICENSE",
            "https://username:password@www.apache.org/licenses/LICENSE-2.0.txt",
            "https://www.apache.org:443/licenses/LICENSE-2.0.txt",
            "https://www.apache.org/licenses/LICENSE-2.0.txt?alternate=1",
            "https://www.apache.org/licenses/LICENSE-2.0.txt#fragment",
        ]
        documents = [self.document(url=url) for url in sources]
        documents.append({"url": self.URL, "sha256": "A" * 64})
        for document in documents:
            with self.subTest(source=document["url"]), mock.patch.object(MODULE.urllib.request, "urlopen", side_effect=AssertionError("invalid source reached transport")) as opened, mock.patch("time.sleep") as slept:
                with self.assertRaises(ValueError):
                    MODULE.fetch_document(document, self.cache)
            opened.assert_not_called()
            slept.assert_not_called()
            self.assertEqual(list(self.cache.iterdir()), [])

    def test_untrusted_or_insecure_redirect_is_not_retried_even_with_pinned_bytes(self):
        for destination in ["https://untrusted.invalid/LICENSE", "http://www.apache.org/licenses/LICENSE-2.0.txt"]:
            response = self.response(self.PAYLOAD, destination)
            with self.subTest(destination=destination), mock.patch.object(MODULE.urllib.request, "urlopen", side_effect=[response, AssertionError("invalid redirect retried")]) as opened, mock.patch("time.sleep") as slept:
                with self.assertRaises(ValueError):
                    MODULE.fetch_document(self.document(), self.cache)
            self.assertEqual(opened.call_count, 1)
            slept.assert_not_called()
            self.assertTrue(response.closed)
            self.assertEqual(list(self.cache.iterdir()), [])

    def test_transient_http_error_from_untrusted_redirect_is_closed_without_retry(self):
        for destination in ["https://untrusted.invalid/LICENSE", "http://www.apache.org/licenses/LICENSE-2.0.txt"]:
            body = io.BytesIO(b"temporary unavailable")
            failure = urllib.error.HTTPError(destination, 503, "Service Unavailable", {}, body)
            with self.subTest(destination=destination), mock.patch.object(MODULE.urllib.request, "urlopen", side_effect=[failure, AssertionError("invalid redirect retried")]) as opened, mock.patch("time.sleep") as slept:
                with self.assertRaises(ValueError):
                    MODULE.fetch_document(self.document(), self.cache)
            self.assertEqual(opened.call_count, 1)
            slept.assert_not_called()
            self.assertTrue(body.closed)
            self.assertEqual(list(self.cache.iterdir()), [])

    def test_paper_checksum_and_size_failures_do_not_retry_or_replace_cached_bytes(self):
        oversize = b"x" * (MODULE.MAX_DOCUMENT_BYTES + 1)
        cases = [
            (b"wrong document", self.document()["sha256"]),
            (oversize, hashlib.sha256(oversize).hexdigest()),
        ]
        for index, (payload, digest) in enumerate(cases):
            cache = self.cache / str(index)
            cache.mkdir()
            cached = cache / (digest + ".txt")
            prior = b"previous cache must not be overwritten with invalid data"
            cached.write_bytes(prior)
            response = self.response(payload, self.URL)
            with self.subTest(size=len(payload)), mock.patch.object(MODULE.urllib.request, "urlopen", side_effect=[response, AssertionError("invalid document retried")]) as opened, mock.patch("time.sleep") as slept:
                with self.assertRaises(ValueError):
                    MODULE.fetch_document({"url": self.URL, "sha256": digest}, cache)
            self.assertEqual(opened.call_count, 1)
            slept.assert_not_called()
            self.assertTrue(response.closed)
            self.assertEqual(cached.read_bytes(), prior)
            self.assertEqual(list(cache.iterdir()), [cached])

    def test_invalid_pinned_text_is_still_rejected_without_transport_retry(self):
        for index, payload in enumerate([b"license\0binary", b"\xff"]):
            response = self.response(payload, self.URL)
            with self.subTest(payload=payload), mock.patch.object(MODULE.urllib.request, "urlopen", side_effect=[response, AssertionError("invalid text retried")]) as opened, mock.patch("time.sleep") as slept:
                with self.assertRaises(ValueError):
                    MODULE.fetch_document(self.document(payload), self.cache / str(index))
            self.assertEqual(opened.call_count, 1)
            slept.assert_not_called()
            self.assertTrue(response.closed)


if __name__ == "__main__":
    unittest.main()
