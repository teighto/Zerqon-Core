# Rust plugins / Rust-плагины

Zerqon loads server plugins written in Rust and compiled to WebAssembly. They are server-side plugins; players do not install anything. Place `.wasm` files in `plugins/rust-plugins/`. Java plugin JARs belong in `plugins/java-plugins/` and use a separate compatibility host.

ABI 1 provides lifecycle events, command registration, replies, logging, ticks and player join/quit notifications. The optional `native-read` feature adds player, world and server queries. World changes, inventory access, entity control, persistent storage and scheduling APIs are still being developed.

## Build the example

Install the repository's Rust toolchain and the `wasm32-unknown-unknown` target:

```sh
rustup target add wasm32-unknown-unknown
cargo build -p zerqon-hello-plugin --target wasm32-unknown-unknown --release --locked
```

Copy `target/wasm32-unknown-unknown/release/zerqon_hello_plugin.wasm` to `plugins/rust-plugins/hello.wasm` in the server directory. Restart the server to load it. The example registers `/hello`. Plugin file names must contain 1–64 ASCII letters, digits, underscores or hyphens before `.wasm`.

Verify the compiled plugin independently of a client:

```sh
cargo run -p zerqon-plugins --example verify-plugin --locked -- target/wasm32-unknown-unknown/release/zerqon_hello_plugin.wasm
```

This check loads the actual Rust-generated WebAssembly, verifies command registration and the response, executes 1,000 ticks and checks unloading. It does not substitute for an in-game integration test.

## Create a plugin

Use `examples/hello-plugin` as the minimal project. Its library has `crate-type = ["cdylib", "rlib"]` and depends on `zerqon-plugin-api`. For a separate repository, pin the SDK to the exact tested Zerqon commit or release; it is not published on crates.io yet.

```rust
use zerqon_plugin_api::{Action, Event};

pub fn handle(event: Event) -> Vec<Action> {
    match event {
        Event::Enable { .. } => vec![Action::RegisterCommand {
            name: "hello".into(),
            description: "A greeting from Rust".into(),
            permission: None,
        }],
        Event::Command { sender, command, .. } if command == "hello" => {
            vec![Action::Reply { message: format!("Hello, {sender}!") }]
        }
        _ => Vec::new(),
    }
}

zerqon_plugin_api::export_plugin!(handle);
```

Commands register only during `Enable`, use lowercase names and cannot contain spaces. `Reply` is valid only while handling a command and is returned to that command's sender. `permission: None` requests a public command; a permission string is a requirement that the server's command dispatcher must enforce. The plugin host returns actions; it does not bypass the server's permission or command-collision checks.

`Event::Command.sender_kind` is `SenderKind::Console` or `SenderKind::Player`; `operator` carries the server's authorization decision and defaults to false when absent. The `sender` string is a display name, never evidence of console privileges: a player can be named `CONSOLE`. Use the explicit sender kind and server-enforced permissions for privileged behavior.

Callbacks execute serially within one plugin instance. Keep them short. The host does not give a plugin mutable access to the world or spawn an OS thread for it. Plugin memory survives between callbacks until disable, a fault, unload or server shutdown. There is no filesystem persistence API in ABI 1.

The player remains registered while Join and Quit run. Disconnecting during an accepted Join lets that callback finish before Quit. Quit goes only to the plugin instance that completed Join; reloading a plugin does not transfer old player events to its replacement. Join and Quit wait for queue space within a ten-second transaction deadline. A running Rust callback has a one-second deadline, capped by the remaining transaction time. Cancellation is cooperative at interpreter checkpoints, so an individual WebAssembly operation can finish after that deadline.

## Read player and world data

Enable `features = ["native-read"]` on the `zerqon-plugin-api` dependency. Build the example with:

```sh
cargo build -p zerqon-hello-plugin --target wasm32-unknown-unknown --release --locked --features native-read
```

Copy the WASM into the Rust plugin directory and restart. `/rwhere` reports the command sender's UUID, session, world and coordinates. A player named `CONSOLE` is still a player; use the source from `NativeReads`, not the display name.

```rust
use zerqon_plugin_api::{guest::NativeReads, native::NativeSource};

fn location() -> Result<String, Box<dyn std::error::Error>> {
    let mut reads = NativeReads::current()?;
    let (uuid, session_id) = match reads.source()? {
        NativeSource::Player { uuid, session_id } => (*uuid, *session_id),
        _ => return Ok("This command requires a player.".into()),
    };
    let player = reads.player(uuid, Some(session_id))?
        .ok_or("Player session is no longer active")?;
    Ok(format!("{}, {}, {}", player.position.x, player.position.y, player.position.z))
}
```

Call this inside a callback. `NativeReads` belongs to that callback and must not be stored for later. `player(uuid, Some(session_id))` selects that exact session; an old session returns `None` after reconnect. `player(uuid, None)` selects the current session. `worlds()` supplies persistent world UUIDs, names, height bounds and spawn positions. `online_players()` is limited to 1,024 entries and `worlds()` to 16; results must also fit the byte limit. Use `server_snapshot().online_players` when only a count is needed.

Player and world results are cached within one callback. A later callback fetches fresh data. Completed service errors can be handled by the plugin; malformed replies, expired callback deadlines and guest faults terminate the callback. Native calls share its deadline, fuel and byte limits. Waiting for a query does not hold the plugin registry lock or occupy a CPU worker.

The extension uses `zerqon_read_v1` imports. Plugins built without it keep their existing ABI 1 imports. A read-enabled plugin requires a host with this extension; an older host rejects it at load time. `native-types` exposes the data types without adding imports. The standalone verifier above exercises the legacy ABI; server tests separately cover native reads.

## ABI 1

