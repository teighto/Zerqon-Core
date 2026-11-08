use proptest::prelude::*;
use std::{collections::BTreeMap, sync::Arc};
use uuid::Uuid;
use zerqon_world::{
    Chunk, ChunkKey, ChunkWrite, CommitError, DropData, DropKey, DropWrite, FlatPalette, PlayerKey,
    PlayerWrite, WorldStore,
};

const PALETTE: FlatPalette = FlatPalette {
    air: 0,
    bedrock: 1,
    stone: 2,
    dirt: 3,
    grass: 4,
};

fn player(id: u128) -> PlayerKey {
    PlayerKey::new(Uuid::from_u128(id))
}

fn drop_key(id: u128) -> DropKey {
    DropKey::new(Uuid::from_u128(id))
}

fn chunk_write(key: ChunkKey, value: u32) -> ChunkWrite {
    let mut chunk = Chunk::flat(key.x, key.z, PALETTE);
    chunk.set_block(0, 64, 0, value).unwrap();
    ChunkWrite {
        key,
        replacement: Arc::new(chunk),
    }
}

fn player_write(key: PlayerKey, value: u8) -> PlayerWrite {
    PlayerWrite {
        key,
        replacement: Arc::from([value]),
    }
}

fn drop_write(key: DropKey, chunk: ChunkKey, value: u8) -> DropWrite {
    DropWrite {
        key,
        replacement: Some(DropData {
            chunk,
            data: Arc::from([value]),
        }),
    }
}

#[test]
fn mixed_commit_has_one_revision_and_survives_reopen_with_its_receipt() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let chunk = ChunkKey::new(-1, 31);
    let player = player(1);
    let drop_id = drop_key(2);
    let before = store
        .snapshot_domain(&[chunk], &[player], &[drop_id])
        .unwrap();
    assert_eq!(before.chunks[0].revision(), 0);
    assert_eq!(before.players[0].revision(), 0);
    assert_eq!(before.drops[0].revision(), 0);
    assert!(before.players[0].data().is_none());
    assert!(before.drops[0].chunk().is_none());
    assert!(before.drops[0].data().is_none());

    let operation = Uuid::new_v4();
    let chunks = [chunk_write(chunk, 11)];
    let players = [player_write(player, 22)];
    let drops = [drop_write(drop_id, chunk, 33)];
    let receipt = store
        .commit_domain(operation, &before, &chunks, &players, &drops)
        .unwrap();
    assert_eq!(receipt.operation, operation);
    assert!(receipt.revision > 0);
    assert_eq!(receipt.chunks.len(), 1);
    assert_eq!(receipt.players.len(), 1);
    assert_eq!(receipt.drops.len(), 1);
    assert_eq!(receipt.chunks[0].key, chunk);
    assert_eq!(receipt.players[0].key, player);
    assert_eq!(receipt.drops[0].key, drop_id);
    assert_eq!(receipt.chunks[0].revision, receipt.revision);
    assert_eq!(receipt.players[0].revision, receipt.revision);
    assert_eq!(receipt.drops[0].revision, receipt.revision);
    assert!(!receipt.drops[0].deleted);
    assert_eq!(
        store
            .commit_domain(operation, &before, &chunks, &players, &drops)
            .unwrap(),
        receipt
    );
    assert_eq!(
        store.domain_receipt(operation).unwrap(),
        Some(receipt.clone())
    );
    assert_eq!(
        store.snapshot(&[chunk]).unwrap()[0].revision(),
        receipt.revision
    );
    drop(store);

    let reopened = WorldStore::open(root.path()).unwrap();
    let after = reopened
        .snapshot_domain(&[chunk], &[player], &[drop_id])
        .unwrap();
    assert_eq!(after.chunks[0].revision(), receipt.revision);
    assert_eq!(after.players[0].revision(), receipt.revision);
    assert_eq!(after.drops[0].revision(), receipt.revision);
    assert_eq!(after.players[0].key(), player);
    assert_eq!(after.drops[0].key(), drop_id);
    assert_eq!(
        after.chunks[0].chunk().unwrap().get_block(0, 64, 0),
        Some(11)
    );
    assert_eq!(after.players[0].data().unwrap().as_ref(), &[22]);
    assert_eq!(after.drops[0].data().unwrap().as_ref(), &[33]);
    assert_eq!(after.drops[0].chunk(), Some(chunk));
    assert_eq!(reopened.domain_receipt(operation).unwrap(), Some(receipt));
}

