#[path = "../src/mob_store.rs"]
mod mob_store;

use std::{fs, fs::File};
use uuid::Uuid;
use zerqon_entities::{EntityWorld, MobKind, PlayerTarget, WorldQuery};

struct Floor;

#[test]
fn store_errors_distinguish_precommit_failure() {
    assert!(!mob_store::StoreError::Commit(std::io::Error::other("test")).was_committed());
    assert!(!mob_store::StoreError::Io(std::io::Error::other("test")).was_committed());
    #[cfg(unix)]
    assert!(mob_store::StoreError::Durability(std::io::Error::other("test")).was_committed());
}
impl WorldQuery for Floor {
    fn block_solid(&self, _x: i32, y: i32, _z: i32) -> bool {
        y < 64
    }
    fn surface_y(&self, _x: i32, _z: i32) -> Option<i32> {
        Some(64)
    }
}

fn populated() -> EntityWorld {
    let mut world = EntityWorld::new(2341863012247763324, 8).unwrap();
    world
        .spawn(
            7,
            Uuid::from_u128(7),
            MobKind::Pig,
            [1.0, 64.0, 2.0],
            &Floor,
        )
        .unwrap();
    world
        .spawn(
            8,
            Uuid::from_u128(8),
            MobKind::Zombie,
            [7.0, 67.0, 3.0],
            &Floor,
        )
        .unwrap();
    for _ in 0..34 {
        world.tick(&Floor, &[]);
    }
    world
}

#[test]
fn atomic_snapshot_roundtrip_preserves_exact_next_ticks() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("entities.zqe");
    assert!(mob_store::load(&path, 8).unwrap().is_none());
    let mut original = populated();
    mob_store::save(&path, &original.snapshot()).unwrap();
    let mut restored = mob_store::load(&path, 8)
        .unwrap()
        .expect("saved entity world");
    assert_eq!(restored.snapshot(), original.snapshot());
    let players = [PlayerTarget {
        id: 100,
        position: [10.0, 64.0, 2.0],
        attackable: true,
    }];
    for _ in 0..80 {
        assert_eq!(
            original.tick(&Floor, &players),
            restored.tick(&Floor, &players)
        );
    }
    mob_store::save(&path, &original.snapshot()).unwrap();
    assert_eq!(
        mob_store::load(&path, 8).unwrap().unwrap().snapshot(),
        original.snapshot()
    );
    assert_eq!(
        fs::read_dir(directory.path()).unwrap().count(),
        1,
        "temporary snapshot files leaked"
    );
}

#[test]
fn invalid_snapshot_never_replaces_a_previous_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("entities.zqe");
    fs::write(&path, b"previous valuable file").unwrap();
    let mut snapshot = populated().snapshot();
    snapshot.entities[0].health = f32::NAN;
    assert!(mob_store::save(&path, &snapshot).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"previous valuable file");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn corrupted_checksum_payload_header_and_trailing_bytes_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("entities.zqe");
    mob_store::save(&path, &populated().snapshot()).unwrap();
    let good = fs::read(&path).unwrap();
    for offset in [0, 8, 10, 14, 46, good.len() - 1] {
        let mut corrupt = good.clone();
        corrupt[offset] ^= 1;
        fs::write(&path, &corrupt).unwrap();
        assert!(
            mob_store::load(&path, 8).is_err(),
            "corruption accepted at {offset}"
        );
    }
    let mut trailing = good.clone();
    trailing.push(0);
    fs::write(&path, &trailing).unwrap();
    assert!(mob_store::load(&path, 8).is_err());
    for length in [0, 7, 8, 13, 45, good.len() - 1] {
        fs::write(&path, &good[..length]).unwrap();
        assert!(
            mob_store::load(&path, 8).is_err(),
            "truncation accepted at {length}"
        );
    }
}

#[test]
fn oversized_and_advertised_huge_payloads_fail_without_large_allocation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("entities.zqe");
    let file = File::create(&path).unwrap();
    file.set_len(8 * 1024 * 1024 + 1).unwrap();
    drop(file);
    assert!(mob_store::load(&path, 8).is_err());
    fs::remove_file(&path).unwrap();
    mob_store::save(&path, &populated().snapshot()).unwrap();
    let mut data = fs::read(&path).unwrap();
    data[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&path, data).unwrap();
    assert!(mob_store::load(&path, 8).is_err());
}

