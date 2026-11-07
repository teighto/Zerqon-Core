# Original collision checks

The oracle invokes the original Minecraft 26.3 `Entity.collide` method. It does not reimplement the step-selection algorithm in Java. `FixtureLevel` supplies fixed block shapes and an empty entity-collision list; unrelated world operations throw if called. A probe entity uses the original pig, cow or zombie dimensions and their original step-height attributes. The fixture seeds its previous on-ground state directly, without running a world tick.

```sh
python scripts/test-entity-physics.py
cargo test -p zerqon-entities --test collision --locked
```

The script requires prepared 26.3 data and a Java 25 JDK. `--java`, `--javac` and `--data` select their locations. It verifies the pinned server SHA-256 before running. Use `--write` only to regenerate the checked-in fixtures deliberately.

The 438 scenes cover bottom/top slabs, both stair treads, headroom, negative coordinates, falling, ledges, horizontal axis order, tiny displacements and seeded combinations of blocks. Each case records the original full collision result and the original static clipper result. Rust resolves the same block states through its embedded shape catalog and compares both results.

This checks terrain collision and step selection. Entity pushing, fluid travel, dynamic collision contexts, jumping AI and full living-entity travel behavior are outside these fixtures. The Java sources are test harnesses; no Minecraft JAR or decompiled source is distributed here.