#[test]
fn inventory_only_commit_does_not_require_a_chunk_or_drop_write() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let key = player(1);
    let initial = store.snapshot_domain(&[], &[key], &[]).unwrap();
    let first = store
        .commit_domain(Uuid::new_v4(), &initial, &[], &[player_write(key, 12)], &[])
        .unwrap();
    let current = store.snapshot_domain(&[], &[key], &[]).unwrap();
    assert_eq!(current.players[0].data().unwrap().as_ref(), &[12]);
    let second = store
        .commit_domain(Uuid::new_v4(), &current, &[], &[player_write(key, 34)], &[])
        .unwrap();
    assert!(second.revision > first.revision);
    assert!(second.chunks.is_empty());
    assert!(second.drops.is_empty());
    let after = store.snapshot_domain(&[], &[key], &[]).unwrap();
    assert_eq!(after.players[0].revision(), second.revision);
    assert_eq!(after.players[0].data().unwrap().as_ref(), &[34]);
    assert!(store.scan_drops(None, 16).unwrap().is_empty());
}

#[test]
fn player_records_allow_legacy_envelope_headroom_and_enforce_the_public_bound() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let key = player(1);
    let limit = zerqon_world::MAX_PLAYER_RECORD_BYTES;
    let record: Arc<[u8]> = vec![7; 2 * 1024 * 1024 + 128].into();
    let reads = store.snapshot_domain(&[], &[key], &[]).unwrap();
    store
        .commit_domain(
            Uuid::new_v4(),
            &reads,
            &[],
            &[PlayerWrite {
                key,
                replacement: Arc::clone(&record),
            }],
            &[],
        )
        .unwrap();
    let reads = store.snapshot_domain(&[], &[key], &[]).unwrap();
    assert_eq!(reads.players[0].data().unwrap().as_ref(), record.as_ref());
    assert!(
        store
            .commit_domain(
                Uuid::new_v4(),
                &reads,
                &[],
                &[PlayerWrite {
                    key,
                    replacement: vec![8; limit + 1].into()
                }],
                &[]
            )
            .is_err()
    );
    store
        .commit_domain(
            Uuid::new_v4(),
            &reads,
            &[],
            &[PlayerWrite {
                key,
                replacement: vec![9; limit].into(),
            }],
            &[],
        )
        .unwrap();
    assert_eq!(
        store.snapshot_domain(&[], &[key], &[]).unwrap().players[0]
            .data()
            .unwrap()
            .len(),
        limit
    );
}

#[test]
fn changed_read_only_dependency_in_any_domain_rejects_every_write() {
    for changed_domain in 0..3 {
        let root = tempfile::tempdir().unwrap();
        let store = WorldStore::open(root.path()).unwrap();
        let chunks = [ChunkKey::new(0, 0), ChunkKey::new(1, 0)];
        let players = [player(1), player(2)];
        let drops = [drop_key(1), drop_key(2)];
        let stale = store.snapshot_domain(&chunks, &players, &drops).unwrap();
        let (chunk_writes, player_writes, drop_writes) = match changed_domain {
            0 => (vec![chunk_write(chunks[1], 99)], vec![], vec![]),
            1 => (vec![], vec![player_write(players[1], 99)], vec![]),
            _ => (vec![], vec![], vec![drop_write(drops[1], chunks[1], 99)]),
        };
        store
            .commit_domain(
                Uuid::new_v4(),
                &stale,
                &chunk_writes,
                &player_writes,
                &drop_writes,
            )
            .unwrap();
        let rejected_operation = Uuid::new_v4();
        assert!(matches!(
            store.commit_domain(
                rejected_operation,
                &stale,
                &[chunk_write(chunks[0], 11)],
                &[player_write(players[0], 22)],
                &[drop_write(drops[0], chunks[0], 33)],
            ),
            Err(CommitError::Conflict)
        ));
        let unchanged = store
            .snapshot_domain(&chunks[..1], &players[..1], &drops[..1])
            .unwrap();
        assert!(unchanged.chunks[0].chunk().is_none());
        assert!(unchanged.players[0].data().is_none());
        assert!(unchanged.drops[0].data().is_none());
        assert_eq!(unchanged.chunks[0].revision(), 0);
        assert_eq!(unchanged.players[0].revision(), 0);
        assert_eq!(unchanged.drops[0].revision(), 0);
        assert!(store.domain_receipt(rejected_operation).unwrap().is_none());
    }
}

