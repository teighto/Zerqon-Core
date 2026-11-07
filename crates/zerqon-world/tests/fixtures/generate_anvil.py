import gzip
import struct
import zlib
from pathlib import Path

ROOT = Path(__file__).parent

def string(value):
    data = value.encode('utf-8')
    return struct.pack('>H', len(data)) + data

def tag(kind, name, value):
    return bytes([kind]) + string(name) + value

def integer(name, value):
    return tag(3, name, struct.pack('>i', value))

def text(name, value):
    return tag(8, name, string(value))

def compound(entries):
    return b''.join(entries) + b'\0'

def sequence(name, kind, values):
    return tag(9, name, bytes([kind]) + struct.pack('>i', len(values)) + b''.join(values))

def state(name, props=None):
    return compound([text('id', name), tag(10, 'properties', compound([text(k, v) for k, v in props.items()]))]) if props else compound([text('', name)])

palette = [state('minecraft:air'), state('minecraft:stone'), state('minecraft:oak_log', {'axis': 'y'})]
words = [0] * 256
for index, value in [(0, 1), (15, 2), (256, 2), (4095, 1)]:
    words[index // 16] |= value << (4 * (index % 16))
packed = struct.pack('>i', len(words)) + b''.join(struct.pack('>Q', word) for word in words)
sections = []
for y, entries in [(-4, [sequence('palette', 10, palette), tag(12, 'data', packed)]), (0, [sequence('palette', 10, [state('minecraft:oak_log', {'axis': 'x'})])]), (19, [sequence('palette', 8, [string('minecraft:air')])])]:
    sections.append(compound([tag(1, 'Y', struct.pack('b', y)), tag(10, 'block_states', compound(entries)), tag(10, 'biomes', compound([sequence('palette', 8, [string('minecraft:plains')])]))]))
chunk = tag(10, '', compound([integer('DataVersion', 5023), integer('xPos', -1), integer('zPos', -33), integer('yPos', -4), text('Status', 'minecraft:full'), sequence('sections', 10, sections), sequence('block_entities', 10, [compound([text('id', 'minecraft:chest'), integer('x', -1), integer('y', 0), integer('z', -513)])]), sequence('block_ticks', 10, [compound([text('i', 'minecraft:stone')])]), sequence('fluid_ticks', 10, [])]))
ROOT.joinpath('negative-chunk.nbt').write_bytes(chunk)
for method, payload in [(1, gzip.compress(chunk, mtime=0)), (2, zlib.compress(chunk)), (3, chunk)]:
    sectors = (len(payload) + 5 + 4095) // 4096
    header = bytearray(8192)
    header[1023 * 4:1024 * 4] = struct.pack('>I', (2 << 8) | sectors)
    body = struct.pack('>I', len(payload) + 1) + bytes([method]) + payload
    ROOT.joinpath(f'negative-{method}.mca').write_bytes(header + body.ljust(sectors * 4096, b'\0'))
