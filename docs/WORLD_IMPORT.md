# Vanilla world import

Zerqon's native Rust importer reads **Minecraft Java Edition 26.3, DataVersion 5023** Anvil region files and transfers the block states of completed overworld chunks into a new `.zqc` world. This is a block import stage toward full world compatibility. It is **not a complete world migration**: entities, mobs, block-entity contents such as chests, player inventories, dimensions, game rules, biome data, lighting, scheduled ticks, structures and other world metadata are not yet restored in Zerqon.

The original world is read without modification. Stop its server and retain a backup before importing. The destination must not exist and must be outside the source directory. The importer recognizes `region/` and the 26.3 layout `dimensions/minecraft/overworld/region/`; ambiguity is rejected.

Supported region compression is gzip (1), zlib (2), and uncompressed NBT (3). External `.mcc` chunks, LZ4, different DataVersions, unknown block names/properties, incomplete/corrupt sectors and overlapping sector allocations fail explicitly. Unfinished vanilla generation chunks are counted and skipped; completed chunks are never silently replaced with generated terrain. The importer accepts the 26.3 string/compound/mixed block palettes and packed long arrays.

## Rust API

`zerqon_world::anvil::import_world_blocks(source, destination, &resolver)` returns `ImportReport`. The resolver receives a Minecraft block name and its property map and must return the matching 26.3 global state ID. A bare block name denotes its registry-defined default state; supplied properties must be checked against the block's registered values. Unknown names or property values must return `None`, never air or an arbitrary default. The server's generated `reports/blocks.json` is the authoritative mapping.

`RegionReader::open(path, region_x, region_z)` and `read_chunk(local_x, local_z, &resolver)` provide streaming access without loading a whole region. The caller supplies region coordinates, while every chunk's NBT coordinates must match its slot.

Each decompressed chunk is limited to 8 MiB. Before deserialization, NBT structure, duplicate keys, sequence sizes, nesting (64 levels) and tag count (65,536) are validated. Region headers are checked before chunk reads. Worlds are limited to 65,536 region files per import.

`import-report.json` records the imported regions/chunks, skipped unfinished chunks, omitted biome/light sections, block-entity/tick counts, and unsupported world fields/artifacts. `full_world_import` remains `false`. Failed imports retain `IMPORT_INCOMPLETE`; Zerqon refuses to open such a destination. Retry into another new directory after correcting the source or mapping. Source data is never deleted and unsupported source files are not rewritten.

## Verification

```sh
cargo test -p zerqon-world --locked
python scripts/generate-test-world.py --server-jar .cache/vanilla/26.3/server.jar --directory .local/vanilla-world-filled --accept-eula
cargo test -p zerqon-world --test anvil -- --ignored --nocapture
```

Read the [Minecraft EULA](https://aka.ms/MinecraftEULA) before using the fixture generator. It requires Java 25 and an independently verified official 26.3 server JAR. It creates a new private localhost world, forces two chunks, places identifiable blocks, saves and stops the server. Existing directories are rejected. Generated Minecraft worlds/assets stay outside Git. The real-world regression resolves states from prepared 26.3 reports and checks positive and negative coordinates after conversion and reopening.

Default tests use small independently generated Python NBT/MCA fixtures (`crates/zerqon-world/tests/fixtures/generate_anvil.py`) and include malformed allocations, compression, decompression limits, invalid NBT, wrong versions, unknown states, source preservation, and failed-import handling.

Implementation dependencies: [fastnbt deserialization](https://docs.rs/fastnbt/latest/fastnbt/) and [flate2 bounded decompression](https://docs.rs/flate2/latest/flate2/bufread/index.html). These libraries handle NBT and compression; world storage and import validation are Zerqon Rust code.

## По-русски

Импортёр переносит состояния блоков завершённых чанков обычного мира из Anvil Minecraft 26.3 / DataVersion 5023 в новый мир Zerqon. Это проверенный этап импорта блоков, **не полный перенос мира**. Мобы, сущности, содержимое сундуков, инвентари игроков, остальные измерения, биомы, освещение, отложенные обновления и настройки мира пока не восстанавливаются.

Оригинал не изменяется. Исходный сервер необходимо остановить; каталог назначения должен быть новым и находиться вне исходного мира. Неизвестные версии/блоки/свойства и повреждённые файлы вызывают ошибку. Неполные чанки генерации учитываются отдельно. Отчёт перечисляет перенесённое и пропущенное; незавершённый импорт помечается `IMPORT_INCOMPLETE`, и такой мир нельзя запустить в Zerqon.

Проверки включают собственные небольшие NBT/MCA-примеры и настоящий мир, созданный официальным сервером 26.3: после импорта и повторного открытия сверяются поставленные блоки на положительных и отрицательных координатах. Полная совместимость миров и восстановление всех игровых данных остаются обязательными последующими этапами.
