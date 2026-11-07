use crate::{
    lifecycle::{JobGuard, Lifecycle, StopToken},
    managed_tasks,
    native_query_runtime::{
        NativeAdmission, NativeBackend, NativeExecutionError, NativePluginKey, NativeQueryRuntime,
        NativeQueryScope, NativeReplyFormat, NativeScopeContext, NativeScopeGuard,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::File,
    io::{Read, Write},
    mem::size_of,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    time::Instant,
};
use zerqon_plugin_api::{
    Action, Event,
    native::{NativeCallbackContext, NativeSource, READ_CAPABILITIES},
};
use zerqon_plugins::{
    Limits, PluginInfo,
    resumable::{
        CallbackControl, CallbackOutput, CallbackStats, Completion, Invocation, InvocationStep,
        PluginInstance, ResumableHost, StartFailure,
    },
};

static NEXT_ACTOR_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct ActorLimits {
    pub per_plugin_queue: usize,
    pub total_callbacks: usize,
    pub event_bytes: usize,
    pub cpu_jobs: usize,
    pub quantum: u64,
    pub callback_timeout: Duration,
    pub lifecycle_timeout: Duration,
}

impl Default for ActorLimits {
    fn default() -> Self {
        Self {
            per_plugin_queue: 8,
            total_callbacks: 64,
            event_bytes: 4 * 1024 * 1024,
            cpu_jobs: 2,
            quantum: 50_000,
            callback_timeout: Duration::from_secs(1),
            lifecycle_timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum RustPluginError {
    #[error("invalid Rust plugin configuration")]
    Configuration,
    #[error("Rust plugin callback queue or byte budget is full")]
    Busy,
    #[error("Rust plugins are stopping")]
    Stopped,
    #[error("Rust plugin callback deadline exceeded")]
    Deadline,
    #[error("Rust plugin callback cancelled")]
    Cancelled,
    #[error("Rust plugin not found: {0}")]
    NotFound(String),
    #[error("Rust plugin already loaded: {0}")]
    Duplicate(String),
    #[error("Rust plugin: {0}")]
    Plugin(String),
    #[error("Rust plugin file: {0}")]
    Io(String),
    #[error("Rust plugin worker: {0}")]
    Worker(String),
}

impl managed_tasks::OperationError for RustPluginError {
    fn is_cancelled(&self) -> bool {
        matches!(self, Self::Stopped | Self::Cancelled | Self::Deadline)
    }
    fn is_cleanup_failure(&self) -> bool {
        matches!(self, Self::Worker(_))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RustPluginStats {
    pub callbacks: usize,
    pub bytes: usize,
    pub peak_callbacks: usize,
    pub peak_bytes: usize,
    pub rejections: u64,
    pub cpu_jobs: usize,
    pub peak_cpu_jobs: usize,
    pub cleanup_callbacks: usize,
    pub cleanup_bytes: usize,
    pub action_bytes_reserved: usize,
}

struct Admission {
    state: Mutex<RustPluginStats>,
    released: Notify,
    limits: ActorLimits,
    action_bytes: usize,
}

impl Admission {
    fn state(&self) -> MutexGuard<'_, RustPluginStats> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn reserve(
        self: &Arc<Self>,
        bytes: usize,
        cleanup: bool,
    ) -> Result<EventLease, RustPluginError> {
        let mut state = self.state();
        if !cleanup
            && (state.callbacks >= self.limits.total_callbacks
                || bytes > self.limits.event_bytes.saturating_sub(state.bytes))
        {
            state.rejections = state.rejections.saturating_add(1);
            return Err(RustPluginError::Busy);
        }
        if cleanup {
            state.cleanup_callbacks += 1;
            state.cleanup_bytes += bytes;
        } else {
            state.callbacks += 1;
            state.bytes += bytes;
            state.peak_callbacks = state.peak_callbacks.max(state.callbacks);
            state.peak_bytes = state.peak_bytes.max(state.bytes);
        }
        state.action_bytes_reserved += self.action_bytes;
        Ok(EventLease {
            admission: Arc::clone(self),
            bytes,
            cleanup,
        })
    }
}

struct EventLease {
    admission: Arc<Admission>,
    bytes: usize,
    cleanup: bool,
}

impl Drop for EventLease {
    fn drop(&mut self) {
        let mut state = self.admission.state();
        if self.cleanup {
            state.cleanup_callbacks -= 1;
            state.cleanup_bytes -= self.bytes;
        } else {
            state.callbacks -= 1;
            state.bytes -= self.bytes;
        }
        state.action_bytes_reserved -= self.admission.action_bytes;
        drop(state);
        if !self.cleanup {
            self.admission.released.notify_waiters();
        }
    }
}

struct ActorLifetime {
    name: String,
    job: Mutex<Option<JobGuard>>,
    failure: Mutex<Option<String>>,
}

impl ActorLifetime {
    fn fail(&self, reason: String) {
        self.failure
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_or_insert_with(|| format!("{}: {reason}", self.name));
    }
}

impl Drop for ActorLifetime {
    fn drop(&mut self) {
        let reason = self
            .failure
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(job) = self
            .job
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            job.finish(reason.map_or(Ok(()), Err));
        }
    }
}

struct ActorTaskGuard {
    lifetime: Arc<ActorLifetime>,
    completed: bool,
}

impl Drop for ActorTaskGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.lifetime
                .fail("actor task ended without cleanup acknowledgement".into());
        }
    }
}

struct CompiledInstance {
    result: Result<PluginInstance, zerqon_plugins::PluginError>,
    _actor: Arc<ActorLifetime>,
}

struct CallbackLease {
    native: Mutex<Option<NativeScopeGuard>>,
    _event: EventLease,
    _actor: Arc<ActorLifetime>,
}

impl CallbackLease {
    fn release_native(&self) {
        self.native
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}

pub struct RustCallbackOutput {
    output: CallbackOutput,
}

impl RustCallbackOutput {
    pub fn actions(&self) -> &[Action] {
        self.output.actions()
    }
    pub fn stats(&self) -> CallbackStats {
        self.output.stats()
    }
}

impl fmt::Debug for RustCallbackOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RustCallbackOutput")
            .field("actions", &self.actions().len())
            .finish()
    }
}