#[test]
fn capacity_and_duplicate_entities_are_validated_on_restore() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("entities.zqe");
    mob_store::save(&path, &populated().snapshot()).unwrap();
    assert!(mob_store::load(&path, 1).is_err());
    let original = fs::read(&path).unwrap();
    let mut snapshot = populated().snapshot();
    snapshot.entities.push(snapshot.entities[0].clone());
    assert!(mob_store::save(&path, &snapshot).is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[test]
fn saving_does_not_silently_overwrite_a_corrupt_existing_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("entities.zqe");
    let snapshot = populated().snapshot();
    mob_store::save(&path, &snapshot).unwrap();
    let mut bytes = fs::read(&path).unwrap();
    bytes[14] ^= 1;
    fs::write(&path, &bytes).unwrap();
    assert!(mob_store::save(&path, &snapshot).is_err());
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn valid_checksum_does_not_bypass_semantic_snapshot_validation() {
    use aws_lc_rs::digest::{Context, SHA256};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("entities.zqe");
    let original = populated().snapshot();
    mob_store::save(&path, &original).unwrap();
    let prefix = fs::read(&path).unwrap()[..14].to_vec();
    for mode in 0..3 {
        let mut snapshot = original.clone();
        match mode {
            0 => snapshot.entities[0].health = f32::NAN,
            1 => snapshot.schema = 999,
            _ => snapshot.entities.push(snapshot.entities[0].clone()),
        }
        let payload = serde_json::to_vec(&snapshot).unwrap();
        let mut bytes = prefix.clone();
        bytes[10..14].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let mut checksum = Context::new(&SHA256);
        checksum.update(&bytes);
        checksum.update(&payload);
        bytes.extend_from_slice(checksum.finish().as_ref());
        bytes.extend_from_slice(&payload);
        fs::write(&path, bytes).unwrap();
        assert!(
            mob_store::load(&path, 8).is_err(),
            "invalid semantic state accepted: {mode}"
        );
    }
}

#[test]
fn concurrent_reader_observes_complete_old_or_new_snapshots() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("entities.zqe");
    let mut world = populated();
    mob_store::save(&path, &world.snapshot()).unwrap();
    let complete = Arc::new(AtomicBool::new(false));
    let writer_complete = Arc::clone(&complete);
    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(move || {
            for _ in 0..50 {
                world.tick(&Floor, &[]);
                mob_store::save(&writer_path, &world.snapshot()).unwrap();
            }
        });
        writer_complete.store(true, Ordering::Release);
        result.unwrap();
    });
    loop {
        let snapshot = mob_store::load(&path, 8).unwrap().unwrap().snapshot();
        assert!((34..=84).contains(&snapshot.tick));
        assert_eq!(snapshot.entities.len(), 2);
        if complete.load(Ordering::Acquire) {
            break;
        }
    }
    writer.join().unwrap();
    assert_eq!(
        mob_store::load(&path, 8).unwrap().unwrap().snapshot().tick,
        84
    );
}

#[cfg(windows)]
#[test]
fn blocked_windows_replacement_is_bounded_and_keeps_previous_snapshot() {
    use std::{
        fs::OpenOptions,
        os::windows::fs::OpenOptionsExt,
        time::{Duration, Instant},
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("entities.zqe");
    let mut world = populated();
    mob_store::save(&path, &world.snapshot()).unwrap();
    let original = fs::read(&path).unwrap();
    let held = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&path)
        .unwrap();
    world.tick(&Floor, &[]);
    let started = Instant::now();
    assert!(matches!(
        mob_store::save(&path, &world.snapshot()),
        Err(mob_store::StoreError::Commit(_))
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    drop(held);
    mob_store::save(&path, &world.snapshot()).unwrap();
    assert_eq!(
        mob_store::load(&path, 8).unwrap().unwrap().snapshot(),
        world.snapshot()
    );
}