#[test]
fn reused_operation_rejects_changed_payload_in_each_domain_and_changed_drop_location() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let chunk = ChunkKey::new(0, 0);
    let player = player(1);
    let key = drop_key(2);
    let reads = store.snapshot_domain(&[chunk], &[player], &[key]).unwrap();
    let operation = Uuid::new_v4();
    let receipt = store
        .commit_domain(
            operation,
            &reads,
            &[chunk_write(chunk, 11)],
            &[player_write(player, 22)],
            &[drop_write(key, chunk, 33)],
        )
        .unwrap();
    for changed in 0..5 {
        let chunks = [chunk_write(chunk, if changed == 0 { 99 } else { 11 })];
        let players = [player_write(player, if changed == 1 { 99 } else { 22 })];
        let drops = [if changed == 4 {
            DropWrite {
                key,
                replacement: None,
            }
        } else {
            drop_write(
                key,
                if changed == 3 {
                    ChunkKey::new(5, -6)
                } else {
                    chunk
                },
                if changed == 2 { 99 } else { 33 },
            )
        }];
        assert!(
            store
                .commit_domain(operation, &reads, &chunks, &players, &drops)
                .is_err()
        );
        assert_eq!(
            store.domain_receipt(operation).unwrap(),
            Some(receipt.clone())
        );
    }
    let after = store.snapshot_domain(&[chunk], &[player], &[key]).unwrap();
    assert_eq!(
        after.chunks[0].chunk().unwrap().get_block(0, 64, 0),
        Some(11)
    );
    assert_eq!(after.players[0].data().unwrap().as_ref(), &[22]);
    assert_eq!(after.drops[0].data().unwrap().as_ref(), &[33]);
    assert_eq!(after.drops[0].chunk(), Some(chunk));
    assert_eq!(after.drops[0].revision(), receipt.revision);
}

#[test]
fn drop_moves_and_tombstones_preserve_index_order_and_exclusive_pagination_after_reopen() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let a = ChunkKey::new(-4, 7);
    let b = ChunkKey::new(8, -9);
    let keys = [drop_key(30), drop_key(10), drop_key(40), drop_key(20)];
    let writes: Vec<_> = keys.iter().map(|key| drop_write(*key, a, 7)).collect();
    store
        .commit_domain(
            Uuid::new_v4(),
            &store.snapshot_domain(&[], &[], &keys).unwrap(),
            &[],
            &[],
            &writes,
        )
        .unwrap();
    let operation = Uuid::new_v4();
    let receipt = store
        .commit_domain(
            operation,
            &store.snapshot_domain(&[], &[], &keys).unwrap(),
            &[],
            &[],
            &[
                drop_write(drop_key(20), b, 8),
                DropWrite {
                    key: drop_key(30),
                    replacement: None,
                },
            ],
        )
        .unwrap();
    assert!(
        receipt
            .drops
            .iter()
            .find(|entry| entry.key == drop_key(30))
            .unwrap()
            .deleted
    );
    drop(store);

    let store = WorldStore::open(root.path()).unwrap();
    let first = store.scan_drops(None, 2).unwrap();
    let second = store.scan_drops(Some(first[1].key()), 2).unwrap();
    assert_eq!(
        first.iter().map(|entry| entry.key()).collect::<Vec<_>>(),
        vec![drop_key(10), drop_key(20)]
    );
    assert_eq!(
        second.iter().map(|entry| entry.key()).collect::<Vec<_>>(),
        vec![drop_key(30), drop_key(40)]
    );
    assert!(store.scan_drops(Some(drop_key(40)), 2).unwrap().is_empty());
    assert_eq!(
        store.scan_drops(Some(drop_key(25)), 1).unwrap()[0].key(),
        drop_key(30)
    );
    assert!(second[0].data().is_none());
    assert!(second[0].chunk().is_none());
    assert_eq!(second[0].revision(), receipt.revision);
    assert_eq!(first[1].chunk(), Some(b));
    assert_eq!(first[1].data().unwrap().as_ref(), &[8]);
    assert_eq!(first[1].revision(), receipt.revision);

    let in_a = store.drops_in_chunk(a, None, 16).unwrap();
    assert_eq!(
        in_a.iter().map(|entry| entry.key()).collect::<Vec<_>>(),
        vec![drop_key(10), drop_key(40)]
    );
    assert!(
        in_a.iter()
            .all(|entry| entry.chunk() == Some(a) && entry.data().is_some())
    );
    assert_eq!(
        store.drops_in_chunk(a, Some(drop_key(20)), 1).unwrap()[0].key(),
        drop_key(40)
    );
    assert!(
        store
            .drops_in_chunk(a, Some(drop_key(40)), 1)
            .unwrap()
            .is_empty()
    );
    let in_b = store.drops_in_chunk(b, None, 16).unwrap();
    assert_eq!(in_b.len(), 1);
    assert_eq!(in_b[0].key(), drop_key(20));
    assert_eq!(store.domain_receipt(operation).unwrap(), Some(receipt));
}

