# Experimental Paper bridge / Экспериментальный Paper bridge

## English

This JVM adapter runs a limited, tested subset of the original Paper **26.3.build.242-stable** API. Game state and networking belong to Rust. Zerqon's Java host accepts these plugins alongside its custom Java SDK through the same bounded JSON protocol. It is not a complete Paper replacement.

Build with Python 3 and JDK 25:

```text
python tools/paper-bridge/build.py --java-home /path/to/jdk-25
```

The build downloads 50 exact dependencies from Paper's official Maven repository into `.cache/libraries/paper-26.3/`, verifies every SHA-256 against `dependencies.lock.json`, compiles fixtures against the official API without the bridge on their compilation classpath, then runs the process test. Nothing is downloaded by the adapter at runtime. The tested library is copied to `dist/zerqon-paper-bridge-0.0.1.jar` only after successful checks. Runtime classpaths need the pinned dependency JARs as well as the bridge.

`python scripts/build-java-plugins.py --java-home /path/to/jdk-25` builds the integrated host, stages all optional JARs in `libraries/paper/` and tests real Paper plugins over host JSON. JARs containing `plugin.yml` use this adapter. Hard dependencies are loaded before dependents; cycles and missing dependencies fail explicitly. Plain command names and aliases are exported to Rust with their permission metadata. Native namespaced command routing is not yet exported. Protocol 2 supplies authoritative Player context for commands and join/quit events; legacy events without that context cannot fabricate a Player from its name.

| Area | Implemented and tested | Not implemented |
| --- | --- | --- |
| Loading | `JavaPlugin`, configured classloader, service providers, bounded `plugin.yml`, duplicate/missing/circular-dependency rejection, host hard-dependency ordering, classes from loaded hard dependencies | `paper-plugin.yml`, soft-dependency ordering, Maven library loading, legacy API transformation |
| Lifecycle | `onLoad`, `onEnable`, `onDisable`, enable/disable events, failed-enable rollback, classloader cleanup | Modern Paper lifecycle registrars/bootstrap |
| Configuration | Original Bukkit YAML API, bundled `config.yml`, load/save; data under `config/<PluginName>/` | Sandboxing arbitrary plugin filesystem access |
| Commands | `plugin.yml` commands, aliases, namespaced commands, `PluginCommand` executors, output to the calling sender | Brigadier command trees and modern command registration |
| Permissions | Direct permission defaults (`true`, `false`, `op`, `not op`), authenticated console distinction | Permission attachments, permission inheritance/children and provider plugins |
| Scheduler | Synchronous delayed/repeating tasks, task IDs, queries, cancellation, `callSyncMethod` future cancellation, supplied increasing tick clock | Asynchronous and region/entity schedulers; cross-thread scheduling |
| Events | Synchronous custom events, handler priorities, cancellation/ignoreCancelled, removal on disable, native PlayerJoinEvent/PlayerQuitEvent once per host event | Other gameplay events, join/quit message broadcasts, asynchronous events |
| Player reads | UUID/entity ID/name, current presence, session validity/location/world, operator flag and direct permission defaults, UUID lookup, exact-name and shortest-prefix lookup | Inventory/health/game-mode APIs, offline permissions, teleport and other mutations |
| World reads | UUID/name/key/environment, spawn Location copies, height bounds, online players, world lookup | Blocks/chunks, time/weather/seed APIs, world mutations |
| Other game state | Named `UnsupportedOperationException` for unimplemented calls | NMS/CraftBukkit and full Paper plugin parity |

The host creates one `PaperBridge(serverDirectory)` per JVM, calls `load(jar)` in dependency order, `enable(plugin)`, `tick(tickNumber)`, `execute(senderName, isOperator, isConsole, commandLine)`, then `disable(plugin)` or `close()`. Callbacks run on the bridge's creating thread. `isConsole` must come from the trusted host; the player name `CONSOLE` does not grant privileges. Plugins are trusted executable code, not sandboxed applications. A host must isolate and bound their execution separately.

Player equality and hashing follow Paper's UUID contract. Each session still has its own proxy and entity ID: retaining an old proxy cannot retarget its location to a reconnected session. `isOnline()` follows current UUID presence; `isValid()` remains tied to the original session. During PlayerQuitEvent the player remains online, as in the pinned Paper implementation. Once the event completes, the proxy retains its last observed location. Player/world query data is cached only within a host request; the online-player collection reflects subsequent requests. Proxy indexes are bounded to 2,048 player sessions and 16 worlds. Messages are supported only to the actual current command sender, checked by UUID and session; sending to another player or from join/quit listeners is explicitly unsupported. Quit reasons currently use Paper's default `DISCONNECTED` value.

## Русский

Адаптер исполняет проверенную часть оригинального Paper API **26.3.build.242-stable**. Игровое состояние и сеть остаются в Rust. Java-host загружает такие плагины рядом с плагинами собственного SDK Zerqon и передаёт команды/тики через общий JSON-протокол. Полная совместимость с Paper не заявлена.

Сборка командой выше загружает закреплённые зависимости из официального Maven-репозитория Paper, сверяет SHA-256, компилирует тестовые плагины только против настоящего Paper API и выполняет проверки. Кэш находится в `.cache/libraries/paper-26.3/`; во время работы адаптера загрузок нет. Итоговый JAR попадает в `dist/` после успешного теста.

Общая сборка `scripts/build-java-plugins.py` размещает библиотеки в `libraries/paper/` и проверяет Paper-плагины через настоящий процесс host. Он распознаёт `plugin.yml`, упорядочивает обязательные зависимости, экспортирует обычные команды и алиасы. Native-маршрутизация команд с namespace пока не экспортируется. Протокол 2 передаёт подтверждённые Rust данные Player для команд и событий входа/выхода; старые события без такого контекста не создают Player по одному имени.

Проверены жизненный цикл `JavaPlugin`, сохранение YAML, команды и алиасы, базовые права, синхронные задачи с реальным tick clock, отмена future, события с приоритетами и отменой, классы зависимостей и очистка после ошибки. Добавлены чтение UUID/ID сущности/имени/положения/мира/операторского статуса игроков, поиск игроков и миров, точки появления и границы высоты. PlayerJoinEvent и PlayerQuitEvent вызываются один раз на событие хоста. Во время QuitEvent игрок ещё онлайн. Равенство Player определяется UUID; прежний объект после переподключения сохраняет свою сессию и последнее известное положение. Кэш индексов ограничен 2 048 сессиями и 16 мирами.

Ответы доступны только текущему отправителю команды с проверкой UUID и сессии. Отправка другому игроку, сообщения из обработчиков входа/выхода, телепортация, изменение мира, инвентари и offline-права пока явно отклоняются. Изменение текста join/quit-события ещё не отправляет сообщение игрокам; причина выхода пока использует стандартное значение Paper `DISCONNECTED`. Современный Paper bootstrap, остальные игровые API, наследование прав, async/region scheduler и NMS требуют дальнейшей реализации.

Host создаёт один `PaperBridge` на JVM, загружает зависимости до использующих их плагинов и вызывает API с одного потока. Признак консоли берётся из доверенного источника, не из ника игрока. Плагины имеют права обычного Java-кода; для ограничения времени и ресурсов требуется изоляция со стороны host.
