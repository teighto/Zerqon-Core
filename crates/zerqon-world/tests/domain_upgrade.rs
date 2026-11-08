use redb::{Database, ReadableDatabase, TableDefinition};
use serde_json::{Value, json};
use std::{fs, path::Path, sync::Arc};
use uuid::Uuid;
use zerqon_world::{
    Chunk, ChunkKey, ChunkWrite, DropData, DropKey, DropWrite, FlatPalette, GenerationPolicy,
    PlayerKey, PlayerWrite, WorldStore,
};

const GUARD_V2: &[u8] =
    b"ZERQON_STORAGE_V2\nThis world uses chunks.redb. Older Zerqon versions must not open it.\n";
const CHUNKS: TableDefinition<u64, &[u8]> = TableDefinition::new("chunks");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const RECEIPTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("receipts");
const RECEIPT_ORDER: TableDefinition<u64, &[u8]> = TableDefinition::new("receipt_order");
const PLAYERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("players");
const PLAYER_CREATED: TableDefinition<&[u8], u8> = TableDefinition::new("player_created");
const DROPS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("drops");
const DROP_CHUNKS: TableDefinition<&[u8], u8> = TableDefinition::new("drop_chunks");
const CHUNK_KEY: ChunkKey = ChunkKey::new(-2, 3);
const PLAYER_ID: Uuid = Uuid::from_u128(10);
const DROP_ID: Uuid = Uuid::from_u128(20);
const INVENTORY_NAME: &str = "00000000-0000-0000-0000-00000000000a.zinv";
const LEGACY_INVENTORY: &[u8] = b"preserved legacy inventory\0\xff\n";
const PALETTE: FlatPalette = FlatPalette {
    air: 0,
    bedrock: 1,
    stone: 2,
    dirt: 3,
    grass: 4,
};

struct V2Fixture {
    root: tempfile::TempDir,
    world: Uuid,
    operation: Uuid,
    identity: Value,
    legacy_chunk: Vec<u8>,
    stored_chunk: Vec<u8>,
    stored_receipt: Vec<u8>,
}

fn chunk_encoding(key: ChunkKey) -> u64 {
    ((key.x as u32 as u64) << 32) | key.z as u32 as u64
}

fn drop_index_key(chunk: ChunkKey, drop: DropKey) -> [u8; 24] {
    let mut key = [0; 24];
    key[..8].copy_from_slice(&chunk_encoding(chunk).to_be_bytes());
    key[8..].copy_from_slice(drop.uuid.as_bytes());
    key
}

fn v2_fixture() -> V2Fixture {
    let root = tempfile::tempdir().unwrap();
    let world = Uuid::new_v4();
    let operation = Uuid::new_v4();
    let identity = json!({
        "schema": 2,
        "minecraft": "26.3",
        "data_version": 5023,
        "world": world,
        "policy": GenerationPolicy::NativeFlat,
        "complete": true,
    });
    let mut chunk = Chunk::flat(CHUNK_KEY.x, CHUNK_KEY.z, PALETTE);
    chunk.set_block(0, 64, 0, 11).unwrap();
    let legacy_chunk = chunk.to_bytes();
    chunk.set_block(0, 64, 0, 99).unwrap();
    let stored_chunk = chunk.to_bytes();
    let stored_receipt = serde_json::to_vec(&json!({
        "digest": vec![0u8; 32],
        "receipt": {
            "operation": operation,
            "revision": 1,
            "chunks": [{ "key": CHUNK_KEY, "revision": 1 }],
        },
    }))
    .unwrap();
    fs::write(root.path().join("c.-2.3.zqc"), &legacy_chunk).unwrap();
    fs::create_dir(root.path().join("players")).unwrap();
    fs::write(
        root.path().join("players").join(INVENTORY_NAME),
        LEGACY_INVENTORY,
    )
    .unwrap();
    fs::write(root.path().join("IMPORT_INCOMPLETE"), GUARD_V2).unwrap();
    fs::write(
        root.path().join("storage-format.json"),
        serde_json::to_vec(&identity).unwrap(),
    )
    .unwrap();

    // Construct the old format directly: no V3 domain tables or receipt fields.
    let db = Database::create(root.path().join("chunks.redb")).unwrap();
    let transaction = db.begin_write().unwrap();
    {
        let mut chunks = transaction.open_table(CHUNKS).unwrap();
        let mut value = 1u64.to_le_bytes().to_vec();
        value.extend_from_slice(&stored_chunk);
        chunks
            .insert(chunk_encoding(CHUNK_KEY), value.as_slice())
            .unwrap();
        let mut meta = transaction.open_table(META).unwrap();
        meta.insert(
            "identity",
            serde_json::to_vec(&identity).unwrap().as_slice(),
        )
        .unwrap();
        meta.insert("revision", 1u64.to_le_bytes().as_slice())
            .unwrap();
        transaction
            .open_table(RECEIPTS)
            .unwrap()
            .insert(operation.as_bytes().as_slice(), stored_receipt.as_slice())
            .unwrap();
        transaction
            .open_table(RECEIPT_ORDER)
            .unwrap()
            .insert(1, operation.as_bytes().as_slice())
            .unwrap();
    }
    transaction.commit().unwrap();
    drop(db);
    V2Fixture {
        root,
        world,
        operation,
        identity,
        legacy_chunk,
        stored_chunk,
        stored_receipt,
    }
}