#[derive(Debug)]
pub struct RustDispatchResult {
    pub name: String,
    pub recipient: Option<RustRecipient>,
    pub delivery: RustDelivery,
    pub result: Result<RustCallbackOutput, RustPluginError>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RustRecipient {
    name: String,
    generation: u64,
}

impl RustRecipient {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RustDelivery {
    Delivered,
    NotAdmitted,
    Faulted,
    Retired,
}

#[derive(Debug)]
pub struct RustLoadResult {
    pub path: PathBuf,
    pub result: Result<RustCallbackOutput, RustPluginError>,
}

struct Services {
    host: ResumableHost,
    queries: NativeQueryRuntime,
    lifecycle: Lifecycle,
    limits: Limits,
    actor_limits: ActorLimits,
    admission: Arc<Admission>,
    cpu: Arc<Semaphore>,
}

struct ActorControl {
    generation: u64,
    info: Mutex<PluginInfo>,
    stopping: AtomicBool,
    done: AtomicBool,
    abandoned: AtomicBool,
    changed: Notify,
    exit: Mutex<Option<Result<Option<RustCallbackOutput>, RustPluginError>>>,
    cleanup: Mutex<Option<EventLease>>,
}

impl ActorControl {
    fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }
    async fn stopping(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.stopping.load(Ordering::Acquire) {
                return;
            }
            changed.await;
        }
    }
    fn set_enabled(&self, enabled: bool, reason: Option<String>) {
        let mut info = self.info.lock().unwrap_or_else(|error| error.into_inner());
        info.enabled = enabled;
        info.disabled_reason = reason;
    }
    fn complete(&self, result: Result<Option<RustCallbackOutput>, RustPluginError>) {
        self.cleanup
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        {
            let mut info = self.info.lock().unwrap_or_else(|error| error.into_inner());
            info.enabled = false;
            if let Err(error) = &result {
                info.disabled_reason = Some(error.to_string());
            }
        }
        *self.exit.lock().unwrap_or_else(|error| error.into_inner()) = Some(result);
        self.done.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }
    async fn wait(&self, deadline: Instant) -> Result<Option<RustCallbackOutput>, RustPluginError> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.done.load(Ordering::Acquire) {
                return self
                    .exit
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take()
                    .unwrap_or(Ok(None));
            }
            tokio::time::timeout_at(deadline, changed)
                .await
                .map_err(|_| RustPluginError::Deadline)?;
        }
    }
}

#[derive(Clone)]
struct ActorHandle {
    sender: mpsc::Sender<Request>,
    control: Arc<ActorControl>,
}

struct Service {
    actors: Mutex<BTreeMap<String, ActorHandle>>,
    services: Arc<Services>,
    loading: tokio::sync::Mutex<()>,
    stopping: AtomicBool,
    next_parent: AtomicU64,
}

impl Drop for Service {
    fn drop(&mut self) {
        for actor in self
            .actors
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .values()
        {
            actor.control.stop();
        }
    }
}

#[derive(Clone)]
pub struct RustPlugins {
    inner: Arc<Service>,
}

struct Request {
    event: Event,
    context: NativeCallbackContext,
    parent_id: u64,
    deadline: Instant,
    parent: StopToken,
    cleanup: bool,
    critical: bool,
    cancelled: Arc<AtomicBool>,
    wake: Arc<Notify>,
    lease: EventLease,
    reply: oneshot::Sender<Result<RustCallbackOutput, RustPluginError>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DispatchMode {
    Ordinary,
    AdmittedJoin,
    TargetedQuit,
}

impl DispatchMode {
    fn critical(self) -> bool {
        self != Self::Ordinary
    }
    fn admitted_join(self) -> bool {
        self == Self::AdmittedJoin
    }
}

struct DispatchInput<'a> {
    event: &'a Event,
    context: &'a NativeCallbackContext,
    parent: &'a StopToken,
    deadline: Instant,
    mode: DispatchMode,
}

struct PendingReply {
    receiver: oneshot::Receiver<Result<RustCallbackOutput, RustPluginError>>,
    cancelled: Arc<AtomicBool>,
    wake: Arc<Notify>,
    parent: StopToken,
    deadline: Instant,
    armed: bool,
}

impl PendingReply {
    async fn wait(mut self) -> Result<RustCallbackOutput, RustPluginError> {
        let result = tokio::select! {
            biased;
            result = &mut self.receiver => result.map_err(|_| RustPluginError::Stopped)?,
            _ = self.parent.cancelled() => return Err(RustPluginError::Cancelled),
            _ = tokio::time::sleep_until(self.deadline) => return Err(RustPluginError::Deadline),
        };
        if self.parent.is_stopped() {
            return Err(RustPluginError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(RustPluginError::Deadline);
        }
        self.armed = false;
        result
    }

    async fn settle(mut self) -> Result<RustCallbackOutput, RustPluginError> {
        let completed = tokio::select! {
            biased;
            result = &mut self.receiver => Some(result),
            _ = self.parent.cancelled() => None,
            _ = tokio::time::sleep_until(self.deadline) => None,
        };
        let result = match completed {
            Some(result) => result,
            None => {
                self.cancelled.store(true, Ordering::Release);
                self.wake.notify_waiters();
                (&mut self.receiver).await
            }
        };
        self.armed = false;
        result.map_err(|_| RustPluginError::Stopped)?
    }
}

impl Drop for PendingReply {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
            self.wake.notify_waiters();
        }
    }
}

struct CancelLoad {
    control: Arc<ActorControl>,
    owner: Weak<Service>,
    name: String,
    armed: bool,
}
impl Drop for CancelLoad {
    fn drop(&mut self) {
        if self.armed {
            self.control.abandoned.store(true, Ordering::Release);
            self.control.stop();
            remove_abandoned(&self.owner, &self.name, &self.control);
        }
    }
}

impl RustPlugins {
    pub fn new(
        limits: Limits,
        queries: NativeQueryRuntime,
        lifecycle: Lifecycle,
        actor_limits: ActorLimits,
    ) -> Result<Self, RustPluginError> {
        if !(1..=64).contains(&actor_limits.per_plugin_queue)
            || !(1..=256).contains(&actor_limits.total_callbacks)
            || actor_limits.event_bytes == 0
            || actor_limits.event_bytes > 64 * 1024 * 1024
            || !(1..=64).contains(&actor_limits.cpu_jobs)
            || actor_limits.callback_timeout.is_zero()
            || actor_limits.lifecycle_timeout.is_zero()
        {
            return Err(RustPluginError::Configuration);
        }
        let host =
            ResumableHost::new(limits.clone(), actor_limits.quantum).map_err(plugin_error)?;
        let admission = Arc::new(Admission {
            state: Mutex::new(RustPluginStats::default()),
            released: Notify::new(),
            limits: actor_limits.clone(),
            action_bytes: limits.output_bytes * 2
                + limits.actions_per_event * size_of::<Action>() * 2,
        });
        let services = Arc::new(Services {
            host,
            queries,
            lifecycle,
            limits,
            cpu: Arc::new(Semaphore::new(actor_limits.cpu_jobs)),
            actor_limits,
            admission,
        });
        Ok(Self {
            inner: Arc::new(Service {
                actors: Mutex::new(BTreeMap::new()),
                services,
                loading: tokio::sync::Mutex::new(()),
                stopping: AtomicBool::new(false),
                next_parent: AtomicU64::new(1),
            }),
        })
    }

