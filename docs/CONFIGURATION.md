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
└── world/
    ├── chunks.redb        # chunks, inventories and player modes
    └── playerdata/        # preserved inventory files from older builds, if present
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
| `gamemode` | `creative` | Initial mode for new players: `creative` or `survival`; existing players keep their saved mode |
| `view-distance` | `4` | Chunk view radius, 1–32 |
| `network-compression-threshold` | `256` | Compression threshold in bytes; `-1` disables it |
| `level-name` | `world` | World subdirectory name; path separators are rejected |
| `level-type` | `normal` in newly created files | `normal` or `flat`; older files without this key retain `flat` |
| `level-seed` | `0` | Unsigned 64-bit seed, 0–18,446,744,073,709,551,615 |

The parser accepts `key=value` lines and `#`/`!` comments. Unknown and duplicate keys are rejected. It does not support Java `.properties` escapes or continued lines. Configuration files are limited to 1 MiB.

`level-type=normal` selects the native Rust terrain generator: hills, mountains, oceans, rivers, climate biomes, caves, ores and trees. It is a custom initial generator; the same seed does not reproduce a vanilla Minecraft world. A normal world keeps its original generator type and seed; changing them is rejected. To create normal terrain alongside an existing flat or imported world, choose a new unused `level-name`. Existing worlds are preserved and are not converted. Keep `normal-generator.json` and `IMPORT_INCOMPLETE` with the world backup; the latter also prevents older builds from extending it with flat chunks.

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

`/give PLAYER minecraft:ITEM [COUNT]` gives items to a connected player and requires operator level 2. The default count is 1; the limit is 4,096. If the full amount does not fit, nothing is added. Inventories and player modes are saved by UUID in the world's `chunks.redb` database.

`/speed LEVEL [PLAYER]` changes flight speed through the temporary built-in Rust plugin `ZerqonSpeed`. It requires operator level 2 and accepts levels 1–10; level 1 restores the vanilla speed of 0.05. Without a player name it targets the sender; the console must supply a connected player. Flight permission follows the game mode. The speed resets on reconnect.

The personal 2×2 grid supports 478 ordinary recipes, shift crafting, hotbar swaps and ingredient remainders. Closing the menu or disconnecting drops any overflow as persistent world items in the same transaction as the inventory change. Special recipes, containers and 3×3 crafting remain unfinished.

An older `playerdata/UUID.zinv` is imported on that player's first login. Its grid, cursor and item components are preserved, and its mode remains Creative. The original file stays unchanged; later logins use the database. A legacy file with a nonempty saved result slot is rejected and preserved for explicit recovery. Do not delete that file to bypass the check.

Creative is the default for new players. In Creative, ordinary blocks, logs, dry slabs and stairs can be placed and broken, including stair connections across chunk boundaries. Placement checks the held item, range and entity collisions. `gamemode=survival` enables ordinary inventory actions and personal crafting from items supplied with `/give`; it refuses creative item creation and flight requests. Basic Survival mining and placement are available. Hunger and full loot rules are still being developed. TNT damage, death and respawn work during the player session; health is not saved across reconnects yet. Runtime `/gamemode` changes, fluids, random block ticks and interactive containers are not implemented yet. See [world storage](WORLD_STORAGE.md) before restoring a backup or moving to an older build.

`/tps` reports the recent coordinator tick rate and average measured tick work. The built-in Rust CPU profiler uses `/spark profiler start` and `/spark profiler stop` on Linux. It saves reports in `profiles/`; see [Profiling](PROFILING.md). The append-only `logs/latest.log` is not automatically rotated. `RUST_LOG` selects log verbosity, for example `info` or `zerqon=debug`.

## Русский

| Генерация в `server.properties` | Значение | Назначение |
|---|---|---|
| `level-name` | `world` | Подкаталог мира; для нового мира укажите ещё не занятое имя |
| `level-type` | `normal` в новых файлах | `normal` или `flat`; отсутствие ключа в старом конфиге сохраняет `flat` |
| `level-seed` | `0` | Целое число без знака от 0 до 18 446 744 073 709 551 615 |

