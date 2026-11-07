use crate::lifecycle::{Lifecycle, StopToken, Stopped};
use crate::managed_tasks::{self as tasks, OperationError, TaskError};
use aws_lc_rs::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const VERSION: &str = "26.3";
pub const PROTOCOL: i32 = 777;
pub const SERVER_SHA1: &str = "33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c";
pub const SERVER_BYTES: u64 = 62_294_556;
const SERVER_URL: &str =
    "https://piston-data.mojang.com/v1/objects/33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c/server.jar";
const REGISTRIES: &[&str] = &[
    "worldgen/biome",
    "chat_type",
    "trim_pattern",
    "trim_material",
    "wolf_variant",
    "wolf_sound_variant",
    "pig_variant",
    "pig_sound_variant",
    "frog_variant",
    "cat_variant",
    "cat_sound_variant",
    "cow_sound_variant",
    "cow_variant",
    "chicken_sound_variant",
    "chicken_variant",
    "zombie_nautilus_variant",
    "painting_variant",
    "sulfur_cube_archetype",
    "dimension_type",
    "damage_type",
    "banner_pattern",
    "enchantment",
    "jukebox_song",
    "instrument",
    "test_environment",
    "test_instance",
    "dialog",
    "world_clock",
    "timeline",
    "decorated_pot_pattern",
    "block_transformer",
    "worldgen/block_state_provider",
];
const REPORTS: &[&str] = &["packets.json", "registries.json", "blocks.json"];

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("Minecraft data preparation: {0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error(transparent)]
    JavaLaunch(#[from] crate::java_launch::JavaLaunchError),
    #[error(transparent)]
    Task(#[from] tokio::task::JoinError),
    #[error("{0}")]
    Managed(#[from] TaskError),
    #[error("{0}")]
    Stopped(#[from] Stopped),
    #[error("{0}")]
    Cleanup(#[from] tasks::CleanupError),
}
impl OperationError for BootstrapError {
    fn is_cancelled(&self) -> bool {
        matches!(self, Self::Stopped(_))
            || matches!(self, Self::Managed(error) if error.is_cancelled())
    }
    fn is_cleanup_failure(&self) -> bool {
        matches!(self, Self::Cleanup(_) | Self::Task(_))
            || matches!(self, Self::Managed(error) if error.is_cleanup_failure())
    }
}
pub struct PreparedData {
    pub directory: PathBuf,
    pub reused: bool,
}
pub struct Bootstrap;
impl Bootstrap {
    pub async fn is_prepared_with_lifecycle(
        directory: &Path,
        lifecycle: &Lifecycle,
        stop: &StopToken,
    ) -> Result<bool, BootstrapError> {
        let directory = directory.to_owned();
        tasks::blocking(lifecycle, stop, "check Minecraft data cache", move |stop| {
            stop.check()?;
            let ready = cache_ready(&directory)?;
            stop.check()?;
            Ok::<_, BootstrapError>(ready)
        })
        .await?
    }

    pub async fn prepare_with_lifecycle(
        directory: &Path,
        java: &Path,
        lifecycle: &Lifecycle,
        stop: &StopToken,
    ) -> Result<PreparedData, BootstrapError> {
        let directory = directory.to_owned();
        let java = java.to_owned();
        let context = lifecycle.clone();
        tasks::run(lifecycle, stop, "prepare Minecraft data", move |stop| async move {
            Self::prepare_inner(&directory, &java, &context, &stop).await
        }).await?
    }

    async fn prepare_inner(
        directory: &Path,
        java: &Path,
        lifecycle: &Lifecycle,
        stop: &StopToken,
    ) -> Result<PreparedData, BootstrapError> {
        stop.check()?;
        let directory = checked_cache_directory(directory)?;
        reject_cache_links(&directory.join("prepare.lock"))?;
        let lock = std::sync::Arc::new(
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(directory.join("prepare.lock"))?,
        );
        acquire_prepare_lock_tracked(&lock, Duration::from_secs(600), stop).await?;
        let jar = directory.join("server.jar");
        let verify_path = jar.clone();
        let verify_lock = std::sync::Arc::clone(&lock);
        let verified = tasks::blocking(
            lifecycle,
            stop,
            "verify Minecraft server archive",
            move |stop| {
                let _lock = verify_lock;
                stop.check()?;
                let verified = verify_file(&verify_path, SERVER_BYTES, SERVER_SHA1)?;
                stop.check()?;
                Ok::<_, BootstrapError>(verified)
            },
        )
        .await??;
        if !verified {
            if jar.exists() {
                return Err(invalid(
                    "cached server.jar failed the pinned Minecraft 26.3 checksum; preserve/remove it before retrying",
                ));
            }
            tracing::info!("Downloading official Minecraft 26.3 server data (59.4 MiB)");
            download_tracked(&jar, SERVER_URL, SERVER_BYTES, SERVER_SHA1, stop).await?;
        }
        let check_directory = directory.clone();
        let check_lock = std::sync::Arc::clone(&lock);
        if tasks::blocking(
            lifecycle,
            stop,
            "validate Minecraft data cache",
            move |stop| {
                let _lock = check_lock;
                stop.check()?;
                let ready = cache_ready(&check_directory)?;
                stop.check()?;
                Ok::<_, BootstrapError>(ready)
            },
        )
        .await??
        {
            tracing::info!("Minecraft 26.3 data cache verified");
            return Ok(PreparedData {
                directory,
                reused: true,
            });
        }
        tracing::info!("Preparing Minecraft 26.3 registries and protocol reports");
        let java = java_runtime_tracked(java, lifecycle, stop).await?;
        let stage = std::sync::Arc::new(
            tempfile::Builder::new()
                .prefix("prepare-")
                .tempdir_in(&directory)?,
        );
        generate_reports_tracked(&java, &jar, std::sync::Arc::clone(&stage), lifecycle, stop)
            .await?;
        let jar_path = jar.clone();
        let prepared = preparation_task_tracked(
            std::sync::Arc::clone(&stage),
            std::sync::Arc::clone(&lock),
            move |stage| prepare_payloads(&jar_path, stage),
            lifecycle,
            stop,
        )
        .await?;
        let destination = directory.clone();
        preparation_task_tracked(
            std::sync::Arc::clone(&stage),
            std::sync::Arc::clone(&lock),
            move |stage| install_prepared(stage, &destination, &prepared),
            lifecycle,
            stop,
        )
        .await?;
        stop.check()?;
        let checked = directory.clone();
        if !preparation_task_tracked(
            std::sync::Arc::clone(&stage),
            std::sync::Arc::clone(&lock),
            move |_| cache_ready(&checked),
            lifecycle,
            stop,
        )
        .await?
        {
            return Err(invalid("prepared data failed final validation"));
        }
        tracing::info!("Minecraft 26.3 data preparation complete");
        Ok(PreparedData {
            directory,
            reused: false,
        })
    }
}

struct PreparationResources {
    stage: std::sync::Arc<tempfile::TempDir>,
    _lock: std::sync::Arc<File>,
}

async fn preparation_task_tracked<T: Send + 'static>(
    stage: std::sync::Arc<tempfile::TempDir>,
    lock: std::sync::Arc<File>,
    work: impl FnOnce(&Path) -> Result<T, BootstrapError> + Send + 'static,
    lifecycle: &Lifecycle,
    stop: &StopToken,
) -> Result<T, BootstrapError> {
    let resources = PreparationResources { stage, _lock: lock };
    tasks::blocking(lifecycle, stop, "Minecraft data worker", move |stop| {
        stop.check()?;
        let result = work(resources.stage.path());
        drop(resources);
        result
    })
    .await?
}

async fn acquire_prepare_lock_tracked(
    lock: &File,
    timeout: Duration,
    stop: &StopToken,
) -> Result<(), BootstrapError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        stop.check()?;
        match lock.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            Err(std::fs::TryLockError::WouldBlock) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(invalid(
                        "timed out waiting for another process to finish preparing Minecraft data",
                    ));
                }
                tasks::cancellable(stop, async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok::<_, BootstrapError>(())
                })
                .await?;
            }
        }
    }
}

