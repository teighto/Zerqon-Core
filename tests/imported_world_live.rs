use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::{net::TcpStream, time::timeout};
use zerqon_protocol::codec::{
    get_string, get_varint, put_string, put_varint, read_frame, take, write_frame,
};

struct Server(Child);
fn server_executable() -> PathBuf {
    std::env::var_os("ZERQON_TEST_SERVER_EXE").map_or_else(
        || PathBuf::from(env!("CARGO_BIN_EXE_zerqon")),
        PathBuf::from,
    )
}
impl Server {
    fn start(directory: &Path, data: &Path) -> (Self, String) {
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
        );
        let output = server.0.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                if let Some((_, address)) = line.split_once("Listening on ") {
                    let _ = tx.send(address.trim().to_owned());
                }
            }
        });
        let address = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("server listening");
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
        .unwrap();
        assert!(status.success());
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Client {
    socket: TcpStream,
    threshold: usize,
}
impl Client {
    async fn join(address: &str, name: &str) -> (Self, Vec<Vec<u8>>) {
        let started = std::time::Instant::now();
        let mut socket = TcpStream::connect(address).await.unwrap();
        let mut packet = vec![0];
        put_varint(777, &mut packet);
        put_string("localhost", &mut packet);
        packet.extend_from_slice(&25565_u16.to_be_bytes());
        packet.push(2);
        write_frame(&mut socket, &packet, None).await.unwrap();
        let mut hello = vec![0];
        put_string(name, &mut hello);
        hello.extend_from_slice(&[0; 16]);
        write_frame(&mut socket, &hello, None).await.unwrap();
        let compression = read_frame(&mut socket, 65536, None).await.unwrap();
        let mut body = compression.as_slice();
        assert_eq!(get_varint(&mut body).unwrap(), 3);
        let mut client = Self {
            socket,
            threshold: get_varint(&mut body).unwrap() as usize,
        };
        assert_eq!(client.read().await[0], 2);
        client.send(&[3]).await;
        let mut known = client.read().await;
        assert_eq!(known[0], 15);
        known[0] = 7;
        client.send(&known).await;
        while client.read().await[0] != 3 {}
        client.send(&[3]).await;
        let mut received = Vec::new();
        loop {
            let packet = client.read().await;
            if packet[0] == 73 {
                let (id, xyz, rotation) = teleport(&packet);
                assert_eq!(xyz, [0.5, -60.0, 0.5], "source spawn must be active");
                assert_eq!(rotation, [137.5, -25.0]);
                client.ack(id).await;
            }
            let finished = packet[0] == 11;
            received.push(packet);
            if finished {
                break;
            }
        }
        eprintln!("Imported-world join {name}: {:?}", started.elapsed());
        (client, received)
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
        timeout(
            Duration::from_secs(5),
            read_frame(&mut self.socket, 2_097_151, Some(self.threshold)),
        )
        .await
        .unwrap()
        .unwrap()
    }
    async fn until(&mut self, id: u8) -> Vec<u8> {
        timeout(Duration::from_secs(8), async {
            loop {
                let packet = self.read().await;
                assert_ne!(packet[0], 32, "client disconnected: {packet:?}");
                if packet[0] == 45 {
                    let mut reply = packet;
                    reply[0] = 28;
                    self.send(&reply).await;
                } else if packet[0] == id {
                    break packet;
                }
            }
        })
        .await
        .unwrap()
    }
    async fn ack(&mut self, id: i32) {
        let mut ack = vec![0];
        put_varint(id, &mut ack);
        self.send(&ack).await;
    }
    async fn movement(&mut self, xyz: [f64; 3]) {
        let mut packet = vec![30];
        for value in xyz {
            packet.extend_from_slice(&value.to_be_bytes());
        }
        packet.push(1);
        self.send(&packet).await;
    }
    async fn tps(&mut self) {
        let mut packet = vec![7];
        put_string("tps", &mut packet);
        self.send(&packet).await;
        loop {
            if self.until(124).await.windows(3).any(|s| s == b"TPS") {
                break;
            }
        }
    }
}

