use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use zerqon_protocol::codec::{get_varint, put_string, put_varint, read_frame, write_frame};

struct Server {
    child: Child,
    address: String,
    _directory: tempfile::TempDir,
}

impl Server {
    fn start(status_only: bool, online: bool, max_connections: usize, idle_seconds: u64) -> Self {
        Self::start_with_ops(status_only, online, max_connections, idle_seconds, None)
    }
    fn start_with_ops(
        status_only: bool,
        online: bool,
        max_connections: usize,
        idle_seconds: u64,
        operators: Option<serde_json::Value>,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let executable = env!("CARGO_BIN_EXE_zerqon");
        assert!(
            Command::new(executable)
                .arg("--init")
                .arg("--directory")
                .arg(directory.path())
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(directory.path().join("eula.txt"), "eula=true\n").unwrap();
        std::fs::write(directory.path().join("server.properties"), format!("server-ip=127.0.0.1\nserver-port=0\nmax-players={max_connections}\nonline-mode={online}\nview-distance=1\nnetwork-compression-threshold=-1\n")).unwrap();
        std::fs::write(directory.path().join("server.settings"), format!("max_connections={max_connections}\nconnection_timeout_seconds={idle_seconds}\nnetwork_threads=2\nworld_threads=1\nupdate_check=false\n")).unwrap();
        if let Some(operators) = operators {
            std::fs::write(
                directory.path().join("ops.json"),
                serde_json::to_vec(&operators).unwrap(),
            )
            .unwrap();
        }
        let mut command = Command::new(executable);
        command
            .env("RUST_LOG", "info")
            .arg("--directory")
            .arg(directory.path())
            .arg("--no-color")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if status_only {
            command.arg("--status-only");
        } else {
            let data = Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/vanilla/26.3");
            assert!(
                data.join("prepared.json").exists(),
                "run scripts/prepare-data.py"
            );
            command.arg("--data").arg(data);
        }
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some((_, address)) = line.split_once("Listening on ") {
                    let _ = send.send(address.trim().to_owned());
                }
            }
        });
        let address = match receive.recv_timeout(Duration::from_secs(15)) {
            Ok(address) => address,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("server failed to listen: {error}");
            }
        };
        Self {
            child,
            address,
            _directory: directory,
        }
    }

    async fn connect(&self) -> TcpStream {
        timeout(Duration::from_secs(3), TcpStream::connect(&self.address))
            .await
            .unwrap()
            .unwrap()
    }

    async fn stop(&mut self) {
        writeln!(self.child.stdin.as_mut().unwrap(), "stop").unwrap();
        let status = timeout(Duration::from_secs(10), async {
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("graceful shutdown must finish");
        assert!(status.success());
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn handshake(intent: u8, host: &str) -> Vec<u8> {
    let mut packet = vec![0];
    put_varint(777, &mut packet);
    put_string(host, &mut packet);
    packet.extend_from_slice(&25565u16.to_be_bytes());
    packet.push(intent);
    packet
}

async fn status(socket: &mut TcpStream) -> Vec<u8> {
    write_frame(socket, &handshake(1, "localhost"), None)
        .await
        .unwrap();
    write_frame(socket, &[0], None).await.unwrap();
    timeout(Duration::from_secs(3), read_frame(socket, 65_536, None))
        .await
        .unwrap()
        .unwrap()
}

async fn assert_closed(socket: &mut TcpStream) {
    let mut byte = [0];
    let result = timeout(Duration::from_secs(3), socket.read(&mut byte))
        .await
        .expect("rejected connection must close");
    assert!(
        matches!(result, Ok(0) | Err(_)),
        "rejected packet unexpectedly produced data: {result:?}"
    );
}

#[tokio::test]
async fn rejects_oversized_and_malformed_handshakes_without_waiting_for_body() {
    timeout(Duration::from_secs(20), async {
        let mut server = Server::start(true, false, 4, 3);
        for prefix in [&[0x81, 0x80, 0x04][..], &[0x80, 0x80, 0x80], &[0]] {
            let mut socket = server.connect().await;
            socket.write_all(prefix).await.unwrap();
            assert_closed(&mut socket).await;
        }
        let mut trailing = handshake(1, "localhost");
        trailing.push(0);
        for packet in [
            handshake(1, &"x".repeat(256)),
            trailing,
            handshake(4, "localhost"),
        ] {
            let mut socket = server.connect().await;
            write_frame(&mut socket, &packet, None).await.unwrap();
            assert_closed(&mut socket).await;
        }
        let mut valid = server.connect().await;
        assert_eq!(status(&mut valid).await[0], 0);
        drop(valid);
        server.stop().await;
    })
    .await
    .expect("security scenario timed out");
}

#[tokio::test]
async fn caps_status_connections_and_reclaims_idle_slots() {
    timeout(Duration::from_secs(20), async {
        let mut server = Server::start(true, false, 1, 1);
        let mut first = server.connect().await;
        assert_eq!(status(&mut first).await[0], 0);
        let mut rejected = server.connect().await;
        assert_closed(&mut rejected).await;
        assert_closed(&mut first).await;
        let mut replacement = server.connect().await;
        assert_eq!(status(&mut replacement).await[0], 0);
        drop(replacement);
        server.stop().await;
    })
    .await
    .expect("capacity scenario timed out");
}

#[tokio::test]
async fn operator_persistence_failure_does_not_stop_the_server() {
    timeout(Duration::from_secs(20), async {
        let mut server = Server::start(true, false, 4, 3);
        std::fs::create_dir(server._directory.path().join("ops.json")).unwrap();
        writeln!(
            server.child.stdin.as_mut().unwrap(),
            "deop 00000000-0000-0000-0000-000000000001"
        )
        .unwrap();
        timeout(Duration::from_secs(3), async {
            loop {
                let log = std::fs::read_to_string(server._directory.path().join("logs/latest.log"))
                    .unwrap();
                if log.contains("Operator update failed") {
                    break;
                }
                assert!(
                    server.child.try_wait().unwrap().is_none(),
                    "failed operator save stopped the server"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("operator failure must be reported");
        let mut socket = server.connect().await;
        assert_eq!(status(&mut socket).await[0], 0);
        assert!(server._directory.path().join("ops.json").is_dir());
        drop(socket);
        server.stop().await;
    })
    .await
    .expect("operator persistence scenario timed out");
}

async fn login_start(socket: &mut TcpStream, name: &str) {
    write_frame(socket, &handshake(2, "localhost"), None)
        .await
        .unwrap();
    let mut hello = vec![0];
    put_string(name, &mut hello);
    hello.extend_from_slice(&[0; 16]);
    write_frame(socket, &hello, None).await.unwrap();
}

async fn offline_join(socket: &mut TcpStream, name: &str) {
    offline_join_claimed(socket, name, uuid::Uuid::nil()).await;
}

async fn offline_join_claimed(
    socket: &mut TcpStream,
    name: &str,
    claimed: uuid::Uuid,
) -> uuid::Uuid {
    write_frame(socket, &handshake(2, "localhost"), None)
        .await
        .unwrap();
    let mut hello = vec![0];
    put_string(name, &mut hello);
    hello.extend_from_slice(claimed.as_bytes());
    write_frame(socket, &hello, None).await.unwrap();
    let success = read_frame(socket, 65_536, None).await.unwrap();
    assert_eq!(success[0], 2);
    let actual = uuid::Uuid::from_slice(&success[1..17]).unwrap();
    write_frame(socket, &[3], None).await.unwrap();
    let mut known = read_frame(socket, 65_536, None).await.unwrap();
    assert_eq!(known[0], 15);
    known[0] = 7;
    write_frame(socket, &known, None).await.unwrap();
    loop {
        let packet = read_frame(socket, 2_097_151, None).await.unwrap();
        if packet == [3] {
            break;
        }
    }
    write_frame(socket, &[3], None).await.unwrap();
    loop {
        let packet = read_frame(socket, 2_097_151, None).await.unwrap();
        match packet[0] {
            73 => {
                let mut view = &packet[1..];
                let teleport = get_varint(&mut view).unwrap();
                let mut answer = vec![0];
                put_varint(teleport, &mut answer);
                write_frame(socket, &answer, None).await.unwrap();
            }
            11 => break,
            _ => {}
        }
    }
    actual
}

async fn command_reply(socket: &mut TcpStream, command: &str) -> Vec<u8> {
    let mut packet = vec![7];
    put_string(command, &mut packet);
    write_frame(socket, &packet, None).await.unwrap();
    timeout(Duration::from_secs(4), async {
        loop {
            let packet = read_frame(socket, 2_097_151, None).await.unwrap();
            if packet[0] == 124 {
                return packet;
            }
        }
    })
    .await
    .unwrap()
}
fn reply_contains(reply: &[u8], text: &str) -> bool {
    reply
        .windows(text.len())
        .any(|window| window == text.as_bytes())
}

#[tokio::test]
#[ignore = "Requires verified official 26.3 cache; real UUID authorization protocol regression"]
async fn claimed_uuid_and_reused_name_cannot_escalate_but_authoritative_uuid_retains_rights() {
    timeout(Duration::from_secs(40),async{
        let owner=uuid::Uuid::parse_str("b50ad385-829d-3141-a216-7e7d7539ba7f").unwrap();
        let operators=serde_json::json!([{"uuid":owner,"name":"FormerName","level":4,"bypassesPlayerLimit":false}]);
        let mut server=Server::start_with_ops(false,false,4,10,Some(operators));
        let mut impostor=server.connect().await;let actual=offline_join_claimed(&mut impostor,"FormerName",owner).await;assert_ne!(actual,owner);
        assert!(reply_contains(&command_reply(&mut impostor,"stop").await,"Permission denied"));assert!(server.child.try_wait().unwrap().is_none());
        let mut real_owner=server.connect().await;assert_eq!(offline_join_claimed(&mut real_owner,"Notch",uuid::Uuid::from_u128(999)).await,owner);
        assert!(reply_contains(&command_reply(&mut real_owner,"setblock 0 63 0 0").await,"Block set at"));
        drop(real_owner);drop(impostor);server.stop().await;
    }).await.expect("UUID authorization scenario timed out");
}

#[tokio::test]
#[ignore = "Requires verified official 26.3 cache; real level-two operator protocol regression"]
async fn level_two_operator_cannot_stop_or_profile_but_can_change_blocks() {
    timeout(Duration::from_secs(35),async{
        let owner=uuid::Uuid::parse_str("fcba7fd2-f61f-3061-a8dc-debd0a29b506").unwrap();
        let operators=serde_json::json!([{"uuid":owner,"name":"UnrelatedName","level":2,"bypassesPlayerLimit":false}]);
        let mut server=Server::start_with_ops(false,false,3,10,Some(operators));let mut player=server.connect().await;assert_eq!(offline_join_claimed(&mut player,"LevelTwo",uuid::Uuid::nil()).await,owner);
        for command in ["stop","spark profiler start","spark profiler stop"]{assert!(reply_contains(&command_reply(&mut player,command).await,"Permission denied"));assert!(server.child.try_wait().unwrap().is_none());}
        assert!(reply_contains(&command_reply(&mut player,"setblock 0 63 0 0").await,"Block set at"));drop(player);server.stop().await;
    }).await.expect("operator level scenario timed out");
}

#[tokio::test]
#[ignore = "Requires verified official 26.3 cache; console grants require a connected known UUID"]
async fn console_grants_connected_uuid_and_revokes_by_metadata_or_uuid() {
    timeout(Duration::from_secs(35), async {
        let mut server = Server::start(false, false, 4, 10);
        let path = server._directory.path().join("ops.json");
        writeln!(server.child.stdin.as_mut().unwrap(), "op UnknownOffline").unwrap();
        timeout(Duration::from_secs(3), async {
            loop {
                let log = std::fs::read_to_string(server._directory.path().join("logs/latest.log"))
                    .unwrap();
                if log.contains("must be connected") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!path.exists());
        let mut player = server.connect().await;
        let actual =
            offline_join_claimed(&mut player, "GuestAudit", uuid::Uuid::from_u128(1)).await;
        for revoke in ["guestaudit".to_owned(), actual.to_string()] {
            writeln!(server.child.stdin.as_mut().unwrap(), "op GuestAudit").unwrap();
            timeout(Duration::from_secs(3), async {
                loop {
                    if let Ok(bytes) = std::fs::read(&path) {
                        let stored: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                        if stored.as_array().is_some_and(|entries| {
                            entries
                                .iter()
                                .any(|entry| entry["uuid"] == actual.to_string())
                        }) {
                            break;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(reply_contains(
                &command_reply(&mut player, "setblock 0 63 0 0").await,
                "Block set at"
            ));
            writeln!(server.child.stdin.as_mut().unwrap(), "deop {revoke}").unwrap();
            timeout(Duration::from_secs(3), async {
                loop {
                    let stored: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                    if stored == serde_json::json!([]) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(reply_contains(
                &command_reply(&mut player, "setblock 0 63 0 0").await,
                "Permission denied"
            ));
        }
        drop(player);
        server.stop().await;
    })
    .await
    .expect("console UUID operator scenario timed out");
}

#[tokio::test]
#[ignore = "requires official 26.3 reports: python scripts/prepare-data.py"]
async fn player_named_console_cannot_execute_console_only_commands() {
    timeout(Duration::from_secs(30), async {
        let mut server = Server::start(false, false, 4, 5);
        let mut player = server.connect().await;
        offline_join(&mut player, "CONSOLE").await;
        for command in ["setblock 0 63 0 0", "stop"] {
            let mut packet = vec![7];
            put_string(command, &mut packet);
            write_frame(&mut player, &packet, None).await.unwrap();
            let reply = timeout(Duration::from_secs(3), async {
                loop {
                    let packet = read_frame(&mut player, 65_536, None).await.unwrap();
                    if packet[0] == 124 {
                        break packet;
                    }
                }
            })
            .await
            .expect("permission response");
            assert!(
                reply
                    .windows(b"Permission denied".len())
                    .any(|bytes| bytes == b"Permission denied"),
                "non-operator command was accepted"
            );
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "player stopped the server"
            );
        }
        drop(player);
        server.stop().await;
    })
    .await
    .expect("permission scenario timed out");
}

#[tokio::test]
#[ignore = "requires official 26.3 reports: python scripts/prepare-data.py"]
async fn malformed_encryption_response_never_creates_a_player_session() {
    timeout(Duration::from_secs(30), async {
        let mut server = Server::start(false, true, 4, 5);
        let mut player = server.connect().await;
        login_start(&mut player, "InvalidRSA").await;
        let request = read_frame(&mut player, 65_536, None).await.unwrap();
        assert_eq!(request[0], 1);
        let mut response = vec![1];
        put_varint(256, &mut response);
        response.extend_from_slice(&[0; 256]);
        put_varint(256, &mut response);
        response.extend_from_slice(&[0; 256]);
        write_frame(&mut player, &response, None).await.unwrap();
        assert_closed(&mut player).await;
        let mut probe = server.connect().await;
        let status = status(&mut probe).await;
        assert!(
            status
                .windows(b"\"online\":0".len())
                .any(|bytes| bytes == b"\"online\":0")
        );
        drop(probe);
        server.stop().await;
    })
    .await
    .expect("encryption scenario timed out");
}