`normal` включает нативный Rust-генератор холмов, гор, океанов, рек, климатических биомов, пещер, руд и деревьев. Это первоначальный собственный генератор: совпадение seed не воспроизводит мир vanilla Minecraft. Тип генератора и seed уже созданного обычного мира менять нельзя. Для перехода с существующего плоского или импортированного мира выберите новый свободный `level-name`; прежний мир сохраняется без преобразования. Вместе с резервной копией мира сохраняйте `normal-generator.json` и `IMPORT_INCOMPLETE`: последний также защищает мир от генерации плоских чанков старой сборкой.

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

Команда `/give ИГРОК minecraft:ПРЕДМЕТ [КОЛИЧЕСТВО]` выдаёт предметы подключённому игроку; нужен второй уровень оператора. По умолчанию выдаётся один предмет, максимум 4 096. Если места не хватает на всё количество, команда ничего не добавляет. Инвентарь и режим игрока сохраняются по UUID в `world/chunks.redb`.

В личной сетке 2×2 работают 478 обычных рецептов. Поддержаны Shift-крафт, обмен с хотбаром и остатки ингредиентов. Результат пересчитывается после входа. При переполнении закрытие меню и выход создают предметы в мире; они сохраняются вместе с изменением инвентаря. Специальные рецепты, контейнеры и крафт 3×3 ещё не реализованы.

Старый файл `playerdata/UUID.zinv` переносится в базу при первом входе игрока. Сетка, курсор и компоненты предметов сохраняются; режим остаётся творческим. Исходный файл не меняется, следующие входы используют запись в базе. Если старый файл содержит предмет в выходном слоте крафта, загрузка останавливается для отдельного восстановления. Удалять его для обхода проверки не нужно.

Новые игроки по умолчанию входят в Creative. `gamemode=survival` в `server.properties` задаёт Survival для новых игроков; ранее сохранённый режим не меняется. Работают добыча и установка обычных блоков, расход предметов, износ инструмента, крафт 2×2, выбрасывание и подбор. Сервер проверяет режим, дистанцию и владение предметами. Голод, контейнеры, крафт 3×3, полная симуляция жидкостей и случайные тики ещё в работе.

`cargo run --release -- --init` создаёт конфиги и завершает работу, `cargo run --release -- --check-config` проверяет значения без запуска сети. Настройки применяются после перезапуска. `server.properties` содержит адрес, порт, MOTD, онлайн-проверку аккаунтов и параметры Minecraft. `server.settings` — TOML с настройками потоков, кеша, очередей и плагинов. Неизвестные и повторяющиеся ключи отвергаются.

Для папки `run` используйте `--directory run`. Игровые данные подготовятся автоматически в `run/.cache/vanilla/26.3`; Java-библиотеки из дистрибутива должны находиться в `run/libraries/`. Плагины устанавливаются в `plugins/rust-plugins/` и `plugins/java-plugins/` внутри выбранной папки.

Java обычно скачивается в `.cache/java/`. Уже установленная рабочая копия используется повторно, даже при отключённой загрузке. Для новой загрузки на Windows, если путь содержит символы вне ASCII, используется `%LOCALAPPDATA%\Zerqon\cache\java`: запускатор Java 25 не всегда умеет работать с такими путями. Этот кеш общий для серверов Zerqon одного пользователя. Если и путь профиля содержит такие символы, укажите в `java_executable` установленную Java 25 с путём из ASCII-символов. Системная Java и PATH не меняются.

Перед запуском управляемой Java Zerqon сверяет размеры и SHA-256 файлов JRE со встроенным списком для закреплённой версии. Изменённый, отсутствующий или лишний файл останавливает запуск; существующие файлы кеша сохраняются. Для системной Java значение `java` использует только абсолютные каталоги PATH. Явно указанный путь `java_executable` выбирает администратор; хеши такой Java по списку управляемого кеша не проверяются.

