import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import zlib

ROOT = Path(__file__).resolve().parents[2]
BUNDLE_SHA1 = "33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c"
SERVER_SHA256 = "a362163eec5d1612d520772bc16e5b39c09e3b234fdc045f56bf544284ee8ae6"


def checked_hash(path, algorithm, expected):
    digest = hashlib.new(algorithm)
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(65536), b""):
            digest.update(chunk)
    if digest.hexdigest() != expected:
        raise ValueError(f"Official {path.name} checksum mismatch")


def main():
    parser = argparse.ArgumentParser(description="Reproduce Zerqon's Minecraft 26.3 block facts using the original server")
    parser.add_argument("--data", type=Path, default=ROOT / ".cache/vanilla/26.3")
    parser.add_argument("--java", default="java")
    parser.add_argument("--javac", default="javac")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    data = args.data.resolve()
    server = data / "versions/26.3/server-26.3.jar"
    checked_hash(data / "server.jar", "sha1", BUNDLE_SHA1)
    checked_hash(server, "sha256", SERVER_SHA256)
    with tempfile.TemporaryDirectory(prefix="zerqon-block-data-") as temporary:
        stage = Path(temporary)
        source = stage / "BlockDataExporter.java"
        shutil.copyfile(Path(__file__).with_name(source.name), source)
        classpath = os.pathsep.join(map(str, [stage, server, *sorted((data / "libraries").rglob("*.jar"))]))
        subprocess.run([args.javac, "--release", "25", "-encoding", "UTF-8", "-classpath", classpath, "-d", str(stage), str(source)], check=True)
        subprocess.run([args.java, "-Dsun.stdout.encoding=UTF-8", "-Dsun.stderr.encoding=UTF-8", "-classpath", classpath, "BlockDataExporter", "facts.json", str(server)], cwd=stage, check=True)
        raw = json.loads((stage / "facts.json").read_text(encoding="utf-8"))
    blocks, states, items = raw["blocks"], raw["states"], raw["items"]
    if len(states) != 35723 or len(items) != 1658:
        raise ValueError("Unexpected official registry counts")
    for values in [blocks, states, items]:
        if [entry["id"] for entry in values] != list(range(len(values))):
            raise ValueError("Registry IDs are not complete and ordered")
    definitions = [[] for _ in blocks]
    for state in states:
        definitions[state["block"]].append(state)
    compact_blocks = []
    indexes = []
    for block, entries in zip(blocks, definitions):
        properties = [(key, sorted({state["properties"][key] for state in entries})) for key in sorted(entries[0]["properties"])]
        indexes.append(properties)
        compact_blocks.append([block["key"], block["default"], block["family"], block["flags"], block["item"], properties])
    shapes, shape_ids, compact_states = [], {}, []
    for state in states:
        shape_key = json.dumps(state["collision"], separators=(",", ":"))
        if shape_key not in shape_ids:
            shape_ids[shape_key] = len(shapes)
            shapes.append(state["collision"])
        code, radix = 0, 1
        for name, values in indexes[state["block"]]:
            code += values.index(state["properties"][name]) * radix
            radix *= len(values)
        compact_states.append([state["block"], code, shape_ids[shape_key], state["flags"]])
    compact = {"schema": 1, "minecraft": "26.3", "protocol": 777, "server_sha256": SERVER_SHA256,
               "blocks": compact_blocks, "states": compact_states, "items": [[item["key"], item["block"], item["ordinary"], item["creative_destroy"] if item["creative_destroy_supported"] else None] for item in items], "shapes": shapes}
    canonical = json.dumps(compact, separators=(",", ":"), ensure_ascii=True).encode("ascii")
    compressed = zlib.compress(canonical, 9)
    target = ROOT / "crates/zerqon-blocks/assets/26.3.json.zlib"
    fixtures = ROOT / "crates/zerqon-blocks/tests/fixtures/stair-shapes.json"
    fixture_bytes = (json.dumps(raw["stair_cases"], separators=(",", ":")) + "\n").encode("ascii")
    snow_fixtures = fixtures.with_name("snowy-neighbors.json")
    snow_bytes = (json.dumps(raw["snow_cases"], separators=(",", ":")) + "\n").encode("ascii")
    if args.check:
        stored = target.read_bytes()
        if hashlib.sha256(stored).hexdigest() != target.with_suffix(".sha256").read_text(encoding="ascii").strip():
            raise ValueError("Embedded asset checksum mismatch")
        if zlib.decompress(stored) != canonical or fixtures.read_bytes() != fixture_bytes or snow_fixtures.read_bytes() != snow_bytes:
            raise ValueError("Embedded block metadata or stair fixtures differ from the official server")
    else:
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(compressed)
        target.with_suffix(".sha256").write_text(hashlib.sha256(compressed).hexdigest() + "\n", encoding="ascii")
        fixtures.parent.mkdir(parents=True, exist_ok=True)
        fixtures.write_bytes(fixture_bytes)
        snow_fixtures.write_bytes(snow_bytes)
    print(f"Verified {len(blocks)} blocks, {len(states)} states, {len(items)} items, {len(shapes)} shapes; asset {len(compressed)} bytes")


if __name__ == "__main__":
    main()