fn invalid(message: impl Into<String>) -> BootstrapError {
    BootstrapError::Invalid(message.into())
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn verify_file(path: &Path, size: u64, sha1: &str) -> Result<bool, BootstrapError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if file.metadata()?.len() != size {
        return Ok(false);
    }
    let mut digest = Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
    let mut buffer = [0; 65536];
    let mut read = 0;
    loop {
        let count = Read::read(&mut file, &mut buffer)?;
        if count == 0 {
            break;
        }
        read += count as u64;
        if read > size {
            return Ok(false);
        }
        digest.update(&buffer[..count]);
    }
    Ok(read == size && hex(digest.finish().as_ref()) == sha1)
}

async fn download_tracked(
    destination: &Path,
    url: &str,
    size: u64,
    sha1: &str,
    stop: &StopToken,
) -> Result<(), BootstrapError> {
    stop.check()?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(300))
        .user_agent("Zerqon/0.0.1")
        .build()?;
    let mut response = tasks::cancellable(stop, async {
        Ok::<_, BootstrapError>(client.get(url).send().await?.error_for_status()?)
    })
    .await?;
    if response
        .content_length()
        .is_some_and(|length| length != size)
    {
        return Err(invalid("download Content-Length differs from pinned size"));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| invalid("download destination needs a parent"))?;
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    let mut file = tokio::fs::File::from_std(temporary.reopen()?);
    let mut length = 0u64;
    let mut digest = Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
    while let Some(bytes) = tasks::cancellable(stop, async {
        Ok::<_, BootstrapError>(response.chunk().await?)
    })
    .await?
    {
        length = length
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| invalid("download size overflow"))?;
        if length > size {
            return Err(invalid("download exceeds pinned size"));
        }
        digest.update(&bytes);
        file.write_all(&bytes).await?;
        file.flush().await?;
        stop.check()?;
    }
    if length != size || hex(digest.finish().as_ref()) != sha1 {
        return Err(invalid("download failed pinned length/SHA-1 verification"));
    }
    file.sync_all().await?;
    stop.check()?;
    drop(file);
    temporary
        .persist_noclobber(destination)
        .map_err(|error| error.error)?;
    Ok(())
}

