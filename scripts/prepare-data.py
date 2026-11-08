"""Prepare local Minecraft protocol data; the game JAR is never redistributed."""

import argparse
import contextlib
import errno
import hashlib
import io
import json
import math
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
import zipfile
from pathlib import Path

VERSION = "26.3"
PROTOCOL = 777
SERVER_SHA1 = "33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c"
SERVER_BYTES = 62_294_556
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
RECIPE_LIMIT = 4096
JSON_LIMIT = 1024 * 1024
RECIPE_BYTES_LIMIT = 16 * 1024 * 1024
REPORTS = ("packets.json", "registries.json", "blocks.json")
PREPARE_LOCK_TIMEOUT = 600
# Keep aligned with zerqon_inventory::catalog::supported_component: these defaults
# are retained by ItemCatalog and therefore must resolve through its registry.
SUPPORTED_COMPONENTS = frozenset((
    "minecraft:max_stack_size", "minecraft:max_damage", "minecraft:damage",
    "minecraft:repair_cost", "minecraft:unbreakable", "minecraft:custom_name",
    "minecraft:item_name", "minecraft:item_model", "minecraft:rarity",
    "minecraft:enchantment_glint_override", "minecraft:block_state",
))


def is_link(path: Path):
    metadata = path.lstat()
    return path.is_symlink() or bool(getattr(metadata, "st_file_attributes", 0) & 0x400)


def reject_links(path: Path):
    for ancestor in (path, *path.parents):
        if ancestor.exists() or ancestor.is_symlink():
            if is_link(ancestor):
                raise ValueError("Cache path contains a link or reparse point")


@contextlib.contextmanager
def prepare_lock(output: Path):
    path = output / "prepare.lock"
    reject_links(path)
    if os.name == "nt":
        import msvcrt

        def acquire(handle):
            handle.seek(0)
            msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)

        def release(handle):
            handle.seek(0)
            msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
    else:
        import fcntl

        def acquire(handle):
            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)

        def release(handle):
            fcntl.flock(handle, fcntl.LOCK_UN)

    # Windows byte zero overlaps Rust File::try_lock's exclusive whole-file range;
    # Unix flock is the same primitive used by the native preparer.
    with path.open("a+b") as handle:
        deadline = time.monotonic() + PREPARE_LOCK_TIMEOUT
        acquired = False
        try:
            while not acquired:
                try:
                    acquire(handle)
                    acquired = True
                except OSError as error:
                    if error.errno not in (errno.EACCES, errno.EAGAIN, errno.EDEADLK):
                        raise
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise TimeoutError("Timed out waiting for Minecraft prepare.lock") from error
                    time.sleep(min(0.1, remaining))
            yield
        finally:
            if acquired:
                release(handle)


def safe_archive_path(name: str):
    if name.startswith("/") or "\\" in name:
        raise ValueError("Unsafe archive path")
    parts = name.rstrip("/").split("/")
    for part in parts:
        base = part.split(".", 1)[0].upper()
        if (not part or part in (".", "..") or any(c in ':<>"|?*' or ord(c) < 32 for c in part)
                or part.endswith((".", " ")) or base in ("CON", "NUL", "PRN", "AUX")
                or re.fullmatch(r"(?:COM|LPT)[1-9]", base)):
            raise ValueError("Unsafe archive path component")
    return parts


def validate_recipe(path: str, raw: bytes):
    parts = safe_archive_path(path)
    if (len(parts) < 3 or parts[1] != "recipe" or not path.endswith(".json")
            or len(path) > 256 or not re.fullmatch(r"[a-z0-9_.-]+", parts[0])
            or not re.fullmatch(r"[a-z0-9_./-]+", "/".join(parts[2:]))):
        raise ValueError("Invalid recipe path")
    value = json.loads(raw)
    if (not isinstance(value, dict) or not isinstance(value.get("type"), str)
            or not re.fullmatch(r"[a-z0-9_.-]+:[a-z0-9_./-]+", value["type"])):
        raise ValueError("Invalid recipe JSON type")


