use crate::{
    Result,
    auth::{self, CipherReader, CipherWriter, VerifiedProfile},
    packets,
    player_inventory::InventoryHandle,
    players::{Identity, PacketBatch, PlayerHandle, Position},
    runtime::{CommandSource, State, World, block_update},
    world_view,
};
use md5::{Digest, Md5};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{Instant, timeout},
};
use uuid::Uuid;
use zerqon_plugin_api::Event;
use zerqon_protocol::{
    MINECRAFT_VERSION, PROTOCOL_VERSION,
    codec::{get_string, get_varint, put_string, put_varint, read_frame, take, write_frame},
};

struct Reader {
    io: Box<dyn AsyncRead + Unpin + Send>,
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
}
impl Reader {
    async fn packet(&mut self) -> Result<Vec<u8>> {
        if *self.shutdown.borrow() {
            return Err("server stopping".into());
        }
        tokio::select! {result=timeout(self.deadline,read_frame(&mut self.io,self.limit,self.compression))=>Ok(result??),_=self.shutdown.changed()=>Err("server stopping".into())}
    }
}
impl Writer {
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
    let Ok(_slot) = Arc::clone(&state.slots).try_acquire_owned() else {
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
    if reader.packet().await? != [3] {
        return Err("expected login acknowledgement".into());
    }
    writer.send(&packets::known_packs()).await?;
    let mut known = false;
    for _ in 0..32 {
        let packet = reader.packet().await?;
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
            0 | 2 => {}
            _ => return Err("unexpected configuration packet".into()),
        }
    }
    if !known {
        writer
            .send(&disconnect(
                2,
                "The client must provide Minecraft 26.3 built-in data",
            ))
            .await?;
        return Ok(());
    }
    let data = state.data.as_ref().ok_or("game data unavailable")?;
    for packet in data.configuration_packets() {
        writer.send(packet).await?;
    }
    let mut finished = false;
    for _ in 0..32 {
        let packet = reader.packet().await?;
        if packet == [3] {
            finished = true;
            break;
        }
        if !matches!(packet.first(), Some(0 | 2)) {
            return Err("expected finish configuration".into());
        }
    }
    if !finished {
        return Err("configuration did not finish".into());
    }
    tracing::info!(player=%profile.name,"Player joined");
    state
        .event(Event::PlayerJoin {
            uuid: profile.uuid.to_string(),
            name: profile.name.clone(),
        })
        .await?;
    let result = play(
        (reader, writer),
        Arc::clone(&state),
        &profile,
        (&registration, &inventory),
        packets,
        disconnect_rx,
    )
    .await;
    state
        .event(Event::PlayerQuit {
            uuid: profile.uuid.to_string(),
            name: profile.name.clone(),
        })
        .await?;
    tracing::info!(player=%profile.name,"Player left");
    result
}

