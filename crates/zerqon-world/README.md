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

`WorldStore::open` locks `world.lock` for its lifetime. Chunk files are `c.<x>.<z>.zqc`, in Zerqon's own versioned format, **not Minecraft Anvil**. The loader checks a 1 MiB file limit, format version, coordinates, payload length, CRC32, palette lengths and every packed palette index. Missing chunks return `None`; corruption returns an error. `save` refuses to replace an existing corrupt chunk.

Saves write and synchronize a temporary file in the same directory, then atomically replace the destination. Unix additionally synchronizes the parent directory. This prevents readers from observing partial writes; it is not a substitute for backups or a guarantee against faulty storage hardware.

World simulation owns update ordering. Concurrent saves of different snapshots are atomic individually but the last completed replacement wins; callers must serialize read-modify-save transactions for each chunk. The store checks structural integrity, not Minecraft registry semantics. Callers validate block state IDs against the active registry before sending or changing blocks.

## Checks

```sh
cargo test -p zerqon-world --locked
cargo clippy -p zerqon-world --all-targets --locked -- -D warnings
cargo run -p zerqon-world --release --locked --example world_bench -- 10000 4
```

The benchmark measures synthetic flat generation, decoding and allocated section payload sizes. It does not measure gameplay, network throughput or RSS. Compare runs using the same machine and load.

## По-русски

Секции хранят блоки компактно; ID берутся из реестра вызывающей стороны. Генератор создаёт плоский тестовый мир с поверхностью Y=63. Рабочие потоки и очередь имеют явные пределы; при заполнении очереди вызывающий код получает ошибку и должен ограничить поступление задач. Кэш возвращает вытесненные чанки для сохранения изменений.

Формат `.zqc` собственный, несовместим с Anvil. Хранилище блокирует одновременное открытие несколькими процессами, проверяет CRC и структуру файлов, атомарно заменяет исправные чанки и отказывается затирать повреждённые. Тесты проверяют запись, повторное открытие, повреждения, уплотнённые палитры и работу потоков. Полноценная генерация Minecraft, сущности и игровая симуляция находятся вне этого модуля.
