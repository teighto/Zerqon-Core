use crate::lifecycle::{Lifecycle, StopToken, Stopped};
use crate::managed_tasks::{self as tasks, OperationError, TaskError};
use aws_lc_rs::digest::{Context, SHA256};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const RELEASE: &str = "jdk-25.0.5+8";
const ARCHIVE_ROOT: &str = "jdk-25.0.5+8-jre";
const MAX_UNPACKED: u64 = 512 * 1024 * 1024;
const MAX_FILE: u64 = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    WindowsX64,
    LinuxX64,
}
#[derive(Clone, Copy)]
struct Asset {
    platform: Platform,
    size: u64,
    sha256: &'static str,
    name: &'static str,
    url: &'static str,
}
const WINDOWS: Asset = Asset {
    platform: Platform::WindowsX64,
    size: 58_589_419,
    sha256: "da78d54a2fcdbba1e92d5e3f07dd30236f96297891c4a21a781c3f38f5c0beae",
    name: "OpenJDK25U-jre_x64_windows_hotspot_25.0.5_8.zip",
    url: "https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5%2B8/OpenJDK25U-jre_x64_windows_hotspot_25.0.5_8.zip",
};
const LINUX: Asset = Asset {
    platform: Platform::LinuxX64,
    size: 61_736_206,
    sha256: "6fac98f0c8202efc3135a9e9e42aa61963ec42d1c5cbb40caf231e7b14a24ef6",
    name: "OpenJDK25U-jre_x64_linux_hotspot_25.0.5_8.tar.gz",
    url: "https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5%2B8/OpenJDK25U-jre_x64_linux_hotspot_25.0.5_8.tar.gz",
};

#[derive(Debug, thiserror::Error)]
pub enum JavaRuntimeError {
    #[error("Java runtime: {0}")]
    Invalid(String),
    #[error("Java runtime probe exited with {0}")]
    ProbeExit(std::process::ExitStatus),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error(transparent)]
    Task(#[from] tokio::task::JoinError),
    #[error("{0}")]
    Managed(#[from] TaskError),
    #[error("{0}")]
    Stopped(#[from] Stopped),
    #[error("{0}")]
    Cleanup(#[from] tasks::CleanupError),
}
impl OperationError for JavaRuntimeError {
    fn is_cancelled(&self) -> bool {
        matches!(self, Self::Stopped(_))
            || matches!(self, Self::Managed(error) if error.is_cancelled())
    }
    fn is_cleanup_failure(&self) -> bool {
        matches!(self, Self::Cleanup(_) | Self::Task(_))
            || matches!(self, Self::Managed(error) if error.is_cleanup_failure())
    }
}
fn invalid(message: impl Into<String>) -> JavaRuntimeError {
    JavaRuntimeError::Invalid(message.into())
}
impl Platform {
    fn os(self) -> &'static str {
        match self {
            Self::WindowsX64 => "windows",
            Self::LinuxX64 => "linux",
        }
    }
    fn cache_name(self) -> &'static str {
        match self {
            Self::WindowsX64 => "windows-x64",
            Self::LinuxX64 => "linux-x64",
        }
    }
    fn executable(self) -> &'static str {
        match self {
            Self::WindowsX64 => "java.exe",
            Self::LinuxX64 => "java",
        }
    }
}
fn current_asset() -> Result<Asset, JavaRuntimeError> {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Ok(WINDOWS)
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Ok(LINUX)
    } else {
        Err(invalid(
            "managed Java download supports Windows/Linux x64; configure an installed Java 25 executable on this platform",
        ))
    }
}

pub async fn resolve_java_with_lifecycle(
    requested: &Path,
    cache: &Path,
    allow_download: bool,
    lifecycle: &Lifecycle,
    stop: &StopToken,
) -> Result<PathBuf, JavaRuntimeError> {
    let requested = requested.to_owned();
    let cache = cache.to_owned();
    let context = lifecycle.clone();
    tasks::run(
        lifecycle,
        stop,
        "resolve Java runtime",
        move |stop| async move {
            resolve_java_inner(&requested, &cache, allow_download, &context, &stop).await
        },
    )
    .await?
}

