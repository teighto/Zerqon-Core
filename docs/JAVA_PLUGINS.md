# Zerqon Java API / Java API Zerqon

Zerqon has its own Java plugin API alongside its Rust plugin API. The server engine and world remain Rust. Java plugins run in one optional Java 25 process and communicate with the Rust core through bounded JSON requests. The same host can load an experimental subset of original Paper 26.3 plugins when the libraries in `libraries/paper/` are on its classpath. The [Paper compatibility matrix](../tools/paper-bridge/README.md) lists what is implemented.

## Build and install

Requirements: Python 3.10+ and JDK 25. From the source checkout:

```sh
python scripts/build-java-plugins.py
```

The script compiles with `--release 25 -Xlint:all -Werror`, builds the example and runs real subprocess/JAR integration tests for both APIs. It verifies the pinned SHA-256 for Gson and the original Paper API dependencies, then stages the optional Paper libraries in `libraries/paper/`. Downloads and binaries are excluded from Git; the host never downloads dependencies at startup.

Outputs:

| File | Purpose |
|---|---|
| `libraries/zerqon-java-api-0.0.1.jar` | Compile-time SDK; no Gson dependency |
| `libraries/zerqon-java-host-0.0.1.jar` | Optional host, including SDK classes |
| `libraries/gson-2.13.2.jar` | Host JSON library |
| `libraries/paper/*.jar` | Optional pinned Paper API, dependencies and experimental bridge |
| `dist/hello-java-plugin-0.0.1.jar` | Example registering `/jhello` and `/jserver` |

Copy the example JAR into the server's `plugins/java-plugins/hello.jar`. Place the host and Gson JARs in the server's `libraries/` directory. `java_plugins = true` in `server.settings` enables the host when Java plugin files are present; `java_executable` selects Java 25. Rust plugins remain in `plugins/rust-plugins/`.

## Plugin contract

Implement `io.github.teighto.zerqon.api.Plugin` with a public no-argument constructor. The `onEvent(Event)` method returns `List<Action>`; use `List.of()` for no actions. The API uses immutable Java records and does not depend on Bukkit, Paper or Gson.

```java
package io.github.teighto.zerqon.plugins.greeting;

import io.github.teighto.zerqon.api.Action;
import io.github.teighto.zerqon.api.Event;
import io.github.teighto.zerqon.api.Plugin;
import java.util.List;

public final class Greeting implements Plugin {
    @Override
    public List<Action> onEvent(Event event) {
        return switch (event) {
            case Event.Enable ignored -> List.of(
                new Action.RegisterCommand("greet", "Send a greeting", null));
            case Event.Command command when command.command().equals("greet") ->
                List.of(new Action.Reply("Hello, " + command.sender() + "!"));
            default -> List.of();
        };
    }
}
```

Compile against the SDK, but do not bundle SDK classes in your plugin. Include a manifest ending with a blank line:

```text
Manifest-Version: 1.0
Zerqon-Plugin: io.github.teighto.zerqon.plugins.greeting.Greeting
Zerqon-API: 1

```

Example commands for the sample above, saved as `src/io/github/teighto/zerqon/plugins/greeting/Greeting.java`:

```sh
javac --release 25 -cp libraries/zerqon-java-api-0.0.1.jar -d build src/io/github/teighto/zerqon/plugins/greeting/Greeting.java
jar --create --file greeting.jar --manifest MANIFEST.MF -C build .
```

JAR file names contain 1–64 ASCII letters, digits, underscores or hyphens before `.jar`. Each JAR has its own classloader with the SDK as parent. Include private dependencies in your JAR, relocating packages if needed. Dependencies are not downloaded at plugin runtime, and inter-plugin class sharing is not part of this API.

## Events and actions

| Event | Data |
|---|---|
| `Event.Enable` | Zerqon server version |
| `Event.Disable` | Shutdown notification |
| `Event.Command` | Sender display name, `SenderKind`, trusted `operator` flag, command name, immutable argument list |
| `Event.Tick` | Nonnegative tick counter |
| `Event.PlayerJoin` | Player UUID string and name |
| `Event.PlayerQuit` | Player UUID string and name |

