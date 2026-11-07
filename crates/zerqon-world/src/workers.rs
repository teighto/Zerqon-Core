use crate::{Chunk, FlatPalette, WorldError};
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

type Job = Box<dyn FnOnce() + Send>;
struct Request {
    job: Job,
}

pub struct ChunkWorkers {
    sender: Option<mpsc::SyncSender<Request>>,
    workers: Vec<JoinHandle<()>>,
}

pub struct ChunkTicket<T = Chunk> {
    receiver: mpsc::Receiver<Result<T, WorldError>>,
}

impl ChunkWorkers {
    pub fn new(threads: usize, queue_capacity: usize) -> Result<Self, WorldError> {
        if !(1..=256).contains(&threads) || !(1..=65_536).contains(&queue_capacity) {
            return Err(WorldError::InvalidLimits);
        }
        let (sender, receiver) = mpsc::sync_channel::<Request>(queue_capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        let mut pool = Self {
            sender: Some(sender),
            workers: Vec::with_capacity(threads),
        };
        for index in 0..threads {
            let receiver = receiver.clone();
            pool.workers.push(
                thread::Builder::new()
                    .name(format!("zerqon-world-{index}"))
                    .spawn(move || {
                        loop {
                            let request = {
                                let Ok(receiver) = receiver.lock() else {
                                    break;
                                };
                                receiver.recv()
                            };
                            let Ok(request) = request else {
                                break;
                            };
                            (request.job)();
                        }
                    })?,
            );
        }
        Ok(pool)
    }

    pub fn try_generate(
        &self,
        x: i32,
        z: i32,
        palette: FlatPalette,
    ) -> Result<ChunkTicket, WorldError> {
        self.try_submit(move || Ok(Chunk::flat(x, z, palette)))
    }

    pub fn try_submit<T: Send + 'static>(
        &self,
        job: impl FnOnce() -> Result<T, WorldError> + Send + 'static,
    ) -> Result<ChunkTicket<T>, WorldError> {
        let sender = self.sender.as_ref().ok_or(WorldError::WorkersStopped)?;
        let (reply, receiver) = mpsc::sync_channel(1);
        let job = Box::new(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(job))
                .unwrap_or(Err(WorldError::WorkerPanicked));
            // A dropped ticket cancels delivery, not an already running job.
            let _ = reply.send(result);
        });
        sender
            .try_send(Request { job })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => WorldError::QueueFull,
                mpsc::TrySendError::Disconnected(_) => WorldError::WorkersStopped,
            })?;
        Ok(ChunkTicket { receiver })
    }

    pub fn shutdown(mut self) -> Result<(), WorldError> {
        self.join()
    }

    fn join(&mut self) -> Result<(), WorldError> {
        self.sender.take();
        let current = thread::current().id();
        if self
            .workers
            .iter()
            .any(|worker| worker.thread().id() == current)
        {
            let workers = std::mem::take(&mut self.workers);
            thread::Builder::new()
                .name("zerqon-world-reaper".into())
                .spawn(move || {
                    for worker in workers {
                        if worker.join().is_err() {
                            eprintln!("Zerqon world worker stopped with a panic");
                        }
                    }
                })?;
            return Ok(());
        }
        let mut panicked = false;
        for worker in self.workers.drain(..) {
            panicked |= worker.join().is_err();
        }
        if panicked {
            Err(WorldError::WorkerPanicked)
        } else {
            Ok(())
        }
    }
}

impl Drop for ChunkWorkers {
    fn drop(&mut self) {
        if let Err(error) = self.join() {
            eprintln!("Zerqon world worker shutdown: {error}");
        }
    }
}

impl<T> ChunkTicket<T> {
    pub fn wait(self) -> Result<T, WorldError> {
        self.receiver
            .recv()
            .map_err(|_| WorldError::WorkersStopped)?
    }
    pub fn wait_timeout(self, timeout: Duration) -> Result<T, WorldError> {
        self.receiver
            .recv_timeout(timeout)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => WorldError::Timeout,
                mpsc::RecvTimeoutError::Disconnected => WorldError::WorkersStopped,
            })?
    }
    pub fn try_result(&self) -> Result<Option<T>, WorldError> {
        match self.receiver.try_recv() {
            Ok(result) => result.map(Some),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(WorldError::WorkersStopped),
        }
    }
}
