use crate::{Chunk, GenerationPolicy, WorldError, WorldMetadata};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
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
pub(crate) const GUARD: &[u8] =
    b"ZERQON_STORAGE_V2\nThis world uses chunks.redb. Older Zerqon versions must not open it.\n";
const CHUNKS: TableDefinition<u64, &[u8]> = TableDefinition::new("chunks");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const RECEIPTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("receipts");
const RECEIPT_ORDER: TableDefinition<u64, &[u8]> = TableDefinition::new("receipt_order");
const MAX_CHUNK: usize = 1024 * 1024;
const MAX_READS: usize = 64;
const MAX_WRITES: usize = 16;
const RECEIPT_LIMIT: u64 = 4096;

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
    receipt: CommitReceipt,
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
            if marker.try_exists()? {
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
        let identity: Identity = serde_json::from_slice(bytes.value()).map_err(error)?;
        if identity.schema != 2
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
        if marker.try_exists()? {
            let marked: Identity =
                serde_json::from_slice(&crate::metadata::read_regular(&marker, 4096)?)
                    .map_err(error)?;
            if marked.schema != 2
                || marked.minecraft != "26.3"
                || marked.data_version != 5023
                || marked.world != identity.world
                || marked.policy != policy
                || !marked.complete
            {
                return Err(error("storage format marker mismatch"));
            }
        } else {
            write_marker(root, MARKER, &serde_json::to_vec(&identity).map_err(error)?)?;
        }
        let guard = root.join("IMPORT_INCOMPLETE");
        if guard.try_exists()? {
            if crate::metadata::read_regular(&guard, 4096)? != GUARD {
                return Err(error("invalid old-version storage guard"));
            }
        } else {
            write_marker(root, "IMPORT_INCOMPLETE", GUARD)?;
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
            schema: 2,
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
            if crate::metadata::read_regular(&root.join("IMPORT_INCOMPLETE"), 4096)? != GUARD {
                return Err(error("unexpected migration guard"));
            }
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
        self.healthy()?;
        let read = self.db.begin_read().map_err(error)?;
        let table = read.open_table(RECEIPTS).map_err(error)?;
        table
            .get(operation.as_bytes().as_slice())
            .map_err(error)?
            .map(|value| decode_receipt(value.value()).map(|saved| saved.receipt))
            .transpose()
    }

    pub(crate) fn commit(
        &self,
        operation: Uuid,
        reads: &[ChunkSnapshot],
        writes: &[ChunkWrite],
    ) -> Result<CommitReceipt, CommitError> {
        self.healthy()?;
        if operation.is_nil()
            || reads.is_empty()
            || reads.len() > MAX_READS
            || writes.is_empty()
            || writes.len() > MAX_WRITES
        {
            return Err(error("invalid transaction limits or operation ID").into());
        }
        let mut expected = BTreeMap::new();
        for snapshot in reads {
            if snapshot.world != self.token_owner
                || expected.insert(snapshot.key, snapshot.revision).is_some()
            {
                return Err(error("duplicate or foreign snapshot").into());
            }
        }
        let mut encoded = BTreeMap::new();
        for write in writes {
            if write.key.x != write.replacement.x
                || write.key.z != write.replacement.z
                || !expected.contains_key(&write.key)
            {
                return Err(error("write requires matching read snapshot and coordinates").into());
            }
            let bytes = write.replacement.to_bytes();
            if bytes.len() > MAX_CHUNK {
                return Err(error("chunk exceeds transaction limit").into());
            }
            Chunk::from_bytes(&bytes)?;
            if encoded.insert(write.key, bytes).is_some() {
                return Err(error("duplicate write").into());
            }
        }
        let mut digest = Sha256::new();
        for (key, revision) in &expected {
            digest.update(key.encoded().to_le_bytes());
            digest.update(revision.to_le_bytes());
        }
        for (key, bytes) in &encoded {
            digest.update(key.encoded().to_le_bytes());
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
        }
        let digest: [u8; 32] = digest.finalize().into();
        let mut transaction = self
            .db
            .begin_write()
            .map_err(|cause| self.backend_error(cause))?;
        transaction.set_two_phase_commit(true);
        let mut table = transaction
            .open_table(CHUNKS)
            .map_err(|cause| self.backend_error(cause))?;
        let mut receipts = transaction
            .open_table(RECEIPTS)
            .map_err(|cause| self.backend_error(cause))?;
        if let Some(saved) = receipts
            .get(operation.as_bytes().as_slice())
            .map_err(|cause| self.backend_error(cause))?
        {
            let saved = decode_receipt(saved.value())?;
            return if saved.digest == digest {
                Ok(saved.receipt)
            } else {
                Err(error("operation ID already has another payload").into())
            };
        }
        for (key, revision) in &expected {
            let current = table
                .get(key.encoded())
                .map_err(|cause| self.backend_error(cause))?;
            let actual = if let Some(value) = current {
                decode_chunk(*key, value.value())?.0
            } else {
                0
            };
            if actual != *revision {
                return Err(CommitError::Conflict);
            }
            if actual == 0 && self.policy == GenerationPolicy::ExistingOnly {
                return Err(WorldError::MissingImportedChunk { x: key.x, z: key.z }.into());
            }
        }
        let mut meta = transaction
            .open_table(META)
            .map_err(|cause| self.backend_error(cause))?;
        let prior = meta
            .get("revision")
            .map_err(|cause| self.backend_error(cause))?
            .ok_or_else(|| error("missing transaction revision"))?;
        let revision = u64::from_le_bytes(
            prior
                .value()
                .try_into()
                .map_err(|_| error("invalid transaction revision"))?,
        )
        .checked_add(1)
        .ok_or_else(|| error("transaction revision overflow"))?;
        drop(prior);
        let mut chunks = Vec::with_capacity(encoded.len());
        for (key, bytes) in encoded {
            let mut value = Vec::with_capacity(bytes.len() + 8);
            value.extend_from_slice(&revision.to_le_bytes());
            value.extend_from_slice(&bytes);
            table
                .insert(key.encoded(), value.as_slice())
                .map_err(|cause| self.backend_error(cause))?;
            chunks.push(CommittedChunk { key, revision });
        }
        let receipt = CommitReceipt {
            operation,
            revision,
            chunks,
        };
        let saved = serde_json::to_vec(&SavedReceipt {
            digest,
            receipt: receipt.clone(),
        })
        .map_err(|cause| self.backend_error(cause))?;
        receipts
            .insert(operation.as_bytes().as_slice(), saved.as_slice())
            .map_err(|cause| self.backend_error(cause))?;
        let mut order = transaction
            .open_table(RECEIPT_ORDER)
            .map_err(|cause| self.backend_error(cause))?;
        order
            .insert(revision, operation.as_bytes().as_slice())
            .map_err(|cause| self.backend_error(cause))?;
        if revision > RECEIPT_LIMIT {
            let old = order
                .remove(revision - RECEIPT_LIMIT)
                .map_err(|cause| self.backend_error(cause))?;
            if let Some(old) = old {
                receipts
                    .remove(old.value())
                    .map_err(|cause| self.backend_error(cause))?;
            }
        }
        meta.insert("revision", revision.to_le_bytes().as_slice())
            .map_err(|cause| self.backend_error(cause))?;
        drop(order);
        drop(meta);
        drop(receipts);
        drop(table);
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
    serde_json::from_slice(value).map_err(error)
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