#[test]
fn drop_pagination_uses_full_uuid_order_across_byte_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let chunk = ChunkKey::new(i32::MIN, i32::MAX);
    let ids = [256, 1, u128::MAX, 255, 1 << 64, 1 << 120];
    let keys: Vec<_> = ids.into_iter().map(drop_key).collect();
    let writes: Vec<_> = keys.iter().map(|key| drop_write(*key, chunk, 1)).collect();
    store
        .commit_domain(
            Uuid::new_v4(),
            &store.snapshot_domain(&[], &[], &keys).unwrap(),
            &[],
            &[],
            &writes,
        )
        .unwrap();
    let mut cursor = None;
    let mut scanned = Vec::new();
    let mut indexed = Vec::new();
    for _ in 0..3 {
        let page = store.scan_drops(cursor, 2).unwrap();
        let in_chunk = store.drops_in_chunk(chunk, cursor, 2).unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(in_chunk.len(), 2);
        scanned.extend(page.iter().map(|entry| entry.key().uuid.as_u128()));
        indexed.extend(in_chunk.iter().map(|entry| entry.key().uuid.as_u128()));
        cursor = Some(page[1].key());
    }
    assert_eq!(scanned, vec![1, 255, 256, 1 << 64, 1 << 120, u128::MAX]);
    assert_eq!(indexed, vec![1, 255, 256, 1 << 64, 1 << 120, u128::MAX]);
    assert!(store.scan_drops(cursor, 2).unwrap().is_empty());
    assert!(store.drops_in_chunk(chunk, cursor, 2).unwrap().is_empty());
}

#[test]
fn tombstone_revision_prevents_stale_resurrection_and_allows_fresh_recreation() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let key = drop_key(1);
    let a = ChunkKey::new(0, 0);
    let b = ChunkKey::new(1, 0);
    let missing = store.snapshot_domain(&[], &[], &[key]).unwrap();
    store
        .commit_domain(Uuid::new_v4(), &missing, &[], &[], &[drop_write(key, a, 1)])
        .unwrap();
    let live = store.snapshot_domain(&[], &[], &[key]).unwrap();
    let deleted = store
        .commit_domain(
            Uuid::new_v4(),
            &live,
            &[],
            &[],
            &[DropWrite {
                key,
                replacement: None,
            }],
        )
        .unwrap();
    for stale in [&missing, &live] {
        assert!(matches!(
            store.commit_domain(Uuid::new_v4(), stale, &[], &[], &[drop_write(key, b, 2)]),
            Err(CommitError::Conflict)
        ));
    }
    assert!(store.drops_in_chunk(a, None, 16).unwrap().is_empty());
    assert!(store.drops_in_chunk(b, None, 16).unwrap().is_empty());
    let tombstone = store.snapshot_domain(&[], &[], &[key]).unwrap();
    assert_eq!(tombstone.drops[0].revision(), deleted.revision);
    assert!(tombstone.drops[0].data().is_none());
    let recreated = store
        .commit_domain(
            Uuid::new_v4(),
            &tombstone,
            &[],
            &[],
            &[drop_write(key, b, 3)],
        )
        .unwrap();
    assert!(recreated.revision > deleted.revision);
    assert!(store.drops_in_chunk(a, None, 16).unwrap().is_empty());
    let actual = store.drops_in_chunk(b, None, 16).unwrap();
    assert_eq!(actual.len(), 1);
    assert_eq!(actual[0].data().unwrap().as_ref(), &[3]);
    assert_eq!(store.scan_drops(None, 16).unwrap().len(), 1);
}