def recipe_paths(data: Path):
    def fail_walk(error):
        raise error

    paths = []
    reject_links(data)
    namespaces = list(data.iterdir())
    if len(namespaces) > RECIPE_LIMIT:
        raise ValueError("Too many data namespaces")
    directory_count = 0
    for namespace in sorted(namespaces):
        reject_links(namespace)
        root = namespace / "recipe"
        reject_links(root)
        if not root.exists():
            continue
        if not root.is_dir():
            raise ValueError("Recipe root must be a directory")
        for directory, directories, files in os.walk(root, followlinks=False, onerror=fail_walk):
            directory_count += 1
            if directory_count > RECIPE_LIMIT or len(Path(directory).relative_to(root).parts) > 64:
                raise ValueError("Too many or deeply nested recipe directories")
            for child in directories + files:
                path = Path(directory) / child
                if is_link(path):
                    raise ValueError("Recipe path is a link")
            paths.extend(Path(directory) / name for name in files)
            if len(paths) > RECIPE_LIMIT:
                raise ValueError("Too many recipes")
    return paths


def recipe_identity(data: Path):
    digest = hashlib.sha256()
    paths = recipe_paths(data)
    total = 0
    for path in sorted(paths, key=lambda path: path.relative_to(data).as_posix()):
        size = path.stat().st_size
        if not path.is_file():
            raise ValueError("Recipe must be a regular file")
        if size > JSON_LIMIT:
            raise ValueError("Recipe data exceeds its size limit")
        with path.open("rb") as source:
            raw = source.read(JSON_LIMIT + 1)
        total += len(raw)
        if len(raw) > JSON_LIMIT or total > RECIPE_BYTES_LIMIT:
            raise ValueError("Recipe grew beyond its size limit")
        relative = path.relative_to(data).as_posix()
        validate_recipe(relative, raw)
        # NUL-terminated path plus fixed-width SHA-256 matches the native manifest.
        digest.update(relative.encode("utf-8") + b"\0" + hashlib.sha256(raw).digest())
    if not paths:
        raise ValueError("No recipes in prepared data")
    return {"recipes": len(paths), "recipes_sha256": digest.hexdigest()}


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
    reject_links(destination)
    if verified_file(destination, SERVER_SHA1):
        return
    if destination.exists():
        raise ValueError("Cached server.jar failed its pinned checksum; preserve/remove it before retrying")
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(prefix="server-", suffix=".download", dir=destination.parent, delete=False) as target:
            temporary = Path(target.name)
            request = urllib.request.Request(SERVER_URL, headers={"User-Agent": "Zerqon-data-preparer/0.0.1"})
            with urllib.request.urlopen(request, timeout=60) as response:
                total = 0
                while block := response.read(1024 * 1024):
                    total += len(block)
                    if total > SERVER_BYTES:
                        raise ValueError("Downloaded Minecraft server exceeds its pinned size")
                    target.write(block)
            target.flush()
            os.fsync(target.fileno())
        if not verified_file(temporary, SERVER_SHA1):
            raise ValueError("Downloaded Minecraft server failed its pinned SHA-1 check")
        os.replace(temporary, destination)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def extract_data(jar: Path, destination: Path):
    def read_entry(archive, name, limit):
        entry = archive.getinfo(name)
        if entry.file_size > limit:
            raise ValueError("ZIP entry exceeds its size limit")
        with archive.open(entry) as source:
            raw = source.read(limit + 1)
        if len(raw) > limit:
            raise ValueError("Inflated ZIP entry exceeds its size limit")
        return raw

    with zipfile.ZipFile(jar) as outer:
        version = json.loads(read_entry(outer, "version.json", 65536))
        if (version.get("id") != VERSION or version.get("protocol_version") != PROTOCOL
                or version.get("world_version") != 5023 or version.get("java_version") != 25):
            raise ValueError("Minecraft version/protocol does not match Zerqon's data contract")
        nested_name = f"META-INF/versions/{VERSION}/server-{VERSION}.jar"
        with zipfile.ZipFile(io.BytesIO(read_entry(outer, nested_name, 64 * 1024 * 1024))) as inner:
            if len(inner.infolist()) > 100_000:
                raise ValueError("Server JAR contains too many entries")
            seen = set()
            total = 0
            recipe_count = 0
            recipe_bytes = 0
            for entry in inner.infolist():
                parts = safe_archive_path(entry.filename)
                if len(parts) < 4 or parts[0] != "data" or not entry.filename.endswith(".json"):
                    continue
                relative = "/".join(parts[2:])
                recipe = relative.startswith("recipe/")
                if not recipe and not relative.startswith("tags/") and not any(relative.startswith(f"{registry}/") for registry in REGISTRIES):
                    continue
                key = "/".join(parts[1:])
                if key in seen:
                    raise ValueError("Duplicate data ZIP entry")
                seen.add(key)
                if entry.file_size > JSON_LIMIT:
                    raise ValueError("Data JSON exceeds its size limit")
                with inner.open(entry) as source:
                    raw = source.read(JSON_LIMIT + 1)
                total += len(raw)
                if len(raw) > JSON_LIMIT or total > 64 * 1024 * 1024:
                    raise ValueError("Inflated JSON exceeds its size limit")
                if recipe:
                    recipe_count += 1
                    recipe_bytes += len(raw)
                    if recipe_count > RECIPE_LIMIT or recipe_bytes > RECIPE_BYTES_LIMIT:
                        raise ValueError("Recipe assets exceed their count or size limit")
                    validate_recipe(key, raw)
                else:
                    json.loads(raw)
                target = destination.joinpath(*parts[1:])
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(raw)
    return version


