"""Bounded Minecraft 26.3 load test for an isolated offline Zerqon on loopback.

Uses only Python 3.11+ standard-library modules. Mixed mode runs idle, movement,
then creative place/break phases. It edits only initially empty cells above a
delivered flat stone/dirt/grass/bedrock surface and attempts to remove its blocks.
Use a disposable server directory: failed connections can leave test blocks or
saved players. The report measures this workload, not player capacity.

Server: online-mode=false, view-distance>=1, max-players>=client count. Disable
natural spawning for repeatable terrain tests. No operator permissions needed.
Example: python3 scripts/load-test.py --port 25570 --clients 16 --duration 60
Tests: python3 -m unittest discover -s scripts -p test_load_test.py
"""

import argparse
import asyncio
from collections import Counter
from datetime import datetime, timezone
import ipaddress
import json
import math
from pathlib import Path
import platform
import statistics
import struct
import sys
import time
import zlib

PROTOCOL = 777
MAX_FRAME = 2_097_151
MAX_CLIENTS = 64
MAX_SECONDS = 600
MAX_SAMPLES = 2048
AIR = 0
STONE = 1
SAFE_GROUND = {1, 9, 10, 88}


class Cursor:
    def __init__(self, data):
        self.data = memoryview(data)
        self.offset = 0

    def take(self, size):
        if size < 0 or size > len(self.data) - self.offset:
            raise ValueError('truncated packet')
        start = self.offset
        self.offset += size
        return bytes(self.data[start:self.offset])

    def varint(self):
        result = 0
        for index in range(5):
            byte = self.take(1)[0]
            if index == 4 and byte & 0xf0:
                raise ValueError('VarInt overflow')
            result |= (byte & 127) << (index * 7)
            if not byte & 128:
                return result if result < 2**31 else result - 2**32
        raise ValueError('VarInt too long')

    def string(self, limit=32767):
        size = self.varint()
        if not 0 <= size <= limit * 3:
            raise ValueError('invalid string size')
        text = self.take(size).decode('utf-8')
        if len(text.encode('utf-16-be')) // 2 > limit:
            raise ValueError('string exceeds character limit')
        return text

    def rest(self):
        return self.take(len(self.data) - self.offset)


def varint(number):
    if not -(2**31) <= number < 2**31:
        raise ValueError('VarInt out of range')
    number &= 0xffffffff
    result = bytearray()
    while True:
        byte = number & 127
        number >>= 7
        result.append(byte | (128 if number else 0))
        if not number:
            return bytes(result)


def string(text):
    data = text.encode('utf-8')
    return varint(len(data)) + data


def pack_position(position):
    x, y, z = position
    return struct.pack('>Q', ((x & 0x3ffffff) << 38) | ((z & 0x3ffffff) << 12) | (y & 0xfff))


def unpack_position(data):
    value = struct.unpack('>Q', data)[0]
    x, y, z = value >> 38, value & 0xfff, (value >> 12) & 0x3ffffff
    return (x if x < 2**25 else x - 2**26,
            y if y < 2**11 else y - 2**12,
            z if z < 2**25 else z - 2**26)


def encode_frame(packet, threshold):
    if not 0 < len(packet) <= MAX_FRAME:
        raise ValueError('invalid packet size')
    if threshold is None:
        body = packet
    elif len(packet) >= threshold:
        body = varint(len(packet)) + zlib.compress(packet, 1)
    else:
        body = b'\x00' + packet
    if len(body) > MAX_FRAME:
        raise ValueError('frame exceeds size limit')
    return varint(len(body)) + body


def decode_frame(body, threshold):
    if not 0 < len(body) <= MAX_FRAME:
        raise ValueError('invalid frame size')
    if threshold is None:
        return body
    view = Cursor(body)
    expected = view.varint()
    compressed = view.rest()
    if expected == 0:
        if not compressed or len(compressed) >= threshold:
            raise ValueError('uncompressed packet above threshold')
        return compressed
    if not threshold <= expected <= MAX_FRAME:
        raise ValueError('invalid decompressed size')
    decoder = zlib.decompressobj()
    try:
        packet = decoder.decompress(compressed, expected + 1)
    except zlib.error as error:
        raise ValueError(f'invalid zlib frame: {error}') from error
    if (len(packet) != expected or not decoder.eof
            or decoder.unused_data or decoder.unconsumed_tail):
        raise ValueError('compression length mismatch')
    return packet


