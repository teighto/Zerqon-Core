//! Owned WebAssembly execution segments. Native services and registry locks belong
//! to the caller; this module never waits for a provider.

use super::*;
use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};
use wasmi::{
    CompilationMode, TypedResumableCall, TypedResumableCallHostTrap, TypedResumableCallOutOfFuel,
    Val, ValType,
};
use wasmi_core::{LimiterError, MemoryError, ResourceLimiter, TableError};
use zerqon_plugin_api::native::{
    CAP_CONTEXT, MAX_NATIVE_CALL_BYTES, MAX_NATIVE_CALLS, MAX_NATIVE_CONTEXT_BYTES,
    MAX_NATIVE_CONTEXT_COPY_BYTES, MAX_NATIVE_RESULT_BYTES, MAX_NATIVE_TOTAL_BYTES,
    MAX_NATIVE_TOTAL_REQUEST_BYTES, NativeCallbackContext, NativeDecodeError, NativeErrorCode,
    NativeQuery, NativeQueryError, NativeQueryResponse, READ_API_VERSION, READ_CAPABILITIES,
};

static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);

/// A per-callback absolute deadline and monotonic cancellation flag.
#[derive(Clone, Debug)]
pub struct CallbackControl(Arc<ControlState>);

#[derive(Debug)]
struct ControlState {
    cancelled: AtomicBool,
    deadline: Instant,
}

impl CallbackControl {
    pub fn new(deadline: Instant) -> Self {
        Self(Arc::new(ControlState {
            cancelled: AtomicBool::new(false),
            deadline,
        }))
    }
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
    }
    pub fn deadline(&self) -> Instant {
        self.0.deadline
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }
    fn check(&self) -> Result<(), PluginError> {
        if self.is_cancelled() {
            return Err(PluginError::Cancelled);
        }
        if Instant::now() >= self.deadline() {
            return Err(PluginError::Deadline);
        }
        Ok(())
    }
}

/// Immutable response bytes together with their host-owned allocation lease.
/// Implementations must keep the same bytes for the owner's lifetime.
pub trait ReadReply: Send + 'static {
    fn bytes(&self) -> &[u8];
}
impl ReadReply for Vec<u8> {
    fn bytes(&self) -> &[u8] {
        self
    }
}
impl ReadReply for Box<[u8]> {
    fn bytes(&self) -> &[u8] {
        self
    }
}

/// Host-generated identifiers; none is accepted from a guest request body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvocationIdentity {
    pub instance_id: u64,
    pub callback_id: u64,
    pub call_id: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallbackStats {
    pub fuel_consumed: u64,
    pub native_calls: u64,
    pub request_bytes: usize,
    pub response_bytes: usize,
    pub context_bytes: usize,
    pub segments: u64,
}

pub(super) struct FuelLedger {
    total: u64,
    unissued: u64,
}

impl ResourceLimiter for HostState {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool, LimiterError> {
        self.limits.memory_growing(current, desired, maximum)
    }
    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool, LimiterError> {
        self.limits.table_growing(current, desired, maximum)
    }
    fn memory_grow_failed(&mut self, error: &MemoryError) -> Result<(), LimiterError> {
        if self.fuel.is_some() && matches!(error, MemoryError::OutOfFuel { .. }) {
            return Ok(());
        }
        self.limits.memory_grow_failed(error)
    }
    fn table_grow_failed(&mut self, error: &TableError) -> Result<(), LimiterError> {
        if self.fuel.is_some() && matches!(error, TableError::OutOfFuel { .. }) {
            return Ok(());
        }
        self.limits.table_grow_failed(error)
    }
    fn instances(&self) -> usize {
        self.limits.instances()
    }
    fn tables(&self) -> usize {
        self.limits.tables()
    }
    fn memories(&self) -> usize {
        self.limits.memories()
    }
}

pub(super) struct NativeState {
    instance_id: u64,
    capabilities: u64,
    token: i64,
    control: Option<CallbackControl>,
    context: Vec<u8>,
    pending: Option<PendingQuery>,
    result: Option<ReadyReply>,
    stats: CallbackStats,
}
impl NativeState {
    fn new(instance_id: u64, capabilities: u64) -> Self {
        Self {
            instance_id,
            capabilities,
            token: 0,
            control: None,
            context: Vec::new(),
            pending: None,
            result: None,
            stats: CallbackStats::default(),
        }
    }
    fn clear(&mut self) {
        self.token = 0;
        self.control = None;
        self.context = Vec::new();
        self.pending = None;
        self.result = None;
    }
}

