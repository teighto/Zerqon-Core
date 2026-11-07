use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use zerqon_plugin_api::{
    Event,
    native::{NativeCallbackContext, NativeQuery, READ_CAPABILITIES},
};
use zerqon_plugins::{
    Limits, PluginError,
    resumable::{CallbackControl, Invocation, InvocationStep, ResumableHost},
};

fn simple_module(handler: &str) -> Vec<u8> {
    fixture_module(handler, "")
}

fn fixture_module(handler: &str, declarations: &str) -> Vec<u8> {
    custom_fixture(handler, declarations, 1, r#"{"query":"worlds"}"#)
}

fn custom_fixture(handler: &str, declarations: &str, pages: u32, query: &str) -> Vec<u8> {
    let data: String = query.bytes().map(|byte| format!("\\{byte:02x}")).collect();
    wat::parse_str(format!(r#"(module
        (import "zerqon" "event_len" (func $event_len (result i32)))
        (import "zerqon" "event_read" (func $event_read (param i32 i32) (result i32)))
        (import "zerqon" "emit" (func $emit (param i32 i32) (result i32)))
        (import "zerqon_read_v1" "callback_token" (func $token (result i64)))
        (import "zerqon_read_v1" "capabilities" (func $caps (param i32) (result i64)))
        (import "zerqon_read_v1" "context_len" (func $context_len (param i64) (result i32)))
        (import "zerqon_read_v1" "context_read" (func $context_read (param i64 i32 i32) (result i32)))
        (import "zerqon_read_v1" "query" (func $query (param i64 i32 i32) (result i32)))
        (import "zerqon_read_v1" "result_len" (func $len (param i64 i32) (result i32)))
        (import "zerqon_read_v1" "result_read" (func $read (param i64 i32 i32 i32) (result i32)))
        (memory (export "memory") {pages})
        {declarations}
        (data (i32.const 0) "{data}")
        (global $entries (mut i32) (i32.const 0))
        (global $saved_token (mut i64) (i64.const 0))
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (func $consume (param $handle i32)
            call $token local.get $handle i32.const 1024
            call $token local.get $handle call $len call $read drop)
        (func (export "zerqon_on_event") (result i32)
            (local $handle i32) (local $length i32) (local $number i32)
            global.get $entries i32.const 1 i32.add global.set $entries
            {handler} i32.const 0))"#)).unwrap()
}

fn enable(
    plugin: zerqon_plugins::resumable::PluginInstance,
) -> zerqon_plugins::resumable::PluginInstance {
    let prepared = plugin
        .prepare(
            &Event::Enable {
                server_version: "0.0.1".into(),
            },
            &context(),
        )
        .unwrap();
    let call = plugin
        .begin(
            prepared,
            CallbackControl::new(Instant::now() + Duration::from_secs(5)),
            Box::new(()),
        )
        .unwrap();
    let InvocationStep::Finished(done) = advance(call) else {
        panic!("unexpected read");
    };
    done.commit().unwrap().0
}

#[test]
fn stale_prepared_enable_is_rejected_before_executing_again() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let plugin = host
        .instantiate("lifecycle", &simple_module(""), READ_CAPABILITIES)
        .unwrap();
    let stale = plugin
        .prepare(
            &Event::Enable {
                server_version: "0.0.1".into(),
            },
            &context(),
        )
        .unwrap();
    let plugin = enable(plugin);
    let result = plugin.begin(
        stale,
        CallbackControl::new(Instant::now() + Duration::from_secs(5)),
        Box::new(()),
    );
    assert!(matches!(result, Err(failure) if matches!(failure.error, PluginError::Lifecycle)));
}

#[test]
fn disabled_instance_cannot_be_enabled_or_given_a_stale_prepared_callback() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let plugin = enable(
        host.instantiate("lifecycle", &simple_module(""), READ_CAPABILITIES)
            .unwrap(),
    );
    let stale = plugin
        .prepare(&Event::Tick { tick: 1 }, &context())
        .unwrap();
    let disable = plugin.prepare(&Event::Disable, &context()).unwrap();
    let call = plugin
        .begin(
            disable,
            CallbackControl::new(Instant::now() + Duration::from_secs(5)),
            Box::new(()),
        )
        .unwrap();
    let InvocationStep::Finished(done) = advance(call) else {
        panic!("unexpected read");
    };
    let (plugin, _) = done.commit().unwrap();
    assert!(matches!(
        plugin.prepare(
            &Event::Enable {
                server_version: "0.0.1".into()
            },
            &context()
        ),
        Err(PluginError::Lifecycle)
    ));
    let result = plugin.begin(
        stale,
        CallbackControl::new(Instant::now() + Duration::from_secs(5)),
        Box::new(()),
    );
    assert!(matches!(result, Err(failure) if matches!(failure.error, PluginError::Lifecycle)));
}