fn read_json(path: &Path, limit: u64) -> Result<Value, BootstrapError> {
    reject_cache_links(path)?;
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(invalid("JSON path must be a regular file"));
    }
    let file = File::open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > limit {
        return Err(invalid(format!(
            "{} exceeds its size limit or is not a file",
            path.display()
        )));
    }
    let mut data = Vec::new();
    Read::take(file, limit + 1).read_to_end(&mut data)?;
    if data.len() as u64 > limit {
        return Err(invalid("JSON grew beyond its size limit"));
    }
    Ok(serde_json::from_slice(&data)?)
}

fn cache_ready(directory: &Path) -> Result<bool, BootstrapError> {
    reject_cache_links(&directory.join("prepared.json"))?;
    if !directory.join("prepared.json").is_file() {
        return Ok(false);
    }
    let identity = match read_json(&directory.join("prepared.json"), 65536) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    if identity["version"] != VERSION
        || identity["protocol"] != PROTOCOL
        || identity["server_sha1"] != SERVER_SHA1
        || identity["world_version"] != 5023
        || identity["java_major"] != 25
    {
        return Ok(false);
    }
    for report in REPORTS {
        reject_cache_links(&directory.join("generated/reports").join(report))?;
        let value = match read_json(
            &directory.join("generated/reports").join(report),
            16 * 1024 * 1024,
        ) {
            Ok(value) => value,
            Err(_) => return Ok(false),
        };
        if !value.is_object() || value.as_object().is_none_or(|object| object.is_empty()) {
            return Ok(false);
        }
        let valid = match *report {
            "packets.json" => {
                value["handshake"]["serverbound"]["minecraft:intention"]["protocol_id"] == 0
                    && value["login"]["clientbound"]["minecraft:login_finished"]["protocol_id"] == 2
                    && value["play"]["clientbound"]["minecraft:login"]["protocol_id"] == 50
            }
            "blocks.json" => value["minecraft:air"]["states"]
                .as_array()
                .is_some_and(|states| {
                    states
                        .iter()
                        .any(|state| state["id"] == 0 && state["default"] == true)
                }),
            "registries.json" => {
                value["minecraft:entity_type"]["entries"]["minecraft:player"]["protocol_id"]
                    .as_i64()
                    .is_some_and(|id| id >= 0)
            }
            _ => false,
        };
        if !valid {
            return Ok(false);
        }
    }
    let item_reports = match item_report_paths(directory) {
        Ok(paths) => paths,
        Err(_) => return Ok(false),
    };
    let mut item_bytes = 0u64;
    let mut checked_parents = HashSet::new();
    for report in item_reports {
        let path = directory.join(report);
        let parent = path
            .parent()
            .ok_or_else(|| invalid("missing item report parent"))?;
        if checked_parents.insert(parent.to_owned()) {
            reject_cache_links(parent)?;
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if is_cache_link(&metadata) {
            return Err(invalid("item report is a link or reparse point"));
        }
        if !metadata.is_file() || metadata.len() > 1024 * 1024 {
            return Ok(false);
        }
        item_bytes += metadata.len();
        if item_bytes > 32 * 1024 * 1024 {
            return Ok(false);
        }
    }
    if zerqon_inventory::ItemCatalog::load(directory).is_err() {
        return Ok(false);
    }
    let registries = match read_json(&directory.join("registries-sync.json"), 16 * 1024 * 1024) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    if registries["version"] != VERSION
        || registries["protocol"] != PROTOCOL
        || registries["registries"]
            .as_array()
            .is_none_or(|array| array.len() != REGISTRIES.len())
    {
        return Ok(false);
    }
    let mut entry_count = 0usize;
    for (index, registry) in registries["registries"]
        .as_array()
        .ok_or_else(|| invalid("missing cached registries"))?
        .iter()
        .enumerate()
    {
        if registry["name"] != format!("minecraft:{}", REGISTRIES[index]) {
            return Ok(false);
        }
        let Some(entries) = registry["entries"].as_array() else {
            return Ok(false);
        };
        let mut names = HashSet::new();
        for entry in entries {
            let Some(name) = entry["id"].as_str() else {
                return Ok(false);
            };
            if !names.insert(name)
                || (!entry["data"].is_object()
                    && !(registry["name"] == "minecraft:block_transformer"
                        && entry["data"].is_array()))
            {
                return Ok(false);
            }
        }
        entry_count += entries.len();
    }
    if identity["entries"].as_u64() != Some(entry_count as u64) || entry_count == 0 {
        return Ok(false);
    }
    let tags = match read_json(&directory.join("tags-sync.json"), 16 * 1024 * 1024) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    let Some(tags) = tags["registries"].as_array() else {
        return Ok(false);
    };
    if tags.is_empty() || identity["tag_registries"].as_u64() != Some(tags.len() as u64) {
        return Ok(false);
    }
    let mut count = 0usize;
    for registry in tags {
        if !registry["name"].is_string() {
            return Ok(false);
        }
        let Some(tags) = registry["tags"].as_array() else {
            return Ok(false);
        };
        count += tags.len();
        for tag in tags {
            if !tag["name"].is_string()
                || tag["ids"].as_array().is_none_or(|ids| {
                    ids.iter().any(|id| {
                        id.as_i64()
                            .is_none_or(|id| !(0..=i32::MAX as i64).contains(&id))
                    })
                })
            {
                return Ok(false);
            }
        }
    }
    Ok(identity["tags"].as_u64() == Some(count as u64))
}

fn item_report_paths(directory: &Path) -> Result<Vec<PathBuf>, BootstrapError> {
    let registries = read_json(
        &directory.join("generated/reports/registries.json"),
        8 * 1024 * 1024,
    )?;
    let items = registries["minecraft:item"]["entries"]
        .as_object()
        .filter(|items| !items.is_empty() && items.len() <= 65_536)
        .ok_or_else(|| invalid("missing or oversized item registry"))?;
    items
        .keys()
        .map(|key| {
            let (namespace, name) = key
                .split_once(':')
                .filter(|(namespace, name)| {
                    key.len() <= 256
                        && !namespace.is_empty()
                        && !name.is_empty()
                        && namespace.bytes().all(|byte| {
                            byte.is_ascii_lowercase()
                                || byte.is_ascii_digit()
                                || b"_.-".contains(&byte)
                        })
                        && name.bytes().all(|byte| {
                            byte.is_ascii_lowercase()
                                || byte.is_ascii_digit()
                                || b"/_.-".contains(&byte)
                        })
                })
                .ok_or_else(|| invalid("invalid item registry key"))?;
            let relative = format!("generated/reports/{namespace}/components/item/{name}.json");
            safe_archive_path(&relative)?;
            Ok(PathBuf::from(relative))
        })
        .collect()
}

async fn java_runtime_tracked(
    java: &Path,
    lifecycle: &Lifecycle,
    stop: &StopToken,
) -> Result<PathBuf, BootstrapError> {
    let java = java.to_owned();
    tasks::run(
        lifecycle,
        stop,
        "Minecraft Java probe",
        move |stop| async move { run_java_probe(&java, &stop).await },
    )
    .await?
}

async fn run_java_probe(java: &Path, stop: &StopToken) -> Result<PathBuf, BootstrapError> {
    stop.check()?;
    let mut child=tokio::process::Command::new(java).args(["-Dfile.encoding=UTF-8","-Dsun.stdout.encoding=UTF-8","-Dsun.stderr.encoding=UTF-8","-XshowSettings:properties","-version"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true).spawn().map_err(|error|invalid(format!("Java 25+ is required once to prepare official reports; set java_executable to its java binary ({error})")))?;
    let run = async {
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| invalid("missing Java runtime probe pipe"))?;
        let mut bytes = Vec::new();
        stderr.take(65537).read_to_end(&mut bytes).await?;
        if bytes.len() > 65536 {
            return Err(invalid("Java runtime probe exceeds output limit"));
        }
        if !child.wait().await?.success() {
            return Err(invalid("Java runtime probe failed; Java 25+ is required"));
        }
        let text =
            std::str::from_utf8(&bytes).map_err(|_| invalid("invalid Java runtime properties"))?;
        let home = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("java.home = "))
            .ok_or_else(|| invalid("Java home not reported"))?;
        let major = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("java.specification.version = "))
            .and_then(|version| version.parse::<u32>().ok())
            .ok_or_else(|| invalid("Java version not reported"))?;
        if major < 25 {
            return Err(invalid("Java 25+ is required to prepare Minecraft 26.3"));
        }
        let executable =
            Path::new(home)
                .join("bin")
                .join(if cfg!(windows) { "java.exe" } else { "java" });
        if !executable.is_file() {
            return Err(invalid("reported Java executable does not exist"));
        }
        Ok(executable)
    };
    let result = tokio::select! {
        biased;
        _ = stop.cancelled() => Err(Stopped.into()),
        result = tokio::time::timeout(Duration::from_secs(20), tasks::catch_panic(run)) => match result {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => Err(error.into()),
            Err(_) => Err(invalid("Java runtime probe timed out")),
        },
    };
    tasks::terminate_child(&mut child).await?;
    result
}

