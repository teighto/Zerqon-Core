import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
SERVER_SHA256 = "a362163eec5d1612d520772bc16e5b39c09e3b234fdc045f56bf544284ee8ae6"


def run(command):
    result = subprocess.run([str(value) for value in command], cwd=ROOT, timeout=120)
    if result.returncode:
        raise SystemExit(f"{Path(command[0]).name} failed with exit code {result.returncode}")


def main():
    parser = argparse.ArgumentParser(description="Verify inventory bytes with the original Minecraft 26.3 codecs")
    parser.add_argument("--data", type=Path, default=ROOT / ".cache/vanilla/26.3")
    parser.add_argument("--fixtures", type=Path, default=ROOT / "tests/inventory_oracle/fixtures.json")
    parser.add_argument("--export", type=Path)
    parser.add_argument("--export-default-items", action="store_true")
    parser.add_argument("--export-equipment", action="store_true")
    parser.add_argument("--java", default="java")
    parser.add_argument("--javac", default="javac")
    args = parser.parse_args()
    data = args.data.resolve()
    metadata = json.loads((data / "prepared.json").read_text(encoding="utf-8"))
    if metadata.get("version") != "26.3" or metadata.get("protocol") != 777:
        raise SystemExit("Expected prepared Minecraft 26.3 protocol 777 data")
    server = data / "versions/26.3/server-26.3.jar"
    if not server.is_file() or hashlib.sha256(server.read_bytes()).hexdigest() != SERVER_SHA256:
        raise SystemExit("Official Minecraft 26.3 server checksum mismatch; run scripts/prepare-data.py")
    output = ROOT / ".cache/inventory-oracle/classes"
    output.mkdir(parents=True, exist_ok=True)
    classpath = os.pathsep.join(map(str, [output, server, *sorted((data / "libraries").rglob("*.jar"))]))
    run([args.javac, "--release", "25", "-encoding", "UTF-8", "-Xlint:all,-classfile", "-Werror", "-classpath", classpath, "-d", output, ROOT / "tests/inventory_oracle/InventoryCodecOracle.java"])
    command = [args.java, "-classpath", classpath, "InventoryCodecOracle", data, args.fixtures.resolve()]
    if args.export is not None:
        command.extend(["--export", args.export.resolve()])
    if args.export_default_items:
        if args.export is None:
            parser.error("--export-default-items requires --export")
        command.append("--export-default-items")
    if args.export_equipment:
        if args.export is None:
            parser.error("--export-equipment requires --export")
        command.append("--export-equipment")
    run(command)


if __name__ == "__main__":
    main()
