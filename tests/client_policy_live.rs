use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::{net::TcpStream, time::timeout};
use zerqon_protocol::codec::{
    get_string, get_varint, put_string, put_varint, read_frame, take, write_frame,
};

struct Server {
    child: Child,
    address: String,
    _directory: tempfile::TempDir,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Server {
    fn start(java: bool, horizon: bool, timeout_seconds: u64) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let binary = env!("CARGO_BIN_EXE_zerqon");
        assert!(
            Command::new(binary)
                .arg("--init")
                .arg("--directory")
                .arg(directory.path())
                .output()
                .unwrap()
                .status
                .success()
        );
        std::fs::write(directory.path().join("eula.txt"), "eula=true\n").unwrap();
        std::fs::write(directory.path().join("server.properties"), "server-ip=127.0.0.1\nserver-port=0\nonline-mode=false\nview-distance=1\nnetwork-compression-threshold=-1\n").unwrap();
        std::fs::write(directory.path().join("server.settings"), format!("network_threads=2\nworld_threads=1\njava_plugins=false\nupdate_check=false\nspawn_mobs=false\nallow_java_clients={java}\nallow_horizon_clients={horizon}\nconnection_timeout_seconds={timeout_seconds}\n")).unwrap();
        let mut child = Command::new(binary)
            .arg("--directory")
            .arg(directory.path())
            .arg("--data")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/vanilla/26.3"))
            .arg("--no-color")
            .env("RUST_LOG", "info")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some((_, address)) = line.split_once("Listening on ") {
                    let _ = send.send(address.trim().to_owned());
                }
            }
        });
        let address = match receive.recv_timeout(Duration::from_secs(20)) {
            Ok(address) => address,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("server startup: {error}");
            }
        };
        Self {
            child,
            address,
            _directory: directory,
        }
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
        .unwrap();
        assert!(status.success());
    }
    async fn status(&self) {
        let mut socket = connect(&self.address, 1).await;
        write_frame(&mut socket, &[0], None).await.unwrap();
        let packet = receive(&mut socket).await;
        let mut body = packet.as_slice();
        assert_eq!(get_varint(&mut body).unwrap(), 0);
        let status: serde_json::Value =
            serde_json::from_str(get_string(&mut body, 32767).unwrap()).unwrap();
        assert_eq!(status["version"]["protocol"], 777);
        assert!(body.is_empty());
        let ping = [1, 0, 0, 0, 0, 0, 0, 0, 42];
        write_frame(&mut socket, &ping, None).await.unwrap();
        assert_eq!(receive(&mut socket).await, ping);
    }
}