fn v3_guard() -> Vec<u8> {
    let root = tempfile::tempdir().unwrap();
    drop(WorldStore::open(root.path()).unwrap());
    fs::read(root.path().join("IMPORT_INCOMPLETE")).unwrap()
}

fn database_identity(root: &Path) -> Value {
    let db = Database::open(root.join("chunks.redb")).unwrap();
    let transaction = db.begin_read().unwrap();
    let table = transaction.open_table(META).unwrap();
    serde_json::from_slice(table.get("identity").unwrap().unwrap().value()).unwrap()
}

fn publish_v3_database_only(fixture: &V2Fixture) {
    let db = Database::open(fixture.root.path().join("chunks.redb")).unwrap();
    let transaction = db.begin_write().unwrap();
    transaction.open_table(PLAYERS).unwrap();
    transaction.open_table(PLAYER_CREATED).unwrap();
    transaction.open_table(DROPS).unwrap();
    transaction.open_table(DROP_CHUNKS).unwrap();
    let mut identity = fixture.identity.clone();
    identity["schema"] = json!(3);
    transaction
        .open_table(META)
        .unwrap()
        .insert(
            "identity",
            serde_json::to_vec(&identity).unwrap().as_slice(),
        )
        .unwrap();
    transaction.commit().unwrap();
}

#[test]
fn v2_upgrade_preserves_authoritative_chunk_sources_and_old_receipt_projection() {
    let fixture = v2_fixture();
    let expected_guard = v3_guard();
    {
        let db = Database::open(fixture.root.path().join("chunks.redb")).unwrap();
        let read = db.begin_read().unwrap();
        assert!(read.open_table(PLAYERS).is_err());
        assert!(read.open_table(DROPS).is_err());
    }
    let store = WorldStore::open(fixture.root.path()).unwrap();
    assert_eq!(store.uuid().unwrap(), fixture.world);
    assert_eq!(
        store
            .load(CHUNK_KEY.x, CHUNK_KEY.z)
            .unwrap()
            .unwrap()
            .to_bytes(),
        fixture.stored_chunk
    );
    let receipt = store.domain_receipt(fixture.operation).unwrap().unwrap();
    let old_projection = store.receipt(fixture.operation).unwrap().unwrap();
    assert_eq!(receipt.operation, fixture.operation);
    assert_eq!(receipt.revision, 1);
    assert_eq!(receipt.chunks.len(), 1);
    assert_eq!(receipt.chunks[0].key, CHUNK_KEY);
    assert_eq!(receipt.chunks[0].revision, 1);
    assert!(receipt.players.is_empty());
    assert!(receipt.drops.is_empty());
    assert_eq!(old_projection.operation, receipt.operation);
    assert_eq!(old_projection.revision, receipt.revision);
    assert_eq!(old_projection.chunks, receipt.chunks);
    let player = PlayerKey::new(PLAYER_ID);
    let reads = store.snapshot_domain(&[], &[player], &[]).unwrap();
    assert!(reads.players[0].data().is_none());
    let created = store
        .commit_domain(
            Uuid::new_v4(),
            &reads,
            &[],
            &[PlayerWrite {
                key: player,
                replacement: Arc::from([7]),
            }],
            &[],
        )
        .unwrap();
    assert_eq!(created.revision, 2);
    assert_eq!(
        store.domain_receipt(fixture.operation).unwrap(),
        Some(receipt)
    );
    drop(store);

    let marker: Value =
        serde_json::from_slice(&fs::read(fixture.root.path().join("storage-format.json")).unwrap())
            .unwrap();
    let identity = database_identity(fixture.root.path());
    assert_eq!(identity["schema"], 3);
    assert_eq!(identity["world"], fixture.identity["world"]);
    assert_eq!(marker, identity);
    assert_eq!(
        fs::read(fixture.root.path().join("IMPORT_INCOMPLETE")).unwrap(),
        expected_guard
    );
    assert_eq!(
        fs::read(fixture.root.path().join("c.-2.3.zqc")).unwrap(),
        fixture.legacy_chunk
    );
    assert_eq!(
        fs::read(fixture.root.path().join("players").join(INVENTORY_NAME)).unwrap(),
        LEGACY_INVENTORY
    );
    let db = Database::open(fixture.root.path().join("chunks.redb")).unwrap();
    let read = db.begin_read().unwrap();
    assert_eq!(
        read.open_table(RECEIPTS)
            .unwrap()
            .get(fixture.operation.as_bytes().as_slice())
            .unwrap()
            .unwrap()
            .value(),
        fixture.stored_receipt
    );
}

