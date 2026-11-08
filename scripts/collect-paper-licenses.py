"""Collect verified license texts for the exact Paper compatibility artifacts."""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import tempfile
import urllib.parse
import urllib.request
import zipfile

from license_fetch import read_license_url

MAX_JAR_BYTES = 64 * 1024 * 1024
MAX_DOCUMENT_BYTES = 2 * 1024 * 1024
NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._+-]*\.jar\Z")
DOCUMENT_NAME = re.compile(r"^(LICENSE|LICENCE|COPYING|NOTICE|COPYRIGHT)([._-]|$)", re.I)
LICENSE_NAME = re.compile(r"^(LICENSE|LICENCE|COPYING)([._-]|$)", re.I)
TEXT_EXTENSIONS = {"", ".txt", ".md", ".html", ".htm", ".rst"}


def text_document(data):
    if not data or b"\0" in data:
        raise ValueError("license document is empty or binary")
    if b"charset=ISO-8859-1" in data[:1024]:
        return data.decode("iso-8859-1")
    return data.decode("utf-8")


def jar_documents(data):
    documents = []
    has_license = False
    with zipfile.ZipFile(io.BytesIO(data)) as jar:
        names = set()
        for entry in jar.infolist():
            path = PurePosixPath(entry.filename)
            if path.is_absolute() or ".." in path.parts or "\\" in entry.filename:
                raise ValueError("unsafe JAR entry path")
            if entry.filename in names:
                raise ValueError("duplicate JAR entry path")
            names.add(entry.filename)
            if entry.is_dir() or path.suffix.lower() not in TEXT_EXTENSIONS:
                continue
            if not DOCUMENT_NAME.match(path.name) and path.name.lower() != "about.html":
                continue
            if entry.file_size > MAX_DOCUMENT_BYTES:
                raise ValueError("oversized bundled license document")
            text = text_document(jar.read(entry))
            documents.append((entry.filename, text))
            has_license |= bool(LICENSE_NAME.match(path.name))
    return documents, has_license


def fetch_document(document, cache):
    url = document["url"]
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme != "https" or parsed.hostname not in {"raw.githubusercontent.com", "www.apache.org"} or parsed.username or parsed.password or parsed.query or parsed.fragment or parsed.port:
        raise ValueError("license source must be a pinned primary HTTPS URL")
    if parsed.hostname == "raw.githubusercontent.com" and not re.fullmatch(r"/[^/]+/[^/]+/[0-9a-f]{40}/.+", parsed.path):
        raise ValueError("GitHub license source must use a commit revision")
    digest = document["sha256"]
    if not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise ValueError("invalid license SHA-256")
    cached = cache / (digest + ".txt")
    data = cached.read_bytes() if cached.is_file() else b""
    if hashlib.sha256(data).hexdigest() != digest:
        def validate_redirect(destination):
            final = urllib.parse.urlsplit(destination)
            if final.scheme != "https" or final.hostname not in {"raw.githubusercontent.com", "www.apache.org"}:
                raise ValueError("unexpected license redirect")
        data = read_license_url(url, MAX_DOCUMENT_BYTES, validate_redirect)
        if len(data) > MAX_DOCUMENT_BYTES or hashlib.sha256(data).hexdigest() != digest:
            raise ValueError("license source checksum mismatch: " + url)
        cache.mkdir(parents=True, exist_ok=True)
        cached.write_bytes(data)
    if len(data) > MAX_DOCUMENT_BYTES:
        raise ValueError("oversized cached license document")
    text = text_document(data)
    if document.get("extract") == "copyright_comment":
        comments = re.findall(r"/\*.*?\*/", text, re.S)
        comments = [comment for comment in comments if "Copyright" in comment and "License" in comment]
        if len(comments) != 1:
            raise ValueError("expected one pinned copyright/license comment")
        text = comments[0]
    elif "extract" in document:
        raise ValueError("unknown license extraction rule")
    return url, text


def collect(dependency_lock, license_lock, libraries, cache):
    artifacts = dependency_lock["artifacts"]
    if not 1 <= len(artifacts) <= 256:
        raise ValueError("invalid dependency count")
    files = [entry["file"] for entry in artifacts]
    if len(set(files)) != len(files) or set(files) != set(license_lock["artifacts"]):
        raise ValueError("license mapping must cover exactly the locked artifacts")
    sections = ["Zerqon Paper compatibility dependencies: licenses and notices\n", "Zerqon author: teighto. Upstream authors retain their copyrights.\n", "The files below are separate, unmodified upstream JAR artifacts, verified against dependencies.lock.json.\n", "Bundled legal notices are retained. Missing texts are fetched from the primary sources pinned in licenses.lock.json.\n"]
    for artifact in sorted(artifacts, key=lambda item: item["file"]):
        name = artifact["file"]
        if not NAME.fullmatch(name):
            raise ValueError("invalid dependency file name")
        path = libraries / name
        if path.is_symlink() or not path.resolve().is_relative_to(libraries.resolve()):
            raise ValueError("dependency path escapes library directory")
        with path.open("rb") as source:
            data = source.read(MAX_JAR_BYTES + 1)
        if len(data) > MAX_JAR_BYTES or hashlib.sha256(data).hexdigest() != artifact["sha256"]:
            raise ValueError("dependency checksum mismatch: " + name)
        record = license_lock["artifacts"][name]
        documents, has_license = jar_documents(data)
        sections.append(f"\n{'=' * 78}\n{name}\nArtifact: {artifact['url']}\nSHA-256: {artifact['sha256']}\nLicense: {record['license']}\n")
        if "source_archive" in record:
            source_archive = record["source_archive"]
            if not source_archive["url"].startswith("https://") or not re.fullmatch(r"[0-9a-f]{64}", source_archive["sha256"]):
                raise ValueError("invalid corresponding source archive pin")
            sections.append(f"Corresponding upstream source archive: {source_archive['url']}\nSource SHA-256: {source_archive['sha256']}\n")
        for source_name in record["sources"]:
            source = license_lock["sources"][source_name]
            sections.append("Upstream source: " + source["source"] + "\n")
            for document in source["documents"]:
                documents.append(fetch_document(document, cache))
                has_license = True
        if not documents or not has_license:
            raise ValueError("no license text for " + name)
        for document_name, content in documents:
            sections.append(f"\n--- {document_name} ---\n{content}")
            if not content.endswith("\n"):
                sections.append("\n")
    return "".join(sections)


def main():
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=root / "target/release-notices/PAPER-LICENSES.txt")
    parser.add_argument("--libraries", type=Path, default=root / "libraries/paper")
    parser.add_argument("--cache", type=Path, default=root / ".cache/licenses/paper")
    args = parser.parse_args()
    dependency = json.loads((root / "tools/paper-bridge/dependencies.lock.json").read_text(encoding="utf-8"))
    licenses = json.loads((root / "tools/paper-bridge/licenses.lock.json").read_text(encoding="utf-8"))
    content = collect(dependency, licenses, args.libraries, args.cache)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile("w", encoding="utf-8", newline="", dir=args.output.parent, delete=False) as target:
            temporary = Path(target.name)
            target.write(content)
        os.replace(temporary, args.output)
    finally:
        if temporary:
            temporary.unlink(missing_ok=True)
    print(f"Collected complete notices for {len(dependency['artifacts'])} locked Paper dependencies into {args.output}")


if __name__ == "__main__":
    main()