async fn resolve_java_inner(
    requested: &Path,
    cache: &Path,
    allow_download: bool,
    lifecycle: &Lifecycle,
    stop: &StopToken,
) -> Result<PathBuf, JavaRuntimeError> {
    stop.check()?;
    if !is_default_java(requested) {
        return probe_java_tracked(requested, lifecycle, stop)
            .await
            .map_err(|error| {
                if error.is_cancelled() || error.is_cleanup_failure() {
                    return error;
                }
                invalid(format!(
                    "configured Java executable {} is unavailable or is not Java 25: {error}",
                    requested.display()
                ))
            });
    }
    let mut failed_unicode_runtime = None;
    for candidate in default_java_candidates() {
        stop.check()?;
        let managed = if let Some(runtime) = managed_runtime_for(&candidate, cache)? {
            runtime
        } else {
            let java = match probe_java_tracked(&candidate, lifecycle, stop).await {
                Ok(java) => java,
                Err(error) if error.is_cancelled() || error.is_cleanup_failure() => {
                    return Err(error);
                }
                Err(JavaRuntimeError::Io(error))
                    if cfg!(unix) && error.kind() == io::ErrorKind::PermissionDenied =>
                {
                    continue;
                }
                Err(_) => break,
            };
            if let Some(runtime) = managed_runtime_for(&java, cache)? {
                runtime
            } else {
                return Ok(java);
            }
        };
        match validated_cached_runtime_tracked(&managed, current_asset()?, (), lifecycle, stop)
            .await
        {
            Ok((java, ())) => return Ok(java),
            Err(JavaRuntimeError::ProbeExit(_))
                if cfg!(windows) && !managed.as_os_str().as_encoded_bytes().is_ascii() =>
            {
                failed_unicode_runtime = Some(managed);
                break;
            }
            Err(error) => return Err(error),
        }
    }
    let asset = current_asset()?;
    let requested_cache = std::path::absolute(cache)?;
    let requested_runtime = requested_cache
        .join("25")
        .join(asset.platform.cache_name())
        .join(ARCHIVE_ROOT);
    reject_runtime_links(&requested_runtime)?;
    if failed_unicode_runtime.as_ref() != Some(&requested_runtime)
        && requested_runtime.try_exists()?
    {
        match validated_cached_runtime_tracked(&requested_runtime, asset, (), lifecycle, stop).await
        {
            Ok((java, ())) => return Ok(java),
            Err(JavaRuntimeError::ProbeExit(_))
                if cfg!(windows) && !requested_cache.as_os_str().as_encoded_bytes().is_ascii() => {}
            Err(error) => return Err(error),
        }
    }
    let cache = managed_cache_location(&requested_cache)?;
    let directory = cache.join("25").join(asset.platform.cache_name());
    let runtime = directory.join(ARCHIVE_ROOT);
    if runtime.try_exists()? {
        return Ok(
            validated_cached_runtime_tracked(&runtime, asset, (), lifecycle, stop)
                .await?
                .0,
        );
    }
    if !allow_download {
        return Err(invalid(
            "Java 25 was not found and automatic runtime downloads are disabled; configure java_executable",
        ));
    }
    install_managed_tracked(&cache, asset, lifecycle, stop).await
}

fn default_java_candidates() -> impl Iterator<Item = PathBuf> {
    let directories = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    let name = if cfg!(windows) { "java.exe" } else { "java" };
    directories
        .into_iter()
        .filter(|directory| directory.is_absolute())
        .map(move |directory| directory.join(name))
        .filter(|candidate| {
            let Ok(metadata) = candidate.metadata() else {
                return false;
            };
            if !metadata.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    return false;
                }
            }
            true
        })
}

fn managed_runtime_for(
    candidate: &Path,
    cache: &Path,
) -> Result<Option<PathBuf>, JavaRuntimeError> {
    let Ok(asset) = current_asset() else {
        return Ok(None);
    };
    let requested = std::path::absolute(cache)?;
    #[cfg(windows)]
    let fallback = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|local| local.is_absolute())
        .map(|local| local.join("Zerqon/cache/java"));
    #[cfg(not(windows))]
    let fallback = None;
    let absolute = std::path::absolute(candidate)?;
    let canonical = candidate.canonicalize().ok();
    for cache in std::iter::once(requested).chain(fallback) {
        let runtime = cache
            .join("25")
            .join(asset.platform.cache_name())
            .join(ARCHIVE_ROOT);
        if absolute.starts_with(&runtime)
            || canonical.as_ref().is_some_and(|candidate| {
                runtime
                    .canonicalize()
                    .is_ok_and(|runtime| candidate.starts_with(runtime))
            })
        {
            return Ok(Some(runtime));
        }
    }
    Ok(None)
}

fn managed_cache_location(cache: &Path) -> Result<PathBuf, JavaRuntimeError> {
    #[cfg(windows)]
    {
        let local_data = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        windows_cache_location(cache, local_data.as_deref())
    }
    #[cfg(not(windows))]
    {
        reject_runtime_links(cache)?;
        Ok(std::path::absolute(cache)?)
    }
}

#[cfg(windows)]
fn windows_cache_location(
    cache: &Path,
    local_data: Option<&Path>,
) -> Result<PathBuf, JavaRuntimeError> {
    reject_runtime_links(cache)?;
    let absolute = std::path::absolute(cache)?;
    if absolute.as_os_str().as_encoded_bytes().is_ascii() {
        return Ok(absolute);
    }
    let local_data = local_data
        .filter(|path| path.is_absolute() && path.as_os_str().as_encoded_bytes().is_ascii())
        .ok_or_else(|| invalid(
            "Windows Java 25 cannot reliably launch from this runtime cache path; set java_executable to Java 25 installed in an ASCII path",
        ))?;
    let fallback = local_data.join("Zerqon/cache/java");
    reject_runtime_links(&fallback)?;
    tracing::info!(
        "Using the per-user Java cache at {} for Windows launcher path compatibility",
        fallback.display()
    );
    Ok(fallback)
}

