use super::*;
use crate::{
    lifecycle::Lifecycle,
    plugin_services::{
        NativeCallContext, NativeErrorCode, NativeQuery, NativeQueryError, NativeQueryFuture,
        NativeQueryService, NativeQueryValue, ServerSnapshot,
    },
};
use std::{
    collections::VecDeque,
    future::{Future, poll_fn},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::Poll,
    time::Duration,
};
use tokio::{
    sync::Semaphore,
    time::{Instant, timeout},
};
use zerqon_plugin_api::native::{NativePosition, NativeQueryResponse};

fn snapshot() -> NativeQueryValue {
    NativeQueryValue::ServerSnapshot(ServerSnapshot {
        server_version: "0.0.1".into(),
        minecraft_version: "26.3".into(),
        protocol: 777,
        max_players: 64,
        online_players: 2,
        tick: 123,
    })
}

struct SequenceProvider {
    calls: AtomicUsize,
    contexts: Mutex<Vec<NativeCallContext>>,
    responses: Mutex<VecDeque<Result<NativeQueryValue, NativeQueryError>>>,
}

impl SequenceProvider {
    fn new(responses: Vec<Result<NativeQueryValue, NativeQueryError>>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            contexts: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into()),
        })
    }
}

impl NativeQueryService for SequenceProvider {
    fn query(&self, context: NativeCallContext, _: NativeQuery) -> NativeQueryFuture<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.contexts.lock().unwrap().push(context);
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(snapshot()));
        Box::pin(async move { response })
    }
}

fn runtime_for(
    provider: Arc<dyn NativeQueryService>,
    bytes: usize,
) -> (NativeQueryRuntime, NativeReadBudget, Lifecycle) {
    let budget = NativeReadBudget::new(bytes).unwrap();
    let lifecycle = Lifecycle::new();
    let runtime = NativeQueryRuntime::new(
        provider,
        budget.clone(),
        lifecycle.clone(),
        NativeProviderContract::future_owned(MAX_PROVIDER_WORKING_SET_BYTES).unwrap(),
    );
    (runtime, budget, lifecycle)
}

