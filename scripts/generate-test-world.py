import argparse
import hashlib
import json
import queue
import re
import socket
import subprocess
import threading
import time
from pathlib import Path


class ConsoleWriter:
    def __init__(self, process):
        self.process = process
        self.requests = queue.Queue(maxsize=1)
        self.stopped = threading.Event()
        self.worker = threading.Thread(target=self.write_requests, name='zerqon-fixture-stdin', daemon=True)
        self.worker.start()

    def write_requests(self):
        while not self.stopped.is_set():
            try:
                text, completion = self.requests.get(timeout=0.1)
            except queue.Empty:
                continue
            try:
                self.process.stdin.write(text)
                self.process.stdin.flush()
            except (OSError, ValueError) as error:
                completion.put(error)
            else:
                completion.put(None)

    def send(self, text, deadline):
        completion = queue.Queue(maxsize=1)
        try:
            self.requests.put((text, completion), timeout=max(0, deadline - time.monotonic()))
            error = completion.get(timeout=max(0, deadline - time.monotonic()))
        except (queue.Full, queue.Empty) as error:
            raise TimeoutError('Vanilla console write timed out') from error
        if error is not None:
            raise RuntimeError('Vanilla console write failed') from error

    def close(self):
        self.stopped.set()
        self.worker.join(timeout=5)
        if self.worker.is_alive():
            raise RuntimeError('Vanilla console writer did not stop after process exit')


parser = argparse.ArgumentParser(description='Generate a private vanilla fixture for Anvil import tests.')
parser.add_argument('--server-jar', type=Path, required=True)
parser.add_argument('--directory', type=Path, required=True)
parser.add_argument('--accept-eula', action='store_true')
parser.add_argument('--terrain', choices=('flat', 'normal'), default='flat')
parser.add_argument('--seed', type=int)
parser.add_argument('--side-chunks', type=int, default=16, help='Normal fixture side length, from 1 to 16 chunks')
parser.add_argument('--timeout-seconds', type=float, default=180, help='Startup and normal generation deadline, from 1 to 600 seconds')
args = parser.parse_args()
if not 1 <= args.timeout_seconds <= 600:
    parser.error('--timeout-seconds must be between 1 and 600.')
if args.seed is not None and not -(2**63) <= args.seed < 2**63:
    parser.error('Seed must fit a signed 64-bit integer.')
if args.terrain == 'normal' and args.seed is None:
    parser.error('Normal fixtures require an explicit --seed.')
if not 1 <= args.side_chunks <= 16:
    parser.error('--side-chunks must be between 1 and 16.')
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
properties = [
    'server-ip=127.0.0.1', f'server-port={port}', 'online-mode=false', 'level-name=world',
    f'level-type=minecraft:{args.terrain}',
    f'generate-structures={str(args.terrain == "normal").lower()}',
    'view-distance=2', 'simulation-distance=2', 'max-players=1', 'max-tick-time=60000',
    'enable-rcon=false', 'enable-query=false',
]
if args.terrain == 'flat':
    properties.append('generator-settings={"layers":[{"height":1,"block":"minecraft:bedrock"},{"height":2,"block":"minecraft:dirt"},{"height":1,"block":"minecraft:grass_block"}],"biome":"minecraft:plains"}')
if args.seed is not None:
    properties.append(f'level-seed={args.seed}')
(args.directory / 'server.properties').write_text('\n'.join(properties) + '\n', encoding='utf-8')
lines = queue.Queue()
heap = '-Xmx2G' if args.terrain == 'normal' else '-Xmx1G'
process = subprocess.Popen([str(java), '-Xms256M', heap, '-jar', str(server), '--nogui'], cwd=args.directory, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, encoding='utf-8', errors='replace')
def read_log():
    with (args.directory / 'fixture.log').open('w', encoding='utf-8') as log:
        for line in process.stdout:
            log.write(line)
            log.flush()
            lines.put(line)
reader = threading.Thread(target=read_log, daemon=True)
console = None
try:
    reader.start()
    console = ConsoleWriter(process)
    deadline = time.monotonic() + args.timeout_seconds
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
        raise TimeoutError(f'Vanilla did not finish startup within {args.timeout_seconds:g} seconds')
    if args.terrain == 'normal':
        last_block = args.side_chunks * 16 - 1
        deadline = time.monotonic() + args.timeout_seconds
        console.send(f'forceload add 0 0 {last_block} {last_block}\n', deadline)
        pending = {(x, z) for x in range(args.side_chunks) for z in range(args.side_chunks)}
        ready_pattern = re.compile(r'\[Server\] ZERQON_BENCH_READY_(\d+)_(\d+)\s*$')
        next_probe = 0
        while pending:
            if process.poll() is not None:
                raise RuntimeError(f'Vanilla exited {process.returncode}; inspect fixture.log')
            now = time.monotonic()
            if now >= deadline:
                raise TimeoutError(f'{len(pending)} requested chunks did not become loaded within {args.timeout_seconds:g} seconds')
            if now >= next_probe:
                probes = ''.join(f'execute if loaded {x * 16 + 8} 64 {z * 16 + 8} run say ZERQON_BENCH_READY_{x}_{z}\n' for x, z in sorted(pending))
                console.send(probes, deadline)
                next_probe = now + 1
            try:
                line = lines.get(timeout=0.25)
            except queue.Empty:
                continue
            if match := ready_pattern.search(line):
                pending.discard((int(match[1]), int(match[2])))
            elif 'Unknown or incomplete command' in line:
                raise RuntimeError('Vanilla rejected the chunk readiness command; inspect fixture.log')
        console.send('save-all flush\nstop\n', time.monotonic() + 60)
    else:
        console.send('forceload add 0 0 15 15\nforceload add -1 -513 -1 -513\n', time.monotonic() + args.timeout_seconds)
        time.sleep(5)
        console.send('setblock 0 70 0 minecraft:diamond_block\nsetblock -1 70 -513 minecraft:oak_log[axis=x]\nsave-all flush\nstop\n', time.monotonic() + 60)
    code = process.wait(timeout=60)
    if code != 0:
        raise RuntimeError(f'Vanilla stopped with exit {code}')
    regions = list((args.directory / 'world').rglob('*.mca'))
    if not regions:
        raise RuntimeError('Vanilla produced no region files')
    with server.open('rb') as source:
        source_hash = hashlib.file_digest(source, 'sha256').hexdigest()
    (args.directory / 'fixture.json').write_text(json.dumps({
        'terrain': args.terrain, 'seed': args.seed, 'server_jar_sha256': source_hash,
        'requested_chunk_rectangle': [0, 0, args.side_chunks - 1, args.side_chunks - 1] if args.terrain == 'normal' else None,
        'java_heap_limit': heap, 'region_files': len(regions),
    }, indent=2) + '\n', encoding='utf-8')
    print(f'Generated {len(regions)} vanilla region files in {args.directory / "world"}; server stopped cleanly.')
finally:
    try:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=15)
    finally:
        try:
            if console is not None:
                console.close()
        finally:
            if reader.ident is not None:
                reader.join(timeout=5)
            for stream in (process.stdin, process.stdout):
                try:
                    stream.close()
                except OSError:
                    pass
