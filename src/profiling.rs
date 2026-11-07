use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, watch};
use zerqon_profiler::{ProfileConfig, ProfileSession, ProfileSummary};

#[derive(Debug, thiserror::Error)]
pub enum ProfilerServiceError {
    #[error("Permission denied")]
    PermissionDenied,
    #[error("Usage: /spark profiler start [--timeout 1..60] | stop | status")]
    Usage,
    #[error("invalid profiler configuration: {0}")]
    Configuration(String),
    #[error("native profiler is already active")]
    AlreadyActive,
    #[error("native profiler is not running")]
    NotRunning,
    #[error("server is shutting down")]
    ShuttingDown,
    #[error("native profiler failed: {0}")]
    Backend(String),
}

pub struct ProfilerService {
    directory: PathBuf,
    config: ProfileConfig,
    state: Mutex<ServiceState>,
}

#[derive(Default)]
struct ServiceState {
    job: Option<Job>,
    shutting_down: bool,
}

#[derive(Clone)]
struct Job {
    stop: mpsc::SyncSender<()>,
    status: watch::Receiver<Status>,
}

#[derive(Clone)]
enum Status {
    Starting,
    Running { started: Instant, timeout: Duration },
    Saving,
    Complete(SavedProfile),
    Failed(String),
}

#[derive(Clone)]
struct SavedProfile {
    path: PathBuf,
    summary: ProfileSummary,
}

enum Command {
    Help,
    Status,
    Start(Option<Duration>),
    Stop,
}

impl ProfilerService {
    pub fn new(directory: PathBuf, config: ProfileConfig) -> Result<Self, ProfilerServiceError> {
        config
            .validate()
            .map_err(|error| ProfilerServiceError::Configuration(error.to_string()))?;
        if !(Duration::from_secs(1)..=Duration::from_secs(60)).contains(&config.duration) {
            return Err(ProfilerServiceError::Configuration(
                "duration must be 1..60 seconds".into(),
            ));
        }
        Ok(Self {
            directory: directory.join("profiles"),
            config,
            state: Mutex::new(ServiceState::default()),
        })
    }

    pub async fn execute(
        &self,
        args: &[String],
        operator: bool,
    ) -> Result<Vec<String>, ProfilerServiceError> {
        let command = parse_command(args)?;
        if matches!(command, Command::Start(_) | Command::Stop) && !operator {
            return Err(ProfilerServiceError::PermissionDenied);
        }
        match command {
            Command::Help => Ok(vec!["Native CPU profiler (Linux): /spark profiler start [--timeout 1..60] | stop | status. Profiles are saved locally; no uploads.".into()]),
            Command::Status => self.status(operator).await,
            Command::Start(duration) => self.start(duration).await,
            Command::Stop => {
                let job = self.state.lock().await.job.clone().filter(Job::active).ok_or(ProfilerServiceError::NotRunning)?;
                job.request_stop();
                job.wait_complete().await.map(|saved| saved.message(true))
            }
        }
    }

    async fn start(&self, duration: Option<Duration>) -> Result<Vec<String>, ProfilerServiceError> {
        let job = {
            let mut state = self.state.lock().await;
            if state.shutting_down {
                return Err(ProfilerServiceError::ShuttingDown);
            }
            if state.job.as_ref().is_some_and(Job::active) {
                return Err(ProfilerServiceError::AlreadyActive);
            }
            let mut config = self.config.clone();
            if let Some(duration) = duration {
                config.duration = duration;
            }
            let directory = self.directory.clone();
            let (stop_tx, stop_rx) = mpsc::sync_channel(1);
            let (status_tx, status_rx) = watch::channel(Status::Starting);
            let job = Job {
                stop: stop_tx,
                status: status_rx,
            };
            state.job = Some(job.clone());
            tokio::task::spawn_blocking(move || {
                let result = capture(directory, config, stop_rx, &status_tx);
                status_tx.send_replace(match result {
                    Ok(saved) => Status::Complete(saved),
                    Err(error) => Status::Failed(error.to_string()),
                });
            });
            job
        };
        job.wait_started().await
    }

