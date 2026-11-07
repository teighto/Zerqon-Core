# Configuration / Настройки

Run `zerqon --init` in an empty server directory to create defaults. Existing files are preserved. `zerqon --check-config` validates the properties and settings without opening the listener. Changes require a restart; there is no live reload in 0.0.1.

## Runtime directory

```text
server/
├── zerqon                 # zerqon.exe on Windows
├── zerqon.jar             # optional native-process launcher
├── server.properties      # Minecraft-facing settings
├── server.settings        # Zerqon settings in TOML
├── eula.txt
├── ops.json               # operator names, managed from the console
├── plugins/
│   ├── rust-plugins/
│   └── java-plugins/
├── libraries/             # optional Java host, SDK and Gson
├── .cache/vanilla/26.3/    # locally prepared official data
├── config/                # reserved configuration directory
├── logs/latest.log
└── world/                 # Zerqon chunk storage
```

`--directory PATH` selects the server directory. Relative `data_directory` paths resolve from it. For example, with `--directory run`, prepare data using `python scripts/prepare-data.py --output run/.cache/vanilla/26.3` and copy the optional Java libraries to `run/libraries/`.

## server.properties

| Key | Default | Meaning |
|---|---|---|
| `server-ip` | `127.0.0.1` | Bind address; `0.0.0.0` or empty binds all IPv4 interfaces |
| `server-port` | `25565` | TCP port; `0` requests an available port for tests |
| `motd` | `Zerqon 26.3` | Server-list text, up to 4,096 UTF-8 bytes |
| `max-players` | `20` | Maximum simultaneous players, 1–65,536 |
| `online-mode` | `true` | Authenticate players; `false` is for isolated development tests |
| `view-distance` | `4` | Chunk view radius, 1–32 |
| `network-compression-threshold` | `256` | Compression threshold in bytes; `-1` disables it |
| `level-name` | `world` | World subdirectory name; path separators are rejected |

The alpha parser accepts `key=value` lines and `#`/`!` comments. Duplicate keys are rejected. It is not a complete Java `.properties` escape/continuation parser. Other Paper/Bukkit settings are not automatically implemented by placing them here.

## server.settings

This file uses TOML. Missing keys use defaults; unknown keys are errors.

| Key | Default | Allowed range / purpose |
|---|---|---|
| `network_threads` | CPU count clamped to 2–4 | 1–128 Tokio workers |
| `world_threads` | CPU count minus 2, clamped to 1–8 | 1–128 chunk workers |
| `max_connections` | `128` | 1–65,536 accepted network connections |
| `max_packet_bytes` | `2097151` | 1,024–2,097,151 frame/decompression bound |
| `connection_timeout_seconds` | `30` | 1–300 seconds |
| `chunk_cache_capacity` | `256` | 1–65,536 cached chunks; the cache also has a 256 MiB byte bound |
| `work_queue_capacity` | `128` | 1–65,536 queued world jobs |
| `data_directory` | `.cache/vanilla/26.3` | Prepared official game data |
| `plugin_fuel` | `1000000` | 1,000–100,000,000 WASM fuel units per event |
| `plugin_memory_mib` | `16` | 1–256 MiB WASM linear memory per plugin |
| `java_plugins` | `true` | Enable the optional host when Java JAR plugins are present |
| `java_executable` | `java` | Java 25 executable path |

Thread counts, cache capacity and plugin limits trade resource use for concurrency; increasing every value does not guarantee better performance. WASM memory limits cover linear memory, and the chunk byte bound covers the cache; neither is a process-wide RAM cap. The optional Java host has a separate 128 MiB heap limit.

## Command line

| Flag | Behavior |
|---|---|
| `--directory PATH` | Server directory; defaults to `.` |
| `--init` | Create missing configuration and exit |
| `--check-config` | Validate configuration and exit |
| `--status-only` | Start server-list/status mode without loading game data or a world |
| `--bind ADDRESS` | Override `server-ip` |
| `--port PORT` | Override `server-port` |
| `--threads COUNT` | Override network workers |
| `--world-threads COUNT` | Override chunk workers |
| `--data PATH` | Override prepared-data path |
| `--no-console` | Disable stdin command reading |
| `--no-color` | Disable ANSI log colors |
| `--nogui` | Accepted for Java-server-style launch scripts; Zerqon has no GUI |
| `--help`, `--version` | Print CLI help or version |

Flags override files for that run without rewriting the files. The JAR launcher also accepts `--native PATH` as its first option to select a different native executable. Arguments after it are forwarded to Zerqon. JVM options such as `-Xmx` belong before `-jar` and affect the launcher only.

## Administration

Use `help`, `version`, `tps`, `plugins` and `stop` in the console. `op NAME` and `deop NAME` manage the operator-name list. In-game `/tpsbar` toggles a boss bar; `/setblock X Y Z STATE-ID` changes and persists a block for operators. The alpha permission model grants non-null plugin permission requirements to operators; there is no full permission-provider API yet.

`/tps` reports the recent coordinator tick rate and average measured tick work. Worker CPU, JVM and WASM costs require separate profiling. Integrated spark is not available in this release. The append-only `logs/latest.log` is not automatically rotated; archive it between runs when needed. `RUST_LOG` selects log verbosity, for example `info` or `zerqon=debug`.

## Русский

`--init` создаёт недостающие файлы, `--check-config` проверяет значения без запуска сети. Настройки применяются после перезапуска. `server.properties` содержит адрес, порт, MOTD, онлайн-проверку аккаунтов и параметры Minecraft. `server.settings` — TOML с настройками потоков, кеша, очередей и плагинов. Неизвестные TOML-ключи отвергаются.

Для папки `run` используйте `--directory run`; подготовьте данные в `run/.cache/vanilla/26.3`, Java-библиотеки поместите в `run/libraries/`. Плагины устанавливаются в `plugins/rust-plugins/` и `plugins/java-plugins/` внутри выбранной папки.

Параметры командной строки переопределяют настройки на один запуск, не меняя файлы. `--threads` задаёт сетевые потоки, `--world-threads` — обработку чанков. `-Xmx` запускатора не ограничивает память Rust-процесса. Увеличение всех лимитов не гарантирует ускорение.

Команды консоли: `help`, `version`, `tps`, `plugins`, `op NAME`, `deop NAME`, `setblock X Y Z STATE-ID`, `stop`. Права плагинов сейчас проверяются через статус оператора. `/tpsbar` отображается в игре. Полного permission API и встроенного spark пока нет. Журнал `logs/latest.log` дописывается и автоматически не ротируется.
