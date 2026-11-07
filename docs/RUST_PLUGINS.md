# Rust plugins / Rust-плагины

Zerqon loads server plugins written in Rust and compiled to WebAssembly. They are server-side plugins; players do not install anything. Place `.wasm` files in `plugins/rust-plugins/`. Java plugin JARs belong in `plugins/java-plugins/` and use a separate compatibility host.

The current API implements lifecycle events, command registration, command replies, logging, ticks and player join/quit notifications. World, inventory, entity, persistent storage and scheduling APIs are not yet part of ABI 1. The server must explicitly dispatch an event for a plugin to receive it.

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

## ABI 1

The module exports 32-bit linear memory as `memory` and two functions:

| Export | Signature | Meaning |
|---|---|---|
| `zerqon_abi_version` | `() -> i32` | Returns `1` |
| `zerqon_on_event` | `() -> i32` | Handles the current event; `0` is success |

Only these imports are available, in module `zerqon`:

| Import | Signature | Meaning |
|---|---|---|
| `event_len` | `() -> i32` | Current JSON event length |
| `event_read` | `(pointer: i32, length: i32) -> i32` | Copies the complete event into guest memory; length must match exactly |
| `emit` | `(pointer: i32, length: i32) -> i32` | Reads one JSON action and returns `0` |

Events use a tagged JSON object, for example `{"event":"command","sender":"teighto","sender_kind":"player","operator":false,"command":"hello","args":[]}`. Actions use `{"action":"reply","message":"Hello"}`. The SDK performs serialization and exposes Rust enums. Unknown fields and variants are rejected. Start functions, other imports, WASI, native libraries, host file access and network access are unavailable.

The SDK's small WebAssembly FFI boundary contains the pointer operations; plugin authors use the safe `Event` and `Action` interface. The native host reads and writes guest memory through checked Wasmi APIs and contains no unsafe Rust.

## Resource limits and faults

Default limits per plugin are 16 MiB linear memory, 1,000,000 fuel units per callback, a 4 MiB module, 64 KiB serialized input, 64 KiB total output and 32 actions per callback. Replies and individual log messages are limited to 4,096 UTF-8 bytes. There are at most 64 loaded plugins by default. Fuel measures interpreter work, not milliseconds; host memory copies also consume fuel and a separate I/O budget. Tables, recursion, stack height and module structure have additional bounds.

The memory limit covers WebAssembly linear memory, not the entire native process or compiled module metadata. These limits are not performance guarantees. Do not install plugins from untrusted publishers without reviewing their purpose and source.

A guest trap, nonzero return, malformed action, exceeded fuel or invalid memory access disables that plugin and releases its runtime. Actions accumulated by a failed callback are discarded. Other plugins continue. Oversized server-generated events return an error without disabling the plugin. Restart or explicitly unload/reload a disabled plugin after correcting the cause.

## Русский

Rust-плагины устанавливаются на сервер в `plugins/rust-plugins/` как файлы `.wasm`. Клиенту ничего устанавливать не нужно. Java-плагины `.jar` находятся отдельно в `plugins/java-plugins/`.

Для сборки примера выполните команды выше и скопируйте `zerqon_hello_plugin.wasm` под именем `hello.wasm`. После перезапуска плагин регистрирует `/hello`. Проверка `verify-plugin` запускает настоящий скомпилированный плагин: включение, команда, 1 000 тиков и выключение.

API версии 1 содержит события включения/выключения, команд, тиков и входа/выхода игроков; действия — регистрация команды, ответ отправителю и запись в журнал. События поступают только при их отправке сервером. API мира, инвентарей, сущностей, постоянного хранения и планировщика пока отсутствуют. Неподдерживаемые возможности не имитируются.

Обработчик `handle(Event) -> Vec<Action>` возвращает действия. Команда регистрируется только в `Enable`, ответ допустим только в `Command`. Проверку разрешений и конфликтов имён выполняет диспетчер сервера. Обработчики одного плагина вызываются последовательно; работа должна укладываться в лимит инструкций.

Тип отправителя задаётся полем `sender_kind`: `Console` или `Player`. Поле `operator` передаёт решение сервера о правах; при отсутствии оно равно false. Ник `CONSOLE` не даёт игроку права консоли. Для проверки привилегий нельзя использовать текст поля `sender`.

У каждого плагина отдельная память, ограничение инструкций, размера входа/выхода и количества действий. Нет доступа к файлам, сети и системным библиотекам. Ошибка плагина отключает только его, освобождает его runtime и отменяет действия неуспешного вызова. После исправления нужен перезапуск или явная выгрузка/загрузка. Лимит памяти WebAssembly не равен лимиту всей памяти процесса.