#[test]
fn invalid_ids_duplicates_missing_reads_and_foreign_tokens_never_write() {
    let root = tempfile::tempdir().unwrap();
    let foreign_root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let foreign = WorldStore::open(foreign_root.path()).unwrap();
    let chunk = ChunkKey::new(0, 0);
    let p = player(1);
    let d = drop_key(1);
    assert!(store.snapshot_domain(&[], &[player(0)], &[]).is_err());
    assert!(store.snapshot_domain(&[], &[], &[drop_key(0)]).is_err());
    assert!(store.snapshot_domain(&[chunk, chunk], &[], &[]).is_err());
    assert!(store.snapshot_domain(&[], &[p, p], &[]).is_err());
    assert!(store.snapshot_domain(&[], &[], &[d, d]).is_err());
    let valid = store.snapshot_domain(&[chunk], &[p], &[d]).unwrap();
    assert!(
        store
            .commit_domain(Uuid::nil(), &valid, &[], &[player_write(p, 1)], &[])
            .is_err()
    );
    for invalid in 0..11 {
        let mut reads = store.snapshot_domain(&[chunk], &[p], &[d]).unwrap();
        let mut chunks = vec![chunk_write(chunk, 1)];
        let mut players = vec![player_write(p, 2)];
        let mut drops = vec![drop_write(d, chunk, 3)];
        match invalid {
            0 => players.push(player_write(p, 4)),
            1 => drops.push(drop_write(d, chunk, 4)),
            2 => players[0] = player_write(player(2), 4),
            3 => drops[0] = drop_write(drop_key(2), chunk, 4),
            4 => players[0] = player_write(player(0), 4),
            5 => drops[0] = drop_write(drop_key(0), chunk, 4),
            6 => reads.players = foreign.snapshot_domain(&[], &[p], &[]).unwrap().players,
            7 => reads.drops = foreign.snapshot_domain(&[], &[], &[d]).unwrap().drops,
            8 => reads.players.push(reads.players[0].clone()),
            9 => reads.drops.push(reads.drops[0].clone()),
            _ => chunks.push(chunk_write(chunk, 4)),
        }
        let operation = Uuid::new_v4();
        assert!(
            store
                .commit_domain(operation, &reads, &chunks, &players, &drops)
                .is_err()
        );
        assert!(store.domain_receipt(operation).unwrap().is_none());
        let after = store.snapshot_domain(&[chunk], &[p], &[d]).unwrap();
        assert!(after.chunks[0].chunk().is_none());
        assert!(after.players[0].data().is_none());
        assert!(after.drops[0].data().is_none());
        assert!(store.scan_drops(None, 16).unwrap().is_empty());
    }
    store
        .commit_domain(Uuid::new_v4(), &valid, &[], &[player_write(p, 9)], &[])
        .unwrap();
}

#[test]
fn read_and_write_count_limits_apply_to_the_sum_of_all_domains() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let chunks: Vec<_> = (0..22).map(|x| ChunkKey::new(x, -x)).collect();
    let players: Vec<_> = (1..=22).map(player).collect();
    let drops: Vec<_> = (1..=22).map(drop_key).collect();
    assert!(
        store
            .snapshot_domain(&chunks[..21], &players, &drops[..21])
            .is_ok()
    );
    assert!(
        store
            .snapshot_domain(&chunks, &players, &drops[..21])
            .is_err()
    );
    let reads = store
        .snapshot_domain(&chunks[..6], &players[..6], &drops[..5])
        .unwrap();
    let chunk_writes: Vec<_> = chunks[..6].iter().map(|key| chunk_write(*key, 1)).collect();
    let player_writes: Vec<_> = players[..6]
        .iter()
        .map(|key| player_write(*key, 2))
        .collect();
    let drop_writes: Vec<_> = drops[..5]
        .iter()
        .map(|key| drop_write(*key, chunks[0], 3))
        .collect();
    let rejected = Uuid::new_v4();
    assert!(
        store
            .commit_domain(
                rejected,
                &reads,
                &chunk_writes,
                &player_writes,
                &drop_writes
            )
            .is_err()
    );
    assert!(store.domain_receipt(rejected).unwrap().is_none());
    let unchanged = store
        .snapshot_domain(&chunks[..6], &players[..6], &drops[..5])
        .unwrap();
    assert!(unchanged.chunks.iter().all(|entry| entry.chunk().is_none()));
    assert!(unchanged.players.iter().all(|entry| entry.data().is_none()));
    assert!(unchanged.drops.iter().all(|entry| entry.data().is_none()));

    let accepted = store
        .commit_domain(
            Uuid::new_v4(),
            &reads,
            &chunk_writes,
            &player_writes[..5],
            &drop_writes,
        )
        .unwrap();
    assert_eq!(
        accepted.chunks.len() + accepted.players.len() + accepted.drops.len(),
        16
    );
    assert!(
        store
            .snapshot_domain(&[], &players[5..6], &[])
            .unwrap()
            .players[0]
            .data()
            .is_none()
    );
}

