import hashlib
import http.client
import importlib.util
import io
from pathlib import Path
import ssl
import tempfile
import unittest
import urllib.error
from unittest import mock

SCRIPT = Path(__file__).with_name("collect-licenses.py")
SPEC = importlib.util.spec_from_file_location("collect_licenses", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class LicenseTests(unittest.TestCase):
    URL = "https://licenses.invalid/pinned/LICENSE"
    PAYLOAD = b"Complete pinned license text\nCopyright teighto\nAll conditions retained.\n"

    def setUp(self):
        self.enterContext(mock.patch("socket.create_connection", side_effect=AssertionError("license tests must not open network connections")))
        self.enterContext(mock.patch("socket.getaddrinfo", side_effect=AssertionError("license tests must not resolve network hosts")))

    @staticmethod
    def response(payload, url):
        response = io.BytesIO(payload)
        response.geturl = lambda: url
        return response

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
            with mock.patch.object(MODULE, "GSON", gson), mock.patch.object(MODULE, "MUSL_RUNTIME", musl, create=True), mock.patch.object(MODULE.urllib.request, "urlopen", side_effect=AssertionError("verified fixture cache must avoid network")):
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
            with mock.patch.object(MODULE.urllib.request, "urlopen", return_value=response) as opened, mock.patch("time.sleep") as slept:
                with self.assertRaises(ValueError):
                    MODULE.fetched_url(response.geturl(), expected, cache)
            self.assertEqual(opened.call_count, 1)
            slept.assert_not_called()
            self.assertEqual(cached.read_bytes(), b"invalid cached copy")

    def test_runtime_notice_refuses_insecure_redirect_and_oversize_download(self):
        for destination, payload in [("http://untrusted.invalid/license", b"license"), ("https://git.musl-libc.org/license", b"x" * (4 * 1024 * 1024 + 1))]:
            with self.subTest(destination=destination), tempfile.TemporaryDirectory() as temporary:
                response = io.BytesIO(payload)
                response.geturl = lambda: destination
                with mock.patch.object(MODULE.urllib.request, "urlopen", return_value=response) as opened, mock.patch("time.sleep") as slept:
                    with self.assertRaises(ValueError):
                        MODULE.fetched_url("https://git.musl-libc.org/license", "0" * 64, Path(temporary))
                self.assertEqual(opened.call_count, 1)
                slept.assert_not_called()
                self.assertEqual(list(Path(temporary).iterdir()), [])

    def test_connection_reset_retries_then_preserves_the_pinned_document(self):
        windows_reset = OSError("connection forcibly closed by peer")
        windows_reset.winerror = 10054
        errors = [
            ("reset", ConnectionResetError(10054, "connection reset by peer")),
            ("wrapped_reset", urllib.error.URLError(ConnectionResetError(10054, "connection reset by peer"))),
            ("windows_oserror", urllib.error.URLError(windows_reset)),
            ("timeout", TimeoutError("download timed out")),
            ("wrapped_timeout", urllib.error.URLError(TimeoutError("download timed out"))),
        ]
        for label, failure in errors:
            with self.subTest(error=label), tempfile.TemporaryDirectory() as temporary:
                cache = Path(temporary)
                expected = hashlib.sha256(self.PAYLOAD).hexdigest()
                response = self.response(self.PAYLOAD, self.URL)
                with mock.patch.object(MODULE.urllib.request, "urlopen", side_effect=[failure, response]) as opened, mock.patch("time.sleep"):
                    try:
                        source, document = MODULE.fetched_url(self.URL, expected, cache)
                    except OSError as error:
                        self.fail(f"transient reset was not retried: {error}")
                self.assertEqual((source, document), (self.URL, self.PAYLOAD.decode("utf-8")))
                self.assertEqual(opened.call_count, 2)
                self.assertEqual((cache / f"{expected}.txt").read_bytes(), self.PAYLOAD)
                self.assertTrue(response.closed)

    def test_body_read_reset_closes_failed_response_before_retry(self):
        class InterruptedResponse(io.BytesIO):
            def __init__(self, failure):
                super().__init__(b"partial unverified document")
                self.failure = failure

            def read(self, size=-1):
                super().read(7)
                raise self.failure

        for failure in [ConnectionResetError(10054, "body read reset"), http.client.IncompleteRead(b"partial", 17)]:
            with self.subTest(error=type(failure).__name__), tempfile.TemporaryDirectory() as temporary:
                cache = Path(temporary)
                expected = hashlib.sha256(self.PAYLOAD).hexdigest()
                failed = InterruptedResponse(failure)
                failed.geturl = lambda: self.URL
                response = self.response(self.PAYLOAD, self.URL)
                observed_closed = []

                def open_response(*_args, **_kwargs):
                    if not observed_closed:
                        observed_closed.append(False)
                        return failed
                    observed_closed.append(failed.closed)
                    return response

                with mock.patch.object(MODULE.urllib.request, "urlopen", side_effect=open_response) as opened, mock.patch("time.sleep"):
                    try:
                        source, document = MODULE.fetched_url(self.URL, expected, cache)
                    except (OSError, http.client.IncompleteRead) as error:
                        self.fail(f"response-body interruption was not retried: {error}")
                self.assertEqual(opened.call_count, 2)
                self.assertEqual(observed_closed, [False, True])
                self.assertEqual((source, document), (self.URL, self.PAYLOAD.decode("utf-8")))
                self.assertEqual((cache / f"{expected}.txt").read_bytes(), self.PAYLOAD)
                self.assertTrue(response.closed)

    def test_http_503_retries_after_closing_the_error_response(self):
        with tempfile.TemporaryDirectory() as temporary:
            cache = Path(temporary)
            expected = hashlib.sha256(self.PAYLOAD).hexdigest()
            error_body = io.BytesIO(b"temporarily unavailable")
            failure = urllib.error.HTTPError(self.URL, 503, "Unavailable", {}, error_body)
            response = self.response(self.PAYLOAD, self.URL)
            observed_closed = []

            def open_response(*_args, **_kwargs):
                if not observed_closed:
                    observed_closed.append(False)
                    raise failure
                observed_closed.append(error_body.closed)
                return response

            with mock.patch.object(MODULE.urllib.request, "urlopen", side_effect=open_response) as opened, mock.patch("time.sleep"):
                source, document = MODULE.fetched_url(self.URL, expected, cache)
            self.assertEqual(opened.call_count, 2)
            self.assertEqual(observed_closed, [False, True])
            self.assertEqual((source, document), (self.URL, self.PAYLOAD.decode("utf-8")))
            self.assertEqual((cache / f"{expected}.txt").read_bytes(), self.PAYLOAD)

    def test_certificate_and_permanent_http_errors_are_not_retried(self):
        certificate = ssl.SSLCertVerificationError(1, "certificate verification failed")
        certificate_with_socket_code = ssl.SSLCertVerificationError(1, "certificate verification failed")
        certificate_with_socket_code.winerror = 10054
        not_found_body = io.BytesIO(b"not found")
        cases = [
            ("certificate", certificate, None),
            ("wrapped_certificate", urllib.error.URLError(certificate), None),
            ("certificate_with_socket_code", urllib.error.URLError(certificate_with_socket_code), None),
            ("not_found", urllib.error.HTTPError(self.URL, 404, "Not Found", {}, not_found_body), not_found_body),
        ]
        for label, failure, error_body in cases:
            with self.subTest(error=label), tempfile.TemporaryDirectory() as temporary:
                cache = Path(temporary)
                expected = hashlib.sha256(self.PAYLOAD).hexdigest()
                cached = cache / f"{expected}.txt"
                cached.write_bytes(b"existing unverified bytes")
                response = self.response(self.PAYLOAD, self.URL)
                with mock.patch.object(MODULE.urllib.request, "urlopen", side_effect=[failure, response]) as opened, mock.patch("time.sleep") as slept:
                    with self.assertRaises(OSError) as raised:
                        MODULE.fetched_url(self.URL, expected, cache)
                self.assertEqual(opened.call_count, 1)
                slept.assert_not_called()
                self.assertIn(self.URL, str(raised.exception))
                self.assertEqual(cached.read_bytes(), b"existing unverified bytes")
                if error_body is not None:
                    self.assertTrue(error_body.closed)
                response.close()

    def test_retry_exhaustion_is_bounded_and_does_not_publish_unverified_cache(self):
        for cached_bytes in (None, b"previous invalid cache must be preserved"):
            with self.subTest(existing_cache=cached_bytes is not None), tempfile.TemporaryDirectory() as temporary:
                cache = Path(temporary)
                expected = hashlib.sha256(self.PAYLOAD).hexdigest()
                cached = cache / f"{expected}.txt"
                if cached_bytes is not None:
                    cached.write_bytes(cached_bytes)
                failures = [urllib.error.URLError(ConnectionResetError(10054, "network reset")) for _ in range(3)]
                failures.append(AssertionError("license download exceeded three attempts"))
                with mock.patch.object(MODULE.urllib.request, "urlopen", side_effect=failures) as opened, mock.patch("time.sleep"):
                    with self.assertRaises(OSError) as raised:
                        MODULE.fetched_url(self.URL, expected, cache)
                self.assertEqual(opened.call_count, 3)
                self.assertIn(self.URL, str(raised.exception))
                if cached_bytes is None:
                    self.assertEqual(list(cache.iterdir()), [])
                else:
                    self.assertEqual(cached.read_bytes(), cached_bytes)
                    self.assertEqual(list(cache.iterdir()), [cached])

    def test_invalid_source_or_digest_is_rejected_before_network_without_retry(self):
        valid_digest = hashlib.sha256(self.PAYLOAD).hexdigest()
        cases = [
            ("http://licenses.invalid/pinned/LICENSE", valid_digest),
            (self.URL, "not-a-digest"),
            (self.URL, "a" * 63),
            (self.URL, "A" * 64),
        ]
        for url, digest in cases:
            with self.subTest(url=url, digest=digest), tempfile.TemporaryDirectory() as temporary:
                with mock.patch.object(MODULE.urllib.request, "urlopen", side_effect=AssertionError("invalid pin must not reach network")) as opened, mock.patch("time.sleep") as slept:
                    with self.assertRaises(ValueError):
                        MODULE.fetched_url(url, digest, Path(temporary))
                opened.assert_not_called()
                slept.assert_not_called()
                self.assertEqual(list(Path(temporary).iterdir()), [])

    def test_download_validation_failures_are_never_retried_or_cached(self):
        oversize = b"x" * (4 * 1024 * 1024 + 1)
        cases = [
            ("http://untrusted.invalid/license", self.PAYLOAD, hashlib.sha256(self.PAYLOAD).hexdigest()),
            (self.URL, oversize, hashlib.sha256(oversize).hexdigest()),
            (self.URL, b"wrong document", hashlib.sha256(self.PAYLOAD).hexdigest()),
        ]
        for destination, payload, digest in cases:
            with self.subTest(destination=destination, size=len(payload)), tempfile.TemporaryDirectory() as temporary:
                response = self.response(payload, destination)
                unexpected_retry = self.response(self.PAYLOAD, self.URL)
                with mock.patch.object(MODULE.urllib.request, "urlopen", side_effect=[response, unexpected_retry]) as opened, mock.patch("time.sleep") as slept:
                    with self.assertRaises(ValueError):
                        MODULE.fetched_url(self.URL, digest, Path(temporary))
                self.assertEqual(opened.call_count, 1)
                slept.assert_not_called()
                self.assertTrue(response.closed)
                self.assertEqual(list(Path(temporary).iterdir()), [])
                unexpected_retry.close()

    def test_verified_cache_hit_returns_full_document_without_network(self):
        with tempfile.TemporaryDirectory() as temporary:
            cache = Path(temporary)
            expected = hashlib.sha256(self.PAYLOAD).hexdigest()
            cached = cache / f"{expected}.txt"
            cached.write_bytes(self.PAYLOAD)
            with mock.patch.object(MODULE.urllib.request, "urlopen", side_effect=AssertionError("cache hit must avoid network")) as opened, mock.patch("time.sleep") as slept:
                source, document = MODULE.fetched_url(self.URL, expected, cache)
            self.assertEqual((source, document), (self.URL, self.PAYLOAD.decode("utf-8")))
            self.assertEqual(cached.read_bytes(), self.PAYLOAD)
            opened.assert_not_called()
            slept.assert_not_called()


if __name__ == "__main__":
    unittest.main()
