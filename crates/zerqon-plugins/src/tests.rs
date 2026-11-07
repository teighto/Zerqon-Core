use super::*;

fn reply_module() -> Vec<u8> {
    wat::parse_str(
        r#"(module
        (import "zerqon" "emit" (func $emit (param i32 i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 0) "{\22action\22:\22reply\22,\22message\22:\22hello\22}")
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (global $calls (mut i32) (i32.const 0))
        (func (export "zerqon_on_event") (result i32)
            global.get $calls i32.const 1 i32.add global.set $calls
            global.get $calls i32.const 1 i32.gt_u
            if i32.const 0 i32.const 36 call $emit drop end i32.const 0)
    )"#,
    )
    .unwrap()
}

fn module_with_handler(handler: &str) -> Vec<u8> {
    wat::parse_str(format!(
        r#"(module
        (memory (export "memory") 1)
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (global $calls (mut i32) (i32.const 0))
        (func (export "zerqon_on_event") (result i32)
            global.get $calls i32.const 1 i32.add global.set $calls
            global.get $calls i32.const 1 i32.gt_u if {handler} end
            i32.const 0))"#
    ))
    .unwrap()
}

#[test]
fn rejects_non_binary_modules() {
    let mut manager = PluginManager::new(Limits::default()).unwrap();
    assert!(manager.load("broken", b"not wasm").is_err());
}

#[test]
fn rejects_zero_fuel_configuration() {
    assert!(
        PluginManager::new(Limits {
            fuel_per_event: 0,
            ..Limits::default()
        })
        .is_err()
    );
}

#[test]
fn rejects_unsupported_abi() {
    let bytes = wat::parse_str(
        r#"(module
        (memory (export "memory") 1)
        (func (export "zerqon_abi_version") (result i32) i32.const 2)
        (func (export "zerqon_on_event") (result i32) i32.const 0))"#,
    )
    .unwrap();
    let mut manager = PluginManager::new(Limits::default()).unwrap();
    assert!(manager.load("future", &bytes).is_err());
}

#[test]
fn rejects_wasi_and_unknown_imports() {
    let bytes = wat::parse_str(
        r#"(module
        (import "wasi_snapshot_preview1" "fd_write" (func))
        (memory (export "memory") 1)
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (func (export "zerqon_on_event") (result i32) i32.const 0))"#,
    )
    .unwrap();
    let mut manager = PluginManager::new(Limits::default()).unwrap();
    assert!(manager.load("wasi", &bytes).is_err());
}

#[test]
fn traps_infinite_loop_and_disables_failing_plugin() {
    let mut manager = PluginManager::new(Limits {
        fuel_per_event: 1000,
        ..Limits::default()
    })
    .unwrap();
    manager
        .load("loop", &module_with_handler("(loop $again br $again)"))
        .unwrap();
    assert!(manager.dispatch("loop", &Event::Tick { tick: 1 }).is_err());
    assert!(
        manager
            .dispatch("loop", &Event::Tick { tick: 2 })
            .unwrap_err()
            .to_string()
            .contains("disabled")
    );
}

#[test]
fn enforces_memory_growth_limit() {
    let mut manager = PluginManager::new(Limits {
        memory_bytes: 65_536,
        ..Limits::default()
    })
    .unwrap();
    manager
        .load(
            "memory",
            &module_with_handler("i32.const 1 memory.grow drop"),
        )
        .unwrap();
    assert!(
        manager
            .dispatch("memory", &Event::Tick { tick: 1 })
            .is_err()
    );
}

#[test]
fn rejects_oversized_initial_memory() {
    let bytes = wat::parse_str(
        r#"(module
        (memory (export "memory") 2)
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (func (export "zerqon_on_event") (result i32) i32.const 0))"#,
    )
    .unwrap();
    let mut manager = PluginManager::new(Limits {
        memory_bytes: 65_536,
        ..Limits::default()
    })
    .unwrap();
    assert!(manager.load("memory", &bytes).is_err());
}

