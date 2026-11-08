use crate::{Chunk, GenerationPolicy, WorldError, WorldMetadata};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use uuid::Uuid;

pub(crate) const DB_NAME: &str = "chunks.redb";
const MARKER: &str = "storage-format.json";
pub(crate) const GUARD_V2: &[u8] =
    b"ZERQON_STORAGE_V2\nThis world uses chunks.redb. Older Zerqon versions must not open it.\n";
pub(crate) const GUARD: &[u8] =
    b"ZERQON_STORAGE_V3\nThis world uses one chunks.redb domain for chunks, players and item drops. Older Zerqon versions must not open it.\n";
const CHUNKS: TableDefinition<u64, &[u8]> = TableDefinition::new("chunks");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const RECEIPTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("receipts");
const RECEIPT_ORDER: TableDefinition<u64, &[u8]> = TableDefinition::new("receipt_order");
const PLAYERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("players");
const PLAYER_CREATED: TableDefinition<&[u8], u8> = TableDefinition::new("player_created");
const DROPS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("drops");
const DROP_CHUNKS: TableDefinition<&[u8], u8> = TableDefinition::new("drop_chunks");
const MAX_CHUNK: usize = 1024 * 1024;
const MAX_READS: usize = 64;
const MAX_WRITES: usize = 16;
const RECEIPT_LIMIT: u64 = 4096;
const MAX_RECORD: usize = 2 * 1024 * 1024;
/// Includes bounded world/mode envelope headroom for a maximum-size legacy ZINV file.
pub const MAX_PLAYER_RECORD_BYTES: usize = 2 * 1024 * 1024 + 4096;
const MAX_WRITE_BYTES: usize = 16 * 1024 * 1024;
const MAX_READ_BYTES: usize = 64 * 1024 * 1024;

