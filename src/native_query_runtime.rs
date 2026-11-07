use crate::{
    lifecycle::{JobGuard, Lifecycle, RegistrationError, StopToken},
    plugin_services::{
        NativeCallContext, NativeErrorCode, NativeQuery, NativeQueryError, NativeQueryService,
        NativeQueryValue,
    },
};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    fmt,
    io::{self, Write},
    mem::size_of,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Notify, oneshot},
    time::Instant,
};
use zerqon_plugin_api::native::{
    MAX_NATIVE_CALL_BYTES, MAX_NATIVE_CALLS, MAX_NATIVE_RESULT_BYTES, MAX_NATIVE_TOTAL_BYTES,
    MAX_NATIVE_TOTAL_REQUEST_BYTES, NativePlayer, NativeQueryResponse, NativeWorld,
};

pub const WORKSPACE_BYTES: usize = 2 * 1024 * 1024;
pub const DEFAULT_READ_BUDGET_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_PROVIDER_WORKING_SET_BYTES: usize = 1024 * 1024;
const REPLY_BUFFER_BYTES: usize = MAX_NATIVE_RESULT_BYTES + 1;
const QUERY_TIMEOUT: Duration = Duration::from_millis(250);
const VALIDATION_WORKING_SET_BYTES: usize = 384 * 1024;
const EXECUTION_OVERHEAD_BYTES: usize = 128 * 1024;
const _: () = assert!(
    MAX_PROVIDER_WORKING_SET_BYTES
        + REPLY_BUFFER_BYTES
        + MAX_NATIVE_CALL_BYTES
        + VALIDATION_WORKING_SET_BYTES
        + EXECUTION_OVERHEAD_BYTES
        <= WORKSPACE_BYTES
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum NativeBackend {
    Java,
    Wasm,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NativePluginKey {
    backend: NativeBackend,
    name: String,
    generation: u64,
}

impl NativePluginKey {
    pub fn new(
        backend: NativeBackend,
        name: &str,
        generation: u64,
    ) -> Result<Self, NativeExecutionError> {
        if generation == 0
            || !(1..=64).contains(&name.len())
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        {
            return Err(NativeExecutionError::InvalidContext);
        }
        Ok(Self {
            backend,
            name: name.into(),
            generation,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NativeExecutionError {
    #[error("invalid native query configuration: {0}")]
    Configuration(&'static str),
    #[error("native query capacity exceeded")]
    Capacity,
    #[error("native query scope is busy")]
    Busy,
    #[error("invalid native query context")]
    InvalidContext,
    #[error("native query scope has ended")]
    Terminated,
    #[error("native query cancelled")]
    Cancelled,
    #[error("native query deadline exceeded")]
    DeadlineExceeded,
    #[error("native query worker failed")]
    ProviderFailed,
    #[error("native query buffer allocation failed")]
    Allocation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeBudgetSnapshot {
    pub limit_bytes: usize,
    pub reserved_bytes: usize,
    pub peak_bytes: usize,
    pub active_scopes: usize,
    pub active_operations: usize,
    pub quarantined_operations: usize,
    pub rejections: u64,
}

struct BudgetState {
    snapshot: NativeBudgetSnapshot,
    keys: BTreeSet<NativePluginKey>,
}

struct BudgetInner {
    state: Mutex<BudgetState>,
    released: Notify,
}

impl BudgetInner {
    fn state(&self) -> MutexGuard<'_, BudgetState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// Bounds reserved query working sets, not the process RSS or a provider's allocator.
#[derive(Clone)]
pub struct NativeReadBudget {
    inner: Arc<BudgetInner>,
}

impl NativeReadBudget {
    pub fn new(bytes: usize) -> Result<Self, NativeExecutionError> {
        if bytes < WORKSPACE_BYTES
            || bytes > isize::MAX as usize
            || !bytes.is_multiple_of(WORKSPACE_BYTES)
        {
            return Err(NativeExecutionError::Configuration(
                "budget must be a nonzero whole number of query workspaces",
            ));
        }
        Ok(Self {
            inner: Arc::new(BudgetInner {
                state: Mutex::new(BudgetState {
                    snapshot: NativeBudgetSnapshot {
                        limit_bytes: bytes,
                        reserved_bytes: 0,
                        peak_bytes: 0,
                        active_scopes: 0,
                        active_operations: 0,
                        quarantined_operations: 0,
                        rejections: 0,
                    },
                    keys: BTreeSet::new(),
                }),
                released: Notify::new(),
            }),
        })
    }

    pub fn snapshot(&self) -> NativeBudgetSnapshot {
        self.inner.state().snapshot
    }

    fn reserve(&self, key: &NativePluginKey) -> Result<BudgetLease, NativeExecutionError> {
        let mut state = self.inner.state();
        let rejection = if state.keys.contains(key) {
            Some(NativeExecutionError::Busy)
        } else if WORKSPACE_BYTES > state.snapshot.limit_bytes - state.snapshot.reserved_bytes {
            Some(NativeExecutionError::Capacity)
        } else {
            None
        };
        if let Some(error) = rejection {
            state.snapshot.rejections = state.snapshot.rejections.saturating_add(1);
            return Err(error);
        }
        state.keys.insert(key.clone());
        state.snapshot.reserved_bytes += WORKSPACE_BYTES;
        state.snapshot.peak_bytes = state.snapshot.peak_bytes.max(state.snapshot.reserved_bytes);
        state.snapshot.active_scopes += 1;
        Ok(BudgetLease {
            budget: Arc::clone(&self.inner),
            key: key.clone(),
        })
    }
}

struct BudgetLease {
    budget: Arc<BudgetInner>,
    key: NativePluginKey,
}

impl Drop for BudgetLease {
    fn drop(&mut self) {
        let mut state = self.budget.state();
        if state.keys.remove(&self.key) {
            state.snapshot.reserved_bytes -= WORKSPACE_BYTES;
            state.snapshot.active_scopes -= 1;
        }
        drop(state);
        self.budget.released.notify_waiters();
    }
}

/// An audited internal provider must keep all accepted work in its future until
/// completion, including error returns. Caller cancellation never drops that future.
/// Forced runtime teardown additionally requires the provider to be Drop-safe.
/// Detached work and unaccounted allocations are outside this contract.
#[derive(Clone, Copy, Debug)]
pub struct NativeProviderContract {
    max_live_bytes: usize,
}

impl NativeProviderContract {
    pub fn future_owned(max_live_bytes: usize) -> Result<Self, NativeExecutionError> {
        if max_live_bytes == 0 || max_live_bytes > MAX_PROVIDER_WORKING_SET_BYTES {
            return Err(NativeExecutionError::Configuration(
                "provider working set must fit its reserved share",
            ));
        }
        Ok(Self { max_live_bytes })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeAdmission {
    Normal,
    /// Resource work for an already admitted bounded host callback. Its caller
    /// must retain the owning lifecycle job and enforce top-level admission.
    Admitted,
    Cleanup,
}

#[derive(Clone)]
pub struct NativeScopeContext {
    pub key: NativePluginKey,
    pub parent_id: u64,
    pub deadline: Instant,
    pub stop: StopToken,
    pub admission: NativeAdmission,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeReplyFormat {
    Wasm,
    JavaProtocol2,
}

struct RuntimeInner {
    service: Arc<dyn NativeQueryService>,
    budget: NativeReadBudget,
    lifecycle: Lifecycle,
    contract: NativeProviderContract,
}

#[derive(Clone)]
pub struct NativeQueryRuntime {
    inner: Arc<RuntimeInner>,
}

impl NativeQueryRuntime {
    pub fn new(
        service: Arc<dyn NativeQueryService>,
        budget: NativeReadBudget,
        lifecycle: Lifecycle,
        contract: NativeProviderContract,
    ) -> Self {
        Self {
            inner: Arc::new(RuntimeInner {
                service,
                budget,
                lifecycle,
                contract,
            }),
        }
    }

    pub fn try_scope(
        &self,
        context: NativeScopeContext,
    ) -> Result<NativeQueryScope, NativeExecutionError> {
        self.try_scope_attempt(context, &mut false)
    }

    /// Waits for actual workspace release. Callers must already have bounded
    /// callback admission; this method creates no provider work or waiting task.
    pub async fn wait_scope(
        &self,
        context: NativeScopeContext,
    ) -> Result<NativeQueryScope, NativeExecutionError> {
        loop {
            let released = self.inner.budget.inner.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            let mut budget_contended = false;
            match self.try_scope_attempt(context.clone(), &mut budget_contended) {
                Ok(scope) => return Ok(scope),
                Err(error) if !budget_contended => return Err(error),
                Err(_) => {}
            }
            tokio::select! {
                biased;
                _ = context.stop.cancelled() => return Err(NativeExecutionError::Cancelled),
                _ = tokio::time::sleep_until(context.deadline) => return Err(NativeExecutionError::DeadlineExceeded),
                _ = released => {},
            }
        }
    }

    fn try_scope_attempt(
        &self,
        context: NativeScopeContext,
        budget_contended: &mut bool,
    ) -> Result<NativeQueryScope, NativeExecutionError> {
        if context.parent_id == 0 || context.parent_id > i64::MAX as u64 {
            return Err(NativeExecutionError::InvalidContext);
        }
        if context.stop.is_stopped() {
            return Err(NativeExecutionError::Cancelled);
        }
        if Instant::now() >= context.deadline {
            return Err(NativeExecutionError::DeadlineExceeded);
        }
        let lease = self.inner.budget.reserve(&context.key).inspect_err(|_| {
            *budget_contended = true;
        })?;
        let job = match context.admission {
            NativeAdmission::Normal => self.inner.lifecycle.register("native query scope"),
            NativeAdmission::Admitted | NativeAdmission::Cleanup => {
                self.inner.lifecycle.register_cleanup("native query scope")
            }
        }
        .map_err(|error| match error {
            RegistrationError::Capacity => NativeExecutionError::Capacity,
            RegistrationError::Stopped => NativeExecutionError::Cancelled,
            RegistrationError::Sealed => NativeExecutionError::Terminated,
        })?;
        let failed = Arc::new(AtomicBool::new(false));
        let resources = ScopeResources {
            _lease: lease,
            _completion: ScopeCompletion {
                job: Some(job),
                failed: Arc::clone(&failed),
            },
        };
        let buffer = allocate_buffer()?;
        let stop = match context.admission {
            NativeAdmission::Normal | NativeAdmission::Admitted => context.stop.child(),
            NativeAdmission::Cleanup => self.inner.lifecycle.cleanup_token(),
        };
        if stop.is_stopped() || context.stop.is_stopped() {
            return Err(NativeExecutionError::Cancelled);
        }
        Ok(NativeQueryScope {
            inner: Arc::new(ScopeInner {
                state: Mutex::new(ScopeState {
                    buffer: Some(buffer),
                    busy: false,
                    phase: Phase::Idle,
                    quarantined: false,
                    calls: 0,
                    last_call_id: 0,
                    request_bytes: 0,
                    response_bytes: 0,
                }),
                runtime: Arc::clone(&self.inner),
                key: context.key,
                parent_id: context.parent_id,
                deadline: context.deadline,
                stop,
                parent_stop: context.stop,
                terminal: AtomicBool::new(false),
                failed,
                _resources: resources,
            }),
        })
    }
}

struct ScopeCompletion {
    job: Option<JobGuard>,
    failed: Arc<AtomicBool>,
}

impl Drop for ScopeCompletion {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            let result = if self.failed.load(Ordering::Acquire) {
                Err("native query worker ended without settling its operation".into())
            } else {
                Ok(())
            };
            job.finish(result);
        }
    }
}

// Fields drop in this order: memory/key reservations precede the final job acknowledgement.
struct ScopeResources {
    _lease: BudgetLease,
    _completion: ScopeCompletion,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Provider,
    Encoding,
    Reply,
}

struct ScopeState {
    buffer: Option<Box<[u8]>>,
    busy: bool,
    phase: Phase,
    quarantined: bool,
    calls: u64,
    last_call_id: u64,
    request_bytes: usize,
    response_bytes: usize,
}

struct ScopeInner {
    state: Mutex<ScopeState>,
    runtime: Arc<RuntimeInner>,
    key: NativePluginKey,
    parent_id: u64,
    deadline: Instant,
    stop: StopToken,
    parent_stop: StopToken,
    terminal: AtomicBool,
    failed: Arc<AtomicBool>,
    _resources: ScopeResources,
}

impl ScopeInner {
    fn state(&self) -> MutexGuard<'_, ScopeState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn stopped(&self) -> bool {
        self.stop.is_stopped() || self.parent_stop.is_stopped()
    }

    fn terminate(&self) {
        self.terminal.store(true, Ordering::Release);
        let mut state = self.state();
        if state.busy && state.phase != Phase::Reply && !state.quarantined {
            state.quarantined = true;
            self.runtime
                .budget
                .inner
                .state()
                .snapshot
                .quarantined_operations += 1;
        }
    }

    fn fail(&self) {
        self.failed.store(true, Ordering::Release);
        self.terminate();
    }
}

pub struct NativeQueryScope {
    inner: Arc<ScopeInner>,
}

impl fmt::Debug for NativeQueryScope {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output
            .debug_struct("NativeQueryScope")
            .field("key", &self.inner.key)
            .field("terminal", &self.is_terminal())
            .field("busy", &self.is_busy())
            .finish()
    }
}

impl NativeQueryScope {
    pub fn is_terminal(&self) -> bool {
        self.inner.terminal.load(Ordering::Acquire)
    }

    pub fn is_busy(&self) -> bool {
        self.inner.state().busy
    }

    /// Keeps the workspace and lifecycle registration alive without query authority.
    pub fn hold(&self) -> NativeScopeGuard {
        NativeScopeGuard {
            _inner: Arc::clone(&self.inner),
        }
    }

    pub async fn execute(
        &self,
        call_id: u64,
        query: NativeQuery,
        format: NativeReplyFormat,
    ) -> Result<NativeReply, NativeExecutionError> {
        if self.is_terminal() {
            return Err(NativeExecutionError::Terminated);
        }
        if self.inner.stopped() {
            self.inner.terminate();
            return Err(NativeExecutionError::Cancelled);
        }
        if Instant::now() >= self.inner.deadline {
            self.inner.terminate();
            return Err(NativeExecutionError::DeadlineExceeded);
        }
        if call_id == 0 || call_id > i64::MAX as u64 {
            return Err(NativeExecutionError::InvalidContext);
        }
        let mut count = CountingWriter(0);
        serde_json::to_writer(&mut count, &query)
            .map_err(|_| NativeExecutionError::InvalidContext)?;
        let buffer = {
            let mut state = self.inner.state();
            if self.is_terminal() {
                return Err(NativeExecutionError::Terminated);
            }
            if self.inner.stopped() {
                return Err(NativeExecutionError::Cancelled);
            }
            if Instant::now() >= self.inner.deadline {
                return Err(NativeExecutionError::DeadlineExceeded);
            }
            if state.busy {
                return Err(NativeExecutionError::Busy);
            }
            if call_id <= state.last_call_id {
                return Err(NativeExecutionError::InvalidContext);
            }
            if state.calls >= MAX_NATIVE_CALLS
                || count.0 > MAX_NATIVE_CALL_BYTES
                || count.0 > MAX_NATIVE_TOTAL_REQUEST_BYTES.saturating_sub(state.request_bytes)
            {
                return Err(NativeExecutionError::Capacity);
            }
            let buffer = state.buffer.take().ok_or(NativeExecutionError::Busy)?;
            state.busy = true;
            state.phase = Phase::Provider;
            state.calls += 1;
            state.last_call_id = call_id;
            state.request_bytes += count.0;
            self.inner
                .runtime
                .budget
                .inner
                .state()
                .snapshot
                .active_operations += 1;
            buffer
        };
        let deadline = self.inner.deadline.min(Instant::now() + QUERY_TIMEOUT);
        let operation = OperationOwner {
            buffer: Some(buffer),
            scope: Arc::clone(&self.inner),
            settled: false,
            reusable: false,
        };
        let mut waiter = WaiterGuard {
            scope: Arc::clone(&self.inner),
            armed: true,
        };
        let context = NativeCallContext {
            plugin: self.inner.key.name.clone(),
            parent_id: self.inner.parent_id,
            call_id,
            deadline,
        };
        let worker = tokio::spawn(run_provider(operation, context, query, format));
        let (sender, mut receiver) = oneshot::channel();
        let supervisor_scope = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let result = match worker.await {
                Ok(result) => result,
                Err(_) => {
                    supervisor_scope.fail();
                    Err(NativeExecutionError::ProviderFailed)
                }
            };
            if let Err(result) = sender.send(result) {
                supervisor_scope.terminate();
                drop(result);
            }
        });
        let result = tokio::select! {
            biased;
            result = &mut receiver => result.unwrap_or(Err(NativeExecutionError::ProviderFailed)),
            _ = self.inner.stop.cancelled() => Err(NativeExecutionError::Cancelled),
            _ = self.inner.parent_stop.cancelled() => Err(NativeExecutionError::Cancelled),
            _ = tokio::time::sleep_until(deadline) => Err(NativeExecutionError::DeadlineExceeded),
        };
        match result {
            Ok(reply)
                if !self.is_terminal() && !self.inner.stopped() && Instant::now() < deadline =>
            {
                waiter.armed = false;
                Ok(reply)
            }
            Ok(reply) => {
                self.inner.terminate();
                drop(reply);
                if Instant::now() >= deadline {
                    Err(NativeExecutionError::DeadlineExceeded)
                } else {
                    Err(NativeExecutionError::Cancelled)
                }
            }
            Err(error) => Err(error),
        }
    }
}

impl Drop for NativeQueryScope {
    fn drop(&mut self) {
        let active = {
            let state = self.inner.state();
            state.busy && state.phase != Phase::Reply
        };
        if active {
            self.inner.terminate();
        }
    }
}

#[derive(Clone)]
pub struct NativeScopeGuard {
    _inner: Arc<ScopeInner>,
}

struct WaiterGuard {
    scope: Arc<ScopeInner>,
    armed: bool,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        if self.armed {
            self.scope.terminate();
        }
    }
}

struct WorkerGuard {
    scope: Arc<ScopeInner>,
    settled: bool,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        if !self.settled {
            self.scope.fail();
        }
    }
}

struct OperationOwner {
    buffer: Option<Box<[u8]>>,
    scope: Arc<ScopeInner>,
    settled: bool,
    reusable: bool,
}

impl Drop for OperationOwner {
    fn drop(&mut self) {
        if !self.settled {
            self.scope.fail();
        } else if !self.reusable {
            self.scope.terminate();
        }
        let mut state = self.scope.state();
        state.buffer = self.buffer.take();
        state.busy = false;
        state.phase = Phase::Idle;
        let mut budget = self.scope.runtime.budget.inner.state();
        budget.snapshot.active_operations -= 1;
        if state.quarantined {
            state.quarantined = false;
            budget.snapshot.quarantined_operations -= 1;
        }
    }
}

/// Owns its workspace through socket writes or a suspended Wasm result slot.
/// Copying the borrowed bytes requires separate accounting by the caller.
pub struct NativeReply {
    used: usize,
    json_len: usize,
    operation: OperationOwner,
}

impl NativeReply {
    pub fn bytes(&self) -> &[u8] {
        &self
            .operation
            .buffer
            .as_ref()
            .expect("reply owns its buffer until drop")[..self.used]
    }

    /// Matches protocol 2's aggregate byte budget, which excludes its final LF.
    pub fn json_len(&self) -> usize {
        self.json_len
    }
}

impl fmt::Debug for NativeReply {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output
            .debug_struct("NativeReply")
            .field("bytes", &self.used)
            .finish_non_exhaustive()
    }
}

impl zerqon_plugins::resumable::ReadReply for NativeReply {
    fn bytes(&self) -> &[u8] {
        self.bytes()
    }
}

async fn run_provider(
    mut operation: OperationOwner,
    context: NativeCallContext,
    query: NativeQuery,
    format: NativeReplyFormat,
) -> Result<NativeReply, NativeExecutionError> {
    let scope = Arc::clone(&operation.scope);
    let mut worker = WorkerGuard {
        scope: Arc::clone(&scope),
        settled: false,
    };
    if scope.stopped() || scope.terminal.load(Ordering::Acquire) {
        operation.settled = true;
        worker.settled = true;
        return Err(NativeExecutionError::Cancelled);
    }
    if Instant::now() >= context.deadline {
        operation.settled = true;
        worker.settled = true;
        return Err(NativeExecutionError::DeadlineExceeded);
    }
    let result = if query.valid() {
        scope
            .runtime
            .service
            .query(context.clone(), query.clone())
            .await
    } else {
        Err(NativeQueryError::new(
            NativeErrorCode::InvalidArgument,
            "Invalid native query arguments",
        ))
    };
    operation.settled = true;
    if scope.stopped() || scope.terminal.load(Ordering::Acquire) {
        drop(result);
        worker.settled = true;
        return Err(NativeExecutionError::Cancelled);
    }
    if Instant::now() >= context.deadline {
        drop(result);
        worker.settled = true;
        return Err(NativeExecutionError::DeadlineExceeded);
    }
    scope.state().phase = Phase::Encoding;
    let work = EncodeWork {
        result,
        context,
        query,
        format,
        operation,
    };
    let encoded = tokio::task::spawn_blocking(move || work.run()).await;
    match encoded {
        Ok(result) => {
            worker.settled = true;
            result
        }
        Err(_) => Err(NativeExecutionError::ProviderFailed),
    }
}

#[derive(Serialize)]
struct JavaReply<'a> {
    kind: &'static str,
    parent_id: u64,
    call_id: u64,
    result: &'a NativeQueryResponse,
}

fn encode_reply(
    mut operation: OperationOwner,
    context: NativeCallContext,
    query: NativeQuery,
    result: Result<NativeQueryValue, NativeQueryError>,
    format: NativeReplyFormat,
) -> Result<NativeReply, NativeExecutionError> {
    if operation.scope.stopped() || operation.scope.terminal.load(Ordering::Acquire) {
        return Err(NativeExecutionError::Cancelled);
    }
    if Instant::now() >= context.deadline {
        return Err(NativeExecutionError::DeadlineExceeded);
    }
    let bound = operation.scope.runtime.contract.max_live_bytes;
    let mut response = match result {
        Ok(value) if value_bytes(&value) <= bound && value.valid_for(&query) => {
            NativeQueryResponse::Success { value }
        }
        Err(error)
            if error
                .message
                .capacity()
                .saturating_add(size_of::<NativeQueryError>())
                <= bound
                && error.valid() =>
        {
            NativeQueryResponse::Error { error }
        }
        _ => NativeQueryResponse::Error {
            error: NativeQueryError::new(
                NativeErrorCode::Internal,
                "Native query service returned an invalid result",
            ),
        },
    };
    let buffer = operation
        .buffer
        .as_mut()
        .expect("operation owns its reserved buffer");
    let mut writer = FixedWriter {
        bytes: buffer,
        used: 0,
    };
    if write_response(&mut writer, &response, &context, format).is_err() {
        writer.used = 0;
        response = NativeQueryResponse::Error {
            error: NativeQueryError::new(
                NativeErrorCode::Capacity,
                "Native query result exceeds its byte limit",
            ),
        };
        write_response(&mut writer, &response, &context, format)
            .map_err(|_| NativeExecutionError::ProviderFailed)?;
    }
    let json_len = writer.used;
    if format == NativeReplyFormat::JavaProtocol2 {
        writer.bytes[writer.used] = b'\n';
        writer.used += 1;
    }
    let used = writer.used;
    drop(response);
    {
        let mut state = operation.scope.state();
        if json_len > MAX_NATIVE_TOTAL_BYTES.saturating_sub(state.response_bytes) {
            return Err(NativeExecutionError::Capacity);
        }
        state.response_bytes += json_len;
        state.phase = Phase::Reply;
    }
    Ok(NativeReply {
        used,
        json_len,
        operation: {
            operation.reusable = true;
            operation
        },
    })
}

// Queued encodes destroy owned DTO allocations before their operation reservation.
struct EncodeWork {
    result: Result<NativeQueryValue, NativeQueryError>,
    context: NativeCallContext,
    query: NativeQuery,
    format: NativeReplyFormat,
    operation: OperationOwner,
}

impl EncodeWork {
    fn run(self) -> Result<NativeReply, NativeExecutionError> {
        encode_reply(
            self.operation,
            self.context,
            self.query,
            self.result,
            self.format,
        )
    }
}

fn write_response(
    writer: &mut FixedWriter<'_>,
    response: &NativeQueryResponse,
    context: &NativeCallContext,
    format: NativeReplyFormat,
) -> Result<(), serde_json::Error> {
    match format {
        NativeReplyFormat::Wasm => serde_json::to_writer(writer, response),
        NativeReplyFormat::JavaProtocol2 => serde_json::to_writer(
            writer,
            &JavaReply {
                kind: "native_result",
                parent_id: context.parent_id,
                call_id: context.call_id,
                result: response,
            },
        ),
    }
}

struct FixedWriter<'a> {
    bytes: &'a mut [u8],
    used: usize,
}

impl Write for FixedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_NATIVE_RESULT_BYTES.saturating_sub(self.used) {
            return Err(io::Error::other("native result exceeds its byte limit"));
        }
        let end = self.used + bytes.len();
        self.bytes[self.used..end].copy_from_slice(bytes);
        self.used = end;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct CountingWriter(usize);

impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn allocate_buffer() -> Result<Box<[u8]>, NativeExecutionError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(REPLY_BUFFER_BYTES)
        .map_err(|_| NativeExecutionError::Allocation)?;
    bytes.resize(REPLY_BUFFER_BYTES, 0);
    Ok(bytes.into_boxed_slice())
}

fn value_bytes(value: &NativeQueryValue) -> usize {
    let owned = match value {
        NativeQueryValue::ServerSnapshot(snapshot) => snapshot
            .server_version
            .capacity()
            .saturating_add(snapshot.minecraft_version.capacity()),
        NativeQueryValue::OnlinePlayers { players } => players.iter().fold(
            players.capacity().saturating_mul(size_of::<NativePlayer>()),
            |size, player| size.saturating_add(player.name.capacity()),
        ),
        NativeQueryValue::Player { player } => {
            player.as_ref().map_or(0, |player| player.name.capacity())
        }
        NativeQueryValue::Worlds { worlds } => worlds.iter().fold(
            worlds.capacity().saturating_mul(size_of::<NativeWorld>()),
            |size, world| {
                size.saturating_add(world.name.capacity())
                    .saturating_add(world.key.capacity())
                    .saturating_add(world.dimension.capacity())
            },
        ),
    };
    size_of::<NativeQueryValue>().saturating_add(owned)
}

#[cfg(test)]
#[path = "native_query_runtime/tests.rs"]
mod tests;
