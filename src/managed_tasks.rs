use crate::lifecycle::{JobGuard, Lifecycle, RegistrationError, StopToken, Stopped};
use std::{error::Error, fmt::Display, future::Future};

pub trait OperationError: Display + Send + 'static {
    fn is_cancelled(&self) -> bool;
    fn is_cleanup_failure(&self) -> bool {
        false
    }
}

#[derive(Debug, thiserror::Error)]
#[error("resource cleanup failed: {0}")]
pub struct CleanupError(pub String);

pub fn is_cleanup_failure(error: &(dyn Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.is::<CleanupError>()
            || error.is::<tokio::task::JoinError>()
            || error
                .downcast_ref::<TaskError>()
                .is_some_and(OperationError::is_cleanup_failure)
        {
            return true;
        }
        current = error.source();
    }
    false
}

impl OperationError for CleanupError {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn is_cleanup_failure(&self) -> bool {
        true
    }
}

pub fn is_cancelled(error: &(dyn Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.is::<Stopped>()
            || error
                .downcast_ref::<TaskError>()
                .is_some_and(OperationError::is_cancelled)
            || matches!(
                error.downcast_ref::<RegistrationError>(),
                Some(RegistrationError::Stopped)
            )
        {
            return true;
        }
        current = error.source();
    }
    false
}

impl OperationError for Stopped {
    fn is_cancelled(&self) -> bool {
        true
    }
}

impl OperationError for std::io::Error {
    fn is_cancelled(&self) -> bool {
        false
    }
}

impl OperationError for Box<dyn Error + Send + Sync> {
    fn is_cancelled(&self) -> bool {
        is_cancelled(self.as_ref())
    }
    fn is_cleanup_failure(&self) -> bool {
        is_cleanup_failure(self.as_ref())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("{0}")]
    Registration(#[from] RegistrationError),
    #[error("{0}")]
    Stopped(#[from] Stopped),
    #[error("{0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("managed task supervisor ended without returning its result")]
    SupervisorLost,
}

impl OperationError for TaskError {
    fn is_cancelled(&self) -> bool {
        matches!(
            self,
            Self::Stopped(_) | Self::Registration(RegistrationError::Stopped)
        )
    }
    fn is_cleanup_failure(&self) -> bool {
        matches!(self, Self::Join(_) | Self::SupervisorLost)
    }
}

pub async fn cancellable<T, E, F>(stop: &StopToken, work: F) -> Result<T, E>
where
    E: From<Stopped>,
    F: Future<Output = Result<T, E>>,
{
    stop.check()?;
    tokio::select! {
        biased;
        _ = stop.cancelled() => Err(Stopped.into()),
        result = work => result,
    }
}

pub async fn terminate_child(child: &mut tokio::process::Child) -> Result<(), CleanupError> {
    let mut failures = Vec::new();
    match child.try_wait() {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(error) => failures.push(format!("cannot observe child exit: {error}")),
    }
    let wait_again =
        match tokio::time::timeout(std::time::Duration::from_secs(2), child.kill()).await {
            Ok(Ok(())) => false,
            Ok(Err(error)) => {
                failures.push(format!("child termination failed: {error}"));
                true
            }
            Err(_) => {
                failures.push("child termination exceeded two seconds".into());
                true
            }
        };
    if wait_again {
        tracing::warn!("Waiting for child exit after termination failure");
        if let Err(error) = child.wait().await {
            failures.push(format!("child reaping failed: {error}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(CleanupError(failures.join("; ")))
    }
}

pub async fn catch_panic<F: Future>(work: F) -> Result<F::Output, CleanupError> {
    tokio::pin!(work);
    std::future::poll_fn(|context| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work.as_mut().poll(context)))
        {
            Ok(std::task::Poll::Ready(value)) => std::task::Poll::Ready(Ok(value)),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(_) => {
                std::task::Poll::Ready(Err(CleanupError("owned child operation panicked".into())))
            }
        }
    })
    .await
}

struct StopOnDrop(Option<StopToken>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Some(stop) = &self.0 {
            stop.request_stop();
        }
    }
}

pub async fn run<T, E, F, Fut>(
    lifecycle: &Lifecycle,
    parent: &StopToken,
    label: &'static str,
    work: F,
) -> Result<Result<T, E>, TaskError>
where
    T: Send + 'static,
    E: OperationError,
    F: FnOnce(StopToken) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
{
    parent.check()?;
    let job = lifecycle.register(label)?;
    let stop = parent.child();
    let worker_stop = stop.clone();
    let task = tokio::spawn(async move { work(worker_stop).await });
    wait_owned(job, stop, task, false).await
}

pub async fn blocking<T, E, F>(
    lifecycle: &Lifecycle,
    parent: &StopToken,
    label: &'static str,
    work: F,
) -> Result<Result<T, E>, TaskError>
where
    T: Send + 'static,
    E: OperationError,
    F: FnOnce(StopToken) -> Result<T, E> + Send + 'static,
{
    parent.check()?;
    let job = lifecycle.register(label)?;
    let stop = parent.child();
    let worker_stop = stop.clone();
    let task = tokio::task::spawn_blocking(move || work(worker_stop));
    wait_owned(job, stop, task, false).await
}

#[cfg(test)]
pub async fn run_cleanup<T, E, F, Fut>(
    lifecycle: &Lifecycle,
    parent: &StopToken,
    label: &'static str,
    work: F,
) -> Result<Result<T, E>, TaskError>
where
    T: Send + 'static,
    E: OperationError,
    F: FnOnce(StopToken) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
{
    parent.check()?;
    let job = lifecycle.register_cleanup(label)?;
    let stop = parent.child();
    let worker_stop = stop.clone();
    let task = tokio::spawn(async move { work(worker_stop).await });
    wait_owned(job, stop, task, true).await
}

#[cfg(test)]
pub async fn blocking_cleanup<T, E, F>(
    lifecycle: &Lifecycle,
    parent: &StopToken,
    label: &'static str,
    work: F,
) -> Result<Result<T, E>, TaskError>
where
    T: Send + 'static,
    E: OperationError,
    F: FnOnce(StopToken) -> Result<T, E> + Send + 'static,
{
    parent.check()?;
    let job = lifecycle.register_cleanup(label)?;
    let stop = parent.child();
    let worker_stop = stop.clone();
    let task = tokio::task::spawn_blocking(move || work(worker_stop));
    wait_owned(job, stop, task, true).await
}

async fn wait_owned<T: Send + 'static, E: OperationError>(
    job: JobGuard,
    stop: StopToken,
    task: tokio::task::JoinHandle<Result<T, E>>,
    cleanup: bool,
) -> Result<Result<T, E>, TaskError> {
    let mut cancel = StopOnDrop(Some(stop));
    let (reply, receiver) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let outcome = Outcome {
            value: Some(task.await),
            job: Some(job),
            cleanup,
        };
        if let Err(outcome) = reply.send(outcome) {
            drop(outcome);
        }
    });
    let outcome = receiver.await.map_err(|_| TaskError::SupervisorLost)?;
    cancel.0 = None;
    outcome.take()
}

