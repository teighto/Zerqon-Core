use std::fs;
use zerqon_world::{Chunk, FlatPalette, WorldStore};

const PALETTE: FlatPalette = FlatPalette {
    air: 0,
    bedrock: 1,
    stone: 2,
    dirt: 3,
    grass: 4,
};

#[test]
fn save_restart_keeps_mutated_blocks_and_coordinates() {
    let root = tempfile::tempdir().unwrap();
    let mut chunk = Chunk::flat(-12, 400, PALETTE);
    chunk.set_block(14, 153, 7, u32::MAX).unwrap();
    {
        let store = WorldStore::open(root.path()).unwrap();
        store.save(&chunk).unwrap();
    }
    let store = WorldStore::open(root.path()).unwrap();
    let loaded = store.load(-12, 400).unwrap().unwrap();
    assert_eq!((loaded.x, loaded.z), (-12, 400));
    assert_eq!(loaded.get_block(14, 153, 7), Some(u32::MAX));
    assert_eq!(loaded.get_block(13, 153, 7), Some(0));
}

#[test]
fn rewriting_existing_chunk_replaces_it_and_leaves_no_temporary_files() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let mut chunk = Chunk::flat(0, 0, PALETTE);
    store.save(&chunk).unwrap();
    chunk.set_block(3, 64, 8, 98).unwrap();
    store.save(&chunk).unwrap();
    assert_eq!(
        store.load(0, 0).unwrap().unwrap().get_block(3, 64, 8),
        Some(98)
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
}

#[test]
fn checksum_failure_is_an_error_and_does_not_change_existing_data() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let chunk = Chunk::flat(1, 2, PALETTE);
    store.save(&chunk).unwrap();
    let path = root.path().join("c.1.2.zqc");
    let mut data = fs::read(&path).unwrap();
    *data.last_mut().unwrap() ^= 1;
    fs::write(&path, &data).unwrap();
    assert!(store.load(1, 2).is_err());
    assert!(store.save(&chunk).is_err());
    assert_eq!(fs::read(path).unwrap(), data);
}

#[test]
fn malformed_data_cannot_be_loaded_as_an_empty_chunk() {
    let valid = Chunk::flat(0, 0, PALETTE).to_bytes();
    assert!(!valid.is_empty());
    for cut in [0, 1, 7, 10, valid.len() - 1] {
        assert!(Chunk::from_bytes(&valid[..cut]).is_err());
    }
    let mut trailing = valid.clone();
    trailing.push(0);
    assert!(Chunk::from_bytes(&trailing).is_err());
    let mut corrupted_header = valid;
    corrupted_header[10] ^= 1;
    assert!(Chunk::from_bytes(&corrupted_header).is_err());
}

#[test]
fn world_lock_prevents_two_writers_but_is_released_on_drop() {
    let root = tempfile::tempdir().unwrap();
    let first = WorldStore::open(root.path()).unwrap();
    assert!(WorldStore::open(root.path()).is_err());
    drop(first);
    assert!(WorldStore::open(root.path()).is_ok());
}

#[test]
fn absent_chunk_is_distinct_from_oversized_or_wrong_coordinate_chunk() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    assert!(store.load(10, 10).unwrap().is_none());
    let file = fs::File::create(root.path().join("c.10.10.zqc")).unwrap();
    file.set_len(2 * 1024 * 1024).unwrap();
    assert!(store.load(10, 10).is_err());
    fs::write(
        root.path().join("c.10.10.zqc"),
        Chunk::flat(9, 9, PALETTE).to_bytes(),
    )
    .unwrap();
    assert!(store.load(10, 10).is_err());
}

#[test]
fn all_palette_widths_roundtrip_without_reordering_coordinates() {
    let mut chunk = Chunk::flat(i32::MIN, i32::MAX, PALETTE);
    for index in 0..4096 {
        chunk
            .set_block(
                (index % 16) as u8,
                64 + index / 256,
                ((index / 16) % 16) as u8,
                index as u32,
            )
            .unwrap();
    }
    let bytes = chunk.to_bytes();
    let loaded = Chunk::from_bytes(&bytes).unwrap();
    for (before, after) in chunk.sections().iter().zip(loaded.sections()) {
        assert!(before.iter().eq(after.iter()));
    }
    assert_eq!(bytes, loaded.to_bytes());
}

#[test]
fn valid_checksum_cannot_bypass_palette_validation() {
    let original = Chunk::flat(0, 0, PALETTE).to_bytes();
    for (offset, value) in [(26, 0), (36, 255), (37, 255)] {
        let mut data = original.clone();
        data[offset] = value;
        let mut checksum = crc32fast::Hasher::new();
        checksum.update(&data[..22]);
        checksum.update(&data[26..]);
        data[22..26].copy_from_slice(&checksum.finalize().to_le_bytes());
        assert!(Chunk::from_bytes(&data).is_err(), "offset {offset}");
    }
}

#[test]
fn concurrent_readers_observe_whole_snapshots_during_atomic_replacement() {
    use std::sync::{Arc, Barrier};
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(WorldStore::open(root.path()).unwrap());
    store.save(&Chunk::flat(2, 3, PALETTE)).unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            for _ in 0..100 {
                let chunk = store.load(2, 3).unwrap().unwrap();
                let first = chunk.get_block(0, 64, 0).unwrap();
                assert_eq!(chunk.get_block(15, 319, 15), Some(first));
            }
        }));
    }
    barrier.wait();
    let mut chunk = Chunk::flat(2, 3, PALETTE);
    for state in 1..=100 {
        chunk.set_block(0, 64, 0, state).unwrap();
        chunk.set_block(15, 319, 15, state).unwrap();
        store.save(&chunk).unwrap();
    }
    for thread in threads {
        thread.join().unwrap();
    }
    drop(store);
    let reopened = WorldStore::open(root.path()).unwrap();
    assert_eq!(
        reopened.load(2, 3).unwrap().unwrap().get_block(0, 64, 0),
        Some(100)
    );
}
#[test]
fn an_existing_vanilla_world_cannot_be_silently_opened_as_an_empty_zerqon_world() {
    for marker in [
        "level.dat",
        "level.dat_old",
        "region",
        "dimensions/minecraft/overworld/region",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(marker);
        if marker.ends_with(".dat") || marker.ends_with(".dat_old") {
            std::fs::write(&path, b"original world data").unwrap();
        } else {
            std::fs::create_dir_all(&path).unwrap();
        }
        assert!(
            WorldStore::open(directory.path()).is_err(),
            "accepted foreign world marker {marker}"
        );
        assert!(!directory.path().join("world.lock").exists());
        if path.is_file() {
            assert_eq!(std::fs::read(&path).unwrap(), b"original world data");
        }
    }
}
