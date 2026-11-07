use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorEntry {
    pub uuid: Uuid,
    pub name: String,
    pub level: u8,
    #[serde(rename = "bypassesPlayerLimit", default)]
    pub bypasses_player_limit: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum OperatorStoreError {
    #[error("operator name must be 1..16 ASCII letters, digits or underscores")]
    InvalidName,
    #[error("operator UUID must not be nil")]
    InvalidUuid,
    #[error("operator level must be 1..4")]
    InvalidLevel,
    #[error("duplicate operator UUID or name")]
    Duplicate,
    #[error(
        "name-only ops.json is not supported; preserve that file and replace it with UUID operator records"
    )]
    LegacyNames,
    #[error("operator file changed externally; reload before updating")]
    Stale,
    #[error("operator file exceeds 1 MiB")]
    TooLarge,
    #[error("another process is updating operators")]
    Busy,
    #[error("operator storage paths must be regular files and directories")]
    InvalidPath,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

const MAX_BYTES: usize = 1024 * 1024;

pub struct OperatorStore {
    path: PathBuf,
    entries: BTreeMap<Uuid, OperatorEntry>,
    snapshot: Option<Vec<u8>>,
}
impl OperatorStore {
    pub fn load(path: &Path) -> Result<Self, OperatorStoreError> {
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        if path.file_name().is_none() {
            return Err(OperatorStoreError::InvalidPath);
        }
        let snapshot = read_snapshot(&path)?;
        let entries = match &snapshot {
            None => BTreeMap::new(),
            Some(bytes) => parse_entries(bytes)?,
        };
        Ok(Self {
            path,
            entries,
            snapshot,
        })
    }
    pub fn grant(&mut self, uuid: Uuid, name: &str, level: u8) -> Result<bool, OperatorStoreError> {
        validate_entry(uuid, name, level)?;
        if self
            .entries
            .values()
            .any(|entry| entry.uuid != uuid && entry.name.eq_ignore_ascii_case(name))
        {
            return Err(OperatorStoreError::Duplicate);
        }
        let bypasses_player_limit = self
            .entries
            .get(&uuid)
            .is_some_and(|entry| entry.bypasses_player_limit);
        let mut next = self.entries.clone();
        next.insert(
            uuid,
            OperatorEntry {
                uuid,
                name: name.into(),
                level,
                bypasses_player_limit,
            },
        );
        self.persist(next)
    }
    pub fn revoke(&mut self, uuid: Uuid) -> Result<bool, OperatorStoreError> {
        if uuid.is_nil() {
            return Err(OperatorStoreError::InvalidUuid);
        }
        let mut next = self.entries.clone();
        next.remove(&uuid);
        self.persist(next)
    }
    pub fn level(&self, uuid: Uuid) -> u8 {
        self.entries.get(&uuid).map_or(0, |entry| entry.level)
    }
    pub fn is_operator(&self, uuid: Uuid) -> bool {
        self.level(uuid) > 0
    }
    /// Administrative metadata lookup; authorization must use the player's UUID.
    pub fn lookup_by_name(&self, name: &str) -> Option<&OperatorEntry> {
        self.entries
            .values()
            .find(|entry| entry.name.eq_ignore_ascii_case(name))
    }
    pub fn entries(&self) -> impl ExactSizeIterator<Item = &OperatorEntry> {
        self.entries.values()
    }

    fn ensure_current(&self) -> Result<(), OperatorStoreError> {
        if read_snapshot(&self.path)? != self.snapshot {
            return Err(OperatorStoreError::Stale);
        }
        Ok(())
    }

    fn persist(&mut self, next: BTreeMap<Uuid, OperatorEntry>) -> Result<bool, OperatorStoreError> {
        if next == self.entries {
            self.ensure_current()?;
            return Ok(false);
        }
        let mut bytes = serde_json::to_vec_pretty(&next.values().collect::<Vec<_>>())?;
        bytes.push(b'\n');
        if bytes.len() > MAX_BYTES {
            return Err(OperatorStoreError::TooLarge);
        }
        let mut lock_name = self
            .path
            .file_name()
            .ok_or(OperatorStoreError::InvalidPath)?
            .to_owned();
        lock_name.push(".lock");
        let lock_path = self.path.with_file_name(lock_name);
        regular_file_or_missing(&lock_path)?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(OperatorStoreError::Busy),
            Err(fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        self.ensure_current()?;
        let parent = self.path.parent().ok_or(OperatorStoreError::InvalidPath)?;
        #[cfg(unix)]
        let directory = File::open(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        self.ensure_current()?;
        if self.snapshot.is_none() {
            temporary
                .persist_noclobber(&self.path)
                .map_err(|error| OperatorStoreError::Io(error.error))?;
        } else {
            temporary
                .persist(&self.path)
                .map_err(|error| OperatorStoreError::Io(error.error))?;
        }
        self.entries = next;
        self.snapshot = Some(bytes);
        #[cfg(unix)]
        directory.sync_all()?;
        Ok(true)
    }
}

fn validate_entry(uuid: Uuid, name: &str, level: u8) -> Result<(), OperatorStoreError> {
    if uuid.is_nil() {
        return Err(OperatorStoreError::InvalidUuid);
    }
    if name.is_empty()
        || name.len() > 16
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(OperatorStoreError::InvalidName);
    }
    if !(1..=4).contains(&level) {
        return Err(OperatorStoreError::InvalidLevel);
    }
    Ok(())
}

fn parse_entries(bytes: &[u8]) -> Result<BTreeMap<Uuid, OperatorEntry>, OperatorStoreError> {
    let entries: Vec<OperatorEntry> = match serde_json::from_slice(bytes) {
        Ok(entries) => entries,
        Err(error) => {
            if serde_json::from_slice::<Vec<String>>(bytes).is_ok() {
                return Err(OperatorStoreError::LegacyNames);
            }
            return Err(error.into());
        }
    };
    let mut by_uuid = BTreeMap::new();
    let mut names = HashSet::new();
    for entry in entries {
        validate_entry(entry.uuid, &entry.name, entry.level)?;
        if !names.insert(entry.name.to_ascii_lowercase())
            || by_uuid.insert(entry.uuid, entry).is_some()
        {
            return Err(OperatorStoreError::Duplicate);
        }
    }
    Ok(by_uuid)
}

fn regular_file_or_missing(path: &Path) -> Result<Option<fs::Metadata>, OperatorStoreError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    #[cfg(windows)]
    let linked = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let linked = metadata.file_type().is_symlink();
    if linked || !metadata.is_file() {
        return Err(OperatorStoreError::InvalidPath);
    }
    Ok(Some(metadata))
}

fn read_snapshot(path: &Path) -> Result<Option<Vec<u8>>, OperatorStoreError> {
    let Some(metadata) = regular_file_or_missing(path)? else {
        return Ok(None);
    };
    if metadata.len() > MAX_BYTES as u64 {
        return Err(OperatorStoreError::TooLarge);
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BYTES {
        return Err(OperatorStoreError::TooLarge);
    }
    Ok(Some(bytes))
}
