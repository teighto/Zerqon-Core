import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
SHA256 = "a362163eec5d1612d520772bc16e5b39c09e3b234fdc045f56bf544284ee8ae6"


def main():
    parser = argparse.ArgumentParser(description="Check static mob collision against original Minecraft 26.3 Entity.collide")
    parser.add_argument("--data", type=Path, default=ROOT / ".cache/vanilla/26.3")
    parser.add_argument("--java", default="java")
    parser.add_argument("--javac", default="javac")
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    data = args.data.resolve()
    server = data / "versions/26.3/server-26.3.jar"
    if hashlib.sha256(server.read_bytes()).hexdigest() != SHA256:
        raise SystemExit("Expected pinned official Minecraft 26.3 server")
    output = ROOT / ".cache/entity-physics"
    output.mkdir(parents=True, exist_ok=True)
    classpath = os.pathsep.join(map(str, [output, server, *sorted((data / "libraries").rglob("*.jar"))]))
    sources = [ROOT / "tests/inventory_oracle/InventoryCodecOracle.java", *sorted((ROOT / "tools/entity-physics").glob("*.java"))]
    commands = [[args.javac,"--release","25","-encoding","UTF-8","-Xlint:all,-classfile","-Werror","-classpath",classpath,"-d",output,*sources],
                [args.java,"-Dsun.stdout.encoding=UTF-8","-Dsun.stderr.encoding=UTF-8","-classpath",classpath,"EntityPhysicsOracle",data,output / "fixtures.json"]]
    for command in commands:
        result = subprocess.run([str(value) for value in command], cwd=ROOT, timeout=120)
        if result.returncode:
            raise SystemExit(f"{Path(command[0]).name} failed: {result.returncode}")
    generated = json.loads((output / "fixtures.json").read_text())
    target = ROOT / "crates/zerqon-entities/tests/fixtures/collision-26.3.json"
    if args.write:
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(json.dumps(generated, separators=(",", ":")) + "\n", encoding="utf-8")
    elif json.loads(target.read_text()) != generated:
        raise SystemExit("Original collision fixtures changed")
    print(f"Verified {len(generated['cases'])} original collision and step cases")


if __name__ == "__main__":
    main()
