import gzip
import struct
from pathlib import Path

ROOT = Path(__file__).parent

def string(value):
    raw = value.encode('utf-8')
    return struct.pack('>H', len(raw)) + raw

def tag(kind, name, raw):
    return bytes([kind]) + string(name) + raw

def number(name, value, kind=3):
    return tag(kind, name, struct.pack({1:'b',3:'>i',4:'>q',5:'>f'}[kind], value))

def text(name, value):
    return tag(8, name, string(value))

def compound(name, fields):
    return tag(10, name, b''.join(fields) + b'\0')

level = compound('', [compound('Data', [
    number('DataVersion', 5023),
    compound('Version', [number('Id', 5023), text('Name', '26.3'), text('Series', 'main'), number('Snapshot', 0, 1)]),
    text('LevelName', 'Fixture Мир'),
    tag(9, 'version_history', bytes([3]) + struct.pack('>ii', 1, 5023)),
    number('Time', 1234, 4), number('GameType', 0),
    compound('difficulty_settings', [text('difficulty', 'easy'), number('hardcore', 0, 1), number('locked', 0, 1)]),
    compound('spawn', [text('dimension', 'minecraft:overworld'), tag(11, 'pos', struct.pack('>iiii', 3, -1, -60, -513)), number('yaw', 45, 5), number('pitch', -12.5, 5)]),
    text('unsupported_fixture_field', 'preserve the original bytes'),
])])
generator = compound('', [number('DataVersion', 5023), compound('data', [
    number('seed', -12345, 4), number('generate_structures', 0, 1), number('bonus_chest', 0, 1),
    compound('dimensions', [
        compound('minecraft:overworld', [text('type', 'minecraft:overworld'), compound('generator', [text('type', 'minecraft:flat'), compound('settings', [text('biome', 'minecraft:plains')])])]),
        compound('minecraft:the_nether', [text('type', 'minecraft:the_nether'), compound('generator', [text('type', 'minecraft:noise'), text('settings', 'minecraft:nether')])]),
    ])
])])
rules = compound('', [number('DataVersion', 5023), compound('data', [number('minecraft:respawn_radius', 10)])])
for name, data in [('metadata-level.dat', level), ('metadata-world-gen-settings.dat', generator), ('metadata-game-rules.dat', rules)]:
    ROOT.joinpath(name).write_bytes(gzip.compress(data, mtime=0))