fn generator_main_class(jar: &Path) -> Result<String, BootstrapError> {
    let file = File::open(jar)?;
    if file.metadata()?.len() > SERVER_BYTES {
        return Err(invalid("generator JAR exceeds its size limit"));
    }
    let mut archive = zip::ZipArchive::new(file)?;
    let entry = archive.by_name("META-INF/MANIFEST.MF")?;
    if entry.size() > 65_536 {
        return Err(invalid("generator manifest exceeds 64 KiB"));
    }
    let mut bytes = Vec::new();
    entry.take(65_537).read_to_end(&mut bytes)?;
    if bytes.len() > 65_536 {
        return Err(invalid("generator manifest grew beyond its size limit"));
    }
    let text =
        std::str::from_utf8(&bytes).map_err(|_| invalid("generator manifest is not UTF-8"))?;
    let mut attributes: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            break;
        }
        if let Some(continuation) = line.strip_prefix(' ') {
            attributes
                .last_mut()
                .ok_or_else(|| invalid("generator manifest starts with a continuation"))?
                .push_str(continuation);
        } else {
            attributes.push(line.to_owned());
        }
    }
    let mut main_class = None;
    for attribute in attributes {
        let (key, value) = attribute
            .split_once(": ")
            .ok_or_else(|| invalid("invalid generator manifest attribute"))?;
        if key.eq_ignore_ascii_case("Main-Class") && main_class.replace(value.to_owned()).is_some()
        {
            return Err(invalid("duplicate generator Main-Class"));
        }
    }
    main_class.ok_or_else(|| invalid("generator manifest has no Main-Class"))
}