fn is_default_java(path: &Path) -> bool {
    path.components().count() == 1
        && path.to_str().is_some_and(|value| {
            if cfg!(windows) {
                value.eq_ignore_ascii_case("java") || value.eq_ignore_ascii_case("java.exe")
            } else {
                value == "java"
            }
        })
}

async fn validated_cached_runtime_tracked<T: Send + 'static>(
    runtime: &Path,
    asset: Asset,
    owner: T,
    lifecycle: &Lifecycle,
    stop: &StopToken,
) -> Result<(PathBuf, T), JavaRuntimeError> {
    stop.check()?;
    let checked = runtime.to_owned();
    let owner = tasks::blocking(lifecycle, stop, "validate Java runtime", move |stop| {
        stop.check()?;
        validate_cached_files(&checked, asset, &|| {
            stop.check().map_err(JavaRuntimeError::from)
        })?;
        stop.check()?;
        Ok::<_, JavaRuntimeError>(owner)
    })
    .await??;
    let binary = runtime.join("bin").join(asset.platform.executable());
    reject_runtime_links(&binary)?;
    let binary = binary.canonicalize()?;
    if !binary.starts_with(runtime.canonicalize()?) {
        return Err(invalid("managed Java binary is outside the runtime cache"));
    }
    let (resolved, owner) = probe_java_with_owner_tracked(&binary, owner, lifecycle, stop).await?;
    if !resolved
        .canonicalize()?
        .starts_with(runtime.canonicalize()?)
    {
        return Err(invalid(
            "managed Java executable resolved outside its cache",
        ));
    }
    Ok((resolved, owner))
}

fn validate_cached_files(
    runtime: &Path,
    asset: Asset,
    check: &dyn Fn() -> Result<(), JavaRuntimeError>,
) -> Result<(), JavaRuntimeError> {
    check()?;
    reject_runtime_links(runtime)?;
    let marker = runtime.join("zerqon-runtime.json");
    reject_runtime_links(&marker)?;
    if !fs::symlink_metadata(&marker)?.is_file() {
        return Err(invalid("managed runtime marker must be a regular file"));
    }
    let bytes = read_limited(&marker, 65536)?;
    let identity: Value = serde_json::from_slice(&bytes)?;
    if identity["release"] != RELEASE
        || identity["sha256"] != asset.sha256
        || identity["size"] != asset.size
        || identity["platform"] != asset.platform.cache_name()
    {
        return Err(invalid(
            "managed Java cache identity is invalid; existing files were preserved",
        ));
    }
    validate_runtime_tree(runtime, asset, true, check)
}

async fn install_managed_tracked(
    cache: &Path,
    asset: Asset,
    lifecycle: &Lifecycle,
    stop: &StopToken,
) -> Result<PathBuf, JavaRuntimeError> {
    stop.check()?;
    let cache = checked_runtime_directory(cache)?;
    let version_dir = cache.join("25");
    create_managed_directory(&version_dir)?;
    let directory = version_dir.join(asset.platform.cache_name());
    create_managed_directory(&directory)?;
    reject_runtime_links(&directory.join("install.lock"))?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("install.lock"))?;
    acquire_install_lock_tracked(&lock, Duration::from_secs(600), stop).await?;
    let runtime = directory.join(ARCHIVE_ROOT);
    if runtime.try_exists()? {
        let (java, _lock) =
            validated_cached_runtime_tracked(&runtime, asset, lock, lifecycle, stop).await?;
        return Ok(java);
    }
    tracing::info!(
        "Downloading Eclipse Temurin Java 25.0.5+8 for {} into the local cache",
        asset.platform.cache_name()
    );
    let client = trusted_client()?;
    let metadata = fetch_metadata(&client, asset, stop).await?;
    validate_metadata(&metadata, asset)?;
    let mut archive = tempfile::NamedTempFile::new_in(&directory)?;
    download_archive_tracked(
        &client,
        asset.url,
        asset.size,
        asset.sha256,
        &mut archive,
        stop,
    )
    .await?;
    let staging = tempfile::Builder::new()
        .prefix("install-")
        .tempdir_in(&directory)?;
    let output = staging.path().join("runtime");
    fs::create_dir(&output)?;
    let extraction_stop = stop.clone();
    finish_installation_tracked(
        Installation {
            archive,
            stage: staging,
            runtime,
            _lock: lock,
        },
        asset,
        move |archive, output, platform| {
            extract_archive(archive, output, platform, &|| {
                extraction_stop.check().map_err(JavaRuntimeError::from)
            })
        },
        lifecycle,
        stop,
    )
    .await
}

struct Installation {
    archive: tempfile::NamedTempFile,
    stage: tempfile::TempDir,
    runtime: PathBuf,
    _lock: File,
}