async fn status(reader: &mut Reader, writer: &mut Writer, state: &State) -> Result<()> {
    if reader.packet().await? != [0] {
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

async fn play(
    (mut reader, mut writer): (Reader, Writer),
    state: Arc<State>,
    profile: &VerifiedProfile,
    (registration, inventory): (&PlayerHandle, &InventoryHandle),
    mut outbound: mpsc::Receiver<PacketBatch>,
    mut disconnected: watch::Receiver<bool>,
) -> Result<()> {
    let (incoming_tx, mut incoming) = mpsc::channel(32);
    let _reader_task = AbortTask(tokio::spawn(async move {
        loop {
            let packet = reader.packet().await;
            let failed = packet.is_err();
            if incoming_tx.send(packet).await.is_err() || failed {
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
    writer
        .send(&packets::login(
            registration.id(),
            state.properties.max_players as i32,
            state.properties.view_distance as i32,
            data.overworld_dimension,
        ))
        .await?;
    writer.send(&packets::abilities()).await?;
    writer
        .send(&crate::entity_packets::health(20.0, 20, 5.0))
        .await?;
    for packet in inventory.full_sync().await? {
        writer.send(&packet).await?;
    }
    registration.move_to(initial)?;
    registration.activate()?;
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
    let mut loaded = HashSet::new();
    let mut center = world_view::center(initial.xyz[0], initial.xyz[2]);
    let mut position = (initial.xyz[0], initial.xyz[1], initial.xyz[2]);
    let mut rotation = (initial.yaw, initial.pitch);
    let mut desired = world_view::desired(center, radius);
    let (chunk_sender, mut chunk_events) = mpsc::channel(4);
    let mut stream = Some(stream_chunks(
        chunk_sender.clone(),
        center,
        &desired,
        &loaded,
    ));
    let mut streaming = true;
    let mut chunk_count = 0;
    let mut maintenance = tokio::time::interval(Duration::from_secs(1));
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_keepalive = Instant::now();
    let mut outstanding: Option<(i64, Instant)> = None;
    let mut nonce = 0i64;
    let bar = Uuid::new_v4();
    let mut show_bar = false;
    let mut teleport_confirmed = false;
    let mut teleport_id = 1i32;
    let mut boundary_notice = false;
    let mut rate_start = Instant::now();
    let mut packet_count = 0u32;
    let mut inventory_count = 0u32;
    let mut shutdown = state.shutdown.subscribe();
    let updates = state.updates.clone();
    let mut update_notifications = crate::updates::NotificationState::default();
    loop {
        tokio::select! {
            _=shutdown.changed()=>{writer.send(&disconnect(32,"Server stopping")).await?;return Ok(());},
            _=disconnected.changed()=>return Err("outbound queue exceeded".into()),
            batch=outbound.recv()=>{let Some(batch)=batch else{return Ok(());};if inventory.allow_outbound(&batch){for packet in batch{writer.send(&packet).await?;}}},
            event=chunk_events.recv()=>{
                if write_chunk_event(event.ok_or("chunk stream stopped")?,&mut writer,world,&mut loaded,&mut chunk_count).await?{streaming=false;}
            },
            _=maintenance.tick()=>{
                let notice = updates.as_ref().and_then(|updates| {
                    let update = updates.borrow();
                    update_notifications.next(state.is_operator(profile.uuid), update.as_ref())
                });
                if let Some(message) = notice { writer.send(&packets::system_chat(&message)).await?; }
                if outstanding.is_some_and(|(_,sent)|sent.elapsed()>Duration::from_secs(30)){return Err("keepalive timeout".into());}
                if outstanding.is_none()&&last_keepalive.elapsed()>=Duration::from_secs(10){nonce=nonce.wrapping_add(1);let mut packet=vec![45];packet.extend_from_slice(&nonce.to_be_bytes());writer.send(&packet).await?;outstanding=Some((nonce,Instant::now()));last_keepalive=Instant::now();}
                if show_bar{let (title,progress)=tpsbar_metrics(&state);for packet in packets::bossbar_update(bar,&title,progress){writer.send(&packet).await?;}}
                if stream.as_ref().is_some_and(|task|task.0.is_finished())&&let Some(mut task)=stream.take(){(&mut task.0).await??;}
                if !streaming&&stream.is_none(){
                    let next=world_view::center(position.0,position.2);
                    if next!=center{center=next;desired=world_view::desired(center,radius);streaming=true;stream=Some(stream_chunks(chunk_sender.clone(),center,&desired,&loaded));}
                }
            },
            packet=incoming.recv()=>{
                let packet=packet.ok_or("connection reader stopped")??;
                if rate_start.elapsed()>=Duration::from_secs(1){rate_start=Instant::now();packet_count=0;inventory_count=0;}
                packet_count+=1;if packet_count>500{return Err("packet rate limit exceeded".into());}
                let mut view=packet.as_slice();let id=get_varint(&mut view)?;
                match id{
                    18|54|57=>{
                        inventory_count+=1;if inventory_count>64{if inventory_count==65{inventory_response(Err(crate::player_inventory::InventoryServiceError::RateLimited),inventory,&mut writer).await?;}continue;}
                        let policy=zerqon_inventory::InventoryPolicy {creative:true,can_interact:teleport_confirmed,allow_drops:false};
                        inventory_response(inventory.apply_packet(id,view,policy).await,inventory,&mut writer).await?;
                    },
                    1=>{
                        let target=get_varint(&mut view)?;
                        if target<=0||!view.is_empty(){return Err("invalid attack packet".into());}
                        if teleport_confirmed&&let Some(mobs)=&state.mobs{mobs.attack(profile.uuid,registration.id(),target).await?;}
                    },
                    0=>{
                        let id=get_varint(&mut view)?;
                        if id<=0||id>teleport_id||!view.is_empty(){return Err("invalid teleport acknowledgement".into());}
                        if id==teleport_id{teleport_confirmed=true;}
                    },
                    28=>{
                        let received=i64::from_be_bytes(take(&mut view,8)?.try_into()?);
                        if !outstanding.is_some_and(|(expected,_)|expected==received){return Err("invalid keepalive response".into());}
                        if let Some((_,sent))=outstanding{registration.latency(sent.elapsed().as_millis().min(i32::MAX as u128) as i32)?;}
                        outstanding=None;
                    },
                    7|8=>{let command=get_string(&mut view,256)?;if command.trim()=="tpsbar"{show_bar = !show_bar;if show_bar{let (title,progress)=tpsbar_metrics(&state);writer.send(&packets::bossbar_add(bar,&title,progress)).await?;}else{writer.send(&packets::bossbar_remove(bar)).await?;}}else{match state.command(CommandSource::Player{uuid:profile.uuid,name:&profile.name},command).await{Ok(replies)=>for reply in replies{writer.send(&packets::system_chat(&reply)).await?;},Err(error)=>writer.send(&packets::system_chat(&format!("Command failed: {error}"))).await?,}}},
                    30|31=>{
                        let x=f64::from_be_bytes(take(&mut view,8)?.try_into()?);
                        let y=f64::from_be_bytes(take(&mut view,8)?.try_into()?);
                        let z=f64::from_be_bytes(take(&mut view,8)?.try_into()?);
                        let next_rotation=if id==31{(f32::from_be_bytes(take(&mut view,4)?.try_into()?),f32::from_be_bytes(take(&mut view,4)?.try_into()?))}else{rotation};
                        let flags=take(&mut view,1)?[0];
                        let next=Position{xyz:[x,y,z],yaw:next_rotation.0,pitch:next_rotation.1,on_ground:flags&1!=0};
                        if !view.is_empty()||!world_view::valid_position(next){return Err("invalid movement packet".into());}
                        if teleport_confirmed{
                            if world.existing_only()&&!world_view::can_move([position.0,position.1,position.2],next.xyz,&loaded,&desired){
                                teleport_id=teleport_id.checked_add(1).ok_or("teleport identifier exhausted")?;
                                teleport_confirmed=false;
                                writer.send(&packets::position_rotated(teleport_id,[position.0,position.1,position.2],[rotation.0,rotation.1])).await?;
                                if !boundary_notice{writer.send(&packets::system_chat("This area is outside the available imported terrain.")).await?;boundary_notice=true;}
                            }else{registration.move_to(next)?;position=(x,y,z);rotation=next_rotation;}
                        }
                    },
                    32|33=>{
                        let next_rotation=if id==32{(f32::from_be_bytes(take(&mut view,4)?.try_into()?),f32::from_be_bytes(take(&mut view,4)?.try_into()?))}else{rotation};
                        let flags=take(&mut view,1)?[0];
                        let next=Position{xyz:[position.0,position.1,position.2],yaw:next_rotation.0,pitch:next_rotation.1,on_ground:flags&1!=0};
                        if !view.is_empty()||!world_view::valid_position(next){return Err("invalid movement packet".into());}
                        if teleport_confirmed{registration.move_to(next)?;rotation=next_rotation;}
                    },
                    41=>{
                        let action=get_varint(&mut view)?;let packed=i64::from_be_bytes(take(&mut view,8)?.try_into()?);take(&mut view,1)?;let sequence=get_varint(&mut view)?;
                        if !(0..=8).contains(&action)||sequence<0||!view.is_empty(){return Err("invalid player action".into());}
                        if matches!(action,4|5|7){
                            inventory_count+=1;
                            if inventory_count<=64{
                                let result=if action==7{inventory.swap_offhand(zerqon_inventory::InventoryPolicy{creative:true,can_interact:teleport_confirmed,allow_drops:false}).await}else{Err(zerqon_inventory::InventoryError::DroppingUnavailable.into())};
                                inventory_response(result,inventory,&mut writer).await?;
                            }else if inventory_count==65{inventory_response(Err(crate::player_inventory::InventoryServiceError::RateLimited),inventory,&mut writer).await?;}
                        }else{
                            let x=(packed>>38) as i32;let y=(packed<<52>>52) as i32;let z=(packed<<26>>38) as i32;
                            if action==0&&teleport_confirmed&&(f64::from(x)+0.5-position.0).powi(2)+(f64::from(y)+0.5-position.1).powi(2)+(f64::from(z)+0.5-position.2).powi(2)<=64.0{
                                let air=data.flat.air;
                                match world.set_block(x,y,z,air).await{
                                    Ok(())=>state.broadcast(block_update(x,y,z,air)),
                                    Err(error) if World::missing_imported_chunk(error.as_ref())=>{},
                                    Err(error)=>return Err(error),
                                }
                            }
                        }
                        let mut ack=vec![4];put_varint(sequence,&mut ack);writer.send(&ack).await?;
                    },
                    _=>{},
                }
            }
        }
    }
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

async fn write_chunk_event(
    event: ChunkEvent,
    writer: &mut Writer,
    world: &World,
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
    sender: mpsc::Sender<ChunkEvent>,
    center: (i32, i32),
    desired: &HashSet<(i32, i32)>,
    loaded: &HashSet<(i32, i32)>,
) -> AbortTask<Result<()>> {
    let removed: Vec<_> = loaded.difference(desired).copied().collect();
    let mut added: Vec<_> = desired.difference(loaded).copied().collect();
    added.sort_by_key(|&(x, z)| (x - center.0).abs() + (z - center.1).abs());
    AbortTask(tokio::spawn(async move {
        for (x, z) in removed {
            sender.send(ChunkEvent::Unload(x, z)).await?;
        }
        sender.send(ChunkEvent::Begin(center.0, center.1)).await?;
        for (x, z) in added {
            sender.send(ChunkEvent::Chunk { x, z }).await?;
        }
        sender.send(ChunkEvent::Finished).await?;
        Ok(())
    }))
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    #[tokio::test]
    async fn reader_created_after_shutdown_does_not_wait_for_network_timeout() {
        let (shutdown, _) = watch::channel(false);
        shutdown.send_replace(true);
        let (stream, _peer) = tokio::io::duplex(1);
        let mut reader = Reader {
            io: Box::new(stream),
            compression: None,
            limit: 1024,
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
        let _stream = stream_chunks(sender, (0, 0), &desired, &loaded);
        assert!(matches!(
            events.recv().await.unwrap(),
            ChunkEvent::Begin(0, 0)
        ));
        let queued = events.recv().await.unwrap();
        assert!(matches!(&queued, ChunkEvent::Chunk { x: 0, z: 0, .. }));
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
        };
        let update = block_update(0, 70, 0, block);
        writer.send(&update).await.unwrap();
        let mut count = 0;
        write_chunk_event(queued, &mut writer, world, &mut loaded, &mut count)
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