fn context() -> NativeCallbackContext {
    serde_json::from_str(r#"{"source":{"kind":"server"}}"#).unwrap()
}

fn two_queries() -> Vec<u8> {
    let query = r#"{"query":"worlds"}"#;
    let data: String = query.bytes().map(|byte| format!("\\{byte:02x}")).collect();
    wat::parse_str(format!(
        r#"(module
        (import "zerqon_read_v1" "callback_token" (func $token (result i64)))
        (import "zerqon_read_v1" "query" (func $query (param i64 i32 i32) (result i32)))
        (import "zerqon_read_v1" "result_len" (func $len (param i64 i32) (result i32)))
        (import "zerqon_read_v1" "result_read" (func $read (param i64 i32 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 0) "{data}")
        (global $entries (mut i32) (i32.const 0))
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (func $consume (param $handle i32)
            call $token local.get $handle i32.const 1024
            call $token local.get $handle call $len call $read drop)
        (func (export "zerqon_on_event") (result i32) (local $saved i32) (local $handle i32)
            global.get $entries i32.const 1 i32.add global.set $entries
            i32.const 7 local.set $saved
            call $token i32.const 0 i32.const {size} call $query local.tee $handle
            local.get $saved i32.add local.set $saved
            local.get $handle call $consume
            call $token i32.const 0 i32.const {size} call $query local.tee $handle
            local.get $saved i32.add i32.const 100 i32.add local.set $saved
            local.get $handle call $consume
            global.get $entries i32.const 1 i32.ne if unreachable end
            local.get $saved i32.const 110 i32.ne if unreachable end
            i32.const 0))"#,
        size = query.len()
    ))
    .unwrap()
}

fn advance(mut invocation: Invocation) -> InvocationStep {
    loop {
        match invocation.run().unwrap() {
            InvocationStep::Quantum(next) => invocation = next,
            step => return step,
        }
    }
}

fn start(
    host: &ResumableHost,
    code: &str,
    caps: u64,
    guard: Box<dyn Send>,
) -> (Invocation, CallbackControl) {
    let plugin = host
        .instantiate("fixture", &simple_module(code), caps)
        .unwrap();
    let prepared = plugin
        .prepare(
            &Event::Enable {
                server_version: "0.0.1".into(),
            },
            &context(),
        )
        .unwrap();
    let control = CallbackControl::new(Instant::now() + Duration::from_secs(5));
    (
        plugin.begin(prepared, control.clone(), guard).unwrap(),
        control,
    )
}

fn run_result(mut invocation: Invocation) -> Result<InvocationStep, PluginError> {
    for _ in 0..10_000 {
        match invocation.run()? {
            InvocationStep::Quantum(next) => invocation = next,
            step => return Ok(step),
        }
    }
    panic!("execution failed to respect its fuel bound")
}

struct Guard(Arc<AtomicUsize>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}
struct Reply {
    bytes: Vec<u8>,
    dropped: Arc<AtomicUsize>,
}
impl zerqon_plugins::resumable::ReadReply for Reply {
    fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
impl Drop for Reply {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::AcqRel);
    }
}
fn reply() -> Box<dyn zerqon_plugins::resumable::ReadReply> {
    Box::new(br#"{"ok":true,"value":{"worlds":[]}}"#.to_vec())
}

#[test]
fn owned_store_and_continuation_are_send_and_resume_on_a_different_thread() {
    fn assert_send<T: Send>() {}
    assert_send::<Invocation>();
    assert_send::<zerqon_plugins::resumable::ReadSuspension>();
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let (call, _) = start(
        &host,
        "call $token i32.const 0 i32.const 18 call $query call $consume",
        READ_CAPABILITIES,
        Box::new(()),
    );
    let InvocationStep::Read(read) = std::thread::spawn(move || advance(call)).join().unwrap()
    else {
        panic!("no read");
    };
    let done = std::thread::spawn(move || advance(read.resume(reply()).unwrap()))
        .join()
        .unwrap();
    assert!(matches!(done, InvocationStep::Finished(_)));
}

#[test]
fn total_fuel_decreases_and_an_infinite_guest_does_not_receive_unlimited_quanta() {
    let host = ResumableHost::new(
        Limits {
            fuel_per_event: 1000,
            ..Limits::default()
        },
        16,
    )
    .unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    let (mut call, _) = start(
        &host,
        "(loop $again br $again)",
        READ_CAPABILITIES,
        Box::new(Guard(Arc::clone(&dropped))),
    );
    let mut remaining = call.fuel_remaining();
    let mut segments = 0;
    loop {
        segments += 1;
        assert!(segments < 1000);
        match call.run() {
            Ok(InvocationStep::Quantum(next)) => {
                assert!(next.fuel_remaining() <= remaining);
                remaining = next.fuel_remaining();
                call = next;
            }
            Err(PluginError::FuelExhausted) => break,
            _ => panic!("loop was not bounded by total fuel"),
        }
    }
    assert!(segments > 1);
    assert_eq!(host.live_instances(), 0);
    assert_eq!(dropped.load(Ordering::Acquire), 1);
}

#[test]
fn host_copies_larger_than_a_quantum_debit_the_existing_total_budget() {
    let host = ResumableHost::new(
        Limits {
            fuel_per_event: 1000,
            ..Limits::default()
        },
        1,
    )
    .unwrap();
    let (call, _) = start(
        &host,
        "i32.const 1024 call $event_len call $event_read drop call $token i32.const 2048 call $token call $context_len call $context_read drop",
        READ_CAPABILITIES,
        Box::new(()),
    );
    let InvocationStep::Finished(done) = advance(call) else {
        panic!("unexpected read");
    };
    let (_plugin, output) = done.commit().unwrap();
    assert!(output.stats().context_bytes > 1);
    assert!(output.stats().fuel_consumed > output.stats().context_bytes as u64);
    assert!(output.stats().fuel_consumed <= 1000);
}

#[test]
fn ordinary_traps_and_host_faults_never_become_resumable_native_reads() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    for code in [
        "unreachable",
        "i32.const -1 call $event_len call $event_read drop",
        "i64.const 0 i32.const 0 i32.const 18 call $query drop",
    ] {
        let (call, _) = start(&host, code, READ_CAPABILITIES, Box::new(()));
        assert!(run_result(call).is_err(), "{code}");
        assert_eq!(host.live_instances(), 0);
    }
}

