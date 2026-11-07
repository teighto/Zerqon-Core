# Vanilla world import

Zerqon imports completed overworld block states and source metadata from **Minecraft Java Edition 26.3, DataVersion 5023** into a new native world. The original spawn, source identity and generator description are recorded in `world-metadata.json`. A new UUID identifies the Zerqon world; vanilla does not provide that UUID.

This is **not a complete world migration**. Source game mode, difficulty, game time, rules, clocks and generator are retained, not activated. Vanilla entities, player inventories, chest contents, other dimensions, biomes, lighting, scheduled ticks and datapack behavior are not yet restored. `full_world_import` remains `false`.

## Source and destination

Stop the source server and retain a backup. The destination must not exist and must be outside the source world. Import requires valid gzip NBT `level.dat` and `data/minecraft/world_gen_settings.dat`, both for 26.3 / 5023. It recognizes `region/` and the modern `dimensions/minecraft/overworld/region/` layout; ambiguity is rejected.

Only an overworld spawn with the standard `minecraft:overworld` dimension type is currently supported. Its original block position, yaw and pitch are preserved. Non-finite rotations, unsupported dimensions and out-of-bounds coordinates fail explicitly. Bounded summaries of available namespaced game rules, clocks, weather, overworld border and chunk tickets are read as source metadata; their behavior is not implemented by that parsing.

Source links, reparse points and special files are rejected. An existing `session.lock` is checked and held without rewriting its contents. Unix additionally uses a Java-compatible POSIX record lock; a hard-linked session.lock is rejected because closing an alias could release that lock. Stop external editors as well: size/timestamp changes during copying are errors, but this is not a live-filesystem snapshot mechanism.

## Raw data retention

The importer keeps an inert source snapshot:

```text
world-metadata.json
import-report.json
source-data/manifest.json
source-data/files/level.dat
source-data/files/data/minecraft/world_gen_settings.dat
source-data/files/dimensions/minecraft/overworld/region/...
```

Other regular files and directories, including player data, other dimensions, datapacks, unknown files and unsupported payloads inside MCA, are retained there as well. `session.lock` is operational and is not copied. No `level.dat` is placed at the native root, which remains protected by the foreign-world guard.

The manifest records original relative paths, byte sizes and SHA-256 digests. Native metadata records the manifest digest and source identity hashes. Terrain is read from this retained snapshot after copying. Data is not executed or silently converted into supported behavior. Preserving a chest's NBT does not make its inventory available in the game.

`import-report.json` separates `applied`, `retained` and `not_supported`. Applied entries describe native blocks, stored identity/spawn metadata and generation policy. Retention records the snapshot location, file count and bytes. Existing counts of omitted block entities, ticks, biome/light sections and unfinished chunks remain available.

Failed imports retain `IMPORT_INCOMPLETE`; opening such a destination fails. Metadata and the final report are published atomically, and the marker is removed only after successful completion. Retry into a new directory after correcting the error. Source files are never deleted.

The first server startup migrates imported chunk files into the current [transactional store](WORLD_STORAGE.md). Its versioned downgrade guard also uses the name `IMPORT_INCOMPLETE`; this specific guard is accepted only alongside a valid storage database. It is distinct from a failed-import marker.

## No substitute terrain

Imported worlds use `GenerationPolicy::ExistingOnly`. Loading an absent chunk returns `WorldError::MissingImportedChunk { x, z }`; saving a new chunk outside the imported set also fails. Missing or skipped source terrain must never silently become Zerqon's flat generator.

The runtime starts players at the stored spawn, including yaw and pitch, and validates that its chunk is present and readable before starting. Chunk streaming skips absent imported chunks and counts only the packets actually sent. It propagates corruption and I/O errors instead of treating them as missing terrain.

The bounded per-connection chunk queue stores coordinates. The socket writer requests the current chunk snapshot immediately before sending it, so an older queued snapshot cannot overwrite a newer block change already delivered to that client.

Players can move within delivered imported terrain. A movement whose swept horizontal footprint reaches an absent or unloading chunk is corrected to the last accepted position; it does not generate substitute terrain or routinely disconnect the player. The check uses bounded in-memory chunk sets, accepts at most 64 blocks per horizontal axis per movement packet, and gates interactions until the client acknowledges the correction. It is a terrain-availability guard, not vanilla collision, physics or anti-cheat.

Legacy native `.zqc` worlds without import metadata retain `NativeFlat`. Old imports without the new metadata are rejected; reimport the source into a new directory instead of guessing its spawn or generator.

## Formats and limits

