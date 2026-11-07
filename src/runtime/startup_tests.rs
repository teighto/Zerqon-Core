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

async fn paired_barrier_state(
    lifecycle: &Lifecycle,
    expected_pairs: usize,
) -> (tempfile::TempDir, Arc<State>) {
    paired_barrier_state_with_timeout(lifecycle, expected_pairs, Duration::from_secs(5)).await
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
