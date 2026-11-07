use super::*;
use redb::StorageBackend;
use std::{
    io,
    sync::{Mutex, atomic::AtomicUsize},
};

#[derive(Debug, Default)]
struct Disk {
    live: Mutex<Vec<u8>>,
    durable: Mutex<Vec<u8>>,
    events: Mutex<Vec<&'static str>>,
    count: AtomicUsize,
    fail: AtomicUsize,
    fail_after_sync: AtomicUsize,
    syncs: AtomicUsize,
}

#[derive(Debug, Clone)]
struct Backend(Arc<Disk>);

impl Backend {
    fn event(&self, event: &'static str) -> io::Result<()> {
        self.0.events.lock().unwrap().push(event);
        let count = self.0.count.fetch_add(1, Ordering::SeqCst) + 1;
        if count == self.0.fail.load(Ordering::SeqCst) {
            Err(io::Error::other("injected storage failure"))
        } else {
            Ok(())
        }
    }
}

impl StorageBackend for Backend {
    fn len(&self) -> io::Result<u64> {
        Ok(self.0.live.lock().unwrap().len() as u64)
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let offset = usize::try_from(offset).map_err(io::Error::other)?;
        let data = self.0.live.lock().unwrap();
        let end = offset
            .checked_add(out.len())
            .ok_or_else(|| io::Error::other("overflow"))?;
        out.copy_from_slice(
            data.get(offset..end)
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "read"))?,
        );
        Ok(())
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.event("set_len")?;
        if len > 64 * 1024 * 1024 {
            return Err(io::Error::other("test disk capacity"));
        }
        self.0.live.lock().unwrap().resize(len as usize, 0);
        Ok(())
    }
    fn sync_data(&self) -> io::Result<()> {
        self.event("sync")?;
        *self.0.durable.lock().unwrap() = self.0.live.lock().unwrap().clone();
        let sync = self.0.syncs.fetch_add(1, Ordering::SeqCst) + 1;
        if sync == self.0.fail_after_sync.load(Ordering::SeqCst) {
            Err(io::Error::other(
                "sync persisted data then reported failure",
            ))
        } else {
            Ok(())
        }
    }
    fn write(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.event("write")?;
        let offset = usize::try_from(offset).map_err(io::Error::other)?;
        let end = offset
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("overflow"))?;
        let mut data = self.0.live.lock().unwrap();
        data.get_mut(offset..end)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "write"))?
            .copy_from_slice(bytes);
        Ok(())
    }
}

fn database(disk: Arc<Disk>) -> Database {
    Database::builder()
        .set_cache_size(1024 * 1024)
        .create_with_backend(Backend(disk))
        .unwrap()
}
fn reset(disk: &Disk) {
    disk.count.store(0, Ordering::SeqCst);
    disk.syncs.store(0, Ordering::SeqCst);
    disk.events.lock().unwrap().clear();
}
fn store(db: Database) -> Transactions {
    Transactions {
        db,
        world_uuid: Uuid::new_v4(),
        token_owner: Uuid::new_v4(),
        policy: GenerationPolicy::NativeFlat,
        failed: AtomicBool::new(false),
    }
}
fn chunk(x: i32, value: u32) -> Chunk {
    let mut chunk = Chunk::flat(
        x,
        0,
        crate::FlatPalette {
            air: 0,
            bedrock: 1,
            stone: 2,
            dirt: 3,
            grass: 4,
        },
    );
    chunk.set_block(0, 64, 0, value).unwrap();
    chunk
}
fn baseline() -> Vec<u8> {
    let disk = Arc::new(Disk::default());
    let db = database(Arc::clone(&disk));
    let mut transaction = db.begin_write().unwrap();
    transaction.set_two_phase_commit(true);
    {
        let mut meta = transaction.open_table(META).unwrap();
        meta.insert("revision", 1u64.to_le_bytes().as_slice())
            .unwrap();
        transaction.open_table(RECEIPTS).unwrap();
        transaction.open_table(RECEIPT_ORDER).unwrap();
        transaction.open_table(CHUNKS).unwrap();
    }
    transaction.commit().unwrap();
    migration_batch(
        &db,
        &[
            (ChunkKey::new(0, 0), chunk(0, 0).to_bytes()),
            (ChunkKey::new(1, 0), chunk(1, 0).to_bytes()),
        ],
    )
    .unwrap();
    drop(db);
    disk.durable.lock().unwrap().clone()
}
fn restore(bytes: &[u8]) -> Arc<Disk> {
    Arc::new(Disk {
        live: Mutex::new(bytes.to_vec()),
        durable: Mutex::new(bytes.to_vec()),
        ..Disk::default()
    })
}
fn update(store: &Transactions, op: Uuid) -> Result<CommitReceipt, CommitError> {
    let keys = [ChunkKey::new(0, 0), ChunkKey::new(1, 0)];
    let reads = store.snapshot(&keys).unwrap();
    store.commit(
        op,
        &reads,
        &[
            ChunkWrite {
                key: keys[0],
                replacement: Arc::new(chunk(0, 99)),
            },
            ChunkWrite {
                key: keys[1],
                replacement: Arc::new(chunk(1, 99)),
            },
        ],
    )
}
fn values(store: &Transactions) -> (u32, u32) {
    let chunks = store
        .snapshot(&[ChunkKey::new(0, 0), ChunkKey::new(1, 0)])
        .unwrap();
    (
        chunks[0].chunk().unwrap().get_block(0, 64, 0).unwrap(),
        chunks[1].chunk().unwrap().get_block(0, 64, 0).unwrap(),
    )
}

