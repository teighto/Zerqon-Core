#[allow(dead_code)]
#[path = "../src/lifecycle.rs"]
mod lifecycle;
#[allow(dead_code)]
#[path = "../src/shutdown_signals.rs"]
mod shutdown_signals;

use lifecycle::{DrainError, Lifecycle, RegistrationError};
use std::time::Duration;
use tokio::time::{Instant, timeout};

#[tokio::test]
async fn stopping_is_latched_before_waiters_and_children_only_stop_descendants() {
    let root = Lifecycle::new();
    let parent = root.token().child();
    let child = parent.child();
    let sibling = root.token().child();
    parent.request_stop();
    timeout(Duration::from_millis(100), child.cancelled())
        .await
        .unwrap();
    assert!(parent.is_stopped() && child.is_stopped());
    assert!(!root.token().is_stopped() && !sibling.is_stopped());
    root.stop();
    timeout(Duration::from_millis(100), sibling.cancelled())
        .await
        .unwrap();
    assert!(root.token().check().is_err());
}

#[tokio::test]
async fn cancelled_scope_does_not_wake_siblings_as_completed() {
    let root = Lifecycle::new();
    let a = root.token().child();
    let b = root.token().child();
    let wait = tokio::spawn({
        let b = b.clone();
        async move { b.cancelled().await }
    });
    a.request_stop();
    tokio::task::yield_now().await;
    assert!(!wait.is_finished());
    b.request_stop();
    timeout(Duration::from_secs(1), wait)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn drain_timeout_keeps_active_jobs_owned_and_seals_late_admission() {
    let root = Lifecycle::new();
    let guard = root.register("extract").unwrap();
    root.stop();
    assert!(matches!(
        root.register("new work"),
        Err(RegistrationError::Stopped)
    ));
    let cleanup = root.register_cleanup("close").unwrap();
    cleanup.finish(Ok(()));
    let error = root
        .drain(Instant::now() + Duration::from_millis(10))
        .await
        .unwrap_err();
    assert!(matches!(error, DrainError::Deadline { ref pending, .. } if pending == &["extract"]));
    assert_eq!(root.pending(), ["extract"]);
    assert!(matches!(
        root.register("late"),
        Err(RegistrationError::Sealed)
    ));
    root.register_cleanup("close existing resource")
        .unwrap()
        .finish(Ok(()));
    guard.finish(Ok(()));
    root.drain(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    assert!(matches!(
        root.register_cleanup("after completion"),
        Err(RegistrationError::Sealed)
    ));
}

#[tokio::test]
async fn abandoned_or_failed_jobs_never_report_a_clean_drain() {
    let root = Lifecycle::new();
    drop(root.register("abandoned").unwrap());
    root.register("child")
        .unwrap()
        .finish(Err("wait failed".into()));
    let error = root.drain(Instant::now()).await.unwrap_err();
    let DrainError::Cleanup { failures, omitted } = error else {
        panic!("expected cleanup failure");
    };
    assert_eq!(failures.len(), 2);
    assert!(failures[0].contains("abandoned"));
    assert!(failures[1].contains("wait failed"));
    assert_eq!(omitted, 0);
}

#[tokio::test]
async fn bounded_failure_storage_preserves_failure_status_and_utf8() {
    let root = Lifecycle::new();
    for _ in 0..140 {
        root.register("child")
            .unwrap()
            .finish(Err("🚀".repeat(400)));
    }
    let DrainError::Cleanup { failures, omitted } = root.drain(Instant::now()).await.unwrap_err()
    else {
        panic!("expected failure");
    };
    assert_eq!(failures.len(), 128);
    assert_eq!(omitted, 12);
    assert!(failures.iter().all(|error| error.len() <= 1031));
}

#[tokio::test]
async fn blocked_worker_retains_registration_after_its_awaiter_is_cancelled() {
    let root = Lifecycle::new();
    let guard = root.register("blocked worker").unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let waiter = tokio::spawn(async move {
        tokio::task::spawn_blocking(move || {
            started_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(3));
            guard.finish(Ok(()));
        })
        .await
        .unwrap();
    });
    started_rx.await.unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    root.stop();
    let result = root.drain(Instant::now() + Duration::from_millis(10)).await;
    release_tx.send(()).unwrap();
    assert!(matches!(result, Err(DrainError::Deadline { .. })));
    root.drain(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
}

#[test]
fn stop_before_ready_prevents_the_transition() {
    let root = Lifecycle::new();
    root.stop();
    assert!(root.mark_ready().is_err());
    assert!(!root.is_ready());
}

#[test]
fn a_failed_cleanup_prevents_readiness_even_when_no_stop_was_requested() {
    let root = Lifecycle::new();
    root.register("java probe")
        .unwrap()
        .finish(Err("child was not reaped".into()));
    assert!(!root.token().is_stopped());
    assert!(
        root.mark_ready().is_err(),
        "readiness was published after an unresolved cleanup failure"
    );
    assert!(!root.is_ready());
}

#[tokio::test]
async fn cleanup_scope_survives_global_stop_but_honors_its_local_cancellation() {
    let root = Lifecycle::new();
    let cleanup = root.cleanup_token();
    let nested = cleanup.child();
    root.stop();
    assert!(root.token().is_stopped());
    assert!(cleanup.check().is_ok() && nested.check().is_ok());
    cleanup.request_stop();
    timeout(Duration::from_secs(1), nested.cancelled())
        .await
        .unwrap();
}

#[tokio::test]
async fn legacy_shutdown_sender_wakes_scoped_waiters_and_repeated_stop_keeps_deadline() {
    let root = Lifecycle::new();
    let child = root.token().child();
    root.shutdown_sender().send_replace(true);
    timeout(Duration::from_secs(1), child.cancelled())
        .await
        .unwrap();
    root.stop();
    let first = root.cleanup_deadline().unwrap();
    root.stop();
    assert_eq!(root.cleanup_deadline(), Some(first));
    assert!(root.mark_ready().is_err());
}

#[tokio::test]
async fn preexisting_shutdown_marker_is_latched_before_install_returns() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("stop");
    std::fs::write(&marker, "").unwrap();
    let root = Lifecycle::new();
    let signals = shutdown_signals::ShutdownSignals::install(root.clone(), Some(marker)).unwrap();
    assert!(root.token().is_stopped());
    assert!(root.mark_ready().is_err());
    signals.close().await;
}

#[tokio::test]
async fn admitted_work_survives_global_stop_but_still_observes_parent_and_local_cancellation() {
    let root = Lifecycle::new();
    let parent = root.token().child();
    let admitted = parent.admitted_child();
    let sibling = root.token().admitted_child();
    root.stop();
    assert!(parent.is_stopped());
    assert!(
        !admitted.is_stopped(),
        "global stop cancelled already admitted work"
    );
    assert!(!sibling.is_stopped());
    parent.request_stop();
    timeout(Duration::from_secs(1), admitted.cancelled())
        .await
        .unwrap();
    assert!(!sibling.is_stopped());
    sibling.request_stop();
    timeout(Duration::from_secs(1), sibling.cancelled())
        .await
        .unwrap();
    let stopped_child = parent.admitted_child();
    assert!(
        stopped_child.is_stopped(),
        "admission erased a prior explicit parent cancellation"
    );
}
