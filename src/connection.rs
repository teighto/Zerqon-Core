use crate::{
    Result,
    auth::{self, CipherReader, CipherWriter, VerifiedProfile},
    packets,
    players::{Identity, PacketBatch, PlayerHandle, Position},
    runtime::{CommandSource, State, block_update},
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
        &registration,
        packets,
        outbound,
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
    registration: &PlayerHandle,
    mut outbound: mpsc::Receiver<PacketBatch>,
    sender: mpsc::Sender<PacketBatch>,
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
    writer.send(&packets::default_spawn(0, 64, 0)).await?;
    writer.send(&packets::position(1, 0.5, 64.0, 0.5)).await?;
    writer.send(&packets::game_event(13, 0.0)).await?;
    let radius = i32::from(state.properties.view_distance);
    let mut loaded = HashSet::new();
    let mut center = (0, 0);
    let mut position: (f64, f64, f64) = (0.5, 64.0, 0.5);
    let mut rotation = (0.0f32, 0.0f32);
    let mut stream = stream_chunks(
        Arc::clone(&state),
        sender.clone(),
        center,
        radius,
        &mut loaded,
    );
    let mut maintenance = tokio::time::interval(Duration::from_secs(1));
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_keepalive = Instant::now();
    let mut outstanding: Option<(i64, Instant)> = None;
    let mut nonce = 0i64;
    let bar = Uuid::new_v4();
    let mut show_bar = false;
    let mut teleport_confirmed = false;
    let mut rate_start = Instant::now();
    let mut packet_count = 0u32;
    let mut shutdown = state.shutdown.subscribe();
    let updates = state.updates.clone();
    let mut update_notifications = crate::updates::NotificationState::default();
    loop {
        tokio::select! {
            _=shutdown.changed()=>{writer.send(&disconnect(32,"Server stopping")).await?;return Ok(());},
            _=disconnected.changed()=>return Err("outbound queue exceeded".into()),
            batch=outbound.recv()=>{let Some(batch)=batch else{return Ok(());};for packet in batch{writer.send(&packet).await?;}},
            _=maintenance.tick()=>{
                let notice = updates.as_ref().and_then(|updates| {
                    let update = updates.borrow();
                    update_notifications.next(state.is_operator(profile.uuid), update.as_ref())
                });
                if let Some(message) = notice { writer.send(&packets::system_chat(&message)).await?; }
                if outstanding.is_some_and(|(_,sent)|sent.elapsed()>Duration::from_secs(30)){return Err("keepalive timeout".into());}
                if outstanding.is_none()&&last_keepalive.elapsed()>=Duration::from_secs(10){nonce=nonce.wrapping_add(1);let mut packet=vec![45];packet.extend_from_slice(&nonce.to_be_bytes());writer.send(&packet).await?;outstanding=Some((nonce,Instant::now()));last_keepalive=Instant::now();}
                if show_bar{let (title,progress)=tpsbar_metrics(&state);for packet in packets::bossbar_update(bar,&title,progress){writer.send(&packet).await?;}}
                if stream.0.is_finished(){if let Some(error)=(&mut stream.0).await?{return Err(error);}let desired=((position.0/16.0).floor() as i32,(position.2/16.0).floor() as i32);if desired!=center{center=desired;stream=stream_chunks(Arc::clone(&state),sender.clone(),center,radius,&mut loaded);}else{stream=AbortTask(tokio::spawn(async{None}));}}
            },
            packet=incoming.recv()=>{
                let packet=packet.ok_or("connection reader stopped")??;
                if rate_start.elapsed()>=Duration::from_secs(1){rate_start=Instant::now();packet_count=0;}
                packet_count+=1;if packet_count>500{return Err("packet rate limit exceeded".into());}
                let mut view=packet.as_slice();let id=get_varint(&mut view)?;
                match id{
                    1=>{
                        let target=get_varint(&mut view)?;
                        if target<=0||!view.is_empty(){return Err("invalid attack packet".into());}
                        if teleport_confirmed&&let Some(mobs)=&state.mobs{mobs.attack(profile.uuid,registration.id(),target).await?;}
                    },
                    0=>{if get_varint(&mut view)?!=1{return Err("invalid teleport acknowledgement".into());}teleport_confirmed=true;},
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
                        if id==31{rotation=(f32::from_be_bytes(take(&mut view,4)?.try_into()?),f32::from_be_bytes(take(&mut view,4)?.try_into()?));}
                        let flags=take(&mut view,1)?[0];
                        if teleport_confirmed{registration.move_to(Position{xyz:[x,y,z],yaw:rotation.0,pitch:rotation.1,on_ground:flags&1!=0})?;position=(x,y,z);}
                    },
                    32|33=>{
                        if id==32{rotation=(f32::from_be_bytes(take(&mut view,4)?.try_into()?),f32::from_be_bytes(take(&mut view,4)?.try_into()?));}
                        let flags=take(&mut view,1)?[0];
                        if teleport_confirmed{registration.move_to(Position{xyz:[position.0,position.1,position.2],yaw:rotation.0,pitch:rotation.1,on_ground:flags&1!=0})?;}
                    },
                    41=>{let action=get_varint(&mut view)?;let packed=i64::from_be_bytes(take(&mut view,8)?.try_into()?);take(&mut view,1)?;let sequence=get_varint(&mut view)?;let x=(packed>>38) as i32;let y=(packed<<52>>52) as i32;let z=(packed<<26>>38) as i32;if action==0&&teleport_confirmed&&(f64::from(x)+0.5-position.0).powi(2)+(f64::from(y)+0.5-position.1).powi(2)+(f64::from(z)+0.5-position.2).powi(2)<=64.0{let air=data.flat.air;state.world.as_ref().ok_or("world unavailable")?.set_block(x,y,z,air).await?;state.broadcast(block_update(x,y,z,air));}let mut ack=vec![4];put_varint(sequence,&mut ack);writer.send(&ack).await?;},
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

fn stream_chunks(
    state: Arc<State>,
    sender: mpsc::Sender<PacketBatch>,
    center: (i32, i32),
    radius: i32,
    loaded: &mut HashSet<(i32, i32)>,
) -> AbortTask<Option<Box<dyn std::error::Error + Send + Sync>>> {
    let desired: HashSet<_> = (-radius..=radius)
        .flat_map(|x| (-radius..=radius).map(move |z| (center.0 + x, center.1 + z)))
        .collect();
    let removed: Vec<_> = loaded.difference(&desired).copied().collect();
    let mut added: Vec<_> = desired.difference(loaded).copied().collect();
    added.sort_by_key(|&(x, z)| (x - center.0).abs() + (z - center.1).abs());
    *loaded = desired;
    AbortTask(tokio::spawn(async move {
        let result: Result<()> = async {
            for (x, z) in removed {
                let mut packet = vec![38];
                packet.extend_from_slice(&z.to_be_bytes());
                packet.extend_from_slice(&x.to_be_bytes());
                sender.send(vec![packet.into()]).await?;
            }
            sender
                .send(vec![packets::chunk_center(center.0, center.1).into()])
                .await?;
            sender
                .send(vec![packets::chunk_batch_start().into()])
                .await?;
            let world = state.world.as_ref().ok_or("world unavailable")?;
            let count = added.len();
            for (x, z) in added {
                sender.send(vec![world.packet(x, z).await?.into()]).await?;
            }
            sender
                .send(vec![packets::chunk_batch_finished(count as i32).into()])
                .await?;
            Ok(())
        }
        .await;
        result.err()
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
}
