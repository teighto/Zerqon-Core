use super::*;
use crate::{
    lifecycle::Lifecycle,
    native_query_runtime::{
        DEFAULT_READ_BUDGET_BYTES, MAX_PROVIDER_WORKING_SET_BYTES, NativeProviderContract,
        NativeReadBudget,
    },
    plugin_services::{
        NativeCallContext, NativeQuery, NativeQueryFuture, NativeQueryService, NativeQueryValue,
    },
};
use std::{
    future::{Future, poll_fn},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
    time::Duration,
};
use tokio::{sync::Semaphore, time::Instant};
use zerqon_plugin_api::{
    Action, Event, LogLevel,
    native::{NativeCallbackContext, NativeSource},
};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(3)
}

fn server_context() -> NativeCallbackContext {
    NativeCallbackContext {
        source: NativeSource::Server,
    }
}

fn escaped(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("\\{byte:02x}")).collect()
}

fn plain_module(handler: &str) -> Vec<u8> {
    wat::parse_str(format!(
        r#"(module
        (import "zerqon" "event_len" (func $len (result i32)))
        (import "zerqon" "event_read" (func $read (param i32 i32) (result i32)))
        (memory (export "memory") 1)
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (func (export "zerqon_on_event") (result i32)
          i32.const 0 call $len call $read drop
          {handler}
          i32.const 0))"#
    ))
    .unwrap()
}

fn observed_module(query_on_tick: bool, query_on_cleanup: bool) -> Vec<u8> {
    observed_module_for_events(false, query_on_tick, query_on_cleanup)
}

fn observed_module_for_events(
    query_on_enable: bool,
    query_on_tick: bool,
    query_on_cleanup: bool,
) -> Vec<u8> {
    let actions = ["enable", "tick", "quit", "disable"].map(|message| {
        serde_json::to_vec(&Action::Log {
            level: LogLevel::Info,
            message: message.into(),
        })
        .unwrap()
    });
    let data: String = actions
        .iter()
        .enumerate()
        .map(|(index, bytes)| {
            format!(
                "(data (i32.const {}) \"{}\")",
                4096 + index * 256,
                escaped(bytes)
            )
        })
        .collect();
    let query = br#"{"query":"worlds"}"#;
    let read = "call $token i32.const 2048 i32.const 18 call $query local.set $handle\ncall $token local.get $handle call $result_len local.set $length\ncall $token local.get $handle i32.const 8192 local.get $length call $result_read drop";
    wat::parse_str(format!(
        r#"(module
        (import "zerqon" "event_len" (func $event_len (result i32)))
        (import "zerqon" "event_read" (func $event_read (param i32 i32) (result i32)))
        (import "zerqon" "emit" (func $emit (param i32 i32) (result i32)))
        (import "zerqon_read_v1" "callback_token" (func $token (result i64)))
        (import "zerqon_read_v1" "query" (func $query (param i64 i32 i32) (result i32)))
        (import "zerqon_read_v1" "result_len" (func $result_len (param i64 i32) (result i32)))
        (import "zerqon_read_v1" "result_read" (func $result_read (param i64 i32 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 2048) "{query}")
        {data}
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (func (export "zerqon_on_event") (result i32)
          (local $handle i32) (local $length i32)
          i32.const 0 call $event_len call $event_read drop
          i32.const 10 i32.load8_u i32.const 101 i32.eq
          if {enable_query} i32.const 4096 i32.const {enable_len} call $emit drop end
          i32.const 10 i32.load8_u i32.const 116 i32.eq
          if
            {tick_query}
            i32.const 4352 i32.const {tick_len} call $emit drop
          end
          i32.const 10 i32.load8_u i32.const 112 i32.eq
          if
            {cleanup_query}
            i32.const 4608 i32.const {quit_len} call $emit drop
          end
          i32.const 10 i32.load8_u i32.const 100 i32.eq
          if
            {cleanup_query}
            i32.const 4864 i32.const {disable_len} call $emit drop
          end
          i32.const 0))"#,
        query = escaped(query),
        data = data,
        enable_len = actions[0].len(),
        tick_len = actions[1].len(),
        quit_len = actions[2].len(),
        disable_len = actions[3].len(),
        enable_query = if query_on_enable { read } else { "" },
        tick_query = if query_on_tick { read } else { "" },
        cleanup_query = if query_on_cleanup { read } else { "" },
    ))
    .unwrap()
}

fn assert_action(output: &RustCallbackOutput, message: &str) {
    assert_eq!(
        output.actions(),
        &[Action::Log {
            level: LogLevel::Info,
            message: message.into(),
        }]
    );
}