async fn finish_installation_tracked(
    installation: Installation,
    asset: Asset,
    extract: impl FnOnce(&Path, &Path, Platform) -> Result<(), JavaRuntimeError> + Send + 'static,
    lifecycle: &Lifecycle,
    stop: &StopToken,
) -> Result<PathBuf, JavaRuntimeError> {
    let installation = tasks::blocking(lifecycle, stop, "extract Java runtime", move |stop| {
        stop.check()?;
        let output = installation.stage.path().join("runtime");
        extract(installation.archive.path(), &output, asset.platform)?;
        stop.check()?;
        validate_runtime_tree(&output, asset, false, &|| {
            stop.check().map_err(JavaRuntimeError::from)
        })?;
        stop.check()?;
        Ok::<_, JavaRuntimeError>(installation)
    })
    .await??;
    let output = installation.stage.path().join("runtime");
    let executable = output.join("bin").join(asset.platform.executable());
    let (_, installation) =
        probe_java_with_owner_tracked(&executable, installation, lifecycle, stop).await?;
    stop.check()?;
    let mut marker = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(output.join("zerqon-runtime.json"))?;
    marker.write_all(&serde_json::to_vec_pretty(&json!({"release":RELEASE,"sha256":asset.sha256,"size":asset.size,"platform":asset.platform.cache_name(),"source":"Eclipse Adoptium"}))?)?;
    marker.sync_all()?;
    drop(marker);
    if installation.runtime.try_exists()? {
        return Err(invalid(
            "runtime destination appeared during installation; existing files were preserved",
        ));
    }
    fs::rename(&output, &installation.runtime)?;
    #[cfg(unix)]
    File::open(
        installation
            .runtime
            .parent()
            .ok_or_else(|| invalid("runtime destination has no parent"))?,
    )?
    .sync_all()?;
    let runtime = installation.runtime.clone();
    let (java, _installation) =
        validated_cached_runtime_tracked(&runtime, asset, installation, lifecycle, stop).await?;
    tracing::info!("Managed Java 25 is ready at {}", java.display());
    Ok(java)
}

fn create_managed_directory(path: &Path) -> Result<(), JavaRuntimeError> {
    reject_runtime_links(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !is_runtime_link(&metadata) => Ok(()),
        Ok(_) => Err(invalid(
            "managed Java cache path is not a regular directory",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => match fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(path)?;
                if metadata.is_dir() && !is_runtime_link(&metadata) {
                    Ok(())
                } else {
                    Err(invalid(
                        "managed Java cache path changed to a non-directory",
                    ))
                }
            }
            Err(error) => Err(error.into()),
        },
        Err(error) => Err(error.into()),
    }
}