#[test]
fn empty_and_oversized_opaque_values_reject_the_entire_mixed_commit() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let chunk = ChunkKey::new(0, 0);
    let p = player(1);
    let d = drop_key(1);
    let reads = store.snapshot_domain(&[chunk], &[p], &[d]).unwrap();
    for invalid_player in [false, true] {
        let limit = if invalid_player {
            zerqon_world::MAX_PLAYER_RECORD_BYTES
        } else {
            2 * 1024 * 1024
        };
        for length in [0, limit + 1] {
            let value: Arc<[u8]> = vec![7; length].into();
            let players = [if invalid_player {
                PlayerWrite {
                    key: p,
                    replacement: Arc::clone(&value),
                }
            } else {
                player_write(p, 2)
            }];
            let drops = [if invalid_player {
                drop_write(d, chunk, 3)
            } else {
                DropWrite {
                    key: d,
                    replacement: Some(DropData { chunk, data: value }),
                }
            }];
            let operation = Uuid::new_v4();
            assert!(
                store
                    .commit_domain(
                        operation,
                        &reads,
                        &[chunk_write(chunk, 1)],
                        &players,
                        &drops
                    )
                    .is_err()
            );
            assert!(store.domain_receipt(operation).unwrap().is_none());
            let after = store.snapshot_domain(&[chunk], &[p], &[d]).unwrap();
            assert!(after.chunks[0].chunk().is_none());
            assert!(after.players[0].data().is_none());
            assert!(after.drops[0].data().is_none());
        }
    }
    let boundary: Arc<[u8]> = vec![9; 2 * 1024 * 1024].into();
    store
        .commit_domain(
            Uuid::new_v4(),
            &reads,
            &[],
            &[PlayerWrite {
                key: p,
                replacement: Arc::clone(&boundary),
            }],
            &[DropWrite {
                key: d,
                replacement: Some(DropData {
                    chunk,
                    data: boundary,
                }),
            }],
        )
        .unwrap();
    let after = store.snapshot_domain(&[], &[p], &[d]).unwrap();
    assert_eq!(after.players[0].data().unwrap().len(), 2 * 1024 * 1024);
    assert_eq!(after.drops[0].data().unwrap().len(), 2 * 1024 * 1024);
}

#[test]
fn aggregate_write_limit_counts_player_and_drop_payloads_together() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let chunk = ChunkKey::new(0, 0);
    let players: Vec<_> = (1..=5).map(player).collect();
    let drops: Vec<_> = (1..=4).map(drop_key).collect();
    let reads = store.snapshot_domain(&[], &players, &drops).unwrap();
    let payload: Arc<[u8]> = vec![1; 2 * 1024 * 1024].into();
    let player_writes: Vec<_> = players
        .iter()
        .map(|key| PlayerWrite {
            key: *key,
            replacement: Arc::clone(&payload),
        })
        .collect();
    let drop_writes: Vec<_> = drops
        .iter()
        .map(|key| DropWrite {
            key: *key,
            replacement: Some(DropData {
                chunk,
                data: Arc::clone(&payload),
            }),
        })
        .collect();
    let operation = Uuid::new_v4();
    assert!(
        store
            .commit_domain(operation, &reads, &[], &player_writes, &drop_writes)
            .is_err()
    );
    assert!(store.domain_receipt(operation).unwrap().is_none());
    let after = store.snapshot_domain(&[], &players, &drops).unwrap();
    assert!(after.players.iter().all(|entry| entry.data().is_none()));
    assert!(after.drops.iter().all(|entry| entry.data().is_none()));
    assert!(store.drops_in_chunk(chunk, None, 16).unwrap().is_empty());
}

