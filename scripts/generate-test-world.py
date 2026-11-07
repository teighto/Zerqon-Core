import argparse
import queue
import socket
import subprocess
import threading
import time
from pathlib import Path

parser = argparse.ArgumentParser(description='Generate a private vanilla fixture for Anvil import tests.')
parser.add_argument('--server-jar', type=Path, required=True)
parser.add_argument('--directory', type=Path, required=True)
parser.add_argument('--accept-eula', action='store_true')
args = parser.parse_args()
if not args.accept_eula:
    parser.error('Read https://aka.ms/MinecraftEULA and pass --accept-eula for this isolated test.')
if args.directory.exists():
    parser.error('Fixture directory must not exist; existing worlds are never overwritten.')
server = args.server_jar.resolve(strict=True)
probe = subprocess.run(['java', '-XshowSettings:properties', '-version'], capture_output=True, text=True, timeout=15, check=True)
java_home = next(line.split(' = ', 1)[1].strip() for line in probe.stderr.splitlines() if 'java.home = ' in line)
java = Path(java_home) / 'bin' / ('java.exe' if __import__('os').name == 'nt' else 'java')
args.directory.mkdir(parents=True)
with socket.socket() as sock:
    sock.bind(('127.0.0.1', 0))
    port = sock.getsockname()[1]
(args.directory / 'eula.txt').write_text('eula=true\n', encoding='utf-8')
(args.directory / 'server.properties').write_text(f'server-ip=127.0.0.1\nserver-port={port}\nonline-mode=false\nlevel-name=world\nlevel-type=minecraft:flat\ngenerator-settings={{"layers":[{{"height":1,"block":"minecraft:bedrock"}},{{"height":2,"block":"minecraft:dirt"}},{{"height":1,"block":"minecraft:grass_block"}}],"biome":"minecraft:plains"}}\ngenerate-structures=false\nview-distance=2\nsimulation-distance=2\nmax-players=1\nmax-tick-time=60000\nenable-rcon=false\nenable-query=false\n', encoding='utf-8')
lines = queue.Queue()
process = subprocess.Popen([str(java), '-Xms256M', '-Xmx1G', '-jar', str(server), '--nogui'], cwd=args.directory, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, encoding='utf-8', errors='replace')
def read_log():
    with (args.directory / 'fixture.log').open('w', encoding='utf-8') as log:
        for line in process.stdout:
            log.write(line)
            log.flush()
            lines.put(line)
threading.Thread(target=read_log, daemon=True).start()
try:
    deadline = time.monotonic() + 180
    ready = False
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f'Vanilla exited {process.returncode}; inspect fixture.log')
        try:
            line = lines.get(timeout=0.25)
        except queue.Empty:
            continue
        if 'Done (' in line:
            ready = True
            break
    if not ready:
        raise TimeoutError('Vanilla did not finish startup within 180 seconds')
    process.stdin.write('forceload add 0 0 15 15\nforceload add -1 -513 -1 -513\n')
    process.stdin.flush()
    time.sleep(5)
    process.stdin.write('setblock 0 70 0 minecraft:diamond_block\nsetblock -1 70 -513 minecraft:oak_log[axis=x]\nsave-all flush\nstop\n')
    process.stdin.flush()
    code = process.wait(timeout=60)
    if code != 0:
        raise RuntimeError(f'Vanilla stopped with exit {code}')
    regions = list((args.directory / 'world').rglob('*.mca'))
    if not regions:
        raise RuntimeError('Vanilla produced no region files')
    print(f'Generated {len(regions)} vanilla region files in {args.directory / "world"}; server stopped cleanly.')
finally:
    if process.poll() is None:
        process.kill()
        process.wait(timeout=15)