struct Outcome<T, E: OperationError> {
    value: Option<Result<Result<T, E>, tokio::task::JoinError>>,
    job: Option<JobGuard>,
    cleanup: bool,
}

impl<T, E: OperationError> Outcome<T, E> {
    fn acknowledgement(&self, observed: bool) -> Result<(), String> {
        match &self.value {
            Some(Ok(Ok(_))) => Ok(()),
            Some(Ok(Err(error))) if self.cleanup || error.is_cleanup_failure() => {
                Err(error.to_string())
            }
            Some(Ok(Err(error))) if observed || error.is_cancelled() => Ok(()),
            Some(Ok(Err(error))) => Err(format!("unobserved operation failed: {error}")),
            Some(Err(error)) => Err(format!("task failed: {error}")),
            None => Err("task completed without a result".into()),
        }
    }
    fn take(mut self) -> Result<Result<T, E>, TaskError> {
        let acknowledgement = self.acknowledgement(true);
        let value = self.value.take().ok_or(TaskError::SupervisorLost)?;
        if let Some(job) = self.job.take() {
            job.finish(acknowledgement);
        }
        Ok(value?)
    }
}

impl<T, E: OperationError> Drop for Outcome<T, E> {
    fn drop(&mut self) {
        if self.job.is_some() {
            let acknowledgement = self.acknowledgement(false);
            drop(self.value.take());
            if let Some(job) = self.job.take() {
                job.finish(acknowledgement);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::DrainError;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;
    use tokio::time::Instant;

    struct Resource(Arc<AtomicBool>);
    impl Drop for Resource {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[test]
    fn nested_transparent_task_errors_preserve_stop_and_cleanup_classification() {
        #[derive(Debug, thiserror::Error)]
        #[error(transparent)]
        struct Nested(TaskError);
        for error in [
            TaskError::Stopped(Stopped),
            TaskError::Registration(RegistrationError::Stopped),
        ] {
            assert!(error.is_cancelled());
            assert!(is_cancelled(&Nested(error)));
        }
        let io = std::io::Error::other("independent I/O failure");
        assert!(!is_cancelled(&io));
        assert!(!is_cleanup_failure(&io));
        assert!(is_cleanup_failure(&CleanupError(
            "child wait failed".into()
        )));
    }

    #[tokio::test]
    async fn abandoned_blocking_result_keeps_drain_pending_until_resources_are_dropped() {
        let lifecycle = Lifecycle::new();
        let context = lifecycle.clone();
        let dropped = Arc::new(AtomicBool::new(false));
        let returned = Resource(Arc::clone(&dropped));
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            blocking(
                &context,
                &context.token(),
                "controlled resource",
                move |_| {
                    started.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok::<_, Stopped>(returned)
                },
            )
            .await
        });
        running.await.unwrap();
        task.abort();
        assert!(task.await.is_err());
        let before_completion = lifecycle
            .drain(Instant::now() + Duration::from_millis(20))
            .await;
        release.send(()).unwrap();
        lifecycle
            .drain(Instant::now() + Duration::from_secs(5))
            .await
            .unwrap();
        assert!(
            matches!(before_completion, Err(DrainError::Deadline { .. })),
            "drain completed while an owned worker was active"
        );
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn handled_operation_error_does_not_poison_cleanup() {
        let lifecycle = Lifecycle::new();
        let outcome = run(
            &lifecycle,
            &lifecycle.token(),
            "discovery probe",
            |_| async { Err::<(), _>(std::io::Error::from(std::io::ErrorKind::NotFound)) },
        )
        .await
        .unwrap();
        assert_eq!(outcome.unwrap_err().kind(), std::io::ErrorKind::NotFound);
        lifecycle
            .drain(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn observed_cleanup_error_is_retained_after_stop() {
        let lifecycle = Lifecycle::new();
        lifecycle.stop();
        let outcome = blocking_cleanup(
            &lifecycle,
            &lifecycle.cleanup_token(),
            "controlled flush",
            |_| Err::<(), _>(std::io::Error::other("cleanup disk failure")),
        )
        .await
        .unwrap();
        assert!(outcome.is_err());
        assert!(matches!(
            lifecycle.drain(Instant::now() + Duration::from_secs(1)).await,
            Err(DrainError::Cleanup { failures, .. }) if failures.iter().any(|error| error.contains("cleanup disk failure"))
        ));
    }

    #[tokio::test]
    async fn asynchronous_cleanup_can_finish_after_global_stop() {
        let lifecycle = Lifecycle::new();
        lifecycle.stop();
        let value = run_cleanup(
            &lifecycle,
            &lifecycle.cleanup_token(),
            "async cleanup",
            |stop| async move {
                stop.check()?;
                Ok::<_, Stopped>(42)
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(value, 42);
        lifecycle
            .drain(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn completed_unread_result_keeps_ownership_until_the_waiter_is_dropped() {
        let lifecycle = Lifecycle::new();
        let stop = lifecycle.token();
        let dropped = Arc::new(AtomicBool::new(false));
        let returned = Resource(Arc::clone(&dropped));
        let (completed, complete) = tokio::sync::oneshot::channel();
        let mut operation = Box::pin(run(
            &lifecycle,
            &stop,
            "unread result",
            move |_| async move {
                completed.send(()).unwrap();
                Ok::<_, Stopped>(returned)
            },
        ));
        std::future::poll_fn(|cx| {
            assert!(operation.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        complete.await.unwrap();
        let before_drop = lifecycle
            .drain(Instant::now() + Duration::from_millis(20))
            .await;
        drop(operation);
        lifecycle
            .drain(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        assert!(matches!(before_drop, Err(DrainError::Deadline { .. })));
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn unobserved_io_error_is_retained_after_cancellation() {
        let lifecycle = Lifecycle::new();
        let stop = lifecycle.token();
        let (completed, complete) = tokio::sync::oneshot::channel();
        let mut operation = Box::pin(run(
            &lifecycle,
            &stop,
            "unread failure",
            move |_| async move {
                completed.send(()).unwrap();
                Err::<(), _>(std::io::Error::other("controlled disk failure"))
            },
        ));
        std::future::poll_fn(|cx| {
            assert!(operation.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        complete.await.unwrap();
        drop(operation);
        assert!(matches!(
            lifecycle.drain(Instant::now() + Duration::from_secs(1)).await,
            Err(DrainError::Cleanup { failures, .. }) if failures.iter().any(|error| error.contains("controlled disk failure"))
        ));
    }
}
