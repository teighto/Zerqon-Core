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
use tokio::{
    net::TcpListener,
    sync::{Semaphore, mpsc, watch},
    task::JoinSet,
    time::MissedTickBehavior,
};
use zerqon_protocol::{MINECRAFT_VERSION, PROTOCOL_VERSION};

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
) -> Result<()> {
    let bind = if properties.address.is_empty() {
        "0.0.0.0"
    } else {
        &properties.address
    };
    let listener = TcpListener::bind((bind, properties.port)).await?;
    let (shutdown_tx, mut shutdown) = watch::channel(false);
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
    let state = State::new(
        &directory,
        properties,
        settings,
        status_only,
        shutdown_tx.clone(),
        update_receiver,
    )
    .await?;
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
    tracing::info!("Listening on {}", listener.local_addr()?);
    let (commands_tx, mut commands) = mpsc::channel::<String>(32);
    if !no_console {
        std::thread::Builder::new()
            .name("zerqon-console".into())
            .spawn(move || {
                for line in std::io::stdin().lock().lines() {
                    match line {
                        Ok(line) if line.len() <= 4096 => {
                            if commands_tx.blocking_send(line).is_err() {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            })?;
    }
    let signal = shutdown_tx.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = signal.send(true);
        }
    });
    let marker = std::env::var_os("ZERQON_SHUTDOWN_FILE").map(PathBuf::from);
    let permits = Arc::new(Semaphore::new(state.settings.max_connections));
    let mut clients = JoinSet::new();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut commands_open = !no_console;
    let mut tick_number = 0u64;
    let mut update_notifications = crate::updates::NotificationState::default();
    loop {
        tokio::select! {
            _=shutdown.changed()=>break,
            _=tick.tick()=>{
                let started=Instant::now();
                if let Some(updates) = &state.updates {
                    let update = updates.borrow();
                    if let Some(message) = update_notifications.next(true, update.as_ref()) {
                        tracing::info!("{message}");
                    }
                }
                if marker.as_ref().is_some_and(|path|path.exists()){break;}
                tick_number=tick_number.wrapping_add(1);
                if let Some(mobs)=&state.mobs&&let Err(error)=mobs.tick(tick_number).await{tracing::error!(%error,"Mob tick failed");}
                if let Err(error)=state.event(zerqon_plugin_api::Event::Tick{tick:tick_number}).await{tracing::error!(%error,"Plugin tick failed");}
                if let Ok(mut stats)=state.stats.lock(){stats.record(started,started.elapsed());}
            },
            line=commands.recv(),if commands_open=>{
                match line.as_deref().map(str::trim) {
                    Some("stop"|"/stop")=>break,
                    Some("tps"|"/tps")=>{if let Ok(stats)=state.stats.lock(){tracing::info!("{}",stats.report());}},
                    Some("version"|"/version")=>tracing::info!("Zerqon {} / Minecraft {MINECRAFT_VERSION}",env!("CARGO_PKG_VERSION")),
                    Some("help"|"/help")=>tracing::info!("Commands: help, version, tps, plugins, op NAME, deop NAME, setblock X Y Z STATE, stop"),
                    Some("")=>{},
                    Some(line)=>{
                        if let Some((name,add))=line.strip_prefix("op ").map(|name|(name,true)).or_else(||line.strip_prefix("deop ").map(|name|(name,false))){
                            match state.update_operator(name.trim(),add){
                                Ok(changed)=>tracing::info!(operator=name.trim(),granted=add,changed,"Operator list updated"),
                                Err(error)=>tracing::warn!(%error,"Operator update failed"),
                            }
                        }
                        else {match state.command(crate::runtime::CommandSource::Console,line).await{Ok(replies)=>for reply in replies{tracing::info!("{reply}");},Err(error)=>tracing::warn!(%error,"Command failed")}}
                    },
                    None=>commands_open=false,
                }
            },
            accepted=listener.accept()=>{
                let (socket,peer)=accepted?;
                let Ok(permit)=Arc::clone(&permits).try_acquire_owned() else {drop(socket);continue;};
                let state=Arc::clone(&state);
                clients.spawn(async move {
                    let _permit=permit;
                    if let Err(error)=crate::connection::handle(socket,Arc::clone(&state)).await{tracing::debug!(%peer,%error,"Connection closed");}
                });
            },
            Some(result)=clients.join_next(),if !clients.is_empty()=>{if let Err(error)=result{tracing::error!(%error,"Connection task failed");}},
        }
    }
    let _ = shutdown_tx.send(true);
    while let Some(result) = clients.join_next().await {
        if let Err(error) = result {
            tracing::error!(%error,"Connection shutdown failed");
        }
    }
    signal_task.abort();
    if let Some(monitor) = update_monitor {
        monitor.stop().await;
    }
    shutdown_components(&state).await?;
    tracing::info!("Stopped cleanly; directory {}", directory.display());
    Ok(())
}

async fn shutdown_components(state: &State) -> Result<()> {
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