fn is_runtime_link(metadata: &fs::Metadata) -> bool {
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

fn reject_runtime_links(path: &Path) -> Result<(), JavaRuntimeError> {
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
            Ok(metadata) if is_runtime_link(&metadata) => {
                return Err(invalid(
                    "managed runtime ancestry contains a link or reparse point",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn checked_runtime_directory(path: &Path) -> Result<PathBuf, JavaRuntimeError> {
    reject_runtime_links(path)?;
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for part in absolute.components() {
        current.push(part.as_os_str());
        if matches!(part, std::path::Component::Normal(_)) {
            create_managed_directory(&current)?;
        }
    }
    reject_runtime_links(&absolute)?;
    Ok(absolute.canonicalize()?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeInventory {
    schema: u32,
    release: String,
    platform: String,
    archive_root: String,
    archive_name: String,
    archive_size: u64,
    archive_sha256: String,
    files: Vec<RuntimeFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeFile {
    path: PathBuf,
    size: u64,
    sha256: String,
    executable: bool,
}

fn runtime_inventory(asset: Asset) -> Result<RuntimeInventory, JavaRuntimeError> {
    let bytes = match asset.platform {
        Platform::WindowsX64 => include_str!("assets/java-runtime-25.0.5-windows-x64.json"),
        Platform::LinuxX64 => include_str!("assets/java-runtime-25.0.5-linux-x64.json"),
    };
    let inventory: RuntimeInventory = serde_json::from_str(bytes)?;
    if inventory.schema != 1
        || inventory.release != RELEASE
        || inventory.platform != asset.platform.cache_name()
        || inventory.archive_root != ARCHIVE_ROOT
        || inventory.archive_name != asset.name
        || inventory.archive_size != asset.size
        || inventory.archive_sha256 != asset.sha256
        || inventory.files.is_empty()
        || inventory.files.len() > MAX_ENTRIES
    {
        return Err(invalid(
            "embedded runtime inventory differs from the pinned release",
        ));
    }
    Ok(inventory)
}

fn validate_runtime_tree(
    root: &Path,
    asset: Asset,
    marker_allowed: bool,
    check: &dyn Fn() -> Result<(), JavaRuntimeError>,
) -> Result<(), JavaRuntimeError> {
    check()?;
    reject_runtime_links(root)?;
    if !root.is_dir() {
        return Err(invalid("managed runtime must be a directory"));
    }
    let inventory = runtime_inventory(asset)?;
    let mut files = HashMap::new();
    let mut directories = HashSet::new();
    let mut expected_total = 0_u64;
    for file in &inventory.files {
        check()?;
        if file.path.is_absolute()
            || file
                .path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
            || file.size > MAX_FILE
            || file.sha256.len() != 64
            || !file
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || files.insert(file.path.clone(), file).is_some()
        {
            return Err(invalid("invalid embedded runtime file inventory"));
        }
        expected_total = expected_total
            .checked_add(file.size)
            .filter(|total| *total <= MAX_UNPACKED)
            .ok_or_else(|| invalid("embedded runtime content exceeds its bound"))?;
        let mut parent = file.path.parent();
        while let Some(directory) = parent.filter(|path| !path.as_os_str().is_empty()) {
            directories.insert(directory.to_owned());
            parent = directory.parent();
        }
    }
    let mut pending = vec![root.to_owned()];
    let mut count = 0usize;
    let mut buffer = [0_u8; 65_536];
    while let Some(directory) = pending.pop() {
        check()?;
        for entry in fs::read_dir(directory)? {
            check()?;
            let entry = entry?;
            count += 1;
            if count > MAX_ENTRIES {
                return Err(invalid("managed runtime contains too many entries"));
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if is_runtime_link(&metadata) {
                return Err(invalid(
                    "managed runtime contains a link or an escaping library path",
                ));
            }
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| invalid("runtime entry escaped root"))?;
            if metadata.is_dir() {
                if !directories.contains(relative) {
                    return Err(invalid(
                        "managed runtime contains an unrecognized directory; files were preserved",
                    ));
                }
                pending.push(path);
            } else if metadata.is_file() {
                if marker_allowed && relative == Path::new("zerqon-runtime.json") {
                    if metadata.len() > 65_536 {
                        return Err(invalid("runtime metadata exceeds its bound"));
                    }
                    continue;
                }
                let expected = files.remove(relative).ok_or_else(|| {
                    invalid("managed runtime contains an unrecognized file; files were preserved")
                })?;
                let mut input = File::open(&path)?;
                let opened = input.metadata()?;
                if !opened.is_file()
                    || metadata.len() != expected.size
                    || opened.len() != expected.size
                {
                    return Err(invalid(format!(
                        "managed runtime file size differs from pinned content: {}",
                        expected.path.display()
                    )));
                }
                #[cfg(unix)]
                if asset.platform == Platform::LinuxX64 {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = opened.permissions().mode();
                    if (mode & 0o111 != 0) != expected.executable || mode & 0o6000 != 0 {
                        return Err(invalid(
                            "managed runtime file execution permissions differ from pinned content",
                        ));
                    }
                }
                #[cfg(not(unix))]
                let _ = expected.executable;
                validate_runtime_contents(&mut input, expected, &mut buffer, check)?;
            } else {
                return Err(invalid("managed runtime contains a special file"));
            }
        }
    }
    if !files.is_empty() {
        return Err(invalid(
            "managed runtime is missing pinned files; existing files were preserved",
        ));
    }
    Ok(())
}

fn validate_runtime_contents(
    input: &mut impl Read,
    expected: &RuntimeFile,
    buffer: &mut [u8; 65_536],
    check: &dyn Fn() -> Result<(), JavaRuntimeError>,
) -> Result<(), JavaRuntimeError> {
    let mut digest = Context::new(&SHA256);
    let mut read = 0_u64;
    loop {
        check()?;
        let count = input.read(buffer)?;
        if count == 0 {
            break;
        }
        read += count as u64;
        if read > expected.size {
            return Err(invalid("managed runtime file grew beyond pinned size"));
        }
        digest.update(&buffer[..count]);
    }
    if read != expected.size || hex(digest.finish().as_ref()) != expected.sha256 {
        return Err(invalid(format!(
            "managed runtime file contents differ from pinned content: {}",
            expected.path.display()
        )));
    }
    Ok(())
}

async fn acquire_install_lock_tracked(
    lock: &File,
    timeout: Duration,
    stop: &StopToken,
) -> Result<(), JavaRuntimeError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        stop.check()?;
        match lock.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            Err(std::fs::TryLockError::WouldBlock) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(invalid("timed out waiting for managed Java installation"));
                }
                tasks::cancellable(stop, async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok::<_, JavaRuntimeError>(())
                })
                .await?;
            }
        }
    }
}

fn trusted_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && matches!(
            url.host_str(),
            Some(
                "api.adoptium.net"
                    | "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
}
fn trusted_client() -> Result<reqwest::Client, JavaRuntimeError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(300))
        .user_agent("Zerqon/0.0.1 (+https://github.com/teighto/Zerqon-Core)")
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || !trusted_url(attempt.url()) {
                attempt.error("untrusted runtime download redirect")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|error| invalid(error.without_url().to_string()))
}
async fn fetch_metadata(
    client: &reqwest::Client,
    asset: Asset,
    stop: &StopToken,
) -> Result<Value, JavaRuntimeError> {
    tasks::cancellable(stop, async {
    let url = format!(
        "https://api.adoptium.net/v3/assets/release_name/eclipse/jdk-25.0.5%2B8?architecture=x64&heap_size=normal&image_type=jre&jvm_impl=hotspot&os={}&project=jdk",
        asset.platform.os()
    );
    let mut response = client
        .get(&url)
        .send()
        .await
        .map_err(|error| invalid(error.without_url().to_string()))?
        .error_for_status()
        .map_err(|error| invalid(error.without_url().to_string()))?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| invalid(error.without_url().to_string()))?
    {
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err(invalid("Adoptium metadata exceeds1MiB"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
    }).await
}
fn validate_metadata(value: &Value, asset: Asset) -> Result<(), JavaRuntimeError> {
    if value["release_name"] != RELEASE
        || value["version_data"]["major"] != 25
        || value["version_data"]["security"] != 5
        || value["version_data"]["build"] != 8
    {
        return Err(invalid(
            "Adoptium release metadata differs from pinned Java25 build",
        ));
    }
    let binaries = value["binaries"]
        .as_array()
        .ok_or_else(|| invalid("Adoptium binary list is missing"))?;
    let matches: Vec<_> = binaries
        .iter()
        .filter(|binary| {
            binary["architecture"] == "x64"
                && binary["os"] == asset.platform.os()
                && binary["image_type"] == "jre"
                && binary["jvm_impl"] == "hotspot"
        })
        .collect();
    if matches.len() != 1 {
        return Err(invalid("Adoptium platform binary is missing or ambiguous"));
    }
    let package = &matches[0]["package"];
    if package["name"] != asset.name
        || package["size"] != asset.size
        || package["checksum"] != asset.sha256
        || package["link"] != asset.url
    {
        return Err(invalid(
            "Adoptium asset metadata differs from pinned filename/size/SHA256/URL",
        ));
    }
    Ok(())
}

async fn download_archive_tracked(
    client: &reqwest::Client,
    url: &str,
    size: u64,
    sha256: &str,
    target: &mut tempfile::NamedTempFile,
    stop: &StopToken,
) -> Result<(), JavaRuntimeError> {
    stop.check()?;
    if size > 128 * 1024 * 1024 {
        return Err(invalid("runtime archive exceeds128MiB"));
    }
    let mut response = tasks::cancellable(stop, async {
        client
            .get(url)
            .send()
            .await
            .map_err(|error| invalid(error.without_url().to_string()))?
            .error_for_status()
            .map_err(|error| invalid(error.without_url().to_string()))
    })
    .await?;
    if response
        .content_length()
        .is_some_and(|length| length != size)
    {
        return Err(invalid("runtime Content-Length differs from pinned size"));
    }
    let mut file = tokio::fs::File::from_std(target.reopen()?);
    let mut count = 0u64;
    let mut digest = Context::new(&SHA256);
    while let Some(bytes) = tasks::cancellable(stop, async {
        response
            .chunk()
            .await
            .map_err(|error| invalid(error.without_url().to_string()))
    })
    .await?
    {
        count = count
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| invalid("runtime size overflow"))?;
        if count > size {
            return Err(invalid("runtime download exceeds pinned size"));
        }
        digest.update(&bytes);
        file.write_all(&bytes).await?;
        file.flush().await?;
        stop.check()?;
    }
    if count != size || hex(digest.finish().as_ref()) != sha256 {
        return Err(invalid("runtime archive failed pinned length/SHA256 check"));
    }
    file.sync_all().await?;
    stop.check()?;
    Ok(())
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn component_ok(part: &str) -> bool {
    let base = part
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    !part.is_empty()
        && !matches!(part, "." | "..")
        && !part.contains(['\\', ':', '<', '>', '"', '|', '?', '*'])
        && !part.ends_with(['.', ' '])
        && !part.chars().any(char::is_control)
        && !matches!(
            base.as_str(),
            "CON" | "NUL" | "AUX" | "PRN" | "CONIN$" | "CONOUT$"
        )
        && !(base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && matches!(base.as_bytes()[3], b'1'..=b'9'))
}
fn archive_path(name: &str) -> Result<PathBuf, JavaRuntimeError> {
    let parts: Vec<_> = name.trim_end_matches('/').split('/').collect();
    if parts.first() != Some(&ARCHIVE_ROOT) || parts.iter().any(|part| !component_ok(part)) {
        return Err(invalid("runtime archive has an unsafe or unexpected path"));
    }
    Ok(parts.iter().skip(1).collect())
}
fn link_target(source: &Path, target: &str) -> Result<PathBuf, JavaRuntimeError> {
    if !source.starts_with("legal") || target.starts_with('/') || target.contains('\\') {
        return Err(invalid(
            "only archive-internal legal file links can be materialized",
        ));
    }
    let mut components: Vec<String> = source
        .parent()
        .ok_or_else(|| invalid("link needs parent"))?
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    for part in target.split('/') {
        match part {
            "." => {}
            ".." => {
                if components.pop().is_none() {
                    return Err(invalid("runtime link escapes root"));
                }
            }
            _ => {
                if !component_ok(part) {
                    return Err(invalid("invalid runtime link target"));
                }
                components.push(part.into());
            }
        }
    }
    let target: PathBuf = components.into_iter().collect();
    if !target.starts_with("legal") {
        return Err(invalid("runtime link target is outside legal directory"));
    }
    Ok(target)
}

fn extract_archive(
    archive: &Path,
    output: &Path,
    platform: Platform,
    check: &dyn Fn() -> Result<(), JavaRuntimeError>,
) -> Result<(), JavaRuntimeError> {
    check()?;
    match platform {
        Platform::WindowsX64 => extract_zip(archive, output, check),
        Platform::LinuxX64 => extract_tar(archive, output, check),
    }
}
fn write_entry(
    reader: &mut impl Read,
    path: &Path,
    size: u64,
    total: &mut u64,
    mode: u32,
    check: &dyn Fn() -> Result<(), JavaRuntimeError>,
) -> Result<(), JavaRuntimeError> {
    check()?;
    if size > MAX_FILE || *total > MAX_UNPACKED.saturating_sub(size) {
        return Err(invalid("runtime archive content exceeds limits"));
    }
    *total += size;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("runtime entry needs parent"))?;
    fs::create_dir_all(parent)?;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    let mut reader = reader.take(size + 1);
    let mut buffer = [0; 65_536];
    let mut copied = 0_u64;
    loop {
        check()?;
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        file.write_all(&buffer[..count])?;
        copied += count as u64;
    }
    if copied != size {
        return Err(invalid("runtime entry length mismatch"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(if mode & 0o111 != 0 {
            0o755
        } else {
            0o644
        }))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    check()?;
    file.sync_all()?;
    check()?;
    Ok(())
}
fn extract_zip(
    path: &Path,
    output: &Path,
    check: &dyn Fn() -> Result<(), JavaRuntimeError>,
) -> Result<(), JavaRuntimeError> {
    check()?;
    let mut archive = zip::ZipArchive::new(File::open(path)?)?;
    if archive.len() > MAX_ENTRIES {
        return Err(invalid("too many runtime ZIP entries"));
    }
    let mut total = 0;
    let mut seen = HashSet::new();
    for index in 0..archive.len() {
        check()?;
        let mut entry = archive.by_index(index)?;
        let name = entry.name()?.into_owned();
        let relative = archive_path(&name)?;
        let mode = entry.unix_mode().unwrap_or(0);
        if mode & 0o170000 == 0o120000 {
            return Err(invalid("runtime ZIP symlinks are not allowed"));
        }
        if relative.as_os_str().is_empty() {
            if !entry.is_dir() {
                return Err(invalid("runtime root must be directory"));
            }
            continue;
        }
        if !seen.insert(relative.to_string_lossy().to_ascii_lowercase()) {
            return Err(invalid("duplicate or case-colliding runtime ZIP entry"));
        }
        if entry.is_dir() {
            fs::create_dir_all(output.join(relative))?;
        } else {
            let size = entry.size();
            write_entry(
                &mut entry,
                &output.join(relative),
                size,
                &mut total,
                mode,
                check,
            )?;
        }
    }
    Ok(())
}
struct LimitedReader<R> {
    inner: R,
    left: u64,
}
impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            return Err(io::Error::other("decompressed runtime exceeds512MiB"));
        }
        let count = buffer.len().min(self.left as usize);
        let read = self.inner.read(&mut buffer[..count])?;
        self.left -= read as u64;
        Ok(read)
    }
}
fn extract_tar(
    path: &Path,
    output: &Path,
    check: &dyn Fn() -> Result<(), JavaRuntimeError>,
) -> Result<(), JavaRuntimeError> {
    check()?;
    let gzip = flate2::read::GzDecoder::new(File::open(path)?);
    let mut archive = tar::Archive::new(LimitedReader {
        inner: gzip,
        left: MAX_UNPACKED,
    });
    let mut total = 0;
    let mut seen = HashSet::new();
    let mut links = HashMap::new();
    for (index, entry) in archive.entries()?.enumerate() {
        check()?;
        if index >= MAX_ENTRIES {
            return Err(invalid("too many runtime tar entries"));
        }
        let mut entry = entry?;
        let name = std::str::from_utf8(&entry.path_bytes())
            .map_err(|_| invalid("runtime tar path is not UTF-8"))?
            .to_owned();
        let relative = archive_path(&name)?;
        let kind = entry.header().entry_type();
        if relative.as_os_str().is_empty() {
            if !kind.is_dir() {
                return Err(invalid("runtime root must be directory"));
            }
            continue;
        }
        if !seen.insert(relative.clone()) {
            return Err(invalid("duplicate runtime tar entry"));
        }
        if kind.is_dir() {
            fs::create_dir_all(output.join(relative))?;
        } else if kind.is_file() {
            let size = entry.size();
            let mode = entry.header().mode()?;
            write_entry(
                &mut entry,
                &output.join(relative),
                size,
                &mut total,
                mode,
                check,
            )?;
        } else if kind.is_symlink() {
            let name = entry
                .link_name()?
                .ok_or_else(|| invalid("runtime symlink has no target"))?;
            let target = name
                .to_str()
                .ok_or_else(|| invalid("runtime link target is not UTF-8"))?;
            let target = link_target(&relative, target)?;
            links.insert(relative, target);
        } else {
            return Err(invalid(
                "runtime tar hard links and special entries are not allowed",
            ));
        }
    }
    let mut remaining = archive.into_inner();
    let mut buffer = [0; 65_536];
    loop {
        check()?;
        if remaining.read(&mut buffer)? == 0 {
            break;
        }
    }
    while !links.is_empty() {
        check()?;
        let before = links.len();
        let pending: Vec<_> = links.keys().cloned().collect();
        for path in pending {
            check()?;
            let target = &links[&path];
            let target_path = output.join(target);
            if target_path.is_file() {
                let metadata = fs::symlink_metadata(&target_path)?;
                if metadata.file_type().is_symlink() {
                    return Err(invalid("unexpected installed symlink"));
                }
                write_entry(
                    &mut File::open(&target_path)?,
                    &output.join(&path),
                    metadata.len(),
                    &mut total,
                    0o644,
                    check,
                )?;
                links.remove(&path);
            }
        }
        if links.len() == before {
            return Err(invalid("cyclic or missing runtime license link target"));
        }
    }
    Ok(())
}

fn parse_probe(text: &str) -> Result<PathBuf, JavaRuntimeError> {
    let home = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("java.home = "))
        .filter(|home| !home.is_empty())
        .ok_or_else(|| invalid("Java runtime did not report its home"))?;
    let version = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("java.specification.version = "));
    if version != Some("25") {
        return Err(invalid("Java 25 is required"));
    }
    Ok(Path::new(home)
        .join("bin")
        .join(if cfg!(windows) { "java.exe" } else { "java" }))
}

