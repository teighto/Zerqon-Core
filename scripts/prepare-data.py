"""Prepare local Minecraft protocol data; the game JAR is never redistributed."""

import argparse
import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import urllib.request
import zipfile
from pathlib import Path, PurePosixPath

VERSION = "26.3"
PROTOCOL = 777
SERVER_SHA1 = "33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c"
SERVER_URL = f"https://piston-data.mojang.com/v1/objects/{SERVER_SHA1}/server.jar"
REGISTRIES = (
    "worldgen/biome", "chat_type", "trim_pattern", "trim_material", "wolf_variant",
    "wolf_sound_variant", "pig_variant", "pig_sound_variant", "frog_variant", "cat_variant",
    "cat_sound_variant", "cow_sound_variant", "cow_variant", "chicken_sound_variant",
    "chicken_variant", "zombie_nautilus_variant", "painting_variant", "sulfur_cube_archetype",
    "dimension_type", "damage_type", "banner_pattern", "enchantment", "jukebox_song",
    "instrument", "test_environment", "test_instance", "dialog", "world_clock", "timeline",
    "decorated_pot_pattern", "block_transformer", "worldgen/block_state_provider",
)


def verified_file(path: Path, expected_sha1: str) -> bool:
    if not path.is_file():
        return False
    digest = hashlib.sha1()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest() == expected_sha1


def registry_entries(root: Path, registry: str):
    registry_namespace, registry_path = registry.split(":", 1)
    directory = registry_path if registry_namespace == "minecraft" else f"{registry_namespace}/{registry_path}"
    entries = []
    for namespace in root.iterdir():
        location = namespace / directory
        if not namespace.is_dir() or not location.is_dir():
            continue
        for path in location.rglob("*.json"):
            relative = path.relative_to(location).as_posix()
            entries.append((relative, namespace.name, json.loads(path.read_text(encoding="utf-8"))))
    entries.sort(key=lambda entry: (entry[0], entry[1]))
    return [{"id": f"{namespace}:{path[:-5]}", "data": data} for path, namespace, data in entries]


def resolve_tags(definitions: dict, identifiers: dict):
    resolved = {}
    visiting = set()

    def resolve(name):
        if name in resolved:
            return resolved[name]
        if name in visiting:
            raise ValueError(f"Tag reference cycle at {name}")
        visiting.add(name)
        values = []
        for entry in definitions[name]:
            identifier = entry["id"] if isinstance(entry, dict) else entry
            required = entry.get("required", True) if isinstance(entry, dict) else True
            nested = identifier.startswith("#")
            key = identifier[1:] if nested else identifier
            key = key if ":" in key else f"minecraft:{key}"
            if key not in (definitions if nested else identifiers):
                if required:
                    raise ValueError(f"Missing required entry {identifier} in tag {name}")
                continue
            for value in resolve(key) if nested else [identifiers[key]]:
                if value not in values:
                    values.append(value)
        visiting.remove(name)
        resolved[name] = values
        return values

    for name in definitions:
        resolve(name)
    return resolved


def collect_tags(data: Path, builtin: dict, registries: list):
    identifiers = {
        name: {key: value["protocol_id"] for key, value in registry["entries"].items()}
        for name, registry in builtin.items()
    }
    identifiers.update({
        registry["name"]: {entry["id"]: index for index, entry in enumerate(registry["entries"])}
        for registry in registries
    })
    result = []
    for registry, ids in sorted(identifiers.items()):
        namespace, relative = registry.split(":", 1)
        registry_path = relative if namespace == "minecraft" else f"{namespace}/{relative}"
        definitions = {}
        for directory in sorted(data.iterdir()):
            root = directory / "tags" / registry_path
            for path in sorted(root.rglob("*.json")):
                name = f"{directory.name}:{path.relative_to(root).as_posix()[:-5]}"
                definitions[name] = json.loads(path.read_text(encoding="utf-8"))["values"]
        if definitions:
            tags = resolve_tags(definitions, ids)
            result.append({"name": registry, "tags": [{"name": name, "ids": ids} for name, ids in sorted(tags.items())]})
    return result