def report_paths(output: Path):
    report = output / "generated/reports"
    builtin = read_json(report / "registries.json", 8 * 1024 * 1024)
    paths = [Path("generated/reports") / name for name in REPORTS]
    items = builtin["minecraft:item"]["entries"]
    if not isinstance(items, dict) or not items or len(items) > 65536:
        raise ValueError("Invalid item registry")
    for key in items:
        if len(key) > 256 or not re.fullmatch(r"[a-z0-9_.-]+:[a-z0-9_./-]+", key):
            raise ValueError("Invalid item registry key")
        namespace, name = key.split(":", 1)
        relative = f"generated/reports/{namespace}/components/item/{name}.json"
        safe_archive_path(relative)
        paths.append(Path(relative))
    return paths


def read_json(path: Path, limit: int):
    reject_links(path)
    if not path.is_file() or path.stat().st_size > limit:
        raise ValueError("JSON exceeds its size limit or is not a file")
    with path.open("rb") as source:
        raw = source.read(limit + 1)
    if len(raw) > limit:
        raise ValueError("JSON grew beyond its size limit")
    def unique_members(pairs):
        result = {}
        for name, value in pairs:
            if name in result:
                raise ValueError("Duplicate JSON object member")
            result[name] = value
        return result

    def invalid_constant(value):
        raise ValueError(f"Invalid JSON constant {value}")

    def parse_integer(value):
        # serde_json treats negative zero as a float, which typed registry IDs reject.
        return -0.0 if value == "-0" else int(value)

    try:
        value = json.loads(raw.decode("utf-8"), object_pairs_hook=unique_members,
                           parse_constant=invalid_constant, parse_int=parse_integer)
    except RecursionError as error:
        raise ValueError("JSON nesting exceeds the native parser limit") from error
    pending = [(value, 0)]
    while pending:
        child, depth = pending.pop()
        if isinstance(child, (dict, list)):
            if depth >= 127:
                raise ValueError("JSON nesting exceeds the native parser limit")
            if isinstance(child, dict):
                for key in child:
                    key.encode("utf-8")
                children = child.values()
            else:
                children = child
            pending.extend((entry, depth + 1) for entry in children)
        elif isinstance(child, str):
            child.encode("utf-8")  # Reject unpaired escaped surrogates as serde_json does.
        elif type(child) in (int, float):
            try:
                finite = math.isfinite(child)
            except OverflowError:
                finite = False
            if not finite:
                raise ValueError("JSON number exceeds the native parser range")
    return value


