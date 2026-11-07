#!/usr/bin/env python3
"""Build a local Zerqon development distribution from an explicit artifact allowlist."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform as host_platform
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]


def run(*args):
    subprocess.run([str(value) for value in args], cwd=ROOT, check=True)


def release_version(root):
    version = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?", version):
        raise ValueError("invalid release version")
    return version


def paper_dependencies(root):
    lock = json.loads(project_file(root, "tools/paper-bridge/dependencies.lock.json").read_text(encoding="utf-8"))
    artifacts = lock["artifacts"]
    if not 1 <= len(artifacts) <= 256:
        raise ValueError("invalid Paper dependency count")
    result = {}
    for artifact in artifacts:
        name = artifact["file"]
        digest = artifact["sha256"]
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+-]*\.jar", name) or not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise ValueError("invalid Paper dependency name or SHA-256")
        if name in result:
            raise ValueError("duplicate Paper dependency name")
        result[name] = digest
    return result


def allowlist(version, platform, paper_files):
    if platform not in ("windows-x86_64", "linux-x86_64"):
        raise ValueError("supported release platforms: windows-x86_64, linux-x86_64")
    native = "zerqon.exe" if platform.startswith("windows") else "zerqon"
    files = {
        native: f"target/release/{native}",
        "libraries/gson-2.13.2.jar": "libraries/gson-2.13.2.jar",
        f"libraries/zerqon-java-api-{version}.jar": f"libraries/zerqon-java-api-{version}.jar",
        f"libraries/zerqon-java-host-{version}.jar": f"libraries/zerqon-java-host-{version}.jar",
        "examples/java-plugins/hello.jar": f"dist/hello-java-plugin-{version}.jar",
        "examples/rust-plugins/hello.wasm": "target/wasm32-unknown-unknown/release/zerqon_hello_plugin.wasm",
        f"libraries/paper/zerqon-paper-bridge-{version}.jar": f"libraries/paper/zerqon-paper-bridge-{version}.jar",
        "PAPER-LICENSES.txt": "target/release-notices/PAPER-LICENSES.txt",
    }
    for name in paper_files:
        path = "libraries/paper/" + name
        files[path] = path
    for path in ("README.md", "LICENSE", "docs/CONFIGURATION.md", "docs/RUST_PLUGINS.md", "docs/JAVA_PLUGINS.md", "docs/JAVA_COMPATIBILITY.md", "docs/PROFILING.md", "crates/zerqon-profiler/NOTICE.md", "tools/paper-bridge/dependencies.lock.json", "tools/paper-bridge/licenses.lock.json", "docs/ROADMAP.md", "docs/images/architecture.png", "docs/images/architecture.svg", "docs/images/performance.png", "docs/images/performance.svg", "docs/images/plugins.png", "docs/images/plugins.svg", "scripts/prepare-data.py"):
        files[path] = path
    return files


def project_file(root, relative):
    path = root / relative
    resolved = path.resolve(strict=True)
    if path.is_symlink() or not resolved.is_relative_to(root.resolve()):
        raise ValueError(f"refusing release input outside project or symlink: {relative}")
    if not path.is_file():
        raise ValueError(f"release input is not a regular file: {relative}")
    return path


def archive_entry(archive, name, data, executable=False):
    info = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
    info.create_system = 3
    info.external_attr = (stat.S_IFREG | (0o755 if executable else 0o644)) << 16
    info.compress_type = zipfile.ZIP_DEFLATED
    archive.writestr(info, data)


def package(root: Path, output: Path, platform: str, notices: Path) -> Path:
    root = root.resolve()
    version = release_version(root)
    paper_files = paper_dependencies(root)
    pinned_files = {"libraries/paper/" + name: digest for name, digest in paper_files.items()}
    files = {name: project_file(root, path) for name, path in allowlist(version, platform, paper_files).items()}
    if not notices.is_file() or notices.is_symlink():
        raise FileNotFoundError(f"third-party notices missing: {notices}")
    files["THIRD-PARTY-NOTICES.txt"] = notices
    revision = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root, capture_output=True, text=True)
    commit = revision.stdout.strip() if revision.returncode == 0 else None
    status = subprocess.run(["git", "status", "--porcelain"], cwd=root, capture_output=True, text=True)
    dirty = bool(status.stdout.strip()) if status.returncode == 0 else None
    build_info = {
        "project": "Zerqon", "version": version, "release_channel": "development", "minecraft": "26.3",
        "protocol": 777, "platform": platform, "author": "teighto", "git_commit": commit, "source_dirty": dirty,
        "source": "https://github.com/teighto/Zerqon-Core",
        "source_at_commit": f"https://github.com/teighto/Zerqon-Core/tree/{commit}" if commit and not dirty else None,
    }
    output.mkdir(parents=True, exist_ok=True)
    destination = output / f"zerqon-{version}-{platform}.zip"
    handle, temporary_name = tempfile.mkstemp(prefix=".zerqon-package-", suffix=".zip", dir=output)
    os.close(handle)
    temporary = Path(temporary_name)
    try:
        checksums = []
        with zipfile.ZipFile(temporary, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=6) as archive:
            for name, path in sorted(files.items()):
                data = path.read_bytes()
                if name in pinned_files and hashlib.sha256(data).hexdigest() != pinned_files[name]:
                    raise ValueError("release dependency checksum mismatch: " + name)
                archive_entry(archive, name, data, executable=name == "zerqon")
                checksums.append(f"{hashlib.sha256(data).hexdigest()}  {name}")
            data = (json.dumps(build_info, indent=2) + "\n").encode()
            archive_entry(archive, "BUILD-INFO.json", data)
            checksums.append(f"{hashlib.sha256(data).hexdigest()}  BUILD-INFO.json")
            archive_entry(archive, "SHA256SUMS", ("\n".join(sorted(checksums)) + "\n").encode())
        with zipfile.ZipFile(temporary) as archive:
            if archive.testzip() is not None:
                raise ValueError("release archive failed CRC verification")
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)
    with destination.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    destination.with_suffix(".zip.sha256").write_text(f"{digest}  {destination.name}\n", encoding="utf-8")
    return destination


def build():
    run("cargo", "fmt", "--all", "--check")
    run("cargo", "clippy", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings")
    run("cargo", "test", "--workspace", "--locked")
    run("cargo", "build", "-p", "zerqon", "--release", "--locked")
    run("cargo", "build", "-p", "zerqon-hello-plugin", "--target", "wasm32-unknown-unknown", "--release", "--locked")
    run("cargo", "run", "-p", "zerqon-plugins", "--example", "verify-plugin", "--locked", "--", "target/wasm32-unknown-unknown/release/zerqon_hello_plugin.wasm")
    run(sys.executable, ROOT / "scripts/build-java-plugins.py")


def verify_distribution(archive_path):
    with tempfile.TemporaryDirectory(prefix="zerqon-release-check-") as temporary:
        directory = Path(temporary)
        with zipfile.ZipFile(archive_path) as archive:
            archive.extractall(directory)
            for line in archive.read("SHA256SUMS").decode("utf-8").splitlines():
                expected, name = line.split("  ", 1)
                if hashlib.sha256((directory / name).read_bytes()).hexdigest() != expected:
                    raise ValueError(f"release checksum mismatch: {name}")
        binary = directory / ("zerqon.exe" if os.name == "nt" else "zerqon")
        binary.chmod(binary.stat().st_mode | 0o111)
        commands = [
            [str(binary), "--version"],
            [str(binary), "--init", "--directory", "run"],
            [str(binary), "--check-config", "--directory", "run"],
        ]
        for command in commands:
            subprocess.run(command, cwd=directory, check=True, timeout=30)
        plugin_directory = directory / "java-smoke/plugins"
        plugin_directory.mkdir(parents=True)
        shutil.copy2(directory / "examples/java-plugins/hello.jar", plugin_directory / "hello.jar")
        version = json.loads((directory / "BUILD-INFO.json").read_text(encoding="utf-8"))["version"]
        classpath = [directory / f"libraries/zerqon-java-host-{version}.jar", directory / "libraries/gson-2.13.2.jar"]
        classpath.extend(directory / "libraries/paper" / name for name in paper_dependencies(directory))
        classpath.append(directory / f"libraries/paper/zerqon-paper-bridge-{version}.jar")
        requests = [
            {"id": 1, "op": "load", "directory": str(plugin_directory), "server_version": version},
            {"id": 2, "op": "event", "plugin": "hello", "event": {"event": "command", "sender": "distributioncheck", "sender_kind": "console", "operator": True, "command": "jhello", "args": []}},
            {"id": 3, "op": "shutdown"},
        ]
        result = subprocess.run(["java", "-Xmx128M", "-cp", os.pathsep.join(map(str, classpath)), "io.github.teighto.zerqon.host.PluginHost"], input="".join(json.dumps(request) + "\n" for request in requests), capture_output=True, text=True, encoding="utf-8", cwd=directory, check=True, timeout=30)
        responses = [json.loads(line) for line in result.stdout.splitlines()]
        if len(responses) != 3 or [response["id"] for response in responses] != [1, 2, 3] or not all(response["ok"] for response in responses):
            raise ValueError("packaged Java host failed its request protocol smoke test")
        actions = responses[1]["results"][0]["actions"]
        if actions != [{"action": "reply", "message": "Hello, distributioncheck! This plugin uses the Zerqon Java API."}]:
            raise ValueError("packaged Java plugin did not return its expected greeting")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--skip-build", action="store_true", help="Package already built and verified artifacts")
    parser.add_argument("--output", type=Path, default=ROOT / "dist/releases")
    parser.add_argument("--platform", choices=("windows-x86_64", "linux-x86_64"))
    args = parser.parse_args()
    if host_platform.machine().lower() not in ("amd64", "x86_64"):
        raise ValueError("release packaging currently supports x86-64 hosts only")
    current_platform = "windows-x86_64" if os.name == "nt" else "linux-x86_64" if sys.platform.startswith("linux") else None
    if current_platform is None or args.platform not in (None, current_platform):
        raise ValueError("build this release on its target Windows or Linux platform")
    if not args.skip_build:
        build()
    notices = ROOT / "target/release-notices/THIRD-PARTY-NOTICES.txt"
    run(sys.executable, ROOT / "scripts/collect-licenses.py", "--output", notices)
    run(sys.executable, ROOT / "scripts/collect-paper-licenses.py")
    archive = package(ROOT, args.output.resolve(), current_platform, notices)
    verify_distribution(archive)
    print(f"Packaged {archive}")



if __name__ == "__main__":
    main()
