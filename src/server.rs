use crate::{
    Result,
    config::{Properties, Settings},
    runtime::State,
};
use std::{
    collections::VecDeque,
    io::BufRead,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
#[cfg(test)]
use tokio::sync::watch;
use tokio::{
    net::TcpListener,
    sync::{Semaphore, mpsc},
    task::JoinSet,
    time::MissedTickBehavior,
};
use zerqon_protocol::{MINECRAFT_VERSION, PROTOCOL_VERSION};

mod jobs;
use jobs::{Kind as JobKind, OrderedJobs};
#[cfg(test)]
mod responsiveness_tests;

#[derive(Clone, Copy, Debug)]
enum ConsoleControl {
    Tps,
    Version,
    Help,
}

fn submit_console_line(
    line: String,
    commands: &mpsc::Sender<String>,
    controls: &mpsc::Sender<ConsoleControl>,
    lifecycle: &crate::lifecycle::Lifecycle,
) -> bool {
    let control = match line.trim() {
        "stop" | "/stop" => {
            lifecycle.stop();
            return false;
        }
        "tps" | "/tps" => Some(ConsoleControl::Tps),
        "version" | "/version" => Some(ConsoleControl::Version),
        "help" | "/help" => Some(ConsoleControl::Help),
        "" => return true,
        _ => None,
    };
    if let Some(control) = control {
        match controls.try_send(control) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("Console control queue is full; request rejected")
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return false,
        }
    } else {
        match commands.try_send(line) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("Console command queue is full; command rejected")
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return false,
        }
    }
    true
}

pub struct TickStats {
    samples: VecDeque<(Instant, Duration)>,
}
impl TickStats {
    pub fn bar(&self) -> (String, f32) {
        self.bar_at(Instant::now())
    }
    fn bar_at(&self, now: Instant) -> (String, f32) {
        (
            self.report_at(now),
            (self.tps_at(now).unwrap_or(0.0) / 20.0).clamp(0.0, 1.0) as f32,
        )
    }
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(1200),
        }
    }
    pub fn record(&mut self, now: Instant, busy: Duration) {
        self.samples.push_back((now, busy));
        while self.samples.len() > 1200 {
            self.samples.pop_front();
        }
    }
    pub fn report(&self) -> String {
        self.report_at(Instant::now())
    }
    fn tps_at(&self, now: Instant) -> Option<f64> {
        if self.samples.len() < 2 {
            return None;
        }
        let elapsed = now
            .saturating_duration_since(self.samples.front()?.0)
            .as_secs_f64();
        Some(((self.samples.len() - 1) as f64 / elapsed.max(0.001)).min(20.0))
    }
    fn report_at(&self, now: Instant) -> String {
        if self.samples.is_empty() {
            return "TPS warming up (no tick samples)".into();
        }
        let Some(tps) = self.tps_at(now) else {
            return "TPS warming up (1 tick sample)".into();
        };
        let mspt = self
            .samples
            .iter()
            .map(|(_, busy)| busy.as_secs_f64() * 1000.0)
            .sum::<f64>()
            / self.samples.len().max(1) as f64;
        format!(
            "TPS {tps:.2} | tick work {mspt:.3} ms | samples {}",
            self.samples.len()
        )
    }
}

