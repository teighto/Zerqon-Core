use crate::{
    Result,
    lifecycle::JobGuard,
    player_control::{ControlSender, SessionKey},
    player_inventory::InventoryHandle,
    players::PlayerHandle,
    runtime::{CommandSource, State},
};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Notify, OwnedSemaphorePermit, oneshot, watch};
use uuid::Uuid;
use zerqon_protocol::{codec::get_varint, inbound::InboundPacket};

#[cfg(test)]
mod control_binding_tests;

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
    control: Mutex<Option<ControlSender>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginAdmission {
    Available,
    Active,
}

impl SessionRegistry {
    pub fn control(&self, key: SessionKey) -> Option<ControlSender> {
        let entry = self
            .0
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&(key.uuid, key.session_id))
            .cloned()?;
        if entry.closing.load(Ordering::Acquire) {
            return None;
        }
        entry
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
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
    command_running: bool,
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
    commands_changed: Notify,
    control: Mutex<Option<ControlSender>>,
}

pub struct ConnectionSession {
    inner: Arc<SessionInner>,
}

pub struct ControlBinding(Arc<RegistryEntry>);

impl Drop for ControlBinding {
    fn drop(&mut self) {
        self.0
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}

type CommandResult = std::result::Result<Vec<String>, String>;
type CommandCompletion = Result<CommandResult>;

pub struct CommandReceipt(oneshot::Receiver<CommandCompletion>);

impl CommandReceipt {
    pub async fn wait(&mut self) -> Result<CommandResult> {
        (&mut self.0).await.map_err(|_| {
            crate::managed_tasks::CleanupError("player command ended without a result".into())
        })?
    }
}

struct CommandWork {
    inner: Arc<SessionInner>,
    packet: Option<InboundPacket>,
    resources: Option<Arc<SessionResources>>,
    reply: Option<oneshot::Sender<CommandCompletion>>,
    job: Option<JobGuard>,
}

impl CommandWork {
    fn finish(mut self, result: CommandCompletion, cleanup: Outcome) {
        self.settle(result, cleanup);
    }