| Action | Behavior |
|---|---|
| `Action.Log` | INFO/WARN/ERROR message |
| `Action.Reply` | Respond to the current command sender |
| `Action.RegisterCommand` | Register a name, description and nullable permission during Enable |

Command names are lowercase ASCII letters, digits, underscores or hyphens. For this custom API, a non-null permission requires an operator or trusted console; `null` requests a public command. The Java host enforces that check using the trusted flags supplied by Rust. Paper commands use their own supported permission defaults in the adapter. A plugin cannot register commands from a tick or return a reply outside a command callback. The core checks command collisions before dispatch. Events are delivered only when the server sends them.

`command.senderKind()` is `Event.SenderKind.CONSOLE` or `PLAYER`; `command.operator()` reports the server's authorization decision. Never infer privileges from `command.sender()`: it is a display name, and a player may be named `CONSOLE`. Missing `operator` in the wire protocol defaults to false.

World changes, inventory and entity management, task scheduling and persistent storage are not available through the custom Java API yet. Unsupported operations fail explicitly.

Callbacks execute serially on the host thread. Constructor and callback thread context classloaders point to the plugin. Keep callbacks short and release resources during Disable. Do not retain thread-local data or spawn threads that outlive Disable. `System.out` is redirected to stderr, but return `Action.Log` for normal server logging.

The player remains registered during Join and Quit. Disconnecting while an accepted Join is running waits for its result before sending Quit. These events wait for queue space within a ten-second transaction deadline; execution has a two-second limit once the host starts the request, capped by the remaining transaction time. A Java request includes all plugin handlers for that event. Exceeding its execution limit terminates the Java host, so handlers must finish promptly.

## Native server queries

`NativeQueries.serverSnapshot()` reads an immutable snapshot from the Rust core. It is available inside synchronous callbacks, including Enable and Disable, when the host connection negotiates protocol 2. The plugin manifest remains `Zerqon-API: 1`; existing plugin interfaces and events are unchanged. Constructors and plugin-created threads cannot issue native queries.

```java
import io.github.teighto.zerqon.api.NativeQueries;

NativeQueries.ServerSnapshot server = NativeQueries.serverSnapshot();
String message = "Players " + server.onlinePlayers() + "/" + server.maxPlayers();
```

The snapshot contains `serverVersion()`, `minecraftVersion()`, `protocol()`, `maxPlayers()`, `onlinePlayers()` and `tick()`. `/jserver` in the example demonstrates the complete command path.

`NativeQueries.onlinePlayers()` returns immutable `PlayerSnapshot` records; `player(UUID)` selects the current session and `player(UUID, sessionId)` selects that exact session, returning an empty `Optional` when it no longer exists. Player data includes UUID, session/entity IDs, name, world UUID, position, operator status and online status. `NativeQueries.worlds()` returns immutable world identities, names/keys, dimensions, environments, height bounds and spawn positions. No mutation API is exposed.

Player/world query results are cached for the current host request and refreshed for the next one. A player list is limited to 1,024 entries, a world list to 16, and both remain subject to the transport byte limits. Oversized results fail explicitly rather than silently truncating the server state. Only Rust supplies player command and lifecycle context; a display name is never used to construct a Player identity.

Catch `NativeQueries.NativeQueryException` for a service error and inspect `code()`. `UNSUPPORTED` means native queries are unavailable on the legacy host protocol; `UNAVAILABLE` means the connected native service cannot currently answer. Other typed codes include `CAPACITY`, `CANCELLED` and `DEADLINE_EXCEEDED`. Errors do not become empty or invented snapshots. A malformed response terminates the Java host even if a plugin catches the resulting exception.

Java and Rust native queries use the same 32 MiB reservation pool. Each active query workspace reserves 2 MiB. A completed service error can be handled by the plugin. If an executing provider exceeds the callback deadline, the Java host is stopped and the workspace remains reserved until that work finishes; a timeout does not make its memory immediately reusable. These reservations cover the query workspace, not the JVM heap or total process RAM.