#[test]
fn old_callback_tokens_cannot_authorize_a_later_event() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let code = "global.get $entries i32.const 1 i32.eq if call $token global.set $saved_token else global.get $saved_token i32.const 0 i32.const 18 call $query drop end";
    let (call, _) = start(&host, code, READ_CAPABILITIES, Box::new(()));
    let InvocationStep::Finished(done) = advance(call) else {
        panic!("unexpected read");
    };
    let (plugin, _) = done.commit().unwrap();
    let event = plugin
        .prepare(&Event::Tick { tick: 2 }, &context())
        .unwrap();
    let call = plugin
        .begin(
            event,
            CallbackControl::new(Instant::now() + Duration::from_secs(5)),
            Box::new(()),
        )
        .unwrap();
    assert!(run_result(call).is_err());
    assert_eq!(host.live_instances(), 0);
}

#[test]
fn dropping_or_cancelling_a_suspended_call_destroys_its_instance_and_guard() {
    let host = ResumableHost::new(
        Limits {
            plugins: 1,
            ..Limits::default()
        },
        64,
    )
    .unwrap();
    for cancel in [false, true] {
        let dropped = Arc::new(AtomicUsize::new(0));
        let (call, control) = start(
            &host,
            "call $token i32.const 0 i32.const 18 call $query drop",
            READ_CAPABILITIES,
            Box::new(Guard(Arc::clone(&dropped))),
        );
        let InvocationStep::Read(read) = advance(call) else {
            panic!("no read");
        };
        assert_eq!(dropped.load(Ordering::Acquire), 0);
        assert!(matches!(
            host.instantiate("another", &simple_module(""), READ_CAPABILITIES),
            Err(PluginError::Count)
        ));
        if cancel {
            control.cancel();
            assert!(matches!(read.resume(reply()), Err(PluginError::Cancelled)));
        } else {
            drop(read);
        }
        assert_eq!(host.live_instances(), 0);
        assert_eq!(dropped.load(Ordering::Acquire), 1);
    }
}

#[test]
fn cancellation_after_finish_prevents_publication_and_output_owns_its_guard() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    for cancelled in [false, true] {
        let dropped = Arc::new(AtomicUsize::new(0));
        let (call, control) = start(
            &host,
            "",
            READ_CAPABILITIES,
            Box::new(Guard(Arc::clone(&dropped))),
        );
        let InvocationStep::Finished(done) = advance(call) else {
            panic!("unexpected read");
        };
        if cancelled {
            control.cancel();
            assert!(matches!(done.commit(), Err(PluginError::Cancelled)));
        } else {
            let (plugin, output) = done.commit().unwrap();
            drop(plugin);
            assert_eq!(dropped.load(Ordering::Acquire), 0);
            drop(output);
        }
        assert_eq!(dropped.load(Ordering::Acquire), 1);
        assert_eq!(host.live_instances(), 0);
    }
}