#[test]
fn rejects_duplicate_plugin_names_without_replacing_original() {
    let mut manager = PluginManager::new(Limits::default()).unwrap();
    manager.load("hello", &reply_module()).unwrap();
    assert!(
        manager
            .load("hello", &module_with_handler("unreachable"))
            .is_err()
    );
    assert!(
        manager
            .dispatch(
                "hello",
                &Event::Command {
                    sender: "console".into(),
                    sender_kind: zerqon_plugin_api::SenderKind::Console,
                    operator: false,
                    command: "hello".into(),
                    args: vec![],
                }
            )
            .is_ok()
    );
}

#[test]
fn enforces_plugin_count_limit() {
    let mut manager = PluginManager::new(Limits {
        plugins: 1,
        ..Limits::default()
    })
    .unwrap();
    manager.load("one", &module_with_handler("")).unwrap();
    assert!(manager.load("two", &module_with_handler("")).is_err());
}

#[test]
fn rejects_oversized_events_before_guest_execution() {
    let mut manager = PluginManager::new(Limits {
        event_bytes: 128,
        ..Limits::default()
    })
    .unwrap();
    manager.load("hello", &reply_module()).unwrap();
    let event = Event::Command {
        sender: "console".into(),
        sender_kind: zerqon_plugin_api::SenderKind::Console,
        operator: false,
        command: "hello".into(),
        args: vec!["x".repeat(256)],
    };
    assert!(manager.dispatch("hello", &event).is_err());
    assert!(
        manager
            .dispatch(
                "hello",
                &Event::Command {
                    sender: "console".into(),
                    sender_kind: zerqon_plugin_api::SenderKind::Console,
                    operator: false,
                    command: "hello".into(),
                    args: vec![],
                }
            )
            .is_ok()
    );
}

#[test]
fn rejects_start_functions_before_execution() {
    let bytes = wat::parse_str(
        r#"(module
        (func $start (loop $again br $again)) (start $start)
        (memory (export "memory") 1)
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (func (export "zerqon_on_event") (result i32) i32.const 0))"#,
    )
    .unwrap();
    let mut manager = PluginManager::new(Limits::default()).unwrap();
    assert!(manager.load("start", &bytes).is_err());
}

#[test]
fn rejects_out_of_bounds_guest_output() {
    let bytes = wat::parse_str(
        r#"(module
        (import "zerqon" "emit" (func $emit (param i32 i32) (result i32)))
        (memory (export "memory") 1)
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (func (export "zerqon_on_event") (result i32) i32.const 65530 i32.const 100 call $emit))"#,
    )
    .unwrap();
    let mut manager = PluginManager::new(Limits::default()).unwrap();
    assert!(manager.load("bad-output", &bytes).is_err());
}

#[test]
fn rejects_module_larger_than_limit() {
    let mut manager = PluginManager::new(Limits {
        module_bytes: 8,
        ..Limits::default()
    })
    .unwrap();
    assert!(manager.load("large", &reply_module()).is_err());
}

fn action_module(json: &str, repetitions: usize) -> Vec<u8> {
    let encoded: String = json.bytes().map(|b| format!("\\{b:02x}")).collect();
    let calls =
        format!("i32.const 0 i32.const {} call $emit drop ", json.len()).repeat(repetitions);
    wat::parse_str(format!(
        r#"(module
        (import "zerqon" "emit" (func $emit (param i32 i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 0) "{encoded}")
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (func (export "zerqon_on_event") (result i32) {calls} i32.const 0))"#
    ))
    .unwrap()
}

