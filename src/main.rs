mod auth;
mod bootstrap;
mod config;
mod connection;
mod entity_packets;
mod game_data;
mod java_launch;
mod java_plugins;
mod java_runtime;
mod mob_store;
mod mobs;
mod operator_store;
mod packets;
mod paper_libraries;
mod player_inventory;
mod player_store;
mod players;
mod profiling;
mod runtime;
mod server;
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
    if let Some(data) = args.data {
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
        if !args.status_only || args.import_blocks.is_some() {
            let data_directory=args.directory.join(&settings.data_directory);
            let java_plugins_present=std::fs::read_dir(args.directory.join("plugins/java-plugins"))?.collect::<std::result::Result<Vec<_>,_>>()?.iter().any(|entry|entry.path().extension().is_some_and(|ext|ext=="jar"));
            if !bootstrap::Bootstrap::is_prepared(&data_directory).await? || (settings.java_plugins&&java_plugins_present) {
                let java=java_runtime::resolve_java(Path::new(&settings.java_executable),&args.directory.join(".cache/java"),settings.auto_download_runtime).await?;
                settings.java_executable=java.to_str().ok_or("Java runtime path is not valid UTF-8")?.to_owned();
            }
            let prepared=bootstrap::Bootstrap::prepare(&data_directory,Path::new(&settings.java_executable)).await?;
            tracing::info!(directory=%prepared.directory.display(),reused=prepared.reused,"Game data ready");
        }
        if let Some(source)=args.import_blocks {
            let destination=args.directory.join(&properties.level_name);
            let resolver=game_data::BlockResolver::load(&args.directory.join(&settings.data_directory))?;
            let report=tokio::task::spawn_blocking(move||zerqon_world::anvil::import_world_blocks(&source,&destination,&|name,properties|resolver.resolve(name,properties))).await??;
            tracing::info!(chunks=report.chunks,"Block import complete; full world import is not yet supported. See import-report.json for omitted data.");
            return Ok(());
        }
        server::run(args.directory,properties,settings,args.status_only,args.no_console).await
    })
}