Each native query has a 250 ms deadline, capped by the time remaining on the original Rust request; making another query does not extend that parent deadline. One host request, including all plugin callbacks it dispatches, permits at most 128 native calls. A call frame is limited to 64 KiB, a result frame to 256 KiB and all result frames together to 1 MiB. Avoid polling the same snapshot repeatedly within one callback.

## Isolation and limits

Java plugins are trusted code with the operating-system permissions of the host process. Classloader separation and the subprocess boundary do not form a security sandbox. Plugins can use Java file/network APIs. Install reviewed plugins and run the server under an account with appropriate permissions.

The host accepts up to 64 JARs, each at most 16 MiB. A callback may return up to 32 actions and 64 KiB serialized action data. Replies and individual log messages are limited to 4,096 UTF-8 bytes, descriptions to 256 bytes and permission strings to 128 bytes. Unknown JSON fields, malformed Unicode and duplicate JSON fields are rejected. Plugin exceptions, linkage errors, assertion failures or invalid actions disable the offending plugin; that callback's actions are discarded.

The Rust bridge applies a request deadline. A hung callback or broken host process ends the entire optional JVM, disabling all Java plugins in that process while the Rust core handles the failure. There is no safe per-plugin preemption in Java 25. JVM heap limits apply to all Java plugins together, not to Rust core memory. Forced termination cannot guarantee Disable callbacks or rollback plugin filesystem/network side effects.

## Host protocol

UTF-8 JSON Lines on stdin/stdout, with no unsolicited startup output. Legacy protocol 1 is the default and accepts an unsigned 64-bit `id`, echoed exactly in the response. The line limit is 65,536 bytes; the response limit is 8 MiB.

The Rust host explicitly negotiates protocol 2 before loading plugins to enable native queries. Protocol 2 uses positive IDs up to `Long.MAX_VALUE`; native-query replies are handled inside the current request. Plugin authors use `NativeQueries` rather than writing transport messages. Ordinary event responses retain their protocol 1 shape.

```json
{"id":1,"op":"load","directory":"/srv/zerqon/plugins/java-plugins","server_version":"0.0.1"}
{"id":2,"op":"event","plugin":"hello","event":{"event":"command","sender":"console","sender_kind":"console","operator":true,"command":"jhello","args":[]}}
{"id":3,"op":"list"}
{"id":4,"op":"shutdown"}
```

Omitting `plugin` broadcasts an event to enabled plugins. Load invokes Enable; shutdown invokes Disable, closes classloaders and exits after its response. Responses are:

```json
{"id":2,"ok":true,"results":[{"name":"hello","enabled":true,"actions":[{"action":"reply","message":"Hello"}]}]}
```

An operation-level failure sets `ok:false` and `error`; a plugin-level failure appears in that result's `error`, with `enabled:false`. Duplicate loading leaves an existing plugin intact and returns an error for that entry. Logs go only to stderr. An invalid or oversized protocol stream can terminate the host.

Direct diagnostic launch uses `java -Xms16M -Xmx128M -cp "libraries/zerqon-java-host-0.0.1.jar:./libraries/paper/*:./libraries/gson-2.13.2.jar" io.github.teighto.zerqon.host.PluginHost`. On Windows, use `;` between classpath entries. Leave out `libraries/paper/*` to run only the custom API. Paper's pinned Gson takes precedence when the optional libraries are present.

## Русский

Native-запросы Java и Rust используют общий резерв памяти 32 МиБ, по 2 МиБ на активную рабочую область. Завершённую ошибку сервиса плагин может обработать. Если выполняющийся запрос выходит за срок обработчика, Java-хост останавливается, а его резерв освобождается только после завершения принятой работы. Этот лимит не включает кучу JVM и остальную память процесса.

У Zerqon собственный Java API. Ядро и мир работают на Rust; Java-плагины исполняются в одном дополнительном процессе Java 25. Этот же host распознаёт `plugin.yml` и запускает проверенную часть Paper 26.3 API при наличии `libraries/paper/` в classpath. Полной совместимости Paper пока нет; ограничения указаны в матрице. Папка Java-плагинов — `plugins/java-plugins/`, для Rust — `plugins/rust-plugins/`.

