use super::*;
use crate::lifecycle::{DrainError, Lifecycle};
use std::{future::Future, task::Poll};
use tokio::{sync::Semaphore, time::Instant};

const OBSERVER: &str = "handoff-observer";

// Enable succeeds; the next callback traps, making an attempted Disable observable.
const DISABLE_OBSERVER: &[u8] = &[
    0, 97, 115, 109, 1, 0, 0, 0, 1, 5, 1, 96, 0, 1, 127, 3, 3, 2, 0, 0, 5, 3, 1, 0, 1, 6, 6, 1,
    127, 1, 65, 0, 11, 7, 49, 3, 6, 109, 101, 109, 111, 114, 121, 2, 0, 18, 122, 101, 114, 113,
    111, 110, 95, 97, 98, 105, 95, 118, 101, 114, 115, 105, 111, 110, 0, 0, 15, 122, 101, 114, 113,
    111, 110, 95, 111, 110, 95, 101, 118, 101, 110, 116, 0, 1, 10, 21, 2, 4, 0, 65, 1, 11, 14, 0,
    35, 0, 4, 64, 0, 11, 65, 1, 36, 0, 65, 0, 11,
];

#[test]
fn disable_observer_enables_successfully_and_traps_only_on_the_next_callback() {
    let mut plugins = PluginManager::new(Limits::default()).unwrap();
    assert!(plugins.load(OBSERVER, DISABLE_OBSERVER).unwrap().is_empty());
    assert_eq!(plugins.list().len(), 1);
    assert!(plugins.list()[0].enabled);
    assert!(plugins.unload(OBSERVER).is_err());
    assert!(plugins.list().is_empty());
}

async fn empty_state(lifecycle: &Lifecycle) -> (tempfile::TempDir, Arc<State>) {
    let directory = tempfile::tempdir().unwrap();
    crate::config::initialize(directory.path()).unwrap();
    let state = State::new_with_lifecycle(
        directory.path(),
        Properties::parse("online-mode=false\n").unwrap(),
        Settings::default(),
        true,
        None,
        lifecycle,
        &lifecycle.token(),
    )
    .await
    .unwrap();
    (directory, state)
}