#[test]
fn rejects_output_action_flood() {
    let module = action_module(r#"{"action":"log","level":"info","message":"hello"}"#, 2);
    let mut manager = PluginManager::new(Limits {
        actions_per_event: 1,
        ..Limits::default()
    })
    .unwrap();
    assert!(manager.load("flood", &module).is_err());
}

#[test]
fn rejects_output_byte_flood() {
    let module = action_module(r#"{"action":"log","level":"info","message":"hello"}"#, 2);
    let mut manager = PluginManager::new(Limits {
        output_bytes: 60,
        ..Limits::default()
    })
    .unwrap();
    assert!(manager.load("flood", &module).is_err());
}

#[test]
fn rejects_invalid_action_json() {
    let mut manager = PluginManager::new(Limits::default()).unwrap();
    assert!(
        manager
            .load("invalid", &action_module("not json", 1))
            .is_err()
    );
}

#[test]
fn command_registration_requires_enable_event() {
    let module = action_module(
        r#"{"action":"register_command","name":"hello","description":"Greeting","permission":null}"#,
        1,
    );
    let mut manager = PluginManager::new(Limits::default()).unwrap();
    assert_eq!(manager.load("commands", &module).unwrap().len(), 1);
    assert!(
        manager
            .dispatch("commands", &Event::Tick { tick: 1 })
            .is_err()
    );
}

#[test]
fn failed_plugin_does_not_prevent_other_plugins_running() {
    let mut manager = PluginManager::new(Limits::default()).unwrap();
    manager
        .load("bad", &module_with_handler("unreachable"))
        .unwrap();
    manager.load("good", &module_with_handler("")).unwrap();
    let results = manager.dispatch_all(&Event::Tick { tick: 1 });
    assert!(
        results
            .iter()
            .find(|entry| entry.name == "bad")
            .unwrap()
            .result
            .is_err()
    );
    assert!(
        results
            .iter()
            .find(|entry| entry.name == "good")
            .unwrap()
            .result
            .is_ok()
    );
    let second = manager.dispatch_all(&Event::Tick { tick: 2 });
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].name, "good");
}

#[test]
fn unload_removes_plugin_even_if_disable_handler_traps() {
    let mut manager = PluginManager::new(Limits::default()).unwrap();
    manager
        .load("bad", &module_with_handler("unreachable"))
        .unwrap();
    assert!(manager.unload("bad").is_err());
    assert!(manager.list().is_empty());
}

#[test]
fn directory_loader_keeps_valid_plugins_when_another_file_is_broken() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("broken.wasm"), b"broken").unwrap();
    std::fs::write(dir.path().join("good.wasm"), module_with_handler("")).unwrap();
    std::fs::write(dir.path().join("unrelated.jar"), b"unrelated").unwrap();
    let mut manager = PluginManager::new(Limits::default()).unwrap();
    let results = manager.load_directory(dir.path()).unwrap();
    assert_eq!(results.len(), 2);
    assert!(results[0].result.is_err());
    assert!(results[1].result.is_ok());
    assert_eq!(manager.list()[0].name, "good");
}

#[test]
fn rejects_invalid_command_names() {
    let module = action_module(
        r#"{"action":"register_command","name":"hello world","description":"Greeting","permission":null}"#,
        1,
    );
    let mut manager = PluginManager::new(Limits::default()).unwrap();
    assert!(manager.load("commands", &module).is_err());
}

#[test]
fn rejects_oversized_log_message_before_returning_it_to_server() {
    let message = "x".repeat(4097);
    let json = format!(r#"{{"action":"log","level":"info","message":"{message}"}}"#);
    let mut manager = PluginManager::new(Limits::default()).unwrap();
    assert!(manager.load("too-long", &action_module(&json, 1)).is_err());
}

#[test]
fn executes_guest_and_returns_its_reply() {
    let mut manager = PluginManager::new(Limits::default()).unwrap();
    manager.load("hello", &reply_module()).unwrap();
    let result = manager
        .dispatch(
            "hello",
            &Event::Command {
                sender: "console".into(),
                sender_kind: zerqon_plugin_api::SenderKind::Console,
                operator: false,
                command: "hello".into(),
                args: vec![],
            },
        )
        .unwrap();
    assert_eq!(
        result,
        vec![Action::Reply {
            message: "hello".into()
        }]
    );
}