async fn generate_reports_tracked(
    java: &Path,
    jar: &Path,
    stage: std::sync::Arc<tempfile::TempDir>,
    lifecycle: &Lifecycle,
    stop: &StopToken,
) -> Result<(), BootstrapError> {
    let java = java.to_owned();
    let jar = jar.to_owned();
    let context = lifecycle.clone();
    tasks::run(
        lifecycle,
        stop,
        "Minecraft report generator",
        move |stop| async move { run_reports(&java, &jar, stage, &context, &stop).await },
    )
    .await?
}

async fn run_reports(
    java: &Path,
    jar: &Path,
    stage: std::sync::Arc<tempfile::TempDir>,
    lifecycle: &Lifecycle,
    stop: &StopToken,
) -> Result<(), BootstrapError> {
    stop.check()?;
    let source_jar = jar.to_owned();
    let launch = tasks::blocking(
        lifecycle,
        stop,
        "build Minecraft generator launch",
        move |stop| {
            stop.check()?;
            let main_class = generator_main_class(&source_jar)?;
            Ok::<_, BootstrapError>(crate::java_launch::manifest_jar(
                &[source_jar.canonicalize()?],
                Some(&main_class),
            )?)
        },
    )
    .await??;
    stop.check()?;
    let mut child = tokio::process::Command::new(java)
        .args([
            "-Dfile.encoding=UTF-8",
            "-Dsun.stdout.encoding=UTF-8",
            "-Dsun.stderr.encoding=UTF-8",
            "-Xms64M",
            "-Xmx512M",
            "-DbundlerMainClass=net.minecraft.data.Main",
            "-jar",
        ])
        .arg(&launch.argument)
        .arg("--reports")
        .arg("--output")
        .arg("generated")
        .current_dir(stage.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let run = async {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| invalid("missing data generator stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| invalid("missing data generator stderr"))?;
        let (out, err, status) =
            tokio::join!(drain_output(stdout), drain_output(stderr), child.wait());
        out?;
        err?;
        if !status?.success() {
            return Err(invalid(
                "Mojang report generator failed; inspect preparation logs",
            ));
        }
        Ok(())
    };
    let result = tokio::select! {
        biased;
        _ = stop.cancelled() => Err(Stopped.into()),
        result = tokio::time::timeout(Duration::from_secs(180), tasks::catch_panic(run)) => {
            match result {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => Err(error.into()),
                Err(_) => Err(invalid("Mojang report generation exceeded 180 seconds")),
            }
        }
    };
    tasks::terminate_child(&mut child).await?;
    drop(launch);
    drop(stage);
    result
}
async fn drain_output(mut output: impl tokio::io::AsyncRead + Unpin) -> Result<(), BootstrapError> {
    let mut bytes = [0; 4096];
    let mut count = 0;
    loop {
        let n = output.read(&mut bytes).await?;
        if n == 0 {
            return Ok(());
        }
        count += n;
        if count > 16 * 1024 * 1024 {
            return Err(invalid("report generator output limit exceeded"));
        }
        tracing::debug!("{}", String::from_utf8_lossy(&bytes[..n]).trim());
    }
}

fn safe_archive_path(name: &str) -> Result<Vec<&str>, BootstrapError> {
    if name.starts_with('/') || name.contains('\\') {
        return Err(invalid("unsafe ZIP path"));
    }
    let parts: Vec<_> = name.trim_end_matches('/').split('/').collect();
    for part in &parts {
        let base = part
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if part.is_empty()
            || matches!(*part, "." | "..")
            || part.contains([':', '<', '>', '"', '|', '?', '*'])
            || part.ends_with(['.', ' '])
            || part.chars().any(char::is_control)
            || matches!(base.as_str(), "CON" | "NUL" | "PRN" | "AUX")
            || (base.len() == 4
                && (base.starts_with("COM") || base.starts_with("LPT"))
                && matches!(base.as_bytes()[3], b'1'..=b'9'))
        {
            return Err(invalid("unsafe ZIP path component"));
        }
    }
    Ok(parts)
}

fn zip_bytes<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
    limit: u64,
) -> Result<Vec<u8>, BootstrapError> {
    let file = archive.by_name(name)?;
    if file.size() > limit {
        return Err(invalid("ZIP entry exceeds its size limit"));
    }
    let mut bytes = Vec::new();
    Read::take(file, limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("inflated ZIP entry exceeds its size limit"));
    }
    Ok(bytes)
}