def fetch_server(destination: Path):
    if verified_file(destination, SERVER_SHA1):
        return
    temporary = destination.with_suffix(".download")
    try:
        request = urllib.request.Request(SERVER_URL, headers={"User-Agent": "Zerqon-data-preparer/0.0.1"})
        with urllib.request.urlopen(request, timeout=60) as response, temporary.open("wb") as target:
            shutil.copyfileobj(response, target, 1024 * 1024)
        if not verified_file(temporary, SERVER_SHA1):
            raise ValueError("Downloaded Minecraft server failed its pinned SHA-1 check")
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)


def extract_data(jar: Path, destination: Path):
    with zipfile.ZipFile(jar) as outer:
        version = json.loads(outer.read("version.json"))
        if version.get("id") != VERSION or version.get("protocol_version") != PROTOCOL:
            raise ValueError("Minecraft version/protocol does not match Zerqon's data contract")
        nested_name = f"META-INF/versions/{VERSION}/server-{VERSION}.jar"
        with zipfile.ZipFile(io.BytesIO(outer.read(nested_name))) as inner:
            for entry in inner.infolist():
                path = PurePosixPath(entry.filename)
                if path.is_absolute() or ".." in path.parts or "\\" in entry.filename:
                    raise ValueError("Unsafe archive path")
                if len(path.parts) < 4 or path.parts[0] != "data" or path.suffix != ".json":
                    continue
                relative = "/".join(path.parts[2:])
                if not relative.startswith("tags/") and not any(relative.startswith(f"{registry}/") for registry in REGISTRIES):
                    continue
                target = destination.joinpath(*path.parts[1:])
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(inner.read(entry))
    return version


def prepare(output: Path, java: str, server_jar: Path | None):
    output.mkdir(parents=True, exist_ok=True)
    jar = output / "server.jar"
    if server_jar is not None:
        if not verified_file(server_jar, SERVER_SHA1):
            raise ValueError("--server-jar must be the unmodified Mojang Minecraft 26.3 server JAR")
        if server_jar.resolve() != jar.resolve():
            shutil.copyfile(server_jar, jar)
    else:
        fetch_server(jar)
    generated = output / "generated"
    report = generated / "reports"
    identity = output / "prepared.json"
    previous = json.loads(identity.read_text(encoding="utf-8")) if identity.exists() else {}
    if previous.get("server_sha1") != SERVER_SHA1 or not all(
        (report / name).is_file() for name in ("packets.json", "registries.json", "blocks.json")
    ):
        subprocess.run([
            java, "-DbundlerMainClass=net.minecraft.data.Main", "-jar", str(jar),
            "--reports", "--output", str(generated),
        ], cwd=output, check=True)
    data = output / "data"
    data.mkdir(exist_ok=True)
    version = extract_data(jar, data)
    registries = [
        {"name": f"minecraft:{name}", "entries": registry_entries(data, f"minecraft:{name}")}
        for name in REGISTRIES
    ]
    payload = {"version": VERSION, "protocol": PROTOCOL, "registries": registries}
    (output / "registries-sync.json").write_text(json.dumps(payload, separators=(",", ":")), encoding="utf-8")
    builtin = json.loads((report / "registries.json").read_text(encoding="utf-8"))
    tags = collect_tags(data, builtin, registries)
    (output / "tags-sync.json").write_text(json.dumps({"registries": tags}, separators=(",", ":")), encoding="utf-8")
    prepared = {
        "version": VERSION, "protocol": PROTOCOL, "server_sha1": SERVER_SHA1,
        "world_version": version["world_version"], "java_major": version["java_version"],
        "registries": len(registries), "entries": sum(len(value["entries"]) for value in registries),
        "tag_registries": len(tags), "tags": sum(len(value["tags"]) for value in tags),
    }
    identity.write_text(json.dumps(prepared, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"data_directory": str(output), **prepared}, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path(".cache/vanilla/26.3"))
    parser.add_argument("--java", default="java", help="Path to a Java 25+ runtime")
    parser.add_argument("--server-jar", type=Path, help="Use an already downloaded official 26.3 server JAR")
    args = parser.parse_args()
    try:
        prepare(args.output.resolve(), args.java, args.server_jar)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError, zipfile.BadZipFile) as error:
        print(f"Data preparation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