struct PendingQuery {
    identity: InvocationIdentity,
    query: NativeQuery,
}
struct ReadyReply {
    handle: i32,
    len: usize,
    owner: Box<dyn ReadReply>,
}

#[derive(Debug)]
struct ReadYield(InvocationIdentity);
impl fmt::Display for ReadYield {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("native read suspension")
    }
}
impl wasmi::errors::HostError for ReadYield {}

#[derive(Debug)]
struct HostFault(&'static str);
impl fmt::Display for HostFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl wasmi::errors::HostError for HostFault {}
fn fault(message: &'static str) -> wasmi::Error {
    wasmi::Error::host(HostFault(message))
}

struct InstanceSlot(Arc<AtomicUsize>);
impl Drop for InstanceSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Shared engine and limits; instantiation does not call `Enable`.
#[derive(Clone)]
pub struct ResumableHost {
    engine: Engine,
    limits: Arc<Limits>,
    quantum: u64,
    live: Arc<AtomicUsize>,
}

impl ResumableHost {
    pub fn new(limits: Limits, quantum: u64) -> Result<Self, PluginError> {
        validate_limits(&limits)?;
        if quantum == 0 || quantum > 100_000_000 {
            return Err(PluginError::Limits("fuel quantum must be 1..100,000,000"));
        }
        let mut config = Config::default();
        config
            .consume_fuel(true)
            .enforced_limits(EnforcedLimits::strict())
            .allow_start_fn(false)
            .compilation_mode(CompilationMode::Eager)
            .set_max_recursion_depth(256)
            .set_min_stack_height(4096)
            .set_max_stack_height(1024 * 1024)
            .set_max_cached_stacks(2);
        Ok(Self {
            engine: Engine::new(&config),
            limits: Arc::new(limits),
            quantum,
            live: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn live_instances(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    pub fn instantiate(
        &self,
        name: &str,
        bytes: &[u8],
        capabilities: u64,
    ) -> Result<PluginInstance, PluginError> {
        if !valid_name(name) {
            return Err(PluginError::Name);
        }
        if bytes.len() > self.limits.module_bytes {
            return Err(PluginError::ModuleSize);
        }
        if capabilities & !READ_CAPABILITIES != 0 {
            return Err(PluginError::Limits("unknown native read capability"));
        }
        self.live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count
                    .checked_add(1)
                    .filter(|count| *count <= self.limits.plugins)
            })
            .map_err(|_| PluginError::Count)?;
        let slot = InstanceSlot(Arc::clone(&self.live));
        let id = NEXT_INSTANCE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| PluginError::NativeProtocol("instance identity exhausted"))?;
        let module = Module::new(&self.engine, bytes)?;
        let uses_native_reads = module
            .imports()
            .any(|import| import.module() == "zerqon_read_v1");
        let state = HostState {
            limits: StoreLimitsBuilder::new()
                .memory_size(self.limits.memory_bytes)
                .table_elements(16_384)
                .instances(1)
                .memories(1)
                .tables(1)
                .trap_on_grow_failure(true)
                .build(),
            event: Vec::new(),
            actions: Vec::new(),
            output_bytes: 0,
            copied_bytes: 0,
            output_limit: self.limits.output_bytes,
            copy_limit: self.limits.event_bytes * 4 + self.limits.output_bytes,
            action_limit: self.limits.actions_per_event,
            kind: EventKind::None,
            native: Some(NativeState::new(id, capabilities)),
            fuel: None,
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| state);
        store.set_fuel(self.limits.fuel_per_event)?;
        let mut linker = Linker::new(&self.engine);
        linker.func_wrap(
            "zerqon",
            "event_len",
            |caller: Caller<'_, HostState>| -> Result<i32, wasmi::Error> {
                check_control(&caller)?;
                Ok(caller.data().event.len() as i32)
            },
        )?;
        linker.func_wrap("zerqon", "event_read", read_event)?;
        linker.func_wrap("zerqon", "emit", emit_action)?;
        linker.func_wrap("zerqon_read_v1", "capabilities", capabilities_import)?;
        linker.func_wrap("zerqon_read_v1", "callback_token", token_import)?;
        linker.func_wrap("zerqon_read_v1", "context_len", context_len)?;
        linker.func_wrap("zerqon_read_v1", "context_read", context_read)?;
        linker.func_wrap("zerqon_read_v1", "query", query)?;
        linker.func_wrap("zerqon_read_v1", "result_len", result_len)?;
        linker.func_wrap("zerqon_read_v1", "result_read", result_read)?;
        let instance = linker.instantiate_and_start(&mut store, &module)?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or(PluginError::Memory)?;
        if memory.ty(&store).is_64() {
            return Err(PluginError::Memory);
        }
        let abi = instance
            .get_typed_func::<(), u32>(&store, "zerqon_abi_version")?
            .call(&mut store, ())?;
        if abi != ABI_VERSION {
            return Err(PluginError::Abi(abi));
        }
        let handler = instance.get_typed_func::<(), i32>(&store, "zerqon_on_event")?;
        Ok(PluginInstance {
            runtime: Box::new(Runtime { store, handler }),
            limits: Arc::clone(&self.limits),
            quantum: self.quantum,
            id,
            token: 0,
            enabled: false,
            stopped: false,
            uses_native_reads,
            name: name.into(),
            _slot: slot,
        })
    }
}

/// An exclusively owned ready instance. A suspended or abandoned callback cannot
/// return one; only successful completion can make the instance ready again.
pub struct PluginInstance {
    runtime: Box<Runtime>,
    limits: Arc<Limits>,
    quantum: u64,
    id: u64,
    token: i64,
    enabled: bool,
    stopped: bool,
    uses_native_reads: bool,
    name: String,
    _slot: InstanceSlot,
}
impl fmt::Debug for PluginInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginInstance")
            .field("name", &self.name)
            .field("id", &self.id)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

pub struct PreparedCallback {
    instance_id: u64,
    event: Vec<u8>,
    context: Vec<u8>,
    kind: EventKind,
    disabling: bool,
}

#[derive(Debug)]
pub struct StartFailure {
    pub instance: PluginInstance,
    pub error: PluginError,
}

struct CallbackInput {
    prepared: PreparedCallback,
    control: CallbackControl,
    guard: Box<dyn Send>,
}

impl PluginInstance {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
    pub fn uses_native_reads(&self) -> bool {
        self.uses_native_reads
    }

    /// Validate and serialize under the caller's admission budget, before execution.
    pub fn prepare(
        &self,
        event: &Event,
        context: &NativeCallbackContext,
    ) -> Result<PreparedCallback, PluginError> {
        if !context.valid() || !context.matches_event(event) {
            return Err(PluginError::CallbackContext);
        }
        if self.stopped || matches!(event, Event::Enable { .. }) == self.enabled {
            return Err(PluginError::Lifecycle);
        }
        let mut encoded = BoundedWriter {
            bytes: Vec::new(),
            limit: self.limits.event_bytes,
        };
        serde_json::to_writer(&mut encoded, event).map_err(|_| PluginError::EventSize)?;
        let mut source = BoundedWriter {
            bytes: Vec::new(),
            limit: MAX_NATIVE_CONTEXT_BYTES,
        };
        serde_json::to_writer(&mut source, context).map_err(|_| PluginError::CallbackContext)?;
        Ok(PreparedCallback {
            instance_id: self.id,
            event: encoded.bytes,
            context: source.bytes,
            kind: match event {
                Event::Enable { .. } => EventKind::Enable,
                Event::Command { .. } => EventKind::Command,
                _ => EventKind::Other,
            },
            disabling: matches!(event, Event::Disable),
        })
    }

    /// Transfer the instance and callback allocation owner to a CPU job.
    pub fn begin(
        mut self,
        prepared: PreparedCallback,
        control: CallbackControl,
        guard: Box<dyn Send>,
    ) -> Result<Invocation, Box<StartFailure>> {
        let input = CallbackInput {
            prepared,
            control,
            guard,
        };
        let prepared = &input.prepared;
        let validate = || {
            input.control.check()?;
            if prepared.instance_id != self.id {
                return Err(PluginError::NativeProtocol(
                    "callback belongs to another instance",
                ));
            }
            if self.stopped || (prepared.kind == EventKind::Enable) == self.enabled {
                return Err(PluginError::Lifecycle);
            }
            self.token
                .checked_add(1)
                .ok_or(PluginError::NativeProtocol("callback token exhausted"))
        };
        let token = match validate() {
            Ok(token) => token,
            Err(error) => {
                return Err(Box::new(StartFailure {
                    instance: self,
                    error,
                }));
            }
        };
        let initial = self.quantum.min(self.limits.fuel_per_event);
        if let Err(error) = self.runtime.store.set_fuel(initial) {
            return Err(Box::new(StartFailure {
                instance: self,
                error: error.into(),
            }));
        }
        let CallbackInput {
            prepared,
            control,
            guard,
        } = input;
        self.token = token;
        let state = self.runtime.store.data_mut();
        state.event = prepared.event;
        state.actions.clear();
        state.output_bytes = 0;
        state.copied_bytes = 0;
        state.kind = prepared.kind;
        state.fuel = Some(FuelLedger {
            total: self.limits.fuel_per_event,
            unissued: self.limits.fuel_per_event - initial,
        });
        let native = state
            .native
            .as_mut()
            .expect("owned instances have native state");
        native.token = token;
        native.context = prepared.context;
        native.control = Some(control.clone());
        native.stats = CallbackStats::default();
        Ok(Invocation {
            continuation: Continuation::Start,
            instance: self,
            control,
            guard,
            disabling: prepared.disabling,
            segments: 0,
        })
    }
}

enum Continuation {
    Start,
    Fuel(TypedResumableCallOutOfFuel<i32>),
    Host(TypedResumableCallHostTrap<i32>, i32),
}

pub struct Invocation {
    continuation: Continuation,
    instance: PluginInstance,
    control: CallbackControl,
    guard: Box<dyn Send>,
    disabling: bool,
    segments: u64,
}

pub enum InvocationStep {
    Finished(Completion),
    Read(ReadSuspension),
    Quantum(Invocation),
}

impl Invocation {
    pub fn fuel_remaining(&self) -> u64 {
        self.instance.runtime.store.get_fuel().unwrap_or(0)
            + self
                .instance
                .runtime
                .store
                .data()
                .fuel
                .as_ref()
                .map_or(0, |fuel| fuel.unissued)
    }
    pub fn run(mut self) -> Result<InvocationStep, PluginError> {
        self.control.check()?;
        self.segments += 1;
        let store = &mut self.instance.runtime.store;
        let continuation = std::mem::replace(&mut self.continuation, Continuation::Start);
        let call = match continuation {
            Continuation::Start => self.instance.runtime.handler.call_resumable(store, ()),
            Continuation::Host(trap, handle) => trap.resume(store, &[Val::I32(handle)]),
            Continuation::Fuel(paused) => {
                let current = store.get_fuel()?;
                let fuel = store
                    .data_mut()
                    .fuel
                    .as_mut()
                    .ok_or(PluginError::FuelExhausted)?;
                let required = paused.required_fuel().saturating_sub(current);
                if required > fuel.unissued || fuel.unissued == 0 {
                    return Err(PluginError::FuelExhausted);
                }
                let extra = self.instance.quantum.max(required).min(fuel.unissued);
                fuel.unissued -= extra;
                store.set_fuel(current + extra)?;
                paused.resume(store)
            }
        };
        self.control.check()?;
        let call = call?;
        match call {
            TypedResumableCall::Finished(code) => {
                if code != 0 {
                    return Err(PluginError::Handler(code));
                }
                let remaining = self.fuel_remaining();
                let state = self.instance.runtime.store.data_mut();
                let mut stats = state
                    .native
                    .as_ref()
                    .ok_or(PluginError::CallbackContext)?
                    .stats;
                stats.fuel_consumed =
                    state.fuel.as_ref().ok_or(PluginError::FuelExhausted)?.total - remaining;
                stats.segments = self.segments;
                let actions = std::mem::take(&mut state.actions);
                state.event = Vec::new();
                state.kind = EventKind::None;
                state.fuel = None;
                state
                    .native
                    .as_mut()
                    .ok_or(PluginError::CallbackContext)?
                    .clear();
                self.instance.enabled = !self.disabling;
                self.instance.stopped = self.disabling;
                Ok(InvocationStep::Finished(Completion {
                    instance: self.instance,
                    actions,
                    guard: self.guard,
                    control: self.control,
                    stats,
                }))
            }
            TypedResumableCall::OutOfFuel(paused) => {
                self.continuation = Continuation::Fuel(paused);
                Ok(InvocationStep::Quantum(self))
            }
            TypedResumableCall::HostTrap(trap) => {
                let Some(marker) = trap.host_error().downcast_ref::<ReadYield>() else {
                    let message = trap
                        .host_error()
                        .downcast_ref::<HostFault>()
                        .map_or("non-native host trap", |fault| fault.0);
                    return Err(PluginError::NativeProtocol(message));
                };
                let state = self.instance.runtime.store.data_mut();
                let pending = state
                    .native
                    .as_mut()
                    .and_then(|native| native.pending.take())
                    .ok_or(PluginError::NativeProtocol("yield has no pending request"))?;
                if marker.0 != pending.identity
                    || marker.0.instance_id != self.instance.id
                    || marker.0.callback_id != self.instance.token as u64
                    || trap.host_func().ty(&self.instance.runtime.store).results() != [ValType::I32]
                {
                    return Err(PluginError::NativeProtocol("invalid native yield identity"));
                }
                self.continuation = Continuation::Start;
                Ok(InvocationStep::Read(ReadSuspension {
                    trap,
                    pending,
                    invocation: self,
                }))
            }
        }
    }
}

pub struct ReadSuspension {
    trap: TypedResumableCallHostTrap<i32>,
    pending: PendingQuery,
    invocation: Invocation,
}
impl ReadSuspension {
    pub fn request(&self) -> &NativeQuery {
        &self.pending.query
    }
    pub fn identity(&self) -> InvocationIdentity {
        self.pending.identity
    }
    pub fn control(&self) -> &CallbackControl {
        &self.invocation.control
    }
    pub fn fuel_remaining(&self) -> u64 {
        self.invocation.fuel_remaining()
    }

    /// Validate a leased reply without executing the guest. Run this bounded
    /// decoding step under CPU admission just like `Invocation::run`.
    pub fn resume(mut self, reply: Box<dyn ReadReply>) -> Result<Invocation, PluginError> {
        self.invocation.control.check()?;
        let reply = if reply.bytes().len() > MAX_NATIVE_RESULT_BYTES {
            drop(reply);
            error_reply(
                NativeErrorCode::Capacity,
                "Native response exceeds its byte limit",
            )?
        } else if NativeQueryResponse::decode_for(&self.pending.query, reply.bytes()).is_err() {
            drop(reply);
            error_reply(NativeErrorCode::Internal, "Invalid native response")?
        } else {
            reply
        };
        self.invocation.control.check()?;
        let native = self
            .invocation
            .instance
            .runtime
            .store
            .data_mut()
            .native
            .as_mut()
            .ok_or(PluginError::CallbackContext)?;
        install_reply(native, self.pending.identity.call_id as i32, reply)?;
        self.invocation.continuation =
            Continuation::Host(self.trap, self.pending.identity.call_id as i32);
        Ok(self.invocation)
    }
}

pub struct Completion {
    instance: PluginInstance,
    actions: Vec<Action>,
    guard: Box<dyn Send>,
    control: CallbackControl,
    stats: CallbackStats,
}
impl Completion {
    /// Discard successful callback actions and run only `Disable` with a fresh,
    /// bounded cleanup control. Never exposes a cancelled instance for normal use.
    pub fn begin_cleanup(
        self,
        control: CallbackControl,
        guard: Box<dyn Send>,
    ) -> Result<Invocation, PluginError> {
        let Self {
            instance,
            actions,
            guard: previous,
            ..
        } = self;
        drop(actions);
        drop(previous);
        if !instance.enabled || instance.stopped {
            return Err(PluginError::Lifecycle);
        }
        let context = NativeCallbackContext {
            source: zerqon_plugin_api::native::NativeSource::Server,
        };
        let prepared = instance.prepare(&Event::Disable, &context)?;
        instance
            .begin(prepared, control, guard)
            .map_err(|failure| failure.error)
    }

    /// The caller must also verify its own registry generation before publication.
    pub fn commit(self) -> Result<(PluginInstance, CallbackOutput), PluginError> {
        self.control.check()?;
        Ok((
            self.instance,
            CallbackOutput {
                actions: self.actions,
                _guard: self.guard,
                stats: self.stats,
            },
        ))
    }
}

/// Returned actions retain their callback allocation owner until publication/drop.
pub struct CallbackOutput {
    actions: Vec<Action>,
    _guard: Box<dyn Send>,
    stats: CallbackStats,
}
impl CallbackOutput {
    pub fn actions(&self) -> &[Action] {
        &self.actions
    }
    pub fn stats(&self) -> CallbackStats {
        self.stats
    }
}

pub(super) fn check_control(caller: &Caller<'_, HostState>) -> Result<(), wasmi::Error> {
    if let Some(control) = caller
        .data()
        .native
        .as_ref()
        .and_then(|native| native.control.as_ref())
    {
        control
            .check()
            .map_err(|_| fault("native callback is no longer active"))?;
    }
    Ok(())
}

pub(super) fn charge_fuel(
    caller: &mut Caller<'_, HostState>,
    amount: u64,
) -> Result<(), wasmi::Error> {
    let current = caller.get_fuel()?;
    let unissued = caller.data().fuel.as_ref().map_or(0, |fuel| fuel.unissued);
    if amount > current.saturating_add(unissued) {
        return Err(fault("plugin I/O fuel exhausted"));
    }
    let from_store = current.min(amount);
    caller.set_fuel(current - from_store)?;
    if let Some(fuel) = caller.data_mut().fuel.as_mut() {
        fuel.unissued -= amount - from_store;
    }
    Ok(())
}

fn active<'a>(
    caller: &'a Caller<'_, HostState>,
    token: i64,
) -> Result<&'a NativeState, wasmi::Error> {
    check_control(caller)?;
    let native = caller
        .data()
        .native
        .as_ref()
        .ok_or_else(|| fault("native reads unavailable"))?;
    if token <= 0 || token != native.token || caller.data().kind == EventKind::None {
        return Err(fault("invalid native callback token"));
    }
    Ok(native)
}