#[test]
fn bad_result_buffers_handles_and_unread_overwrites_are_terminal() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    for tail in [
        "call $token local.get $handle i32.const -1 call $token local.get $handle call $len call $read drop",
        "call $token local.get $handle i32.const 65536 call $token local.get $handle call $len call $read drop",
        "call $token local.get $handle i32.const 1024 i32.const 1 call $read drop",
        "call $token i32.const 99 call $len drop",
        "local.get $handle call $consume local.get $handle call $consume",
        "call $token i32.const 0 i32.const 18 call $query drop",
    ] {
        let code =
            format!("call $token i32.const 0 i32.const 18 call $query local.set $handle {tail}");
        let (call, _) = start(&host, &code, READ_CAPABILITIES, Box::new(()));
        let InvocationStep::Read(read) = advance(call) else {
            panic!("no read");
        };
        let dropped = Arc::new(AtomicUsize::new(0));
        let owner = Reply {
            bytes: br#"{"ok":true,"value":{"worlds":[]}}"#.to_vec(),
            dropped: Arc::clone(&dropped),
        };
        let resumed = read.resume(Box::new(owner)).unwrap();
        assert!(run_result(resumed).is_err(), "{tail}");
        assert_eq!(dropped.load(Ordering::Acquire), 1);
        assert_eq!(host.live_instances(), 0);
    }
}

#[test]
fn result_copy_uses_current_memory_after_a_guest_growth() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let code = "call $token i32.const 0 i32.const 18 call $query local.set $handle i32.const 1 memory.grow drop call $token local.get $handle i32.const 65536 call $token local.get $handle call $len call $read drop";
    let (call, _) = start(&host, code, READ_CAPABILITIES, Box::new(()));
    let InvocationStep::Read(read) = advance(call) else {
        panic!("no read");
    };
    assert!(matches!(
        advance(read.resume(reply()).unwrap()),
        InvocationStep::Finished(_)
    ));
}

#[test]
fn table_growth_can_yield_for_fuel_without_bypassing_resource_limits() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let module = fixture_module(
        "ref.null func i32.const 4096 table.grow 0 i32.const 1 i32.ne if unreachable end",
        "(table 1 funcref)",
    );
    let plugin = host
        .instantiate("table", &module, READ_CAPABILITIES)
        .unwrap();
    assert!(enable(plugin).is_enabled());
    assert!(
        host.instantiate(
            "two-tables",
            &fixture_module("", "(table 1 funcref) (table 1 funcref)"),
            READ_CAPABILITIES
        )
        .is_err()
    );
}

#[test]
fn genuine_memory_and_table_growth_limit_denials_still_trap() {
    let host = ResumableHost::new(
        Limits {
            memory_bytes: 65_536,
            ..Limits::default()
        },
        64,
    )
    .unwrap();
    let (call, _) = start(
        &host,
        "i32.const 1 memory.grow drop",
        READ_CAPABILITIES,
        Box::new(()),
    );
    assert!(
        matches!(run_result(call), Err(PluginError::Wasm(error)) if matches!(error.kind(), wasmi::errors::ErrorKind::TrapCode(wasmi::TrapCode::GrowthOperationLimited)))
    );
    let plugin = host
        .instantiate(
            "table",
            &fixture_module(
                "ref.null func i32.const 16384 table.grow 0 drop",
                "(table 1 funcref)",
            ),
            READ_CAPABILITIES,
        )
        .unwrap();
    let prepared = plugin
        .prepare(
            &Event::Enable {
                server_version: "0.0.1".into(),
            },
            &context(),
        )
        .unwrap();
    let call = plugin
        .begin(
            prepared,
            CallbackControl::new(Instant::now() + Duration::from_secs(5)),
            Box::new(()),
        )
        .unwrap();
    assert!(
        matches!(run_result(call), Err(PluginError::Wasm(error)) if matches!(error.kind(), wasmi::errors::ErrorKind::TrapCode(wasmi::TrapCode::GrowthOperationLimited)))
    );
    assert_eq!(host.live_instances(), 0);
}

#[test]
fn a_paused_plugin_does_not_prevent_another_owned_instance_running() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let (call, _) = start(
        &host,
        "call $token i32.const 0 i32.const 18 call $query drop",
        READ_CAPABILITIES,
        Box::new(()),
    );
    let InvocationStep::Read(paused) = advance(call) else {
        panic!("no read");
    };
    let second = host
        .instantiate("independent", &simple_module(""), READ_CAPABILITIES)
        .unwrap();
    let second = enable(second);
    assert!(second.is_enabled());
    assert_eq!(host.live_instances(), 2);
    drop(paused);
    assert_eq!(host.live_instances(), 1);
}

#[test]
fn unavailable_capabilities_return_a_result_without_yielding_to_a_provider() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let (call, _) = start(
        &host,
        "call $token i32.const 0 i32.const 18 call $query call $consume",
        0,
        Box::new(()),
    );
    let InvocationStep::Finished(done) = advance(call) else {
        panic!("denied query reached provider");
    };
    let (_, output) = done.commit().unwrap();
    assert_eq!(output.stats().native_calls, 1);
    assert!(output.stats().response_bytes > 0);
}

#[test]
fn invalid_provider_reply_is_normalized_and_dropped_before_resumption() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let (call, _) = start(
        &host,
        "call $token i32.const 0 i32.const 18 call $query call $consume",
        READ_CAPABILITIES,
        Box::new(()),
    );
    let InvocationStep::Read(read) = advance(call) else {
        panic!("no read");
    };
    let dropped = Arc::new(AtomicUsize::new(0));
    let owner = Reply {
        bytes: br#"{"ok":true,"value":{"players":[]}}"#.to_vec(),
        dropped: Arc::clone(&dropped),
    };
    let resumed = read.resume(Box::new(owner)).unwrap();
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    assert!(matches!(advance(resumed), InvocationStep::Finished(_)));
}

