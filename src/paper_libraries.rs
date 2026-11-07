use aws_lc_rs::digest::{Context, SHA256};
use serde::Deserialize;
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("Paper libraries: {0}")]
    Invalid(String),
    #[error("Paper library I/O: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Deserialize)]
struct Manifest {
    artifacts: Vec<Artifact>,
}
#[derive(Deserialize)]
struct Artifact {
    file: String,
    sha256: String,
}

pub fn classpath(directory: &Path) -> Result<Vec<PathBuf>, LibraryError> {
    if !directory.try_exists()? {
        return Ok(Vec::new());
    }
    validate(
        directory,
        include_str!("../tools/paper-bridge/dependencies.lock.json"),
        &format!("zerqon-paper-bridge-{}.jar", env!("CARGO_PKG_VERSION")),
    )
}

fn validate(directory: &Path, manifest: &str, bridge: &str) -> Result<Vec<PathBuf>, LibraryError> {
    let manifest: Manifest =
        serde_json::from_str(manifest).map_err(|error| LibraryError::Invalid(error.to_string()))?;
    if manifest.artifacts.is_empty() || manifest.artifacts.len() > 63 {
        return Err(LibraryError::Invalid("invalid dependency count".into()));
    }
    let mut paths = vec![library_path(directory, bridge)?];
    let mut seen = std::collections::HashSet::new();
    for artifact in manifest.artifacts {
        if !seen.insert(artifact.file.clone())
            || artifact.sha256.len() != 64
            || !artifact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(LibraryError::Invalid("invalid dependency manifest".into()));
        }
        let path = library_path(directory, &artifact.file)?;
        let mut file = File::open(&path)?;
        let mut digest = Context::new(&SHA256);
        let mut buffer = [0u8; 65536];
        let mut size = 0usize;
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            size += count;
            if size > 40 * 1024 * 1024 {
                return Err(LibraryError::Invalid(format!(
                    "{} exceeds size limit",
                    artifact.file
                )));
            }
            digest.update(&buffer[..count]);
        }
        let checksum: String = digest
            .finish()
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if checksum != artifact.sha256.to_ascii_lowercase() {
            return Err(LibraryError::Invalid(format!(
                "{} failed its pinned SHA-256 check",
                artifact.file
            )));
        }
        paths.push(path);
    }
    Ok(paths)
}

fn library_path(directory: &Path, name: &str) -> Result<PathBuf, LibraryError> {
    if !name.ends_with(".jar")
        || name.contains(['/', '\\', ':'])
        || name.starts_with('.')
        || name.chars().any(char::is_control)
    {
        return Err(LibraryError::Invalid("invalid library name".into()));
    }
    let path = directory.join(name);
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 40 * 1024 * 1024 {
        return Err(LibraryError::Invalid(format!(
            "{name} must be a regular JAR of at most 40 MiB"
        )));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_pinned_files_are_added_and_changed_libraries_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("runtime.jar"), b"abc").unwrap();
        std::fs::write(dir.path().join("bridge.jar"), b"bridge").unwrap();
        std::fs::write(dir.path().join("unrelated.jar"), b"unrelated").unwrap();
        let manifest = r#"{"artifacts":[{"file":"runtime.jar","sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}]}"#;
        assert_eq!(
            validate(dir.path(), manifest, "bridge.jar").unwrap(),
            vec![
                dir.path().join("bridge.jar"),
                dir.path().join("runtime.jar")
            ]
        );
        std::fs::write(dir.path().join("runtime.jar"), b"abd").unwrap();
        assert!(validate(dir.path(), manifest, "bridge.jar").is_err());
    }
    #[test]
    fn traversal_and_partial_install_are_explicit_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bridge.jar"), b"bridge").unwrap();
        for file in ["../outside.jar", "missing.jar", "C:\\outside.jar"] {
            let manifest =
                serde_json::json!({"artifacts":[{"file":file,"sha256":"00".repeat(32)}]})
                    .to_string();
            assert!(validate(dir.path(), &manifest, "bridge.jar").is_err());
        }
    }
}