fn capabilities_import(caller: Caller<'_, HostState>, version: i32) -> i64 {
    if version == READ_API_VERSION {
        caller
            .data()
            .native
            .as_ref()
            .map_or(0, |native| native.capabilities as i64)
    } else {
        0
    }
}
fn token_import(caller: Caller<'_, HostState>) -> i64 {
    if caller.data().kind == EventKind::None {
        0
    } else {
        caller
            .data()
            .native
            .as_ref()
            .map_or(0, |native| native.token)
    }
}
fn context_len(caller: Caller<'_, HostState>, token: i64) -> Result<i32, wasmi::Error> {
    let native = active(&caller, token)?;
    if native.capabilities & CAP_CONTEXT == 0 {
        return Err(fault("native context capability denied"));
    }
    Ok(native.context.len() as i32)
}
fn range(pointer: i32, len: i32) -> Result<std::ops::Range<usize>, wasmi::Error> {
    if pointer < 0 || len < 0 {
        return Err(fault("invalid native memory range"));
    }
    let start = pointer as usize;
    let end = start
        .checked_add(len as usize)
        .ok_or_else(|| fault("native memory range overflow"))?;
    Ok(start..end)
}
fn context_read(
    mut caller: Caller<'_, HostState>,
    token: i64,
    pointer: i32,
    len: i32,
) -> Result<i32, wasmi::Error> {
    let native = active(&caller, token)?;
    if native.capabilities & CAP_CONTEXT == 0
        || len < 0
        || len as usize != native.context.len()
        || len as usize > MAX_NATIVE_CONTEXT_COPY_BYTES.saturating_sub(native.stats.context_bytes)
    {
        return Err(fault("invalid native context copy"));
    }
    let destination = range(pointer, len)?;
    charge_fuel(&mut caller, len as u64)?;
    let memory = guest_memory(&caller)?;
    let (bytes, state) = memory.data_and_store_mut(&mut caller);
    let target = bytes
        .get_mut(destination)
        .ok_or_else(|| fault("native context pointer out of bounds"))?;
    let native = state
        .native
        .as_mut()
        .ok_or_else(|| fault("native reads unavailable"))?;
    target.copy_from_slice(&native.context);
    native.stats.context_bytes += len as usize;
    Ok(len)
}