#[test]
fn native_call_count_is_global_to_the_callback_and_not_reset_on_resume() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let (mut call, _) = start(
        &host,
        "(loop $again call $token i32.const 0 i32.const 18 call $query call $consume br $again)",
        READ_CAPABILITIES,
        Box::new(()),
    );
    let mut queries = 0;
    loop {
        match run_result(call) {
            Ok(InvocationStep::Read(read)) => {
                queries += 1;
                assert!(queries <= 128);
                assert_eq!(read.identity().call_id, queries);
                call = read.resume(reply()).unwrap();
            }
            Err(PluginError::NativeProtocol(_)) => break,
            _ => panic!("unexpected callback result"),
        }
    }
    assert_eq!(queries, 128);
    assert_eq!(host.live_instances(), 0);
}

#[test]
fn cumulative_request_bytes_are_limited_before_another_provider_call() {
    let host = ResumableHost::new(
        Limits {
            fuel_per_event: 100_000_000,
            ..Limits::default()
        },
        50_000,
    )
    .unwrap();
    let mut query = r#"{"query":"worlds"}"#.to_owned();
    query.extend(std::iter::repeat_n(' ', 65_536 - query.len()));
    let code = "(loop $again call $token i32.const 0 i32.const 65536 call $query local.set $handle call $token local.get $handle i32.const 65536 call $token local.get $handle call $len call $read drop br $again)";
    let plugin = host
        .instantiate(
            "requests",
            &custom_fixture(code, "", 2, &query),
            READ_CAPABILITIES,
        )
        .unwrap();
    let prepared = plugin
        .prepare(
            &Event::Enable {
                server_version: "0.0.1".into(),
            },
            &context(),
        )
        .unwrap();
    let mut call = plugin
        .begin(
            prepared,
            CallbackControl::new(Instant::now() + Duration::from_secs(5)),
            Box::new(()),
        )
        .unwrap();
    let mut queries = 0;
    loop {
        match run_result(call) {
            Ok(InvocationStep::Read(read)) => {
                queries += 1;
                call = read.resume(reply()).unwrap();
            }
            Err(PluginError::NativeProtocol(_)) => break,
            _ => panic!("unexpected callback result"),
        }
    }
    assert_eq!(queries, 16);
}

#[test]
fn cumulative_result_bytes_include_every_completed_read() {
    let host = ResumableHost::new(
        Limits {
            fuel_per_event: 100_000_000,
            ..Limits::default()
        },
        50_000,
    )
    .unwrap();
    let module = custom_fixture(
        "(loop $again call $token i32.const 0 i32.const 18 call $query call $consume br $again)",
        "",
        5,
        r#"{"query":"worlds"}"#,
    );
    let plugin = host
        .instantiate("results", &module, READ_CAPABILITIES)
        .unwrap();
    let prepared = plugin
        .prepare(
            &Event::Enable {
                server_version: "0.0.1".into(),
            },
            &context(),
        )
        .unwrap();
    let mut call = plugin
        .begin(
            prepared,
            CallbackControl::new(Instant::now() + Duration::from_secs(5)),
            Box::new(()),
        )
        .unwrap();
    for query in 1..=5 {
        let InvocationStep::Read(read) = run_result(call).unwrap() else {
            panic!("expected read");
        };
        let mut bytes = br#"{"ok":true,"value":{"worlds":[]}}"#.to_vec();
        bytes.resize(262_144, b' ');
        match read.resume(Box::new(bytes)) {
            Ok(next) => {
                assert!(query < 5);
                call = next;
            }
            Err(PluginError::NativeProtocol(_)) => {
                assert_eq!(query, 5);
                return;
            }
            Err(error) => panic!("{error}"),
        }
    }
    panic!("response total was not enforced");
}

#[test]
fn malformed_query_encodings_and_guest_ranges_do_not_reach_the_provider() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    for query in [
        r#"{"query":"worlds","parent_id":1}"#,
        r#"{"query":"worlds","query":"worlds"}"#,
        "not json",
        "[]",
    ] {
        let code = format!(
            "call $token i32.const 0 i32.const {} call $query drop",
            query.len()
        );
        let plugin = host
            .instantiate(
                "malformed",
                &custom_fixture(&code, "", 1, query),
                READ_CAPABILITIES,
            )
            .unwrap();
        let prepared = plugin
            .prepare(
                &Event::Enable {
                    server_version: "0.0.1".into(),
                },
                &context(),
            )
            .unwrap();
        let call = plugin
            .begin(
                prepared,
                CallbackControl::new(Instant::now() + Duration::from_secs(5)),
                Box::new(()),
            )
            .unwrap();
        assert!(run_result(call).is_err());
    }
    for (pointer, len) in [(-1, 18), (65_530, 18), (i32::MAX, 18), (0, -1), (0, 65_537)] {
        let code = format!("call $token i32.const {pointer} i32.const {len} call $query drop");
        let (call, _) = start(&host, &code, READ_CAPABILITIES, Box::new(()));
        assert!(run_result(call).is_err());
    }
    assert_eq!(host.live_instances(), 0);
}

