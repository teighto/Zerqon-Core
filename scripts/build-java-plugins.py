#!/usr/bin/env python3
import argparse
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import urllib.request
import json

ROOT = Path(__file__).resolve().parents[1]
GSON_NAME = "gson-2.13.2.jar"
GSON_SHA256 = "dd0ce1b55a3ed2080cb70f9c655850cda86c206862310009dcb5e5c95265a5e0"
GSON_URL = f"https://repo.maven.apache.org/maven2/com/google/code/gson/gson/2.13.2/{GSON_NAME}"


def run(*args):
    subprocess.run([str(arg) for arg in args], cwd=ROOT, check=True)


def jdk_tools(java_home):
    if not java_home:
        result = subprocess.run(["java", "-XshowSettings:properties", "-version"], capture_output=True, text=True, check=True)
        match = re.search(r"java.home\s*=\s*([^\r\n]+)", result.stderr)
        if not match:
            raise RuntimeError("Set JAVA_HOME to JDK 25 or later")
        java_home = match.group(1).strip()
    suffix = ".exe" if os.name == "nt" else ""
    tools = [Path(java_home) / "bin" / f"{name}{suffix}" for name in ("java", "javac", "jar")]
    if not all(tool.is_file() for tool in tools):
        raise RuntimeError(f"Missing JDK tools in {java_home}")
    return tools


def download_gson(libraries):
    path = libraries / GSON_NAME
    if path.exists() and hashlib.sha256(path.read_bytes()).hexdigest() != GSON_SHA256:
        raise RuntimeError(f"Checksum mismatch: {path}; refusing to overwrite")
    if not path.exists():
        with urllib.request.urlopen(GSON_URL, timeout=30) as response:
            content = response.read(2_000_001)
        if len(content) > 2_000_000 or hashlib.sha256(content).hexdigest() != GSON_SHA256:
            raise RuntimeError("Gson download failed SHA-256 verification")
        with path.open("xb") as output:
            output.write(content)
    return path


def compile_java(javac, sources, target, classpath=None):
    build_root = (ROOT / "target/java-plugins").resolve()
    if target.is_symlink() or not target.resolve().is_relative_to(build_root) or target.resolve() == build_root:
        raise RuntimeError(f"Unsafe build output path: {target}")
    if target.exists():
        shutil.rmtree(target)
    target.mkdir(parents=True, exist_ok=True)
    args = [javac, "--release", "25", "-encoding", "UTF-8", "-Xlint:all", "-Werror"]
    if classpath:
        args.extend(["-cp", os.pathsep.join(str(path) for path in classpath)])
    args.extend(["-d", target, *sorted(sources.rglob("*.java"))])
    run(*args)


def main():
    parser = argparse.ArgumentParser(description="Build and test Zerqon's Java plugin API and host")
    parser.add_argument("--java-home", default=os.environ.get("JAVA_HOME"))
    parser.add_argument("--skip-tests", action="store_true")
    args = parser.parse_args()
    java, javac, jar = jdk_tools(args.java_home)
    build = ROOT / "target" / "java-plugins"
    libraries = ROOT / "libraries"
    dist = ROOT / "dist"
    for directory in (build, libraries, dist):
        directory.mkdir(parents=True, exist_ok=True)
    gson = download_gson(libraries)
    run(sys.executable, ROOT / "tools/paper-bridge/build.py", "--java-home", java.parent.parent)
    paper_libraries = libraries / "paper"
    paper_libraries.mkdir(parents=True, exist_ok=True)
    paper_lock = json.loads((ROOT / "tools/paper-bridge/dependencies.lock.json").read_text(encoding="utf-8"))
    paper_dependencies = []
    for artifact in paper_lock["artifacts"]:
        source = ROOT / ".cache/libraries/paper-26.3" / artifact["file"]
        destination = paper_libraries / artifact["file"]
        shutil.copy2(source, destination)
        paper_dependencies.append(destination)
    paper_bridge = paper_libraries / "zerqon-paper-bridge-0.0.1.jar"
    shutil.copy2(ROOT / "dist" / paper_bridge.name, paper_bridge)
    paper_dependencies.append(paper_bridge)
    api_classes = build / "api"
    host_classes = build / "host"
    example_classes = build / "example"
    fixture_classes = build / "fixtures"
    compile_java(javac, ROOT / "tools/java-plugins/api", api_classes)
    compile_java(javac, ROOT / "tools/java-plugins/host", host_classes, [api_classes, *paper_dependencies, gson])
    api_jar = libraries / "zerqon-java-api-0.0.1.jar"
    host_jar = libraries / "zerqon-java-host-0.0.1.jar"
    run(jar, "--create", "--file", api_jar, "-C", api_classes, ".")
    run(jar, "--create", "--file", host_jar, "--main-class", "io.github.teighto.zerqon.host.PluginHost", "-C", api_classes, ".", "-C", host_classes, ".")
    compile_java(javac, ROOT / "examples/hello-java-plugin/src", example_classes, [api_jar])
    example_jar = dist / "hello-java-plugin-0.0.1.jar"
    run(jar, "--create", "--file", example_jar, "--manifest", ROOT / "examples/hello-java-plugin/MANIFEST.MF", "-C", example_classes, ".")
    if not args.skip_tests:
        compile_java(javac, ROOT / "tools/java-plugins/tests/fixtures", fixture_classes, [api_jar])
        run(sys.executable, ROOT / "tools/java-plugins/tests/test_host.py", "--java", java, "--fixtures", fixture_classes)
    print(f"Built {api_jar.name}, {host_jar.name}, {example_jar.name}")


if __name__ == "__main__":
    main()