fn query(
    mut caller: Caller<'_, HostState>,
    token: i64,
    pointer: i32,
    len: i32,
) -> Result<i32, wasmi::Error> {
    let native = active(&caller, token)?;
    if len <= 0
        || len as usize > MAX_NATIVE_CALL_BYTES
        || native.pending.is_some()
        || native.result.is_some()
        || native.stats.native_calls >= MAX_NATIVE_CALLS
        || len as usize > MAX_NATIVE_TOTAL_REQUEST_BYTES.saturating_sub(native.stats.request_bytes)
    {
        return Err(fault("native query limit or state violation"));
    }
    let source = range(pointer, len)?;
    charge_fuel(&mut caller, len as u64)?;
    let memory = guest_memory(&caller)?;
    let bytes = memory
        .data(&caller)
        .get(source)
        .ok_or_else(|| fault("native query pointer out of bounds"))?;
    let decoded = NativeQuery::decode(bytes);
    let native = caller
        .data_mut()
        .native
        .as_mut()
        .ok_or_else(|| fault("native reads unavailable"))?;
    native.stats.native_calls += 1;
    native.stats.request_bytes += len as usize;
    let handle = native.stats.native_calls as i32;
    let request = match decoded {
        Ok(request) => request,
        Err(NativeDecodeError::InvalidValue) => {
            let reply = error_reply(
                NativeErrorCode::InvalidArgument,
                "Invalid native query argument",
            )
            .map_err(|_| fault("native response encoding failed"))?;
            install_reply(native, handle, reply)
                .map_err(|_| fault("native response byte limit exceeded"))?;
            return Ok(handle);
        }
        Err(_) => return Err(fault("invalid native query encoding")),
    };
    if native.capabilities & request.capability() == 0 {
        let reply = error_reply(
            NativeErrorCode::Unsupported,
            "Native query capability denied",
        )
        .map_err(|_| fault("native response encoding failed"))?;
        install_reply(native, handle, reply)
            .map_err(|_| fault("native response byte limit exceeded"))?;
        return Ok(handle);
    }
    let identity = InvocationIdentity {
        instance_id: native.instance_id,
        callback_id: token as u64,
        call_id: native.stats.native_calls,
    };
    native.pending = Some(PendingQuery {
        identity,
        query: request,
    });
    Err(wasmi::Error::host(ReadYield(identity)))
}