Адрес и порт задаются только в `server.properties`, количество потоков — только в `server.settings`. Переопределений этих настроек через командную строку нет. Для служебных флагов Cargo нужен разделитель `--`: например, `cargo run --release -- --directory run`.

Проверка обновлений задаётся `update_check = true` и интервалом `update_check_interval_hours = 6` (1–168 часов). Она обращается к публичному GitHub без токена и не скачивает программы. О новой стабильной версии консоль и операторы получают одно английское уведомление со ссылкой; для игрока повторное уведомление возможно в новом сеансе или при появлении более новой версии. `update_check = false` полностью отключает запросы, а диагностический режим статуса их не запускает.

Команды консоли: `help`, `version`, `tps`, `plugins`, `op NAME`, `deop NAME`, `setblock X Y Z STATE-ID`, `stop`. Операторы определяются по UUID. Для `op NAME` игрок должен быть на сервере; `deop` принимает имя или UUID. Для команд ядра учитывается уровень оператора. Полный API прав ещё дорабатывается. `/tpsbar` отображается в игре. Rust-профилировщик работает на Linux и сохраняет отчёты spark в `profiles/`. Журнал `logs/latest.log` дописывается и автоматически не ротируется.

`/speed УРОВЕНЬ [ИГРОК]` — команда временного встроенного Rust-плагина `ZerqonSpeed`. Нужен уровень оператора 2; допустимы значения 1–10. Значение 1 возвращает стандартную скорость полёта 0,05. Без имени команда действует на отправителя, а из консоли нужно указать подключённого игрока. Право полёта зависит от режима игры. При переподключении скорость сбрасывается.

## ZerqonEdit

TNT can also be lit by right-clicking it with flint and steel or a fire charge in either hand. Survival uses one durability point or one fire charge; Creative does not consume either. The TNT block and held-item change are saved together. Creative breaking supports natural terrain such as sand, gravel and leaves, including beside other natural blocks or TNT. Falling sand, leaf decay and general fire placement remain unfinished.

TNT можно поджечь правой кнопкой зажигалкой или огненным зарядом в любой руке. В Survival тратится одна единица прочности либо один огненный заряд; в Creative расхода нет. Изменение блока и предмета сохраняется одной операцией. В Creative исправлено ломание природных блоков — в том числе песка, гравия и листвы рядом с другими природными блоками или TNT. Падение песка, гниение листвы и обычная установка огня ещё не реализованы.

The built-in Rust plugin provides `//pos1 [x y z]`, `//pos2 [x y z]`, `//size`, `//set block`, `//replace from-block to-block` and `//desel`. It requires operator level 2. Without coordinates, a corner uses the player's current position. Names such as `stone`, `tnt` and `minecraft:oak_log` use their default block states. Edits are atomic and limited to 65,536 blocks across 16 chunks. Selections reset when the player disconnects. This is Zerqon's editor, not the EngineHub WorldEdit plugin; brushes, schematics and undo are not implemented.

`//ignite` primes TNT in the selection, with a four-second fuse at 20 TPS and a limit of 4,096 active charges. `//tntstatus` reports active, ready and detonated counts. `tnt_explosions_per_tick = 32` in `server.settings` controls the work per tick (1–4096); compare the backlog and total completion time alongside TPS. `//tntcancel` removes pending charges. Primed charges are not restored as blocks and do not survive shutdown.

TNT now falls under gravity, collides with static block shapes and explodes at its current position. Blasts persist terrain changes and trigger nearby TNT. Mobs and Survival players receive damage and knockback based on distance and exposure through the pre-blast terrain; Creative players are immune. Survival players can respawn after death, with three seconds of protection. Mob damage persists; player health is currently session-only and resets on reconnect. Fluid currents, special slime restitution, armor/enchantment reductions, drops, redstone ignition and vanilla block-resistance rules remain unfinished. Unsupported or unavailable collision geometry pauses TNT and its fuse; `//tntstatus` includes the paused count. This is not a full vanilla TNT performance comparison.

