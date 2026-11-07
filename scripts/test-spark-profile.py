"""Decode a native capture with the pinned official spark schema and Google protoc."""

import argparse
import hashlib
import os
from pathlib import Path
import platform
import re
import subprocess
import urllib.request

SPARK_REVISION = "7ffae3d7ceb69df95480fd03e3f89d979c780fd8"
PROTO_FILES = {
    "spark.proto": "d1ff30266e68295b423ba1debc6a93bd69044d926baf044896281e6a3c9496a3",
    "spark_sampler.proto": "088cd48a546ece3c0298407e33bb3dbd4427bc97e0b86a7c206e10b811f6ed8d",
}
PROTOC = {
    "Windows": ("windows-x86_64.exe", "5a1b5350308309c9729ce4484a798981f281c10909144c4bdefd6a37688e4b1f"),
    "Linux": ("linux-x86_64.exe", "f6ee22d65361338459208da8bff801c9ca689741cba9187b34990dd45198f864"),
}


def fetch(url, path, expected):
    if path.is_file() and hashlib.sha256(path.read_bytes()).hexdigest() == expected:
        return
    with urllib.request.urlopen(url, timeout=30) as response:
        body = response.read(16 * 1024 * 1024 + 1)
    if hashlib.sha256(body).hexdigest() != expected:
        raise ValueError("checksum mismatch: " + url)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(body)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("profile", type=Path)
    parser.add_argument("--cache", type=Path, default=Path(".cache/spark-proto"))
    args = parser.parse_args()
    if args.profile.stat().st_size > 32 * 1024 * 1024:
        raise ValueError("profile exceeds 32 MiB")
    system = platform.system()
    if system not in PROTOC or platform.machine().lower() not in ("amd64", "x86_64"):
        raise ValueError("this verifier currently supports Windows/Linux x86-64")
    artifact, digest = PROTOC[system]
    compiler = args.cache.resolve() / ("protoc.exe" if os.name == "nt" else "protoc")
    fetch(f"https://repo.maven.apache.org/maven2/com/google/protobuf/protoc/4.36.2/protoc-4.36.2-{artifact}", compiler, digest)
    compiler.chmod(0o755)
    schema = args.cache.resolve() / "schema"
    for name, digest in PROTO_FILES.items():
        fetch(f"https://raw.githubusercontent.com/lucko/spark/{SPARK_REVISION}/spark-common/src/main/proto/spark/{name}", schema / "spark" / name, digest)
    decoded = subprocess.run([str(compiler), "--proto_path=" + str(schema), "--decode=spark.SamplerData", "spark/spark_sampler.proto"], input=args.profile.read_bytes(), capture_output=True, timeout=15, check=True).stdout.decode("utf-8")
    for required in ('name: "Zerqon"', 'sampler_engine: 2', 'method_name:', 'thread_dumper', 'time_windows:', 'profiler_backend'):
        if required not in decoded:
            raise ValueError("decoded profile missing " + required)
    weights = [float(value) for value in re.findall(r"\btimes: ([0-9.eE+-]+)", decoded)]
    if not weights or not all(value > 0 for value in weights):
        raise ValueError("decoded capture has no positive measured weights")
    result = args.cache.resolve() / "decoded-profile.txt"
    result.write_text(decoded, encoding="utf-8")
    print(f"Official spark schema decoded {len(weights)} positive stack/thread weights; report: {result}")


if __name__ == "__main__":
    main()
