use crate::{
    Result, lifecycle::JobGuard, player_inventory::InventoryHandle, players::PlayerHandle,
    runtime::State,
};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Notify, OwnedSemaphorePermit, watch};
use uuid::Uuid;

#[derive(Clone, Default)]
pub struct SessionRegistry(Arc<RegistryInner>);

#[derive(Default)]
struct RegistryInner {
    entries: Mutex<BTreeMap<(Uuid, i32), Arc<RegistryEntry>>>,
    changed: Notify,
}

#[derive(Default)]
struct RegistryEntry {
    closing: AtomicBool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginAdmission {
    Available,
    Active,
}

impl SessionRegistry {
    /// Waits for closing sessions only; the caller must still reserve the identity.
    pub async fn wait_for_login(&self, uuid: Uuid) -> LoginAdmission {
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let entries = self
                    .0
                    .entries
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let mut closing = false;
                for (_, entry) in entries.range((uuid, i32::MIN)..=(uuid, i32::MAX)) {
                    if !entry.closing.load(Ordering::Acquire) {
                        return LoginAdmission::Active;
                    }
                    closing = true;
                }
                if !closing {
                    return LoginAdmission::Available;
                }
            }
            changed.await;
        }
    }

    pub async fn drain(&self) {
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self
                .0
                .entries
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_empty()
            {
                return;
            }
            changed.await;
        }
    }
    fn enter(&self, uuid: Uuid, id: i32) -> Result<RegistryLease> {
        if uuid.is_nil() || id <= 0 {
            return Err("invalid player session identity".into());
        }
        let key = (uuid, id);
        let entry = Arc::new(RegistryEntry::default());
        {
            let mut entries = self
                .0
                .entries
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            match entries.entry(key) {
                Entry::Occupied(_) => return Err("player session is already registered".into()),
                Entry::Vacant(slot) => slot.insert(Arc::clone(&entry)),
            };
        }
        self.0.changed.notify_waiters();
        Ok(RegistryLease {
            registry: Arc::clone(&self.0),
            key,
            entry,
        })
    }

    fn mark_closing(&self, entry: &RegistryEntry) {
        entry.closing.store(true, Ordering::Release);
        self.0.changed.notify_waiters();
    }
}