Сборка: `python scripts/build-java-plugins.py`. Она проверяет SHA-256 библиотеки Gson, компилирует SDK, хост и пример, запускает интеграционные тесты с настоящими JAR и JVM. Скопируйте `dist/hello-java-plugin-0.0.1.jar` в `plugins/java-plugins/hello.jar`, хост и Gson — в `libraries/`. Настройка `java_plugins = true` в `server.settings` включает хост при наличии JAR; `java_executable` задаёт Java 25. Пример добавляет `/jhello` и `/jserver`.

Плагин собственного API реализует `Plugin.onEvent(Event)` и возвращает действия `Action`. В манифесте обязательны `Zerqon-Plugin` с именем класса и `Zerqon-API: 1`. Есть события включения, выключения, команд, тиков и входа/выхода игроков. Действия — журнал, ответ на команду и регистрация команды. Java-host проверяет права по доверенным признакам `senderKind` и `operator`, полученным от Rust; конфликты команд проверяет ядро.

`senderKind()` различает консоль и игрока. Строка `sender()` — отображаемое имя; игрок с ником `CONSOLE` остаётся игроком. Не проверяйте привилегии по этому тексту.

`NativeQueries.serverSnapshot()` возвращает неизменяемый снимок состояния Rust-ядра: версии сервера и Minecraft, номер протокола, число игроков, лимит игроков и номер тика. Метод доступен только внутри синхронного обработчика, включая Enable и Disable, после согласования протокола хоста 2. В манифесте остаётся `Zerqon-API: 1`. Конструктор и созданные плагином потоки не могут делать такие запросы. Пример `/jserver` показывает полный рабочий сценарий; при старом протоколе он сообщает о недоступности запросов.

`NativeQueries.onlinePlayers()` возвращает неизменяемые данные игроков, `player(UUID)` — текущую сессию, `player(UUID, sessionId)` — строго указанную сессию или пустой `Optional`. В данных есть UUID, ID сессии и сущности, имя, UUID мира, положение, признак оператора и онлайн-статус. `worlds()` возвращает идентификаторы миров, имена/ключи, измерения, окружение, границы высоты и точки появления. Результаты чтения игроков и миров кэшируются на один вызов хоста; следующие вызовы получают актуальные данные. Максимумы — 1 024 игрока и 16 миров с сохранением ограничений размера сообщений. API не изменяет игровое состояние.

Ошибка сервиса представлена `NativeQueries.NativeQueryException`: `code()` возвращает `UNSUPPORTED` для старого протокола, `UNAVAILABLE` для недоступного сервиса или другой конкретный код. Ошибки не подменяются пустыми снимками. Один запрос ограничен 250 мс и оставшимся временем исходного вызова Rust; новые запросы не продлевают общий дедлайн. На один вызов хоста, включая все обработчики плагинов, разрешено до 128 вложенных запросов; пределы сообщения запроса, ответа и суммы ответов — 64 КиБ, 256 КиБ и 1 МиБ. Повреждение протокола завершает Java-хост. Новый API только читает сведения о сервере.

Собственный Java API пока не поддерживает изменение мира, управление инвентарями и сущностями, планировщик задач и постоянное хранилище. Во время Join и Quit игрок остаётся в реестре сервера. При отключении во время принятого Join сервер сначала ждёт его завершения, затем отправляет Quit. На ожидание очереди и доставку этих событий отведено до десяти секунд. После начала запроса все Java-обработчики вместе должны уложиться в две секунды и оставшееся время события. Обработчики последовательные; долгие операции блокируют Java-хост. При исключении отключается плагин, при зависании таймаут Rust-моста завершает весь Java-процесс. Java-плагины имеют права пользователя ОС и не изолированы как WASM; лимит JVM относится ко всем Java-плагинам вместе. Остановка процесса не отменяет уже выполненные внешние действия плагина.

Проверенный пример и его манифест находятся в `examples/hello-java-plugin/`. SDK не требует Gson при компиляции плагинов.