Region compression supports gzip (1), zlib (2), and uncompressed NBT (3). External `.mcc`, LZ4, other DataVersions and unknown block states still fail explicitly. Unfinished generation chunks are counted and skipped. Current 26.3 string/compound/mixed palettes and packed long arrays are supported.

| Input | Bound |
|---|---:|
| Compressed and decompressed level/saved-data NBT | 8 MiB each |
| Decompressed chunk NBT | 8 MiB |
| NBT nesting / tag count | 64 levels / 65,536 tags |
| Native metadata JSON | 1 MiB |
| Retained snapshot manifest | 16 MiB |
| Source regular files / directories | 16,384 each |
| One source file / total snapshot bytes | 2 GiB / 64 GiB |
| Directory depth / relative path | 32 levels / 1,024 UTF-8 bytes |
| Declared source dimensions | 64 |

Retention streams files through a fixed buffer. It adds disk usage without loading an entire source world into RAM. Limit errors preserve the source and do not mark the destination complete.

## Rust API

`zerqon_world::anvil::import_world_blocks(source, destination, &resolver)` returns `ImportReport`. The resolver maps a block name and validated properties to its exact 26.3 state ID. A bare name means the registry-defined default. Unknown names/properties return `None`, never air or an arbitrary fallback.

`WorldStore::metadata()` exposes `WorldMetadata`, including `spawn.position`, `spawn.dimension`, `spawn.yaw`, `spawn.pitch`, `world_id` and retained source settings. `WorldStore::generation_policy()` exposes the enforced policy. `RegionReader` provides bounded low-level access to individual chunks.

## Verification

```sh
cargo test -p zerqon-world --locked
cargo clippy -p zerqon-world --all-targets --locked -- -D warnings
cargo test -p zerqon-world --test anvil -- --ignored --nocapture
cargo test --test imported_world_live -- --ignored --nocapture
```

The ignored tests need prepared 26.3 reports and the generated fixture. The world generator requires Java 25. After reading the [Minecraft EULA](https://aka.ms/MinecraftEULA), create a new isolated fixture with:

```sh
python scripts/generate-test-world.py --server-jar .cache/vanilla/26.3/server.jar --directory .local/vanilla-world-filled --accept-eula
```

The generator refuses existing directories, listens only on localhost, saves identifiable blocks and stops cleanly. Generated Minecraft worlds/assets stay outside Git. Tests use independently encoded Python NBT fixtures, verify the actual vanilla spawn `(0,-60,0)`, compare original/retained bytes, test negative coordinates and malformed metadata, and prove that absent imported chunks cannot become flat terrain. A Java `FileChannel` fixture checks source-lock interoperability.

The wire integration test imports a temporary copy of that world with nonzero spawn rotation, joins twice across a server restart, checks actual chunk coordinates and batch counts, attempts boundary and distant-island movement, acknowledges corrective teleports and executes `/tps`. It compares the original source, retained files and native chunk bytes after the session.

## По-русски

Импорт переносит блоки завершённых чанков overworld и сохраняет исходную точку появления в `world-metadata.json`. UUID нового мира создаёт Zerqon. В `source-data/files/` остаются исходные файлы с манифестом SHA-256; `level.dat` не помещается в корень native-мира. Исходник не изменяется, неподдержанные данные не исполняются.

Нужны корректные `level.dat` и `data/minecraft/world_gen_settings.dat` версии 26.3 / 5023. Поддержан стандартный overworld. Неизвестное измерение spawn, повреждения, ссылки и превышение лимитов вызывают явную ошибку. Исходный сервер нужно остановить до импорта.

Политика `ExistingOnly` запрещает создавать flat-чанки вместо отсутствующего импортированного ландшафта. Игрок появляется в исходной точке с сохранённым направлением взгляда. Отсутствующие чанки пропускаются при отправке; попытка пересечь край доступной территории возвращает игрока в последнюю принятую позицию. Это проверка доступности территории, а не полноценная физика или античит. Чанк spawn проверяется при запуске. Ошибка импорта оставляет `IMPORT_INCOMPLETE`, и такой каталог нельзя открыть. Старые native-миры сохраняют прежний режим; старый импорт без метаданных требуется повторить в новый каталог.

Сохранение сущностей, сундуков, playerdata, правил, времени и режима игры не означает восстановления их поведения. Отчёт разделяет применённые поля, сохранённые оригиналы и неподдержанные возможности. Полный импорт миров и его игровая совместимость остаются последующими этапами.

При первом запуске импортированные чанки переносятся в [базу хранилища](WORLD_STORAGE.md). После этого файл `IMPORT_INCOMPLETE` с другим, строго определённым содержимым защищает мир от старых сборок. Такая защита допускается только при исправной базе; отметка неудачного импорта по-прежнему запрещает запуск.
