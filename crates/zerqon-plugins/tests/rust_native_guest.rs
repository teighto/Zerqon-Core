use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use zerqon_plugin_api::{
    Action, Event, SenderKind,
    native::{
        NativeCallbackContext, NativeEnvironment, NativeErrorCode, NativePlayer, NativePosition,
        NativeQuery, NativeQueryError, NativeQueryResponse, NativeQueryValue, NativeSource,
        NativeWorld, READ_CAPABILITIES, ServerSnapshot,
    },
};
use zerqon_plugins::{
    Limits,
    resumable::{CallbackControl, CallbackOutput, InvocationStep, PluginInstance, ResumableHost},
};

fn fixture(variable: &str, name: &str) -> Vec<u8> {
    let path = std::env::var_os(variable).map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../.cache/rust-plugin-fixtures")
                .join(name)
        },
        PathBuf::from,
    );
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!("compile the Rust WASM fixture or set {variable}: {path:?}: {error}")
    })
}

fn server_context() -> NativeCallbackContext {
    NativeCallbackContext {
        source: NativeSource::Server,
    }
}

fn player_context(player: &NativePlayer) -> NativeCallbackContext {
    NativeCallbackContext {
        source: NativeSource::Player {
            uuid: player.uuid,
            session_id: player.session_id,
        },
    }
}

fn player(session_id: i32) -> NativePlayer {
    NativePlayer {
        uuid: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
        session_id,
        entity_id: session_id,
        name: "CONSOLE".into(),
        world_uuid: "00000000-0000-0000-0000-000000000002".parse().unwrap(),
        position: NativePosition {
            x: -14027620.401934333,
            y: 64.0,
            z: -2.5,
            yaw: 90.0,
            pitch: -12.0,
        },
        operator: false,
        online: true,
    }
}

fn world() -> NativeWorld {
    NativeWorld {
        uuid: "00000000-0000-0000-0000-000000000002".parse().unwrap(),
        name: "world".into(),
        key: "minecraft:overworld".into(),
        dimension: "minecraft:overworld".into(),
        environment: NativeEnvironment::Normal,
        min_height: -64,
        max_height: 320,
        spawn: NativePosition {
            x: 0.5,
            y: 64.0,
            z: 0.5,
            yaw: 0.0,
            pitch: 0.0,
        },
    }
}

fn read_value(query: &NativeQuery, current: &NativePlayer) -> NativeQueryResponse {
    let value = match query {
        NativeQuery::Player { uuid, session_id } => NativeQueryValue::Player {
            player: (*uuid == current.uuid
                && session_id.is_none_or(|session| session == current.session_id))
            .then(|| current.clone()),
        },
        NativeQuery::ServerSnapshot => NativeQueryValue::ServerSnapshot(ServerSnapshot {
            server_version: "0.0.1".into(),
            minecraft_version: "26.3".into(),
            protocol: 777,
            max_players: 4096,
            online_players: 2048,
            tick: 7,
        }),
        NativeQuery::Worlds => NativeQueryValue::Worlds {
            worlds: vec![world()],
        },
        NativeQuery::OnlinePlayers => {
            panic!("rwhere must not request the complete player list")
        }
    };
    NativeQueryResponse::Success { value }
}

fn callback(
    plugin: PluginInstance,
    event: Event,
    context: NativeCallbackContext,
    mut provider: impl FnMut(&NativeQuery) -> NativeQueryResponse,
) -> (PluginInstance, CallbackOutput, Vec<NativeQuery>) {
    let prepared = plugin.prepare(&event, &context).unwrap();
    let instance = plugin.id();
    let control = CallbackControl::new(Instant::now() + Duration::from_secs(5));
    let invocation = plugin.begin(prepared, control, Box::new(())).unwrap();
    let mut step = invocation.run().unwrap();
    let mut queries = Vec::new();
    let mut callback_id = None;
    for _ in 0..512 {
        step = match step {
            InvocationStep::Quantum(invocation) => invocation.run().unwrap(),
            InvocationStep::Read(read) => {
                let identity = read.identity();
                assert_eq!(identity.instance_id, instance);
                assert_eq!(identity.call_id, queries.len() as u64 + 1);
                assert!(identity.callback_id > 0);
                assert_eq!(
                    *callback_id.get_or_insert(identity.callback_id),
                    identity.callback_id
                );
                let query = read.request().clone();
                let response = provider(&query);
                assert!(response.valid_for(&query));
                queries.push(query);
                let bytes = serde_json::to_vec(&response).unwrap();
                std::thread::spawn(move || read.resume(Box::new(bytes)).unwrap().run().unwrap())
                    .join()
                    .unwrap()
            }
            InvocationStep::Finished(completion) => {
                let (plugin, output) = completion.commit().unwrap();
                assert_eq!(output.stats().native_calls, queries.len() as u64);
                assert!(output.stats().fuel_consumed <= Limits::default().fuel_per_event);
                return (plugin, output, queries);
            }
        };
    }
    panic!("Rust callback did not finish within its bounded fuel segments");
}

fn no_query(query: &NativeQuery) -> NativeQueryResponse {
    panic!("unexpected native query: {query:?}");
}

fn command(name: &str, sender_kind: SenderKind) -> Event {
    Event::Command {
        sender: "CONSOLE".into(),
        sender_kind,
        operator: false,
        command: name.into(),
        args: Vec::new(),
    }
}

fn reply(output: &CallbackOutput) -> &str {
    let [Action::Reply { message }] = output.actions() else {
        panic!("expected a single reply, got {:?}", output.actions());
    };
    message
}