The module exports 32-bit linear memory as `memory` and two functions:

| Export | Signature | Meaning |
|---|---|---|
| `zerqon_abi_version` | `() -> i32` | Returns `1` |
| `zerqon_on_event` | `() -> i32` | Handles the current event; `0` is success |

ABI 1 uses these imports in module `zerqon`; `native-read` adds the separate `zerqon_read_v1` module:

| Import | Signature | Meaning |
|---|---|---|
| `event_len` | `() -> i32` | Current JSON event length |
| `event_read` | `(pointer: i32, length: i32) -> i32` | Copies the complete event into guest memory; length must match exactly |
| `emit` | `(pointer: i32, length: i32) -> i32` | Reads one JSON action and returns `0` |

Events use a tagged JSON object, for example `{"event":"command","sender":"teighto","sender_kind":"player","operator":false,"command":"hello","args":[]}`. Actions use `{"action":"reply","message":"Hello"}`. The SDK performs serialization and exposes Rust enums. Unknown fields and variants are rejected. Start functions, unrecognized imports, WASI, native libraries, host file access and network access are unavailable.

The SDK's small WebAssembly FFI boundary contains the pointer operations; plugin authors use the safe `Event` and `Action` interface. The native host reads and writes guest memory through checked Wasmi APIs and contains no unsafe Rust.

## Resource limits and faults

Default limits per plugin are 16 MiB linear memory, 1,000,000 fuel units per callback, a 4 MiB module, 64 KiB serialized input, 64 KiB total output and 32 actions per callback. Replies and individual log messages are limited to 4,096 UTF-8 bytes. There are at most 64 loaded plugins by default. Fuel measures interpreter work, not milliseconds; host memory copies also consume fuel and a separate I/O budget. Tables, recursion, stack height and module structure have additional bounds.

The memory limit covers WebAssembly linear memory, not the entire native process or compiled module metadata. These limits are not performance guarantees. Do not install plugins from untrusted publishers without reviewing their purpose and source.

A guest trap, nonzero return, malformed action, exceeded fuel or invalid memory access disables that plugin and releases its runtime. Actions accumulated by a failed callback are discarded. Other plugins continue. Oversized server-generated events return an error without disabling the plugin. Restart or explicitly unload/reload a disabled plugin after correcting the cause.

## Русский

Rust-плагины устанавливаются на сервер в `plugins/rust-plugins/` как файлы `.wasm`. Клиенту ничего устанавливать не нужно. Java-плагины `.jar` находятся отдельно в `plugins/java-plugins/`.

Для сборки примера выполните команды выше и скопируйте `zerqon_hello_plugin.wasm` под именем `hello.wasm`. После перезапуска плагин регистрирует `/hello`. Проверка `verify-plugin` запускает настоящий скомпилированный плагин: включение, команда, 1 000 тиков и выключение.

API версии 1 содержит события включения/выключения, команд, тиков и входа/выхода игроков; действия — регистрация команды, ответ отправителю и запись в журнал. Функция `native-read` добавляет чтение данных игроков, миров и сервера. Изменение мира, API инвентарей, управление сущностями, постоянное хранилище и планировщик пока дорабатываются.

Во время Join и Quit игрок остаётся в реестре сервера. Если соединение оборвалось во время принятого Join, сервер дождётся результата и отправит Quit тому же экземпляру плагина. Новая версия плагина после перезагрузки не получает события старого экземпляра. На ожидание очереди и доставку события отведено до десяти секунд, на выполнение Rust-обработчика — одна секунда, но не больше оставшегося времени события. Остановка проверяется между операциями интерпретатора; отдельная операция WebAssembly может закончиться позже дедлайна.

Для чтения данных включите `features = ["native-read"]` у зависимости `zerqon-plugin-api`. Пример собирается командой выше с `--features native-read` и добавляет `/rwhere`. Внутри обработчика создайте `NativeReads::current()`, получите `source()` и используйте UUID вместе с `session_id`. Это идентификаторы от ядра; имя игрока их не заменяет. После переподключения ID сессии меняется, а запрос старой сессии возвращает `None`.

`player()` возвращает положение и данные игрока, `worlds()` содержит UUID мира, точку появления и границы высоты. `online_players()` ограничен 1 024 записями, `worlds()` — 16. Для количества игроков достаточно `server_snapshot().online_players`. Данные игроков и миров кешируются в пределах одного обработчика; следующий получает свежие сведения. Объект `NativeReads` нельзя сохранять между вызовами.

Старые плагины ABI 1 продолжают работать. Плагин с `native-read` использует дополнительные импорты `zerqon_read_v1`, поэтому на старой сборке без этого расширения он не загрузится. Лимиты инструкций, памяти и размера сообщений действуют и во время таких запросов.

Обработчик `handle(Event) -> Vec<Action>` возвращает действия. Команда регистрируется только в `Enable`, ответ допустим только в `Command`. Проверку разрешений и конфликтов имён выполняет диспетчер сервера. Обработчики одного плагина вызываются последовательно; работа должна укладываться в лимит инструкций.

Тип отправителя задаётся полем `sender_kind`: `Console` или `Player`. Поле `operator` передаёт решение сервера о правах; при отсутствии оно равно false. Ник `CONSOLE` не даёт игроку права консоли. Для проверки привилегий нельзя использовать текст поля `sender`.

У каждого плагина отдельная память, ограничение инструкций, размера входа/выхода и количества действий. Нет доступа к файлам, сети и системным библиотекам. Ошибка плагина отключает только его, освобождает его runtime и отменяет действия неуспешного вызова. После исправления нужен перезапуск или явная выгрузка/загрузка. Лимит памяти WebAssembly не равен лимиту всей памяти процесса.
