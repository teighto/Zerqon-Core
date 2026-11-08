use crate::{
    config::{Properties, Settings},
    native_query_runtime::{
        MAX_PROVIDER_WORKING_SET_BYTES, NativeProviderContract, NativeQueryRuntime,
    },
    plugin_services::{
        NativeCallContext, NativeErrorCode, NativeQuery, NativeQueryError, NativeQueryFuture,
        NativeQueryService,
    },
    runtime::State,
    rust_plugins::{ActorLimits, RustPlugins},
};
use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::Poll,
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    time::timeout,
};
use uuid::Uuid;
use zerqon_plugin_api::{Action, Event};
use zerqon_protocol::{
    PROTOCOL_VERSION,
    codec::{get_string, get_varint, put_string, put_varint, read_frame, take, write_frame},
    inbound::{BudgetScope, ReadFrameError},
};

const MIB: usize = 1024 * 1024;
const FRAME_BYTES: usize = 65_536;
type Connection = Pin<Box<dyn Future<Output = crate::Result<()>> + Send>>;

struct Fixture {
    state: Arc<State>,
    listener: TcpListener,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let data = std::env::var_os("ZERQON_TEST_DATA").map_or_else(
            || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".cache/vanilla/26.3"),
            PathBuf::from,
        );
        assert!(data.join("prepared.json").is_file(), "run prepare-data.py");
        let directory = tempfile::tempdir().unwrap();
        crate::config::initialize(directory.path()).unwrap();
        let settings = Settings {
            data_directory: data.to_string_lossy().into_owned(),
            world_threads: 1,
            work_queue_capacity: 8,
            packet_memory_mib: 1,
            connection_packet_memory_mib: 1,
            java_plugins: false,
            spawn_mobs: false,
            update_check: false,
            ..Settings::default()
        };
        settings.validate().unwrap();
        let (shutdown, _) = watch::channel(false);
        let state = timeout(
            Duration::from_secs(30),
            State::new(
                directory.path(),
                Properties::parse(
                    "online-mode=false\nview-distance=1\nnetwork-compression-threshold=-1\n",
                )
                .unwrap(),
                settings,
                false,
                shutdown,
                None,
            ),
        )
        .await
        .expect("fixture startup timed out")
        .unwrap();
        Self {
            state,
            listener: TcpListener::bind("127.0.0.1:0").await.unwrap(),
            _directory: directory,
        }
    }

    async fn connect(&self, name: &str) -> (Client, Connection) {
        self.connect_with_initial_ack(name, true).await
    }

    async fn connect_with_initial_ack(
        &self,
        name: &str,
        acknowledge: bool,
    ) -> (Client, Connection) {
        self.connect_with_probe(name, acknowledge, None).await
    }

    async fn connect_with_probe(
        &self,
        name: &str,
        acknowledge: bool,
        probe: Option<Arc<ProbeGate>>,
    ) -> (Client, Connection) {
        let socket = TcpStream::connect(self.listener.local_addr().unwrap())
            .await
            .unwrap();
        socket.set_nodelay(true).unwrap();
        let (server, _) = self.listener.accept().await.unwrap();
        let mut connection: Connection = Box::pin(super::handle(server, Arc::clone(&self.state)));
        let client = timeout(
            Duration::from_secs(20),
            drive(
                &mut connection,
                Client::join(socket, name, acknowledge, probe),
            ),
        )
        .await
        .expect("client join timed out");
        (client, connection)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.shutdown.send_replace(true);
    }
}

struct Client {
    socket: TcpStream,
    uuid: Uuid,
    position: [f64; 3],
    pending_ack: Option<Vec<u8>>,
    join_probe_seen: bool,
}

impl Client {
    async fn join(
        mut socket: TcpStream,
        name: &str,
        acknowledge: bool,
        probe: Option<Arc<ProbeGate>>,
    ) -> Self {
        let mut handshake = vec![0];
        put_varint(PROTOCOL_VERSION, &mut handshake);
        put_string("localhost", &mut handshake);
        handshake.extend_from_slice(&socket.peer_addr().unwrap().port().to_be_bytes());
        handshake.push(2);
        write_frame(&mut socket, &handshake, None).await.unwrap();
        let mut hello = vec![0];
        put_string(name, &mut hello);
        hello.extend_from_slice(&[0; 16]);
        write_frame(&mut socket, &hello, None).await.unwrap();
        let success = read_frame(&mut socket, FRAME_BYTES, None).await.unwrap();
        let mut body = success.as_slice();
        assert_eq!(get_varint(&mut body).unwrap(), 2);
        let uuid = Uuid::from_slice(take(&mut body, 16).unwrap()).unwrap();
        assert_eq!(get_string(&mut body, 16).unwrap(), name);
        assert_eq!(get_varint(&mut body).unwrap(), 0);
        take(&mut body, 16).unwrap();
        assert!(body.is_empty());
        let mut client = Self {
            socket,
            uuid,
            position: [0.0; 3],
            pending_ack: None,
            join_probe_seen: false,
        };
        client.send(&[3]).await;
        assert_eq!(client.read().await, crate::client_policy::PROBE_PACKET);
        let mut known = client.read().await;
        assert_eq!(known[0], 15);
        known[0] = 7;
        client.send(&known).await;
        loop {
            let packet = client.read().await;
            if packet == [3] {
                break;
            }
            assert_ne!(packet[0], 2, "configuration disconnect: {packet:?}");
        }
        client.send(&[3]).await;
        let mut logged_in = false;
        let mut teleported = false;
        let mut chunks = 0;
        loop {
            let packet = client.read().await;
            let mut body = packet.as_slice();
            match get_varint(&mut body).unwrap() {
                50 => {
                    assert!(
                        i32::from_be_bytes(take(&mut body, 4).unwrap().try_into().unwrap()) > 0
                    );
                    logged_in = true;
                }
                73 => {
                    let teleport = get_varint(&mut body).unwrap();
                    let coordinates = take(&mut body, 24).unwrap();
                    for (index, bytes) in coordinates.chunks_exact(8).enumerate() {
                        client.position[index] = f64::from_be_bytes(bytes.try_into().unwrap());
                    }
                    take(&mut body, 24).unwrap();
                    let rotation = take(&mut body, 8).unwrap();
                    assert_eq!(take(&mut body, 4).unwrap(), &[0; 4]);
                    assert!(body.is_empty());
                    let mut ack = vec![0];
                    put_varint(teleport, &mut ack);
                    ack.extend_from_slice(coordinates);
                    ack.extend_from_slice(rotation);
                    if acknowledge {
                        client.send(&ack).await;
                    } else {
                        client.pending_ack = Some(ack);
                    }
                    teleported = true;
                }
                46 => chunks += 1,
                124 if probe.is_some() => {
                    let text = probe_chat_text(body).expect("invalid Join probe chat");
                    assert_eq!(text, CONTROL_PROBE_MESSAGE);
                    assert!(!client.join_probe_seen, "Join probe was sent twice");
                    client.join_probe_seen = true;
                    probe.as_ref().unwrap().release();
                }
                11 => {
                    assert_eq!(get_varint(&mut body).unwrap(), chunks);
                    assert!(body.is_empty());
                    break;
                }
                32 => panic!("play disconnect: {packet:?}"),
                _ => {}
            }
        }
        assert!(logged_in && teleported && chunks == 9);
        client.send(&[44]).await;
        let mut batch = vec![11];
        batch.extend_from_slice(&8.0_f32.to_be_bytes());
        client.send(&batch).await;
        client
    }

    async fn send(&mut self, packet: &[u8]) {
        write_frame(&mut self.socket, packet, None).await.unwrap();
    }

    async fn read(&mut self) -> Vec<u8> {
        read_frame(&mut self.socket, 2_097_151, None).await.unwrap()
    }

    async fn version(&mut self) {
        let mut command = vec![7];
        put_string("version", &mut command);
        self.send(&command).await;
        loop {
            let packet = self.read().await;
            match packet[0] {
                124 => {
                    let mut body = &packet[1..];
                    assert_eq!(take(&mut body, 1).unwrap(), &[8]);
                    let len = u16::from_be_bytes(take(&mut body, 2).unwrap().try_into().unwrap());
                    let text =
                        std::str::from_utf8(take(&mut body, usize::from(len)).unwrap()).unwrap();
                    assert!(text.contains("Zerqon") && text.contains("26.3"), "{text}");
                    assert_eq!(take(&mut body, 1).unwrap(), &[0]);
                    assert!(body.is_empty());
                    return;
                }
                45 => {
                    let mut response = packet;
                    response[0] = 28;
                    self.send(&response).await;
                }
                32 => panic!("healthy client disconnected: {packet:?}"),
                _ => {}
            }
        }
    }

    async fn movement_and_marker(&mut self) -> usize {
        let mut movement = vec![30];
        for coordinate in [self.position[0] + 0.25, self.position[1], self.position[2]] {
            movement.extend_from_slice(&coordinate.to_be_bytes());
        }
        movement.push(1);
        self.send(&movement).await;
        self.send(&[127]).await;
        movement.len() + 1
    }