fn teleport(packet: &[u8]) -> (i32, [f64; 3], [f32; 2]) {
    assert_eq!(packet[0], 73);
    let mut body = &packet[1..];
    let id = get_varint(&mut body).unwrap();
    let xyz = std::array::from_fn(|_| {
        f64::from_be_bytes(take(&mut body, 8).unwrap().try_into().unwrap())
    });
    assert_eq!(take(&mut body, 24).unwrap(), &[0; 24]);
    let rotation = std::array::from_fn(|_| {
        f32::from_be_bytes(take(&mut body, 4).unwrap().try_into().unwrap())
    });
    assert_eq!(body, &[0; 4]);
    (id, xyz, rotation)
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &destination.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
        }
    }
}
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}
fn rotate_spawn(path: &Path) {
    let mut nbt = Vec::new();
    flate2::read::GzDecoder::new(fs::File::open(path).unwrap())
        .read_to_end(&mut nbt)
        .unwrap();
    for (name, value) in [("yaw", 137.5_f32), ("pitch", -25.0_f32)] {
        let mut tag = vec![5, 0, name.len() as u8];
        tag.extend_from_slice(name.as_bytes());
        let matches: Vec<_> = nbt
            .windows(tag.len())
            .enumerate()
            .filter_map(|(i, bytes)| (bytes == tag).then_some(i))
            .collect();
        assert_eq!(matches.len(), 1);
        let offset = matches[0] + tag.len();
        nbt[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }
    let mut output = flate2::write::GzEncoder::new(
        fs::File::create(path).unwrap(),
        flate2::Compression::default(),
    );
    output.write_all(&nbt).unwrap();
    output.finish().unwrap();
}
fn batch_chunks(packets: &[Vec<u8>]) -> std::collections::BTreeSet<(i32, i32)> {
    let chunks: Vec<_> = packets
        .iter()
        .filter(|p| p[0] == 46)
        .map(|p| {
            (
                i32::from_be_bytes(p[1..5].try_into().unwrap()),
                i32::from_be_bytes(p[5..9].try_into().unwrap()),
            )
        })
        .collect();
    let mut end = &packets.last().unwrap()[1..];
    assert_eq!(get_varint(&mut end).unwrap() as usize, chunks.len());
    let unique = chunks
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(unique.len(), chunks.len());
    unique
}

async fn startup_failure(directory: &Path, data: &Path, expected: &str) {
    let result = timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(server_executable())
            .args([
                "--directory",
                directory.to_str().unwrap(),
                "--data",
                data.to_str().unwrap(),
                "--no-color",
                "--no-console",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("invalid imported spawn did not reject startup")
    .unwrap();
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains(expected),
        "unexpected startup error: {stderr}"
    );
}

#[tokio::test]
#[ignore = "requires prepared 26.3 data and the owned .local/vanilla-world-filled/world fixture"]
async fn imported_spawn_sparse_view_and_boundary_corrections_survive_restart() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join(".local/vanilla-world-filled/world");
    let data = root.join(".cache/vanilla/26.3");
    assert!(fixture.join("level.dat").exists());
    let original = snapshot(&fixture);
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let directory = temporary.path().join("server");
    copy_tree(&fixture, &source);
    rotate_spawn(&source.join("level.dat"));
    let source_before = snapshot(&source);
    assert!(
        Command::new(server_executable())
            .args(["--init", "--directory", directory.to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    fs::write(directory.join("eula.txt"), "eula=true\n").unwrap();
    fs::write(
        directory.join("server.settings"),
        "update_check=false\njava_plugins=false\nspawn_mobs=false\n",
    )
    .unwrap();
    fs::write(directory.join("server.properties"), "server-ip=127.0.0.1\nserver-port=0\nonline-mode=false\nview-distance=4\nnetwork-compression-threshold=256\n").unwrap();
    let import = Command::new(server_executable())
        .args([
            "--directory",
            directory.to_str().unwrap(),
            "--data",
            data.to_str().unwrap(),
            "--import-blocks",
            source.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        import.status.success(),
        "{}",
        String::from_utf8_lossy(&import.stderr)
    );
    let world = directory.join("world");
    let chunks_before: BTreeMap<_, _> = snapshot(&world)
        .into_iter()
        .filter(|(path, _)| path.extension().is_some_and(|ext| ext == "zqc"))
        .collect();
    let coordinates: std::collections::BTreeSet<_> = chunks_before
        .values()
        .map(|bytes| {
            let chunk = zerqon_world::Chunk::from_bytes(bytes).unwrap();
            (chunk.x, chunk.z)
        })
        .collect();
    assert!(
        coordinates.contains(&(2, 0)) && !coordinates.contains(&(3, 0)),
        "fixture needs an edge at x=48"
    );
    let retained_before = snapshot(&world.join("source-data"));
    for iteration in 0..2 {
        let (mut server, address) = Server::start(&directory, &data);
        let (mut client, packets) = Client::join(&address, "ImportedPlayer").await;
        let expected = coordinates
            .iter()
            .copied()
            .filter(|(x, z)| x.abs() <= 4 && z.abs() <= 4)
            .collect();
        assert_eq!(batch_chunks(&packets), expected);
        let spawn = packets.iter().find(|p| p[0] == 99).unwrap();
        let mut body = &spawn[1..];
        assert_eq!(get_string(&mut body, 64).unwrap(), "minecraft:overworld");
        assert_eq!(
            i64::from_be_bytes(take(&mut body, 8).unwrap().try_into().unwrap()),
            4036
        );
        assert_eq!(
            f32::from_be_bytes(take(&mut body, 4).unwrap().try_into().unwrap()),
            137.5
        );
        assert_eq!(
            f32::from_be_bytes(take(&mut body, 4).unwrap().try_into().unwrap()),
            -25.0
        );
        assert!(body.is_empty());
        if iteration == 0 {
            let (mut observer, observed) = Client::join(&address, "ImportedObserver").await;
            let player_spawn = match observed.iter().find(|packet| packet[0] == 1) {
                Some(packet) => packet.clone(),
                None => observer.until(1).await,
            };
            let mut body = &player_spawn[1..];
            get_varint(&mut body).unwrap();
            take(&mut body, 16).unwrap();
            get_varint(&mut body).unwrap();
            let actual: [f64; 3] = std::array::from_fn(|_| {
                f64::from_be_bytes(take(&mut body, 8).unwrap().try_into().unwrap())
            });
            assert_eq!(
                actual,
                [0.5, -60.0, 0.5],
                "registry activation must use imported spawn"
            );
            client.movement([32.5, -60.0, 0.5]).await;
            client.until(11).await;
            let moved = observer.until(35).await;
            let mut body = &moved[1..];
            get_varint(&mut body).unwrap();
            assert_eq!(take(&mut body, 1).unwrap(), &[0]);
            let actual: [f64; 3] = std::array::from_fn(|_| {
                f64::from_be_bytes(take(&mut body, 8).unwrap().try_into().unwrap())
            });
            assert_eq!(actual, [32.5, -60.0, 0.5]);
            let mut last_id = 1;
            for target in [
                [47.8, -60.0, 0.5],
                [48.5, -60.0, 0.5],
                [-0.5, -60.0, -512.5],
            ] {
                client.movement(target).await;
                let correction = client.until(73).await;
                let (id, xyz, rotation) = teleport(&correction);
                assert!(id > last_id);
                assert_eq!(xyz, [32.5, -60.0, 0.5]);
                assert_eq!(rotation, [137.5, -25.0]);
                last_id = id;
                client.ack(id - 1).await;
                client.movement([16.5, -60.0, 0.5]).await;
                client.tps().await;
                client.ack(id).await;
                client.tps().await;
            }
            client.movement([47.5, -60.0, 0.5]).await;
            client.tps().await;
            let mut dig = vec![41, 0];
            let packed = (48_u64 << 38) | ((-61_i64 as u64) & 0xfff);
            dig.extend_from_slice(&packed.to_be_bytes());
            dig.extend_from_slice(&[1, 9]);
            client.send(&dig).await;
            assert_eq!(client.until(4).await, [4, 9]);
            client.tps().await;
            observer.tps().await;
            drop(observer);
        }
        client.tps().await;
        drop(client);
        server.stop().await;
    }
    let (spawn_path, spawn_bytes) = chunks_before
        .iter()
        .find(|(_, bytes)| {
            let chunk = zerqon_world::Chunk::from_bytes(bytes).unwrap();
            (chunk.x, chunk.z) == (0, 0)
        })
        .unwrap();
    let spawn_path = world.join(spawn_path);
    let held = spawn_path.with_extension("held");
    fs::rename(&spawn_path, &held).unwrap();
    startup_failure(&directory, &data, "imported world has no chunk").await;
    fs::rename(&held, &spawn_path).unwrap();
    fs::write(&spawn_path, b"corrupt").unwrap();
    startup_failure(&directory, &data, "invalid chunk length").await;
    fs::write(&spawn_path, spawn_bytes).unwrap();
    let chunks_after: BTreeMap<_, _> = snapshot(&world)
        .into_iter()
        .filter(|(path, _)| path.extension().is_some_and(|ext| ext == "zqc"))
        .collect();
    assert_eq!(
        chunks_after, chunks_before,
        "missing imported terrain must never become flat chunks"
    );
    assert_eq!(snapshot(&world.join("source-data")), retained_before);
    assert_eq!(snapshot(&source), source_before);
    assert_eq!(snapshot(&fixture), original);
}
