#!/usr/bin/env python3
import argparse
from contextlib import contextmanager
from dataclasses import dataclass
import gzip
import hashlib
import json
import os
from pathlib import Path
import stat
import tarfile
import tempfile
import unicodedata
import zipfile

RELEASE = "jdk-25.0.5+8"
ARCHIVE_ROOT = "jdk-25.0.5+8-jre"
MAX_UNPACKED = 512 * 1024 * 1024
MAX_FILE = 256 * 1024 * 1024
MAX_ENTRIES = 10_000
MAX_MANIFEST = 2 * 1024 * 1024
CHUNK = 1024 * 1024
ROOT = Path(__file__).resolve().parents[1]
ASCII_LOWER = str.maketrans("ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghijklmnopqrstuvwxyz")
ASCII_UPPER = str.maketrans("abcdefghijklmnopqrstuvwxyz", "ABCDEFGHIJKLMNOPQRSTUVWXYZ")


@dataclass(frozen=True)
class Asset:
    platform: str
    name: str
    size: int
    sha256: str


ASSETS = {
    "windows-x64": Asset("windows-x64", "OpenJDK25U-jre_x64_windows_hotspot_25.0.5_8.zip", 58_589_419,
                         "da78d54a2fcdbba1e92d5e3f07dd30236f96297891c4a21a781c3f38f5c0beae"),
    "linux-x64": Asset("linux-x64", "OpenJDK25U-jre_x64_linux_hotspot_25.0.5_8.tar.gz", 61_736_206,
                       "6fac98f0c8202efc3135a9e9e42aa61963ec42d1c5cbb40caf231e7b14a24ef6"),
}


def component_ok(part):
    base = part.split(".", 1)[0].translate(ASCII_UPPER)
    return (bool(part) and part not in (".", "..") and not any(char in part for char in '\\:<>"|?*')
            and not part.endswith((".", " ")) and not any(unicodedata.category(char) == "Cc" for char in part)
            and base not in ("CON", "NUL", "AUX", "PRN", "CONIN$", "CONOUT$")
            and not (len(base) == 4 and base[:3] in ("COM", "LPT") and base[3] in "123456789"))


def archive_path(name):
    name.encode("utf-8", errors="strict")
    parts = name.rstrip("/").split("/")
    if len(name.encode("utf-8")) > 4096 or parts[0] != ARCHIVE_ROOT or not all(map(component_ok, parts)):
        raise ValueError("unsafe or unexpected runtime archive path")
    return "/".join(parts[1:])


def link_target(source, target):
    target.encode("utf-8", errors="strict")
    if source.split("/")[0] != "legal" or target.startswith("/") or "\\" in target:
        raise ValueError("only relative archive-internal legal links are supported")
    parts = source.split("/")[:-1]
    for part in target.split("/"):
        if part == ".":
            continue
        if part == "..":
            if not parts:
                raise ValueError("runtime legal link escapes root")
            parts.pop()
        else:
            if not component_ok(part):
                raise ValueError("invalid runtime legal link target")
            parts.append(part)
    if not parts or parts[0] != "legal":
        raise ValueError("runtime legal link target is outside legal")
    return "/".join(parts)


@contextmanager
def verified_archive(path, asset):
    with Path(path).open("rb") as source, tempfile.SpooledTemporaryFile(max_size=8 * CHUNK) as snapshot:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size != asset.size:
            raise ValueError("archive does not match pinned file size")
        digest = hashlib.sha256()
        count = 0
        while block := source.read(CHUNK):
            count += len(block)
            if count > asset.size:
                raise ValueError("archive grew beyond pinned file size")
            digest.update(block)
            snapshot.write(block)
        if count != asset.size or digest.hexdigest() != asset.sha256:
            raise ValueError("archive does not match pinned SHA-256")
        snapshot.seek(0)
        yield snapshot


class Inventory:
    def __init__(self, platform):
        self.platform = platform
        self.nodes = {}
        self.parents = set()
        self.files = {}
        self.links = {}
        self.total = 0

    def key(self, path):
        return path.translate(ASCII_LOWER) if self.platform == "windows-x64" else path

    def declare(self, path, kind):
        key = self.key(path)
        if key in self.nodes or key in self.parents and kind != "directory":
            raise ValueError("duplicate, case-colliding or conflicting archive entry")
        parts = path.split("/")
        for index in range(1, len(parts)):
            parent = self.key("/".join(parts[:index]))
            if parent in self.nodes and self.nodes[parent] != "directory":
                raise ValueError("archive file conflicts with a parent directory")
            self.parents.add(parent)
        self.nodes[key] = kind

    def reserve(self, size):
        if size < 0 or size > MAX_FILE or self.total + size > MAX_UNPACKED:
            raise ValueError("runtime archive content exceeds limits")
        self.total += size

    def file(self, path, stream, size, mode):
        self.reserve(size)
        digest = hashlib.sha256()
        count = 0
        while block := stream.read(min(CHUNK, size + 1 - count)):
            count += len(block)
            if count > size:
                raise ValueError("archive entry exceeds declared size")
            digest.update(block)
        if count != size:
            raise ValueError("archive entry does not match declared size")
        self.files[path] = {"path": path, "size": size, "sha256": digest.hexdigest(),
                            "executable": self.platform == "linux-x64" and bool(mode & 0o111)}

    def resolve_links(self):
        while self.links:
            before = len(self.links)
            for path, target in sorted(list(self.links.items())):
                if target not in self.files:
                    continue
                target_file = self.files[target]
                self.reserve(target_file["size"])
                self.files[path] = {"path": path, "size": target_file["size"],
                                    "sha256": target_file["sha256"], "executable": False}
                del self.links[path]
            if before == len(self.links):
                raise ValueError("cyclic or missing runtime legal link target")