    async fn status(&self, operator: bool) -> Result<Vec<String>, ProfilerServiceError> {
        let job = self.state.lock().await.job.clone();
        let Some(job) = job else {
            return Ok(vec!["Native profiler is idle.".into()]);
        };
        let status = job.status.borrow().clone();
        if job.status.has_changed().is_err()
            && !matches!(status, Status::Complete(_) | Status::Failed(_))
        {
            return Ok(vec![
                "Native profiler is idle; its worker stopped unexpectedly.".into(),
            ]);
        }
        Ok(match status {
            Status::Starting => vec!["Native profiler is starting.".into()],
            Status::Running { started, timeout } => vec![format!(
                "Native CPU profiler is running: {:.1}s elapsed; timeout {}s.",
                started.elapsed().as_secs_f64(),
                timeout.as_secs()
            )],
            Status::Saving => vec!["Native profiler is saving the captured stacks.".into()],
            Status::Complete(saved) => saved.message(operator),
            Status::Failed(reason) if operator => vec![format!(
                "Native profiler is idle; last capture failed: {reason}"
            )],
            Status::Failed(_) => vec!["Native profiler is idle; last capture failed.".into()],
        })
    }

    pub async fn shutdown(&self) -> Result<Vec<String>, ProfilerServiceError> {
        let job = {
            let mut state = self.state.lock().await;
            state.shutting_down = true;
            state.job.clone().filter(Job::active)
        };
        if let Some(job) = job {
            job.request_stop();
            job.wait_complete().await.map(|saved| saved.message(true))
        } else {
            Ok(Vec::new())
        }
    }
}

fn parse_command(args: &[String]) -> Result<Command, ProfilerServiceError> {
    match args {
        [] => Ok(Command::Help),
        [help] if help == "help" => Ok(Command::Help),
        [profiler] if profiler == "profiler" => Ok(Command::Help),
        [profiler, action] if profiler == "profiler" => match action.as_str() {
            "help" => Ok(Command::Help),
            "status" => Ok(Command::Status),
            "start" => Ok(Command::Start(None)),
            "stop" => Ok(Command::Stop),
            _ => Err(ProfilerServiceError::Usage),
        },
        [profiler, start, flag, seconds]
            if profiler == "profiler" && start == "start" && flag == "--timeout" =>
        {
            let seconds = seconds
                .parse::<u64>()
                .map_err(|_| ProfilerServiceError::Usage)?;
            if !(1..=60).contains(&seconds) {
                return Err(ProfilerServiceError::Usage);
            }
            Ok(Command::Start(Some(Duration::from_secs(seconds))))
        }
        _ => Err(ProfilerServiceError::Usage),
    }
}

impl Job {
    fn active(&self) -> bool {
        !matches!(
            *self.status.borrow(),
            Status::Complete(_) | Status::Failed(_)
        ) && self.status.has_changed().is_ok()
    }

    fn request_stop(&self) {
        match self.stop.try_send(()) {
            Ok(())
            | Err(mpsc::TrySendError::Full(()))
            | Err(mpsc::TrySendError::Disconnected(())) => {}
        }
    }

    async fn wait_started(mut self) -> Result<Vec<String>, ProfilerServiceError> {
        loop {
            let status = self.status.borrow().clone();
            match status {
                Status::Running { timeout, .. } => {
                    return Ok(vec![format!(
                        "Native CPU profiler started for up to {}s. Use /spark profiler status or stop.",
                        timeout.as_secs()
                    )]);
                }
                Status::Complete(saved) => return Ok(saved.message(true)),
                Status::Failed(reason) => return Err(ProfilerServiceError::Backend(reason)),
                Status::Starting | Status::Saving => {}
            }
            self.status
                .changed()
                .await
                .map_err(|_| ProfilerServiceError::Backend("worker stopped unexpectedly".into()))?;
        }
    }

    async fn wait_complete(mut self) -> Result<SavedProfile, ProfilerServiceError> {
        loop {
            let status = self.status.borrow().clone();
            match status {
                Status::Complete(saved) => return Ok(saved),
                Status::Failed(reason) => return Err(ProfilerServiceError::Backend(reason)),
                Status::Starting | Status::Running { .. } | Status::Saving => {}
            }
            self.status
                .changed()
                .await
                .map_err(|_| ProfilerServiceError::Backend("worker stopped unexpectedly".into()))?;
        }
    }
}