    fn settle(&mut self, result: CommandCompletion, cleanup: Outcome) {
        drop(self.packet.take());
        drop(self.resources.take());
        if let Err(error) = &cleanup {
            self.inner
                .failure
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get_or_insert_with(|| error.clone());
        }
        let mut phase = self
            .inner
            .phase
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(job) = self.job.take() {
            job.finish(cleanup);
        }
        phase.command_running = false;
        drop(phase);
        self.inner.commands_changed.notify_waiters();
        if let Some(reply) = self.reply.take() {
            drop(reply.send(result));
        }
    }
}

impl Drop for CommandWork {
    fn drop(&mut self) {
        if self.job.is_some() {
            let error = "player command task ended before settling".to_owned();
            self.settle(
                Err(crate::managed_tasks::CleanupError(error.clone()).into()),
                Err(error),
            );
        }
    }
}

fn command_input(packet: &InboundPacket) -> Result<&str> {
    let mut body = packet.as_slice();
    let id = get_varint(&mut body)?;
    if !matches!(id, 7 | 8) {
        return Err("expected a command packet".into());
    }
    Ok(zerqon_protocol::decode_command(body, id == 8)?)
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
                    command_running: false,
                }),
                joined,
                closed,
                outcome,
                failure: Mutex::new(None),
                registry_entry,
                commands_changed: Notify::new(),
                control: Mutex::new(None),
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

    pub fn bind_control(&self, sender: ControlSender) -> Result<ControlBinding> {
        if sender.target()
            != (SessionKey {
                uuid: self.inner.uuid,
                session_id: self.inner.id,
            })
        {
            return Err("player control belongs to a different session".into());
        }
        let phase = self
            .inner
            .phase
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if phase.closing {
            return Err("player session is closing".into());
        }
        let mut owner = self
            .inner
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if owner.is_some() {
            return Err("player session already owns a control transport".into());
        }
        *self
            .inner
            .registry_entry
            .control
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(sender.clone());
        *owner = Some(sender);
        Ok(ControlBinding(Arc::clone(&self.inner.registry_entry)))
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

    pub fn start_command(&self, packet: InboundPacket) -> Result<CommandReceipt> {
        command_input(&packet)?;
        let mut phase = self
            .inner
            .phase
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if phase.closing || phase.command_running {
            return Err("player command admission is closed or busy".into());
        }
        if self
            .inner
            .failure
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
        {
            return Err("player session cleanup has failed".into());
        }
        if !matches!(&*self.inner.joined.borrow(), Some(Ok(()))) {
            return Err("player Join has not completed".into());
        }
        let job = self.inner.state.lifecycle.register("player command")?;
        let resources = self.resources();
        let (reply, receiver) = oneshot::channel();
        let work = CommandWork {
            inner: Arc::clone(&self.inner),
            packet: Some(packet),
            resources: Some(resources),
            reply: Some(reply),
            job: Some(job),
        };
        phase.command_running = true;
        drop(phase);
        self.inner.runtime.spawn(async move {
            let result = crate::managed_tasks::catch_panic(async {
                let command = command_input(work.packet.as_ref().expect("command owns its input"))?;
                work.inner
                    .state
                    .command(
                        CommandSource::Player {
                            uuid: work.inner.uuid,
                            session_id: work.inner.id,
                            name: &work.inner.name,
                        },
                        command,
                    )
                    .await
            })
            .await;
            let (reply, cleanup) = match result {
                Ok(Ok(replies)) => (Ok(Ok(replies)), Ok(())),
                Ok(Err(error)) => {
                    if crate::managed_tasks::is_cleanup_failure(error.as_ref()) {
                        let cleanup = Err(error.to_string());
                        (Err(error), cleanup)
                    } else {
                        (Ok(Err(error.to_string())), Ok(()))
                    }
                }
                Err(error) => {
                    let cleanup = Err(error.to_string());
                    (Err(error.into()), cleanup)
                }
            };
            work.finish(reply, cleanup);
        });
        Ok(CommandReceipt(receiver))
    }
}

impl Drop for ConnectionSession {
    fn drop(&mut self) {
        self.inner.schedule_close();
    }
}

impl SessionInner {
    async fn wait_commands(&self) {
        loop {
            let changed = self.commands_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self
                .phase
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .command_running
            {
                return;
            }
            changed.await;
        }
    }
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
            inner.wait_commands().await;
            let control = inner
                .control
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            if let Some(control) = control {
                control.drain().await;
            }
            let resources = inner
                .resources
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            let inventory_cleanup = match resources {
                Some(resources) => resources
                    .inventory
                    .close_and_drain()
                    .await
                    .map_err(|error| error.to_string()),
                None => Ok(()),
            };
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
            if let Err(failure) = inventory_cleanup {
                cleanup = Err(match cleanup {
                    Ok(()) => failure,
                    Err(error) => format!("{failure}; {error}"),
                });
            }
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

#[cfg(test)]
mod command_tests {
    use super::*;
    use crate::{
        config::{Properties, Settings},
        lifecycle::{DrainError, Lifecycle},
        players::Identity,
    };
    use std::{
        future::Future,
        path::Path,
        task::{Context, Poll, Wake, Waker},
        time::Duration,
    };
    use tokio::sync::{Semaphore, mpsc};
    use zerqon_plugin_api::{
        Action, Event, SenderKind,
        native::{NativeCallbackContext, NativeSource},
    };
    use zerqon_protocol::{
        codec::{put_string, write_frame},
        inbound::{ConnectionBudget, read_frame_budgeted},
    };

    struct Fixture {
        directory: tempfile::TempDir,
        state: Arc<State>,
        input: ConnectionBudget,
        slots: usize,
    }

