use crate::{
    config::{Properties, Settings},
    runtime::State,
};
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc, task::Poll, time::Duration};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::watch,
    time::timeout,
};
use uuid::Uuid;
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
        let socket = TcpStream::connect(self.listener.local_addr().unwrap())
            .await
            .unwrap();
        socket.set_nodelay(true).unwrap();
        let (server, _) = self.listener.accept().await.unwrap();
        let mut connection: Connection = Box::pin(super::handle(server, Arc::clone(&self.state)));
        let client = timeout(
            Duration::from_secs(20),
            drive(&mut connection, Client::join(socket, name)),
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
}

impl Client {
    async fn join(mut socket: TcpStream, name: &str) -> Self {
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
                    client.send(&ack).await;
                    teleported = true;
                }
                46 => chunks += 1,
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
        assert_eq!(fixture.state.players.count(), 0);
        client.expect_closed().await;
        drop(blocker);
    })
    .await
    .expect("TCP handler cancellation exceeded its five-second window");
    fixture.state.shutdown.send_replace(true);
    fixture.state.profiler.shutdown().await.unwrap();
}
