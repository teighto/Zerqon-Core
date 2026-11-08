use super::*;
use crate::{
    player_inventory::InventoryHandle,
    players::{Identity, PacketBatch, PlayerHandle},
};
use tokio::sync::mpsc;

#[tokio::test]
#[ignore = "requires official game data and a built Java25 plugin host"]
async fn java_quit_context_failure_still_delivers_the_rust_quit_event() {
    let (_directory, mut state) = state().await;
    let (player, _inventory, _receiver) = actor(&state).await;
    // The guest counts PlayerQuit and reports 0, 1 or 2+ through a command reply.
    let observer: &[u8] = &[
        0, 97, 115, 109, 1, 0, 0, 0, 1, 11, 2, 96, 0, 1, 127, 96, 2, 127, 127, 1, 127, 2, 54, 3, 6,
        122, 101, 114, 113, 111, 110, 9, 101, 118, 101, 110, 116, 95, 108, 101, 110, 0, 0, 6, 122,
        101, 114, 113, 111, 110, 10, 101, 118, 101, 110, 116, 95, 114, 101, 97, 100, 0, 1, 6, 122,
        101, 114, 113, 111, 110, 4, 101, 109, 105, 116, 0, 1, 3, 3, 2, 0, 0, 5, 3, 1, 0, 1, 6, 6,
        1, 127, 1, 65, 0, 11, 7, 49, 3, 6, 109, 101, 109, 111, 114, 121, 2, 0, 18, 122, 101, 114,
        113, 111, 110, 95, 97, 98, 105, 95, 118, 101, 114, 115, 105, 111, 110, 0, 3, 15, 122, 101,
        114, 113, 111, 110, 95, 111, 110, 95, 101, 118, 101, 110, 116, 0, 4, 10, 79, 2, 4, 0, 65,
        1, 11, 72, 0, 65, 0, 16, 0, 16, 1, 26, 65, 17, 45, 0, 0, 65, 241, 0, 70, 4, 64, 35, 0, 65,
        1, 106, 36, 0, 11, 65, 10, 45, 0, 0, 65, 227, 0, 70, 4, 64, 35, 0, 69, 4, 127, 65, 128, 8,
        5, 35, 0, 65, 1, 70, 4, 127, 65, 192, 8, 5, 65, 128, 9, 11, 11, 65, 32, 16, 2, 26, 11, 65,
        0, 11, 11, 115, 3, 0, 65, 128, 8, 11, 32, 123, 34, 97, 99, 116, 105, 111, 110, 34, 58, 34,
        114, 101, 112, 108, 121, 34, 44, 34, 109, 101, 115, 115, 97, 103, 101, 34, 58, 34, 48, 34,
        125, 0, 65, 192, 8, 11, 32, 123, 34, 97, 99, 116, 105, 111, 110, 34, 58, 34, 114, 101, 112,
        108, 121, 34, 44, 34, 109, 101, 115, 115, 97, 103, 101, 34, 58, 34, 49, 34, 125, 0, 65,
        128, 9, 11, 32, 123, 34, 97, 99, 116, 105, 111, 110, 34, 58, 34, 114, 101, 112, 108, 121,
        34, 44, 34, 109, 101, 115, 115, 97, 103, 101, 34, 58, 34, 50, 34, 125, 0, 35, 4, 110, 97,
        109, 101, 1, 18, 3, 0, 3, 108, 101, 110, 1, 4, 114, 101, 97, 100, 2, 4, 101, 109, 105, 116,
        7, 8, 1, 0, 5, 113, 117, 105, 116, 115,
    ];
    state
        .plugins
        .load(
            "quit-observer",
            observer,
            &state.lifecycle.token(),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let java = JavaPlugins::spawn_with_services(
        JavaPluginConfig {
            java: "java".into(),
            host_jar: root.join("libraries/zerqon-java-host-0.0.1.jar"),
            gson_jar: root.join("libraries/gson-2.13.2.jar"),
            extra_classpath: Vec::new(),
            directory: _directory.path().join("plugins/java-plugins"),
            server_version: "0.0.1".into(),
            request_timeout: Duration::from_secs(2),
            startup_timeout: Duration::from_secs(15),
            queue_capacity: 4,
        },
        state.queries.clone(),
    )
    .await
    .unwrap();
    Arc::get_mut(&mut state).unwrap().java = Some(java);
    state
        .player_event(Uuid::from_u128(42), player.id(), "Builder", true)
        .await
        .unwrap();
    let count = Event::Command {
        sender: "CONSOLE".into(),
        sender_kind: SenderKind::Console,
        operator: true,
        command: "count".into(),
        args: Vec::new(),
    };
    assert_eq!(
        rust_actions(&state, "quit-observer", &count).await,
        [Action::Reply {
            message: "0".into()
        }]
    );
    let mut control = PluginManager::new(Limits::default()).unwrap();
    control.load("control", observer).unwrap();
    control
        .dispatch(
            "control",
            &Event::PlayerQuit {
                uuid: Uuid::from_u128(42).to_string(),
                name: "Builder".into(),
            },
        )
        .unwrap();
    assert_eq!(
        control.dispatch("control", &count).unwrap(),
        [Action::Reply {
            message: "1".into()
        }]
    );
    let operators = Arc::clone(&state.operators);
    let (locked, ready) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = operators.lock().unwrap();
        locked.send(()).unwrap();
        let _ = released.recv_timeout(Duration::from_secs(3));
    });
    ready.await.unwrap();
    let quit = state
        .player_event(Uuid::from_u128(42), player.id(), "Builder", false)
        .await;
    release.send(()).unwrap();
    holder.join().unwrap();
    let observed = rust_actions(&state, "quit-observer", &count).await;
    state.java.as_ref().unwrap().shutdown().await.unwrap();
    assert!(quit.is_err(), "Java context error must remain observable");
    assert_eq!(
        observed,
        [Action::Reply {
            message: "1".into()
        }],
        "Rust PlayerQuit must be delivered exactly once despite Java context failure"
    );
}