    async fn large_frames(&mut self, count: usize) {
        let mut packet = vec![0; FRAME_BYTES];
        packet[0] = 127;
        for _ in 0..count {
            self.send(&packet).await;
        }
    }

    async fn expect_closed(&mut self) {
        assert!(
            timeout(
                Duration::from_secs(1),
                read_frame(&mut self.socket, 2_097_151, None),
            )
            .await
            .expect("server did not close TCP connection")
            .is_err()
        );
    }
}

async fn drive<T>(connection: &mut Connection, client: impl Future<Output = T>) -> T {
    tokio::select! {
        result = connection => panic!("connection ended before client operation: {result:?}"),
        result = client => result,
    }
}

async fn poll_connection_once(connection: &mut Connection) {
    tokio::task::unconstrained(std::future::poll_fn(|context| {
        assert!(
            connection.as_mut().poll(context).is_pending(),
            "connection ended during the movement barrier"
        );
        Poll::Ready(())
    }))
    .await;
}

async fn wait_used(state: &State, expected: usize) {
    timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = state.packet_budget.snapshot();
            assert!(snapshot.used_bytes <= MIB && snapshot.peak_bytes <= MIB);
            if snapshot.used_bytes == expected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "expected {expected} reserved bytes: {:?}",
            state.packet_budget.snapshot()
        )
    });
}

async fn block_movement(client: &mut Client, connection: &mut Connection, state: &State) -> usize {
    let held = client.movement_and_marker().await;
    // The reader reserves the marker only after enqueueing the complete movement.
    // Polling the quiescent connection must then reach the held gameplay read lock.
    wait_used(state, held).await;
    poll_connection_once(connection).await;
    assert_eq!(state.packet_budget.snapshot().used_bytes, held);
    assert_eq!(
        state
            .players
            .query_snapshot(client.uuid)
            .unwrap()
            .unwrap()
            .position
            .xyz,
        client.position,
    );
    held
}

#[tokio::test]
#[ignore = "requires prepared official 26.3 game data"]
async fn saturated_connection_budget_preserves_other_tcp_clients_and_releases_memory() {
    let fixture = Fixture::new().await;
    let (mut bad, mut bad_connection) = fixture.connect("BudgetFlood").await;
    let (mut healthy, healthy_connection) = fixture.connect("BudgetHealthy").await;
    let mut healthy_task = super::AbortTask(tokio::spawn(healthy_connection));

    timeout(Duration::from_secs(5), async {
        healthy.version().await;
        drive(&mut bad_connection, bad.version()).await;
        poll_connection_once(&mut bad_connection).await;
        wait_used(&fixture.state, 0).await;
        assert_eq!(fixture.state.players.count(), 2);
        let blocker = fixture.state.gameplay.write().await;
        let held = block_movement(&mut bad, &mut bad_connection, &fixture.state).await;
        bad.large_frames(15).await;
        let mut rejected_frame_length = Vec::new();
        put_varint(FRAME_BYTES as i32, &mut rejected_frame_length);
        bad.socket.write_all(&rejected_frame_length).await.unwrap();
        let queued = held + 15 * FRAME_BYTES;
        wait_used(&fixture.state, queued).await;
        drive(&mut bad_connection, healthy.version()).await;
        wait_used(&fixture.state, queued).await;
        assert!(fixture.state.packet_budget.snapshot().peak_bytes >= queued);

        drop(blocker);
        let error = bad_connection.as_mut().await.unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<ReadFrameError>(),
                Some(ReadFrameError::Budget {
                    scope: BudgetScope::Connection,
                    requested: FRAME_BYTES,
                    limit: MIB,
                })
            ),
            "expected per-connection memory rejection, got {error}"
        );
        drop(bad_connection);
        bad.expect_closed().await;
        healthy.version().await;
        drop(healthy);
        assert!((&mut healthy_task.0).await.unwrap().is_err());
        wait_used(&fixture.state, 0).await;
        assert_eq!(fixture.state.players.count(), 0);
        assert_eq!(fixture.state.packet_budget.snapshot().limit_bytes, MIB);
        assert!(fixture.state.packet_budget.snapshot().peak_bytes <= MIB);
    })
    .await
    .expect("TCP budget isolation exceeded its five-second window");
    fixture.state.shutdown.send_replace(true);
    fixture.state.profiler.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires prepared official 26.3 game data"]
async fn cancelling_a_blocked_tcp_handler_reclaims_queued_packet_leases() {
    let fixture = Fixture::new().await;
    let (mut client, mut connection) = fixture.connect("BudgetCancel").await;
    timeout(Duration::from_secs(5), async {
        drive(&mut connection, client.version()).await;
        poll_connection_once(&mut connection).await;
        wait_used(&fixture.state, 0).await;
        let blocker = fixture.state.gameplay.write().await;
        let held = block_movement(&mut client, &mut connection, &fixture.state).await;
        client.large_frames(4).await;
        wait_used(&fixture.state, held + 4 * FRAME_BYTES).await;
        drop(connection);
        wait_used(&fixture.state, 0).await;
        client.expect_closed().await;
        drop(blocker);
        fixture.state.sessions.drain().await;
        assert_eq!(fixture.state.players.count(), 0);
    })
    .await
    .expect("TCP handler cancellation exceeded its five-second window");
    fixture.state.shutdown.send_replace(true);
    fixture.state.profiler.shutdown().await.unwrap();
}

const CONTROL_PROBE_PLUGIN: &str = "control-probe";
const CONTROL_PROBE_MESSAGE: &str = "Native callback is waiting for this message";
const QUIT_PROBE_PLUGIN: &str = "a-quit-probe";
const JAVA_JOIN_PROBE_PLUGIN: &str = "native-join";

struct ProbeGate {
    released: AtomicBool,
    permit: Semaphore,
}

impl ProbeGate {
    fn release(&self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            self.permit.add_permits(1);
        }
    }
}

struct ReleaseProbe(Arc<ProbeGate>);

impl Drop for ReleaseProbe {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[derive(Clone, Debug)]
struct ProbeObservation {
    plugin: String,
    uuid: Uuid,
    session_id: i32,
    parent_id: u64,
    call_id: u64,
}

struct ControlProbeProvider {
    inner: Arc<crate::server_queries::ServerQueries>,
    players: Arc<crate::players::Players>,
    expected: Mutex<Option<(Uuid, i32)>>,
    armed: AtomicBool,
    gate: Arc<ProbeGate>,
    observed: Mutex<Option<ProbeObservation>>,
    completed: AtomicBool,
    released_before_deadline: AtomicBool,
    lifecycle: crate::lifecycle::Lifecycle,
    packet_budget: zerqon_protocol::inbound::PacketBudget,
    quit_seen: Semaphore,
    quit_count: AtomicUsize,
    quit_during_command: AtomicBool,
    quit_packet_bytes: Mutex<Option<usize>>,
    join_worlds: AtomicBool,
}

impl NativeQueryService for ControlProbeProvider {
    fn query(&self, context: NativeCallContext, query: NativeQuery) -> NativeQueryFuture<'_> {
        Box::pin(async move {
            if context.plugin == QUIT_PROBE_PLUGIN && matches!(query, NativeQuery::Worlds) {
                self.quit_during_command.store(
                    self.lifecycle.pending().contains(&"player command"),
                    Ordering::Release,
                );
                *self.quit_packet_bytes.lock().unwrap() =
                    Some(self.packet_budget.snapshot().used_bytes);
                self.quit_count.fetch_add(1, Ordering::AcqRel);
                self.quit_seen.add_permits(1);
            }
            let join_worlds = self.join_worlds.load(Ordering::Acquire)
                && context.plugin == JAVA_JOIN_PROBE_PLUGIN
                && matches!(query, NativeQuery::Worlds);
            let player = if join_worlds {
                let players = self.players.snapshots().map_err(|_| {
                    NativeQueryError::new(NativeErrorCode::Unavailable, "Join registry unavailable")
                })?;
                if players.len() != 1 {
                    return Err(NativeQueryError::new(
                        NativeErrorCode::Unavailable,
                        "Join probe needs exactly one player",
                    ));
                }
                Some((players[0].uuid, players[0].id))
            } else {
                match &query {
                    NativeQuery::Player {
                        uuid,
                        session_id: Some(session_id),
                    } => Some((*uuid, *session_id)),
                    _ => None,
                }
            };
            let gated = player.is_some() && self.armed.swap(false, Ordering::AcqRel);
            if let Some((uuid, session_id)) = player.filter(|_| gated) {
                let expected = {
                    let mut expected = self.expected.lock().unwrap();
                    if join_worlds && expected.is_none() {
                        *expected = Some((uuid, session_id));
                    }
                    *expected
                };
                let expected_plugin = if join_worlds {
                    JAVA_JOIN_PROBE_PLUGIN
                } else {
                    CONTROL_PROBE_PLUGIN
                };
                if expected != Some((uuid, session_id)) || context.plugin != expected_plugin {
                    return Err(NativeQueryError::new(
                        NativeErrorCode::InvalidArgument,
                        "Probe did not receive the connected player's exact session",
                    ));
                }
                self.players
                    .dispatch_batches(vec![(
                        uuid,
                        session_id,
                        vec![bytes::Bytes::from(crate::packets::system_chat(
                            CONTROL_PROBE_MESSAGE,
                        ))],
                    )])
                    .map_err(|_| {
                        NativeQueryError::new(
                            NativeErrorCode::Unavailable,
                            "Probe queue unavailable",
                        )
                    })?;
                *self.observed.lock().unwrap() = Some(ProbeObservation {
                    plugin: context.plugin.clone(),
                    uuid,
                    session_id,
                    parent_id: context.parent_id,
                    call_id: context.call_id,
                });
                self.gate.permit.acquire().await.unwrap().forget();
                self.released_before_deadline.store(
                    tokio::time::Instant::now() < context.deadline,
                    Ordering::Release,
                );
            }
            let result = self.inner.query(context, query).await;
            if gated {
                self.completed.store(true, Ordering::Release);
            }
            result
        })
    }
}

