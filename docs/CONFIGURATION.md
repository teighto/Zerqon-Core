# Configuration / Настройки

The normal source-checkout command is `cargo run --release`. For a packaged build, run `./zerqon` on Linux or `.\zerqon.exe` on Windows. The first run creates missing configuration files and asks you to accept the Minecraft EULA before the server starts. Existing files are preserved.

`cargo run --release -- --init` creates configuration and exits. `cargo run --release -- --check-config` validates it without opening the listener. Port, bind address and thread counts are configured only in the files below. Changes require a restart.

## Runtime directory

```text
server/
├── zerqon                 # zerqon.exe on Windows
├── server.properties      # Minecraft-facing settings
├── server.settings        # Zerqon settings in TOML
├── eula.txt
├── ops.json               # operator UUIDs, managed from the console
├── plugins/
│   ├── rust-plugins/
│   └── java-plugins/
├── libraries/             # optional Java host, SDK and Gson
├── .cache/vanilla/26.3/    # locally prepared official data
├── config/                # reserved configuration directory
├── logs/latest.log
└── world/                 # Zerqon chunk storage
```

`--directory PATH` selects the server directory. Relative `data_directory` paths resolve from it. Game data is prepared automatically inside `.cache/vanilla/26.3`. Keep the Java plugin libraries from the distribution in that server directory's `libraries/` folder.

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

The parser accepts `key=value` lines and `#`/`!` comments. Unknown and duplicate keys are rejected. It does not support Java `.properties` escapes or continued lines. Configuration files are limited to 1 MiB.

## server.settings

This file uses TOML. Missing keys use defaults; unknown keys are errors.

| Key | Default | Allowed range / purpose |
|---|---|---|
| `network_threads` | CPU count clamped to 2–4 | 1–128 Tokio workers |
| `world_threads` | CPU count minus 2, clamped to 1–8 | 1–128 chunk workers |
| `max_connections` | `128` | 1–65,536 accepted network connections; must be at least `max-players` |
| `max_packet_bytes` | `2097151` | 1,024–2,097,151 frame/decompression bound |
| `connection_timeout_seconds` | `30` | 1–300 seconds |
| `chunk_cache_capacity` | `256` | 1–65,536 cached chunks; the cache also has a 256 MiB byte bound |
| `work_queue_capacity` | `128` | 1–65,536 queued world jobs |
| `data_directory` | `.cache/vanilla/26.3` | Prepared official game data |
| `plugin_fuel` | `1000000` | 1,000–100,000,000 WASM fuel units per event |
| `plugin_memory_mib` | `16` | 1–256 MiB WASM linear memory per plugin |
| `java_plugins` | `true` | Enable the optional host when Java JAR plugins are present |
| `java_executable` | `java` | Java 25 executable path |
| `auto_download_runtime` | `true` | Download a private Java 25 runtime when needed; custom paths are never replaced |
| `max_entities` | `128` | Maximum mobs, from 1 to 10,000 |
| `spawn_mobs` | `true` | Natural passive mob spawning near players |
| `update_check` | `true` | Check the public GitHub repository for a newer stable release |
| `update_check_interval_hours` | `6` | 1–168 hours between successful update checks |

Set the bind address and port in `server.properties`, and thread counts in `server.settings`. These values have no command-line overrides. WASM memory limits cover linear memory, and the chunk byte bound covers the cache; neither is a process-wide RAM cap. The optional Java plugin host has a separate 128 MiB heap limit.

The update checker runs in the background, starting with one check at startup. It uses no GitHub token, skips drafts and prereleases, and downloads no software. New-version notices appear once in the console and once per operator session and version, in English, with the GitHub release link. Failed checks back off; status-only mode never starts the checker. Set `update_check = false` to disable all update-check requests.

## Command line

| Flag | Behavior |
|---|---|
| `--directory PATH` | Server directory; defaults to `.` |
| `--init` | Create missing configuration and exit |
| `--check-config` | Validate configuration and exit |
| `--status-only` | Start server-list/status mode without loading game data, worlds or plugins |
| `--import-blocks SOURCE` | Import Anvil 26.3 blocks into a new world directory; other world data is not yet imported |
| `--data PATH` | Override prepared-data path |
| `--no-console` | Disable stdin command reading |
| `--no-color` | Disable ANSI log colors |
| `--help`, `--version` | Print CLI help or version |

When running through Cargo, place operational flags after `--`, for example `cargo run --release -- --directory run`. The native executable accepts them directly. No flags are required for the normal startup command.

## Administration

Use `help`, `version`, `tps`, `plugins` and `stop` in the console. `op NAME` grants access to a connected player by UUID. `deop NAME` or `deop UUID` removes it. Names in `ops.json` are labels; matching a name does not grant access. Old name-only files are rejected. In-game `/tpsbar` toggles a boss bar; `/setblock X Y Z STATE-ID` changes and saves a block. Core commands use operator levels. A full permission-provider API is still being developed.

`/tps` reports the recent coordinator tick rate and average measured tick work. The built-in Rust CPU profiler uses `/spark profiler start` and `/spark profiler stop` on Linux. It saves reports in `profiles/`; see [Profiling](PROFILING.md). The append-only `logs/latest.log` is not automatically rotated. `RUST_LOG` selects log verbosity, for example `info` or `zerqon=debug`.

## Русский

Обычный запуск из исходников — `cargo run --release`, без флагов. Нативная сборка запускается через `./zerqon` или `.\zerqon.exe`. Первый запуск создаёт недостающие конфиги; перед запуском сервера нужно принять Minecraft EULA.

`cargo run --release -- --init` создаёт конфиги и завершает работу, `cargo run --release -- --check-config` проверяет значения без запуска сети. Настройки применяются после перезапуска. `server.properties` содержит адрес, порт, MOTD, онлайн-проверку аккаунтов и параметры Minecraft. `server.settings` — TOML с настройками потоков, кеша, очередей и плагинов. Неизвестные и повторяющиеся ключи отвергаются.

Для папки `run` используйте `--directory run`. Игровые данные подготовятся автоматически в `run/.cache/vanilla/26.3`; Java-библиотеки из дистрибутива должны находиться в `run/libraries/`. Плагины устанавливаются в `plugins/rust-plugins/` и `plugins/java-plugins/` внутри выбранной папки.

Адрес и порт задаются только в `server.properties`, количество потоков — только в `server.settings`. Переопределений этих настроек через командную строку нет. Для служебных флагов Cargo нужен разделитель `--`: например, `cargo run --release -- --directory run`.

Проверка обновлений задаётся `update_check = true` и интервалом `update_check_interval_hours = 6` (1–168 часов). Она обращается к публичному GitHub без токена и не скачивает программы. О новой стабильной версии консоль и операторы получают одно английское уведомление со ссылкой; для игрока повторное уведомление возможно в новом сеансе или при появлении более новой версии. `update_check = false` полностью отключает запросы, а диагностический режим статуса их не запускает.

Команды консоли: `help`, `version`, `tps`, `plugins`, `op NAME`, `deop NAME`, `setblock X Y Z STATE-ID`, `stop`. Операторы определяются по UUID. Для `op NAME` игрок должен быть на сервере; `deop` принимает имя или UUID. Для команд ядра учитывается уровень оператора. Полный API прав ещё дорабатывается. `/tpsbar` отображается в игре. Rust-профилировщик работает на Linux и сохраняет отчёты spark в `profiles/`. Журнал `logs/latest.log` дописывается и автоматически не ротируется.
