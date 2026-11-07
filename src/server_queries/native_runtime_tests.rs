use super::*;
use crate::{
    lifecycle::Lifecycle,
    native_query_runtime::{
        MAX_PROVIDER_WORKING_SET_BYTES, NativeAdmission, NativeBackend, NativeExecutionError,
        NativePluginKey, NativeProviderContract, NativeQueryRuntime, NativeReadBudget,
        NativeReplyFormat, NativeScopeContext, WORKSPACE_BYTES,
    },
    players::{EntityIds, Identity, PlayerHandle, Position},
};
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;
use zerqon_plugin_api::native::{NativeEnvironment, NativeQueryResponse};

fn world() -> NativeWorld {
    NativeWorld {
        uuid: Uuid::from_u128(42),
        name: "native-world".into(),
        key: "minecraft:overworld".into(),
        dimension: "minecraft:overworld".into(),
        environment: NativeEnvironment::Normal,
        min_height: -64,
        max_height: 320,
        spawn: NativePosition {
            x: 0.5,
            y: 72.0,
            z: 0.5,
            yaw: 0.0,
            pitch: 0.0,
        },
    }
}

fn fixture() -> (
    tempfile::TempDir,
    Arc<ServerQueries>,
    NativeQueryRuntime,
    NativeReadBudget,
    Lifecycle,
) {
    let directory = tempfile::tempdir().unwrap();
    let lifecycle = Lifecycle::new();
    let service = Arc::new(ServerQueries {
        players: Players::new(8, 64.0, 159, Arc::new(EntityIds::default())),
        tick: Arc::new(AtomicU64::new(37)),
        max_players: 8,
        shutdown: lifecycle.subscribe(),
        operators: Arc::new(Mutex::new(
            OperatorStore::load(&directory.path().join("ops.json")).unwrap(),
        )),
        worlds: Arc::from([world()]),
    });
    let budget = NativeReadBudget::new(2 * WORKSPACE_BYTES).unwrap();
    let runtime = NativeQueryRuntime::new(
        service.clone(),
        budget.clone(),
        lifecycle.clone(),
        NativeProviderContract::future_owned(MAX_PROVIDER_WORKING_SET_BYTES).unwrap(),
    );
    (directory, service, runtime, budget, lifecycle)
}

fn scope_context(lifecycle: &Lifecycle, backend: NativeBackend, name: &str) -> NativeScopeContext {
    NativeScopeContext {
        key: NativePluginKey::new(backend, name, 1).unwrap(),
        parent_id: 12,
        deadline: Instant::now() + Duration::from_secs(2),
        stop: lifecycle.token().child(),
        admission: NativeAdmission::Normal,
    }
}

fn player(service: &ServerQueries, uuid: Uuid, name: &str) -> PlayerHandle {
    let (outbound, _packets) = mpsc::channel(64);
    let (disconnect, _closed) = watch::channel(false);
    service
        .players
        .reserve(
            Identity {
                uuid,
                name: name.into(),
                properties: vec![("textures".into(), "not-a-native-read-field".into(), None)],
            },
            outbound,
            disconnect,
        )
        .unwrap()
}

fn decode(
    query: &NativeQuery,
    bytes: &[u8],
    format: NativeReplyFormat,
    call_id: u64,
) -> NativeQueryResponse {
    if format == NativeReplyFormat::Wasm {
        NativeQueryResponse::decode_for(query, bytes).unwrap()
    } else {
        assert!(bytes.ends_with(b"\n"));
        let frame: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(frame["kind"], "native_result");
        assert_eq!(frame["parent_id"], 12);
        assert_eq!(frame["call_id"], call_id);
        NativeQueryResponse::decode_for(query, &serde_json::to_vec(&frame["result"]).unwrap())
            .unwrap()
    }
}