#[test]
fn every_write_and_sync_failure_recovers_one_complete_transaction() {
    let original = baseline();
    let disk = restore(&original);
    let successful = store(database(Arc::clone(&disk)));
    reset(&disk);
    update(&successful, Uuid::new_v4()).unwrap();
    let boundaries = disk.events.lock().unwrap().len();
    assert!(boundaries >= 4);
    drop(successful);
    for boundary in 1..=boundaries {
        let disk = restore(&original);
        let current = store(database(Arc::clone(&disk)));
        reset(&disk);
        disk.fail.store(boundary, Ordering::SeqCst);
        let operation = Uuid::new_v4();
        let result = update(&current, operation);
        assert!(
            result.is_err(),
            "injected boundary{boundary} did not fail the operation"
        );
        let persisted = disk.durable.lock().unwrap().clone();
        if result.is_err() {
            assert!(
                matches!(
                    current.snapshot(&[ChunkKey::new(0, 0)]),
                    Err(WorldError::StorageUnavailable)
                ),
                "boundary{boundary}"
            );
        }
        drop(current);
        let recovered_disk = restore(&persisted);
        let recovered = store(database(Arc::clone(&recovered_disk)));
        let pair = values(&recovered);
        assert!(
            pair == (0, 0) || pair == (99, 99),
            "boundary{boundary}:{pair:?}"
        );
        if result.is_ok() {
            assert_eq!(pair, (99, 99));
        }
        if matches!(result, Err(CommitError::Rejected(_))) {
            assert_eq!(
                pair,
                (0, 0),
                "rejected boundary{boundary} persisted a commit"
            );
        }
        assert_eq!(
            recovered.receipt(operation).unwrap().is_some(),
            pair == (99, 99)
        );
        drop(recovered);
        let twice = store(database(recovered_disk));
        assert_eq!(values(&twice), pair);
    }
}

#[test]
fn sync_failure_after_durable_commit_is_indeterminate_and_never_rollback() {
    let original = baseline();
    let disk = restore(&original);
    let current = store(database(Arc::clone(&disk)));
    reset(&disk);
    disk.fail_after_sync.store(2, Ordering::SeqCst);
    let operation = Uuid::new_v4();
    assert!(matches!(
        update(&current, operation),
        Err(CommitError::Indeterminate { .. })
    ));
    assert!(matches!(
        current.snapshot(&[ChunkKey::new(0, 0)]),
        Err(WorldError::StorageUnavailable)
    ));
    let persisted = disk.durable.lock().unwrap().clone();
    drop(current);
    let recovered = store(database(restore(&persisted)));
    assert_eq!(values(&recovered), (99, 99));
    assert!(recovered.receipt(operation).unwrap().is_some());
}
