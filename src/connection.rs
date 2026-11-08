use crate::{
    Result,
    auth::{self, CipherReader, CipherWriter, VerifiedProfile},
    packets,
    player_inventory::InventoryHandle,
    player_session::LoginAdmission,
    players::{Identity, PacketBatch, PlayerError, PlayerHandle, Position},
    runtime::{BlockInteraction, State, World},
    world_view,
};
use md5::{Digest, Md5};
use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{Instant, timeout, timeout_at},
};
use uuid::Uuid;
#[cfg(test)]
use zerqon_protocol::codec::read_frame;
use zerqon_protocol::{
    MINECRAFT_VERSION, PROTOCOL_VERSION,
    codec::{get_string, get_varint, put_string, put_varint, take, write_frame},
    inbound::{ConnectionBudget, InboundPacket, read_frame_budgeted},
};

#[cfg(test)]
mod budget_wire_tests;
#[cfg(test)]
mod chunk_publication_tests;
#[cfg(test)]
mod control_write_tests;

struct Reader {
    io: Box<dyn AsyncRead + Unpin + Send>,
    budget: ConnectionBudget,
    compression: Option<usize>,
    limit: usize,
    deadline: Duration,
    shutdown: watch::Receiver<bool>,
}
struct Writer {
    io: Box<dyn AsyncWrite + Unpin + Send>,
    compression: Option<usize>,
    deadline: Duration,
    shutdown: watch::Receiver<bool>,
    _control_transport: Option<crate::player_control::TransportGuard>,
}
impl Reader {
    async fn packet(&mut self) -> Result<InboundPacket> {
        if *self.shutdown.borrow() {
            return Err("server stopping".into());
        }
        tokio::select! {result=timeout(self.deadline,read_frame_budgeted(&mut self.io,self.limit,self.compression,&self.budget))=>Ok(result??),_=self.shutdown.changed()=>Err("server stopping".into())}
    }
}
impl Writer {
    async fn send_control(
        &mut self,
        request: crate::player_control::QueuedMessage,
        key: crate::player_control::SessionKey,
    ) -> Result<()> {
        if let Some(writing) = request.begin_write(key) {
            match self.send(writing.packet()).await {
                Ok(()) => writing.finish_written(),
                Err(error) => {
                    writing.finish_closed();
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    async fn send(&mut self, packet: &[u8]) -> Result<()> {
        if *self.shutdown.borrow() {
            return Err("server stopping".into());
        }
        tokio::select! {
            result=timeout(self.deadline,write_frame(&mut self.io,packet,self.compression))=>{result??;Ok(())},
            _=self.shutdown.changed()=>Err("server stopping".into()),
        }
    }
}
struct AbortTask<T>(JoinHandle<T>);
impl<T> Drop for AbortTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub async fn handle(socket: TcpStream, state: Arc<State>) -> Result<()> {
    socket.set_nodelay(true)?;
    let (read, write) = socket.into_split();
    let deadline = Duration::from_secs(state.settings.connection_timeout_seconds);
    let mut reader = Reader {
        io: Box::new(read),
        budget: state
            .packet_budget
            .connection(state.settings.connection_packet_memory_mib * 1024 * 1024)?,
        compression: None,
        limit: state.settings.max_packet_bytes.min(65536),
        deadline,
        shutdown: state.shutdown.subscribe(),
    };
    let mut writer = Writer {
        io: Box::new(write),
        compression: None,
        deadline,
        shutdown: state.shutdown.subscribe(),
        _control_transport: None,
    };
    let packet = reader.packet().await?;
    let mut view = packet.as_slice();
    if get_varint(&mut view)? != 0 {
        return Err("expected handshake".into());
    }
    let protocol = get_varint(&mut view)?;
    get_string(&mut view, 255)?;
    take(&mut view, 2)?;
    let intent = get_varint(&mut view)?;
    if !view.is_empty() {
        return Err("trailing handshake bytes".into());
    }
    drop(packet);
    if intent == 1 {
        return status(&mut reader, &mut writer, &state).await;
    }
    if intent != 2 && intent != 3 {
        return Err("unsupported handshake intent".into());
    }
    if protocol != PROTOCOL_VERSION {
        return login_disconnect(&mut writer, "This server requires Minecraft 26.3").await;
    }
    if state.world.is_none() {
        return login_disconnect(
            &mut writer,
            "Zerqon is running in status-only diagnostic mode",
        )
        .await;
    }
    let hello = reader.packet().await?;
    let mut view = hello.as_slice();
    if get_varint(&mut view)? != 0 {
        return Err("expected login hello".into());
    }
    let name = get_string(&mut view, 16)?.to_owned();
    take(&mut view, 16)?;
    if !view.is_empty()
        || name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return login_disconnect(&mut writer, "Invalid username").await;
    }
    drop(hello);
    if !state.settings.allow_java_clients && !state.settings.allow_horizon_clients {
        return login_disconnect(
            &mut writer,
            crate::client_policy::PolicyError::AllDisabled.disconnect_reason(),
        )
        .await;
    }
    let profile = if let Some(authenticator) = &state.auth {
        let token = auth::fresh_verify_token()?;
        let mut request = vec![1];
        put_string("", &mut request);
        bytes(authenticator.public_key_der(), &mut request);
        bytes(&token, &mut request);
        request.push(1);
        writer.send(&request).await?;
        let response = reader.packet().await?;
        let mut view = response.as_slice();
        if get_varint(&mut view)? != 1 {
            return Err("expected encryption response".into());
        }
        let secret = byte_array(&mut view, 512)?;
        let challenge = byte_array(&mut view, 512)?;
        if !view.is_empty() {
            return Err("trailing encryption response".into());
        }
        let shared = authenticator.verify_response(secret, challenge, &token)?;
        drop(response);
        reader.io = Box::new(CipherReader::new(reader.io, &shared));
        writer.io = Box::new(CipherWriter::new(writer.io, &shared));
        let hash = auth::session_hash(&shared, authenticator.public_key_der());
        match authenticator.authenticate(&name, &hash, None).await {
            Ok(profile) => profile,
            Err(error) => {
                login_disconnect(&mut writer, &error.to_string()).await?;
                return Err(error.into());
            }
        }
    } else {
        VerifiedProfile {
            uuid: offline_uuid(&name),
            name,
            properties: Vec::new(),
        }
    };
    let stop = state.lifecycle.token();
    let admission_deadline = Instant::now() + deadline;
    let admission = tokio::select! {
        biased;
        _ = stop.cancelled() => return Err(crate::lifecycle::Stopped.into()),
        result = timeout_at(admission_deadline, state.sessions.wait_for_login(profile.uuid)) => {
            match result {
                Ok(admission) => admission,
                Err(_) => return login_disconnect(&mut writer, "Previous connection is still closing; please try again").await,
            }
        },
    };
    stop.check()?;
    if Instant::now() >= admission_deadline {
        return login_disconnect(
            &mut writer,
            "Previous connection is still closing; please try again",
        )
        .await;
    }
    if matches!(admission, LoginAdmission::Active) {
        return login_disconnect(&mut writer, &PlayerError::Duplicate.to_string()).await;
    }
    let Ok(slot) = Arc::clone(&state.slots).try_acquire_owned() else {
        return login_disconnect(&mut writer, "The server is full").await;
    };
    let (outbound, packets) = mpsc::channel(32);
    let (disconnect_tx, disconnect_rx) = watch::channel(false);
    let identity = Identity {
        uuid: profile.uuid,
        name: profile.name.clone(),
        properties: profile
            .properties
            .iter()
            .map(|property| {
                (
                    property.name.clone(),
                    property.value.clone(),
                    property.signature.clone(),
                )
            })
            .collect(),
    };
    let registration = match state
        .players
        .reserve(identity, outbound.clone(), disconnect_tx)
    {
        Ok(handle) => handle,
        Err(error) => {
            return login_disconnect(&mut writer, &error.to_string()).await;
        }
    };
    let inventory = match state
        .inventories
        .as_ref()
        .ok_or("inventory service unavailable")?
        .open(profile.uuid, registration.id())
        .await
    {
        Ok(inventory) => inventory,
        Err(error) => {
            tracing::error!(uuid=%profile.uuid,%error,"Player inventory load failed");
            login_disconnect(
                &mut writer,
                "Could not load player inventory; contact the server operator",
            )
            .await?;
            return Err(error.into());
        }
    };
    if let Some(threshold) = state.properties.compression_threshold {
        let mut compression = vec![3];
        put_varint(threshold as i32, &mut compression);
        writer.send(&compression).await?;
        reader.compression = Some(threshold);
        writer.compression = Some(threshold);
    }
    let mut success = vec![2];
    success.extend_from_slice(profile.uuid.as_bytes());
    put_string(&profile.name, &mut success);
    put_varint(profile.properties.len() as i32, &mut success);
    for property in &profile.properties {
        put_string(&property.name, &mut success);
        put_string(&property.value, &mut success);
        success.push(u8::from(property.signature.is_some()));
        if let Some(signature) = &property.signature {
            put_string(signature, &mut success);
        }
    }
    success.extend_from_slice(Uuid::new_v4().as_bytes());
    writer.send(&success).await?;
    if reader.packet().await?.as_slice() != [3] {
        return Err("expected login acknowledgement".into());
    }
    let mut client_profile = crate::client_policy::Negotiation::new(
        state.settings.allow_java_clients,
        state.settings.allow_horizon_clients,
        std::time::Instant::now(),
        deadline,
    )?;
    configuration_send(
        &mut writer,
        crate::client_policy::PROBE_PACKET,
        &client_profile,
    )
    .await?;
    configuration_send(&mut writer, &packets::known_packs(), &client_profile).await?;
    let mut known = false;
    for _ in 0..32 {
        let packet = configuration_packet(&mut reader, &client_profile).await?;
        let mut view = packet.as_slice();
        match get_varint(&mut view)? {
            7 => {
                let count = get_varint(&mut view)?;
                if !(0..=16).contains(&count) {
                    return Err("invalid known packs".into());
                }
                for _ in 0..count {
                    let namespace = get_string(&mut view, 32767)?;
                    let id = get_string(&mut view, 32767)?;
                    let version = get_string(&mut view, 32767)?;
                    known |= namespace == "minecraft" && id == "core" && version == "26.3";
                }
                if !view.is_empty() {
                    return Err("trailing known packs".into());
                }
                break;
            }
            0 => {}
            2 => {
                if let Err(error) =
                    client_profile.observe_custom_payload(view, std::time::Instant::now())
                {
                    configuration_send(
                        &mut writer,
                        &disconnect(2, error.disconnect_reason()),
                        &client_profile,
                    )
                    .await?;
                    return Ok(());
                }
            }
            _ => return Err("unexpected configuration packet".into()),
        }
    }
    if !known {
        configuration_send(
            &mut writer,
            &disconnect(2, "The client must provide Minecraft 26.3 built-in data"),
            &client_profile,
        )
        .await?;
        return Ok(());
    }
    match client_profile.finish(std::time::Instant::now()) {
        Ok(profile) => tracing::debug!(?profile, "Client capability negotiation complete"),
        Err(error) => {
            configuration_send(
                &mut writer,
                &disconnect(2, error.disconnect_reason()),
                &client_profile,
            )
            .await?;
            return Ok(());
        }
    }
    let data = state.data.as_ref().ok_or("game data unavailable")?;
    configuration_send(
        &mut writer,
        &packets::configuration_brand(&state.settings.server_brand),
        &client_profile,
    )
    .await?;
    for packet in data.configuration_packets() {
        configuration_send(&mut writer, packet, &client_profile).await?;
    }
    let mut finished = false;
    for _ in 0..32 {
        let packet = configuration_packet(&mut reader, &client_profile).await?;
        if packet.as_slice() == [3] {
            finished = true;
            break;
        }
        let mut view = packet.as_slice();
        match get_varint(&mut view)? {
            0 => {}
            2 => {
                if let Err(error) =
                    client_profile.observe_custom_payload(view, std::time::Instant::now())
                {
                    configuration_send(
                        &mut writer,
                        &disconnect(2, error.disconnect_reason()),
                        &client_profile,
                    )
                    .await?;
                    return Ok(());
                }
            }
            _ => return Err("expected finish configuration".into()),
        }
    }
    if !finished {
        return Err("configuration did not finish".into());
    }
    configuration_deadline(&client_profile)?;
    let session = crate::player_session::ConnectionSession::new(
        Arc::clone(&state),
        profile.uuid,
        profile.name.clone(),
        registration,
        inventory,
        slot,
    )?;
    let resources = session.resources();
    let result = play(
        (reader, writer),
        Arc::clone(&state),
        &profile,
        (&resources.registration, &resources.inventory),
        packets,
        disconnect_rx,
        &session,
    )
    .await;
    drop(resources);
    let cleanup = session.close().await;
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!("{error}; player cleanup: {cleanup}").into()),
    }
}

async fn configuration_packet(
    reader: &mut Reader,
    profile: &crate::client_policy::Negotiation,
) -> Result<InboundPacket> {
    let deadline = configuration_deadline(profile)?;
    let packet = tokio::time::timeout_at(deadline, reader.packet())
        .await
        .map_err(|_| "client capability negotiation timed out")??;
    configuration_deadline(profile)?;
    Ok(packet)
}

async fn configuration_send(
    writer: &mut Writer,
    packet: &[u8],
    profile: &crate::client_policy::Negotiation,
) -> Result<()> {
    let deadline = configuration_deadline(profile)?;
    tokio::time::timeout_at(deadline, writer.send(packet))
        .await
        .map_err(|_| "client capability negotiation timed out")??;
    configuration_deadline(profile)?;
    Ok(())
}

fn configuration_deadline(profile: &crate::client_policy::Negotiation) -> Result<Instant> {
    let deadline = Instant::from_std(profile.deadline());
    if Instant::now() >= deadline {
        return Err("client capability negotiation timed out".into());
    }
    Ok(deadline)
}

async fn status(reader: &mut Reader, writer: &mut Writer, state: &State) -> Result<()> {
    if reader.packet().await?.as_slice() != [0] {
        return Err("expected status request".into());
    }
    let status = serde_json::json!({"version":{"name":MINECRAFT_VERSION,"protocol":PROTOCOL_VERSION},"players":{"max":state.properties.max_players,"online":state.online()},"description":{"text":state.properties.motd},"enforcesSecureChat":false});
    let mut packet = vec![0];
    put_string(&status.to_string(), &mut packet);
    writer.send(&packet).await?;
    let ping = reader.packet().await?;
    if ping.len() != 9 || ping[0] != 1 {
        return Err("invalid status ping".into());
    }
    writer.send(&ping).await
}
async fn login_disconnect(writer: &mut Writer, message: &str) -> Result<()> {
    let mut packet = vec![0];
    put_string(
        &serde_json::json!({"text":message}).to_string(),
        &mut packet,
    );
    writer.send(&packet).await
}
fn disconnect(id: u8, message: &str) -> Vec<u8> {
    let mut packet = packets::system_chat(message);
    packet[0] = id;
    packet.pop();
    packet
}
fn bytes(value: &[u8], out: &mut Vec<u8>) {
    put_varint(value.len() as i32, out);
    out.extend_from_slice(value);
}
fn byte_array<'a>(view: &mut &'a [u8], max: usize) -> Result<&'a [u8]> {
    let length = get_varint(view)?;
    if length < 0 || length as usize > max {
        return Err("byte array exceeds limit".into());
    }
    Ok(take(view, length as usize)?)
}
fn offline_uuid(name: &str) -> Uuid {
    let mut bytes: [u8; 16] = Md5::digest(format!("OfflinePlayer:{name}").as_bytes()).into();
    bytes[6] = (bytes[6] & 0x0f) | 0x30;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

struct PlayState {
    radius: i32,
    loaded: HashSet<(i32, i32)>,
    center: (i32, i32),
    position: (f64, f64, f64),
    rotation: (f32, f32),
    desired: HashSet<(i32, i32)>,
    stream: Option<AbortTask<Result<()>>>,
    stream_epoch: u64,
    streaming: bool,
    chunk_count: i32,
    last_keepalive: Instant,
    outstanding: Option<(i64, Instant)>,
    nonce: i64,
    bar: Uuid,
    show_bar: bool,
    teleport_confirmed: bool,
    teleport_id: i32,
    last_ack_sequence: u64,
    boundary_notice: bool,
    rate_start: Instant,
    packet_count: u32,
    inventory_count: u32,
    flying: bool,
}

struct ReceivedPacket {
    sequence: u64,
    packet: Result<InboundPacket>,
    counted: bool,
}

struct PendingNotice {
    next: crate::updates::NotificationState,
    completion: std::pin::Pin<
        Box<dyn std::future::Future<Output = crate::player_control::ControlOutcome> + Send>,
    >,
}

struct PendingChunk {
    epoch: u64,
    x: i32,
    z: i32,
    snapshot: crate::runtime::ChunkSnapshotFence,
    completion: std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<crate::runtime::PreparedChunk>> + Send>,
    >,
}

impl PendingChunk {
    fn new(
        world: &Arc<World>,
        x: i32,
        z: i32,
        epoch: u64,
        deadline: Instant,
        stop: crate::lifecycle::StopToken,
    ) -> Self {
        let world = Arc::clone(world);
        let snapshot = crate::runtime::ChunkSnapshotFence::default();
        let worker_snapshot = snapshot.clone();
        Self {
            epoch,
            x,
            z,
            snapshot,
            completion: Box::pin(async move {
                world
                    .prepare_packet_with_fence(x, z, &stop, deadline, worker_snapshot)
                    .await
            }),
        }
    }
}

async fn send_outbound_batch(
    writer: &mut Writer,
    inventory: &InventoryHandle,
    batch: PacketBatch,
) -> Result<()> {
    if inventory.allow_outbound(&batch) {
        for packet in batch {
            writer.send(&packet).await?;
        }
    }
    Ok(())
}

async fn hold_or_send_outbound(
    writer: &mut Writer,
    inventory: &InventoryHandle,
    batch: PacketBatch,
    snapshot: Option<&crate::runtime::ChunkSnapshotFence>,
    held: &mut Option<PacketBatch>,
) -> Result<()> {
    if snapshot.is_some_and(crate::runtime::ChunkSnapshotFence::started) {
        if held.is_some() {
            return Err("chunk publication already holds an outbound batch".into());
        }
        *held = Some(batch);
        Ok(())
    } else {
        send_outbound_batch(writer, inventory, batch).await
    }
}

async fn publish_prepared_chunk(
    writer: &mut Writer,
    inventory: &InventoryHandle,
    play: &mut PlayState,
    preparing: PendingChunk,
    result: Result<crate::runtime::PreparedChunk>,
    held: &mut Option<PacketBatch>,
) -> Result<()> {
    match result {
        Ok(packet) => {
            if preparing.epoch == play.stream_epoch
                && play.desired.contains(&(preparing.x, preparing.z))
            {
                writer.send(packet.bytes()).await?;
                play.loaded.insert((preparing.x, preparing.z));
                play.chunk_count += 1;
            }
        }
        Err(error) if World::missing_imported_chunk(error.as_ref()) => {}
        Err(error) => return Err(error),
    }
    if let Some(batch) = held.take() {
        send_outbound_batch(writer, inventory, batch).await?;
    }
    Ok(())
}

async fn play(
    (mut reader, mut writer): (Reader, Writer),
    state: Arc<State>,
    profile: &VerifiedProfile,
    (registration, inventory): (&PlayerHandle, &InventoryHandle),
    mut outbound: mpsc::Receiver<PacketBatch>,
    mut disconnected: watch::Receiver<bool>,
    session: &crate::player_session::ConnectionSession,
) -> Result<()> {
    let (incoming_tx, mut incoming) = mpsc::channel(32);
    let _reader_task = AbortTask(tokio::spawn(async move {
        let mut sequence = 0_u64;
        while let Some(next) = sequence.checked_add(1) {
            sequence = next;
            let packet = reader.packet().await;
            let failed = packet.is_err();
            if incoming_tx
                .send(ReceivedPacket {
                    sequence,
                    packet,
                    counted: false,
                })
                .await
                .is_err()
                || failed
            {
                break;
            }
        }
    }));
    let data = state.data.as_ref().ok_or("game data unavailable")?;
    let world = state.world.as_ref().ok_or("world unavailable")?;
    let spawn = world.spawn();
    let initial = Position {
        xyz: [
            f64::from(spawn.position[0]) + 0.5,
            f64::from(spawn.position[1]),
            f64::from(spawn.position[2]) + 0.5,
        ],
        yaw: spawn.yaw,
        pitch: spawn.pitch,
        on_ground: true,
    };
    let (game_mode, previous_game_mode) = inventory.modes()?;
    registration.initialize_game_mode(game_mode)?;
    writer
        .send(&packets::login(
            registration.id(),
            state.properties.max_players as i32,
            state.properties.view_distance as i32,
            data.overworld_dimension,
            game_mode,
            previous_game_mode,
            state.properties.online_mode,
        ))
        .await?;
    writer.send(&packets::abilities(game_mode, false)).await?;
    writer
        .send(&crate::entity_packets::health(20.0, 20, 5.0))
        .await?;
    for packet in inventory.full_sync().await? {
        writer.send(&packet).await?;
    }
    {
        let _gameplay = state.gameplay.read().await;
        registration.move_to(initial)?;
        registration.activate()?;
    }
    if let Some(mobs) = &state.mobs {
        mobs.synchronize().await?;
    }
    let mut commands = vec![
        "help".to_string(),
        "version".into(),
        "tps".into(),
        "tpsbar".into(),
        "plugins".into(),
        "list".into(),
        "setblock".into(),
        "give".into(),
        "summon".into(),
        "spark".into(),
        "stop".into(),
    ];
    if let Ok(registered) = state.commands.lock() {
        commands.extend(registered.keys().cloned());
    }
    writer
        .send(&packets::command_tree(
            &commands.iter().map(String::as_str).collect::<Vec<_>>(),
        ))
        .await?;
    if world.existing_only() {
        writer
            .send(&packets::world_spawn(
                &spawn.dimension,
                spawn.position,
                [spawn.yaw, spawn.pitch],
            ))
            .await?;
        writer
            .send(&packets::position_rotated(
                1,
                initial.xyz,
                [initial.yaw, initial.pitch],
            ))
            .await?;
    } else {
        writer.send(&packets::default_spawn(0, 64, 0)).await?;
        writer.send(&packets::position(1, 0.5, 64.0, 0.5)).await?;
    }
    writer.send(&packets::game_event(13, 0.0)).await?;
    let radius = i32::from(state.properties.view_distance);
    let loaded = HashSet::new();
    let center = world_view::center(initial.xyz[0], initial.xyz[2]);
    let position = (initial.xyz[0], initial.xyz[1], initial.xyz[2]);
    let rotation = (initial.yaw, initial.pitch);
    let desired = world_view::desired(center, radius);
    let (chunk_sender, mut chunk_events) = mpsc::channel(4);
    let stream: Option<AbortTask<Result<()>>> = None;
    let streaming = false;
    let chunk_count = 0;
    let mut maintenance = tokio::time::interval(Duration::from_secs(1));
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let last_keepalive = Instant::now();
    let outstanding: Option<(i64, Instant)> = None;
    let nonce = 0i64;
    let bar = Uuid::new_v4();
    let show_bar = false;
    let teleport_confirmed = false;
    let teleport_id = 1i32;
    let last_ack_sequence = 0_u64;
    let mut command: Option<crate::player_session::CommandReceipt> = None;
    let mut join = Some(Box::pin(session.join()));
    let mut deferred = VecDeque::<ReceivedPacket>::new();
    let mut deferred_commands = 0_usize;
    let boundary_notice = false;
    let rate_start = Instant::now();
    let packet_count = 0u32;
    let inventory_count = 0u32;
    let mut shutdown = state.shutdown.subscribe();
    let updates = state.updates.clone();
    let mut update_notifications = crate::updates::NotificationState::default();
    let mut pending_notice: Option<PendingNotice> = None;
    let mut pending_chunk: Option<PendingChunk> = None;
    let mut held_outbound: Option<PacketBatch> = None;
    let mut play = PlayState {
        radius,
        loaded,
        center,
        position,
        rotation,
        desired,
        stream,
        stream_epoch: 0,
        streaming,
        chunk_count,
        last_keepalive,
        outstanding,
        nonce,
        bar,
        show_bar,
        teleport_confirmed,
        teleport_id,
        last_ack_sequence,
        boundary_notice,
        rate_start,
        packet_count,
        inventory_count,
        flying: false,
    };
    let control_key = crate::player_control::SessionKey {
        uuid: profile.uuid,
        session_id: registration.id(),
    };
    let (control_sender, mut control_receiver, control_transport) = crate::player_control::channel(
        control_key,
        state.player_control_budget.clone(),
        state.lifecycle.clone(),
        16,
    )?;
    writer._control_transport = Some(control_transport);
    let _control_binding = session.bind_control(control_sender)?;
    loop {
        let command_running = command.is_some();
        let joining = join.is_some();
        let ordered_work_pending = command_running || joining;
        let notice_pending = pending_notice.is_some();
        let chunk_pending = pending_chunk.is_some();
        let hold_publication = pending_chunk
            .as_ref()
            .is_some_and(|chunk| chunk.snapshot.started());
        let (received, from_deferred) = tokio::select! {
            _=shutdown.changed()=>{writer.send(&disconnect(32,"Server stopping")).await?;return Ok(());},
            _=disconnected.changed()=>return Err("outbound queue exceeded".into()),
            outcome=async { pending_notice.as_mut().expect("notice branch is enabled").completion.as_mut().await }, if notice_pending => {
                let notice = pending_notice.take().expect("notice branch is enabled");
                match outcome {
                    crate::player_control::ControlOutcome::Written { .. } => update_notifications = notice.next,
                    crate::player_control::ControlOutcome::Rejected { .. } => {},
                    crate::player_control::ControlOutcome::ConnectionClosed { .. } => return Err("player control transport closed".into()),
                }
                continue;
            },
            request=control_receiver.recv()=>{
                let request = request.ok_or("player control receiver stopped")?;
                writer.send_control(request, control_key).await?;
                continue;
            },
            result=async { join.as_mut().expect("join branch is enabled").await }, if joining => {
                result?;
                join = None;
                tracing::info!(player=%profile.name,"Player joined");
                play.stream_epoch = play.stream_epoch.checked_add(1).ok_or("chunk stream epoch exhausted")?;
                play.stream = Some(stream_chunks(chunk_sender.clone(), play.stream_epoch, play.center, &play.desired, &play.loaded));
                play.streaming = true;
                continue;
            },
            result=async { command.as_mut().expect("command branch is enabled").wait().await }, if command_running => {
                let replies = result?;
                command = None;
                match replies {
                    Ok(replies) => for reply in replies { writer.send(&packets::system_chat(&reply)).await?; },
                    Err(error) => writer.send(&packets::system_chat(&format!("Command failed: {error}"))).await?,
                }
                continue;
            },
            result=async { pending_chunk.as_mut().expect("chunk branch is enabled").completion.as_mut().await }, if chunk_pending => {
                let preparing = pending_chunk.take().expect("chunk branch is enabled");
                publish_prepared_chunk(&mut writer, inventory, &mut play, preparing, result, &mut held_outbound).await?;
                continue;
            },
            batch=outbound.recv(), if held_outbound.is_none() && !hold_publication=>{
                let Some(batch)=batch else{return Ok(());};
                hold_or_send_outbound(&mut writer, inventory, batch, pending_chunk.as_ref().map(|chunk| &chunk.snapshot), &mut held_outbound).await?;
                continue;
            },
            event=chunk_events.recv(), if !chunk_pending=>{
                let event = event.ok_or("chunk stream stopped")?;
                if event.epoch != play.stream_epoch { continue; }
                if let ChunkEvent::Chunk { x, z } = event.event {
                    pending_chunk = Some(PendingChunk::new(world, x, z, event.epoch, Instant::now()+writer.deadline, state.lifecycle.token()));
                } else if write_chunk_event(event.event,&mut writer,world,&mut play.loaded,&mut play.chunk_count).await?{play.streaming=false;}
                continue;
            },
            _=maintenance.tick()=>{
                if !notice_pending {
                    let mut next = update_notifications.clone();
                    let notice = updates.as_ref().and_then(|updates| {
                        let update = updates.borrow();
                        next.next(state.is_operator(profile.uuid), update.as_ref())
                    });
                    if let Some(message) = notice
                        && let Some(control) = state.sessions.control(control_key)
                    {
                        match control.try_system_message(&message, Instant::now() + writer.deadline, &state.lifecycle.token()) {
                            Ok(receipt) => pending_notice = Some(PendingNotice { next, completion: Box::pin(receipt.wait()) }),
                            Err(crate::player_control::ControlError::Busy) => {},
                            Err(error) => return Err(error.into()),
                        }
                    }
                }
                if play.outstanding.is_some_and(|(_,sent)|sent.elapsed()>Duration::from_secs(30)){return Err("keepalive timeout".into());}
                if play.outstanding.is_none()&&play.last_keepalive.elapsed()>=Duration::from_secs(10){play.nonce=play.nonce.wrapping_add(1);let mut packet=vec![45];packet.extend_from_slice(&play.nonce.to_be_bytes());writer.send(&packet).await?;play.outstanding=Some((play.nonce,Instant::now()));play.last_keepalive=Instant::now();}
                if play.show_bar{let (title,progress)=tpsbar_metrics(&state);for packet in packets::bossbar_update(play.bar,&title,progress){writer.send(&packet).await?;}}
                if play.stream.as_ref().is_some_and(|task|task.0.is_finished())&&let Some(mut task)=play.stream.take(){(&mut task.0).await??;}
                if !joining&&!play.streaming&&play.stream.is_none(){
                    let next=world_view::center(play.position.0,play.position.2);
                    if next!=play.center{play.center=next;play.desired=world_view::desired(play.center,play.radius);play.streaming=true;play.stream_epoch=play.stream_epoch.checked_add(1).ok_or("chunk stream epoch exhausted")?;play.stream=Some(stream_chunks(chunk_sender.clone(),play.stream_epoch,play.center,&play.desired,&play.loaded));}
                }
                continue;
            },
            packet=incoming.recv()=>(packet.ok_or("connection reader stopped")?, false),
            _=std::future::ready(()), if !ordered_work_pending && !deferred.is_empty() => (deferred.pop_front().expect("deferred branch is enabled"), true),
        };
        let ReceivedPacket {
            sequence,
            packet,
            counted,
        } = received;
        let packet = packet?;
        if !counted {
            if play.rate_start.elapsed() >= Duration::from_secs(1) {
                play.rate_start = Instant::now();
                play.packet_count = 0;
                play.inventory_count = 0;
            }
            play.packet_count += 1;
            if play.packet_count > 500 {
                return Err("packet rate limit exceeded".into());
            }
        }
        let mut view = packet.as_slice();
        let id = get_varint(&mut view)?;
        let is_command = matches!(id, 7 | 8);
        if from_deferred && is_command {
            deferred_commands -= 1;
        }
        if !from_deferred
            && (ordered_work_pending || !deferred.is_empty())
            && matches!(
                id,
                1 | 7 | 8 | 18 | 19 | 30 | 31 | 32 | 33 | 40 | 41 | 54 | 57 | 66
            )
        {
            if is_command {
                zerqon_protocol::decode_command(view, id == 8)?;
            }
            if deferred.len() >= 64 || is_command && deferred_commands >= 4 {
                return Err("deferred gameplay queue exceeded".into());
            }
            deferred_commands += usize::from(is_command);
            deferred.push_back(ReceivedPacket {
                sequence,
                packet: Ok(packet),
                counted: true,
            });
            continue;
        }
        let can_interact = play.teleport_confirmed && sequence > play.last_ack_sequence;
        match id {
            40 => {
                let requested = zerqon_protocol::decode_player_abilities(view)?;
                let (mode, _) = inventory.modes()?;
                if can_interact {
                    play.flying = requested && mode == crate::game_mode::GameMode::Creative;
                }
                writer.send(&packets::abilities(mode, play.flying)).await?;
            }
            18 | 19 | 54 | 57 => {
                play.inventory_count += 1;
                if play.inventory_count > 64 {
                    if play.inventory_count == 65 {
                        inventory_response(
                            Err(crate::player_inventory::InventoryServiceError::RateLimited),
                            inventory,
                            &mut writer,
                        )
                        .await?;
                    }
                    continue;
                }
                let policy = zerqon_inventory::InventoryPolicy {
                    creative: true,
                    can_interact,
                    allow_drops: false,
                };
                let body_offset = packet.as_slice().len() - view.len();
                inventory_response(
                    inventory
                        .apply_inbound_packet(id, packet, body_offset, policy)
                        .await,
                    inventory,
                    &mut writer,
                )
                .await?;
            }
            66 => {
                let request = zerqon_inventory::decode_use_on(view)?;
                let sequence = request.sequence;
                play.inventory_count += 1;
                let result = if play.inventory_count > 64 {
                    Err("Block interactions are rate limited".into())
                } else if !can_interact {
                    Err("Wait for teleport confirmation".into())
                } else if !play.loaded.contains(&(
                    request.block.x.div_euclid(16),
                    request.block.z.div_euclid(16),
                )) {
                    Err("Block is outside the loaded terrain".into())
                } else {
                    state
                        .edit_block(
                            profile.uuid,
                            registration.id(),
                            inventory,
                            BlockInteraction::Place(request),
                        )
                        .await
                };
                block_response(&state, profile.uuid, registration.id(), sequence, result)?;
            }
            1 => {
                let target = get_varint(&mut view)?;
                if target <= 0 || !view.is_empty() {
                    return Err("invalid attack packet".into());
                }
                if can_interact && let Some(mobs) = &state.mobs {
                    mobs.attack(profile.uuid, registration.id(), target).await?;
                }
            }
            0 => {
                let acknowledgement = zerqon_protocol::decode_teleport_ack(view)?;
                if acknowledgement.id > play.teleport_id {
                    return Err("invalid teleport acknowledgement".into());
                }
                if acknowledgement.id == play.teleport_id && !play.teleport_confirmed {
                    play.teleport_confirmed = true;
                    play.last_ack_sequence = sequence;
                }
            }
            28 => {
                let received = zerqon_protocol::decode_keepalive(view)?;
                if !play
                    .outstanding
                    .is_some_and(|(expected, _)| expected == received)
                {
                    return Err("invalid keepalive response".into());
                }
                if let Some((_, sent)) = play.outstanding {
                    registration.latency(sent.elapsed().as_millis().min(i32::MAX as u128) as i32)?;
                }
                play.outstanding = None;
            }
            7 | 8 => {
                let input = zerqon_protocol::decode_command(view, id == 8)?;
                if input.trim() == "tpsbar" {
                    play.show_bar = !play.show_bar;
                    if play.show_bar {
                        let (title, progress) = tpsbar_metrics(&state);
                        writer
                            .send(&packets::bossbar_add(play.bar, &title, progress))
                            .await?;
                    } else {
                        writer.send(&packets::bossbar_remove(play.bar)).await?;
                    }
                } else {
                    command = Some(session.start_command(packet)?);
                }
            }
            30 | 31 => {
                let x = f64::from_be_bytes(take(&mut view, 8)?.try_into()?);
                let y = f64::from_be_bytes(take(&mut view, 8)?.try_into()?);
                let z = f64::from_be_bytes(take(&mut view, 8)?.try_into()?);
                let next_rotation = if id == 31 {
                    (
                        f32::from_be_bytes(take(&mut view, 4)?.try_into()?),
                        f32::from_be_bytes(take(&mut view, 4)?.try_into()?),
                    )
                } else {
                    play.rotation
                };
                let flags = take(&mut view, 1)?[0];
                let next = Position {
                    xyz: [x, y, z],
                    yaw: next_rotation.0,
                    pitch: next_rotation.1,
                    on_ground: flags & 1 != 0,
                };
                if !view.is_empty() || !world_view::valid_position(next) {
                    return Err("invalid movement packet".into());
                }
                if can_interact {
                    if world.existing_only()
                        && !world_view::can_move(
                            [play.position.0, play.position.1, play.position.2],
                            next.xyz,
                            &play.loaded,
                            &play.desired,
                        )
                    {
                        play.teleport_id = play
                            .teleport_id
                            .checked_add(1)
                            .ok_or("teleport identifier exhausted")?;
                        play.teleport_confirmed = false;
                        writer
                            .send(&packets::position_rotated(
                                play.teleport_id,
                                [play.position.0, play.position.1, play.position.2],
                                [play.rotation.0, play.rotation.1],
                            ))
                            .await?;
                        if !play.boundary_notice {
                            writer
                                .send(&packets::system_chat(
                                    "This area is outside the available imported terrain.",
                                ))
                                .await?;
                            play.boundary_notice = true;
                        }
                    } else {
                        let _gameplay = state.gameplay.read().await;
                        registration.move_to(next)?;
                        play.position = (x, y, z);
                        play.rotation = next_rotation;
                    }
                }
            }
            32 | 33 => {
                let next_rotation = if id == 32 {
                    (
                        f32::from_be_bytes(take(&mut view, 4)?.try_into()?),
                        f32::from_be_bytes(take(&mut view, 4)?.try_into()?),
                    )
                } else {
                    play.rotation
                };
                let flags = take(&mut view, 1)?[0];
                let next = Position {
                    xyz: [play.position.0, play.position.1, play.position.2],
                    yaw: next_rotation.0,
                    pitch: next_rotation.1,
                    on_ground: flags & 1 != 0,
                };
                if !view.is_empty() || !world_view::valid_position(next) {
                    return Err("invalid movement packet".into());
                }
                if can_interact {
                    let _gameplay = state.gameplay.read().await;
                    registration.move_to(next)?;
                    play.rotation = next_rotation;
                }
            }
            41 => {
                use zerqon_inventory::PlayerAction;
                let request = zerqon_inventory::decode_player_action(view)?;
                let action = request.action;
                if matches!(
                    action,
                    PlayerAction::DropAllItems
                        | PlayerAction::DropItem
                        | PlayerAction::SwapItemWithOffhand
                ) {
                    play.inventory_count += 1;
                    if play.inventory_count <= 64 {
                        let result = if action == PlayerAction::SwapItemWithOffhand {
                            inventory
                                .swap_offhand(zerqon_inventory::InventoryPolicy {
                                    creative: true,
                                    can_interact,
                                    allow_drops: false,
                                })
                                .await
                        } else {
                            Err(zerqon_inventory::InventoryError::DroppingUnavailable.into())
                        };
                        inventory_response(result, inventory, &mut writer).await?;
                    } else if play.inventory_count == 65 {
                        inventory_response(
                            Err(crate::player_inventory::InventoryServiceError::RateLimited),
                            inventory,
                            &mut writer,
                        )
                        .await?;
                    }
                } else if matches!(
                    action,
                    PlayerAction::StartDestroyBlock
                        | PlayerAction::ChangeDestroyDirection
                        | PlayerAction::AbortDestroyBlock
                        | PlayerAction::StopDestroyBlock
                ) {
                    let sequence = request.sequence;
                    play.inventory_count += 1;
                    let result = if play.inventory_count > 64 {
                        Err("Block interactions are rate limited".into())
                    } else if !can_interact {
                        Err("Wait for teleport confirmation".into())
                    } else if !play.loaded.contains(&(
                        request.block.x.div_euclid(16),
                        request.block.z.div_euclid(16),
                    )) {
                        Err("Block is outside the loaded terrain".into())
                    } else if action == PlayerAction::StartDestroyBlock
                        || (action == PlayerAction::StopDestroyBlock
                            && inventory.modes()?.0 != crate::game_mode::GameMode::Creative)
                    {
                        state
                            .edit_block(
                                profile.uuid,
                                registration.id(),
                                inventory,
                                BlockInteraction::Break(request),
                            )
                            .await
                    } else {
                        Ok(())
                    };
                    block_response(&state, profile.uuid, registration.id(), sequence, result)?;
                }
            }
            _ => {}
        }
    }
}

fn block_response(
    state: &State,
    uuid: Uuid,
    id: i32,
    sequence: u32,
    result: Result<()>,
) -> Result<()> {
    if *state.shutdown.borrow() {
        return Err("server is stopping; block operation outcome is not being acknowledged".into());
    }
    if let Some(world) = &state.world {
        world.check_available()?;
    }
    let mut batch: Vec<bytes::Bytes> = Vec::new();
    if let Err(error) = result {
        batch.push(packets::system_chat(&format!("Block interaction rejected: {error}")).into());
    }
    let mut acknowledgement = vec![4];
    put_varint(sequence as i32, &mut acknowledgement);
    batch.push(acknowledgement.into());
    state.players.dispatch_batches(vec![(uuid, id, batch)])?;
    Ok(())
}

fn tpsbar_metrics(state: &State) -> (String, f32) {
    state.stats.lock().map_or_else(
        |_| ("Tick metrics unavailable".into(), 0.0),
        |stats| stats.bar(),
    )
}

async fn inventory_response(
    result: std::result::Result<PacketBatch, crate::player_inventory::InventoryServiceError>,
    inventory: &InventoryHandle,
    writer: &mut Writer,
) -> Result<()> {
    match result {
        Ok(packets) => {
            for packet in packets {
                writer.send(&packet).await?;
            }
        }
        Err(error) => {
            if crate::managed_tasks::is_cleanup_failure(&error) {
                return Err(error.into());
            }
            tracing::debug!(%error,"Inventory action rejected");
            for packet in inventory.full_sync().await? {
                writer.send(&packet).await?;
            }
            writer
                .send(&packets::system_chat(&format!(
                    "Inventory action rejected: {error}"
                )))
                .await?;
        }
    }
    Ok(())
}

enum ChunkEvent {
    Unload(i32, i32),
    Begin(i32, i32),
    Chunk { x: i32, z: i32 },
    Finished,
}

struct StreamEvent {
    epoch: u64,
    event: ChunkEvent,
}

async fn write_chunk_event(
    event: ChunkEvent,
    writer: &mut Writer,
    world: &Arc<World>,
    loaded: &mut HashSet<(i32, i32)>,
    count: &mut i32,
) -> Result<bool> {
    match event {
        ChunkEvent::Unload(x, z) => {
            let mut packet = vec![38];
            packet.extend_from_slice(&z.to_be_bytes());
            packet.extend_from_slice(&x.to_be_bytes());
            writer.send(&packet).await?;
            loaded.remove(&(x, z));
        }
        ChunkEvent::Begin(x, z) => {
            *count = 0;
            writer.send(&packets::chunk_center(x, z)).await?;
            writer.send(&packets::chunk_batch_start()).await?;
        }
        ChunkEvent::Chunk { x, z } => {
            let packet = match world.packet(x, z).await {
                Ok(packet) => packet,
                Err(error) if World::missing_imported_chunk(error.as_ref()) => return Ok(false),
                Err(error) => return Err(error),
            };
            writer.send(&packet).await?;
            loaded.insert((x, z));
            *count += 1;
        }
        ChunkEvent::Finished => {
            writer.send(&packets::chunk_batch_finished(*count)).await?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn stream_chunks(
    sender: mpsc::Sender<StreamEvent>,
    epoch: u64,
    center: (i32, i32),
    desired: &HashSet<(i32, i32)>,
    loaded: &HashSet<(i32, i32)>,
) -> AbortTask<Result<()>> {
    let removed: Vec<_> = loaded.difference(desired).copied().collect();
    let mut added: Vec<_> = desired.difference(loaded).copied().collect();
    added.sort_by_key(|&(x, z)| (x - center.0).abs() + (z - center.1).abs());
    AbortTask(tokio::spawn(async move {
        for (x, z) in removed {
            sender
                .send(StreamEvent {
                    epoch,
                    event: ChunkEvent::Unload(x, z),
                })
                .await?;
        }
        sender
            .send(StreamEvent {
                epoch,
                event: ChunkEvent::Begin(center.0, center.1),
            })
            .await?;
        for (x, z) in added {
            sender
                .send(StreamEvent {
                    epoch,
                    event: ChunkEvent::Chunk { x, z },
                })
                .await?;
        }
        sender
            .send(StreamEvent {
                epoch,
                event: ChunkEvent::Finished,
            })
            .await?;
        Ok(())
    }))
}

#[cfg(test)]
mod inbound_budget_tests {
    use super::*;
    use zerqon_protocol::inbound::PacketBudget;

    #[tokio::test]
    async fn reader_keeps_packet_memory_until_the_handler_releases_it() {
        let global = PacketBudget::new(8).unwrap();
        let account = global.connection(4).unwrap();
        let (shutdown, _) = watch::channel(false);
        let mut reader = Reader {
            io: Box::new(&b"\x04abcd\x04efgh"[..]),
            budget: account.clone(),
            compression: None,
            limit: 1024,
            deadline: Duration::from_secs(1),
            shutdown: shutdown.subscribe(),
        };
        let packet = reader.packet().await.unwrap();
        assert_eq!(packet.as_slice(), b"abcd");
        assert_eq!(account.snapshot().used_bytes, 4);
        assert_eq!(global.snapshot().used_bytes, 4);
        assert!(reader.packet().await.is_err());
        drop(packet);
        assert_eq!(account.snapshot().used_bytes, 0);
        assert_eq!(global.snapshot().used_bytes, 0);
    }

    #[tokio::test]
    async fn shutdown_releases_an_incomplete_inbound_frame_reservation() {
        use tokio::io::AsyncWriteExt;
        let global = PacketBudget::new(4096).unwrap();
        let account = global.connection(2048).unwrap();
        let (shutdown, _) = watch::channel(false);
        let (input, mut peer) = tokio::io::duplex(32);
        let mut reader = Reader {
            io: Box::new(input),
            budget: account.clone(),
            compression: None,
            limit: 1024,
            deadline: Duration::from_secs(30),
            shutdown: shutdown.subscribe(),
        };
        peer.write_all(&[0x80, 8, 1]).await.unwrap();
        let pending = tokio::spawn(async move { reader.packet().await });
        timeout(Duration::from_secs(2), async {
            while account.snapshot().used_bytes == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reader never reserved its incomplete frame");
        shutdown.send_replace(true);
        assert!(
            timeout(Duration::from_secs(1), pending)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert_eq!(account.snapshot().used_bytes, 0);
        assert_eq!(global.snapshot().used_bytes, 0);
    }
}

#[cfg(test)]
mod configuration_deadline_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn profile(timeout: Duration) -> crate::client_policy::Negotiation {
        crate::client_policy::Negotiation::new(true, true, std::time::Instant::now(), timeout)
            .unwrap()
    }

    #[tokio::test]
    async fn expired_configuration_deadline_rejects_buffered_finish() {
        let (shutdown, _) = watch::channel(false);
        let (input, mut peer) = tokio::io::duplex(1024);
        let mut reader = Reader {
            io: Box::new(input),
            compression: None,
            limit: 1024,
            budget: zerqon_protocol::inbound::PacketBudget::new(4096)
                .unwrap()
                .connection(4096)
                .unwrap(),
            deadline: Duration::from_secs(30),
            shutdown: shutdown.subscribe(),
        };
        let start = std::time::Instant::now() - Duration::from_secs(2);
        let mut profile =
            crate::client_policy::Negotiation::new(true, true, start, Duration::from_secs(1))
                .unwrap();
        profile.finish(start).unwrap();
        write_frame(&mut peer, &[3], None).await.unwrap();
        assert!(configuration_packet(&mut reader, &profile).await.is_err());
    }

    #[tokio::test]
    async fn expired_configuration_send_writes_no_frame_bytes() {
        let (shutdown, _) = watch::channel(false);
        let (output, mut peer) = tokio::io::duplex(1024);
        let mut writer = Writer {
            io: Box::new(output),
            compression: None,
            deadline: Duration::from_secs(30),
            shutdown: shutdown.subscribe(),
            _control_transport: None,
        };
        let start = std::time::Instant::now() - Duration::from_secs(2);
        let profile =
            crate::client_policy::Negotiation::new(true, true, start, Duration::from_secs(1))
                .unwrap();
        assert!(
            configuration_send(&mut writer, &[3], &profile)
                .await
                .is_err()
        );
        drop(writer);
        let mut received = Vec::new();
        peer.read_to_end(&mut received).await.unwrap();
        assert!(received.is_empty());
    }

    #[tokio::test]
    async fn interrupted_configuration_write_does_not_append_a_disconnect_frame() {
        let (shutdown, _) = watch::channel(false);
        let (output, mut peer) = tokio::io::duplex(1);
        let mut writer = Writer {
            io: Box::new(output),
            compression: None,
            deadline: Duration::from_secs(30),
            shutdown: shutdown.subscribe(),
            _control_transport: None,
        };
        let profile = profile(Duration::from_millis(50));
        let pending =
            tokio::spawn(
                async move { configuration_send(&mut writer, &[13; 1024], &profile).await },
            );
        let first = peer.read_u8().await.unwrap();
        assert_eq!(first, 0x80);
        let result = timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
        let mut tail = Vec::new();
        timeout(Duration::from_secs(1), peer.read_to_end(&mut tail))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            tail,
            [8],
            "only the unfinished original frame prefix may remain"
        );
    }

    #[tokio::test]
    async fn configuration_frames_share_one_outbound_deadline() {
        let (shutdown, _) = watch::channel(false);
        let (output, mut peer) = tokio::io::duplex(1);
        let mut writer = Writer {
            io: Box::new(output),
            compression: None,
            deadline: Duration::from_secs(30),
            shutdown: shutdown.subscribe(),
            _control_transport: None,
        };
        let profile = profile(Duration::from_millis(90));
        let pending = tokio::spawn(async move {
            let mut completed = 0;
            for packet in [&[13, 1][..], &[7, 0], &[3]] {
                if configuration_send(&mut writer, packet, &profile)
                    .await
                    .is_err()
                {
                    break;
                }
                completed += 1;
            }
            completed
        });
        let drain = tokio::spawn(async move {
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_millis(40)).await;
                if read_frame(&mut peer, 1024, None).await.is_err() {
                    break;
                }
            }
        });
        assert!(
            timeout(Duration::from_secs(2), pending)
                .await
                .unwrap()
                .unwrap()
                < 3
        );
        timeout(Duration::from_secs(1), drain)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn configuration_cancellation_interrupts_a_partial_inbound_frame() {
        let (shutdown, _) = watch::channel(false);
        let (input, mut peer) = tokio::io::duplex(1024);
        let mut reader = Reader {
            io: Box::new(input),
            compression: None,
            limit: 1024,
            budget: zerqon_protocol::inbound::PacketBudget::new(4096)
                .unwrap()
                .connection(4096)
                .unwrap(),
            deadline: Duration::from_secs(30),
            shutdown: shutdown.subscribe(),
        };
        peer.write_all(&[128, 8, 2]).await.unwrap();
        let profile = profile(Duration::from_secs(30));
        let pending =
            tokio::spawn(async move { configuration_packet(&mut reader, &profile).await });
        tokio::task::yield_now().await;
        shutdown.send_replace(true);
        assert!(
            timeout(Duration::from_secs(1), pending)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }

    #[tokio::test]
    async fn configuration_rejects_a_ready_read_that_finishes_after_the_deadline() {
        struct SlowReady {
            delayed: bool,
            bytes: &'static [u8],
        }
        impl AsyncRead for SlowReady {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                buffer: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                if !self.delayed {
                    self.delayed = true;
                    std::thread::sleep(Duration::from_millis(100));
                }
                let count = self.bytes.len().min(buffer.remaining());
                buffer.put_slice(&self.bytes[..count]);
                self.bytes = &self.bytes[count..];
                std::task::Poll::Ready(Ok(()))
            }
        }
        let (shutdown, _) = watch::channel(false);
        let mut reader = Reader {
            io: Box::new(SlowReady {
                delayed: false,
                bytes: &[1, 3],
            }),
            compression: None,
            limit: 1024,
            budget: zerqon_protocol::inbound::PacketBudget::new(4096)
                .unwrap()
                .connection(4096)
                .unwrap(),
            deadline: Duration::from_secs(30),
            shutdown: shutdown.subscribe(),
        };
        assert!(
            configuration_packet(&mut reader, &profile(Duration::from_millis(50)))
                .await
                .is_err()
        );
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use crate::runtime::block_update;

    #[tokio::test]
    async fn reader_created_after_shutdown_does_not_wait_for_network_timeout() {
        let (shutdown, _) = watch::channel(false);
        shutdown.send_replace(true);
        let (stream, _peer) = tokio::io::duplex(1);
        let mut reader = Reader {
            io: Box::new(stream),
            compression: None,
            limit: 1024,
            budget: zerqon_protocol::inbound::PacketBudget::new(4096)
                .unwrap()
                .connection(4096)
                .unwrap(),
            deadline: Duration::from_secs(30),
            shutdown: shutdown.subscribe(),
        };
        let result = timeout(Duration::from_millis(100), reader.packet()).await;
        assert!(matches!(result, Ok(Err(_))));
    }

    #[tokio::test]
    async fn shutdown_interrupts_a_blocked_socket_write() {
        let (shutdown, receiver) = watch::channel(false);
        let (stream, _peer) = tokio::io::duplex(1);
        let mut writer = Writer {
            io: Box::new(stream),
            compression: None,
            deadline: Duration::from_secs(30),
            shutdown: receiver,
            _control_transport: None,
        };
        let task = tokio::spawn(async move { writer.send(&[1; 1024]).await });
        tokio::task::yield_now().await;
        shutdown.send_replace(true);
        let result = timeout(Duration::from_millis(100), task).await;
        assert!(matches!(result, Ok(Ok(Err(_)))));
    }

    #[tokio::test]
    #[ignore = "requires prepared official 26.3 game data"]
    async fn queued_chunk_snapshot_cannot_overwrite_a_newer_delivered_block_change() {
        let directory = tempfile::tempdir().unwrap();
        crate::config::initialize(directory.path()).unwrap();
        let (shutdown, _) = watch::channel(false);
        let settings = crate::config::Settings {
            data_directory: std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(".cache/vanilla/26.3")
                .to_string_lossy()
                .into_owned(),
            spawn_mobs: false,
            java_plugins: false,
            ..Default::default()
        };
        let state = State::new(
            directory.path(),
            crate::config::Properties::parse("online-mode=false").unwrap(),
            settings,
            false,
            shutdown.clone(),
            None,
        )
        .await
        .unwrap();
        let world = state.world.as_ref().unwrap();
        let (sender, mut events) = mpsc::channel(4);
        let desired = HashSet::from([(0, 0)]);
        let mut loaded = HashSet::new();
        let _stream = stream_chunks(sender, 1, (0, 0), &desired, &loaded);
        assert!(matches!(
            events.recv().await.unwrap(),
            StreamEvent {
                epoch: 1,
                event: ChunkEvent::Begin(0, 0)
            }
        ));
        let queued = events.recv().await.unwrap();
        assert!(matches!(
            &queued,
            StreamEvent {
                epoch: 1,
                event: ChunkEvent::Chunk { x: 0, z: 0 }
            }
        ));
        let old = world.packet(0, 0).await.unwrap();
        let block = state.data.as_ref().unwrap().flat.bedrock;
        world.set_block(0, 70, 0, block).await.unwrap();
        let current = world.packet(0, 0).await.unwrap();
        assert_ne!(old, current);
        let (output, mut client) = tokio::io::duplex(2_097_151);
        let mut writer = Writer {
            io: Box::new(output),
            compression: None,
            deadline: Duration::from_secs(5),
            shutdown: shutdown.subscribe(),
            _control_transport: None,
        };
        let update = block_update(0, 70, 0, block);
        writer.send(&update).await.unwrap();
        let mut count = 0;
        write_chunk_event(queued.event, &mut writer, world, &mut loaded, &mut count)
            .await
            .unwrap();
        assert_eq!(
            read_frame(&mut client, 2_097_151, None).await.unwrap(),
            update
        );
        let delivered = read_frame(&mut client, 2_097_151, None).await.unwrap();
        assert!(
            delivered == current,
            "queued snapshot overwrote a newer delivered block change"
        );
        assert_eq!(loaded, desired);
        assert_eq!(count, 1);
    }
}
