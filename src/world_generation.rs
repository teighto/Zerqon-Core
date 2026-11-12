use crate::Result;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
};
use zerqon_world::{NORMAL_GENERATION_GUARD, WorldStore};

const BINDING: &str = "normal-generator.json";
const GUARD: &str = "IMPORT_INCOMPLETE";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    version: u32,
    minecraft: String,
    protocol: u32,
    seed: u64,
}

pub fn prepare(path: &Path, normal: bool, seed: u64) -> Result<()> {
    let exists = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => true,
        Ok(_) => return Err("world path must be a regular directory".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if exists {
        let binding_path = path.join(BINDING);
        let binding = read_optional(&binding_path)?;
        let guard = read_optional(&path.join(GUARD))?;
        if !normal {
            if binding.is_some() || guard.as_deref() == Some(NORMAL_GENERATION_GUARD) {
                return Err(
                    "this world requires level-type=normal and its original level-seed".into(),
                );
            }
            return Ok(());
        }
        let Some(binding) = binding else {
            return Err(
                "normal generation requires a new level-name; existing worlds are preserved".into(),
            );
        };
        let binding: Binding = serde_json::from_slice(&binding)?;
        if binding.version != 1
            || binding.minecraft != "26.3"
            || binding.protocol != 777
            || binding.seed != seed
        {
            return Err("normal generator version or level-seed differs from this world".into());
        }
        if guard.as_deref() != Some(NORMAL_GENERATION_GUARD) {
            return Err("normal world downgrade guard is missing or invalid".into());
        }
        return Ok(());
    }
    if !normal {
        return Ok(());
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let stage = tempfile::Builder::new()
        .prefix(".zerqon-normal-")
        .tempdir_in(parent)?;
    let store = WorldStore::open(stage.path())?;
    let binding = Binding {
        version: 1,
        minecraft: "26.3".into(),
        protocol: 777,
        seed,
    };
    write_synced(&stage.path().join(BINDING), &serde_json::to_vec(&binding)?)?;
    write_synced(&stage.path().join(GUARD), NORMAL_GENERATION_GUARD)?;
    drop(store);
    #[cfg(unix)]
    File::open(stage.path())?.sync_all()?;
    publish(stage.path(), path)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn publish(stage: &Path, destination: &Path) -> Result<()> {
    // TempDir owns cleanup; persist_noclobber also fences a concurrently created destination.
    tempfile::TempPath::try_from_path(stage)?
        .persist_noclobber(destination)
        .map_err(|error| error.error)?;
    Ok(())
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 4096 {
        return Err(format!("invalid world marker: {}", path.display()).into());
    }
    let mut bytes = Vec::new();
    File::open(path)?.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err("world marker exceeds its size limit".into());
    }
    Ok(Some(bytes))
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_world_pins_seed_type_and_guard_across_reopen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("world");
        prepare(&path, true, 73).unwrap();
        let store = WorldStore::open(&path).unwrap();
        let id = store.uuid().unwrap();
        drop(store);
        prepare(&path, true, 73).unwrap();
        assert_eq!(WorldStore::open(&path).unwrap().uuid().unwrap(), id);
        assert!(prepare(&path, true, 74).is_err());
        assert!(prepare(&path, false, 73).is_err());
        assert_eq!(fs::read(path.join(GUARD)).unwrap(), NORMAL_GENERATION_GUARD);
        fs::remove_file(path.join("chunks.redb")).unwrap();
        assert!(WorldStore::open(&path).is_err());
    }

    #[test]
    fn normal_initialization_preserves_existing_world_and_rejects_missing_guard() {
        let root = tempfile::tempdir().unwrap();
        let existing = root.path().join("existing");
        fs::create_dir(&existing).unwrap();
        let stage = tempfile::tempdir_in(root.path()).unwrap();
        fs::write(stage.path().join("sentinel"), b"preserved").unwrap();
        assert!(publish(stage.path(), &existing).is_err());
        assert_eq!(fs::read_dir(&existing).unwrap().count(), 0);
        assert_eq!(
            fs::read(stage.path().join("sentinel")).unwrap(),
            b"preserved"
        );
        let flat = root.path().join("flat");
        let store = WorldStore::open(&flat).unwrap();
        let id = store.uuid().unwrap();
        drop(store);
        assert!(prepare(&flat, true, 73).is_err());
        prepare(&flat, false, 0).unwrap();
        assert_eq!(WorldStore::open(&flat).unwrap().uuid().unwrap(), id);
        let normal = root.path().join("normal");
        prepare(&normal, true, 73).unwrap();
        fs::remove_file(normal.join(GUARD)).unwrap();
        assert!(prepare(&normal, true, 73).is_err());
    }
}
