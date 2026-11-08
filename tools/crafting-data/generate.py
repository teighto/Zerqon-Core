"""Export crafting remainder facts and check recipes against pinned Minecraft 26.3."""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
BUNDLE_SHA1 = "33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c"
SERVER_SHA256 = "a362163eec5d1612d520772bc16e5b39c09e3b234fdc045f56bf544284ee8ae6"
MAX_JSON_BYTES = 1_048_576
KEY = re.compile(r"minecraft:[a-z0-9_./-]+\Z")


def checked_hash(path, algorithm, expected):
    digest = hashlib.new(algorithm)
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(65_536), b""):
            digest.update(chunk)
    if digest.hexdigest() != expected:
        raise ValueError(f"Official {path.name} checksum mismatch")


def run(command, **options):
    result = subprocess.run(command, timeout=120, **options)
    if result.returncode:
        raise SystemExit(f"{Path(command[0]).name} failed with exit code {result.returncode}")


def checked_inputs(data):
    bundle = data / "server.jar"
    server = data / "versions/26.3/server-26.3.jar"
    checked_hash(bundle, "sha1", BUNDLE_SHA1)
    checked_hash(server, "sha256", SERVER_SHA256)
    with zipfile.ZipFile(bundle) as archive:
        entry = archive.getinfo("META-INF/libraries.list")
        if entry.file_size > 131_072:
            raise ValueError("Official library manifest exceeds 128 KiB")
        rows = archive.read(entry).decode("utf-8").splitlines()
    if not 1 <= len(rows) <= 256:
        raise ValueError("Unexpected official library count")
    libraries = []
    root = (data / "libraries").resolve()
    for row in rows:
        checksum, _coordinate, relative = row.split("\t")
        path = PurePosixPath(relative)
        if not re.fullmatch(r"[0-9a-f]{64}", checksum) or path.is_absolute() or ".." in path.parts or "\\" in relative or path.suffix != ".jar":
            raise ValueError("Invalid official library manifest entry")
        local = root.joinpath(*path.parts).resolve()
        if not local.is_relative_to(root) or local in libraries:
            raise ValueError("Duplicate or escaping official library path")
        checked_hash(local, "sha256", checksum)
        libraries.append(local)
    return server, sorted(libraries)