async fn probe_java_tracked(
    executable: &Path,
    lifecycle: &Lifecycle,
    stop: &StopToken,
) -> Result<PathBuf, JavaRuntimeError> {
    Ok(
        probe_java_with_owner_tracked(executable, (), lifecycle, stop)
            .await?
            .0,
    )
}

async fn probe_java_with_owner_tracked<T: Send + 'static>(
    executable: &Path,
    owner: T,
    lifecycle: &Lifecycle,
    stop: &StopToken,
) -> Result<(PathBuf, T), JavaRuntimeError> {
    let executable = executable.to_owned();
    tasks::run(
        lifecycle,
        stop,
        "Java runtime probe",
        move |stop| async move {
            let java = run_probe(&executable, &stop).await?;
            Ok::<_, JavaRuntimeError>((java, owner))
        },
    )
    .await?
}

async fn run_probe(executable: &Path, stop: &StopToken) -> Result<PathBuf, JavaRuntimeError> {
    stop.check()?;
    let mut child = tokio::process::Command::new(executable)
        .args([
            "-Dfile.encoding=UTF-8",
            "-Dsun.stdout.encoding=UTF-8",
            "-Dsun.stderr.encoding=UTF-8",
            "-XshowSettings:properties",
            "-version",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let run = async {
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| invalid("Java probe has no stderr"))?;
        let mut bytes = Vec::new();
        stderr.take(65537).read_to_end(&mut bytes).await?;
        if bytes.len() > 65536 {
            return Err(invalid("Java probe output exceeds64KiB"));
        }
        let status = child.wait().await?;
        if !status.success() {
            return Err(JavaRuntimeError::ProbeExit(status));
        }
        let text =
            std::str::from_utf8(&bytes).map_err(|_| invalid("Java probe text is not UTF-8"))?;
        let resolved = parse_probe(text)?;
        if !resolved.is_file() {
            return Err(invalid("Java reported a nonexistent runtime executable"));
        }
        Ok(resolved)
    };
    let result = tokio::select! {
        biased;
        _ = stop.cancelled() => Err(Stopped.into()),
        result = tokio::time::timeout(Duration::from_secs(15), tasks::catch_panic(run)) => {
            match result {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => Err(error.into()),
                Err(_) => Err(invalid("Java runtime probe timed out")),
            }
        },
    };
    tasks::terminate_child(&mut child).await?;
    result
}
fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>, JavaRuntimeError> {
    let file = File::open(path)?;
    if file.metadata()?.len() > limit {
        return Err(invalid("runtime metadata file exceeds limit"));
    }
    let mut bytes = Vec::new();
    Read::take(file, limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("runtime metadata grew beyond limit"));
    }
    Ok(bytes)
}