    impl Fixture {
        async fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            crate::config::initialize(directory.path()).unwrap();
            let lifecycle = Lifecycle::new();
            let state = State::new_with_lifecycle(
                directory.path(),
                Properties::parse("online-mode=false\nmax-players=2\nview-distance=1\n").unwrap(),
                Settings {
                    data_directory: Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join(".cache/vanilla/26.3")
                        .to_string_lossy()
                        .into_owned(),
                    java_plugins: false,
                    spawn_mobs: false,
                    update_check: false,
                    ..Settings::default()
                },
                false,
                None,
                &lifecycle,
                &lifecycle.token(),
            )
            .await
            .unwrap();
            let observer = wat::parse_str(
                r#"(module
                (import "zerqon" "event_len" (func $len (result i32)))
                (import "zerqon" "event_read" (func $read (param i32 i32) (result i32)))
                (import "zerqon" "emit" (func $emit (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (global $quits (mut i32) (i32.const 0))
                (data (i32.const 1024) "{\"action\":\"reply\",\"message\":\"0\"}")
                (data (i32.const 1088) "{\"action\":\"reply\",\"message\":\"1\"}")
                (data (i32.const 1152) "{\"action\":\"reply\",\"message\":\"2\"}")
                (func (export "zerqon_abi_version") (result i32) i32.const 1)
                (func (export "zerqon_on_event") (result i32)
                  i32.const 0 call $len call $read drop
                  i32.const 17 i32.load8_u i32.const 113 i32.eq
                  if global.get $quits i32.const 1 i32.add global.set $quits end
                  i32.const 10 i32.load8_u i32.const 99 i32.eq
                  if global.get $quits i32.eqz
                    if (result i32) i32.const 1024
                    else global.get $quits i32.const 1 i32.eq
                      if (result i32) i32.const 1088 else i32.const 1152 end
                    end i32.const 32 call $emit drop
                  end i32.const 0))"#,
            )
            .unwrap();
            drop(
                state
                    .plugins
                    .load(
                        "command-quit-counter",
                        &observer,
                        &lifecycle.token(),
                        tokio::time::Instant::now() + Duration::from_secs(5),
                    )
                    .await
                    .unwrap(),
            );
            let input = state.packet_budget.connection(65_536).unwrap();
            let slots = state.slots.available_permits();
            Self {
                directory,
                state,
                input,
                slots,
            }
        }

        async fn session(
            &self,
        ) -> (
            ConnectionSession,
            mpsc::Receiver<crate::players::PacketBatch>,
        ) {
            let uuid = Uuid::from_u128(42);
            let (outbound, receiver) = mpsc::channel(32);
            let (disconnect, _closed) = watch::channel(false);
            let player = self
                .state
                .players
                .reserve(
                    Identity {
                        uuid,
                        name: "CONSOLE".into(),
                        properties: Vec::new(),
                    },
                    outbound,
                    disconnect,
                )
                .unwrap();
            let inventory = self
                .state
                .inventories
                .as_ref()
                .unwrap()
                .open(uuid, player.id())
                .await
                .unwrap();
            player.activate().unwrap();
            self.state
                .operators
                .lock()
                .unwrap()
                .grant(uuid, "CONSOLE", 2)
                .unwrap();
            let slot = Arc::clone(&self.state.slots).acquire_owned().await.unwrap();
            (
                ConnectionSession::new(
                    Arc::clone(&self.state),
                    uuid,
                    "CONSOLE".into(),
                    player,
                    inventory,
                    slot,
                )
                .unwrap(),
                receiver,
            )
        }

        async fn packet(&self, command: &str) -> InboundPacket {
            let mut payload = vec![7];
            put_string(command, &mut payload);
            let mut wire = Vec::new();
            write_frame(&mut wire, &payload, None).await.unwrap();
            read_frame_budgeted(&mut wire.as_slice(), 65_536, None, &self.input)
                .await
                .unwrap()
        }

        async fn quit_count(&self) -> String {
            let mut results = self
                .state
                .plugins
                .dispatch(
                    &Event::Command {
                        sender: "CONSOLE".into(),
                        sender_kind: SenderKind::Console,
                        operator: true,
                        command: "count".into(),
                        args: Vec::new(),
                    },
                    &NativeCallbackContext {
                        source: NativeSource::Console,
                    },
                    Some("command-quit-counter"),
                    &self.state.lifecycle.token(),
                    tokio::time::Instant::now() + Duration::from_secs(5),
                )
                .await;
            let output = results.pop().unwrap().result.unwrap();
            match output.actions() {
                [Action::Reply { message }] => message.clone(),
                actions => panic!("counter did not return its observed Quit count: {actions:?}"),
            }
        }

        async fn shutdown(&self) -> std::result::Result<(), DrainError> {
            self.state.lifecycle.stop();
            let disabled = self
                .state
                .plugins
                .shutdown(tokio::time::Instant::now() + Duration::from_secs(5))
                .await;
            assert!(disabled.iter().all(|result| result.result.is_ok()));
            drop(disabled);
            self.state.profiler.shutdown().await.unwrap();
            self.state
                .lifecycle
                .drain(tokio::time::Instant::now() + Duration::from_secs(5))
                .await
        }
    }