fn prepare_payloads(jar: &Path, stage: &Path) -> Result<Value, BootstrapError> {
    let mut outer = zip::ZipArchive::new(File::open(jar)?)?;
    let version: Value = serde_json::from_slice(&zip_bytes(&mut outer, "version.json", 65536)?)?;
    if version["id"] != VERSION
        || version["protocol_version"] != PROTOCOL
        || version["world_version"] != 5023
        || version["java_version"] != 25
    {
        return Err(invalid(
            "official server version/protocol does not match26.3",
        ));
    }
    let nested = zip_bytes(
        &mut outer,
        "META-INF/versions/26.3/server-26.3.jar",
        64 * 1024 * 1024,
    )?;
    let mut inner = zip::ZipArchive::new(Cursor::new(nested))?;
    if inner.len() > 100_000 {
        return Err(invalid("server JAR contains too many entries"));
    }
    let mut data = BTreeMap::new();
    let mut total = 0u64;
    for index in 0..inner.len() {
        let mut file = inner.by_index(index)?;
        let name = file.name()?.into_owned();
        let parts = safe_archive_path(&name)?;
        if parts.len() < 4 || parts[0] != "data" || !name.ends_with(".json") {
            continue;
        }
        let relative = parts[2..].join("/");
        if !relative.starts_with("tags/")
            && !REGISTRIES
                .iter()
                .any(|registry| relative.starts_with(&format!("{registry}/")))
        {
            continue;
        }
        if file.size() > 1024 * 1024 {
            return Err(invalid("registry JSON exceeds1MiB"));
        }
        total += file.size();
        if total > 64 * 1024 * 1024 {
            return Err(invalid("registry JSON total exceeds64MiB"));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            return Err(invalid("inflated registry JSON exceeds1MiB"));
        }
        let value: Value = serde_json::from_slice(&bytes)?;
        let key = parts[1..].join("/");
        if data.insert(key.clone(), value).is_some() {
            return Err(invalid("duplicate registry ZIP entry"));
        }
        let target = stage.join("data").join(key);
        fs::create_dir_all(
            target
                .parent()
                .ok_or_else(|| invalid("missing data parent"))?,
        )?;
        fs::write(target, bytes)?;
    }
    let registries = collect_registries(&data)?;
    let builtin = read_json(
        &stage.join("generated/reports/registries.json"),
        16 * 1024 * 1024,
    )?;
    let tags = collect_tags(&data, &builtin, &registries)?;
    fs::write(
        stage.join("registries-sync.json"),
        serde_json::to_vec(
            &json!({"version":VERSION,"protocol":PROTOCOL,"registries":registries}),
        )?,
    )?;
    fs::write(
        stage.join("tags-sync.json"),
        serde_json::to_vec(&json!({"registries":tags}))?,
    )?;
    let prepared = json!({"version":VERSION,"protocol":PROTOCOL,"server_sha1":SERVER_SHA1,"world_version":5023,"java_major":25,"registries":registries.len(),"entries":registries.iter().map(|registry|registry["entries"].as_array().map_or(0,Vec::len)).sum::<usize>(),"tag_registries":tags.len(),"tags":tags.iter().map(|registry|registry["tags"].as_array().map_or(0,Vec::len)).sum::<usize>()});
    fs::write(
        stage.join("prepared.json"),
        serde_json::to_vec_pretty(&prepared)?,
    )?;
    if !cache_ready(stage)? {
        return Err(invalid(
            "staged Minecraft reports or registry data failed validation",
        ));
    }
    Ok(prepared)
}

