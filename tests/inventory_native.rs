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
    socket: TcpStream,
    uuid: Uuid,
    entity: i32,
    snapshot: Snapshot,
    login_mode: Option<LoginMode>,
    abilities: Option<Abilities>,
    tab: BTreeMap<Uuid, TabEntry>,
    block_updates: BTreeMap<[i32; 3], i32>,
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
            socket,
            uuid,
            entity: 0,
            snapshot,
            login_mode: None,
            abilities: None,
            tab: BTreeMap::new(),
            block_updates: BTreeMap::new(),
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
                assert_eq!(flying_speed, 0.05);
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
                    .windows(b"survival block interactions".len())
                    .any(|part| part == b"survival block interactions");
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
async fn native_crafting_no_drop_rollback_and_preview_recovery_after_restart() {
    timeout(Duration::from_secs(90), async {
        let directory = tempfile::tempdir().unwrap();
        let mut server = Server::start(directory.path());
        let mut client = Client::join(&server.address, "CraftFull").await;
        for slot in 9..=44 {
            if slot == 33 {
                // Respect the service's independent 32 durable mutations/second
                // budget; this delay is not used as an operation-completion fence.
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let seeded = client.creative_stack(slot, stone(64)).await;
            assert_eq!(
                seeded.slots[slot as usize],
                stone(64),
                "setup write was rejected"
            );
        }
        client.creative_stack(44, item_stack(OAK_PLANKS, 62)).await;
        let before = client.creative_stack(1, item_stack(OAK_LOG, 1)).await;
        assert_eq!(before.slots[0], item_stack(OAK_PLANKS, 4));
        assert_eq!(real_count(&before, 1), 2240);
        assert_eq!(real_count(&before, OAK_PLANKS), 62);
        assert_eq!(real_count(&before, OAK_LOG), 1);
        let uuid = client.uuid;
        drop(client);
        server.player_left("CraftFull").await;
        server.stop().await;
        drop(server);
        let saved_before = saved_inventory(directory.path(), uuid);
        assert_eq!(
            saved_before.inventory["slots"][1]["item"],
            "minecraft:oak_log"
        );
        assert_eq!(saved_before.inventory["slots"][1]["count"], 1);
        assert_eq!(saved_count(&saved_before, "minecraft:stone"), 2240);
        assert_eq!(saved_count(&saved_before, "minecraft:oak_planks"), 62);
        assert_eq!(saved_count(&saved_before, "minecraft:oak_log"), 1);

        let mut server = Server::start(directory.path());
        let mut client = Client::join(&server.address, "CraftFull").await;
        assert_eq!(client.uuid, uuid);
        assert_eq!(client.snapshot.slots, before.slots);
        assert_eq!(client.snapshot.cursor, before.cursor);
        assert_eq!(client.snapshot.selected, before.selected);
        let before = client.snapshot.clone();
        let rejected = client.crafting_click(0, 0, 1, before.state, false).await;
        assert_same_inventory(&rejected, &before);
        assert_same_inventory(&client.close_crafting().await, &before);
        drop(client);
        server.player_left("CraftFull").await;
        server.stop().await;
        drop(server);
        let saved_rejected = saved_inventory(directory.path(), uuid);
        assert_eq!(
            saved_rejected.payload, saved_before.payload,
            "rejected craft or overflowing close changed durable player contents"
        );

        let mut server = Server::start(directory.path());
        let mut client = Client::join(&server.address, "CraftFull").await;
        assert_eq!(client.uuid, uuid);
        assert_eq!(client.snapshot.slots, before.slots);
        assert_eq!(client.snapshot.cursor, before.cursor);
        assert_eq!(client.snapshot.slots[0], item_stack(OAK_PLANKS, 4));
        assert_eq!(real_count(&client.snapshot, OAK_PLANKS), 62);
        assert_eq!(real_count(&client.snapshot, OAK_LOG), 1);
        client.creative_stack(43, None).await;
        let completed = client
            .crafting_click(0, 0, 1, client.snapshot.state, false)
            .await;
        assert!(completed.slots[..5].iter().all(Option::is_none));
        assert_eq!(completed.slots[44], item_stack(OAK_PLANKS, 64));
        assert_eq!(completed.slots[43], item_stack(OAK_PLANKS, 2));
        assert_eq!(real_count(&completed, OAK_PLANKS), 66);
        drop(client);
        server.stop().await;
        drop(server);
        let saved = saved_inventory(directory.path(), uuid);
        assert_eq!(saved_count(&saved, "minecraft:oak_planks"), 66);
        assert_eq!(saved_count(&saved, "minecraft:oak_log"), 0);
        assert_eq!(saved_count(&saved, "minecraft:stone"), 2176);
        assert!(saved.inventory["cursor"].is_null());
    })
    .await
    .expect("native no-drop rollback/restart scenario timed out");
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
async fn native_survival_policy_blocks_forged_privileges_and_world_edits_but_allows_crafting() {
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
        for (action, sequence) in [(0, 100), (3, 101)] {
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
                &place_body(target, 102),
                102,
                &[(target, grass), ([2, 64, 0], air)],
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