Встроенный Rust-плагин поддерживает `//pos1 [x y z]`, `//pos2 [x y z]`, `//size`, `//set блок`, `//replace исходный-блок новый-блок` и `//desel`. Нужен уровень оператора 2. Без координат угол берётся из текущего положения игрока. Имена блоков принимаются с `minecraft:` или без него. За одну операцию можно изменить до 65 536 блоков в 16 чанках; запись атомарная. Выделение сбрасывается при выходе. Это собственный редактор Zerqon, а не EngineHub WorldEdit: кистей, схем и отмены пока нет.

Пример области из 4096 TNT / example with 4096 TNT:

```text
//pos1 0 100 0
//pos2 15 115 15
//set tnt
//ignite
//tntstatus
```

`//ignite` поджигает TNT в выделении, запал — 4 секунды при 20 TPS. Одновременно допускается 4096 зарядов. В `server.settings` настройка `tnt_explosions_per_tick = 32` задаёт число взрывов за тик (1–4096). При замере смотрите и TPS, и очередь `ready`, и время завершения всей нагрузки. `//tntcancel` убирает ожидающие заряды, не возвращая их блоки. При остановке сервера запущенные заряды не сохраняются.

TNT падает под действием гравитации, сталкивается со статическими формами блоков и взрывается в текущей точке. Взрыв сохраняет разрушения и запускает соседние TNT. Мобы и игроки Survival получают урон и отбрасывание с учётом расстояния и укрытий до разрушения блоков; Creative защищён. После смерти в Survival работает возрождение с защитой на три секунды. Урон мобам сохраняется; здоровье игрока пока действует в пределах сеанса и сбрасывается при переподключении. Течения жидкости, особый отскок от слизи, броня и чары, дроп, редстоун-поджиг и точные правила прочности блоков vanilla ещё не реализованы. При недоступной или неподдерживаемой форме столкновения TNT и запал приостанавливаются; число таких зарядов видно в `//tntstatus`. Это ещё не полное воспроизведение TNT vanilla.


## Gameplay framework / Игровой каркас

Survival players can mine ordinary terrain and place blocks. Mining uses the official 26.3 hardness and tool tags; the server checks elapsed digging time, range, held item and current block before committing. Mining creates item entities with a pickup delay. A full inventory leaves the items in the world. Tools lose durability, and placement consumes one held block. Fortune, Silk Touch and complete loot tables are not implemented.

Pigs and cows follow players holding their food within ten blocks: wheat for cows, carrots/potatoes/beetroots for pigs. Right-click an eligible adult with its food to enter love mode for 30 seconds. Two animals of the same species approach each other and breed after three seconds of courtship when within three blocks. Survival consumes one food item; Creative does not. Parents then have a five-minute cooldown. Babies take 20 minutes of simulated time to grow; feeding accelerates growth using the 26.3 formula. Feeding does not heal animals. Zombies pursue and attack living Survival players; Creative players are ignored. Navigation routes around static obstacles, jumps one-block steps and avoids large drops. Animals flee after being hit and separate when crowded. Unloaded or unsupported terrain stops movement. Path searches are bounded; this is not a full vanilla AI port.

В Survival можно добывать обычные блоки и строить. Время добычи и требования к инструменту взяты из данных 26.3. Сервер проверяет время, дистанцию, предмет в руке и состояние блока. После добычи предмет появляется в мире с задержкой подбора. Если инвентарь полон, предмет остаётся лежать. Инструмент изнашивается, при установке расходуется один блок. Fortune, Silk Touch и полные таблицы добычи пока не реализованы.

