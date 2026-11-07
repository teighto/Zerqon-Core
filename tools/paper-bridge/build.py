import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import urllib.request
import zipfile
from datetime import datetime, timezone
import shutil
import tempfile

ROOT = Path(__file__).resolve().parents[2]
SOURCE = Path(__file__).resolve().parent


def run(*args):
    result = subprocess.run([str(arg) for arg in args], cwd=ROOT, timeout=120)
    if result.returncode:
        raise SystemExit(f"{Path(args[0]).name} failed with exit code {result.returncode}")


def dependencies():
    target = ROOT / ".cache/libraries/paper-26.3"
    target.mkdir(parents=True, exist_ok=True)
    files = []
    for artifact in json.loads((SOURCE / "dependencies.lock.json").read_text(encoding="utf-8"))["artifacts"]:
        path = target / artifact["file"]
        if not path.exists():
            with urllib.request.urlopen(artifact["url"], timeout=60) as response:
                data = response.read(40_000_001)
            if len(data) > 40_000_000 or hashlib.sha256(data).hexdigest() != artifact["sha256"]:
                raise RuntimeError(f"Dependency checksum mismatch: {path.name}")
            with path.open("xb") as output:
                output.write(data)
        if hashlib.sha256(path.read_bytes()).hexdigest() != artifact["sha256"]:
            raise RuntimeError(f"Dependency checksum mismatch: {path.name}; preserving existing file")
        files.append(path)
    return files


def compile_java(javac, paths, target, classpath):
    build_root = (ROOT / "target/paper-bridge").resolve()
    if target.is_symlink() or not target.resolve().is_relative_to(build_root) or target.resolve() == build_root:
        raise RuntimeError(f"Unsafe compiler output path: {target}")
    if target.exists():
        shutil.rmtree(target)
    target.mkdir(parents=True, exist_ok=True)
    run(javac, "--release", "25", "-encoding", "UTF-8", "-Xlint:all", "-Werror", "-cp", os.pathsep.join(map(str, classpath)), "-d", target, *paths)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--java-home", default=os.environ.get("JAVA_HOME"), required=not bool(os.environ.get("JAVA_HOME")))
    args = parser.parse_args()
    suffix = ".exe" if os.name == "nt" else ""
    java, javac, jar = [Path(args.java_home) / "bin" / (tool + suffix) for tool in ("java", "javac", "jar")]
    libraries = dependencies()
    build = ROOT / "target/paper-bridge"
    classes, fixtures, tests = [build / folder for folder in ("classes", "fixtures", "tests")]
    compile_java(javac, sorted((SOURCE / "src").rglob("*.java")), classes, libraries)
    (classes / "zerqon-paper-build.properties").write_text("build-time=" + datetime.now(timezone.utc).isoformat().replace("+00:00", "Z") + "\n", encoding="ascii")
    bridge = build / "zerqon-paper-bridge-0.0.1.jar"
    bridge.parent.mkdir(parents=True, exist_ok=True)
    run(jar, "--create", "--file", bridge, "-C", classes, ".")
    if (SOURCE / "resources").exists():
        run(jar, "--update", "--file", bridge, "-C", SOURCE / "resources", ".")
    compile_java(javac, sorted((SOURCE / "tests/fixtures").rglob("*.java")), fixtures, libraries)
    probe = build / "paper-probe.jar"
    failed = build / "failing-probe.jar"
    base = build / "base-probe.jar"
    dependent = build / "dependent-probe.jar"
    for path, name, main_class in [(probe, "PaperProbe", "PaperProbe"), (failed, "FailingProbe", "FailingProbe"), (base, "BaseProbe", "BaseProbe"), (dependent, "DependentProbe", "DependentProbe")]:
        with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
            for file in fixtures.rglob("*.class"):
                if file.stem == main_class or file.stem.startswith(main_class + "$"):
                    archive.write(file, file.relative_to(fixtures).as_posix())
            metadata = f"name: {name}\nversion: 1.0\nmain: io.github.teighto.zerqon.fixtures.{main_class}\napi-version: '26.3'\n"
            if main_class == "PaperProbe":
                metadata += "commands:\n  probe:\n    aliases: [p]\n    permission: zerqon.probe\npermissions:\n  zerqon.probe:\n    default: op\n"
            if main_class == "DependentProbe":
                metadata += "depend: [BaseProbe]\n"
            archive.writestr("plugin.yml", metadata)
            archive.writestr("config.yml", "greeting: hello\n")
    compile_java(javac, [SOURCE / "tests/PaperBridgeTest.java", SOURCE / "tests/PaperReadModelTest.java"], tests, [*libraries, bridge])
    with tempfile.TemporaryDirectory(prefix="zerqon-paper-") as directory:
        run(java, "-cp", os.pathsep.join(map(str, [*libraries, bridge, tests])), "io.github.teighto.zerqon.paper.PaperBridgeTest", directory, probe, failed, base, dependent)
    with tempfile.TemporaryDirectory(prefix="zerqon-paper-reads-") as directory:
        run(java, "-cp", os.pathsep.join(map(str, [*libraries, bridge, tests])), "io.github.teighto.zerqon.paper.PaperReadModelTest", directory)
    output = ROOT / "dist" / bridge.name
    output.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(bridge, output)
    print(f"Built {output}")


if __name__ == "__main__":
    main()