#[test]
#[ignore = "requires compiled native-read Rust WASM: set ZERQON_TEST_NATIVE_WASM"]
fn real_rust_guest_reads_trusted_player_world_and_exact_reconnected_session() {
    let host = ResumableHost::new(Limits::default(), 4096).unwrap();
    let plugin = host
        .instantiate(
            "native_rust",
            &fixture("ZERQON_TEST_NATIVE_WASM", "hello-native-read.wasm"),
            READ_CAPABILITIES,
        )
        .unwrap();
    let (plugin, enabled, _) = callback(
        plugin,
        Event::Enable {
            server_version: "0.0.1".into(),
        },
        server_context(),
        no_query,
    );
    assert!(plugin.is_enabled());
    assert!(
        enabled.actions().iter().any(
            |action| matches!(action, Action::RegisterCommand { name, .. } if name == "rwhere")
        )
    );
    drop(enabled);

    let original = player(7);
    let (plugin, output, queries) = callback(
        plugin,
        command("rwhere", SenderKind::Player),
        player_context(&original),
        |query| read_value(query, &original),
    );
    assert_eq!(
        queries,
        vec![
            NativeQuery::Player {
                uuid: original.uuid,
                session_id: Some(7),
            },
            NativeQuery::ServerSnapshot,
            NativeQuery::Worlds,
        ]
    );
    assert!(output.stats().segments > 1);
    assert_eq!(
        reply(&output),
        "CONSOLE: uuid=00000000-0000-0000-0000-000000000001, session=7, world=world(00000000-0000-0000-0000-000000000002), position=(-14027620.401934333,64,-2.5), online=2048/4096, protocol=777"
    );
    drop(output);

    let (plugin, unavailable, queries) = callback(
        plugin,
        command("rwhere", SenderKind::Player),
        player_context(&original),
        |_| NativeQueryResponse::Error {
            error: NativeQueryError::new(
                NativeErrorCode::DeadlineExceeded,
                "Read temporarily busy",
            ),
        },
    );
    assert!(plugin.is_enabled());
    assert_eq!(queries.len(), 1);
    assert_eq!(
        reply(&unavailable),
        "Native read failed: Read temporarily busy"
    );
    drop(unavailable);

    let (plugin, console, queries) = callback(
        plugin,
        command("rwhere", SenderKind::Console),
        NativeCallbackContext {
            source: NativeSource::Console,
        },
        no_query,
    );
    assert!(queries.is_empty());
    assert_eq!(reply(&console), "This command requires a player.");
    drop(console);

    let reconnected = NativePlayer {
        position: NativePosition {
            x: 1.25,
            ..original.position.clone()
        },
        ..player(8)
    };
    let (plugin, stale, queries) = callback(
        plugin,
        command("rwhere", SenderKind::Player),
        player_context(&original),
        |query| read_value(query, &reconnected),
    );
    assert_eq!(queries.len(), 1);
    assert_eq!(
        reply(&stale),
        "Native read failed: Player session is no longer active"
    );
    drop(stale);

    let (plugin, current, _) = callback(
        plugin,
        command("rwhere", SenderKind::Player),
        player_context(&reconnected),
        |query| read_value(query, &reconnected),
    );
    assert!(reply(&current).contains("session=8"));
    assert!(reply(&current).contains("position=(1.25,64,-2.5)"));
    drop(current);

    let (plugin, disabled, _) = callback(plugin, Event::Disable, server_context(), no_query);
    assert!(!plugin.is_enabled());
    assert!(matches!(disabled.actions(), [Action::Log { .. }]));
    drop(disabled);
    drop(plugin);
    assert_eq!(host.live_instances(), 0);
}

#[test]
#[ignore = "requires a compiled ABI 1 Rust WASM: set ZERQON_TEST_LEGACY_WASM"]
fn compiled_abi_one_rust_guest_keeps_lifecycle_commands_and_ticks_in_new_driver() {
    let host = ResumableHost::new(Limits::default(), 4096).unwrap();
    let plugin = host
        .instantiate(
            "legacy_rust",
            &fixture("ZERQON_TEST_LEGACY_WASM", "hello-abi1.wasm"),
            0,
        )
        .unwrap();
    let (mut plugin, enabled, _) = callback(
        plugin,
        Event::Enable {
            server_version: "0.0.1".into(),
        },
        server_context(),
        no_query,
    );
    assert!(
        enabled.actions().iter().any(
            |action| matches!(action, Action::RegisterCommand { name, .. } if name == "hello")
        )
    );
    assert!(
        !enabled.actions().iter().any(
            |action| matches!(action, Action::RegisterCommand { name, .. } if name == "rwhere")
        )
    );
    drop(enabled);
    for tick in 0..1000 {
        let (next, output, _) = callback(plugin, Event::Tick { tick }, server_context(), no_query);
        assert!(output.actions().is_empty());
        drop(output);
        plugin = next;
    }
    let (plugin, output, _) = callback(
        plugin,
        command("hello", SenderKind::Console),
        NativeCallbackContext {
            source: NativeSource::Console,
        },
        no_query,
    );
    assert_eq!(
        reply(&output),
        "Hello, CONSOLE! This plugin is written in Rust."
    );
    drop(output);
    let (plugin, disabled, _) = callback(plugin, Event::Disable, server_context(), no_query);
    assert!(!plugin.is_enabled());
    assert!(matches!(disabled.actions(), [Action::Log { .. }]));
    drop(disabled);
    drop(plugin);
    assert_eq!(host.live_instances(), 0);
}