    #[tokio::test]
    #[ignore = "requires prepared official Minecraft 26.3 data"]
    async fn cancelled_inventory_caller_settles_before_session_quit() {
        let fixture = Fixture::new().await;
        let (session, _outbound) = fixture.session().await;
        session.join().await.unwrap();
        let resources = session.resources();
        let pending_resources = Arc::clone(&resources);
        let (started, running) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let caller = tokio::spawn(async move {
            pending_resources
                .inventory
                .test_blocked_change(started, released)
                .await
        });
        running.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        session.inner.schedule_close();
        let before_quit = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let quits = fixture.quit_count().await;
                if quits != "0" || resources.inventory.test_admission_closed() {
                    break quits;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let retained_slot = fixture.state.slots.available_permits() == fixture.slots - 1;
        release.send(()).unwrap();
        session.close().await.unwrap();
        drop(resources);
        drop(session);
        fixture.state.sessions.drain().await;
        let after_quit = fixture.quit_count().await;
        fixture.shutdown().await.unwrap();
        let catalog = Arc::clone(fixture.state.items.as_ref().unwrap());
        drop(fixture.state);
        let world = zerqon_world::WorldStore::open(fixture.directory.path().join("world")).unwrap();
        let player = Uuid::from_u128(42);
        let snapshot = world
            .snapshot_domain(&[], &[zerqon_world::PlayerKey::new(player)], &[])
            .unwrap();
        let saved = crate::player_record::PlayerRecord::decode(
            snapshot.players[0].data().unwrap(),
            world.uuid().unwrap(),
            player,
            &catalog,
        )
        .unwrap();
        assert_eq!(
            before_quit, "0",
            "Quit overtook accepted physical inventory work"
        );
        assert!(retained_slot);
        assert_eq!(after_quit, "1");
        assert_eq!(saved.inventory.selected(), 3);
    }

    #[tokio::test]
    #[ignore = "requires prepared official Minecraft 26.3 data"]
    async fn accepted_command_outlives_receipt_and_connection_and_finishes_before_quit() {
        let fixture = Fixture::new().await;
        let (session, _outbound) = fixture.session().await;
        session.join().await.unwrap();
        let stone = fixture.state.data.as_ref().unwrap().flat.stone;
        let blocker = fixture.state.gameplay.write().await;
        let packet = fixture.packet(&format!("setblock 2 64 0 {stone}")).await;
        let retained = fixture.input.snapshot().used_bytes;
        let receipt = session.start_command(packet).unwrap();
        let duplicate = session.start_command(fixture.packet("version").await);
        let duplicate_rejected = duplicate.is_err();
        drop(duplicate);
        drop(receipt);
        drop(session);
        let before_quit = fixture.quit_count().await;
        let pending =
            tokio::time::timeout(Duration::from_millis(20), fixture.state.sessions.drain()).await;
        let registration_retained = fixture
            .state
            .players
            .query_snapshot(Uuid::from_u128(42))
            .unwrap()
            .is_some();
        let slot_retained = fixture.state.slots.available_permits() == fixture.slots - 1;
        let input_retained = fixture.input.snapshot().used_bytes == retained;
        drop(blocker);
        tokio::time::timeout(Duration::from_secs(5), fixture.state.sessions.drain())
            .await
            .unwrap();
        let after_quit = fixture.quit_count().await;
        let released_input = fixture.input.snapshot().used_bytes;
        let released_slots = fixture.state.slots.available_permits();
        let registered = fixture.state.players.count();
        fixture.shutdown().await.unwrap();
        let path = fixture.directory.path().join("world");
        drop(fixture.state);
        let world = zerqon_world::WorldStore::open(path).unwrap();
        let saved = world.load(0, 0).unwrap().unwrap();
        assert!(
            duplicate_rejected,
            "a second simultaneous command was accepted"
        );
        assert!(
            pending.is_err(),
            "session cleanup overtook its accepted command"
        );
        assert_eq!(before_quit, "0", "Quit ran before command completion");
        assert!(registration_retained && slot_retained && input_retained);
        assert_eq!(after_quit, "1");
        assert_eq!(released_input, 0);
        assert_eq!(released_slots, fixture.slots);
        assert_eq!(registered, 0);
        assert_eq!(saved.get_block(2, 64, 0), Some(stone));
    }

    #[tokio::test]
    #[ignore = "requires prepared official Minecraft 26.3 data"]
    async fn command_admission_requires_join_and_rejects_closed_sessions_without_leases() {
        let fixture = Fixture::new().await;
        let (session, _outbound) = fixture.session().await;
        let before_join = session
            .start_command(fixture.packet("version").await)
            .is_err();
        let before_jobs = fixture
            .state
            .lifecycle
            .pending()
            .iter()
            .filter(|label| **label == "player command")
            .count();
        let before_bytes = fixture.input.snapshot().used_bytes;
        session.join().await.unwrap();
        let mut denied_stop = session.start_command(fixture.packet("stop").await).unwrap();
        let replies = denied_stop.wait().await.unwrap().unwrap();
        session.close().await.unwrap();
        let after_close = session
            .start_command(fixture.packet("version").await)
            .is_err();
        drop(session);
        fixture.state.sessions.drain().await;
        let final_bytes = fixture.input.snapshot().used_bytes;
        let stopped = *fixture.state.shutdown.borrow();
        fixture.shutdown().await.unwrap();
        assert!(before_join && after_close);
        assert_eq!(before_jobs, 0);
        assert_eq!(before_bytes, 0);
        assert_eq!(final_bytes, 0);
        assert_eq!(
            replies,
            ["Permission denied"],
            "player named CONSOLE gained console authority"
        );
        assert!(!stopped);
    }

    struct CompletionProbe {
        inner: std::sync::Weak<SessionInner>,
        lifecycle: Lifecycle,
        input: ConnectionBudget,
        observed: Mutex<Option<(bool, bool, usize)>>,
    }
    impl CompletionProbe {
        fn observe(&self) {
            let job_active = self.lifecycle.pending().contains(&"player command");
            let phase_idle = self.inner.upgrade().is_some_and(|inner| {
                inner
                    .phase
                    .try_lock()
                    .is_ok_and(|phase| !phase.command_running)
            });
            *self.observed.lock().unwrap() =
                Some((job_active, phase_idle, self.input.snapshot().used_bytes));
        }
    }
    impl Wake for CompletionProbe {
        fn wake(self: Arc<Self>) {
            self.observe();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.observe();
        }
    }

    #[tokio::test]
    #[ignore = "requires prepared official Minecraft 26.3 data"]
    async fn command_receipt_is_published_only_after_resources_and_admission_are_settled() {
        let fixture = Fixture::new().await;
        let (session, _outbound) = fixture.session().await;
        session.join().await.unwrap();
        let mut receipt = session
            .start_command(fixture.packet("version").await)
            .unwrap();
        let probe = Arc::new(CompletionProbe {
            inner: Arc::downgrade(&session.inner),
            lifecycle: fixture.state.lifecycle.clone(),
            input: fixture.input.clone(),
            observed: Mutex::new(None),
        });
        let waker = Waker::from(Arc::clone(&probe));
        let mut context = Context::from_waker(&waker);
        let mut result = Box::pin(receipt.wait());
        assert!(matches!(result.as_mut().poll(&mut context), Poll::Pending));
        tokio::task::yield_now().await;
        let replies = result.await.unwrap().unwrap();
        let observed = *probe.observed.lock().unwrap();
        session.close().await.unwrap();
        drop(session);
        fixture.shutdown().await.unwrap();
        assert!(replies.iter().any(|reply| reply.contains("Minecraft 26.3")));
        assert_eq!(
            observed,
            Some((false, true, 0)),
            "receipt wake preceded its completion fence"
        );
    }

    #[tokio::test]
    #[ignore = "requires prepared official Minecraft 26.3 data"]
    async fn ordinary_command_errors_preserve_session_and_allow_the_next_command() {
        let fixture = Fixture::new().await;
        let (session, _outbound) = fixture.session().await;
        session.join().await.unwrap();
        let mut invalid = session
            .start_command(fixture.packet("setblock invalid 64 0 1").await)
            .unwrap();
        let failed = invalid.wait().await;
        let mut next = session
            .start_command(fixture.packet("version").await)
            .unwrap();
        let recovered = next.wait().await;
        let closed = session.close().await;
        drop(session);
        let drain = fixture.shutdown().await;
        assert!(
            matches!(failed, Ok(Err(_))),
            "ordinary parse error was not a command error"
        );
        assert!(matches!(recovered, Ok(Ok(_))));
        assert!(closed.is_ok());
        assert!(drain.is_ok());
    }

    #[tokio::test]
    #[ignore = "requires prepared official Minecraft 26.3 data"]
    async fn aborted_command_owner_reports_cleanup_failure_but_still_runs_quit() {
        let fixture = Fixture::new().await;
        let (session, _outbound) = fixture.session().await;
        session.join().await.unwrap();
        let packet = fixture.packet("version").await;
        let (reply, receiver) = oneshot::channel();
        let work = CommandWork {
            inner: Arc::clone(&session.inner),
            packet: Some(packet),
            resources: Some(session.resources()),
            reply: Some(reply),
            job: Some(fixture.state.lifecycle.register("player command").unwrap()),
        };
        session.inner.phase.lock().unwrap().command_running = true;
        let (ready, started) = oneshot::channel();
        let gate = Arc::new(Semaphore::new(0));
        let worker = tokio::spawn(async move {
            let _work = work;
            ready.send(()).unwrap();
            let _permit = gate.acquire().await.unwrap();
        });
        started.await.unwrap();
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        let mut receipt = CommandReceipt(receiver);
        let result = receipt.wait().await;
        let retry = session.start_command(fixture.packet("version").await);
        let retry_rejected = retry.is_err();
        if let Ok(mut retry) = retry {
            let _ = retry.wait().await;
        }
        let closed = session.close().await;
        drop(session);
        let quits = fixture.quit_count().await;
        let input_bytes = fixture.input.snapshot().used_bytes;
        let online = fixture.state.players.count();
        let slots = fixture.state.slots.available_permits();
        let drained = fixture.shutdown().await;
        assert!(
            matches!(result, Err(error) if crate::managed_tasks::is_cleanup_failure(error.as_ref())),
            "abandoned owner became an ordinary command error"
        );
        assert!(
            retry_rejected,
            "session admitted work after failed command cleanup"
        );
        assert!(closed.is_err());
        assert_eq!(quits, "1");
        assert_eq!(input_bytes, 0);
        assert_eq!(online, 0);
        assert_eq!(slots, fixture.slots);
        assert!(matches!(drained, Err(DrainError::Cleanup { .. })));
        assert!(fixture.state.lifecycle.pending().is_empty());
    }
}
