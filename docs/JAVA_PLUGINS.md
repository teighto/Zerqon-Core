# Zerqon Java API / Java API Zerqon

Zerqon has its own Java plugin API alongside its Rust plugin API. The server engine and world remain Rust. Java plugins run in one optional Java 25 process and communicate with the Rust core through bounded JSON requests. This API is separate from Paper/Bukkit compatibility; a Paper JAR is rejected explicitly by this host.

## Build and install

Requirements: Python 3.10+ and JDK 25. From the source checkout:

```sh
python scripts/build-java-plugins.py
```

The script compiles with `--release 25 -Xlint:all -Werror`, builds the example and runs real subprocess/JAR integration tests. It downloads Google's Gson 2.13.2 from Maven Central into `libraries/` and checks the pinned SHA-256 before use. Downloads and binaries are excluded from Git.

Outputs:

| File | Purpose |
|---|---|
| `libraries/zerqon-java-api-0.0.1.jar` | Compile-time SDK; no Gson dependency |
| `libraries/zerqon-java-host-0.0.1.jar` | Optional host, including SDK classes |
| `libraries/gson-2.13.2.jar` | Host JSON library |
| `dist/hello-java-plugin-0.0.1.jar` | Example registering `/jhello` |

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
| `Event.Command` | Sender display name, `SenderKind`, command name, immutable argument list |
| `Event.Tick` | Nonnegative tick counter |
| `Event.PlayerJoin` | Player UUID string and name |
| `Event.PlayerQuit` | Player UUID string and name |

| Action | Behavior |
|---|---|
| `Action.Log` | INFO/WARN/ERROR message |
| `Action.Reply` | Respond to the current command sender |
| `Action.RegisterCommand` | Register a name, description and nullable permission during Enable |

Command names are lowercase ASCII letters, digits, underscores or hyphens. A non-null permission is a requirement that the Rust command dispatcher must enforce; `null` requests a public command. A plugin cannot register commands from a tick or return a reply outside a command callback. The core checks command collisions and permissions before dispatch. Events are delivered only when the server sends them.

`command.senderKind()` is `Event.SenderKind.CONSOLE` or `PLAYER`. Never infer privileges from `command.sender()`: it is a display name, and a player may be named `CONSOLE`. Use the explicit sender kind and server-enforced permissions.

ABI 1 does not yet provide world, inventory, entity manipulation, a task scheduler or a persistent storage API. These capabilities are not represented by successful no-ops.

Callbacks execute serially on the host thread. Constructor and callback thread context classloaders point to the plugin. Keep callbacks short and release resources during Disable. Do not retain thread-local data or spawn threads that outlive Disable. `System.out` is redirected to stderr, but return `Action.Log` for normal server logging.

## Isolation and limits

Java plugins are trusted code with the operating-system permissions of the host process. Classloader separation and the subprocess boundary do not form a security sandbox. Plugins can use Java file/network APIs. Install reviewed plugins and run the server under an account with appropriate permissions.

The host accepts up to 64 JARs, each at most 16 MiB. A callback may return up to 32 actions and 64 KiB serialized action data. Replies and individual log messages are limited to 4,096 UTF-8 bytes, descriptions to 256 bytes and permission strings to 128 bytes. Unknown JSON fields, malformed Unicode and duplicate JSON fields are rejected. Plugin exceptions, linkage errors, assertion failures or invalid actions disable the offending plugin; that callback's actions are discarded.

The Rust bridge applies a request deadline. A hung callback or broken host process ends the entire optional JVM, disabling all Java plugins in that process while the Rust core handles the failure. There is no safe per-plugin preemption in Java 25. JVM heap limits apply to all Java plugins together, not to Rust core memory. Forced termination cannot guarantee Disable callbacks or rollback plugin filesystem/network side effects.

## Host protocol

UTF-8 JSON Lines on stdin/stdout, with no unsolicited startup output. Every request has an unsigned 64-bit `id`, echoed exactly in the response. The line limit is 65,536 bytes; the response limit is 8 MiB.

```json
{"id":1,"op":"load","directory":"/srv/zerqon/plugins/java-plugins","server_version":"0.0.1"}
{"id":2,"op":"event","plugin":"hello","event":{"event":"command","sender":"console","sender_kind":"console","command":"jhello","args":[]}}
{"id":3,"op":"list"}
{"id":4,"op":"shutdown"}
```

Omitting `plugin` broadcasts an event to enabled plugins. Load invokes Enable; shutdown invokes Disable, closes classloaders and exits after its response. Responses are:

```json
{"id":2,"ok":true,"results":[{"name":"hello","enabled":true,"actions":[{"action":"reply","message":"Hello"}]}]}
```

An operation-level failure sets `ok:false` and `error`; a plugin-level failure appears in that result's `error`, with `enabled:false`. Duplicate loading leaves an existing plugin intact and returns an error for that entry. Logs go only to stderr. An invalid or oversized protocol stream can terminate the host.

Direct diagnostic launch uses `java -Xms16M -Xmx128M -cp "libraries/zerqon-java-host-0.0.1.jar:./libraries/gson-2.13.2.jar" io.github.teighto.zerqon.host.PluginHost`. On Windows, use `;` between classpath entries.

## Русский

У Zerqon собственный Java API, отдельный от совместимости Paper/Bukkit. Ядро и мир работают на Rust; Java-плагины исполняются в одном дополнительном процессе Java 25. Папка плагинов — `plugins/java-plugins/`, для Rust — `plugins/rust-plugins/`.

Сборка: `python scripts/build-java-plugins.py`. Она проверяет SHA-256 библиотеки Gson, компилирует SDK, хост и пример, запускает интеграционные тесты с настоящими JAR и JVM. Скопируйте `dist/hello-java-plugin-0.0.1.jar` в `plugins/java-plugins/hello.jar`, хост и Gson — в `libraries/`. Настройка `java_plugins = true` в `server.settings` включает хост при наличии JAR; `java_executable` задаёт Java 25. Пример добавляет `/jhello`.

Плагин реализует `Plugin.onEvent(Event)` и возвращает действия `Action`. В манифесте обязательны `Zerqon-Plugin` с именем класса и `Zerqon-API: 1`. Есть события включения, выключения, команд, тиков и входа/выхода игроков. Действия — журнал, ответ на команду и регистрация команды. Право доступа и конфликт имён проверяет Rust-диспетчер сервера.

`senderKind()` различает консоль и игрока. Строка `sender()` — отображаемое имя; игрок с ником `CONSOLE` остаётся игроком. Не проверяйте привилегии по этому тексту.

API мира, инвентарей, сущностей, планировщика и постоянного хранения пока отсутствуют. Обработчики последовательные; долгие операции блокируют Java-хост. При исключении отключается плагин, при зависании таймаут Rust-моста завершает весь Java-процесс. Java-плагины имеют права пользователя ОС и не изолированы как WASM; лимит JVM относится ко всем Java-плагинам вместе. Остановка процесса не отменяет уже выполненные внешние действия плагина.

Проверенный пример и его манифест находятся в `examples/hello-java-plugin/`. SDK не требует Gson при компиляции плагинов.
