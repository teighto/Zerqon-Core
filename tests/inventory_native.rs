use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use tokio::{net::TcpStream, time::timeout};
use uuid::Uuid;
use zerqon_protocol::codec::{
    get_string, get_varint, put_string, put_varint, read_frame, take, write_frame,
};

struct Server {
    child: Child,
    address: String,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Server {
    fn start(directory: &Path) -> Self {
        let data = Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/vanilla/26.3");
        assert!(
            data.join("prepared.json").is_file(),
            "prepare official26.3 data"
        );
        let exe = env!("CARGO_BIN_EXE_zerqon");
        if !directory.join("eula.txt").exists() {
            assert!(
                Command::new(exe)
                    .arg("--init")
                    .arg("--directory")
                    .arg(directory)
                    .status()
                    .unwrap()
                    .success()
            );
            std::fs::write(directory.join("eula.txt"), "eula=true\n").unwrap();
            std::fs::write(directory.join("server.properties"),"server-ip=127.0.0.1\nserver-port=0\nmax-players=4\nonline-mode=false\nview-distance=1\nnetwork-compression-threshold=-1\n").unwrap();
            std::fs::write(directory.join("server.settings"),"network_threads=2\nworld_threads=1\njava_plugins=false\nupdate_check=false\nspawn_mobs=false\n").unwrap();
        }
        let mut child = Command::new(exe)
            .env("RUST_LOG", "info")
            .arg("--directory")
            .arg(directory)
            .arg("--data")
            .arg(data)
            .arg("--no-color")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
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
                panic!("server startup failed: {error}");
            }
        };
        Self { child, address }
    }
    fn command(&mut self, command: &str) {
        writeln!(self.child.stdin.as_mut().unwrap(), "{command}").unwrap();
    }
    async fn stop(&mut self) {
        self.command("stop");
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stack {
    item: i32,
    count: i32,
    name: Option<String>,
}
#[derive(Debug, Clone)]
struct Snapshot {
    state: i32,
    slots: Vec<Option<Stack>>,
    cursor: Option<Stack>,
    selected: u8,
    content: Vec<u8>,
    equipment: Vec<u8>,
    hands: [Option<Stack>; 2],
}
struct Client {
    socket: TcpStream,
    uuid: Uuid,
    entity: i32,
    snapshot: Snapshot,
}

fn read_stack(data: &mut &[u8]) -> Option<Stack> {
    let count = get_varint(data).unwrap();
    if count == 0 {
        return None;
    }
    assert!((1..=99).contains(&count));
    let item = get_varint(data).unwrap();
    let added = get_varint(data).unwrap();
    let removed = get_varint(data).unwrap();
    assert_eq!(removed, 0);
    let mut name = None;
    for _ in 0..added {
        let kind = get_varint(data).unwrap();
        assert_eq!(kind, 6, "unexpected component in native test");
        assert_eq!(take(data, 1).unwrap(), [8]);
        let length = u16::from_be_bytes(take(data, 2).unwrap().try_into().unwrap()) as usize;
        name = Some(String::from_utf8(take(data, length).unwrap().to_vec()).unwrap());
    }
    Some(Stack { item, count, name })
}

impl Client {
    async fn begin(address: &str, name: &str) -> TcpStream {
        let mut socket = TcpStream::connect(address).await.unwrap();
        let mut handshake = vec![0];
        put_varint(777, &mut handshake);
        put_string("localhost", &mut handshake);
        handshake.extend_from_slice(&25565u16.to_be_bytes());
        handshake.push(2);
        write_frame(&mut socket, &handshake, None).await.unwrap();
        let mut hello = vec![0];
        put_string(name, &mut hello);
        hello.extend_from_slice(&[0x55; 16]);
        write_frame(&mut socket, &hello, None).await.unwrap();
        socket
    }
    async fn join(address: &str, name: &str) -> Self {
        let mut socket = Self::begin(address, name).await;
        let success = timeout(
            Duration::from_secs(5),
            read_frame(&mut socket, 65_536, None),
        )
        .await
        .unwrap()
        .unwrap();
        let mut view = success.as_slice();
        assert_eq!(get_varint(&mut view).unwrap(), 2);
        let uuid = Uuid::from_slice(take(&mut view, 16).unwrap()).unwrap();
        assert_ne!(uuid, Uuid::from_bytes([0x55; 16]));
        write_frame(&mut socket, &[3], None).await.unwrap();
        let mut known = read_frame(&mut socket, 65_536, None).await.unwrap();
        assert_eq!(known[0], 15);
        known[0] = 7;
        write_frame(&mut socket, &known, None).await.unwrap();
        loop {
            let packet = timeout(
                Duration::from_secs(5),
                read_frame(&mut socket, 2_097_151, None),
            )
            .await
            .unwrap()
            .unwrap();
            if packet == [3] {
                break;
            }
        }
        write_frame(&mut socket, &[3], None).await.unwrap();
        let snapshot = Snapshot {
            state: 0,
            slots: Vec::new(),
            cursor: None,
            selected: 0,
            content: Vec::new(),
            equipment: Vec::new(),
            hands: [None, None],
        };
        let mut client = Self {
            socket,
            uuid,
            entity: 0,
            snapshot,
        };
        loop {
            let (id, body) = client.read().await;
            client.inspect(id, &body).await;
            if id == 11 {
                break;
            }
        }
        assert_eq!(client.snapshot.slots.len(), 46);
        assert!(client.entity > 0);
        client
    }
    async fn read(&mut self) -> (i32, Vec<u8>) {
        loop {
            let packet = timeout(
                Duration::from_secs(5),
                read_frame(&mut self.socket, 2_097_151, None),
            )
            .await
            .unwrap()
            .unwrap();
            let mut view = packet.as_slice();
            let id = get_varint(&mut view).unwrap();
            if id == 45 {
                let mut answer = vec![28];
                answer.extend_from_slice(view);
                write_frame(&mut self.socket, &answer, None).await.unwrap();
                continue;
            }
            return (id, view.to_vec());
        }
    }
    async fn inspect(&mut self, id: i32, body: &[u8]) {
        let mut view = body;
        match id {
            50 => self.entity = i32::from_be_bytes(take(&mut view, 4).unwrap().try_into().unwrap()),
            73 => {
                let teleport = get_varint(&mut view).unwrap();
                let mut ack = vec![0];
                put_varint(teleport, &mut ack);
                write_frame(&mut self.socket, &ack, None).await.unwrap();
            }
            18 => {
                assert_eq!(get_varint(&mut view).unwrap(), 0);
                self.snapshot.state = get_varint(&mut view).unwrap();
                let count = get_varint(&mut view).unwrap();
                assert_eq!(count, 46);
                self.snapshot.slots = (0..count).map(|_| read_stack(&mut view)).collect();
                self.snapshot.cursor = read_stack(&mut view);
                assert!(view.is_empty());
                self.snapshot.content = body.to_vec();
            }
            107 => {
                self.snapshot.selected = get_varint(&mut view).unwrap() as u8;
                assert!(view.is_empty());
            }
            104 => {
                let entity = get_varint(&mut view).unwrap();
                let mut hands = [None, None];
                loop {
                    let slot = take(&mut view, 1).unwrap()[0];
                    let index = usize::from(slot & 0x7f);
                    assert!(index < 2);
                    hands[index] = read_stack(&mut view);
                    if slot & 0x80 == 0 {
                        break;
                    }
                }
                assert!(view.is_empty());
                if entity == self.entity {
                    self.snapshot.hands = hands;
                    self.snapshot.equipment = body.to_vec();
                }
            }
            _ => {}
        }
    }
    async fn sync(&mut self) -> Snapshot {
        let mut content = false;
        let mut held = false;
        loop {
            let (id, body) = self.read().await;
            self.inspect(id, &body).await;
            if id == 18 {
                content = true;
            }
            if content && id == 107 {
                held = true;
            }
            if content && held && id == 104 {
                return self.snapshot.clone();
            }
        }
    }
    async fn send(&mut self, id: i32, body: &[u8]) {
        let mut packet = Vec::new();
        put_varint(id, &mut packet);
        packet.extend_from_slice(body);
        write_frame(&mut self.socket, &packet, None).await.unwrap();
    }
    async fn creative(&mut self, slot: i16, count: u8) -> Snapshot {
        let mut body = slot.to_be_bytes().to_vec();
        body.extend_from_slice(&[count, 1, 0, 0]);
        self.send(57, &body).await;
        self.sync().await
    }
    async fn click(&mut self, slot: i16, button: u8, forged: bool) -> Snapshot {
        let mut body = vec![0, 0];
        body.extend_from_slice(&slot.to_be_bytes());
        body.extend_from_slice(&[button, 0]);
        if forged {
            body.extend_from_slice(&[1, 0, 36, 1, 1, 99, 0, 0, 1, 1, 99, 0, 0]);
        } else {
            body.extend_from_slice(&[0, 0]);
        }
        self.send(18, &body).await;
        self.sync().await
    }
    async fn action(&mut self, action: u8) -> Snapshot {
        let mut body = vec![action];
        body.extend_from_slice(&0_i64.to_be_bytes());
        body.extend_from_slice(&[0, 17]);
        self.send(41, &body).await;
        self.sync().await
    }
    async fn command(&mut self, command: &str) {
        let mut body = Vec::new();
        put_string(command, &mut body);
        self.send(7, &body).await;
    }
    async fn chat_containing(&mut self, text: &str) {
        loop {
            let (id, body) = self.read().await;
            self.inspect(id, &body).await;
            if id == 124 && body.windows(text.len()).any(|part| part == text.as_bytes()) {
                return;
            }
        }
    }
}

fn stone(count: i32) -> Option<Stack> {
    Some(Stack {
        item: 1,
        count,
        name: None,
    })
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 data"]
async fn native_inventory_creative_clicks_give_swap_drop_rejection_and_restart() {
    timeout(Duration::from_secs(60), async {
        let directory = tempfile::tempdir().unwrap();
        let mut server = Server::start(directory.path());
        let mut client = Client::join(&server.address, "Tester").await;
        assert_eq!(client.creative(36, 20).await.slots[36], stone(20));
        let mut duplicate = Client::begin(&server.address, "Tester").await;
        let response = read_frame(&mut duplicate, 65_536, None).await.unwrap();
        let mut view = response.as_slice();
        assert_eq!(get_varint(&mut view).unwrap(), 0);
        assert!(get_string(&mut view, 32767).unwrap().contains("already"));
        drop(duplicate);
        let picked = client.click(36, 0, true).await;
        assert!(picked.slots[36].is_none());
        assert_eq!(picked.cursor, stone(20));
        let split = client.click(37, 1, false).await;
        assert_eq!(split.slots[37], stone(1));
        assert_eq!(split.cursor, stone(19));
        let merged = client.click(37, 0, false).await;
        assert_eq!(merged.slots[37], stone(20));
        assert!(merged.cursor.is_none());
        client.send(54, &[0, 1]).await;
        let selected = client.sync().await;
        assert_eq!(selected.selected, 1);
        assert_eq!(selected.hands[0], stone(20));
        let swapped = client.action(7).await;
        assert!(swapped.slots[37].is_none());
        assert_eq!(swapped.slots[45], stone(20));
        assert_eq!(swapped.hands[1], stone(20));
        client.action(7).await;
        for action in [4, 5] {
            let snapshot = client.action(action).await;
            assert_eq!(snapshot.slots[37], stone(20));
        }
        client.command("give Tester minecraft:stone 5").await;
        client.chat_containing("Permission denied").await;
        server.command("op Tester");
        timeout(Duration::from_secs(5), async {
            loop {
                if std::fs::read_to_string(directory.path().join("ops.json"))
                    .is_ok_and(|text| text.contains(&client.uuid.to_string()))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        client.command("give Tester minecraft:stone 5").await;
        let given = client.sync().await;
        assert_eq!(given.slots[37], stone(25));
        assert_eq!(given.hands[0], stone(25));
        let uuid = client.uuid;
        assert!(
            directory
                .path()
                .join("world/playerdata")
                .join(format!("{uuid}.zinv"))
                .is_file()
        );
        assert!(
            !directory
                .path()
                .join("world/playerdata")
                .join(format!("{}.zinv", Uuid::from_bytes([0x55; 16])))
                .exists()
        );
        drop(client);
        server.stop().await;
        drop(server);
        let mut server = Server::start(directory.path());
        let mut client = Client::join(&server.address, "Tester").await;
        assert_eq!(client.uuid, uuid);
        assert_eq!(client.snapshot.selected, 1);
        assert_eq!(client.snapshot.slots[37], stone(25));
        assert_eq!(client.snapshot.hands[0], stone(25));
        let bad = [0, 36, 1, 1, 1, 0, 6, 2, 10, 0];
        client.send(57, &bad).await;
        let rejected = client.sync().await;
        assert_eq!(rejected.slots[37], stone(25));
        assert!(rejected.slots[36].is_none());
        client.command("tps").await;
        client.chat_containing("TPS").await;
        drop(client);
        server.stop().await;
    })
    .await
    .expect("native inventory scenario timed out");
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 data"]
async fn native_inventory_emits_original_codec_fixtures_and_preserves_typed_names() {
    let directory = tempfile::tempdir().unwrap();
    let mut server = Server::start(directory.path());
    let mut client = Client::join(&server.address, "Named").await;
    client
        .send(
            57,
            &[0, 36, 1, 1, 1, 0, 6, 7, 8, 0, 4, b'S', b'a', b'f', b'e'],
        )
        .await;
    let snapshot = client.sync().await;
    assert_eq!(
        snapshot.slots[36],
        Some(Stack {
            item: 1,
            count: 1,
            name: Some("Safe".into())
        })
    );
    let stack_json = |stack: &Option<Stack>| match stack {
        None => serde_json::Value::Null,
        Some(stack) => {
            serde_json::json!({"item":"minecraft:stone","count":stack.count,"added":stack.name.as_ref().map_or_else(||serde_json::json!({}),|name|serde_json::json!({"minecraft:custom_name":name})),"removed":[]})
        }
    };
    let to_hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    let fixture = serde_json::json!({"version":"26.3","protocol":777,"fixtures":[
        {"name":"native_inventory_content","kind":"content","body_hex":to_hex(&snapshot.content),"expected":{"container_id":0,"state_id":snapshot.state,"items":snapshot.slots.iter().map(stack_json).collect::<Vec<_>>(),"carried":stack_json(&snapshot.cursor)}},
        {"name":"native_equipment","kind":"equipment","body_hex":to_hex(&snapshot.equipment),"expected":{"entity_id":client.entity,"slots":[{"slot":0,"stack":stack_json(&snapshot.hands[0])},{"slot":1,"stack":stack_json(&snapshot.hands[1])}]}}
    ]});
    let output =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".cache/native-inventory-fixtures.json");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(output, serde_json::to_vec_pretty(&fixture).unwrap()).unwrap();
    drop(client);
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 data"]
async fn inventory_rate_limit_keeps_connection_alive_and_give_requires_level_two() {
    let directory = tempfile::tempdir().unwrap();
    let mut server = Server::start(directory.path());
    let mut client = Client::join(&server.address, "Limited").await;
    let frame = [9, 18, 0, 0, 0, 9, 0, 0, 0, 0];
    let mut flood = Vec::new();
    for _ in 0..128 {
        flood.extend_from_slice(&frame);
    }
    client.socket.write_all(&flood).await.unwrap();
    client.chat_containing("rate limited").await;
    client.command("tps").await;
    client.chat_containing("TPS").await;
    let uuid = client.uuid;
    drop(client);
    server.stop().await;
    drop(server);
    for level in [1, 2] {
        let ops = serde_json::json!([{"uuid":uuid,"name":"Limited","level":level,"bypassesPlayerLimit":false}]);
        std::fs::write(
            directory.path().join("ops.json"),
            serde_json::to_vec(&ops).unwrap(),
        )
        .unwrap();
        let mut server = Server::start(directory.path());
        let mut client = Client::join(&server.address, "Limited").await;
        client.command("give Limited stone 2").await;
        if level == 1 {
            client.chat_containing("Permission denied").await;
        } else {
            assert_eq!(client.sync().await.slots[36], stone(2));
        }
        drop(client);
        server.stop().await;
    }
}
