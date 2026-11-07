# Native profiling

On Linux, an operator with level 4 can run:

```text
/spark profiler start --timeout 30
/spark profiler status
/spark profiler stop
```

The server saves the report in `profiles/`. Open the file in the spark viewer. No report is uploaded automatically.

`zerqon-profiler` samples the Rust process on Linux with kernel `perf_event_open` CPU-clock events. Each thread present at capture start has a fixed 64 KiB ring buffer. Kernel callchains are resolved to native symbols and exported as spark `SamplerData` protobuf. Worker CPU time is measured independently of the coordinator's `/tps` timing.

The first backend is Linux only. Windows returns `Unsupported`; it does not substitute Java stacks or tick timings. Linux kernels may deny perf access. The sampler reports that error and does not change kernel settings, capabilities or permissions. It does not sample a separate Java-plugin JVM or threads created after capture start.

The Linux CI test grants `CAP_PERFMON` to its test executable for that one check and removes it afterwards. This follows the kernel's per-executable capability model; it does not change system-wide perf settings. [Kernel documentation](https://www.kernel.org/doc/html/latest/admin-guide/perf-security.html).

Build Linux binaries with frame pointers and symbols. The repository release profile retains symbols and `.cargo/config.toml` enables Linux frame pointers. Libraries built without frame pointers can produce shorter callchains. Unresolved addresses remain visible as hexadecimal addresses. Inlined function attribution is represented by the enclosing native symbol.

## API

`ProfileSession::start(ProfileConfig)` starts one capture; a second concurrent capture is rejected. `is_finished()` reports automatic completion. `finish()` requests stop and returns the measured profile. Dropping a session stops and joins its worker. Start and finish can perform system calls and symbol resolution; call them from a blocking worker, not the network executor.

`Profile::summary()` exposes actual samples, sampled CPU milliseconds, thread/node counts, duration, observed kernel loss and truncation. `write_spark(path)` writes a raw `.sparkprofile` file atomically without replacing an existing file. `spark_gzip()` prepares the same protobuf in gzip form with a size cap; it does not upload anything.

Default bounds: 30 seconds, 100 Hz per thread, 128 threads, 20,000 samples, 4,096 distinct stacks, 64 frames per stack, 65,536 tree nodes and 16 MiB output. Duration is limited to 300 seconds; other configurable bounds have finite upper limits. Reaching the sample/stack bound stops capture and marks it truncated. Ring overflow is reported as kernel sample loss. Empty captures return an error. Byte limits apply to the export and bounded data structures, not all process memory or the symbol resolver's internal cache.

## Independent verification

On a Linux machine that permits profiling its own user-space threads:

```sh
cargo test -p zerqon-profiler --locked
cargo run -p zerqon-profiler --example capture --release --locked -- .cache/native.sparkprofile
python scripts/test-spark-profile.py .cache/native.sparkprofile
```

The example runs two CPU workers and captures their actual stacks. An optional final number limits samples, e.g. `... .cache/limited.sparkprofile 3`. The verifier downloads pinned Google `protoc` and official spark schemas, verifies SHA-256, decodes the file and checks measured stack weights. Downloads remain in `.cache/`.

The exporter uses `application/x-spark-sampler` and the official spark schema at `7ffae3d7ceb69df95480fd03e3f89d979c780fd8`. Metadata identifies the native Rust backend and uses unknown sampler-engine enum value 2 instead of claiming spark's Java or async-profiler engine. The profile contains one aggregated time window and omits unmeasured JVM/GC/tick values. See [upstream attribution](../crates/zerqon-profiler/NOTICE.md).

Automatic uploads are not implemented. Uploading a profile to a viewer exposes its symbols and thread names and must be an explicit action. The official spark browser viewer has also loaded a local capture and rendered both sampled worker threads.

## Русский

Профилировщик снимает настоящие CPU-стеки Rust-процесса через Linux `perf_event_open`. Используются фиксированные кольцевые буферы, ограничения времени, количества потоков, стеков и размера результата. Профиль сохраняется локально в формате spark; автоматической загрузки нет.

На Windows возвращается явная ошибка неподдерживаемой платформы. Java-стеки и время тиков не выдаются за профиль Rust. Права Linux профилировщик не меняет. Замер охватывает потоки, существовавшие при старте; отдельный JVM-процесс Java-плагинов в него не входит.

Для проверки выполните команды выше. Пример действительно нагружает два CPU-потока, а Python-скрипт декодирует результат официальной схемой spark. Счётчики JVM и GC отсутствуют, если они не измерялись. Официальный браузерный viewer spark также загрузил локальный профиль и отобразил оба измеренных потока.
