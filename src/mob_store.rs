use aws_lc_rs::digest::{Context, SHA256};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::Path,
};
use zerqon_entities::{EntityWorld, Snapshot};

const MAGIC: &[u8; 8] = b"ZQENTITY";
const FORMAT_VERSION: u16 = 1;
const PREFIX_BYTES: usize = 14;
const HEADER_BYTES: usize = PREFIX_BYTES + 32;
const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ENTITIES: usize = 10_000;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("invalid entity snapshot store: {0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Entity(#[from] zerqon_entities::EntityError),
    #[error("cannot atomically replace entity snapshot: {0}")]
    Commit(#[source] std::io::Error),
    #[cfg(unix)]
    #[error("entity snapshot was replaced but directory synchronization failed: {0}")]
    Durability(#[source] std::io::Error),
}

impl StoreError {
    pub fn was_committed(&self) -> bool {
        #[cfg(unix)]
        {
            matches!(self, Self::Durability(_))
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
}

pub fn save(path: &Path, snapshot: &Snapshot) -> Result<(), StoreError> {
    if path.file_name().is_none() {
        return Err(StoreError::Invalid("snapshot needs a filename"));
    }
    if snapshot.entities.len() > MAX_ENTITIES {
        return Err(StoreError::Invalid("snapshot exceeds 10000 entities"));
    }
    EntityWorld::restore(snapshot.clone(), MAX_ENTITIES)?;
    let mut payload = LimitedBuffer(Vec::with_capacity(4096));
    serde_json::to_writer(&mut payload, snapshot)?;
    let payload = payload.0;
    load(path, MAX_ENTITIES)?;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    #[cfg(unix)]
    let parent_directory = File::open(parent)?;
    let mut prefix = [0_u8; PREFIX_BYTES];
    prefix[..8].copy_from_slice(MAGIC);
    prefix[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    prefix[10..14].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    let mut checksum = Context::new(&SHA256);
    checksum.update(&prefix);
    checksum.update(&payload);

    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&prefix)?;
    temporary.write_all(checksum.finish().as_ref())?;
    temporary.write_all(&payload)?;
    temporary.as_file().sync_all()?;
    commit(temporary, path)?;
    #[cfg(unix)]
    parent_directory
        .sync_all()
        .map_err(StoreError::Durability)?;
    Ok(())
}

fn commit(mut temporary: tempfile::NamedTempFile, path: &Path) -> Result<(), StoreError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    loop {
        match temporary.persist(path) {
            Ok(_) => return Ok(()),
            Err(error) => {
                // Windows can briefly deny replacement while a reader releases its handle.
                if cfg!(windows)
                    && matches!(error.error.raw_os_error(), Some(5 | 32 | 33))
                    && std::time::Instant::now() < deadline
                {
                    temporary = error.file;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                } else {
                    return Err(StoreError::Commit(error.error));
                }
            }
        }
    }
}

pub fn load(path: &Path, capacity: usize) -> Result<Option<EntityWorld>, StoreError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() {
        return Err(StoreError::Invalid("snapshot must be a regular file"));
    }
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || !(HEADER_BYTES as u64..=MAX_FILE_BYTES as u64).contains(&metadata.len())
    {
        return Err(StoreError::Invalid(
            "snapshot size is outside allowed bounds",
        ));
    }
    let mut header = [0_u8; HEADER_BYTES];
    file.read_exact(&mut header)?;
    if &header[..8] != MAGIC {
        return Err(StoreError::Invalid("invalid snapshot magic"));
    }
    if u16::from_le_bytes([header[8], header[9]]) != FORMAT_VERSION {
        return Err(StoreError::Invalid("unsupported snapshot file version"));
    }
    let payload_bytes =
        u32::from_le_bytes([header[10], header[11], header[12], header[13]]) as usize;
    if payload_bytes > MAX_FILE_BYTES - HEADER_BYTES
        || metadata.len() != (HEADER_BYTES + payload_bytes) as u64
    {
        return Err(StoreError::Invalid("snapshot payload length mismatch"));
    }
    let mut payload = vec![0_u8; payload_bytes];
    file.read_exact(&mut payload)?;
    if file.read(&mut [0_u8; 1])? != 0 {
        return Err(StoreError::Invalid("trailing snapshot bytes"));
    }
    let mut checksum = Context::new(&SHA256);
    checksum.update(&header[..PREFIX_BYTES]);
    checksum.update(&payload);
    if checksum.finish().as_ref() != &header[PREFIX_BYTES..] {
        return Err(StoreError::Invalid("snapshot checksum mismatch"));
    }
    let snapshot: Snapshot = serde_json::from_slice(&payload)?;
    Ok(Some(EntityWorld::restore(snapshot, capacity)?))
}

struct LimitedBuffer(Vec<u8>);

impl Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > (MAX_FILE_BYTES - HEADER_BYTES).saturating_sub(self.0.len()) {
            return Err(io::Error::other("entity snapshot exceeds 8 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
