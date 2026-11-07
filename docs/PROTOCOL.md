# Minecraft Java 26.3 protocol

Zerqon targets protocol **777**, world data version **5023**. These values were
read from `version.json` in the official 26.3 server JAR. Java 25 is needed to run
Mojang's development data generator; the Rust server runtime is independent of it.
Horizon currently targets the same Java Edition protocol. A protocol match is
necessary but does not prove complete client compatibility.

## Reproducible local data

```sh
python scripts/prepare-data.py
python scripts/test_prepare_data.py
python scripts/test-vanilla-codec.py
```

Options: `--output PATH`, `--java PATH`, `--server-jar PATH`. The default is
`.cache/vanilla/26.3`. The preparer verifies the official server SHA-1
`33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c`, runs its report generator, and extracts
registry JSON locally. It does not start a server or accept the Minecraft EULA.
Downloaded JARs, extracted dependencies, game data and generated reports are
cache files and must not be committed or included in Zerqon releases.

The codec check exports Rust packet fixtures and decodes them with the official
Java classes, including all 24 chunk sections and Unicode chat components. It
checks encoding compatibility; it does not replace a complete client join or
gameplay test. It requires Java 25's `java` and `javac` commands.

| Local file | Purpose |
| --- | --- |
| `prepared.json` | Version, protocol, source checksum and generated counts |
| `generated/reports/packets.json` | Authoritative packet IDs by state and direction |
| `generated/reports/registries.json` | Built-in registry entry protocol IDs |
| `generated/reports/blocks.json` | Block state IDs and properties |
| `registries-sync.json` | Ordered dynamic registries, identifiers and source JSON |
| `tags-sync.json` | Expanded tags with numeric registry IDs |
| `data/minecraft/` | Extracted dynamic registry and tag JSON |

The generated snapshot has 32 synchronized registries, 432 entries and 773 tags
in 15 registries. `registries-sync.json` has this shape:

```json
{"version":"26.3","protocol":777,"registries":[{"name":"minecraft:dimension_type","entries":[{"id":"minecraft:overworld","data":{}}]}]}
```

The empty object above represents source JSON, not a valid dimension definition.
The numeric ID of a dynamic entry is its array index. File identifiers sort by
path including `.json`, then namespace; sorting only the name without its suffix
can produce different IDs. Built-in IDs must come from `registries.json`, not
from block-state IDs. `tags-sync.json` contains
`{"registries":[{"name":"minecraft:block","tags":[{"name":"minecraft:logs","ids":[1]}]}]}`;
the IDs in this illustration are placeholders.

## Framing and state changes

Integers and floats of fixed width use big endian. VarInt is signed 32-bit,
encoded in at most five bytes; negative values occupy five bytes. Strings use a
VarInt byte length followed by UTF-8, with separate encoded-byte and UTF-16
character bounds. A frame is VarInt length followed by VarInt packet ID and body.
Reject oversized frames before allocating their declared size.

Compression adds a VarInt uncompressed length inside the frame. Zero means the
remaining packet is uncompressed; otherwise the remaining bytes are zlib data.
Validate both declared and actual decompressed sizes. Compression negotiation
itself uses the previous framing mode. Online login encrypts the entire TCP
stream using AES-128-CFB8 after the key response, with the shared secret as IV.

1. Handshake serverbound ID 0: protocol VarInt, host String(255), port u16,
   intent VarInt (`1=status`, `2=login`, `3=transfer`).
2. Status request ID 0 produces clientbound ID 0 with a JSON string; ping ID 1
   contains i64 and pong ID 1 repeats it.
3. Login serverbound Hello ID 0: name String(16), UUID (16 bytes). In offline
   mode, derive the identity using UUIDv3/MD5 of `OfflinePlayer:<name>`.
4. Login clientbound Finished ID 2: UUID, name String(16), property count,
   properties (name, value, optional signature), **session UUID**. Login
   Compression ID 3 contains threshold VarInt. Disconnect ID 0 carries JSON.
5. Wait for serverbound LoginAcknowledged ID 3 before configuration packets.
6. Configuration may receive client information ID 0 and custom payload ID 2
   before the known-pack response. Do not mistake their IDs for play packets.
7. Offer clientbound SelectKnownPacks ID 15; the client replies with ID 7.
   Each pack is three strings: namespace, ID, version. Vanilla's core pack is
   `minecraft`, `core`, `26.3`.
8. Send features ID 13, registry data ID 7, tags ID 14 and FinishConfiguration
   ID 3. Wait for the client's FinishConfiguration ID 3 before play.

