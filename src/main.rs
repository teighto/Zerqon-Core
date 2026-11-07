mod auth;
mod bootstrap;
mod client_policy;
mod config;
mod connection;
mod entity_packets;
mod game_data;
mod java_launch;
mod java_plugins;
mod java_runtime;
mod lifecycle;
mod managed_tasks;
mod mob_store;
mod mobs;
mod native_query_runtime;
mod operator_store;
mod packets;
mod paper_libraries;
mod player_inventory;
mod player_session;
mod player_store;
mod players;
mod plugin_services;
mod profiling;
mod runtime;
mod rust_plugins;
mod server;
mod server_queries;
mod shutdown_signals;
mod updates;
mod world_view;

use clap::Parser;
use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Mutex,
};
use tracing_subscriber::fmt::writer::MakeWriterExt;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Parser, Debug)]
#[command(
    name = "zerqon",
    version,
    about = "Zerqon — Minecraft 26.3 server by teighto"
)]
struct Arguments {
    #[arg(long, default_value = ".")]
    directory: PathBuf,
    #[arg(long)]
    init: bool,
    #[arg(long)]
    check_config: bool,
    #[arg(long)]
    status_only: bool,
    #[arg(long)]
    no_console: bool,
    #[arg(long)]
    no_color: bool,
    #[arg(long)]
    nogui: bool,
    #[arg(long)]
    data: Option<PathBuf>,
    #[arg(long, value_name = "SOURCE")]
    import_blocks: Option<PathBuf>,
}

fn main() -> ExitCode {
    match start(Arguments::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Zerqon: {error}");
            ExitCode::FAILURE
        }
    }
}

fn start(args: Arguments) -> Result<()> {
    config::initialize(&args.directory)?;
    if args.init {
        println!(
            "Created Zerqon configuration in {}",
            args.directory.display()
        );
        return Ok(());
    }
    let properties = config::Properties::load(&args.directory.join("server.properties"))?;
    let mut settings = config::Settings::load(&args.directory.join("server.settings"))?;
    if let Some(data) = &args.data {
        settings.data_directory = data.to_string_lossy().into_owned();
    }
    settings.validate_with_properties(&properties)?;
    if args.check_config {
        println!("Configuration is valid");
        return Ok(());
    }
    let accepted = config::accepted_eula(&args.directory.join("eula.txt"))?;
    if !accepted {
        return Err(
            "Read https://aka.ms/MinecraftEULA and set eula=true in eula.txt before starting"
                .into(),
        );
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(args.directory.join("logs/latest.log"))?;
    tracing_subscriber::fmt()
        .with_ansi(!args.no_color)
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stdout.and(Mutex::new(log)))
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(settings.network_threads)
        .max_blocking_threads(settings.world_threads + 4)
        .thread_name("zerqon-net")
        .build()?;
    runtime.block_on(async move {
        let lifecycle = lifecycle::Lifecycle::new();
        let marker = std::env::var_os("ZERQON_SHUTDOWN_FILE").map(PathBuf::from);
        let signals = shutdown_signals::ShutdownSignals::install(lifecycle.clone(), marker)?;
        let directory = args.directory.clone();
        let result = run_program(args, properties, settings, &lifecycle).await;
        let interrupted = lifecycle.token().is_stopped();
        lifecycle.stop();
        let mut deadline = lifecycle.cleanup_deadline().unwrap_or_else(tokio::time::Instant::now);
        let mut late = false;
        let cleanup = loop {
            match lifecycle.drain(deadline).await {
                Err(error @ lifecycle::DrainError::Deadline { .. }) => {
                    tracing::error!(%error, "Cleanup is still running; waiting for owned resources");
                    late = true;
                    deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                }
                result => break result,
            }
        };
        signals.close().await;
        let mut failures = Vec::new();
        let cancelled = result.as_ref().err().is_some_and(|error| {
            interrupted && managed_tasks::is_cancelled(error.as_ref())
        });
        if let Err(error) = result && !cancelled { failures.push(error.to_string()); }
        if let Err(error) = cleanup { failures.push(error.to_string()); }
        if late { failures.push("cleanup exceeded its original deadline".into()); }
        if !failures.is_empty() { return Err(failures.join("; ").into()); }
        if lifecycle.is_ready() {
            tracing::info!("Stopped cleanly; directory {}", directory.display());
        } else if cancelled {
            tracing::info!("Startup cancelled; owned resources stopped");
        }
        Ok(())
    })
}

async fn run_program(
    args: Arguments,
    properties: config::Properties,
    mut settings: config::Settings,
    lifecycle: &lifecycle::Lifecycle,
) -> Result<()> {
    let stop = lifecycle.token();
    stop.check()?;
    if !args.status_only || args.import_blocks.is_some() {
        let data_directory = args.directory.join(&settings.data_directory);
        let plugins_directory = args.directory.join("plugins/java-plugins");
        let java_plugins_present =
            managed_tasks::blocking(lifecycle, &stop, "inspect Java plugins", move |stop| {
                for entry in std::fs::read_dir(plugins_directory)? {
                    stop.check()?;
                    if entry?.path().extension().is_some_and(|ext| ext == "jar") {
                        return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(true);
                    }
                }
                Ok(false)
            })
            .await??;
        if !bootstrap::Bootstrap::is_prepared_with_lifecycle(&data_directory, lifecycle, &stop)
            .await?
            || (settings.java_plugins && java_plugins_present)
        {
            let java = java_runtime::resolve_java_with_lifecycle(
                Path::new(&settings.java_executable),
                &args.directory.join(".cache/java"),
                settings.auto_download_runtime,
                lifecycle,
                &stop,
            )
            .await?;
            settings.java_executable = java
                .to_str()
                .ok_or("Java runtime path is not valid UTF-8")?
                .to_owned();
        }
        let prepared = bootstrap::Bootstrap::prepare_with_lifecycle(
            &data_directory,
            Path::new(&settings.java_executable),
            lifecycle,
            &stop,
        )
        .await?;
        stop.check()?;
        tracing::info!(directory=%prepared.directory.display(), reused=prepared.reused, "Game data ready");
    }
    if let Some(source) = args.import_blocks {
        let destination = args.directory.join(&properties.level_name);
        let data_directory = args.directory.join(&settings.data_directory);
        let report =
            managed_tasks::blocking(lifecycle, &stop, "import world blocks", move |stop| {
                stop.check()?;
                let resolver = game_data::BlockResolver::load(&data_directory)?;
                stop.check()?;
                let result = zerqon_world::anvil::import_world_blocks_cancellable(
                    &source,
                    &destination,
                    &|name, properties| resolver.resolve(name, properties),
                    &|| stop.is_stopped(),
                );
                match result {
                    Err(zerqon_world::anvil::ImportError::World(
                        zerqon_world::WorldError::Cancelled,
                    )) => Err(crate::lifecycle::Stopped.into()),
                    result => Ok::<_, Box<dyn std::error::Error + Send + Sync>>(result?),
                }
            })
            .await??;
        tracing::info!(
            chunks = report.chunks,
            "Block import complete; full world import is not yet supported. See import-report.json for omitted data."
        );
        return Ok(());
    }
    stop.check()?;
    server::run(
        args.directory,
        properties,
        settings,
        args.status_only,
        args.no_console,
        lifecycle,
    )
    .await
}
