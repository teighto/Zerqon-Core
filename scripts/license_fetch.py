"""Bounded transport retries shared by the release license collectors."""

import errno
import http.client
import socket
import ssl
import time
import urllib.error
import urllib.request


def transient_download_error(error):
    if isinstance(error, urllib.error.HTTPError):
        return error.code in {408, 429, 500, 502, 503, 504}
    if isinstance(error, urllib.error.URLError):
        error = error.reason
    if isinstance(error, ssl.SSLCertVerificationError):
        return False
    if isinstance(error, (ConnectionError, TimeoutError, http.client.IncompleteRead, ssl.SSLEOFError)):
        return True
    if isinstance(error, socket.gaierror):
        return error.errno == socket.EAI_AGAIN
    return isinstance(error, OSError) and (
        error.errno in {
            errno.ECONNRESET, errno.ECONNABORTED, errno.ETIMEDOUT,
            errno.EPIPE, errno.ENETRESET, errno.ENETUNREACH, errno.EHOSTUNREACH,
        }
        or getattr(error, "winerror", None) in {
            10051, 10052, 10053, 10054, 10060, 10061, 10065,
        }
    )


def read_license_url(url, max_bytes, validate_redirect):
    for attempt in range(1, 4):
        try:
            with urllib.request.urlopen(url, timeout=30) as response:
                validate_redirect(response.geturl())
                return response.read(max_bytes + 1)
        except (OSError, http.client.IncompleteRead) as error:
            if isinstance(error, urllib.error.HTTPError):
                try:
                    validate_redirect(error.geturl())
                finally:
                    error.close()
            retry = transient_download_error(error)
            if not retry or attempt == 3:
                raise OSError(
                    f"License download failed for {url} after {attempt} attempt(s): {error}"
                ) from error
            time.sleep(0.5 * attempt)
