use redb::{Database, TableDefinition};
use std::sync::Arc;
use uuid::Uuid;
use zerqon_world::{
    Chunk, ChunkKey, ChunkWrite, CommitError, DomainSnapshot, DropData, DropKey, DropWrite,
    FlatPalette, PlayerKey, PlayerWrite, WorldError, WorldStore,
};

fn chunk_write(key: ChunkKey) -> ChunkWrite {
    ChunkWrite {
        key,
        replacement: Arc::new(Chunk::flat(
            key.x,
            key.z,
            FlatPalette {
                air: 0,
                bedrock: 1,
                stone: 2,
                dirt: 3,
                grass: 4,
            },
        )),
    }
}

fn player_write(key: PlayerKey) -> PlayerWrite {
    PlayerWrite {
        key,
        replacement: Arc::from([7]),
    }
}

fn drop_write(key: DropKey, chunk: ChunkKey) -> DropWrite {
    DropWrite {
        key,
        replacement: Some(DropData {
            chunk,
            data: Arc::from([9]),
        }),
    }
}

#[test]
fn mixed_512_record_receipt_survives_retry_reopen_and_history_eviction() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let chunks: Vec<_> = (0..16).map(|x| ChunkKey::new(x, -x)).collect();
    let player = PlayerKey::new(Uuid::from_u128(1));
    let drops: Vec<_> = (1..=495)
        .map(|id| DropKey::new(Uuid::from_u128(id)))
        .collect();
    let reads = store.snapshot_domain(&chunks, &[player], &drops).unwrap();
    let chunk_writes: Vec<_> = chunks.iter().copied().map(chunk_write).collect();
    let player_writes = [player_write(player)];
    let drop_writes: Vec<_> = drops
        .iter()
        .copied()
        .map(|key| drop_write(key, chunks[0]))
        .collect();
    let operation = Uuid::new_v4();
    let receipt = store
        .commit_domain(
            operation,
            &reads,
            &chunk_writes,
            &player_writes,
            &drop_writes,
        )
        .unwrap();
    assert_eq!(
        (
            receipt.chunks.len(),
            receipt.players.len(),
            receipt.drops.len()
        ),
        (16, 1, 495)
    );
    assert!(serde_json::to_vec(&receipt).unwrap().len() > 8192);
    assert_eq!(
        store
            .commit_domain(
                operation,
                &reads,
                &chunk_writes,
                &player_writes,
                &drop_writes
            )
            .unwrap(),
        receipt
    );
    assert_eq!(
        store.domain_receipt(operation).unwrap(),
        Some(receipt.clone())
    );
    drop(store);

    let store = WorldStore::open(root.path()).unwrap();
    assert_eq!(
        store.domain_receipt(operation).unwrap(),
        Some(receipt.clone())
    );
    let persisted = store.snapshot_domain(&chunks, &[player], &drops).unwrap();
    assert!(
        persisted
            .chunks
            .iter()
            .all(|entry| entry.revision() == receipt.revision && entry.chunk().is_some())
    );
    assert_eq!(persisted.players[0].data().unwrap().as_ref(), &[7]);
    assert!(
        persisted
            .drops
            .iter()
            .all(|entry| entry.revision() == receipt.revision
                && entry.data().unwrap().as_ref() == [9])
    );
    let mut cursor = None;
    let mut indexed = Vec::new();
    loop {
        let page = store.drops_in_chunk(chunks[0], cursor, 64).unwrap();
        if page.is_empty() {
            break;
        }
        cursor = Some(page.last().unwrap().key());
        indexed.extend(page.into_iter().map(|entry| entry.key()));
    }
    assert_eq!(indexed, drops);
    drop(store);

    // A revision gap reaches the real history-pruning path without thousands of disk commits.
    let db = Database::open(root.path().join("chunks.redb")).unwrap();
    let transaction = db.begin_write().unwrap();
    {
        let mut meta = transaction
            .open_table(TableDefinition::<&str, &[u8]>::new("meta"))
            .unwrap();
        meta.insert(
            "revision",
            (receipt.revision + 4095).to_le_bytes().as_slice(),
        )
        .unwrap();
    }
    transaction.commit().unwrap();
    drop(db);
    let store = WorldStore::open(root.path()).unwrap();
    let fresh = store.snapshot_domain(&[], &[player], &[]).unwrap();
    let next = store
        .commit_domain(Uuid::new_v4(), &fresh, &[], &player_writes, &[])
        .unwrap();
    assert_eq!(next.revision, receipt.revision + 4096);
    assert!(store.domain_receipt(operation).unwrap().is_none());
    assert_eq!(store.domain_receipt(next.operation).unwrap(), Some(next));
    assert_eq!(store.drops_in_chunk(chunks[0], None, 64).unwrap().len(), 64);
    store.check_available().unwrap();
}

