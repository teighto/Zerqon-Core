use crate::{
    config::{Properties, Settings},
    player_inventory::InventoryHandle,
    players::{Identity, PacketBatch, PlayerHandle},
    runtime::{BlockInteraction, State, World, block_update},
};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;
use zerqon_world::{ChunkTicket, PlayerKey};

const PLAYER: Uuid = Uuid::from_u128(42);

async fn fixture() -> (tempfile::TempDir, Arc<State>) {
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
    let (shutdown, _) = watch::channel(false);
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
    let (sender, receiver) = mpsc::channel(32);
    let (disconnect, _) = watch::channel(false);
    let player = state
        .players
        .reserve(
            Identity {
                uuid: PLAYER,
                name: "MixedBuilder".into(),
                properties: Vec::new(),
            },
            sender,
            disconnect,
        )
        .unwrap();
    let service = state.inventories.as_ref().unwrap();
    let inventory = service.open(PLAYER, player.id()).await.unwrap();
    player.activate().unwrap();
    service.give(PLAYER, player.id(), "stone", 1).await.unwrap();
    let world = state.world.as_ref().unwrap();
    world
        .set_block(2, 63, 0, state.data.as_ref().unwrap().flat.stone)
        .await
        .unwrap();
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

// Own the barrier's release even if an assertion fails while the worker is blocked.
struct BlockedWorker {
    release: Option<std::sync::mpsc::Sender<()>>,
    ticket: Option<ChunkTicket<()>>,
}

impl BlockedWorker {
    async fn new(world: &World) -> Self {
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let ticket = world
            .workers
            .try_submit(move || {
                entered.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(10)).unwrap();
                Ok(())
            })
            .unwrap();
        ready.await.unwrap();
        Self {
            release: Some(release),
            ticket: Some(ticket),
        }
    }

    fn finish(mut self) {
        self.release.take().unwrap().send(()).unwrap();
        self.ticket.take().unwrap().wait().unwrap();
    }
}

impl Drop for BlockedWorker {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(ticket) = self.ticket.take() {
            let _ = ticket.wait();
        }
    }
}