#[test]
fn aggregate_snapshot_limit_counts_player_and_drop_payloads_together() {
    let root = tempfile::tempdir().unwrap();
    let store = WorldStore::open(root.path()).unwrap();
    let players: Vec<_> = (1..=17).map(player).collect();
    let drops: Vec<_> = (1..=16).map(drop_key).collect();
    // One byte above 64 MiB; each write batch remains below 16 MiB.
    let payload: Arc<[u8]> = vec![1; 2 * 1024 * 1024].into();
    for index in 0..8 {
        let player_keys = &players[index * 2..index * 2 + 2];
        let drop_keys = &drops[index * 2..index * 2 + 2];
        let reads = store.snapshot_domain(&[], player_keys, drop_keys).unwrap();
        let player_writes: Vec<_> = player_keys
            .iter()
            .map(|key| PlayerWrite {
                key: *key,
                replacement: Arc::clone(&payload),
            })
            .collect();
        let drop_writes: Vec<_> = drop_keys
            .iter()
            .map(|key| DropWrite {
                key: *key,
                replacement: Some(DropData {
                    chunk: ChunkKey::new(index as i32, 0),
                    data: Arc::clone(&payload),
                }),
            })
            .collect();
        store
            .commit_domain(Uuid::new_v4(), &reads, &[], &player_writes, &drop_writes)
            .unwrap();
    }
    let reads = store.snapshot_domain(&[], &players[16..], &[]).unwrap();
    store
        .commit_domain(
            Uuid::new_v4(),
            &reads,
            &[],
            &[player_write(players[16], 2)],
            &[],
        )
        .unwrap();
    assert!(store.snapshot_domain(&[], &players[..15], &drops).is_ok());
    assert!(store.snapshot_domain(&[], &players, &drops).is_err());
    // Rejection is local to this oversized request, not a poison of the store.
    assert_eq!(
        store
            .snapshot_domain(&[], &players[16..], &[])
            .unwrap()
            .players[0]
            .data()
            .unwrap()
            .as_ref(),
        &[2]
    );
}