RegistryData: registry identifier, entry count, then identifier plus bool
`has_data` and optional unnamed network NBT for each entry. When the client
confirmed the vanilla core pack, its built-in entries can omit NBT and use the
client's own data. Sending raw JSON converted to NBT when the client does not
know the pack requires validation against each registry's network codec.
Tags are a registry-count followed by registry identifier, tag-count, then tag
identifier, member-count and member VarInts. Nested tag references must be
expanded before transmission.

## Play bootstrap

IDs below are decimal and were checked against the generated 26.3 packet report.

| Direction | ID | Packet |
| --- | ---: | --- |
| Server to client | 50 | Login / join game |
| Server to client | 65 | Player abilities |
| Server to client | 73 | Player position |
| Server to client | 96, 97 | Chunk cache center, radius |
| Server to client | 99 | Default spawn position |
| Server to client | 12, 11 | Chunk batch start, finished |
| Server to client | 46 | Level chunk with light |
| Server to client | 39 | Game event |
| Server to client | 45 | Keep alive (i64) |
| Server to client | 124 | System chat (network NBT component, overlay bool) |
| Server to client | 9 | Boss event |
| Client to server | 0 | Accept teleportation (VarInt) |
| Client to server | 7, 8 | Unsigned, signed command |
| Client to server | 11 | Chunk batch received (f32 chunks per tick) |
| Client to server | 28 | Keep alive response (i64) |
| Client to server | 30, 31, 32, 33 | Position, position and rotation, rotation, status |
| Client to server | 44 | Player loaded |

Join game: player ID i32, hardcore bool, dimension identifier list, max players
VarInt, view radius VarInt, simulation distance VarInt, reduced debug bool,
show death screen bool, limited crafting bool, spawn information, online mode
bool, enforce secure chat bool.

Spawn information: dimension-type registry ID VarInt, dimension identifier,
seed hash i64, game mode VarInt, previous mode optional VarInt (zero absent,
otherwise mode + 1), debug bool, flat bool, optional death location (dimension
identifier and packed block position), portal cooldown VarInt, sea level VarInt.

Position: teleport ID VarInt, position xyz f64, velocity xyz f64, yaw/pitch f32,
relative flags i32. Wait for the matching teleport acknowledgment. Default spawn
contains dimension identifier, packed block position, yaw f32, pitch f32. Game
event 13 with parameter 0 signals level chunk loading. Send actual nearby chunks
and handle player-loaded and batch responses; login alone does not establish a
playable connection. Keep-alive IDs must match an outstanding request.

## Chunk encoding

Chunk packet: x i32, z i32, heightmap map, section-buffer byte array, block-entity
list, light data. Heightmaps are a VarInt count of (type VarInt, VarInt long count,
big-endian i64 values), **not an NBT compound** in 26.3.

Each section contains non-air count i16, fluid count i16, a 4096-entry block
palette and a 64-entry biome palette. Block order is `(y * 16 + z) * 16 + x`.
Palette encoding starts with bits-per-entry u8. Zero stores a single VarInt
registry ID and no packed longs. Local block palettes use 4–8 bits; local biome
palettes use 1–3 bits. Local palettes encode a count and that many VarInt IDs.
Larger palettes use global registry IDs with the required global width.

Packed values never straddle a 64-bit word. There are `floor(64/bits)` values
per word and `ceil(entry_count/floor(64/bits))` words. **No packed-long array
length is transmitted**. Uniform empty sections are small; retain this form in
memory instead of allocating 4096 block IDs per empty section.

Light data contains sky, block, empty-sky and empty-block bitsets, then sky and
block arrays. A bitset uses a VarInt byte count and little-endian packed bits,
with trailing zero bytes omitted (`BitSet.toByteArray`). Light arrays use a
VarInt array count; each element has byte length 2048 and 2048 packed nibble bytes.
The light section range includes one section below and above the world.

## Sources and limits

- [Official 26.3 version manifest](https://piston-meta.mojang.com/v1/packages/80608ff6753705b6a82e2cb86a5937d4d233abd4/26.3.json).
- [Official server distribution](https://piston-data.mojang.com/v1/objects/33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c/server.jar): `version.json`, generated reports and public packet class signatures.
- [Horizon protocol implementation](https://github.com/horizon-rust/horizon/tree/0ad4f59af247fe66529169233c6d0dd5d223375b/crates/horizon-protocol/src): independent client reference used to cross-check field encodings.

This is an implementation reference for connection bootstrap, not the complete
Minecraft protocol or evidence of gameplay parity. Packet layouts must be
validated with independent vanilla and Horizon clients before release claims.
