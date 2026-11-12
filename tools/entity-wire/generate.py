"""Check exact low-precision Vec3 fixtures against the pinned Minecraft 26.3 codec."""
import argparse
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data", type=Path, default=ROOT / ".cache/vanilla/26.3")
    parser.add_argument("--java", default="java")
    parser.add_argument("--javac", default="javac")
    parser.add_argument("--inputs", type=Path, default=HERE / "inputs.tsv")
    parser.add_argument("--output", type=Path, default=HERE / "26.3.tsv")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    if args.inputs.resolve() == args.output.resolve():
        parser.error("Output must not overwrite input fixtures")
    spec = importlib.util.spec_from_file_location("original_crafting_inputs", ROOT / "tools/crafting-data/generate.py")
    original = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(original)
    server, libraries = original.checked_inputs(args.data.resolve())
    cache = ROOT / ".cache/entity-wire-oracle"
    cache.mkdir(parents=True, exist_ok=True)
    classpath = os.pathsep.join(map(str, [cache, server, *libraries]))
    subprocess.run([args.javac, "--release", "25", "-encoding", "UTF-8", "-Xlint:all,-classfile", "-Werror",
                    "-classpath", classpath, "-d", str(cache), str(HERE / "LpVec3Oracle.java")], check=True, timeout=60)
    with tempfile.TemporaryDirectory(prefix="lp-check-", dir=cache) as temporary:
        generated = Path(temporary) / "26.3.tsv"
        subprocess.run([args.java, "-classpath", classpath, "LpVec3Oracle", str(args.inputs.resolve()), str(generated)], check=True, timeout=60)
        content = generated.read_bytes()
        if args.check:
            if args.output.read_bytes() != content:
                raise SystemExit("LP Vec3 fixtures differ from the original codec")
        else:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_bytes(content)


if __name__ == "__main__":
    main()
