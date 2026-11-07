use crate::java_launch::{JavaLaunchError, JavaLaunchPath};
use crate::plugin_services::{
    MAX_NATIVE_CALL_BYTES, MAX_NATIVE_CALLS, MAX_NATIVE_RESULT_BYTES, MAX_NATIVE_TOTAL_BYTES,
    NativeCallContext, NativeErrorCode, NativeEventContext, NativeQuery, NativeQueryError,
    NativeQueryService, NativeQueryValue,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout_at};
use zerqon_plugin_api::{Action, Event};

const MAX_REQUEST_BYTES: usize = 65_536;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct JavaPluginConfig {
    pub java: PathBuf,
    pub host_jar: PathBuf,
    pub gson_jar: PathBuf,
    pub extra_classpath: Vec<PathBuf>,
    pub directory: PathBuf,
    pub server_version: String,
    pub request_timeout: Duration,
    pub startup_timeout: Duration,
    pub queue_capacity: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JavaPluginResult {
    pub name: String,
    pub enabled: bool,
    pub actions: Vec<Action>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum JavaPluginError {
    #[error("invalid Java plugin configuration: {0}")]
    Configuration(&'static str),
    #[error("Java plugin request exceeds its size limit")]
    RequestTooLarge,
    #[error("Java plugin request queue is full")]
    QueueFull,
    #[error("Java plugin host has stopped")]
    Stopped,
    #[error("Java plugin host request timed out")]
    Timeout,
    #[error("invalid Java plugin host response: {0}")]
    Protocol(&'static str),
    #[error("Java plugin host rejected the request")]
    Rejected,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Launch(#[from] JavaLaunchError),
}

pub struct JavaPlugins {
    sender: mpsc::Sender<Request>,
    cancel: Option<oneshot::Sender<()>>,
    sequence: AtomicU64,
    request_timeout: Duration,
    startup_results: Vec<JavaPluginResult>,
}

#[derive(Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Operation {
    Hello {
        min_protocol: u32,
        max_protocol: u32,
    },
    Load {
        directory: String,
        server_version: String,
    },
    Event {
        event: Event,
        #[serde(skip_serializing_if = "Option::is_none")]
        plugin: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<NativeEventContext>,
    },
    List,
    Shutdown,
}

#[derive(Serialize)]
struct Envelope {
    id: u64,
    #[serde(flatten)]
    operation: Operation,
}

struct Request {
    id: u64,
    bytes: Vec<u8>,
    kind: RequestKind,
    deadline: Instant,
    reply: oneshot::Sender<Result<Vec<JavaPluginResult>, JavaPluginError>>,
}

enum RequestKind {
    Hello,
    Load,
    Event { plugin: Option<String> },
    List,
    Shutdown,
}

impl RequestKind {
    fn of(operation: &Operation) -> Self {
        match operation {
            Operation::Hello { .. } => Self::Hello,
            Operation::Load { .. } => Self::Load,
            Operation::Event { plugin, .. } => Self::Event {
                plugin: plugin.clone(),
            },
            Operation::List => Self::List,
            Operation::Shutdown => Self::Shutdown,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    id: u64,
    ok: bool,
    #[serde(default)]
    results: Vec<JavaPluginResult>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HelloResponse {
    id: u64,
    ok: bool,
    results: Vec<JavaPluginResult>,
    protocol: u32,
    capabilities: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum HostFrame {
    Response(Response),
    NativeCall(NativeCall),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum NativeCallKind {
    NativeCall,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeCall {
    kind: NativeCallKind,
    parent_id: u64,
    call_id: u64,
    plugin: String,
    operation: NativeQuery,
}

#[derive(Serialize)]
struct NativeResult {
    kind: &'static str,
    parent_id: u64,
    call_id: u64,
    result: NativeResultValue,
}

#[derive(Serialize)]
#[serde(untagged)]
enum NativeResultValue {
    Success { ok: bool, value: NativeQueryValue },
    Error { ok: bool, error: NativeQueryError },
}

struct NativeHost {
    service: Arc<dyn NativeQueryService>,
    available: HashSet<String>,
    known: HashSet<String>,
    active: HashSet<String>,
    negotiated: bool,
}

impl NativeHost {
    fn permits(&self, kind: &RequestKind, plugin: &str) -> bool {
        self.negotiated
            && valid_plugin_name(plugin)
            && match kind {
                RequestKind::Load => self.available.contains(plugin),
                RequestKind::Event { .. } => self.active.contains(plugin),
                RequestKind::Shutdown => self.active.contains(plugin),
                RequestKind::Hello | RequestKind::List => false,
            }
    }
}

impl JavaPlugins {
    pub async fn spawn_with_services(
        config: JavaPluginConfig,
        services: Arc<dyn NativeQueryService>,
    ) -> Result<Self, JavaPluginError> {
        Self::spawn_internal(config, Some(services)).await
    }

    async fn spawn_internal(
        config: JavaPluginConfig,
        services: Option<Arc<dyn NativeQueryService>>,
    ) -> Result<Self, JavaPluginError> {
        if !(1..=1024).contains(&config.queue_capacity)
            || config.request_timeout.is_zero()
            || config.startup_timeout.is_zero()
        {
            return Err(JavaPluginError::Configuration(
                "invalid queue capacity or timeout",
            ));
        }
        if !config.host_jar.is_file() || !config.gson_jar.is_file() {
            return Err(JavaPluginError::Configuration(
                "host and Gson JARs must exist",
            ));
        }
        std::fs::create_dir_all(&config.directory)?;
        let negotiate = services.is_some();
        let native = services
            .map(|service| {
                Ok::<_, JavaPluginError>(NativeHost {
                    service,
                    available: plugin_candidates(&config.directory)?,
                    known: HashSet::new(),
                    active: HashSet::new(),
                    negotiated: false,
                })
            })
            .transpose()?;
        let directory = std::fs::canonicalize(&config.directory)?
            .to_str()
            .ok_or(JavaPluginError::Configuration(
                "plugin directory is not UTF-8",
            ))?
            .to_owned();
        let classpath = java_classpath(&config)?;
        let java = resolve_java(&config.java, config.startup_timeout).await?;
        let mut child = Command::new(java)
            .args([
                "-Dfile.encoding=UTF-8",
                "-Dsun.stdout.encoding=UTF-8",
                "-Dsun.stderr.encoding=UTF-8",
                "-Xms16M",
                "-Xmx128M",
                "-cp",
            ])
            .arg(&classpath.argument)
            .arg("io.github.teighto.zerqon.host.PluginHost")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or(JavaPluginError::Protocol("missing host stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(JavaPluginError::Protocol("missing host stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or(JavaPluginError::Protocol("missing host stderr"))?;
        let (sender, receiver) = mpsc::channel(config.queue_capacity);
        let (cancel, cancelled) = oneshot::channel();
        tokio::spawn(async move {
            let _classpath = classpath;
            let stderr_task = StderrTask(tokio::spawn(drain_stderr(stderr)));
            tokio::select! {
                () = run_host(&mut child, stdin, stdout, receiver, native) => {}
                _ = cancelled => terminate(&mut child).await,
            }
            drop(stderr_task);
        });
        let mut plugins = Self {
            sender,
            cancel: Some(cancel),
            sequence: AtomicU64::new(1),
            request_timeout: config.request_timeout,
            startup_results: Vec::new(),
        };
        if negotiate {
            plugins
                .request(
                    Operation::Hello {
                        min_protocol: 2,
                        max_protocol: 2,
                    },
                    config.startup_timeout,
                )
                .await?;
        }
        plugins.startup_results = plugins
            .request(
                Operation::Load {
                    directory,
                    server_version: config.server_version,
                },
                config.startup_timeout,
            )
            .await?;
        Ok(plugins)
    }

    pub async fn dispatch(
        &self,
        event: Event,
        plugin: Option<String>,
    ) -> Result<Vec<JavaPluginResult>, JavaPluginError> {
        if plugin.as_ref().is_some_and(|name| !valid_plugin_name(name)) {
            return Err(JavaPluginError::Configuration("invalid plugin name"));
        }
        self.request(
            Operation::Event {
                event,
                plugin,
                context: None,
            },
            self.request_timeout,
        )
        .await
    }

    pub async fn dispatch_with_context(
        &self,
        event: Event,
        plugin: Option<String>,
        context: NativeEventContext,
    ) -> Result<Vec<JavaPluginResult>, JavaPluginError> {
        if !context.matches_event(&event)
            || plugin.as_ref().is_some_and(|name| !valid_plugin_name(name))
        {
            return Err(JavaPluginError::Configuration(
                "invalid native event context",
            ));
        }
        self.request(
            Operation::Event {
                event,
                plugin,
                context: Some(context),
            },
            self.request_timeout,
        )
        .await
    }
    pub async fn list(&self) -> Result<Vec<JavaPluginResult>, JavaPluginError> {
        self.request(Operation::List, self.request_timeout).await
    }
    pub async fn shutdown(&self) -> Result<Vec<JavaPluginResult>, JavaPluginError> {
        self.request(Operation::Shutdown, self.request_timeout)
            .await
    }
    pub fn take_startup_results(&mut self) -> Vec<JavaPluginResult> {
        std::mem::take(&mut self.startup_results)
    }

    async fn request(
        &self,
        operation: Operation,
        duration: Duration,
    ) -> Result<Vec<JavaPluginResult>, JavaPluginError> {
        let kind = RequestKind::of(&operation);
        let id = self
            .sequence
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
                (id < i64::MAX as u64).then_some(id + 1)
            })
            .map_err(|_| JavaPluginError::Stopped)?;
        let mut writer = LimitedWriter {
            bytes: Vec::new(),
            limit: MAX_REQUEST_BYTES - 1,
        };
        serde_json::to_writer(&mut writer, &Envelope { id, operation })
            .map_err(|_| JavaPluginError::RequestTooLarge)?;
        writer.bytes.push(b'\n');
        let deadline = Instant::now() + duration;
        let (reply, receiver) = oneshot::channel();
        self.sender
            .try_send(Request {
                id,
                bytes: writer.bytes,
                kind,
                deadline,
                reply,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => JavaPluginError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => JavaPluginError::Stopped,
            })?;
        timeout_at(deadline, receiver)
            .await
            .map_err(|_| JavaPluginError::Timeout)?
            .map_err(|_| JavaPluginError::Stopped)?
    }
}

fn java_classpath(config: &JavaPluginConfig) -> Result<JavaLaunchPath, JavaPluginError> {
    if config.extra_classpath.len() > 64 {
        return Err(JavaPluginError::Configuration(
            "extra classpath exceeds 64 JARs",
        ));
    }
    let mut paths = Vec::with_capacity(config.extra_classpath.len() + 2);
    let mut total = 0_u64;
    for path in std::iter::once(&config.host_jar)
        .chain(&config.extra_classpath)
        .chain(std::iter::once(&config.gson_jar))
    {
        let canonical = std::fs::canonicalize(path)?;
        let metadata = canonical.metadata()?;
        if !metadata.is_file()
            || metadata.len() > 40 * 1024 * 1024
            || canonical
                .extension()
                .is_none_or(|extension| extension != "jar")
        {
            return Err(JavaPluginError::Configuration(
                "classpath entries must be regular JAR files of at most 40 MiB",
            ));
        }
        total += metadata.len();
        if total > 256 * 1024 * 1024 {
            return Err(JavaPluginError::Configuration("classpath exceeds 256 MiB"));
        }
        paths.push(canonical);
    }
    Ok(crate::java_launch::classpath(&paths)?)
}

fn plugin_candidates(directory: &std::path::Path) -> Result<HashSet<String>, JavaPluginError> {
    let mut candidates = HashSet::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file()
            || entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "jar")
        {
            continue;
        }
        let name = entry
            .path()
            .file_stem()
            .and_then(|name| name.to_str())
            .filter(|name| valid_plugin_name(name))
            .ok_or(JavaPluginError::Configuration(
                "invalid Java plugin file name",
            ))?
            .to_owned();
        candidates.insert(name);
        if candidates.len() > 64 {
            return Err(JavaPluginError::Configuration("Java plugin limit is 64"));
        }
    }
    Ok(candidates)
}

impl Drop for JavaPlugins {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

struct StderrTask(JoinHandle<()>);
impl Drop for StderrTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn run_host(
    child: &mut Child,
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    mut receiver: mpsc::Receiver<Request>,
    mut native: Option<NativeHost>,
) {
    let mut stdout = BufReader::with_capacity(8192, stdout);
    while let Some(mut request) = receiver.recv().await {
        if request.reply.is_closed() {
            continue;
        }
        if Instant::now() >= request.deadline {
            let _ = request.reply.send(Err(JavaPluginError::Timeout));
            continue;
        }
        let exchange = async {
            stdin.write_all(&request.bytes).await?;
            stdin.flush().await?;
            if matches!(request.kind, RequestKind::Hello) {
                let line = read_line_limited(&mut stdout, MAX_REQUEST_BYTES).await?;
                let hello: HelloResponse = serde_json::from_slice(&line)
                    .map_err(|_| JavaPluginError::Protocol("invalid protocol negotiation"))?;
                let native = native.as_mut().ok_or(JavaPluginError::Protocol(
                    "native query services are unavailable",
                ))?;
                if hello.id != request.id
                    || !hello.ok
                    || !hello.results.is_empty()
                    || hello.protocol != 2
                    || hello.capabilities != ["native_queries"]
                    || native.negotiated
                {
                    return Err(JavaPluginError::Protocol(
                        "unsupported protocol negotiation",
                    ));
                }
                native.negotiated = true;
                return Ok(Vec::new());
            }
            let mut calls = 0;
            let mut response_bytes = 0;
            loop {
                let line = read_line_limited(&mut stdout, MAX_RESPONSE_BYTES).await?;
                let frame: HostFrame = serde_json::from_slice(&line)
                    .map_err(|_| JavaPluginError::Protocol("invalid JSON envelope"))?;
                match frame {
                    HostFrame::Response(response) => {
                        let results = validate_response(response, request.id)?;
                        if let Some(native) = &mut native {
                            for result in &results {
                                let allowed = if matches!(request.kind, RequestKind::Load) {
                                    &native.available
                                } else {
                                    &native.known
                                };
                                if !allowed.contains(&result.name) {
                                    return Err(JavaPluginError::Protocol("unknown plugin result"));
                                }
                                if let RequestKind::Event {
                                    plugin: Some(target),
                                } = &request.kind
                                    && result.name != *target
                                {
                                    return Err(JavaPluginError::Protocol("wrong plugin result"));
                                }
                            }
                            for result in &results {
                                native.known.insert(result.name.clone());
                                if result.enabled {
                                    native.active.insert(result.name.clone());
                                } else {
                                    native.active.remove(&result.name);
                                }
                            }
                        }
                        return Ok(results);
                    }
                    HostFrame::NativeCall(call) => {
                        let _ = call.kind;
                        let native = native.as_ref().ok_or(JavaPluginError::Protocol(
                            "native query before protocol negotiation",
                        ))?;
                        if line.len() > MAX_NATIVE_CALL_BYTES
                            || calls >= MAX_NATIVE_CALLS
                            || call.parent_id != request.id
                            || call.call_id != calls + 1
                            || !native.permits(&request.kind, &call.plugin)
                        {
                            return Err(JavaPluginError::Protocol("invalid native query frame"));
                        }
                        calls += 1;
                        let deadline = request
                            .deadline
                            .min(Instant::now() + Duration::from_millis(250));
                        let context = NativeCallContext {
                            plugin: call.plugin,
                            parent_id: request.id,
                            call_id: call.call_id,
                            deadline,
                        };
                        let query = call.operation;
                        let queried = if query.valid() {
                            timeout_at(deadline, native.service.query(context, query.clone()))
                                .await
                                .unwrap_or_else(|_| {
                                    Err(NativeQueryError::new(
                                        NativeErrorCode::DeadlineExceeded,
                                        "Native query deadline exceeded",
                                    ))
                                })
                        } else {
                            Err(NativeQueryError::new(
                                NativeErrorCode::InvalidArgument,
                                "Invalid native query arguments",
                            ))
                        };
                        let result = match queried {
                            Ok(value) if value.valid_for(&query) => {
                                NativeResultValue::Success { ok: true, value }
                            }
                            Err(error) if error.valid() => {
                                NativeResultValue::Error { ok: false, error }
                            }
                            _ => NativeResultValue::Error {
                                ok: false,
                                error: NativeQueryError::new(
                                    NativeErrorCode::Internal,
                                    "Native query service returned an invalid result",
                                ),
                            },
                        };
                        let mut writer = LimitedWriter {
                            bytes: Vec::new(),
                            limit: MAX_NATIVE_RESULT_BYTES,
                        };
                        let encoded = serde_json::to_writer(
                            &mut writer,
                            &NativeResult {
                                kind: "native_result",
                                parent_id: request.id,
                                call_id: call.call_id,
                                result,
                            },
                        );
                        if encoded.is_err() {
                            writer.bytes.clear();
                            serde_json::to_writer(
                                &mut writer,
                                &NativeResult {
                                    kind: "native_result",
                                    parent_id: request.id,
                                    call_id: call.call_id,
                                    result: NativeResultValue::Error {
                                        ok: false,
                                        error: NativeQueryError::new(
                                            NativeErrorCode::Capacity,
                                            "Native query result exceeds its byte limit",
                                        ),
                                    },
                                },
                            )
                            .map_err(|_| {
                                JavaPluginError::Protocol("native query error exceeds limit")
                            })?;
                        }
                        response_bytes += writer.bytes.len();
                        if response_bytes > MAX_NATIVE_TOTAL_BYTES {
                            return Err(JavaPluginError::Protocol("native query budget exceeded"));
                        }
                        writer.bytes.push(b'\n');
                        stdin.write_all(&writer.bytes).await?;
                        stdin.flush().await?;
                    }
                }
            }
        };
        let result = tokio::select! {
            result = timeout_at(request.deadline, exchange) => result.unwrap_or(Err(JavaPluginError::Timeout)),
            _ = request.reply.closed() => Err(JavaPluginError::Stopped),
        };
        let fatal = matches!(result, Err(ref error) if !matches!(error, JavaPluginError::Rejected));
        if fatal {
            terminate(child).await;
            let _ = request.reply.send(result);
            return;
        }
        if matches!(request.kind, RequestKind::Shutdown) {
            drop(stdin);
            let result = match timeout_at(request.deadline, child.wait()).await {
                Ok(Ok(status)) if status.success() => result,
                Ok(Ok(_)) => Err(JavaPluginError::Protocol("host exited unsuccessfully")),
                Ok(Err(error)) => Err(error.into()),
                Err(_) => {
                    terminate(child).await;
                    Err(JavaPluginError::Timeout)
                }
            };
            let _ = request.reply.send(result);
            return;
        }
        let _ = request.reply.send(result);
    }
    terminate(child).await;
}

async fn terminate(child: &mut Child) {
    match tokio::time::timeout(Duration::from_secs(2), child.kill()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(kind = ?error.kind(), "Java host termination failed"),
        Err(_) => tracing::warn!("Java host termination timed out"),
    }
}

fn validate_response(
    response: Response,
    id: u64,
) -> Result<Vec<JavaPluginResult>, JavaPluginError> {
    if response.id != id {
        return Err(JavaPluginError::Protocol("request ID mismatch"));
    }
    if response
        .error
        .as_ref()
        .is_some_and(|error| error.len() > 1024)
    {
        return Err(JavaPluginError::Protocol("error exceeds its size limit"));
    }
    if !response.ok {
        return Err(JavaPluginError::Rejected);
    }
    if response.results.len() > 64 {
        return Err(JavaPluginError::Protocol("too many plugin results"));
    }
    let mut names = std::collections::HashSet::new();
    for result in &response.results {
        if !valid_plugin_name(&result.name) || !names.insert(&result.name) {
            return Err(JavaPluginError::Protocol(
                "invalid or duplicate plugin identity",
            ));
        }
        if result
            .error
            .as_ref()
            .is_some_and(|error| error.len() > 1024)
            || result.actions.len() > 32
        {
            return Err(JavaPluginError::Protocol(
                "plugin result exceeds its limits",
            ));
        }
        for action in &result.actions {
            let valid = match action {
                Action::Log { message, .. } | Action::Reply { message } => message.len() <= 4096,
                Action::RegisterCommand {
                    name,
                    description,
                    permission,
                } => {
                    valid_name(name)
                        && description.len() <= 256
                        && permission.as_ref().is_none_or(|permission| {
                            !permission.is_empty() && permission.len() <= 128
                        })
                }
            };
            if !valid {
                return Err(JavaPluginError::Protocol(
                    "plugin action exceeds its limits",
                ));
            }
        }
        let mut writer = LimitedWriter {
            bytes: Vec::new(),
            limit: 65_536,
        };
        serde_json::to_writer(&mut writer, &result.actions)
            .map_err(|_| JavaPluginError::Protocol("plugin action payload exceeds its limit"))?;
    }
    Ok(response.results)
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn valid_plugin_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

async fn resolve_java(
    java: &std::path::Path,
    duration: Duration,
) -> Result<PathBuf, JavaPluginError> {
    let mut child = Command::new(java)
        .args([
            "-Dfile.encoding=UTF-8",
            "-Dsun.stdout.encoding=UTF-8",
            "-Dsun.stderr.encoding=UTF-8",
            "-XshowSettings:properties",
            "-version",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stderr = child.stderr.take().ok_or(JavaPluginError::Configuration(
        "Java runtime probe has no stderr",
    ))?;
    let result = tokio::time::timeout(duration, async {
        let mut bytes = Vec::new();
        stderr.take(65_537).read_to_end(&mut bytes).await?;
        if bytes.len() > 65_536 {
            return Err(JavaPluginError::Configuration(
                "Java runtime probe output exceeds limit",
            ));
        }
        let status = child.wait().await?;
        if !status.success() {
            return Err(JavaPluginError::Configuration("Java runtime probe failed"));
        }
        let output = std::str::from_utf8(&bytes)
            .map_err(|_| JavaPluginError::Configuration("Java runtime properties are not UTF-8"))?;
        let home = output
            .lines()
            .find_map(|line| line.trim().strip_prefix("java.home = "))
            .ok_or(JavaPluginError::Configuration(
                "Java runtime home was not reported",
            ))?;
        let version = output
            .lines()
            .find_map(|line| line.trim().strip_prefix("java.specification.version = "))
            .and_then(|version| version.parse::<u32>().ok())
            .ok_or(JavaPluginError::Configuration(
                "Java runtime version was not reported",
            ))?;
        if version < 25 {
            return Err(JavaPluginError::Configuration(
                "Java 25 or newer is required",
            ));
        }
        let executable =
            PathBuf::from(home)
                .join("bin")
                .join(if cfg!(windows) { "java.exe" } else { "java" });
        if !executable.is_file() {
            return Err(JavaPluginError::Configuration(
                "Java runtime executable does not exist",
            ));
        }
        Ok(executable)
    })
    .await;
    match result {
        Ok(Ok(executable)) => Ok(executable),
        Ok(Err(error)) => {
            terminate(&mut child).await;
            Err(error)
        }
        Err(_) => {
            terminate(&mut child).await;
            Err(JavaPluginError::Timeout)
        }
    }
}

struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
}
impl std::io::Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("JSON size limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn read_line_limited<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Vec<u8>, JavaPluginError> {
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return Err(if line.is_empty() {
                JavaPluginError::Stopped
            } else {
                JavaPluginError::Protocol("unterminated response")
            });
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(buffer.len());
        if count > limit.saturating_sub(line.len()) {
            return Err(JavaPluginError::Protocol("response exceeds its size limit"));
        }
        line.extend_from_slice(&buffer[..count]);
        reader.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(line);
        }
    }
}

#[derive(Default)]
struct StderrDecoder {
    pending: [u8; 3],
    pending_len: usize,
}

impl StderrDecoder {
    fn push(&mut self, bytes: &[u8]) -> String {
        let mut input = Vec::with_capacity(self.pending_len + bytes.len());
        input.extend_from_slice(&self.pending[..self.pending_len]);
        input.extend_from_slice(bytes);
        self.pending_len = 0;
        let mut remaining = input.as_slice();
        let mut decoded = String::with_capacity(input.len());
        loop {
            match std::str::from_utf8(remaining) {
                Ok(valid) => {
                    decoded.push_str(valid);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    decoded.push_str(&String::from_utf8_lossy(&remaining[..valid]));
                    if let Some(length) = error.error_len() {
                        decoded.push('\u{fffd}');
                        remaining = &remaining[valid + length..];
                    } else {
                        let suffix = &remaining[valid..];
                        if let Some(pending) = self.pending.get_mut(..suffix.len()) {
                            pending.copy_from_slice(suffix);
                            self.pending_len = suffix.len();
                        } else {
                            decoded.push_str(&String::from_utf8_lossy(suffix));
                        }
                        break;
                    }
                }
            }
        }
        decoded
    }
    fn finish(&mut self) -> String {
        let decoded = String::from_utf8_lossy(&self.pending[..self.pending_len]).into_owned();
        self.pending_len = 0;
        decoded
    }
}

async fn drain_stderr(mut stderr: tokio::process::ChildStderr) {
    let mut buffer = [0; 4096];
    let mut decoder = StderrDecoder::default();
    let mut window = Instant::now();
    let mut logged = 0;
    loop {
        let (decoded, finished) = match stderr.read(&mut buffer).await {
            Ok(0) => (decoder.finish(), true),
            Ok(count) => (decoder.push(&buffer[..count]), false),
            Err(error) => {
                tracing::warn!(kind = ?error.kind(), "Java host stderr read failed");
                (decoder.finish(), true)
            }
        };
        if window.elapsed() >= Duration::from_secs(1) {
            window = Instant::now();
            logged = 0;
        }
        if logged < 10 && !decoded.is_empty() {
            let message: String = decoded
                .chars()
                .take(1024)
                .map(|character| {
                    if character.is_control() {
                        ' '
                    } else {
                        character
                    }
                })
                .collect();
            tracing::debug!(target:"zerqon::java",stderr=%message);
            logged += 1;
        }
        if finished {
            break;
        }
    }
}