#[test]
fn published_v3_guard_with_v2_database_resumes_upgrade() {
    let fixture = v2_fixture();
    let guard = v3_guard();
    fs::write(fixture.root.path().join("IMPORT_INCOMPLETE"), &guard).unwrap();
    assert_eq!(database_identity(fixture.root.path())["schema"], 2);
    let store = WorldStore::open(fixture.root.path()).unwrap();
    assert_eq!(
        store
            .load(CHUNK_KEY.x, CHUNK_KEY.z)
            .unwrap()
            .unwrap()
            .to_bytes(),
        fixture.stored_chunk
    );
    assert!(store.domain_receipt(fixture.operation).unwrap().is_some());
    drop(store);
    let identity = database_identity(fixture.root.path());
    let marker: Value =
        serde_json::from_slice(&fs::read(fixture.root.path().join("storage-format.json")).unwrap())
            .unwrap();
    assert_eq!(identity["schema"], 3);
    assert_eq!(marker, identity);
    assert_eq!(
        fs::read(fixture.root.path().join("IMPORT_INCOMPLETE")).unwrap(),
        guard
    );
    assert_eq!(
        fs::read(fixture.root.path().join("c.-2.3.zqc")).unwrap(),
        fixture.legacy_chunk
    );
    assert_eq!(
        fs::read(fixture.root.path().join("players").join(INVENTORY_NAME)).unwrap(),
        LEGACY_INVENTORY
    );
}

