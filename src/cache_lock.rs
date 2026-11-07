use crate::lifecycle::{JobGuard, Lifecycle, RegistrationError};
use std::fs::{File, TryLockError};
use zerqon_world::ExclusiveFileLock;

pub struct TrackedCacheLock {
    lock: Option<ExclusiveFileLock>,
    job: Option<JobGuard>,
}

impl TrackedCacheLock {
    pub fn new(
        file: File,
        lifecycle: &Lifecycle,
        label: &'static str,
    ) -> Result<Self, RegistrationError> {
        let job = lifecycle.register(label)?;
        Ok(Self {
            lock: Some(ExclusiveFileLock::new(file)),
            job: Some(job),
        })
    }

    pub fn try_lock(&mut self) -> Result<(), TryLockError> {
        match self.lock.as_mut() {
            Some(lock) => lock.try_lock(),
            None => Err(TryLockError::Error(std::io::Error::other(
                "cache lock is already closed",
            ))),
        }
    }
}

impl Drop for TrackedCacheLock {
    fn drop(&mut self) {
        let released = self.lock.take().map_or(Ok(()), ExclusiveFileLock::release);
        if let Some(job) = self.job.take() {
            job.finish(released.map_err(|error| format!("cache lock release failed: {error}")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};

    #[tokio::test]
    async fn last_tracked_owner_unlocks_while_an_inherited_description_remains_open() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache.lock");
        let file = File::options()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let inherited = file.try_clone().unwrap();
        let contender = File::options().read(true).write(true).open(&path).unwrap();
        let lifecycle = Lifecycle::new();
        let mut lock = TrackedCacheLock::new(file, &lifecycle, "controlled cache lock").unwrap();
        lock.try_lock().unwrap();
        let owner = Arc::new(lock);
        let worker = Arc::clone(&owner);
        drop(owner);
        assert!(matches!(
            contender.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        lifecycle.stop();
        let pending = lifecycle
            .drain(tokio::time::Instant::now() + Duration::from_millis(20))
            .await;
        assert!(matches!(
            pending,
            Err(crate::lifecycle::DrainError::Deadline { .. })
        ));
        drop(worker);
        lifecycle
            .drain(tokio::time::Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        contender.try_lock().unwrap();
        assert!(inherited.metadata().unwrap().is_file());
        contender.unlock().unwrap();
    }

    #[tokio::test]
    async fn dropping_an_unacquired_guard_does_not_unlock_another_owner() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache.lock");
        let owner = File::options()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        owner.try_lock().unwrap();
        let lifecycle = Lifecycle::new();
        let unused = TrackedCacheLock::new(
            owner.try_clone().unwrap(),
            &lifecycle,
            "unacquired cache lock",
        )
        .unwrap();
        drop(unused);
        lifecycle
            .drain(tokio::time::Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        let contender = File::options().read(true).write(true).open(&path).unwrap();
        assert!(matches!(
            contender.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        owner.unlock().unwrap();
    }
}