fn join_observer_module(tick_reads: bool, join_reads: bool, fault_join: bool) -> Vec<u8> {
    let actions = ["join", "quit", "disable"].map(|message| {
        serde_json::to_vec(&Action::Log {
            level: LogLevel::Info,
            message: message.into(),
        })
        .unwrap()
    });
    let data: String = actions
        .iter()
        .enumerate()
        .map(|(index, bytes)| {
            format!(
                "(data (i32.const {}) \"{}\")",
                4096 + index * 256,
                escaped(bytes)
            )
        })
        .collect();
    let read = "call $token i32.const 2048 i32.const 18 call $query local.set $handle call $token local.get $handle call $length local.set $size call $token local.get $handle i32.const 8192 local.get $size call $copy drop";
    wat::parse_str(format!(
        r#"(module
        (import "zerqon" "event_len" (func $event_len (result i32)))
        (import "zerqon" "event_read" (func $event_read (param i32 i32) (result i32)))
        (import "zerqon" "emit" (func $emit (param i32 i32) (result i32)))
        (import "zerqon_read_v1" "callback_token" (func $token (result i64)))
        (import "zerqon_read_v1" "query" (func $query (param i64 i32 i32) (result i32)))
        (import "zerqon_read_v1" "result_len" (func $length (param i64 i32) (result i32)))
        (import "zerqon_read_v1" "result_read" (func $copy (param i64 i32 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (global $joined (mut i32) (i32.const 0))
        (data (i32.const 2048) "{query}") {data}
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (func (export "zerqon_on_event") (result i32) (local $handle i32) (local $size i32)
            i32.const 0 call $event_len call $event_read drop
            i32.const 10 i32.load8_u i32.const 116 i32.eq if {tick_query} end
            i32.const 10 i32.load8_u i32.const 112 i32.eq if
                i32.const 17 i32.load8_u i32.const 106 i32.eq if
                    {join_fault} {join_query}
                    global.get $joined i32.const 1 i32.add global.set $joined
                    i32.const 4096 i32.const {join_len} call $emit drop
                else
                    global.get $joined i32.eqz if unreachable end
                    global.get $joined i32.const 1 i32.sub global.set $joined
                    i32.const 4352 i32.const {quit_len} call $emit drop
                end
            end
            i32.const 10 i32.load8_u i32.const 100 i32.eq if
                i32.const 4608 i32.const {disable_len} call $emit drop
            end
            i32.const 0))"#,
        query = escaped(br#"{"query":"worlds"}"#),
        data = data,
        tick_query = if tick_reads { read } else { "" },
        join_query = if join_reads { read } else { "" },
        join_fault = if fault_join { "unreachable" } else { "" },
        join_len = actions[0].len(),
        quit_len = actions[1].len(),
        disable_len = actions[2].len(),
    ))
    .unwrap()
}

fn player_lifecycle_events() -> (NativeCallbackContext, Event, Event) {
    let uuid = uuid::Uuid::from_u128(123);
    (
        NativeCallbackContext {
            source: NativeSource::Player {
                uuid,
                session_id: 1,
            },
        },
        Event::PlayerJoin {
            uuid: uuid.to_string(),
            name: "Receipt".into(),
        },
        Event::PlayerQuit {
            uuid: uuid.to_string(),
            name: "Receipt".into(),
        },
    )
}

fn delivered_recipients(results: &[RustDispatchResult]) -> Vec<RustRecipient> {
    results
        .iter()
        .filter(|result| result.delivery == RustDelivery::Delivered)
        .map(|result| {
            assert!(result.result.is_ok());
            result
                .recipient
                .clone()
                .expect("delivered callback has its captured generation")
        })
        .collect()
}

#[tokio::test]
async fn partial_join_admission_records_only_the_delivered_generation_for_quit() {
    let provider = Provider::new(true);
    let (plugins, budget, lifecycle) = fixture(
        provider.clone(),
        ActorLimits {
            per_plugin_queue: 1,
            ..ActorLimits::default()
        },
    );
    let stop = lifecycle.token();
    drop(
        plugins
            .load(
                "a_busy",
                &join_observer_module(true, false, false),
                &stop,
                deadline(),
            )
            .await
            .unwrap(),
    );
    drop(
        plugins
            .load(
                "b_joined",
                &join_observer_module(false, false, false),
                &stop,
                deadline(),
            )
            .await
            .unwrap(),
    );
    let recipients = plugins.recipients();
    let tick = Event::Tick { tick: 1 };
    let server = server_context();
    let mut active = Box::pin(plugins.dispatch(&tick, &server, Some("a_busy"), &stop, deadline()));
    tokio::select! { _ = started(&provider) => {}, _ = &mut active => panic!("busy actor did not reach its provider") }
    let mut queued = Box::pin(plugins.dispatch(&tick, &server, Some("a_busy"), &stop, deadline()));
    poll_fn(|cx| {
        assert!(queued.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let (context, join, quit) = player_lifecycle_events();
    let joined_b = plugins
        .dispatch_admitted_join(
            &join,
            &context,
            &recipients[1..],
            &stop.admitted_child(),
            deadline(),
        )
        .await
        .unwrap();
    assert_eq!(joined_b[0].delivery, RustDelivery::Delivered);
    assert_action(joined_b[0].result.as_ref().unwrap(), "join");
    let delivered = delivered_recipients(&joined_b);
    drop(joined_b);
    let results = plugins
        .dispatch_admitted_join(
            &join,
            &context,
            &recipients[..1],
            &stop.admitted_child(),
            Instant::now() + Duration::from_millis(30),
        )
        .await
        .unwrap();
    provider.release.add_permits(2);
    let active = active.await;
    let queued = queued.await;
    assert!(active[0].result.is_ok() && queued[0].result.is_ok());
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "a_busy");
    assert_eq!(results[0].delivery, RustDelivery::NotAdmitted);
    assert!(results[0].result.is_err());
    assert!(delivered_recipients(&results).is_empty());
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].name(), "b_joined");
    let quit = plugins
        .dispatch_to_recipients(
            &quit,
            &context,
            &delivered,
            &lifecycle.cleanup_token(),
            deadline(),
        )
        .await
        .unwrap();
    assert_eq!(quit.len(), 1);
    assert_eq!(quit[0].name, "b_joined");
    assert_eq!(quit[0].delivery, RustDelivery::Delivered);
    assert_action(quit[0].result.as_ref().unwrap(), "quit");
    assert!(plugins.list().iter().all(|plugin| plugin.enabled));
    drop(results);
    drop(quit);
    drop(active);
    drop(queued);
    finish(&plugins, &budget, &lifecycle).await;
}

#[tokio::test]
async fn critical_targeted_quit_admits_twenty_sessions_through_one_bounded_actor_queue() {
    let provider = Provider::new(true);
    let (plugins, budget, lifecycle) = fixture(provider.clone(), ActorLimits::default());
    drop(
        plugins
            .load(
                "quit_backpressure",
                &observed_module(false, true),
                &lifecycle.token(),
                deadline(),
            )
            .await
            .unwrap(),
    );
    let recipient = plugins.recipients().remove(0);
    let actor = plugins
        .inner
        .actors
        .lock()
        .unwrap()
        .get("quit_backpressure")
        .unwrap()
        .clone();
    let attempted = Arc::new(AtomicUsize::new(0));
    let initial_rejections = plugins.stats().rejections;
    let mut calls = tokio::task::JoinSet::new();
    lifecycle.stop();
    for index in 1..=20 {
        let plugins = Arc::clone(&plugins);
        let recipient = recipient.clone();
        let parent = lifecycle.cleanup_token().child();
        let attempted = Arc::clone(&attempted);
        calls.spawn(async move {
            let uuid = uuid::Uuid::from_u128(index as u128);
            let event = Event::PlayerQuit {
                uuid: uuid.to_string(),
                name: format!("Player{index}"),
            };
            let context = NativeCallbackContext {
                source: NativeSource::Player {
                    uuid,
                    session_id: index,
                },
            };
            attempted.fetch_add(1, Ordering::SeqCst);
            let result = plugins
                .dispatch_to_recipients(&event, &context, &[recipient], &parent, deadline())
                .await;
            let outcome = match &result {
                Ok(results)
                    if results.len() == 1
                        && results[0].delivery == RustDelivery::Delivered
                        && results[0].result.is_ok() =>
                {
                    Ok(())
                }
                other => Err(format!("Quit {index} was not delivered: {other:?}")),
            };
            drop(result);
            outcome
        });
    }
    started(&provider).await;
    let full = tokio::time::timeout(Duration::from_millis(100), async {
        while attempted.load(Ordering::SeqCst) != 20
            || (actor.sender.capacity() != 0 && plugins.stats().rejections == initial_rejections)
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    provider.release.add_permits(21);
    let mut failures = Vec::new();
    while let Some(result) = calls.join_next().await {
        if let Err(error) = result.unwrap() {
            failures.push(error);
        }
    }
    let delivered_calls = provider.calls.load(Ordering::SeqCst);
    let peak = plugins.stats();
    finish(&plugins, &budget, &lifecycle).await;
    assert!(
        full.is_ok(),
        "twenty submitted callbacks never saturated the bounded actor queue"
    );
    assert!(
        failures.is_empty(),
        "{} critical Quit callbacks were lost: {failures:?}",
        failures.len()
    );
    assert_eq!(delivered_calls, 20);
    assert!(peak.peak_callbacks <= 64);
    assert!(peak.peak_bytes <= 4 * 1024 * 1024);
}

#[tokio::test]
async fn critical_callback_execution_timeout_starts_at_dequeue_after_long_queue_waits() {
    #[derive(Clone, Copy)]
    struct QueryTiming {
        started: Instant,
        deadline: Instant,
        finished: Option<Instant>,
    }
    struct SlowCleanupProvider {
        timings: Mutex<Vec<QueryTiming>>,
        entered: Semaphore,
    }
    impl NativeQueryService for SlowCleanupProvider {
        fn query(&self, context: NativeCallContext, query: NativeQuery) -> NativeQueryFuture<'_> {
            assert_eq!(query, NativeQuery::Worlds);
            let index = {
                let mut timings = self.timings.lock().unwrap();
                let index = timings.len();
                timings.push(QueryTiming {
                    started: Instant::now(),
                    deadline: context.deadline,
                    finished: None,
                });
                index
            };
            self.entered.add_permits(1);
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(120)).await;
                self.timings.lock().unwrap()[index].finished = Some(Instant::now());
                Ok(NativeQueryValue::Worlds { worlds: Vec::new() })
            })
        }
    }
    let provider = Arc::new(SlowCleanupProvider {
        timings: Mutex::new(Vec::new()),
        entered: Semaphore::new(0),
    });
    let lifecycle = Lifecycle::new();
    let budget = NativeReadBudget::new(DEFAULT_READ_BUDGET_BYTES).unwrap();
    let queries = NativeQueryRuntime::new(
        provider.clone(),
        budget.clone(),
        lifecycle.clone(),
        NativeProviderContract::future_owned(MAX_PROVIDER_WORKING_SET_BYTES).unwrap(),
    );
    let plugins = Arc::new(
        RustPlugins::new(
            Limits::default(),
            queries,
            lifecycle.clone(),
            ActorLimits {
                callback_timeout: Duration::from_millis(200),
                ..ActorLimits::default()
            },
        )
        .unwrap(),
    );
    drop(
        plugins
            .load(
                "timed_quit",
                &observed_module(false, true),
                &lifecycle.token(),
                deadline(),
            )
            .await
            .unwrap(),
    );
    let recipient = plugins.recipients().remove(0);
    let actor = plugins
        .inner
        .actors
        .lock()
        .unwrap()
        .get("timed_quit")
        .unwrap()
        .clone();
    let transaction_deadline = Instant::now() + Duration::from_secs(2);
    let attempted = Arc::new(AtomicUsize::new(0));
    let mut callers = tokio::task::JoinSet::new();
    lifecycle.stop();
    for index in 1..=4 {
        let plugins = Arc::clone(&plugins);
        let recipient = recipient.clone();
        let attempted = Arc::clone(&attempted);
        let parent = lifecycle.cleanup_token();
        callers.spawn(async move {
            let uuid = uuid::Uuid::from_u128(index as u128);
            let event = Event::PlayerQuit {
                uuid: uuid.to_string(),
                name: format!("Queued{index}"),
            };
            let context = NativeCallbackContext {
                source: NativeSource::Player {
                    uuid,
                    session_id: index,
                },
            };
            attempted.fetch_add(1, Ordering::SeqCst);
            let result = plugins
                .dispatch_to_recipients(
                    &event,
                    &context,
                    &[recipient],
                    &parent,
                    transaction_deadline,
                )
                .await;
            let outcome = match &result {
                Ok(results)
                    if results.len() == 1
                        && results[0].delivery == RustDelivery::Delivered
                        && results[0].result.is_ok() =>
                {
                    Ok(())
                }
                other => Err(format!("queued Quit {index} failed: {other:?}")),
            };
            drop(result);
            outcome
        });
    }
    tokio::time::timeout(Duration::from_secs(1), provider.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let queued = tokio::time::timeout(Duration::from_millis(60), async {
        while attempted.load(Ordering::SeqCst) != 4 || actor.sender.capacity() != 5 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let mut failures = Vec::new();
    while let Some(result) = callers.join_next().await {
        if let Err(error) = result.unwrap() {
            failures.push(error);
        }
    }
    let timings = provider.timings.lock().unwrap().clone();
    finish(&plugins, &budget, &lifecycle).await;
    assert!(
        queued.is_ok(),
        "all four requests were not queued before the first execution completed"
    );
    assert!(
        failures.is_empty(),
        "queue residence consumed a callback execution timeout: {failures:?}"
    );
    assert_eq!(timings.len(), 4);
    assert!(timings[3].started.duration_since(timings[0].started) >= Duration::from_millis(300));
    for timing in timings {
        let finished = timing.finished.expect("accepted provider did not finish");
        assert!(timing.deadline.duration_since(timing.started) <= Duration::from_millis(200));
        assert!(
            finished < timing.deadline,
            "a callback exceeded its own execution cap"
        );
        assert!(finished.duration_since(timing.started) >= Duration::from_millis(110));
    }
    assert_eq!(provider.timings.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn old_join_receipts_never_send_quit_to_reloaded_or_new_plugins() {
    let (plugins, budget, lifecycle) = fixture(Provider::new(false), ActorLimits::default());
    let stop = lifecycle.token();
    let code = join_observer_module(false, false, false);
    drop(
        plugins
            .load("reload", &code, &stop, deadline())
            .await
            .unwrap(),
    );
    let original = plugins.recipients();
    let (context, join, quit) = player_lifecycle_events();
    let joined = plugins
        .dispatch_admitted_join(
            &join,
            &context,
            &original,
            &stop.admitted_child(),
            deadline(),
        )
        .await
        .unwrap();
    let delivered = delivered_recipients(&joined);
    assert_eq!(delivered, original);
    drop(joined);
    drop(plugins.unload("reload", deadline()).await.unwrap());
    drop(
        plugins
            .load("reload", &code, &stop, deadline())
            .await
            .unwrap(),
    );
    drop(
        plugins
            .load("new_plugin", &code, &stop, deadline())
            .await
            .unwrap(),
    );
    let current = plugins.recipients();
    assert_ne!(
        current
            .iter()
            .find(|id| id.name() == "reload")
            .unwrap()
            .generation(),
        original[0].generation()
    );
    let retired = plugins
        .dispatch_to_recipients(
            &quit,
            &context,
            &delivered,
            &lifecycle.cleanup_token(),
            deadline(),
        )
        .await
        .unwrap();
    assert_eq!(retired.len(), 1);
    assert_eq!(retired[0].recipient.as_ref(), Some(&original[0]));
    assert_eq!(retired[0].delivery, RustDelivery::Retired);
    assert!(matches!(
        retired[0].result,
        Err(RustPluginError::NotFound(_))
    ));
    assert!(plugins.list().iter().all(|plugin| plugin.enabled));
    assert_eq!(current.len(), 2);
    assert!(matches!(
        plugins
            .dispatch_admitted_join(
                &join,
                &context,
                &current,
                &stop.admitted_child(),
                deadline()
            )
            .await,
        Err(RustPluginError::Configuration)
    ));
    assert!(matches!(
        plugins
            .dispatch_to_recipients(
                &quit,
                &context,
                &current,
                &lifecycle.cleanup_token(),
                deadline()
            )
            .await,
        Err(RustPluginError::Configuration)
    ));
    assert!(
        plugins
            .dispatch_admitted_join(&join, &context, &[], &stop.admitted_child(), deadline())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        plugins
            .dispatch_to_recipients(&quit, &context, &[], &lifecycle.cleanup_token(), deadline())
            .await
            .unwrap()
            .is_empty()
    );
    for id in &current {
        let joined = plugins
            .dispatch_admitted_join(
                &join,
                &context,
                std::slice::from_ref(id),
                &stop.admitted_child(),
                deadline(),
            )
            .await
            .unwrap();
        assert_eq!(
            delivered_recipients(&joined).as_slice(),
            std::slice::from_ref(id)
        );
        assert_action(joined[0].result.as_ref().unwrap(), "join");
        drop(joined);
        let quit = plugins
            .dispatch_to_recipients(
                &quit,
                &context,
                std::slice::from_ref(id),
                &lifecycle.cleanup_token(),
                deadline(),
            )
            .await
            .unwrap();
        assert_eq!(quit[0].delivery, RustDelivery::Delivered);
        assert_action(quit[0].result.as_ref().unwrap(), "quit");
        drop(quit);
    }
    drop(retired);
    finish(&plugins, &budget, &lifecycle).await;
}

#[tokio::test]
async fn a_join_receipt_cannot_target_an_independent_service_with_the_same_plugin_name() {
    let (first, first_budget, first_lifecycle) =
        fixture(Provider::new(false), ActorLimits::default());
    let (second, second_budget, second_lifecycle) =
        fixture(Provider::new(false), ActorLimits::default());
    let code = join_observer_module(false, false, false);
    drop(
        first
            .load("shared", &code, &first_lifecycle.token(), deadline())
            .await
            .unwrap(),
    );
    drop(
        second
            .load("shared", &code, &second_lifecycle.token(), deadline())
            .await
            .unwrap(),
    );
    let first_recipient = first.recipients();
    let second_recipient = second.recipients();
    assert_ne!(
        first_recipient[0].generation(),
        second_recipient[0].generation()
    );
    let (context, join, quit) = player_lifecycle_events();
    let joined = first
        .dispatch_admitted_join(
            &join,
            &context,
            &first_recipient,
            &first_lifecycle.token().admitted_child(),
            deadline(),
        )
        .await
        .unwrap();
    let delivered = delivered_recipients(&joined);
    drop(joined);
    let foreign = second
        .dispatch_to_recipients(
            &quit,
            &context,
            &delivered,
            &second_lifecycle.cleanup_token(),
            deadline(),
        )
        .await
        .unwrap();
    assert_eq!(foreign.len(), 1);
    assert_eq!(foreign[0].delivery, RustDelivery::Retired);
    assert!(matches!(
        foreign[0].result,
        Err(RustPluginError::NotFound(_))
    ));
    assert!(second.list()[0].enabled);
    let joined = second
        .dispatch_admitted_join(
            &join,
            &context,
            &second_recipient,
            &second_lifecycle.token().admitted_child(),
            deadline(),
        )
        .await
        .unwrap();
    assert_action(joined[0].result.as_ref().unwrap(), "join");
    drop(joined);
    let second_quit = second
        .dispatch_to_recipients(
            &quit,
            &context,
            &second_recipient,
            &second_lifecycle.cleanup_token(),
            deadline(),
        )
        .await
        .unwrap();
    assert_action(second_quit[0].result.as_ref().unwrap(), "quit");
    let first_quit = first
        .dispatch_to_recipients(
            &quit,
            &context,
            &delivered,
            &first_lifecycle.cleanup_token(),
            deadline(),
        )
        .await
        .unwrap();
    assert_action(first_quit[0].result.as_ref().unwrap(), "quit");
    drop(foreign);
    drop(second_quit);
    drop(first_quit);
    finish(&first, &first_budget, &first_lifecycle).await;
    finish(&second, &second_budget, &second_lifecycle).await;
}

#[tokio::test]
async fn an_admitted_join_uses_frozen_recipients_after_stop_and_rejects_invalid_or_cancelled_work()
{
    let provider = Provider::new(false);
    let (plugins, budget, lifecycle) = fixture(provider.clone(), ActorLimits::default());
    let stop = lifecycle.token();
    let code = join_observer_module(false, false, false);
    drop(
        plugins
            .load("before", &code, &stop, deadline())
            .await
            .unwrap(),
    );
    let frozen = plugins.recipients();
    let admitted = stop.admitted_child();
    drop(
        plugins
            .load("after", &code, &stop, deadline())
            .await
            .unwrap(),
    );
    lifecycle.stop();
    let (context, join, quit) = player_lifecycle_events();
    let cancelled = lifecycle.cleanup_token().child();
    cancelled.request_stop();
    let rejected = plugins
        .dispatch_admitted_join(&join, &context, &frozen, &cancelled, deadline())
        .await
        .unwrap();
    assert_eq!(rejected[0].delivery, RustDelivery::NotAdmitted);
    assert!(matches!(
        rejected[0].result,
        Err(RustPluginError::Cancelled)
    ));
    assert!(
        plugins
            .dispatch_admitted_join(
                &Event::Tick { tick: 1 },
                &server_context(),
                &frozen,
                &admitted,
                deadline()
            )
            .await
            .is_err()
    );
    assert!(
        plugins
            .dispatch_admitted_join(&join, &server_context(), &frozen, &admitted, deadline())
            .await
            .is_err()
    );
    assert!(
        plugins
            .dispatch_admitted_join(
                &join,
                &context,
                &[frozen[0].clone(), frozen[0].clone()],
                &admitted,
                deadline()
            )
            .await
            .is_err()
    );
    let joined = plugins
        .dispatch_admitted_join(&join, &context, &frozen, &admitted, deadline())
        .await
        .unwrap();
    assert_eq!(delivered_recipients(&joined), frozen);
    assert_action(joined[0].result.as_ref().unwrap(), "join");
    drop(joined);
    let quit = plugins
        .dispatch_to_recipients(
            &quit,
            &context,
            &frozen,
            &lifecycle.cleanup_token(),
            deadline(),
        )
        .await
        .unwrap();
    assert_eq!(quit.len(), 1);
    assert_eq!(quit[0].name, "before");
    assert_action(quit[0].result.as_ref().unwrap(), "quit");
    assert!(plugins.list().iter().all(|plugin| plugin.enabled));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    drop(quit);
    drop(rejected);
    finish(&plugins, &budget, &lifecycle).await;
}

#[tokio::test]
async fn a_buffered_successful_join_remains_delivered_when_its_waiter_is_polled_after_deadline() {
    let (plugins, budget, lifecycle) = fixture(Provider::new(false), ActorLimits::default());
    let stop = lifecycle.token();
    drop(
        plugins
            .load(
                "buffered",
                &join_observer_module(false, false, false),
                &stop,
                deadline(),
            )
            .await
            .unwrap(),
    );
    let recipients = plugins.recipients();
    let (context, join, quit) = player_lifecycle_events();
    let admitted = stop.admitted_child();
    let actor = plugins
        .inner
        .actors
        .lock()
        .unwrap()
        .get("buffered")
        .unwrap()
        .clone();
    let expires = Instant::now() + Duration::from_millis(100);
    let waiting = plugins
        .enqueue_critical(
            &actor,
            &DispatchInput {
                event: &join,
                context: &context,
                parent: &admitted,
                deadline: expires,
                mode: DispatchMode::AdmittedJoin,
            },
        )
        .await
        .unwrap();
    settled(|| !waiting.receiver.is_empty()).await;
    tokio::time::sleep_until(expires).await;
    let result = waiting.settle().await;
    assert!(Instant::now() >= expires);
    assert_eq!(delivery(&result, Some(&actor)), RustDelivery::Delivered);
    assert_action(result.as_ref().unwrap(), "join");
    drop(result);
    let quit = plugins
        .dispatch_to_recipients(
            &quit,
            &context,
            &recipients,
            &lifecycle.cleanup_token(),
            deadline(),
        )
        .await
        .unwrap();
    assert_action(quit[0].result.as_ref().unwrap(), "quit");
    drop(quit);
    finish(&plugins, &budget, &lifecycle).await;
}

#[tokio::test]
async fn a_guest_join_fault_is_distinct_from_healthy_admission_failure() {
    let (plugins, budget, lifecycle) = fixture(Provider::new(false), ActorLimits::default());
    let stop = lifecycle.token();
    drop(
        plugins
            .load(
                "faulted",
                &join_observer_module(false, false, true),
                &stop,
                deadline(),
            )
            .await
            .unwrap(),
    );
    let recipients = plugins.recipients();
    let (context, join, _) = player_lifecycle_events();
    let results = plugins
        .dispatch_admitted_join(
            &join,
            &context,
            &recipients,
            &stop.admitted_child(),
            deadline(),
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].delivery, RustDelivery::Faulted);
    assert!(results[0].result.is_err());
    assert!(delivered_recipients(&results).is_empty());
    assert!(
        !plugins
            .list()
            .iter()
            .any(|plugin| plugin.name == "faulted" && plugin.enabled)
    );
    drop(results);
    finish(&plugins, &budget, &lifecycle).await;
}

#[tokio::test]
async fn critical_queue_wait_honors_cancellation_deadline_and_retirement_before_guest_execution() {
    for mode in ["cancel", "deadline", "retire"] {
        let provider = Provider::new(true);
        let (plugins, budget, lifecycle) = fixture(
            provider.clone(),
            ActorLimits {
                per_plugin_queue: 1,
                ..ActorLimits::default()
            },
        );
        let stop = lifecycle.token();
        drop(
            plugins
                .load("saturated", &observed_module(true, true), &stop, deadline())
                .await
                .unwrap(),
        );
        let recipient = plugins.recipients();
        let tick = Event::Tick { tick: 1 };
        let server = server_context();
        let mut active =
            Box::pin(plugins.dispatch(&tick, &server, Some("saturated"), &stop, deadline()));
        tokio::select! { _ = started(&provider) => {}, _ = &mut active => panic!("provider fence was not reached") }
        let mut queued =
            Box::pin(plugins.dispatch(&tick, &server, Some("saturated"), &stop, deadline()));
        poll_fn(|cx| {
            assert!(queued.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let (context, _, quit) = player_lifecycle_events();
        let parent = lifecycle.cleanup_token().child();
        let expires = if mode == "deadline" {
            Instant::now() + Duration::from_millis(30)
        } else {
            deadline()
        };
        let mut waiting =
            Box::pin(plugins.dispatch_to_recipients(&quit, &context, &recipient, &parent, expires));
        let pending = poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx).is_pending())).await;
        let held_callbacks = plugins.stats().callbacks;
        if mode == "cancel" {
            parent.request_stop();
        }
        if mode == "retire" {
            drop(plugins.unload("saturated", deadline()).await.unwrap());
        }
        let outcome = if pending {
            Some(tokio::time::timeout(Duration::from_millis(100), waiting).await)
        } else {
            None
        };
        provider.release.add_permits(3);
        let active = active.await;
        let queued = queued.await;
        assert!(
            pending,
            "{mode}: critical callback did not wait for queue admission"
        );
        assert_eq!(
            held_callbacks, 2,
            "waiting admission allocated a third callback lease"
        );
        let result = outcome.unwrap().unwrap().unwrap();
        assert_eq!(result.len(), 1);
        match mode {
            "cancel" => {
                assert_eq!(result[0].delivery, RustDelivery::NotAdmitted);
                assert!(matches!(result[0].result, Err(RustPluginError::Cancelled)));
            }
            "deadline" => {
                assert_eq!(result[0].delivery, RustDelivery::NotAdmitted);
                assert!(matches!(result[0].result, Err(RustPluginError::Deadline)));
            }
            "retire" => assert_eq!(result[0].delivery, RustDelivery::Retired),
            _ => unreachable!(),
        }
        if mode != "retire" {
            assert!(active[0].result.is_ok() && queued[0].result.is_ok());
        }
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            if mode == "retire" { 1 } else { 2 }
        );
        drop(result);
        drop(active);
        drop(queued);
        settled(|| budget.snapshot().reserved_bytes == 0).await;
        finish(&plugins, &budget, &lifecycle).await;
    }
}

#[tokio::test]
async fn critical_global_admission_releases_the_channel_permit_while_output_holds_the_quota() {
    let (plugins, budget, lifecycle) = fixture(
        Provider::new(false),
        ActorLimits {
            total_callbacks: 1,
            per_plugin_queue: 1,
            ..ActorLimits::default()
        },
    );
    let stop = lifecycle.token();
    let code = observed_module(false, false);
    drop(
        plugins
            .load("holder", &code, &stop, deadline())
            .await
            .unwrap(),
    );
    drop(
        plugins
            .load("waiting", &code, &stop, deadline())
            .await
            .unwrap(),
    );
    let held = plugins
        .dispatch(
            &Event::Tick { tick: 1 },
            &server_context(),
            Some("holder"),
            &stop,
            deadline(),
        )
        .await;
    assert!(held[0].result.is_ok());
    let actor = plugins
        .inner
        .actors
        .lock()
        .unwrap()
        .get("waiting")
        .unwrap()
        .clone();
    let recipient: Vec<_> = plugins
        .recipients()
        .into_iter()
        .filter(|id| id.name() == "waiting")
        .collect();
    let (context, _, quit) = player_lifecycle_events();
    let parent = lifecycle.cleanup_token();
    let mut waiting =
        Box::pin(plugins.dispatch_to_recipients(&quit, &context, &recipient, &parent, deadline()));
    let pending = poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx).is_pending())).await;
    let free_channel = actor.sender.capacity();
    let held_count = plugins.stats().callbacks;
    drop(held);
    assert!(
        pending,
        "critical callback did not wait for global output ownership to end"
    );
    assert_eq!(
        free_channel, 1,
        "admission retained a channel slot while waiting for global quota"
    );
    assert_eq!(held_count, 1);
    let results = tokio::time::timeout(Duration::from_millis(100), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(results[0].delivery, RustDelivery::Delivered);
    assert_action(results[0].result.as_ref().unwrap(), "quit");
    assert!(plugins.stats().peak_callbacks <= 1);
    drop(results);
    finish(&plugins, &budget, &lifecycle).await;
}

#[tokio::test]
async fn an_impossible_critical_payload_fails_before_waiting_or_cloning() {
    let provider = Provider::new(false);
    let (plugins, budget, lifecycle) = fixture(
        provider.clone(),
        ActorLimits {
            event_bytes: 1024,
            ..ActorLimits::default()
        },
    );
    drop(
        plugins
            .load(
                "bounded",
                &observed_module(false, false),
                &lifecycle.token(),
                deadline(),
            )
            .await
            .unwrap(),
    );
    let recipient = plugins.recipients();
    let (context, _, mut quit) = player_lifecycle_events();
    let Event::PlayerQuit { name, .. } = &mut quit else {
        unreachable!()
    };
    *name = "x".repeat(2048);
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        plugins.dispatch_to_recipients(
            &quit,
            &context,
            &recipient,
            &lifecycle.cleanup_token(),
            deadline(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result[0].delivery, RustDelivery::NotAdmitted);
    assert!(matches!(result[0].result, Err(RustPluginError::Busy)));
    assert_eq!(plugins.stats().callbacks, 0);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(plugins.list()[0].enabled);
    drop(result);
    finish(&plugins, &budget, &lifecycle).await;
}

struct Provider {
    calls: AtomicUsize,
    contexts: Mutex<Vec<NativeCallContext>>,
    block_first: bool,
    started: Semaphore,
    release: Semaphore,
}

impl Provider {
    fn new(block_first: bool) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            contexts: Mutex::new(Vec::new()),
            block_first,
            started: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }
}

impl NativeQueryService for Provider {
    fn query(&self, context: NativeCallContext, query: NativeQuery) -> NativeQueryFuture<'_> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        self.contexts.lock().unwrap().push(context);
        assert_eq!(query, NativeQuery::Worlds);
        Box::pin(async move {
            if self.block_first && index == 0 {
                self.started.add_permits(1);
                self.release.acquire().await.unwrap().forget();
            }
            Ok(NativeQueryValue::Worlds { worlds: Vec::new() })
        })
    }
}

fn fixture(
    provider: Arc<Provider>,
    actor_limits: ActorLimits,
) -> (Arc<RustPlugins>, NativeReadBudget, Lifecycle) {
    let lifecycle = Lifecycle::new();
    let budget = NativeReadBudget::new(DEFAULT_READ_BUDGET_BYTES).unwrap();
    let queries = NativeQueryRuntime::new(
        provider,
        budget.clone(),
        lifecycle.clone(),
        NativeProviderContract::future_owned(MAX_PROVIDER_WORKING_SET_BYTES).unwrap(),
    );
    let plugins = RustPlugins::new(
        Limits {
            fuel_per_event: 100_000,
            ..Limits::default()
        },
        queries,
        lifecycle.clone(),
        actor_limits,
    )
    .unwrap();
    (Arc::new(plugins), budget, lifecycle)
}

async fn started(provider: &Provider) {
    tokio::time::timeout(Duration::from_secs(2), provider.started.acquire())
        .await
        .expect("actor did not start its native provider")
        .unwrap()
        .forget();
}

async fn settled(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actor did not settle its owned resources");
}

async fn finish(plugins: &RustPlugins, budget: &NativeReadBudget, lifecycle: &Lifecycle) {
    let results = plugins.shutdown(deadline()).await;
    assert!(results.iter().all(|result| result.result.is_ok()));
    drop(results);
    lifecycle.stop();
    lifecycle.drain(deadline()).await.unwrap();
    let stats = plugins.stats();
    assert_eq!(stats.callbacks, 0);
    assert_eq!(stats.bytes, 0);
    assert_eq!(stats.cpu_jobs, 0);
    assert_eq!(budget.snapshot().reserved_bytes, 0);
}

#[tokio::test]
async fn returned_outputs_retain_callback_quota_until_the_caller_drops_them() {
    let provider = Provider::new(false);
    let limits = ActorLimits {
        total_callbacks: 2,
        ..ActorLimits::default()
    };
    let (plugins, budget, lifecycle) = fixture(provider, limits);
    let code = plain_module("");
    let stop = lifecycle.token();
    let first = plugins
        .load("first", &code, &stop, deadline())
        .await
        .unwrap();
    let second = plugins
        .load("second", &code, &stop, deadline())
        .await
        .unwrap();
    assert_eq!(plugins.stats().callbacks, 2);
    assert!(plugins.stats().bytes > 0);
    let rejected = plugins
        .dispatch(
            &Event::Tick { tick: 1 },
            &server_context(),
            Some("first"),
            &stop,
            deadline(),
        )
        .await;
    assert_eq!(rejected.len(), 1);
    assert!(rejected[0].result.is_err());
    assert_eq!(plugins.stats().callbacks, 2);
    assert!(plugins.stats().rejections > 0);
    drop(first);
    let delivered = plugins
        .dispatch(
            &Event::Tick { tick: 2 },
            &server_context(),
            Some("first"),
            &stop,
            deadline(),
        )
        .await;
    assert_eq!(delivered.len(), 1);
    assert!(delivered[0].result.is_ok());
    assert_eq!(plugins.stats().callbacks, 2);
    assert!(plugins.stats().peak_callbacks <= 2);
    drop(delivered);
    drop(second);
    finish(&plugins, &budget, &lifecycle).await;
}

#[tokio::test]
async fn failed_load_and_duplicate_name_do_not_consume_callback_or_instance_capacity() {
    let provider = Provider::new(false);
    let (plugins, budget, lifecycle) = fixture(provider, ActorLimits::default());
    let stop = lifecycle.token();
    assert!(
        plugins
            .load("bad", b"not wasm", &stop, deadline())
            .await
            .is_err()
    );
    assert!(
        plugins
            .list()
            .iter()
            .all(|plugin| plugin.name != "bad" || !plugin.enabled)
    );
    let code = plain_module("");
    drop(
        plugins
            .load("valid", &code, &stop, deadline())
            .await
            .unwrap(),
    );
    assert!(
        plugins
            .load("valid", &code, &stop, deadline())
            .await
            .is_err()
    );
    assert_eq!(
        plugins
            .list()
            .iter()
            .filter(|plugin| plugin.name == "valid")
            .count(),
        1
    );
    settled(|| plugins.stats().callbacks == 0).await;
    finish(&plugins, &budget, &lifecycle).await;
}

#[tokio::test]
async fn a_native_wait_releases_the_cpu_slot_and_does_not_block_another_plugin_or_list() {
    let provider = Provider::new(true);
    let (plugins, budget, lifecycle) = fixture(
        provider.clone(),
        ActorLimits {
            cpu_jobs: 1,
            ..ActorLimits::default()
        },
    );
    let stop = lifecycle.token();
    drop(
        plugins
            .load("slow", &observed_module(true, false), &stop, deadline())
            .await
            .unwrap(),
    );
    drop(
        plugins
            .load("fast", &observed_module(false, false), &stop, deadline())
            .await
            .unwrap(),
    );
    let event = Event::Tick { tick: 1 };
    let context = server_context();
    let mut slow = Box::pin(plugins.dispatch(&event, &context, Some("slow"), &stop, deadline()));
    tokio::select! {
        _ = started(&provider) => {}
        _ = &mut slow => panic!("slow callback completed before its provider fence"),
    }
    assert_eq!(plugins.stats().cpu_jobs, 0);
    assert_eq!(
        plugins
            .list()
            .iter()
            .filter(|plugin| plugin.enabled)
            .count(),
        2
    );
    let fast = plugins
        .dispatch(&event, &context, Some("fast"), &stop, deadline())
        .await;
    assert_eq!(fast.len(), 1);
    assert_action(fast[0].result.as_ref().unwrap(), "tick");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    provider.release.add_permits(1);
    let slow = slow.await;
    assert_action(slow[0].result.as_ref().unwrap(), "tick");
    assert!(plugins.stats().peak_cpu_jobs <= 1);
    drop(fast);
    drop(slow);
    finish(&plugins, &budget, &lifecycle).await;
}

#[tokio::test]
async fn per_plugin_queue_is_bounded_and_a_cancelled_queued_callback_never_reaches_the_provider() {
    let provider = Provider::new(true);
    let (plugins, budget, lifecycle) = fixture(
        provider.clone(),
        ActorLimits {
            per_plugin_queue: 2,
            total_callbacks: 8,
            ..ActorLimits::default()
        },
    );
    let stop = lifecycle.token();
    drop(
        plugins
            .load("slow", &observed_module(true, false), &stop, deadline())
            .await
            .unwrap(),
    );
    let event = Event::Tick { tick: 1 };
    let context = server_context();
    let mut first = Box::pin(plugins.dispatch(&event, &context, Some("slow"), &stop, deadline()));
    tokio::select! {
        _ = started(&provider) => {}
        _ = &mut first => panic!("provider fence was not retained"),
    }
    let mut cancelled =
        Box::pin(plugins.dispatch(&event, &context, Some("slow"), &stop, deadline()));
    let mut queued = Box::pin(plugins.dispatch(&event, &context, Some("slow"), &stop, deadline()));
    poll_fn(|cx| {
        assert!(cancelled.as_mut().poll(cx).is_pending());
        assert!(queued.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let overflow = plugins
        .dispatch(&event, &context, Some("slow"), &stop, deadline())
        .await;
    assert_eq!(overflow.len(), 1);
    assert!(overflow[0].result.is_err());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(plugins.stats().rejections > 0);
    drop(cancelled);
    provider.release.add_permits(1);
    let first = first.await;
    let queued = queued.await;
    assert!(first[0].result.is_ok());
    assert!(queued[0].result.is_ok());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    drop(first);
    drop(queued);
    drop(overflow);
    finish(&plugins, &budget, &lifecycle).await;
}

#[tokio::test]
async fn cancelling_a_running_callback_retains_native_work_until_its_provider_fence_finishes() {
    let provider = Provider::new(true);
    let (plugins, budget, lifecycle) = fixture(provider.clone(), ActorLimits::default());
    let stop = lifecycle.token();
    drop(
        plugins
            .load("slow", &observed_module(true, false), &stop, deadline())
            .await
            .unwrap(),
    );
    let event = Event::Tick { tick: 1 };
    let context = server_context();
    let mut running = Box::pin(plugins.dispatch(&event, &context, Some("slow"), &stop, deadline()));
    tokio::select! {
        _ = started(&provider) => {}
        _ = &mut running => panic!("provider fence was not retained"),
    }
    drop(running);
    settled(|| budget.snapshot().quarantined_operations == 1).await;
    assert!(budget.snapshot().reserved_bytes > 0);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    provider.release.add_permits(1);
    settled(|| budget.snapshot().reserved_bytes == 0).await;
    assert!(
        !plugins
            .list()
            .iter()
            .any(|plugin| plugin.name == "slow" && plugin.enabled)
    );
    finish(&plugins, &budget, &lifecycle).await;
}

#[tokio::test]
async fn global_stop_rejects_normal_callbacks_but_delivers_quit_before_disable() {
    let provider = Provider::new(false);
    let (plugins, budget, lifecycle) = fixture(provider.clone(), ActorLimits::default());
    let stop = lifecycle.token();
    let enabled = plugins
        .load("observer", &observed_module(false, true), &stop, deadline())
        .await
        .unwrap();
    assert_action(&enabled, "enable");
    drop(enabled);
    lifecycle.stop();
    let rejected = plugins
        .dispatch(
            &Event::Tick { tick: 1 },
            &server_context(),
            Some("observer"),
            &stop,
            deadline(),
        )
        .await;
    assert_eq!(rejected.len(), 1);
    assert!(rejected[0].result.is_err());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    let uuid = uuid::Uuid::from_u128(42);
    let cleanup = lifecycle.cleanup_token();
    let quit = plugins
        .dispatch(
            &Event::PlayerQuit {
                uuid: uuid.to_string(),
                name: "Player".into(),
            },
            &NativeCallbackContext {
                source: NativeSource::Player {
                    uuid,
                    session_id: 1,
                },
            },
            Some("observer"),
            &cleanup,
            deadline(),
        )
        .await;
    assert_eq!(quit.len(), 1);
    assert_action(quit[0].result.as_ref().unwrap(), "quit");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    drop(quit);
    let disabled = plugins.shutdown(deadline()).await;
    assert_eq!(disabled.len(), 1);
    assert_action(disabled[0].result.as_ref().unwrap(), "disable");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    drop(disabled);
    lifecycle.drain(deadline()).await.unwrap();
    assert_eq!(budget.snapshot().reserved_bytes, 0);
    assert_eq!(plugins.stats().callbacks, 0);
}

#[tokio::test]
async fn a_guest_fuel_fault_disables_only_that_plugin_and_releases_the_cpu_permit() {
    let provider = Provider::new(false);
    let (plugins, budget, lifecycle) = fixture(
        provider,
        ActorLimits {
            cpu_jobs: 1,
            quantum: 32,
            ..ActorLimits::default()
        },
    );
    let stop = lifecycle.token();
    let fault = plain_module(
        "i32.const 10 i32.load8_u i32.const 116 i32.eq if (loop $forever br $forever) end",
    );
    drop(
        plugins
            .load("broken", &fault, &stop, deadline())
            .await
            .unwrap(),
    );
    drop(
        plugins
            .load("healthy", &plain_module(""), &stop, deadline())
            .await
            .unwrap(),
    );
    let results = plugins
        .dispatch(
            &Event::Tick { tick: 1 },
            &server_context(),
            Some("broken"),
            &stop,
            deadline(),
        )
        .await;
    assert_eq!(results.len(), 1);
    assert!(results[0].result.is_err());
    assert!(
        !plugins
            .list()
            .iter()
            .any(|plugin| plugin.name == "broken" && plugin.enabled)
    );
    let healthy = plugins
        .dispatch(
            &Event::Tick { tick: 2 },
            &server_context(),
            Some("healthy"),
            &stop,
            deadline(),
        )
        .await;
    assert_eq!(healthy.len(), 1);
    assert!(healthy[0].result.is_ok());
    assert!(plugins.stats().peak_cpu_jobs <= 1);
    drop(results);
    drop(healthy);
    finish(&plugins, &budget, &lifecycle).await;
}

#[tokio::test]
async fn dropping_the_last_service_owner_disables_healthy_instances_before_drain_finishes() {
    let provider = Provider::new(false);
    let (plugins, budget, lifecycle) = fixture(provider.clone(), ActorLimits::default());
    let stop = lifecycle.token();
    drop(
        plugins
            .load(
                "drop_owner",
                &observed_module(false, true),
                &stop,
                deadline(),
            )
            .await
            .unwrap(),
    );
    lifecycle.stop();
    drop(plugins);
    lifecycle.drain(deadline()).await.unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(budget.snapshot().reserved_bytes, 0);
}

#[tokio::test]
async fn owned_command_arguments_are_charged_before_cloning_even_when_their_json_is_small() {
    let provider = Provider::new(false);
    let (plugins, budget, lifecycle) = fixture(
        provider,
        ActorLimits {
            event_bytes: 1024,
            total_callbacks: 64,
            ..ActorLimits::default()
        },
    );
    let stop = lifecycle.token();
    drop(
        plugins
            .load("bytes", &plain_module(""), &stop, deadline())
            .await
            .unwrap(),
    );
    let mut command = Event::Command {
        sender: "CONSOLE".into(),
        sender_kind: zerqon_plugin_api::SenderKind::Console,
        operator: true,
        command: "probe".into(),
        args: vec![String::new(); 128],
    };
    assert!(serde_json::to_vec(&command).unwrap().len() < 1024);
    assert!(128 * std::mem::size_of::<String>() > 1024);
    let context = NativeCallbackContext {
        source: NativeSource::Console,
    };
    let rejected = plugins
        .dispatch(&command, &context, Some("bytes"), &stop, deadline())
        .await;
    assert_eq!(rejected.len(), 1);
    assert!(
        rejected[0].result.is_err(),
        "cloned Vec<String> storage exceeded the byte quota"
    );
    assert_eq!(plugins.stats().callbacks, 0);
    assert!(plugins.stats().rejections > 0);
    let Event::Command { args, .. } = &mut command else {
        unreachable!()
    };
    args.clear();
    args.push("small".into());
    let accepted = plugins
        .dispatch(&command, &context, Some("bytes"), &stop, deadline())
        .await;
    assert_eq!(accepted.len(), 1);
    assert!(accepted[0].result.is_ok());
    assert!(plugins.stats().bytes <= 1024);
    drop(accepted);
    finish(&plugins, &budget, &lifecycle).await;
}

#[tokio::test]
async fn cancelled_native_enable_cleans_its_generation_and_allows_the_same_name_to_load_again() {
    let provider = Provider::new(true);
    let (plugins, budget, lifecycle) = fixture(provider.clone(), ActorLimits::default());
    let stop = lifecycle.token();
    let code = observed_module_for_events(true, false, false);
    let mut loading = Box::pin(plugins.load("cancelled_enable", &code, &stop, deadline()));
    tokio::select! {
        _ = started(&provider) => {}
        _ = &mut loading => panic!("Enable completed before its native provider fence"),
    }
    assert!(
        !plugins
            .list()
            .iter()
            .any(|plugin| plugin.name == "cancelled_enable" && plugin.enabled)
    );
    drop(loading);
    settled(|| budget.snapshot().quarantined_operations == 1).await;
    assert!(budget.snapshot().reserved_bytes > 0);
    provider.release.add_permits(1);
    settled(|| budget.snapshot().reserved_bytes == 0).await;
    settled(|| {
        !plugins
            .list()
            .iter()
            .any(|plugin| plugin.name == "cancelled_enable")
    })
    .await;
    let enabled = plugins
        .load("cancelled_enable", &code, &stop, deadline())
        .await
        .unwrap();
    assert_action(&enabled, "enable");
    assert_eq!(enabled.stats().native_calls, 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert!(
        plugins
            .list()
            .iter()
            .any(|plugin| plugin.name == "cancelled_enable" && plugin.enabled)
    );
    drop(enabled);
    finish(&plugins, &budget, &lifecycle).await;
}

#[tokio::test]
async fn disable_output_keeps_its_actor_lifecycle_and_cleanup_allocation_owned_until_drop() {
    let provider = Provider::new(false);
    let (plugins, budget, lifecycle) = fixture(provider, ActorLimits::default());
    let stop = lifecycle.token();
    drop(
        plugins
            .load(
                "output_owner",
                &observed_module(false, false),
                &stop,
                deadline(),
            )
            .await
            .unwrap(),
    );
    let disabled = plugins.shutdown(deadline()).await;
    assert_eq!(disabled.len(), 1);
    assert_action(disabled[0].result.as_ref().unwrap(), "disable");
    assert_eq!(plugins.stats().cleanup_callbacks, 1);
    assert!(plugins.stats().cleanup_bytes > 0);
    lifecycle.stop();
    let mut drain = Box::pin(lifecycle.drain(deadline()));
    poll_fn(|cx| {
        assert!(drain.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(disabled);
    drain.await.unwrap();
    assert_eq!(plugins.stats().cleanup_callbacks, 0);
    assert_eq!(plugins.stats().cleanup_bytes, 0);
    assert_eq!(plugins.stats().action_bytes_reserved, 0);
    assert_eq!(budget.snapshot().reserved_bytes, 0);
}

#[tokio::test]
async fn a_late_old_generation_reply_cannot_affect_the_reloaded_plugin() {
    let provider = Provider::new(true);
    let (plugins, budget, lifecycle) = fixture(provider.clone(), ActorLimits::default());
    let stop = lifecycle.token();
    drop(
        plugins
            .load("reload", &observed_module(true, false), &stop, deadline())
            .await
            .unwrap(),
    );
    let event = Event::Tick { tick: 1 };
    let context = server_context();
    let mut old = Box::pin(plugins.dispatch(&event, &context, Some("reload"), &stop, deadline()));
    tokio::select! {
        _ = started(&provider) => {}
        _ = &mut old => panic!("old generation completed before its provider fence"),
    }
    drop(plugins.unload("reload", deadline()).await.unwrap());
    let old = old.await;
    assert!(old[0].result.is_err());
    settled(|| budget.snapshot().quarantined_operations == 1).await;
    assert!(budget.snapshot().reserved_bytes > 0);
    let enabled = plugins
        .load("reload", &observed_module(false, false), &stop, deadline())
        .await
        .unwrap();
    assert_action(&enabled, "enable");
    drop(enabled);
    provider.release.add_permits(1);
    settled(|| budget.snapshot().reserved_bytes == 0).await;
    let current = plugins
        .dispatch(&event, &context, Some("reload"), &stop, deadline())
        .await;
    assert_eq!(current.len(), 1);
    assert_action(current[0].result.as_ref().unwrap(), "tick");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    drop(current);
    finish(&plugins, &budget, &lifecycle).await;
}

#[tokio::test]
async fn a_finished_enable_cancelled_before_publication_runs_only_owned_cleanup() {
    let provider = Provider::new(false);
    let (plugins, budget, lifecycle) = fixture(provider.clone(), ActorLimits::default());
    let services = Arc::clone(&plugins.inner.services);
    let lifetime = Arc::new(ActorLifetime {
        name: "retired".into(),
        job: Mutex::new(Some(
            lifecycle.register("controlled completed Enable").unwrap(),
        )),
        failure: Mutex::new(None),
    });
    let actor = Arc::new(ActorControl {
        generation: 1,
        info: Mutex::new(PluginInfo {
            name: "retired".into(),
            enabled: false,
            disabled_reason: None,
        }),
        stopping: AtomicBool::new(false),
        done: AtomicBool::new(false),
        abandoned: AtomicBool::new(false),
        changed: Notify::new(),
        exit: Mutex::new(None),
        cleanup: Mutex::new(Some(
            services
                .admission
                .reserve(
                    event_bytes(&Event::Disable, &server_context(), &services.limits).unwrap(),
                    true,
                )
                .unwrap(),
        )),
    });
    let event = Event::Enable {
        server_version: "0.0.1".into(),
    };
    let lease = Arc::new(CallbackLease {
        native: Mutex::new(None),
        _event: services
            .admission
            .reserve(
                event_bytes(&event, &server_context(), &services.limits).unwrap(),
                false,
            )
            .unwrap(),
        _actor: Arc::clone(&lifetime),
    });
    let instance = services
        .host
        .instantiate("retired", &observed_module(false, true), READ_CAPABILITIES)
        .unwrap();
    let prepared = instance.prepare(&event, &server_context()).unwrap();
    let control = CallbackControl::new(deadline().into_std());
    let mut invocation = instance
        .begin(prepared, control.clone(), Box::new(lease))
        .unwrap();
    let done = loop {
        match invocation.run().unwrap() {
            InvocationStep::Finished(done) => break done,
            InvocationStep::Quantum(next) => invocation = next,
            InvocationStep::Read(_) => panic!("Enable must finish before the cancellation barrier"),
        }
    };
    assert_eq!(plugins.stats().callbacks, 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    control.cancel();
    actor.stop();
    lifecycle.stop();
    let output = retire_finished(&services, &actor, &lifetime, "retired", true, done)
        .await
        .unwrap();
    assert_action(&output, "disable");
    assert_eq!(output.stats().native_calls, 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(services.host.live_instances(), 0);
    assert_eq!(plugins.stats().callbacks, 0);
    assert_eq!(plugins.stats().cleanup_callbacks, 1);
    assert!(actor.cleanup.lock().unwrap().is_none());
    assert_eq!(budget.snapshot().reserved_bytes, 0);
    drop(actor);
    drop(lifetime);
    let mut drain = Box::pin(lifecycle.drain(deadline()));
    poll_fn(|cx| {
        assert!(drain.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(output);
    drain.await.unwrap();
    assert_eq!(plugins.stats().cleanup_callbacks, 0);
    assert_eq!(plugins.stats().cleanup_bytes, 0);
    assert_eq!(plugins.stats().action_bytes_reserved, 0);
}

#[tokio::test]
async fn quit_cleanup_keeps_its_callers_explicit_cancellation_scope() {
    let provider = Provider::new(true);
    let (plugins, budget, lifecycle) = fixture(provider.clone(), ActorLimits::default());
    drop(
        plugins
            .load(
                "quit_cancel",
                &observed_module(false, true),
                &lifecycle.token(),
                deadline(),
            )
            .await
            .unwrap(),
    );
    lifecycle.stop();
    let parent = lifecycle.cleanup_token().child();
    let uuid = uuid::Uuid::from_u128(81);
    let event = Event::PlayerQuit {
        uuid: uuid.to_string(),
        name: "Quitter".into(),
    };
    let context = NativeCallbackContext {
        source: NativeSource::Player {
            uuid,
            session_id: 1,
        },
    };
    let mut pending =
        Box::pin(plugins.dispatch(&event, &context, Some("quit_cancel"), &parent, deadline()));
    tokio::select! {
        _ = started(&provider) => {}
        _ = &mut pending => panic!("Quit callback completed before its provider fence"),
    }
    parent.request_stop();
    let observed = tokio::time::timeout(Duration::from_millis(100), &mut pending).await;
    provider.release.add_permits(1);
    let timely = observed.is_ok();
    let result = match observed {
        Ok(result) => result,
        Err(_) => pending.await,
    };
    assert!(
        timely,
        "Quit ignored its caller's scoped cancellation until the provider completed"
    );
    assert_eq!(result.len(), 1);
    assert!(result[0].result.is_err());
    drop(result);
    settled(|| budget.snapshot().reserved_bytes == 0).await;
    finish(&plugins, &budget, &lifecycle).await;
}

#[tokio::test]
async fn concurrent_native_disable_callbacks_wait_for_the_shared_read_workspace() {
    let provider = Provider::new(false);
    let lifecycle = Lifecycle::new();
    let workspace = crate::native_query_runtime::WORKSPACE_BYTES;
    let budget = NativeReadBudget::new(workspace).unwrap();
    let queries = NativeQueryRuntime::new(
        provider.clone(),
        budget.clone(),
        lifecycle.clone(),
        NativeProviderContract::future_owned(MAX_PROVIDER_WORKING_SET_BYTES).unwrap(),
    );
    let plugins = Arc::new(
        RustPlugins::new(
            Limits::default(),
            queries,
            lifecycle.clone(),
            ActorLimits {
                cpu_jobs: 1,
                ..ActorLimits::default()
            },
        )
        .unwrap(),
    );
    let code = observed_module(false, true);
    let stop = lifecycle.token();
    drop(
        plugins
            .load("cleanup_one", &code, &stop, deadline())
            .await
            .unwrap(),
    );
    drop(
        plugins
            .load("cleanup_two", &code, &stop, deadline())
            .await
            .unwrap(),
    );
    assert_eq!(budget.snapshot().reserved_bytes, 0);
    let baseline_rejections = budget.snapshot().rejections;
    let held_cpu = Arc::clone(&plugins.inner.services.cpu)
        .acquire_owned()
        .await
        .unwrap();
    let shutting_down = Arc::clone(&plugins);
    let shutdown = tokio::spawn(async move { shutting_down.shutdown(deadline()).await });
    let acquired = tokio::time::timeout(Duration::from_secs(1), async {
        while budget.snapshot().reserved_bytes != workspace {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let contended = tokio::time::timeout(Duration::from_millis(100), async {
        while budget.snapshot().rejections == baseline_rejections {
            tokio::task::yield_now().await;
        }
    })
    .await;
    drop(held_cpu);
    let results = tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .unwrap()
        .unwrap();
    assert!(
        acquired.is_ok(),
        "the first Disable never acquired its read workspace"
    );
    assert!(
        contended.is_ok(),
        "the second Disable never attempted the occupied workspace"
    );
    assert_eq!(results.len(), 2);
    for result in &results {
        assert_action(result.result.as_ref().unwrap(), "disable");
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(budget.snapshot().peak_bytes, workspace);
    drop(results);
    lifecycle.stop();
    lifecycle.drain(deadline()).await.unwrap();
    assert_eq!(budget.snapshot().reserved_bytes, 0);
    assert_eq!(plugins.stats().callbacks, 0);
    assert_eq!(plugins.stats().cleanup_callbacks, 0);
    assert_eq!(plugins.stats().action_bytes_reserved, 0);
}

#[tokio::test]
async fn normal_callbacks_wait_for_memory_and_respect_cancellation_without_disabling_the_instance()
{
    for cancellation in [Some(true), Some(false), None] {
        let provider = Provider::new(true);
        let lifecycle = Lifecycle::new();
        let budget = NativeReadBudget::new(crate::native_query_runtime::WORKSPACE_BYTES).unwrap();
        let queries = NativeQueryRuntime::new(
            provider.clone(),
            budget.clone(),
            lifecycle.clone(),
            NativeProviderContract::future_owned(MAX_PROVIDER_WORKING_SET_BYTES).unwrap(),
        );
        let plugins = Arc::new(
            RustPlugins::new(
                Limits::default(),
                queries,
                lifecycle.clone(),
                ActorLimits::default(),
            )
            .unwrap(),
        );
        let stop = lifecycle.token();
        drop(
            plugins
                .load("occupier", &observed_module(true, false), &stop, deadline())
                .await
                .unwrap(),
        );
        drop(
            plugins
                .load("waiter", &observed_module(false, false), &stop, deadline())
                .await
                .unwrap(),
        );
        let event = Event::Tick { tick: 1 };
        let context = server_context();
        let mut occupier =
            Box::pin(plugins.dispatch(&event, &context, Some("occupier"), &stop, deadline()));
        tokio::select! {
            _ = started(&provider) => {}
            _ = &mut occupier => panic!("occupier completed before its provider fence"),
        }
        let baseline = budget.snapshot().rejections;
        let caller = lifecycle.token().child();
        let waiting_deadline = if cancellation == Some(false) {
            Instant::now() + Duration::from_millis(30)
        } else {
            deadline()
        };
        let mut waiting =
            Box::pin(plugins.dispatch(&event, &context, Some("waiter"), &caller, waiting_deadline));
        let mut early = None;
        let reached = tokio::select! {
            _ = settled(|| budget.snapshot().rejections > baseline) => true,
            result = &mut waiting => { early = Some(result); false },
        };
        if cancellation == Some(true) {
            caller.request_stop();
        }
        if cancellation.is_none() {
            provider.release.add_permits(1);
        }
        let result = match early {
            Some(result) => Ok(result),
            None => tokio::time::timeout(Duration::from_millis(100), &mut waiting).await,
        };
        if cancellation.is_some() {
            provider.release.add_permits(1);
        }
        let occupier = occupier.await;
        assert!(
            reached,
            "callback did not wait for the occupied native workspace"
        );
        let waiting = result.unwrap();
        assert_eq!(waiting.len(), 1);
        match cancellation {
            Some(true) => assert!(matches!(waiting[0].result, Err(RustPluginError::Cancelled))),
            Some(false) => assert!(matches!(waiting[0].result, Err(RustPluginError::Deadline))),
            None => assert_action(waiting[0].result.as_ref().unwrap(), "tick"),
        }
        assert!(occupier[0].result.is_ok());
        assert!(
            plugins
                .list()
                .iter()
                .any(|plugin| plugin.name == "waiter" && plugin.enabled)
        );
        drop(waiting);
        drop(occupier);
        let retried = plugins
            .dispatch(&event, &context, Some("waiter"), &stop, deadline())
            .await;
        assert_action(retried[0].result.as_ref().unwrap(), "tick");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        drop(retried);
        finish(&plugins, &budget, &lifecycle).await;
    }
}

#[tokio::test]
async fn all_sixty_four_native_plugins_disable_with_the_default_sixteen_workspace_pool() {
    let provider = Provider::new(false);
    let (plugins, budget, lifecycle) = fixture(provider.clone(), ActorLimits::default());
    let code = observed_module(false, true);
    let stop = lifecycle.token();
    for index in 0..64 {
        drop(
            plugins
                .load(&format!("native_{index}"), &code, &stop, deadline())
                .await
                .unwrap(),
        );
    }
    assert_eq!(plugins.list().len(), 64);
    assert_eq!(plugins.stats().callbacks, 0);
    assert_eq!(budget.snapshot().reserved_bytes, 0);
    let held_cpu = Arc::clone(&plugins.inner.services.cpu)
        .acquire_many_owned(2)
        .await
        .unwrap();
    lifecycle.stop();
    let shutting_down = Arc::clone(&plugins);
    let shutdown = tokio::spawn(async move { shutting_down.shutdown(deadline()).await });
    let full = tokio::time::timeout(Duration::from_secs(1), async {
        while budget.snapshot().reserved_bytes != DEFAULT_READ_BUDGET_BYTES {
            tokio::task::yield_now().await;
        }
    })
    .await;
    drop(held_cpu);
    let results = tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .unwrap()
        .unwrap();
    assert!(
        full.is_ok(),
        "default read pool never held its sixteen reserved workspaces"
    );
    assert_eq!(results.len(), 64);
    for result in &results {
        assert_action(result.result.as_ref().unwrap(), "disable");
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 64);
    assert_eq!(budget.snapshot().peak_bytes, DEFAULT_READ_BUDGET_BYTES);
    assert!(plugins.stats().peak_cpu_jobs <= 2);
    assert!(plugins.stats().peak_callbacks <= 64);
    assert!(plugins.stats().peak_bytes <= 4 * 1024 * 1024);
    drop(results);
    lifecycle.drain(deadline()).await.unwrap();
    assert_eq!(budget.snapshot().reserved_bytes, 0);
    assert_eq!(plugins.stats().callbacks, 0);
    assert_eq!(plugins.stats().bytes, 0);
    assert_eq!(plugins.stats().cleanup_callbacks, 0);
    assert_eq!(plugins.stats().cleanup_bytes, 0);
    assert_eq!(plugins.stats().action_bytes_reserved, 0);
}

#[tokio::test]
async fn cancellation_while_waiting_for_a_cpu_permit_preserves_a_never_started_legacy_instance() {
    for mode in ["parent_cancel", "waiter_drop", "deadline"] {
        let provider = Provider::new(false);
        let limits = ActorLimits {
            cpu_jobs: 1,
            ..ActorLimits::default()
        };
        let queue_capacity = limits.per_plugin_queue;
        let (plugins, budget, lifecycle) = fixture(provider, limits);
        let stop = lifecycle.token();
        drop(
            plugins
                .load("preflight", &plain_module(""), &stop, deadline())
                .await
                .unwrap(),
        );
        let actor = plugins
            .inner
            .actors
            .lock()
            .unwrap()
            .get("preflight")
            .unwrap()
            .clone();
        let held_cpu = Arc::clone(&plugins.inner.services.cpu)
            .acquire_owned()
            .await
            .unwrap();
        let caller = lifecycle.token().child();
        let event = Event::Tick { tick: 1 };
        let context = server_context();
        let expires = if mode == "deadline" {
            Instant::now() + Duration::from_millis(30)
        } else {
            deadline()
        };
        let mut pending =
            Box::pin(plugins.dispatch(&event, &context, Some("preflight"), &caller, expires));
        let admitted = tokio::select! {
            _ = settled(|| plugins.stats().callbacks == 1 && actor.sender.capacity() == queue_capacity) => true,
            _ = &mut pending => false,
        };
        if !admitted {
            drop(held_cpu);
            panic!("{mode}: callback did not reach CPU admission after leaving the queue");
        }
        let result = if mode == "waiter_drop" {
            drop(pending);
            None
        } else {
            if mode == "parent_cancel" {
                caller.request_stop();
            }
            Some(tokio::time::timeout(Duration::from_millis(100), pending).await)
        };
        drop(held_cpu);
        if let Some(result) = result {
            let result = result.unwrap();
            assert_eq!(result.len(), 1);
            if mode == "deadline" {
                assert!(matches!(result[0].result, Err(RustPluginError::Deadline)));
            } else {
                assert!(matches!(result[0].result, Err(RustPluginError::Cancelled)));
            }
        }
        settled(|| plugins.stats().callbacks == 0).await;
        assert!(
            plugins
                .list()
                .iter()
                .any(|plugin| plugin.name == "preflight" && plugin.enabled),
            "{mode}: a callback that never executed destroyed its healthy instance"
        );
        let retried = plugins
            .dispatch(&event, &context, Some("preflight"), &stop, deadline())
            .await;
        assert_eq!(retried.len(), 1);
        assert!(retried[0].result.is_ok());
        drop(retried);
        finish(&plugins, &budget, &lifecycle).await;
    }
}

#[tokio::test]
async fn a_cancelled_cleanup_parent_cannot_admit_quit_after_global_stop() {
    let provider = Provider::new(false);
    let (plugins, budget, lifecycle) = fixture(provider.clone(), ActorLimits::default());
    drop(
        plugins
            .load(
                "quit_pre_cancelled",
                &observed_module(false, true),
                &lifecycle.token(),
                deadline(),
            )
            .await
            .unwrap(),
    );
    lifecycle.stop();
    let parent = lifecycle.cleanup_token().child();
    parent.request_stop();
    let uuid = uuid::Uuid::from_u128(82);
    let before = plugins.stats();
    let result = plugins
        .dispatch(
            &Event::PlayerQuit {
                uuid: uuid.to_string(),
                name: "Quitter".into(),
            },
            &NativeCallbackContext {
                source: NativeSource::Player {
                    uuid,
                    session_id: 1,
                },
            },
            Some("quit_pre_cancelled"),
            &parent,
            deadline(),
        )
        .await;
    let rejected = matches!(result[0].result, Err(RustPluginError::Cancelled));
    let provider_calls = provider.calls.load(Ordering::SeqCst);
    let after = plugins.stats();
    drop(result);
    finish(&plugins, &budget, &lifecycle).await;
    assert!(
        rejected,
        "an explicitly cancelled cleanup scope admitted Quit"
    );
    assert_eq!(provider_calls, 0, "cancelled Quit reached the provider");
    assert_eq!(after.callbacks, before.callbacks);
    assert_eq!(after.bytes, before.bytes);
}

#[tokio::test]
async fn global_stop_drains_an_accepted_native_tick_then_delivers_quit_and_disable() {
    let provider = Provider::new(true);
    let (plugins, budget, lifecycle) = fixture(provider.clone(), ActorLimits::default());
    let stop = lifecycle.token();
    drop(
        plugins
            .load("graceful", &observed_module(true, true), &stop, deadline())
            .await
            .unwrap(),
    );
    let event = Event::Tick { tick: 1 };
    let context = server_context();
    let mut tick =
        Box::pin(plugins.dispatch(&event, &context, Some("graceful"), &stop, deadline()));
    tokio::select! {
        _ = started(&provider) => {}
        _ = &mut tick => panic!("Tick finished before its accepted provider fence"),
    }
    lifecycle.stop();
    provider.release.add_permits(3);
    let tick = tick.await;
    assert_eq!(tick.len(), 1);
    assert_action(tick[0].result.as_ref().unwrap(), "tick");
    assert!(
        plugins
            .list()
            .iter()
            .any(|plugin| plugin.name == "graceful" && plugin.enabled)
    );
    drop(tick);
    let uuid = uuid::Uuid::from_u128(83);
    let quit = plugins
        .dispatch(
            &Event::PlayerQuit {
                uuid: uuid.to_string(),
                name: "Closing".into(),
            },
            &NativeCallbackContext {
                source: NativeSource::Player {
                    uuid,
                    session_id: 1,
                },
            },
            Some("graceful"),
            &lifecycle.cleanup_token(),
            deadline(),
        )
        .await;
    assert_eq!(quit.len(), 1);
    assert_action(quit[0].result.as_ref().unwrap(), "quit");
    drop(quit);
    let disabled = plugins.shutdown(deadline()).await;
    assert_eq!(disabled.len(), 1);
    assert_action(disabled[0].result.as_ref().unwrap(), "disable");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    drop(disabled);
    assert!(plugins.shutdown(deadline()).await.is_empty());
    lifecycle.drain(deadline()).await.unwrap();
    assert_eq!(budget.snapshot().reserved_bytes, 0);
    assert_eq!(plugins.stats().callbacks, 0);
    assert_eq!(plugins.stats().cleanup_callbacks, 0);
}

#[tokio::test]
async fn global_stop_drains_already_queued_legacy_and_native_callbacks_but_rejects_new_ones() {
    for native in [false, true] {
        let provider = Provider::new(native);
        let limits = ActorLimits {
            cpu_jobs: 1,
            ..ActorLimits::default()
        };
        let queue_capacity = limits.per_plugin_queue;
        let (plugins, budget, lifecycle) = fixture(provider.clone(), limits);
        let stop = lifecycle.token();
        let code = if native {
            observed_module(true, true)
        } else {
            plain_module("")
        };
        drop(
            plugins
                .load("queued_stop", &code, &stop, deadline())
                .await
                .unwrap(),
        );
        let actor = plugins
            .inner
            .actors
            .lock()
            .unwrap()
            .get("queued_stop")
            .unwrap()
            .clone();
        let held_cpu = if native {
            None
        } else {
            Some(
                Arc::clone(&plugins.inner.services.cpu)
                    .acquire_owned()
                    .await
                    .unwrap(),
            )
        };
        let event = Event::Tick { tick: 1 };
        let context = server_context();
        let mut first =
            Box::pin(plugins.dispatch(&event, &context, Some("queued_stop"), &stop, deadline()));
        let admitted = if native {
            tokio::select! {
                _ = started(&provider) => true,
                _ = &mut first => false,
            }
        } else {
            tokio::select! {
                _ = settled(|| plugins.stats().callbacks == 1 && actor.sender.capacity() == queue_capacity) => true,
                _ = &mut first => false,
            }
        };
        let mut second =
            Box::pin(plugins.dispatch(&event, &context, Some("queued_stop"), &stop, deadline()));
        let pending = poll_fn(|cx| Poll::Ready(second.as_mut().poll(cx).is_pending())).await;
        let queued_capacity = actor.sender.capacity();
        lifecycle.stop();
        provider.release.add_permits(2);
        drop(held_cpu);
        assert!(
            admitted && pending,
            "callbacks were not accepted before global stop"
        );
        assert!(
            queued_capacity < queue_capacity,
            "second callback never entered the actor queue"
        );
        let first = first.await;
        let second = second.await;
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        for result in [&first[0], &second[0]] {
            if native {
                assert_action(result.result.as_ref().unwrap(), "tick");
            } else {
                assert!(result.result.as_ref().unwrap().actions().is_empty());
            }
        }
        assert!(
            plugins
                .list()
                .iter()
                .any(|plugin| plugin.name == "queued_stop" && plugin.enabled)
        );
        let rejected = plugins
            .dispatch(&event, &context, Some("queued_stop"), &stop, deadline())
            .await;
        assert_eq!(rejected.len(), 1);
        assert!(matches!(rejected[0].result, Err(RustPluginError::Stopped)));
        drop(first);
        drop(second);
        drop(rejected);
        finish(&plugins, &budget, &lifecycle).await;
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            if native { 3 } else { 0 }
        );
    }
}

#[tokio::test]
async fn an_accepted_native_callback_waiting_for_memory_survives_global_stop() {
    let provider = Provider::new(false);
    let lifecycle = Lifecycle::new();
    let budget = NativeReadBudget::new(crate::native_query_runtime::WORKSPACE_BYTES).unwrap();
    let queries = NativeQueryRuntime::new(
        provider.clone(),
        budget.clone(),
        lifecycle.clone(),
        NativeProviderContract::future_owned(MAX_PROVIDER_WORKING_SET_BYTES).unwrap(),
    );
    let plugins = RustPlugins::new(
        Limits::default(),
        queries.clone(),
        lifecycle.clone(),
        ActorLimits::default(),
    )
    .unwrap();
    let stop = lifecycle.token();
    drop(
        plugins
            .load(
                "admitted_wait",
                &observed_module(true, true),
                &stop,
                deadline(),
            )
            .await
            .unwrap(),
    );
    let held = queries
        .try_scope(NativeScopeContext {
            key: NativePluginKey::new(NativeBackend::Java, "external", 1).unwrap(),
            parent_id: 99,
            deadline: deadline(),
            stop: lifecycle.cleanup_token(),
            admission: NativeAdmission::Normal,
        })
        .unwrap();
    let baseline = budget.snapshot().rejections;
    let event = Event::Tick { tick: 1 };
    let context = server_context();
    let mut tick =
        Box::pin(plugins.dispatch(&event, &context, Some("admitted_wait"), &stop, deadline()));
    let waiting = tokio::select! {
        _ = settled(|| budget.snapshot().rejections > baseline) => true,
        _ = &mut tick => false,
    };
    lifecycle.stop();
    drop(held);
    assert!(
        waiting,
        "the accepted Tick never reached shared memory admission"
    );
    let tick = tick.await;
    assert_eq!(tick.len(), 1);
    assert_action(tick[0].result.as_ref().unwrap(), "tick");
    assert!(
        plugins
            .list()
            .iter()
            .any(|plugin| plugin.name == "admitted_wait" && plugin.enabled)
    );
    drop(tick);
    let rejected = plugins
        .dispatch(&event, &context, Some("admitted_wait"), &stop, deadline())
        .await;
    assert!(matches!(rejected[0].result, Err(RustPluginError::Stopped)));
    drop(rejected);
    let uuid = uuid::Uuid::from_u128(84);
    let quit = plugins
        .dispatch(
            &Event::PlayerQuit {
                uuid: uuid.to_string(),
                name: "Closing".into(),
            },
            &NativeCallbackContext {
                source: NativeSource::Player {
                    uuid,
                    session_id: 1,
                },
            },
            Some("admitted_wait"),
            &lifecycle.cleanup_token(),
            deadline(),
        )
        .await;
    assert_action(quit[0].result.as_ref().unwrap(), "quit");
    drop(quit);
    let disabled = plugins.shutdown(deadline()).await;
    assert_eq!(disabled.len(), 1);
    assert_action(disabled[0].result.as_ref().unwrap(), "disable");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    drop(disabled);
    lifecycle.drain(deadline()).await.unwrap();
    assert_eq!(budget.snapshot().reserved_bytes, 0);
    assert_eq!(plugins.stats().callbacks, 0);
    assert_eq!(plugins.stats().cleanup_callbacks, 0);
}