struct RegistryLease {
    registry: Arc<RegistryInner>,
    key: (Uuid, i32),
    entry: Arc<RegistryEntry>,
}
impl Drop for RegistryLease {
    fn drop(&mut self) {
        {
            let mut entries = self
                .registry
                .entries
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if entries
                .get(&self.key)
                .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry))
            {
                entries.remove(&self.key);
            }
        }
        self.registry.changed.notify_waiters();
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
    registry_entry: Arc<RegistryEntry>,
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
        let id = registration.id();
        let registry = match state.sessions.enter(uuid, id) {
            Ok(registry) => registry,
            Err(error) => {
                drop(registration);
                drop(inventory);
                drop(slot);
                job.finish(Ok(()));
                return Err(error);
            }
        };
        let registry_entry = Arc::clone(&registry.entry);
        let outcome = Arc::new(Mutex::new(None));
        let resources = SessionResources {
            registration,
            inventory,
            _slot: slot,
            _completion: SessionCompletion {
                outcome: Arc::clone(&outcome),
                job: Some(job),
                registry: Some(registry),
            },
        };
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
                registry_entry,
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
            self.state.sessions.mark_closing(&self.registry_entry);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{future::Future, pin::Pin, task::Poll, time::Duration};

    async fn pending<F: Future>(mut future: Pin<&mut F>) {
        std::future::poll_fn(|context| {
            assert!(future.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
    }

    async fn complete<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(1), future)
            .await
            .expect("session registry did not observe resource release")
    }

    #[tokio::test]
    async fn active_sessions_reject_duplicate_login_without_blocking_unrelated_identity() {
        let registry = SessionRegistry::default();
        let uuid = Uuid::from_u128(1);
        let _active = registry.enter(uuid, 1).unwrap();
        assert_eq!(
            complete(registry.wait_for_login(uuid)).await,
            LoginAdmission::Active
        );
        assert_eq!(
            complete(registry.wait_for_login(Uuid::from_u128(2))).await,
            LoginAdmission::Available
        );
    }

    #[tokio::test]
    async fn closing_login_and_global_drain_wait_for_the_final_resource_lease() {
        let registry = SessionRegistry::default();
        let uuid = Uuid::from_u128(1);
        let owner = Arc::new(registry.enter(uuid, 1).unwrap());
        let retained = Arc::clone(&owner);
        let metadata = Arc::clone(&owner.entry);
        registry.mark_closing(&metadata);
        let mut login = Box::pin(registry.wait_for_login(uuid));
        let mut drain = Box::pin(registry.drain());
        pending(login.as_mut()).await;
        pending(drain.as_mut()).await;
        drop(owner);
        pending(login.as_mut()).await;
        pending(drain.as_mut()).await;
        drop(retained);
        assert_eq!(complete(login).await, LoginAdmission::Available);
        complete(drain).await;
    }

    #[tokio::test]
    async fn abandoning_a_login_waiter_does_not_release_the_closing_session() {
        let registry = SessionRegistry::default();
        let uuid = Uuid::from_u128(1);
        let owner = registry.enter(uuid, 1).unwrap();
        registry.mark_closing(&owner.entry);
        let mut abandoned = Box::pin(registry.wait_for_login(uuid));
        pending(abandoned.as_mut()).await;
        drop(abandoned);
        assert!(registry.enter(uuid, 1).is_err());
        let mut next = Box::pin(registry.wait_for_login(uuid));
        pending(next.as_mut()).await;
        drop(owner);
        assert_eq!(complete(next).await, LoginAdmission::Available);
    }

    #[tokio::test]
    async fn releasing_an_older_generation_does_not_remove_a_newer_active_session() {
        let registry = SessionRegistry::default();
        let uuid = Uuid::from_u128(1);
        let old = registry.enter(uuid, 1).unwrap();
        let current = registry.enter(uuid, 2).unwrap();
        registry.mark_closing(&old.entry);
        assert_eq!(
            complete(registry.wait_for_login(uuid)).await,
            LoginAdmission::Active
        );
        drop(old);
        assert_eq!(
            complete(registry.wait_for_login(uuid)).await,
            LoginAdmission::Active
        );
        registry.mark_closing(&current.entry);
        let mut waiting = Box::pin(registry.wait_for_login(uuid));
        pending(waiting.as_mut()).await;
        drop(current);
        assert_eq!(complete(waiting).await, LoginAdmission::Available);
    }

    #[tokio::test]
    async fn duplicate_registration_cannot_replace_the_original_resource_fence() {
        let registry = SessionRegistry::default();
        let uuid = Uuid::from_u128(1);
        let owner = registry.enter(uuid, 1).unwrap();
        assert!(registry.enter(uuid, 1).is_err());
        registry.mark_closing(&owner.entry);
        let mut waiting = Box::pin(registry.wait_for_login(uuid));
        pending(waiting.as_mut()).await;
        drop(owner);
        assert_eq!(complete(waiting).await, LoginAdmission::Available);
        complete(registry.drain()).await;
    }

    #[tokio::test]
    async fn a_retained_old_closing_marker_cannot_close_a_reused_key() {
        let registry = SessionRegistry::default();
        let uuid = Uuid::from_u128(1);
        let old = registry.enter(uuid, 1).unwrap();
        let marker = Arc::clone(&old.entry);
        registry.mark_closing(&marker);
        drop(old);
        let current = registry.enter(uuid, 1).unwrap();
        registry.mark_closing(&marker);
        assert_eq!(
            complete(registry.wait_for_login(uuid)).await,
            LoginAdmission::Active
        );
        registry.mark_closing(&current.entry);
        let mut waiting = Box::pin(registry.wait_for_login(uuid));
        pending(waiting.as_mut()).await;
        drop(current);
        assert_eq!(complete(waiting).await, LoginAdmission::Available);
    }

    #[tokio::test]
    async fn per_identity_release_does_not_wait_for_other_players_but_drain_does() {
        let registry = SessionRegistry::default();
        let uuid = Uuid::from_u128(1);
        let closing = registry.enter(uuid, 1).unwrap();
        let other = registry.enter(Uuid::from_u128(2), 2).unwrap();
        registry.mark_closing(&closing.entry);
        let mut login = Box::pin(registry.wait_for_login(uuid));
        let mut drain = Box::pin(registry.drain());
        pending(login.as_mut()).await;
        pending(drain.as_mut()).await;
        drop(closing);
        assert_eq!(complete(login).await, LoginAdmission::Available);
        pending(drain.as_mut()).await;
        drop(other);
        complete(drain).await;
    }
}