#[test]
fn committed_v3_database_with_v2_marker_repairs_only_the_marker() {
    let fixture = v2_fixture();
    let guard = v3_guard();
    fs::write(fixture.root.path().join("IMPORT_INCOMPLETE"), &guard).unwrap();
    publish_v3_database_only(&fixture);
    assert_eq!(database_identity(fixture.root.path())["schema"], 3);
    let store = WorldStore::open(fixture.root.path()).unwrap();
    assert_eq!(store.uuid().unwrap(), fixture.world);
    assert_eq!(
        store
            .load(CHUNK_KEY.x, CHUNK_KEY.z)
            .unwrap()
            .unwrap()
            .to_bytes(),
        fixture.stored_chunk
    );
    assert_eq!(
        store
            .domain_receipt(fixture.operation)
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    drop(store);
    let marker: Value =
        serde_json::from_slice(&fs::read(fixture.root.path().join("storage-format.json")).unwrap())
            .unwrap();
    assert_eq!(marker, database_identity(fixture.root.path()));
    assert_eq!(marker["schema"], 3);
    assert_eq!(
        fs::read(fixture.root.path().join("IMPORT_INCOMPLETE")).unwrap(),
        guard
    );
    assert_eq!(
        fs::read(fixture.root.path().join("c.-2.3.zqc")).unwrap(),
        fixture.legacy_chunk
    );
    assert_eq!(
        fs::read(fixture.root.path().join("players").join(INVENTORY_NAME)).unwrap(),
        LEGACY_INVENTORY
    );
}

#[test]
fn marker_from_another_world_is_rejected_without_upgrade_or_fallback() {
    for schema in [2, 3] {
        let fixture = v2_fixture();
        if schema == 3 {
            publish_v3_database_only(&fixture);
            fs::write(fixture.root.path().join("IMPORT_INCOMPLETE"), v3_guard()).unwrap();
        }
        let mut foreign = fixture.identity.clone();
        foreign["schema"] = json!(schema);
        foreign["world"] = json!(Uuid::new_v4());
        let foreign_bytes = serde_json::to_vec(&foreign).unwrap();
        fs::write(
            fixture.root.path().join("storage-format.json"),
            &foreign_bytes,
        )
        .unwrap();
        let before_guard = fs::read(fixture.root.path().join("IMPORT_INCOMPLETE")).unwrap();
        assert!(WorldStore::open(fixture.root.path()).is_err());
        assert_eq!(database_identity(fixture.root.path())["schema"], schema);
        assert_eq!(
            database_identity(fixture.root.path())["world"],
            fixture.identity["world"]
        );
        assert_eq!(
            fs::read(fixture.root.path().join("storage-format.json")).unwrap(),
            foreign_bytes
        );
        assert_eq!(
            fs::read(fixture.root.path().join("IMPORT_INCOMPLETE")).unwrap(),
            before_guard
        );
        assert_eq!(
            fs::read(fixture.root.path().join("c.-2.3.zqc")).unwrap(),
            fixture.legacy_chunk
        );
        assert_eq!(
            fs::read(fixture.root.path().join("players").join(INVENTORY_NAME)).unwrap(),
            LEGACY_INVENTORY
        );
    }
}

#[test]
fn v3_database_requires_its_v3_guard_and_never_recreates_it_when_missing() {
    for old_guard in [false, true] {
        let fixture = v2_fixture();
        publish_v3_database_only(&fixture);
        let identity = database_identity(fixture.root.path());
        let marker_bytes = serde_json::to_vec(&identity).unwrap();
        fs::write(
            fixture.root.path().join("storage-format.json"),
            &marker_bytes,
        )
        .unwrap();
        if !old_guard {
            fs::remove_file(fixture.root.path().join("IMPORT_INCOMPLETE")).unwrap();
        }
        assert!(WorldStore::open(fixture.root.path()).is_err());
        assert_eq!(database_identity(fixture.root.path()), identity);
        assert_eq!(
            fs::read(fixture.root.path().join("storage-format.json")).unwrap(),
            marker_bytes
        );
        if old_guard {
            assert_eq!(
                fs::read(fixture.root.path().join("IMPORT_INCOMPLETE")).unwrap(),
                GUARD_V2
            );
        } else {
            assert!(!fixture.root.path().join("IMPORT_INCOMPLETE").exists());
        }
        assert_eq!(
            fs::read(fixture.root.path().join("c.-2.3.zqc")).unwrap(),
            fixture.legacy_chunk
        );
        assert_eq!(
            fs::read(fixture.root.path().join("players").join(INVENTORY_NAME)).unwrap(),
            LEGACY_INVENTORY
        );
    }
}

#[test]
fn v3_guard_without_database_or_marker_fails_closed_instead_of_importing_preserved_chunks() {
    let fixture = v2_fixture();
    let guard = v3_guard();
    fs::write(fixture.root.path().join("IMPORT_INCOMPLETE"), &guard).unwrap();
    fs::remove_file(fixture.root.path().join("chunks.redb")).unwrap();
    fs::remove_file(fixture.root.path().join("storage-format.json")).unwrap();
    for _ in 0..2 {
        assert!(WorldStore::open(fixture.root.path()).is_err());
        assert!(!fixture.root.path().join("chunks.redb").exists());
        assert!(!fixture.root.path().join("storage-format.json").exists());
        assert_eq!(
            fs::read(fixture.root.path().join("IMPORT_INCOMPLETE")).unwrap(),
            guard
        );
        assert_eq!(
            fs::read(fixture.root.path().join("c.-2.3.zqc")).unwrap(),
            fixture.legacy_chunk
        );
        assert_eq!(
            fs::read(fixture.root.path().join("players").join(INVENTORY_NAME)).unwrap(),
            LEGACY_INVENTORY
        );
    }
}

#[test]
fn v3_guard_is_rejected_by_the_g1_exact_v2_guard_check() {
    // G1's pre-database check accepted only an exact match with GUARD_V2.
    // Exercise that historical predicate without running an old server binary.
    let g1_accepts_guard = |bytes: &[u8]| bytes == GUARD_V2;
    assert!(g1_accepts_guard(GUARD_V2));
    assert!(!g1_accepts_guard(&v3_guard()));
}

#[test]
fn missing_player_row_or_creation_marker_prevents_reopen_without_using_legacy_inventory() {
    for remove_player in [false, true] {
        let fixture = v2_fixture();
        let key = PlayerKey::new(PLAYER_ID);
        let store = WorldStore::open(fixture.root.path()).unwrap();
        store
            .commit_domain(
                Uuid::new_v4(),
                &store.snapshot_domain(&[], &[key], &[]).unwrap(),
                &[],
                &[PlayerWrite {
                    key,
                    replacement: Arc::from([42]),
                }],
                &[],
            )
            .unwrap();
        drop(store);
        let db = Database::open(fixture.root.path().join("chunks.redb")).unwrap();
        let transaction = db.begin_write().unwrap();
        if remove_player {
            assert!(
                transaction
                    .open_table(PLAYERS)
                    .unwrap()
                    .remove(PLAYER_ID.as_bytes().as_slice())
                    .unwrap()
                    .is_some()
            );
        } else {
            assert!(
                transaction
                    .open_table(PLAYER_CREATED)
                    .unwrap()
                    .remove(PLAYER_ID.as_bytes().as_slice())
                    .unwrap()
                    .is_some()
            );
        }
        transaction.commit().unwrap();
        drop(db);

        assert!(WorldStore::open(fixture.root.path()).is_err());
        assert_eq!(
            fs::read(fixture.root.path().join("players").join(INVENTORY_NAME)).unwrap(),
            LEGACY_INVENTORY
        );
        assert_eq!(
            fs::read(fixture.root.path().join("c.-2.3.zqc")).unwrap(),
            fixture.legacy_chunk
        );
        let db = Database::open(fixture.root.path().join("chunks.redb")).unwrap();
        let read = db.begin_read().unwrap();
        assert_eq!(
            read.open_table(PLAYERS)
                .unwrap()
                .get(PLAYER_ID.as_bytes().as_slice())
                .unwrap()
                .is_none(),
            remove_player
        );
        assert_eq!(
            read.open_table(PLAYER_CREATED)
                .unwrap()
                .get(PLAYER_ID.as_bytes().as_slice())
                .unwrap()
                .is_some(),
            remove_player
        );
        assert_eq!(
            &read
                .open_table(CHUNKS)
                .unwrap()
                .get(chunk_encoding(CHUNK_KEY))
                .unwrap()
                .unwrap()
                .value()[8..],
            fixture.stored_chunk
        );
    }
}

#[test]
fn equal_count_mismatched_player_markers_poison_reads_and_prevent_any_following_commit() {
    let fixture = v2_fixture();
    let keys = [
        PlayerKey::new(PLAYER_ID),
        PlayerKey::new(Uuid::from_u128(11)),
    ];
    let store = WorldStore::open(fixture.root.path()).unwrap();
    store
        .commit_domain(
            Uuid::new_v4(),
            &store.snapshot_domain(&[], &keys, &[]).unwrap(),
            &[],
            &[
                PlayerWrite {
                    key: keys[0],
                    replacement: Arc::from([42]),
                },
                PlayerWrite {
                    key: keys[1],
                    replacement: Arc::from([43]),
                },
            ],
            &[],
        )
        .unwrap();
    drop(store);
    let db = Database::open(fixture.root.path().join("chunks.redb")).unwrap();
    let transaction = db.begin_write().unwrap();
    assert!(
        transaction
            .open_table(PLAYERS)
            .unwrap()
            .remove(keys[0].uuid.as_bytes().as_slice())
            .unwrap()
            .is_some()
    );
    assert!(
        transaction
            .open_table(PLAYER_CREATED)
            .unwrap()
            .remove(keys[1].uuid.as_bytes().as_slice())
            .unwrap()
            .is_some()
    );
    transaction.commit().unwrap();
    drop(db);

    for key in keys {
        let store = WorldStore::open(fixture.root.path()).unwrap();
        let healthy_reads = store.snapshot_domain(&[CHUNK_KEY], &[], &[]).unwrap();
        assert!(store.snapshot_domain(&[], &[key], &[]).is_err());
        assert!(store.check_available().is_err());
        let rejected = Uuid::new_v4();
        assert!(
            store
                .commit_domain(
                    rejected,
                    &healthy_reads,
                    &[ChunkWrite {
                        key: CHUNK_KEY,
                        replacement: Arc::new(Chunk::flat(CHUNK_KEY.x, CHUNK_KEY.z, PALETTE))
                    }],
                    &[],
                    &[],
                )
                .is_err()
        );
        drop(store);
        let db = Database::open(fixture.root.path().join("chunks.redb")).unwrap();
        let read = db.begin_read().unwrap();
        assert!(
            read.open_table(RECEIPTS)
                .unwrap()
                .get(rejected.as_bytes().as_slice())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            &read
                .open_table(CHUNKS)
                .unwrap()
                .get(chunk_encoding(CHUNK_KEY))
                .unwrap()
                .unwrap()
                .value()[8..],
            fixture.stored_chunk
        );
        assert!(
            read.open_table(PLAYERS)
                .unwrap()
                .get(keys[0].uuid.as_bytes().as_slice())
                .unwrap()
                .is_none()
        );
        assert!(
            read.open_table(PLAYER_CREATED)
                .unwrap()
                .get(keys[1].uuid.as_bytes().as_slice())
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(
        fs::read(fixture.root.path().join("players").join(INVENTORY_NAME)).unwrap(),
        LEGACY_INVENTORY
    );
    assert_eq!(
        fs::read(fixture.root.path().join("c.-2.3.zqc")).unwrap(),
        fixture.legacy_chunk
    );
}

#[test]
fn missing_live_drop_index_is_rejected_by_snapshot_and_scan_and_blocks_following_commits() {
    let fixture = v2_fixture();
    let key = DropKey::new(DROP_ID);
    let store = WorldStore::open(fixture.root.path()).unwrap();
    store
        .commit_domain(
            Uuid::new_v4(),
            &store.snapshot_domain(&[], &[], &[key]).unwrap(),
            &[],
            &[],
            &[DropWrite {
                key,
                replacement: Some(DropData {
                    chunk: CHUNK_KEY,
                    data: Arc::from([42]),
                }),
            }],
        )
        .unwrap();
    drop(store);
    let db = Database::open(fixture.root.path().join("chunks.redb")).unwrap();
    let transaction = db.begin_write().unwrap();
    assert!(
        transaction
            .open_table(DROP_CHUNKS)
            .unwrap()
            .remove(drop_index_key(CHUNK_KEY, key).as_slice())
            .unwrap()
            .is_some()
    );
    transaction.commit().unwrap();
    drop(db);

    // Reopen separately so each read API discovers the damage independently.
    for scan in [false, true] {
        let store = WorldStore::open(fixture.root.path()).unwrap();
        let healthy_reads = store.snapshot_domain(&[CHUNK_KEY], &[], &[]).unwrap();
        if scan {
            assert!(store.scan_drops(None, 16).is_err());
        } else {
            assert!(store.snapshot_domain(&[], &[], &[key]).is_err());
        }
        assert!(store.check_available().is_err());
        let rejected = Uuid::new_v4();
        assert!(
            store
                .commit_domain(
                    rejected,
                    &healthy_reads,
                    &[ChunkWrite {
                        key: CHUNK_KEY,
                        replacement: Arc::new(Chunk::flat(CHUNK_KEY.x, CHUNK_KEY.z, PALETTE))
                    }],
                    &[],
                    &[],
                )
                .is_err()
        );
        drop(store);
        let db = Database::open(fixture.root.path().join("chunks.redb")).unwrap();
        let read = db.begin_read().unwrap();
        assert!(
            read.open_table(RECEIPTS)
                .unwrap()
                .get(rejected.as_bytes().as_slice())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            &read
                .open_table(CHUNKS)
                .unwrap()
                .get(chunk_encoding(CHUNK_KEY))
                .unwrap()
                .unwrap()
                .value()[8..],
            fixture.stored_chunk
        );
        assert!(
            read.open_table(DROPS)
                .unwrap()
                .get(DROP_ID.as_bytes().as_slice())
                .unwrap()
                .is_some()
        );
        assert!(
            read.open_table(DROP_CHUNKS)
                .unwrap()
                .get(drop_index_key(CHUNK_KEY, key).as_slice())
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(
        fs::read(fixture.root.path().join("players").join(INVENTORY_NAME)).unwrap(),
        LEGACY_INVENTORY
    );
    assert_eq!(
        fs::read(fixture.root.path().join("c.-2.3.zqc")).unwrap(),
        fixture.legacy_chunk
    );
}

#[test]
fn chunk_drop_query_rejects_dangling_foreign_chunk_tombstone_and_invalid_marker_indexes() {
    for corruption in 0..4 {
        let fixture = v2_fixture();
        let key = DropKey::new(DROP_ID);
        let other_chunk = ChunkKey::new(5, -7);
        let store = WorldStore::open(fixture.root.path()).unwrap();
        store
            .commit_domain(
                Uuid::new_v4(),
                &store.snapshot_domain(&[], &[], &[key]).unwrap(),
                &[],
                &[],
                &[DropWrite {
                    key,
                    replacement: Some(DropData {
                        chunk: CHUNK_KEY,
                        data: Arc::from([42]),
                    }),
                }],
            )
            .unwrap();
        if corruption == 2 {
            store
                .commit_domain(
                    Uuid::new_v4(),
                    &store.snapshot_domain(&[], &[], &[key]).unwrap(),
                    &[],
                    &[],
                    &[DropWrite {
                        key,
                        replacement: None,
                    }],
                )
                .unwrap();
        }
        drop(store);
        let db = Database::open(fixture.root.path().join("chunks.redb")).unwrap();
        let transaction = db.begin_write().unwrap();
        match corruption {
            0 => {
                assert!(
                    transaction
                        .open_table(DROPS)
                        .unwrap()
                        .remove(DROP_ID.as_bytes().as_slice())
                        .unwrap()
                        .is_some()
                );
            }
            1 => {
                transaction
                    .open_table(DROP_CHUNKS)
                    .unwrap()
                    .insert(drop_index_key(other_chunk, key).as_slice(), 1)
                    .unwrap();
            }
            2 => {
                transaction
                    .open_table(DROP_CHUNKS)
                    .unwrap()
                    .insert(drop_index_key(CHUNK_KEY, key).as_slice(), 1)
                    .unwrap();
            }
            _ => {
                transaction
                    .open_table(DROP_CHUNKS)
                    .unwrap()
                    .insert(drop_index_key(CHUNK_KEY, key).as_slice(), 2)
                    .unwrap();
            }
        }
        transaction.commit().unwrap();
        drop(db);
        let store = WorldStore::open(fixture.root.path()).unwrap();
        let healthy_reads = store.snapshot_domain(&[CHUNK_KEY], &[], &[]).unwrap();
        let query_chunk = if corruption == 1 {
            other_chunk
        } else {
            CHUNK_KEY
        };
        assert!(store.drops_in_chunk(query_chunk, None, 16).is_err());
        assert!(store.check_available().is_err());
        let rejected = Uuid::new_v4();
        assert!(
            store
                .commit_domain(
                    rejected,
                    &healthy_reads,
                    &[ChunkWrite {
                        key: CHUNK_KEY,
                        replacement: Arc::new(Chunk::flat(CHUNK_KEY.x, CHUNK_KEY.z, PALETTE))
                    }],
                    &[],
                    &[],
                )
                .is_err()
        );
        drop(store);
        let db = Database::open(fixture.root.path().join("chunks.redb")).unwrap();
        let read = db.begin_read().unwrap();
        assert!(
            read.open_table(RECEIPTS)
                .unwrap()
                .get(rejected.as_bytes().as_slice())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            &read
                .open_table(CHUNKS)
                .unwrap()
                .get(chunk_encoding(CHUNK_KEY))
                .unwrap()
                .unwrap()
                .value()[8..],
            fixture.stored_chunk
        );
        assert_eq!(
            fs::read(fixture.root.path().join("players").join(INVENTORY_NAME)).unwrap(),
            LEGACY_INVENTORY
        );
        assert_eq!(
            fs::read(fixture.root.path().join("c.-2.3.zqc")).unwrap(),
            fixture.legacy_chunk
        );
    }
}