fn context(
    lifecycle: &Lifecycle,
    backend: NativeBackend,
    name: &str,
    generation: u64,
) -> NativeScopeContext {
    NativeScopeContext {
        key: NativePluginKey::new(backend, name, generation).unwrap(),
        parent_id: 77,
        deadline: Instant::now() + Duration::from_secs(5),
        stop: lifecycle.token().child(),
        admission: NativeAdmission::Normal,
    }
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    timeout(Duration::from_secs(2), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native query cleanup did not reach its barrier");
}

fn assert_empty(budget: &NativeReadBudget, lifecycle: &Lifecycle) {
    let current = budget.snapshot();
    assert_eq!(current.reserved_bytes, 0);
    assert_eq!(current.active_scopes, 0);
    assert_eq!(current.active_operations, 0);
    assert_eq!(current.quarantined_operations, 0);
    assert!(lifecycle.pending().is_empty());
}

#[test]
fn configuration_and_plugin_keys_reject_unbounded_or_ambiguous_admission() {
    for bytes in [0, WORKSPACE_BYTES - 1, WORKSPACE_BYTES + 1] {
        assert!(NativeReadBudget::new(bytes).is_err());
    }
    assert!(NativeReadBudget::new(DEFAULT_READ_BUDGET_BYTES).is_ok());
    for name in ["", "bad/name", "bad:name", "bad name"] {
        assert!(NativePluginKey::new(NativeBackend::Wasm, name, 1).is_err());
    }
    assert!(NativePluginKey::new(NativeBackend::Java, &"a".repeat(65), 1).is_err());
    assert!(NativePluginKey::new(NativeBackend::Wasm, "valid", 0).is_err());
    assert!(NativeProviderContract::future_owned(0).is_err());
    assert!(NativeProviderContract::future_owned(MAX_PROVIDER_WORKING_SET_BYTES + 1).is_err());
    assert!(NativeProviderContract::future_owned(MAX_PROVIDER_WORKING_SET_BYTES).is_ok());
}

#[test]
fn fixed_pool_and_backend_generation_keys_admit_without_spawning_work() {
    let provider = SequenceProvider::new(Vec::new());
    let (runtime, budget, lifecycle) = runtime_for(provider.clone(), DEFAULT_READ_BUDGET_BYTES);
    let mut scopes = Vec::new();
    for index in 0..DEFAULT_READ_BUDGET_BYTES / WORKSPACE_BYTES {
        scopes.push(
            runtime
                .try_scope(context(
                    &lifecycle,
                    NativeBackend::Wasm,
                    &format!("plugin_{index}"),
                    1,
                ))
                .unwrap(),
        );
    }
    let full = budget.snapshot();
    assert_eq!(full.limit_bytes, DEFAULT_READ_BUDGET_BYTES);
    assert_eq!(full.reserved_bytes, DEFAULT_READ_BUDGET_BYTES);
    assert_eq!(full.peak_bytes, DEFAULT_READ_BUDGET_BYTES);
    assert_eq!(full.active_scopes, scopes.len());
    assert_eq!(full.active_operations, 0);
    assert_eq!(lifecycle.pending().len(), scopes.len());
    assert!(
        runtime
            .try_scope(context(&lifecycle, NativeBackend::Wasm, "overflow", 1))
            .is_err()
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(budget.snapshot().active_scopes, scopes.len());
    drop(scopes.pop());
    assert!(
        runtime
            .try_scope(context(&lifecycle, NativeBackend::Wasm, "plugin_0", 1))
            .is_err()
    );
    let java = runtime
        .try_scope(context(&lifecycle, NativeBackend::Java, "plugin_0", 1))
        .unwrap();
    drop(java);
    let generation = runtime
        .try_scope(context(&lifecycle, NativeBackend::Wasm, "plugin_0", 2))
        .unwrap();
    drop(generation);
    assert!(budget.snapshot().rejections >= 1);
    drop(scopes);
    assert_empty(&budget, &lifecycle);
}

#[tokio::test]
async fn reply_holds_scope_registration_and_memory_after_the_public_scope_is_dropped() {
    for format in [NativeReplyFormat::Wasm, NativeReplyFormat::JavaProtocol2] {
        let java = matches!(&format, NativeReplyFormat::JavaProtocol2);
        let provider = SequenceProvider::new(Vec::new());
        let (runtime, budget, lifecycle) = runtime_for(provider.clone(), WORKSPACE_BYTES);
        let scope = runtime
            .try_scope(context(
                &lifecycle,
                if java {
                    NativeBackend::Java
                } else {
                    NativeBackend::Wasm
                },
                "reply",
                1,
            ))
            .unwrap();
        let hold = scope.hold();
        let reply = scope
            .execute(3, NativeQuery::ServerSnapshot, format)
            .await
            .unwrap();
        assert!(scope.is_busy());
        assert!(!scope.is_terminal());
        if java {
            assert!(reply.bytes().ends_with(b"\n"));
            assert_eq!(reply.json_len() + 1, reply.bytes().len());
            let wire: serde_json::Value = serde_json::from_slice(reply.bytes()).unwrap();
            assert_eq!(wire.as_object().unwrap().len(), 4);
            assert_eq!(wire["kind"], "native_result");
            assert_eq!(wire["parent_id"], 77);
            assert_eq!(wire["call_id"], 3);
            assert_eq!(
                wire["result"],
                serde_json::to_value(NativeQueryResponse::Success { value: snapshot() }).unwrap()
            );
        } else {
            assert!(!reply.bytes().ends_with(b"\n"));
            assert_eq!(reply.json_len(), reply.bytes().len());
            assert_eq!(
                NativeQueryResponse::decode_for(&NativeQuery::ServerSnapshot, reply.bytes())
                    .unwrap(),
                NativeQueryResponse::Success { value: snapshot() }
            );
        }
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        {
            let contexts = provider.contexts.lock().unwrap();
            assert_eq!(
                (
                    &*contexts[0].plugin,
                    contexts[0].parent_id,
                    contexts[0].call_id
                ),
                ("reply", 77, 3)
            );
        }
        drop(scope);
        drop(hold);
        assert_eq!(budget.snapshot().reserved_bytes, WORKSPACE_BYTES);
        assert_eq!(lifecycle.pending().len(), 1);
        assert!(
            runtime
                .try_scope(context(&lifecycle, NativeBackend::Wasm, "other", 1))
                .is_err()
        );
        drop(reply);
        wait_until(|| budget.snapshot().reserved_bytes == 0 && lifecycle.pending().is_empty())
            .await;
        assert_empty(&budget, &lifecycle);
        lifecycle
            .drain(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn completed_typed_errors_are_recoverable_and_unread_replies_are_single_flight() {
    let provider = SequenceProvider::new(vec![
        Err(NativeQueryError::new(
            NativeErrorCode::DeadlineExceeded,
            "provider deadline",
        )),
        Ok(snapshot()),
    ]);
    let (runtime, budget, lifecycle) = runtime_for(provider.clone(), WORKSPACE_BYTES);
    let scope = runtime
        .try_scope(context(&lifecycle, NativeBackend::Wasm, "recover", 1))
        .unwrap();
    let reply = scope
        .execute(1, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm)
        .await
        .unwrap();
    assert!(matches!(
        NativeQueryResponse::decode_for(&NativeQuery::ServerSnapshot, reply.bytes()).unwrap(),
        NativeQueryResponse::Error {
            error: NativeQueryError {
                code: NativeErrorCode::DeadlineExceeded,
                ..
            }
        }
    ));
    assert!(!scope.is_terminal());
    assert!(matches!(
        scope
            .execute(2, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm)
            .await,
        Err(NativeExecutionError::Busy)
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    drop(reply);
    assert!(!scope.is_busy());
    let reply = scope
        .execute(2, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm)
        .await
        .unwrap();
    assert!(matches!(
        NativeQueryResponse::decode_for(&NativeQuery::ServerSnapshot, reply.bytes()).unwrap(),
        NativeQueryResponse::Success { .. }
    ));
    drop(reply);
    let hold = scope.hold();
    drop(scope);
    assert_eq!(budget.snapshot().reserved_bytes, WORKSPACE_BYTES);
    assert_eq!(lifecycle.pending().len(), 1);
    drop(hold);
    wait_until(|| budget.snapshot().reserved_bytes == 0).await;
    assert_empty(&budget, &lifecycle);
}

#[derive(Default)]
struct BlockingGate {
    open: Mutex<bool>,
    changed: Condvar,
}
impl BlockingGate {
    fn release(&self) {
        *self.open.lock().unwrap_or_else(|error| error.into_inner()) = true;
        self.changed.notify_all();
    }
    fn wait(&self) {
        let guard = self.open.lock().unwrap();
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, Duration::from_secs(3), |open| !*open)
            .unwrap();
        assert!(*guard, "test did not release the provider fence");
    }
}
struct ReleaseGate(Arc<BlockingGate>);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct WorkingSetDrop {
    budget: NativeReadBudget,
    dropped: Arc<AtomicUsize>,
    protected: Arc<AtomicBool>,
    _memory: Vec<u8>,
}
impl Drop for WorkingSetDrop {
    fn drop(&mut self) {
        self.protected.store(
            self.budget.snapshot().reserved_bytes >= WORKSPACE_BYTES,
            Ordering::SeqCst,
        );
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

struct BlockingProvider {
    budget: NativeReadBudget,
    calls: AtomicUsize,
    started: Arc<Semaphore>,
    gate: Arc<BlockingGate>,
    dropped: Arc<AtomicUsize>,
    protected: Arc<AtomicBool>,
}
impl NativeQueryService for BlockingProvider {
    fn query(&self, _: NativeCallContext, _: NativeQuery) -> NativeQueryFuture<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let gate = Arc::clone(&self.gate);
        let started = Arc::clone(&self.started);
        let working = WorkingSetDrop {
            budget: self.budget.clone(),
            dropped: Arc::clone(&self.dropped),
            protected: Arc::clone(&self.protected),
            _memory: vec![0; 4096],
        };
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let _working = working;
                started.add_permits(1);
                gate.wait();
                Ok(snapshot())
            })
            .await
            .unwrap()
        })
    }
}

fn fenced_runtime() -> (
    NativeQueryRuntime,
    NativeReadBudget,
    Lifecycle,
    Arc<BlockingProvider>,
    ReleaseGate,
) {
    let budget = NativeReadBudget::new(WORKSPACE_BYTES).unwrap();
    let lifecycle = Lifecycle::new();
    let gate = Arc::new(BlockingGate::default());
    let provider = Arc::new(BlockingProvider {
        budget: budget.clone(),
        calls: AtomicUsize::new(0),
        started: Arc::new(Semaphore::new(0)),
        gate: Arc::clone(&gate),
        dropped: Arc::new(AtomicUsize::new(0)),
        protected: Arc::new(AtomicBool::new(false)),
    });
    let runtime = NativeQueryRuntime::new(
        provider.clone(),
        budget.clone(),
        lifecycle.clone(),
        NativeProviderContract::future_owned(MAX_PROVIDER_WORKING_SET_BYTES).unwrap(),
    );
    (runtime, budget, lifecycle, provider, ReleaseGate(gate))
}

async fn started(provider: &BlockingProvider) {
    timeout(Duration::from_secs(1), provider.started.acquire())
        .await
        .expect("provider never started")
        .unwrap()
        .forget();
}

#[tokio::test]
async fn dropping_an_unpolled_execute_creates_no_operation_or_provider_work() {
    let provider = SequenceProvider::new(Vec::new());
    let (runtime, budget, lifecycle) = runtime_for(provider.clone(), WORKSPACE_BYTES);
    let scope = runtime
        .try_scope(context(&lifecycle, NativeBackend::Wasm, "unpolled", 1))
        .unwrap();
    let baseline = lifecycle.pending().len();
    drop(scope.execute(1, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(budget.snapshot().active_operations, 0);
    assert_eq!(lifecycle.pending().len(), baseline);
    assert!(!scope.is_busy() && !scope.is_terminal());
    drop(scope);
    assert_empty(&budget, &lifecycle);
}

#[tokio::test]
async fn cancelling_waiter_quarantines_the_provider_fence_and_rejects_repeated_work() {
    let (runtime, budget, lifecycle, provider, _release) = fenced_runtime();
    let scope = runtime
        .try_scope(context(&lifecycle, NativeBackend::Wasm, "cancelled", 1))
        .unwrap();
    {
        let execute = scope.execute(1, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm);
        tokio::pin!(execute);
        tokio::select! {
            _ = started(&provider) => {}
            result = &mut execute => panic!("query completed before provider fence: {}", result.is_ok()),
        }
    }
    wait_until(|| scope.is_terminal() && budget.snapshot().quarantined_operations == 1).await;
    assert!(scope.is_busy());
    assert_eq!(budget.snapshot().reserved_bytes, WORKSPACE_BYTES);
    assert_eq!(budget.snapshot().active_operations, 1);
    assert_eq!(provider.dropped.load(Ordering::SeqCst), 0);
    for _ in 0..1000 {
        assert!(matches!(
            scope
                .execute(2, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm)
                .await,
            Err(NativeExecutionError::Terminated)
        ));
        assert!(
            runtime
                .try_scope(context(&lifecycle, NativeBackend::Wasm, "cancelled", 1))
                .is_err()
        );
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(lifecycle.pending().len(), 1);
    assert_eq!(budget.snapshot().active_scopes, 1);
    drop(scope);
    assert_eq!(budget.snapshot().reserved_bytes, WORKSPACE_BYTES);
    provider.gate.release();
    wait_until(|| budget.snapshot().reserved_bytes == 0 && lifecycle.pending().is_empty()).await;
    assert_eq!(provider.dropped.load(Ordering::SeqCst), 1);
    assert!(provider.protected.load(Ordering::SeqCst));
    assert_empty(&budget, &lifecycle);
    lifecycle
        .drain(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
}

#[tokio::test]
async fn aborting_caller_task_and_dropping_scope_does_not_cancel_accepted_blocking_work() {
    let (runtime, budget, lifecycle, provider, _release) = fenced_runtime();
    let scope = runtime
        .try_scope(context(&lifecycle, NativeBackend::Java, "aborted", 1))
        .unwrap();
    let hold = scope.hold();
    let caller = tokio::spawn(async move {
        scope
            .execute(
                1,
                NativeQuery::ServerSnapshot,
                NativeReplyFormat::JavaProtocol2,
            )
            .await
    });
    started(&provider).await;
    caller.abort();
    assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
    drop(hold);
    wait_until(|| budget.snapshot().quarantined_operations == 1).await;
    assert_eq!(budget.snapshot().reserved_bytes, WORKSPACE_BYTES);
    assert_eq!(provider.dropped.load(Ordering::SeqCst), 0);
    assert_eq!(lifecycle.pending().len(), 1);
    provider.gate.release();
    wait_until(|| budget.snapshot().reserved_bytes == 0 && lifecycle.pending().is_empty()).await;
    assert_eq!(provider.dropped.load(Ordering::SeqCst), 1);
    assert!(provider.protected.load(Ordering::SeqCst));
    assert_empty(&budget, &lifecycle);
}

#[tokio::test]
async fn wrapper_deadline_is_terminal_until_the_provider_fence_finishes() {
    let (runtime, budget, lifecycle, provider, _release) = fenced_runtime();
    let mut request = context(&lifecycle, NativeBackend::Wasm, "timeout", 1);
    request.deadline = Instant::now() + Duration::from_secs(1);
    let scope = runtime.try_scope(request).unwrap();
    let result = timeout(
        Duration::from_secs(2),
        scope.execute(1, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm),
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        Err(NativeExecutionError::DeadlineExceeded)
    ));
    assert!(scope.is_terminal());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.dropped.load(Ordering::SeqCst), 0);
    wait_until(|| budget.snapshot().quarantined_operations == 1).await;
    assert_eq!(budget.snapshot().reserved_bytes, WORKSPACE_BYTES);
    provider.gate.release();
    wait_until(|| budget.snapshot().active_operations == 0).await;
    assert_eq!(budget.snapshot().quarantined_operations, 0);
    assert!(scope.is_terminal());
    assert_eq!(budget.snapshot().reserved_bytes, WORKSPACE_BYTES);
    drop(scope);
    wait_until(|| budget.snapshot().reserved_bytes == 0 && lifecycle.pending().is_empty()).await;
    assert!(provider.protected.load(Ordering::SeqCst));
    assert_empty(&budget, &lifecycle);
}

#[tokio::test]
async fn cleanup_admission_remains_open_while_drain_has_an_admitted_parent() {
    let provider = SequenceProvider::new(Vec::new());
    let (runtime, budget, lifecycle) = runtime_for(provider.clone(), 2 * WORKSPACE_BYTES);
    let parent = runtime
        .try_scope(context(&lifecycle, NativeBackend::Wasm, "parent", 1))
        .unwrap();
    lifecycle.stop();
    assert!(
        runtime
            .try_scope(context(&lifecycle, NativeBackend::Wasm, "normal", 1))
            .is_err()
    );
    let drain = lifecycle.drain(Instant::now() + Duration::from_secs(2));
    tokio::pin!(drain);
    poll_fn(|cx| {
        assert!(drain.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let mut cleanup = context(&lifecycle, NativeBackend::Java, "cleanup", 1);
    cleanup.stop = lifecycle.cleanup_token();
    cleanup.admission = NativeAdmission::Cleanup;
    let cleanup = runtime.try_scope(cleanup).unwrap();
    let reply = cleanup
        .execute(
            1,
            NativeQuery::ServerSnapshot,
            NativeReplyFormat::JavaProtocol2,
        )
        .await
        .unwrap();
    drop(reply);
    drop(cleanup);
    assert_eq!(budget.snapshot().reserved_bytes, WORKSPACE_BYTES);
    drop(parent);
    drain.await.unwrap();
    assert_empty(&budget, &lifecycle);
    let mut after_seal = context(&lifecycle, NativeBackend::Java, "late_cleanup", 1);
    after_seal.stop = lifecycle.cleanup_token();
    after_seal.admission = NativeAdmission::Cleanup;
    assert!(runtime.try_scope(after_seal).is_err());
    assert_empty(&budget, &lifecycle);
}

fn players(count: usize) -> NativeQueryValue {
    NativeQueryValue::OnlinePlayers {
        players: (1..=count)
            .map(|index| NativePlayer {
                uuid: uuid::Uuid::from_u128(index as u128),
                session_id: index as i32,
                entity_id: index as i32,
                name: format!("player{index:010}"),
                world_uuid: uuid::Uuid::from_u128(10_000),
                position: NativePosition {
                    x: -14_027_620.401_934_333,
                    y: 128.123_456_789,
                    z: 29_999_999.123_456_79,
                    yaw: 123.456,
                    pitch: -78.901,
                },
                operator: false,
                online: true,
            })
            .collect(),
    }
}

fn reply_error(reply: &NativeReply, query: &NativeQuery) -> NativeErrorCode {
    match NativeQueryResponse::decode_for(query, reply.bytes()).unwrap() {
        NativeQueryResponse::Error { error } => error.code,
        NativeQueryResponse::Success { .. } => panic!("expected a typed query error"),
    }
}

#[tokio::test]
async fn invalid_provider_results_are_internal_errors_and_allow_the_next_query() {
    let mut oversized_capacity = String::with_capacity(MAX_PROVIDER_WORKING_SET_BYTES + 1);
    oversized_capacity.push_str("0.0.1");
    let capacity_snapshot = NativeQueryValue::ServerSnapshot(ServerSnapshot {
        server_version: oversized_capacity,
        minecraft_version: "26.3".into(),
        protocol: 777,
        max_players: 64,
        online_players: 2,
        tick: 1,
    });
    assert!(capacity_snapshot.valid_for(&NativeQuery::ServerSnapshot));
    let provider = SequenceProvider::new(vec![
        Ok(NativeQueryValue::Player { player: None }),
        Ok(NativeQueryValue::ServerSnapshot(ServerSnapshot {
            server_version: String::new(),
            minecraft_version: "26.3".into(),
            protocol: 777,
            max_players: 64,
            online_players: 2,
            tick: 1,
        })),
        Ok(capacity_snapshot),
        Ok(snapshot()),
    ]);
    let (runtime, budget, lifecycle) = runtime_for(provider.clone(), WORKSPACE_BYTES);
    let scope = runtime
        .try_scope(context(&lifecycle, NativeBackend::Wasm, "invalid_dto", 1))
        .unwrap();
    for call in 1..=3 {
        let reply = scope
            .execute(call, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm)
            .await
            .unwrap();
        assert_eq!(
            reply_error(&reply, &NativeQuery::ServerSnapshot),
            NativeErrorCode::Internal
        );
        drop(reply);
        assert!(!scope.is_terminal() && !scope.is_busy());
    }
    let reply = scope
        .execute(4, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm)
        .await
        .unwrap();
    assert!(matches!(
        NativeQueryResponse::decode_for(&NativeQuery::ServerSnapshot, reply.bytes()).unwrap(),
        NativeQueryResponse::Success { .. }
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 4);
    drop(reply);
    drop(scope);
    wait_until(|| budget.snapshot().reserved_bytes == 0 && lifecycle.pending().is_empty()).await;
    assert_empty(&budget, &lifecycle);
}

#[tokio::test]
async fn valid_oversized_result_returns_capacity_and_the_next_query_succeeds() {
    let value = players(1024);
    assert!(value.valid_for(&NativeQuery::OnlinePlayers));
    assert!(value_bytes(&value) <= MAX_PROVIDER_WORKING_SET_BYTES);
    assert!(serde_json::to_vec(&value).unwrap().len() > MAX_NATIVE_RESULT_BYTES);
    let provider = SequenceProvider::new(vec![Ok(value), Ok(snapshot())]);
    let (runtime, budget, lifecycle) = runtime_for(provider.clone(), WORKSPACE_BYTES);
    let scope = runtime
        .try_scope(context(&lifecycle, NativeBackend::Wasm, "large_result", 1))
        .unwrap();
    let reply = scope
        .execute(1, NativeQuery::OnlinePlayers, NativeReplyFormat::Wasm)
        .await
        .unwrap();
    assert_eq!(
        reply_error(&reply, &NativeQuery::OnlinePlayers),
        NativeErrorCode::Capacity
    );
    assert!(reply.bytes().len() < MAX_NATIVE_RESULT_BYTES);
    drop(reply);
    assert!(!scope.is_terminal() && !scope.is_busy());
    let reply = scope
        .execute(2, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm)
        .await
        .unwrap();
    assert!(matches!(
        NativeQueryResponse::decode_for(&NativeQuery::ServerSnapshot, reply.bytes()).unwrap(),
        NativeQueryResponse::Success { .. }
    ));
    drop(reply);
    drop(scope);
    wait_until(|| budget.snapshot().reserved_bytes == 0 && lifecycle.pending().is_empty()).await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_empty(&budget, &lifecycle);
}

#[tokio::test]
async fn call_limit_counts_accepted_queries_and_rejects_replays_without_provider_work() {
    let provider = SequenceProvider::new(Vec::new());
    let (runtime, budget, lifecycle) = runtime_for(provider.clone(), WORKSPACE_BYTES);
    let scope = runtime
        .try_scope(context(&lifecycle, NativeBackend::Wasm, "calls", 1))
        .unwrap();
    assert!(matches!(
        scope
            .execute(0, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm)
            .await,
        Err(NativeExecutionError::InvalidContext)
    ));
    let mut total_bytes = 0;
    for call in 1..=MAX_NATIVE_CALLS {
        let reply = scope
            .execute(call, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm)
            .await
            .unwrap();
        total_bytes += reply.json_len();
        drop(reply);
        assert!(matches!(
            scope
                .execute(call, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm)
                .await,
            Err(NativeExecutionError::InvalidContext)
        ));
    }
    assert!(matches!(
        scope
            .execute(
                MAX_NATIVE_CALLS + 1,
                NativeQuery::ServerSnapshot,
                NativeReplyFormat::Wasm
            )
            .await,
        Err(NativeExecutionError::Capacity)
    ));
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        MAX_NATIVE_CALLS as usize
    );
    {
        let state = scope.inner.state();
        assert_eq!(state.calls, MAX_NATIVE_CALLS);
        assert_eq!(state.response_bytes, total_bytes);
        assert_eq!(
            state.request_bytes,
            serde_json::to_vec(&NativeQuery::ServerSnapshot)
                .unwrap()
                .len()
                * MAX_NATIVE_CALLS as usize
        );
    }
    assert!(!scope.is_terminal() && !scope.is_busy());
    drop(scope);
    wait_until(|| budget.snapshot().reserved_bytes == 0 && lifecycle.pending().is_empty()).await;
    assert_empty(&budget, &lifecycle);
}

struct PlayerListProvider(AtomicUsize);
impl NativeQueryService for PlayerListProvider {
    fn query(&self, _: NativeCallContext, _: NativeQuery) -> NativeQueryFuture<'_> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(players(600)) })
    }
}

#[tokio::test]
async fn aggregate_response_budget_counts_json_bytes_and_excludes_the_java_newline() {
    for format in [NativeReplyFormat::Wasm, NativeReplyFormat::JavaProtocol2] {
        let provider = Arc::new(PlayerListProvider(AtomicUsize::new(0)));
        let (runtime, budget, lifecycle) = runtime_for(provider.clone(), WORKSPACE_BYTES);
        let scope = runtime
            .try_scope(context(&lifecycle, NativeBackend::Java, "aggregate", 1))
            .unwrap();
        let mut delivered = 0;
        let mut last_len = 0;
        let mut accepted = 0;
        let mut rejected = false;
        for call in 1..=12 {
            match scope
                .execute(call, NativeQuery::OnlinePlayers, format)
                .await
            {
                Ok(reply) => {
                    assert!(reply.json_len() <= MAX_NATIVE_RESULT_BYTES);
                    assert_eq!(
                        reply.bytes().len() - reply.json_len(),
                        usize::from(format == NativeReplyFormat::JavaProtocol2)
                    );
                    last_len = reply.json_len();
                    delivered += last_len;
                    accepted += 1;
                }
                Err(NativeExecutionError::Capacity) => {
                    rejected = true;
                    break;
                }
                Err(error) => panic!("unexpected query outcome: {error}"),
            }
        }
        assert!(rejected, "aggregate response limit was never enforced");
        assert!(accepted >= 2);
        assert!(delivered <= MAX_NATIVE_TOTAL_BYTES);
        assert!(delivered + last_len > MAX_NATIVE_TOTAL_BYTES);
        assert_eq!(scope.inner.state().response_bytes, delivered);
        assert_eq!(provider.0.load(Ordering::SeqCst), accepted + 1);
        assert!(scope.is_terminal());
        drop(scope);
        wait_until(|| budget.snapshot().reserved_bytes == 0 && lifecycle.pending().is_empty())
            .await;
        assert_empty(&budget, &lifecycle);
    }
}

struct PanicProvider {
    budget: NativeReadBudget,
    during_poll: bool,
    dropped: Arc<AtomicUsize>,
    protected: Arc<AtomicBool>,
}
impl NativeQueryService for PanicProvider {
    fn query(&self, _: NativeCallContext, _: NativeQuery) -> NativeQueryFuture<'_> {
        let working = WorkingSetDrop {
            budget: self.budget.clone(),
            dropped: Arc::clone(&self.dropped),
            protected: Arc::clone(&self.protected),
            _memory: vec![0; 4096],
        };
        assert!(self.during_poll, "controlled provider constructor panic");
        Box::pin(async move {
            let _working = working;
            panic!("controlled provider poll panic")
        })
    }
}

#[tokio::test]
async fn provider_constructor_and_poll_panics_release_working_memory_before_the_lease() {
    for during_poll in [false, true] {
        let budget = NativeReadBudget::new(WORKSPACE_BYTES).unwrap();
        let lifecycle = Lifecycle::new();
        let provider = Arc::new(PanicProvider {
            budget: budget.clone(),
            during_poll,
            dropped: Arc::new(AtomicUsize::new(0)),
            protected: Arc::new(AtomicBool::new(false)),
        });
        let runtime = NativeQueryRuntime::new(
            provider.clone(),
            budget.clone(),
            lifecycle.clone(),
            NativeProviderContract::future_owned(MAX_PROVIDER_WORKING_SET_BYTES).unwrap(),
        );
        let scope = runtime
            .try_scope(context(&lifecycle, NativeBackend::Wasm, "panicking", 1))
            .unwrap();
        assert!(matches!(
            scope
                .execute(1, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm)
                .await,
            Err(NativeExecutionError::ProviderFailed | NativeExecutionError::Cancelled)
        ));
        assert!(scope.is_terminal());
        assert_eq!(provider.dropped.load(Ordering::SeqCst), 1);
        assert!(provider.protected.load(Ordering::SeqCst));
        drop(scope);
        wait_until(|| budget.snapshot().reserved_bytes == 0 && lifecycle.pending().is_empty())
            .await;
        assert_empty(&budget, &lifecycle);
        assert!(matches!(
            lifecycle
                .drain(Instant::now() + Duration::from_secs(1))
                .await,
            Err(crate::lifecycle::DrainError::Cleanup { .. })
        ));
    }
}

struct PendingProvider {
    budget: NativeReadBudget,
    started: Arc<Semaphore>,
    dropped: Arc<AtomicUsize>,
    protected: Arc<AtomicBool>,
}
impl NativeQueryService for PendingProvider {
    fn query(&self, _: NativeCallContext, _: NativeQuery) -> NativeQueryFuture<'_> {
        let working = WorkingSetDrop {
            budget: self.budget.clone(),
            dropped: Arc::clone(&self.dropped),
            protected: Arc::clone(&self.protected),
            _memory: vec![0; 4096],
        };
        Box::pin(async move {
            let _working = working;
            self.started.add_permits(1);
            std::future::pending().await
        })
    }
}

#[test]
fn runtime_drop_preserves_the_lease_until_a_drop_safe_provider_future_is_destroyed() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let budget = NativeReadBudget::new(WORKSPACE_BYTES).unwrap();
    let lifecycle = Lifecycle::new();
    let provider = Arc::new(PendingProvider {
        budget: budget.clone(),
        started: Arc::new(Semaphore::new(0)),
        dropped: Arc::new(AtomicUsize::new(0)),
        protected: Arc::new(AtomicBool::new(false)),
    });
    let runtime = NativeQueryRuntime::new(
        provider.clone(),
        budget.clone(),
        lifecycle.clone(),
        NativeProviderContract::future_owned(MAX_PROVIDER_WORKING_SET_BYTES).unwrap(),
    );
    executor.block_on(async {
        let scope = runtime
            .try_scope(context(&lifecycle, NativeBackend::Wasm, "runtime_drop", 1))
            .unwrap();
        tokio::spawn(async move {
            scope
                .execute(1, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm)
                .await
        });
        timeout(Duration::from_secs(1), provider.started.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        assert_eq!(budget.snapshot().active_operations, 1);
    });
    assert_eq!(provider.dropped.load(Ordering::SeqCst), 0);
    drop(executor);
    assert_eq!(provider.dropped.load(Ordering::SeqCst), 1);
    assert!(provider.protected.load(Ordering::SeqCst));
    assert_empty(&budget, &lifecycle);
}

struct LateProvider(bool);
impl NativeQueryService for LateProvider {
    fn query(&self, _: NativeCallContext, _: NativeQuery) -> NativeQueryFuture<'_> {
        Box::pin(async move {
            std::thread::sleep(QUERY_TIMEOUT + Duration::from_millis(40));
            if self.0 {
                Err(NativeQueryError::new(
                    NativeErrorCode::DeadlineExceeded,
                    "late provider",
                ))
            } else {
                Ok(snapshot())
            }
        })
    }
}

#[tokio::test]
async fn provider_blocking_poll_past_subdeadline_is_terminal_even_when_completion_is_ready() {
    for typed_error in [false, true] {
        let (runtime, budget, lifecycle) =
            runtime_for(Arc::new(LateProvider(typed_error)), WORKSPACE_BYTES);
        let scope = runtime
            .try_scope(context(&lifecycle, NativeBackend::Wasm, "late", 1))
            .unwrap();
        assert!(matches!(
            scope
                .execute(1, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm)
                .await,
            Err(NativeExecutionError::DeadlineExceeded)
        ));
        assert!(scope.is_terminal());
        drop(scope);
        wait_until(|| budget.snapshot().reserved_bytes == 0 && lifecycle.pending().is_empty())
            .await;
        assert_empty(&budget, &lifecycle);
        lifecycle
            .drain(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
    }
}

struct ExpiredProvider(AtomicUsize);
impl NativeQueryService for ExpiredProvider {
    fn query(&self, context: NativeCallContext, _: NativeQuery) -> NativeQueryFuture<'_> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            tokio::time::sleep_until(context.deadline + Duration::from_millis(1)).await;
            Ok(snapshot())
        })
    }
}

#[tokio::test]
async fn completed_wrapper_error_terminalizes_before_the_waiter_observes_it() {
    let provider = Arc::new(ExpiredProvider(AtomicUsize::new(0)));
    let (runtime, budget, lifecycle) = runtime_for(provider.clone(), WORKSPACE_BYTES);
    let scope = runtime
        .try_scope(context(
            &lifecycle,
            NativeBackend::Wasm,
            "unread_timeout",
            1,
        ))
        .unwrap();
    let mut first =
        Box::pin(scope.execute(1, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm));
    poll_fn(|cx| {
        assert!(first.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    wait_until(|| {
        provider.0.load(Ordering::SeqCst) == 1 && budget.snapshot().active_operations == 0
    })
    .await;
    let second = scope
        .execute(2, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm)
        .await;
    assert!(matches!(second, Err(NativeExecutionError::Terminated)));
    assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    assert!(matches!(
        first.await,
        Err(NativeExecutionError::DeadlineExceeded)
    ));
    drop(scope);
    wait_until(|| budget.snapshot().reserved_bytes == 0 && lifecycle.pending().is_empty()).await;
    assert_empty(&budget, &lifecycle);
}

#[tokio::test]
async fn workspace_waiters_create_no_jobs_or_provider_work_and_wake_only_after_the_last_owner_drops()
 {
    let provider = SequenceProvider::new(Vec::new());
    let (runtime, budget, lifecycle) = runtime_for(provider.clone(), WORKSPACE_BYTES);
    let first = runtime
        .try_scope(context(&lifecycle, NativeBackend::Wasm, "first_owner", 1))
        .unwrap();
    let hold = first.hold();
    let reply = first
        .execute(1, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm)
        .await
        .unwrap();
    let mut waiting =
        Box::pin(runtime.wait_scope(context(&lifecycle, NativeBackend::Java, "waiting_owner", 1)));
    for _ in 0..32 {
        poll_fn(|cx| {
            assert!(waiting.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
    }
    assert_eq!(budget.snapshot().reserved_bytes, WORKSPACE_BYTES);
    assert_eq!(budget.snapshot().peak_bytes, WORKSPACE_BYTES);
    assert_eq!(budget.snapshot().active_scopes, 1);
    assert_eq!(lifecycle.pending().len(), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    drop(first);
    drop(reply);
    poll_fn(|cx| {
        assert!(waiting.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(lifecycle.pending().len(), 1);
    drop(hold);
    let next = timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(budget.snapshot().active_scopes, 1);
    assert_eq!(lifecycle.pending().len(), 1);
    drop(next);
    assert_empty(&budget, &lifecycle);
}

#[tokio::test]
async fn workspace_wait_respects_explicit_cancellation_deadlines_and_waiter_drop() {
    let provider = SequenceProvider::new(Vec::new());
    let (runtime, budget, lifecycle) = runtime_for(provider.clone(), WORKSPACE_BYTES);
    let first = runtime
        .try_scope(context(&lifecycle, NativeBackend::Wasm, "occupied", 1))
        .unwrap();
    let request = context(&lifecycle, NativeBackend::Wasm, "cancel_wait", 1);
    let stop = request.stop.clone();
    let mut waiting = Box::pin(runtime.wait_scope(request));
    poll_fn(|cx| {
        assert!(waiting.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    stop.request_stop();
    assert!(matches!(
        timeout(Duration::from_millis(100), waiting).await.unwrap(),
        Err(NativeExecutionError::Cancelled)
    ));
    let mut expired = context(&lifecycle, NativeBackend::Wasm, "deadline_wait", 1);
    expired.deadline = Instant::now() + Duration::from_millis(20);
    assert!(matches!(
        timeout(Duration::from_millis(100), runtime.wait_scope(expired))
            .await
            .unwrap(),
        Err(NativeExecutionError::DeadlineExceeded)
    ));
    let mut abandoned = Box::pin(runtime.wait_scope(context(
        &lifecycle,
        NativeBackend::Wasm,
        "abandoned_wait",
        1,
    )));
    poll_fn(|cx| {
        assert!(abandoned.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(abandoned);
    assert_eq!(budget.snapshot().active_scopes, 1);
    assert_eq!(lifecycle.pending().len(), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    drop(first);
    assert_empty(&budget, &lifecycle);
}

#[tokio::test]
async fn a_busy_plugin_key_waits_until_quarantined_provider_work_actually_finishes() {
    let (runtime, budget, lifecycle, provider, _release) = fenced_runtime();
    let scope = runtime
        .try_scope(context(&lifecycle, NativeBackend::Wasm, "same_key", 1))
        .unwrap();
    {
        let execute = scope.execute(1, NativeQuery::ServerSnapshot, NativeReplyFormat::Wasm);
        tokio::pin!(execute);
        tokio::select! {
            _ = started(&provider) => {}
            _ = &mut execute => panic!("provider fence completed before release"),
        }
    }
    drop(scope);
    wait_until(|| budget.snapshot().quarantined_operations == 1).await;
    let mut waiting =
        Box::pin(runtime.wait_scope(context(&lifecycle, NativeBackend::Wasm, "same_key", 1)));
    poll_fn(|cx| {
        assert!(waiting.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(provider.dropped.load(Ordering::SeqCst), 0);
    assert_eq!(lifecycle.pending().len(), 1);
    provider.gate.release();
    let next = timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(provider.dropped.load(Ordering::SeqCst), 1);
    assert!(provider.protected.load(Ordering::SeqCst));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(budget.snapshot().quarantined_operations, 0);
    assert_eq!(budget.snapshot().active_operations, 0);
    drop(next);
    assert_empty(&budget, &lifecycle);
}

#[tokio::test]
async fn exhausted_lifecycle_capacity_returns_promptly_without_retrying_its_own_lease_release() {
    let provider = SequenceProvider::new(Vec::new());
    let (runtime, budget, lifecycle) = runtime_for(provider.clone(), WORKSPACE_BYTES);
    let mut jobs = Vec::new();
    for _ in 0..4096 {
        jobs.push(lifecycle.register("capacity fixture").unwrap());
    }
    let result = timeout(
        Duration::from_millis(100),
        runtime.wait_scope(context(&lifecycle, NativeBackend::Wasm, "job_capacity", 1)),
    )
    .await;
    assert!(matches!(result, Ok(Err(NativeExecutionError::Capacity))));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(lifecycle.pending().len(), jobs.len());
    assert_eq!(budget.snapshot().reserved_bytes, 0);
    assert_eq!(budget.snapshot().active_scopes, 0);
    for job in jobs {
        job.finish(Ok(()));
    }
    assert_empty(&budget, &lifecycle);
}