#[test]
fn semantic_invalid_arguments_receive_a_consumable_error_without_provider_dispatch() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    for query in [
        r#"{"query":"player","uuid":"00000000-0000-0000-0000-000000000000"}"#,
        r#"{"query":"player","uuid":"00000000-0000-0000-0000-000000000001","session_id":0}"#,
    ] {
        let code = format!(
            "call $token i32.const 0 i32.const {} call $query call $consume",
            query.len()
        );
        let plugin = host
            .instantiate(
                "invalid-value",
                &custom_fixture(&code, "", 1, query),
                READ_CAPABILITIES,
            )
            .unwrap();
        let plugin = enable(plugin);
        assert!(plugin.is_enabled());
    }
}

#[test]
fn unread_result_owner_is_released_at_completion_without_a_guest_copy() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let (call, _) = start(
        &host,
        "call $token i32.const 0 i32.const 18 call $query drop",
        READ_CAPABILITIES,
        Box::new(()),
    );
    let InvocationStep::Read(read) = advance(call) else {
        panic!("no read");
    };
    let dropped = Arc::new(AtomicUsize::new(0));
    let owner = Reply {
        bytes: br#"{"ok":true,"value":{"worlds":[]}}"#.to_vec(),
        dropped: Arc::clone(&dropped),
    };
    let resumed = read.resume(Box::new(owner)).unwrap();
    assert_eq!(dropped.load(Ordering::Acquire), 0);
    let InvocationStep::Finished(done) = advance(resumed) else {
        panic!("unexpected read");
    };
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    drop(done);
}

#[test]
fn expired_or_cancelled_pre_execution_admission_returns_the_unexecuted_instance() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    for cancelled in [false, true] {
        let plugin = host
            .instantiate("before-start", &simple_module(""), READ_CAPABILITIES)
            .unwrap();
        let prepared = plugin
            .prepare(
                &Event::Enable {
                    server_version: "0.0.1".into(),
                },
                &context(),
            )
            .unwrap();
        let control = CallbackControl::new(if cancelled {
            Instant::now() + Duration::from_secs(5)
        } else {
            Instant::now() - Duration::from_secs(1)
        });
        if cancelled {
            control.cancel();
        }
        let result = plugin.begin(prepared, control, Box::new(()));
        let Err(failed) = result else {
            panic!("cancelled admission succeeded");
        };
        assert!(!failed.instance.is_enabled());
        assert!(enable(failed.instance).is_enabled());
    }
}

#[test]
fn an_expired_suspended_callback_cannot_resume() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let plugin = host
        .instantiate(
            "expiry",
            &simple_module("call $token i32.const 0 i32.const 18 call $query drop"),
            READ_CAPABILITIES,
        )
        .unwrap();
    let prepared = plugin
        .prepare(
            &Event::Enable {
                server_version: "0.0.1".into(),
            },
            &context(),
        )
        .unwrap();
    let call = plugin
        .begin(
            prepared,
            CallbackControl::new(Instant::now() + Duration::from_millis(100)),
            Box::new(()),
        )
        .unwrap();
    let InvocationStep::Read(read) = advance(call) else {
        panic!("no read");
    };
    std::thread::sleep(Duration::from_millis(120));
    assert!(matches!(read.resume(reply()), Err(PluginError::Deadline)));
    assert_eq!(host.live_instances(), 0);
}

#[test]
fn abi_exports_cannot_query_and_start_functions_are_never_executed() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let query = wat::parse_str(r#"(module
        (import "zerqon_read_v1" "query" (func $query (param i64 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 0) "{\22query\22:\22worlds\22}")
        (func (export "zerqon_abi_version") (result i32) i64.const 1 i32.const 0 i32.const 18 call $query drop i32.const 1)
        (func (export "zerqon_on_event") (result i32) i32.const 0))"#).unwrap();
    assert!(
        host.instantiate("abi-query", &query, READ_CAPABILITIES)
            .is_err()
    );
    let start = wat::parse_str(
        r#"(module
        (func $start (loop $again br $again)) (start $start)
        (memory (export "memory") 1)
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (func (export "zerqon_on_event") (result i32) i32.const 0))"#,
    )
    .unwrap();
    assert!(
        host.instantiate("start", &start, READ_CAPABILITIES)
            .is_err()
    );
    assert_eq!(host.live_instances(), 0);
}