fn install_reply(
    native: &mut NativeState,
    handle: i32,
    owner: Box<dyn ReadReply>,
) -> Result<(), PluginError> {
    let len = owner.bytes().len();
    if native.result.is_some()
        || len == 0
        || len > MAX_NATIVE_RESULT_BYTES
        || len > MAX_NATIVE_TOTAL_BYTES.saturating_sub(native.stats.response_bytes)
    {
        return Err(PluginError::NativeProtocol(
            "native response byte limit exceeded",
        ));
    }
    native.stats.response_bytes += len;
    native.result = Some(ReadyReply { handle, len, owner });
    Ok(())
}
fn error_reply(
    code: NativeErrorCode,
    message: &'static str,
) -> Result<Box<dyn ReadReply>, PluginError> {
    let response = NativeQueryResponse::Error {
        error: NativeQueryError::new(code, message),
    };
    let mut writer = BoundedWriter {
        bytes: Vec::new(),
        limit: MAX_NATIVE_RESULT_BYTES,
    };
    serde_json::to_writer(&mut writer, &response)?;
    Ok(Box::new(writer.bytes))
}
fn result_len(caller: Caller<'_, HostState>, token: i64, handle: i32) -> Result<i32, wasmi::Error> {
    let native = active(&caller, token)?;
    let result = native
        .result
        .as_ref()
        .filter(|result| result.handle == handle)
        .ok_or_else(|| fault("invalid native result handle"))?;
    Ok(result.len as i32)
}
fn result_read(
    mut caller: Caller<'_, HostState>,
    token: i64,
    handle: i32,
    pointer: i32,
    len: i32,
) -> Result<i32, wasmi::Error> {
    let native = active(&caller, token)?;
    let result = native
        .result
        .as_ref()
        .filter(|result| result.handle == handle)
        .ok_or_else(|| fault("invalid native result handle"))?;
    if len <= 0 || len as usize != result.len {
        return Err(fault("invalid native result copy length"));
    }
    let destination = range(pointer, len)?;
    charge_fuel(&mut caller, len as u64)?;
    let memory = guest_memory(&caller)?;
    let (bytes, state) = memory.data_and_store_mut(&mut caller);
    let target = bytes
        .get_mut(destination)
        .ok_or_else(|| fault("native result pointer out of bounds"))?;
    let native = state
        .native
        .as_mut()
        .ok_or_else(|| fault("native reads unavailable"))?;
    let result = native
        .result
        .as_ref()
        .ok_or_else(|| fault("native result unavailable"))?;
    let source = result.owner.bytes();
    if source.len() != target.len() {
        return Err(fault("native response owner changed its buffer"));
    }
    target.copy_from_slice(source);
    native.result = None;
    Ok(len)
}