async fn wait_for_admitted_edit(world: &World) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while world.inflight.available_permits() == 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn cancelled_queued_edit_retains_inventory_session_until_physical_completion() {
    let (_directory, state) = fixture().await;
    let world = Arc::clone(state.world.as_ref().unwrap());
    let (player, inventory, mut outbound) = actor(&state).await;
    while outbound.try_recv().is_ok() {}
    let blocker = BlockedWorker::new(&world).await;
    let editing = Arc::clone(&state);
    let id = player.id();
    let pending = tokio::spawn(async move {
        editing
            .edit_block(PLAYER, id, &inventory, placement())
            .await
    });
    wait_for_admitted_edit(&world).await;
    assert!(state.gameplay.try_read().is_err());
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    let held_jobs = state.lifecycle.pending();
    let held_permits = world.inflight.available_permits();
    let reopened_early = tokio::time::timeout(
        Duration::from_secs(2),
        state.inventories.as_ref().unwrap().open(PLAYER, id),
    )
    .await
    .unwrap();
    let rejected_reopen = reopened_early.is_err();
    drop(reopened_early);
    blocker.finish();
    let lease = tokio::time::timeout(Duration::from_secs(5), state.gameplay.read())
        .await
        .unwrap();
    let stone = state.data.as_ref().unwrap().flat.stone;
    assert_eq!(
        world.store.load(0, 0).unwrap().unwrap().get_block(2, 64, 0),
        Some(stone)
    );
    assert_eq!(
        outbound.recv().await.unwrap()[0].as_ref(),
        block_update(2, 64, 0, stone)
    );
    drop(lease);
    tokio::time::timeout(Duration::from_secs(3), async {
        while world.inflight.available_permits() != 8 || !state.lifecycle.pending().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let reopened_after = state
        .inventories
        .as_ref()
        .unwrap()
        .open(PLAYER, id)
        .await
        .unwrap();
    drop(reopened_after);
    assert!(
        rejected_reopen,
        "cancelled caller retired its inventory session before the accepted edit physically finished"
    );
    assert_eq!(held_permits, 7);
    assert!(
        !held_jobs.is_empty(),
        "accepted physical edit was absent from lifecycle drain"
    );
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn committed_cache_failure_poison_closes_inventory_without_stale_autosave_or_packets() {
    let (_directory, state) = fixture().await;
    let world = state.world.as_ref().unwrap();
    let (player, inventory, mut outbound) = actor(&state).await;
    let old_batch = inventory.full_sync().await.unwrap();
    assert!(old_batch.inventory_revision().is_some());
    assert!(inventory.allow_outbound(&old_batch));
    while outbound.try_recv().is_ok() {}
    let before = world
        .store
        .snapshot_domain(&[], &[PlayerKey::new(PLAYER)], &[])
        .unwrap();
    let cache = Arc::clone(&world.cache);
    assert!(
        std::thread::spawn(move || {
            let _guard = cache.lock().unwrap();
            panic!("controlled mixed-edit cache publication failure");
        })
        .join()
        .is_err()
    );
    let edit = state
        .edit_block(PLAYER, player.id(), &inventory, placement())
        .await;
    assert!(edit.is_err());
    assert!(*state.shutdown.borrow());
    assert_eq!(
        world.store.load(0, 0).unwrap().unwrap().get_block(2, 64, 0),
        Some(state.data.as_ref().unwrap().flat.stone)
    );
    let stale_packet_suppressed = !inventory.allow_outbound(&old_batch);
    let close = inventory.close_and_drain().await;
    let after = world
        .store
        .snapshot_domain(&[], &[PlayerKey::new(PLAYER)], &[])
        .unwrap();
    let unchanged_player = before.players[0].revision() == after.players[0].revision()
        && before.players[0].data() == after.players[0].data();
    assert!(
        close.is_err(),
        "committed world publication failure allowed a successful inventory finalizer; unchanged player={unchanged_player}, stale packet suppressed={stale_packet_suppressed}"
    );
    assert!(
        unchanged_player,
        "failed committed edit allowed a stale close-save to advance durable player state"
    );
    assert!(
        stale_packet_suppressed,
        "inventory snapshot prepared before terminal failure remained sendable"
    );
    assert!(outbound.try_recv().is_err());
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn queue_full_rejection_releases_admission_and_keeps_inventory_session_healthy() {
    let (_directory, state) = fixture().await;
    let world = state.world.as_ref().unwrap();
    let (player, inventory, _outbound) = actor(&state).await;
    let jobs_before = state.lifecycle.pending();
    let blocker = BlockedWorker::new(world).await;
    let queued: Vec<_> = (0..8)
        .map(|_| world.workers.try_submit(|| Ok(())).unwrap())
        .collect();
    let error = state
        .edit_block(PLAYER, player.id(), &inventory, placement())
        .await
        .unwrap_err();
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(error.as_ref());
    let mut queue_full = false;
    while let Some(current) = cause {
        if matches!(
            current.downcast_ref::<zerqon_world::WorldError>(),
            Some(zerqon_world::WorldError::QueueFull)
        ) {
            queue_full = true;
            break;
        }
        cause = current.source();
    }
    let permits_after = world.inflight.available_permits();
    let jobs_after = state.lifecycle.pending();
    let gameplay_released = state.gameplay.try_read().is_ok();
    blocker.finish();
    for ticket in queued {
        ticket.wait().unwrap();
    }
    assert!(queue_full, "expected bounded queue rejection, got {error}");
    assert_eq!(permits_after, 8);
    assert_eq!(jobs_after, jobs_before);
    assert!(gameplay_released);
    assert!(!inventory.test_admission_closed());
    assert!(!*state.shutdown.borrow());
    world.check_health().unwrap();
    let sync = inventory.full_sync().await.unwrap();
    assert!(inventory.allow_outbound(&sync));
    state
        .edit_block(PLAYER, player.id(), &inventory, placement())
        .await
        .unwrap();
    inventory.close_and_drain().await.unwrap();
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn abandoned_admitted_mixed_job_fails_closed_and_releases_physical_ownership() {
    let (_directory, state) = fixture().await;
    let world = state.world.as_ref().unwrap();
    let (player, inventory, mut outbound) = actor(&state).await;
    let old_batch = inventory.full_sync().await.unwrap();
    while outbound.try_recv().is_ok() {}
    let before = world
        .store
        .snapshot_domain(&[], &[PlayerKey::new(PLAYER)], &[])
        .unwrap();
    let before_chunk = world.store.load(0, 0).unwrap().unwrap().to_bytes();
    let job = super::prepare(&state, PLAYER, player.id(), &inventory, placement())
        .await
        .unwrap();
    let retained_gameplay = state.gameplay.try_read().is_err();
    let retained_permits = world.inflight.available_permits();
    let retained_jobs = !state.lifecycle.pending().is_empty();
    drop(job);
    let closed = inventory.close_and_drain().await;
    let after = world
        .store
        .snapshot_domain(&[], &[PlayerKey::new(PLAYER)], &[])
        .unwrap();
    assert!(retained_gameplay && retained_jobs);
    assert_eq!(retained_permits, 7);
    assert!(
        closed.is_err(),
        "an executor-abandoned admitted mutation was treated as clean completion"
    );
    assert!(*state.shutdown.borrow());
    assert!(world.check_health().is_err());
    assert!(!inventory.allow_outbound(&old_batch));
    assert_eq!(after.players[0].revision(), before.players[0].revision());
    assert_eq!(after.players[0].data(), before.players[0].data());
    assert_eq!(
        world.store.load(0, 0).unwrap().unwrap().to_bytes(),
        before_chunk
    );
    assert_eq!(world.inflight.available_permits(), 8);
    assert!(state.gameplay.try_read().is_ok());
    assert!(state.lifecycle.pending().is_empty());
    assert!(outbound.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn postcommit_panic_retains_gameplay_until_poison_and_prevents_close_autosave() {
    use std::{future::Future, task::Poll};

    let (_directory, state) = fixture().await;
    let world = state.world.as_ref().unwrap();
    let (player, inventory, mut outbound) = actor(&state).await;
    let old_batch = inventory.full_sync().await.unwrap();
    while outbound.try_recv().is_ok() {}
    let before = world
        .store
        .snapshot_domain(&[], &[PlayerKey::new(PLAYER)], &[])
        .unwrap();
    let mut job = super::prepare(&state, PLAYER, player.id(), &inventory, placement())
        .await
        .unwrap();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let Some(super::Operation::Block(operation)) = job.operation.as_mut() else {
        panic!("expected block edit");
    };
    operation.after_commit = Some(Box::new(move || {
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
        panic!("controlled postcommit mixed-job panic");
    }));
    let running = tokio::task::spawn_blocking(move || job.run());
    ready.await.unwrap();
    let guarded_after_commit = state.gameplay.try_read().is_err();
    let committed = world.store.load(0, 0).unwrap().unwrap().get_block(2, 64, 0);
    let mut close = Box::pin(inventory.close_and_drain());
    let early_close = std::future::poll_fn(|context| {
        Poll::Ready(match close.as_mut().poll(context) {
            Poll::Pending => None,
            Poll::Ready(result) => Some(result),
        })
    })
    .await;
    let close_waited = early_close.is_none();
    release.send(()).unwrap();
    let outcome = running.await.unwrap();
    let closed = match early_close {
        Some(result) => result,
        None => tokio::time::timeout(Duration::from_secs(3), close)
            .await
            .unwrap(),
    };
    let after = world
        .store
        .snapshot_domain(&[], &[PlayerKey::new(PLAYER)], &[])
        .unwrap();
    assert!(
        guarded_after_commit,
        "postcommit worker released gameplay before a terminal outcome was recorded"
    );
    assert!(
        close_waited,
        "inventory cleanup passed a still-running committed mutation"
    );
    assert_eq!(committed, Some(state.data.as_ref().unwrap().flat.stone));
    assert!(outcome.is_err());
    assert!(closed.is_err());
    assert!(*state.shutdown.borrow());
    assert!(world.check_health().is_err());
    assert!(!inventory.allow_outbound(&old_batch));
    assert_eq!(after.players[0].revision(), before.players[0].revision());
    assert_eq!(after.players[0].data(), before.players[0].data());
    assert_eq!(world.inflight.available_permits(), 8);
    assert!(state.gameplay.try_read().is_ok());
    assert!(state.lifecycle.pending().is_empty());
    assert!(outbound.try_recv().is_err());
}

async fn install_quit_observer(state: &State) {
    let observer = wat::parse_str(
        r#"(module
        (import "zerqon" "event_len" (func $len (result i32)))
        (import "zerqon" "event_read" (func $read (param i32 i32) (result i32)))
        (import "zerqon" "emit" (func $emit (param i32 i32) (result i32)))
        (memory (export "memory") 1)
        (global $quits (mut i32) (i32.const 0))
        (data (i32.const 1024) "{\"action\":\"reply\",\"message\":\"0\"}")
        (data (i32.const 1088) "{\"action\":\"reply\",\"message\":\"1\"}")
        (data (i32.const 1152) "{\"action\":\"reply\",\"message\":\"2\"}")
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (func (export "zerqon_on_event") (result i32)
          i32.const 0 call $len call $read drop
          i32.const 17 i32.load8_u i32.const 113 i32.eq
          if global.get $quits i32.const 1 i32.add global.set $quits end
          i32.const 10 i32.load8_u i32.const 99 i32.eq
          if global.get $quits i32.eqz
            if (result i32) i32.const 1024
            else global.get $quits i32.const 1 i32.eq
              if (result i32) i32.const 1088 else i32.const 1152 end
            end i32.const 32 call $emit drop
          end i32.const 0))"#,
    )
    .unwrap();
    state
        .plugins
        .load(
            "mixed-quit-counter",
            &observer,
            &state.lifecycle.token(),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
}

async fn observed_quits(state: &State) -> String {
    use zerqon_plugin_api::{
        Action, Event, SenderKind,
        native::{NativeCallbackContext, NativeSource},
    };
    let mut results = state
        .plugins
        .dispatch(
            &Event::Command {
                sender: "CONSOLE".into(),
                sender_kind: SenderKind::Console,
                operator: true,
                command: "count".into(),
                args: Vec::new(),
            },
            &NativeCallbackContext {
                source: NativeSource::Console,
            },
            Some("mixed-quit-counter"),
            &state.lifecycle.token(),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await;
    let output = results.pop().unwrap().result.unwrap();
    match output.actions() {
        [Action::Reply { message }] => message.clone(),
        actions => panic!("unexpected Quit observer reply: {actions:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn connection_session_close_and_quit_wait_for_cancelled_mixed_edit_settlement() {
    use std::{future::Future, task::Poll};

    let (_directory, state) = fixture().await;
    install_quit_observer(&state).await;
    let world = state.world.as_ref().unwrap();
    let (player, inventory, _outbound) = actor(&state).await;
    let slots_before = state.slots.available_permits();
    let slot = Arc::clone(&state.slots).acquire_owned().await.unwrap();
    let session = crate::player_session::ConnectionSession::new(
        Arc::clone(&state),
        PLAYER,
        "MixedBuilder".into(),
        player,
        inventory,
        slot,
    )
    .unwrap();
    session.join().await.unwrap();
    let resources = session.resources();
    let editing_resources = Arc::clone(&resources);
    let editing = Arc::clone(&state);
    let blocker = BlockedWorker::new(world).await;
    let caller = tokio::spawn(async move {
        editing
            .edit_block(
                PLAYER,
                editing_resources.registration.id(),
                &editing_resources.inventory,
                placement(),
            )
            .await
    });
    wait_for_admitted_edit(world).await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let mut closing = Box::pin(session.close());
    std::future::poll_fn(|context| {
        assert!(closing.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while !resources.inventory.test_admission_closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let quits_before_settlement = observed_quits(&state).await;
    let retained_slot = state.slots.available_permits() == slots_before - 1;
    std::future::poll_fn(|context| {
        assert!(closing.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    blocker.finish();
    tokio::time::timeout(Duration::from_secs(5), closing)
        .await
        .unwrap()
        .unwrap();
    let committed = world.store.load(0, 0).unwrap().unwrap().get_block(2, 64, 0);
    let quits_after_settlement = observed_quits(&state).await;
    // A repeated close must not dispatch Quit again.
    session.close().await.unwrap();
    let quits_after_repeated_close = observed_quits(&state).await;
    drop(resources);
    drop(session);
    tokio::time::timeout(Duration::from_secs(3), state.sessions.drain())
        .await
        .unwrap();
    state.lifecycle.stop();
    let disabled = state
        .plugins
        .shutdown(tokio::time::Instant::now() + Duration::from_secs(5))
        .await;
    assert!(disabled.iter().all(|result| result.result.is_ok()));
    drop(disabled);
    state.profiler.shutdown().await.unwrap();
    state
        .lifecycle
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        quits_before_settlement, "0",
        "Quit overtook accepted mixed world/inventory work"
    );
    assert!(retained_slot);
    assert_eq!(committed, Some(state.data.as_ref().unwrap().flat.stone));
    assert_eq!(quits_after_settlement, "1");
    assert_eq!(quits_after_repeated_close, "1");
    assert_eq!(state.slots.available_permits(), slots_before);
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn creative_coordinator_uses_latest_committed_mode_and_held_stack() {
    for change_mode in [true, false] {
        let (_directory, state) = fixture().await;
        let world = state.world.as_ref().unwrap();
        let (registration, inventory, mut outbound) = actor(&state).await;
        let old_batch = inventory.full_sync().await.unwrap();
        let unchanged_chunk = world.store.load(0, 0).unwrap().unwrap().to_bytes();
        let gameplay = Arc::clone(&state.gameplay).write_owned().await;
        let permit = inventory.reserve_mixed_io().await.unwrap();
        let lease = inventory
            .admit_mixed(
                crate::player_control::SessionKey {
                    uuid: PLAYER,
                    session_id: registration.id(),
                },
                permit,
            )
            .unwrap();
        let published_revision = {
            let io = lease.lock_io().unwrap();
            let mut player = io.lock_player().unwrap();
            let reads = world
                .store
                .snapshot_domain(&[], &[PlayerKey::new(PLAYER)], &[])
                .unwrap();
            player.check_snapshot(&reads.players[0]).unwrap();
            let mut candidate = player.current().record.clone();
            if change_mode {
                candidate.game_mode = crate::game_mode::GameMode::Survival;
                candidate.previous_game_mode = Some(crate::game_mode::GameMode::Creative);
            } else {
                let selected = 36 + i16::from(candidate.inventory.selected());
                candidate
                    .inventory
                    .set_slot(state.items.as_ref().unwrap(), selected, None)
                    .unwrap();
            }
            let prepared = player.prepare_after(candidate).unwrap();
            let receipt = world
                .store
                .commit_domain(
                    Uuid::new_v4(),
                    &reads,
                    &[],
                    std::slice::from_ref(prepared.write()),
                    &[],
                )
                .unwrap();
            prepared.publish(receipt.revision).enqueue().unwrap();
            receipt.revision
        };
        drop(gameplay);
        lease.settle(Ok(()));
        while outbound.try_recv().is_ok() {}
        let rejection = state
            .edit_block(PLAYER, registration.id(), &inventory, placement())
            .await
            .unwrap_err();
        assert!(
            !crate::managed_tasks::is_cleanup_failure(rejection.as_ref()),
            "valid changed player state was treated as terminal divergence: {rejection}"
        );
        assert_eq!(
            world.store.load(0, 0).unwrap().unwrap().to_bytes(),
            unchanged_chunk
        );
        let after = world
            .store
            .snapshot_domain(&[], &[PlayerKey::new(PLAYER)], &[])
            .unwrap();
        assert_eq!(after.players[0].revision(), published_revision);
        assert!(!inventory.allow_outbound(&old_batch));
        assert!(!inventory.test_admission_closed());
        assert!(!*state.shutdown.borrow());
        world.check_health().unwrap();
        inventory.close_and_drain().await.unwrap();
    }
}
