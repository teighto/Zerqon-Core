use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    sync::{Notify, watch},
    time::Instant,
};

const MAX_JOBS: usize = 4096;
const MAX_FAILURES: usize = 128;

#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("operation stopped")]
pub struct Stopped;

#[derive(Debug, thiserror::Error)]
pub enum ReadyError {
    #[error("{0}")]
    Stopped(#[from] Stopped),
    #[error("startup cleanup failed; readiness is unavailable")]
    CleanupFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RegistrationError {
    #[error("server is stopping")]
    Stopped,
    #[error("lifecycle is sealed for cleanup")]
    Sealed,
    #[error("lifecycle job capacity exhausted")]
    Capacity,
}

#[derive(Debug, thiserror::Error)]
pub enum DrainError {
    #[error(
        "cleanup deadline expired; pending jobs: {pending:?}; failures: {failures:?}; omitted: {omitted}"
    )]
    Deadline {
        pending: Vec<&'static str>,
        failures: Vec<String>,
        omitted: u64,
    },
    #[error("cleanup failed: {failures:?}; omitted: {omitted}")]
    Cleanup { failures: Vec<String>, omitted: u64 },
}

struct State {
    draining: bool,
    sealed: bool,
    ready: bool,
    next: u64,
    jobs: BTreeMap<u64, &'static str>,
    failures: Vec<String>,
    omitted: u64,
    stop_deadline: Option<Instant>,
}

struct Inner {
    state: Mutex<State>,
    stop: watch::Sender<bool>,
    _receiver: watch::Receiver<bool>,
    changed: Notify,
    cancelled: Notify,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
    fn finish(&self, id: u64, failure: Option<String>) {
        let mut state = self.state();
        let Some(label) = state.jobs.remove(&id) else {
            return;
        };
        if let Some(error) = failure {
            if state.failures.len() < MAX_FAILURES {
                let mut bounded = error;
                let mut boundary = bounded.len().min(1024);
                while !bounded.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                bounded.truncate(boundary);
                state.failures.push(format!("{label}: {bounded}"));
            } else {
                state.omitted = state.omitted.saturating_add(1);
            }
        }
        drop(state);
        self.changed.notify_waiters();
    }
}

#[derive(Clone)]
pub struct Lifecycle {
    inner: Arc<Inner>,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl Lifecycle {
    pub fn new() -> Self {
        let (stop, _) = watch::channel(false);
        Self::from_shutdown(stop)
    }
    pub fn from_shutdown(stop: watch::Sender<bool>) -> Self {
        let receiver = stop.subscribe();
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    draining: false,
                    sealed: false,
                    ready: false,
                    next: 1,
                    jobs: BTreeMap::new(),
                    failures: Vec::new(),
                    omitted: 0,
                    stop_deadline: None,
                }),
                stop,
                _receiver: receiver,
                changed: Notify::new(),
                cancelled: Notify::new(),
            }),
        }
    }
    pub fn token(&self) -> StopToken {
        StopToken {
            lifecycle: self.clone(),
            scope: None,
            ignore_global: false,
        }
    }
    pub fn cleanup_token(&self) -> StopToken {
        StopToken {
            lifecycle: self.clone(),
            scope: Some(Arc::new(Scope {
                stopped: AtomicBool::new(false),
                parent: None,
            })),
            ignore_global: true,
        }
    }
    pub fn shutdown_sender(&self) -> watch::Sender<bool> {
        self.inner.stop.clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.inner.stop.subscribe()
    }
    pub fn stop(&self) {
        let mut state = self.inner.state();
        state
            .stop_deadline
            .get_or_insert_with(|| Instant::now() + std::time::Duration::from_secs(30));
        self.inner.stop.send_if_modified(|stopped| {
            if *stopped {
                false
            } else {
                *stopped = true;
                true
            }
        });
    }
    pub fn cleanup_deadline(&self) -> Option<Instant> {
        self.inner.state().stop_deadline
    }
    pub fn mark_ready(&self) -> Result<(), ReadyError> {
        let mut state = self.inner.state();
        if !state.failures.is_empty() || state.omitted != 0 {
            return Err(ReadyError::CleanupFailed);
        }
        if state.draining || state.sealed || *self.inner.stop.borrow() {
            return Err(Stopped.into());
        }
        state.ready = true;
        Ok(())
    }
    pub fn is_ready(&self) -> bool {
        self.inner.state().ready
    }
    pub fn register(&self, label: &'static str) -> Result<JobGuard, RegistrationError> {
        self.admit(label, false)
    }
    pub fn register_cleanup(&self, label: &'static str) -> Result<JobGuard, RegistrationError> {
        self.admit(label, true)
    }
    fn admit(&self, label: &'static str, cleanup: bool) -> Result<JobGuard, RegistrationError> {
        let mut state = self.inner.state();
        if state.sealed {
            return Err(RegistrationError::Sealed);
        }
        if state.draining && !cleanup {
            return Err(RegistrationError::Sealed);
        }
        if !cleanup && *self.inner.stop.borrow() {
            return Err(RegistrationError::Stopped);
        }
        if state.jobs.len() == MAX_JOBS || state.next == u64::MAX {
            return Err(RegistrationError::Capacity);
        }
        let id = state.next;
        state.next += 1;
        state.jobs.insert(id, label);
        Ok(JobGuard {
            inner: Arc::clone(&self.inner),
            id: Some(id),
        })
    }
    #[cfg(test)]
    pub fn pending(&self) -> Vec<&'static str> {
        self.inner.state().jobs.values().copied().collect()
    }
    pub async fn drain(&self, deadline: Instant) -> Result<(), DrainError> {
        self.inner.state().draining = true;
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self.inner.state();
                if state.jobs.is_empty() {
                    state.sealed = true;
                    return if state.failures.is_empty() && state.omitted == 0 {
                        Ok(())
                    } else {
                        Err(DrainError::Cleanup {
                            failures: state.failures.clone(),
                            omitted: state.omitted,
                        })
                    };
                }
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                let state = self.inner.state();
                if state.jobs.is_empty() {
                    continue;
                }
                return Err(DrainError::Deadline {
                    pending: state.jobs.values().copied().collect(),
                    failures: state.failures.clone(),
                    omitted: state.omitted,
                });
            }
        }
    }
}