def canonical_facts(raw):
    if (set(raw) != {"minecraft", "protocol", "data_version", "item_count", "remainders"}
            or raw["minecraft"] != "26.3" or raw["protocol"] != 777 or raw["data_version"] != 5023
            or raw["item_count"] != 1658):
        raise ValueError("Unexpected official item registry")
    rows = raw["remainders"]
    if not isinstance(rows, list) or not 1 <= len(rows) <= 1658:
        raise ValueError("Unexpected crafting remainder count")
    for row in rows:
        if (not isinstance(row, list) or len(row) != 3
                or not all(isinstance(value, str) and KEY.fullmatch(value) for value in row[:2])
                or "minecraft:air" in row[:2]
                or type(row[2]) is not int or not 1 <= row[2] <= 99):
            raise ValueError("Invalid crafting remainder record")
    keys = [row[0] for row in rows]
    if keys != sorted(set(keys)):
        raise ValueError("Crafting remainders must be unique and lexically ordered")
    result = {"schema": 1, "minecraft": raw["minecraft"], "protocol": raw["protocol"], "data_version": raw["data_version"],
              "bundle_sha1": BUNDLE_SHA1, "server_sha256": SERVER_SHA256,
              "item_count": raw["item_count"], "remainders": rows}
    return (json.dumps(result, separators=(",", ":"), ensure_ascii=True) + "\n").encode("ascii")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data", type=Path, default=ROOT / ".cache/vanilla/26.3")
    parser.add_argument("--java", default="java")
    parser.add_argument("--javac", default="javac")
    parser.add_argument("--fixtures", type=Path, default=HERE / "fixtures.json")
    parser.add_argument("--output", type=Path, default=HERE / "26.3.json")
    parser.add_argument("--oracle-output", type=Path, help="Defaults to oracle.json next to the facts output")
    parser.add_argument("--report", type=Path)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    fixtures = args.fixtures.resolve()
    with fixtures.open("rb") as stream:
        fixture_bytes = stream.read(MAX_JSON_BYTES + 1)
    if len(fixture_bytes) > MAX_JSON_BYTES:
        raise ValueError("Fixture exceeds 1 MiB")
    output = args.output.resolve()
    oracle_output = args.oracle_output.resolve() if args.oracle_output else output.with_name("oracle.json")
    destinations = [output, oracle_output, *([args.report.resolve()] if args.report else [])]
    if len(set(destinations)) != len(destinations) or fixtures in destinations:
        raise ValueError("Output files must be distinct and must not overwrite input fixtures")
    server, libraries = checked_inputs(args.data.resolve())
    cache = ROOT / ".cache/crafting-data"
    cache.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="run-", dir=cache) as temporary:
        stage = Path(temporary)
        staged_fixtures = stage / "fixtures.json"
        staged_fixtures.write_bytes(fixture_bytes)
        classpath = os.pathsep.join(map(str, [stage, server, *libraries]))
        run([args.javac, "--release", "25", "-encoding", "UTF-8", "-Xlint:all,-classfile", "-Werror",
                        "-classpath", classpath, "-d", str(stage), str(HERE / "CraftingDataExporter.java"),
                        str(HERE / "CraftingRecipeOracle.java")])
        run([args.java, "-Dfile.encoding=UTF-8", "-Dsun.stdout.encoding=UTF-8", "-Dsun.stderr.encoding=UTF-8",
                        "-classpath", classpath, "CraftingDataExporter", str(server), str(staged_fixtures),
                        "facts.json", "report.json"], cwd=stage)
        if (stage / "facts.json").stat().st_size > MAX_JSON_BYTES or (stage / "report.json").stat().st_size > MAX_JSON_BYTES:
            raise ValueError("Exporter output exceeds 1 MiB")
        canonical = canonical_facts(json.loads((stage / "facts.json").read_text(encoding="utf-8")))
        report = json.loads((stage / "report.json").read_text(encoding="utf-8"))
    report.update(schema=1, minecraft="26.3", protocol=777, data_version=5023, bundle_sha1=BUNDLE_SHA1,
                  server_sha256=SERVER_SHA256, fixture_sha256=hashlib.sha256(fixture_bytes).hexdigest())
    oracle_bytes = (json.dumps(report, sort_keys=True, separators=(",", ":"), ensure_ascii=True) + "\n").encode("ascii")
    if len(canonical) > MAX_JSON_BYTES or len(oracle_bytes) > MAX_JSON_BYTES:
        raise ValueError("Canonical exporter output exceeds 1 MiB")
    if args.check:
        if output.stat().st_size > MAX_JSON_BYTES:
            raise ValueError("Stored crafting asset exceeds limit")
        stored = output.read_bytes()
        if stored != canonical:
            raise ValueError("Crafting facts differ from the official server")
        if oracle_output.stat().st_size > MAX_JSON_BYTES or oracle_output.read_bytes() != oracle_bytes:
            raise ValueError("Crafting oracle export differs from the official server")
    else:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_bytes(canonical)
        oracle_output.parent.mkdir(parents=True, exist_ok=True)
        oracle_output.write_bytes(oracle_bytes)
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_bytes(oracle_bytes)
    print(f"Verified {json.loads(canonical)['item_count']} items, {len(json.loads(canonical)['remainders'])} crafting remainders, "
          f"{report['item_tags']} item tags, {len(report['cases'])} original recipe cases, {len(report['codec_cases'])} custom codec cases "
          f"and {len(report['strict_rejections'])} intentional Rust rejection cases")


if __name__ == "__main__":
    main()