async fn install_control_probe(fixture: &mut Fixture) -> Arc<ControlProbeProvider> {
    let provider = Arc::new(ControlProbeProvider {
        inner: Arc::clone(&fixture.state.queries),
        players: Arc::clone(&fixture.state.players),
        expected: Mutex::new(None),
        armed: AtomicBool::new(false),
        gate: Arc::new(ProbeGate {
            released: AtomicBool::new(false),
            permit: Semaphore::new(0),
        }),
        observed: Mutex::new(None),
        completed: AtomicBool::new(false),
        released_before_deadline: AtomicBool::new(false),
        lifecycle: fixture.state.lifecycle.clone(),
        packet_budget: fixture.state.packet_budget.clone(),
        quit_seen: Semaphore::new(0),
        quit_count: AtomicUsize::new(0),
        quit_during_command: AtomicBool::new(false),
        quit_packet_bytes: Mutex::new(None),
        join_worlds: AtomicBool::new(false),
    });
    let runtime = NativeQueryRuntime::new(
        provider.clone(),
        fixture.state.native_read_budget.clone(),
        fixture.state.lifecycle.clone(),
        NativeProviderContract::future_owned(MAX_PROVIDER_WORKING_SET_BYTES).unwrap(),
    );
    assert!(fixture.state.plugins.list().is_empty());
    let replacement = RustPlugins::new(
        zerqon_plugins::Limits {
            fuel_per_event: fixture.state.settings.plugin_fuel,
            memory_bytes: fixture.state.settings.plugin_memory_mib * 1024 * 1024,
            ..zerqon_plugins::Limits::default()
        },
        runtime,
        fixture.state.lifecycle.clone(),
        ActorLimits {
            cpu_jobs: 1,
            ..ActorLimits::default()
        },
    )
    .unwrap();
    Arc::get_mut(&mut fixture.state).unwrap().plugins = replacement;
    let guest = std::env::var_os("ZERQON_TEST_NATIVE_WASM").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(".cache/rust-plugin-fixtures/hello-native-read.wasm")
        },
        PathBuf::from,
    );
    let bytes = std::fs::read(guest).expect("build the actual native-read Rust WASM fixture");
    let output = fixture
        .state
        .plugins
        .load(
            CONTROL_PROBE_PLUGIN,
            &bytes,
            &fixture.state.lifecycle.token(),
            tokio::time::Instant::now() + Duration::from_secs(10),
        )
        .await
        .unwrap();
    {
        let mut commands = fixture.state.commands.lock().unwrap();
        for action in output.actions() {
            if let Action::RegisterCommand {
                name, permission, ..
            } = action
            {
                assert!(
                    commands
                        .insert(
                            name.clone(),
                            (CONTROL_PROBE_PLUGIN.into(), permission.clone())
                        )
                        .is_none()
                );
            }
        }
        assert!(
            commands.contains_key("rwhere"),
            "fixture must have native-read enabled"
        );
    }
    drop(output);
    provider
}

async fn probe_send_command(client: &mut Client, command: &str) -> Result<(), String> {
    let mut packet = vec![7];
    put_string(command, &mut packet);
    write_frame(&mut client.socket, &packet, None)
        .await
        .map_err(|error| error.to_string())
}

