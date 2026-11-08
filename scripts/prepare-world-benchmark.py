"""Freeze imported native payloads for an untimed benchmark preflight."""

import argparse
import hashlib
import json
from pathlib import Path
import struct
import zlib


def bounded(path, limit):
    if path.is_symlink() or not path.is_file() or path.stat().st_size > limit:
        raise ValueError(f"not a bounded regular file: {path}")
    with path.open("rb") as source:
        data = source.read(limit + 1)
    if len(data) > limit:
        raise ValueError("input grew beyond its limit")
    return data


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--world", type=Path, required=True)
    selection = parser.add_mutually_exclusive_group(required=True)
    selection.add_argument("--coordinates", type=Path, help="explicit JSON array [[x,z],...] from the completed normal fixture")
    selection.add_argument("--rectangle", nargs=4, type=int, metavar=("MIN_X", "MIN_Z", "MAX_X", "MAX_Z"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = args.world.resolve(strict=True)
    metadata = json.loads(bounded(root / "world-metadata.json", 8 * 1024 * 1024))
    source = metadata["source"]
    if metadata["generation_policy"] != "existing_only" or source["minecraft_version"] != "26.3" or source["data_version"] != 5023:
        raise ValueError("requires an imported Minecraft 26.3 world with ExistingOnly storage")
    if metadata["generator"]["dimensions"]["minecraft:overworld"]["generator_kind"] != "minecraft:noise":
        raise ValueError("flat or unknown generator cannot be labelled ordinary terrain")
    if args.rectangle is not None:
        min_x, min_z, max_x, max_z = args.rectangle
        if any(not -(2**31) <= value < 2**31 for value in args.rectangle) or min_x > max_x or min_z > max_z:
            raise ValueError("invalid rectangle bounds")
        if (max_x - min_x + 1) * (max_z - min_z + 1) > 4096:
            raise ValueError("rectangle exceeds 4096 selected chunks")
        coordinates = [[x, z] for x in range(min_x, max_x + 1) for z in range(min_z, max_z + 1)]
    else:
        coordinates = json.loads(bounded(args.coordinates, 1024 * 1024))
    if not isinstance(coordinates, list) or not 1 <= len(coordinates) <= 4096:
        raise ValueError("coordinate selection must contain 1..4096 entries")
    seen = set()
    for pair in coordinates:
        if not isinstance(pair, list) or len(pair) != 2 or any(type(value) is not int or not -(2**31) <= value < 2**31 for value in pair):
            raise ValueError("invalid coordinate entry")
        key = tuple(pair)
        if key in seen:
            raise ValueError("duplicate coordinate")
        seen.add(key)
    chunks = []
    for x, z in sorted(seen):
        data = bounded(root / f"c.{x}.{z}.zqc", 1024 * 1024)
        if len(data) < 26 or data[:8] != b"ZERQON\0\x01":
            raise ValueError("invalid native chunk signature")
        version, stored_x, stored_z, payload_length, checksum = struct.unpack_from("<HiiII", data, 8)
        crc = zlib.crc32(data[26:], zlib.crc32(data[:22])) & 0xffffffff
        if version != 1 or (stored_x, stored_z) != (x, z) or payload_length != len(data) - 26 or checksum != crc:
            raise ValueError(f"invalid native chunk header/CRC: ({x},{z})")
        chunks.append({"x": x, "z": z, "encoded_bytes": len(data), "encoded_sha256": hashlib.sha256(data).hexdigest()})
    selection = {
        "schema": 1, "minecraft": "26.3", "data_version": 5023,
        "world_id": metadata["world_id"], "seed": metadata["generator"]["seed"],
        "generator_kind": "minecraft:noise", "generator_settings": "minecraft:overworld",
        "level_dat_sha256": source["level_dat_sha256"], "generator_sha256": source["generator_sha256"],
        "retained_manifest_sha256": metadata["retained_source"]["manifest_sha256"], "chunks": chunks,
    }
    with args.output.open("x", encoding="utf-8", newline="\n") as output:
        json.dump(selection, output, indent=2)
        output.write("\n")
    print(f"Frozen {len(chunks)} native payloads; Rust preflight must verify actual normal-generator metadata and loaded bodies before timing.")


if __name__ == "__main__":
    main()
