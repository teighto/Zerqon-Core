# Block data export

`generate.py` runs `BlockDataExporter.java` against the original Minecraft 26.3 server and creates the compact data used by `zerqon-blocks`. The exporter is a development tool. The Rust runtime reads its embedded asset directly.

Requirements: Python 3 and a Java 25 JDK. Prepare the existing `.cache/vanilla/26.3` data first with the repository's data preparation command. Then run from the repository root:

```sh
python tools/block-data/generate.py --check
```

To regenerate the asset and original stair fixtures:

```sh
python tools/block-data/generate.py
```

Use `--data PATH`, `--java PATH` and `--javac PATH` for other local locations. The exporter checks both official server JAR hashes before loading their classes. `--check` compares the regenerated facts, original stair fixtures and asset checksum without modifying repository outputs.

The outputs contain factual registry IDs, property domains, collision boxes, item mappings and supported behavior categories. The exporter binds the original `minecraft:snow` tag directly from the verified JAR and evaluates 180 snowy-neighbor cases using its `SnowyBlock.updateShape` method. No Minecraft JAR or decompiled source is included in the repository. Dynamic collision shapes remain flagged and are not used as ordinary static geometry. Special item destruction methods remain unsupported even when their default TOOL component would otherwise permit mining.

Pinned inputs:

- Bundled server SHA-1: `33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c`
- Nested server SHA-256: `a362163eec5d1612d520772bc16e5b39c09e3b234fdc045f56bf544284ee8ae6`
- Minecraft 26.3, protocol 777; [official release](https://www.minecraft.net/en-us/article/minecraft-java-edition-26-3).

Source code in this directory belongs to Zerqon and uses the repository's GPL-3.0-only license. Obtain the official game files separately under their own terms.
