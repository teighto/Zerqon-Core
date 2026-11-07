use std::{fs, sync::Arc};
use uuid::Uuid;
use zerqon_world::{Chunk, ChunkKey, ChunkWrite, CommitError, FlatPalette, WorldStore};

const PALETTE: FlatPalette = FlatPalette {
    air: 0,
    bedrock: 1,
    stone: 2,
    dirt: 3,
    grass: 4,
};

#[test]
fn migration_preserves_original_chunks_and_activates_one_transaction_store() {
    let root = tempfile::tempdir().unwrap();
    let chunk = Chunk::flat(0, 0, PALETTE);
    let original = chunk.to_bytes();
    fs::write(root.path().join("c.0.0.zqc"), &original).unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    assert!(root.path().join("chunks.redb").is_file());
    assert!(root.path().join("storage-format.json").is_file());
    assert!(root.path().join("IMPORT_INCOMPLETE").is_file());
    assert_eq!(store.load(0, 0).unwrap().unwrap().to_bytes(), original);
    let mut changed = chunk;
    changed.set_block(1, 64, 1, 55).unwrap();
    store.save(&changed).unwrap();
    drop(store);
    let reopened = WorldStore::open(root.path()).unwrap();
    assert_eq!(
        reopened.load(0, 0).unwrap().unwrap().get_block(1, 64, 1),
        Some(55)
    );
    assert_eq!(fs::read(root.path().join("c.0.0.zqc")).unwrap(), original);
}

fn write(key: ChunkKey, value: u32) -> ChunkWrite {
    let mut chunk = Chunk::flat(key.x, key.z, PALETTE);
    chunk.set_block(0, 64, 0, value).unwrap();
    ChunkWrite {
        key,
        replacement: Arc::new(chunk),
    }
}

#[test]
fn multi_chunk_commit_is_atomic_persistent_and_idempotent_by_operation() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let keys = [ChunkKey::new(-1, 0), ChunkKey::new(0, -1)];
    let before = store.snapshot(&keys).unwrap();
    let operation = Uuid::new_v4();
    let writes = [write(keys[0], 11), write(keys[1], 22)];
    let receipt = store.commit(operation, &before, &writes).unwrap();
    assert_eq!(store.commit(operation, &before, &writes).unwrap(), receipt);
    assert_eq!(store.receipt(operation).unwrap(), Some(receipt.clone()));
    let after = store.snapshot(&keys).unwrap();
    assert_eq!(
        after
            .iter()
            .map(|snapshot| snapshot.revision())
            .collect::<Vec<_>>(),
        vec![receipt.revision; 2]
    );
    assert_eq!(after[0].chunk().unwrap().get_block(0, 64, 0), Some(11));
    assert_eq!(after[1].chunk().unwrap().get_block(0, 64, 0), Some(22));
    drop(store);
    let reopened = WorldStore::open(root.path()).unwrap();
    assert_eq!(reopened.receipt(operation).unwrap(), Some(receipt));
    assert_eq!(
        reopened.load(-1, 0).unwrap().unwrap().get_block(0, 64, 0),
        Some(11)
    );
    assert_eq!(
        reopened.load(0, -1).unwrap().unwrap().get_block(0, 64, 0),
        Some(22)
    );
}

#[test]
fn read_only_neighbor_conflict_prevents_every_write() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let keys = [ChunkKey::new(0, 0), ChunkKey::new(1, 0)];
    let stale = store.snapshot(&keys).unwrap();
    store
        .commit(
            Uuid::new_v4(),
            &store.snapshot(&keys[1..]).unwrap(),
            &[write(keys[1], 22)],
        )
        .unwrap();
    assert!(matches!(
        store.commit(Uuid::new_v4(), &stale, &[write(keys[0], 11)]),
        Err(CommitError::Conflict)
    ));
    assert!(store.load(0, 0).unwrap().is_none());
    assert_eq!(
        store.load(1, 0).unwrap().unwrap().get_block(0, 64, 0),
        Some(22)
    );
}