pub(crate) fn is_storage_guard(bytes: &[u8]) -> bool {
    bytes == GUARD || bytes == GUARD_V2
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChunkKey {
    pub x: i32,
    pub z: i32,
}
impl ChunkKey {
    pub const fn new(x: i32, z: i32) -> Self {
        Self { x, z }
    }
    fn encoded(self) -> u64 {
        ((self.x as u32 as u64) << 32) | self.z as u32 as u64
    }
}

#[derive(Debug, Clone)]
pub struct ChunkSnapshot {
    key: ChunkKey,
    chunk: Option<Arc<Chunk>>,
    revision: u64,
    world: Uuid,
}
impl ChunkSnapshot {
    pub fn key(&self) -> ChunkKey {
        self.key
    }
    pub fn chunk(&self) -> Option<&Arc<Chunk>> {
        self.chunk.as_ref()
    }
    pub(crate) fn into_chunk(self) -> Option<Chunk> {
        self.chunk.map(Arc::unwrap_or_clone)
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

pub struct ChunkWrite {
    pub key: ChunkKey,
    pub replacement: Arc<Chunk>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PlayerKey {
    pub uuid: Uuid,
}
impl PlayerKey {
    pub const fn new(uuid: Uuid) -> Self {
        Self { uuid }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DropKey {
    pub uuid: Uuid,
}
impl DropKey {
    pub const fn new(uuid: Uuid) -> Self {
        Self { uuid }
    }
}

#[derive(Debug, Clone)]
/// Opaque player bytes from one store snapshot; absence never masks a missing creation marker.
pub struct PlayerSnapshot {
    key: PlayerKey,
    data: Option<Arc<[u8]>>,
    revision: u64,
    world: Uuid,
}
impl PlayerSnapshot {
    pub fn key(&self) -> PlayerKey {
        self.key
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn data(&self) -> Option<&Arc<[u8]>> {
        self.data.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// The root validates item semantics and the position's membership in this chunk.
pub struct DropData {
    pub chunk: ChunkKey,
    pub data: Arc<[u8]>,
}

#[derive(Debug, Clone)]
/// A nonzero revision with no data is a persistent deletion tombstone.
pub struct DropSnapshot {
    key: DropKey,
    data: Option<DropData>,
    revision: u64,
    world: Uuid,
}
impl DropSnapshot {
    pub fn key(&self) -> DropKey {
        self.key
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn chunk(&self) -> Option<ChunkKey> {
        self.data.as_ref().map(|data| data.chunk)
    }
    pub fn data(&self) -> Option<&Arc<[u8]>> {
        self.data.as_ref().map(|data| &data.data)
    }
}

/// A complete, semantically validated root PlayerRecord, including its game mode.
pub struct PlayerWrite {
    pub key: PlayerKey,
    pub replacement: Arc<[u8]>,
}
/// A complete drop after-image, or a deletion that retains the previous key's revision history.
pub struct DropWrite {
    pub key: DropKey,
    pub replacement: Option<DropData>,
}

#[derive(Debug, Clone, Default)]
/// All records are read by snapshot_domain in one redb read transaction.
pub struct DomainSnapshot {
    pub chunks: Vec<ChunkSnapshot>,
    pub players: Vec<PlayerSnapshot>,
    pub drops: Vec<DropSnapshot>,
}

struct DomainReads<'a> {
    chunks: &'a [ChunkSnapshot],
    players: &'a [PlayerSnapshot],
    drops: &'a [DropSnapshot],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedChunk {
    pub key: ChunkKey,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitReceipt {
    pub operation: Uuid,
    pub revision: u64,
    pub chunks: Vec<CommittedChunk>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedPlayer {
    pub key: PlayerKey,
    pub revision: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedDrop {
    pub key: DropKey,
    pub revision: u64,
    pub deleted: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainReceipt {
    pub operation: Uuid,
    pub revision: u64,
    pub chunks: Vec<CommittedChunk>,
    #[serde(default)]
    pub players: Vec<CommittedPlayer>,
    #[serde(default)]
    pub drops: Vec<CommittedDrop>,
}
impl From<CommitReceipt> for DomainReceipt {
    fn from(receipt: CommitReceipt) -> Self {
        Self {
            operation: receipt.operation,
            revision: receipt.revision,
            chunks: receipt.chunks,
            players: Vec::new(),
            drops: Vec::new(),
        }
    }
}
impl From<DomainReceipt> for CommitReceipt {
    fn from(receipt: DomainReceipt) -> Self {
        Self {
            operation: receipt.operation,
            revision: receipt.revision,
            chunks: receipt.chunks,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("world transaction conflicts with a newer snapshot")]
    Conflict,
    #[error(transparent)]
    Rejected(#[from] WorldError),
    #[error("world commit {operation} has an uncertain outcome: {cause}")]
    Indeterminate { operation: Uuid, cause: String },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    schema: u32,
    minecraft: String,
    data_version: u32,
    world: Uuid,
    policy: GenerationPolicy,
    complete: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedReceipt {
    digest: [u8; 32],
    receipt: DomainReceipt,
}

pub(crate) struct Transactions {
    db: Database,
    world_uuid: Uuid,
    token_owner: Uuid,
    policy: GenerationPolicy,
    failed: AtomicBool,
}

fn error(value: impl std::fmt::Display) -> WorldError {
    WorldError::Storage(value.to_string())
}

impl Transactions {
    pub(crate) fn open(root: &Path, metadata: Option<&WorldMetadata>) -> Result<Self, WorldError> {
        let path = root.join(DB_NAME);
        let marker = root.join(MARKER);
        crate::metadata::reject_links(&path)?;
        crate::metadata::reject_links(&marker)?;
        let policy = metadata.map_or(GenerationPolicy::NativeFlat, |metadata| {
            metadata.generation_policy
        });
        if !path.try_exists()? {
            let guarded_v3 = root.join("IMPORT_INCOMPLETE").try_exists()?
                && crate::metadata::read_regular(&root.join("IMPORT_INCOMPLETE"), 4096)? == GUARD;
            if marker.try_exists()? || guarded_v3 {
                return Err(error(
                    "transaction database is missing; preserved chunk files are not a fallback",
                ));
            }
            Self::migrate(root, metadata, policy)?;
        }
        let file = checked_file(&path, true)?;
        if file.metadata()?.len() == 0 {
            return Err(error("empty active transaction database"));
        }
        let db = Database::builder()
            .set_cache_size(16 * 1024 * 1024)
            .create_file(file)
            .map_err(error)?;
        let read = db.begin_read().map_err(error)?;
        let meta = read.open_table(META).map_err(error)?;
        let bytes = meta
            .get("identity")
            .map_err(error)?
            .ok_or_else(|| error("missing database identity"))?;
        if bytes.value().len() > 4096 {
            return Err(error("oversized database identity"));
        }
        let mut identity: Identity = serde_json::from_slice(bytes.value()).map_err(error)?;
        if !matches!(identity.schema, 2 | 3)
            || identity.minecraft != "26.3"
            || identity.data_version != 5023
            || identity.world.is_nil()
            || !identity.complete
            || identity.policy != policy
            || metadata.is_some_and(|metadata| metadata.world_id != identity.world)
        {
            return Err(error(
                "transaction database identity differs from this world",
            ));
        }
        drop(bytes);
        drop(meta);
        drop(read);
        validate_base_tables(&db)?;
        let marked = if marker.try_exists()? {
            let marked: Identity =
                serde_json::from_slice(&crate::metadata::read_regular(&marker, 4096)?)
                    .map_err(error)?;
            if !matches!(marked.schema, 2 | 3)
                || marked.minecraft != "26.3"
                || marked.data_version != 5023
                || marked.world != identity.world
                || marked.policy != policy
                || !marked.complete
            {
                return Err(error("storage format marker mismatch"));
            }
            Some(marked)
        } else {
            None
        };
        let guard = root.join("IMPORT_INCOMPLETE");
        let guard_bytes = if guard.try_exists()? {
            let bytes = crate::metadata::read_regular(&guard, 4096)?;
            if !is_storage_guard(&bytes) {
                return Err(error("invalid old-version storage guard"));
            }
            Some(bytes)
        } else {
            None
        };
        if identity.schema == 3 && guard_bytes.as_deref() != Some(GUARD) {
            return Err(error("V3 database requires its downgrade guard"));
        }
        if identity.schema == 2 {
            if marked.as_ref().is_some_and(|marker| marker.schema != 2) {
                return Err(error("V3 marker cannot identify a V2 database"));
            }
            // Fence old binaries before any player/drop authority is published.
            replace_marker(root, "IMPORT_INCOMPLETE", GUARD)?;
            let mut upgrade = db.begin_write().map_err(error)?;
            upgrade.set_two_phase_commit(true);
            create_domain_tables(&upgrade)?;
            identity.schema = 3;
            {
                let mut meta = upgrade.open_table(META).map_err(error)?;
                meta.insert(
                    "identity",
                    serde_json::to_vec(&identity).map_err(error)?.as_slice(),
                )
                .map_err(error)?;
            }
            upgrade.commit().map_err(error)?;
        }
        validate_domain_tables(&db)?;
        if marked.as_ref().is_none_or(|marker| marker.schema != 3) {
            replace_marker(root, MARKER, &serde_json::to_vec(&identity).map_err(error)?)?;
        }
        Ok(Self {
            db,
            world_uuid: identity.world,
            token_owner: Uuid::new_v4(),
            policy,
            failed: AtomicBool::new(false),
        })
    }

    pub(crate) fn uuid(&self) -> Uuid {
        self.world_uuid
    }

    fn migrate(
        root: &Path,
        metadata: Option<&WorldMetadata>,
        policy: GenerationPolicy,
    ) -> Result<(), WorldError> {
        let stage = tempfile::Builder::new()
            .prefix("storage-migration-")
            .tempfile_in(root)?;
        let db = Database::builder()
            .set_cache_size(16 * 1024 * 1024)
            .create_file(stage.reopen()?)
            .map_err(error)?;
        let mut transaction = db.begin_write().map_err(error)?;
        transaction.set_two_phase_commit(true);
        {
            transaction.open_table(META).map_err(error)?;
            transaction.open_table(RECEIPTS).map_err(error)?;
            transaction.open_table(RECEIPT_ORDER).map_err(error)?;
            transaction.open_table(CHUNKS).map_err(error)?;
            create_domain_tables(&transaction)?;
        }
        transaction.commit().map_err(error)?;
        let mut batch = Vec::new();
        let mut total = 0usize;
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.ends_with(".zqc") {
                continue;
            }
            let key = parse_chunk_name(name)?;
            let file = checked_file(&entry.path(), false)?;
            if file.metadata()?.len() > MAX_CHUNK as u64 {
                return Err(error("legacy chunk exceeds1MiB"));
            }
            let mut bytes = Vec::new();
            file.take(MAX_CHUNK as u64 + 1).read_to_end(&mut bytes)?;
            let chunk = Chunk::from_bytes(&bytes)?;
            if chunk.x != key.x || chunk.z != key.z {
                return Err(error("legacy chunk coordinate mismatch"));
            }
            total += 1;
            if total > 16_777_216 {
                return Err(error("legacy world chunk count exceeds limit"));
            }
            batch.push((key, bytes));
            if batch.len() == 16 {
                migration_batch(&db, &batch)?;
                batch.clear();
            }
        }
        if !batch.is_empty() {
            migration_batch(&db, &batch)?;
        }
        let identity = Identity {
            schema: 3,
            minecraft: "26.3".into(),
            data_version: 5023,
            world: metadata.map_or_else(Uuid::new_v4, |metadata| metadata.world_id),
            policy,
            complete: true,
        };
        let mut transaction = db.begin_write().map_err(error)?;
        transaction.set_two_phase_commit(true);
        {
            let mut meta = transaction.open_table(META).map_err(error)?;
            let identity = serde_json::to_vec(&identity).map_err(error)?;
            meta.insert("identity", identity.as_slice())
                .map_err(error)?;
            meta.insert("revision", 1u64.to_le_bytes().as_slice())
                .map_err(error)?;
        }
        transaction.commit().map_err(error)?;
        drop(db);
        stage.as_file().sync_all()?;
        if root.join("IMPORT_INCOMPLETE").try_exists()? {
            if !is_storage_guard(&crate::metadata::read_regular(
                &root.join("IMPORT_INCOMPLETE"),
                4096,
            )?) {
                return Err(error("unexpected migration guard"));
            }
            replace_marker(root, "IMPORT_INCOMPLETE", GUARD)?;
        } else {
            write_marker(root, "IMPORT_INCOMPLETE", GUARD)?;
        }
        stage
            .persist_noclobber(root.join(DB_NAME))
            .map_err(|error| error.error)?;
        #[cfg(unix)]
        File::open(root)?.sync_all()?;
        Ok(())
    }

    pub(crate) fn healthy(&self) -> Result<(), WorldError> {
        if self.failed.load(Ordering::Acquire) {
            Err(WorldError::StorageUnavailable)
        } else {
            Ok(())
        }
    }
    fn backend_error(&self, cause: impl std::fmt::Display) -> WorldError {
        self.failed.store(true, Ordering::Release);
        error(cause)
    }

    pub(crate) fn snapshot(&self, keys: &[ChunkKey]) -> Result<Vec<ChunkSnapshot>, WorldError> {
        self.healthy()?;
        if keys.is_empty()
            || keys.len() > MAX_READS
            || keys.iter().collect::<BTreeSet<_>>().len() != keys.len()
        {
            return Err(error("snapshot requires unique bounded chunk keys"));
        }
        self.read_snapshot(keys)
            .inspect_err(|_| self.failed.store(true, Ordering::Release))
    }

    fn read_snapshot(&self, keys: &[ChunkKey]) -> Result<Vec<ChunkSnapshot>, WorldError> {
        let read = self.db.begin_read().map_err(error)?;
        let table = read.open_table(CHUNKS).map_err(error)?;
        let mut snapshots = Vec::with_capacity(keys.len());
        for key in keys {
            let value = table.get(key.encoded()).map_err(error)?;
            let (revision, chunk) = if let Some(value) = value {
                let (revision, chunk) = decode_chunk(*key, value.value())?;
                (revision, Some(Arc::new(chunk)))
            } else {
                (0, None)
            };
            snapshots.push(ChunkSnapshot {
                key: *key,
                chunk,
                revision,
                world: self.token_owner,
            });
        }
        Ok(snapshots)
    }

    pub(crate) fn receipt(&self, operation: Uuid) -> Result<Option<CommitReceipt>, WorldError> {
        self.domain_receipt(operation)
            .map(|receipt| receipt.map(Into::into))
    }

    pub(crate) fn snapshot_domain(
        &self,
        chunks: &[ChunkKey],
        players: &[PlayerKey],
        drops: &[DropKey],
    ) -> Result<DomainSnapshot, WorldError> {
        self.healthy()?;
        let count = chunks
            .len()
            .saturating_add(players.len())
            .saturating_add(drops.len());
        if count == 0 || count > MAX_READS {
            return Err(WorldError::TransactionLimit("snapshot key count"));
        }
        if chunks.iter().collect::<BTreeSet<_>>().len() != chunks.len()
            || players.iter().collect::<BTreeSet<_>>().len() != players.len()
            || drops.iter().collect::<BTreeSet<_>>().len() != drops.len()
            || players.iter().any(|key| key.uuid.is_nil())
            || drops.iter().any(|key| key.uuid.is_nil())
        {
            return Err(error("snapshot requires unique nonnil keys"));
        }
        self.read_domain(chunks, players, drops)
            .inspect_err(|cause| {
                if !matches!(cause, WorldError::TransactionLimit(_)) {
                    self.failed.store(true, Ordering::Release);
                }
            })
    }

    fn read_domain(
        &self,
        chunks: &[ChunkKey],
        players: &[PlayerKey],
        drops: &[DropKey],
    ) -> Result<DomainSnapshot, WorldError> {
        let read = self.db.begin_read().map_err(error)?;
        let chunk_table = read.open_table(CHUNKS).map_err(error)?;
        let player_table = read.open_table(PLAYERS).map_err(error)?;
        let created = read.open_table(PLAYER_CREATED).map_err(error)?;
        let drop_table = read.open_table(DROPS).map_err(error)?;
        let index = read.open_table(DROP_CHUNKS).map_err(error)?;
        let mut total = 0;
        let mut snapshots = DomainSnapshot {
            chunks: Vec::with_capacity(chunks.len()),
            players: Vec::with_capacity(players.len()),
            drops: Vec::with_capacity(drops.len()),
        };
        for key in chunks {
            let value = chunk_table.get(key.encoded()).map_err(error)?;
            let (revision, chunk) = if let Some(value) = value {
                if value.value().len() > MAX_CHUNK + 8 {
                    return Err(error("invalid stored chunk length"));
                }
                reserve_bytes(
                    &mut total,
                    value.value().len().saturating_sub(8),
                    MAX_READ_BYTES,
                )?;
                let (revision, chunk) = decode_chunk(*key, value.value())?;
                (revision, Some(Arc::new(chunk)))
            } else {
                (0, None)
            };
            snapshots.chunks.push(ChunkSnapshot {
                key: *key,
                chunk,
                revision,
                world: self.token_owner,
            });
        }
        for key in players {
            let value = player_table
                .get(key.uuid.as_bytes().as_slice())
                .map_err(error)?;
            let marker = created.get(key.uuid.as_bytes().as_slice()).map_err(error)?;
            let (revision, data) = player_record(
                value.as_ref().map(|value| value.value()),
                marker.map(|value| value.value()),
            )?;
            if let Some(data) = data {
                reserve_bytes(&mut total, data.len(), MAX_READ_BYTES)?;
            }
            snapshots.players.push(PlayerSnapshot {
                key: *key,
                data: data.map(Arc::from),
                revision,
                world: self.token_owner,
            });
        }
        for key in drops {
            let value = drop_table
                .get(key.uuid.as_bytes().as_slice())
                .map_err(error)?;
            let record = drop_record(value.as_ref().map(|value| value.value()))?;
            check_drop_index(&index, *key, &record)?;
            snapshots
                .drops
                .push(owned_drop(*key, record, self.token_owner, &mut total)?);
        }
        Ok(snapshots)
    }

    pub(crate) fn scan_drops(
        &self,
        chunk: Option<ChunkKey>,
        after: Option<DropKey>,
        limit: usize,
    ) -> Result<Vec<DropSnapshot>, WorldError> {
        self.healthy()?;
        if limit == 0 || limit > MAX_READS {
            return Err(WorldError::TransactionLimit("drop page count"));
        }
        if after.is_some_and(|key| key.uuid.is_nil()) {
            return Err(error("drop cursor UUID is nil"));
        }
        self.read_drop_page(chunk, after, limit)
            .inspect_err(|cause| {
                if !matches!(cause, WorldError::TransactionLimit(_)) {
                    self.failed.store(true, Ordering::Release);
                }
            })
    }

    fn read_drop_page(
        &self,
        chunk: Option<ChunkKey>,
        after: Option<DropKey>,
        limit: usize,
    ) -> Result<Vec<DropSnapshot>, WorldError> {
        use std::ops::Bound;
        let read = self.db.begin_read().map_err(error)?;
        let table = read.open_table(DROPS).map_err(error)?;
        let index = read.open_table(DROP_CHUNKS).map_err(error)?;
        let mut result = Vec::with_capacity(limit);
        let mut total = 0;
        if let Some(chunk) = chunk {
            let start = drop_index_key(chunk, after.unwrap_or(DropKey::new(Uuid::nil())));
            let end = drop_index_key(chunk, DropKey::new(Uuid::from_bytes([255; 16])));
            let start = if after.is_some() {
                Bound::Excluded(start.as_slice())
            } else {
                Bound::Included(start.as_slice())
            };
            for entry in index
                .range::<&[u8]>((start, Bound::Included(end.as_slice())))
                .map_err(error)?
                .take(limit)
            {
                let (key, marker) = entry.map_err(error)?;
                let key = key.value();
                if key.len() != 24
                    || key[..8] != chunk.encoded().to_be_bytes()
                    || marker.value() != 1
                {
                    return Err(error("invalid drop chunk index"));
                }
                let key = DropKey::new(uuid_key(&key[8..])?);
                let value = table
                    .get(key.uuid.as_bytes().as_slice())
                    .map_err(error)?
                    .ok_or_else(|| error("dangling drop chunk index"))?;
                let record = drop_record(Some(value.value()))?;
                if record.1.is_none_or(|(actual, _)| actual != chunk) {
                    return Err(error(
                        "drop chunk index points to another chunk or tombstone",
                    ));
                }
                result.push(owned_drop(key, record, self.token_owner, &mut total)?);
            }
        } else {
            let start = after.map_or([0; 16], |key| *key.uuid.as_bytes());
            let start = if after.is_some() {
                Bound::Excluded(start.as_slice())
            } else {
                Bound::Included(start.as_slice())
            };
            for entry in table
                .range::<&[u8]>((start, Bound::Unbounded))
                .map_err(error)?
                .take(limit)
            {
                let (key, value) = entry.map_err(error)?;
                let key = DropKey::new(uuid_key(key.value())?);
                let record = drop_record(Some(value.value()))?;
                check_drop_index(&index, key, &record)?;
                result.push(owned_drop(key, record, self.token_owner, &mut total)?);
            }
        }
        Ok(result)
    }

    pub(crate) fn domain_receipt(
        &self,
        operation: Uuid,
    ) -> Result<Option<DomainReceipt>, WorldError> {
        self.healthy()?;
        let result = self.read_domain_receipt(operation);
        result.inspect_err(|_| self.failed.store(true, Ordering::Release))
    }

    fn read_domain_receipt(&self, operation: Uuid) -> Result<Option<DomainReceipt>, WorldError> {
        let read = self.db.begin_read().map_err(error)?;
        let table = read.open_table(RECEIPTS).map_err(error)?;
        table
            .get(operation.as_bytes().as_slice())
            .map_err(error)?
            .map(|value| {
                decode_receipt(value.value()).and_then(|saved| {
                    if saved.receipt.operation != operation {
                        return Err(error("receipt operation differs from its key"));
                    }
                    Ok(saved.receipt)
                })
            })
            .transpose()
    }

    pub(crate) fn commit(
        &self,
        operation: Uuid,
        reads: &[ChunkSnapshot],
        writes: &[ChunkWrite],
    ) -> Result<CommitReceipt, CommitError> {
        self.commit_records(
            operation,
            DomainReads {
                chunks: reads,
                players: &[],
                drops: &[],
            },
            writes,
            &[],
            &[],
        )
        .map(Into::into)
    }

    pub(crate) fn commit_domain(
        &self,
        operation: Uuid,
        reads: &DomainSnapshot,
        chunks: &[ChunkWrite],
        players: &[PlayerWrite],
        drops: &[DropWrite],
    ) -> Result<DomainReceipt, CommitError> {
        self.commit_records(
            operation,
            DomainReads {
                chunks: &reads.chunks,
                players: &reads.players,
                drops: &reads.drops,
            },
            chunks,
            players,
            drops,
        )
    }

    fn commit_records(
        &self,
        operation: Uuid,
        reads: DomainReads<'_>,
        chunk_writes: &[ChunkWrite],
        player_writes: &[PlayerWrite],
        drop_writes: &[DropWrite],
    ) -> Result<DomainReceipt, CommitError> {
        self.healthy()?;
        let read_count = reads
            .chunks
            .len()
            .saturating_add(reads.players.len())
            .saturating_add(reads.drops.len());
        let write_count = chunk_writes
            .len()
            .saturating_add(player_writes.len())
            .saturating_add(drop_writes.len());
        if operation.is_nil() || read_count == 0 || write_count == 0 {
            return Err(error("transaction requires a nonnil operation and reads/writes").into());
        }
        if read_count > MAX_READS || write_count > MAX_WRITES {
            return Err(WorldError::TransactionLimit("read/write count").into());
        }
        let mut expected_chunks = BTreeMap::new();
        let mut expected_players = BTreeMap::new();
        let mut expected_drops = BTreeMap::new();
        for snapshot in reads.chunks {
            if snapshot.world != self.token_owner
                || expected_chunks
                    .insert(snapshot.key, snapshot.revision)
                    .is_some()
            {
                return Err(error("duplicate or foreign chunk snapshot").into());
            }
        }
        for snapshot in reads.players {
            if snapshot.key.uuid.is_nil()
                || snapshot.world != self.token_owner
                || expected_players
                    .insert(snapshot.key, snapshot.revision)
                    .is_some()
            {
                return Err(error("duplicate, nil or foreign player snapshot").into());
            }
        }
        for snapshot in reads.drops {
            if snapshot.key.uuid.is_nil()
                || snapshot.world != self.token_owner
                || expected_drops
                    .insert(snapshot.key, snapshot.revision)
                    .is_some()
            {
                return Err(error("duplicate, nil or foreign drop snapshot").into());
            }
        }
        let mut encoded_chunks = BTreeMap::new();
        let mut encoded_players = BTreeMap::new();
        let mut encoded_drops = BTreeMap::new();
        let mut total = 0;
        for write in chunk_writes {
            if write.key.x != write.replacement.x
                || write.key.z != write.replacement.z
                || !expected_chunks.contains_key(&write.key)
            {
                return Err(
                    error("chunk write requires matching read snapshot and coordinates").into(),
                );
            }
            let bytes = write.replacement.to_bytes();
            if bytes.len() > MAX_CHUNK {
                return Err(WorldError::TransactionLimit("chunk bytes").into());
            }
            reserve_bytes(&mut total, bytes.len(), MAX_WRITE_BYTES)?;
            Chunk::from_bytes(&bytes)?;
            if encoded_chunks.insert(write.key, bytes).is_some() {
                return Err(error("duplicate chunk write").into());
            }
        }
        for write in player_writes {
            if write.key.uuid.is_nil() || !expected_players.contains_key(&write.key) {
                return Err(error("player write requires matching nonnil read snapshot").into());
            }
            if write.replacement.is_empty() || write.replacement.len() > MAX_PLAYER_RECORD_BYTES {
                return Err(WorldError::TransactionLimit("player record bytes").into());
            }
            reserve_bytes(&mut total, write.replacement.len(), MAX_WRITE_BYTES)?;
            if encoded_players
                .insert(write.key, Arc::clone(&write.replacement))
                .is_some()
            {
                return Err(error("duplicate player write").into());
            }
        }
        for write in drop_writes {
            if write.key.uuid.is_nil() || !expected_drops.contains_key(&write.key) {
                return Err(error("drop write requires matching nonnil read snapshot").into());
            }
            if let Some(data) = &write.replacement {
                if data.data.is_empty() || data.data.len() > MAX_RECORD {
                    return Err(WorldError::TransactionLimit("drop record bytes").into());
                }
                reserve_bytes(&mut total, data.data.len(), MAX_WRITE_BYTES)?;
            }
            if encoded_drops
                .insert(write.key, write.replacement.clone())
                .is_some()
            {
                return Err(error("duplicate drop write").into());
            }
        }
        let mut digest = Sha256::new();
        digest.update(b"ZERQON_DOMAIN_V3\0");
        digest.update([0]);
        digest.update((expected_chunks.len() as u64).to_le_bytes());
        for (key, revision) in &expected_chunks {
            digest.update(key.encoded().to_le_bytes());
            digest.update(revision.to_le_bytes());
        }
        digest.update([1]);
        digest.update((expected_players.len() as u64).to_le_bytes());
        for (key, revision) in &expected_players {
            digest.update(key.uuid.as_bytes());
            digest.update(revision.to_le_bytes());
        }
        digest.update([2]);
        digest.update((expected_drops.len() as u64).to_le_bytes());
        for (key, revision) in &expected_drops {
            digest.update(key.uuid.as_bytes());
            digest.update(revision.to_le_bytes());
        }
        digest.update([3]);
        digest.update((encoded_chunks.len() as u64).to_le_bytes());
        for (key, bytes) in &encoded_chunks {
            digest.update(key.encoded().to_le_bytes());
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
        }
        digest.update([4]);
        digest.update((encoded_players.len() as u64).to_le_bytes());
        for (key, bytes) in &encoded_players {
            digest.update(key.uuid.as_bytes());
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
        }
        digest.update([5]);
        digest.update((encoded_drops.len() as u64).to_le_bytes());
        for (key, data) in &encoded_drops {
            digest.update(key.uuid.as_bytes());
            match data {
                Some(data) => {
                    digest.update([1]);
                    digest.update(data.chunk.encoded().to_le_bytes());
                    digest.update((data.data.len() as u64).to_le_bytes());
                    digest.update(&data.data);
                }
                None => digest.update([0]),
            }
        }
        let digest: [u8; 32] = digest.finalize().into();
        #[cfg(test)]
        tests::before_domain_writer();
        let mut transaction = self
            .db
            .begin_write()
            .map_err(|cause| self.backend_error(cause))?;
        // This is writer admission: a waiter must recheck failure after acquiring
        // redb's serialized writer. An already admitted commit may finish if an
        // independent reader latches failure later; records still commit atomically.
        self.healthy()?;
        transaction.set_two_phase_commit(true);
        let mut chunks = transaction
            .open_table(CHUNKS)
            .map_err(|cause| self.backend_error(cause))?;
        let mut players = transaction
            .open_table(PLAYERS)
            .map_err(|cause| self.backend_error(cause))?;
        let mut created = transaction
            .open_table(PLAYER_CREATED)
            .map_err(|cause| self.backend_error(cause))?;
        let mut drops = transaction
            .open_table(DROPS)
            .map_err(|cause| self.backend_error(cause))?;
        let mut drop_chunks = transaction
            .open_table(DROP_CHUNKS)
            .map_err(|cause| self.backend_error(cause))?;
        let mut receipts = transaction
            .open_table(RECEIPTS)
            .map_err(|cause| self.backend_error(cause))?;
        if let Some(saved) = receipts
            .get(operation.as_bytes().as_slice())
            .map_err(|cause| self.backend_error(cause))?
        {
            let saved = decode_receipt(saved.value()).map_err(|cause| self.backend_error(cause))?;
            if saved.receipt.operation != operation {
                return Err(self
                    .backend_error("receipt operation differs from its key")
                    .into());
            }
            return if saved.digest == digest {
                Ok(saved.receipt)
            } else {
                Err(error("operation ID already has another payload").into())
            };
        }
        for (key, expected) in &expected_chunks {
            let value = chunks
                .get(key.encoded())
                .map_err(|cause| self.backend_error(cause))?;
            let actual = value
                .as_ref()
                .map(|value| decode_chunk(*key, value.value()).map(|record| record.0))
                .transpose()
                .map_err(|cause| self.backend_error(cause))?
                .unwrap_or(0);
            if actual != *expected {
                return Err(CommitError::Conflict);
            }
            if actual == 0
                && self.policy == GenerationPolicy::ExistingOnly
                && encoded_chunks.contains_key(key)
            {
                return Err(WorldError::MissingImportedChunk { x: key.x, z: key.z }.into());
            }
        }
        for (key, expected) in &expected_players {
            let value = players
                .get(key.uuid.as_bytes().as_slice())
                .map_err(|cause| self.backend_error(cause))?;
            let marker = created
                .get(key.uuid.as_bytes().as_slice())
                .map_err(|cause| self.backend_error(cause))?;
            let (actual, _) = player_record(
                value.as_ref().map(|value| value.value()),
                marker.map(|value| value.value()),
            )
            .map_err(|cause| self.backend_error(cause))?;
            if actual != *expected {
                return Err(CommitError::Conflict);
            }
        }
        let mut old_drop_chunks = BTreeMap::new();
        for (key, expected) in &expected_drops {
            let value = drops
                .get(key.uuid.as_bytes().as_slice())
                .map_err(|cause| self.backend_error(cause))?;
            let record = drop_record(value.as_ref().map(|value| value.value()))
                .map_err(|cause| self.backend_error(cause))?;
            check_drop_index(&drop_chunks, *key, &record)
                .map_err(|cause| self.backend_error(cause))?;
            if record.0 != *expected {
                return Err(CommitError::Conflict);
            }
            old_drop_chunks.insert(*key, record.1.map(|(chunk, _)| chunk));
        }
        let mut meta = transaction
            .open_table(META)
            .map_err(|cause| self.backend_error(cause))?;
        let prior = meta
            .get("revision")
            .map_err(|cause| self.backend_error(cause))?
            .ok_or_else(|| self.backend_error("missing transaction revision"))?;
        let revision = u64::from_le_bytes(
            prior
                .value()
                .try_into()
                .map_err(|_| self.backend_error("invalid transaction revision"))?,
        )
        .checked_add(1)
        .ok_or_else(|| error("transaction revision overflow"))?;
        drop(prior);
        let mut receipt = DomainReceipt {
            operation,
            revision,
            chunks: Vec::with_capacity(encoded_chunks.len()),
            players: Vec::with_capacity(encoded_players.len()),
            drops: Vec::with_capacity(encoded_drops.len()),
        };
        for (key, bytes) in encoded_chunks {
            let mut value = Vec::with_capacity(bytes.len() + 8);
            value.extend_from_slice(&revision.to_le_bytes());
            value.extend_from_slice(&bytes);
            chunks
                .insert(key.encoded(), value.as_slice())
                .map_err(|cause| self.backend_error(cause))?;
            receipt.chunks.push(CommittedChunk { key, revision });
        }
        for (key, bytes) in encoded_players {
            let mut value = Vec::with_capacity(bytes.len() + 8);
            value.extend_from_slice(&revision.to_le_bytes());
            value.extend_from_slice(&bytes);
            players
                .insert(key.uuid.as_bytes().as_slice(), value.as_slice())
                .map_err(|cause| self.backend_error(cause))?;
            created
                .insert(key.uuid.as_bytes().as_slice(), 1)
                .map_err(|cause| self.backend_error(cause))?;
            receipt.players.push(CommittedPlayer { key, revision });
        }
        for (key, data) in encoded_drops {
            if let Some(chunk) = old_drop_chunks[&key] {
                let index_key = drop_index_key(chunk, key);
                if drop_chunks
                    .remove(index_key.as_slice())
                    .map_err(|cause| self.backend_error(cause))?
                    .is_none()
                {
                    return Err(self
                        .backend_error("drop index disappeared during commit")
                        .into());
                }
            }
            let mut value =
                Vec::with_capacity(data.as_ref().map_or(9, |data| data.data.len() + 17));
            value.extend_from_slice(&revision.to_le_bytes());
            if let Some(data) = &data {
                value.push(1);
                value.extend_from_slice(&data.chunk.x.to_le_bytes());
                value.extend_from_slice(&data.chunk.z.to_le_bytes());
                value.extend_from_slice(&data.data);
                let index_key = drop_index_key(data.chunk, key);
                if drop_chunks
                    .insert(index_key.as_slice(), 1)
                    .map_err(|cause| self.backend_error(cause))?
                    .is_some()
                {
                    return Err(self.backend_error("duplicate live drop chunk index").into());
                }
            } else {
                value.push(0);
            }
            drops
                .insert(key.uuid.as_bytes().as_slice(), value.as_slice())
                .map_err(|cause| self.backend_error(cause))?;
            receipt.drops.push(CommittedDrop {
                key,
                revision,
                deleted: data.is_none(),
            });
        }
        let saved = serde_json::to_vec(&SavedReceipt {
            digest,
            receipt: receipt.clone(),
        })
        .map_err(|cause| self.backend_error(cause))?;
        if saved.len() > 8192 {
            return Err(WorldError::TransactionLimit("receipt bytes").into());
        }
        receipts
            .insert(operation.as_bytes().as_slice(), saved.as_slice())
            .map_err(|cause| self.backend_error(cause))?;
        let mut order = transaction
            .open_table(RECEIPT_ORDER)
            .map_err(|cause| self.backend_error(cause))?;
        order
            .insert(revision, operation.as_bytes().as_slice())
            .map_err(|cause| self.backend_error(cause))?;
        let oldest_kept = revision.saturating_sub(RECEIPT_LIMIT);
        loop {
            let first = {
                match order.first().map_err(|cause| self.backend_error(cause))? {
                    None => None,
                    Some((key, operation)) => {
                        let operation = uuid_key(operation.value())
                            .map_err(|cause| self.backend_error(cause))?;
                        Some((key.value(), *operation.as_bytes()))
                    }
                }
            };
            let Some((key, operation)) = first else {
                break;
            };
            if key == 0 || key > revision {
                return Err(self
                    .backend_error("invalid receipt retention revision")
                    .into());
            }
            if key > oldest_kept {
                break;
            }
            {
                let saved = receipts
                    .get(operation.as_slice())
                    .map_err(|cause| self.backend_error(cause))?
                    .ok_or_else(|| self.backend_error("receipt retention entry has no receipt"))?;
                let saved =
                    decode_receipt(saved.value()).map_err(|cause| self.backend_error(cause))?;
                if saved.receipt.operation.as_bytes() != &operation || saved.receipt.revision != key
                {
                    return Err(self
                        .backend_error("receipt retention identity mismatch")
                        .into());
                }
            }
            order
                .remove(key)
                .map_err(|cause| self.backend_error(cause))?;
            receipts
                .remove(operation.as_slice())
                .map_err(|cause| self.backend_error(cause))?;
        }
        meta.insert("revision", revision.to_le_bytes().as_slice())
            .map_err(|cause| self.backend_error(cause))?;
        drop(order);
        drop(meta);
        drop(receipts);
        drop(drop_chunks);
        drop(drops);
        drop(created);
        drop(players);
        drop(chunks);
        // Stop an admitted write if failure is already visible before physical
        // commit. This is not an atomic fence with concurrent reader failures.
        self.healthy()?;
        match transaction.commit() {
            Ok(()) => Ok(receipt),
            Err(cause) => {
                self.failed.store(true, Ordering::Release);
                Err(CommitError::Indeterminate {
                    operation,
                    cause: cause.to_string(),
                })
            }
        }
    }
}

fn create_domain_tables(transaction: &redb::WriteTransaction) -> Result<(), WorldError> {
    let players = transaction.open_table(PLAYERS).map_err(error)?;
    let created = transaction.open_table(PLAYER_CREATED).map_err(error)?;
    let drops = transaction.open_table(DROPS).map_err(error)?;
    let chunks = transaction.open_table(DROP_CHUNKS).map_err(error)?;
    if !players.is_empty().map_err(error)?
        || !created.is_empty().map_err(error)?
        || !drops.is_empty().map_err(error)?
        || !chunks.is_empty().map_err(error)?
    {
        return Err(error("V2 database already contains domain records"));
    }
    Ok(())
}

fn validate_base_tables(db: &Database) -> Result<(), WorldError> {
    let read = db.begin_read().map_err(error)?;
    read.open_table(CHUNKS).map_err(error)?;
    let receipts = read.open_table(RECEIPTS).map_err(error)?;
    let order = read.open_table(RECEIPT_ORDER).map_err(error)?;
    if receipts.len().map_err(error)? != order.len().map_err(error)?
        || receipts.len().map_err(error)? > RECEIPT_LIMIT
    {
        return Err(error("receipt table and retention order disagree"));
    }
    let meta = read.open_table(META).map_err(error)?;
    let revision = meta
        .get("revision")
        .map_err(error)?
        .ok_or_else(|| error("missing transaction revision"))?;
    if u64::from_le_bytes(
        revision
            .value()
            .try_into()
            .map_err(|_| error("invalid transaction revision"))?,
    ) == 0
    {
        return Err(error("zero transaction revision"));
    }
    Ok(())
}

fn validate_domain_tables(db: &Database) -> Result<(), WorldError> {
    let read = db.begin_read().map_err(error)?;
    let players = read.open_table(PLAYERS).map_err(error)?;
    let created = read.open_table(PLAYER_CREATED).map_err(error)?;
    read.open_table(DROPS).map_err(error)?;
    read.open_table(DROP_CHUNKS).map_err(error)?;
    if players.len().map_err(error)? != created.len().map_err(error)? {
        return Err(error("player records and creation markers disagree"));
    }
    Ok(())
}

fn replace_marker(root: &Path, name: &str, bytes: &[u8]) -> Result<(), WorldError> {
    crate::metadata::reject_links(&root.join(name))?;
    let mut temporary = tempfile::NamedTempFile::new_in(root)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(root.join(name))
        .map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(root)?.sync_all()?;
    Ok(())
}

fn reserve_bytes(total: &mut usize, count: usize, limit: usize) -> Result<(), WorldError> {
    *total = total
        .checked_add(count)
        .ok_or(WorldError::TransactionLimit("aggregate bytes"))?;
    if *total > limit {
        return Err(WorldError::TransactionLimit("aggregate bytes"));
    }
    Ok(())
}

fn player_record(
    value: Option<&[u8]>,
    created: Option<u8>,
) -> Result<(u64, Option<&[u8]>), WorldError> {
    match (value, created) {
        (None, None) => Ok((0, None)),
        (Some(value), Some(1)) => {
            if !(9..=MAX_PLAYER_RECORD_BYTES + 8).contains(&value.len()) {
                return Err(error("invalid player record length"));
            }
            let revision = u64::from_le_bytes(
                value[..8]
                    .try_into()
                    .map_err(|_| error("player revision"))?,
            );
            if revision == 0 {
                return Err(error("zero player revision"));
            }
            Ok((revision, Some(&value[8..])))
        }
        _ => Err(error(
            "player record and permanent creation marker disagree",
        )),
    }
}

type BorrowedDrop<'a> = (u64, Option<(ChunkKey, &'a [u8])>);
fn drop_record(value: Option<&[u8]>) -> Result<BorrowedDrop<'_>, WorldError> {
    let Some(value) = value else {
        return Ok((0, None));
    };
    if !(9..=MAX_RECORD + 17).contains(&value.len()) {
        return Err(error("invalid drop record length"));
    }
    let revision = u64::from_le_bytes(value[..8].try_into().map_err(|_| error("drop revision"))?);
    if revision == 0 {
        return Err(error("zero drop revision"));
    }
    match value[8] {
        0 if value.len() == 9 => Ok((revision, None)),
        1 if value.len() > 17 => {
            let x = i32::from_le_bytes(value[9..13].try_into().map_err(|_| error("drop chunk x"))?);
            let z = i32::from_le_bytes(
                value[13..17]
                    .try_into()
                    .map_err(|_| error("drop chunk z"))?,
            );
            Ok((revision, Some((ChunkKey::new(x, z), &value[17..]))))
        }
        _ => Err(error("invalid drop tombstone or body")),
    }
}

fn drop_index_key(chunk: ChunkKey, key: DropKey) -> [u8; 24] {
    let mut value = [0; 24];
    value[..8].copy_from_slice(&chunk.encoded().to_be_bytes());
    value[8..].copy_from_slice(key.uuid.as_bytes());
    value
}

fn owned_drop(
    key: DropKey,
    record: BorrowedDrop<'_>,
    world: Uuid,
    total: &mut usize,
) -> Result<DropSnapshot, WorldError> {
    if let Some((_, data)) = record.1 {
        reserve_bytes(total, data.len(), MAX_READ_BYTES)?;
    }
    Ok(DropSnapshot {
        key,
        revision: record.0,
        world,
        data: record.1.map(|(chunk, data)| DropData {
            chunk,
            data: Arc::from(data),
        }),
    })
}

fn check_drop_index(
    index: &impl ReadableTable<&'static [u8], u8>,
    key: DropKey,
    record: &BorrowedDrop<'_>,
) -> Result<(), WorldError> {
    if let Some((chunk, _)) = record.1 {
        let key = drop_index_key(chunk, key);
        if index
            .get(key.as_slice())
            .map_err(error)?
            .is_none_or(|value| value.value() != 1)
        {
            return Err(error("live drop has no matching chunk index"));
        }
    }
    Ok(())
}

fn uuid_key(bytes: &[u8]) -> Result<Uuid, WorldError> {
    let uuid = Uuid::from_bytes(
        bytes
            .try_into()
            .map_err(|_| error("invalid UUID record key"))?,
    );
    if uuid.is_nil() {
        return Err(error("nil UUID record key"));
    }
    Ok(uuid)
}

fn migration_batch(db: &Database, batch: &[(ChunkKey, Vec<u8>)]) -> Result<(), WorldError> {
    let mut transaction = db.begin_write().map_err(error)?;
    transaction.set_two_phase_commit(true);
    {
        let mut table = transaction.open_table(CHUNKS).map_err(error)?;
        for (key, bytes) in batch {
            let mut value = Vec::with_capacity(bytes.len() + 8);
            value.extend_from_slice(&1u64.to_le_bytes());
            value.extend_from_slice(bytes);
            if table
                .insert(key.encoded(), value.as_slice())
                .map_err(error)?
                .is_some()
            {
                return Err(error("duplicate legacy chunk"));
            }
        }
    }
    transaction.commit().map_err(error)
}
fn decode_chunk(key: ChunkKey, value: &[u8]) -> Result<(u64, Chunk), WorldError> {
    if value.len() < 8 || value.len() > MAX_CHUNK + 8 {
        return Err(error("invalid stored chunk length"));
    }
    let revision = u64::from_le_bytes(value[..8].try_into().map_err(|_| error("chunk revision"))?);
    let chunk = Chunk::from_bytes(&value[8..])?;
    if revision == 0 || chunk.x != key.x || chunk.z != key.z {
        return Err(error("stored chunk identity mismatch"));
    }
    Ok((revision, chunk))
}
fn decode_receipt(value: &[u8]) -> Result<SavedReceipt, WorldError> {
    if value.len() > 8192 {
        return Err(error("oversized operation receipt"));
    }
    let saved: SavedReceipt = serde_json::from_slice(value).map_err(error)?;
    let receipt = &saved.receipt;
    let count = receipt
        .chunks
        .len()
        .saturating_add(receipt.players.len())
        .saturating_add(receipt.drops.len());
    if receipt.operation.is_nil()
        || receipt.revision == 0
        || count == 0
        || count > MAX_WRITES
        || receipt
            .chunks
            .iter()
            .any(|entry| entry.revision != receipt.revision)
        || receipt
            .players
            .iter()
            .any(|entry| entry.key.uuid.is_nil() || entry.revision != receipt.revision)
        || receipt
            .drops
            .iter()
            .any(|entry| entry.key.uuid.is_nil() || entry.revision != receipt.revision)
        || receipt
            .chunks
            .iter()
            .map(|entry| entry.key)
            .collect::<BTreeSet<_>>()
            .len()
            != receipt.chunks.len()
        || receipt
            .players
            .iter()
            .map(|entry| entry.key)
            .collect::<BTreeSet<_>>()
            .len()
            != receipt.players.len()
        || receipt
            .drops
            .iter()
            .map(|entry| entry.key)
            .collect::<BTreeSet<_>>()
            .len()
            != receipt.drops.len()
    {
        return Err(error("invalid operation receipt identity or revisions"));
    }
    Ok(saved)
}
fn parse_chunk_name(name: &str) -> Result<ChunkKey, WorldError> {
    let parts: Vec<_> = name.split('.').collect();
    if parts.len() != 4 || parts[0] != "c" || parts[3] != "zqc" {
        return Err(error("invalid legacy chunk filename"));
    }
    let x = parts[1].parse().map_err(error)?;
    let z = parts[2].parse().map_err(error)?;
    if name != format!("c.{x}.{z}.zqc") {
        return Err(error("noncanonical legacy chunk filename"));
    }
    Ok(ChunkKey { x, z })
}
fn write_marker(root: &Path, name: &str, bytes: &[u8]) -> Result<(), WorldError> {
    crate::metadata::reject_links(&root.join(name))?;
    let mut temporary = tempfile::NamedTempFile::new_in(root)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(root.join(name))
        .map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(root)?.sync_all()?;
    Ok(())
}
fn checked_file(path: &Path, write: bool) -> Result<File, WorldError> {
    crate::metadata::reject_links(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(error("database path must be a regular file"));
    }
    let file = OpenOptions::new().read(true).write(write).open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if file.metadata()?.nlink() != 1 {
            return Err(error("database cannot have hard-link aliases"));
        }
    }
    #[cfg(windows)]
    {
        if winapi_util::file::information(&file)?.number_of_links() != 1 {
            return Err(error("database cannot have hard-link aliases"));
        }
    }
    Ok(file)
}

#[cfg(test)]
mod tests;
