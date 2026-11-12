import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
from generate import ROOT, BUNDLE_SHA1, SERVER_SHA256, checked_hash


def main():
    parser = argparse.ArgumentParser(description="Export Minecraft 26.3 mining hardness and tool tags")
    parser.add_argument("--data", type=Path, default=ROOT / ".cache/vanilla/26.3")
    parser.add_argument("--java", default="java")
    parser.add_argument("--javac", default="javac")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    data = args.data.resolve()
    server = data / "versions/26.3/server-26.3.jar"
    checked_hash(data / "server.jar", "sha1", BUNDLE_SHA1)
    checked_hash(server, "sha256", SERVER_SHA256)
    with tempfile.TemporaryDirectory(prefix="zerqon-mining-") as temporary:
        stage = Path(temporary)
        classpath = os.pathsep.join(map(str, [stage, server, *sorted((data / "libraries").rglob("*.jar"))]))
        subprocess.run([args.javac, "--release", "25", "-cp", classpath, "-d", temporary, str(Path(__file__).with_name("MiningFacts.java"))], check=True)
        subprocess.run([args.java, "-cp", classpath, "MiningFacts", str(stage / "mining.json")], check=True)
        result = json.loads((stage / "mining.json").read_text())
    tags = data / "data/minecraft/tags/block"

    def tag(name, seen=frozenset()):
        if name in seen:
            raise ValueError("Cyclic block tag: " + name)
        result = set()
        for value in json.loads((tags / (name + ".json")).read_text())["values"]:
            value = value if isinstance(value, str) else value["id"]
            result |= tag(value[1:].removeprefix("minecraft:"), seen | {name}) if value.startswith("#") else {value}
        return result

    for tool in ["axe", "hoe", "pickaxe", "shovel"]:
        for block in tag("mineable/" + tool):
            if block in result:
                result[block]["tool"] = tool
    for tier, name in enumerate(["wooden", "gold", "stone", "copper", "iron", "diamond", "netherite"]):
        for block in tag("incorrect_for_" + name + "_tool"):
            if block in result:
                result[block]["incorrect"] = result[block].get("incorrect", 0) | (1 << tier)
    encoded = json.dumps(result, separators=(",", ":"), sort_keys=True) + "\n"
    target = ROOT / "crates/zerqon-blocks/assets/mining-26.3.json"
    if args.check:
        if target.read_text() != encoded:
            raise ValueError("Embedded mining definitions differ from the official data")
    else:
        target.write_text(encoded)


if __name__ == "__main__":
    main()