class Metrics:
    def __init__(self):
        self.wire_rx = self.wire_tx = 0
        self.payload_rx = self.payload_tx = 0
        self.received = Counter()
        self.sent = Counter()
        self.latencies = {}
        self.latency_counts = Counter()

    def sample(self, name, milliseconds):
        self.latency_counts[name] += 1
        values = self.latencies.setdefault(name, [])
        if len(values) < MAX_SAMPLES:
            values.append(round(milliseconds, 4))

    def report(self):
        return {
            'wire_bytes_received': self.wire_rx, 'wire_bytes_sent': self.wire_tx,
            'payload_bytes_received': self.payload_rx, 'payload_bytes_sent': self.payload_tx,
            'packets_received': dict(self.received), 'packets_sent': dict(self.sent),
            'latencies_ms': {name: latency_summary(values) for name, values in self.latencies.items()},
            'latency_observations': dict(self.latency_counts),
            'latency_sampling': f'first {MAX_SAMPLES} observations per metric; all packet and byte counters are exact',
        }


def latency_summary(values):
    ordered = sorted(values)
    if not ordered:
        return {'count': 0}
    return {'count': len(values), 'min': ordered[0], 'median': statistics.median(ordered),
            'p95': ordered[math.ceil(len(ordered) * .95) - 1],
            'p99': ordered[math.ceil(len(ordered) * .99) - 1], 'max': ordered[-1]}


async def read_frame(reader, threshold, metrics):
    async def exact(size):
        try:
            data = await reader.readexactly(size)
        except asyncio.IncompleteReadError as error:
            metrics.wire_rx += len(error.partial)
            raise
        metrics.wire_rx += len(data)
        return data
    size = 0
    for index in range(3):
        byte = (await exact(1))[0]
        size |= (byte & 127) << (index * 7)
        if not byte & 128:
            break
    else:
        raise ValueError('frame length exceeds three bytes')
    if not 0 < size <= MAX_FRAME:
        raise ValueError('invalid frame size')
    packet = decode_frame(await exact(size), threshold)
    metrics.payload_rx += len(packet)
    return packet