#[test]
fn abi_growth_failure_does_not_enter_the_callback_quantum_path() {
    let host = ResumableHost::new(
        Limits {
            fuel_per_event: 1000,
            ..Limits::default()
        },
        64,
    )
    .unwrap();
    let module = wat::parse_str(
        r#"(module
        (memory (export "memory") 1)
        (func (export "zerqon_abi_version") (result i32) i32.const 1 memory.grow drop i32.const 1)
        (func (export "zerqon_on_event") (result i32) i32.const 0))"#,
    )
    .unwrap();
    assert!(
        matches!(host.instantiate("abi-grow", &module, READ_CAPABILITIES), Err(PluginError::Wasm(error))
        if matches!(error.kind(), wasmi::errors::ErrorKind::TrapCode(wasmi::TrapCode::GrowthOperationLimited)))
    );
    assert_eq!(host.live_instances(), 0);
}

#[test]
fn rejected_host_context_and_cross_instance_preparation_do_not_execute_guests() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let first = host
        .instantiate("one", &simple_module(""), READ_CAPABILITIES)
        .unwrap();
    let second = host
        .instantiate("two", &simple_module(""), READ_CAPABILITIES)
        .unwrap();
    let event = Event::Enable {
        server_version: "0.0.1".into(),
    };
    let console: NativeCallbackContext =
        serde_json::from_str(r#"{"source":{"kind":"console"}}"#).unwrap();
    assert!(matches!(
        first.prepare(&event, &console),
        Err(PluginError::CallbackContext)
    ));
    let prepared = first.prepare(&event, &context()).unwrap();
    let result = second.begin(
        prepared,
        CallbackControl::new(Instant::now() + Duration::from_secs(5)),
        Box::new(()),
    );
    let Err(failure) = result else {
        panic!("cross-instance callback accepted");
    };
    assert!(enable(first).is_enabled());
    assert!(enable(failure.instance).is_enabled());
}

#[test]
fn oversized_provider_reply_becomes_a_small_consumable_capacity_response() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let (call, _) = start(
        &host,
        "call $token i32.const 0 i32.const 18 call $query call $consume",
        READ_CAPABILITIES,
        Box::new(()),
    );
    let InvocationStep::Read(read) = advance(call) else {
        panic!("no read");
    };
    let dropped = Arc::new(AtomicUsize::new(0));
    let owner = Reply {
        bytes: vec![b'x'; 262_145],
        dropped: Arc::clone(&dropped),
    };
    let resumed = read.resume(Box::new(owner)).unwrap();
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    let InvocationStep::Finished(done) = advance(resumed) else {
        panic!("unexpected read");
    };
    let (_, output) = done.commit().unwrap();
    assert!(output.stats().response_bytes < 1024);
}

#[test]
fn context_copy_total_and_exact_lengths_are_enforced() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    for code in [
        "call $token i32.const 1024 i32.const 1 call $context_read drop",
        "call $token i32.const -1 call $token call $context_len call $context_read drop",
        "(loop $again call $token i32.const 1024 call $token call $context_len call $context_read drop br $again)",
    ] {
        let (call, _) = start(&host, code, READ_CAPABILITIES, Box::new(()));
        assert!(matches!(
            run_result(call),
            Err(PluginError::NativeProtocol(_))
        ));
        assert_eq!(host.live_instances(), 0);
    }
}

#[test]
fn cancelled_successful_enable_can_only_run_one_cleanup_disable_without_publishing_enable_actions()
{
    let enabled = r#"{"action":"log","level":"info","message":"enabled"}"#;
    let disabled = r#"{"action":"log","level":"info","message":"disabled"}"#;
    let encode = |s: &str| {
        s.bytes()
            .map(|byte| format!("\\{byte:02x}"))
            .collect::<String>()
    };
    let extra = format!(
        "(data (i32.const 2048) \"{}\") (data (i32.const 4096) \"{}\")",
        encode(enabled),
        encode(disabled)
    );
    let handler = format!(
        "global.get $entries i32.const 1 i32.eq if i32.const 2048 i32.const {} call $emit drop else global.get $entries i32.const 2 i32.ne if unreachable end i32.const 4096 i32.const {} call $emit drop end",
        enabled.len(),
        disabled.len()
    );
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let plugin = host
        .instantiate(
            "cleanup",
            &fixture_module(&handler, &extra),
            READ_CAPABILITIES,
        )
        .unwrap();
    let prepared = plugin
        .prepare(
            &Event::Enable {
                server_version: "0.0.1".into(),
            },
            &context(),
        )
        .unwrap();
    let parent = CallbackControl::new(Instant::now() + Duration::from_secs(5));
    let released = Arc::new(AtomicUsize::new(0));
    let call = plugin
        .begin(
            prepared,
            parent.clone(),
            Box::new(Guard(Arc::clone(&released))),
        )
        .unwrap();
    let InvocationStep::Finished(done) = advance(call) else {
        panic!("unexpected read");
    };
    parent.cancel();
    let cleanup = done
        .begin_cleanup(
            CallbackControl::new(Instant::now() + Duration::from_secs(5)),
            Box::new(()),
        )
        .unwrap();
    assert_eq!(released.load(Ordering::Acquire), 1);
    let InvocationStep::Finished(done) = advance(cleanup) else {
        panic!("unexpected read");
    };
    let (stopped, output) = done.commit().unwrap();
    assert!(!stopped.is_enabled());
    assert_eq!(
        output.actions(),
        &[zerqon_plugin_api::Action::Log {
            level: zerqon_plugin_api::LogLevel::Info,
            message: "disabled".into()
        }]
    );
    assert!(matches!(
        stopped.prepare(&Event::Disable, &context()),
        Err(PluginError::Lifecycle)
    ));
}