fn collect_registries(data: &BTreeMap<String, Value>) -> Result<Vec<Value>, BootstrapError> {
    let mut registries = Vec::new();
    for registry in REGISTRIES {
        let mut entries = Vec::new();
        for (path, value) in data {
            let Some((namespace, relative)) = path.split_once('/') else {
                return Err(invalid("invalid registry data path"));
            };
            if let Some(name) = relative.strip_prefix(&format!("{registry}/")) {
                entries.push((name, namespace, value));
            }
        }
        entries.sort_by_key(|(name, namespace, _)| (*name, *namespace));
        registries.push(json!({"name":format!("minecraft:{registry}"),"entries":entries.into_iter().map(|(name,namespace,value)|json!({"id":format!("{namespace}:{}",name.trim_end_matches(".json")),"data":value})).collect::<Vec<_>>()}));
    }
    Ok(registries)
}

fn collect_tags(
    data: &BTreeMap<String, Value>,
    builtin: &Value,
    registries: &[Value],
) -> Result<Vec<Value>, BootstrapError> {
    let mut identifiers: BTreeMap<String, BTreeMap<String, i32>> = BTreeMap::new();
    for (name, registry) in builtin
        .as_object()
        .ok_or_else(|| invalid("builtin registries must be an object"))?
    {
        let entries = registry["entries"]
            .as_object()
            .ok_or_else(|| invalid("registry entries must be an object"))?;
        let mut ids = BTreeMap::new();
        for (name, value) in entries {
            let id = value["protocol_id"]
                .as_i64()
                .and_then(|id| i32::try_from(id).ok())
                .filter(|id| *id >= 0)
                .ok_or_else(|| invalid("invalid registry protocol ID"))?;
            ids.insert(name.clone(), id);
        }
        identifiers.insert(name.clone(), ids);
    }
    for registry in registries {
        let name = registry["name"]
            .as_str()
            .ok_or_else(|| invalid("missing dynamic registry name"))?;
        let mut ids = BTreeMap::new();
        for (index, entry) in registry["entries"]
            .as_array()
            .ok_or_else(|| invalid("missing registry entries"))?
            .iter()
            .enumerate()
        {
            let name = entry["id"]
                .as_str()
                .ok_or_else(|| invalid("missing registry entry ID"))?;
            ids.insert(name.into(), index as i32);
        }
        identifiers.insert(name.into(), ids);
    }
    let mut result = Vec::new();
    for (registry, ids) in identifiers {
        let (namespace, relative) = registry
            .split_once(':')
            .ok_or_else(|| invalid("invalid registry identifier"))?;
        let registry_path = if namespace == "minecraft" {
            relative.to_owned()
        } else {
            format!("{namespace}/{relative}")
        };
        let mut definitions = BTreeMap::new();
        for (path, value) in data {
            let Some((namespace, relative)) = path.split_once('/') else {
                continue;
            };
            if let Some(name) = relative.strip_prefix(&format!("tags/{registry_path}/")) {
                let entries = value["values"]
                    .as_array()
                    .ok_or_else(|| invalid("tag values must be an array"))?;
                definitions.insert(
                    format!("{namespace}:{}", name.trim_end_matches(".json")),
                    entries.clone(),
                );
            }
        }
        if !definitions.is_empty() {
            let tags = resolve_tags(&definitions, &ids)?;
            result.push(json!({"name":registry,"tags":tags.into_iter().map(|(name,ids)|json!({"name":name,"ids":ids})).collect::<Vec<_>>()}));
        }
    }
    Ok(result)
}

fn resolve_tags(
    definitions: &BTreeMap<String, Vec<Value>>,
    identifiers: &BTreeMap<String, i32>,
) -> Result<BTreeMap<String, Vec<i32>>, BootstrapError> {
    fn resolve(
        name: &str,
        definitions: &BTreeMap<String, Vec<Value>>,
        ids: &BTreeMap<String, i32>,
        visiting: &mut HashSet<String>,
        resolved: &mut BTreeMap<String, Vec<i32>>,
    ) -> Result<Vec<i32>, BootstrapError> {
        if let Some(values) = resolved.get(name) {
            return Ok(values.clone());
        }
        if visiting.len() >= 128 || !visiting.insert(name.into()) {
            return Err(invalid(format!("tag cycle or depth limit at {name}")));
        }
        let entries = definitions
            .get(name)
            .ok_or_else(|| invalid("missing tag definition"))?;
        let mut values = Vec::new();
        let mut unique = HashSet::new();
        for entry in entries {
            let (identifier, required) = if let Some(id) = entry.as_str() {
                (id, true)
            } else {
                let id = entry["id"]
                    .as_str()
                    .ok_or_else(|| invalid("tag entry needs string ID"))?;
                let required = match entry.get("required") {
                    None => true,
                    Some(value) => value
                        .as_bool()
                        .ok_or_else(|| invalid("tag required must be boolean"))?,
                };
                (id, required)
            };
            let nested = identifier.starts_with('#');
            let key = identifier.strip_prefix('#').unwrap_or(identifier);
            let key = if key.contains(':') {
                key.to_owned()
            } else {
                format!("minecraft:{key}")
            };
            if !(if nested {
                definitions.contains_key(&key)
            } else {
                ids.contains_key(&key)
            }) {
                if required {
                    return Err(invalid(format!(
                        "missing required tag entry {identifier} in {name}"
                    )));
                }
                continue;
            }
            let added = if nested {
                resolve(&key, definitions, ids, visiting, resolved)?
            } else {
                vec![ids[&key]]
            };
            for id in added {
                if unique.insert(id) {
                    values.push(id);
                    if values.len() > 65536 {
                        return Err(invalid("resolved tag exceeds65536 IDs"));
                    }
                }
            }
        }
        visiting.remove(name);
        resolved.insert(name.into(), values.clone());
        Ok(values)
    }
    let mut resolved = BTreeMap::new();
    let mut visiting = HashSet::new();
    for name in definitions.keys() {
        resolve(name, definitions, identifiers, &mut visiting, &mut resolved)?;
    }
    Ok(resolved)
}