Коровы идут за пшеницей, свиньи — за морковью, картошкой или свёклой в радиусе десяти блоков. ПКМ с кормом включает готовность взрослого животного к размножению на 30 секунд. Два животных одного вида подходят друг к другу; после трёх секунд ухаживания в пределах трёх блоков появляется детёныш. В Survival расходуется один предмет, в Creative расхода нет. У родителей начинается пауза на пять минут. Детёныш растёт 20 минут активной симуляции; корм ускоряет рост по формуле 26.3. Здоровье корм больше не восстанавливает. Зомби преследуют и атакуют живых игроков Survival, но игнорируют Creative. Мобы обходят статические препятствия, перепрыгивают ступень высотой в блок и избегают больших обрывов. Животные убегают после удара и расходятся при скоплении. На недоступной или неподдерживаемой геометрии движение останавливается. Полный перенос ванильного AI, голод, контейнеры и крафт 3×3 остаются в работе.


Animal snapshots now use schema 2, storing age, forced age and love time. Existing schema 1 adults load automatically. Back up `world/entities.zqe` before updating; older Zerqon builds cannot read newly saved animal snapshots. Timers stop when the entities are not simulated. Baby collision boxes and navigation use half size. Breeding experience orbs and inherited variants remain unfinished.

Состояние животных теперь сохраняется в схеме 2: возраст, ускоренный кормлением рост и время готовности к размножению. Старые взрослые животные из схемы 1 загружаются автоматически. Перед обновлением сохраните копию `world/entities.zqe`: старые сборки Zerqon не прочитают обновлённый файл. Когда сущности не симулируются, таймеры стоят. Размер коллизии и навигация детёнышей уменьшены вдвое. Сферы опыта за размножение и наследование вариантов животных ещё не реализованы.


## Building and items / Строительство и предметы

Ordinary building no longer rejects an edit just because water, a chest or another unrelated block is next to it. Wooden/copper doors, trapdoors and fence gates open by right-click; Shift uses the held block instead. Doors place and break both halves together. Ground/wall torches and waterlogged slabs/stairs have dedicated placement rules. Inventory-bearing blocks and full redstone/neighbor simulation remain unfinished.

Q drops one held item; Ctrl+Q drops the stack. Inventory throws, crafting overflow and disconnect overflow create real item entities. Items fall, collide, merge compatible stacks and can be picked up by another player. Thrower attribution does not make an item private. Pickup, partial pickup, merges and deletion are durable. Item stacks, motion, remaining pickup delay and age survive restart. Despawn is after 6,000 simulated ticks; unloaded terrain pauses simulation. Ownership/count changes are saved immediately; motion is checkpointed every 100 ticks and at graceful shutdown.

`max_item_entities = 2048` and `item_memory_mib = 32` in `server.settings` bound active items and their accounted payload memory. These are not a total process-memory limit. Hitting the cap rejects the creating action without spending its items. `/tps` also reports item count. Basic water/lava behavior is present; fluid currents, all special block effects and explosion loot remain incomplete.

Установка больше не отклоняется только из-за воды, сундука или другого соседнего блока. Деревянные/медные двери, люки и калитки открываются правой кнопкой. Shift позволяет вместо открытия поставить блок рядом. Обе половины двери ставятся и ломаются вместе. Добавлены правила установки обычных/настенных факелов и плит/лестниц в воде. Интерфейсы контейнеров, полная редстоун-схема и все обновления соседей ещё не готовы.

Q выбрасывает один предмет, Ctrl+Q — стак. Выбрасывание из инвентаря, переполнение при крафте и выходе создают настоящие предметы в мире. Они падают, сталкиваются с блоками, объединяются и подбираются, в том числе другими игроками. Подбор, остаток после частичного подбора и удаление сохраняются одной операцией с инвентарём. Стак, движение, задержка подбора и возраст переживают перезапуск. Исчезновение — через 6 000 тиков симуляции. Количество и владелец сохраняются сразу; положение — раз в 100 тиков и при штатной остановке.

В `server.settings` параметры `max_item_entities = 2048` и `item_memory_mib = 32` ограничивают число предметов и учитываемую память их записей. Это не ограничение всей памяти процесса. При достижении лимита создающее предметы действие отклоняется без расхода вещей. `/tps` показывает количество предметов. Течения, все особые эффекты блоков и добыча после взрывов ещё требуют работы.
