use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, mpsc},
    time::Duration,
};
use uuid::Uuid;
use zerqon_world::{
    Chunk, ChunkKey, ChunkWrite, DropData, DropKey, DropWrite, FlatPalette, PlayerKey, PlayerWrite,
    WorldStore,
};

const PALETTE: FlatPalette = FlatPalette {
    air: 0,
    bedrock: 1,
    stone: 2,
    dirt: 3,
    grass: 4,
};
const OPERATION: u128 = 0x4f8e96d0_64c1_4bbd_ab17_65769351bf9e;

fn keys() -> Vec<ChunkKey> {
    (0..16).map(|x| ChunkKey::new(x, -x)).collect()
}

fn writes(value: u32) -> Vec<ChunkWrite> {
    keys()
        .into_iter()
        .map(|key| {
            let mut chunk = Chunk::flat(key.x, key.z, PALETTE);
            chunk.set_block(0, 64, 0, value).unwrap();
            ChunkWrite {
                key,
                replacement: Arc::new(chunk),
            }
        })
        .collect()
}

struct ChildProcess(Child);
impl Drop for ChildProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_child(test: &str, root: &Path) -> (ChildProcess, mpsc::Receiver<String>) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("ZERQON_RECOVERY_CHILD", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    (ChildProcess(child), receiver)
}

fn wait_for(lines: &mpsc::Receiver<String>, expected: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let line = lines
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .unwrap();
        if line == expected {
            return;
        }
    }
}

fn signal(line: &str) {
    println!("{line}");
    std::io::stdout().flush().unwrap();
}

fn child_root() -> Option<PathBuf> {
    std::env::var_os("ZERQON_RECOVERY_CHILD").map(PathBuf::from)
}

