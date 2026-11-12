# Minecraft 26.3 entity wire fixtures

`generate.py` checks low-precision vectors with the original `Vec3.LP_STREAM_CODEC`. It reuses the pinned server and library checksum checks from `tools/crafting-data`; Java 25 and prepared local data are required. It downloads nothing and starts no world.

```sh
python tools/entity-wire/generate.py --check
cargo test -p zerqon-protocol --test velocity --locked
```

`inputs.tsv` stores exact IEEE-754 input bits. `26.3.tsv` adds the original encoded bytes and decoded bits for 143 vectors: signed zero, the zero threshold, rounding ties, each axis, extended-scale VarInt boundaries, maximum scale, finite overflow, NaN, infinities and deterministic ordinary samples. Hex bits preserve values that decimal JSON cannot represent reliably.

The original codec converts NaN to zero, clamps components to ±(2^34−1), and uses one zero byte when every component is smaller than 1/32766 in magnitude. Other vectors use six packed bytes plus an optional scale VarInt. The first two packed bytes are written individually; the upper four use big-endian order. The full encoding is at most eleven bytes.

The Rust protocol encoder follows those original rules. Live entity-packet constructors additionally reject nonfinite server state. This does not grant authority to client-supplied positions, items or entity IDs.

To regenerate the reference deliberately:

```sh
python tools/entity-wire/generate.py
```

`--java`, `--javac`, `--data`, `--inputs` and `--output` select explicit local paths. `--check` never rewrites the expected file. The Java wrapper also checks truncated inputs and complete consumption; the underlying stream codec itself allows trailing fields.

Moving item spawn, motion, metadata and collect packets are also checked by the existing `scripts/test-vanilla-codec.py` workflow against original packet codecs. Codec agreement does not establish drop physics, pickup rules or durable item ownership.
