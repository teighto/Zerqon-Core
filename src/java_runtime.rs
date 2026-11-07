use aws_lc_rs::digest::{Context, SHA256};
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
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error(transparent)]
    Task(#[from] tokio::task::JoinError),
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

pub async fn resolve_java(
    requested: &Path,
    cache: &Path,
    allow_download: bool,
) -> Result<PathBuf, JavaRuntimeError> {
    match probe_java(requested).await {
        Ok(java) => return Ok(java),
        Err(error) if !is_default_java(requested) => {
            return Err(invalid(format!(
                "configured Java executable {} is unavailable or is not Java 25: {error}",
                requested.display()
            )));
        }
        Err(_) => {}
    }
    let asset = current_asset()?;
    let directory = cache.join("25").join(asset.platform.cache_name());
    let runtime = directory.join(ARCHIVE_ROOT);
    if runtime.try_exists()? {
        return validated_cached_runtime(&runtime, asset).await;
    }
    if !allow_download {
        return Err(invalid(
            "Java 25 was not found and automatic runtime downloads are disabled; configure java_executable",
        ));
    }
    install_managed(cache, asset).await
}

fn is_default_java(path: &Path) -> bool {
    path.components().count() == 1
        && path.to_str().is_some_and(|value| {
            value == "java" || (cfg!(windows) && value.eq_ignore_ascii_case("java.exe"))
        })
}

async fn validated_cached_runtime(
    runtime: &Path,
    asset: Asset,
) -> Result<PathBuf, JavaRuntimeError> {
    reject_runtime_links(runtime)?;
    validate_runtime_tree(runtime)?;
    let marker = runtime.join("zerqon-runtime.json");
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
    let binary = runtime.join("bin").join(asset.platform.executable());
    reject_runtime_links(&binary)?;
    let binary = binary.canonicalize()?;
    if !binary.starts_with(runtime.canonicalize()?) {
        return Err(invalid("managed Java binary is outside the runtime cache"));
    }
    let resolved = probe_java(&binary).await?;
    if !resolved
        .canonicalize()?
        .starts_with(runtime.canonicalize()?)
    {
        return Err(invalid(
            "managed Java executable resolved outside its cache",
        ));
    }
    Ok(resolved)
}

async fn install_managed(cache: &Path, asset: Asset) -> Result<PathBuf, JavaRuntimeError> {
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
    acquire_install_lock(&lock, Duration::from_secs(600)).await?;
    let runtime = directory.join(ARCHIVE_ROOT);
    if runtime.try_exists()? {
        return validated_cached_runtime(&runtime, asset).await;
    }
    tracing::info!(
        "Downloading Eclipse Temurin Java 25.0.5+8 for {} into the local cache",
        asset.platform.cache_name()
    );
    let client = trusted_client()?;
    let metadata = fetch_metadata(&client, asset).await?;
    validate_metadata(&metadata, asset)?;
    let mut archive = tempfile::NamedTempFile::new_in(&directory)?;
    download_archive(&client, asset.url, asset.size, asset.sha256, &mut archive).await?;
    let staging = tempfile::Builder::new()
        .prefix("install-")
        .tempdir_in(&directory)?;
    let output = staging.path().join("runtime");
    fs::create_dir(&output)?;
    let archive_path = archive.path().to_owned();
    let output_path = output.clone();
    tokio::task::spawn_blocking(move || {
        extract_archive(&archive_path, &output_path, asset.platform)
    })
    .await??;
    let executable = output.join("bin").join(asset.platform.executable());
    probe_java(&executable).await?;
    let mut marker = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(output.join("zerqon-runtime.json"))?;
    marker.write_all(&serde_json::to_vec_pretty(&json!({"release":RELEASE,"sha256":asset.sha256,"size":asset.size,"platform":asset.platform.cache_name(),"source":"Eclipse Adoptium"}))?)?;
    marker.sync_all()?;
    drop(marker);
    if runtime.try_exists()? {
        return Err(invalid(
            "runtime destination appeared during installation; existing files were preserved",
        ));
    }
    fs::rename(&output, &runtime)?;
    #[cfg(unix)]
    File::open(&directory)?.sync_all()?;
    let java = validated_cached_runtime(&runtime, asset).await?;
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

fn validate_runtime_tree(root: &Path) -> Result<(), JavaRuntimeError> {
    if !root.is_dir() {
        return Err(invalid("managed runtime must be a directory"));
    }
    let canonical = root.canonicalize()?;
    let mut pending = vec![root.to_owned()];
    let mut count = 0usize;
    let mut bytes = 0u64;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            count += 1;
            if count > MAX_ENTRIES {
                return Err(invalid("managed runtime contains too many entries"));
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if is_runtime_link(&metadata) || !entry.path().canonicalize()?.starts_with(&canonical) {
                return Err(invalid(
                    "managed runtime contains a link or an escaping library path",
                ));
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                if metadata.len() > MAX_FILE || bytes > MAX_UNPACKED.saturating_sub(metadata.len())
                {
                    return Err(invalid("managed runtime exceeds content limits"));
                }
                bytes += metadata.len();
            } else {
                return Err(invalid("managed runtime contains a special file"));
            }
        }
    }
    Ok(())
}

async fn acquire_install_lock(lock: &File, timeout: Duration) -> Result<(), JavaRuntimeError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            Err(std::fs::TryLockError::WouldBlock) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(invalid("timed out waiting for managed Java installation"));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
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
async fn fetch_metadata(client: &reqwest::Client, asset: Asset) -> Result<Value, JavaRuntimeError> {
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

async fn download_archive(
    client: &reqwest::Client,
    url: &str,
    size: u64,
    sha256: &str,
    target: &mut tempfile::NamedTempFile,
) -> Result<(), JavaRuntimeError> {
    if size > 128 * 1024 * 1024 {
        return Err(invalid("runtime archive exceeds128MiB"));
    }
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|error| invalid(error.without_url().to_string()))?
        .error_for_status()
        .map_err(|error| invalid(error.without_url().to_string()))?;
    if response
        .content_length()
        .is_some_and(|length| length != size)
    {
        return Err(invalid("runtime Content-Length differs from pinned size"));
    }
    let mut file = tokio::fs::File::from_std(target.reopen()?);
    let mut count = 0u64;
    let mut digest = Context::new(&SHA256);
    while let Some(bytes) = response
        .chunk()
        .await
        .map_err(|error| invalid(error.without_url().to_string()))?
    {
        count = count
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| invalid("runtime size overflow"))?;
        if count > size {
            return Err(invalid("runtime download exceeds pinned size"));
        }
        digest.update(&bytes);
        file.write_all(&bytes).await?;
    }
    if count != size || hex(digest.finish().as_ref()) != sha256 {
        return Err(invalid("runtime archive failed pinned length/SHA256 check"));
    }
    file.sync_all().await?;
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
) -> Result<(), JavaRuntimeError> {
    match platform {
        Platform::WindowsX64 => extract_zip(archive, output),
        Platform::LinuxX64 => extract_tar(archive, output),
    }
}
fn write_entry(
    reader: &mut impl Read,
    path: &Path,
    size: u64,
    total: &mut u64,
    mode: u32,
) -> Result<(), JavaRuntimeError> {
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
    let copied = io::copy(&mut reader.take(size + 1), &mut file)?;
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
    file.sync_all()?;
    Ok(())
}
fn extract_zip(path: &Path, output: &Path) -> Result<(), JavaRuntimeError> {
    let mut archive = zip::ZipArchive::new(File::open(path)?)?;
    if archive.len() > MAX_ENTRIES {
        return Err(invalid("too many runtime ZIP entries"));
    }
    let mut total = 0;
    let mut seen = HashSet::new();
    for index in 0..archive.len() {
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
            write_entry(&mut entry, &output.join(relative), size, &mut total, mode)?;
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
fn extract_tar(path: &Path, output: &Path) -> Result<(), JavaRuntimeError> {
    let gzip = flate2::read::GzDecoder::new(File::open(path)?);
    let mut archive = tar::Archive::new(LimitedReader {
        inner: gzip,
        left: MAX_UNPACKED,
    });
    let mut total = 0;
    let mut seen = HashSet::new();
    let mut links = HashMap::new();
    for (index, entry) in archive.entries()?.enumerate() {
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
            write_entry(&mut entry, &output.join(relative), size, &mut total, mode)?;
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
    io::copy(&mut archive.into_inner(), &mut io::sink())?;
    while !links.is_empty() {
        let before = links.len();
        let pending: Vec<_> = links.keys().cloned().collect();
        for path in pending {
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
async fn probe_java(executable: &Path) -> Result<PathBuf, JavaRuntimeError> {
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
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| invalid("Java probe has no stderr"))?;
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let mut bytes = Vec::new();
        stderr.take(65537).read_to_end(&mut bytes).await?;
        if bytes.len() > 65536 {
            return Err(invalid("Java probe output exceeds64KiB"));
        }
        if !child.wait().await?.success() {
            return Err(invalid("Java probe exited unsuccessfully"));
        }
        let text =
            std::str::from_utf8(&bytes).map_err(|_| invalid("Java probe text is not UTF-8"))?;
        let resolved = parse_probe(text)?;
        if !resolved.is_file() {
            return Err(invalid("Java reported a nonexistent runtime executable"));
        }
        Ok(resolved)
    })
    .await;
    match result {
        Ok(Ok(path)) => Ok(path),
        Ok(Err(error)) => {
            let _ = child.kill().await;
            Err(error)
        }
        Err(_) => {
            child.kill().await?;
            Err(invalid("Java runtime probe timed out"))
        }
    }
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