#[test]
fn killed_multi_chunk_writer_recovers_one_revision_and_matching_receipt() {
    if let Some(root) = child_root() {
        let store = WorldStore::open(root).unwrap();
        let reads = store.snapshot(&keys()).unwrap();
        let writes = writes(22);
        signal("ready");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        signal("committing");
        store
            .commit(Uuid::from_u128(OPERATION), &reads, &writes)
            .unwrap();
        signal("committed");
        line.clear();
        std::io::stdin().read_line(&mut line).unwrap();
        return;
    }
    for (mode, delay) in [("during", 0), ("during", 1), ("after", 0)] {
        let root = tempfile::tempdir().unwrap();
        let store = WorldStore::open(root.path()).unwrap();
        let before = store
            .commit(
                Uuid::new_v4(),
                &store.snapshot(&keys()).unwrap(),
                &writes(11),
            )
            .unwrap();
        drop(store);
        let (mut child, lines) = spawn_child(
            "killed_multi_chunk_writer_recovers_one_revision_and_matching_receipt",
            root.path(),
        );
        wait_for(&lines, "ready");
        writeln!(child.0.stdin.as_mut().unwrap(), "go").unwrap();
        wait_for(
            &lines,
            if mode == "after" {
                "committed"
            } else {
                "committing"
            },
        );
        if delay != 0 {
            std::thread::sleep(Duration::from_millis(delay));
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        let recovered = WorldStore::open(root.path()).unwrap();
        let snapshots = recovered.snapshot(&keys()).unwrap();
        let value = snapshots[0].chunk().unwrap().get_block(0, 64, 0).unwrap();
        let revision = snapshots[0].revision();
        assert!(matches!(value, 11 | 22));
        for snapshot in snapshots {
            assert_eq!(snapshot.chunk().unwrap().get_block(0, 64, 0), Some(value));
            assert_eq!(snapshot.revision(), revision);
        }
        let receipt = recovered.receipt(Uuid::from_u128(OPERATION)).unwrap();
        if value == 22 {
            let receipt = receipt.unwrap();
            assert_eq!(receipt.revision, revision);
            assert_eq!(receipt.chunks.len(), 16);
            assert!(revision > before.revision);
        } else {
            assert!(receipt.is_none());
            assert_eq!(revision, before.revision);
        }
        if mode == "after" {
            assert_eq!(value, 22);
        }
    }
}

#[test]
fn killed_mixed_writer_recovers_player_block_drop_and_chunk_index_together() {
    let chunk_key = ChunkKey::new(0, 0);
    let old_drop_chunk = ChunkKey::new(1, 0);
    let player_key = PlayerKey::new(Uuid::from_u128(1));
    let drop_key = DropKey::new(Uuid::from_u128(2));
    let commit = |store: &WorldStore, operation: Uuid, value: u8, location: ChunkKey| {
        let reads = store
            .snapshot_domain(&[chunk_key], &[player_key], &[drop_key])
            .unwrap();
        let mut chunk = Chunk::flat(0, 0, PALETTE);
        chunk.set_block(0, 64, 0, u32::from(value)).unwrap();
        store
            .commit_domain(
                operation,
                &reads,
                &[ChunkWrite {
                    key: chunk_key,
                    replacement: Arc::new(chunk),
                }],
                &[PlayerWrite {
                    key: player_key,
                    replacement: Arc::from([value]),
                }],
                &[DropWrite {
                    key: drop_key,
                    replacement: Some(DropData {
                        chunk: location,
                        data: Arc::from([value]),
                    }),
                }],
            )
            .unwrap()
    };
    if let Some(root) = child_root() {
        let store = WorldStore::open(root).unwrap();
        signal("ready");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        signal("committing");
        commit(&store, Uuid::from_u128(OPERATION), 22, chunk_key);
        signal("committed");
        line.clear();
        std::io::stdin().read_line(&mut line).unwrap();
        return;
    }
    for (mode, delay) in [("during", 0), ("during", 1), ("after", 0)] {
        let root = tempfile::tempdir().unwrap();
        let store = WorldStore::open(root.path()).unwrap();
        let before = commit(&store, Uuid::new_v4(), 11, old_drop_chunk);
        drop(store);
        let (mut child, lines) = spawn_child(
            "killed_mixed_writer_recovers_player_block_drop_and_chunk_index_together",
            root.path(),
        );
        wait_for(&lines, "ready");
        writeln!(child.0.stdin.as_mut().unwrap(), "go").unwrap();
        wait_for(
            &lines,
            if mode == "after" {
                "committed"
            } else {
                "committing"
            },
        );
        if delay != 0 {
            std::thread::sleep(Duration::from_millis(delay));
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        let recovered = WorldStore::open(root.path()).unwrap();
        let state = recovered
            .snapshot_domain(&[chunk_key], &[player_key], &[drop_key])
            .unwrap();
        let value = state.players[0].data().unwrap()[0];
        assert!(matches!(value, 11 | 22));
        assert_eq!(
            state.chunks[0].chunk().unwrap().get_block(0, 64, 0),
            Some(u32::from(value))
        );
        assert_eq!(state.drops[0].data().unwrap().as_ref(), &[value]);
        assert_eq!(state.chunks[0].revision(), state.players[0].revision());
        assert_eq!(state.drops[0].revision(), state.players[0].revision());
        let expected_chunk = if value == 22 {
            chunk_key
        } else {
            old_drop_chunk
        };
        let empty_chunk = if value == 22 {
            old_drop_chunk
        } else {
            chunk_key
        };
        assert_eq!(state.drops[0].chunk(), Some(expected_chunk));
        assert_eq!(
            recovered
                .drops_in_chunk(expected_chunk, None, 64)
                .unwrap()
                .len(),
            1
        );
        assert!(
            recovered
                .drops_in_chunk(empty_chunk, None, 64)
                .unwrap()
                .is_empty()
        );
        let receipt = recovered
            .domain_receipt(Uuid::from_u128(OPERATION))
            .unwrap();
        if value == 22 {
            let receipt = receipt.unwrap();
            assert_eq!(receipt.revision, state.players[0].revision());
            assert!(receipt.revision > before.revision);
        } else {
            assert!(receipt.is_none());
            assert_eq!(state.players[0].revision(), before.revision);
        }
        if mode == "after" {
            assert_eq!(value, 22);
        }
    }
}

#[test]
fn missing_or_corrupt_active_database_never_falls_back_to_preserved_chunks() {
    for corrupt in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let original = Chunk::flat(0, 0, PALETTE).to_bytes();
        fs::write(root.path().join("c.0.0.zqc"), &original).unwrap();
        let store = WorldStore::open(root.path()).unwrap();
        store.save(&writes(22).remove(0).replacement).unwrap();
        drop(store);
        let database = root.path().join("chunks.redb");
        if corrupt {
            fs::write(&database, b"corrupt active database").unwrap();
        } else {
            fs::remove_file(&database).unwrap();
        }
        assert!(WorldStore::open(root.path()).is_err());
        assert_eq!(fs::read(root.path().join("c.0.0.zqc")).unwrap(), original);
        if corrupt {
            assert_eq!(fs::read(database).unwrap(), b"corrupt active database");
        } else {
            assert!(!database.exists());
        }
    }
}

#[test]
fn active_database_hardlink_alias_is_rejected_without_modification() {
    let root = tempfile::tempdir().unwrap();
    let world = root.path().join("world");
    drop(WorldStore::open(&world).unwrap());
    let database = world.join("chunks.redb");
    let alias = root.path().join("alias.redb");
    fs::hard_link(&database, &alias).unwrap();
    let original = fs::read(&database).unwrap();
    assert!(WorldStore::open(&world).is_err());
    assert_eq!(fs::read(database).unwrap(), original);
    assert_eq!(fs::read(alias).unwrap(), original);
}

#[test]
fn corrupt_legacy_chunk_cannot_publish_an_active_database() {
    let root = tempfile::tempdir().unwrap();
    let bytes = Chunk::flat(0, 0, PALETTE).to_bytes();
    fs::write(root.path().join("c.0.0.zqc"), &bytes).unwrap();
    fs::write(root.path().join("c.1.0.zqc"), b"invalid chunk").unwrap();
    assert!(WorldStore::open(root.path()).is_err());
    assert!(!root.path().join("chunks.redb").exists());
    assert_eq!(fs::read(root.path().join("c.0.0.zqc")).unwrap(), bytes);
    assert_eq!(
        fs::read(root.path().join("c.1.0.zqc")).unwrap(),
        b"invalid chunk"
    );
}

#[test]
fn interrupted_v2_guard_only_migration_preserves_and_recovers_original_chunks() {
    let guard = b"ZERQON_STORAGE_V2\nThis world uses chunks.redb. Older Zerqon versions must not open it.\n";
    let root = tempfile::tempdir().unwrap();
    let original = Chunk::flat(-2, 3, PALETTE).to_bytes();
    fs::write(root.path().join("c.-2.3.zqc"), &original).unwrap();
    fs::write(root.path().join("IMPORT_INCOMPLETE"), guard).unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    assert_eq!(store.load(-2, 3).unwrap().unwrap().to_bytes(), original);
    assert_eq!(fs::read(root.path().join("c.-2.3.zqc")).unwrap(), original);
    assert!(
        fs::read(root.path().join("IMPORT_INCOMPLETE"))
            .unwrap()
            .starts_with(b"ZERQON_STORAGE_V3\n")
    );
    assert!(root.path().join("storage-format.json").is_file());
}

#[test]
fn legacy_hardlink_alias_cannot_be_migrated_as_owned_world_data() {
    let root = tempfile::tempdir().unwrap();
    let world = root.path().join("world");
    fs::create_dir(&world).unwrap();
    let external = root.path().join("external.zqc");
    let original = Chunk::flat(0, 0, PALETTE).to_bytes();
    fs::write(&external, &original).unwrap();
    fs::hard_link(&external, world.join("c.0.0.zqc")).unwrap();
    assert!(WorldStore::open(&world).is_err());
    assert!(!world.join("chunks.redb").exists());
    assert_eq!(fs::read(external).unwrap(), original);
}

#[test]
fn copied_database_cannot_accept_another_open_stores_snapshot_tokens() {
    let root = tempfile::tempdir().unwrap();
    let original = root.path().join("original");
    let copy = root.path().join("copy");
    drop(WorldStore::open(&original).unwrap());
    fs::create_dir(&copy).unwrap();
    for file in ["chunks.redb", "storage-format.json", "IMPORT_INCOMPLETE"] {
        fs::copy(original.join(file), copy.join(file)).unwrap();
    }
    let original = WorldStore::open(original).unwrap();
    let copy = WorldStore::open(copy).unwrap();
    let foreign_reads = original.snapshot(&keys()).unwrap();
    assert!(
        copy.commit(Uuid::new_v4(), &foreign_reads, &writes(22))
            .is_err()
    );
    assert!(copy.load(0, 0).unwrap().is_none());
}

#[test]
fn killed_migration_preserves_originals_and_never_reimports_under_v3_guard() {
    if let Some(root) = child_root() {
        signal("migrating");
        let store = WorldStore::open(&root).unwrap();
        signal("opened");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        drop(store);
        return;
    }
    for delay in [0, 2, 10] {
        let root = tempfile::tempdir().unwrap();
        let originals: Vec<_> = (0..64)
            .map(|x| {
                let name = format!("c.{x}.0.zqc");
                let bytes = Chunk::flat(x, 0, PALETTE).to_bytes();
                fs::write(root.path().join(&name), &bytes).unwrap();
                (name, bytes)
            })
            .collect();
        let (mut child, lines) = spawn_child(
            "killed_migration_preserves_originals_and_never_reimports_under_v3_guard",
            root.path(),
        );
        wait_for(&lines, "migrating");
        if delay != 0 {
            std::thread::sleep(Duration::from_millis(delay));
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        if root.path().join("chunks.redb").exists() {
            assert!(
                root.path().join("IMPORT_INCOMPLETE").is_file(),
                "active database must already be guarded from old binaries"
            );
        }
        for (name, bytes) in &originals {
            assert_eq!(fs::read(root.path().join(name)).unwrap(), *bytes);
        }
        if !root.path().join("chunks.redb").exists()
            && fs::read(root.path().join("IMPORT_INCOMPLETE"))
                .is_ok_and(|guard| guard.starts_with(b"ZERQON_STORAGE_V3\n"))
        {
            assert!(
                WorldStore::open(root.path()).is_err(),
                "ambiguous V3 loss cannot resurrect legacy chunks"
            );
            continue;
        }
        let recovered = WorldStore::open(root.path()).unwrap();
        for x in 0..64 {
            assert_eq!(
                recovered.load(x, 0).unwrap().unwrap().to_bytes(),
                originals[x as usize].1
            );
        }
    }
}