async fn enabled_state(lifecycle: &Lifecycle) -> (tempfile::TempDir, Arc<State>) {
    let (directory, state) = empty_state(lifecycle).await;
    drop(
        state
            .plugins
            .load(
                OBSERVER,
                DISABLE_OBSERVER,
                &lifecycle.token(),
                Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap(),
    );
    (directory, state)
}

async fn signal(semaphore: &Semaphore) {
    tokio::time::timeout(Duration::from_secs(2), semaphore.acquire())
        .await
        .expect("startup handoff did not reach its barrier")
        .unwrap()
        .forget();
}

fn assert_disable_failure(result: std::result::Result<(), DrainError>) {
    assert!(
        matches!(result, Err(DrainError::Cleanup { ref failures, .. }) if failures.iter().any(|failure| failure.contains(OBSERVER))),
        "unread enabled state must run Disable and retain its failure: {result:?}"
    );
}

#[tokio::test]
async fn dropping_a_completed_unread_state_runs_disable_and_records_its_failure() {
    let lifecycle = Lifecycle::new();
    let (_directory, state) = enabled_state(&lifecycle).await;
    let returned = Arc::new(Semaphore::new(0));
    let returned_by_worker = Arc::clone(&returned);
    let stop = lifecycle.token();
    let mut result = Box::pin(crate::managed_tasks::run(
        &lifecycle,
        &stop,
        "unread enabled state",
        move |_| async move {
            returned_by_worker.add_permits(1);
            Ok::<_, crate::lifecycle::Stopped>(state)
        },
    ));
    std::future::poll_fn(|cx| {
        assert!(result.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    signal(&returned).await;
    lifecycle.stop();
    drop(result);
    assert_disable_failure(
        lifecycle
            .drain(Instant::now() + Duration::from_secs(2))
            .await,
    );
    assert!(lifecycle.pending().is_empty());
}

#[tokio::test]
async fn abandoning_the_waiter_before_state_delivery_keeps_disable_owned_by_the_drain() {
    let lifecycle = Lifecycle::new();
    let (_directory, state) = enabled_state(&lifecycle).await;
    let started = Arc::new(Semaphore::new(0));
    let started_by_worker = Arc::clone(&started);
    let release = Arc::new(Semaphore::new(0));
    let release_to_worker = Arc::clone(&release);
    let stop = lifecycle.token();
    let mut result = Box::pin(crate::managed_tasks::run(
        &lifecycle,
        &stop,
        "abandoned enabled state",
        move |_| async move {
            started_by_worker.add_permits(1);
            release_to_worker.acquire().await.unwrap().forget();
            Ok::<_, crate::lifecycle::Stopped>(state)
        },
    ));
    std::future::poll_fn(|cx| {
        assert!(result.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    signal(&started).await;
    drop(result);
    lifecycle.stop();
    let mut drain = Box::pin(lifecycle.drain(Instant::now() + Duration::from_secs(2)));
    std::future::poll_fn(|cx| {
        assert!(drain.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    release.add_permits(1);
    assert_disable_failure(drain.await);
    assert!(lifecycle.pending().is_empty());
}

#[tokio::test]
async fn cancellation_before_plugin_worker_admission_does_not_report_a_cleanup_failure() {
    let lifecycle = Lifecycle::new();
    let (_directory, state) = empty_state(&lifecycle).await;
    let stop = lifecycle.token().child();
    stop.request_stop();
    let result = state
        .plugins
        .load(
            OBSERVER,
            DISABLE_OBSERVER,
            &stop,
            Instant::now() + Duration::from_secs(2),
        )
        .await;
    assert!(matches!(
        result,
        Err(crate::rust_plugins::RustPluginError::Cancelled)
    ));
    assert!(state.plugins.list().is_empty());
    drop(state);
    lifecycle.stop();
    let drain = lifecycle
        .drain(Instant::now() + Duration::from_secs(2))
        .await;
    assert!(drain.is_ok(), "no plugin was created or enabled: {drain:?}");
    assert!(lifecycle.pending().is_empty());
}

#[tokio::test]
async fn cancelled_world_open_does_not_keep_database_workers_or_lifecycle_jobs() {
    let directory = tempfile::tempdir().unwrap();
    let world = directory.path().join("world");
    let lifecycle = Lifecycle::new();
    let stop = lifecycle.token().child();
    let cancel = stop.clone();
    let path = world.clone();
    let result = startup_work(&lifecycle, &stop, "cancelled world open", move || {
        let store = WorldStore::open(path)?;
        cancel.request_stop();
        Ok(store)
    })
    .await;
    assert!(matches!(result, Err(ref error) if crate::managed_tasks::is_cancelled(error.as_ref())));
    lifecycle.stop();
    lifecycle
        .drain(Instant::now() + Duration::from_secs(2))
        .await
        .unwrap();
    assert!(
        WorldStore::open(world).is_ok(),
        "cancelled construction retained the world lock"
    );
}

async fn join_barrier_state(lifecycle: &Lifecycle) -> (tempfile::TempDir, Arc<State>) {
    paired_barrier_state(lifecycle, 1).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires prepared 26.3 data, Java25 JoinBarrier and native-read Rust WASM fixtures"]
async fn reconnect_waits_for_closing_session_after_java_quit_before_login_success() {
    use crate::native_query_runtime::{
        MAX_PROVIDER_WORKING_SET_BYTES, NativeAdmission, NativeBackend, NativePluginKey,
        NativeProviderContract, NativeQueryRuntime, NativeScopeContext, WORKSPACE_BYTES,
    };
    use tokio::net::{TcpListener, TcpStream};
    use zerqon_protocol::codec::{
        get_string, get_varint, put_string, put_varint, read_frame, write_frame,
    };

    let lifecycle = Lifecycle::new();
    let (directory, state) = join_barrier_state(&lifecycle).await;
    let java_directory = directory.path().join("plugins/java-plugins");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let guest = std::env::var_os("ZERQON_TEST_NATIVE_WASM").map_or_else(
        || root.join(".cache/rust-plugin-fixtures/hello-native-read.wasm"),
        std::path::PathBuf::from,
    );
    drop(
        state
            .plugins
            .load(
                "native-quit",
                &std::fs::read(guest).expect("build the native-read Rust WASM fixture"),
                &lifecycle.token(),
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap(),
    );
    let uuid = Uuid::parse_str("34e0a659-c68d-35ad-bc0a-fc5378e6b5b9").unwrap();
    let (outbound, _packets) = tokio::sync::mpsc::channel(32);
    let (disconnect, _closed) = tokio::sync::watch::channel(false);
    let slot = Arc::clone(&state.slots).acquire_owned().await.unwrap();
    let player = state
        .players
        .reserve(
            crate::players::Identity {
                uuid,
                name: "CONSOLE".into(),
                properties: Vec::new(),
            },
            outbound,
            disconnect,
        )
        .unwrap();
    let old_id = player.id();
    let inventory = state
        .inventories
        .as_ref()
        .unwrap()
        .open(uuid, old_id)
        .await
        .unwrap();
    player.activate().unwrap();
    let session = crate::player_session::ConnectionSession::new(
        Arc::clone(&state),
        uuid,
        "CONSOLE".into(),
        player,
        inventory,
        slot,
    )
    .unwrap();
    std::fs::write(java_directory.join("join-release"), "release").unwrap();
    session.join().await.unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    client.set_nodelay(true).unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    let mut handshake = vec![0];
    put_varint(zerqon_protocol::PROTOCOL_VERSION, &mut handshake);
    put_string("localhost", &mut handshake);
    handshake.extend_from_slice(&listener.local_addr().unwrap().port().to_be_bytes());
    handshake.push(2);
    let mut hello = vec![0];
    put_string("CONSOLE", &mut hello);
    hello.extend_from_slice(&[0; 16]);
    assert!(hello.len() > handshake.len());
    write_frame(&mut client, &handshake, None).await.unwrap();
    write_frame(&mut client, &hello, None).await.unwrap();
    assert_eq!(state.packet_budget.snapshot().peak_bytes, 0);
    let mut incoming = Box::pin(crate::connection::handle(socket, Arc::clone(&state)));

    let queries = NativeQueryRuntime::new(
        state.queries.clone(),
        state.native_read_budget.clone(),
        lifecycle.clone(),
        NativeProviderContract::future_owned(MAX_PROVIDER_WORKING_SET_BYTES).unwrap(),
    );
    assert_eq!(state.native_read_budget.snapshot().reserved_bytes, 0);
    let workspace_count = state.native_read_budget.snapshot().limit_bytes / WORKSPACE_BYTES;
    let mut workspaces = Vec::with_capacity(workspace_count);
    for index in 0..workspace_count {
        workspaces.push(
            queries
                .try_scope(NativeScopeContext {
                    key: NativePluginKey::new(
                        NativeBackend::Wasm,
                        &format!("reconnect-held-{index}"),
                        1,
                    )
                    .unwrap(),
                    parent_id: index as u64 + 1,
                    deadline: Instant::now() + Duration::from_secs(30),
                    stop: lifecycle.token().child(),
                    admission: NativeAdmission::Normal,
                })
                .unwrap(),
        );
    }
    let mut closing = tokio::spawn(async move { session.close().await });
    let barrier = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let java_finished = std::fs::read_to_string(java_directory.join("join-counts"))
                .is_ok_and(|counts| counts == "1:1");
            if java_finished && state.plugins.stats().callbacks > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let retained_before_login = state.players.query_snapshot(uuid);
    let closing_was_pending = !closing.is_finished();
    let slots_before_login = state.slots.available_permits();
    let mut incoming_result = None;
    let progress = tokio::time::timeout(
        Duration::from_secs(5),
        std::future::poll_fn(|context| match incoming.as_mut().poll(context) {
            Poll::Ready(result) => {
                incoming_result = Some(result);
                Poll::Ready(())
            }
            Poll::Pending => {
                let budget = state.packet_budget.snapshot();
                if budget.peak_bytes >= hello.len() && budget.used_bytes == 0 {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
        }),
    )
    .await;
    let parsed = state.packet_budget.snapshot();
    let waited_for_old_session = incoming_result.is_none();
    let slots_while_waiting = state.slots.available_permits();
    let retained_during_login = state.players.query_snapshot(uuid);

    drop(workspaces);
    let first_frame = if incoming_result.is_some() {
        tokio::time::timeout(
            Duration::from_secs(5),
            read_frame(&mut client, 65_536, None),
        )
        .await
    } else {
        let mut reading = Box::pin(read_frame(&mut client, 65_536, None));
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                biased;
                frame = &mut reading => frame,
                result = &mut incoming => {
                    incoming_result = Some(result);
                    (&mut reading).await
                },
            }
        })
        .await
    };
    let first_frame = first_frame
        .map_err(|error| error.to_string())
        .and_then(|frame| frame.map_err(|error| error.to_string()));
    let first_login = first_frame.and_then(|packet| {
        let mut body = packet.as_slice();
        let id = get_varint(&mut body).map_err(|error| error.to_string())?;
        let description = match id {
            0 => {
                let json = get_string(&mut body, 32767).map_err(|error| error.to_string())?;
                serde_json::from_str::<serde_json::Value>(json)
                    .map_err(|error| error.to_string())?
                    .to_string()
            }
            3 => format!(
                "compression={}",
                get_varint(&mut body).map_err(|error| error.to_string())?
            ),
            other => return Err(format!("unexpected login packet {other}")),
        };
        if !body.is_empty() {
            return Err("trailing login packet bytes".into());
        }
        Ok((id, description))
    });
    drop(client);
    drop(incoming);
    let close_result = tokio::time::timeout(Duration::from_secs(5), &mut closing).await;
    if close_result.is_err() {
        closing.abort();
        let _ = closing.await;
    }
    let drained_sessions =
        tokio::time::timeout(Duration::from_secs(5), state.sessions.drain()).await;
    let remaining_player = state.players.query_snapshot(uuid);
    let slots_after_cleanup = state.slots.available_permits();
    let java_counts = std::fs::read_to_string(java_directory.join("join-counts"));
    lifecycle.stop();
    let java_shutdown = state.java.as_ref().unwrap().shutdown().await;
    let rust_shutdown = state
        .plugins
        .shutdown(Instant::now() + Duration::from_secs(5))
        .await;
    let rust_errors: Vec<_> = rust_shutdown
        .iter()
        .filter_map(|result| result.result.as_ref().err().map(ToString::to_string))
        .collect();
    drop(rust_shutdown);
    let profiler_shutdown = state.profiler.shutdown().await;
    let drained = lifecycle
        .drain(Instant::now() + Duration::from_secs(5))
        .await;
    assert!(
        barrier.is_ok(),
        "Java Quit/Rust workspace barrier did not settle"
    );
    assert!(
        closing_was_pending,
        "old session cleanup already finished before reconnect"
    );
    assert!(matches!(retained_before_login, Ok(Some(ref player)) if player.id == old_id));
    assert!(
        progress.is_ok(),
        "new login did not reach its admission boundary"
    );
    assert!(parsed.peak_bytes >= hello.len() && parsed.used_bytes == 0);
    assert!(matches!(retained_during_login, Ok(Some(ref player)) if player.id == old_id));
    assert!(
        matches!(close_result, Ok(Ok(Ok(())))),
        "old session close failed: {close_result:?}"
    );
    assert!(drained_sessions.is_ok(), "sessions did not drain");
    assert!(
        matches!(remaining_player, Ok(None)),
        "registration survived cleanup: {remaining_player:?}"
    );
    assert_eq!(slots_after_cleanup, state.properties.max_players);
    assert_eq!(java_counts.unwrap(), "1:1");
    assert!(
        java_shutdown
            .unwrap()
            .iter()
            .all(|result| result.error.is_none())
    );
    assert!(
        rust_errors.is_empty(),
        "Rust Disable failed: {rust_errors:?}"
    );
    assert!(profiler_shutdown.is_ok());
    assert!(drained.is_ok(), "lifecycle did not drain: {drained:?}");
    assert_eq!(slots_before_login, state.properties.max_players - 1);
    assert_eq!(slots_while_waiting, slots_before_login);
    assert!(
        waited_for_old_session,
        "login finished while old Rust Quit was held: {first_login:?}; handler={incoming_result:?}"
    );
    assert!(
        matches!(first_login, Ok((3, _))),
        "reconnect did not enter login after cleanup: {first_login:?}"
    );
}

async fn paired_barrier_state(
    lifecycle: &Lifecycle,
    expected_pairs: usize,
) -> (tempfile::TempDir, Arc<State>) {
    paired_barrier_state_with_timeout(lifecycle, expected_pairs, Duration::from_secs(5)).await
}

#[derive(Clone, Copy)]
enum RetainedReconnectEnd {
    Deadline,
    Stop,
    Active,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires prepared 26.3 data and Java25 JoinBarrier fixture"]
async fn reconnect_deadline_keeps_closed_session_resources_and_does_not_take_a_slot() {
    retained_resources_reconnect(RetainedReconnectEnd::Deadline).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires prepared 26.3 data and Java25 JoinBarrier fixture"]
async fn stopping_reconnect_wait_does_not_release_closed_session_resources() {
    retained_resources_reconnect(RetainedReconnectEnd::Stop).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires prepared 26.3 data and Java25 JoinBarrier fixture"]
async fn active_session_duplicate_login_is_rejected_without_replacement() {
    retained_resources_reconnect(RetainedReconnectEnd::Active).await;
}

async fn retained_resources_reconnect(end: RetainedReconnectEnd) {
    use tokio::net::{TcpListener, TcpStream};
    use zerqon_protocol::codec::{
        get_string, get_varint, put_string, put_varint, read_frame, write_frame,
    };

    let lifecycle = Lifecycle::new();
    let (directory, mut state) = join_barrier_state(&lifecycle).await;
    Arc::get_mut(&mut state)
        .unwrap()
        .settings
        .connection_timeout_seconds = 1;
    let java_directory = directory.path().join("plugins/java-plugins");
    let uuid = Uuid::parse_str("34e0a659-c68d-35ad-bc0a-fc5378e6b5b9").unwrap();
    let (outbound, _packets) = tokio::sync::mpsc::channel(32);
    let (disconnect, _closed) = tokio::sync::watch::channel(false);
    let slot = Arc::clone(&state.slots).acquire_owned().await.unwrap();
    let player = state
        .players
        .reserve(
            crate::players::Identity {
                uuid,
                name: "CONSOLE".into(),
                properties: Vec::new(),
            },
            outbound,
            disconnect,
        )
        .unwrap();
    let old_id = player.id();
    let inventory = state
        .inventories
        .as_ref()
        .unwrap()
        .open(uuid, old_id)
        .await
        .unwrap();
    player.activate().unwrap();
    let session = crate::player_session::ConnectionSession::new(
        Arc::clone(&state),
        uuid,
        "CONSOLE".into(),
        player,
        inventory,
        slot,
    )
    .unwrap();
    let retained = session.resources();
    std::fs::write(java_directory.join("join-release"), "release").unwrap();
    session.join().await.unwrap();
    if !matches!(end, RetainedReconnectEnd::Active) {
        session.close().await.unwrap();
    }
    let java_counts_before_login = std::fs::read_to_string(java_directory.join("join-counts"));
    let mut pending_drain = Box::pin(state.sessions.drain());
    let resources_still_registered = std::future::poll_fn(|context| {
        Poll::Ready(pending_drain.as_mut().poll(context).is_pending())
    })
    .await;
    drop(pending_drain);
    let slots_before_login = state.slots.available_permits();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    client.set_nodelay(true).unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    let mut handshake = vec![0];
    put_varint(zerqon_protocol::PROTOCOL_VERSION, &mut handshake);
    put_string("localhost", &mut handshake);
    handshake.extend_from_slice(&listener.local_addr().unwrap().port().to_be_bytes());
    handshake.push(2);
    let mut hello = vec![0];
    put_string("CONSOLE", &mut hello);
    hello.extend_from_slice(&[0; 16]);
    assert!(hello.len() > handshake.len());
    write_frame(&mut client, &handshake, None).await.unwrap();
    write_frame(&mut client, &hello, None).await.unwrap();
    assert_eq!(state.packet_budget.snapshot().peak_bytes, 0);
    let mut incoming = Box::pin(crate::connection::handle(socket, Arc::clone(&state)));
    let started = Instant::now();
    let mut incoming_result = None;
    let progress = tokio::time::timeout(
        Duration::from_secs(5),
        std::future::poll_fn(|context| match incoming.as_mut().poll(context) {
            Poll::Ready(result) => {
                incoming_result = Some(result);
                Poll::Ready(())
            }
            Poll::Pending => {
                let budget = state.packet_budget.snapshot();
                if budget.peak_bytes >= hello.len() && budget.used_bytes == 0 {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
        }),
    )
    .await;
    let parsed = state.packet_budget.snapshot();
    let waited = incoming_result.is_none();
    let slots_while_waiting = state.slots.available_permits();
    let mut stop_completed = true;
    if matches!(end, RetainedReconnectEnd::Stop) {
        lifecycle.stop();
        if incoming_result.is_none() {
            match tokio::time::timeout(Duration::from_secs(5), &mut incoming).await {
                Ok(result) => incoming_result = Some(result),
                Err(_) => stop_completed = false,
            }
        }
    }
    let first_frame = if incoming_result.is_some() || !stop_completed {
        tokio::time::timeout(
            Duration::from_secs(5),
            read_frame(&mut client, 65_536, None),
        )
        .await
    } else {
        let mut reading = Box::pin(read_frame(&mut client, 65_536, None));
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                biased;
                frame = &mut reading => frame,
                result = &mut incoming => {
                    incoming_result = Some(result);
                    (&mut reading).await
                },
            }
        })
        .await
    };
    let elapsed = started.elapsed();
    let first_read_completed = first_frame.is_ok();
    let first_frame = first_frame
        .map_err(|error| error.to_string())
        .and_then(|frame| frame.map_err(|error| error.to_string()));
    let first_login = first_frame.and_then(|packet| {
        let mut body = packet.as_slice();
        let id = get_varint(&mut body).map_err(|error| error.to_string())?;
        if id != 0 {
            return Err(format!("unexpected login packet {id}"));
        }
        let json = get_string(&mut body, 32767).map_err(|error| error.to_string())?;
        let reason = serde_json::from_str::<serde_json::Value>(json)
            .map_err(|error| error.to_string())?
            .to_string();
        if !body.is_empty() {
            return Err("trailing login-disconnect data".into());
        }
        Ok(reason)
    });
    let stopped_login = matches!(&incoming_result, Some(Err(error)) if crate::managed_tasks::is_cancelled(error.as_ref()));
    let retained_after_end = state.players.query_snapshot(uuid);
    let slots_after_end = state.slots.available_permits();
    drop(client);
    drop(incoming);
    let close_result = session.close().await;
    drop(session);
    let retained_after_close = state.players.query_snapshot(uuid);
    drop(retained);
    let session_drain = tokio::time::timeout(Duration::from_secs(5), state.sessions.drain()).await;
    let remaining = state.players.query_snapshot(uuid);
    let free_slots = state.slots.available_permits();
    let java_counts = std::fs::read_to_string(java_directory.join("join-counts"));
    lifecycle.stop();
    let java_shutdown = state.java.as_ref().unwrap().shutdown().await;
    let rust_shutdown = state
        .plugins
        .shutdown(Instant::now() + Duration::from_secs(5))
        .await;
    let rust_errors: Vec<_> = rust_shutdown
        .iter()
        .filter_map(|result| result.result.as_ref().err().map(ToString::to_string))
        .collect();
    drop(rust_shutdown);
    let profiler_shutdown = state.profiler.shutdown().await;
    let drain = lifecycle
        .drain(Instant::now() + Duration::from_secs(5))
        .await;

    assert!(
        resources_still_registered,
        "session.close returned but its held resources lost their registration"
    );
    assert!(
        progress.is_ok(),
        "login did not reach its admission boundary"
    );
    assert!(parsed.peak_bytes >= hello.len() && parsed.used_bytes == 0);
    assert_eq!(slots_before_login, state.properties.max_players - 1);
    assert_eq!(slots_while_waiting, slots_before_login);
    assert!(matches!(retained_after_end, Ok(Some(ref player)) if player.id == old_id));
    assert_eq!(slots_after_end, slots_before_login);
    assert!(
        close_result.is_ok(),
        "old session close failed: {close_result:?}"
    );
    assert!(matches!(retained_after_close, Ok(Some(ref player)) if player.id == old_id));
    assert!(session_drain.is_ok());
    assert!(matches!(remaining, Ok(None)));
    assert_eq!(free_slots, state.properties.max_players);
    assert_eq!(java_counts.unwrap(), "1:1");
    assert!(
        java_shutdown
            .unwrap()
            .iter()
            .all(|result| result.error.is_none())
    );
    assert!(
        rust_errors.is_empty(),
        "Rust Disable failed: {rust_errors:?}"
    );
    assert!(profiler_shutdown.is_ok());
    assert!(drain.is_ok(), "lifecycle did not drain: {drain:?}");
    match end {
        RetainedReconnectEnd::Deadline => {
            assert_eq!(java_counts_before_login.unwrap(), "1:1");
            assert!(
                waited,
                "closed notification bypassed the final resource fence: {first_login:?}"
            );
            assert!(
                elapsed >= Duration::from_secs(1),
                "login did not wait for its configured deadline: {elapsed:?}"
            );
            assert!(
                first_login
                    .as_ref()
                    .is_ok_and(|reason| reason.contains("closing")),
                "unclear deadline refusal: {first_login:?}"
            );
        }
        RetainedReconnectEnd::Stop => {
            assert_eq!(java_counts_before_login.unwrap(), "1:1");
            assert!(waited, "login did not wait on held resources before stop");
            assert!(
                stop_completed && stopped_login,
                "global stop did not cancel login: {incoming_result:?}"
            );
            assert!(
                first_read_completed && first_login.is_err(),
                "stopped login unexpectedly received a response: {first_login:?}"
            );
        }
        RetainedReconnectEnd::Active => {
            assert!(
                first_login
                    .as_ref()
                    .is_ok_and(|reason| reason.contains("already connected")),
                "active identity was not rejected: {first_login:?}"
            );
        }
    }
}

async fn paired_barrier_state_with_timeout(
    lifecycle: &Lifecycle,
    expected_pairs: usize,
    request_timeout: Duration,
) -> (tempfile::TempDir, Arc<State>) {
    use crate::native_query_runtime::{
        MAX_PROVIDER_WORKING_SET_BYTES, NativeProviderContract, NativeQueryRuntime,
    };
    use std::{io::Write, path::Path};
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let directory = tempfile::tempdir().unwrap();
    crate::config::initialize(directory.path()).unwrap();
    let mut state = State::new_with_lifecycle(
        directory.path(),
        Properties::parse("online-mode=false\nview-distance=1\n").unwrap(),
        Settings {
            data_directory: root
                .join(".cache/vanilla/26.3")
                .to_string_lossy()
                .into_owned(),
            java_plugins: false,
            spawn_mobs: false,
            update_check: false,
            ..Settings::default()
        },
        false,
        None,
        lifecycle,
        &lifecycle.token(),
    )
    .await
    .unwrap();
    let observer=wat::parse_str(format!(r#"(module
        (import "zerqon" "event_len" (func $len (result i32)))
        (import "zerqon" "event_read" (func $read (param i32 i32) (result i32)))
        (memory (export "memory") 1)
        (global $joins (mut i32) (i32.const 0)) (global $quits (mut i32) (i32.const 0))
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (func (export "zerqon_on_event") (result i32)
          i32.const 0 call $len call $read drop
          i32.const 17 i32.load8_u i32.const 106 i32.eq if global.get $joins i32.const 1 i32.add global.set $joins end
          i32.const 17 i32.load8_u i32.const 113 i32.eq if global.get $quits i32.const 1 i32.add global.set $quits end
          i32.const 10 i32.load8_u i32.const 100 i32.eq if
            global.get $joins i32.const {expected_pairs} i32.ne if unreachable end
            global.get $quits i32.const {expected_pairs} i32.ne if unreachable end
          end i32.const 0))"#)).unwrap();
    drop(
        state
            .plugins
            .load(
                "paired-join",
                &observer,
                &lifecycle.token(),
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap(),
    );
    let java_directory = directory.path().join("plugins/java-plugins");
    let class = root
        .join("target/java-plugins/fixtures/io/github/teighto/zerqon/fixtures/JoinBarrier.class");
    let artifact = std::fs::File::create(java_directory.join("join-barrier.jar")).unwrap();
    let mut jar = zip::ZipWriter::new(artifact);
    let options = zip::write::SimpleFileOptions::default();
    jar.start_file("META-INF/MANIFEST.MF", options).unwrap();
    jar.write_all(b"Manifest-Version: 1.0\nZerqon-Plugin: io.github.teighto.zerqon.fixtures.JoinBarrier\nZerqon-API: 1\n\n").unwrap();
    jar.start_file(
        "io/github/teighto/zerqon/fixtures/JoinBarrier.class",
        options,
    )
    .unwrap();
    jar.write_all(
        &std::fs::read(class).expect("compile JoinBarrier with the Java fixture builder"),
    )
    .unwrap();
    jar.finish().unwrap();
    let query_runtime = NativeQueryRuntime::new(
        state.queries.clone(),
        state.native_read_budget.clone(),
        lifecycle.clone(),
        NativeProviderContract::future_owned(MAX_PROVIDER_WORKING_SET_BYTES).unwrap(),
    );
    let java = JavaPlugins::spawn_with_query_runtime(
        JavaPluginConfig {
            java: "java".into(),
            host_jar: root.join("libraries/zerqon-java-host-0.0.1.jar"),
            gson_jar: root.join("libraries/gson-2.13.2.jar"),
            extra_classpath: Vec::new(),
            directory: java_directory.clone(),
            server_version: "0.0.1".into(),
            request_timeout,
            startup_timeout: Duration::from_secs(15),
            queue_capacity: 4,
        },
        query_runtime,
        lifecycle,
        &lifecycle.token(),
    )
    .await
    .unwrap();
    Arc::get_mut(&mut state).unwrap().java = Some(java);
    (directory, state)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires prepared 26.3 data, Java25 host and compiled JoinBarrier fixture"]
async fn global_stop_between_java_and_rust_join_keeps_both_join_quit_pairs() {
    let lifecycle = Lifecycle::new();
    let (directory, state) = join_barrier_state(&lifecycle).await;
    let java_directory = directory.path().join("plugins/java-plugins");
    let uuid = Uuid::from_u128(42);
    let (outbound, _receiver) = tokio::sync::mpsc::channel(32);
    let (disconnect, _closed) = tokio::sync::watch::channel(false);
    let player = state
        .players
        .reserve(
            crate::players::Identity {
                uuid,
                name: "Joiner".into(),
                properties: Vec::new(),
            },
            outbound,
            disconnect,
        )
        .unwrap();
    let inventory = state
        .inventories
        .as_ref()
        .unwrap()
        .open(uuid, player.id())
        .await
        .unwrap();
    player.activate().unwrap();
    let joined_state = Arc::clone(&state);
    let session = player.id();
    let join = tokio::spawn(async move {
        joined_state
            .player_event(uuid, session, "Joiner", true)
            .await
    });
    let entered = tokio::time::timeout(Duration::from_secs(3), async {
        while !java_directory.join("join-entered").is_file() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    lifecycle.stop();
    std::fs::write(java_directory.join("join-release"), "release").unwrap();
    let joined = tokio::time::timeout(Duration::from_secs(7), join).await;
    let quit = state.player_event(uuid, session, "Joiner", false).await;
    let java_shutdown = state.java.as_ref().unwrap().shutdown().await;
    let rust_shutdown = state
        .plugins
        .shutdown(Instant::now() + Duration::from_secs(5))
        .await;
    let rust_errors: Vec<_> = rust_shutdown
        .iter()
        .filter_map(|result| result.result.as_ref().err().map(ToString::to_string))
        .collect();
    drop(rust_shutdown);
    let java_counts = std::fs::read_to_string(java_directory.join("join-counts"));
    drop(inventory);
    drop(player);
    state.profiler.shutdown().await.unwrap();
    let drain = lifecycle
        .drain(Instant::now() + Duration::from_secs(5))
        .await;
    assert!(entered.is_ok(), "Java Join never reached its barrier");
    assert!(
        matches!(joined, Ok(Ok(Ok(())))),
        "Join did not finish: {joined:?}"
    );
    assert!(quit.is_ok(), "Quit failed: {quit:?}");
    assert_eq!(java_counts.unwrap(), "1:1");
    assert!(
        java_shutdown
            .unwrap()
            .iter()
            .all(|result| result.error.is_none())
    );
    assert!(
        rust_errors.is_empty(),
        "Rust received an unpaired Join/Quit: {rust_errors:?}"
    );
    assert!(drain.is_ok(), "lifecycle cleanup failed: {drain:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires prepared 26.3 data, Java25 host and compiled JoinBarrier fixture"]
async fn aborting_session_join_waiter_retains_resources_until_paired_quit() {
    let lifecycle = Lifecycle::new();
    let (directory, state) = join_barrier_state(&lifecycle).await;
    let java_directory = directory.path().join("plugins/java-plugins");
    let uuid = Uuid::from_u128(43);
    let (outbound, _receiver) = tokio::sync::mpsc::channel(32);
    let (disconnect, _closed) = tokio::sync::watch::channel(false);
    let player = state
        .players
        .reserve(
            crate::players::Identity {
                uuid,
                name: "Joiner".into(),
                properties: Vec::new(),
            },
            outbound,
            disconnect,
        )
        .unwrap();
    let inventory = state
        .inventories
        .as_ref()
        .unwrap()
        .open(uuid, player.id())
        .await
        .unwrap();
    player.activate().unwrap();
    let slots = Arc::new(Semaphore::new(1));
    let slot = Arc::clone(&slots).acquire_owned().await.unwrap();
    let session = Arc::new(
        crate::player_session::ConnectionSession::new(
            Arc::clone(&state),
            uuid,
            "Joiner".into(),
            player,
            inventory,
            slot,
        )
        .unwrap(),
    );
    let waiting_session = Arc::clone(&session);
    let join = tokio::spawn(async move { waiting_session.join().await });
    let entered = tokio::time::timeout(Duration::from_secs(3), async {
        while !java_directory.join("join-entered").is_file() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    join.abort();
    let aborted = join.await;
    drop(session);
    let held_drain = tokio::time::timeout(Duration::from_millis(25), state.sessions.drain()).await;
    let held_player = state.players.query_snapshot(uuid);
    let held_slots = slots.available_permits();
    lifecycle.stop();
    std::fs::write(java_directory.join("join-release"), "release").unwrap();
    let sessions_drained =
        tokio::time::timeout(Duration::from_secs(7), state.sessions.drain()).await;
    let released_player = state.players.query_snapshot(uuid);
    let released_slots = slots.available_permits();
    let java_shutdown = state.java.as_ref().unwrap().shutdown().await;
    let rust_shutdown = state
        .plugins
        .shutdown(Instant::now() + Duration::from_secs(5))
        .await;
    let rust_errors: Vec<_> = rust_shutdown
        .iter()
        .filter_map(|result| result.result.as_ref().err().map(ToString::to_string))
        .collect();
    drop(rust_shutdown);
    let java_counts = std::fs::read_to_string(java_directory.join("join-counts"));
    let profiler_shutdown = state.profiler.shutdown().await;
    let drain = lifecycle
        .drain(Instant::now() + Duration::from_secs(5))
        .await;
    assert!(entered.is_ok(), "Java Join never reached its barrier");
    assert!(
        matches!(aborted, Err(ref error) if error.is_cancelled()),
        "Join waiter did not abort: {aborted:?}"
    );
    assert!(held_drain.is_err(), "session drained during its Java Join");
    assert!(
        matches!(held_player, Ok(Some(_))),
        "player registration was released during its Java Join"
    );
    assert_eq!(
        held_slots, 0,
        "player slot was released during its Java Join"
    );
    assert!(sessions_drained.is_ok(), "session cleanup did not finish");
    assert!(
        matches!(released_player, Ok(None)),
        "player registration survived session cleanup"
    );
    assert_eq!(released_slots, 1, "player slot survived session cleanup");
    assert_eq!(java_counts.unwrap(), "1:1");
    assert!(
        java_shutdown
            .unwrap()
            .iter()
            .all(|result| result.error.is_none())
    );
    assert!(
        rust_errors.is_empty(),
        "Rust received an unpaired Join/Quit: {rust_errors:?}"
    );
    assert!(profiler_shutdown.is_ok(), "profiler cleanup failed");
    assert!(drain.is_ok(), "lifecycle cleanup failed: {drain:?}");
    assert!(lifecycle.pending().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires prepared 26.3 data, Java25 host and compiled JoinBarrier fixture"]
async fn simultaneous_session_quits_wait_for_java_queue_capacity() {
    const PLAYERS: usize = 8;
    let lifecycle = Lifecycle::new();
    let (directory, state) = paired_barrier_state(&lifecycle, PLAYERS).await;
    let java_directory = directory.path().join("plugins/java-plugins");
    std::fs::write(java_directory.join("join-release"), "release").unwrap();
    std::fs::write(java_directory.join("hold-quit"), "hold").unwrap();
    let slots = Arc::new(Semaphore::new(PLAYERS));
    let mut sessions = Vec::new();
    let mut channels = Vec::new();
    for index in 0..PLAYERS {
        let uuid = Uuid::from_u128(index as u128 + 100);
        let name = format!("Joiner{index}");
        let (outbound, receiver) = tokio::sync::mpsc::channel(32);
        let (disconnect, closed) = tokio::sync::watch::channel(false);
        channels.push((receiver, closed));
        let player = state
            .players
            .reserve(
                crate::players::Identity {
                    uuid,
                    name: name.clone(),
                    properties: Vec::new(),
                },
                outbound,
                disconnect,
            )
            .unwrap();
        let inventory = state
            .inventories
            .as_ref()
            .unwrap()
            .open(uuid, player.id())
            .await
            .unwrap();
        player.activate().unwrap();
        let slot = Arc::clone(&slots).acquire_owned().await.unwrap();
        let session = crate::player_session::ConnectionSession::new(
            Arc::clone(&state),
            uuid,
            name,
            player,
            inventory,
            slot,
        )
        .unwrap();
        session.join().await.unwrap();
        sessions.push(session);
    }
    let mut closing = tokio::task::JoinSet::new();
    let first = sessions.pop().unwrap();
    closing.spawn(async move { first.close().await.map_err(|error| error.to_string()) });
    let entered = tokio::time::timeout(Duration::from_secs(3), async {
        while !java_directory.join("quit-entered").is_file() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    lifecycle.stop();
    for session in sessions {
        closing.spawn(async move { session.close().await.map_err(|error| error.to_string()) });
    }
    let early_close = tokio::time::timeout(Duration::from_millis(250), closing.join_next()).await;
    std::fs::write(java_directory.join("quit-release"), "release").unwrap();
    let mut results = Vec::new();
    if let Ok(Some(result)) = early_close {
        results.push(result);
    }
    let close_drain = tokio::time::timeout(Duration::from_secs(7), async {
        while let Some(result) = closing.join_next().await {
            results.push(result);
        }
    })
    .await;
    if close_drain.is_err() {
        closing.abort_all();
        while let Some(result) = closing.join_next().await {
            results.push(result);
        }
    }
    let session_drain = tokio::time::timeout(Duration::from_secs(7), state.sessions.drain()).await;
    let java_counts = std::fs::read_to_string(java_directory.join("join-counts"));
    let java_shutdown = state.java.as_ref().unwrap().shutdown().await;
    let rust_shutdown = state
        .plugins
        .shutdown(Instant::now() + Duration::from_secs(5))
        .await;
    let rust_errors: Vec<_> = rust_shutdown
        .iter()
        .filter_map(|result| result.result.as_ref().err().map(ToString::to_string))
        .collect();
    drop(rust_shutdown);
    let profiler_shutdown = state.profiler.shutdown().await;
    let drain = lifecycle
        .drain(Instant::now() + Duration::from_secs(5))
        .await;
    assert!(entered.is_ok(), "Java Quit never reached its barrier");
    assert!(close_drain.is_ok(), "session close waiters did not finish");
    assert!(
        session_drain.is_ok(),
        "owned session cleanup did not finish"
    );
    assert_eq!(results.len(), PLAYERS);
    assert!(
        results.iter().all(|result| matches!(result, Ok(Ok(())))),
        "healthy session Quit was lost: {results:?}; Java counts: {java_counts:?}"
    );
    assert_eq!(java_counts.unwrap(), format!("{PLAYERS}:{PLAYERS}"));
    assert!(
        java_shutdown
            .unwrap()
            .iter()
            .all(|result| result.error.is_none())
    );
    assert!(
        rust_errors.is_empty(),
        "Rust pairing failed: {rust_errors:?}"
    );
    assert!(profiler_shutdown.is_ok(), "profiler cleanup failed");
    assert!(drain.is_ok(), "lifecycle cleanup failed: {drain:?}");
    assert_eq!(slots.available_permits(), PLAYERS);
    assert_eq!(state.online(), 0);
    assert!(lifecycle.pending().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires prepared 26.3 data, Java25 host and compiled JoinBarrier fixture"]
async fn timed_out_java_join_rejects_session_and_cleans_completed_rust_join() {
    let lifecycle = Lifecycle::new();
    let (directory, state) =
        paired_barrier_state_with_timeout(&lifecycle, 1, Duration::from_millis(100)).await;
    let uuid = Uuid::from_u128(44);
    let (outbound, _receiver) = tokio::sync::mpsc::channel(32);
    let (disconnect, _closed) = tokio::sync::watch::channel(false);
    let player = state
        .players
        .reserve(
            crate::players::Identity {
                uuid,
                name: "TimeoutJoiner".into(),
                properties: Vec::new(),
            },
            outbound,
            disconnect,
        )
        .unwrap();
    let id = player.id();
    let inventory = state
        .inventories
        .as_ref()
        .unwrap()
        .open(uuid, id)
        .await
        .unwrap();
    player.activate().unwrap();
    let slots = Arc::new(Semaphore::new(1));
    let slot = Arc::clone(&slots).acquire_owned().await.unwrap();
    let session = crate::player_session::ConnectionSession::new(
        Arc::clone(&state),
        uuid,
        "TimeoutJoiner".into(),
        player,
        inventory,
        slot,
    )
    .unwrap();
    let joined = session.join().await;
    let entered = directory
        .path()
        .join("plugins/java-plugins/join-entered")
        .is_file();
    let receipt = state
        .player_events
        .lock()
        .unwrap()
        .get(&(uuid, id))
        .map(|receipt| (receipt.java, receipt.rust.len()));
    lifecycle.stop();
    let closed = session.close().await;
    drop(session);
    let sessions_drained =
        tokio::time::timeout(Duration::from_secs(5), state.sessions.drain()).await;
    let java_closed = state.java.as_ref().unwrap().wait_closed().await;
    let rust_shutdown = state
        .plugins
        .shutdown(Instant::now() + Duration::from_secs(5))
        .await;
    let rust_errors: Vec<_> = rust_shutdown
        .iter()
        .filter_map(|result| result.result.as_ref().err().map(ToString::to_string))
        .collect();
    drop(rust_shutdown);
    let profiler_shutdown = state.profiler.shutdown().await;
    let drain = lifecycle
        .drain(Instant::now() + Duration::from_secs(5))
        .await;
    assert!(entered, "Java Join did not begin its callback");
    assert!(joined.is_err(), "timed out Java Join was accepted");
    assert_eq!(receipt, Some((None, 1)));
    assert!(
        closed.is_ok(),
        "successful Rust Join lost its Quit: {closed:?}"
    );
    assert!(sessions_drained.is_ok(), "session cleanup did not finish");
    assert!(java_closed.is_ok(), "timed out JVM was not reaped");
    assert!(
        rust_errors.is_empty(),
        "Rust pairing failed: {rust_errors:?}"
    );
    assert!(profiler_shutdown.is_ok(), "profiler cleanup failed");
    assert!(drain.is_ok(), "lifecycle cleanup failed: {drain:?}");
    assert!(state.player_events.lock().unwrap().is_empty());
    assert_eq!(state.online(), 0);
    assert_eq!(slots.available_permits(), 1);
    assert!(lifecycle.pending().is_empty());
}
