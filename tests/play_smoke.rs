use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::net::TcpStream;
use zerqon_protocol::codec::{
    get_string, get_varint, put_string, put_varint, read_frame, write_frame,
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
#[ignore = "requires official 26.3 reports: python scripts/prepare-data.py"]
async fn offline_login_configuration_chunks_commands_and_shutdown() {
    play_session(None, "zerqon").await;
}

#[tokio::test]
#[ignore = "requires official 26.3 reports: python scripts/prepare-data.py"]
async fn configured_server_brand_is_sent_during_login() {
    play_session(Some("teighto • тест 🚀"), "teighto • тест 🚀").await;
}

async fn play_session(server_brand: Option<&str>, expected_brand: &str) {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/vanilla/26.3");
    assert!(data.join("prepared.json").exists(), "run prepare-data.py");
    let directory = tempfile::tempdir().unwrap();
    let exe = env!("CARGO_BIN_EXE_zerqon");
    assert!(
        Command::new(exe)
            .args(["--init", "--directory", directory.path().to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(directory.path().join("eula.txt"), "eula=true\n").unwrap();
    let mut settings =
        toml::Table::from_iter([("update_check".into(), toml::Value::Boolean(false))]);
    if let Some(brand) = server_brand {
        settings.insert("server_brand".into(), toml::Value::String(brand.into()));
    }
    std::fs::write(
        directory.path().join("server.settings"),
        toml::to_string(&settings).unwrap(),
    )
    .unwrap();
    std::fs::write(directory.path().join("server.properties"),"server-ip=127.0.0.1\nserver-port=0\nonline-mode=false\nview-distance=1\nnetwork-compression-threshold=256\n").unwrap();
    let mut child = Process(
        Command::new(exe)
            .env("RUST_LOG", "info")
            .args([
                "--directory",
                directory.path().to_str().unwrap(),
                "--data",
                data.to_str().unwrap(),
                "--no-color",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let out = child.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if let Some((_, address)) = line.split_once("Listening on ") {
                let _ = tx.send(address.trim().to_owned());
            }
        }
    });
    let address = rx
        .recv_timeout(Duration::from_secs(15))
        .expect("play server listening");
    let mut socket = TcpStream::connect(address).await.unwrap();
    let mut packet = vec![0];
    put_varint(777, &mut packet);
    put_string("localhost", &mut packet);
    packet.extend_from_slice(&25565u16.to_be_bytes());
    packet.push(2);
    write_frame(&mut socket, &packet, None).await.unwrap();
    let mut hello = vec![0];
    put_string("TestPlayer", &mut hello);
    hello.extend_from_slice(&[0; 16]);
    write_frame(&mut socket, &hello, None).await.unwrap();
    let compression = read_frame(&mut socket, 65536, None).await.unwrap();
    let mut view = compression.as_slice();
    assert_eq!(get_varint(&mut view).unwrap(), 3);
    let threshold = get_varint(&mut view).unwrap() as usize;
    let success = read_frame(&mut socket, 65536, Some(threshold))
        .await
        .unwrap();
    assert_eq!(success[0], 2);
    write_frame(&mut socket, &[3], Some(threshold))
        .await
        .unwrap();
    assert_eq!(
        read_frame(&mut socket, 65536, Some(threshold))
            .await
            .unwrap(),
        b"\x01\x10horizon:mod_list\x00"
    );
    let known = read_frame(&mut socket, 65536, Some(threshold))
        .await
        .unwrap();
    assert_eq!(known[0], 15);
    let mut answer = known;
    answer[0] = 7;
    write_frame(&mut socket, &answer, Some(threshold))
        .await
        .unwrap();
    let mut registries = 0;
    let mut server_brand = None;
    loop {
        let packet = read_frame(&mut socket, 2_097_151, Some(threshold))
            .await
            .unwrap();
        if packet[0] == 7 {
            registries += 1;
        }
        if packet[0] == 1 {
            let mut body = &packet[1..];
            if get_string(&mut body, 32767).unwrap() == "minecraft:brand" {
                assert!(server_brand.is_none(), "server sent its brand twice");
                server_brand = Some(get_string(&mut body, 128).unwrap().to_owned());
                assert!(body.is_empty());
            }
        }
        if packet[0] == 3 {
            break;
        }
    }
    assert!(registries >= 20);
    assert_eq!(server_brand.as_deref(), Some(expected_brand));
    write_frame(&mut socket, &[3], Some(threshold))
        .await
        .unwrap();
    let mut chunks = 0;
    let mut joined = false;
    let mut positioned = false;
    loop {
        let packet = read_frame(&mut socket, 2_097_151, Some(threshold))
            .await
            .unwrap();
        match packet[0] {
            50 => joined = true,
            73 => {
                positioned = true;
                let mut data = &packet[1..];
                let teleport = get_varint(&mut data).unwrap();
                let mut ack = vec![0];
                put_varint(teleport, &mut ack);
                ack.extend_from_slice(&data[..24]);
                ack.extend_from_slice(&data[48..56]);
                write_frame(&mut socket, &ack, Some(threshold))
                    .await
                    .unwrap();
            }
            46 => chunks += 1,
            11 => break,
            _ => {}
        }
    }
    assert!(joined && positioned);
    assert_eq!(chunks, 9);
    let mut command = vec![7];
    put_string("tps", &mut command);
    write_frame(&mut socket, &command, Some(threshold))
        .await
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let packet = read_frame(&mut socket, 65536, Some(threshold))
                .await
                .unwrap();
            if packet[0] == 124 && packet.windows(3).any(|w| w == b"TPS") {
                break packet;
            }
        }
    })
    .await
    .unwrap();
    assert!(reply.len() > 8);
    writeln!(child.0.stdin.as_mut().unwrap(), "stop").unwrap();
    let status = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(status.success());
}