def valid_identifier(key):
    return (isinstance(key, str) and len(key) <= 256
            and re.fullmatch(r"[a-z0-9_.-]+:[a-z0-9_./-]+", key) is not None
            and all(part not in ("", ".", "..") for part in key.split(":", 1)[1].split("/")))


def valid_item_registries(value):
    for name, limit in (("minecraft:item", 65536), ("minecraft:data_component_type", 1024)):
        entries = value[name]["entries"]
        if not isinstance(entries, dict) or len(entries) > limit or (name == "minecraft:item" and not entries):
            return False
        identifiers = set()
        for key, entry in entries.items():
            if not valid_identifier(key) or not isinstance(entry, dict):
                return False
            identifier = entry.get("protocol_id")
            if type(identifier) is not int or not 0 <= identifier <= 65535 or identifier in identifiers:
                return False
            identifiers.add(identifier)
    return True


def valid_report(relative: Path, value):
    if not isinstance(value, dict):
        return False
    if relative == Path("generated/reports/packets.json"):
        ids = (value["handshake"]["serverbound"]["minecraft:intention"]["protocol_id"],
               value["login"]["clientbound"]["minecraft:login_finished"]["protocol_id"],
               value["play"]["clientbound"]["minecraft:login"]["protocol_id"])
        return all(type(identifier) is int for identifier in ids) and ids == (0, 2, 50)
    if relative == Path("generated/reports/blocks.json"):
        return any(type(state.get("id")) is int and state["id"] == 0 and state.get("default") is True for state in value["minecraft:air"]["states"])
    if relative == Path("generated/reports/registries.json"):
        player = value["minecraft:entity_type"]["entries"]["minecraft:player"]["protocol_id"]
        return type(player) is int and 0 <= player <= 2147483647 and valid_item_registries(value)
    components = value.get("components")
    if not isinstance(components, dict):
        return False
    stack = components.get("minecraft:max_stack_size", 1)
    damage = components.get("minecraft:max_damage", 1)
    return (type(stack) is int and 1 <= stack <= 99 and type(damage) is int and 1 <= damage <= 2147483647
            and ("minecraft:max_damage" not in components or stack == 1))


def reusable_report_paths(output: Path, identity):
    if (not isinstance(identity, dict) or identity.get("version") != VERSION or identity.get("protocol") != PROTOCOL
            or identity.get("server_sha1") != SERVER_SHA1 or identity.get("world_version") != 5023
            or identity.get("java_major") != 25
            or any(type(identity.get(key)) is not int for key in ("protocol", "world_version", "java_major"))):
        return None
    try:
        report = output / "generated/reports/registries.json"
        reject_links(report)
        if not report.is_file() or report.stat().st_size > 16 * 1024 * 1024:
            return None
        paths = report_paths(output)
        builtin = read_json(report, 8 * 1024 * 1024)
        if not valid_item_registries(builtin):
            return None
        registered_components = builtin["minecraft:data_component_type"]["entries"]
        item_bytes = 0
        for relative in paths:
            source = output / relative
            reject_links(source)
            main_report = relative.parent == Path("generated/reports")
            limit = 16 * 1024 * 1024 if main_report else JSON_LIMIT
            if not source.is_file() or source.stat().st_size > limit:
                return None
            value = read_json(source, limit)
            if not valid_report(relative, value):
                return None
            if not main_report:
                if any(key in SUPPORTED_COMPONENTS and key not in registered_components for key in value["components"]):
                    return None
                item_bytes += source.stat().st_size
                if item_bytes > 32 * 1024 * 1024:
                    return None
        return paths
    except (OSError, ValueError, KeyError, TypeError, AttributeError):
        return None


def atomic_copy(source: Path, destination: Path, expected_sha1: str | None = None):
    reject_links(destination)
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as target:
            temporary = Path(target.name)
            with source.open("rb") as original:
                shutil.copyfileobj(original, target)
            target.flush()
            os.fsync(target.fileno())
        if expected_sha1 is not None and not verified_file(temporary, expected_sha1):
            raise ValueError("Copied server.jar failed its pinned checksum")
        os.replace(temporary, destination)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def prepare(output: Path, java: str, server_jar: Path | None):
    reject_links(output)
    output.mkdir(parents=True, exist_ok=True)
    with prepare_lock(output):
        prepare_locked(output, java, server_jar)