#[test]
fn foreign_tokens_duplicate_keys_and_reused_ids_with_changed_payload_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let foreign = WorldStore::open(other.path()).unwrap();
    let key = ChunkKey::new(0, 0);
    let reads = store.snapshot(&[key]).unwrap();
    assert!(store.snapshot(&[key, key]).is_err());
    assert!(
        store
            .commit(
                Uuid::new_v4(),
                &foreign.snapshot(&[key]).unwrap(),
                &[write(key, 1)]
            )
            .is_err()
    );
    assert!(store.commit(Uuid::nil(), &reads, &[write(key, 1)]).is_err());
    assert!(
        store
            .commit(Uuid::new_v4(), &reads, &[write(key, 1), write(key, 2)])
            .is_err()
    );
    let op = Uuid::new_v4();
    store.commit(op, &reads, &[write(key, 11)]).unwrap();
    assert!(store.commit(op, &reads, &[write(key, 22)]).is_err());
    assert_eq!(
        store.load(0, 0).unwrap().unwrap().get_block(0, 64, 0),
        Some(11)
    );
}

#[test]
fn concurrent_multi_chunk_readers_never_observe_partial_commits() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(WorldStore::open(root.path()).unwrap());
    let keys = [ChunkKey::new(0, 0), ChunkKey::new(1, 31)];
    store
        .commit(
            Uuid::new_v4(),
            &store.snapshot(&keys).unwrap(),
            &[write(keys[0], 0), write(keys[1], 0)],
        )
        .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            for _ in 0..100 {
                let read = store.snapshot(&keys).unwrap();
                assert_eq!(
                    read[0].chunk().unwrap().get_block(0, 64, 0),
                    read[1].chunk().unwrap().get_block(0, 64, 0)
                );
                assert_eq!(read[0].revision(), read[1].revision());
            }
        }));
    }
    barrier.wait();
    for value in 1..=30 {
        store
            .commit(
                Uuid::new_v4(),
                &store.snapshot(&keys).unwrap(),
                &[write(keys[0], value), write(keys[1], value)],
            )
            .unwrap();
    }
    for thread in threads {
        thread.join().unwrap();
    }
}

#[test]
fn conflicting_writers_from_one_snapshot_have_exactly_one_winner() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(WorldStore::open(root.path()).unwrap());
    let key = ChunkKey::new(0, 0);
    let before = store.snapshot(&[key]).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let mut workers = Vec::new();
    for value in [11, 22] {
        let store = Arc::clone(&store);
        let before = before.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            store.commit(Uuid::new_v4(), &before, &[write(key, value)])
        }));
    }
    barrier.wait();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(CommitError::Conflict)))
            .count(),
        1
    );
    assert!(matches!(
        store.load(0, 0).unwrap().unwrap().get_block(0, 64, 0),
        Some(11 | 22)
    ));
}

#[test]
fn writes_outside_read_set_and_excessive_batches_do_not_change_world() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let key = ChunkKey::new(0, 0);
    let before = store.snapshot(&[key]).unwrap();
    assert!(
        store
            .commit(Uuid::new_v4(), &before, &[write(ChunkKey::new(1, 0), 22)])
            .is_err()
    );
    let keys: Vec<_> = (0..65).map(|x| ChunkKey::new(x, 0)).collect();
    assert!(store.snapshot(&keys).is_err());
    let reads = store.snapshot(&keys[..17]).unwrap();
    let writes: Vec<_> = keys[..17].iter().map(|key| write(*key, 22)).collect();
    assert!(store.commit(Uuid::new_v4(), &reads, &writes).is_err());
    for key in &keys {
        assert!(store.load(key.x, key.z).unwrap().is_none());
    }
}

#[test]
fn corrupted_chunk_value_is_rejected_without_using_preserved_legacy_bytes() {
    use redb::TableDefinition;
    let root = tempfile::tempdir().unwrap();
    let original = Chunk::flat(0, 0, PALETTE).to_bytes();
    fs::write(root.path().join("c.0.0.zqc"), &original).unwrap();
    drop(WorldStore::open(root.path()).unwrap());
    const TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("chunks");
    let db = redb::Database::open(root.path().join("chunks.redb")).unwrap();
    let transaction = db.begin_write().unwrap();
    {
        let mut table = transaction.open_table(TABLE).unwrap();
        let mut invalid = 1u64.to_le_bytes().to_vec();
        invalid.extend_from_slice(&original);
        *invalid.last_mut().unwrap() ^= 1;
        table.insert(0, invalid.as_slice()).unwrap();
    }
    transaction.commit().unwrap();
    drop(db);
    let store = WorldStore::open(root.path()).unwrap();
    assert!(store.load(0, 0).is_err());
    assert!(store.check_available().is_err());
    assert!(store.save(&Chunk::flat(0, 0, PALETTE)).is_err());
    assert_eq!(fs::read(root.path().join("c.0.0.zqc")).unwrap(), original);
}
