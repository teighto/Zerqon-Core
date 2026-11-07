use crate::{
    Result,
    lifecycle::{JobGuard, Lifecycle, StopToken},
};
use std::{future::Future, pin::Pin};
use tokio::{sync::mpsc, task::JoinHandle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Tick,
    Console,
}

#[derive(Debug)]
pub(super) struct Completion {
    pub kind: Kind,
    pub cancelled: bool,
    pub result: std::result::Result<(), String>,
}

struct Job {
    work: Pin<Box<dyn Future<Output = Result<()>> + Send>>,
    guard: JobGuard,
    kind: Kind,
}

pub(super) struct OrderedJobs {
    sender: Option<mpsc::Sender<Job>>,
    completed: mpsc::Receiver<Completion>,
    worker: Option<JoinHandle<()>>,
    stop: StopToken,
    lifecycle: Lifecycle,
    tick: bool,
    console: bool,
}

impl OrderedJobs {
    pub fn new(lifecycle: &Lifecycle) -> Result<Self> {
        let worker_guard = lifecycle.register("server ordered work")?;
        let stop = lifecycle.token().child();
        let worker_stop = stop.clone();
        let (sender, mut pending) = mpsc::channel::<Job>(2);
        let (results, completed) = mpsc::channel(2);
        let worker = tokio::spawn(async move {
            while let Some(job) = pending.recv().await {
                let Job { work, guard, kind } = job;
                let (cancelled, result) = if worker_stop.is_stopped() {
                    drop(work);
                    (true, Ok(()))
                } else {
                    let result = crate::managed_tasks::catch_panic(work).await;
                    match result {
                        Ok(Ok(())) => (false, Ok(())),
                        Ok(Err(error))
                            if worker_stop.is_stopped()
                                && crate::managed_tasks::is_cancelled(error.as_ref()) =>
                        {
                            (true, Ok(()))
                        }
                        Ok(Err(error)) => (false, Err(error.to_string())),
                        Err(error) => (false, Err(error.to_string())),
                    }
                };
                guard.finish(result.clone());
                let _ = results
                    .send(Completion {
                        kind,
                        cancelled,
                        result,
                    })
                    .await;
            }
            worker_guard.finish(Ok(()));
        });
        Ok(Self {
            sender: Some(sender),
            completed,
            worker: Some(worker),
            stop,
            lifecycle: lifecycle.clone(),
            tick: false,
            console: false,
        })
    }

    pub fn available(&self, kind: Kind) -> bool {
        self.sender.is_some()
            && !self.stop.is_stopped()
            && match kind {
                Kind::Tick => !self.tick,
                Kind::Console => !self.console,
            }
    }

    pub fn submit(
        &mut self,
        kind: Kind,
        work: impl Future<Output = Result<()>> + Send + 'static,
    ) -> Result<()> {
        if !self.available(kind) {
            return Err("server work slot is unavailable".into());
        }
        let guard = self.lifecycle.register(match kind {
            Kind::Tick => "server tick",
            Kind::Console => "console command",
        })?;
        let job = Job {
            work: Box::pin(work),
            guard,
            kind,
        };
        if let Err(error) = self
            .sender
            .as_ref()
            .ok_or("server work stopped")?
            .try_send(job)
        {
            let Job { work, guard, .. } = error.into_inner();
            drop(work);
            guard.finish(Ok(()));
            return Err("server work queue is full or closed".into());
        }
        match kind {
            Kind::Tick => self.tick = true,
            Kind::Console => self.console = true,
        }
        Ok(())
    }

    pub async fn next(&mut self) -> Option<Completion> {
        let result = self.completed.recv().await?;
        match result.kind {
            Kind::Tick => self.tick = false,
            Kind::Console => self.console = false,
        }
        Some(result)
    }

    pub fn has_pending(&self) -> bool {
        self.tick || self.console
    }

    pub fn close(&mut self) {
        self.stop.request_stop();
        self.sender.take();
    }

