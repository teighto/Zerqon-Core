use std::fs::{File, TryLockError};
use std::io;

/// Owns an exclusive lock and explicitly unlocks before closing its file.
/// Wrap an unlocked file before acquiring through `try_lock`. Duplicates of the
/// same file description must not acquire or release this guard's lock.
#[derive(Debug)]
pub struct ExclusiveFileLock {
    file: Option<File>,
    held: bool,
}

impl ExclusiveFileLock {
    pub fn new(file: File) -> Self {
        Self {
            file: Some(file),
            held: false,
        }
    }

    pub fn try_lock(&mut self) -> Result<(), TryLockError> {
        if self.held {
            return Ok(());
        }
        self.file
            .as_ref()
            .expect("file remains open until consuming release or drop")
            .try_lock()?;
        self.held = true;
        Ok(())
    }

    pub fn release(mut self) -> io::Result<()> {
        self.unlock_and_close()
    }

    fn unlock_and_close(&mut self) -> io::Result<()> {
        let Some(file) = self.file.take() else {
            self.held = false;
            return Ok(());
        };
        let held = std::mem::take(&mut self.held);
        let result = if held { file.unlock() } else { Ok(()) };
        drop(file);
        result
    }
}

impl Drop for ExclusiveFileLock {
    fn drop(&mut self) {
        if let Err(error) = self.unlock_and_close() {
            tracing::error!(%error, "failed to release exclusive file lock");
        }
    }
}

#[cfg(unix)]
impl std::os::fd::AsFd for ExclusiveFileLock {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.file
            .as_ref()
            .expect("file remains open until consuming release or drop")
            .as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::path::Path;
    use std::sync::Arc;

    fn open_file(path: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap()
    }

    fn assert_blocked(path: &Path) {
        let mut contender = ExclusiveFileLock::new(open_file(path));
        assert!(matches!(
            contender.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
    }

    fn assert_available(path: &Path) {
        let mut contender = ExclusiveFileLock::new(open_file(path));
        contender.try_lock().unwrap();
        contender.release().unwrap();
    }

    #[test]
    fn held_lock_blocks_independent_contender_until_release() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.lock");
        let mut owner = ExclusiveFileLock::new(open_file(&path));
        owner.try_lock().unwrap();
        assert_blocked(&path);
        owner.release().unwrap();
        assert_available(&path);
    }

    #[test]
    fn repeated_acquisition_preserves_exclusive_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.lock");
        let mut owner = ExclusiveFileLock::new(open_file(&path));
        owner.try_lock().unwrap();
        owner.try_lock().unwrap();
        assert_blocked(&path);
        owner.release().unwrap();
        assert_available(&path);
    }

    #[test]
    fn unarmed_duplicate_drop_preserves_owner_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.lock");
        let file = open_file(&path);
        let duplicate = ExclusiveFileLock::new(file.try_clone().unwrap());
        let mut owner = ExclusiveFileLock::new(file);
        owner.try_lock().unwrap();
        drop(duplicate);
        assert_blocked(&path);
    }

    #[test]
    fn unarmed_duplicate_release_preserves_owner_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.lock");
        let file = open_file(&path);
        let duplicate = ExclusiveFileLock::new(file.try_clone().unwrap());
        let mut owner = ExclusiveFileLock::new(file);
        owner.try_lock().unwrap();
        duplicate.release().unwrap();
        assert_blocked(&path);
    }

    #[test]
    fn failed_acquisition_drop_preserves_owner_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.lock");
        let mut owner = ExclusiveFileLock::new(open_file(&path));
        owner.try_lock().unwrap();
        let mut contender = ExclusiveFileLock::new(open_file(&path));
        assert!(matches!(
            contender.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        drop(contender);
        assert_blocked(&path);
    }

    #[test]
    fn failed_acquisition_can_retry_after_owner_releases() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.lock");
        let mut owner = ExclusiveFileLock::new(open_file(&path));
        owner.try_lock().unwrap();
        let mut contender = ExclusiveFileLock::new(open_file(&path));
        assert!(matches!(
            contender.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        owner.release().unwrap();
        contender.try_lock().unwrap();
        assert_blocked(&path);
        contender.release().unwrap();
        assert_available(&path);
    }

    #[test]
    fn explicit_release_unlocks_while_duplicate_remains_open() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.lock");
        let file = open_file(&path);
        let duplicate = file.try_clone().unwrap();
        let mut owner = ExclusiveFileLock::new(file);
        owner.try_lock().unwrap();
        owner.release().unwrap();
        assert_available(&path);
        drop(duplicate);
    }

    #[test]
    fn drop_unlocks_while_duplicate_remains_open() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.lock");
        let file = open_file(&path);
        let duplicate = file.try_clone().unwrap();
        let mut owner = ExclusiveFileLock::new(file);
        owner.try_lock().unwrap();
        drop(owner);
        assert_available(&path);
        drop(duplicate);
    }

    #[test]
    fn last_arc_releases_lock_while_duplicate_remains_open() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.lock");
        let file = open_file(&path);
        let duplicate = file.try_clone().unwrap();
        let mut owner = ExclusiveFileLock::new(file);
        owner.try_lock().unwrap();
        let owner = Arc::new(owner);
        let last_owner = Arc::clone(&owner);
        drop(owner);
        assert_blocked(&path);
        drop(last_owner);
        assert_available(&path);
        drop(duplicate);
    }

    #[test]
    fn unwinding_unlocks_while_duplicate_remains_open() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.lock");
        let file = open_file(&path);
        let duplicate = file.try_clone().unwrap();
        let result = std::panic::catch_unwind(move || {
            let mut owner = ExclusiveFileLock::new(file);
            owner.try_lock().unwrap();
            panic!("failure after acquiring file lock");
        });
        assert!(result.is_err());
        assert_available(&path);
        drop(duplicate);
    }

    #[cfg(unix)]
    #[test]
    fn borrowed_descriptor_supports_source_snapshot_posix_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.lock");
        let mut owner = ExclusiveFileLock::new(open_file(&path));
        owner.try_lock().unwrap();
        rustix::fs::fcntl_lock(&owner, rustix::fs::FlockOperation::NonBlockingLockShared).unwrap();
        owner.release().unwrap();
        assert_available(&path);
    }
}
