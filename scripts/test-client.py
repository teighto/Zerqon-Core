"""Prepare or launch an isolated vanilla 26.3 client for loopback development tests."""

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import subprocess
import sys
import urllib.request
import uuid

VERSION = "26.3"
METADATA_SHA1 = "80608ff6753705b6a82e2cb86a5937d4d233abd4"
METADATA_URL = f"https://piston-meta.mojang.com/v1/packages/{METADATA_SHA1}/{VERSION}.json"


def allowed(rules, os_name="windows", features=None):
    result = not rules
    features = features or {}
    architecture = {"AMD64": "x86_64", "x86": "x86"}.get(platform.machine(), platform.machine())
    for rule in rules:
        operating_system = rule.get("os", {})
        if operating_system.get("name", os_name) != os_name:
            continue
        if "arch" in operating_system and not re.fullmatch(operating_system["arch"], architecture):
            continue
        if "version" in operating_system and not re.search(operating_system["version"], platform.version()):
            continue
        if any(features.get(name, False) != required for name, required in rule.get("features", {}).items()):
            continue
        result = rule["action"] == "allow"
    return result


def safe_artifact_path(root: Path, relative: str):
    path = PurePosixPath(relative)
    if path.is_absolute() or ".." in path.parts or "\\" in relative or ":" in relative:
        raise ValueError(f"Unsafe artifact path: {relative}")
    return root.joinpath(*path.parts)


def offline_uuid(name: str):
    digest = hashlib.md5(f"OfflinePlayer:{name}".encode("utf-8")).digest()
    return uuid.UUID(bytes=digest, version=3).hex


def verified(path: Path, digest: str):
    if not path.is_file():
        return False
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha1").hexdigest() == digest


def ensure_artifact(path: Path, digest: str, url: str, source: Path | None = None):
    if verified(path, digest):
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".download")
    try:
        if source and verified(source, digest):
            shutil.copyfile(source, temporary)
        else:
            request = urllib.request.Request(url, headers={"User-Agent": "Zerqon-test-client/0.0.1"})
            with urllib.request.urlopen(request, timeout=60) as response, temporary.open("wb") as target:
                shutil.copyfileobj(response, target, 1024 * 1024)
        if not verified(temporary, digest):
            raise ValueError(f"SHA-1 mismatch: {path.name}")
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def arguments(definitions, variables, os_name, features):
    result = []
    for definition in definitions:
        if isinstance(definition, str):
            values = [definition]
        elif allowed(definition.get("rules", []), os_name, features):
            values = definition["value"]
            values = [values] if isinstance(values, str) else values
        else:
            continue
        for value in values:
            for name, replacement in variables.items():
                value = value.replace("${" + name + "}", replacement)
            if "${" in value:
                raise ValueError(f"Unresolved launch variable: {value}")
            result.append(value)
    return result


def prepare(cache: Path, minecraft: Path):
    metadata_path = cache / "versions/26.3/26.3.json"
    ensure_artifact(metadata_path, METADATA_SHA1, METADATA_URL)
    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    os_name = {"Windows": "windows", "Darwin": "osx", "Linux": "linux"}.get(platform.system())
    if os_name is None:
        raise ValueError("Unsupported operating system")
    artifact = metadata["downloads"]["client"]
    jar = cache / "versions/26.3/26.3.jar"
    ensure_artifact(jar, artifact["sha1"], artifact["url"], minecraft / "versions/26.3/26.3.jar")
    classpath = []
    for library in metadata["libraries"]:
        if not allowed(library.get("rules", []), os_name):
            continue
        artifact = library["downloads"]["artifact"]
        destination = safe_artifact_path(cache / "libraries", artifact["path"])
        source = safe_artifact_path(minecraft / "libraries", artifact["path"])
        ensure_artifact(destination, artifact["sha1"], artifact["url"], source)
        classpath.append(str(destination))
    classpath.append(str(jar))
    index = metadata["assetIndex"]
    index_path = cache / "assets/indexes" / f"{index['id']}.json"
    ensure_artifact(index_path, index["sha1"], index["url"], minecraft / "assets/indexes" / f"{index['id']}.json")
    assets = json.loads(index_path.read_text(encoding="utf-8"))["objects"]
    hashes = {asset["hash"] for asset in assets.values()}

    def asset(digest):
        relative = f"assets/objects/{digest[:2]}/{digest}"
        ensure_artifact(cache / relative, digest, f"https://resources.download.minecraft.net/{digest[:2]}/{digest}", minecraft / relative)

    with ThreadPoolExecutor(max_workers=8) as workers:
        list(workers.map(asset, hashes))
    log = metadata["logging"]["client"]["file"]
    log_path = cache / "assets/log_configs" / log["id"]
    ensure_artifact(log_path, log["sha1"], log["url"], minecraft / "assets/log_configs" / log["id"])
    (cache / "natives").mkdir(exist_ok=True)
    print(f"Prepared vanilla {VERSION}: {len(classpath) - 1} libraries, {len(hashes)} verified assets; no account files read.", flush=True)
    return metadata, os_name, classpath, log_path