#[test]
fn domain_513th_key_or_write_is_rejected_without_publishing_any_record() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let players: Vec<_> = (1..=256)
        .map(|id| PlayerKey::new(Uuid::from_u128(id)))
        .collect();
    let drops: Vec<_> = (1..=257)
        .map(|id| DropKey::new(Uuid::from_u128(id)))
        .collect();
    let reads = store.snapshot_domain(&[], &players, &drops[..256]).unwrap();
    assert!(matches!(
        store.snapshot_domain(&[], &players, &drops),
        Err(WorldError::TransactionLimit(_))
    ));
    let extra = store.snapshot_domain(&[], &[], &drops[256..]).unwrap();
    let mut too_many_reads = reads.clone();
    too_many_reads.drops.extend(extra.drops);
    let player_writes: Vec<_> = players.iter().copied().map(player_write).collect();
    let drop_writes: Vec<_> = drops
        .iter()
        .copied()
        .map(|key| drop_write(key, ChunkKey::new(0, 0)))
        .collect();
    for (reads, players, drops) in [
        (&too_many_reads, &player_writes[..1], &drop_writes[..0]),
        (&reads, player_writes.as_slice(), drop_writes.as_slice()),
    ] {
        let operation = Uuid::new_v4();
        assert!(matches!(
            store.commit_domain(operation, reads, &[], players, drops),
            Err(CommitError::Rejected(WorldError::TransactionLimit(_)))
        ));
        assert!(store.domain_receipt(operation).unwrap().is_none());
    }
    let unchanged = store.snapshot_domain(&[], &players, &drops[..256]).unwrap();
    assert!(unchanged.players.iter().all(|entry| entry.data().is_none()));
    assert!(unchanged.drops.iter().all(|entry| entry.data().is_none()));
    assert!(store.scan_drops(None, 64).unwrap().is_empty());
    store
        .commit_domain(
            Uuid::new_v4(),
            &reads,
            &[],
            &player_writes,
            &drop_writes[..256],
        )
        .unwrap();
    store.check_available().unwrap();
}

#[test]
fn expanded_domain_preserves_chunk_read_write_and_drop_page_bounds() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let chunks: Vec<_> = (0..65).map(|x| ChunkKey::new(x, 0)).collect();
    let players: Vec<_> = (1..=449)
        .map(|id| PlayerKey::new(Uuid::from_u128(id)))
        .collect();
    assert_eq!(store.snapshot(&chunks[..64]).unwrap().len(), 64);
    assert!(store.snapshot(&chunks).is_err());
    assert!(
        store
            .snapshot_domain(&chunks[..64], &players[..448], &[])
            .is_ok()
    );
    assert!(store.snapshot_domain(&chunks[..64], &players, &[]).is_err());
    assert!(store.snapshot_domain(&chunks, &[], &[]).is_err());
    let mut excessive_chunks = DomainSnapshot {
        chunks: store.snapshot(&chunks[..64]).unwrap(),
        ..Default::default()
    };
    excessive_chunks
        .chunks
        .extend(store.snapshot(&chunks[64..]).unwrap());
    let writes: Vec<_> = chunks[..17].iter().copied().map(chunk_write).collect();
    assert!(
        store
            .commit_domain(Uuid::new_v4(), &excessive_chunks, &writes[..1], &[], &[])
            .is_err()
    );
    assert!(
        store
            .commit(Uuid::new_v4(), &excessive_chunks.chunks, &writes[..1])
            .is_err()
    );
    let reads = store
        .snapshot_domain(&chunks[..17], &players[..1], &[])
        .unwrap();
    assert!(
        store
            .commit_domain(
                Uuid::new_v4(),
                &reads,
                &writes,
                &[player_write(players[0])],
                &[]
            )
            .is_err()
    );
    assert!(
        store
            .commit(Uuid::new_v4(), &reads.chunks, &writes)
            .is_err()
    );
    assert!(
        store
            .snapshot(&chunks[..17])
            .unwrap()
            .iter()
            .all(|entry| entry.chunk().is_none())
    );
    assert!(store.scan_drops(None, 65).is_err());
    assert!(store.drops_in_chunk(chunks[0], None, 65).is_err());
    assert!(store.scan_drops(None, 0).is_err());
    assert!(store.drops_in_chunk(chunks[0], None, 0).is_err());
    assert!(store.scan_drops(None, 64).unwrap().is_empty());
    assert!(
        store
            .drops_in_chunk(chunks[0], None, 64)
            .unwrap()
            .is_empty()
    );
    store
        .commit_domain(
            Uuid::new_v4(),
            &reads,
            &writes[..16],
            &[player_write(players[0])],
            &[],
        )
        .unwrap();
    store.check_available().unwrap();
}
