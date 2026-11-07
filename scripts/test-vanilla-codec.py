import argparse
import os
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description="Decode Zerqon packets using the official Minecraft 26.3 codecs")
    parser.add_argument("--data", type=Path, default=Path(".cache/vanilla/26.3"))
    parser.add_argument("--java", default="java")
    parser.add_argument("--javac", default="javac")
    args = parser.parse_args()
    data = args.data.resolve()
    output = Path(".cache/protocol-decoder").resolve()
    output.mkdir(parents=True, exist_ok=True)
    server = data / "versions/26.3/server-26.3.jar"
    if not server.is_file():
        raise SystemExit("Run python scripts/prepare-data.py first")
    classpath = os.pathsep.join(str(path) for path in [output, server, *sorted((data / "libraries").rglob("*.jar"))])
    environment = {**os.environ, "ZERQON_TEST_DATA": str(data)}
    subprocess.run(["cargo", "test", "-p", "zerqon", "--test", "packet_data", "export_vanilla_decoder_fixtures", "--", "--ignored"], env=environment, check=True)
    subprocess.run([args.javac, "-encoding", "UTF-8", "--release", "25", "-classpath", classpath, "-d", str(output), "tests/vanilla/PacketDecoder.java"], check=True)
    subprocess.run([args.java, "-classpath", classpath, "PacketDecoder", str(data), str(Path(".cache/protocol-fixtures").resolve())], check=True)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        raise SystemExit(f"{error.cmd[0]} failed with exit code {error.returncode}") from None
