# Minecraft 26.3 client packet oracle

This tool uses the original Mojang codecs from the pinned 26.3 server JAR. It checks shared client decoders and selected gameplay packet formats: IDs 0, 1, 7, 8, 18, 28, 30–33, 40, 41, 54, 57 and 66.

```sh
python scripts/test-client-packet-codec.py
cargo test -p zerqon --test client_packet_data --locked
```

Java 25 JDK and prepared `.cache/vanilla/26.3` data are required for the original-code check. Use `--java`, `--javac` and `--data` for other locations. The script verifies the nested server SHA-256 before loading it and reuses the inventory oracle's registry initialization and item-field normalization.

`fixtures.json` contains 97 cases with decoded fields, packet bodies, packet IDs and uncompressed frames. It includes three teleport acknowledgements with nonzero/negative coordinates, every movement flag combination, signed-command metadata, flying intent with reserved ability bits, all player actions and both use-on booleans. The Java check compares all decoded fields, verifies framing and rejects a truncated or extended body for each case. Its exhaustion check wraps the original codec; it does not claim each Mojang codec rejects trailing bytes itself.

The Rust integration test exercises the actual shared decoders for 80 cases. Movement and attack packets are currently parsed inside the connection handler, so they have original-code fixtures here but are not claimed as shared-decoder integration coverage. Signed command parsing checks the envelope; cryptographic signatures, timestamp freshness and the last-seen message chain still need separate session-level validation.

Regenerate fixtures only when deliberately updating the pinned protocol:

```sh
python scripts/test-client-packet-codec.py --export tests/client_packet_oracle/fixtures.json
```

## Audit results

| ID | Exact body |
|---:|---|
| 0 | VarInt teleport ID, 3 doubles for position, 2 floats for yaw/pitch |
| 1 | VarInt target entity ID |
| 7 | UTF-8 command string, maximum 32,767 UTF-16 code units |
| 8 | Command string, timestamp i64, salt i64, argument signatures, last-seen update |
| 18 | Container/state VarInts, slot i16, button i8, mode VarInt, changed-slot hashes, carried hash |
| 28 | Keepalive nonce i64 |
| 30 | Position: 3 doubles, flags byte |
| 31 | Position: 3 doubles, yaw/pitch: 2 floats, flags byte |
| 32 | Yaw/pitch: 2 floats, flags byte |
| 33 | Flags byte |
| 40 | Flags byte; only bit1 supplies flying intent |
| 41 | Action VarInt, packed position i64, face byte, sequence VarInt |
| 54 | Selected hotbar slot i16 |
| 57 | Slot i16, untrusted length-delimited ItemStack |
| 66 | Hand VarInt, packed position i64, face VarInt, 3 local-hit floats, inside bool, border-hit bool, sequence VarInt |

Movement flags: bit0 on-ground, bit1 horizontal collision. The handler currently uses the on-ground flag; horizontal-collision gameplay is not added by this oracle.

Player abilities use a different flags byte. `decode_player_abilities` requires exactly one byte and reads only bit1; other bits do not grant permissions. The five original-decoder cases use flags 0, 2, 13, 15 and 255. Reserved-bit inputs are supplied directly because the original encoder normalizes them to 0 or 2. The server separately checks whether the player may fly.

Signed commands have up to eight argument signatures. Each entry is a name of at most 16 UTF-16 units followed by 256 signature bytes. Last-seen data is an offset VarInt, three little-endian bitset bytes for 20 acknowledgements, then a checksum byte. Negative offsets and set bits above bit19 are rejected by vanilla's message-chain validator; Zerqon's envelope parser rejects them too, without pretending to verify the chain.

The audit exposed an incomplete teleport acknowledgement parser and unchecked trailing/truncated command/keepalive packets. Their shared Rust parsers now have exact-length and boundary tests. Packet syntax does not by itself prove permission checks, gameplay or secure-chat compatibility.