def prepare_locked(output: Path, java: str, server_jar: Path | None):
    jar = output / "server.jar"
    reject_links(jar)
    if server_jar is not None:
        if not verified_file(server_jar, SERVER_SHA1):
            raise ValueError("--server-jar must be the unmodified Mojang Minecraft 26.3 server JAR")
        if jar.exists():
            if not verified_file(jar, SERVER_SHA1):
                raise ValueError("Cached server.jar failed its pinned checksum; preserve/remove it before retrying")
        else:
            atomic_copy(server_jar, jar, expected_sha1=SERVER_SHA1)
    else:
        fetch_server(jar)
    identity = output / "prepared.json"
    reject_links(identity)
    try:
        previous = read_json(identity, 65536) if identity.exists() else {}
    except (ValueError, OSError):
        previous = {}
    cached_reports = reusable_report_paths(output, previous)
    with tempfile.TemporaryDirectory(prefix="prepare-", dir=output) as staging:
        stage = Path(staging)
        generated = stage / "generated"
        if cached_reports is None:
            subprocess.run([
                java, "-DbundlerMainClass=net.minecraft.data.Main", "-jar", str(jar),
                "--reports", "--output", str(generated),
            ], cwd=output, check=True)
        else:
            for relative in cached_reports:
                atomic_copy(output / relative, stage / relative)
        data = stage / "data"
        data.mkdir()
        version = extract_data(jar, data)
        registries = [
            {"name": f"minecraft:{name}", "entries": registry_entries(data, f"minecraft:{name}")}
            for name in REGISTRIES
        ]
        payload = {"version": VERSION, "protocol": PROTOCOL, "registries": registries}
        (stage / "registries-sync.json").write_text(json.dumps(payload, separators=(",", ":")), encoding="utf-8")
        builtin = json.loads((generated / "reports/registries.json").read_text(encoding="utf-8"))
        tags = collect_tags(data, builtin, registries)
        (stage / "tags-sync.json").write_text(json.dumps({"registries": tags}, separators=(",", ":")), encoding="utf-8")
        prepared = {
            "version": VERSION, "protocol": PROTOCOL, "server_sha1": SERVER_SHA1,
            "world_version": version["world_version"], "java_major": version["java_version"],
            "registries": len(registries), "entries": sum(len(value["entries"]) for value in registries),
            "tag_registries": len(tags), "tags": sum(len(value["tags"]) for value in tags),
            **recipe_identity(data),
        }
        if reusable_report_paths(stage, prepared) is None:
            raise ValueError("Staged protocol reports or item defaults failed validation")
        (stage / "prepared.json").write_text(json.dumps(prepared, indent=2) + "\n", encoding="utf-8")
        files = [path.relative_to(stage) for path in data.rglob("*") if path.is_file()]
        files.extend(report_paths(stage))
        files.extend(map(Path, ("registries-sync.json", "tags-sync.json", "prepared.json")))
        # Preflight every destination before writing; publish the readiness marker last.
        if (output / "data").exists():
            expected = {path.relative_to(data) for path in recipe_paths(data)}
            for path in recipe_paths(output / "data"):
                if path.relative_to(output / "data") not in expected:
                    raise ValueError("Unexpected cached recipe; preserve/remove it before retrying")
        for relative in files:
            reject_links(output / relative)
            if (output / relative).exists() and not (output / relative).is_file():
                raise ValueError("Prepared file destination is not a regular file")
        for relative in files:
            atomic_copy(stage / relative, output / relative)
    print(json.dumps({"data_directory": str(output), **prepared}, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path(".cache/vanilla/26.3"))
    parser.add_argument("--java", default="java", help="Path to a Java 25+ runtime")
    parser.add_argument("--server-jar", type=Path, help="Use an already downloaded official 26.3 server JAR")
    args = parser.parse_args()
    try:
        prepare(args.output.absolute(), args.java, args.server_jar)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError, zipfile.BadZipFile) as error:
        print(f"Data preparation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
