# World storage

Zerqon stores current chunks in `world/chunks.redb`. A transaction saves every chunk changed by one operation, its revision and its receipt together. The chunk payload still uses Zerqon's palette format and CRC checks.

Opening an older Zerqon world copies its `c.x.z.zqc` files into a temporary database in bounded batches. The original files remain unchanged. After migration, `chunks.redb` is the only current chunk store. Copying old chunk files back over it does not restore the world. Keep the entire world directory in backups, including the database and metadata.

`storage-format.json` identifies the database and world. The file named `IMPORT_INCOMPLETE` contains a versioned storage guard after migration. Older development builds already refuse directories with this file, which prevents them from loading stale `.zqc` copies. Do not remove the guard to run an older server. A genuine unfinished-import marker still prevents startup.

If migration stops before publishing the database, opening the world retries from the preserved chunk files. If an active database is missing or corrupt, startup stops; it does not silently load older files. Source worlds and the retained `source-data` snapshot are not modified.

The database uses immediate durability and two-phase commit, with a 16 MiB page-cache budget. This budget is separate from the chunk cache and does not cap total process memory. Storage errors during commit can leave its result uncertain even when all chunks were saved atomically. Zerqon marks that store unavailable until it is reopened and recovered; callers must not assume that an error rolled the operation back.

## API

`WorldStore::snapshot` reads up to 64 unique chunk keys in one snapshot. Tokens belong to that open store instance. Missing chunks are returned as `None`; `load` additionally reports `MissingImportedChunk` for an imported world.

`WorldStore::commit` accepts an operation UUID, all observed snapshots and up to 16 chunk replacements. It checks every read token, including unchanged neighbors, before saving. The operation returns a receipt, a conflict, a proven rejection or an indeterminate result. Imported worlds cannot create missing chunks.

`receipt` looks up the last 4,096 committed operations. Retrying the same operation and payload within that history returns the original receipt. Reusing its UUID with another payload is rejected. After reopening, query the receipt instead of reusing snapshot tokens from a closed store.

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

Текущие чанки хранятся в `world/chunks.redb`. Одна операция сохраняет все затронутые чанки целиком: после сбоя получится старое или новое состояние, без половины изменений.

При первом открытии старого мира файлы `.zqc` копируются в базу небольшими порциями. Исходные файлы остаются как резервная копия. После переноса сервер читает текущий мир из базы. Для бэкапа сохраняйте всю папку мира.

Файл `IMPORT_INCOMPLETE` после переноса служит защитой от старых сборок. Его содержимое отличается от отметки незавершённого импорта. Не удаляйте его ради запуска старого ядра: оно может прочитать устаревшие чанки.

Если результат сохранения неизвестен из-за ошибки диска, сервер прекращает работу с этим хранилищем до повторного открытия и восстановления. Ошибка не означает, что данные точно не записались. Пропавшая или повреждённая база не подменяется старой копией мира.