impl SavedProfile {
    fn message(&self, operator: bool) -> Vec<String> {
        let mut lines = vec![format!(
            "Native CPU profile completed: {} samples, {} threads, {:.1} ms sampled CPU{}.",
            self.summary.samples,
            self.summary.threads,
            self.summary.estimated_cpu_ms,
            if self.summary.truncated {
                " (truncated)"
            } else {
                ""
            }
        )];
        if operator {
            lines.push(format!("Saved locally: {}", self.path.display()));
        }
        if self.summary.lost_samples != 0 {
            lines.push(format!(
                "Kernel reported {} lost samples.",
                self.summary.lost_samples
            ));
        }
        lines
    }
}

fn capture(
    directory: PathBuf,
    config: ProfileConfig,
    stop: mpsc::Receiver<()>,
    status: &watch::Sender<Status>,
) -> Result<SavedProfile, zerqon_profiler::ProfileError> {
    let session = ProfileSession::start(config.clone())?;
    let started = Instant::now();
    status.send_replace(Status::Running {
        started,
        timeout: config.duration,
    });
    while !session.is_finished() {
        let remaining = config.duration.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        match stop.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    status.send_replace(Status::Saving);
    let profile = session.finish()?;
    std::fs::create_dir_all(&directory)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let path = directory.join(format!(
        "spark-{timestamp}-{}.sparkprofile",
        uuid::Uuid::new_v4()
    ));
    profile.write_spark(&path)?;
    Ok(SavedProfile {
        path,
        summary: profile.summary().clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn constructor_rejects_limits_outside_the_service_contract() {
        let directory = tempfile::tempdir().unwrap();
        for config in [
            ProfileConfig {
                duration: Duration::ZERO,
                ..ProfileConfig::default()
            },
            ProfileConfig {
                duration: Duration::from_secs(61),
                ..ProfileConfig::default()
            },
            ProfileConfig {
                frequency_hz: 1001,
                ..ProfileConfig::default()
            },
        ] {
            assert!(ProfilerService::new(directory.path().into(), config).is_err());
        }
        assert!(ProfilerService::new(directory.path().into(), ProfileConfig::default()).is_ok());
    }

    #[tokio::test]
    async fn start_and_stop_require_privilege_before_creating_files() {
        let directory = tempfile::tempdir().unwrap();
        let service =
            ProfilerService::new(directory.path().into(), ProfileConfig::default()).unwrap();
        for command in [args(&["profiler", "start"]), args(&["profiler", "stop"])] {
            assert!(matches!(
                service.execute(&command, false).await,
                Err(ProfilerServiceError::PermissionDenied)
            ));
        }
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn rejects_unbounded_or_ambiguous_start_flags() {
        let directory = tempfile::tempdir().unwrap();
        let service =
            ProfilerService::new(directory.path().into(), ProfileConfig::default()).unwrap();
        for command in [
            args(&["profiler", "start", "--timeout", "0"]),
            args(&["profiler", "start", "--timeout", "61"]),
            args(&["profiler", "start", "--timeout", "18446744073709551616"]),
            args(&["profiler", "start", "--timeout"]),
            args(&["profiler", "start", "--timeout", "2", "extra"]),
            args(&["profiler", "start", "--frequency", "10000"]),
            args(&["profiler", "stop", "extra"]),
        ] {
            assert!(
                matches!(
                    service.execute(&command, true).await,
                    Err(ProfilerServiceError::Usage)
                ),
                "{command:?}"
            );
        }
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn public_help_status_and_shutdown_do_not_expose_paths_or_start_capture() {
        let directory = tempfile::tempdir().unwrap();
        let service =
            ProfilerService::new(directory.path().into(), ProfileConfig::default()).unwrap();
        for command in [
            args(&[]),
            args(&["help"]),
            args(&["profiler", "help"]),
            args(&["profiler", "status"]),
        ] {
            let result = service.execute(&command, false).await.unwrap();
            assert!(!result.is_empty());
            assert!(
                !result
                    .join("\n")
                    .contains(directory.path().to_str().unwrap())
            );
        }
        service.shutdown().await.unwrap();
        assert!(matches!(
            service.execute(&args(&["profiler", "start"]), true).await,
            Err(ProfilerServiceError::ShuttingDown)
        ));
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn unsupported_platform_returns_real_backend_error_and_allows_retry() {
        let directory = tempfile::tempdir().unwrap();
        let service =
            ProfilerService::new(directory.path().into(), ProfileConfig::default()).unwrap();
        for _ in 0..2 {
            let failure = service
                .execute(&args(&["profiler", "start", "--timeout", "1"]), true)
                .await;
            assert!(
                matches!(failure, Err(ProfilerServiceError::Backend(ref reason)) if reason.contains("only on Linux")),
                "{failure:?}"
            );
        }
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let status = service
            .execute(&args(&["profiler", "status"]), false)
            .await
            .unwrap()
            .join(" ");
        assert!(!status.contains(directory.path().to_str().unwrap()));
    }

    #[cfg(target_os = "linux")]
    #[inline(never)]
    fn profiling_service_cpu_work(mut value: u64) -> u64 {
        for _ in 0..20_000 {
            value = std::hint::black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
        }
        value
    }

    #[cfg(target_os = "linux")]
    struct CpuWork {
        running: std::sync::Arc<std::sync::atomic::AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    #[cfg(target_os = "linux")]
    impl CpuWork {
        fn start() -> Self {
            use std::sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            };
            let running = Arc::new(AtomicBool::new(true));
            let active = Arc::clone(&running);
            let (ready_tx, ready_rx) = mpsc::channel();
            let worker = std::thread::Builder::new()
                .name("zerqon-profile-test".into())
                .spawn(move || {
                    ready_tx.send(()).unwrap();
                    let mut value = 7;
                    while active.load(Ordering::Relaxed) {
                        value = profiling_service_cpu_work(value);
                    }
                    std::hint::black_box(value);
                })
                .unwrap();
            ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            Self {
                running,
                worker: Some(worker),
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for CpuWork {
        fn drop(&mut self) {
            self.running
                .store(false, std::sync::atomic::Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires Linux perf_event_open access; captures real CPU samples"]
    async fn real_cpu_capture_survives_cancelled_request_autosaves_stops_and_shuts_down() {
        use std::future::Future;
        let _workload = CpuWork::start();
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("profiles")).unwrap();
        let existing = directory.path().join("profiles/existing.sparkprofile");
        std::fs::write(&existing, b"preserve existing profile").unwrap();
        let service = ProfilerService::new(
            directory.path().into(),
            ProfileConfig {
                duration: Duration::from_secs(1),
                ..ProfileConfig::default()
            },
        )
        .unwrap();
        let start = args(&["profiler", "start"]);
        let mut request = Box::pin(service.execute(&start, true));
        let _ = std::future::poll_fn(|cx| std::task::Poll::Ready(request.as_mut().poll(cx))).await;
        drop(request);
        assert!(matches!(
            service.execute(&start, true).await,
            Err(ProfilerServiceError::AlreadyActive)
        ));
        let job = service.state.lock().await.job.clone().unwrap();
        let saved = tokio::time::timeout(Duration::from_secs(10), job.wait_complete())
            .await
            .unwrap()
            .unwrap();
        assert!(saved.summary.samples > 0 && saved.summary.estimated_cpu_ms > 0.0);
        assert!(saved.path.is_file());
        assert!(saved.summary.duration_ms <= 2500);
        let public = service
            .execute(&args(&["profiler", "status"]), false)
            .await
            .unwrap()
            .join(" ");
        assert!(!public.contains(directory.path().to_str().unwrap()));

        service.execute(&start, true).await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        let stop = tokio::time::timeout(
            Duration::from_secs(10),
            service.execute(&args(&["profiler", "stop"]), true),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(stop.iter().any(|line| line.contains("Saved locally:")));
        service.execute(&start, true).await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            !tokio::time::timeout(Duration::from_secs(10), service.shutdown())
                .await
                .unwrap()
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            service.execute(&start, true).await,
            Err(ProfilerServiceError::ShuttingDown)
        ));
        assert_eq!(
            std::fs::read(&existing).unwrap(),
            b"preserve existing profile"
        );
        assert_eq!(
            std::fs::read_dir(directory.path().join("profiles"))
                .unwrap()
                .count(),
            4
        );
    }
}