async fn probe_read_chat(client: &mut Client) -> Result<String, String> {
    loop {
        let packet = read_frame(&mut client.socket, 2_097_151, None)
            .await
            .map_err(|error| error.to_string())?;
        let mut body = packet.as_slice();
        let id = get_varint(&mut body).map_err(|error| error.to_string())?;
        match id {
            124 => {
                return probe_chat_text(body);
            }
            45 => {
                let mut reply = packet;
                reply[0] = 28;
                write_frame(&mut client.socket, &reply, None)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            32 => return Err("server disconnected while its native callback was pending".into()),
            _ => {}
        }
    }
}

fn probe_chat_text(mut body: &[u8]) -> Result<String, String> {
    if take(&mut body, 1).map_err(|error| error.to_string())? != [8] {
        return Err("unexpected system-chat NBT type".into());
    }
    let length = take(&mut body, 2).map_err(|error| error.to_string())?;
    let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
    let text = std::str::from_utf8(take(&mut body, length).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?
        .to_owned();
    if take(&mut body, 1).map_err(|error| error.to_string())? != [0] || !body.is_empty() {
        return Err("unexpected system-chat overlay or trailing data".into());
    }
    Ok(text)
}

async fn probe_command_exchange(
    client: &mut Client,
    provider: &ControlProbeProvider,
    session_id: i32,
) -> Result<(), String> {
    probe_send_command(client, "rwhere").await?;
    let first = probe_read_chat(client).await?;
    if first != CONTROL_PROBE_MESSAGE {
        return Err(format!(
            "callback produced a terminal response before its outbound probe: {first}"
        ));
    }
    provider.gate.release();
    let result = probe_read_chat(client).await?;
    let expected = format!(
        "CONSOLE: uuid={}, session={session_id}, world=world(",
        client.uuid
    );
    if !result.starts_with(&expected) || !result.contains("online=1/20, protocol=777") {
        return Err(format!(
            "native Rust callback did not complete with real player data: {result}"
        ));
    }
    probe_send_command(client, "hello").await?;
    let hello = probe_read_chat(client).await?;
    if hello != "Hello, CONSOLE! This plugin is written in Rust." {
        return Err(format!(
            "Rust instance did not survive the callback: {hello}"
        ));
    }
    Ok(())
}

fn movement_packet(position: [f64; 3]) -> Vec<u8> {
    let mut packet = vec![30];
    for coordinate in position {
        packet.extend_from_slice(&coordinate.to_be_bytes());
    }
    packet.push(1);
    packet
}

fn encoded_command(input: &str) -> Vec<u8> {
    let mut packet = vec![7];
    put_string(input, &mut packet);
    packet
}

async fn await_pending_command_budget(
    state: &State,
    provider: &ControlProbeProvider,
    used: usize,
    peak: usize,
) -> Result<(), String> {
    loop {
        let native = state.native_read_budget.snapshot();
        if native.quarantined_operations != 0 || provider.completed.load(Ordering::Acquire) {
            return Err(format!(
                "command settled before the input barrier: {native:?}"
            ));
        }
        let budget = state.packet_budget.snapshot();
        if budget.used_bytes == used && budget.peak_bytes >= peak {
            return Ok(());
        }
        tokio::task::yield_now().await;
    }
}

async fn finish_control_probe(
    fixture: &Fixture,
    provider: &ControlProbeProvider,
    connection: Option<Connection>,
    client: Option<Client>,
    finished: bool,
) -> Vec<String> {
    provider.gate.release();
    finish_wire_fixture(fixture, connection, client, finished).await
}

async fn finish_wire_fixture(
    fixture: &Fixture,
    mut connection: Option<Connection>,
    client: Option<Client>,
    finished: bool,
) -> Vec<String> {
    fixture.state.lifecycle.stop();
    let mut failures = Vec::new();
    if let Some(connection) = &mut connection
        && !finished
    {
        match timeout(Duration::from_secs(5), connection).await {
            Ok(Ok(())) => {}
            Ok(Err(error))
                if crate::managed_tasks::is_cancelled(error.as_ref())
                    || error.to_string() == "server stopping" => {}
            other => failures.push(format!("connection cleanup: {other:?}")),
        }
    }
    drop(connection);
    drop(client);
    if timeout(Duration::from_secs(5), fixture.state.sessions.drain())
        .await
        .is_err()
    {
        failures.push("player sessions did not drain".into());
    }
    if let Err(error) = fixture.state.event(Event::Disable).await {
        failures.push(format!("plugin cleanup: {error}"));
    }
    if let Err(error) = fixture.state.profiler.shutdown().await {
        failures.push(format!("profiler cleanup: {error}"));
    }
    if let Err(error) = fixture
        .state
        .lifecycle
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await
    {
        failures.push(format!("lifecycle cleanup: {error}"));
    }
    failures
}

#[tokio::test]
#[ignore = "requires prepared official 26.3 data and the compiled native-read Rust WASM fixture"]
async fn pending_command_keeps_packet_leases_and_rejects_movement_received_before_teleport_ack() {
    let mut fixture = Fixture::new().await;
    let provider = install_control_probe(&mut fixture).await;
    let release_on_unwind = ReleaseProbe(Arc::clone(&provider.gate));
    let (mut client, mut connection) = fixture.connect_with_initial_ack("CONSOLE", false).await;
    drive(&mut connection, client.version()).await;
    drive(&mut connection, wait_used(&fixture.state, 0)).await;
    let initial = client.position;
    let uuid = client.uuid;
    let session_id = fixture
        .state
        .players
        .query_snapshot(uuid)
        .unwrap()
        .unwrap()
        .id;
    *provider.expected.lock().unwrap() = Some((uuid, session_id));
    provider.armed.store(true, Ordering::Release);
    let command = encoded_command("rwhere");
    let before_ack = movement_packet([initial[0] + 0.25, initial[1], initial[2]]);
    let fresh = [initial[0] + 1.25, initial[1], initial[2] + 0.25];
    let after_ack = movement_packet(fresh);
    let after_duplicate = [initial[0] + 2.25, initial[1], initial[2] + 0.25];
    let valid_deferred = movement_packet(after_duplicate);
    let acknowledgement = client.pending_ack.take().unwrap();
    let mut marker = vec![0; FRAME_BYTES / 2];
    marker[0] = 127;
    let mut observations = None;
    let mut finished = false;
    let scenario = timeout(Duration::from_secs(5), async {
        tokio::select! {
            result = &mut connection => { finished = true; Err(format!("connection ended: {result:?}")) },
            result = async {
                write_frame(&mut client.socket, &command, None).await.map_err(|error| error.to_string())?;
                let probe = probe_read_chat(&mut client).await?;
                if probe != CONTROL_PROBE_MESSAGE { return Err(format!("unexpected pending-command probe: {probe}")); }
                let command_lease = fixture.state.packet_budget.snapshot().used_bytes;
                write_frame(&mut client.socket, &before_ack, None).await.map_err(|error| error.to_string())?;
                write_frame(&mut client.socket, &acknowledgement, None).await.map_err(|error| error.to_string())?;
                write_frame(&mut client.socket, &marker, None).await.map_err(|error| error.to_string())?;
                await_pending_command_budget(
                    &fixture.state, &provider, command.len() + before_ack.len(),
                    command.len() + before_ack.len() + marker.len(),
                ).await?;
                let held = fixture.state.packet_budget.snapshot().used_bytes;
                let pending_position = fixture.state.players.query_snapshot(uuid).map_err(|error| error.to_string())?.ok_or("player disappeared")?.position.xyz;
                provider.gate.release();
                let reply = probe_read_chat(&mut client).await?;
                if !reply.starts_with("CONSOLE: uuid=") { return Err(format!("native command did not finish: {reply}")); }
                probe_send_command(&mut client, "version").await?;
                let version = probe_read_chat(&mut client).await?;
                if !version.contains("Zerqon") { return Err(format!("FIFO command fence failed: {version}")); }
                loop {
                    if fixture.state.packet_budget.snapshot().used_bytes == 0 { break; }
                    tokio::task::yield_now().await;
                }
                let after_command = fixture.state.players.query_snapshot(uuid).map_err(|error| error.to_string())?.ok_or("player disappeared")?.position.xyz;
                write_frame(&mut client.socket, &after_ack, None).await.map_err(|error| error.to_string())?;
                loop {
                    let position = fixture.state.players.query_snapshot(uuid).map_err(|error| error.to_string())?.ok_or("player disappeared")?.position.xyz;
                    if position == fresh { break; }
                    tokio::task::yield_now().await;
                }
                if !provider.completed.load(Ordering::Acquire) || provider.gate.permit.available_permits() != 0 {
                    return Err("first query did not settle before duplicate-ACK phase".into());
                }
                provider.completed.store(false, Ordering::Release);
                provider.gate.released.store(false, Ordering::Release);
                provider.armed.store(true, Ordering::Release);
                probe_send_command(&mut client, "rwhere").await?;
                let second_probe = probe_read_chat(&mut client).await?;
                if second_probe != CONTROL_PROBE_MESSAGE { return Err(format!("second query did not reach its gate: {second_probe}")); }
                write_frame(&mut client.socket, &valid_deferred, None).await.map_err(|error| error.to_string())?;
                write_frame(&mut client.socket, &acknowledgement, None).await.map_err(|error| error.to_string())?;
                marker.resize(FRAME_BYTES, 0);
                write_frame(&mut client.socket, &marker, None).await.map_err(|error| error.to_string())?;
                await_pending_command_budget(
                    &fixture.state, &provider, command.len() + valid_deferred.len(),
                    command.len() + valid_deferred.len() + marker.len(),
                ).await?;
                provider.gate.release();
                let second_reply = probe_read_chat(&mut client).await?;
                if !second_reply.starts_with("CONSOLE: uuid=") { return Err(format!("second native command did not finish: {second_reply}")); }
                probe_send_command(&mut client, "version").await?;
                let version = probe_read_chat(&mut client).await?;
                if !version.contains("Zerqon") { return Err(format!("duplicate-ACK FIFO fence failed: {version}")); }
                loop {
                    if fixture.state.packet_budget.snapshot().used_bytes == 0 { break; }
                    tokio::task::yield_now().await;
                }
                let after_duplicate_ack = fixture.state.players.query_snapshot(uuid).map_err(|error| error.to_string())?.ok_or("player disappeared")?.position.xyz;
                observations = Some((command_lease, held, pending_position, after_command, after_duplicate_ack));
                Ok(())
            } => result,
        }
    }).await.map_err(|_| "input-order scenario exceeded its watchdog".to_owned()).and_then(|result| result);
    let cleanup = finish_control_probe(
        &fixture,
        &provider,
        Some(connection),
        Some(client),
        finished,
    )
    .await;
    drop(release_on_unwind);
    assert!(
        cleanup.is_empty(),
        "cleanup failed: {cleanup:?}; scenario={scenario:?}"
    );
    assert!(scenario.is_ok(), "{scenario:?}");
    let (command_lease, held, pending_position, after_command, after_duplicate_ack) =
        observations.unwrap();
    assert_eq!(command_lease, command.len());
    assert_eq!(held, command.len() + before_ack.len());
    assert_eq!(pending_position, initial);
    assert_eq!(
        after_command, initial,
        "pre-ACK movement was replayed after ACK"
    );
    assert_eq!(
        after_duplicate_ack, after_duplicate,
        "duplicate ACK invalidated later legitimate movement"
    );
    assert!(provider.released_before_deadline.load(Ordering::Acquire));
    assert_eq!(fixture.state.packet_budget.snapshot().used_bytes, 0);
    assert_eq!(
        fixture.state.native_read_budget.snapshot().reserved_bytes,
        0
    );
}

#[tokio::test]
#[ignore = "requires prepared official 26.3 data and the compiled native-read Rust WASM fixture"]
async fn rust_native_callback_delivers_outbound_to_its_waiting_command_connection() {
    let mut fixture = Fixture::new().await;
    let provider = install_control_probe(&mut fixture).await;
    let release_on_unwind = ReleaseProbe(Arc::clone(&provider.gate));
    let (mut client, mut connection) = fixture.connect("CONSOLE").await;
    drive(&mut connection, client.version()).await;
    let expected_uuid = client.uuid;
    let session_id = fixture
        .state
        .players
        .query_snapshot(client.uuid)
        .unwrap()
        .unwrap()
        .id;
    *provider.expected.lock().unwrap() = Some((client.uuid, session_id));
    provider.armed.store(true, Ordering::Release);
    let mut connection_finished = false;
    let scenario = timeout(Duration::from_secs(5), async {
        tokio::select! {
            result = &mut connection => {
                connection_finished = true;
                Err(format!("connection ended before command completion: {result:?}"))
            },
            result = probe_command_exchange(&mut client, &provider, session_id) => result,
        }
    })
    .await
    .map_err(|_| "command/probe exchange exceeded the outer watchdog".to_owned())
    .and_then(|result| result);
    let healthy = fixture
        .state
        .plugins
        .list()
        .iter()
        .any(|plugin| plugin.name == CONTROL_PROBE_PLUGIN && plugin.enabled);
    provider.gate.release();
    fixture.state.lifecycle.stop();
    let mut cleanup = Vec::new();
    if !connection_finished {
        match timeout(Duration::from_secs(5), &mut connection).await {
            Ok(Ok(())) => {}
            Ok(Err(error))
                if crate::managed_tasks::is_cancelled(error.as_ref())
                    || error.to_string() == "server stopping" => {}
            other => cleanup.push(format!("connection cleanup: {other:?}")),
        }
    }
    drop(connection);
    drop(client);
    if timeout(Duration::from_secs(5), fixture.state.sessions.drain())
        .await
        .is_err()
    {
        cleanup.push("player sessions did not drain".into());
    }
    if let Err(error) = fixture.state.event(Event::Disable).await {
        cleanup.push(format!("plugin cleanup: {error}"));
    }
    if let Err(error) = fixture.state.profiler.shutdown().await {
        cleanup.push(format!("profiler cleanup: {error}"));
    }
    if let Err(error) = fixture
        .state
        .lifecycle
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await
    {
        cleanup.push(format!("lifecycle cleanup: {error}"));
    }
    drop(release_on_unwind);
    let observation = provider.observed.lock().unwrap().clone();
    let budget = fixture.state.native_read_budget.snapshot();
    assert!(
        cleanup.is_empty(),
        "cleanup failed before checking the regression: {cleanup:?}; scenario={scenario:?}"
    );
    assert!(
        scenario.is_ok(),
        "{scenario:?}; observed={observation:?}; released_before_deadline={}",
        provider.released_before_deadline.load(Ordering::Acquire)
    );
    let observation =
        observation.expect("the real WASM callback never entered the native provider");
    assert_eq!(observation.plugin, CONTROL_PROBE_PLUGIN);
    assert_eq!(
        (observation.uuid, observation.session_id),
        (expected_uuid, session_id)
    );
    assert!(observation.parent_id > 0 && observation.call_id > 0);
    assert!(provider.completed.load(Ordering::Acquire));
    assert!(provider.released_before_deadline.load(Ordering::Acquire));
    assert!(healthy, "a native query deadline disabled the Rust plugin");
    assert_eq!(fixture.state.players.count(), 0);
    assert_eq!(fixture.state.packet_budget.snapshot().used_bytes, 0);
    assert_eq!(
        (
            budget.reserved_bytes,
            budget.active_operations,
            budget.quarantined_operations
        ),
        (0, 0, 0)
    );
}

async fn install_quit_probe(fixture: &Fixture) {
    let module = wat::parse_str(
        r#"(module
        (import "zerqon" "event_len" (func $len (result i32)))
        (import "zerqon" "event_read" (func $read (param i32 i32) (result i32)))
        (import "zerqon_read_v1" "callback_token" (func $token (result i64)))
        (import "zerqon_read_v1" "query" (func $query (param i64 i32 i32) (result i32)))
        (import "zerqon_read_v1" "result_len" (func $size (param i64 i32) (result i32)))
        (import "zerqon_read_v1" "result_read" (func $copy (param i64 i32 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 2048) "{\22query\22:\22worlds\22}")
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (func (export "zerqon_on_event") (result i32) (local $handle i32) (local $length i32)
            i32.const 0 call $len call $read drop
            i32.const 10 i32.load8_u i32.const 112 i32.eq
            i32.const 17 i32.load8_u i32.const 113 i32.eq i32.and
            if
                call $token i32.const 2048 i32.const 18 call $query local.set $handle
                call $token local.get $handle call $size local.set $length
                call $token local.get $handle i32.const 8192 local.get $length call $copy drop
            end
            i32.const 0))"#,
    )
    .unwrap();
    // An earlier recipient exposes Quit before it could queue behind the blocked command's actor.
    drop(
        fixture
            .state
            .plugins
            .load(
                QUIT_PROBE_PLUGIN,
                &module,
                &fixture.state.lifecycle.token(),
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap(),
    );
}

async fn install_java_join_probe(fixture: &mut Fixture, provider: &Arc<ControlProbeProvider>) {
    use std::io::Write;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let directory = fixture._directory.path().join("plugins/java-plugins");
    let class = "io/github/teighto/zerqon/fixtures/NativeJoinProbe.class";
    let mut jar =
        zip::ZipWriter::new(std::fs::File::create(directory.join("native-join.jar")).unwrap());
    let options = zip::write::SimpleFileOptions::default();
    jar.start_file("META-INF/MANIFEST.MF", options).unwrap();
    jar.write_all(b"Manifest-Version: 1.0\nZerqon-Plugin: io.github.teighto.zerqon.fixtures.NativeJoinProbe\nZerqon-API: 1\n\n").unwrap();
    jar.start_file(class, options).unwrap();
    jar.write_all(
        &std::fs::read(root.join("target/java-plugins/fixtures").join(class))
            .expect("compile NativeJoinProbe with the Java fixture builder"),
    )
    .unwrap();
    jar.finish().unwrap();
    let runtime = NativeQueryRuntime::new(
        provider.clone(),
        fixture.state.native_read_budget.clone(),
        fixture.state.lifecycle.clone(),
        NativeProviderContract::future_owned(MAX_PROVIDER_WORKING_SET_BYTES).unwrap(),
    );
    let java = crate::java_plugins::JavaPlugins::spawn_with_query_runtime(
        crate::java_plugins::JavaPluginConfig {
            java: "java".into(),
            host_jar: root.join("libraries/zerqon-java-host-0.0.1.jar"),
            gson_jar: root.join("libraries/gson-2.13.2.jar"),
            extra_classpath: Vec::new(),
            directory,
            server_version: "0.0.1".into(),
            request_timeout: Duration::from_secs(2),
            startup_timeout: Duration::from_secs(15),
            queue_capacity: 4,
        },
        runtime,
        &fixture.state.lifecycle,
        &fixture.state.lifecycle.token(),
    )
    .await
    .unwrap();
    Arc::get_mut(&mut fixture.state).unwrap().java = Some(java);
    provider.join_worlds.store(true, Ordering::Release);
    provider.armed.store(true, Ordering::Release);
}

#[tokio::test]
#[ignore = "requires prepared official 26.3 data, native-read Rust WASM and compiled NativeJoinProbe Java fixture"]
async fn java_join_delivers_outbound_before_its_native_world_read_completes() {
    let mut fixture = Fixture::new().await;
    let provider = install_control_probe(&mut fixture).await;
    install_java_join_probe(&mut fixture, &provider).await;
    let release_on_unwind = ReleaseProbe(Arc::clone(&provider.gate));
    let joined = crate::managed_tasks::catch_panic(fixture.connect_with_probe(
        "CONSOLE",
        true,
        Some(Arc::clone(&provider.gate)),
    ))
    .await;
    let directory = fixture._directory.path().join("plugins/java-plugins");
    let after_join = std::fs::read_to_string(directory.join("native-join-counts"));
    let identity = std::fs::read_to_string(directory.join("native-join-identity"));
    let world = fixture
        .state
        .world
        .as_ref()
        .unwrap()
        .descriptor(&fixture.state.properties.level_name)
        .unwrap()
        .uuid;
    let mut client_identity = None;
    let mut probe_seen = false;
    let mut join_error = None;
    let cleanup = match joined {
        Ok((client, connection)) => {
            probe_seen = client.join_probe_seen;
            client_identity = fixture
                .state
                .players
                .query_snapshot(client.uuid)
                .unwrap()
                .map(|player| (client.uuid, player.id));
            finish_control_probe(&fixture, &provider, Some(connection), Some(client), false).await
        }
        Err(error) => {
            join_error = Some(error.to_string());
            finish_control_probe(&fixture, &provider, None, None, true).await
        }
    };
    drop(release_on_unwind);
    let after_quit = std::fs::read_to_string(directory.join("native-join-counts"));
    let disabled = std::fs::read_to_string(directory.join("native-join-disabled"));
    let observation = provider.observed.lock().unwrap().clone();
    assert!(
        cleanup.is_empty(),
        "cleanup failed: {cleanup:?}; join={join_error:?}"
    );
    assert!(
        join_error.is_none(),
        "Java Join connection failed: {join_error:?}"
    );
    assert!(
        probe_seen,
        "Join completed without delivering its native service probe"
    );
    assert!(
        provider.released_before_deadline.load(Ordering::Acquire),
        "Join's native request expired before the owner delivered its probe"
    );
    assert!(provider.completed.load(Ordering::Acquire));
    assert_eq!(after_join.unwrap(), "1:0");
    assert_eq!(after_quit.unwrap(), "1:1");
    assert_eq!(disabled.unwrap(), "disabled");
    let (uuid, session_id) = client_identity.unwrap();
    assert_eq!(identity.unwrap(), format!("{uuid}:{session_id}:{world}"));
    let observation = observation.unwrap();
    assert_eq!(observation.plugin, JAVA_JOIN_PROBE_PLUGIN);
    assert_eq!(
        (observation.uuid, observation.session_id),
        (uuid, session_id)
    );
    assert_eq!(fixture.state.players.count(), 0);
    assert_eq!(fixture.state.packet_budget.snapshot().used_bytes, 0);
    assert_eq!(
        fixture.state.native_read_budget.snapshot().reserved_bytes,
        0
    );
}

async fn drive_probe<T>(
    connection: &mut Connection,
    finished: &mut bool,
    client: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::select! {
        result = connection => {
            *finished = true;
            Err(format!("connection ended during control probe: {result:?}"))
        },
        result = client => result,
    }
}

#[tokio::test]
#[ignore = "requires prepared official 26.3 data and the compiled native-read Rust WASM fixture"]
async fn private_control_receipt_waits_for_the_live_owner_and_writes_during_a_pending_command() {
    use crate::player_control::{ControlOutcome, MESSAGE_RESERVATION_BYTES, SessionKey};
    const MESSAGE: &str = "Private control reached the waiting command's connection";
    let mut fixture = Fixture::new().await;
    let provider = install_control_probe(&mut fixture).await;
    let release_on_unwind = ReleaseProbe(Arc::clone(&provider.gate));
    let (mut client, mut connection) = fixture.connect("CONSOLE").await;
    drive(&mut connection, client.version()).await;
    let key = SessionKey {
        uuid: client.uuid,
        session_id: fixture
            .state
            .players
            .query_snapshot(client.uuid)
            .unwrap()
            .unwrap()
            .id,
    };
    *provider.expected.lock().unwrap() = Some((key.uuid, key.session_id));
    provider.armed.store(true, Ordering::Release);
    let mut finished = false;
    let scenario = timeout(Duration::from_secs(5), async {
        drive_probe(&mut connection, &mut finished, async {
            probe_send_command(&mut client, "rwhere").await?;
            let probe = probe_read_chat(&mut client).await?;
            if probe != CONTROL_PROBE_MESSAGE {
                return Err(format!("command did not reach its probe: {probe}"));
            }
            Ok(())
        })
        .await?;
        let sender = fixture
            .state
            .sessions
            .control(key)
            .ok_or("live control endpoint missing")?;
        let receipt = sender
            .try_system_message(
                MESSAGE,
                tokio::time::Instant::now() + Duration::from_secs(1),
                &fixture.state.lifecycle.token(),
            )
            .map_err(|error| error.to_string())?;
        let reserved_before_write = fixture
            .state
            .player_control_budget
            .snapshot()
            .reserved_bytes;
        let mut waiting = Box::pin(receipt.wait());
        let before_owner =
            std::future::poll_fn(|context| Poll::Ready(waiting.as_mut().poll(context))).await;
        let queued_only = before_owner.is_pending();
        let completed_early = match before_owner {
            Poll::Ready(outcome) => Some(outcome),
            Poll::Pending => None,
        };
        let written = drive_probe(&mut connection, &mut finished, async {
            let (outcome, text) = if let Some(outcome) = completed_early {
                (outcome, probe_read_chat(&mut client).await?)
            } else {
                let (outcome, text) = tokio::join!(&mut waiting, probe_read_chat(&mut client));
                (outcome, text?)
            };
            if text != MESSAGE {
                return Err(format!("unexpected control payload: {text}"));
            }
            match outcome {
                ControlOutcome::Written {
                    target,
                    operation_id,
                } if target == key && operation_id > 0 => Ok(outcome),
                other => Err(format!("control write did not complete: {other:?}")),
            }
        })
        .await?;
        let command_still_pending = !provider.completed.load(Ordering::Acquire)
            && fixture
                .state
                .native_read_budget
                .snapshot()
                .quarantined_operations
                == 0;
        provider.gate.release();
        drive_probe(&mut connection, &mut finished, async {
            let response = probe_read_chat(&mut client).await?;
            if !response.starts_with("CONSOLE: uuid=") {
                return Err(format!("command did not finish: {response}"));
            }
            Ok(())
        })
        .await?;
        Ok((
            queued_only,
            reserved_before_write,
            command_still_pending,
            written,
        ))
    })
    .await
    .map_err(|_| "private control exchange exceeded its watchdog".to_owned())
    .and_then(|result| result);
    let cleanup = finish_control_probe(
        &fixture,
        &provider,
        Some(connection),
        Some(client),
        finished,
    )
    .await;
    drop(release_on_unwind);
    assert!(
        cleanup.is_empty(),
        "cleanup failed: {cleanup:?}; scenario={scenario:?}"
    );
    let (queued_only, reserved_before_write, command_still_pending, written) =
        scenario.expect("private owner control failed");
    assert!(
        queued_only,
        "queue admission was incorrectly reported as Written: {written:?}"
    );
    assert_eq!(reserved_before_write, MESSAGE_RESERVATION_BYTES);
    assert!(
        command_still_pending,
        "control delivery waited for the command to finish"
    );
    assert!(provider.released_before_deadline.load(Ordering::Acquire));
    assert_eq!(
        fixture
            .state
            .player_control_budget
            .snapshot()
            .reserved_bytes,
        0
    );
    assert_eq!(
        fixture.state.native_read_budget.snapshot().reserved_bytes,
        0
    );
    assert_eq!(fixture.state.packet_budget.snapshot().used_bytes, 0);
}

enum NoticePacket {
    Chat(String),
    Bar(Uuid, i32),
}

async fn notice_packet(client: &mut Client) -> Result<NoticePacket, String> {
    loop {
        let packet = read_frame(&mut client.socket, 2_097_151, None)
            .await
            .map_err(|error| error.to_string())?;
        let mut body = packet.as_slice();
        match get_varint(&mut body).map_err(|error| error.to_string())? {
            124 => return Ok(NoticePacket::Chat(probe_chat_text(body)?)),
            9 => {
                let uuid =
                    Uuid::from_slice(take(&mut body, 16).map_err(|error| error.to_string())?)
                        .map_err(|error| error.to_string())?;
                let operation = get_varint(&mut body).map_err(|error| error.to_string())?;
                return Ok(NoticePacket::Bar(uuid, operation));
            }
            45 => {
                let mut reply = packet;
                reply[0] = 28;
                write_frame(&mut client.socket, &reply, None)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            32 => return Err("notice client was disconnected".into()),
            _ => {}
        }
    }
}

async fn notice_tick(
    client: &mut Client,
    bar: Uuid,
    messages: &mut Vec<String>,
) -> Result<(), String> {
    loop {
        match notice_packet(client).await? {
            NoticePacket::Bar(id, 2) if id == bar => return Ok(()),
            NoticePacket::Chat(message) => messages.push(message),
            _ => {}
        }
    }
}

async fn notice_message(
    client: &mut Client,
    expected: &str,
    messages: &mut Vec<String>,
) -> Result<(), String> {
    loop {
        if let NoticePacket::Chat(message) = notice_packet(client).await? {
            let found = message == expected;
            messages.push(message);
            if found {
                return Ok(());
            }
        }
    }
}

async fn notice_fence(
    client: &mut Client,
    state: &State,
    key: crate::player_control::SessionKey,
    marker: &str,
    messages: &mut Vec<String>,
) -> Result<(), String> {
    let sender = state
        .sessions
        .control(key)
        .ok_or("notice control endpoint disappeared")?;
    let receipt = sender
        .try_system_message(
            marker,
            tokio::time::Instant::now() + Duration::from_secs(3),
            &state.lifecycle.token(),
        )
        .map_err(|error| error.to_string())?;
    let reading = async {
        loop {
            if let NoticePacket::Chat(message) = notice_packet(client).await? {
                if message == marker {
                    return Ok::<_, String>(());
                }
                messages.push(message);
            }
        }
    };
    let (written, observed) = tokio::join!(receipt.wait(), reading);
    observed?;
    match written {
        crate::player_control::ControlOutcome::Written {
            target,
            operation_id,
        } if target == key && operation_id > 0 => Ok(()),
        other => Err(format!("notice fence did not finish writing: {other:?}")),
    }
}

#[tokio::test]
#[ignore = "requires prepared official 26.3 data"]
async fn update_notice_retries_full_control_budget_and_announces_each_version_once() {
    use crate::player_control::{self, ControlBudget, MESSAGE_RESERVATION_BYTES, SessionKey};
    let first = crate::updates::UpdateAvailable {
        version: "0.0.2".into(),
        url: "https://github.com/teighto/Zerqon-Core/releases/tag/v0.0.2".into(),
    };
    let second = crate::updates::UpdateAvailable {
        version: "0.0.3".into(),
        url: "https://github.com/teighto/Zerqon-Core/releases/tag/v0.0.3".into(),
    };
    const FIRST: &str = "A new Zerqon version 0.0.2 is available. Download: https://github.com/teighto/Zerqon-Core/releases/tag/v0.0.2";
    const SECOND: &str = "A new Zerqon version 0.0.3 is available. Download: https://github.com/teighto/Zerqon-Core/releases/tag/v0.0.3";
    let mut fixture = Fixture::new().await;
    let (updates, receiver) = watch::channel(None);
    {
        let state = Arc::get_mut(&mut fixture.state).unwrap();
        state.updates = Some(receiver);
        state.player_control_budget = ControlBudget::new(2 * MESSAGE_RESERVATION_BYTES).unwrap();
    }
    let (mut client, mut connection) = fixture.connect("CONSOLE").await;
    drive(&mut connection, client.version()).await;
    assert!(fixture.state.update_operator("CONSOLE", true).unwrap());
    let key = SessionKey {
        uuid: client.uuid,
        session_id: fixture
            .state
            .players
            .query_snapshot(client.uuid)
            .unwrap()
            .unwrap()
            .id,
    };
    let slots_before = fixture.state.slots.available_permits();
    let (holder, mut holding, transport) = player_control::channel(
        SessionKey {
            uuid: Uuid::from_u128(2),
            session_id: 1,
        },
        fixture.state.player_control_budget.clone(),
        fixture.state.lifecycle.clone(),
        2,
    )
    .unwrap();
    let mut receipts = Vec::new();
    for _ in 0..2 {
        receipts.push(
            holder
                .try_system_message(
                    "held",
                    tokio::time::Instant::now() + Duration::from_secs(30),
                    &fixture.state.lifecycle.token(),
                )
                .unwrap(),
        );
    }
    let before = fixture.state.player_control_budget.snapshot();
    let mut messages = Vec::new();
    let mut busy = None;
    let mut finished = false;
    let scenario = timeout(
        Duration::from_secs(12),
        drive_probe(&mut connection, &mut finished, async {
            probe_send_command(&mut client, "tpsbar").await?;
            let bar = loop {
                match notice_packet(&mut client).await? {
                    NoticePacket::Bar(uuid, 0) => break uuid,
                    NoticePacket::Chat(message) => {
                        return Err(format!("unexpected setup chat: {message}"));
                    }
                    _ => {}
                }
            };
            updates.send_replace(Some(first));
            loop {
                notice_tick(&mut client, bar, &mut messages).await?;
                if !messages.is_empty() {
                    return Err("notice bypassed the full control budget".into());
                }
                let budget = fixture.state.player_control_budget.snapshot();
                if budget.rejections > before.rejections {
                    busy = Some(budget);
                    break;
                }
            }
            receipts.clear();
            holding.close();
            notice_message(&mut client, FIRST, &mut messages).await?;
            notice_tick(&mut client, bar, &mut messages).await?;
            notice_fence(
                &mut client,
                &fixture.state,
                key,
                "notice fence 2",
                &mut messages,
            )
            .await?;
            updates.send_replace(Some(second));
            notice_message(&mut client, SECOND, &mut messages).await?;
            notice_tick(&mut client, bar, &mut messages).await?;
            notice_fence(
                &mut client,
                &fixture.state,
                key,
                "notice fence 3",
                &mut messages,
            )
            .await?;
            Ok(())
        }),
    )
    .await
    .map_err(|_| "notice scenario exceeded its watchdog".to_owned())
    .and_then(|result| result);
    let slots_after = fixture.state.slots.available_permits();
    receipts.clear();
    holding.close();
    drop(transport);
    holder.drain().await;
    drop(holder);
    drop(holding);
    let cleanup = finish_wire_fixture(&fixture, Some(connection), Some(client), finished).await;
    assert!(
        cleanup.is_empty(),
        "cleanup failed: {cleanup:?}; scenario={scenario:?}"
    );
    assert!(scenario.is_ok(), "{scenario:?}; messages={messages:?}");
    assert_eq!(before.reserved_bytes, before.limit_bytes);
    let busy = busy.unwrap();
    assert_eq!(busy.reserved_bytes, busy.limit_bytes);
    assert!(busy.rejections > before.rejections);
    assert_eq!(messages, [FIRST, SECOND]);
    assert_eq!(slots_after, slots_before);
    assert_eq!(
        fixture.state.slots.available_permits(),
        fixture.state.properties.max_players
    );
    assert_eq!(fixture.state.players.count(), 0);
    assert_eq!(fixture.state.packet_budget.snapshot().used_bytes, 0);
    assert_eq!(
        fixture
            .state
            .player_control_budget
            .snapshot()
            .reserved_bytes,
        0
    );
}

#[tokio::test]
#[ignore = "requires prepared official 26.3 data and the compiled native-read Rust WASM fixture"]
async fn actual_keepalive_reply_is_processed_while_a_rust_command_waits() {
    let mut fixture = Fixture::new().await;
    let provider = install_control_probe(&mut fixture).await;
    let release_on_unwind = ReleaseProbe(Arc::clone(&provider.gate));
    let (mut client, mut connection) = fixture.connect("CONSOLE").await;
    drive(&mut connection, client.version()).await;
    let uuid = client.uuid;
    let session_id = fixture
        .state
        .players
        .query_snapshot(uuid)
        .unwrap()
        .unwrap()
        .id;
    *provider.expected.lock().unwrap() = Some((uuid, session_id));
    let mut finished = false;
    let scenario = timeout(
        Duration::from_secs(20),
        drive_probe(&mut connection, &mut finished, async {
            probe_send_command(&mut client, "tpsbar").await?;
            let bar = loop {
                if let NoticePacket::Bar(id, 0) = notice_packet(&mut client).await? {
                    break id;
                }
            };
            let mut response = loop {
                let packet = read_frame(&mut client.socket, 2_097_151, None)
                    .await
                    .map_err(|error| error.to_string())?;
                if packet.first() == Some(&45) {
                    if packet.len() != 9 {
                        return Err("invalid actual keepalive request".into());
                    }
                    break packet;
                }
            };
            let mut unexpected_chats = Vec::new();
            notice_tick(&mut client, bar, &mut unexpected_chats).await?;
            notice_tick(&mut client, bar, &mut unexpected_chats).await?;
            if !unexpected_chats.is_empty() {
                return Err(format!("unexpected idle chat: {unexpected_chats:?}"));
            }
            provider.armed.store(true, Ordering::Release);
            let command = encoded_command("rwhere");
            write_frame(&mut client.socket, &command, None)
                .await
                .map_err(|error| error.to_string())?;
            let probe = probe_read_chat(&mut client).await?;
            if probe != CONTROL_PROBE_MESSAGE {
                return Err(format!("command probe missing: {probe}"));
            }
            response[0] = 28;
            write_frame(&mut client.socket, &response, None)
                .await
                .map_err(|error| error.to_string())?;
            let mut marker = vec![0; FRAME_BYTES];
            marker[0] = 127;
            write_frame(&mut client.socket, &marker, None)
                .await
                .map_err(|error| error.to_string())?;
            await_pending_command_budget(
                &fixture.state,
                &provider,
                command.len(),
                command.len() + marker.len(),
            )
            .await?;
            fixture
                .state
                .players
                .tracking_tick()
                .map_err(|error| error.to_string())?;
            let latency = loop {
                let packet = read_frame(&mut client.socket, 2_097_151, None)
                    .await
                    .map_err(|error| error.to_string())?;
                let mut body = packet.as_slice();
                match get_varint(&mut body).map_err(|error| error.to_string())? {
                    71 => {
                        let flags = take(&mut body, 1).map_err(|error| error.to_string())?[0];
                        if flags != 0x10 {
                            continue;
                        }
                        if get_varint(&mut body).map_err(|error| error.to_string())? != 1 {
                            return Err("unexpected latency entry count".into());
                        }
                        let target = Uuid::from_slice(
                            take(&mut body, 16).map_err(|error| error.to_string())?,
                        )
                        .map_err(|error| error.to_string())?;
                        let latency = get_varint(&mut body).map_err(|error| error.to_string())?;
                        if target != uuid || !body.is_empty() {
                            return Err("latency update belonged to another session".into());
                        }
                        break latency;
                    }
                    124 => {
                        return Err(format!(
                            "command ended before latency update: {}",
                            probe_chat_text(body)?
                        ));
                    }
                    32 => return Err("keepalive reply disconnected its client".into()),
                    _ => {}
                }
            };
            let pending = !provider.completed.load(Ordering::Acquire)
                && fixture
                    .state
                    .native_read_budget
                    .snapshot()
                    .quarantined_operations
                    == 0;
            provider.gate.release();
            let reply = probe_read_chat(&mut client).await?;
            if !reply.starts_with("CONSOLE: uuid=") {
                return Err(format!("command failed after keepalive: {reply}"));
            }
            probe_send_command(&mut client, "hello").await?;
            let hello = probe_read_chat(&mut client).await?;
            if !hello.starts_with("Hello, CONSOLE!") {
                return Err(format!("plugin did not remain healthy: {hello}"));
            }
            Ok((latency, pending))
        }),
    )
    .await
    .map_err(|_| "actual keepalive scenario exceeded its watchdog".to_owned())
    .and_then(|result| result);
    let cleanup = finish_control_probe(
        &fixture,
        &provider,
        Some(connection),
        Some(client),
        finished,
    )
    .await;
    drop(release_on_unwind);
    assert!(
        cleanup.is_empty(),
        "cleanup failed: {cleanup:?}; scenario={scenario:?}"
    );
    let (latency, pending) =
        scenario.expect("keepalive did not progress through the pending command");
    assert!(
        latency > 0,
        "held actual keepalive did not update player latency"
    );
    assert!(
        pending,
        "keepalive was only processed after the command settled"
    );
    assert!(provider.released_before_deadline.load(Ordering::Acquire));
    assert_eq!(fixture.state.packet_budget.snapshot().used_bytes, 0);
    assert_eq!(
        fixture.state.native_read_budget.snapshot().reserved_bytes,
        0
    );
}

#[tokio::test]
#[ignore = "requires prepared official 26.3 data and the compiled native-read Rust WASM fixture"]
async fn fifth_queued_command_closes_the_connection_and_settles_accepted_work() {
    deferred_saturation(true).await;
}

#[tokio::test]
#[ignore = "requires prepared official 26.3 data and the compiled native-read Rust WASM fixture"]
async fn sixty_fifth_queued_gameplay_frame_closes_the_connection_and_reclaims_leases() {
    deferred_saturation(false).await;
}

async fn deferred_saturation(commands: bool) {
    let mut fixture = Fixture::new().await;
    let provider = install_control_probe(&mut fixture).await;
    install_quit_probe(&fixture).await;
    let release_on_unwind = ReleaseProbe(Arc::clone(&provider.gate));
    let (mut client, mut connection) = fixture.connect("CONSOLE").await;
    drive(&mut connection, client.version()).await;
    drive(&mut connection, wait_used(&fixture.state, 0)).await;
    let uuid = client.uuid;
    let session_id = fixture
        .state
        .players
        .query_snapshot(uuid)
        .unwrap()
        .unwrap()
        .id;
    *provider.expected.lock().unwrap() = Some((uuid, session_id));
    provider.armed.store(true, Ordering::Release);
    let command = encoded_command("rwhere");
    let (queued, maximum) = if commands {
        (encoded_command("hello"), 4)
    } else {
        (
            movement_packet([
                client.position[0] + 0.25,
                client.position[1],
                client.position[2],
            ]),
            64,
        )
    };
    let mut marker = vec![0; FRAME_BYTES];
    marker[0] = 127;
    let mut accepted_bytes = None;
    let mut ended = None;
    let scenario = timeout(Duration::from_secs(5), async {
        tokio::select! {
            biased;
            result = async {
                write_frame(&mut client.socket, &command, None).await.map_err(|error| error.to_string())?;
                let probe = probe_read_chat(&mut client).await?;
                if probe != CONTROL_PROBE_MESSAGE { return Err(format!("command probe missing: {probe}")); }
                for _ in 0..maximum {
                    write_frame(&mut client.socket, &queued, None).await.map_err(|error| error.to_string())?;
                }
                write_frame(&mut client.socket, &marker, None).await.map_err(|error| error.to_string())?;
                await_pending_command_budget(
                    &fixture.state, &provider, command.len() + maximum * queued.len(),
                    command.len() + maximum * queued.len() + marker.len(),
                ).await?;
                accepted_bytes = Some(fixture.state.packet_budget.snapshot().used_bytes);
                write_frame(&mut client.socket, &queued, None).await.map_err(|error| error.to_string())?;
                provider.quit_seen.acquire().await.map_err(|error| error.to_string())?.forget();
                Ok(())
            } => result,
            result = &mut connection => {
                ended = Some(result);
                Err("connection ended before overflow Quit was observed".into())
            },
        }
    }).await.map_err(|_| "deferred saturation exceeded its watchdog".to_owned()).and_then(|result| result);
    let terminal = if let Some(result) = ended {
        Some(result)
    } else {
        timeout(Duration::from_secs(5), &mut connection).await.ok()
    };
    let rejected_at_bound = matches!(&terminal, Some(Err(error)) if error.to_string() == "deferred gameplay queue exceeded");
    let quit_during_command = provider.quit_during_command.load(Ordering::Acquire);
    let quit_count = provider.quit_count.load(Ordering::Acquire);
    let at_quit = *provider.quit_packet_bytes.lock().unwrap();
    drop(connection);
    drop(client);
    let cleanup = finish_control_probe(&fixture, &provider, None, None, true).await;
    drop(release_on_unwind);
    assert!(
        cleanup.is_empty(),
        "cleanup failed: {cleanup:?}; scenario={scenario:?}"
    );
    assert!(scenario.is_ok(), "{scenario:?}; terminal={terminal:?}");
    assert_eq!(accepted_bytes, Some(command.len() + maximum * queued.len()));
    assert!(
        rejected_at_bound,
        "overflow did not produce the bounded queue refusal: {terminal:?}"
    );
    assert_eq!(quit_count, 1);
    assert!(
        !quit_during_command,
        "overflow sent Quit before accepted work settled"
    );
    assert_eq!(at_quit, Some(0));
    assert_eq!(fixture.state.players.count(), 0);
    assert_eq!(fixture.state.packet_budget.snapshot().used_bytes, 0);
    let budget = fixture.state.native_read_budget.snapshot();
    assert_eq!(
        (
            budget.reserved_bytes,
            budget.active_operations,
            budget.quarantined_operations
        ),
        (0, 0, 0)
    );
}

#[tokio::test]
#[ignore = "requires prepared official 26.3 data and the compiled native-read Rust WASM fixture"]
async fn eof_releases_queued_frames_but_keeps_accepted_command_until_quit_can_run() {
    let mut fixture = Fixture::new().await;
    let provider = install_control_probe(&mut fixture).await;
    install_quit_probe(&fixture).await;
    let release_on_unwind = ReleaseProbe(Arc::clone(&provider.gate));
    let (mut client, mut connection) = fixture.connect("CONSOLE").await;
    drive(&mut connection, client.version()).await;
    drive(&mut connection, wait_used(&fixture.state, 0)).await;
    let uuid = client.uuid;
    let session_id = fixture
        .state
        .players
        .query_snapshot(uuid)
        .unwrap()
        .unwrap()
        .id;
    *provider.expected.lock().unwrap() = Some((uuid, session_id));
    provider.armed.store(true, Ordering::Release);
    let command = encoded_command("rwhere");
    let queued_command = encoded_command("hello");
    let movement = movement_packet([
        client.position[0] + 0.25,
        client.position[1],
        client.position[2],
    ]);
    let mut marker = vec![0; FRAME_BYTES];
    marker[0] = 127;
    let mut queued_bytes = None;
    let scenario = timeout(Duration::from_secs(5), async {
        tokio::select! {
            biased;
            result = async {
                write_frame(&mut client.socket, &command, None).await.map_err(|error| error.to_string())?;
                let probe = probe_read_chat(&mut client).await?;
                if probe != CONTROL_PROBE_MESSAGE { return Err(format!("unexpected command probe: {probe}")); }
                write_frame(&mut client.socket, &movement, None).await.map_err(|error| error.to_string())?;
                write_frame(&mut client.socket, &queued_command, None).await.map_err(|error| error.to_string())?;
                write_frame(&mut client.socket, &marker, None).await.map_err(|error| error.to_string())?;
                await_pending_command_budget(
                    &fixture.state, &provider,
                    command.len() + movement.len() + queued_command.len(),
                    command.len() + movement.len() + queued_command.len() + marker.len(),
                ).await?;
                queued_bytes = Some(fixture.state.packet_budget.snapshot().used_bytes);
                client.socket.shutdown().await.map_err(|error| error.to_string())?;
                provider.quit_seen.acquire().await.map_err(|error| error.to_string())?.forget();
                Ok(())
            } => result,
            result = &mut connection => Err(format!("connection ended before Quit observation: {result:?}")),
        }
    }).await.map_err(|_| "EOF/command-settlement scenario exceeded its watchdog".to_owned()).and_then(|result| result);
    let quit_count = provider.quit_count.load(Ordering::Acquire);
    let quit_during_command = provider.quit_during_command.load(Ordering::Acquire);
    let packet_bytes_at_quit = *provider.quit_packet_bytes.lock().unwrap();
    let provider_still_owned = fixture
        .state
        .native_read_budget
        .snapshot()
        .quarantined_operations;
    drop(connection);
    drop(client);
    let cleanup = finish_control_probe(&fixture, &provider, None, None, true).await;
    drop(release_on_unwind);
    assert!(
        cleanup.is_empty(),
        "cleanup failed: {cleanup:?}; scenario={scenario:?}"
    );
    assert!(scenario.is_ok(), "{scenario:?}");
    assert_eq!(
        queued_bytes,
        Some(command.len() + movement.len() + queued_command.len())
    );
    assert_eq!(quit_count, 1);
    assert!(
        !quit_during_command,
        "Quit began before the accepted command settled"
    );
    assert_eq!(
        packet_bytes_at_quit,
        Some(0),
        "command or queued input survived into Quit"
    );
    assert_eq!(
        provider_still_owned, 1,
        "late provider work lost its quarantine before release"
    );
    assert!(provider.completed.load(Ordering::Acquire));
    assert_eq!(fixture.state.players.count(), 0);
    assert_eq!(fixture.state.packet_budget.snapshot().used_bytes, 0);
    let budget = fixture.state.native_read_budget.snapshot();
    assert_eq!(
        (
            budget.reserved_bytes,
            budget.active_operations,
            budget.quarantined_operations
        ),
        (0, 0, 0)
    );
}
