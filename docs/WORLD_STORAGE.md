# World storage

Zerqon stores current chunks and player inventories in `world/chunks.redb`. Each player record includes the owned inventory, cursor, selected slot and game mode. Crafting output is recalculated from the grid. The chunk payload uses Zerqon's palette format and CRC checks.

Opening an older Zerqon world copies its `c.x.z.zqc` files into a temporary database in bounded batches. The original files remain unchanged. After migration, `chunks.redb` is the only current chunk store. Copying old chunk files back over it does not restore the world. Keep the entire world directory in backups, including the database and metadata.

`storage-format.json` identifies the database and world. Storage version 3 adds player and drop records. Upgrading a version 2 database writes a V3 guard in `IMPORT_INCOMPLETE` before changing the database, then updates `storage-format.json`. Older builds refuse the new guard. Do not remove it to run an older server. A genuine unfinished-import marker still prevents startup.

An interrupted upgrade resumes only when the database, guard and metadata identify a supported intermediate state of the same world. A V3 guard without a database stops startup, including an interrupted first publication: old files are not a safe fallback. Source worlds and the retained `source-data` snapshot are not modified.

Old `playerdata/UUID.zinv` files are imported when that player first connects. The player row and its permanent creation marker are committed together. The original file remains unchanged. Once a player has a database row, the server uses it even if an old file has since changed. A missing or corrupt active row never triggers a fresh import of an older inventory. New players need no `.zinv` file.

The database uses immediate durability and two-phase commit, with a 16 MiB page-cache budget. This budget is separate from the chunk cache and does not cap total process memory. Storage errors during commit can leave its result uncertain even when all chunks were saved atomically. Zerqon marks that store unavailable until it is reopened and recovered; callers must not assume that an error rolled the operation back.

## API

`WorldStore::snapshot` reads up to 64 unique chunk keys in one snapshot. Tokens belong to that open store instance. Missing chunks are returned as `None`; `load` additionally reports `MissingImportedChunk` for an imported world.

`WorldStore::commit` accepts an operation UUID, all observed snapshots and up to 16 chunk replacements. It checks every read token, including unchanged neighbors, before saving. The operation returns a receipt, a conflict, a proven rejection or an indeterminate result. Imported worlds cannot create missing chunks.

`snapshot_domain` reads chunks, players and drops in one database snapshot. `commit_domain` checks all of their revisions and saves their replacements with one receipt. The limits apply across the whole operation: up to 512 reads and 512 writes, with at most 64 chunk reads and 16 chunk writes; 64 MiB of returned payload and 16 MiB of replacement payload. A player record allows 2 MiB plus 4 KiB for its envelope so every valid old inventory fits. Drop records allow 2 MiB. These are storage bounds, not a total server memory limit.

Drop deletion keeps its revision as a tombstone. Moving or deleting a drop updates its chunk index in the same transaction. `drops_in_chunk` returns a bounded page for restoring loaded chunks; the global `scan_drops` also returns tombstones so callers can advance without an unbounded search for live records. The server uses these records for dropped items, merging, pickup and mining loot. Inventory and item ownership changes share a transaction; motion is checkpointed separately.

`receipt` looks up the last 4,096 committed operations. Retrying the same operation and payload within that history returns the original receipt. Reusing its UUID with another payload is rejected. After reopening, query the receipt instead of reusing snapshot tokens from a closed store.

`domain_receipt` includes player and drop revisions as well as chunks. The chunk-only APIs remain available. A storage failure rejects new writers and writers still waiting for admission. An operation already admitted can finish atomically if a reader discovers an unrelated failure later; the health flag is not a rollback mechanism.

`check_available` is an atomic health check for runtime cache paths. Cache publication must happen after durable commit, as one batch. A cancelled waiter or a timeout does not cancel a running storage job. The runtime must retain ownership of completion and must never roll cached state back after a committed operation.

The private import store continues writing `.zqc` files while constructing a new destination. It becomes a transactional store only after import metadata and source retention have completed.

## Verification

```sh
cargo test -p zerqon-world --locked
cargo run -p zerqon-world --example storage_bench --release --locked -- transaction 100 4
cargo run -p zerqon-world --example storage_bench --release --locked -- legacy 100 4
```

The benchmark compares durable writes of the same generated chunks. The legacy mode saves each file separately and has no multi-chunk atomicity. These timings do not measure gameplay or player capacity.

Tests cover restart, concurrent reads and writers, conflicts, migration, imported-world boundaries, corrupted data, link aliases, operation retries and abrupt child-process termination. Fault tests also discard unsynced writes and inject failures before each storage write/sync and after a sync has persisted data. Process-crash tests and simulated disk failures do not replace real power-loss testing.

## По-русски

Текущие чанки, инвентари и режимы игроков хранятся в `world/chunks.redb`. Транзакционный API сохраняет изменения чанков, игроков и записей выпавших предметов одной операцией. Эти записи используются для выбрасывания, добычи, объединения и подбора предметов. Положение сохраняется периодически; смена количества и владельца — сразу вместе с инвентарём.

При первом открытии старого мира файлы `.zqc` копируются в базу небольшими порциями. Исходные файлы остаются как резервная копия. После переноса сервер читает текущий мир из базы. Для бэкапа сохраняйте всю папку мира.

Формат базы V3 добавляет записи игроков и предметов. При обновлении с V2 сначала записывается защита V3 в `IMPORT_INCOMPLETE`, затем обновляются база и `storage-format.json`. Старые сборки отказываются открывать такой мир. Не удаляйте защитный файл ради запуска старого ядра: оно может прочитать устаревшие копии.

Старый `playerdata/UUID.zinv` переносится при первом входе игрока и остаётся неизменным. Вместе с записью игрока база сохраняет отметку её создания. Поэтому потеря активной записи не приводит к повторной загрузке старого инвентаря. Новым игрокам отдельный `.zinv` не создаётся. Для бэкапа нужна вся папка мира.

Незавершённое обновление продолжается только для известных сочетаний базы и метаданных того же мира. Если защита V3 уже есть, а база отсутствует, запуск останавливается. Это правило действует и при сбое первой публикации базы; автоматически возвращаться к старым файлам нельзя.

Если результат сохранения неизвестен из-за ошибки диска, сервер прекращает работу с этим хранилищем до повторного открытия и восстановления. Ошибка не означает, что данные точно не записались. Пропавшая или повреждённая база не подменяется старой копией мира.