async fn rust_actions(state: &State, plugin: &str, event: &Event) -> Vec<Action> {
    let results = state
        .plugins
        .dispatch(
            event,
            &NativeCallbackContext {
                source: NativeSource::Console,
            },
            Some(plugin),
            &state.lifecycle.token(),
            tokio::time::Instant::now() + Duration::from_secs(2),
        )
        .await;
    results
        .into_iter()
        .flat_map(|result| result.result.unwrap().actions().to_vec())
        .collect()
}

async fn state() -> (tempfile::TempDir, Arc<State>) {
    state_in(tempfile::tempdir().unwrap()).await
}

async fn state_in(directory: tempfile::TempDir) -> (tempfile::TempDir, Arc<State>) {
    crate::config::initialize(directory.path()).unwrap();
    let settings = Settings {
        data_directory: Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".cache/vanilla/26.3")
            .to_str()
            .unwrap()
            .into(),
        world_threads: 1,
        work_queue_capacity: 8,
        java_plugins: false,
        spawn_mobs: false,
        update_check: false,
        ..Settings::default()
    };
    let (shutdown, _receiver) = watch::channel(false);
    let state = State::new(
        directory.path(),
        Properties::parse("online-mode=false").unwrap(),
        settings,
        false,
        shutdown,
        None,
    )
    .await
    .unwrap();
    (directory, state)
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn abandoned_chunk_packet_keeps_cleanup_pending_until_physical_completion() {
    use std::{future::Future, task::Poll};
    let (_directory, state) = state().await;
    let world = Arc::clone(state.world.as_ref().unwrap());
    assert!(state.lifecycle.pending().is_empty());
    let (release, holder) = hold_chunk_snapshot(&world).await;
    let parent = state.lifecycle.token();
    let snapshot = ChunkSnapshotFence::default();
    let mut packet = Box::pin(world.prepare_packet_with_fence(
        0,
        0,
        &parent,
        tokio::time::Instant::now() + Duration::from_secs(5),
        snapshot.clone(),
    ));
    std::future::poll_fn(|context| {
        assert!(packet.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    wait_for_chunk_snapshot(&snapshot).await;
    let admitted_permits = world.inflight.available_permits();
    drop(packet);
    state.lifecycle.stop();
    let pending = state
        .lifecycle
        .drain(tokio::time::Instant::now() + Duration::from_millis(20))
        .await;
    let retained_permits = world.inflight.available_permits();
    release.send(()).unwrap();
    holder.join().unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while world.inflight.available_permits() != 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let completed = state
        .lifecycle
        .drain(tokio::time::Instant::now() + Duration::from_secs(2))
        .await;
    assert_eq!(admitted_permits, 7);
    assert_eq!(
        retained_permits, 7,
        "abandoned packet released its physical work quota"
    );
    assert!(
        matches!(pending, Err(crate::lifecycle::DrainError::Deadline { .. })),
        "cleanup completed while a chunk worker was still blocked: {pending:?}"
    );
    completed.unwrap();
}

async fn hold_chunk_snapshot(
    world: &World,
) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let stripes = Arc::clone(&world.stripes);
    let (entered, locked) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = stripes[stripe(0, 0)].lock().unwrap();
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    locked.await.unwrap();
    (release, holder)
}

async fn wait_for_chunk_snapshot(snapshot: &ChunkSnapshotFence) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !snapshot.started() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn chunk_preparation_deadline_keeps_physical_work_and_output_owned() {
    use std::{future::Future, task::Poll};
    let (_directory, state) = state().await;
    let world = Arc::clone(state.world.as_ref().unwrap());
    let (release, holder) = hold_chunk_snapshot(&world).await;
    let parent = state.lifecycle.token();
    let snapshot = ChunkSnapshotFence::default();
    let mut packet = Box::pin(world.prepare_packet_with_fence(
        0,
        0,
        &parent,
        tokio::time::Instant::now() + Duration::from_secs(2),
        snapshot.clone(),
    ));
    std::future::poll_fn(|context| {
        assert!(packet.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    wait_for_chunk_snapshot(&snapshot).await;
    let result = packet.await;
    let work_permits = world.inflight.available_permits();
    let output_bytes = world.packet_output.available_permits();
    state.lifecycle.stop();
    let pending = state
        .lifecycle
        .drain(tokio::time::Instant::now() + Duration::from_millis(20))
        .await;
    release.send(()).unwrap();
    holder.join().unwrap();
    let completed = state
        .lifecycle
        .drain(tokio::time::Instant::now() + Duration::from_secs(2))
        .await;
    assert!(
        matches!(
            result
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<zerqon_world::WorldError>()),
            Some(zerqon_world::WorldError::Timeout)
        ),
        "unexpected packet result: {result:?}"
    );
    assert_eq!(work_permits, 7);
    assert_eq!(output_bytes, 28 * 1024 * 1024);
    assert!(
        matches!(pending, Err(crate::lifecycle::DrainError::Deadline { .. })),
        "{pending:?}"
    );
    completed.unwrap();
    assert_eq!(world.inflight.available_permits(), 8);
    assert_eq!(world.packet_output.available_permits(), 32 * 1024 * 1024);
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn prepared_chunk_outputs_bound_memory_without_holding_world_work_permits() {
    let (_directory, state) = state().await;
    let world = Arc::clone(state.world.as_ref().unwrap());
    let parent = state.lifecycle.token();
    let mut packets = Vec::new();
    for x in 0..8 {
        let packet = world
            .prepare_packet(
                x,
                0,
                &parent,
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert_eq!(packet.bytes()[0], 46);
        assert_eq!(&packet.bytes()[1..5], &x.to_be_bytes());
        packets.push(packet);
    }
    assert_eq!(world.packet_output.available_permits(), 0);
    assert_eq!(world.inflight.available_permits(), 8);
    let ninth = world
        .prepare_packet(
            100,
            0,
            &parent,
            tokio::time::Instant::now() + Duration::from_millis(20),
        )
        .await;
    assert!(
        matches!(
            ninth
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<zerqon_world::WorldError>()),
            Some(zerqon_world::WorldError::Timeout)
        ),
        "{ninth:?}"
    );
    assert!(world.cache.lock().unwrap().get(100, 0).is_none());
    assert_eq!(world.execute(|| Ok(42)).await.unwrap(), 42);
    state.lifecycle.stop();
    let pending = state
        .lifecycle
        .drain(tokio::time::Instant::now() + Duration::from_millis(20))
        .await;
    drop(packets);
    state
        .lifecycle
        .drain(tokio::time::Instant::now() + Duration::from_secs(2))
        .await
        .unwrap();
    assert!(
        matches!(pending, Err(crate::lifecycle::DrainError::Deadline { .. })),
        "{pending:?}"
    );
    assert_eq!(world.packet_output.available_permits(), 32 * 1024 * 1024);
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn chunk_snapshot_fence_starts_after_admission_and_before_snapshot_lock() {
    use std::{future::Future, task::Poll};
    let (_directory, state) = state().await;
    let world = Arc::clone(state.world.as_ref().unwrap());
    let output = Arc::clone(&world.packet_output)
        .acquire_many_owned(32 * 1024 * 1024)
        .await
        .unwrap();
    let stripes = Arc::clone(&world.stripes);
    let (entered, locked) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = stripes[stripe(0, 0)].lock().unwrap();
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    locked.await.unwrap();
    let parent = state.lifecycle.token();
    let fence = ChunkSnapshotFence::default();
    let mut packet = Box::pin(world.prepare_packet_with_fence(
        0,
        0,
        &parent,
        tokio::time::Instant::now() + Duration::from_secs(2),
        fence.clone(),
    ));
    std::future::poll_fn(|context| {
        assert!(packet.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    tokio::task::yield_now().await;
    let before_admission = fence.started();
    let work_before_admission = world.inflight.available_permits();
    drop(output);
    let started = tokio::time::timeout(Duration::from_secs(2), async {
        while !fence.started() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let work_during_snapshot = world.inflight.available_permits();
    release.send(()).unwrap();
    holder.join().unwrap();
    let packet = packet.await.unwrap();
    assert_eq!(packet.bytes()[0], 46);
    drop(packet);
    state.lifecycle.stop();
    state
        .lifecycle
        .drain(tokio::time::Instant::now() + Duration::from_secs(2))
        .await
        .unwrap();
    assert!(!before_admission);
    assert_eq!(work_before_admission, 8);
    started.unwrap();
    assert_eq!(work_during_snapshot, 7);
    assert!(fence.started());
}

async fn abandon_running_chunk(
    state: &Arc<State>,
) -> std::result::Result<(), crate::lifecycle::DrainError> {
    use std::{future::Future, task::Poll};
    let world = Arc::clone(state.world.as_ref().unwrap());
    let (release, holder) = hold_chunk_snapshot(&world).await;
    let parent = state.lifecycle.token();
    let snapshot = ChunkSnapshotFence::default();
    let mut packet = Box::pin(world.prepare_packet_with_fence(
        0,
        0,
        &parent,
        tokio::time::Instant::now() + Duration::from_secs(5),
        snapshot.clone(),
    ));
    std::future::poll_fn(|context| {
        assert!(packet.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    wait_for_chunk_snapshot(&snapshot).await;
    drop(packet);
    state.lifecycle.stop();
    let pending = state
        .lifecycle
        .drain(tokio::time::Instant::now() + Duration::from_millis(20))
        .await;
    release.send(()).unwrap();
    holder.join().unwrap();
    let completed = state
        .lifecycle
        .drain(tokio::time::Instant::now() + Duration::from_secs(2))
        .await;
    assert!(
        matches!(pending, Err(crate::lifecycle::DrainError::Deadline { .. })),
        "{pending:?}"
    );
    assert_eq!(world.inflight.available_permits(), 8);
    assert_eq!(world.packet_output.available_permits(), 32 * 1024 * 1024);
    completed
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn abandoned_missing_imported_chunk_is_not_a_cleanup_failure() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/zerqon-world/tests/fixtures");
    std::fs::create_dir_all(source.join("data/minecraft")).unwrap();
    std::fs::create_dir_all(source.join("region")).unwrap();
    for (fixture, destination) in [
        ("metadata-level.dat", "level.dat"),
        (
            "metadata-world-gen-settings.dat",
            "data/minecraft/world_gen_settings.dat",
        ),
        ("metadata-game-rules.dat", "data/minecraft/game_rules.dat"),
        ("negative-3.mca", "region/r.-1.-2.mca"),
    ] {
        std::fs::copy(fixtures.join(fixture), source.join(destination)).unwrap();
    }
    let resolver = crate::game_data::BlockResolver::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/vanilla/26.3"),
    )
    .unwrap();
    zerqon_world::anvil::import_world_blocks(
        &source,
        &directory.path().join("world"),
        &|name, properties| resolver.resolve(name, properties),
    )
    .unwrap();
    let (_directory, state) = state_in(directory).await;
    let world = state.world.as_ref().unwrap();
    let error = world
        .prepare_packet(
            0,
            0,
            &state.lifecycle.token(),
            tokio::time::Instant::now() + Duration::from_secs(2),
        )
        .await
        .expect_err("imported world unexpectedly generated a missing chunk");
    assert!(World::missing_imported_chunk(error.as_ref()));
    abandon_running_chunk(&state).await.unwrap();
    world.check_health().unwrap();
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn abandoned_corrupt_chunk_still_records_the_storage_failure() {
    let (_directory, state) = state().await;
    let world = state.world.as_ref().unwrap();
    let mut chunk = Chunk::flat(0, 0, state.data.as_ref().unwrap().flat);
    chunk.set_block(0, 64, 0, u32::MAX).unwrap();
    world.store.save(&chunk).unwrap();
    let result = abandon_running_chunk(&state).await;
    assert!(
        matches!(result, Err(crate::lifecycle::DrainError::Cleanup { ref failures, .. })
        if failures.iter().any(|failure| failure.contains("unknown block state"))),
        "{result:?}"
    );
}

async fn actor(state: &Arc<State>) -> (PlayerHandle, InventoryHandle, mpsc::Receiver<PacketBatch>) {
    let uuid = Uuid::from_u128(42);
    let (sender, receiver) = mpsc::channel(32);
    let (disconnect, _disconnected) = watch::channel(false);
    let player = state
        .players
        .reserve(
            Identity {
                uuid,
                name: "Builder".into(),
                properties: Vec::new(),
            },
            sender,
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
    (player, inventory, receiver)
}

fn placement() -> BlockInteraction {
    BlockInteraction::Place(zerqon_inventory::UseOnRequest {
        hand: zerqon_inventory::Hand::Main,
        block: zerqon_inventory::BlockPosition { x: 2, y: 63, z: 0 },
        face: zerqon_inventory::Face::Up,
        hit: [0.5, 1.0, 0.5],
        inside: false,
        world_border_hit: false,
        sequence: 1,
    })
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn native_player_context_waits_for_transient_permissions_without_blocking_network_tasks() {
    let (_directory, state) = state().await;
    let (player, _inventory, _receiver) = actor(&state).await;
    let operators = Arc::clone(&state.operators);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = operators.lock().unwrap();
        ready_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(2));
    });
    ready_rx.await.unwrap();
    let mut context = Box::pin(state.player_context(Uuid::from_u128(42), Some(player.id())));
    let started = std::time::Instant::now();
    assert!(
        tokio::time::timeout(Duration::from_millis(15), &mut context)
            .await
            .is_err()
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "context lookup blocked the executor"
    );
    release_tx.send(()).unwrap();
    let context = tokio::time::timeout(Duration::from_millis(250), context)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(context.player.session_id, player.id());
    assert_eq!(context.player.uuid, Uuid::from_u128(42));
    holder.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn cancelled_accepted_world_write_still_commits_publishes_and_releases_gameplay() {
    let (_directory, state) = state().await;
    let world = Arc::clone(state.world.as_ref().unwrap());
    let (_player, _inventory, mut outbound) = actor(&state).await;
    while outbound.try_recv().is_ok() {}
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = world
        .workers
        .try_submit(move || {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            Ok(())
        })
        .unwrap();
    started_rx.await.unwrap();
    let writing = Arc::clone(&world);
    let stone = state.data.as_ref().unwrap().flat.stone;
    let pending = tokio::spawn(async move { writing.set_block(2, 64, 0, stone).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while world.inflight.available_permits() == 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(state.gameplay.try_read().is_err());
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert!(state.gameplay.try_read().is_err());
    release_tx.send(()).unwrap();
    blocker.wait().unwrap();
    let lease = tokio::time::timeout(Duration::from_secs(5), state.gameplay.read())
        .await
        .unwrap();
    assert_eq!(
        world.store.load(0, 0).unwrap().unwrap().get_block(2, 64, 0),
        Some(stone)
    );
    assert_eq!(
        world
            .cache
            .lock()
            .unwrap()
            .get(0, 0)
            .unwrap()
            .get_block(2, 64, 0),
        Some(stone)
    );
    assert_eq!(
        outbound.recv().await.unwrap()[0].as_ref(),
        block_update(2, 64, 0, stone)
    );
    assert!(!*state.shutdown.borrow());
    drop(lease);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn cancelled_accepted_placement_keeps_its_inventory_snapshot_until_commit() {
    let (_directory, state) = state().await;
    let world = Arc::clone(state.world.as_ref().unwrap());
    let (player, inventory, mut outbound) = actor(&state).await;
    let uuid = Uuid::from_u128(42);
    state
        .inventories
        .as_ref()
        .unwrap()
        .give(uuid, player.id(), "stone", 1)
        .await
        .unwrap();
    let stone = state.data.as_ref().unwrap().flat.stone;
    world.set_block(2, 63, 0, stone).await.unwrap();
    while outbound.try_recv().is_ok() {}
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = world
        .workers
        .try_submit(move || {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            Ok(())
        })
        .unwrap();
    started_rx.await.unwrap();
    let editing = Arc::clone(&state);
    let id = player.id();
    let pending =
        tokio::spawn(async move { editing.edit_block(uuid, id, &inventory, placement()).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while world.inflight.available_permits() == 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert!(state.gameplay.try_read().is_err());
    release_tx.send(()).unwrap();
    blocker.wait().unwrap();
    let lease = tokio::time::timeout(Duration::from_secs(5), state.gameplay.read())
        .await
        .unwrap();
    assert_eq!(
        world.store.load(0, 0).unwrap().unwrap().get_block(2, 64, 0),
        Some(stone)
    );
    assert_eq!(
        world
            .cache
            .lock()
            .unwrap()
            .get(0, 0)
            .unwrap()
            .get_block(2, 64, 0),
        Some(stone)
    );
    assert_eq!(
        outbound.recv().await.unwrap()[0].as_ref(),
        block_update(2, 64, 0, stone)
    );
    assert!(!*state.shutdown.borrow());
    drop(lease);
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn normal_stop_cancels_world_reads_without_marking_storage_unavailable() {
    let (_directory, state) = state().await;
    let world = state.world.as_ref().unwrap();
    world.chunk(0, 0).await.unwrap();
    state.lifecycle.stop();
    let error = world
        .cached_chunks()
        .expect_err("stopped world accepted a read");
    world.store.check_available().unwrap();
    world.check_health().unwrap();
    assert!(
        crate::managed_tasks::is_cancelled(error.as_ref()),
        "ordinary stop was reported as a world failure: {error}"
    );
}

fn assert_failed_world_read(world: &World) {
    let health = world
        .check_health()
        .expect_err("world failure disappeared from the shutdown health check");
    assert!(
        !crate::managed_tasks::is_cancelled(health.as_ref()),
        "world health failure was reported as ordinary cancellation: {health}"
    );
    let error = world
        .cached_chunks()
        .expect_err("failed world accepted a read");
    assert!(
        !crate::managed_tasks::is_cancelled(error.as_ref()),
        "world failure was reported as ordinary cancellation: {error}"
    );
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn cache_publication_failure_after_commit_stops_world_without_claiming_rollback() {
    let (_directory, state) = state().await;
    let world = state.world.as_ref().unwrap();
    let (player, inventory, mut outbound) = actor(&state).await;
    state
        .inventories
        .as_ref()
        .unwrap()
        .give(Uuid::from_u128(42), player.id(), "stone", 1)
        .await
        .unwrap();
    let stone = state.data.as_ref().unwrap().flat.stone;
    world.set_block(2, 63, 0, stone).await.unwrap();
    while outbound.try_recv().is_ok() {}
    let cache = Arc::clone(&world.cache);
    assert!(
        std::thread::spawn(move || {
            let _guard = cache.lock().unwrap();
            panic!("controlled cache mutex poisoning");
        })
        .join()
        .is_err()
    );
    let mut shutdown = state.shutdown.subscribe();
    let result = state
        .edit_block(Uuid::from_u128(42), player.id(), &inventory, placement())
        .await;
    assert!(result.is_err());
    assert!(*shutdown.borrow_and_update());
    assert_eq!(
        world.store.load(0, 0).unwrap().unwrap().get_block(2, 64, 0),
        Some(stone)
    );
    assert_failed_world_read(world);
    assert!(outbound.try_recv().is_err());
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn corrupt_saved_block_state_is_not_echoed_as_a_rejection_correction() {
    let (_directory, state) = state().await;
    let world = state.world.as_ref().unwrap();
    let mut shutdown = state.shutdown.subscribe();
    let (player, inventory, mut outbound) = actor(&state).await;
    state
        .inventories
        .as_ref()
        .unwrap()
        .give(Uuid::from_u128(42), player.id(), "stone", 1)
        .await
        .unwrap();
    while outbound.try_recv().is_ok() {}
    world.chunk(0, 0).await.unwrap();
    let mut chunk = Chunk::flat(0, 0, state.data.as_ref().unwrap().flat);
    chunk.set_block(2, 63, 0, u32::MAX).unwrap();
    world.store.save(&chunk).unwrap();
    let result = state
        .edit_block(Uuid::from_u128(42), player.id(), &inventory, placement())
        .await;
    assert!(result.is_err());
    let invalid = block_update(2, 63, 0, u32::MAX);
    while let Ok(batch) = outbound.try_recv() {
        assert!(
            batch.iter().all(|packet| packet.as_ref() != invalid),
            "corrupt state was sent to client"
        );
    }
    assert!(*shutdown.borrow_and_update());
    assert_failed_world_read(world);
    assert_eq!(
        world.store.load(0, 0).unwrap().unwrap().get_block(2, 63, 0),
        Some(u32::MAX)
    );
}

#[test]
#[ignore = "requires prepared official Minecraft 26.3 data; runs an isolated cancellation child"]
fn cancelled_mob_job_can_release_the_last_world_reference_without_joining_itself() {
    const CHILD: &str = "ZERQON_WORLD_DROP_REVIEW_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let finished = runtime.block_on(async {
            let (_directory, state) = state().await;
            let world = Arc::clone(state.world.as_ref().unwrap());
            let (_player, _inventory, _outbound) = actor(&state).await;
            state
                .mobs
                .as_ref()
                .unwrap()
                .summon(zerqon_entities::MobKind::Pig, [2.5, 64.0, 0.5])
                .await
                .unwrap();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = world
                .workers
                .try_submit(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    Ok(())
                })
                .unwrap();
            started_rx.await.unwrap();
            let mobs = Arc::clone(state.mobs.as_ref().unwrap());
            let pending = tokio::spawn(async move { mobs.tick(1).await });
            tokio::time::timeout(Duration::from_secs(3), async {
                while world.inflight.available_permits() == 8 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            pending.abort();
            assert!(pending.await.unwrap_err().is_cancelled());
            let permits = Arc::clone(&world.inflight);
            drop(state);
            drop(world);
            release_tx.send(()).unwrap();
            blocker.wait().unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                while permits.available_permits() != 8 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .is_ok()
        });
        runtime.shutdown_timeout(Duration::from_millis(100));
        assert!(
            finished,
            "world worker did not finish when it owned the last World reference"
        );
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "runtime::review_tests::cancelled_mob_job_can_release_the_last_world_reference_without_joining_itself", "--ignored", "--nocapture"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "world-drop child timed out: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
