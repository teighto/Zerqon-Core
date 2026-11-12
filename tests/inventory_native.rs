use std::{
    collections::BTreeMap,
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
use zerqon_world::{ChunkKey, MAX_PLAYER_RECORD_BYTES, PlayerKey, WorldStore};

struct Server {
    child: Child,
    address: String,
    lines: std::sync::mpsc::Receiver<String>,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Server {
    fn start(directory: &Path) -> Self {
        Self::start_configured(directory, None)
    }

    fn start_in_mode(directory: &Path, mode: &str) -> Self {
        assert!(matches!(mode, "creative" | "survival"));
        Self::start_configured(directory, Some(mode))
    }

    fn start_configured(directory: &Path, mode: Option<&str>) -> Self {
        let data = Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/vanilla/26.3");
        assert!(
            data.join("prepared.json").is_file(),
            "prepare official26.3 data"
        );
        let exe = std::env::var_os("ZERQON_TEST_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_zerqon")));
        if !directory.join("eula.txt").exists() {
            assert!(
                Command::new(&exe)
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
        if let Some(mode) = mode {
            let path = directory.join("server.properties");
            let previous = std::fs::read_to_string(&path).unwrap();
            let mut properties = previous
                .lines()
                .filter(|line| !line.starts_with("gamemode="))
                .collect::<Vec<_>>()
                .join("\n");
            properties.push_str(&format!("\ngamemode={mode}\n"));
            std::fs::write(path, properties).unwrap();
        }
        let mut child = Command::new(&exe)
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
        let (line_sender, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some((_, address)) = line.split_once("Listening on ") {
                    let _ = send.send(address.trim().to_owned());
                }
                let _ = line_sender.send(line);
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
        Self {
            child,
            address,
            lines,
        }
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

    async fn player_left(&self, name: &str) {
        timeout(Duration::from_secs(10), async {
            loop {
                while let Ok(line) = self.lines.try_recv() {
                    if line.contains("Player left") && line.contains(name) {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("owned server did not finish the disconnected player session");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stack {
    item: i32,
    count: i32,
    name: Option<String>,
    damage: u32,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LoginMode {
    current: i32,
    previous: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Abilities {
    flags: u8,
    flying_speed: f32,
    walking_speed: f32,
}

#[derive(Debug, Default)]
struct TabEntry {
    name: Option<String>,
    mode: Option<i32>,
}

struct Client {
    position: [f64; 3],
    socket: TcpStream,
    uuid: Uuid,
    entity: i32,
    snapshot: Snapshot,
    login_mode: Option<LoginMode>,
    abilities: Option<Abilities>,
    tab: BTreeMap<Uuid, TabEntry>,
    block_updates: BTreeMap<[i32; 3], i32>,
    health: Option<f32>,
}

fn read_bool(body: &mut &[u8]) -> bool {
    match take(body, 1).unwrap()[0] {
        0 => false,
        1 => true,
        value => panic!("invalid boolean byte {value}"),
    }
}

fn read_block_position(body: &mut &[u8]) -> [i32; 3] {
    let packed = i64::from_be_bytes(take(body, 8).unwrap().try_into().unwrap());
    [
        (packed >> 38) as i32,
        (packed << 52 >> 52) as i32,
        (packed << 26 >> 38) as i32,
    ]
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
    let mut damage = 0;
    for _ in 0..added {
        let kind = get_varint(data).unwrap();
        if kind == 3 {
            damage = u32::try_from(get_varint(data).unwrap()).unwrap();
            continue;
        }
        assert_eq!(kind, 6, "unexpected component in native test");
        assert_eq!(take(data, 1).unwrap(), [8]);
        let length = u16::from_be_bytes(take(data, 2).unwrap().try_into().unwrap()) as usize;
        name = Some(String::from_utf8(take(data, length).unwrap().to_vec()).unwrap());
    }
    Some(Stack {
        item,
        count,
        name,
        damage,
    })
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
        assert_eq!(
            read_frame(&mut socket, 65_536, None).await.unwrap(),
            b"\x01\x10horizon:mod_list\x00"
        );
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
            position: [0.0; 3],
            socket,
            uuid,
            entity: 0,
            snapshot,
            login_mode: None,
            abilities: None,
            tab: BTreeMap::new(),
            block_updates: BTreeMap::new(),
            health: None,
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
        assert!(client.login_mode.is_some());
        assert!(client.abilities.is_some());
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
            106 => {
                let health = f32::from_be_bytes(take(&mut view, 4).unwrap().try_into().unwrap());
                assert!(health.is_finite() && (0.0..=20.0).contains(&health));
                assert!((0..=20).contains(&get_varint(&mut view).unwrap()));
                let saturation =
                    f32::from_be_bytes(take(&mut view, 4).unwrap().try_into().unwrap());
                assert!(saturation.is_finite() && (0.0..=20.0).contains(&saturation));
                assert!(view.is_empty());
                self.health = Some(health);
            }
            50 => {
                self.entity = i32::from_be_bytes(take(&mut view, 4).unwrap().try_into().unwrap());
                assert!(!read_bool(&mut view));
                let dimensions = get_varint(&mut view).unwrap();
                assert_eq!(dimensions, 1);
                assert_eq!(get_string(&mut view, 32767).unwrap(), "minecraft:overworld");
                for expected in [4, 1, 1] {
                    assert_eq!(get_varint(&mut view).unwrap(), expected);
                }
                assert!(!read_bool(&mut view));
                assert!(read_bool(&mut view));
                assert!(!read_bool(&mut view));
                assert!(get_varint(&mut view).unwrap() >= 0);
                assert_eq!(get_string(&mut view, 32767).unwrap(), "minecraft:overworld");
                take(&mut view, 8).unwrap();
                let current = get_varint(&mut view).unwrap();
                assert!(matches!(current, 0 | 1));
                let previous = match get_varint(&mut view).unwrap() {
                    0 => None,
                    1 => Some(0),
                    2 => Some(1),
                    value => panic!("unsupported previous game mode {value}"),
                };
                read_bool(&mut view);
                read_bool(&mut view);
                if read_bool(&mut view) {
                    get_string(&mut view, 32767).unwrap();
                    read_block_position(&mut view);
                }
                assert!(get_varint(&mut view).unwrap() >= 0);
                get_varint(&mut view).unwrap();
                assert!(!read_bool(&mut view));
                assert!(!read_bool(&mut view));
                assert!(view.is_empty());
                self.login_mode = Some(LoginMode { current, previous });
            }
            65 => {
                let flags = take(&mut view, 1).unwrap()[0];
                assert_eq!(flags & !0x0f, 0);
                let flying_speed =
                    f32::from_be_bytes(take(&mut view, 4).unwrap().try_into().unwrap());
                let walking_speed =
                    f32::from_be_bytes(take(&mut view, 4).unwrap().try_into().unwrap());
                assert!(view.is_empty());
                assert!(flying_speed.is_finite() && (0.05..=0.5).contains(&flying_speed));
                assert_eq!(walking_speed, 0.1);
                self.abilities = Some(Abilities {
                    flags,
                    flying_speed,
                    walking_speed,
                });
            }
            71 => {
                let actions = take(&mut view, 1).unwrap()[0];
                assert_eq!(actions & !0x9d, 0, "unsupported TAB action");
                let count = get_varint(&mut view).unwrap();
                assert!((0..=4).contains(&count));
                for _ in 0..count {
                    let uuid = Uuid::from_slice(take(&mut view, 16).unwrap()).unwrap();
                    let entry = self.tab.entry(uuid).or_default();
                    if actions & 1 != 0 {
                        entry.name = Some(get_string(&mut view, 16).unwrap().to_owned());
                        let properties = get_varint(&mut view).unwrap();
                        assert!((0..=16).contains(&properties));
                        for _ in 0..properties {
                            get_string(&mut view, 32767).unwrap();
                            get_string(&mut view, 32767).unwrap();
                            if read_bool(&mut view) {
                                get_string(&mut view, 32767).unwrap();
                            }
                        }
                    }
                    if actions & 4 != 0 {
                        let mode = get_varint(&mut view).unwrap();
                        assert!(matches!(mode, 0 | 1));
                        entry.mode = Some(mode);
                    }
                    if actions & 8 != 0 {
                        assert!(read_bool(&mut view));
                    }
                    if actions & 16 != 0 {
                        assert!(get_varint(&mut view).unwrap() >= 0);
                    }
                    if actions & 128 != 0 {
                        assert!(read_bool(&mut view));
                    }
                }
                assert!(view.is_empty());
            }
            70 => {
                let count = get_varint(&mut view).unwrap();
                assert!((0..=4).contains(&count));
                for _ in 0..count {
                    let uuid = Uuid::from_slice(take(&mut view, 16).unwrap()).unwrap();
                    self.tab.remove(&uuid);
                }
                assert!(view.is_empty());
            }
            8 => {
                let position = read_block_position(&mut view);
                let state = get_varint(&mut view).unwrap();
                assert!(state >= 0);
                assert!(view.is_empty());
                self.block_updates.insert(position, state);
            }
            73 => {
                let teleport = get_varint(&mut view).unwrap();
                self.position = std::array::from_fn(|axis| {
                    f64::from_be_bytes(view[axis * 8..axis * 8 + 8].try_into().unwrap())
                });
                let mut ack = vec![0];
                put_varint(teleport, &mut ack);
                ack.extend_from_slice(&view[..24]);
                ack.extend_from_slice(&view[48..56]);
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

    async fn creative_stack(&mut self, slot: i16, stack: Option<Stack>) -> Snapshot {
        let mut body = slot.to_be_bytes().to_vec();
        if let Some(stack) = stack {
            assert!(stack.name.is_none(), "crafting fixture uses default stacks");
            put_varint(stack.count, &mut body);
            put_varint(stack.item, &mut body);
            body.extend_from_slice(&[0, 0]);
        } else {
            body.push(0);
        }
        self.send(57, &body).await;
        self.sync().await
    }

    async fn crafting_click(
        &mut self,
        slot: i16,
        button: u8,
        mode: i32,
        state: i32,
        forged: bool,
    ) -> Snapshot {
        self.send(18, &crafting_click_body(slot, button, mode, state, forged))
            .await;
        self.sync().await
    }

    async fn ordered_snapshot(&mut self) -> Snapshot {
        // Zerqon awaits each inventory mutation and writes its entire snapshot
        // before dispatching the later command. Commands also use the ordered
        // deferred lane; this is a Zerqon fence, not a vanilla command barrier.
        self.command("tps").await;
        self.chat_containing("TPS").await;
        self.snapshot.clone()
    }

    async fn close_crafting(&mut self) -> Snapshot {
        self.send(19, &[0]).await;
        self.ordered_snapshot().await
    }

    async fn tab_mode(&mut self, uuid: Uuid, name: &str) -> i32 {
        loop {
            if let Some(entry) = self.tab.get(&uuid)
                && entry.name.as_deref() == Some(name)
                && let Some(mode) = entry.mode
            {
                return mode;
            }
            let (id, body) = self.read().await;
            self.inspect(id, &body).await;
        }
    }

    async fn flight_intent(&mut self, flags: u8) -> Abilities {
        self.send(40, &[flags]).await;
        self.next_abilities().await
    }

    async fn next_abilities(&mut self) -> Abilities {
        loop {
            let (id, body) = self.read().await;
            self.inspect(id, &body).await;
            if id == 65 {
                return self.abilities.unwrap();
            }
        }
    }

    async fn rejected_block_action(
        &mut self,
        id: i32,
        body: &[u8],
        sequence: i32,
        corrections: &[([i32; 3], i32)],
    ) {
        self.block_updates.clear();
        self.send(id, body).await;
        let mut rejected = false;
        loop {
            let (id, body) = self.read().await;
            self.inspect(id, &body).await;
            if id == 124 {
                rejected |= body
                    .windows(b"Block interaction rejected".len())
                    .any(|part| part == b"Block interaction rejected");
            }
            if id == 4 {
                let mut remaining = body.as_slice();
                assert_eq!(get_varint(&mut remaining).unwrap(), sequence);
                assert!(remaining.is_empty());
                assert!(rejected, "action was not rejected by the Survival policy");
                assert_eq!(
                    self.block_updates,
                    corrections.iter().copied().collect::<BTreeMap<_, _>>(),
                    "authoritative correction did not precede its block ACK"
                );
                return;
            }
        }
    }
}

fn assert_login_mode(client: &Client, mode: i32, flags: u8) {
    assert_eq!(
        client.login_mode,
        Some(LoginMode {
            current: mode,
            previous: None
        })
    );
    assert_eq!(
        client.abilities,
        Some(Abilities {
            flags,
            flying_speed: 0.05,
            walking_speed: 0.1
        })
    );
}

fn block_position_bytes([x, y, z]: [i32; 3]) -> [u8; 8] {
    (((i64::from(x) & 0x3ffffff) << 38)
        | ((i64::from(z) & 0x3ffffff) << 12)
        | (i64::from(y) & 0xfff))
        .to_be_bytes()
}

fn destroy_body(action: i32, block: [i32; 3], sequence: i32) -> Vec<u8> {
    let mut body = Vec::new();
    put_varint(action, &mut body);
    body.extend_from_slice(&block_position_bytes(block));
    body.push(1);
    put_varint(sequence, &mut body);
    body
}

fn place_body(block: [i32; 3], sequence: i32) -> Vec<u8> {
    let mut body = vec![0];
    body.extend_from_slice(&block_position_bytes(block));
    body.push(1);
    for value in [0.5_f32, 1.0, 0.5] {
        body.extend_from_slice(&value.to_be_bytes());
    }
    body.extend_from_slice(&[0, 0]);
    put_varint(sequence, &mut body);
    body
}

fn stone(count: i32) -> Option<Stack> {
    Some(Stack {
        item: 1,
        count,
        name: None,
        damage: 0,
    })
}

// IDs independently exported from the pinned original 26.3 item registry in
// tools/crafting-menu-oracle/oracle.json (protocol 777).
const OAK_LOG: i32 = 163;
const OAK_PLANKS: i32 = 63;
const HONEY_BOTTLE: i32 = 1533;
const HONEY_BLOCK: i32 = 832;
const GLASS_BOTTLE: i32 = 1270;

fn item_stack(item: i32, count: i32) -> Option<Stack> {
    Some(Stack {
        item,
        count,
        name: None,
        damage: 0,
    })
}

fn crafting_click_body(slot: i16, button: u8, mode: i32, state: i32, forged: bool) -> Vec<u8> {
    let mut body = Vec::new();
    put_varint(0, &mut body);
    put_varint(state, &mut body);
    body.extend_from_slice(&slot.to_be_bytes());
    body.push(button);
    put_varint(mode, &mut body);
    if forged {
        // Well-formed but fabricated result-slot and cursor predictions. The
        // server must execute the click using its real inputs and output.
        body.push(1);
        body.extend_from_slice(&0_i16.to_be_bytes());
        body.extend_from_slice(&[1, 1, 64, 0, 0]);
        body.extend_from_slice(&[1, 1, 64, 0, 0]);
    } else {
        body.extend_from_slice(&[0, 0]);
    }
    body
}

fn real_count(snapshot: &Snapshot, item: i32) -> i32 {
    snapshot
        .slots
        .iter()
        .skip(1) // The derived result is not owned item value.
        .chain(std::iter::once(&snapshot.cursor))
        .flatten()
        .filter(|stack| stack.item == item)
        .map(|stack| stack.count)
        .sum()
}

fn assert_same_inventory(actual: &Snapshot, expected: &Snapshot) {
    assert_same_items(actual, expected);
    assert_eq!(
        actual.state, expected.state,
        "rejected mutation changed state ID"
    );
}

fn assert_same_items(actual: &Snapshot, expected: &Snapshot) {
    assert_eq!(
        actual.slots, expected.slots,
        "real or preview slots changed"
    );
    assert_eq!(actual.cursor, expected.cursor, "cursor changed");
    assert_eq!(actual.selected, expected.selected, "selection changed");
}

struct SavedInventory {
    payload: Vec<u8>,
    inventory: serde_json::Value,
}

fn saved_inventory(directory: &Path, uuid: Uuid) -> SavedInventory {
    saved_inventory_in_mode(directory, uuid, "creative")
}

fn saved_inventory_in_mode(directory: &Path, uuid: Uuid, mode: &str) -> SavedInventory {
    let world = WorldStore::open(directory.join("world")).unwrap();
    let snapshot = world
        .snapshot_domain(&[], &[PlayerKey::new(uuid)], &[])
        .unwrap();
    assert_eq!(snapshot.players.len(), 1);
    assert_eq!(snapshot.players[0].key(), PlayerKey::new(uuid));
    assert!(snapshot.players[0].revision() > 0);
    let payload = snapshot.players[0].data().unwrap().to_vec();
    assert!((1..=MAX_PLAYER_RECORD_BYTES).contains(&payload.len()));
    let record: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(record["schema"], 1);
    assert_eq!(record["world"], world.uuid().unwrap().to_string());
    assert_eq!(record["game_mode"], mode);
    assert!(record["previous_game_mode"].is_null());
    let inventory = record["inventory"].clone();
    assert_eq!(inventory["schema"], 2);
    assert_eq!(inventory["minecraft_version"], "26.3");
    assert_eq!(inventory["protocol"], 777);
    assert_eq!(inventory["uuid"], uuid.to_string());
    assert_eq!(inventory["slots"].as_array().unwrap().len(), 46);
    assert!(
        inventory["slots"][0].is_null(),
        "derived result was persisted as item value"
    );
    SavedInventory { payload, inventory }
}

fn saved_count(saved: &SavedInventory, item: &str) -> u64 {
    saved.inventory["slots"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .chain(std::iter::once(&saved.inventory["cursor"]))
        .filter(|stack| stack["item"].as_str() == Some(item))
        .map(|stack| stack["count"].as_u64().unwrap())
        .sum()
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 data"]
async fn native_inventory_creative_clicks_give_swap_drop_and_restart() {
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
        assert_eq!(client.action(5).await.slots[37], stone(19));
        assert!(client.action(4).await.slots[37].is_none());
        assert!(client.action(5).await.slots[37].is_none());
        client.creative_stack(37, stone(20)).await;
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
            !directory
                .path()
                .join("world/playerdata")
                .join(format!("{uuid}.zinv"))
                .exists(),
            "fresh player created a second inventory authority"
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
        let saved = saved_inventory(directory.path(), uuid);
        assert_eq!(saved.inventory["selected"], 1);
        assert_eq!(saved.inventory["slots"][37]["item"], "minecraft:stone");
        assert_eq!(saved.inventory["slots"][37]["count"], 25);
        let world = WorldStore::open(directory.path().join("world")).unwrap();
        let forged = world
            .snapshot_domain(&[], &[PlayerKey::new(Uuid::from_bytes([0x55; 16]))], &[])
            .unwrap();
        assert!(forged.players[0].data().is_none());
        assert_eq!(forged.players[0].revision(), 0);
        drop(world);
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
            name: Some("Safe".into()),
            damage: 0,
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

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 crafting data"]
async fn native_crafting_result_shift_predictions_and_stale_revisions() {
    timeout(Duration::from_secs(60), async {
        let directory = tempfile::tempdir().unwrap();
        let mut server = Server::start(directory.path());
        let mut client = Client::join(&server.address, "CraftClicks").await;
        let seeded = client.creative_stack(1, item_stack(OAK_LOG, 3)).await;
        assert_eq!(seeded.slots[0], item_stack(OAK_PLANKS, 4));
        assert_eq!(real_count(&seeded, OAK_PLANKS), 0);
        let denied = client.creative_stack(0, stone(64)).await;
        assert_same_inventory(&denied, &seeded);

        let stale = seeded.state;
        let first = client.crafting_click(0, 0, 0, stale, true).await;
        assert_eq!(first.cursor, item_stack(OAK_PLANKS, 4));
        assert_eq!(first.slots[1], item_stack(OAK_LOG, 2));
        assert_eq!(
            real_count(&first, 1),
            0,
            "forged stone prediction became item value"
        );
        let second = client.crafting_click(0, 1, 0, stale, false).await;
        assert_eq!(second.cursor, item_stack(OAK_PLANKS, 8));
        assert_eq!(second.slots[1], item_stack(OAK_LOG, 1));
        let third = client.crafting_click(0, 0, 0, stale, false).await;
        assert_eq!(third.cursor, item_stack(OAK_PLANKS, 12));
        assert!(third.slots[..5].iter().all(Option::is_none));
        let exhausted = client.crafting_click(0, 0, 0, stale, true).await;
        assert_eq!(exhausted.cursor, item_stack(OAK_PLANKS, 12));
        assert_eq!(real_count(&exhausted, OAK_PLANKS), 12);
        assert_eq!(real_count(&exhausted, 1), 0);

        let seeded = client.creative_stack(1, item_stack(OAK_LOG, 2)).await;
        let shifted = client.crafting_click(0, 0, 1, seeded.state, false).await;
        assert!(shifted.slots[..5].iter().all(Option::is_none));
        assert_eq!(shifted.slots[44], item_stack(OAK_PLANKS, 8));
        assert_eq!(shifted.cursor, item_stack(OAK_PLANKS, 12));
        assert_eq!(real_count(&shifted, OAK_PLANKS), 20);
        let uuid = client.uuid;
        drop(client);
        server.stop().await;
        drop(server);
        let saved = saved_inventory(directory.path(), uuid);
        assert_eq!(saved_count(&saved, "minecraft:oak_planks"), 20);
        assert_eq!(saved_count(&saved, "minecraft:oak_log"), 0);
        assert_eq!(saved_count(&saved, "minecraft:stone"), 0);
        assert!(saved.inventory["cursor"].is_null());
    })
    .await
    .expect("native crafting click scenario timed out");
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 crafting data"]
async fn native_crafting_honey_remainders_return_to_inputs_and_inventory() {
    timeout(Duration::from_secs(60), async {
        let directory = tempfile::tempdir().unwrap();
        let mut server = Server::start(directory.path());
        let mut client = Client::join(&server.address, "CraftHoney").await;
        for slot in 1..=4 {
            client
                .creative_stack(slot, item_stack(HONEY_BOTTLE, 2))
                .await;
        }
        assert_eq!(client.snapshot.slots[0], item_stack(HONEY_BLOCK, 1));
        let first = client
            .crafting_click(0, 1, 0, client.snapshot.state, false)
            .await;
        assert_eq!(first.cursor, item_stack(HONEY_BLOCK, 1));
        assert!(
            first.slots[1..5]
                .iter()
                .all(|slot| *slot == item_stack(HONEY_BOTTLE, 1))
        );
        assert_eq!(first.slots[36], item_stack(GLASS_BOTTLE, 4));
        let second = client.crafting_click(0, 0, 0, first.state, false).await;
        assert!(second.slots[0].is_none());
        assert_eq!(second.cursor, item_stack(HONEY_BLOCK, 2));
        assert!(
            second.slots[1..5]
                .iter()
                .all(|slot| *slot == item_stack(GLASS_BOTTLE, 1))
        );
        assert_eq!(real_count(&second, HONEY_BOTTLE), 0);
        assert_eq!(real_count(&second, HONEY_BLOCK), 2);
        assert_eq!(real_count(&second, GLASS_BOTTLE), 8);
        let uuid = client.uuid;
        drop(client);
        server.stop().await;
        drop(server);
        let saved = saved_inventory(directory.path(), uuid);
        assert_eq!(saved_count(&saved, "minecraft:honey_bottle"), 0);
        assert_eq!(saved_count(&saved, "minecraft:honey_block"), 2);
        assert_eq!(saved_count(&saved, "minecraft:glass_bottle"), 8);
        assert!(saved.inventory["cursor"].is_null());
    })
    .await
    .expect("native honey remainder scenario timed out");
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 crafting data"]
async fn native_crafting_overflow_creates_durable_items_and_closes_without_loss() {
    timeout(Duration::from_secs(45), async {
        let directory = tempfile::tempdir().unwrap();
        let mut server = Server::start(directory.path());
        let mut client = Client::join(&server.address, "CraftFull").await;
        for slot in 9..=44 {
            if slot == 33 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            client.creative_stack(slot, stone(64)).await;
        }
        client.creative_stack(44, item_stack(OAK_PLANKS, 62)).await;
        client.creative_stack(1, item_stack(OAK_LOG, 1)).await;
        let uuid = client.uuid;
        drop(client);
        server.player_left("CraftFull").await;
        server.stop().await;
        drop(server);
        let saved = saved_inventory(directory.path(), uuid);
        assert_eq!(saved_count(&saved, "minecraft:stone"), 2240);
        assert_eq!(saved_count(&saved, "minecraft:oak_planks"), 62);
        assert_eq!(saved_count(&saved, "minecraft:oak_log"), 0);
        assert!(saved.inventory["slots"][1].is_null());
        let mut server = Server::start(directory.path());
        let mut client = Client::join(&server.address, "CraftFull").await;
        assert!(client.snapshot.slots[..5].iter().all(Option::is_none));
        let before = client.creative_stack(1, item_stack(OAK_LOG, 1)).await;
        assert_eq!(before.slots[0], item_stack(OAK_PLANKS, 4));
        let crafted = client.crafting_click(0, 0, 1, before.state, false).await;
        assert_eq!(real_count(&crafted, OAK_PLANKS), 64);
        assert_eq!(real_count(&crafted, OAK_LOG), 0);
        assert!(crafted.slots[..5].iter().all(Option::is_none));
        client.close_crafting().await;
        drop(client);
        server.player_left("CraftFull").await;
        server.stop().await;
        drop(server);
        let saved = saved_inventory(directory.path(), uuid);
        assert_eq!(saved_count(&saved, "minecraft:oak_planks"), 64);
        assert_eq!(saved_count(&saved, "minecraft:stone"), 2240);
        let world = WorldStore::open(directory.path().join("world")).unwrap();
        let rows = world.scan_drops(None, 64).unwrap();
        let mut contents = BTreeMap::<String, i64>::new();
        for row in rows {
            if let Some(bytes) = row.data() {
                let drop: serde_json::Value = serde_json::from_slice(bytes).unwrap();
                *contents
                    .entry(drop["stack"]["item"].as_str().unwrap().into())
                    .or_default() += drop["stack"]["count"].as_i64().unwrap();
            }
        }
        assert_eq!(contents.get("minecraft:oak_log"), Some(&1));
        assert_eq!(contents.get("minecraft:oak_planks"), Some(&2));
    })
    .await
    .expect("crafting overflow scenario timed out");
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 crafting data"]
async fn native_crafting_close_returns_real_items_and_survives_reconnect_restart() {
    timeout(Duration::from_secs(90), async {
        let directory = tempfile::tempdir().unwrap();
        let mut server = Server::start(directory.path());
        let mut client = Client::join(&server.address, "CraftClose").await;
        let seeded = client.creative_stack(1, item_stack(OAK_LOG, 3)).await;
        let uuid = client.uuid;
        assert_eq!(seeded.slots[1], item_stack(OAK_LOG, 3));
        assert_eq!(seeded.slots[0], item_stack(OAK_PLANKS, 4));
        let carrying = client.crafting_click(0, 0, 0, seeded.state, false).await;
        assert_eq!(carrying.cursor, item_stack(OAK_PLANKS, 4));
        assert_eq!(carrying.slots[1], item_stack(OAK_LOG, 2));
        for invalid_close in [&[1][..], &[0, 0][..]] {
            client.send(19, invalid_close).await;
            let rejected = client.sync().await;
            assert_same_inventory(&rejected, &carrying);
            assert_eq!(rejected.content, carrying.content);
        }

        // Queue the close immediately behind the craft. A later ordered command
        // fences both; no response assumption can hide an ignored close packet.
        client
            .send(18, &crafting_click_body(0, 0, 0, carrying.state, false))
            .await;
        let closed = client.close_crafting().await;
        assert!(
            closed.slots[..5].iter().all(Option::is_none),
            "close left the grid or virtual result populated"
        );
        assert!(closed.cursor.is_none());
        assert_eq!(closed.slots[36], item_stack(OAK_PLANKS, 8));
        assert_eq!(closed.slots[37], item_stack(OAK_LOG, 1));
        assert_eq!(
            real_count(&closed, OAK_PLANKS),
            8,
            "the virtual preview was duplicated"
        );
        assert_eq!(real_count(&closed, OAK_LOG), 1);
        drop(client);
        server.player_left("CraftClose").await;
        let client = Client::join(&server.address, "CraftClose").await;
        assert_eq!(client.snapshot.slots, closed.slots);
        assert_eq!(client.snapshot.cursor, closed.cursor);
        drop(client);
        server.player_left("CraftClose").await;
        server.stop().await;
        drop(server);
        let saved_closed = saved_inventory(directory.path(), uuid);
        assert!(saved_closed.inventory["cursor"].is_null());
        assert!(
            saved_closed.inventory["slots"].as_array().unwrap()[..5]
                .iter()
                .all(serde_json::Value::is_null)
        );
        assert_eq!(saved_count(&saved_closed, "minecraft:oak_planks"), 8);
        assert_eq!(saved_count(&saved_closed, "minecraft:oak_log"), 1);

        let mut server = Server::start(directory.path());
        let client = Client::join(&server.address, "CraftClose").await;
        assert_eq!(client.uuid, uuid);
        assert_eq!(client.snapshot.slots, closed.slots);
        assert_eq!(real_count(&client.snapshot, OAK_PLANKS), 8);
        assert_eq!(real_count(&client.snapshot, OAK_LOG), 1);
        drop(client);
        server.stop().await;
        drop(server);
        assert_eq!(
            saved_inventory(directory.path(), uuid).payload,
            saved_closed.payload,
            "reconnect or restart changed the closed crafting inventory"
        );
    })
    .await
    .expect("native crafting close/reconnect/restart scenario timed out");
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 crafting data"]
async fn native_crafting_eof_finalizes_grid_and_cursor_before_same_uuid_reconnect() {
    timeout(Duration::from_secs(60), async {
        let directory = tempfile::tempdir().unwrap();
        let mut server = Server::start(directory.path());
        let mut client = Client::join(&server.address, "CraftEOF").await;
        let seeded = client.creative_stack(1, item_stack(OAK_LOG, 2)).await;
        let committed = client.crafting_click(0, 0, 0, seeded.state, false).await;
        assert_eq!(committed.cursor, item_stack(OAK_PLANKS, 4));
        assert_eq!(committed.slots[1], item_stack(OAK_LOG, 1));
        let uuid = client.uuid;
        drop(client);
        // This lifecycle event means the former session entered closing. The
        // login fence must then wait for its actual finalization, not race a sleep.
        server.player_left("CraftEOF").await;
        let client = Client::join(&server.address, "CraftEOF").await;
        assert_eq!(client.uuid, uuid);
        assert!(
            client.snapshot.slots[..5].iter().all(Option::is_none),
            "EOF did not return crafting inputs"
        );
        assert!(
            client.snapshot.cursor.is_none(),
            "EOF retained the carried result despite free capacity"
        );
        assert_eq!(client.snapshot.slots[36], item_stack(OAK_PLANKS, 4));
        assert_eq!(client.snapshot.slots[37], item_stack(OAK_LOG, 1));
        assert_eq!(real_count(&client.snapshot, OAK_PLANKS), 4);
        assert_eq!(real_count(&client.snapshot, OAK_LOG), 1);
        drop(client);
        server.stop().await;
        drop(server);
        let saved = saved_inventory(directory.path(), uuid);
        assert!(saved.inventory["cursor"].is_null());
        assert!(
            saved.inventory["slots"].as_array().unwrap()[..5]
                .iter()
                .all(serde_json::Value::is_null)
        );
        assert_eq!(saved_count(&saved, "minecraft:oak_planks"), 4);
        assert_eq!(saved_count(&saved, "minecraft:oak_log"), 1);
    })
    .await
    .expect("native crafting EOF/reconnect scenario timed out");
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 data"]
async fn native_survival_policy_blocks_forged_privileges_and_unfinished_digs_but_allows_crafting() {
    timeout(Duration::from_secs(90), async {
        let directory = tempfile::tempdir().unwrap();
        let mut server = Server::start_in_mode(directory.path(), "survival");
        let mut client = Client::join(&server.address, "Survival").await;
        let mut observer = Client::join(&server.address, "Observer").await;
        assert_login_mode(&client, 0, 0);
        assert_login_mode(&observer, 0, 0);
        let uuid = client.uuid;
        let observer_uuid = observer.uuid;
        assert_eq!(client.tab_mode(uuid, "Survival").await, 0);
        assert_eq!(client.tab_mode(observer_uuid, "Observer").await, 0);
        assert_eq!(observer.tab_mode(uuid, "Survival").await, 0);
        assert_eq!(observer.tab_mode(observer_uuid, "Observer").await, 0);

        let before = client.snapshot.clone();
        assert_eq!(client.flight_intent(15).await.flags, 0);
        assert_eq!(client.flight_intent(255).await.flags, 0);
        assert_same_inventory(&client.ordered_snapshot().await, &before);
        let rejected = client.creative_stack(36, stone(64)).await;
        assert_same_inventory(&rejected, &before);

        server.command("give Survival minecraft:oak_log 2");
        let supplied = client.sync().await;
        assert_eq!(supplied.slots[36], item_stack(OAK_LOG, 2));
        let cloned = client.crafting_click(36, 2, 3, supplied.state, false).await;
        assert_same_items(&cloned, &supplied);
        assert_eq!(real_count(&cloned, OAK_LOG), 2);
        let carrying = client.crafting_click(36, 0, 0, cloned.state, false).await;
        assert_eq!(carrying.cursor, item_stack(OAK_LOG, 2));
        for (slot, button) in [(-999, 8), (9, 9), (-999, 10)] {
            let rejected = client
                .crafting_click(slot, button, 5, carrying.state, false)
                .await;
            assert_same_inventory(&rejected, &carrying);
        }
        let grid = client.crafting_click(1, 0, 0, carrying.state, false).await;
        assert_eq!(grid.slots[1], item_stack(OAK_LOG, 2));
        assert_eq!(grid.slots[0], item_stack(OAK_PLANKS, 4));
        let crafted = client.crafting_click(0, 0, 0, grid.state, false).await;
        assert_eq!(crafted.cursor, item_stack(OAK_PLANKS, 4));
        assert_eq!(crafted.slots[1], item_stack(OAK_LOG, 1));
        let closed = client.close_crafting().await;
        assert!(closed.slots[..5].iter().all(Option::is_none));
        assert!(closed.cursor.is_none());
        assert_eq!(closed.hands[0], item_stack(OAK_PLANKS, 4));
        assert_eq!(real_count(&closed, OAK_PLANKS), 4);
        assert_eq!(real_count(&closed, OAK_LOG), 1);

        let blocks = zerqon_blocks::Catalog::embedded().unwrap();
        let grass = blocks
            .resolve("minecraft:grass_block", &BTreeMap::new())
            .unwrap()
            .0 as i32;
        let air = blocks.air().0 as i32;
        let target = [2, 63, 0];
        for (action, sequence) in [(3, 100), (3, 101)] {
            client
                .rejected_block_action(
                    41,
                    &destroy_body(action, target, sequence),
                    sequence,
                    &[(target, grass)],
                )
                .await;
        }
        client
            .rejected_block_action(
                66,
                &place_body([10, 63, 0], 102),
                102,
                &[([10, 63, 0], grass), ([10, 64, 0], air)],
            )
            .await;
        assert_same_inventory(&client.ordered_snapshot().await, &closed);

        drop(observer);
        server.player_left("Observer").await;
        drop(client);
        server.player_left("Survival").await;
        server.stop().await;
        drop(server);
        let saved = saved_inventory_in_mode(directory.path(), uuid, "survival");
        assert_eq!(saved_count(&saved, "minecraft:oak_planks"), 4);
        assert_eq!(saved_count(&saved, "minecraft:oak_log"), 1);
        assert_eq!(saved_count(&saved, "minecraft:stone"), 0);
        saved_inventory_in_mode(directory.path(), observer_uuid, "survival");
        let world = WorldStore::open(directory.path().join("world")).unwrap();
        let chunks = world
            .snapshot_domain(&[ChunkKey::new(0, 0)], &[], &[])
            .unwrap();
        assert_eq!(
            chunks.chunks[0].revision(),
            0,
            "denied world edit committed a chunk"
        );
        assert!(chunks.chunks[0].chunk().is_none());
    })
    .await
    .expect("native Survival authority scenario timed out");
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 data"]
async fn native_saved_modes_survive_default_changes_and_creative_flight_is_session_owned() {
    timeout(Duration::from_secs(90), async {
        let directory = tempfile::tempdir().unwrap();
        let mut server = Server::start_in_mode(directory.path(), "survival");
        let mut existing = Client::join(&server.address, "KeptMode").await;
        assert_login_mode(&existing, 0, 0);
        let existing_uuid = existing.uuid;
        server.command("give KeptMode minecraft:stone 3");
        assert_eq!(existing.sync().await.slots[36], stone(3));
        drop(existing);
        server.player_left("KeptMode").await;
        let mut existing = Client::join(&server.address, "KeptMode").await;
        assert_eq!(existing.uuid, existing_uuid);
        assert_login_mode(&existing, 0, 0);
        assert_eq!(existing.tab_mode(existing_uuid, "KeptMode").await, 0);
        assert_eq!(existing.snapshot.slots[36], stone(3));
        drop(existing);
        server.player_left("KeptMode").await;
        server.stop().await;
        drop(server);
        let saved_survival = saved_inventory_in_mode(directory.path(), existing_uuid, "survival");
        assert_eq!(saved_count(&saved_survival, "minecraft:stone"), 3);

        let mut server = Server::start_in_mode(directory.path(), "creative");
        let mut existing = Client::join(&server.address, "KeptMode").await;
        let mut fresh = Client::join(&server.address, "FreshMode").await;
        assert_eq!(existing.uuid, existing_uuid);
        assert_login_mode(&existing, 0, 0);
        assert_login_mode(&fresh, 1, 0x0d);
        let fresh_uuid = fresh.uuid;
        assert_ne!(fresh_uuid, existing_uuid);
        assert_eq!(existing.snapshot.slots[36], stone(3));
        assert_eq!(existing.tab_mode(existing_uuid, "KeptMode").await, 0);
        assert_eq!(existing.tab_mode(fresh_uuid, "FreshMode").await, 1);
        assert_eq!(fresh.tab_mode(existing_uuid, "KeptMode").await, 0);
        assert_eq!(fresh.tab_mode(fresh_uuid, "FreshMode").await, 1);
        assert_eq!(existing.flight_intent(15).await.flags, 0);
        assert_eq!(fresh.flight_intent(2).await.flags, 0x0f);
        assert_eq!(fresh.flight_intent(0).await.flags, 0x0d);
        assert_eq!(fresh.flight_intent(255).await.flags, 0x0f);
        assert!(fresh.snapshot.slots.iter().all(Option::is_none));
        drop(fresh);
        server.player_left("FreshMode").await;
        let mut fresh = Client::join(&server.address, "FreshMode").await;
        assert_eq!(fresh.uuid, fresh_uuid);
        assert_login_mode(&fresh, 1, 0x0d);
        assert_eq!(fresh.tab_mode(existing_uuid, "KeptMode").await, 0);
        assert_eq!(fresh.tab_mode(fresh_uuid, "FreshMode").await, 1);
        drop(fresh);
        server.player_left("FreshMode").await;
        drop(existing);
        server.player_left("KeptMode").await;
        server.stop().await;
        drop(server);
        let survived = saved_inventory_in_mode(directory.path(), existing_uuid, "survival");
        assert_eq!(
            survived.payload, saved_survival.payload,
            "configured default replaced saved player state"
        );
        let creative = saved_inventory_in_mode(directory.path(), fresh_uuid, "creative");
        assert!(
            creative.inventory["slots"]
                .as_array()
                .unwrap()
                .iter()
                .all(serde_json::Value::is_null)
        );
        assert!(creative.inventory["cursor"].is_null());
    })
    .await
    .expect("native persisted mode and flight scenario timed out");
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 data"]
async fn native_speed_builtin_updates_exact_target_and_preserves_flight_state() {
    timeout(Duration::from_secs(60), async {
        let directory = tempfile::tempdir().unwrap();
        let mut server = Server::start(directory.path());
        let mut operator = Client::join(&server.address, "SpeedOp").await;
        let mut other = Client::join(&server.address, "SpeedOther").await;
        operator.command("speed 4").await;
        operator.chat_containing("Permission denied").await;
        assert_eq!(operator.flight_intent(0).await.flying_speed, 0.05);

        server.command("op SpeedOp");
        timeout(Duration::from_secs(5), async {
            loop {
                if std::fs::read_to_string(directory.path().join("ops.json"))
                    .is_ok_and(|text| text.contains(&operator.uuid.to_string()))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        operator.command("speed 4").await;
        let updated = operator.next_abilities().await;
        assert_eq!(updated.flags, 0x0d);
        assert_eq!(updated.flying_speed, 0.2);
        let flying = operator.flight_intent(2).await;
        assert_eq!(flying.flags, 0x0f);
        assert_eq!(flying.flying_speed, 0.2);

        operator.command("speed 10 SpeedOther").await;
        let targeted = other.next_abilities().await;
        assert_eq!(targeted.flags, 0x0d);
        assert_eq!(targeted.flying_speed, 0.5);
        assert_eq!(other.flight_intent(2).await.flags, 0x0f);
        assert_eq!(operator.flight_intent(0).await.flying_speed, 0.2);
        operator.command("speed 1 SpeedOther").await;
        let reset = other.next_abilities().await;
        assert_eq!(
            reset.flags, 0x0f,
            "speed reset changed the active flight state"
        );
        assert_eq!(reset.flying_speed, 0.05);
        assert_eq!(other.flight_intent(0).await.flying_speed, 0.05);
        for command in ["speed 0", "speed 11"] {
            operator.command(command).await;
            operator.chat_containing("1..10").await;
            assert_eq!(operator.flight_intent(0).await.flying_speed, 0.2);
        }

        drop(other);
        server.player_left("SpeedOther").await;
        drop(operator);
        server.player_left("SpeedOp").await;
        let reconnected = Client::join(&server.address, "SpeedOp").await;
        assert_eq!(reconnected.abilities.unwrap().flying_speed, 0.05);
        drop(reconnected);
        server.stop().await;
    })
    .await
    .expect("native speed plugin scenario timed out");
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 data"]
async fn native_editor_selection_fill_replace_and_tnt_survive_restart() {
    async fn reply(client: &mut Client, text: &str) -> Vec<(i32, Vec<u8>)> {
        let mut observed = Vec::new();
        loop {
            let (id, body) = client.read().await;
            client.inspect(id, &body).await;
            if id == 124 && body.windows(text.len()).any(|part| part == text.as_bytes()) {
                observed.push((id, body));
                return observed;
            }
            observed.push((id, body));
            assert!(observed.len() < 1024, "editor response did not settle");
        }
    }

    fn section_changes(packets: &[(i32, Vec<u8>)], expected: u32) -> usize {
        let mut total = 0;
        for (_, body) in packets.iter().filter(|(id, _)| *id == 86) {
            let mut body = body.as_slice();
            take(&mut body, 8).unwrap();
            let count = get_varint(&mut body).unwrap();
            assert!((1..=4096).contains(&count));
            for _ in 0..count {
                let mut value = 0u64;
                for shift in (0..70).step_by(7) {
                    let byte = take(&mut body, 1).unwrap()[0];
                    assert!(shift < 63 || byte <= 1, "invalid VarLong");
                    value |= u64::from(byte & 0x7f) << shift;
                    if byte < 0x80 {
                        break;
                    }
                    assert!(shift < 63, "unterminated VarLong");
                }
                assert_eq!(value >> 12, u64::from(expected));
                total += 1;
            }
            assert!(body.is_empty());
        }
        total
    }

    fn assert_air(directory: &Path, air: u32) {
        let store = WorldStore::open(directory.join("world")).unwrap();
        for x in -2i32..=1 {
            for z in -2i32..=1 {
                let chunk = store
                    .load(x.div_euclid(16), z.div_euclid(16))
                    .unwrap()
                    .unwrap();
                for y in 64..=65 {
                    assert_eq!(
                        chunk.get_block(x.rem_euclid(16) as u8, y, z.rem_euclid(16) as u8),
                        Some(air)
                    );
                }
            }
        }
        assert_eq!(
            store.load(0, 0).unwrap().unwrap().get_block(2, 64, 0),
            Some(air),
            "explosion did not destroy the neighboring stone block"
        );
    }

    timeout(Duration::from_secs(60), async {
        let reports =
            Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/vanilla/26.3/generated/reports");
        let blocks: serde_json::Value =
            serde_json::from_slice(&std::fs::read(reports.join("blocks.json")).unwrap()).unwrap();
        let block = |name: &str| {
            blocks[name]["states"]
                .as_array()
                .unwrap()
                .iter()
                .find(|state| state["default"] == true)
                .unwrap()["id"]
                .as_u64()
                .unwrap() as u32
        };
        let (air, stone, tnt) = (
            block("minecraft:air"),
            block("minecraft:stone"),
            block("minecraft:tnt"),
        );
        let registries: serde_json::Value =
            serde_json::from_slice(&std::fs::read(reports.join("registries.json")).unwrap())
                .unwrap();
        let tnt_entity =
            registries["minecraft:entity_type"]["entries"]["minecraft:tnt"]["protocol_id"]
                .as_i64()
                .unwrap() as i32;
        let directory = tempfile::tempdir().unwrap();
        let mut server = Server::start(directory.path());
        let mut client = Client::join(&server.address, "EditOp").await;
        client.command("//pos1 -2 64 -2").await;
        client.chat_containing("Permission denied").await;
        server.command("op EditOp");
        timeout(Duration::from_secs(5), async {
            while !std::fs::read_to_string(directory.path().join("ops.json"))
                .is_ok_and(|text| text.contains(&client.uuid.to_string()))
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        for (command, message) in [
            ("//pos1 -2 64 -2", "Position 1: -2 64 -2"),
            ("//pos2 1 65 1", "Position 2: 1 65 1"),
            ("//size", "Selection: 32 blocks"),
        ] {
            client.command(command).await;
            client.chat_containing(message).await;
        }
        client.command("//set stone").await;
        let filled = reply(&mut client, "Changed 32 blocks").await;
        assert_eq!(section_changes(&filled, stone), 32);
        client.command("//replace stone tnt").await;
        let replaced = reply(&mut client, "Changed 32 blocks").await;
        assert_eq!(section_changes(&replaced, tnt), 32);
        client.command(&format!("setblock 2 64 0 {stone}")).await;
        client.chat_containing("Block set at 2 64 0").await;
        client.command("//ignite").await;
        let primed = reply(&mut client, "Primed 32 TNT").await;
        assert_eq!(section_changes(&primed, air), 32);
        let visible_tnt = primed
            .iter()
            .filter(|(id, body)| {
                if *id != 1 {
                    return false;
                }
                let mut body = body.as_slice();
                get_varint(&mut body).unwrap();
                take(&mut body, 16).unwrap();
                get_varint(&mut body).unwrap() == tnt_entity
            })
            .count();
        assert_eq!(visible_tnt, 32);
        timeout(Duration::from_secs(15), async {
            loop {
                client.command("//tntstatus").await;
                let status = reply(&mut client, "detonated=").await;
                let chat = &status.last().unwrap().1;
                if chat
                    .windows(b"active=0".len())
                    .any(|part| part == b"active=0")
                    && chat
                        .windows(b"detonated=32".len())
                        .any(|part| part == b"detonated=32")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("primed TNT did not detonate");
        client.command("tps").await;
        client.chat_containing("TPS").await;
        drop(client);
        server.stop().await;
        drop(server);
        assert_air(directory.path(), air);
        let mut server = Server::start(directory.path());
        let mut client = Client::join(&server.address, "EditOp").await;
        client.command("//size").await;
        client.chat_containing("Select both corners").await;
        client.command("tps").await;
        client.chat_containing("TPS").await;
        drop(client);
        server.stop().await;
        drop(server);
        assert_air(directory.path(), air);
    })
    .await
    .expect("native editor and TNT scenario timed out");
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 data"]
async fn native_tnt_falls_damages_entities_and_allows_survival_respawn() {
    fn spawn(body: &[u8]) -> (i32, Uuid, i32, [f64; 3]) {
        let mut body = body;
        let id = get_varint(&mut body).unwrap();
        let uuid = Uuid::from_slice(take(&mut body, 16).unwrap()).unwrap();
        let kind = get_varint(&mut body).unwrap();
        let position = std::array::from_fn(|_| {
            f64::from_be_bytes(take(&mut body, 8).unwrap().try_into().unwrap())
        });
        (id, uuid, kind, position)
    }

    fn assert_pig_absent(directory: &Path, uuid: Uuid) {
        let bytes = std::fs::read(directory.join("world/entities.zqe")).unwrap();
        assert_eq!(&bytes[..8], b"ZQENTITY");
        let length = u32::from_le_bytes(bytes[10..14].try_into().unwrap()) as usize;
        assert_eq!(bytes.len(), 46 + length);
        let snapshot: zerqon_entities::Snapshot = serde_json::from_slice(&bytes[46..]).unwrap();
        assert!(
            snapshot.entities.iter().all(|mob| mob.uuid != uuid),
            "killed pig was restored"
        );
    }

    async fn command(client: &mut Client, text: &str, expected: &str) {
        client.command(text).await;
        timeout(Duration::from_secs(5), async {
            loop {
                let (id, body) = client.read().await;
                client.inspect(id, &body).await;
                if id != 124 {
                    continue;
                }
                assert!(
                    ![
                        b"Command failed:".as_slice(),
                        b"Permission denied".as_slice()
                    ]
                    .iter()
                    .any(|message| body.windows(message.len()).any(|part| part == *message)),
                    "command {text:?} failed: {}",
                    String::from_utf8_lossy(&body)
                );
                if body
                    .windows(expected.len())
                    .any(|part| part == expected.as_bytes())
                {
                    return;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("command {text:?} did not reply with {expected:?}"));
    }

    async fn prime(client: &mut Client, x: i32) {
        command(client, &format!("//pos1 {x} 72 0"), "Position 1:").await;
        command(client, &format!("//pos2 {x} 72 0"), "Position 2:").await;
        command(client, "//set tnt", "Changed 1 blocks").await;
        command(client, "//ignite", "Primed 1 TNT").await;
    }

    timeout(Duration::from_secs(75), async {
        let directory = tempfile::tempdir().unwrap();
        let mut server = Server::start_in_mode(directory.path(), "creative");
        let creative = Client::join(&server.address, "BlastCreative").await;
        server.command("op BlastCreative");
        timeout(Duration::from_secs(5), async {
            while !std::fs::read_to_string(directory.path().join("ops.json"))
                .is_ok_and(|text| text.contains(&creative.uuid.to_string()))
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        drop(creative);
        server.player_left("BlastCreative").await;
        server.stop().await;
        drop(server);

        let mut server = Server::start_in_mode(directory.path(), "survival");
        let mut creative = Client::join(&server.address, "BlastCreative").await;
        let mut survivor = Client::join(&server.address, "BlastSurvival").await;
        assert_eq!(creative.login_mode.unwrap().current, 1);
        assert_eq!(survivor.login_mode.unwrap().current, 0);
        assert_eq!(creative.health, Some(20.0));
        assert_eq!(survivor.health, Some(20.0));
        for (text, expected) in [
            ("//pos1 -2 64 -2", "Position 1:"),
            ("//pos2 9 66 2", "Position 2:"),
            ("//set bedrock", "Changed"),
            ("//pos1 -1 64 -1", "Position 1:"),
            ("//pos2 8 66 1", "Position 2:"),
            ("//set air", "Changed"),
        ] {
            command(&mut creative, text, expected).await;
        }
        let registry: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(".cache/vanilla/26.3/generated/reports/registries.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let entity_type = |name: &str| {
            registry["minecraft:entity_type"]["entries"][name]["protocol_id"]
                .as_i64()
                .unwrap() as i32
        };
        let tnt_kind = entity_type("minecraft:tnt");
        let pig_kind = entity_type("minecraft:pig");
        prime(&mut creative, 6).await;

        let mut tnt_id = None;
        let mut heights = Vec::new();
        let mut pig = None;
        let mut pig_requested = false;
        let mut pig_dead = false;
        let mut pig_removed = false;
        let mut knocked_back = false;
        let mut exploded = false;
        timeout(Duration::from_secs(15), async {
            loop {
                let (id, body) = survivor.read().await;
                survivor.inspect(id, &body).await;
                let mut view = body.as_slice();
                match id {
                    1 => {
                        let (id, uuid, kind, position) = spawn(&body);
                        if kind == tnt_kind {
                            assert!(tnt_id.replace(id).is_none());
                            assert_eq!(position[1], 72.0);
                        } else if kind == pig_kind {
                            pig = Some((id, uuid));
                        }
                    }
                    35 => {
                        if Some(get_varint(&mut view).unwrap()) == tnt_id {
                            assert_eq!(get_varint(&mut view).unwrap(), 0);
                            take(&mut view, 8).unwrap();
                            let y =
                                f64::from_be_bytes(take(&mut view, 8).unwrap().try_into().unwrap());
                            assert!(y >= 64.0 - 1e-6, "TNT fell through the floor: {y}");
                            heights.push(y);
                        }
                    }
                    101 => {
                        let entity = get_varint(&mut view).unwrap();
                        if Some(entity) == tnt_id {
                            while let index @ 0..=254 = take(&mut view, 1).unwrap()[0] {
                                match (index, get_varint(&mut view).unwrap()) {
                                    (5, 8) => {
                                        assert!(!read_bool(&mut view), "TNT disables gravity")
                                    }
                                    (8, 1) => {
                                        let fuse = get_varint(&mut view).unwrap();
                                        if fuse <= 20
                                            && !pig_requested
                                            && heights.iter().any(|y| *y < 64.5)
                                        {
                                            server.command("summon pig 6.5 64 0.5");
                                            pig_requested = true;
                                        }
                                    }
                                    metadata => panic!("unexpected TNT metadata {metadata:?}"),
                                }
                            }
                        } else if pig.is_some_and(|(id, _)| entity == id)
                            && view.starts_with(&[9, 3])
                        {
                            take(&mut view, 2).unwrap();
                            pig_dead |=
                                f32::from_be_bytes(take(&mut view, 4).unwrap().try_into().unwrap())
                                    == 0.0;
                        }
                    }
                    103 => {
                        if get_varint(&mut view).unwrap() == survivor.entity {
                            knocked_back |= view != [0];
                        }
                    }
                    78 => {
                        for _ in 0..get_varint(&mut view).unwrap() {
                            let removed = get_varint(&mut view).unwrap();
                            pig_removed |= pig.is_some_and(|(id, _)| removed == id);
                        }
                    }
                    36 => {
                        exploded = true;
                        assert!(heights.iter().any(|y| *y < 70.0), "TNT did not fall");
                        assert!(heights.iter().any(|y| *y < 64.5), "TNT did not land");
                    }
                    _ => {}
                }
                if exploded
                    && pig_dead
                    && pig_removed
                    && knocked_back
                    && survivor.health.is_some_and(|health| health < 20.0)
                {
                    break;
                }
            }
        })
        .await
        .expect("falling TNT did not damage nearby entities");
        assert!(
            survivor
                .health
                .is_some_and(|health| health > 0.0 && health < 20.0),
            "distant blast should be nonlethal"
        );
        let injured_health = survivor.health;
        survivor.send(12, &[0]).await;
        command(&mut survivor, "tps", "TPS").await;
        assert_eq!(
            survivor.health, injured_health,
            "alive player healed through respawn request"
        );
        command(&mut creative, "tps", "TPS").await;
        assert_eq!(
            creative.health,
            Some(20.0),
            "Creative player took blast damage"
        );

        prime(&mut creative, 0).await;
        timeout(Duration::from_secs(15), async {
            while survivor.health != Some(0.0) {
                let (id, body) = survivor.read().await;
                survivor.inspect(id, &body).await;
            }
        })
        .await
        .expect("nearby blast did not kill the Survival player");
        survivor.send(12, &[0]).await;
        let mut respawned = false;
        let mut positioned = false;
        timeout(Duration::from_secs(10), async {
            loop {
                let (id, body) = survivor.read().await;
                survivor.inspect(id, &body).await;
                respawned |= id == 84;
                positioned |= respawned && id == 73;
                if respawned && positioned && survivor.health == Some(20.0) {
                    break;
                }
            }
        })
        .await
        .expect("dead player did not respawn");
        command(&mut survivor, "tps", "TPS").await;
        creative.command("tps").await;
        let mut removed = false;
        let mut added = false;
        let mut healthy = false;
        let mut answered = false;
        timeout(Duration::from_secs(5), async {
            loop {
                let (id, body) = creative.read().await;
                creative.inspect(id, &body).await;
                let mut view = body.as_slice();
                match id {
                    78 => {
                        for _ in 0..get_varint(&mut view).unwrap() {
                            removed |= get_varint(&mut view).unwrap() == survivor.entity;
                        }
                    }
                    1 => {
                        let (entity, uuid, _, _) = spawn(&body);
                        if entity == survivor.entity {
                            assert!(
                                removed,
                                "respawn entity was added before its previous body was removed"
                            );
                            assert_eq!(uuid, survivor.uuid);
                            added = true;
                        }
                    }
                    101 => {
                        if get_varint(&mut view).unwrap() == survivor.entity
                            && view.starts_with(&[9, 3])
                        {
                            take(&mut view, 2).unwrap();
                            healthy |=
                                f32::from_be_bytes(take(&mut view, 4).unwrap().try_into().unwrap())
                                    == 20.0;
                        }
                    }
                    70 => {
                        for _ in 0..get_varint(&mut view).unwrap() {
                            assert_ne!(
                                Uuid::from_slice(take(&mut view, 16).unwrap()).unwrap(),
                                survivor.uuid,
                                "respawn removed the player from TAB"
                            );
                        }
                    }
                    124 => answered |= body.windows(3).any(|part| part == b"TPS"),
                    _ => {}
                }
                if removed && added && healthy && answered {
                    break;
                }
            }
        })
        .await
        .expect("observer retained the dead player body after respawn");
        assert_eq!(creative.health, Some(20.0));
        let (_, pig_uuid) = pig.expect("blast victim was not spawned");
        drop(survivor);
        drop(creative);
        server.stop().await;
        drop(server);
        assert_pig_absent(directory.path(), pig_uuid);
        let mut server = Server::start(directory.path());
        let mut survivor = Client::join(&server.address, "BlastSurvival").await;
        assert_eq!(survivor.login_mode.unwrap().current, 0);
        command(&mut survivor, "tps", "TPS").await;
        drop(survivor);
        server.stop().await;
        drop(server);
        assert_pig_absent(directory.path(), pig_uuid);
    })
    .await
    .expect("native TNT physics and damage scenario timed out");
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 data"]
async fn native_flint_ignites_targeted_tnt_in_both_hands_and_saves_survival_wear() {
    async fn set_tnt(server: &mut Server, client: &mut Client, block: [i32; 3], state: i32) {
        client.block_updates.remove(&block);
        server.command(&format!(
            "setblock {} {} {} {state}",
            block[0], block[1], block[2]
        ));
        while client.block_updates.get(&block) != Some(&state) {
            let (id, body) = client.read().await;
            client.inspect(id, &body).await;
        }
    }

    async fn use_flint(
        client: &mut Client,
        block: [i32; 3],
        hand: u8,
        sequence: i32,
        tnt_kind: i32,
    ) -> (usize, bool) {
        let mut body = place_body(block, sequence);
        body[0] = hand;
        action(client, 66, &body, sequence, tnt_kind).await
    }

    async fn action(
        client: &mut Client,
        packet_id: i32,
        body: &[u8],
        sequence: i32,
        tnt_kind: i32,
    ) -> (usize, bool) {
        client.block_updates.clear();
        client.send(packet_id, body).await;
        client.command("tps").await;
        let (mut ack, mut barrier, mut spawned, mut rejected) = (false, false, 0, false);
        while !ack || !barrier {
            let (id, body) = client.read().await;
            client.inspect(id, &body).await;
            let mut view = body.as_slice();
            match id {
                1 => {
                    get_varint(&mut view).unwrap();
                    take(&mut view, 16).unwrap();
                    if get_varint(&mut view).unwrap() == tnt_kind {
                        spawned += 1;
                    }
                }
                4 => {
                    assert_eq!(get_varint(&mut view).unwrap(), sequence);
                    assert!(view.is_empty());
                    ack = true;
                }
                124 => {
                    rejected |= body
                        .windows(b"Block interaction rejected".len())
                        .any(|part| part == b"Block interaction rejected");
                    barrier |= body.windows(3).any(|part| part == b"TPS");
                }
                _ => {}
            }
        }
        (spawned, rejected)
    }

    async fn cancel(server: &mut Server) {
        server.command("tntcancel");
        timeout(Duration::from_secs(5), async {
            loop {
                while let Ok(line) = server.lines.try_recv() {
                    if line.contains("Pending TNT cancelled") {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    timeout(Duration::from_secs(75), async {
        let data = Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/vanilla/26.3");
        let catalog = zerqon_inventory::ItemCatalog::load(&data).unwrap();
        let flint = catalog.item_id("minecraft:flint_and_steel").unwrap().0 as i32;
        let stick = catalog.item_id("minecraft:stick").unwrap().0 as i32;
        let registries: serde_json::Value = serde_json::from_slice(&std::fs::read(data.join("generated/reports/registries.json")).unwrap()).unwrap();
        assert_eq!(registries["minecraft:data_component_type"]["entries"]["minecraft:damage"]["protocol_id"], 3);
        let tnt_kind = registries["minecraft:entity_type"]["entries"]["minecraft:tnt"]["protocol_id"].as_i64().unwrap() as i32;
        let blocks: serde_json::Value = serde_json::from_slice(&std::fs::read(data.join("generated/reports/blocks.json")).unwrap()).unwrap();
        let state = |name: &str| blocks[name]["states"].as_array().unwrap().iter().find(|state| state["default"].as_bool() == Some(true)).unwrap()["id"].as_i64().unwrap() as i32;
        let (air, tnt) = (state("minecraft:air"), state("minecraft:tnt"));
        let directory = tempfile::tempdir().unwrap();
        let mut server = Server::start_in_mode(directory.path(), "creative");
        let mut client = Client::join(&server.address, "FlintCreative").await;
        client.creative_stack(36, item_stack(flint, 1)).await;
        set_tnt(&mut server, &mut client, [2, 64, 0], tnt).await;
        assert_eq!(use_flint(&mut client, [2, 64, 0], 0, 301, tnt_kind).await, (1, false), "main-hand flint did not prime TNT");
        assert_eq!(client.block_updates.get(&[2, 64, 0]), Some(&air));
        assert_eq!(client.snapshot.slots[36].as_ref().unwrap().damage, 0);
        assert_eq!(use_flint(&mut client, [2, 64, 0], 0, 302, tnt_kind).await.0, 0, "repeat click primed the same block twice");
        client.creative_stack(36, None).await;
        client.creative_stack(45, item_stack(flint, 1)).await;
        set_tnt(&mut server, &mut client, [-2, 64, 0], tnt).await;
        assert_eq!(use_flint(&mut client, [-2, 64, 0], 1, 303, tnt_kind).await, (1, false));
        assert_eq!(client.block_updates.get(&[-2, 64, 0]), Some(&air));
        assert_eq!(client.snapshot.slots[45].as_ref().unwrap().damage, 0);
        set_tnt(&mut server, &mut client, [10, 64, 0], tnt).await;
        assert_eq!(use_flint(&mut client, [10, 64, 0], 1, 304, tnt_kind).await, (0, true));
        assert_eq!(client.block_updates.get(&[10, 64, 0]), Some(&tnt));
        client.creative_stack(36, item_stack(stick, 1)).await;
        set_tnt(&mut server, &mut client, [3, 64, 0], tnt).await;
        assert_eq!(use_flint(&mut client, [3, 64, 0], 0, 305, tnt_kind).await, (0, true));
        assert_eq!(client.block_updates.get(&[3, 64, 0]), Some(&tnt));
        cancel(&mut server).await;
        for (sequence, block, material) in [(309, [2,64,1], "minecraft:sand"), (310, [2,64,-1], "minecraft:gravel")] {
            set_tnt(&mut server, &mut client, block, state(material)).await;
            assert_eq!(action(&mut client, 41, &destroy_body(0,block,sequence),sequence,tnt_kind).await, (0,false), "creative break rejected {material}");
            assert_eq!(client.block_updates.get(&block), Some(&air));
        }
        drop(client);
        server.stop().await;
        drop(server);

        let mut server = Server::start_in_mode(directory.path(), "survival");
        let mut client = Client::join(&server.address, "FlintSurvival").await;
        let uuid = client.uuid;
        server.command("give FlintSurvival minecraft:flint_and_steel 1");
        assert_eq!(client.sync().await.slots[36], item_stack(flint, 1));
        set_tnt(&mut server, &mut client, [2, 64, 0], tnt).await;
        assert_eq!(use_flint(&mut client, [2, 64, 0], 0, 306, tnt_kind).await, (1, false));
        assert_eq!(client.block_updates.get(&[2, 64, 0]), Some(&air));
        assert_eq!(client.snapshot.slots[36].as_ref().unwrap().damage, 1);
        assert_eq!(use_flint(&mut client, [2, 64, 0], 0, 307, tnt_kind).await.0, 0);
        assert_eq!(client.snapshot.slots[36].as_ref().unwrap().damage, 1);
        let swapped = client.action(7).await;
        assert_eq!(swapped.slots[45].as_ref().unwrap().damage, 1);
        set_tnt(&mut server, &mut client, [-3, 64, 0], tnt).await;
        assert_eq!(use_flint(&mut client, [-3, 64, 0], 1, 308, tnt_kind).await, (1, false));
        assert_eq!(client.block_updates.get(&[-3, 64, 0]), Some(&air));
        assert_eq!(client.snapshot.slots[45].as_ref().unwrap().damage, 2);
        cancel(&mut server).await;
        drop(client);
        server.stop().await;
        drop(server);
        let saved = saved_inventory_in_mode(directory.path(), uuid, "survival");
        assert_eq!(saved.inventory["slots"][45]["item"], "minecraft:flint_and_steel");
        let saved_damage = saved.inventory["slots"][45]["added"].as_array().unwrap().iter()
            .find(|entry| entry["component"] == "minecraft:damage")
            .expect("flint durability was not persisted");
        assert_eq!(saved_damage["value"], serde_json::json!({"type":"damage","value":2}));
        {
            let world = WorldStore::open(directory.path().join("world")).unwrap();
            for [x,y,z] in [[2i32,64,0], [-2,64,0], [-3,64,0]] {
                assert_eq!(world.load(x.div_euclid(16),z.div_euclid(16)).unwrap().unwrap().get_block(x.rem_euclid(16) as u8,y,z.rem_euclid(16) as u8), Some(air as u32));
            }
            assert_eq!(world.load(0,0).unwrap().unwrap().get_block(10,64,0), Some(tnt as u32));
            assert_eq!(world.load(0,0).unwrap().unwrap().get_block(3,64,0), Some(tnt as u32), "breaking adjacent sand/gravel changed TNT");
            assert_eq!(world.load(0,0).unwrap().unwrap().get_block(2,64,1), Some(air as u32));
            assert_eq!(world.load(0,-1).unwrap().unwrap().get_block(2,64,15), Some(air as u32));
        }
        let mut server = Server::start_in_mode(directory.path(), "survival");
        let client = Client::join(&server.address, "FlintSurvival").await;
        assert_eq!(client.snapshot.slots[45].as_ref().unwrap().damage, 2);
        drop(client);
        server.stop().await;
    }).await.expect("native flint ignition scenario timed out");
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 data"]
async fn playable_survival_mining_placement_feeding_and_zombie_attack() {
    async fn ack(client: &mut Client, sequence: i32, allow_error: bool) {
        loop {
            let (id, body) = client.read().await;
            client.inspect(id, &body).await;
            if id == 124 && !allow_error {
                assert!(
                    !body.windows(8).any(|s| s == b"rejected"),
                    "{}",
                    String::from_utf8_lossy(&body)
                );
            }
            if id == 4 {
                assert_eq!(get_varint(&mut body.as_slice()).unwrap(), sequence);
                return;
            }
        }
    }
    async fn mob_health(client: &mut Client, target: i32, expected: f32) {
        loop {
            let (id, body) = client.read().await;
            client.inspect(id, &body).await;
            if id == 101 {
                let mut data = body.as_slice();
                if get_varint(&mut data).unwrap() == target && data.starts_with(&[9, 3]) {
                    let hp = f32::from_be_bytes(data[2..6].try_into().unwrap());
                    if hp == expected {
                        return;
                    }
                }
            }
        }
    }
    timeout(Duration::from_secs(35), async {
        let directory = tempfile::tempdir().unwrap();
        let mut server = Server::start_in_mode(directory.path(), "survival");
        let mut client = Client::join(&server.address, "Playful").await;
        let registry: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(".cache/vanilla/26.3/generated/reports/registries.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let dirt = registry["minecraft:item"]["entries"]["minecraft:dirt"]["protocol_id"]
            .as_i64()
            .unwrap() as i32;
        let cow_kind = registry["minecraft:entity_type"]["entries"]["minecraft:cow"]["protocol_id"]
            .as_i64()
            .unwrap() as i32;
        let block = [2, 63, 0];
        client.send(41, &destroy_body(0, block, 501)).await;
        ack(&mut client, 501, false).await;
        client.send(41, &destroy_body(3, block, 502)).await;
        ack(&mut client, 502, true).await;
        assert!(client.snapshot.slots.iter().all(Option::is_none));
        client.send(41, &destroy_body(0, block, 503)).await;
        ack(&mut client, 503, false).await;
        tokio::time::sleep(Duration::from_millis(1100)).await;
        client.send(41, &destroy_body(3, block, 504)).await;
        ack(&mut client, 504, false).await;
        assert_eq!(client.snapshot.slots[36].as_ref().unwrap().item, dirt);
        assert_eq!(client.snapshot.slots[36].as_ref().unwrap().count, 1);
        client.send(66, &place_body([2, 62, 0], 505)).await;
        ack(&mut client, 505, false).await;
        assert!(client.snapshot.slots[36].is_none());
        server.command("give Playful wheat 2");
        client.sync().await;
        server.command("summon cow 1.5 64 1.5");
        let cow = loop {
            let (id, body) = client.read().await;
            client.inspect(id, &body).await;
            if id == 1 {
                let mut data = body.as_slice();
                let id = get_varint(&mut data).unwrap();
                take(&mut data, 16).unwrap();
                if get_varint(&mut data).unwrap() == cow_kind {
                    break id;
                }
            }
        };
        let mut attack = Vec::new();
        put_varint(cow, &mut attack);
        client.send(1, &attack).await;
        mob_health(&mut client, cow, 6.0).await;
        let mut interact = Vec::new();
        put_varint(cow, &mut interact);
        put_varint(0, &mut interact);
        zerqon_protocol::put_lp_vec3([0.0, 0.5, 0.0], &mut interact);
        interact.push(0);
        client.send(26, &interact).await;
        loop {
            let (id, body) = client.read().await;
            client.inspect(id, &body).await;
            if id == 34 && body == [cow.to_be_bytes().as_slice(), &[18]].concat() {
                break;
            }
        }
        assert_eq!(client.snapshot.slots[36].as_ref().unwrap().count, 1);
        server.command("summon zombie 3.5 64 0.5");
        loop {
            let (id, body) = client.read().await;
            client.inspect(id, &body).await;
            if client.health.is_some_and(|hp| hp < 20.0) {
                break;
            }
        }
        assert!(client.health.unwrap() > 0.0);
        client.command("tps").await;
        client.chat_containing("TPS").await;
        drop(client);
        server.stop().await;
        drop(server);
        let world = WorldStore::open(directory.path().join("world")).unwrap();
        assert!(
            world
                .load(0, 0)
                .unwrap()
                .unwrap()
                .get_block(2, 63, 0)
                .unwrap()
                > 0
        );
    })
    .await
    .expect("playable survival scenario timed out");
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 data"]
async fn animals_breed_and_keep_baby_age_and_food_after_restart() {
    async fn feed(client: &mut Client, id: i32) {
        let mut body = Vec::new();
        put_varint(id, &mut body);
        put_varint(0, &mut body);
        zerqon_protocol::put_lp_vec3([0.0, 0.4, 0.0], &mut body);
        body.push(0);
        client.send(26, &body).await;
    }
    async fn barrier(client: &mut Client) {
        client.command("tps").await;
        client.chat_containing("TPS").await;
    }
    async fn love(client: &mut Client, target: i32) {
        loop {
            let (id, body) = client.read().await;
            client.inspect(id, &body).await;
            if id == 34 && body == [target.to_be_bytes().as_slice(), &[18]].concat() {
                return;
            }
            if id == 124 {
                assert!(
                    !body.windows(18).any(|p| p == b"Interaction failed"),
                    "{}",
                    String::from_utf8_lossy(&body)
                );
            }
        }
    }
    timeout(Duration::from_secs(35), async {
        let directory = tempfile::tempdir().unwrap();
        let mut server = Server::start_in_mode(directory.path(), "survival");
        let mut client = Client::join(&server.address, "Breeder").await;
        server.command("give Breeder wheat 4");
        client.sync().await;
        let wheat = client.snapshot.slots[36].as_ref().unwrap().item;
        for command in ["summon cow 1.5 64 1.5", "summon cow -1.5 64 1.5"] {
            server.command(command);
        }
        let mut parents = Vec::new();
        while parents.len() < 2 {
            let (id, body) = client.read().await;
            client.inspect(id, &body).await;
            if id == 1 {
                let mut body = body.as_slice();
                let id = get_varint(&mut body).unwrap();
                let uuid = Uuid::from_slice(take(&mut body, 16).unwrap()).unwrap();
                parents.push((id, uuid));
            }
        }
        feed(&mut client, parents[0].0).await;
        love(&mut client, parents[0].0).await;
        assert_eq!(client.snapshot.slots[36].as_ref().unwrap().count, 3);
        feed(&mut client, parents[0].0).await;
        barrier(&mut client).await;
        assert_eq!(
            client.snapshot.slots[36].as_ref().unwrap().count,
            3,
            "repeat feeding consumed food"
        );
        feed(&mut client, parents[1].0).await;
        love(&mut client, parents[1].0).await;
        assert_eq!(client.snapshot.slots[36].as_ref().unwrap().count, 2);
        let mut child = None;
        loop {
            let (id, body) = client.read().await;
            client.inspect(id, &body).await;
            let mut data = body.as_slice();
            if id == 1 {
                let id = get_varint(&mut data).unwrap();
                let uuid = Uuid::from_slice(take(&mut data, 16).unwrap()).unwrap();
                assert!(!parents.iter().any(|(_, p)| *p == uuid));
                child = Some((id, uuid));
            }
            if id == 101
                && Some(get_varint(&mut data).unwrap()) == child.map(|(id, _)| id)
                && data == [16, 8, 1, 255]
            {
                break;
            }
        }
        let (child_id, child_uuid) = child.unwrap();
        feed(&mut client, parents[0].0).await;
        barrier(&mut client).await;
        assert_eq!(
            client.snapshot.slots[36].as_ref().unwrap().count,
            2,
            "cooldown parent consumed food"
        );
        feed(&mut client, child_id).await;
        barrier(&mut client).await;
        assert_eq!(client.snapshot.slots[36].as_ref().unwrap().count, 1);
        let uuid = client.uuid;
        drop(client);
        server.stop().await;
        drop(server);
        let saved = std::fs::read(directory.path().join("world/entities.zqe")).unwrap();
        let snapshot: zerqon_entities::Snapshot = serde_json::from_slice(&saved[46..]).unwrap();
        assert_eq!(snapshot.schema, 2);
        assert_eq!(snapshot.entities.len(), 3);
        let child = snapshot
            .entities
            .iter()
            .find(|m| m.uuid == child_uuid)
            .unwrap();
        assert!(child.age < 0 && child.age > -24000 && child.forced_age > 0);
        assert!(
            snapshot
                .entities
                .iter()
                .filter(|m| m.uuid != child_uuid)
                .all(|m| m.age > 0 && m.love_ticks == 0)
        );
        let saved_player = saved_inventory_in_mode(directory.path(), uuid, "survival");
        assert_eq!(saved_count(&saved_player, "minecraft:wheat"), 1);
        let mut server = Server::start_in_mode(directory.path(), "survival");
        let client = Client::join(&server.address, "Breeder").await;
        assert_eq!(client.snapshot.slots[36].as_ref().unwrap().item, wheat);
        assert_eq!(client.snapshot.slots[36].as_ref().unwrap().count, 1);
        server.command("tps");
        drop(client);
        server.stop().await;
        let saved = std::fs::read(directory.path().join("world/entities.zqe")).unwrap();
        let snapshot: zerqon_entities::Snapshot = serde_json::from_slice(&saved[46..]).unwrap();
        assert_eq!(snapshot.entities.len(), 3);
        assert!(
            snapshot
                .entities
                .iter()
                .any(|m| m.uuid == child_uuid && m.is_baby())
        );
    })
    .await
    .expect("animal breeding scenario timed out");
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft26.3 data"]
async fn normal_world_building_interaction_and_real_item_pickup_persist() {
    async fn console(server: &mut Server, command: &str, expected: &str) {
        server.command(command);
        timeout(Duration::from_secs(8), async {
            loop {
                while let Ok(line) = server.lines.try_recv() {
                    assert!(!line.contains("Command failed"), "{line}");
                    if line.contains(expected) {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
    async fn ack(client: &mut Client, sequence: i32) {
        loop {
            let (id, body) = client.read().await;
            client.inspect(id, &body).await;
            if id == 124 {
                assert!(
                    !body.windows(8).any(|s| s == b"rejected"),
                    "{}",
                    String::from_utf8_lossy(&body)
                );
            }
            if id == 4 {
                assert_eq!(get_varint(&mut body.as_slice()).unwrap(), sequence);
                return;
            }
        }
    }
    async fn place(client: &mut Client, block: [i32; 3], sequence: i32) {
        client.send(66, &place_body(block, sequence)).await;
        ack(client, sequence).await;
    }
    async fn move_to(client: &mut Client, position: [f64; 3]) {
        let mut body = Vec::new();
        for coordinate in position {
            body.extend_from_slice(&coordinate.to_be_bytes());
        }
        body.push(1);
        client.send(30, &body).await;
        client.command("tps").await;
        client.chat_containing("TPS").await;
    }
    fn live_drops(directory: &Path) -> Vec<zerqon_world::DropSnapshot> {
        let store = WorldStore::open(directory.join("world")).unwrap();
        let mut cursor = None;
        let mut live = Vec::new();
        loop {
            let page = store.scan_drops(cursor, 64).unwrap();
            if page.is_empty() {
                break;
            }
            cursor = page.last().map(|drop| drop.key());
            live.extend(page.into_iter().filter(|drop| drop.data().is_some()));
        }
        live
    }
    timeout(Duration::from_secs(90),async {
        let directory=tempfile::tempdir().unwrap();
        let exe=std::env::var_os("ZERQON_TEST_BINARY").unwrap_or_else(||env!("CARGO_BIN_EXE_zerqon").into());
        assert!(Command::new(exe).args(["--init","--directory"]).arg(directory.path()).status().unwrap().success());
        std::fs::write(directory.path().join("eula.txt"),"eula=true\n").unwrap();
        std::fs::write(directory.path().join("server.properties"),"server-ip=127.0.0.1\nserver-port=0\nonline-mode=false\nmax-players=4\nview-distance=1\nnetwork-compression-threshold=-1\nlevel-type=normal\nlevel-seed=42\n").unwrap();
        std::fs::write(directory.path().join("server.settings"),"world_threads=2\njava_plugins=false\nupdate_check=false\nspawn_mobs=false\n").unwrap();
        let data_path=Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/vanilla/26.3");
        let items=zerqon_inventory::ItemCatalog::load(&data_path).unwrap();let blocks=zerqon_blocks::Catalog::embedded().unwrap();
        let block_id=|key:&str|blocks.resolve(key,&BTreeMap::new()).unwrap().0 as i32;
        let stack=|key:&str,count|item_stack(items.item_id(key).unwrap().0 as i32,count);
        let mut server=Server::start_in_mode(directory.path(),"creative");let mut client=Client::join(&server.address,"Builder").await;
        let [x,y,z]=client.position.map(|value|value.floor() as i32);
        console(&mut server,&format!("//pos1 {} {} {}",x-3,y,z-3),"Position 1:").await;
        console(&mut server,&format!("//pos2 {} {} {}",x+6,y+5,z+6),"Position 2:").await;
        console(&mut server,"//set air","Changed").await;
        console(&mut server,&format!("//pos1 {} {} {}",x-3,y-1,z-3),"Position 1:").await;
        console(&mut server,&format!("//pos2 {} {} {}",x+6,y-1,z+6),"Position 2:").await;
        console(&mut server,"//set stone","Changed").await;
        console(&mut server,&format!("setblock {} {} {} {}",x+3,y,z,block_id("minecraft:water")),"Block set at").await;
        console(&mut server,&format!("setblock {} {} {} {}",x+1,y,z,block_id("minecraft:chest")),"Block set at").await;
        client.creative_stack(36,stack("minecraft:stone",1)).await;
        place(&mut client,[x+2,y-1,z],601).await;assert_eq!(client.block_updates.get(&[x+2,y,z]),Some(&block_id("minecraft:stone")));
        client.creative_stack(36,stack("minecraft:oak_door",1)).await;
        let door=[x+2,y,z+2];place(&mut client,[door[0],y-1,door[2]],602).await;
        for pos in [door,[door[0],y+1,door[2]]] {assert_eq!(blocks.state(zerqon_blocks::StateId(*client.block_updates.get(&pos).unwrap() as u32)).unwrap().key(),"minecraft:oak_door");}
        client.creative_stack(36,None).await;place(&mut client,door,603).await;
        assert_eq!(blocks.state(zerqon_blocks::StateId(client.block_updates[&door] as u32)).unwrap().property("open"),Some("true"));
        client.creative_stack(36,stack("minecraft:stone",1)).await;client.send(43,&[32]).await;
        let mut side=place_body(door,604);side[9]=5;side[10..14].copy_from_slice(&1.0_f32.to_be_bytes());side[14..18].copy_from_slice(&0.5_f32.to_be_bytes());
        client.send(66,&side).await;ack(&mut client,604).await;client.send(43,&[0]).await;
        assert_eq!(client.block_updates.get(&[x+3,y,z+2]),Some(&block_id("minecraft:stone")));
        assert_eq!(blocks.state(zerqon_blocks::StateId(client.block_updates[&door] as u32)).unwrap().property("open"),Some("true"));
        client.creative_stack(36,stack("minecraft:oak_trapdoor",1)).await;place(&mut client,[x-2,y-1,z+2],605).await;
        client.creative_stack(36,None).await;place(&mut client,[x-2,y,z+2],606).await;
        assert_eq!(blocks.state(zerqon_blocks::StateId(client.block_updates[&[x-2,y,z+2]] as u32)).unwrap().property("open"),Some("true"));
        client.creative_stack(36,stack("minecraft:torch",1)).await;place(&mut client,[x+4,y-1,z],607).await;
        assert_eq!(client.block_updates.get(&[x+4,y,z]),Some(&block_id("minecraft:torch")));
        drop(client);server.stop().await;drop(server);

        let mut server=Server::start_in_mode(directory.path(),"survival");let mut dropper=Client::join(&server.address,"Dropper").await;let dropper_uuid=dropper.uuid;
        server.command("give Dropper cobblestone 3");dropper.sync().await;
        for action in [5,4] {dropper.action(action).await;}
        assert!(dropper.snapshot.slots[36].is_none());
        drop(dropper);server.stop().await;drop(server);
        let drops=live_drops(directory.path());assert!((1..=2).contains(&drops.len()));
        let total:i64=drops.iter().map(|drop|{let record:serde_json::Value=serde_json::from_slice(drop.data().unwrap()).unwrap();assert_eq!(record["thrower"],dropper_uuid.to_string());assert!(record["owner"].is_null());record["stack"]["count"].as_i64().unwrap()}).sum();assert_eq!(total,3);

        let mut server=Server::start_in_mode(directory.path(),"survival");let mut full=Client::join(&server.address,"Full2").await;let mut collector=Client::join(&server.address,"Collector").await;let full_uuid=full.uuid;assert!(full_uuid<collector.uuid);
        server.command("give Full2 cobblestone 2304");full.sync().await;
        move_to(&mut full,[f64::from(x)+0.5,f64::from(y),f64::from(z)+4.0]).await;
        move_to(&mut collector,[f64::from(x)+0.5,f64::from(y),f64::from(z)+4.0]).await;
        let cobble=items.item_id("minecraft:cobblestone").unwrap().0 as i32;
        let mut taken=0;
        loop {let (id,body)=collector.read().await;collector.inspect(id,&body).await;
            if id==127 {let mut data=body.as_slice();get_varint(&mut data).unwrap();assert_eq!(get_varint(&mut data).unwrap(),collector.entity);taken+=get_varint(&mut data).unwrap();}
            if real_count(&collector.snapshot,cobble)==3&&taken==3{break;}
        }
        assert_eq!(real_count(&full.snapshot,cobble),2304);
        let dirt=[x+1,y,z+4];console(&mut server,&format!("setblock {} {} {} {}",dirt[0],dirt[1],dirt[2],block_id("minecraft:dirt")),"Block set at").await;
        collector.send(41,&destroy_body(0,dirt,608)).await;ack(&mut collector,608).await;tokio::time::sleep(Duration::from_millis(850)).await;
        collector.send(41,&destroy_body(3,dirt,609)).await;ack(&mut collector,609).await;
        let dirt_item=items.item_id("minecraft:dirt").unwrap().0 as i32;
        assert_eq!(real_count(&collector.snapshot,dirt_item),0,"mining granted loot without a world item");
        loop {let (id,body)=collector.read().await;collector.inspect(id,&body).await;if real_count(&collector.snapshot,dirt_item)==1{break;}}
        collector.send(54,&1_i16.to_be_bytes()).await;collector.sync().await;
        place(&mut collector,[dirt[0],y-1,dirt[2]],610).await;assert_eq!(real_count(&collector.snapshot,dirt_item),0);
        let snapshot=full.ordered_snapshot().await;
        let carried=full.crafting_click(36,0,0,snapshot.state,false).await;
        assert_eq!(carried.cursor.as_ref().unwrap().count,64);
        server.command("give Full2 cobblestone 64");full.sync().await;
        assert_eq!(real_count(&full.snapshot,cobble),2368);
        drop(full);server.player_left("Full2").await;
        drop(collector);server.stop().await;drop(server);
        let saved_full=saved_inventory_in_mode(directory.path(),full_uuid,"survival");assert_eq!(saved_count(&saved_full,"minecraft:cobblestone"),2304);assert!(saved_full.inventory["cursor"].is_null());
        let overflow=live_drops(directory.path());assert_eq!(overflow.len(),1,"disconnect overflow duplicated or lost a stack");
        let record:serde_json::Value=serde_json::from_slice(overflow[0].data().unwrap()).unwrap();assert_eq!(record["stack"]["count"],64);
        let p=record["position_bits"].as_array().unwrap();let pickup=[f64::from_bits(p[0].as_u64().unwrap()),f64::from(y),f64::from_bits(p[2].as_u64().unwrap())+3.5];
        let mut server=Server::start_in_mode(directory.path(),"survival");let mut collector=Client::join(&server.address,"Collector").await;
        move_to(&mut collector,pickup).await;
        loop {let (id,body)=collector.read().await;collector.inspect(id,&body).await;if real_count(&collector.snapshot,cobble)==67{break;}}
        drop(collector);server.stop().await;drop(server);
        assert!(live_drops(directory.path()).is_empty(),"picked overflow returned after stop");
        let world=WorldStore::open(directory.path().join("world")).unwrap();assert_eq!(world.load(dirt[0].div_euclid(16),dirt[2].div_euclid(16)).unwrap().unwrap().get_block(dirt[0].rem_euclid(16) as u8,y,dirt[2].rem_euclid(16) as u8),Some(block_id("minecraft:dirt") as u32));
    }).await.expect("normal building and item ownership scenario timed out");
}