def palette_values(view, count, indirect_max):
    bits = view.take(1)[0]
    if bits == 0:
        value = view.varint()
        if value < 0:
            raise ValueError('negative palette value')
        return [value] * count
    if not 1 <= bits <= 31:
        raise ValueError('invalid palette bit width')
    palette = None
    if bits <= indirect_max:
        length = view.varint()
        if not 1 <= length <= (1 << bits):
            raise ValueError('invalid palette length')
        palette = [view.varint() for _ in range(length)]
        if min(palette) < 0:
            raise ValueError('negative palette value')
    per_word = 64 // bits
    result = []
    for _ in range((count + per_word - 1) // per_word):
        word = struct.unpack('>Q', view.take(8))[0]
        for _ in range(min(per_word, count - len(result))):
            value = word & ((1 << bits) - 1)
            word >>= bits
            if palette is not None:
                if value >= len(palette):
                    raise ValueError('palette index out of range')
                value = palette[value]
            result.append(value)
    return result


def chunk_columns(body, wanted):
    view = Cursor(body)
    cx, cz = struct.unpack('>ii', view.take(8))
    columns = {p: [] for p in wanted if (p[0] // 16, p[1] // 16) == (cx, cz)}
    if not columns:
        return columns
    heightmaps = view.varint()
    if not 0 <= heightmaps <= 16:
        raise ValueError('too many heightmaps')
    for _ in range(heightmaps):
        view.varint()
        words = view.varint()
        if not 0 <= words <= 1024:
            raise ValueError('heightmap too large')
        view.take(words * 8)
    size = view.varint()
    sections = Cursor(view.take(size))
    for _ in range(24):
        sections.take(4)
        blocks = palette_values(sections, 4096, 8)
        palette_values(sections, 64, 3)
        for (x, z), column in columns.items():
            column.extend(blocks[y * 256 + (z % 16) * 16 + x % 16] for y in range(16))
    if sections.rest():
        raise ValueError('unexpected chunk section data')
    return columns


def safe_floor(column):
    for index in range(len(column) - 3, -1, -1):
        if column[index] in SAFE_GROUND and column[index + 1:index + 4] == [AIR] * 3:
            return index - 64 + 1
    return None


def lane(index, spawn):
    x = math.floor(spawn[0]) - 14 + (index % 8) * 4
    z = math.floor(spawn[2]) - 14 + (index // 8) * 4
    return (x, z), (x + 2, z)


def text_component(body):
    view = Cursor(body)
    if view.take(1) != b'\x08':
        return f'[non-string NBT component: {body[:128].hex()}]'
    size = struct.unpack('>H', view.take(2))[0]
    raw = view.take(size).replace(b'\xc0\x80', b'\x00')
    return raw.decode('utf-8', errors='surrogatepass').encode('utf-16', errors='surrogatepass').decode('utf-16')


class Client:
    def __init__(self, args, index, shared):
        self.args, self.index, self.shared = args, index, shared
        self.name = f'ZLoad263_{index:02d}'
        self.metrics = Metrics()
        self.writer = self.reader = self.reader_task = None
        self.threshold = None
        self.phase = 'login'
        self.joined = False
        self.failure = self.disconnect = None
        self.closed = False
        self.closing = False
        self.close_error = None
        self.position = self.spawn = None
        self.anchor_column = self.target_column = None
        self.columns = {}
        self.loaded = set()
        self.batches = self.chunks = self.keepalives = self.moves = 0
        self.current_batch = None
        self.movement_started = None
        self.movement_seconds = 0
        self.logged_in = False
        self.pending_edit = None
        self.sequence = 0
        self.target = self.anchor = None
        self.target_state = None
        self.placed = False
        self.edit_results = Counter()
        self.edit_errors = []
        self.edit_skip = None
        self.pending_tps = None
        self.tps = []
        self.stop = asyncio.Event()
        self.send_lock = asyncio.Lock()

    async def send(self, packet_id, body=b''):
        packet = varint(packet_id) + body
        wire = encode_frame(packet, self.threshold)
        async with self.send_lock:
            self.writer.write(wire)
            self.metrics.wire_tx += len(wire)
            self.metrics.payload_tx += len(packet)
            self.metrics.sent[f'{self.phase}:{packet_id}'] += 1
            await asyncio.wait_for(self.writer.drain(), 3)

    async def read(self):
        packet = await read_frame(self.reader, self.threshold, self.metrics)
        view = Cursor(packet)
        packet_id = view.varint()
        if not 0 <= packet_id <= {'login': 5, 'configuration': 20, 'play': 143}[self.phase]:
            raise ValueError(f'invalid {self.phase} packet identifier {packet_id}')
        self.metrics.received[f'{self.phase}:{packet_id}'] += 1
        return packet_id, view.rest()

    async def join(self):
        try:
            async with asyncio.timeout(20):
                await asyncio.sleep(self.index * .05)
                started = time.monotonic()
                self.reader, self.writer = await asyncio.open_connection(self.args.host, self.args.port)
                await self.send(0, varint(PROTOCOL) + string('localhost') + struct.pack('>H', self.args.port) + b'\x02')
                await self.send(0, string(self.name) + bytes(16))
                while True:
                    kind, body = await self.read()
                    if kind == 3:
                        self.threshold = Cursor(body).varint()
                        if not 0 <= self.threshold <= MAX_FRAME:
                            raise ValueError('invalid compression threshold')
                    elif kind == 2:
                        break
                    elif kind == 0:
                        raise ValueError('login disconnect: ' + Cursor(body).string())
                    elif kind == 1:
                        raise ValueError('online-mode encryption requested; isolated offline test server required')
                    else:
                        raise ValueError(f'unexpected login packet {kind}')
                await self.send(3)
                self.phase = 'configuration'
                while True:
                    kind, body = await self.read()
                    if kind == 1:
                        channel = Cursor(body).string()
                        if channel == 'horizon:mod_list':
                            if self.args.profile == 'horizon':
                                await self.send(2, string(channel) + b'\x00')
                            else:
                                await self.send(2, string('minecraft:brand') + string('vanilla'))
                    elif kind == 15:
                        await self.send(7, body)
                    elif kind == 3:
                        await self.send(3)
                        break
                    elif kind == 2:
                        raise ValueError('configuration disconnect: ' + text_component(body))
                    elif kind == 4:
                        await self.send(4, body)
                self.phase = 'play'
                while not (self.logged_in and self.position and self.batches):
                    await self.handle_play(*await self.read())
                await self.send(44)
                self.joined = True
                self.metrics.sample('join', (time.monotonic() - started) * 1000)
                self.reader_task = asyncio.create_task(self.receive())
                self.prepare_lane()
        except (OSError, ValueError, asyncio.TimeoutError, asyncio.IncompleteReadError) as error:
            self.failure = f'{type(error).__name__}: {error}'
            self.stop.set()
            await self.close()

    def prepare_lane(self):
        anchor = self.columns.get(self.anchor_column)
        target = self.columns.get(self.target_column)
        if not anchor or not target:
            self.edit_skip = 'assigned lane was not delivered in the initial chunks'
            return
        ground = safe_floor(anchor)
        if ground is None or safe_floor(target) != ground:
            self.edit_skip = 'lane lacks a matching flat supported surface with three air blocks'
            return
        if abs(ground - self.spawn[1]) > 2:
            self.edit_skip = 'lane surface is too far from spawn height'
            return
        self.anchor = (self.anchor_column[0] + .5, float(ground), self.anchor_column[1] + .5)
        self.target = (self.target_column[0], ground, self.target_column[1])
        self.target_state = AIR

    async def handle_play(self, kind, body):
        if kind == 32:
            self.disconnect = text_component(body)
            raise ValueError('server disconnect: ' + self.disconnect)
        if kind == 50:
            self.logged_in = True
        elif kind == 73:
            view = Cursor(body)
            teleport = view.varint()
            xyz = struct.unpack('>ddd', view.take(24))
            view.take(24)
            rotation = view.take(8)
            if view.take(4) != bytes(4) or view.rest():
                raise ValueError('relative teleport flags are unsupported by this workload')
            if not all(math.isfinite(value) for value in xyz):
                raise ValueError('invalid teleport position')
            self.position = xyz
            if self.spawn is None:
                self.spawn = xyz
                self.anchor_column, self.target_column = lane(self.index, xyz)
            await self.send(0, varint(teleport) + struct.pack('>ddd', *xyz) + rotation)
        elif kind == 45:
            if len(body) != 8:
                raise ValueError('invalid keepalive length')
            started = time.monotonic()
            await self.send(28, body)
            self.keepalives += 1
            self.metrics.sample('keepalive_client_turnaround', (time.monotonic() - started) * 1000)
        elif kind == 46:
            if self.current_batch is None:
                raise ValueError('chunk received outside a batch')
            self.current_batch += 1
            self.chunks += 1
            chunk = struct.unpack('>ii', Cursor(body).take(8))
            if len(self.loaded) >= 4096 and chunk not in self.loaded:
                raise ValueError('delivered chunk count exceeds client bound')
            self.loaded.add(chunk)
            if self.anchor_column:
                self.columns.update(chunk_columns(body, [self.anchor_column, self.target_column]))
        elif kind == 38:
            z, x = struct.unpack('>ii', Cursor(body).take(8))
            self.loaded.discard((x, z))
        elif kind == 12:
            if self.current_batch is not None or body:
                raise ValueError('invalid chunk batch start')
            self.current_batch = 0
        elif kind == 11:
            view = Cursor(body)
            count = view.varint()
            if count != self.current_batch or view.rest() or (self.batches == 0 and count <= 0):
                raise ValueError('chunk batch count does not match received chunks')
            self.current_batch = None
            self.batches += 1
            await self.send(11, struct.pack('>f', 8.0))
        elif kind == 8:
            view = Cursor(body)
            position = unpack_position(view.take(8))
            state = view.varint()
            if position == self.target:
                self.target_state = state
            if self.pending_edit and position == self.pending_edit['position']:
                self.pending_edit['seen'] = state == self.pending_edit['expected']
        elif kind == 4:
            sequence = Cursor(body).varint()
            pending = self.pending_edit
            if pending and sequence == pending['sequence'] and not pending['future'].done():
                self.edit_results['acknowledged'] += 1
                latency = (time.monotonic() - pending['sent']) * 1000
                self.metrics.sample('block_ack_round_trip', latency)
                pending['future'].set_result({'ok': pending['seen'] and pending['rejection'] is None,
                                              'reason': pending['rejection'] or 'expected block update absent before acknowledgement'})
        elif kind == 124:
            text = text_component(body)
            if self.pending_edit and text.startswith('Block interaction rejected:'):
                self.pending_edit['rejection'] = text
            if self.pending_tps is not None and 'TPS' in text:
                elapsed = (time.monotonic() - self.pending_tps) * 1000
                self.metrics.sample('tps_command_round_trip', elapsed)
                self.tps.append({'elapsed_seconds': round(time.monotonic() - self.shared.get('started', time.monotonic()), 3),
                                 'text': text, 'round_trip_ms': round(elapsed, 3)})
                self.pending_tps = None
        elif kind == 71:
            view = Cursor(body)
            flags = view.take(1)[0]
            count = view.varint()
            if flags & 1 and count == 1:
                view.take(16)
                name = view.string(16)
                if name not in {client.name for client in self.shared['clients']}:
                    self.shared['foreign_player'] = True

    async def receive(self):
        try:
            while True:
                await self.handle_play(*await self.read())
        except asyncio.CancelledError:
            raise
        except (OSError, ValueError, asyncio.IncompleteReadError) as error:
            if not self.closing:
                self.disconnect = self.disconnect or f'{type(error).__name__}: {error}'
            self.stop.set()

    async def move(self, position):
        if (math.floor(position[0]) // 16, math.floor(position[2]) // 16) not in self.loaded:
            raise ValueError('movement destination was not delivered')
        await self.send(31, struct.pack('>dddffB', *position, 0.0, 0.0, 1))
        self.position = position
        self.moves += 1

    def body_at_target(self):
        x, y, z = self.target
        for client in self.shared['clients']:
            if client.position and not client.closed:
                px, py, pz = client.position
                if px + .31 > x and px - .31 < x + 1 and pz + .31 > z and pz - .31 < z + 1 and py + 1.8 > y and py < y + 1:
                    return True
        return False

    async def edit(self, place):
        operation = 'place' if place else 'break'
        if self.shared.get('foreign_player'):
            self.edit_skip = 'a player outside this test was observed; edits stopped'
            return
        if self.body_at_target():
            self.edit_results['body_avoided'] += 1
            return
        if (self.target[0] // 16, self.target[2] // 16) not in self.loaded:
            raise ValueError('edit target was not delivered')
        required = AIR if place else STONE
        if self.target_state != required:
            self.edit_skip = 'target changed outside this client; edits stopped'
            return
        self.sequence += 1
        future = asyncio.get_running_loop().create_future()
        self.pending_edit = {'sequence': self.sequence, 'position': self.target, 'expected': STONE if place else AIR,
                             'seen': False, 'rejection': None, 'sent': time.monotonic(), 'future': future}
        if place:
            x, y, z = self.target
            body = b'\x00' + pack_position((x, y - 1, z)) + b'\x01' + struct.pack('>fff', .5, 1.0, .5) + b'\x00\x00' + varint(self.sequence)
            await self.send(66, body)
        else:
            await self.send(41, b'\x00' + pack_position(self.target) + b'\x01' + varint(self.sequence))
        self.edit_results[f'{operation}_sent'] += 1
        try:
            result = await asyncio.wait_for(future, 3)
            if result['ok']:
                self.edit_results[f'{operation}_confirmed'] += 1
                self.placed = place
            else:
                self.edit_results[f'{operation}_rejected'] += 1
                if len(self.edit_errors) < 16:
                    self.edit_errors.append(result['reason'])
        except asyncio.TimeoutError:
            self.edit_results[f'{operation}_timeout'] += 1
            self.edit_skip = 'edit acknowledgement timed out; outcome unknown'
        finally:
            self.pending_edit = None

    async def workload(self, started):
        if not self.joined:
            return
        duration = self.args.duration
        next_move = next_edit = next_tps = started
        lane_ready = False
        try:
            while not self.stop.is_set():
                now = time.monotonic()
                elapsed = now - started
                if elapsed >= duration:
                    break
                mode = self.args.scenario
                phase = ('idle' if elapsed < duration / 3 else 'move' if elapsed < 2 * duration / 3 else 'edit') if mode == 'mixed' else mode
                if self.index == 0 and now >= next_tps:
                    if self.pending_tps is not None:
                        self.edit_results['tps_timeouts'] += 1
                    self.pending_tps = time.monotonic()
                    await self.send(7, string('tps'))
                    next_tps = now + 5
                if phase in ('move', 'edit') and self.anchor and not lane_ready:
                    await self.move(self.anchor)
                    await self.send(54, struct.pack('>h', 0))
                    await self.send(57, struct.pack('>h', 36) + b'\x40\x01\x00\x00')
                    lane_ready = True
                    self.movement_started = time.monotonic()
                    next_move = now
                if phase in ('move', 'edit') and lane_ready and now >= next_move:
                    self.metrics.sample('movement_scheduler_lateness', (now - next_move) * 1000)
                    angle = elapsed * .8 + self.index
                    x, y, z = self.anchor
                    await self.move((x + .2 * math.sin(angle), y, z + .2 * math.cos(angle)))
                    next_move = now + 1 / self.args.move_hz
                if phase == 'edit' and lane_ready and not self.edit_skip and now >= next_edit:
                    await self.edit(not self.placed)
                    next_edit = time.monotonic() + 1
                await asyncio.sleep(.02)
            if self.placed and not self.stop.is_set() and not self.edit_skip:
                await self.edit(False)
        except (OSError, ValueError, asyncio.TimeoutError) as error:
            self.failure = f'workload {type(error).__name__}: {error}'
        finally:
            if self.movement_started is not None:
                self.movement_seconds = time.monotonic() - self.movement_started
            await self.close()

    async def close(self):
        if self.closed or self.close_error:
            return
        self.closing = True
        if self.reader_task and not self.reader_task.done() and self.writer.can_write_eof():
            try:
                self.writer.write_eof()
                await asyncio.wait_for(asyncio.shield(self.reader_task), 2)
            except (OSError, asyncio.TimeoutError) as error:
                self.close_error = f'graceful shutdown: {error or type(error).__name__}'
        if self.reader_task:
            self.reader_task.cancel()
            await asyncio.gather(self.reader_task, return_exceptions=True)
            self.reader_task = None
        if self.writer:
            self.writer.close()
            try:
                await asyncio.wait_for(self.writer.wait_closed(), 3)
            except (OSError, asyncio.TimeoutError) as error:
                self.close_error = str(error) or type(error).__name__
                self.writer.transport.abort()
                return
        self.closed = True

    def report(self):
        return {'name': self.name, 'joined': self.joined, 'join_or_workload_failure': self.failure,
                'disconnect': self.disconnect, 'connection_closed': self.closed, 'close_error': self.close_error,
                'compression_threshold': self.threshold, 'chunks': self.chunks, 'chunk_batches': self.batches,
                'planned_connect_stagger_seconds': self.index * .05,
                'keepalives_answered': self.keepalives, 'movement_packets': self.moves,
                'movement_active_seconds': round(self.movement_seconds, 3),
                'actual_movement_packets_per_second': round(self.moves / self.movement_seconds, 3) if self.movement_seconds else 0,
                'edit_counts': dict(self.edit_results), 'edit_errors': self.edit_errors, 'edit_skip': self.edit_skip,
                'test_block_left': self.placed or self.target_state == STONE,
                'target': self.target, 'tps_replies': self.tps, **self.metrics.report()}


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--host', default='127.0.0.1', help='literal loopback address only; no DNS')
    parser.add_argument('--port', type=int, default=25565)
    parser.add_argument('--clients', type=int, default=16, help='1..64; no reconnect loop')
    parser.add_argument('--duration', type=float, default=60, help='workload seconds after joins, 1..600')
    parser.add_argument('--move-hz', type=int, default=10, help='movement packets/client/second, 1..20')
    parser.add_argument('--scenario', choices=['idle', 'move', 'edit', 'mixed'], default='mixed')
    parser.add_argument('--profile', choices=['java', 'horizon'], default='java')
    parser.add_argument('--output', type=Path, default=Path('.local/load-test.json'))
    args = parser.parse_args(argv)
    try:
        host = ipaddress.ip_address(args.host)
        if not host.is_loopback:
            raise ValueError('not loopback')
    except ValueError:
        parser.error('--host must be a literal loopback address')
    if (not 1 <= args.port <= 65535 or not 1 <= args.clients <= MAX_CLIENTS
            or not math.isfinite(args.duration) or not 1 <= args.duration <= MAX_SECONDS
            or not 1 <= args.move_hz <= 20):
        parser.error('port 1..65535, clients 1..64, duration 1..600, move-hz 1..20 required')
    return args


async def run(args):
    shared = {'clients': [], 'foreign_player': False}
    clients = [Client(args, index, shared) for index in range(args.clients)]
    shared['clients'] = clients
    started = time.monotonic()
    interrupted = False
    try:
        await asyncio.gather(*(client.join() for client in clients))
        shared['started'] = time.monotonic()
        await asyncio.gather(*(client.workload(shared['started']) for client in clients))
    except asyncio.CancelledError:
        interrupted = True
    finally:
        await asyncio.gather(*(client.close() for client in clients))
        results = [client.report() for client in clients]
        incomplete = [c.name for c in clients if c.joined and (
            (c.index == 0 and not c.tps)
            or
            (args.scenario in ('move', 'mixed', 'edit') and c.moves == 0)
            or (args.scenario in ('edit', 'mixed') and (
                not c.edit_results['place_confirmed'] or not c.edit_results['break_confirmed']
                or c.edit_skip or c.edit_errors or c.placed or c.target_state == STONE)))]
        report = {
            'schema': 1, 'minecraft': '26.3', 'protocol': PROTOCOL,
            'created_utc': datetime.now(timezone.utc).isoformat(), 'python': platform.python_version(),
            'endpoint': f'{args.host}:{args.port}', 'scenario': args.scenario, 'profile': args.profile,
            'requested_clients': args.clients, 'requested_workload_seconds': args.duration,
            'elapsed_seconds_including_join_and_close': round(time.monotonic() - started, 3),
            'movement_hz': args.move_hz, 'edit_interval_seconds': 1, 'interrupted': interrupted,
            'joined': sum(c.joined for c in clients), 'failed_clients': sum(bool(c.failure) for c in clients),
            'unexpected_disconnects': sum(bool(c.disconnect) for c in clients),
            'all_connections_closed': all(c.closed for c in clients),
            'connection_close_errors': sum(bool(c.close_error) for c in clients),
            'foreign_player_observed': shared['foreign_player'],
            'workload_incomplete_clients': incomplete,
            'wire_bytes_received': sum(c.metrics.wire_rx for c in clients),
            'wire_bytes_sent': sum(c.metrics.wire_tx for c in clients),
            'clients': results,
            'measurement_scope': 'Synthetic loopback workload; not a player-capacity claim. Keepalive latency is client turnaround, not network RTT. Block success requires a matching update before its ACK.',
        }
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    return report


def main():
    args = parse_args()
    report = asyncio.run(run(args))
    print(f"Joined {report['joined']}/{args.clients}; failures {report['failed_clients']}; disconnects {report['unexpected_disconnects']}; report {args.output}")
    return 1 if report['failed_clients'] or report['unexpected_disconnects'] or report['interrupted'] or report['workload_incomplete_clients'] or report['connection_close_errors'] or not report['all_connections_closed'] else 0


if __name__ == '__main__':
    sys.exit(main())
