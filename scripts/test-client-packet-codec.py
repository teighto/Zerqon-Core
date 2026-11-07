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
    parser = argparse.ArgumentParser(description="Check handled Minecraft 26.3 client packets against original codecs")
    parser.add_argument("--data", type=Path, default=ROOT / ".cache/vanilla/26.3")
    parser.add_argument("--fixtures", type=Path, default=ROOT / "tests/client_packet_oracle/fixtures.json")
    parser.add_argument("--export", type=Path)
    parser.add_argument("--java", default="java")
    parser.add_argument("--javac", default="javac")
    args = parser.parse_args()
    data = args.data.resolve()
    metadata = json.loads((data / "prepared.json").read_text(encoding="utf-8"))
    server = data / "versions/26.3/server-26.3.jar"
    if metadata.get("version") != "26.3" or metadata.get("protocol") != 777 or hashlib.sha256(server.read_bytes()).hexdigest() != SERVER_SHA256:
        raise SystemExit("Expected pinned official Minecraft 26.3 protocol 777 data")
    output = ROOT / ".cache/client-packet-oracle/classes"
    output.mkdir(parents=True, exist_ok=True)
    classpath = os.pathsep.join(map(str, [output, server, *sorted((data / "libraries").rglob("*.jar"))]))
    run([args.javac, "--release", "25", "-encoding", "UTF-8", "-Xlint:all,-classfile", "-Werror", "-classpath", classpath, "-d", output,
         ROOT / "tests/inventory_oracle/InventoryCodecOracle.java", ROOT / "tests/client_packet_oracle/ClientPacketOracle.java"])
    command = [args.java, "-Dsun.stdout.encoding=UTF-8", "-Dsun.stderr.encoding=UTF-8", "-classpath", classpath, "ClientPacketOracle", data, args.fixtures.resolve()]
    if args.export is not None:
        command.extend(["--export", args.export.resolve()])
    run(command)


if __name__ == "__main__":
    main()
