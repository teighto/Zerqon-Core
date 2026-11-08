# zerqon-world

Rust world primitives for Zerqon. Coordinates use 16×16 chunks, 24 sections and a vertical range of −64 through 319. `Section` stores 4096 `u32` block states in YZX order: `y * 256 + z * 16 + x`. Uniform sections allocate no block storage; mixed sections use a local palette and packed indices.

`Chunk::flat(x, z, FlatPalette)` uses caller-supplied registry IDs. Its layers are bedrock at −64, stone through 59, dirt at 60–62, grass at 63, then air. These primitives do not implement vanilla terrain generation, entities, lighting or gameplay.

```rust
use zerqon_world::{Chunk, ChunkWorkers, FlatPalette, WorldStore};

# fn example() -> Result<(), Box<dyn std::error::Error>> {
let palette = FlatPalette { air: 0, bedrock: 1, stone: 2, dirt: 3, grass: 4 };
// Replace these example values with the loaded 26.3 registry IDs.
let workers = ChunkWorkers::new(4, 64)?;
let mut chunk = workers.try_generate(0, 0, palette)?.wait()?;
chunk.set_block(0, 64, 0, palette.stone)?;
let world = WorldStore::open("world")?;
world.save(&chunk)?;
workers.shutdown()?;
# Ok(())
# }
```

`ChunkWorkers` has a fixed thread count and bounded job queue. `try_submit` also supports owned blocking load/generation jobs. A full queue returns `QueueFull`; callers must apply backpressure. Panicking jobs return `WorkerPanicked`. Dropping a ticket discards delivery; it does not cancel a running job. `shutdown` and `Drop` drain accepted jobs and join all worker threads. Jobs must terminate. Bound outstanding tickets at the caller as well: completed results remain owned by their tickets.

`ChunkCache::with_limits(chunks, bytes)` limits retained chunk count and the sum of chunk structs plus their palette allocations. It returns evicted chunks so the caller can save dirty data. This accounting excludes hash-table/allocator overhead and references retained by callers; it is not a process RSS limit.

## Storage

`WorldStore::open` locks `world.lock` for its lifetime. Current chunks live in `chunks.redb`; each payload uses Zerqon's palette format, coordinates and CRC32. The loader checks size limits, palette lengths and every packed palette index. Missing chunks return `None` in a native world and `MissingImportedChunk` in an imported world. Corruption returns an error.

`snapshot` and `commit` compare revisions and save the affected chunks with an operation receipt in one durable transaction. Opening an older store migrates its `.zqc` files into the database and preserves the originals. The import tool also writes `.zqc` files while preparing a new destination. See [world storage](../../docs/WORLD_STORAGE.md) for recovery and migration rules.

World simulation owns update ordering. Callers handle transaction conflicts and validate block state IDs against the active registry before sending or changing blocks.

## Checks

```sh
cargo test -p zerqon-world --locked
cargo clippy -p zerqon-world --all-targets --locked -- -D warnings
cargo run -p zerqon-world --release --locked --example world_bench -- 10000 4
```

The benchmark measures synthetic flat generation, decoding and allocated section payload sizes. It does not measure gameplay, network throughput or RSS. Compare runs using the same machine and load.

## Ordinary terrain block loading

Create a separate vanilla fixture with a fixed seed:

```sh
python scripts/generate-test-world.py --server-jar .cache/vanilla/26.3/server.jar \
  --directory .local/ordinary-vanilla --terrain normal --seed 20261108 --accept-eula
```

Import its `world/` into a new `.local/ordinary-import` server directory using the [block import instructions](../../docs/WORLD_IMPORT.md). The commands below select 256 completed chunks and prepare that dedicated store before timing. Linux release builds require the musl toolchain described in the [configuration guide](../../docs/CONFIGURATION.md).

```sh
python scripts/prepare-world-benchmark.py --world .local/ordinary-import/world \
  --rectangle 0 0 15 15 --output .local/ordinary-selection.json
cargo build -p zerqon-world --example ordinary_load_bench --release --locked \
  --target x86_64-unknown-linux-musl
target/x86_64-unknown-linux-musl/release/examples/ordinary_load_bench \
  --prepare-store .local/ordinary-import/world .local/ordinary-selection.json
python scripts/benchmark-world-load.py \
  --binary target/x86_64-unknown-linux-musl/release/examples/ordinary_load_bench \
  --world .local/ordinary-import/world --selection .local/ordinary-selection.json \
  --settings .local/ordinary-import/server.settings --output .local/ordinary-results
```

The Linux driver starts a fresh process for each run with 1, 4 and the configured number of world workers. It measures storage reads and block decoding after an untimed warmup. Database setup, full chunk hash checks and shutdown are outside that timer. Process CPU and peak RSS are recorded separately, including setup and shutdown. The OS page cache is not flushed.

This workload excludes terrain generation, pregeneration, biome and light processing, entities and network delivery. The selected chunk bodies are checked before and after every run. Output files retain each result, the median and the full observed range; an existing output directory is refused.

## По-русски

Секции хранят блоки компактно; ID берутся из реестра вызывающей стороны. Генератор создаёт плоский тестовый мир с поверхностью Y=63. Рабочие потоки и очередь имеют явные пределы; при заполнении очереди вызывающий код получает ошибку и должен ограничить поступление задач. Кэш возвращает вытесненные чанки для сохранения изменений.

Текущие чанки хранятся в `chunks.redb`. Транзакция сохраняет изменения чанков вместе с результатом операции и проверяет версии прочитанных данных. При переносе старого хранилища исходные `.zqc` сохраняются. Хранилище проверяет CRC и структуру данных и блокирует одновременное открытие мира несколькими процессами. Правила переноса описаны в [документации хранилища](../../docs/WORLD_STORAGE.md).

Для замеров обычного рельефа используйте команды выше: отдельный мир vanilla с заданным seed, импорт блоков, фиксированный набор чанков и подготовка базы. Драйвер Linux повторяет чтение и декодирование с 1, 4 и заданным в конфиге числом потоков. Перед каждым прогоном он прогревает выбранные данные, а до и после сверяет хеши чанков. CPU и пиковая память процесса учитываются отдельно. Кеш ОС принудительно не очищается. Генерация, предгенерация, освещение, биомы, сущности и отправка по сети в этот замер не входят.
