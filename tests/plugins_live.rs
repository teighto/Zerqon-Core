use std::{
    collections::VecDeque,
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

use tokio::{net::TcpStream, time::timeout};
use uuid::Uuid;
use zerqon_protocol::codec::{
    get_string, get_varint, put_string, put_varint, read_frame, take, write_frame,
};

struct Server(Child, PathBuf);

fn server_executable() -> PathBuf {
    std::env::var_os("ZERQON_TEST_SERVER_EXE").map_or_else(
        || PathBuf::from(env!("CARGO_BIN_EXE_zerqon")),
        PathBuf::from,
    )
}

impl Server {
    fn start(directory: &Path, data: &Path) -> (Self, String) {
        let data =
            std::env::var_os("ZERQON_TEST_DATA").map_or_else(|| data.to_owned(), PathBuf::from);
        let mut server = Self(
            Command::new(server_executable())
                .args([
                    "--directory",
                    directory.to_str().unwrap(),
                    "--data",
                    data.to_str().unwrap(),
                    "--no-color",
                ])
                .env("RUST_LOG", "info")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
            directory.join("logs/latest.log"),
        );
        let output = server.0.stdout.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                if let Some((_, address)) = line.split_once("Listening on ") {
                    let _ = sender.send(address.trim().to_owned());
                }
            }
        });
        let address = receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("test server did not start");
        (server, address)
    }

    async fn stop(&mut self) {
        writeln!(self.0.stdin.as_mut().unwrap(), "stop").unwrap();
        let status = timeout(Duration::from_secs(10), async {
            loop {
                if let Some(status) = self.0.try_wait().unwrap() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("test server did not shut down");
        assert!(
            status.success(),
            "server shutdown failed ({status}):\n{}",
            self.log_tail()
        );
    }

    fn log_tail(&self) -> String {
        let read = || -> std::io::Result<Vec<u8>> {
            let mut file = fs::File::open(&self.1)?;
            let length = file.metadata()?.len().min(65_536);
            file.seek(SeekFrom::End(-(length as i64)))?;
            let mut bytes = Vec::with_capacity(length as usize);
            file.take(length).read_to_end(&mut bytes)?;
            Ok(bytes)
        };
        match read() {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => format!("server log unavailable: {error}"),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(input) = self.0.stdin.as_mut() {
            let _ = writeln!(input, "stop");
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Client {
    socket: TcpStream,
    threshold: usize,
    entity_id: i32,
    uuid: Uuid,
    pending: VecDeque<Vec<u8>>,
    observed: Vec<Vec<u8>>,
}

impl Client {
    async fn join(address: &str, username: &str) -> (Self, Vec<Vec<u8>>) {
        timeout(Duration::from_secs(20), async {
            let mut socket = TcpStream::connect(address).await.unwrap();
            let mut handshake = vec![0];
            put_varint(777, &mut handshake);
            put_string("localhost", &mut handshake);
            handshake.extend_from_slice(&25565_u16.to_be_bytes());
            handshake.push(2);
            write_frame(&mut socket, &handshake, None).await.unwrap();
            let mut hello = vec![0];
            put_string(username, &mut hello);
            hello.extend_from_slice(&[0; 16]);
            write_frame(&mut socket, &hello, None).await.unwrap();
            let compression = read_frame(&mut socket, 65536, None).await.unwrap();
            let mut bytes = compression.as_slice();
            let id = get_varint(&mut bytes).unwrap();
            if id == 0 {
                let reason =
                    get_string(&mut bytes, 32767).expect("invalid login-disconnect string");
                let reason: serde_json::Value =
                    serde_json::from_str(reason).expect("invalid login-disconnect JSON");
                assert!(bytes.is_empty(), "trailing login-disconnect data");
                panic!("login rejected for {username}: {reason}");
            }
            assert_eq!(id, 3, "unexpected first login packet for {username}");
            let threshold = get_varint(&mut bytes).unwrap() as usize;
            let mut client = Self {
                socket,
                threshold,
                entity_id: 0,
                uuid: Uuid::nil(),
                pending: VecDeque::new(),
                observed: Vec::new(),
            };
            let success = client.read().await;
            assert_eq!(success[0], 2);
            client.uuid = Uuid::from_slice(&success[1..17]).unwrap();
            client.send(&[3]).await;
            let mut known = client.read().await;
            if known.first() == Some(&1) {
                let mut probe = vec![1];
                put_string("horizon:mod_list", &mut probe);
                probe.push(0);
                assert_eq!(known, probe);
                known = client.read().await;
            }
            assert_eq!(known[0], 15);
            known[0] = 7;
            client.send(&known).await;
            loop {
                if client.read().await[0] == 3 {
                    break;
                }
            }
            client.send(&[3]).await;
            client.pending.clear();
            client.observed.clear();
            let mut chunks = Vec::new();
            let mut login = false;
            let mut teleported = false;
            loop {
                let packet = client.read().await;
                match packet[0] {
                    50 => {
                        login = true;
                        client.entity_id = i32::from_be_bytes(packet[1..5].try_into().unwrap());
                    }
                    73 => {
                        let mut body = &packet[1..];
                        let teleport = get_varint(&mut body).unwrap();
                        let mut ack = vec![0];
                        put_varint(teleport, &mut ack);
                        ack.extend_from_slice(&body[..24]);
                        ack.extend_from_slice(&body[48..56]);
                        client.send(&ack).await;
                        teleported = true;
                    }
                    46 => chunks.push(packet),
                    11 => {
                        let mut body = &packet[1..];
                        assert_eq!(get_varint(&mut body).unwrap() as usize, chunks.len());
                        break;
                    }
                    1 | 35 | 70 | 71 | 78 | 101 => client.pending.push_back(packet),
                    _ => {}
                }
            }
            assert!(login && teleported);
            assert!(!chunks.is_empty());
            client.send(&[44]).await;
            let mut batch = vec![11];
            batch.extend_from_slice(&8.0_f32.to_be_bytes());
            client.send(&batch).await;
            (client, chunks)
        })
        .await
        .expect("client join timed out")
    }

    async fn send(&mut self, packet: &[u8]) {
        timeout(
            Duration::from_secs(5),
            write_frame(&mut self.socket, packet, Some(self.threshold)),
        )
        .await
        .unwrap()
        .unwrap();
    }

    async fn read(&mut self) -> Vec<u8> {
        let packet = timeout(
            Duration::from_secs(5),
            read_frame(&mut self.socket, 2_097_151, Some(self.threshold)),
        )
        .await
        .unwrap()
        .unwrap();
        if matches!(packet[0], 1 | 35 | 70 | 71 | 78 | 101) {
            self.observed.push(packet.clone());
        }
        packet
    }

    async fn command(&mut self, command: &str) {
        let mut packet = vec![7];
        put_string(command, &mut packet);
        self.send(&packet).await;
    }

    async fn until(&mut self, mut predicate: impl FnMut(&[u8]) -> bool) -> Vec<u8> {
        if let Some(index) = self.pending.iter().position(|packet| predicate(packet)) {
            return self.pending.remove(index).unwrap();
        }
        timeout(Duration::from_secs(8), async {
            loop {
                let packet = self.read().await;
                if packet[0] == 45 {
                    let mut response = packet;
                    response[0] = 28;
                    self.send(&response).await;
                } else {
                    assert_ne!(packet[0], 32, "server disconnected client: {packet:?}");
                    if predicate(&packet) {
                        break packet;
                    }
                }
            }
        })
        .await
        .expect("expected client packet was not received")
    }

    async fn reply(&mut self, command: &str, expected: &str) {
        let text = self.response(command).await;
        assert!(
            text.contains(expected),
            "command /{command}: expected {expected:?}, received {text:?}"
        );
    }

    async fn response(&mut self, command: &str) -> String {
        self.command(command).await;
        let packet = self.until(|packet| packet[0] == 124).await;
        let mut body = &packet[1..];
        assert_eq!(take(&mut body, 1).unwrap(), &[8]);
        let length = u16::from_be_bytes(take(&mut body, 2).unwrap().try_into().unwrap()) as usize;
        let text = std::str::from_utf8(take(&mut body, length).unwrap()).unwrap();
        text.to_owned()
    }

    async fn move_to(&mut self, position: [f64; 3], yaw: f32, pitch: f32) {
        let mut packet = vec![31];
        for value in position {
            packet.extend_from_slice(&value.to_be_bytes());
        }
        packet.extend_from_slice(&yaw.to_be_bytes());
        packet.extend_from_slice(&pitch.to_be_bytes());
        packet.push(1);
        self.send(&packet).await;
    }
    async fn attack(&mut self, id: i32) {
        let mut packet = vec![1];
        put_varint(id, &mut packet);
        self.send(&packet).await;
    }
    async fn mob(&mut self, kind: i32) -> MobWire {
        if let Some(mob) = self
            .observed
            .iter()
            .find_map(|packet| mob_spawn(packet).filter(|mob| mob.kind == kind))
        {
            return mob;
        }
        let packet = self
            .until(|packet| mob_spawn(packet).is_some_and(|mob| mob.kind == kind))
            .await;
        mob_spawn(&packet).unwrap()
    }
}

fn install_read_model_probes(directory: &Path, root: &Path) {
    let libraries = directory.join("libraries/paper");
    fs::create_dir_all(&libraries).unwrap();
    for entry in fs::read_dir(root.join("libraries/paper")).unwrap() {
        let entry = entry.unwrap();
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "jar")
        {
            fs::copy(entry.path(), libraries.join(entry.file_name())).unwrap();
        }
    }
    for index in 0..2 {
        let path = directory.join(format!("plugins/java-plugins/reader-{index}.jar"));
        let mut jar = zip::ZipWriter::new(fs::File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        jar.start_file("plugin.yml", options).unwrap();
        let permissions = if index == 0 {
            "permissions:\n  native.public:\n    default: true\n  native.operator:\n    default: op\n"
        } else {
            ""
        };
        jar.write_all(format!("name: ReadProbe{index}\nversion: 1\nmain: io.github.teighto.zerqon.fixtures.ReadOnlyPaperProbe\napi-version: '26.3'\ncommands:\n  read{index}:\n    permission: native.public\n{permissions}").as_bytes()).unwrap();
        let class = "io/github/teighto/zerqon/fixtures/ReadOnlyPaperProbe.class";
        jar.start_file(class, options).unwrap();
        jar.write_all(&fs::read(root.join("target/paper-bridge/fixtures").join(class)).unwrap())
            .unwrap();
        jar.finish().unwrap();
    }
}

#[tokio::test]
#[ignore = "requires prepared 26.3 data and a native-read Rust WASM example"]
async fn rust_native_reads_observe_real_sessions_positions_and_persistent_world_identity() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let directory = tempfile::tempdir().unwrap();
    setup(directory.path(), &root);
    let guest = std::env::var_os("ZERQON_TEST_NATIVE_WASM").map_or_else(
        || root.join(".cache/rust-plugin-fixtures/hello-native-read.wasm"),
        PathBuf::from,
    );
    fs::copy(
        guest,
        directory.path().join("plugins/rust-plugins/hello.wasm"),
    )
    .unwrap();
    fs::write(
        directory.path().join("server.settings"),
        "update_check=false\njava_plugins=false\nspawn_mobs=false\n",
    )
    .unwrap();
    let data = root.join(".cache/vanilla/26.3");
    let (mut server, address) = Server::start(directory.path(), &data);
    let (mut player, _) = Client::join(&address, "CONSOLE").await;
    let first = player.response("rwhere").await;
    assert!(
        first.contains(&format!(
            "CONSOLE: uuid={}, session={}",
            player.uuid, player.entity_id
        )),
        "{first}"
    );
    assert!(first.contains("position=(0.5,64,0.5)"), "{first}");
    assert!(first.contains("online=1/20, protocol=777"), "{first}");
    let world_uuid = first
        .split("world=world(")
        .nth(1)
        .and_then(|tail| tail.split(')').next())
        .map(|value| Uuid::parse_str(value).unwrap())
        .unwrap();
    assert!(!world_uuid.is_nil());
    player.move_to([4.25, 70.0, -2.5], 90.0, -15.0).await;
    let moved = player.response("rwhere").await;
    assert!(moved.contains("position=(4.25,70,-2.5)"), "{moved}");
    let uuid = player.uuid;
    let old_session = player.entity_id;
    drop(player);
    let (mut observer, _) = Client::join(&address, "Observer").await;
    timeout(Duration::from_secs(5), async {
        loop {
            if observer.response("rwhere").await.contains("online=1/20") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let (mut rejoined, _) = Client::join(&address, "CONSOLE").await;
    assert_eq!(rejoined.uuid, uuid);
    assert_ne!(rejoined.entity_id, old_session);
    let reply = rejoined.response("rwhere").await;
    assert!(
        reply.contains(&format!("session={}", rejoined.entity_id)),
        "{reply}"
    );
    assert!(
        reply.contains(&format!("world=world({world_uuid})")),
        "{reply}"
    );
    drop(rejoined);
    drop(observer);
    server.stop().await;
    let (mut restarted, address) = Server::start(directory.path(), &data);
    let (mut player, _) = Client::join(&address, "CONSOLE").await;
    let reply = player.response("rwhere").await;
    assert!(
        reply.contains(&format!("world=world({world_uuid})")),
        "{reply}"
    );
    drop(player);
    restarted.stop().await;
}

#[tokio::test]
#[ignore = "requires prepared 26.3 data, Java25 Paper probes and Rust WASM example"]
async fn paper_read_models_observe_real_players_worlds_and_once_only_join_quit_events() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let directory = tempfile::tempdir().unwrap();
    setup(directory.path(), &root);
    install_read_model_probes(directory.path(), &root);
    let native_guest = std::env::var_os("ZERQON_TEST_NATIVE_WASM").map_or_else(
        || root.join(".cache/rust-plugin-fixtures/hello-native-read.wasm"),
        PathBuf::from,
    );
    fs::copy(
        native_guest,
        directory.path().join("plugins/rust-plugins/hello.wasm"),
    )
    .unwrap();
    let data = std::env::var_os("ZERQON_TEST_DATA")
        .map_or_else(|| root.join(".cache/vanilla/26.3"), PathBuf::from);
    let (mut server, address) = Server::start(directory.path(), &data);
    let (mut operator, _) = Client::join(&address, "LiveOp").await;
    let (mut ordinary, _) = Client::join(&address, "CONSOLE").await;
    for command in ["read0", "read1"] {
        operator
            .reply(
                command,
                &format!(
                    "{}:{}:LiveOp:true:true:true:true:world:-64:320:0.5",
                    operator.uuid, operator.entity_id
                ),
            )
            .await;
        ordinary
            .reply(
                command,
                &format!(
                    "{}:{}:CONSOLE:true:false:true:false:world:-64:320:0.5",
                    ordinary.uuid, ordinary.entity_id
                ),
            )
            .await;
        operator.reply(&format!("{command} counts"), "2:0").await;
    }
    operator.move_to([4.25, 70.0, 1.5], 90.0, -15.0).await;
    operator
        .reply(
            "read0",
            &format!(
                "{}:{}:LiveOp:true:true:true:true:world:-64:320:4.25",
                operator.uuid, operator.entity_id
            ),
        )
        .await;
    operator
        .reply(
            &format!("read0 other-message {}", ordinary.uuid),
            "other message rejected",
        )
        .await;
    ordinary.reply("read0 lookup Li", "LiveOp").await;
    let world = operator.response("read0 world").await;
    let world_uuid = Uuid::parse_str(world.split(':').next().unwrap()).unwrap();
    assert!(!world_uuid.is_nil());
    assert!(
        world.contains(":minecraft:overworld:NORMAL:-64:320:0.0:64.0:0.0:0.0:0.0"),
        "{world}"
    );
    let rust_view = operator.response("rwhere").await;
    assert!(
        rust_view.contains(&format!(
            "uuid={}, session={}",
            operator.uuid, operator.entity_id
        )),
        "{rust_view}"
    );
    assert!(
        rust_view.contains(&format!("world=world({world_uuid})")),
        "{rust_view}"
    );
    assert!(rust_view.contains("position=(4.25,70,1.5)"), "{rust_view}");
    let departed_uuid = ordinary.uuid;
    let departed_entity = ordinary.entity_id;
    drop(ordinary);
    let started = std::time::Instant::now();
    loop {
        let counts = operator.response("read0 counts").await;
        if counts == "2:1" {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(5), "{counts}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    operator.reply("read1 counts", "2:1").await;
    let (mut rejoined, _) = Client::join(&address, "CONSOLE").await;
    assert_eq!(rejoined.uuid, departed_uuid);
    assert_ne!(rejoined.entity_id, departed_entity);
    for command in ["read0", "read1"] {
        rejoined
            .reply(
                &format!("{command} previous"),
                &format!("true:false:true:false:{departed_entity}"),
            )
            .await;
        operator.reply(&format!("{command} counts"), "3:1").await;
    }
    drop(rejoined);
    drop(operator);
    server.stop().await;
    let (mut restarted, address) = Server::start(directory.path(), &data);
    let (mut reader, _) = Client::join(&address, "LiveOp").await;
    assert_eq!(reader.response("read0 world").await, world);
    reader.reply("read0 counts", "1:0").await;
    drop(reader);
    restarted.stop().await;
}

fn setup(directory: &Path, root: &Path) {
    let status = Command::new(server_executable())
        .args(["--init", "--directory", directory.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());
    fs::write(directory.join("eula.txt"), "eula=true\n").unwrap();
    fs::write(
        directory.join("server.settings"),
        "update_check=false\nspawn_mobs=false\n",
    )
    .unwrap();
    fs::write(directory.join("server.properties"),"server-ip=127.0.0.1\nserver-port=0\nonline-mode=false\nview-distance=1\nnetwork-compression-threshold=256\n").unwrap();
    let mut bytes: [u8; 16] = {
        use md5::{Digest, Md5};
        Md5::digest(b"OfflinePlayer:LiveOp").into()
    };
    bytes[6] = (bytes[6] & 0x0f) | 0x30;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    fs::write(directory.join("ops.json"),serde_json::to_vec(&serde_json::json!([{"uuid":uuid::Uuid::from_bytes(bytes),"name":"LiveOp","level":4,"bypassesPlayerLimit":false}])).unwrap()).unwrap();
    for name in ["zerqon-java-host-0.0.1.jar", "gson-2.13.2.jar"] {
        fs::copy(
            root.join("libraries").join(name),
            directory.join("libraries").join(name),
        )
        .expect("run python scripts/build-java-plugins.py first");
    }
    fs::copy(
        root.join("dist/hello-java-plugin-0.0.1.jar"),
        directory.join("plugins/java-plugins/hello-java.jar"),
    )
    .expect("build the example Java plugin");
    fs::copy(
        root.join("target/wasm32-unknown-unknown/release/zerqon_hello_plugin.wasm"),
        directory.join("plugins/rust-plugins/hello.wasm"),
    )
    .expect("build zerqon-hello-plugin for wasm32-unknown-unknown");
}

fn paletted_value(bytes: &mut &[u8], entries: usize, index: usize, local_limit: u8) -> u32 {
    let bits = take(bytes, 1).unwrap()[0];
    if bits == 0 {
        return get_varint(bytes).unwrap() as u32;
    }
    let palette = if bits <= local_limit {
        let count = get_varint(bytes).unwrap();
        (0..count)
            .map(|_| get_varint(bytes).unwrap() as u32)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let per = 64 / bits as usize;
    let words = take(bytes, entries.div_ceil(per) * 8).unwrap();
    let offset = index / per * 8;
    let word = u64::from_be_bytes(words[offset..offset + 8].try_into().unwrap());
    let value = ((word >> ((index % per) * bits as usize)) & ((1_u64 << bits) - 1)) as u32;
    if palette.is_empty() {
        value
    } else {
        palette[value as usize]
    }
}

fn saved_block(chunks: &[Vec<u8>]) -> u32 {
    saved_block_at(chunks, 0, 64, 0)
}

fn saved_block_at(chunks: &[Vec<u8>], x: i32, y: i32, z: i32) -> u32 {
    let packet = chunks
        .iter()
        .find(|packet| {
            i32::from_be_bytes(packet[1..5].try_into().unwrap()) == x.div_euclid(16)
                && i32::from_be_bytes(packet[5..9].try_into().unwrap()) == z.div_euclid(16)
        })
        .expect("expected chunk");
    let mut body = &packet[9..];
    for _ in 0..get_varint(&mut body).unwrap() {
        get_varint(&mut body).unwrap();
        let words = get_varint(&mut body).unwrap() as usize;
        take(&mut body, words * 8).unwrap();
    }
    let length = get_varint(&mut body).unwrap() as usize;
    let mut sections = take(&mut body, length).unwrap();
    for index in 0..24 {
        take(&mut sections, 4).unwrap();
        let block_index = y.rem_euclid(16) as usize * 256
            + z.rem_euclid(16) as usize * 16
            + x.rem_euclid(16) as usize;
        let state = paletted_value(&mut sections, 4096, block_index, 8);
        paletted_value(&mut sections, 64, 0, 3);
        if index == (y + 64) as usize / 16 {
            return state;
        }
    }
    panic!("missing requested section")
}

#[tokio::test]
#[ignore = "requires prepared 26.3 data, Java 25 host/example JARs and the Rust WASM example"]
async fn truncated_signed_commands_cannot_modify_the_world() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let directory = tempfile::tempdir().unwrap();
    setup(directory.path(), &root);
    let data = root.join(".cache/vanilla/26.3");
    let (mut server, address) = Server::start(directory.path(), &data);
    let (mut client, _) = Client::join(&address, "LiveOp").await;
    let mut packet = vec![8];
    put_string("setblock 0 64 0 1", &mut packet);
    client.send(&packet).await;
    timeout(Duration::from_secs(3), async {
        loop {
            match read_frame(&mut client.socket, 2_097_151, Some(client.threshold)).await {
                Err(_) => break,
                Ok(packet) if packet[0] == 32 => break,
                Ok(packet) => assert_ne!(packet[0], 8, "malformed signed command changed a block"),
            }
        }
    })
    .await
    .expect("malformed command connection stayed open");
    let (mut guest, _) = Client::join(&address, "AfterBadCommand").await;
    guest.reply("tps", "TPS").await;
    drop(client);
    drop(guest);
    server.stop().await;
    let store = zerqon_world::WorldStore::open(directory.path().join("world")).unwrap();
    assert_eq!(
        store
            .load(0, 0)
            .unwrap()
            .map_or(0, |chunk| chunk.get_block(0, 64, 0).unwrap()),
        0
    );
}

#[tokio::test]
#[ignore = "requires prepared 26.3 data, Java 25 host/example JARs and the Rust WASM example"]
async fn creative_building_preserves_orientation_offhand_and_cross_chunk_stair_updates() {
    fn position([x, y, z]: [i32; 3]) -> [u8; 8] {
        (((i64::from(x) & 0x3ffffff) << 38)
            | ((i64::from(z) & 0x3ffffff) << 12)
            | (i64::from(y) & 0xfff))
            .to_be_bytes()
    }
    async fn item(client: &mut Client, slot: i16, id: Option<i32>) {
        let mut packet = vec![57];
        packet.extend_from_slice(&slot.to_be_bytes());
        if let Some(id) = id {
            packet.push(1);
            put_varint(id, &mut packet);
            packet.extend_from_slice(&[0, 0]);
        } else {
            packet.push(0);
        }
        client.send(&packet).await;
    }
    async fn use_on(
        client: &mut Client,
        hand: u8,
        clicked: [i32; 3],
        face: u8,
        hit: [f32; 3],
        sequence: i32,
    ) {
        let mut packet = vec![66, hand];
        packet.extend_from_slice(&position(clicked));
        packet.push(face);
        for value in hit {
            packet.extend_from_slice(&value.to_be_bytes());
        }
        packet.extend_from_slice(&[0, 0]);
        put_varint(sequence, &mut packet);
        client.send(&packet).await;
    }
    async fn dig(client: &mut Client, at: [i32; 3], sequence: i32) {
        let mut packet = vec![41, 0];
        packet.extend_from_slice(&position(at));
        packet.push(1);
        put_varint(sequence, &mut packet);
        client.send(&packet).await;
    }
    async fn changes(
        client: &mut Client,
        sequence: i32,
        expected: &[([i32; 3], i32)],
        rejection: Option<&str>,
    ) {
        let mut wanted: std::collections::BTreeMap<_, _> =
            expected.iter().map(|(p, s)| (position(*p), *s)).collect();
        let mut notice = None;
        loop {
            let packet = client.read().await;
            match packet[0] {
                8 => {
                    let key: [u8; 8] = packet[1..9].try_into().unwrap();
                    if let Some(state) = wanted.remove(&key) {
                        assert_eq!(
                            get_varint(&mut &packet[9..]).unwrap(),
                            state,
                            "block {key:?}, sequence {sequence}"
                        );
                    }
                }
                124 => {
                    let size = u16::from_be_bytes(packet[2..4].try_into().unwrap()) as usize;
                    notice = Some(String::from_utf8(packet[4..4 + size].to_vec()).unwrap());
                }
                45 => {
                    let mut response = packet;
                    response[0] = 28;
                    client.send(&response).await;
                }
                4 => {
                    assert_eq!(get_varint(&mut &packet[1..]).unwrap(), sequence);
                    break;
                }
                32 => panic!("server disconnected during building"),
                _ => {}
            }
        }
        assert!(
            wanted.is_empty(),
            "acknowledgement preceded expected block updates: {wanted:?}, notice={notice:?}"
        );
        if let Some(expected) = rejection {
            assert!(
                notice
                    .as_deref()
                    .is_some_and(|value| value.contains(expected)),
                "{notice:?}"
            );
        } else {
            assert!(notice.is_none(), "unexpected rejection: {notice:?}");
        }
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let directory = tempfile::tempdir().unwrap();
    setup(directory.path(), &root);
    let data = root.join(".cache/vanilla/26.3");
    let (mut server, address) = Server::start(directory.path(), &data);
    let (mut client, _) = Client::join(&address, "LiveOp").await;
    item(&mut client, 36, Some(1)).await;
    use_on(&mut client, 0, [2, 63, 0], 1, [0.5, 1.0, 0.5], 100).await;
    changes(&mut client, 100, &[([2, 64, 0], 1)], None).await;
    item(&mut client, 36, Some(163)).await;
    use_on(&mut client, 0, [2, 64, 0], 5, [1.0, 0.5, 0.5], 101).await;
    changes(&mut client, 101, &[([3, 64, 0], 139)], None).await;
    item(&mut client, 36, Some(341)).await;
    use_on(&mut client, 0, [4, 63, 0], 1, [0.5, 1.0, 0.5], 102).await;
    changes(&mut client, 102, &[([4, 64, 0], 15180)], None).await;
    use_on(&mut client, 0, [4, 64, 0], 1, [0.5, 0.5, 0.5], 103).await;
    changes(&mut client, 103, &[([4, 64, 0], 15182)], None).await;
    item(&mut client, 36, Some(1)).await;
    use_on(&mut client, 0, [0, 63, 0], 1, [0.5, 1.0, 0.5], 104).await;
    changes(
        &mut client,
        104,
        &[([0, 64, 0], 0)],
        Some("intersects an entity"),
    )
    .await;
    use_on(&mut client, 0, [30, 63, 0], 1, [0.5, 1.0, 0.5], 105).await;
    changes(
        &mut client,
        105,
        &[([30, 64, 0], 0)],
        Some("outside interaction range"),
    )
    .await;
    item(&mut client, 45, Some(1)).await;
    item(&mut client, 36, None).await;
    use_on(&mut client, 1, [2, 63, 1], 1, [0.5, 1.0, 0.5], 106).await;
    changes(&mut client, 106, &[([2, 64, 1], 1)], None).await;
    client.move_to([14.5, 64.0, 0.5], 270.0, 0.0).await;
    item(&mut client, 36, Some(515)).await;
    use_on(&mut client, 0, [15, 63, 2], 1, [0.5, 1.0, 0.5], 107).await;
    changes(&mut client, 107, &[([15, 64, 2], 5534)], None).await;
    client.move_to([14.5, 64.0, 0.5], 0.0, 0.0).await;
    use_on(&mut client, 0, [16, 63, 2], 1, [0.5, 1.0, 0.5], 108).await;
    changes(
        &mut client,
        108,
        &[([15, 64, 2], 5542), ([16, 64, 2], 5494)],
        None,
    )
    .await;
    item(&mut client, 36, Some(1050)).await;
    dig(&mut client, [16, 64, 2], 109).await;
    changes(
        &mut client,
        109,
        &[([16, 64, 2], 5494)],
        Some("cannot destroy blocks"),
    )
    .await;
    item(&mut client, 36, Some(1)).await;
    dig(&mut client, [16, 64, 2], 110).await;
    changes(
        &mut client,
        110,
        &[([15, 64, 2], 5534), ([16, 64, 2], 0)],
        None,
    )
    .await;
    drop(client);
    server.stop().await;
    let (mut restarted, address) = Server::start(directory.path(), &data);
    let (client, chunks) = Client::join(&address, "LiveOp").await;
    for ([x, y, z], state) in [
        ([2, 64, 0], 1),
        ([3, 64, 0], 139),
        ([4, 64, 0], 15182),
        ([2, 64, 1], 1),
        ([15, 64, 2], 5534),
        ([16, 64, 2], 0),
    ] {
        assert_eq!(
            saved_block_at(&chunks, x, y, z),
            state,
            "persisted block at {x},{y},{z}"
        );
    }
    drop(client);
    restarted.stop().await;
}

#[tokio::test]
#[ignore = "requires prepared 26.3 data, Java 25 host/example JARs and the Rust WASM example"]
async fn creative_block_placement_uses_the_held_item_and_reaches_the_client() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let directory = tempfile::tempdir().unwrap();
    setup(directory.path(), &root);
    let (mut server, address) = Server::start(directory.path(), &root.join(".cache/vanilla/26.3"));
    let (mut client, _) = Client::join(&address, "LiveOp").await;
    client.reply("give LiveOp stone 8", "Gave 8 stone").await;
    let mut packet = vec![66, 0];
    let clicked = (2_i64 << 38) | 63;
    packet.extend_from_slice(&clicked.to_be_bytes());
    packet.push(1);
    for coordinate in [0.5_f32, 1.0, 0.5] {
        packet.extend_from_slice(&coordinate.to_be_bytes());
    }
    packet.extend_from_slice(&[0, 0]);
    put_varint(100, &mut packet);
    client.send(&packet).await;
    let target = ((2_i64 << 38) | 64).to_be_bytes();
    let update = client
        .until(|packet| packet[0] == 8 && packet[1..9] == target)
        .await;
    let actual = get_varint(&mut &update[9..]).unwrap();
    if actual != 1 {
        let reply = client.until(|packet| packet[0] == 124).await;
        panic!(
            "expected stone, received state {actual}: {}",
            String::from_utf8_lossy(&reply[4..])
        );
    }
    let ack = client.until(|packet| packet[0] == 4).await;
    assert_eq!(get_varint(&mut &ack[1..]).unwrap(), 100);
    client.reply("tps", "TPS").await;
    drop(client);
    server.stop().await;
    let store = zerqon_world::WorldStore::open(directory.path().join("world")).unwrap();
    assert_eq!(
        store
            .load(0, 0)
            .unwrap()
            .unwrap()
            .get_block(2, 64, 0)
            .unwrap(),
        1
    );
}

#[tokio::test]
#[ignore = "requires prepared 26.3 data, Java 25 host/example JARs and the Rust WASM example"]
async fn java_native_queries_observe_live_players_and_the_rust_tick_clock() {
    async fn snapshot(client: &mut Client) -> (u32, u64) {
        client.command("jserver").await;
        let packet = client.until(|packet| packet[0] == 124).await;
        let mut body = &packet[1..];
        assert_eq!(take(&mut body, 1).unwrap(), &[8]);
        let size = u16::from_be_bytes(take(&mut body, 2).unwrap().try_into().unwrap()) as usize;
        let text = std::str::from_utf8(take(&mut body, size).unwrap()).unwrap();
        let prefix = format!(
            "Zerqon {} | Minecraft 26.3 | Players ",
            env!("CARGO_PKG_VERSION")
        );
        let values = text
            .strip_prefix(&prefix)
            .unwrap_or_else(|| panic!("unexpected native query response: {text}"));
        let (online, tick) = values.split_once("/20 | Tick ").unwrap();
        (online.parse().unwrap(), tick.parse().unwrap())
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let directory = tempfile::tempdir().unwrap();
    setup(directory.path(), &root);
    let (mut server, address) = Server::start(directory.path(), &root.join(".cache/vanilla/26.3"));
    let (mut first, _) = Client::join(&address, "LiveOp").await;
    let (second, _) = Client::join(&address, "SnapshotGuest").await;
    let (online, tick) = snapshot(&mut first).await;
    assert_eq!(online, 2);
    drop(second);
    timeout(Duration::from_secs(3), async {
        loop {
            let (online, next_tick) = snapshot(&mut first).await;
            if online == 1 && next_tick > tick {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("snapshot did not follow disconnect and server ticks");
    drop(first);
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires prepared 26.3 data, Java 25 host/example JARs and the Rust WASM example"]
async fn compiled_plugins_tpsbar_permissions_and_saved_world_survive_restart() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let data = root.join(".cache/vanilla/26.3");
    assert!(
        data.join("prepared.json").is_file(),
        "run python scripts/prepare-data.py"
    );
    let directory = tempfile::tempdir().unwrap();
    setup(directory.path(), &root);
    let (mut server, address) = Server::start(directory.path(), &data);
    let (mut client, chunks) = Client::join(&address, "LiveOp").await;
    assert_eq!(saved_block(&chunks), 0);
    client
        .reply(
            "plugins",
            "Rust: hello (enabled); Java: hello-java (enabled)",
        )
        .await;
    client
        .reply("hello", "Hello, LiveOp! This plugin is written in Rust.")
        .await;
    client
        .reply(
            "jhello",
            "Hello, LiveOp! This plugin uses the Zerqon Java API.",
        )
        .await;
    client.reply("tps", "TPS ").await;
    client.command("tpsbar").await;
    let add = client
        .until(|packet| packet[0] == 9 && packet.get(17) == Some(&0))
        .await;
    let id = add[1..17].to_vec();
    let health = client
        .until(|packet| packet[0] == 9 && packet.get(17) == Some(&2))
        .await;
    assert_eq!(&health[1..17], &id);
    let progress = f32::from_be_bytes(health[18..22].try_into().unwrap());
    assert!(progress.is_finite() && (0.0..=1.0).contains(&progress));
    client.command("tpsbar").await;
    let remove = client
        .until(|packet| packet[0] == 9 && packet.get(17) == Some(&1))
        .await;
    assert_eq!(&remove[1..17], &id);
    let (mut guest, _) = Client::join(&address, "Guest").await;
    guest.reply("setblock 0 64 0 1", "Permission denied").await;
    guest.reply("stop", "Permission denied").await;
    drop(guest);
    client
        .reply("setblock 0 64 0 1", "Block set at 0 64 0")
        .await;
    client
        .reply("setblock 0 64 0 2147483647", "Command failed:")
        .await;
    drop(client);
    server.stop().await;
    let (mut restarted, address) = Server::start(directory.path(), &data);
    let (mut client, chunks) = Client::join(&address, "LiveOp").await;
    assert_eq!(
        saved_block(&chunks),
        1,
        "saved block was not present on the wire after restart"
    );
    client.reply("hello", "written in Rust").await;
    client.reply("jhello", "Zerqon Java API").await;
    drop(client);
    restarted.stop().await;
}

fn tab_entry(packet: &[u8]) -> Option<(Uuid, String)> {
    if packet.first() != Some(&71) || packet.get(1).is_none_or(|actions| actions & 1 == 0) {
        return None;
    }
    let mut body = &packet[2..];
    assert_eq!(get_varint(&mut body).unwrap(), 1);
    let uuid = Uuid::from_slice(take(&mut body, 16).unwrap()).unwrap();
    Some((uuid, get_string(&mut body, 16).unwrap().to_owned()))
}

fn spawned_entity(packet: &[u8]) -> Option<(i32, Uuid)> {
    if packet.first() != Some(&1) {
        return None;
    }
    let mut body = &packet[1..];
    let id = get_varint(&mut body).unwrap();
    let uuid = Uuid::from_slice(take(&mut body, 16).unwrap()).unwrap();
    if get_varint(&mut body).unwrap() != 159 {
        return None;
    }
    Some((id, uuid))
}

fn removes_entity(packet: &[u8], expected: i32) -> bool {
    if packet.first() != Some(&78) {
        return false;
    }
    let mut body = &packet[1..];
    (0..get_varint(&mut body).unwrap()).any(|_| get_varint(&mut body).unwrap() == expected)
}

fn removes_tab(packet: &[u8], expected: Uuid) -> bool {
    if packet.first() != Some(&70) {
        return false;
    }
    let mut body = &packet[1..];
    (0..get_varint(&mut body).unwrap())
        .any(|_| Uuid::from_slice(take(&mut body, 16).unwrap()).unwrap() == expected)
}

async fn refused_duplicate(address: &str, name: &str) {
    timeout(Duration::from_secs(5), async {
        let mut socket = TcpStream::connect(address).await.unwrap();
        let mut packet = vec![0];
        put_varint(777, &mut packet);
        put_string("localhost", &mut packet);
        packet.extend_from_slice(&25565_u16.to_be_bytes());
        packet.push(2);
        write_frame(&mut socket, &packet, None).await.unwrap();
        let mut packet = vec![0];
        put_string(name, &mut packet);
        packet.extend_from_slice(&[0; 16]);
        write_frame(&mut socket, &packet, None).await.unwrap();
        let packet = read_frame(&mut socket, 65536, None).await.unwrap();
        assert_eq!(
            packet[0], 0,
            "duplicate login must be rejected before compression/success"
        );
        let mut body = &packet[1..];
        let reason = get_string(&mut body, 32767).unwrap();
        assert!(
            reason.contains("already connected"),
            "unexpected duplicate rejection: {reason}"
        );
    })
    .await
    .expect("duplicate login did not complete");
}

#[tokio::test]
#[ignore = "requires python scripts/prepare-data.py, python scripts/build-java-plugins.py and cargo build -p zerqon-hello-plugin --target wasm32-unknown-unknown --release"]
async fn two_clients_replicate_tab_entities_motion_visibility_and_departures() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let data = root.join(".cache/vanilla/26.3");
    assert!(
        data.join("prepared.json").is_file(),
        "run python scripts/prepare-data.py"
    );
    let directory = tempfile::tempdir().unwrap();
    setup(directory.path(), &root);
    let (mut server, address) = Server::start(directory.path(), &data);
    let (mut first, _) = Client::join(&address, "CONSOLE").await;
    first.reply("stop", "Permission denied").await;
    first.reply("setblock 0 64 0 1", "Permission denied").await;
    refused_duplicate(&address, "console").await;
    first.reply("version", "Zerqon").await;
    let (mut second, _) = Client::join(&address, "Peer").await;
    assert_ne!(first.entity_id, second.entity_id);
    assert_ne!(first.uuid, second.uuid);
    let first_id = first.entity_id;
    let first_uuid = first.uuid;
    let second_id = second.entity_id;
    let second_uuid = second.uuid;
    first
        .until(|packet| tab_entry(packet) == Some((second_uuid, "Peer".into())))
        .await;
    second
        .until(|packet| tab_entry(packet) == Some((first_uuid, "CONSOLE".into())))
        .await;
    first
        .until(|packet| spawned_entity(packet) == Some((second_id, second_uuid)))
        .await;
    second
        .until(|packet| spawned_entity(packet) == Some((first_id, first_uuid)))
        .await;
    second.move_to([2.0, 64.0, 3.0], 90.0, -15.0).await;
    let movement = first.until(|packet| packet[0] == 35).await;
    let mut body = &movement[1..];
    assert_eq!(get_varint(&mut body).unwrap(), second_id);
    assert_eq!(get_varint(&mut body).unwrap(), 0);
    for expected in [2.0, 64.0, 3.0] {
        assert_eq!(
            f64::from_be_bytes(take(&mut body, 8).unwrap().try_into().unwrap()),
            expected
        );
    }
    assert_eq!(
        f32::from_be_bytes(take(&mut body, 4).unwrap().try_into().unwrap()),
        90.0
    );
    assert_eq!(
        f32::from_be_bytes(take(&mut body, 4).unwrap().try_into().unwrap()),
        -15.0
    );
    assert_eq!(body, &[1]);
    let out_mark = first.observed.len();
    second.move_to([64.5, 64.0, 0.5], 0.0, 0.0).await;
    first
        .until(|packet| removes_entity(packet, second_id))
        .await;
    second
        .until(|packet| removes_entity(packet, first_id))
        .await;
    first.reply("version", "Zerqon").await;
    assert!(
        !first.observed[out_mark..]
            .iter()
            .any(|packet| removes_tab(packet, second_uuid)),
        "leaving view distance must retain TAB entry"
    );
    second.move_to([0.5, 64.0, 0.5], 0.0, 0.0).await;
    first
        .until(|packet| spawned_entity(packet) == Some((second_id, second_uuid)))
        .await;
    second
        .until(|packet| spawned_entity(packet) == Some((first_id, first_uuid)))
        .await;
    drop(second);
    let mut entity_removed = false;
    let mut tab_removed = false;
    for _ in 0..2 {
        let packet = first
            .until(|packet| removes_entity(packet, second_id) || removes_tab(packet, second_uuid))
            .await;
        entity_removed |= removes_entity(&packet, second_id);
        tab_removed |= removes_tab(&packet, second_uuid);
    }
    assert!(entity_removed && tab_removed);
    first.reply("list", "Players (1): CONSOLE").await;
    let reconnect_mark = first.observed.len();
    let (mut reconnected, _) = Client::join(&address, "Peer").await;
    assert_eq!(reconnected.uuid, second_uuid);
    assert_ne!(reconnected.entity_id, second_id);
    first
        .until(|packet| tab_entry(packet) == Some((second_uuid, "Peer".into())))
        .await;
    first
        .until(|packet| spawned_entity(packet) == Some((reconnected.entity_id, second_uuid)))
        .await;
    reconnected
        .until(|packet| spawned_entity(packet) == Some((first_id, first_uuid)))
        .await;
    first.reply("list", "Players (2):").await;
    assert!(
        !first.observed[reconnect_mark..]
            .iter()
            .any(|packet| spawned_entity(packet).is_some_and(|(id, _)| id == second_id)),
        "reconnect recreated the departed entity ID"
    );
    drop(reconnected);
    drop(first);
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires prepared 26.3 data and built Java/Rust example plugins"]
async fn summoned_mob_has_real_motion_damage_and_persisted_lifecycle() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let data = root.join(".cache/vanilla/26.3");
    let directory = tempfile::tempdir().unwrap();
    setup(directory.path(), &root);
    let (mut server, address) = Server::start(directory.path(), &data);
    let (mut first, _) = Client::join(&address, "LiveOp").await;
    let (mut second, _) = Client::join(&address, "Guest").await;
    second
        .reply("summon pig 2.5 64 0.5", "Permission denied")
        .await;
    first
        .reply("summon pig 2.5 64 0.5", "Summoned minecraft:pig")
        .await;
    let pig = first.mob(101).await;
    let peer_pig = second.mob(101).await;
    assert_eq!((pig.id, pig.uuid), (peer_pig.id, peer_pig.uuid));
    assert_ne!(pig.id, first.entity_id);
    assert_ne!(pig.id, second.entity_id);
    second.move_to([64.5, 64.0, 0.5], 0.0, 0.0).await;
    second.until(|packet| removes_entity(packet, pig.id)).await;
    second.move_to([1.5, 64.0, 0.5], 0.0, 0.0).await;
    second
        .until(|packet| {
            mob_spawn(packet).is_some_and(|mob| mob.id == pig.id && mob.uuid == pig.uuid)
        })
        .await;
    let moved = first
        .until(|packet| packet_entity(packet, 35) == Some(pig.id))
        .await;
    let mut body = &moved[1..];
    get_varint(&mut body).unwrap();
    assert_eq!(get_varint(&mut body).unwrap(), 0);
    let position = std::array::from_fn(|_| {
        f64::from_be_bytes(take(&mut body, 8).unwrap().try_into().unwrap())
    });
    assert_ne!(
        position, pig.position,
        "mob must run real simulation after summon"
    );
    second.attack(pig.id).await;
    first
        .until(|packet| mob_health(packet) == Some((pig.id, 6.0)))
        .await;
    let hurt = first
        .until(|packet| packet_entity(packet, 43) == Some(pig.id))
        .await;
    let mut hurt_body = &hurt[1..];
    assert_eq!(get_varint(&mut hurt_body).unwrap(), pig.id);
    assert_eq!(hurt_body, &0.0_f32.to_be_bytes());
    let motion = first
        .until(|packet| packet_entity(packet, 103) == Some(pig.id))
        .await;
    let mut motion_body = &motion[1..];
    assert_eq!(get_varint(&mut motion_body).unwrap(), pig.id);
    assert_eq!(motion_body.len(), 6);
    let packed = u64::from(motion_body[0])
        | (u64::from(motion_body[1]) << 8)
        | (u64::from(u32::from_be_bytes(motion_body[2..6].try_into().unwrap())) << 16);
    assert_eq!(packed & 7, 1, "basic knockback must use LP scale 1");
    let velocity = [3, 18, 33].map(|shift| ((packed >> shift) & 0x7fff) as f64 / 16383.0 - 1.0);
    assert!(
        velocity[0].hypot(velocity[2]) > 0.25,
        "hit had no horizontal knockback"
    );
    second
        .until(|packet| mob_health(packet) == Some((pig.id, 6.0)))
        .await;
    let registry: serde_json::Value =
        serde_json::from_slice(&fs::read(data.join("generated/reports/registries.json")).unwrap())
            .unwrap();
    let cow_kind = registry["minecraft:entity_type"]["entries"]["minecraft:cow"]["protocol_id"]
        .as_i64()
        .unwrap() as i32;
    let zombie_kind =
        registry["minecraft:entity_type"]["entries"]["minecraft:zombie"]["protocol_id"]
            .as_i64()
            .unwrap() as i32;
    first
        .reply("summon cow 5.5 64 5.5", "Summoned minecraft:cow")
        .await;
    first
        .reply("summon zombie -5.5 64 5.5", "Summoned minecraft:zombie")
        .await;
    let cow = first.mob(cow_kind).await;
    let zombie = first.mob(zombie_kind).await;
    assert_ne!(cow.id, pig.id);
    assert_ne!(zombie.id, cow.id);
    drop(second);
    drop(first);
    server.stop().await;
    assert!(directory.path().join("world/entities.zqe").is_file());
    let (mut restarted, address) = Server::start(directory.path(), &data);
    let (mut first, _) = Client::join(&address, "LiveOp").await;
    let restored = first.mob(101).await;
    assert_eq!(first.mob(cow_kind).await.uuid, cow.uuid);
    assert_eq!(first.mob(zombie_kind).await.uuid, zombie.uuid);
    assert_eq!((restored.id, restored.uuid), (pig.id, pig.uuid));
    assert_ne!(restored.id, first.entity_id);
    first
        .until(|packet| mob_health(packet) == Some((pig.id, 6.0)))
        .await;
    first
        .move_to(
            [
                restored.position[0] - 1.0,
                restored.position[1],
                restored.position[2],
            ],
            0.0,
            0.0,
        )
        .await;
    first.attack(pig.id).await;
    first.attack(pig.id).await;
    first
        .until(|packet| mob_health(packet) == Some((pig.id, 2.0)))
        .await;
    first.reply("version", "Zerqon").await;
    assert!(
        !first
            .observed
            .iter()
            .any(|packet| removes_entity(packet, pig.id)),
        "attack cooldown failed"
    );
    tokio::time::sleep(Duration::from_millis(550)).await;
    first.reply("version", "Zerqon").await;
    let latest = first
        .observed
        .iter()
        .rev()
        .find_map(|packet| {
            if packet_entity(packet, 35) != Some(pig.id) {
                return None;
            }
            let mut body = &packet[1..];
            get_varint(&mut body).unwrap();
            assert_eq!(get_varint(&mut body).unwrap(), 0);
            Some(std::array::from_fn::<_, 3, _>(|_| {
                f64::from_be_bytes(take(&mut body, 8).unwrap().try_into().unwrap())
            }))
        })
        .unwrap_or(restored.position);
    first
        .move_to([latest[0] - 1.0, latest[1], latest[2]], 0.0, 0.0)
        .await;
    first.reply("version", "Zerqon").await;
    first.attack(pig.id).await;
    first
        .until(|packet| mob_health(packet) == Some((pig.id, 0.0)))
        .await;
    first.until(|packet| removes_entity(packet, pig.id)).await;
    drop(first);
    restarted.stop().await;
    let (mut final_server, address) = Server::start(directory.path(), &data);
    let (mut first, _) = Client::join(&address, "LiveOp").await;
    first.reply("version", "Zerqon").await;
    assert!(
        !first
            .observed
            .iter()
            .any(|packet| mob_spawn(packet).is_some_and(|mob| mob.uuid == pig.uuid)),
        "dead mob reappeared after restart"
    );
    drop(first);
    final_server.stop().await;
}

#[tokio::test]
#[ignore = "requires prepared 26.3 data and built Java/Rust example plugins"]
async fn default_natural_spawning_creates_passive_mobs_and_enforces_cap() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let data = root.join(".cache/vanilla/26.3");
    let directory = tempfile::tempdir().unwrap();
    setup(directory.path(), &root);
    fs::write(
        directory.path().join("server.settings"),
        "update_check=false\nmax_entities=2\n",
    )
    .unwrap();
    let properties = fs::read_to_string(directory.path().join("server.properties"))
        .unwrap()
        .replace("view-distance=1", "view-distance=4");
    fs::write(directory.path().join("server.properties"), properties).unwrap();
    let (mut server, address) = Server::start(directory.path(), &data);
    let (mut client, _) = Client::join(&address, "LiveOp").await;
    let packet = client.until(|packet| mob_spawn(packet).is_some()).await;
    let first = mob_spawn(&packet).unwrap();
    let packet = client
        .until(|packet| mob_spawn(packet).is_some_and(|mob| mob.id != first.id))
        .await;
    let second = mob_spawn(&packet).unwrap();
    let registry: serde_json::Value =
        serde_json::from_slice(&fs::read(data.join("generated/reports/registries.json")).unwrap())
            .unwrap();
    let cow = registry["minecraft:entity_type"]["entries"]["minecraft:cow"]["protocol_id"]
        .as_i64()
        .unwrap() as i32;
    for mob in [first, second] {
        assert!([101, cow].contains(&mob.kind));
        assert!((mob.position[0] - 0.5).hypot(mob.position[2] - 0.5) >= 24.0);
    }
    client
        .reply("summon pig 0.5 64 0.5", "entity capacity reached")
        .await;
    drop(client);
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires prepared 26.3 data, built Java/Rust examples, and Windows or Linux process inspection"]
async fn failed_mob_shutdown_save_still_disables_plugins_and_terminates_java() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let data = root.join(".cache/vanilla/26.3");
    let directory = tempfile::tempdir().unwrap();
    setup(directory.path(), &root);
    let (mut server, address) = Server::start(directory.path(), &data);
    let java_pid = java_child_pid(server.0.id()).await;
    assert!(
        java_process_alive(java_pid).await,
        "Java host must be running before shutdown"
    );
    let (mut client, _) = Client::join(&address, "LiveOp").await;
    client
        .reply("summon pig 2.5 64 0.5", "Summoned minecraft:pig")
        .await;
    let pig = client.mob(101).await;
    client
        .until(|packet| packet_entity(packet, 35) == Some(pig.id))
        .await;
    let path = directory.path().join("world/entities.zqe");
    fs::write(&path, b"shutdown regression corrupt snapshot").unwrap();
    let log_path = directory.path().join("logs/latest.log");
    timeout(Duration::from_secs(12), async {
        loop {
            let log = fs::read_to_string(&log_path).unwrap();
            if log.contains("Mob tick failed") && log.contains("invalid entity snapshot store") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("mob save failure was not observed while entity was dirty");
    writeln!(server.0.stdin.as_mut().unwrap(), "stop").unwrap();
    let status = timeout(Duration::from_secs(10), async {
        loop {
            if let Some(status) = server.0.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("server did not finish failed shutdown");
    assert!(
        !status.success(),
        "failed save must produce a failing exit status"
    );
    timeout(Duration::from_secs(5), async {
        while java_process_alive(java_pid).await {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("Java host survived server shutdown");
    let log = fs::read_to_string(log_path).unwrap();
    assert!(
        log.contains("Hello Java plugin disabled"),
        "Java Disable was skipped: {log}"
    );
    assert!(
        log.contains("Hello Rust plugin disabled"),
        "Rust Disable was skipped: {log}"
    );
    assert!(
        !log.contains("Stopped cleanly"),
        "failed save must not report a clean shutdown"
    );
    assert_eq!(
        fs::read(path).unwrap(),
        b"shutdown regression corrupt snapshot"
    );
    drop(client);
}

#[tokio::test]
#[ignore = "requires prepared 26.3 data, Java 25 javac/jar and built Java/Rust examples"]
async fn java_disable_failure_preserves_other_cleanup_and_fails_process() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let data = root.join(".cache/vanilla/26.3");
    let directory = tempfile::tempdir().unwrap();
    setup(directory.path(), &root);
    build_failing_java_plugin(&root, directory.path()).await;
    let (mut server, _address) = Server::start(directory.path(), &data);
    let log_path = directory.path().join("logs/latest.log");
    let log = fs::read_to_string(&log_path).unwrap();
    let java_pid = log
        .lines()
        .find_map(|line| {
            line.split_once("SHUTDOWN_FIXTURE_PID=")
                .and_then(|(_, tail)| tail.split_whitespace().next())
                .and_then(|pid| pid.parse::<u32>().ok())
        })
        .expect("real fixture plugin did not enable");
    assert!(java_process_alive(java_pid).await);
    writeln!(server.0.stdin.as_mut().unwrap(), "stop").unwrap();
    let status = timeout(Duration::from_secs(10), async {
        loop {
            if let Some(status) = server.0.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("server did not finish callback failure shutdown");
    timeout(Duration::from_secs(5), async {
        while java_process_alive(java_pid).await {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("Java host survived callback failure");
    let log = fs::read_to_string(log_path).unwrap();
    assert!(
        log.contains("deliberate Java Disable failure"),
        "failure callback was not attempted: {log}"
    );
    assert!(
        log.contains("Hello Java plugin disabled"),
        "another Java plugin did not receive Disable: {log}"
    );
    assert!(
        log.contains("Hello Rust plugin disabled"),
        "Rust cleanup was skipped: {log}"
    );
    assert!(
        !status.success(),
        "Java Disable failure was swallowed; process reported success: {log}"
    );
    assert!(
        !log.contains("Stopped cleanly"),
        "failed callback reported a clean shutdown"
    );
}

async fn build_failing_java_plugin(root: &Path, directory: &Path) {
    let fixture = directory.join("fixture-build");
    let classes = fixture.join("classes");
    fs::create_dir_all(&classes).unwrap();
    let mut javac = tokio::process::Command::new("javac");
    javac
        .arg("--release")
        .arg("25")
        .arg("-encoding")
        .arg("UTF-8")
        .arg("-cp")
        .arg(root.join("libraries/zerqon-java-api-0.0.1.jar"))
        .arg("-d")
        .arg(&classes)
        .arg(root.join("tests/fixtures/java-plugin-shutdown/FailingDisablePlugin.java"))
        .kill_on_drop(true);
    #[cfg(windows)]
    javac.creation_flags(0x0800_0000);
    let output = timeout(Duration::from_secs(30), javac.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "javac fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut java = tokio::process::Command::new("java");
    java.args(["-XshowSettings:properties", "-version"])
        .kill_on_drop(true);
    #[cfg(windows)]
    java.creation_flags(0x0800_0000);
    let output = timeout(Duration::from_secs(10), java.output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success());
    let properties = String::from_utf8_lossy(&output.stderr);
    let java_home = properties
        .lines()
        .find_map(|line| line.trim().strip_prefix("java.home = "))
        .expect("java.home missing");
    let manifest = fixture.join("MANIFEST.MF");
    fs::write(
        &manifest,
        "Manifest-Version: 1.0\nZerqon-Plugin: FailingDisablePlugin\nZerqon-API: 1\n\n",
    )
    .unwrap();
    let mut jar =
        tokio::process::Command::new(Path::new(java_home).join("bin").join(if cfg!(windows) {
            "jar.exe"
        } else {
            "jar"
        }));
    jar.arg("--create")
        .arg("--file")
        .arg(directory.join("plugins/java-plugins/fail-disable.jar"))
        .arg("--manifest")
        .arg(manifest)
        .arg("-C")
        .arg(classes)
        .arg(".")
        .kill_on_drop(true);
    #[cfg(windows)]
    jar.creation_flags(0x0800_0000);
    let output = timeout(Duration::from_secs(10), jar.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "jar fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn java_child_pid(parent: u32) -> u32 {
    #[cfg(windows)]
    {
        let script = format!(
            "Get-CimInstance Win32_Process -Filter 'ParentProcessId = {parent}' | Where-Object {{ $_.Name -in @('java.exe','javaw.exe') }} | Select-Object -ExpandProperty ProcessId"
        );
        let mut command = tokio::process::Command::new("powershell.exe");
        command
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .kill_on_drop(true)
            .creation_flags(0x0800_0000);
        let output = timeout(Duration::from_secs(8), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success(), "cannot inspect Java child process");
        let pids = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .collect::<Vec<_>>();
        assert_eq!(pids.len(), 1, "expected one Java host child");
        pids[0]
    }
    #[cfg(target_os = "linux")]
    {
        let mut pids = std::collections::BTreeSet::new();
        for task in fs::read_dir(format!("/proc/{parent}/task")).unwrap() {
            let children =
                fs::read_to_string(task.unwrap().path().join("children")).unwrap_or_default();
            for pid in children
                .split_whitespace()
                .filter_map(|pid| pid.parse::<u32>().ok())
            {
                if fs::read_to_string(format!("/proc/{pid}/comm"))
                    .is_ok_and(|name| name.trim().starts_with("java"))
                {
                    pids.insert(pid);
                }
            }
        }
        assert_eq!(pids.len(), 1, "expected one Java host child");
        *pids.first().unwrap()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = parent;
        panic!("process inspection test supports Windows and Linux")
    }
}

async fn java_process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let mut command = tokio::process::Command::new("tasklist.exe");
        command
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .kill_on_drop(true)
            .creation_flags(0x0800_0000);
        let output = timeout(Duration::from_secs(3), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
    }
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = pid;
        false
    }
}

#[derive(Debug)]
struct MobWire {
    id: i32,
    uuid: Uuid,
    kind: i32,
    position: [f64; 3],
}
fn mob_spawn(packet: &[u8]) -> Option<MobWire> {
    if packet.first() != Some(&1) {
        return None;
    }
    let mut body = &packet[1..];
    let id = get_varint(&mut body).unwrap();
    let uuid = Uuid::from_slice(take(&mut body, 16).unwrap()).unwrap();
    let kind = get_varint(&mut body).unwrap();
    if kind == 159 {
        return None;
    }
    let position = std::array::from_fn(|_| {
        f64::from_be_bytes(take(&mut body, 8).unwrap().try_into().unwrap())
    });
    Some(MobWire {
        id,
        uuid,
        kind,
        position,
    })
}
fn packet_entity(packet: &[u8], packet_id: u8) -> Option<i32> {
    if packet.first() != Some(&packet_id) {
        return None;
    }
    Some(get_varint(&mut &packet[1..]).unwrap())
}
fn mob_health(packet: &[u8]) -> Option<(i32, f32)> {
    if packet.first() != Some(&101) {
        return None;
    }
    let mut body = &packet[1..];
    let id = get_varint(&mut body).unwrap();
    if take(&mut body, 1).unwrap() != [9] || get_varint(&mut body).unwrap() != 3 {
        return None;
    }
    Some((
        id,
        f32::from_be_bytes(take(&mut body, 4).unwrap().try_into().unwrap()),
    ))
}