async fn settled(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn actual_player_world_operator_and_tick_models_match_java_and_wasm_replies() {
    let (_directory, service, runtime, budget, lifecycle) = fixture();
    let admin = player(&service, Uuid::from_u128(1), "Admin");
    admin
        .move_to(Position {
            xyz: [-14_027_620.401_934_333, 85.125, 29_999_999.123_456_79],
            yaw: 31.25,
            pitch: -10.5,
            on_ground: true,
        })
        .unwrap();
    admin.activate().unwrap();
    let other = player(&service, Uuid::from_u128(2), "Visitor");
    other.activate().unwrap();
    let _inactive = player(&service, Uuid::from_u128(3), "Joining");
    service
        .operators
        .lock()
        .unwrap()
        .grant(Uuid::from_u128(1), "Admin", 4)
        .unwrap();
    let queries = [
        NativeQuery::ServerSnapshot,
        NativeQuery::OnlinePlayers,
        NativeQuery::Player {
            uuid: Uuid::from_u128(1),
            session_id: Some(admin.id()),
        },
        NativeQuery::Worlds,
        NativeQuery::Player {
            uuid: Uuid::from_u128(99),
            session_id: None,
        },
    ];
    let mut results = Vec::new();
    for (backend, format) in [
        (NativeBackend::Wasm, NativeReplyFormat::Wasm),
        (NativeBackend::Java, NativeReplyFormat::JavaProtocol2),
    ] {
        let scope = runtime
            .try_scope(scope_context(&lifecycle, backend, "real_service"))
            .unwrap();
        let mut responses = Vec::new();
        for (index, query) in queries.iter().enumerate() {
            let call = index as u64 + 1;
            let reply = scope.execute(call, query.clone(), format).await.unwrap();
            assert!(
                !std::str::from_utf8(reply.bytes())
                    .unwrap()
                    .contains("textures")
            );
            responses.push(decode(query, reply.bytes(), format, call));
        }
        results.push(responses);
        drop(scope);
    }
    assert_eq!(results[0], results[1]);
    let NativeQueryResponse::Success {
        value: NativeQueryValue::ServerSnapshot(snapshot),
    } = &results[0][0]
    else {
        panic!("missing server snapshot")
    };
    assert_eq!(
        (snapshot.online_players, snapshot.max_players, snapshot.tick),
        (2, 8, 37)
    );
    let NativeQueryResponse::Success {
        value: NativeQueryValue::OnlinePlayers { players },
    } = &results[0][1]
    else {
        panic!("missing player list")
    };
    assert_eq!(players.len(), 2);
    assert_eq!(players[0].uuid, Uuid::from_u128(1));
    assert_eq!(players[0].world_uuid, world().uuid);
    assert_eq!(players[0].position.x, -14_027_620.401_934_333);
    assert!(players[0].operator && players[0].online);
    assert!(!players[1].operator);
    assert_eq!(
        results[0][2],
        NativeQueryResponse::Success {
            value: NativeQueryValue::Player {
                player: Some(players[0].clone())
            }
        }
    );
    assert_eq!(
        results[0][3],
        NativeQueryResponse::Success {
            value: NativeQueryValue::Worlds {
                worlds: vec![world()]
            }
        }
    );
    assert_eq!(
        results[0][4],
        NativeQueryResponse::Success {
            value: NativeQueryValue::Player { player: None }
        }
    );
    settled(|| budget.snapshot().reserved_bytes == 0).await;
    lifecycle
        .drain(Instant::now() + Duration::from_secs(2))
        .await
        .unwrap();
}

#[tokio::test]
async fn reconnecting_the_same_uuid_does_not_reuse_a_stale_native_session() {
    let (_directory, service, runtime, budget, lifecycle) = fixture();
    let uuid = Uuid::from_u128(7);
    let old = player(&service, uuid, "Returning");
    old.activate().unwrap();
    let old_id = old.id();
    let scope = runtime
        .try_scope(scope_context(&lifecycle, NativeBackend::Wasm, "reconnect"))
        .unwrap();
    let query = NativeQuery::Player {
        uuid,
        session_id: Some(old_id),
    };
    let reply = scope
        .execute(1, query.clone(), NativeReplyFormat::Wasm)
        .await
        .unwrap();
    assert!(matches!(
        decode(&query, reply.bytes(), NativeReplyFormat::Wasm, 1),
        NativeQueryResponse::Success {
            value: NativeQueryValue::Player { player: Some(_) }
        }
    ));
    drop(reply);
    drop(old);
    let fresh = player(&service, uuid, "Returning");
    fresh.activate().unwrap();
    assert_ne!(fresh.id(), old_id);
    let reply = scope
        .execute(2, query.clone(), NativeReplyFormat::Wasm)
        .await
        .unwrap();
    assert_eq!(
        decode(&query, reply.bytes(), NativeReplyFormat::Wasm, 2),
        NativeQueryResponse::Success {
            value: NativeQueryValue::Player { player: None }
        }
    );
    drop(reply);
    let query = NativeQuery::Player {
        uuid,
        session_id: Some(fresh.id()),
    };
    let reply = scope
        .execute(3, query.clone(), NativeReplyFormat::Wasm)
        .await
        .unwrap();
    assert!(
        matches!(decode(&query, reply.bytes(), NativeReplyFormat::Wasm, 3), NativeQueryResponse::Success { value: NativeQueryValue::Player { player: Some(value) } } if value.session_id == fresh.id())
    );
    drop(reply);
    drop(scope);
    settled(|| budget.snapshot().reserved_bytes == 0).await;
    lifecycle
        .drain(Instant::now() + Duration::from_secs(2))
        .await
        .unwrap();
}

struct OperatorGate {
    release: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for OperatorGate {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

#[tokio::test]
async fn operator_contention_reaches_its_deadline_without_blocking_unrelated_queries_or_cleanup() {
    let (_directory, service, runtime, budget, lifecycle) = fixture();
    let operators = Arc::clone(&service.operators);
    let (started, running) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let _guard = operators.lock().unwrap();
        started.send(()).unwrap();
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
    });
    let gate = OperatorGate {
        release: Some(release),
        thread: Some(thread),
    };
    running.await.unwrap();
    let mut context = scope_context(&lifecycle, NativeBackend::Wasm, "locked");
    context.deadline = Instant::now() + Duration::from_millis(30);
    let locked = runtime.try_scope(context).unwrap();
    assert!(matches!(
        locked
            .execute(1, NativeQuery::OnlinePlayers, NativeReplyFormat::Wasm)
            .await,
        Err(NativeExecutionError::DeadlineExceeded)
    ));
    assert!(locked.is_terminal());
    settled(|| budget.snapshot().active_operations == 0).await;
    let available = runtime
        .try_scope(scope_context(&lifecycle, NativeBackend::Java, "available"))
        .unwrap();
    let worlds = available
        .execute(1, NativeQuery::Worlds, NativeReplyFormat::JavaProtocol2)
        .await
        .unwrap();
    assert_eq!(
        decode(
            &NativeQuery::Worlds,
            worlds.bytes(),
            NativeReplyFormat::JavaProtocol2,
            1
        ),
        NativeQueryResponse::Success {
            value: NativeQueryValue::Worlds {
                worlds: vec![world()]
            }
        }
    );
    drop(worlds);
    drop(gate);
    let players = available
        .execute(
            2,
            NativeQuery::OnlinePlayers,
            NativeReplyFormat::JavaProtocol2,
        )
        .await
        .unwrap();
    assert_eq!(
        decode(
            &NativeQuery::OnlinePlayers,
            players.bytes(),
            NativeReplyFormat::JavaProtocol2,
            2
        ),
        NativeQueryResponse::Success {
            value: NativeQueryValue::OnlinePlayers {
                players: Vec::new()
            }
        }
    );
    drop(players);
    drop(available);
    drop(locked);
    settled(|| budget.snapshot().reserved_bytes == 0).await;
    lifecycle
        .drain(Instant::now() + Duration::from_secs(2))
        .await
        .unwrap();
}
