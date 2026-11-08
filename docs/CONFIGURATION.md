# Configuration / Настройки

The normal source-checkout command is `cargo run --release`. For a packaged build, run `./zerqon` on Linux or `.\zerqon.exe` on Windows. The first run creates missing configuration files and asks you to accept the Minecraft EULA before the server starts. Existing files are preserved.

`cargo run --release -- --init` creates configuration and exits. `cargo run --release -- --check-config` validates it without opening the listener. Port, bind address and thread counts are configured only in the files below. Changes require a restart.

Linux release archives use the static `x86_64-unknown-linux-musl` target. Building them with `python scripts/package-release.py` requires `musl-gcc` from `musl-tools` and `rustup target add x86_64-unknown-linux-musl`. Ordinary `cargo run --release` still builds for the host. The packaged Rust executable does not require glibc; the currently managed Linux Java runtime does, so automatic Java preparation on Alpine is not yet supported.

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
    └── playerdata/        # inventories saved by player UUID
```

`--directory PATH` selects the server directory. Relative `data_directory` paths resolve from it. Game data is prepared automatically inside `.cache/vanilla/26.3`. Keep the Java plugin libraries from the distribution in that server directory's `libraries/` folder.

Managed Java normally lives in `.cache/java/`. An existing working local runtime is reused, including when downloads are disabled. For a new download on Windows, a non-ASCII server path selects `%LOCALAPPDATA%\Zerqon\cache\java` instead: Java 25's launcher cannot reliably start from paths outside the system code page. This cache is shared by Zerqon servers in the same Windows account. Both locations reject links and reparse points. If the user-cache path also contains non-ASCII characters, set `java_executable` to an installed Java 25 in an ASCII path. An explicit executable is never replaced; the system Java installation and PATH are unchanged.

Before running managed Java, Zerqon checks the JRE files against the sizes and SHA-256 hashes embedded for the pinned release. Missing, changed or extra files stop startup; existing cache files are preserved. The default `java` setting looks for installed Java only in absolute directory entries in PATH. An explicit `java_executable` path is the administrator's choice and is not checked against the managed JRE inventory.

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
| `packet_memory_mib` | `64` | 1–4,096 MiB for received packet buffers across all connections |
| `connection_packet_memory_mib` | `2` | 1–64 MiB for one connection; cannot exceed `packet_memory_mib` |
| `connection_timeout_seconds` | `30` | 1–300 seconds per read/write or wait for a closing player session |
| `chunk_cache_capacity` | `256` | 1–65,536 cached chunks; the cache also has a 256 MiB byte bound |
| `work_queue_capacity` | `128` | 1–65,536 queued world jobs |
| `data_directory` | `.cache/vanilla/26.3` | Prepared official game data |
| `server_brand` | `zerqon` | Server name shown in the client's F3 screen; nonempty text, at most 128 UTF-16 units, no line breaks or control characters |
| `plugin_fuel` | `1000000` | 1,000–100,000,000 WASM fuel units per event |
| `plugin_memory_mib` | `16` | 1–256 MiB WASM linear memory per plugin |
| `java_plugins` | `true` | Enable the optional host when Java JAR plugins are present |
| `allow_java_clients` | `true` | Allow clients using the standard Java Edition connection profile |
| `allow_horizon_clients` | `true` | Allow clients that answer the Horizon compatibility exchange |
| `java_executable` | `java` | Java 25 executable path |
| `auto_download_runtime` | `true` | Download a private Java 25 runtime when needed; custom paths are never replaced |
| `max_entities` | `128` | Maximum mobs, from 1 to 10,000 |
| `spawn_mobs` | `true` | Natural passive mob spawning near players |
| `update_check` | `true` | Check the public GitHub repository for a newer stable release |
| `update_check_interval_hours` | `6` | 1–168 hours between successful update checks |

Set the bind address and port in `server.properties`, and thread counts in `server.settings`. These values have no command-line overrides. WASM memory limits cover linear memory, and the chunk byte bound covers the cache; neither is a process-wide RAM cap. The optional Java plugin host has a separate 128 MiB heap limit.

The packet-memory limits cover received buffers until those buffers are freed. Queued inventory decoding keeps its buffer counted even if the player disconnects. A connection closes when another packet would exceed either limit. These settings do not limit total process RAM. Incoming frames and decoded payloads are also limited to the smaller of `max_packet_bytes` and 64 KiB.

While a plugin handles a player's command or Join event, the connection continues to handle keepalive replies and send outgoing packets. Later gameplay waits in order, with room for 64 packets, including at most four commands. Exceeding either queue limit closes the connection. Disconnect cleanup waits for an accepted command to finish before sending Quit.

Chunk preparation reserves 4 MiB per result from a separate 32 MiB pool per world. It keeps that reservation until the packet is sent or discarded. Internal player messages share a 4 MiB pool, with a 64 KiB reservation per message and up to 16 accepted messages per connection. These limits are fixed in this version and are separate from the chunk cache and incoming packet limits. `/plugins` reports the current internal message reservation as `control memory`.

The update checker runs in the background, starting with one check at startup. It uses no GitHub token, skips drafts and prereleases, and downloads no software. New-version notices appear once in the console and once per operator session and version, in English, with the GitHub release link. Failed checks back off; status-only mode never starts the checker. Set `update_check = false` to disable all update-check requests.

## Client connections

When the same player reconnects during logout, the new login waits for the previous session's Quit callbacks and cleanup. The wait uses `connection_timeout_seconds` and does not take another player slot. A login for a still active session is rejected.

Both client settings are enabled by default. Set `allow_java_clients = false` to require the Horizon connection profile, or `allow_horizon_clients = false` to refuse that profile. Setting both to `false` closes player admission; server-list status remains available.

Horizon initially reports the same brand as vanilla, so Zerqon checks its supported configuration exchange before allowing entry. This identifies a connection profile, not the language or authenticity of the client. A modified client can imitate or hide the response. These settings do not grant permissions or replace `online-mode` account authentication. The F3 server name is set separately with `server_brand`.

The exchange does not install or load mods. A Horizon response containing shared content mods is rejected because Zerqon does not support their additional registries. Server plugins stay in the existing Java and Rust plugin directories. Standard clients proceed at the usual configuration response, without an additional timeout wait.

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

`/give PLAYER minecraft:ITEM [COUNT]` gives items to a connected player and requires operator level 2. The default count is 1; the limit is 4,096. If the full amount does not fit, nothing is added. Inventories are saved under the world's `playerdata/` directory and restored by UUID.

The personal 2×2 crafting grid supports 478 ordinary shaped and shapeless recipes. Left/right pickup, shift crafting, hotbar swaps and recipe remainders use server-owned inventory state. The result slot is a preview and is recalculated after reconnect; it is not saved as an extra item. Closing the window or disconnecting returns the grid and cursor to the inventory before Quit. If that would require dropping items into the world, the items stay saved in the grid/cursor for the next session. Crafting actions that require drops are rejected without consuming ingredients. World item drops, special recipes, recipe-book actions and other containers still need work.

Old inventory saves are upgraded on the next save without losing their grid, cursor or item components. A legacy file with a nonempty saved result slot is rejected and preserved for explicit recovery. Do not delete that file to bypass the check.

Players currently use Creative mode. Ordinary blocks, logs, dry slabs and stairs can be placed and broken, including stair connections across chunk boundaries. Placement uses the item in the selected hand and checks range and entity collisions. Unsupported interactions are rejected with a chat message. Survival, fluids, random block ticks and interactive containers still need work. Current chunks are saved in `chunks.redb`; see [world storage](WORLD_STORAGE.md) before restoring a backup or moving to an older build.

`/tps` reports the recent coordinator tick rate and average measured tick work. The built-in Rust CPU profiler uses `/spark profiler start` and `/spark profiler stop` on Linux. It saves reports in `profiles/`; see [Profiling](PROFILING.md). The append-only `logs/latest.log` is not automatically rotated. `RUST_LOG` selects log verbosity, for example `info` or `zerqon=debug`.

## Русский

При быстром повторном входе сервер ждёт завершения `Quit` и очистки прежней закрывающейся сессии. На это отведено `connection_timeout_seconds`; дополнительный слот игрока во время ожидания не занят. Если прежнее соединение ещё активно, повторный вход отклоняется.

`server_brand = "zerqon"` задаёт название сервера в F3. Можно указать свой текст, например `server_brand = "teighto"`, и перезапустить сервер. Допускаются кириллица и эмодзи; пустая строка, переносы строк и управляющие символы запрещены. Предел длины, как в Java, составляет 128 единиц UTF-16.

`packet_memory_mib = 64` ограничивает память входящих пакетов на весь сервер, `connection_packet_memory_mib = 2` задаёт лимит одного соединения. Значения указываются в МиБ; общий предел допускает 1–4096, предел соединения 1–64 и не может быть больше общего. Буфер учитывается до его освобождения. Отключение игрока не снимает учёт с буфера, который ещё ждёт разбора в очереди инвентаря. При превышении лимита соединение закрывается. Это не ограничение всей оперативной памяти процесса. Размер входящего пакета после распаковки также не превышает меньшее из `max_packet_bytes` и 64 КиБ.

Пока плагин обрабатывает команду игрока или событие Join, соединение продолжает принимать ответы keepalive и отправлять пакеты. Следующие игровые действия ждут своей очереди. В ней помещается до 64 пакетов, из них не более четырёх команд. При переполнении соединение закрывается. Если команда уже принята, сервер ждёт её завершения перед Quit даже после отключения игрока.

Для подготовки пакетов чанков выделен отдельный резерв 32 МиБ на мир, по 4 МиБ на результат. Резерв удерживается до отправки или удаления результата. Внутренние сообщения игрокам используют общий резерв 4 МиБ, по 64 КиБ на сообщение; одно соединение принимает до 16 таких сообщений одновременно. В этой версии эти лимиты фиксированы и не входят в настройки кеша и входящих пакетов. Текущий резерв сообщений виден в `/plugins` как `control memory`.

`allow_java_clients` и `allow_horizon_clients` по умолчанию включены. Первый разрешает обычный профиль подключения Java Edition, второй — профиль Horizon. Если выключить оба, вход игроков будет закрыт, но сервер останется виден в списке серверов.

Horizon сначала сообщает тот же бренд, что и vanilla, поэтому сервер проверяет его ответ при подключении. Это проверка совместимого профиля: язык программы и подлинность клиента она не доказывает. Изменённый клиент может скрыть или повторить такой ответ. Права игроков и проверка аккаунта через `online-mode` от этих настроек не зависят. Название для F3 задаётся отдельно в `server_brand`.

Моды при этом не загружаются. Если Horizon сообщает о модах с общим игровым содержимым, подключение отклоняется: их дополнительные реестры пока не поддерживаются. Серверные плагины по-прежнему находятся в папках Java- и Rust-плагинов. Для обычного клиента дополнительного ожидания при входе нет.

Обычный запуск из исходников — `cargo run --release`, без флагов. Нативная сборка запускается через `./zerqon` или `.\zerqon.exe`. Первый запуск создаёт недостающие конфиги; перед запуском сервера нужно принять Minecraft EULA.

Linux-дистрибутив собирается для `x86_64-unknown-linux-musl`. Для `python scripts/package-release.py` нужны `musl-gcc` из пакета `musl-tools` и цель Rust, добавленная командой `rustup target add x86_64-unknown-linux-musl`. Обычный `cargo run --release` использует платформу сборщика. Готовому Rust-файлу glibc не нужна, но автоматически загружаемой Java она пока нужна; автоматическая подготовка Java на Alpine ещё не поддерживается.

Команда `/give ИГРОК minecraft:ПРЕДМЕТ [КОЛИЧЕСТВО]` выдаёт предметы подключённому игроку; нужен второй уровень оператора. По умолчанию выдаётся один предмет, максимум — 4 096. Если места не хватает на всё количество, команда ничего не добавляет. Инвентарь сохраняется по UUID в `world/playerdata/`.

В личной сетке 2×2 работают 478 обычных рецептов. Сервер проверяет взятие результата, Shift-крафт, обмен с хотбаром и остатки ингредиентов. Результат в выходном слоте вычисляется заново после входа и не сохраняется как дополнительный предмет. При закрытии окна или отключении сетка и курсор возвращаются в инвентарь до события Quit. Если для этого пришлось бы выбросить предметы в мир, они остаются сохранёнными в сетке и на курсоре. Крафт, требующий выбрасывания, отклоняется без расхода ингредиентов. Предметы в мире, специальные рецепты, книга рецептов и другие контейнеры ещё дорабатываются.

Старые сохранения обновляются при следующей записи с сохранением сетки, курсора и компонентов предметов. Если старый файл содержит предмет в выходном слоте крафта, загрузка останавливается, а файл сохраняется для отдельного восстановления. Удалять его для обхода проверки не нужно.

Сейчас используется творческий режим. Можно ставить и ломать обычные блоки, брёвна, сухие плиты и лестницы. Соединения лестниц обновляются и на границе чанков. Сервер проверяет предмет в руке, расстояние и пересечение с игроками и мобами. Неподдержанная операция возвращает сообщение в чат. Выживание, жидкости, случайные тики блоков и контейнеры ещё дорабатываются. Чанки сохраняются в `chunks.redb`; правила переноса и восстановления описаны в [документации хранилища](WORLD_STORAGE.md).

`cargo run --release -- --init` создаёт конфиги и завершает работу, `cargo run --release -- --check-config` проверяет значения без запуска сети. Настройки применяются после перезапуска. `server.properties` содержит адрес, порт, MOTD, онлайн-проверку аккаунтов и параметры Minecraft. `server.settings` — TOML с настройками потоков, кеша, очередей и плагинов. Неизвестные и повторяющиеся ключи отвергаются.

Для папки `run` используйте `--directory run`. Игровые данные подготовятся автоматически в `run/.cache/vanilla/26.3`; Java-библиотеки из дистрибутива должны находиться в `run/libraries/`. Плагины устанавливаются в `plugins/rust-plugins/` и `plugins/java-plugins/` внутри выбранной папки.

Java обычно скачивается в `.cache/java/`. Уже установленная рабочая копия используется повторно, даже при отключённой загрузке. Для новой загрузки на Windows, если путь содержит символы вне ASCII, используется `%LOCALAPPDATA%\Zerqon\cache\java`: запускатор Java 25 не всегда умеет работать с такими путями. Этот кеш общий для серверов Zerqon одного пользователя. Если и путь профиля содержит такие символы, укажите в `java_executable` установленную Java 25 с путём из ASCII-символов. Системная Java и PATH не меняются.

Перед запуском управляемой Java Zerqon сверяет размеры и SHA-256 файлов JRE со встроенным списком для закреплённой версии. Изменённый, отсутствующий или лишний файл останавливает запуск; существующие файлы кеша сохраняются. Для системной Java значение `java` использует только абсолютные каталоги PATH. Явно указанный путь `java_executable` выбирает администратор; хеши такой Java по списку управляемого кеша не проверяются.

Адрес и порт задаются только в `server.properties`, количество потоков — только в `server.settings`. Переопределений этих настроек через командную строку нет. Для служебных флагов Cargo нужен разделитель `--`: например, `cargo run --release -- --directory run`.

Проверка обновлений задаётся `update_check = true` и интервалом `update_check_interval_hours = 6` (1–168 часов). Она обращается к публичному GitHub без токена и не скачивает программы. О новой стабильной версии консоль и операторы получают одно английское уведомление со ссылкой; для игрока повторное уведомление возможно в новом сеансе или при появлении более новой версии. `update_check = false` полностью отключает запросы, а диагностический режим статуса их не запускает.

Команды консоли: `help`, `version`, `tps`, `plugins`, `op NAME`, `deop NAME`, `setblock X Y Z STATE-ID`, `stop`. Операторы определяются по UUID. Для `op NAME` игрок должен быть на сервере; `deop` принимает имя или UUID. Для команд ядра учитывается уровень оператора. Полный API прав ещё дорабатывается. `/tpsbar` отображается в игре. Rust-профилировщик работает на Linux и сохраняет отчёты spark в `profiles/`. Журнал `logs/latest.log` дописывается и автоматически не ротируется.