#[test]
fn an_already_completed_disable_has_no_second_cleanup_callback() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let plugin = enable(
        host.instantiate("stopped", &simple_module(""), READ_CAPABILITIES)
            .unwrap(),
    );
    let prepared = plugin.prepare(&Event::Disable, &context()).unwrap();
    let call = plugin
        .begin(
            prepared,
            CallbackControl::new(Instant::now() + Duration::from_secs(5)),
            Box::new(()),
        )
        .unwrap();
    let InvocationStep::Finished(done) = advance(call) else {
        panic!("unexpected read");
    };
    assert!(matches!(
        done.begin_cleanup(
            CallbackControl::new(Instant::now() + Duration::from_secs(5)),
            Box::new(())
        ),
        Err(PluginError::Lifecycle)
    ));
    assert_eq!(host.live_instances(), 0);
}

#[test]
fn bulk_memory_copy_resumes_within_the_same_total_fuel_allowance() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let code = "call $token i32.const 0 i32.const 18 call $query call $consume i32.const 65536 i32.const 0 i32.const 65536 memory.copy i32.const 65536 i32.load8_u i32.const 123 i32.ne if unreachable end";
    let plugin = host
        .instantiate(
            "bulk",
            &custom_fixture(code, "", 2, r#"{"query":"worlds"}"#),
            READ_CAPABILITIES,
        )
        .unwrap();
    let prepared = plugin
        .prepare(
            &Event::Enable {
                server_version: "0.0.1".into(),
            },
            &context(),
        )
        .unwrap();
    let call = plugin
        .begin(
            prepared,
            CallbackControl::new(Instant::now() + Duration::from_secs(5)),
            Box::new(()),
        )
        .unwrap();
    let InvocationStep::Read(read) = advance(call) else {
        panic!("no read");
    };
    let mut remaining = read.fuel_remaining();
    let mut call = read.resume(reply()).unwrap();
    let mut quanta = 0;
    loop {
        match call.run().unwrap() {
            InvocationStep::Quantum(next) => {
                quanta += 1;
                assert!(next.fuel_remaining() <= remaining);
                remaining = next.fuel_remaining();
                call = next;
            }
            InvocationStep::Finished(done) => {
                let (_, output) = done.commit().unwrap();
                assert!(output.stats().fuel_consumed <= 1_000_000);
                break;
            }
            _ => panic!("unexpected read"),
        }
    }
    assert!(quanta > 0);
}

#[test]
fn owned_callback_preserves_locals_and_enters_the_guest_only_once() {
    let host = ResumableHost::new(Limits::default(), 64).unwrap();
    let plugin = host
        .instantiate("native", &two_queries(), READ_CAPABILITIES)
        .unwrap();
    let event = plugin
        .prepare(
            &Event::Enable {
                server_version: "0.0.1".into(),
            },
            &context(),
        )
        .unwrap();
    let control = CallbackControl::new(Instant::now() + Duration::from_secs(5));
    let invocation = plugin.begin(event, control, Box::new(())).unwrap();
    let InvocationStep::Read(first) = advance(invocation) else {
        panic!("missing first query");
    };
    assert_eq!(first.request(), &NativeQuery::Worlds);
    let identity = first.identity();
    let fuel = first.fuel_remaining();
    let next = first
        .resume(Box::new(br#"{"ok":true,"value":{"worlds":[]}}"#.to_vec()))
        .unwrap();
    let InvocationStep::Read(second) = advance(next) else {
        panic!("missing second query");
    };
    assert_eq!(second.identity().instance_id, identity.instance_id);
    assert_eq!(second.identity().callback_id, identity.callback_id);
    assert_eq!(second.identity().call_id, identity.call_id + 1);
    assert!(second.fuel_remaining() < fuel);
    let next = second
        .resume(Box::new(br#"{"ok":true,"value":{"worlds":[]}}"#.to_vec()))
        .unwrap();
    let InvocationStep::Finished(done) = advance(next) else {
        panic!("callback did not finish");
    };
    let (_plugin, output) = done.commit().unwrap();
    assert!(output.actions().is_empty());
    assert_eq!(output.stats().native_calls, 2);
}