async fn receive(socket: &mut TcpStream) -> Vec<u8> {
    timeout(Duration::from_secs(5), read_frame(socket, 2_097_151, None))
        .await
        .unwrap()
        .unwrap()
}
async fn connect(address: &str, intent: i32) -> TcpStream {
    let mut socket = TcpStream::connect(address).await.unwrap();
    let mut handshake = vec![0];
    put_varint(777, &mut handshake);
    put_string("localhost", &mut handshake);
    handshake.extend_from_slice(&25565_u16.to_be_bytes());
    put_varint(intent, &mut handshake);
    write_frame(&mut socket, &handshake, None).await.unwrap();
    socket
}
async fn login(server: &Server, name: &str) -> Result<TcpStream, Vec<u8>> {
    let mut socket = connect(&server.address, 2).await;
    let mut hello = vec![0];
    put_string(name, &mut hello);
    hello.extend_from_slice(&[0; 16]);
    write_frame(&mut socket, &hello, None).await.unwrap();
    let packet = receive(&mut socket).await;
    let mut body = packet.as_slice();
    let id = get_varint(&mut body).unwrap();
    if id == 0 {
        return Err(body.to_vec());
    }
    assert_eq!(id, 2);
    take(&mut body, 16).unwrap();
    assert_eq!(get_string(&mut body, 16).unwrap(), name);
    assert_eq!(get_varint(&mut body).unwrap(), 0);
    take(&mut body, 16).unwrap();
    assert!(body.is_empty());
    write_frame(&mut socket, &[3], None).await.unwrap();
    let probe = receive(&mut socket).await;
    let mut body = probe.as_slice();
    assert_eq!(get_varint(&mut body).unwrap(), 1);
    assert_eq!(get_string(&mut body, 32767).unwrap(), "horizon:mod_list");
    assert_eq!(body, &[0]);
    let packs = receive(&mut socket).await;
    let mut body = packs.as_slice();
    assert_eq!(get_varint(&mut body).unwrap(), 15);
    assert_eq!(get_varint(&mut body).unwrap(), 1);
    for expected in ["minecraft", "core", "26.3"] {
        assert_eq!(get_string(&mut body, 32767).unwrap(), expected);
    }
    assert!(body.is_empty());
    Ok(socket)
}
async fn custom(socket: &mut TcpStream, channel: &str, data: &[u8]) {
    let mut packet = vec![2];
    put_string(channel, &mut packet);
    packet.extend_from_slice(data);
    write_frame(socket, &packet, None).await.unwrap();
}
async fn known_packs(socket: &mut TcpStream) {
    let mut packet = vec![7, 1];
    for value in ["minecraft", "core", "26.3"] {
        put_string(value, &mut packet);
    }
    write_frame(socket, &packet, None).await.unwrap();
}
async fn configuration_end(socket: &mut TcpStream) -> Result<(), Vec<u8>> {
    loop {
        let packet = receive(socket).await;
        let mut body = packet.as_slice();
        match get_varint(&mut body).unwrap() {
            2 => return Err(body.to_vec()),
            3 => {
                assert!(body.is_empty());
                return Ok(());
            }
            _ => {}
        }
    }
}
fn reason(body: &[u8], expected: &str) {
    assert!(
        body.windows(expected.len())
            .any(|part| part == expected.as_bytes()),
        "missing refusal {expected:?}"
    );
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn all_four_profile_settings_control_admission_and_keep_status_available() {
    for (java, horizon) in [(true, true), (true, false), (false, true), (false, false)] {
        let mut server = Server::start(java, horizon, 5);
        server.status().await;
        for advertised in [false, true] {
            let result = login(
                &server,
                if advertised {
                    "HorizonProfile"
                } else {
                    "JavaProfile"
                },
            )
            .await;
            if !java && !horizon {
                reason(&result.unwrap_err(), "Client connections are disabled");
                continue;
            }
            let mut socket = result.unwrap();
            custom(&mut socket, "minecraft:brand", b"\x07vanilla").await;
            if advertised {
                custom(&mut socket, "horizon:mod_list", &[0]).await;
            }
            known_packs(&mut socket).await;
            let allowed = if advertised { horizon } else { java };
            let finished = timeout(Duration::from_secs(3), configuration_end(&mut socket))
                .await
                .unwrap();
            if allowed {
                finished.unwrap();
                custom(&mut socket, "teighto:ignored", &[255]).await;
                write_frame(&mut socket, &[3], None).await.unwrap();
                assert_eq!(
                    get_varint(&mut receive(&mut socket).await.as_slice()).unwrap(),
                    50
                );
            } else {
                reason(
                    &finished.unwrap_err(),
                    if advertised {
                        "Horizon client connections are disabled"
                    } else {
                        "Java-compatible client connections are disabled"
                    },
                );
            }
        }
        server.status().await;
        server.stop().await;
    }
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn malformed_duplicate_conflicting_and_shared_content_answers_are_refused() {
    let mut server = Server::start(true, true, 5);
    for (index, bytes) in [
        vec![0, 1],
        vec![129, 1],
        vec![1, 0],
        vec![1, 1, b'a', 1, b'1', 0, 0],
    ]
    .into_iter()
    .enumerate()
    {
        let mut socket = login(&server, &format!("Invalid{index}")).await.unwrap();
        custom(&mut socket, "horizon:mod_list", &bytes).await;
        reason(
            &configuration_end(&mut socket).await.unwrap_err(),
            if index == 3 {
                "Horizon content mods are not supported"
            } else {
                "Invalid client capability response"
            },
        );
    }
    for (index, repeated) in [vec![0], vec![1]].into_iter().enumerate() {
        let mut socket = login(&server, &format!("Duplicate{index}")).await.unwrap();
        custom(&mut socket, "horizon:mod_list", &[0]).await;
        custom(&mut socket, "horizon:mod_list", &repeated).await;
        reason(
            &configuration_end(&mut socket).await.unwrap_err(),
            "Invalid client capability response",
        );
    }
    server.status().await;
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn late_horizon_response_cannot_change_an_accepted_standard_profile() {
    let mut server = Server::start(true, true, 5);
    let mut socket = login(&server, "LateProfile").await.unwrap();
    known_packs(&mut socket).await;
    configuration_end(&mut socket).await.unwrap();
    custom(&mut socket, "horizon:mod_list", &[0]).await;
    reason(
        &configuration_end(&mut socket).await.unwrap_err(),
        "Invalid client capability response",
    );
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn continuous_unrelated_payloads_do_not_extend_the_configuration_deadline() {
    let mut server = Server::start(true, true, 1);
    let socket = login(&server, "TimeoutProfile").await.unwrap();
    let (mut read, mut write) = socket.into_split();
    let sender = tokio::spawn(async move {
        let mut packet = vec![2];
        put_string("teighto:ignored", &mut packet);
        loop {
            if write_frame(&mut write, &packet, None).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    });
    let error = timeout(Duration::from_secs(3), read_frame(&mut read, 65536, None))
        .await
        .unwrap()
        .unwrap_err();
    sender.abort();
    assert!(matches!(error, zerqon_protocol::codec::Error::Io(error)
        if matches!(error.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted)));
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn finishing_after_registry_delivery_deadline_cannot_enter_play() {
    let mut server = Server::start(true, true, 1);
    let mut socket = login(&server, "LateFinish").await.unwrap();
    known_packs(&mut socket).await;
    configuration_end(&mut socket).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let _ = write_frame(&mut socket, &[3], None).await;
    assert!(
        timeout(Duration::from_secs(2), read_frame(&mut socket, 65536, None))
            .await
            .unwrap()
            .is_err()
    );
    server.status().await;
    server.stop().await;
}