    pub fn stats(&self) -> RustPluginStats {
        *self.inner.services.admission.state()
    }

    pub fn list(&self) -> Vec<PluginInfo> {
        self.inner
            .actors
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .map(|actor| {
                actor
                    .control
                    .info
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone()
            })
            .collect()
    }

    pub async fn load(
        &self,
        name: &str,
        bytes: &[u8],
        parent: &StopToken,
        deadline: Instant,
    ) -> Result<RustCallbackOutput, RustPluginError> {
        let _loading = tokio::select! {
            result = tokio::time::timeout_at(deadline, self.inner.loading.lock()) => result.map_err(|_| RustPluginError::Deadline)?,
            _ = parent.cancelled() => return Err(RustPluginError::Cancelled),
        };
        if self.inner.stopping.load(Ordering::Acquire)
            || self.inner.services.lifecycle.token().is_stopped()
        {
            return Err(RustPluginError::Stopped);
        }
        if parent.is_stopped() {
            return Err(RustPluginError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(RustPluginError::Deadline);
        }
        if !(1..=64).contains(&name.len())
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        {
            return Err(RustPluginError::Plugin("invalid plugin name".into()));
        }
        if bytes.len() > self.inner.services.limits.module_bytes {
            return Err(RustPluginError::Plugin(
                "plugin module exceeds size limit".into(),
            ));
        }
        {
            let actors = self
                .inner
                .actors
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if actors.contains_key(name) {
                return Err(RustPluginError::Duplicate(name.into()));
            }
            if actors.len() >= self.inner.services.limits.plugins {
                return Err(RustPluginError::Busy);
            }
        }
        let generation = next_id(&NEXT_ACTOR_GENERATION)?;
        let parent_id = next_id(&self.inner.next_parent)?;
        let job = self
            .inner
            .services
            .lifecycle
            .register("Rust plugin actor")
            .map_err(|_| RustPluginError::Stopped)?;
        let lifetime = Arc::new(ActorLifetime {
            name: name.into(),
            job: Mutex::new(Some(job)),
            failure: Mutex::new(None),
        });
        let cleanup = self.inner.services.admission.reserve(
            event_bytes(
                &Event::Disable,
                &server_context(),
                &self.inner.services.limits,
            )?,
            true,
        )?;
        let enable = Event::Enable {
            server_version: env!("CARGO_PKG_VERSION").into(),
        };
        let enable_lease = self.inner.services.admission.reserve(
            event_bytes(&enable, &server_context(), &self.inner.services.limits)?,
            false,
        )?;
        let control = Arc::new(ActorControl {
            generation,
            info: Mutex::new(PluginInfo {
                name: name.into(),
                enabled: false,
                disabled_reason: Some("loading".into()),
            }),
            stopping: AtomicBool::new(false),
            done: AtomicBool::new(false),
            abandoned: AtomicBool::new(false),
            changed: Notify::new(),
            exit: Mutex::new(None),
            cleanup: Mutex::new(Some(cleanup)),
        });
        let (sender, receiver) = mpsc::channel(self.inner.services.actor_limits.per_plugin_queue);
        {
            let mut actors = self
                .inner
                .actors
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if self.inner.stopping.load(Ordering::Acquire) {
                return Err(RustPluginError::Stopped);
            }
            actors.insert(
                name.into(),
                ActorHandle {
                    sender,
                    control: Arc::clone(&control),
                },
            );
        }
        let (ready, startup) = oneshot::channel();
        let services = Arc::clone(&self.inner.services);
        let actor_control = Arc::clone(&control);
        let task_lifetime = Arc::clone(&lifetime);
        let owner = Arc::downgrade(&self.inner);
        let actor_name = name.to_owned();
        let initial = Initial {
            name: name.into(),
            bytes: bytes.to_vec(),
            event: enable,
            lease: enable_lease,
            parent: parent.clone(),
            parent_id,
            deadline: deadline.min(Instant::now() + services.actor_limits.lifecycle_timeout),
            ready,
        };
        tokio::spawn(async move {
            let mut task_guard = ActorTaskGuard {
                lifetime: Arc::clone(&task_lifetime),
                completed: false,
            };
            let result = managed_tasks::catch_panic(actor_loop(
                services,
                Arc::clone(&actor_control),
                Arc::clone(&task_lifetime),
                receiver,
                initial,
            ))
            .await;
            match result {
                Ok(result) => actor_control.complete(result),
                Err(error) => {
                    task_lifetime.fail(error.to_string());
                    actor_control.complete(Err(RustPluginError::Worker(error.to_string())));
                }
            }
            remove_abandoned(&owner, &actor_name, &actor_control);
            task_guard.completed = true;
        });
        let mut cancel = CancelLoad {
            control: Arc::clone(&control),
            owner: Arc::downgrade(&self.inner),
            name: name.into(),
            armed: true,
        };
        let result = tokio::select! {
            result = startup => result.map_err(|_| RustPluginError::Stopped)?,
            _ = parent.cancelled() => return Err(RustPluginError::Cancelled),
            _ = tokio::time::sleep_until(deadline) => return Err(RustPluginError::Deadline),
        };
        if parent.is_stopped() {
            return Err(RustPluginError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(RustPluginError::Deadline);
        }
        if result.is_err() {
            control.stop();
            let _ = control.wait(deadline).await;
            if control.done.load(Ordering::Acquire) {
                let mut actors = self
                    .inner
                    .actors
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if actors
                    .get(name)
                    .is_some_and(|actor| actor.control.generation == generation)
                {
                    actors.remove(name);
                }
            }
        }
        cancel.armed = false;
        result
    }

    pub async fn load_directory(
        &self,
        directory: &Path,
        parent: &StopToken,
        deadline: Instant,
    ) -> Result<Vec<RustLoadResult>, RustPluginError> {
        let path = directory.to_owned();
        let maximum = self.inner.services.limits.plugins;
        let files = file_job(&self.inner.services, parent, deadline, move |stop| {
            stop.check().map_err(|_| RustPluginError::Cancelled)?;
            std::fs::create_dir_all(&path).map_err(io_error)?;
            let mut files = Vec::new();
            for entry in std::fs::read_dir(path).map_err(io_error)? {
                stop.check().map_err(|_| RustPluginError::Cancelled)?;
                let entry = entry.map_err(io_error)?;
                if entry.file_type().map_err(io_error)?.is_file()
                    && entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "wasm")
                {
                    files.push(entry.path());
                }
                if files.len() > maximum {
                    return Err(RustPluginError::Busy);
                }
            }
            files.sort();
            Ok(files)
        })
        .await?;
        let mut output = Vec::new();
        for path in files {
            if parent.is_stopped() {
                return Err(RustPluginError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(RustPluginError::Deadline);
            }
            let name = path
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("")
                .to_owned();
            let source = path.clone();
            let limit = self.inner.services.limits.module_bytes;
            let bytes = file_job(&self.inner.services, parent, deadline, move |stop| {
                stop.check().map_err(|_| RustPluginError::Cancelled)?;
                let file = File::open(source).map_err(io_error)?;
                if file.metadata().map_err(io_error)?.len() > limit as u64 {
                    return Err(RustPluginError::Plugin(
                        "plugin module exceeds size limit".into(),
                    ));
                }
                let mut bytes = Vec::new();
                file.take(limit as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(io_error)?;
                stop.check().map_err(|_| RustPluginError::Cancelled)?;
                Ok(bytes)
            })
            .await;
            let result = match bytes {
                Ok(bytes) => self.load(&name, &bytes, parent, deadline).await,
                Err(error) => Err(error),
            };
            output.push(RustLoadResult { path, result });
        }
        Ok(output)
    }

    pub fn recipients(&self) -> Vec<RustRecipient> {
        self.inner
            .actors
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|(_, actor)| {
                actor
                    .control
                    .info
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .enabled
            })
            .map(|(name, actor)| recipient(name, actor))
            .collect()
    }

    pub async fn dispatch(
        &self,
        event: &Event,
        context: &NativeCallbackContext,
        target: Option<&str>,
        parent: &StopToken,
        deadline: Instant,
    ) -> Vec<RustDispatchResult> {
        let targets: Vec<_> = {
            let actors = self
                .inner
                .actors
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            match target {
                Some(name) => match actors.get(name) {
                    Some(actor) => vec![(recipient(name, actor), Some(actor.clone()))],
                    None => {
                        return vec![RustDispatchResult {
                            name: name.into(),
                            recipient: None,
                            delivery: RustDelivery::Retired,
                            result: Err(RustPluginError::NotFound(name.into())),
                        }];
                    }
                },
                None => actors
                    .iter()
                    .filter(|(_, actor)| {
                        actor
                            .control
                            .info
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .enabled
                    })
                    .map(|(name, actor)| (recipient(name, actor), Some(actor.clone())))
                    .collect(),
            }
        };
        self.dispatch_selected(
            event,
            context,
            targets,
            parent,
            deadline,
            DispatchMode::Ordinary,
        )
        .await
    }

    /// Targeted Quit accepts at most one receipt and waits for bounded admission.
    /// Apply and drop its output before dispatching the next lifecycle recipient.
    pub async fn dispatch_to_recipients(
        &self,
        event: &Event,
        context: &NativeCallbackContext,
        recipients: &[RustRecipient],
        parent: &StopToken,
        deadline: Instant,
    ) -> Result<Vec<RustDispatchResult>, RustPluginError> {
        let mode = if matches!(event, Event::PlayerQuit { .. }) {
            if recipients.len() > 1 {
                return Err(RustPluginError::Configuration);
            }
            DispatchMode::TargetedQuit
        } else {
            DispatchMode::Ordinary
        };
        let targets = self.select_recipients(recipients)?;
        Ok(self
            .dispatch_selected(event, context, targets, parent, deadline, mode)
            .await)
    }

    /// Continues an admitted, session-owned Join for at most one recipient.
    /// Retain the transaction through settlement, then apply and drop its output
    /// before advancing. Queue waits use its deadline; execution is capped at dequeue.
    pub async fn dispatch_admitted_join(
        &self,
        event: &Event,
        context: &NativeCallbackContext,
        recipients: &[RustRecipient],
        parent: &StopToken,
        deadline: Instant,
    ) -> Result<Vec<RustDispatchResult>, RustPluginError> {
        if recipients.len() > 1 {
            return Err(RustPluginError::Configuration);
        }
        if !matches!(event, Event::PlayerJoin { .. })
            || !context.valid()
            || !context.matches_event(event)
        {
            return Err(RustPluginError::Plugin(
                "admitted Join requires matching player context".into(),
            ));
        }
        let targets = self.select_recipients(recipients)?;
        Ok(self
            .dispatch_selected(
                event,
                context,
                targets,
                parent,
                deadline,
                DispatchMode::AdmittedJoin,
            )
            .await)
    }

    fn select_recipients(
        &self,
        recipients: &[RustRecipient],
    ) -> Result<Vec<(RustRecipient, Option<ActorHandle>)>, RustPluginError> {
        if recipients.len() > self.inner.services.limits.plugins
            || recipients.iter().collect::<BTreeSet<_>>().len() != recipients.len()
        {
            return Err(RustPluginError::Configuration);
        }
        let actors = self
            .inner
            .actors
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Ok(recipients
            .iter()
            .map(|id| {
                let actor = actors
                    .get(id.name())
                    .filter(|actor| actor.control.generation == id.generation())
                    .cloned();
                (id.clone(), actor)
            })
            .collect())
    }

    async fn dispatch_selected(
        &self,
        event: &Event,
        context: &NativeCallbackContext,
        targets: Vec<(RustRecipient, Option<ActorHandle>)>,
        parent: &StopToken,
        deadline: Instant,
        mode: DispatchMode,
    ) -> Vec<RustDispatchResult> {
        let mut pending = Vec::with_capacity(targets.len());
        for (id, actor) in targets {
            let accepted = match &actor {
                Some(actor) => {
                    let input = DispatchInput {
                        event,
                        context,
                        parent,
                        deadline,
                        mode,
                    };
                    if mode.critical() {
                        self.enqueue_critical(actor, &input).await
                    } else {
                        self.enqueue(actor, &input)
                    }
                }
                None => Err(RustPluginError::NotFound(id.name().into())),
            };
            pending.push((id, actor, accepted));
        }
        let mut output = Vec::with_capacity(pending.len());
        for (id, actor, accepted) in pending {
            let result = match accepted {
                Ok(reply) if mode.critical() => reply.settle().await,
                Ok(reply) => reply.wait().await,
                Err(error) => Err(error),
            };
            let delivery = delivery(&result, actor.as_ref());
            output.push(RustDispatchResult {
                name: id.name().into(),
                recipient: Some(id),
                delivery,
                result,
            });
        }
        output
    }

    fn check_admission(
        &self,
        actor: &ActorHandle,
        input: &DispatchInput<'_>,
    ) -> Result<(), RustPluginError> {
        let cleanup = matches!(input.event, Event::PlayerQuit { .. });
        if matches!(input.event, Event::Enable { .. } | Event::Disable) {
            return Err(RustPluginError::Plugin(
                "use load or shutdown for lifecycle events".into(),
            ));
        }
        if self.inner.stopping.load(Ordering::Acquire)
            || actor.control.stopping.load(Ordering::Acquire)
        {
            return Err(RustPluginError::Stopped);
        }
        if !cleanup
            && !input.mode.admitted_join()
            && self.inner.services.lifecycle.token().is_stopped()
        {
            return Err(RustPluginError::Stopped);
        }
        if input.parent.is_stopped() {
            return Err(RustPluginError::Cancelled);
        }
        if Instant::now() >= input.deadline {
            return Err(RustPluginError::Deadline);
        }
        if !actor
            .control
            .info
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .enabled
        {
            return Err(RustPluginError::Plugin("plugin is disabled".into()));
        }
        Ok(())
    }

    fn enqueue(
        &self,
        actor: &ActorHandle,
        input: &DispatchInput<'_>,
    ) -> Result<PendingReply, RustPluginError> {
        self.check_admission(actor, input)?;
        let bytes = event_bytes(input.event, input.context, &self.inner.services.limits)?;
        let permit = actor.sender.clone().try_reserve_owned().map_err(|_| {
            let mut state = self.inner.services.admission.state();
            state.rejections = state.rejections.saturating_add(1);
            RustPluginError::Busy
        })?;
        let lease = self.inner.services.admission.reserve(bytes, false)?;
        self.publish(input, permit, lease)
    }

    async fn enqueue_critical(
        &self,
        actor: &ActorHandle,
        input: &DispatchInput<'_>,
    ) -> Result<PendingReply, RustPluginError> {
        self.check_admission(actor, input)?;
        let bytes = event_bytes(input.event, input.context, &self.inner.services.limits)?;
        if bytes > self.inner.services.actor_limits.event_bytes {
            let mut state = self.inner.services.admission.state();
            state.rejections = state.rejections.saturating_add(1);
            return Err(RustPluginError::Busy);
        }
        loop {
            let released = self.inner.services.admission.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            self.check_admission(actor, input)?;
            let permit = tokio::select! {
                biased;
                _ = input.parent.cancelled() => return Err(RustPluginError::Cancelled),
                _ = actor.control.stopping() => return Err(RustPluginError::Stopped),
                _ = tokio::time::sleep_until(input.deadline) => return Err(RustPluginError::Deadline),
                permit = actor.sender.clone().reserve_owned() => permit.map_err(|_| RustPluginError::Stopped)?,
            };
            self.check_admission(actor, input)?;
            match self.inner.services.admission.reserve(bytes, false) {
                Ok(lease) => return self.publish(input, permit, lease),
                Err(RustPluginError::Busy) => drop(permit),
                Err(error) => return Err(error),
            }
            tokio::select! {
                biased;
                _ = input.parent.cancelled() => return Err(RustPluginError::Cancelled),
                _ = actor.control.stopping() => return Err(RustPluginError::Stopped),
                _ = tokio::time::sleep_until(input.deadline) => return Err(RustPluginError::Deadline),
                _ = released => {},
            }
        }
    }

    fn publish(
        &self,
        input: &DispatchInput<'_>,
        permit: mpsc::OwnedPermit<Request>,
        lease: EventLease,
    ) -> Result<PendingReply, RustPluginError> {
        let cleanup = matches!(input.event, Event::PlayerQuit { .. });
        let parent_id = next_id(&self.inner.next_parent)?;
        let deadline = if input.mode.critical() {
            input.deadline
        } else {
            input
                .deadline
                .min(Instant::now() + self.inner.services.actor_limits.callback_timeout)
        };
        let parent = if cleanup {
            input.parent.child()
        } else {
            input.parent.admitted_child()
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Notify::new());
        let (reply, receiver) = oneshot::channel();
        permit.send(Request {
            event: input.event.clone(),
            context: input.context.clone(),
            parent_id,
            deadline,
            parent: parent.clone(),
            cleanup,
            critical: input.mode.critical(),
            cancelled: Arc::clone(&cancelled),
            wake: Arc::clone(&wake),
            lease,
            reply,
        });
        Ok(PendingReply {
            receiver,
            cancelled,
            wake,
            parent,
            deadline,
            armed: true,
        })
    }

    pub async fn unload(
        &self,
        name: &str,
        deadline: Instant,
    ) -> Result<Option<RustCallbackOutput>, RustPluginError> {
        let actor = self
            .inner
            .actors
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(name)
            .cloned()
            .ok_or_else(|| RustPluginError::NotFound(name.into()))?;
        actor.control.stop();
        let result = actor.control.wait(deadline).await;
        if actor.control.done.load(Ordering::Acquire) {
            let mut actors = self
                .inner
                .actors
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if actors
                .get(name)
                .is_some_and(|current| current.control.generation == actor.control.generation)
            {
                actors.remove(name);
            }
        }
        result
    }

    pub async fn shutdown(&self, deadline: Instant) -> Vec<RustDispatchResult> {
        self.inner.stopping.store(true, Ordering::Release);
        let actors: Vec<_> = self
            .inner
            .actors
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .map(|(name, actor)| (name.clone(), actor.clone()))
            .collect();
        for (_, actor) in &actors {
            actor.control.stop();
        }
        let mut results = Vec::new();
        for (name, actor) in actors {
            let recipient = Some(recipient(&name, &actor));
            match self.unload(&name, deadline).await {
                Ok(Some(output)) => results.push(RustDispatchResult {
                    name,
                    recipient,
                    delivery: RustDelivery::Delivered,
                    result: Ok(output),
                }),
                Ok(None) => {}
                Err(error) => results.push(RustDispatchResult {
                    name,
                    recipient,
                    delivery: RustDelivery::Faulted,
                    result: Err(error),
                }),
            }
        }
        results
    }
}

fn recipient(name: &str, actor: &ActorHandle) -> RustRecipient {
    RustRecipient {
        name: name.into(),
        generation: actor.control.generation,
    }
}

fn delivery(
    result: &Result<RustCallbackOutput, RustPluginError>,
    actor: Option<&ActorHandle>,
) -> RustDelivery {
    if result.is_ok() {
        return RustDelivery::Delivered;
    }
    let Some(actor) = actor else {
        return RustDelivery::Retired;
    };
    if actor.control.stopping.load(Ordering::Acquire) {
        return RustDelivery::Retired;
    }
    if !actor
        .control
        .info
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .enabled
    {
        return RustDelivery::Faulted;
    }
    RustDelivery::NotAdmitted
}

struct Initial {
    name: String,
    bytes: Vec<u8>,
    event: Event,
    lease: EventLease,
    parent: StopToken,
    parent_id: u64,
    deadline: Instant,
    ready: oneshot::Sender<Result<RustCallbackOutput, RustPluginError>>,
}

struct CallbackRequest {
    event: Event,
    context: NativeCallbackContext,
    parent_id: u64,
    deadline: Instant,
    parent: StopToken,
    cleanup: bool,
    cancelled: Arc<AtomicBool>,
    wake: Arc<Notify>,
    lease: EventLease,
}

struct Driven {
    instance: Option<PluginInstance>,
    result: Result<RustCallbackOutput, RustPluginError>,
    retirement: Option<Result<RustCallbackOutput, RustPluginError>>,
}

#[derive(Clone, Copy)]
struct ExecutionContext<'a> {
    parent_id: u64,
    deadline: Instant,
    parent: &'a StopToken,
    cleanup: bool,
    startup: bool,
    cancelled: &'a AtomicBool,
    wake: &'a Notify,
}

struct BeforeBegin {
    parent: StopToken,
    actor: Arc<ActorControl>,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
    cleanup: bool,
}

impl BeforeBegin {
    fn check(&self) -> Result<(), zerqon_plugins::PluginError> {
        if self.parent.is_stopped()
            || self.cancelled.load(Ordering::Acquire)
            || (!self.cleanup && self.actor.stopping.load(Ordering::Acquire))
        {
            return Err(zerqon_plugins::PluginError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(zerqon_plugins::PluginError::Deadline);
        }
        Ok(())
    }
}

struct StartWork {
    event: Event,
    context: NativeCallbackContext,
    instance: PluginInstance,
    control: CallbackControl,
    before_begin: BeforeBegin,
    _permit: CpuPermit,
    lease: Arc<CallbackLease>,
}

impl StartWork {
    fn run(self) -> Result<Invocation, Box<StartFailure>> {
        if let Err(error) = self.before_begin.check() {
            return Err(Box::new(StartFailure {
                instance: self.instance,
                error,
            }));
        }
        let prepared = match self.instance.prepare(&self.event, &self.context) {
            Ok(prepared) => prepared,
            Err(error) => {
                return Err(Box::new(StartFailure {
                    instance: self.instance,
                    error,
                }));
            }
        };
        if let Err(error) = self.before_begin.check() {
            return Err(Box::new(StartFailure {
                instance: self.instance,
                error,
            }));
        }
        self.instance
            .begin(prepared, self.control, Box::new(self.lease))
    }
}

async fn actor_loop(
    services: Arc<Services>,
    control: Arc<ActorControl>,
    lifetime: Arc<ActorLifetime>,
    mut incoming: mpsc::Receiver<Request>,
    initial: Initial,
) -> Result<Option<RustCallbackOutput>, RustPluginError> {
    let Initial {
        name,
        bytes,
        event,
        lease,
        parent,
        parent_id,
        deadline,
        ready,
    } = initial;
    let host = services.host.clone();
    let plugin_name = name.clone();
    let compile_lifetime = Arc::clone(&lifetime);
    let instance = match cpu_job(&services, deadline, move || CompiledInstance {
        result: host.instantiate(&plugin_name, &bytes, READ_CAPABILITIES),
        _actor: compile_lifetime,
    })
    .await
    {
        Ok(CompiledInstance {
            result: Ok(instance),
            ..
        }) => instance,
        Ok(CompiledInstance {
            result: Err(error), ..
        }) => {
            let error = plugin_error(error);
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
        Err(error) => {
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };
    let request = CallbackRequest {
        event,
        context: server_context(),
        parent_id,
        deadline,
        parent,
        cleanup: false,
        cancelled: Arc::new(AtomicBool::new(false)),
        wake: Arc::new(Notify::new()),
        lease,
    };
    let driven = drive(&services, &control, &lifetime, instance, request).await;
    let mut instance = driven.instance;
    let enabled = driven.result.is_ok();
    control.set_enabled(
        enabled,
        driven.result.as_ref().err().map(ToString::to_string),
    );
    if let Err(result) = ready.send(driven.result) {
        control.stop();
        drop(result);
    }
    if let Some(retired) = driven.retirement {
        return retired.map(Some);
    }
    if !enabled {
        control.stop();
    }
    while !control.stopping.load(Ordering::Acquire) && instance.is_some() {
        let request = tokio::select! {
            biased;
            _ = control.stopping() => break,
            request = incoming.recv() => match request { Some(request) => request, None => break },
        };
        if request.reply.is_closed()
            || request.cancelled.load(Ordering::Acquire)
            || request.parent.is_stopped()
            || Instant::now() >= request.deadline
        {
            let _ = request.reply.send(Err(RustPluginError::Cancelled));
            continue;
        }
        let Request {
            event,
            context,
            parent_id,
            deadline,
            parent,
            cleanup,
            critical,
            cancelled,
            wake,
            lease,
            reply,
        } = request;
        let deadline = if critical {
            deadline.min(Instant::now() + services.actor_limits.callback_timeout)
        } else {
            deadline
        };
        let current = instance.take().expect("actor owns a ready instance");
        let request = CallbackRequest {
            event,
            context,
            parent_id,
            deadline,
            parent,
            cleanup,
            cancelled,
            wake,
            lease,
        };
        let driven = drive(&services, &control, &lifetime, current, request).await;
        instance = driven.instance;
        if instance.is_none() {
            control.set_enabled(false, driven.result.as_ref().err().map(ToString::to_string));
        }
        if let Err(output) = reply.send(driven.result) {
            drop(output);
        }
        if let Some(retired) = driven.retirement {
            return retired.map(Some);
        }
    }
    incoming.close();
    while let Ok(request) = incoming.try_recv() {
        let _ = request.reply.send(Err(RustPluginError::Stopped));
    }
    let Some(instance) = instance else {
        return Ok(None);
    };
    if !instance.is_enabled() {
        return Ok(None);
    }
    let request = CallbackRequest {
        event: Event::Disable,
        context: server_context(),
        parent_id: control.generation,
        deadline: Instant::now() + services.actor_limits.lifecycle_timeout,
        parent: services.lifecycle.cleanup_token(),
        cleanup: true,
        cancelled: Arc::new(AtomicBool::new(false)),
        wake: Arc::new(Notify::new()),
        lease: control
            .cleanup
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .expect("actor retains its cleanup reservation"),
    };
    let driven = drive(&services, &control, &lifetime, instance, request).await;
    drop(driven.instance);
    match driven.result {
        Ok(output) => Ok(Some(output)),
        Err(error) => {
            lifetime.fail(error.to_string());
            Err(error)
        }
    }
}

async fn drive(
    services: &Arc<Services>,
    actor: &Arc<ActorControl>,
    lifetime: &Arc<ActorLifetime>,
    instance: PluginInstance,
    request: CallbackRequest,
) -> Driven {
    let CallbackRequest {
        event,
        context,
        parent_id,
        deadline,
        parent,
        cleanup,
        cancelled,
        wake,
        lease,
    } = request;
    let native_reads = instance.uses_native_reads();
    let name = instance.name().to_owned();
    let execution = ExecutionContext {
        parent_id,
        deadline,
        parent: &parent,
        cleanup,
        startup: matches!(event, Event::Enable { .. }),
        cancelled: &cancelled,
        wake: &wake,
    };
    let native = match native_scope(services, actor, &name, native_reads, &execution).await {
        Ok(scope) => scope,
        Err(error) => {
            drop(event);
            return Driven {
                instance: Some(instance),
                result: Err(error),
                retirement: None,
            };
        }
    };
    let lease = Arc::new(CallbackLease {
        native: Mutex::new(native.as_ref().map(NativeQueryScope::hold)),
        _event: lease,
        _actor: Arc::clone(lifetime),
    });
    let disabling = matches!(event, Event::Disable);
    let control = CallbackControl::new(deadline.into_std());
    let admitted = tokio::select! {
        biased;
        _ = cancellation(&parent, actor, &cancelled, &wake, cleanup) => Err(RustPluginError::Cancelled),
        result = cpu_permit(services, deadline) => result,
    };
    let permit = match admitted {
        Ok(permit) => permit,
        Err(error) => {
            drop(event);
            return Driven {
                instance: Some(instance),
                result: Err(error),
                retirement: None,
            };
        }
    };
    let before_begin = BeforeBegin {
        parent: parent.clone(),
        actor: Arc::clone(actor),
        cancelled: Arc::clone(&cancelled),
        deadline,
        cleanup,
    };
    let work = StartWork {
        event,
        context,
        instance,
        control: control.clone(),
        before_begin,
        _permit: permit,
        lease: Arc::clone(&lease),
    };
    let start = tokio::task::spawn_blocking(move || work.run()).await;
    let invocation = match start {
        Ok(Ok(invocation)) => invocation,
        Ok(Err(failure)) => {
            return Driven {
                instance: Some(failure.instance),
                result: Err(plugin_error(failure.error)),
                retirement: None,
            };
        }
        Err(error) => {
            lifetime.fail(error.to_string());
            return Driven {
                instance: None,
                result: Err(RustPluginError::Worker(error.to_string())),
                retirement: None,
            };
        }
    };
    let finish = run_invocation(
        services,
        actor,
        invocation,
        &control,
        native.as_ref(),
        &execution,
    )
    .await;
    lease.release_native();
    drop(native);
    let done = match finish {
        Ok(done) => done,
        Err(error) => {
            return Driven {
                instance: None,
                result: Err(error),
                retirement: None,
            };
        }
    };
    let cancelled = control.is_cancelled()
        || cancelled.load(Ordering::Acquire)
        || parent.is_stopped()
        || Instant::now() >= deadline
        || (!cleanup && actor.stopping.load(Ordering::Acquire));
    if cancelled && !disabling {
        let error = if Instant::now() >= deadline {
            RustPluginError::Deadline
        } else {
            RustPluginError::Cancelled
        };
        let retired = retire_finished(services, actor, lifetime, &name, native_reads, done).await;
        if let Err(error) = &retired {
            lifetime.fail(error.to_string());
        }
        return Driven {
            instance: None,
            result: Err(error),
            retirement: Some(retired),
        };
    }
    match done.commit() {
        Ok((instance, output)) => Driven {
            instance: Some(instance),
            result: Ok(RustCallbackOutput { output }),
            retirement: None,
        },
        Err(error) => Driven {
            instance: None,
            result: Err(plugin_error(error)),
            retirement: None,
        },
    }
}

async fn native_scope(
    services: &Services,
    actor: &ActorControl,
    name: &str,
    needed: bool,
    execution: &ExecutionContext<'_>,
) -> Result<Option<NativeQueryScope>, RustPluginError> {
    if !needed {
        return Ok(None);
    }
    let key = NativePluginKey::new(NativeBackend::Wasm, name, actor.generation)
        .map_err(|error| RustPluginError::Plugin(error.to_string()))?;
    let context = NativeScopeContext {
        key,
        parent_id: execution.parent_id,
        deadline: execution.deadline,
        stop: execution.parent.clone(),
        admission: if execution.cleanup {
            NativeAdmission::Cleanup
        } else if execution.startup {
            NativeAdmission::Normal
        } else {
            NativeAdmission::Admitted
        },
    };
    tokio::select! {
        result = services.queries.wait_scope(context) => result.map(Some).map_err(native_error),
        _ = cancellation(execution.parent, actor, execution.cancelled, execution.wake, execution.cleanup) => Err(RustPluginError::Cancelled),
    }
}

async fn retire_finished(
    services: &Arc<Services>,
    actor: &Arc<ActorControl>,
    lifetime: &Arc<ActorLifetime>,
    name: &str,
    native_reads: bool,
    done: Completion,
) -> Result<RustCallbackOutput, RustPluginError> {
    let cleanup = actor
        .cleanup
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .ok_or(RustPluginError::Stopped)?;
    let parent = services.lifecycle.cleanup_token();
    let deadline = Instant::now() + services.actor_limits.lifecycle_timeout;
    let cancelled = AtomicBool::new(false);
    let wake = Notify::new();
    let execution = ExecutionContext {
        parent_id: actor.generation,
        deadline,
        parent: &parent,
        cleanup: true,
        startup: false,
        cancelled: &cancelled,
        wake: &wake,
    };
    let native = native_scope(services, actor, name, native_reads, &execution).await?;
    let lease = Arc::new(CallbackLease {
        native: Mutex::new(native.as_ref().map(NativeQueryScope::hold)),
        _event: cleanup,
        _actor: Arc::clone(lifetime),
    });
    let control = CallbackControl::new(deadline.into_std());
    let invocation = done
        .begin_cleanup(control.clone(), Box::new(Arc::clone(&lease)))
        .map_err(plugin_error)?;
    let result = run_invocation(
        services,
        actor,
        invocation,
        &control,
        native.as_ref(),
        &execution,
    )
    .await;
    lease.release_native();
    drop(native);
    match result.and_then(|done| done.commit().map_err(plugin_error)) {
        Ok((instance, output)) => {
            drop(instance);
            Ok(RustCallbackOutput { output })
        }
        Err(error) => {
            lifetime.fail(error.to_string());
            Err(error)
        }
    }
}

async fn run_invocation(
    services: &Services,
    actor: &ActorControl,
    mut current: Invocation,
    control: &CallbackControl,
    native: Option<&NativeQueryScope>,
    execution: &ExecutionContext<'_>,
) -> Result<Completion, RustPluginError> {
    let ExecutionContext {
        deadline,
        parent,
        cancelled,
        wake,
        cleanup,
        ..
    } = *execution;
    loop {
        let task = cpu_job(services, deadline, move || current.run());
        tokio::pin!(task);
        let step = tokio::select! {
            result = &mut task => result,
            _ = cancellation(parent, actor, cancelled, wake, cleanup) => { control.cancel(); task.await },
            _ = tokio::time::sleep_until(deadline) => { control.cancel(); task.await },
        };
        let step = step?.map_err(plugin_error)?;
        match step {
            InvocationStep::Quantum(invocation) => {
                current = invocation;
                tokio::task::yield_now().await;
            }
            InvocationStep::Finished(done) => return Ok(done),
            InvocationStep::Read(read) => {
                let scope = native
                    .ok_or_else(|| RustPluginError::Plugin("native read without a scope".into()))?;
                let call_id = read.identity().call_id;
                let query = read.request().clone();
                let reply = tokio::select! {
                    result = scope.execute(call_id, query, NativeReplyFormat::Wasm) => result.map_err(native_error),
                    _ = cancellation(parent, actor, cancelled, wake, cleanup) => { control.cancel(); Err(RustPluginError::Cancelled) },
                    _ = tokio::time::sleep_until(deadline) => { control.cancel(); Err(RustPluginError::Deadline) },
                }?;
                let task = cpu_job(services, deadline, move || read.resume(Box::new(reply)));
                tokio::pin!(task);
                let resumed = tokio::select! {
                    result = &mut task => result,
                    _ = cancellation(parent, actor, cancelled, wake, cleanup) => { control.cancel(); task.await },
                    _ = tokio::time::sleep_until(deadline) => { control.cancel(); task.await },
                };
                current = resumed?.map_err(plugin_error)?;
            }
        }
    }
}

async fn cancellation(
    parent: &StopToken,
    actor: &ActorControl,
    cancelled: &AtomicBool,
    wake: &Notify,
    cleanup: bool,
) {
    loop {
        let changed = wake.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if cancelled.load(Ordering::Acquire)
            || parent.is_stopped()
            || (!cleanup && actor.stopping.load(Ordering::Acquire))
        {
            return;
        }
        tokio::select! { _ = parent.cancelled() => return, _ = actor.stopping(), if !cleanup => return, _ = changed => {} }
    }
}

struct CpuPermit {
    _permit: OwnedSemaphorePermit,
    admission: Arc<Admission>,
}
impl Drop for CpuPermit {
    fn drop(&mut self) {
        self.admission.state().cpu_jobs -= 1;
    }
}

async fn cpu_job<T: Send + 'static>(
    services: &Services,
    deadline: Instant,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, RustPluginError> {
    let permit = cpu_permit(services, deadline).await?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(|error| RustPluginError::Worker(error.to_string()))
}

async fn cpu_permit(services: &Services, deadline: Instant) -> Result<CpuPermit, RustPluginError> {
    let permit = tokio::time::timeout_at(deadline, Arc::clone(&services.cpu).acquire_owned())
        .await
        .map_err(|_| RustPluginError::Deadline)?
        .map_err(|_| RustPluginError::Stopped)?;
    {
        let mut state = services.admission.state();
        state.cpu_jobs += 1;
        state.peak_cpu_jobs = state.peak_cpu_jobs.max(state.cpu_jobs);
    }
    Ok(CpuPermit {
        _permit: permit,
        admission: Arc::clone(&services.admission),
    })
}

struct CountBytes(usize);
impl Write for CountBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn event_bytes(
    event: &Event,
    context: &NativeCallbackContext,
    limits: &Limits,
) -> Result<usize, RustPluginError> {
    if !context.valid() || !context.matches_event(event) {
        return Err(RustPluginError::Plugin(
            "invalid native callback context".into(),
        ));
    }
    let mut event_size = CountBytes(0);
    serde_json::to_writer(&mut event_size, event)
        .map_err(|error| RustPluginError::Plugin(error.to_string()))?;
    if event_size.0 > limits.event_bytes {
        return Err(RustPluginError::Plugin(
            "plugin event exceeds size limit".into(),
        ));
    }
    let mut context_size = CountBytes(0);
    serde_json::to_writer(&mut context_size, context)
        .map_err(|error| RustPluginError::Plugin(error.to_string()))?;
    let owned = match event {
        Event::Enable { server_version } => server_version.len(),
        Event::Command {
            sender,
            command,
            args,
            ..
        } => sender
            .len()
            .saturating_add(command.len())
            .saturating_add(
                args.len()
                    .saturating_mul(size_of::<String>())
                    .saturating_mul(2),
            )
            .saturating_add(args.iter().map(String::len).sum::<usize>()),
        Event::PlayerJoin { uuid, name } | Event::PlayerQuit { uuid, name } => {
            uuid.len() + name.len()
        }
        Event::Disable | Event::Tick { .. } => 0,
    };
    Ok(event_size
        .0
        .saturating_mul(2)
        .saturating_add(context_size.0.saturating_mul(2))
        .saturating_add(owned)
        .saturating_add(size_of::<Event>() + size_of::<NativeCallbackContext>()))
}

fn remove_abandoned(owner: &Weak<Service>, name: &str, control: &ActorControl) {
    if !control.abandoned.load(Ordering::Acquire) || !control.done.load(Ordering::Acquire) {
        return;
    }
    if let Some(owner) = owner.upgrade() {
        let mut actors = owner
            .actors
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if actors
            .get(name)
            .is_some_and(|actor| actor.control.generation == control.generation)
        {
            actors.remove(name);
        }
    }
}

async fn file_job<T: Send + 'static>(
    services: &Services,
    parent: &StopToken,
    deadline: Instant,
    work: impl FnOnce(StopToken) -> Result<T, RustPluginError> + Send + 'static,
) -> Result<T, RustPluginError> {
    tokio::time::timeout_at(
        deadline,
        managed_tasks::blocking(&services.lifecycle, parent, "Rust plugin file", work),
    )
    .await
    .map_err(|_| RustPluginError::Deadline)?
    .map_err(|error| {
        if managed_tasks::OperationError::is_cancelled(&error) {
            RustPluginError::Cancelled
        } else {
            RustPluginError::Worker(error.to_string())
        }
    })?
}

fn server_context() -> NativeCallbackContext {
    NativeCallbackContext {
        source: NativeSource::Server,
    }
}
fn next_id(next: &AtomicU64) -> Result<u64, RustPluginError> {
    next.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
        id.checked_add(1).filter(|next| *next <= i64::MAX as u64)
    })
    .map_err(|_| RustPluginError::Stopped)
}
fn plugin_error(error: zerqon_plugins::PluginError) -> RustPluginError {
    match error {
        zerqon_plugins::PluginError::Cancelled => RustPluginError::Cancelled,
        zerqon_plugins::PluginError::Deadline => RustPluginError::Deadline,
        error => RustPluginError::Plugin(error.to_string()),
    }
}
fn native_error(error: NativeExecutionError) -> RustPluginError {
    match error {
        NativeExecutionError::Cancelled => RustPluginError::Cancelled,
        NativeExecutionError::DeadlineExceeded => RustPluginError::Deadline,
        NativeExecutionError::Terminated => RustPluginError::Stopped,
        NativeExecutionError::Capacity | NativeExecutionError::Busy => RustPluginError::Busy,
        error => RustPluginError::Plugin(error.to_string()),
    }
}
fn io_error(error: std::io::Error) -> RustPluginError {
    RustPluginError::Io(error.to_string())
}

#[cfg(test)]
#[path = "rust_plugins/tests.rs"]
mod tests;