#[test]
fn concurrent_domain_and_chunk_drop_reads_never_observe_partial_commits() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(WorldStore::open(root.path()).unwrap());
    let chunk = ChunkKey::new(0, 0);
    let other = ChunkKey::new(-1, 2);
    let p = player(1);
    let drops = [drop_key(1), drop_key(2)];
    store
        .commit_domain(
            Uuid::new_v4(),
            &store.snapshot_domain(&[chunk], &[p], &drops).unwrap(),
            &[chunk_write(chunk, 0)],
            &[player_write(p, 0)],
            &[
                drop_write(drops[0], chunk, 0),
                drop_write(drops[1], chunk, 0),
            ],
        )
        .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let mut readers = Vec::new();
    for _ in 0..2 {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        readers.push(std::thread::spawn(move || {
            barrier.wait();
            for _ in 0..150 {
                let actual = store.snapshot_domain(&[chunk], &[p], &drops).unwrap();
                let value = actual.players[0].data().unwrap()[0];
                assert_eq!(
                    actual.chunks[0].chunk().unwrap().get_block(0, 64, 0),
                    Some(u32::from(value))
                );
                assert_eq!(actual.drops[0].data().unwrap()[0], value);
                assert_eq!(actual.drops[1].data().unwrap()[0], value);
                assert_eq!(actual.chunks[0].revision(), actual.players[0].revision());
                assert_eq!(actual.chunks[0].revision(), actual.drops[0].revision());
                assert_eq!(actual.chunks[0].revision(), actual.drops[1].revision());
                let indexed = store.drops_in_chunk(chunk, None, 16).unwrap();
                assert!(indexed.is_empty() || indexed.len() == 2);
                if indexed.len() == 2 {
                    assert_eq!(indexed[0].chunk(), Some(chunk));
                    assert_eq!(indexed[1].chunk(), Some(chunk));
                    assert_eq!(indexed[0].revision(), indexed[1].revision());
                    assert_eq!(indexed[0].data(), indexed[1].data());
                }
            }
        }));
    }
    barrier.wait();
    for value in 1..=30 {
        let destination = if value % 2 == 0 { chunk } else { other };
        store
            .commit_domain(
                Uuid::new_v4(),
                &store.snapshot_domain(&[chunk], &[p], &drops).unwrap(),
                &[chunk_write(chunk, u32::from(value))],
                &[player_write(p, value)],
                &[
                    drop_write(drops[0], destination, value),
                    drop_write(drops[1], destination, value),
                ],
            )
            .unwrap();
    }
    for reader in readers {
        reader.join().unwrap();
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 12,
        failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct(
            concat!(env!("CARGO_MANIFEST_DIR"), "/proptest-regressions/domain-transactions.txt"),
        ))),
        ..ProptestConfig::default()
    })]

    #[test]
    fn domain_action_sequences_match_an_independent_map_model(
        actions in prop::collection::vec((0u8..4, 0u8..4, 0u8..3, any::<u8>()), 1..17),
    ) {
        let root = tempfile::tempdir().unwrap();
        let store = WorldStore::open(root.path()).unwrap();
        let mut players = BTreeMap::<u8, (u64, u8)>::new();
        let mut drops = BTreeMap::<u8, (u64, Option<(u8, u8)>)>::new();
        for (action, id, destination, value) in actions {
            let p = player(u128::from(id) + 1);
            let d = drop_key(u128::from(id) + 1);
            let chunk = ChunkKey::new(i32::from(destination), -i32::from(destination));
            let reads = store.snapshot_domain(&[], &[p], &[d]).unwrap();
            let player_writes = if action == 0 || action == 3 { vec![player_write(p, value)] } else { vec![] };
            let drop_writes = match action {
                0 => vec![],
                2 => vec![DropWrite { key: d, replacement: None }],
                _ => vec![drop_write(d, chunk, value)],
            };
            let receipt = store.commit_domain(Uuid::new_v4(), &reads, &[], &player_writes, &drop_writes).unwrap();
            if action == 0 || action == 3 {
                players.insert(id, (receipt.revision, value));
            }
            if action != 0 {
                drops.insert(id, (receipt.revision, if action == 2 { None } else { Some((destination, value)) }));
            }

            let player_keys: Vec<_> = (1..=4).map(player).collect();
            let drop_keys: Vec<_> = (1..=4).map(drop_key).collect();
            let actual = store.snapshot_domain(&[], &player_keys, &drop_keys).unwrap();
            for id in 0..4u8 {
                let observed = &actual.players[usize::from(id)];
                prop_assert_eq!(observed.revision(), players.get(&id).map_or(0, |entry| entry.0));
                prop_assert_eq!(observed.data().map(|bytes| bytes[0]), players.get(&id).map(|entry| entry.1));
                let observed = &actual.drops[usize::from(id)];
                prop_assert_eq!(observed.revision(), drops.get(&id).map_or(0, |entry| entry.0));
                let expected = drops.get(&id).and_then(|entry| entry.1);
                prop_assert_eq!(observed.data().map(|bytes| bytes[0]), expected.map(|entry| entry.1));
                prop_assert_eq!(observed.chunk().map(|key| (key.x, key.z)), expected.map(|entry| (i32::from(entry.0), -i32::from(entry.0))));
            }
            let actual_keys: Vec<_> = store.scan_drops(None, 16).unwrap().iter().map(|entry| entry.key().uuid.as_u128()).collect();
            let expected_keys: Vec<_> = drops.keys().map(|id| u128::from(*id) + 1).collect();
            prop_assert_eq!(actual_keys, expected_keys);
            for location in 0..3u8 {
                let indexed = store.drops_in_chunk(ChunkKey::new(i32::from(location), -i32::from(location)), None, 16).unwrap();
                let actual_ids: Vec<_> = indexed.iter().map(|entry| entry.key().uuid.as_u128()).collect();
                let expected_ids: Vec<_> = drops.iter().filter(|(_, entry)| entry.1.is_some_and(|drop| drop.0 == location)).map(|(id, _)| u128::from(*id) + 1).collect();
                prop_assert_eq!(actual_ids, expected_ids);
            }
        }
        drop(store);
        let reopened = WorldStore::open(root.path()).unwrap();
        for (id, (revision, value)) in players {
            let actual = reopened.snapshot_domain(&[], &[player(u128::from(id) + 1)], &[]).unwrap();
            prop_assert_eq!(actual.players[0].revision(), revision);
            prop_assert_eq!(actual.players[0].data().unwrap()[0], value);
        }
        for (id, (revision, value)) in drops {
            let actual = reopened.snapshot_domain(&[], &[], &[drop_key(u128::from(id) + 1)]).unwrap();
            prop_assert_eq!(actual.drops[0].revision(), revision);
            prop_assert_eq!(actual.drops[0].data().map(|bytes| bytes[0]), value.map(|entry| entry.1));
            prop_assert_eq!(actual.drops[0].chunk().map(|key| (key.x, key.z)), value.map(|entry| (i32::from(entry.0), -i32::from(entry.0))));
        }
    }
}