    pub async fn drain(&mut self) -> Vec<String> {
        self.close();
        let mut failures = Vec::new();
        while self.has_pending() {
            match self.next().await {
                Some(Completion {
                    result: Err(error), ..
                }) => failures.push(error),
                Some(completion) => {
                    if completion.cancelled {
                        tracing::debug!(?completion.kind, "Queued server work cancelled by shutdown");
                    }
                }
                None => {
                    failures.push("server work results ended before completion".into());
                    break;
                }
            }
        }
        if let Some(worker) = self.worker.take()
            && let Err(error) = worker.await
        {
            failures.push(format!("server work task failed: {error}"));
        }
        failures
    }
}

impl Drop for OrderedJobs {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn pending_tick_and_console_have_one_ordered_slot_each() {
        let lifecycle = Lifecycle::new();
        let mut jobs = OrderedJobs::new(&lifecycle).unwrap();
        let (started, ready) = oneshot::channel();
        let (release, blocked) = oneshot::channel();
        let order = Arc::new(Mutex::new(Vec::new()));
        let first = Arc::clone(&order);
        jobs.submit(Kind::Tick, async move {
            first.lock().unwrap().push(1);
            started.send(()).unwrap();
            blocked.await?;
            first.lock().unwrap().push(2);
            Ok(())
        })
        .unwrap();
        ready.await.unwrap();
        let second = Arc::clone(&order);
        jobs.submit(Kind::Console, async move {
            second.lock().unwrap().push(3);
            Ok(())
        })
        .unwrap();
        assert!(!jobs.available(Kind::Tick) && !jobs.available(Kind::Console));
        assert!(jobs.submit(Kind::Tick, async { Ok(()) }).is_err());
        assert!(jobs.submit(Kind::Console, async { Ok(()) }).is_err());
        assert_eq!(*order.lock().unwrap(), [1]);
        release.send(()).unwrap();
        assert_eq!(jobs.next().await.unwrap().kind, Kind::Tick);
        assert_eq!(jobs.next().await.unwrap().kind, Kind::Console);
        assert_eq!(*order.lock().unwrap(), [1, 2, 3]);
        assert!(jobs.drain().await.is_empty());
        lifecycle
            .drain(tokio::time::Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stop_waits_for_accepted_work_and_cancels_the_queued_job_without_running_it() {
        let lifecycle = Lifecycle::new();
        let mut jobs = OrderedJobs::new(&lifecycle).unwrap();
        let (started, ready) = oneshot::channel();
        let (release, blocked) = oneshot::channel();
        jobs.submit(Kind::Tick, async move {
            started.send(()).unwrap();
            blocked.await?;
            Ok(())
        })
        .unwrap();
        ready.await.unwrap();
        jobs.submit(Kind::Console, async {
            panic!("queued command ran after stop");
        })
        .unwrap();
        lifecycle.stop();
        let mut drain = Box::pin(jobs.drain());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut drain)
                .await
                .is_err()
        );
        release.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), drain)
                .await
                .unwrap()
                .is_empty()
        );
        lifecycle
            .drain(tokio::time::Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn failures_and_panics_are_preserved_in_completion_and_lifecycle_drain() {
        let lifecycle = Lifecycle::new();
        let mut jobs = OrderedJobs::new(&lifecycle).unwrap();
        jobs.submit(Kind::Tick, async { Err("tick storage failed".into()) })
            .unwrap();
        assert!(
            jobs.next()
                .await
                .unwrap()
                .result
                .unwrap_err()
                .contains("storage failed")
        );
        jobs.submit(Kind::Console, async {
            panic!("provider panic");
        })
        .unwrap();
        assert!(jobs.next().await.unwrap().result.is_err());
        assert!(jobs.drain().await.is_empty());
        assert!(
            lifecycle
                .drain(tokio::time::Instant::now() + Duration::from_secs(1))
                .await
                .is_err()
        );
    }
}