fn install_prepared(
    stage: &Path,
    destination: &Path,
    prepared: &Value,
) -> Result<(), BootstrapError> {
    if !cache_ready(stage)? || read_json(&stage.join("prepared.json"), 65536)? != *prepared {
        return Err(invalid("staged data changed before installation"));
    }
    let item_reports = item_report_paths(stage)?;
    let destination = checked_cache_directory(destination)?;
    fn preflight(source: &Path, destination: &Path) -> Result<(), BootstrapError> {
        reject_cache_links(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if is_cache_link(&metadata) {
                return Err(invalid("link in staged data"));
            }
            let target = destination.join(entry.file_name());
            reject_cache_links(&target)?;
            if metadata.is_dir() {
                preflight(&entry.path(), &target)?;
            } else if !metadata.is_file() {
                return Err(invalid("unexpected staged file type"));
            }
        }
        Ok(())
    }
    preflight(&stage.join("data"), &destination.join("data"))?;
    for path in [
        "generated/reports",
        "registries-sync.json",
        "tags-sync.json",
        "prepared.json",
    ] {
        reject_cache_links(&destination.join(path))?;
    }
    for report in REPORTS {
        reject_cache_links(&destination.join("generated/reports").join(report))?;
    }
    for report in &item_reports {
        reject_cache_links(&destination.join(report))?;
    }
    fn copy_tree(source: &Path, destination: &Path) -> Result<(), BootstrapError> {
        checked_cache_directory(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
            } else if entry.file_type()?.is_file() {
                atomic_copy(&entry.path(), &destination.join(entry.file_name()))?;
            } else {
                return Err(invalid("unexpected link in prepared data"));
            }
        }
        Ok(())
    }
    copy_tree(&stage.join("data"), &destination.join("data"))?;
    checked_cache_directory(&destination.join("generated/reports"))?;
    for report in REPORTS {
        atomic_copy(
            &stage.join("generated/reports").join(report),
            &destination.join("generated/reports").join(report),
        )?;
    }
    for report in item_reports {
        let target = destination.join(&report);
        checked_cache_directory(
            target
                .parent()
                .ok_or_else(|| invalid("missing item report parent"))?,
        )?;
        atomic_copy(&stage.join(report), &target)?;
    }
    for file in ["registries-sync.json", "tags-sync.json"] {
        atomic_copy(&stage.join(file), &destination.join(file))?;
    }
    reject_cache_links(&destination.join("prepared.json"))?;
    let mut identity = tempfile::NamedTempFile::new_in(&destination)?;
    identity.write_all(&serde_json::to_vec_pretty(prepared)?)?;
    identity.as_file().sync_all()?;
    identity
        .persist(destination.join("prepared.json"))
        .map_err(|error| error.error)?;
    Ok(())
}
fn atomic_copy(source: &Path, destination: &Path) -> Result<(), BootstrapError> {
    reject_cache_links(destination)?;
    let parent = destination
        .parent()
        .ok_or_else(|| invalid("missing output parent"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::io::copy(&mut File::open(source)?, &mut temporary)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(destination)
        .map_err(|error| error.error)?;
    Ok(())
}

fn is_cache_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn reject_cache_links(path: &Path) -> Result<(), BootstrapError> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for part in absolute.components() {
        current.push(part.as_os_str());
        if !matches!(
            part,
            std::path::Component::Normal(_) | std::path::Component::ParentDir
        ) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if is_cache_link(&metadata) => {
                return Err(invalid(format!(
                    "cache path contains a link or reparse point: {}",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn checked_cache_directory(path: &Path) -> Result<PathBuf, BootstrapError> {
    reject_cache_links(path)?;
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for part in absolute.components() {
        current.push(part.as_os_str());
        if !matches!(part, std::path::Component::Normal(_)) {
            continue;
        }
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let metadata = fs::symlink_metadata(&current)?;
        if is_cache_link(&metadata) || !metadata.is_dir() {
            return Err(invalid(format!(
                "cache path must contain only regular directories: {}",
                current.display()
            )));
        }
    }
    reject_cache_links(&absolute)?;
    Ok(absolute.canonicalize()?)
}
