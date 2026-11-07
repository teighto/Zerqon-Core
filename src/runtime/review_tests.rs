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
    let directory = tempfile::tempdir().unwrap();
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
    assert!(world.cached_chunks().is_err());
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
    assert!(world.cached_chunks().is_err());
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
