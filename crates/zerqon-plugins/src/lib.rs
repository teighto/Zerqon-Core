use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use wasmi::{
    Caller, Config, EnforcedLimits, Engine, Linker, Memory, Module, Store, StoreLimits,
    StoreLimitsBuilder, TypedFunc,
};
use zerqon_plugin_api::{ABI_VERSION, Action, Event, MAX_EVENT_BYTES};

pub mod resumable;

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("invalid plugin limits: {0}")]
    Limits(&'static str),
    #[error("invalid plugin name: use 1..64 ASCII letters, digits, underscores or hyphens")]
    Name,
    #[error("plugin already loaded: {0}")]
    Duplicate(String),
    #[error("plugin count limit reached")]
    Count,
    #[error("plugin module exceeds size limit")]
    ModuleSize,
    #[error("plugin event exceeds size limit")]
    EventSize,
    #[error("native read imports require a resumable driver")]
    NativeDriverRequired,
    #[error("invalid native callback context")]
    CallbackContext,
    #[error("invalid native plugin protocol: {0}")]
    NativeProtocol(&'static str),
    #[error("plugin callback was cancelled")]
    Cancelled,
    #[error("plugin callback deadline exceeded")]
    Deadline,
    #[error("plugin callback fuel exhausted")]
    FuelExhausted,
    #[error("invalid plugin lifecycle transition")]
    Lifecycle,
    #[error("plugin not found: {0}")]
    NotFound(String),
    #[error("plugin is disabled: {0}")]
    Disabled(String),
    #[error("unsupported plugin ABI {0}; expected {ABI_VERSION}")]
    Abi(u32),
    #[error("plugin must export 32-bit linear memory as 'memory'")]
    Memory,
    #[error("plugin handler returned error code {0}")]
    Handler(i32),
    #[error("WebAssembly: {0}")]
    Wasm(#[from] wasmi::Error),
    #[error("WebAssembly linker: {0}")]
    Linker(#[from] wasmi::errors::LinkerError),
    #[error("plugin JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("plugin file: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub memory_bytes: usize,
    pub fuel_per_event: u64,
    pub module_bytes: usize,
    pub event_bytes: usize,
    pub output_bytes: usize,
    pub actions_per_event: usize,
    pub plugins: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 16 * 1024 * 1024,
            fuel_per_event: 1_000_000,
            module_bytes: 4 * 1024 * 1024,
            event_bytes: 65_536,
            output_bytes: 65_536,
            actions_per_event: 32,
            plugins: 64,
        }
    }
}

struct HostState {
    limits: StoreLimits,
    event: Vec<u8>,
    actions: Vec<Action>,
    output_bytes: usize,
    copied_bytes: usize,
    output_limit: usize,
    copy_limit: usize,
    action_limit: usize,
    kind: EventKind,
    native: Option<resumable::NativeState>,
    fuel: Option<resumable::FuelLedger>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EventKind {
    None,
    Enable,
    Command,
    Other,
}

struct Runtime {
    store: Store<HostState>,
    handler: TypedFunc<(), i32>,
}

struct Plugin {
    runtime: Option<Runtime>,
    disabled_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    pub name: String,
    pub enabled: bool,
    pub disabled_reason: Option<String>,
}

#[derive(Debug)]
pub struct DispatchResult {
    pub name: String,
    pub result: Result<Vec<Action>, PluginError>,
}

#[derive(Debug)]
pub struct LoadResult {
    pub path: PathBuf,
    pub result: Result<Vec<Action>, PluginError>,
}

pub struct PluginManager {
    engine: Engine,
    linker: Linker<HostState>,
    limits: Limits,
    plugins: BTreeMap<String, Plugin>,
}

impl PluginManager {
    pub fn new(limits: Limits) -> Result<Self, PluginError> {
        validate_limits(&limits)?;
        let mut config = Config::default();
        config
            .consume_fuel(true)
            .enforced_limits(EnforcedLimits::strict())
            .allow_start_fn(false)
            .set_max_recursion_depth(256)
            .set_min_stack_height(4096)
            .set_max_stack_height(1024 * 1024)
            .set_max_cached_stacks(2);
        let engine = Engine::new(&config);
        let mut linker = Linker::new(&engine);
        linker.func_wrap(
            "zerqon",
            "event_len",
            |caller: Caller<'_, HostState>| -> i32 { caller.data().event.len() as i32 },
        )?;
        linker.func_wrap("zerqon", "event_read", read_event)?;
        linker.func_wrap("zerqon", "emit", emit_action)?;
        Ok(Self {
            engine,
            linker,
            limits,
            plugins: BTreeMap::new(),
        })
    }

    /// Instantiates the plugin, checks its ABI and invokes `Enable` before publishing it.
    pub fn load(&mut self, name: &str, bytes: &[u8]) -> Result<Vec<Action>, PluginError> {
        if !valid_name(name) {
            return Err(PluginError::Name);
        }
        if self.plugins.contains_key(name) {
            return Err(PluginError::Duplicate(name.to_owned()));
        }
        if self.plugins.len() >= self.limits.plugins {
            return Err(PluginError::Count);
        }
        if bytes.len() > self.limits.module_bytes {
            return Err(PluginError::ModuleSize);
        }
        let module = Module::new(&self.engine, bytes)?;
        if module
            .imports()
            .any(|import| import.module() == "zerqon_read_v1")
        {
            return Err(PluginError::NativeDriverRequired);
        }
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
            native: None,
            fuel: None,
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        store.set_fuel(self.limits.fuel_per_event)?;
        let instance = self.linker.instantiate_and_start(&mut store, &module)?;
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
        let mut runtime = Runtime { store, handler };
        let actions = runtime.dispatch(
            &Event::Enable {
                server_version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            &self.limits,
        )?;
        self.plugins.insert(
            name.to_owned(),
            Plugin {
                runtime: Some(runtime),
                disabled_reason: None,
            },
        );
        Ok(actions)
    }

    /// Loads `.wasm` files in deterministic order. Files are read with a hard size cap.
    pub fn load_directory(&mut self, directory: &Path) -> Result<Vec<LoadResult>, PluginError> {
        std::fs::create_dir_all(directory)?;
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && entry.path().extension().is_some_and(|ext| ext == "wasm")
            {
                paths.push(entry.path());
            }
            if paths.len() > self.limits.plugins {
                return Err(PluginError::Count);
            }
        }
        paths.sort();
        Ok(paths
            .into_iter()
            .map(|path| {
                let result = self.load_file(&path);
                LoadResult { path, result }
            })
            .collect())
    }

    fn load_file(&mut self, path: &Path) -> Result<Vec<Action>, PluginError> {
        let name = path
            .file_stem()
            .and_then(|name| name.to_str())
            .ok_or(PluginError::Name)?;
        let file = File::open(path)?;
        if file.metadata()?.len() > self.limits.module_bytes as u64 {
            return Err(PluginError::ModuleSize);
        }
        let mut bytes = Vec::new();
        file.take(self.limits.module_bytes as u64 + 1)
            .read_to_end(&mut bytes)?;
        self.load(name, &bytes)
    }

    /// Guest faults disable the plugin and release its linear memory. Oversized host input does not.
    pub fn dispatch(&mut self, name: &str, event: &Event) -> Result<Vec<Action>, PluginError> {
        let plugin = self
            .plugins
            .get_mut(name)
            .ok_or_else(|| PluginError::NotFound(name.to_owned()))?;
        let runtime = plugin
            .runtime
            .as_mut()
            .ok_or_else(|| PluginError::Disabled(name.to_owned()))?;
        let result = runtime.dispatch(event, &self.limits);
        if let Err(error) = &result
            && !matches!(error, PluginError::EventSize)
        {
            plugin.disabled_reason = Some(error.to_string());
            plugin.runtime = None;
        }
        result
    }

    pub fn dispatch_all(&mut self, event: &Event) -> Vec<DispatchResult> {
        let names: Vec<_> = self
            .plugins
            .iter()
            .filter(|(_, plugin)| plugin.runtime.is_some())
            .map(|(name, _)| name.clone())
            .collect();
        names
            .into_iter()
            .map(|name| {
                let result = self.dispatch(&name, event);
                DispatchResult { name, result }
            })
            .collect()
    }

    pub fn list(&self) -> Vec<PluginInfo> {
        self.plugins
            .iter()
            .map(|(name, plugin)| PluginInfo {
                name: name.clone(),
                enabled: plugin.runtime.is_some(),
                disabled_reason: plugin.disabled_reason.clone(),
            })
            .collect()
    }

    pub fn unload(&mut self, name: &str) -> Result<Vec<Action>, PluginError> {
        let mut plugin = self
            .plugins
            .remove(name)
            .ok_or_else(|| PluginError::NotFound(name.to_owned()))?;
        match plugin.runtime.as_mut() {
            Some(runtime) => runtime.dispatch(&Event::Disable, &self.limits),
            None => Ok(Vec::new()),
        }
    }
}

impl Runtime {
    fn dispatch(&mut self, event: &Event, limits: &Limits) -> Result<Vec<Action>, PluginError> {
        let mut writer = BoundedWriter {
            bytes: Vec::new(),
            limit: limits.event_bytes,
        };
        if serde_json::to_writer(&mut writer, event).is_err() {
            return Err(PluginError::EventSize);
        }
        self.store.set_fuel(limits.fuel_per_event)?;
        let state = self.store.data_mut();
        state.event = writer.bytes;
        state.actions.clear();
        state.output_bytes = 0;
        state.copied_bytes = 0;
        state.kind = match event {
            Event::Enable { .. } => EventKind::Enable,
            Event::Command { .. } => EventKind::Command,
            _ => EventKind::Other,
        };
        let result = self.handler.call(&mut self.store, ());
        self.store.data_mut().kind = EventKind::None;
        let code = result?;
        if code != 0 {
            return Err(PluginError::Handler(code));
        }
        Ok(std::mem::take(&mut self.store.data_mut().actions))
    }
}

fn validate_limits(limits: &Limits) -> Result<(), PluginError> {
    if limits.memory_bytes < 65_536 || limits.memory_bytes > 1024 * 1024 * 1024 {
        return Err(PluginError::Limits(
            "memory must be between 64 KiB and 1 GiB",
        ));
    }
    if limits.fuel_per_event == 0 || limits.fuel_per_event > 100_000_000 {
        return Err(PluginError::Limits(
            "fuel must be between 1 and 100,000,000",
        ));
    }
    if limits.module_bytes == 0 || limits.module_bytes > 64 * 1024 * 1024 {
        return Err(PluginError::Limits(
            "module size must be between 1 and 64 MiB",
        ));
    }
    if limits.event_bytes == 0
        || limits.event_bytes > MAX_EVENT_BYTES
        || limits.output_bytes == 0
        || limits.output_bytes > MAX_EVENT_BYTES
    {
        return Err(PluginError::Limits(
            "event and output size must be between 1 and 64 KiB",
        ));
    }
    if limits.actions_per_event == 0
        || limits.actions_per_event > 1024
        || limits.plugins == 0
        || limits.plugins > 256
    {
        return Err(PluginError::Limits(
            "actions must be 1..1024 and plugin count 1..256",
        ));
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl std::io::Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("event size limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn guest_memory(caller: &Caller<'_, HostState>) -> Result<Memory, wasmi::Error> {
    caller
        .get_export("memory")
        .and_then(|value| value.into_memory())
        .ok_or_else(|| wasmi::Error::new("missing guest memory"))
}

fn charge_copy(caller: &mut Caller<'_, HostState>, len: usize) -> Result<(), wasmi::Error> {
    let state = caller.data_mut();
    state.copied_bytes = state.copied_bytes.saturating_add(len);
    if state.copied_bytes > state.copy_limit {
        return Err(wasmi::Error::new("plugin I/O limit exceeded"));
    }
    resumable::charge_fuel(caller, len as u64)
}

fn read_event(
    mut caller: Caller<'_, HostState>,
    pointer: i32,
    capacity: i32,
) -> Result<i32, wasmi::Error> {
    resumable::check_control(&caller)?;
    let len = caller.data().event.len();
    if pointer < 0
        || capacity < 0
        || capacity as usize != len
        || caller.data().kind == EventKind::None
    {
        return Err(wasmi::Error::new("invalid event buffer"));
    }
    charge_copy(&mut caller, len)?;
    let memory = guest_memory(&caller)?;
    let (memory, state) = memory.data_and_store_mut(&mut caller);
    let end = (pointer as usize)
        .checked_add(len)
        .ok_or_else(|| wasmi::Error::new("event pointer overflow"))?;
    let target = memory
        .get_mut(pointer as usize..end)
        .ok_or_else(|| wasmi::Error::new("event pointer out of bounds"))?;
    target.copy_from_slice(&state.event);
    Ok(len as i32)
}

fn emit_action(
    mut caller: Caller<'_, HostState>,
    pointer: i32,
    len: i32,
) -> Result<i32, wasmi::Error> {
    resumable::check_control(&caller)?;
    if pointer < 0 || len <= 0 || caller.data().kind == EventKind::None {
        return Err(wasmi::Error::new("invalid output buffer"));
    }
    let len = len as usize;
    let state = caller.data();
    if len > state.output_limit.saturating_sub(state.output_bytes)
        || state.actions.len() >= state.action_limit
    {
        return Err(wasmi::Error::new("plugin output limit exceeded"));
    }
    charge_copy(&mut caller, len)?;
    let memory = guest_memory(&caller)?;
    let end = (pointer as usize)
        .checked_add(len)
        .ok_or_else(|| wasmi::Error::new("output pointer overflow"))?;
    let bytes = memory
        .data(&caller)
        .get(pointer as usize..end)
        .ok_or_else(|| wasmi::Error::new("output pointer out of bounds"))?;
    let action: Action = serde_json::from_slice(bytes)
        .map_err(|error| wasmi::Error::new(format!("invalid plugin action: {error}")))?;
    let state = caller.data_mut();
    match &action {
        Action::RegisterCommand {
            name,
            description,
            permission,
        } => {
            if state.kind != EventKind::Enable
                || !valid_name(name)
                || name != &name.to_ascii_lowercase()
                || description.len() > 256
                || permission
                    .as_ref()
                    .is_some_and(|p| p.len() > 128 || p.is_empty())
            {
                return Err(wasmi::Error::new("invalid command registration"));
            }
        }
        Action::Reply { message } => {
            if state.kind != EventKind::Command {
                return Err(wasmi::Error::new("replies require a command event"));
            }
            if message.len() > 4096 {
                return Err(wasmi::Error::new("reply exceeds 4096 bytes"));
            }
        }
        Action::Log { message, .. } if message.len() > 4096 => {
            return Err(wasmi::Error::new("log message exceeds 4096 bytes"));
        }
        _ => {}
    }
    state.output_bytes += len;
    state.actions.push(action);
    Ok(0)
}

#[cfg(test)]
mod tests;
