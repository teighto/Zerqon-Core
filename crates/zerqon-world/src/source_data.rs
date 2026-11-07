use crate::{
    WorldError, WorldMetadata,
    metadata::{self, RetainedSource},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

const MAX_FILES: usize = 16_384;
const MAX_DIRECTORIES: usize = 16_384;
const MAX_TOTAL: u64 = 64 * 1024 * 1024 * 1024;
const MAX_FILE: u64 = 2 * 1024 * 1024 * 1024;

struct SourceFile {
    relative: PathBuf,
    bytes: u64,
    modified: Option<SystemTime>,
}

pub(crate) struct SourceSnapshot {
    root: PathBuf,
    files: Vec<SourceFile>,
    directories: Vec<PathBuf>,
    bytes: u64,
    _lock: Option<File>,
}

#[derive(Serialize)]
struct RetainedFile {
    path: String,
    bytes: u64,
    sha256: String,
}

impl SourceSnapshot {
    pub fn open(source: &Path, cancelled: &impl Fn() -> bool) -> Result<Self, WorldError> {
        check_cancelled(cancelled)?;
        let root = metadata::source_root(source)?;
        let lock_path = root.join("session.lock");
        metadata::reject_links(&lock_path)?;
        match fs::symlink_metadata(&lock_path) {
            Ok(metadata) if !metadata.is_file() || metadata::is_link(&metadata) => {
                return Err(invalid("session.lock must be a regular file"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut lock_options = fs::OpenOptions::new();
        lock_options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            lock_options.custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            );
        }
        let lock = match lock_options.open(&lock_path) {
            Ok(lock) => {
                if !lock.metadata()?.is_file() {
                    return Err(invalid("session.lock must be a regular file"));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if lock.metadata()?.nlink() != 1 {
                        return Err(invalid(
                            "session.lock must not have hard-link aliases; use a plain source copy",
                        ));
                    }
                }
                match lock.try_lock() {
                    Ok(()) => {
                        #[cfg(unix)]
                        if let Err(error) = rustix::fs::fcntl_lock(
                            &lock,
                            rustix::fs::FlockOperation::NonBlockingLockShared,
                        ) {
                            if matches!(error, rustix::io::Errno::AGAIN | rustix::io::Errno::ACCESS)
                            {
                                return Err(invalid(
                                    "source world is open in Java; stop its server before importing",
                                ));
                            }
                            return Err(std::io::Error::from(error).into());
                        }
                        Some(lock)
                    }
                    Err(fs::TryLockError::WouldBlock) => {
                        return Err(invalid(
                            "source world is open; stop its server before importing",
                        ));
                    }
                    Err(fs::TryLockError::Error(error)) => return Err(error.into()),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let mut snapshot = Self {
            root,
            files: Vec::new(),
            directories: Vec::new(),
            bytes: 0,
            _lock: lock,
        };
        let mut pending = vec![(PathBuf::new(), 0usize)];
        while let Some((relative, depth)) = pending.pop() {
            check_cancelled(cancelled)?;
            if depth > 32 {
                return Err(invalid("source tree exceeds 32 directory levels"));
            }
            for entry in fs::read_dir(snapshot.root.join(&relative))? {
                check_cancelled(cancelled)?;
                let entry = entry?;
                let path = relative.join(entry.file_name());
                let name = relative_name(&path)?;
                if name == "session.lock"
                    || cfg!(windows) && name.eq_ignore_ascii_case("session.lock")
                {
                    continue;
                }
                let metadata = fs::symlink_metadata(entry.path())?;
                if metadata::is_link(&metadata) {
                    return Err(invalid(format!(
                        "source tree contains a link or reparse point: {name}"
                    )));
                }
                if metadata.is_dir() {
                    if snapshot.directories.len() >= MAX_DIRECTORIES {
                        return Err(invalid("too many source directories"));
                    }
                    snapshot.directories.push(path.clone());
                    pending.push((path, depth + 1));
                } else if metadata.is_file() {
                    if snapshot.files.len() >= MAX_FILES
                        || metadata.len() > MAX_FILE
                        || snapshot.bytes > MAX_TOTAL.saturating_sub(metadata.len())
                    {
                        return Err(invalid("source snapshot exceeds file count or size limits"));
                    }
                    snapshot.bytes += metadata.len();
                    snapshot.files.push(SourceFile {
                        relative: path,
                        bytes: metadata.len(),
                        modified: metadata.modified().ok(),
                    });
                } else {
                    return Err(invalid("source contains a special file"));
                }
            }
        }
        snapshot.files.sort_by(|a, b| a.relative.cmp(&b.relative));
        snapshot.directories.sort();
        Ok(snapshot)
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn retain_into(
        &self,
        destination: &Path,
        world: &WorldMetadata,
        cancelled: &impl Fn() -> bool,
    ) -> Result<RetainedSource, WorldError> {
        check_cancelled(cancelled)?;
        let destination = metadata::source_root(destination)?;
        let root = destination.join("source-data");
        metadata::reject_links(&root)?;
        fs::create_dir(&root)?;
        let raw = root.join("files");
        fs::create_dir(&raw)?;
        for directory in &self.directories {
            check_cancelled(cancelled)?;
            let path = raw.join(directory);
            metadata::reject_links(&path)?;
            fs::create_dir_all(path)?;
        }
        let mut retained = Vec::with_capacity(self.files.len());
        for source in &self.files {
            check_cancelled(cancelled)?;
            let path = self.root.join(&source.relative);
            metadata::reject_links(&path)?;
            let before = fs::symlink_metadata(&path)?;
            if metadata::is_link(&before)
                || !before.is_file()
                || before.len() != source.bytes
                || before.modified().ok() != source.modified
            {
                return Err(invalid("source file changed during import"));
            }
            let mut input = File::open(&path)?;
            let target = raw.join(&source.relative);
            metadata::reject_links(&target)?;
            let parent = target
                .parent()
                .ok_or_else(|| invalid("retained file needs a parent"))?;
            let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
            let mut digest = Sha256::new();
            let mut bytes = 0u64;
            let mut buffer = [0u8; 65536];
            loop {
                check_cancelled(cancelled)?;
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                bytes = bytes
                    .checked_add(count as u64)
                    .ok_or_else(|| invalid("source file length overflow"))?;
                if bytes > source.bytes {
                    return Err(invalid("source file grew during import"));
                }
                digest.update(&buffer[..count]);
                temporary.write_all(&buffer[..count])?;
            }
            let after = input.metadata()?;
            check_cancelled(cancelled)?;
            if bytes != source.bytes
                || after.len() != source.bytes
                || after.modified().ok() != source.modified
            {
                return Err(invalid("source file changed during copy"));
            }
            metadata::reject_links(&target)?;
            temporary.as_file().sync_all()?;
            temporary
                .persist_noclobber(&target)
                .map_err(|error| error.error)?;
            retained.push(RetainedFile {
                path: relative_name(&source.relative)?,
                bytes,
                sha256: digest
                    .finalize()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            });
        }
        for (path, expected) in
            std::iter::once(("level.dat", world.source.level_dat_sha256.as_str()))
                .chain(std::iter::once((
                    "data/minecraft/world_gen_settings.dat",
                    world.source.generator_sha256.as_str(),
                )))
                .chain(
                    world
                        .saved_data
                        .iter()
                        .map(|entry| (entry.path.as_str(), entry.sha256.as_str())),
                )
        {
            check_cancelled(cancelled)?;
            if retained
                .iter()
                .find(|file| file.path == path)
                .map(|file| file.sha256.as_str())
                != Some(expected)
            {
                return Err(invalid(
                    "retained metadata differs from parsed source metadata",
                ));
            }
        }
        #[cfg(unix)]
        {
            for directory in self.directories.iter().rev() {
                check_cancelled(cancelled)?;
                File::open(raw.join(directory))?.sync_all()?;
            }
            File::open(&raw)?.sync_all()?;
        }
        let directories = self
            .directories
            .iter()
            .map(|path| relative_name(path))
            .collect::<Result<Vec<_>, _>>()?;
        let not_retained = if self._lock.is_some() {
            vec![serde_json::json!({"path":"session.lock","reason":"operational source lock"})]
        } else {
            Vec::new()
        };
        let manifest = serde_json::json!({"schema":1,"source_data_version":5023,"root":"files","files":retained,"directories":directories,"not_retained":not_retained});
        let bytes =
            serde_json::to_vec_pretty(&manifest).map_err(|error| invalid(error.to_string()))?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(invalid("retained source manifest exceeds 16 MiB"));
        }
        let checksum = metadata::checksum(&bytes);
        check_cancelled(cancelled)?;
        let mut file = tempfile::NamedTempFile::new_in(&root)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(root.join("manifest.json"))
            .map_err(|error| error.error)?;
        #[cfg(unix)]
        File::open(&root)?.sync_all()?;
        Ok(RetainedSource {
            manifest: "source-data/manifest.json".into(),
            manifest_sha256: checksum,
            files: self.files.len(),
            bytes: self.bytes,
        })
    }
}

fn check_cancelled(cancelled: &impl Fn() -> bool) -> Result<(), WorldError> {
    if cancelled() {
        Err(WorldError::Cancelled)
    } else {
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> WorldError {
    WorldError::Metadata(message.into())
}
fn relative_name(path: &Path) -> Result<String, WorldError> {
    if path.is_absolute() {
        return Err(invalid("source path must be relative"));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        let std::path::Component::Normal(part) = component else {
            return Err(invalid("source path contains traversal"));
        };
        let part = part
            .to_str()
            .ok_or_else(|| invalid("source path is not UTF-8"))?;
        if part.is_empty()
            || part.len() > 255
            || part.contains(['\\', ':', '<', '>', '"', '|', '?', '*'])
            || part.ends_with(['.', ' '])
            || part.chars().any(char::is_control)
        {
            return Err(invalid("source path is not portable"));
        }
        let base = part
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if matches!(
            base.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || (base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && matches!(base.as_bytes()[3], b'1'..=b'9'))
        {
            return Err(invalid("source path contains a reserved device name"));
        }
        parts.push(part);
    }
    let value = parts.join("/");
    if value.len() > 1024 {
        return Err(invalid("source path exceeds 1024 bytes"));
    }
    Ok(value)
}
