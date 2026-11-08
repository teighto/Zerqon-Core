use super::*;
use std::{future::Future, task::Poll};
use tokio::io::DuplexStream;

struct Fixture {
    state: Arc<State>,
    player: PlayerHandle,
    inventory: InventoryHandle,
    outbound: mpsc::Receiver<PacketBatch>,
    writer: Writer,
    client: DuplexStream,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        crate::config::initialize(directory.path()).unwrap();
        let (shutdown, _) = watch::channel(false);
        let state = State::new(
            directory.path(),
            crate::config::Properties::parse("online-mode=false\nview-distance=1\n").unwrap(),
            crate::config::Settings {
                data_directory: std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(".cache/vanilla/26.3")
                    .to_string_lossy()
                    .into_owned(),
                java_plugins: false,
                spawn_mobs: false,
                update_check: false,
                world_threads: 1,
                ..Default::default()
            },
            false,
            shutdown.clone(),
            None,
        )
        .await
        .unwrap();
        let (sender, mut outbound) = mpsc::channel(32);
        let (disconnect, _closed) = watch::channel(false);
        let uuid = Uuid::from_u128(1);
        let player = state
            .players
            .reserve(
                crate::players::Identity {
                    uuid,
                    name: "ChunkObserver".into(),
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
        while outbound.try_recv().is_ok() {}
        let (output, client) = tokio::io::duplex(8 * 1024 * 1024);
        let writer = Writer {
            io: Box::new(output),
            compression: None,
            deadline: Duration::from_secs(5),
            shutdown: shutdown.subscribe(),
            _control_transport: None,
        };
        Self {
            _directory: directory,
            state,
            player,
            inventory,
            outbound,
            writer,
            client,
        }
    }

    async fn cleanup(self) {
        drop(self.writer);
        drop(self.client);
        drop(self.inventory);
        drop(self.player);
        self.state.lifecycle.stop();
        self.state
            .event(zerqon_plugin_api::Event::Disable)
            .await
            .unwrap();
        self.state.profiler.shutdown().await.unwrap();
        self.state
            .lifecycle
            .drain(Instant::now() + Duration::from_secs(5))
            .await
            .unwrap();
    }
}

fn play_state(epoch: u64) -> PlayState {
    PlayState {
        radius: 0,
        loaded: HashSet::new(),
        center: (0, 0),
        position: (0.5, 64.0, 0.5),
        rotation: (0.0, 0.0),
        desired: HashSet::from([(0, 0)]),
        stream: None,
        stream_epoch: epoch,
        streaming: true,
        chunk_count: 0,
        last_keepalive: Instant::now(),
        outstanding: None,
        nonce: 0,
        bar: Uuid::nil(),
        show_bar: false,
        teleport_confirmed: true,
        teleport_id: 1,
        last_ack_sequence: 1,
        boundary_notice: false,
        rate_start: Instant::now(),
        packet_count: 0,
        inventory_count: 0,
    }
}

async fn ready_frame(client: &mut DuplexStream) -> Option<Vec<u8>> {
    let mut read = Box::pin(read_frame(client, 2_097_151, None));
    std::future::poll_fn(|context| {
        Poll::Ready(match read.as_mut().poll(context) {
            Poll::Pending => None,
            Poll::Ready(frame) => Some(frame.unwrap()),
        })
    })
    .await
}

async fn read_frames(
    client: &mut DuplexStream,
    mut frames: Vec<Vec<u8>>,
    count: usize,
) -> Vec<Vec<u8>> {
    while frames.len() < count {
        frames.push(
            timeout(Duration::from_secs(5), read_frame(client, 2_097_151, None))
                .await
                .unwrap()
                .unwrap(),
        );
    }
    frames
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn prepared_old_chunk_is_written_before_a_newer_block_update_received_after_snapshot_start() {
    let mut fixture = Fixture::new().await;
    let world = Arc::clone(fixture.state.world.as_ref().unwrap());
    let mut preparing = PendingChunk::new(
        &world,
        0,
        0,
        1,
        Instant::now() + Duration::from_secs(5),
        fixture.state.lifecycle.token(),
    );
    let allowed_before_prepare = !preparing.snapshot.started();
    let mut receiving = Box::pin(fixture.outbound.recv());
    std::future::poll_fn(|context| {
        assert!(receiving.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    let packet = preparing.completion.as_mut().await.unwrap();
    let old_bytes = packet.bytes().to_vec();
    let block = fixture.state.data.as_ref().unwrap().flat.bedrock;
    world.set_block(0, 70, 0, block).await.unwrap();
    let batch = receiving.await.unwrap();
    let update = batch[0].to_vec();
    let fresh_bytes = world.packet(0, 0).await.unwrap();
    let mut held = None;
    hold_or_send_outbound(
        &mut fixture.writer,
        &fixture.inventory,
        batch,
        Some(&preparing.snapshot),
        &mut held,
    )
    .await
    .unwrap();
    let held_before_publication = held.is_some();
    let early = ready_frame(&mut fixture.client).await;
    let mut play = play_state(1);
    publish_prepared_chunk(
        &mut fixture.writer,
        &fixture.inventory,
        &mut play,
        preparing,
        Ok(packet),
        &mut held,
    )
    .await
    .unwrap();
    let frames = read_frames(&mut fixture.client, early.clone().into_iter().collect(), 2).await;
    drop(world);
    fixture.cleanup().await;
    assert!(
        allowed_before_prepare,
        "fixture did not enter the pre-recv fence race"
    );
    assert_ne!(
        old_bytes, fresh_bytes,
        "fixture did not create a stale prepared snapshot"
    );
    assert_eq!(update[0], 8, "fixture did not receive a real block update");
    assert!(
        held_before_publication,
        "post-recv fence did not retain the update"
    );
    assert!(
        early.is_none(),
        "new block update was delivered before the old prepared chunk"
    );
    assert_eq!(frames, vec![old_bytes, update]);
    assert!(play.loaded.contains(&(0, 0)));
    assert_eq!(play.chunk_count, 1);
    assert!(held.is_none());
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn outbound_written_before_snapshot_start_is_followed_by_the_latest_chunk() {
    let mut fixture = Fixture::new().await;
    let world = Arc::clone(fixture.state.world.as_ref().unwrap());
    let old = world.packet(0, 0).await.unwrap();
    let mut preparing = PendingChunk::new(
        &world,
        0,
        0,
        1,
        Instant::now() + Duration::from_secs(5),
        fixture.state.lifecycle.token(),
    );
    let block = fixture.state.data.as_ref().unwrap().flat.bedrock;
    world.set_block(0, 70, 0, block).await.unwrap();
    let batch = fixture.outbound.recv().await.unwrap();
    let update = batch[0].to_vec();
    let mut held = None;
    hold_or_send_outbound(
        &mut fixture.writer,
        &fixture.inventory,
        batch,
        Some(&preparing.snapshot),
        &mut held,
    )
    .await
    .unwrap();
    let early = ready_frame(&mut fixture.client).await;
    let packet = preparing.completion.as_mut().await.unwrap();
    let current = packet.bytes().to_vec();
    let mut play = play_state(1);
    publish_prepared_chunk(
        &mut fixture.writer,
        &fixture.inventory,
        &mut play,
        preparing,
        Ok(packet),
        &mut held,
    )
    .await
    .unwrap();
    let frames = read_frames(&mut fixture.client, early.clone().into_iter().collect(), 2).await;
    drop(world);
    fixture.cleanup().await;
    assert_ne!(old, current);
    assert_eq!(
        early.as_ref(),
        Some(&update),
        "ordinary outbound stalled before snapshot admission"
    );
    assert_eq!(frames, vec![update, current]);
    assert_eq!(play.chunk_count, 1);
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn stale_epoch_or_no_longer_desired_preparation_writes_and_counts_nothing() {
    for removed_from_view in [false, true] {
        let mut fixture = Fixture::new().await;
        let world = Arc::clone(fixture.state.world.as_ref().unwrap());
        let mut preparing = PendingChunk::new(
            &world,
            0,
            0,
            1,
            Instant::now() + Duration::from_secs(5),
            fixture.state.lifecycle.token(),
        );
        let packet = preparing.completion.as_mut().await.unwrap();
        let mut play = play_state(if removed_from_view { 1 } else { 2 });
        if removed_from_view {
            play.desired.clear();
        }
        play.chunk_count = 7;
        play.loaded.insert((3, 4));
        let mut held = None;
        publish_prepared_chunk(
            &mut fixture.writer,
            &fixture.inventory,
            &mut play,
            preparing,
            Ok(packet),
            &mut held,
        )
        .await
        .unwrap();
        let frame = ready_frame(&mut fixture.client).await;
        drop(world);
        fixture.cleanup().await;
        assert!(frame.is_none(), "stale preparation reached the wire");
        assert_eq!(play.chunk_count, 7);
        assert_eq!(play.loaded, HashSet::from([(3, 4)]));
    }
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn held_inventory_batch_is_revalidated_at_actual_send_after_chunk_publication() {
    let mut fixture = Fixture::new().await;
    let world = Arc::clone(fixture.state.world.as_ref().unwrap());
    let mut preparing = PendingChunk::new(
        &world,
        0,
        0,
        1,
        Instant::now() + Duration::from_secs(5),
        fixture.state.lifecycle.token(),
    );
    let packet = preparing.completion.as_mut().await.unwrap();
    let chunk = packet.bytes().to_vec();
    let inventories = fixture.state.inventories.as_ref().unwrap();
    inventories
        .give(Uuid::from_u128(1), fixture.player.id(), "stone", 1)
        .await
        .unwrap();
    let first = fixture.outbound.recv().await.unwrap();
    let mut held = None;
    hold_or_send_outbound(
        &mut fixture.writer,
        &fixture.inventory,
        first,
        Some(&preparing.snapshot),
        &mut held,
    )
    .await
    .unwrap();
    inventories
        .give(Uuid::from_u128(1), fixture.player.id(), "stone", 1)
        .await
        .unwrap();
    let current = fixture.outbound.recv().await.unwrap();
    let expected = current
        .iter()
        .map(|packet| packet.to_vec())
        .collect::<Vec<_>>();
    let mut play = play_state(1);
    publish_prepared_chunk(
        &mut fixture.writer,
        &fixture.inventory,
        &mut play,
        preparing,
        Ok(packet),
        &mut held,
    )
    .await
    .unwrap();
    let first_wire = read_frames(&mut fixture.client, Vec::new(), 1).await;
    let stale_wire = ready_frame(&mut fixture.client).await;
    hold_or_send_outbound(
        &mut fixture.writer,
        &fixture.inventory,
        current,
        None,
        &mut held,
    )
    .await
    .unwrap();
    let rest = read_frames(
        &mut fixture.client,
        stale_wire.clone().into_iter().collect(),
        expected.len(),
    )
    .await;
    drop(world);
    fixture.cleanup().await;
    assert_eq!(first_wire, vec![chunk]);
    assert!(
        stale_wire.is_none(),
        "held inventory revision was checked before, not at, send"
    );
    assert_eq!(rest, expected);
    assert!(held.is_none());
}