class LimitedReader:
    def __init__(self, source):
        self.source = source
        self.remaining = MAX_UNPACKED

    def read(self, size=-1):
        if self.remaining == 0:
            raise ValueError("decompressed runtime exceeds its byte limit")
        size = self.remaining if size < 0 else min(size, self.remaining)
        data = self.source.read(size)
        self.remaining -= len(data)
        return data


def read_zip(source, inventory):
    with zipfile.ZipFile(source) as archive:
        entries = archive.infolist()
        if len(entries) > MAX_ENTRIES:
            raise ValueError("too many runtime ZIP entries")
        for entry in entries:
            name = entry.orig_filename
            if not entry.flag_bits & 0x800:
                name = name.encode("cp437").decode("utf-8", errors="strict")
            path = archive_path(name)
            mode = entry.external_attr >> 16
            if stat.S_IFMT(mode) == stat.S_IFLNK:
                raise ValueError("runtime ZIP symlinks are forbidden")
            if not path:
                if not entry.is_dir():
                    raise ValueError("archive root must be a directory")
                continue
            inventory.declare(path, "directory" if entry.is_dir() else "file")
            if not entry.is_dir():
                with archive.open(entry) as stream:
                    inventory.file(path, stream, entry.file_size, mode)


def read_tar(source, inventory):
    with gzip.GzipFile(fileobj=source, mode="rb") as decompressed:
        limited = LimitedReader(decompressed)
        with tarfile.open(fileobj=limited, mode="r|", encoding="utf-8", errors="strict") as archive:
            for index, entry in enumerate(archive):
                if index >= MAX_ENTRIES:
                    raise ValueError("too many runtime TAR entries")
                path = archive_path(entry.name)
                if not path:
                    if not entry.isdir():
                        raise ValueError("archive root must be a directory")
                    continue
                if entry.isdir():
                    inventory.declare(path, "directory")
                elif entry.type in (tarfile.REGTYPE, tarfile.AREGTYPE):
                    inventory.declare(path, "file")
                    with archive.extractfile(entry) as stream:
                        inventory.file(path, stream, entry.size, entry.mode)
                elif entry.issym():
                    inventory.declare(path, "link")
                    inventory.links[path] = link_target(path, entry.linkname)
                else:
                    raise ValueError("runtime TAR hard links and special entries are forbidden")
        while limited.read(CHUNK):
            pass
    inventory.resolve_links()


def generate_inventory(path, asset):
    if asset.platform not in ASSETS:
        raise ValueError("unsupported runtime inventory platform")
    inventory = Inventory(asset.platform)
    with verified_archive(path, asset) as source:
        if asset.platform == "windows-x64":
            read_zip(source, inventory)
        else:
            read_tar(source, inventory)
    if not inventory.files:
        raise ValueError("runtime archive has no regular files")
    return {
        "schema": 1, "release": RELEASE, "platform": asset.platform,
        "archive_root": ARCHIVE_ROOT, "archive_name": asset.name,
        "archive_size": asset.size, "archive_sha256": asset.sha256,
        "files": [inventory.files[name] for name in sorted(inventory.files)],
    }


def encoded_manifest(manifest):
    encoded = (json.dumps(manifest, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    if len(encoded) > MAX_MANIFEST:
        raise ValueError("runtime manifest exceeds its byte limit")
    return encoded


def publish(path, content, check):
    if check:
        with path.open("rb") as current:
            if current.read(MAX_MANIFEST + 1) != content:
                raise ValueError(f"runtime inventory differs: {path.name}")
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=path.parent, prefix=".java-inventory-", delete=False) as target:
            temporary = Path(target.name)
            target.write(content)
            target.flush()
            os.fsync(target.fileno())
        os.replace(temporary, path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description="Generate pinned Temurin 25.0.5+8 installed-file inventories without running Java")
    parser.add_argument("--windows-archive", type=Path)
    parser.add_argument("--linux-archive", type=Path)
    parser.add_argument("--check", action="store_true", help="compare canonical output to committed manifests without modifying them")
    args = parser.parse_args()
    if args.windows_archive is None and args.linux_archive is None:
        parser.error("supply --windows-archive and/or --linux-archive")
    prepared = []
    for platform, path in [("windows-x64", args.windows_archive), ("linux-x64", args.linux_archive)]:
        if path is None:
            continue
        manifest = generate_inventory(path, ASSETS[platform])
        prepared.append((ROOT / "src/assets" / f"java-runtime-25.0.5-{platform}.json", manifest, encoded_manifest(manifest)))
    for output, manifest, content in prepared:
        publish(output, content, args.check)
        print(f"{manifest['platform']}: {len(manifest['files'])} files, {sum(file['size'] for file in manifest['files'])} installed bytes, manifest SHA-256 {hashlib.sha256(content).hexdigest()}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, tarfile.TarError, zipfile.BadZipFile) as failure:
        raise SystemExit(f"Java runtime inventory: {failure}") from failure