def command(metadata, os_name, classpath, log_path, cache, game, java, port, username):
    variables = {
        "auth_player_name": username, "version_name": VERSION,
        "game_directory": str(game), "assets_root": str(cache / "assets"),
        "assets_index_name": metadata["assetIndex"]["id"], "auth_uuid": offline_uuid(username),
        "auth_access_token": "0", "clientid": "", "auth_xuid": "", "version_type": "release",
        "resolution_width": "1280", "resolution_height": "720", "quickPlayPath": str(game / "quickPlay.json"),
        "quickPlayMultiplayer": f"127.0.0.1:{port}", "natives_directory": str(cache / "natives"),
        "launcher_name": "Zerqon-local-test", "launcher_version": "0.0.1", "classpath": os.pathsep.join(classpath),
    }
    features = {"has_custom_resolution": True, "has_quick_plays_support": True, "is_quick_play_multiplayer": True}
    jvm = arguments(metadata["arguments"]["jvm"], variables, os_name, features)
    game_arguments = arguments(metadata["arguments"]["game"], variables, os_name, features)
    log_argument = metadata["logging"]["client"]["argument"].replace("${path}", str(log_path))
    return [java, "-Xms256M", "-Xmx2G", log_argument, *jvm, metadata["mainClass"], *game_arguments]


def main():
    default_minecraft = Path(os.environ.get("APPDATA", Path.home())) / ".minecraft"
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--minecraft-dir", type=Path, default=default_minecraft)
    parser.add_argument("--cache", type=Path, default=Path(".cache/client"))
    parser.add_argument("--game-dir", type=Path, default=Path(".cache/client/game"))
    parser.add_argument("--java", default="java")
    parser.add_argument("--port", type=int, default=25565)
    parser.add_argument("--username", default="ZerqonTest")
    parser.add_argument("--launch", action="store_true", help="Launch the prepared client against 127.0.0.1 only")
    args = parser.parse_args()
    if not 1 <= args.port <= 65535 or not re.fullmatch(r"[A-Za-z0-9_]{1,16}", args.username):
        parser.error("A valid port and a 1-16 character Minecraft test name are required")
    cache, game = args.cache.resolve(), args.game_dir.resolve()
    metadata, os_name, classpath, log_path = prepare(cache, args.minecraft_dir.resolve())
    game.mkdir(parents=True, exist_ok=True)
    options = game / "options.txt"
    if not options.exists():
        options.write_text("lang:en_us\nrenderDistance:4\nsimulationDistance:5\nmaxFps:60\nonboardAccessibility:false\njoinedFirstServer:true\n", encoding="utf-8")
    launch = command(metadata, os_name, classpath, log_path, cache, game, args.java, args.port, args.username)
    if not args.launch:
        print("Ready. Add --launch --port PORT to start a visible client using the isolated test profile.")
        return
    environment = {key: value for key, value in os.environ.items() if key not in {"JAVA_TOOL_OPTIONS", "_JAVA_OPTIONS", "JDK_JAVA_OPTIONS"}}
    log_file = cache / "client-console.log"
    with log_file.open("w", encoding="utf-8") as output:
        process = subprocess.Popen(launch, cwd=game, env=environment, stdout=output, stderr=subprocess.STDOUT)
    (cache / "client-process.json").write_text(json.dumps({"pid": process.pid, "username": args.username, "server": f"127.0.0.1:{args.port}", "game_directory": str(game), "console_log": str(log_file)}, indent=2), encoding="utf-8")
    print(f"Started vanilla client PID {process.pid}; log: {log_file}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError) as error:
        raise SystemExit(f"Client preparation failed: {error}") from None