pub struct JobGuard {
    inner: Arc<Inner>,
    id: Option<u64>,
}
impl JobGuard {
    pub fn finish(mut self, cleanup: Result<(), String>) {
        if let Some(id) = self.id.take() {
            self.inner.finish(id, cleanup.err());
        }
    }
}
impl Drop for JobGuard {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.inner.finish(
                id,
                Some("job ended without a cleanup acknowledgement".into()),
            );
        }
    }
}

struct Scope {
    stopped: AtomicBool,
    parent: Option<Arc<Scope>>,
}

#[derive(Clone)]
pub struct StopToken {
    lifecycle: Lifecycle,
    scope: Option<Arc<Scope>>,
    ignore_global: bool,
}

impl StopToken {
    /// Only for work already accepted while normal admission was open.
    pub fn admitted_child(&self) -> Self {
        let mut child = self.child();
        child.ignore_global = true;
        child
    }

    pub fn child(&self) -> Self {
        Self {
            lifecycle: self.lifecycle.clone(),
            scope: Some(Arc::new(Scope {
                stopped: AtomicBool::new(false),
                parent: self.scope.clone(),
            })),
            ignore_global: self.ignore_global,
        }
    }
    pub fn request_stop(&self) {
        if let Some(scope) = &self.scope {
            scope.stopped.store(true, Ordering::Release);
            self.lifecycle.inner.cancelled.notify_waiters();
        } else {
            self.lifecycle.stop();
        }
    }
    pub fn is_stopped(&self) -> bool {
        if !self.ignore_global && *self.lifecycle.inner.stop.borrow() {
            return true;
        }
        let mut scope = self.scope.as_ref();
        while let Some(current) = scope {
            if current.stopped.load(Ordering::Acquire) {
                return true;
            }
            scope = current.parent.as_ref();
        }
        false
    }
    pub fn check(&self) -> Result<(), Stopped> {
        if self.is_stopped() {
            Err(Stopped)
        } else {
            Ok(())
        }
    }
    pub async fn cancelled(&self) {
        let mut global = self.lifecycle.subscribe();
        loop {
            let changed = self.lifecycle.inner.cancelled.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.is_stopped() {
                return;
            }
            tokio::select! { _ = global.changed() => {}, _ = changed => {} }
        }
    }
}
