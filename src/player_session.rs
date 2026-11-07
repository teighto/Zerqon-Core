use crate::{
    Result, lifecycle::JobGuard, player_inventory::InventoryHandle, players::PlayerHandle,
    runtime::State,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{Notify, OwnedSemaphorePermit, watch};
use uuid::Uuid;

#[derive(Clone, Default)]
pub struct SessionRegistry(Arc<RegistryInner>);

#[derive(Default)]
struct RegistryInner {
    active: AtomicUsize,
    changed: Notify,
}

impl SessionRegistry {
    pub async fn drain(&self) {
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.0.active.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }
    fn enter(&self) -> RegistryLease {
        self.0.active.fetch_add(1, Ordering::AcqRel);
        RegistryLease(Arc::clone(&self.0))
    }
}

struct RegistryLease(Arc<RegistryInner>);
impl Drop for RegistryLease {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_waiters();
    }
}

type Outcome = std::result::Result<(), String>;

struct SessionCompletion {
    outcome: Arc<Mutex<Option<Outcome>>>,
    job: Option<JobGuard>,
    registry: Option<RegistryLease>,
}
impl Drop for SessionCompletion {
    fn drop(&mut self) {
        let outcome = self
            .outcome
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .unwrap_or_else(|| Err("player session ended without event cleanup".into()));
        drop(self.registry.take());
        if let Some(job) = self.job.take() {
            job.finish(outcome);
        }
    }
}

pub struct SessionResources {
    pub registration: PlayerHandle,
    pub inventory: InventoryHandle,
    _slot: OwnedSemaphorePermit,
    _completion: SessionCompletion,
}

struct Phase {
    joining: bool,
    closing: bool,
}

struct SessionInner {
    runtime: tokio::runtime::Handle,
    state: Arc<State>,
    uuid: Uuid,
    id: i32,
    name: String,
    resources: Mutex<Option<Arc<SessionResources>>>,
    phase: Mutex<Phase>,
    joined: watch::Sender<Option<Outcome>>,
    closed: watch::Sender<Option<Outcome>>,
    outcome: Arc<Mutex<Option<Outcome>>>,
    failure: Mutex<Option<String>>,
}

pub struct ConnectionSession {
    inner: Arc<SessionInner>,
}

impl ConnectionSession {
    pub fn new(
        state: Arc<State>,
        uuid: Uuid,
        name: String,
        registration: PlayerHandle,
        inventory: InventoryHandle,
        slot: OwnedSemaphorePermit,
    ) -> Result<Self> {
        let job = state.lifecycle.register("player session events")?;
        let outcome = Arc::new(Mutex::new(None));
        let resources = SessionResources {
            registration,
            inventory,
            _slot: slot,
            _completion: SessionCompletion {
                outcome: Arc::clone(&outcome),
                job: Some(job),
                registry: Some(state.sessions.enter()),
            },
        };
        let id = resources.registration.id();
        let (joined, _) = watch::channel(None);
        let (closed, _) = watch::channel(None);
        Ok(Self {
            inner: Arc::new(SessionInner {
                runtime: tokio::runtime::Handle::current(),
                state,
                uuid,
                id,
                name,
                resources: Mutex::new(Some(Arc::new(resources))),
                phase: Mutex::new(Phase {
                    joining: false,
                    closing: false,
                }),
                joined,
                closed,
                outcome,
                failure: Mutex::new(None),
            }),
        })
    }

    pub fn resources(&self) -> Arc<SessionResources> {
        Arc::clone(
            self.inner
                .resources
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                .expect("open connection owns its resources"),
        )
    }

    pub async fn join(&self) -> Result<()> {
        {
            let mut phase = self
                .inner
                .phase
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if phase.closing {
                return Err("player connection is closing".into());
            }
            if !phase.joining {
                phase.joining = true;
                let inner = Arc::clone(&self.inner);
                self.inner.runtime.spawn(async move {
                    let result = crate::managed_tasks::catch_panic(inner.state.player_event(
                        inner.uuid,
                        inner.id,
                        &inner.name,
                        true,
                    ))
                    .await;
                    let result = match result {
                        Ok(result) => result.map_err(|error| {
                            if crate::managed_tasks::is_cleanup_failure(error.as_ref()) {
                                *inner
                                    .failure
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner()) =
                                    Some(error.to_string());
                            }
                            error.to_string()
                        }),
                        Err(error) => {
                            *inner
                                .failure
                                .lock()
                                .unwrap_or_else(|error| error.into_inner()) =
                                Some(error.to_string());
                            Err(error.to_string())
                        }
                    };
                    inner.joined.send_replace(Some(result));
                });
            }
        }
        wait_result(self.inner.joined.subscribe())
            .await
            .map_err(Into::into)
    }

    pub async fn close(&self) -> Result<()> {
        self.inner.schedule_close();
        wait_result(self.inner.closed.subscribe())
            .await
            .map_err(Into::into)
    }
}

impl Drop for ConnectionSession {
    fn drop(&mut self) {
        self.inner.schedule_close();
    }
}

impl SessionInner {
    fn schedule_close(self: &Arc<Self>) {
        let joining = {
            let mut phase = self.phase.lock().unwrap_or_else(|error| error.into_inner());
            if phase.closing {
                return;
            }
            phase.closing = true;
            phase.joining
        };
        let inner = Arc::clone(self);
        self.runtime.spawn(async move {
            let joined = joining && wait_result(inner.joined.subscribe()).await.is_ok();
            let mut cleanup = if joining {
                match crate::managed_tasks::catch_panic(inner.state.player_event(
                    inner.uuid,
                    inner.id,
                    &inner.name,
                    false,
                ))
                .await
                {
                    Ok(result) => result.map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                }
            } else {
                Ok(())
            };
            if let Some(failure) = inner
                .failure
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                cleanup = Err(match cleanup {
                    Ok(()) => failure,
                    Err(error) => format!("{failure}; {error}"),
                });
            }
            if let Err(error) = &cleanup {
                tracing::warn!(player=%inner.name,%error,"Player event cleanup failed");
            }
            if joined {
                tracing::info!(player=%inner.name,"Player left");
            }
            *inner
                .outcome
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(cleanup.clone());
            drop(
                inner
                    .resources
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take(),
            );
            inner.closed.send_replace(Some(cleanup));
        });
    }
}

async fn wait_result(mut receiver: watch::Receiver<Option<Outcome>>) -> Outcome {
    loop {
        if let Some(result) = receiver.borrow().clone() {
            return result;
        }
        if receiver.changed().await.is_err() {
            return Err("player event owner ended without completion".into());
        }
    }
}
