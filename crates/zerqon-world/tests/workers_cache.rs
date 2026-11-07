use std::sync::{Arc, Barrier, mpsc};
use std::time::Duration;
use zerqon_world::{Chunk, ChunkCache, ChunkWorkers, FlatPalette, WorldError};

const PALETTE: FlatPalette = FlatPalette {
    air: 0,
    bedrock: 1,
    stone: 2,
    dirt: 3,
    grass: 4,
};

#[test]
fn worker_jobs_run_on_distinct_threads_and_return_their_chunks() {
    let pool = ChunkWorkers::new(2, 4).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let (sender, receiver) = mpsc::channel();
    let mut tickets = Vec::new();
    for x in 0..2 {
        let barrier = barrier.clone();
        let sender = sender.clone();
        tickets.push(
            pool.try_submit(move || {
                sender.send(std::thread::current().id()).unwrap();
                barrier.wait();
                Ok(Chunk::flat(x, 8, PALETTE))
            })
            .unwrap(),
        );
    }
    let first = receiver.recv_timeout(Duration::from_secs(3)).unwrap();
    let second = receiver.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_ne!(first, second);
    for (x, ticket) in tickets.into_iter().enumerate() {
        let chunk = ticket.wait_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!((chunk.x, chunk.z), (x as i32, 8));
        assert_eq!(chunk.get_block(0, 63, 0), Some(4));
    }
    pool.shutdown().unwrap();
}

#[test]
fn saturated_queue_rejects_work_instead_of_allocating_without_bound() {
    let pool = ChunkWorkers::new(1, 2).unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let active = pool
        .try_submit(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(Chunk::flat(0, 0, PALETTE))
        })
        .unwrap();
    started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let queued1 = pool.try_generate(1, 0, PALETTE).unwrap();
    let queued2 = pool.try_generate(2, 0, PALETTE).unwrap();
    let rejected = pool.try_generate(3, 0, PALETTE);
    release_tx.send(()).unwrap();
    assert!(matches!(rejected, Err(WorldError::QueueFull)));
    assert_eq!(active.wait().unwrap().x, 0);
    assert_eq!(queued1.wait().unwrap().x, 1);
    assert_eq!(queued2.wait().unwrap().x, 2);
}

#[test]
fn failed_or_abandoned_job_does_not_break_remaining_workers() {
    let pool = ChunkWorkers::new(1, 2).unwrap();
    let failed = pool
        .try_submit::<Chunk>(|| panic!("intentional worker isolation test"))
        .unwrap();
    assert!(matches!(failed.wait(), Err(WorldError::WorkerPanicked)));
    drop(pool.try_generate(1, 0, PALETTE).unwrap());
    assert_eq!(
        pool.try_generate(2, 0, PALETTE).unwrap().wait().unwrap().x,
        2
    );
    pool.shutdown().unwrap();
}

#[test]
fn generic_worker_results_preserve_type_and_job_errors() {
    let pool = ChunkWorkers::new(1, 4).unwrap();
    assert_eq!(
        pool.try_submit(|| Ok(vec![1_u8, 2, 3]))
            .unwrap()
            .wait()
            .unwrap(),
        [1, 2, 3]
    );
    pool.try_submit(|| Ok(())).unwrap().wait().unwrap();
    assert!(matches!(
        pool.try_submit::<String>(|| panic!("typed job failure"))
            .unwrap()
            .wait(),
        Err(WorldError::WorkerPanicked)
    ));
    assert_eq!(
        pool.try_submit(|| Ok("healthy".to_owned()))
            .unwrap()
            .wait()
            .unwrap(),
        "healthy"
    );
    pool.shutdown().unwrap();
}

#[test]
fn zero_or_excessive_worker_limits_are_rejected() {
    for (threads, capacity) in [(0, 1), (1, 0), (257, 1), (1, 1_000_000)] {
        assert!(ChunkWorkers::new(threads, capacity).is_err());
    }
}

#[test]
fn least_recently_used_chunk_is_returned_to_the_caller_on_eviction() {
    let mut cache = ChunkCache::new(2).unwrap();
    cache.insert(Arc::new(Chunk::flat(1, 0, PALETTE)));
    cache.insert(Arc::new(Chunk::flat(2, 0, PALETTE)));
    assert!(cache.get(1, 0).is_some());
    let evicted = cache.insert(Arc::new(Chunk::flat(3, 0, PALETTE)));
    assert_eq!(evicted.iter().map(|chunk| chunk.x).collect::<Vec<_>>(), [2]);
    assert!(cache.get(2, 0).is_none());
    assert_eq!(cache.len(), 2);
}

#[test]
fn cache_accounts_for_replacement_and_refuses_oversized_entries() {
    let first = Arc::new(Chunk::flat(0, 0, PALETTE));
    let size = std::mem::size_of::<Chunk>() + first.heap_bytes();
    let mut cache = ChunkCache::with_limits(100, size).unwrap();
    assert!(cache.insert(first.clone()).is_empty());
    assert_eq!(cache.resident_bytes(), size);
    let evicted = cache.insert(first);
    assert_eq!(evicted.len(), 1);
    assert_eq!(cache.resident_bytes(), size);
    let mut large = Chunk::flat(1, 0, PALETTE);
    for x in 0..16 {
        large.set_block(x, 64, 0, 500 + x as u32).unwrap();
    }
    assert_eq!(cache.insert(Arc::new(large)).len(), 1);
    assert!(cache.get(0, 0).is_some());
    assert_eq!(cache.len(), 1);
}