pub async fn run(
    directory: PathBuf,
    properties: Properties,
    settings: Settings,
    status_only: bool,
    no_console: bool,
    lifecycle: &crate::lifecycle::Lifecycle,
) -> Result<()> {
    let stop = lifecycle.token();
    stop.check()?;
    let bind = if properties.address.is_empty() {
        "0.0.0.0"
    } else {
        &properties.address
    };
    let listener = TcpListener::bind((bind, properties.port)).await?;
    let local_address = listener.local_addr()?;
    let update_monitor = if !status_only && settings.update_check {
        Some(
            crate::updates::UpdateChecker::new(env!("CARGO_PKG_VERSION"))?.spawn(
                Duration::from_secs(settings.update_check_interval_hours * 3600),
            )?,
        )
    } else {
        None
    };
    let update_receiver = update_monitor
        .as_ref()
        .map(crate::updates::UpdateMonitor::subscribe);
    let prepared = State::new_with_lifecycle(
        &directory,
        properties,
        settings,
        status_only,
        update_receiver,
        lifecycle,
        &stop,
    )
    .await;
    let state = match prepared {
        Ok(state) => state,
        Err(error) => {
            if let Some(monitor) = update_monitor {
                monitor.stop().await;
            }
            return Err(error);
        }
    };
    let (commands_tx, commands) = mpsc::channel::<String>(32);
    let (controls_tx, controls) = mpsc::channel::<ConsoleControl>(8);
    if !no_console {
        let console_lifecycle = lifecycle.clone();
        let started = std::thread::Builder::new()
            .name("zerqon-console".into())
            .spawn(move || {
                for line in std::io::stdin().lock().lines() {
                    match line {
                        Ok(line) if line.len() <= 4096 => {
                            if !submit_console_line(
                                line,
                                &commands_tx,
                                &controls_tx,
                                &console_lifecycle,
                            ) {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            });
        if let Err(error) = started {
            lifecycle.stop();
            if let Some(monitor) = update_monitor {
                monitor.stop().await;
            }
            let cleanup = shutdown_components(&state).await;
            return match cleanup {
                Ok(()) => Err(error.into()),
                Err(cleanup) => Err(format!("{error}; {cleanup}").into()),
            };
        }
    }
    if let Err(error) = lifecycle.mark_ready() {
        if let Some(monitor) = update_monitor {
            monitor.stop().await;
        }
        let cleanup = shutdown_components(&state).await;
        return match cleanup {
            Ok(()) => Err(error.into()),
            Err(cleanup) => Err(format!("{error}; {cleanup}").into()),
        };
    }
    tracing::info!(
        "Zerqon {} | Minecraft {MINECRAFT_VERSION} | protocol {PROTOCOL_VERSION}",
        env!("CARGO_PKG_VERSION")
    );
    tracing::info!(
        "Network threads: {}; world threads: {}",
        state.settings.network_threads,
        state.settings.world_threads
    );
    if !state.properties.online_mode {
        tracing::warn!("Offline mode: player identities are not authenticated");
    }
    tracing::info!("Listening on {local_address}");
    let mut failures = match ready_jobs(lifecycle) {
        Ok(Some(jobs)) => {
            accept_loop(
                listener,
                Arc::clone(&state),
                commands,
                controls,
                !no_console,
                lifecycle,
                jobs,
            )
            .await
        }
        Ok(None) => Vec::new(),
        Err(error) => vec![error.to_string()],
    };
    lifecycle.stop();
    if let Some(monitor) = update_monitor {
        monitor.stop().await;
    }
    if let Err(error) = shutdown_components(&state).await {
        failures.push(error.to_string());
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; ").into())
    }
}

fn ready_jobs(lifecycle: &crate::lifecycle::Lifecycle) -> Result<Option<OrderedJobs>> {
    match OrderedJobs::new(lifecycle) {
        Ok(jobs) => Ok(Some(jobs)),
        Err(error)
            if lifecycle.token().is_stopped()
                && matches!(
                    error.downcast_ref::<crate::lifecycle::RegistrationError>(),
                    Some(crate::lifecycle::RegistrationError::Stopped)
                ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

async fn run_tick(state: Arc<State>, tick: u64) -> Result<()> {
    if *state.shutdown.borrow() {
        return Err(crate::lifecycle::Stopped.into());
    }
    let started = Instant::now();
    let mut failures = Vec::new();
    if let Err(error) = state.players.tracking_tick() {
        failures.push(format!("player tracking: {error}"));
    }
    if !*state.shutdown.borrow()
        && let Some(mobs) = &state.mobs
        && let Err(error) = mobs.tick(tick).await
    {
        if error
            .downcast_ref::<crate::mob_store::StoreError>()
            .is_some()
        {
            tracing::error!(%error,"Mob tick failed");
        } else {
            failures.push(format!("mob tick: {error}"));
        }
    }
    if !*state.shutdown.borrow()
        && let Err(error) = state.event(zerqon_plugin_api::Event::Tick { tick }).await
    {
        failures.push(format!("plugin tick: {error}"));
    }
    if let Ok(mut stats) = state.stats.lock() {
        stats.record(started, started.elapsed());
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; ").into())
    }
}

async fn run_console(state: Arc<State>, line: String) -> Result<()> {
    let line = line.trim();
    if let Some((name, add)) = line
        .strip_prefix("op ")
        .map(|name| (name, true))
        .or_else(|| line.strip_prefix("deop ").map(|name| (name, false)))
    {
        let state = Arc::clone(&state);
        let name = name.trim().to_owned();
        tokio::task::spawn_blocking(move || match state.update_operator(&name, add) {
            Ok(changed) => {
                tracing::info!(operator=%name,granted=add,changed,"Operator list updated")
            }
            Err(error) => tracing::warn!(%error,"Operator update failed"),
        })
        .await?;
    } else {
        match state
            .command(crate::runtime::CommandSource::Console, line)
            .await
        {
            Ok(replies) => {
                for reply in replies {
                    tracing::info!("{reply}");
                }
            }
            Err(error) => tracing::warn!(%error,"Command failed"),
        }
    }
    Ok(())
}

async fn accept_loop(
    listener: TcpListener,
    state: Arc<State>,
    mut commands: mpsc::Receiver<String>,
    mut controls: mpsc::Receiver<ConsoleControl>,
    console_enabled: bool,
    lifecycle: &crate::lifecycle::Lifecycle,
    mut jobs: OrderedJobs,
) -> Vec<String> {
    let mut shutdown = lifecycle.subscribe();
    let permits = Arc::new(Semaphore::new(state.settings.max_connections));
    let mut clients = JoinSet::new();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut commands_open = console_enabled;
    let mut controls_open = console_enabled;
    let mut tick_number = 0u64;
    let mut update_notifications = crate::updates::NotificationState::default();
    let mut failures = Vec::new();
    let mut omitted_failures = 0_u64;
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            _=shutdown.changed()=>break,
            _=tick.tick()=>{
                if let Some(updates) = &state.updates {
                    let update = updates.borrow();
                    if let Some(message) = update_notifications.next(true, update.as_ref()) {
                        tracing::info!("{message}");
                    }
                }
                if jobs.available(JobKind::Tick) {
                    tick_number=tick_number.wrapping_add(1);
                    if let Err(error)=jobs.submit(JobKind::Tick,run_tick(Arc::clone(&state),tick_number)) {
                        if !*shutdown.borrow() { record_failure(&mut failures,&mut omitted_failures,error.to_string()); }
                        break;
                    }
                }
            },
            control=controls.recv(),if controls_open=>{
                match control {
                    Some(ConsoleControl::Tps)=>{if let Ok(stats)=state.stats.lock(){tracing::info!("{}",stats.report());}},
                    Some(ConsoleControl::Version)=>tracing::info!("Zerqon {} / Minecraft {MINECRAFT_VERSION}",env!("CARGO_PKG_VERSION")),
                    Some(ConsoleControl::Help)=>tracing::info!("Commands: help, version, tps, plugins, op NAME, deop NAME, setblock X Y Z STATE, stop"),
                    None=>controls_open=false,
                }
            },
            line=commands.recv(),if commands_open&&jobs.available(JobKind::Console)=>{
                match line {
                    Some(line)=>if let Err(error)=jobs.submit(JobKind::Console,run_console(Arc::clone(&state),line)) {
                        if !*shutdown.borrow() { record_failure(&mut failures,&mut omitted_failures,error.to_string()); }
                        break;
                    },
                    None=>commands_open=false,
                }
            },
            result=jobs.next(),if jobs.has_pending()=>{
                match result {
                    Some(completion)=>{
                        if completion.cancelled { tracing::debug!(?completion.kind,"Server work cancelled before completion"); }
                        if let Err(error)=completion.result { tracing::error!(?completion.kind,%error,"Server work failed");record_failure(&mut failures,&mut omitted_failures,error);lifecycle.stop(); }
                    },
                    None=>{record_failure(&mut failures,&mut omitted_failures,"Server worker stopped unexpectedly".into());break;},
                }
            },
            accepted=listener.accept()=>{
                let (socket,peer)=match accepted { Ok(client)=>client, Err(error)=>{record_failure(&mut failures,&mut omitted_failures,error.to_string());break;} };
                let Ok(permit)=Arc::clone(&permits).try_acquire_owned() else {drop(socket);continue;};
                let state=Arc::clone(&state);
                clients.spawn(async move {
                    let _permit=permit;
                    if let Err(error)=crate::connection::handle(socket,Arc::clone(&state)).await{tracing::debug!(%peer,%error,"Connection closed");}
                });
            },
            Some(result)=clients.join_next(),if !clients.is_empty()=>{if let Err(error)=result{tracing::error!(%error,"Connection task failed");record_failure(&mut failures,&mut omitted_failures,error.to_string());}},
        }
    }
    lifecycle.stop();
    commands.close();
    let mut cancelled_commands = 0;
    while commands.try_recv().is_ok() {
        cancelled_commands += 1;
    }
    if cancelled_commands > 0 {
        tracing::warn!(
            count = cancelled_commands,
            "Queued console commands cancelled by shutdown"
        );
    }
    let clients_drain = async {
        let mut errors = Vec::new();
        let mut omitted = 0;
        while let Some(result) = clients.join_next().await {
            if let Err(error) = result {
                record_failure(&mut errors, &mut omitted, error.to_string());
            }
        }
        if omitted > 0 {
            errors.push(format!("{omitted} additional connection shutdown failures"));
        }
        errors
    };
    let (job_failures, client_failures) = tokio::join!(jobs.drain(), clients_drain);
    for error in job_failures.into_iter().chain(client_failures) {
        record_failure(&mut failures, &mut omitted_failures, error);
    }
    if omitted_failures > 0 {
        failures.push(format!(
            "{omitted_failures} additional server work failures"
        ));
    }
    failures
}

fn record_failure(failures: &mut Vec<String>, omitted: &mut u64, error: String) {
    if failures.len() < 32 {
        let mut error = error;
        let mut end = error.len().min(1024);
        while !error.is_char_boundary(end) {
            end -= 1;
        }
        error.truncate(end);
        failures.push(error);
    } else {
        *omitted = omitted.saturating_add(1);
    }
}

async fn shutdown_components(state: &State) -> Result<()> {
    state.sessions.drain().await;
    let mut failures = Vec::new();
    if let Some(mobs) = &state.mobs
        && let Err(error) = mobs.flush().await
    {
        tracing::error!(%error,"Mob snapshot shutdown save failed");
        failures.push(format!("mob snapshot: {error}"));
    }
    match state.profiler.shutdown().await {
        Ok(messages) => {
            for message in messages {
                tracing::info!("{message}");
            }
        }
        Err(error) => {
            tracing::error!(%error,"Profiler shutdown failed");
            failures.push(format!("profiler: {error}"));
        }
    }
    if let Err(error) = state.event(zerqon_plugin_api::Event::Disable).await {
        tracing::error!(%error,"Plugin shutdown failed");
        failures.push(format!("plugins: {error}"));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("shutdown failed: {}", failures.join("; ")).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tick_report_warms_up_before_any_samples() {
        assert_eq!(
            TickStats::new().report(),
            "TPS warming up (no tick samples)"
        );
    }
    #[test]
    fn tpsbar_reports_half_progress_for_ten_ticks_per_second() {
        let start = Instant::now();
        let mut stats = TickStats::new();
        for index in 0..=10 {
            stats.record(
                start + Duration::from_millis(index * 100),
                Duration::from_millis(5),
            );
        }
        let (_, progress) = stats.bar_at(start + Duration::from_secs(1));
        assert_eq!(progress, 0.5);
    }
    #[test]
    fn tpsbar_warmup_has_zero_progress_and_healthy_rate_is_bounded() {
        let start = Instant::now();
        let mut stats = TickStats::new();
        assert_eq!(stats.bar_at(start).1, 0.0);
        stats.record(start, Duration::from_millis(1));
        assert_eq!(stats.bar_at(start).1, 0.0);
        for index in 1..=20 {
            stats.record(
                start + Duration::from_millis(index * 50),
                Duration::from_millis(1),
            );
        }
        assert_eq!(stats.bar_at(start + Duration::from_secs(1)).1, 1.0);
        assert_eq!(stats.bar_at(start + Duration::from_secs(2)).1, 0.5);
    }
    #[test]
    fn tpsbar_caps_burst_tick_rates_at_full() {
        let start = Instant::now();
        let mut stats = TickStats::new();
        for index in 0..=20 {
            stats.record(
                start + Duration::from_millis(index * 25),
                Duration::from_millis(1),
            );
        }
        assert_eq!(stats.bar_at(start + Duration::from_millis(500)).1, 1.0);
    }

    #[tokio::test]
    #[ignore = "requires prepared official Minecraft 26.3 data"]
    async fn mob_save_failure_does_not_skip_profiler_shutdown() {
        use crate::{config, players::Identity, profiling::ProfilerServiceError};
        let directory = tempfile::tempdir().unwrap();
        config::initialize(directory.path()).unwrap();
        let settings = Settings {
            data_directory: std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(".cache/vanilla/26.3")
                .to_string_lossy()
                .into_owned(),
            java_plugins: false,
            update_check: false,
            spawn_mobs: false,
            ..Settings::default()
        };
        let (shutdown, _) = watch::channel(false);
        let state = State::new(
            directory.path(),
            Properties::parse("online-mode=false\nview-distance=1\n").unwrap(),
            settings,
            false,
            shutdown,
            None,
        )
        .await
        .unwrap();
        let (outbound, _receiver) = mpsc::channel(32);
        let (disconnect, _closed) = watch::channel(false);
        let player = state
            .players
            .reserve(
                Identity {
                    uuid: uuid::Uuid::from_u128(1),
                    name: "Tester".into(),
                    properties: vec![],
                },
                outbound,
                disconnect,
            )
            .unwrap();
        player.activate().unwrap();
        let mobs = state.mobs.as_ref().unwrap();
        mobs.summon(zerqon_entities::MobKind::Pig, [2.5, 64.0, 0.5])
            .await
            .unwrap();
        mobs.tick(1).await.unwrap();
        std::fs::write(
            directory.path().join("world/entities.zqe"),
            b"corrupt shutdown snapshot",
        )
        .unwrap();
        assert!(
            run_tick(Arc::clone(&state), 100).await.is_ok(),
            "mob snapshot save failure must remain retryable"
        );
        assert!(
            !*state.shutdown.borrow(),
            "retryable mob save failure stopped the server"
        );
        assert!(shutdown_components(&state).await.is_err());
        assert!(matches!(
            state
                .profiler
                .execute(&["profiler".into(), "start".into()], true)
                .await,
            Err(ProfilerServiceError::ShuttingDown)
        ));
    }
}
