#[path = "../src/lifecycle.rs"]
#[allow(dead_code)]
mod lifecycle;
#[path = "../src/managed_tasks.rs"]
#[allow(dead_code)]
mod managed_tasks;

include!("../src/java_runtime.rs");

#[cfg(test)]
mod tests {
    use super::*;

    pub async fn resolve_java(
        requested: &Path,
        cache: &Path,
        allow_download: bool,
    ) -> Result<PathBuf, JavaRuntimeError> {
        let lifecycle = Lifecycle::new();
        resolve_java_with_lifecycle(
            requested,
            cache,
            allow_download,
            &lifecycle,
            &lifecycle.token(),
        )
        .await
    }
    async fn validated_cached_runtime(
        runtime: &Path,
        asset: Asset,
    ) -> Result<PathBuf, JavaRuntimeError> {
        let lifecycle = Lifecycle::new();
        let (java, ()) =
            validated_cached_runtime_tracked(runtime, asset, (), &lifecycle, &lifecycle.token())
                .await?;
        Ok(java)
    }
    async fn install_managed(cache: &Path, asset: Asset) -> Result<PathBuf, JavaRuntimeError> {
        let lifecycle = Lifecycle::new();
        let context = lifecycle.clone();
        let cache = cache.to_owned();
        tasks::run(
            &lifecycle,
            &lifecycle.token(),
            "install Java runtime",
            move |stop| async move { install_managed_tracked(&cache, asset, &context, &stop).await },
        )
        .await?
    }
    async fn finish_installation(
        installation: Installation,
        asset: Asset,
        extract: impl FnOnce(&Path, &Path, Platform) -> Result<(), JavaRuntimeError> + Send + 'static,
    ) -> Result<PathBuf, JavaRuntimeError> {
        let lifecycle = Lifecycle::new();
        let context = lifecycle.clone();
        tasks::run(
            &lifecycle,
            &lifecycle.token(),
            "finish Java installation",
            move |stop| async move {
                finish_installation_tracked(installation, asset, extract, &context, &stop).await
            },
        )
        .await?
    }
    async fn acquire_install_lock(lock: &File, timeout: Duration) -> Result<(), JavaRuntimeError> {
        acquire_install_lock_tracked(lock, timeout, &Lifecycle::new().token()).await
    }
    async fn download_archive(
        client: &reqwest::Client,
        url: &str,
        size: u64,
        sha256: &str,
        target: &mut tempfile::NamedTempFile,
    ) -> Result<(), JavaRuntimeError> {
        download_archive_tracked(client, url, size, sha256, target, &Lifecycle::new().token()).await
    }
    async fn probe_java(executable: &Path) -> Result<PathBuf, JavaRuntimeError> {
        let (java, ()) = probe_java_with_owner(executable, ()).await?;
        Ok(java)
    }
    async fn probe_java_with_owner<T: Send + 'static>(
        executable: &Path,
        owner: T,
    ) -> Result<(PathBuf, T), JavaRuntimeError> {
        let lifecycle = Lifecycle::new();
        probe_java_with_owner_tracked(executable, owner, &lifecycle, &lifecycle.token()).await
    }

    use serde_json::json;
    use std::sync::OnceLock;

    fn validate_cached_files(path: &Path, asset: Asset) -> Result<(), JavaRuntimeError> {
        super::validate_cached_files(path, asset, &|| Ok(()))
    }
    fn extract_archive(
        path: &Path,
        output: &Path,
        platform: Platform,
    ) -> Result<(), JavaRuntimeError> {
        super::extract_archive(path, output, platform, &|| Ok(()))
    }
    fn extract_zip(path: &Path, output: &Path) -> Result<(), JavaRuntimeError> {
        super::extract_zip(path, output, &|| Ok(()))
    }
    fn extract_tar(path: &Path, output: &Path) -> Result<(), JavaRuntimeError> {
        super::extract_tar(path, output, &|| Ok(()))
    }
    fn write_entry(
        reader: &mut impl Read,
        path: &Path,
        size: u64,
        total: &mut u64,
        mode: u32,
    ) -> Result<(), JavaRuntimeError> {
        super::write_entry(reader, path, size, total, mode, &|| Ok(()))
    }

    fn cached_probe_fixture(cache: &Path, mode: &str) -> PathBuf {
        static FIXTURE: OnceLock<tempfile::TempDir> = OnceLock::new();
        let fixture = FIXTURE.get_or_init(|| {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("probe.rs");
            fs::write(&source, r#"fn main() {
                let executable = std::env::current_exe().unwrap();
                let home = executable.parent().unwrap().parent().unwrap();
                std::fs::write(home.join("executed"), b"yes").unwrap();
                let mode = std::fs::read_to_string(home.join("probe-mode")).unwrap();
                if mode == "exit" { std::process::exit(2); }
                if mode == "wait" {
                    let alive = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(home.join("alive.lock")).unwrap();
                    alive.lock().unwrap();
                    std::fs::write(home.join("probe.pid"), std::process::id().to_string()).unwrap();
                    loop { std::thread::sleep(std::time::Duration::from_millis(10)); }
                }
                eprintln!("java.home = {}", home.display());
                eprintln!("java.specification.version = {}", if mode == "old" { "21" } else { "25" });
            }"#).unwrap();
            let compiled = std::process::Command::new("rustc")
                .arg("--edition=2024")
                .arg(&source)
                .arg("-o")
                .arg(directory.path().join(current_asset().unwrap().platform.executable()))
                .output()
                .unwrap();
            assert!(compiled.status.success(), "{}", String::from_utf8_lossy(&compiled.stderr));
            directory
        });
        let asset = current_asset().unwrap();
        let runtime = cache
            .join("25")
            .join(asset.platform.cache_name())
            .join(ARCHIVE_ROOT);
        fs::create_dir_all(runtime.join("bin")).unwrap();
        fs::copy(
            fixture.path().join(asset.platform.executable()),
            runtime.join("bin").join(asset.platform.executable()),
        )
        .unwrap();
        fs::write(runtime.join("probe-mode"), mode).unwrap();
        fs::write(
            runtime.join("zerqon-runtime.json"),
            serde_json::to_vec(&json!({
                "release": RELEASE, "sha256": asset.sha256, "size": asset.size,
                "platform": asset.platform.cache_name()
            }))
            .unwrap(),
        )
        .unwrap();
        runtime
    }

    fn resolver_child_root() -> Option<PathBuf> {
        std::env::var_os("ZERQON_RUNTIME_TEST_ROOT").map(PathBuf::from)
    }

    fn official_fixture(cache: &Path) -> PathBuf {
        let asset = current_asset().unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let archive = std::env::var_os("ZERQON_TEST_JRE_ARCHIVE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                root.join(match asset.platform {
                    Platform::WindowsX64 => ".local/adoptium-windows25.zip",
                    Platform::LinuxX64 => ".local/adoptium-linux25.tar.gz",
                })
            });
        let mut input =
            File::open(&archive).expect("verified official runtime archive fixture is required");
        assert_eq!(input.metadata().unwrap().len(), asset.size);
        let mut digest = Context::new(&SHA256);
        let mut buffer = [0_u8; 65_536];
        loop {
            let count = input.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
        assert_eq!(
            digest
                .finish()
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            asset.sha256
        );
        let runtime = cache
            .join("25")
            .join(asset.platform.cache_name())
            .join(ARCHIVE_ROOT);
        fs::create_dir_all(&runtime).unwrap();
        extract_archive(&archive, &runtime, asset.platform).unwrap();
        fs::write(runtime.join("zerqon-runtime.json"), serde_json::to_vec(&json!({
            "release":RELEASE,"sha256":asset.sha256,"size":asset.size,"platform":asset.platform.cache_name(),"source":"Eclipse Adoptium"
        })).unwrap()).unwrap();
        runtime
    }

    fn copy_official_tree(source: &Path, target: &Path) {
        fs::create_dir_all(target).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let metadata = fs::symlink_metadata(entry.path()).unwrap();
            assert!(!is_runtime_link(&metadata));
            if metadata.is_dir() {
                copy_official_tree(&entry.path(), &target.join(entry.file_name()));
            } else {
                assert!(metadata.is_file());
                fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
            }
        }
    }

    fn run_resolver_child(test: &str, root: &Path) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--include-ignored", "--nocapture"])
            .env("ZERQON_RUNTIME_TEST_ROOT", root)
            .env("LOCALAPPDATA", root.join("profile"))
            .env("PATH", "")
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn forged_managed_marker_never_executes_a_regular_replacement_binary() {
        if let Some(root) = resolver_child_root() {
            assert!(probe_java(Path::new("java")).await.is_err());
            let cache = root.join("cache");
            let runtime = cache
                .join("25")
                .join(current_asset().unwrap().platform.cache_name())
                .join(ARCHIVE_ROOT);
            let result = resolve_java(Path::new("java"), &cache, false).await;
            assert!(
                !runtime.join("executed").exists(),
                "unverified replacement binary executed from an official-looking managed cache"
            );
            assert!(result.is_err());
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let runtime = cached_probe_fixture(&directory.path().join("cache"), "valid");
        let binary = runtime
            .join("bin")
            .join(current_asset().unwrap().platform.executable());
        let original = fs::read(&binary).unwrap();
        run_resolver_child(
            "tests::forged_managed_marker_never_executes_a_regular_replacement_binary",
            directory.path(),
        );
        assert_eq!(fs::read(binary).unwrap(), original);
        assert!(!runtime.join("executed").exists());
    }

    #[tokio::test]
    async fn default_java_does_not_execute_a_current_directory_candidate() {
        if let Some(root) = resolver_child_root() {
            let runtime = root
                .join("working")
                .join("25")
                .join(current_asset().unwrap().platform.cache_name())
                .join(ARCHIVE_ROOT);
            let result =
                resolve_java(Path::new("java"), &root.join("requested-cache"), false).await;
            assert!(
                !runtime.join("executed").exists(),
                "default Java lookup executed a current-directory binary before cache validation"
            );
            assert!(result.is_err());
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let runtime = cached_probe_fixture(&directory.path().join("working"), "valid");
        let empty_path = directory.path().join("empty-path");
        fs::create_dir(&empty_path).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::default_java_does_not_execute_a_current_directory_candidate",
                "--nocapture",
            ])
            .env("ZERQON_RUNTIME_TEST_ROOT", directory.path())
            .env("LOCALAPPDATA", directory.path().join("profile"))
            .env("PATH", empty_path)
            .current_dir(runtime.join("bin"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!runtime.join("executed").exists());
    }

    #[tokio::test]
    async fn default_java_does_not_execute_an_application_directory_candidate() {
        if let Some(root) = resolver_child_root() {
            let names: &[&str] = if cfg!(windows) {
                &["java", "Java", "JAVA.EXE"]
            } else {
                &["java"]
            };
            for name in names {
                let _ = resolve_java(Path::new(name), &root.join("requested-cache"), false).await;
                assert!(
                    !root.join("executed").exists(),
                    "default Java lookup {name} executed an application-directory binary"
                );
            }
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let runtime = cached_probe_fixture(&directory.path().join("fixture"), "valid");
        let application = directory.path().join("application");
        let empty_path = directory.path().join("empty-path");
        fs::create_dir(&application).unwrap();
        fs::create_dir(&empty_path).unwrap();
        let harness = application.join(if cfg!(windows) {
            "harness.exe"
        } else {
            "harness"
        });
        fs::copy(std::env::current_exe().unwrap(), &harness).unwrap();
        let executable = current_asset().unwrap().platform.executable();
        fs::copy(
            runtime.join("bin").join(executable),
            application.join(executable),
        )
        .unwrap();
        fs::write(directory.path().join("probe-mode"), "valid").unwrap();
        let output = std::process::Command::new(harness)
            .args([
                "--exact",
                "tests::default_java_does_not_execute_an_application_directory_candidate",
                "--nocapture",
            ])
            .env("ZERQON_RUNTIME_TEST_ROOT", directory.path())
            .env("LOCALAPPDATA", directory.path().join("profile"))
            .env("PATH", empty_path)
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!directory.path().join("executed").exists());
    }

    #[tokio::test]
    async fn managed_runtime_on_path_still_requires_integrity_before_execution() {
        if let Some(root) = resolver_child_root() {
            let runtime = root
                .join("cache")
                .join("25")
                .join(current_asset().unwrap().platform.cache_name())
                .join(ARCHIVE_ROOT);
            assert!(
                resolve_java(Path::new("java"), &root.join("cache"), false)
                    .await
                    .is_err()
            );
            assert!(!runtime.join("executed").exists());
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let runtime = cached_probe_fixture(&directory.path().join("cache"), "valid");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::managed_runtime_on_path_still_requires_integrity_before_execution",
                "--nocapture",
            ])
            .env("ZERQON_RUNTIME_TEST_ROOT", directory.path())
            .env("LOCALAPPDATA", directory.path().join("profile"))
            .env("PATH", runtime.join("bin"))
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!runtime.join("executed").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn default_path_lookup_skips_nonexecutable_files_before_installed_java() {
        if let Some(root) = resolver_child_root() {
            let resolved = resolve_java(Path::new("java"), &root.join("cache"), false)
                .await
                .unwrap();
            assert!(resolved.starts_with(root.join("installed")));
            assert!(!root.join("cache").exists());
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let installed = cached_probe_fixture(&directory.path().join("installed"), "valid");
        let tools = directory.path().join("tools");
        fs::create_dir(&tools).unwrap();
        fs::write(tools.join("java"), b"not executable").unwrap();
        fs::set_permissions(tools.join("java"), fs::Permissions::from_mode(0o644)).unwrap();
        let path = std::env::join_paths([tools, installed.join("bin")]).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::default_path_lookup_skips_nonexecutable_files_before_installed_java",
                "--nocapture",
            ])
            .env("ZERQON_RUNTIME_TEST_ROOT", directory.path())
            .env("PATH", path)
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(installed.join("executed").exists());
    }

    #[tokio::test]
    #[ignore = "Requires the pinned official Windows/Linux JRE archive fixture"]
    async fn official_caches_are_reused_offline_with_platform_unicode_fallback() {
        if let Some(root) = resolver_child_root() {
            let asset = current_asset().unwrap();
            let ascii_cache = root.join("profile/Zerqon/cache/java");
            let unicode_cache = root.join("среда-中文");
            let unicode_runtime = unicode_cache
                .join("25")
                .join(asset.platform.cache_name())
                .join(ARCHIVE_ROOT);
            validate_cached_files(&unicode_runtime, asset).unwrap();
            let unicode_probe = probe_java(
                &unicode_runtime
                    .join("bin")
                    .join(asset.platform.executable()),
            )
            .await;
            let unicode_works = unicode_probe.is_ok();
            let expected_unicode = match unicode_probe {
                Ok(java) => java,
                Err(JavaRuntimeError::ProbeExit(_)) if cfg!(windows) => ascii_cache
                    .join("25")
                    .join(asset.platform.cache_name())
                    .join(ARCHIVE_ROOT)
                    .join("bin")
                    .join(asset.platform.executable()),
                Err(error) => panic!("unexpected official Unicode runtime result: {error}"),
            };
            let resolved = resolve_java(Path::new("java"), &unicode_cache, false)
                .await
                .unwrap();
            assert_eq!(
                resolved.canonicalize().unwrap(),
                expected_unicode.canonicalize().unwrap()
            );
            let ascii = resolve_java(Path::new("java"), &ascii_cache, false)
                .await
                .unwrap();
            if std::env::var_os("ZERQON_RUNTIME_TEST_PATH").is_some() && unicode_works {
                assert_eq!(
                    ascii.canonicalize().unwrap(),
                    expected_unicode.canonicalize().unwrap()
                );
            } else {
                assert!(
                    ascii
                        .canonicalize()
                        .unwrap()
                        .starts_with(ascii_cache.canonicalize().unwrap())
                );
            }
            validate_cached_files(&unicode_runtime, asset).unwrap();
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let ascii = official_fixture(&directory.path().join("profile/Zerqon/cache/java"));
        let asset = current_asset().unwrap();
        let unicode = directory
            .path()
            .join("среда-中文/25")
            .join(asset.platform.cache_name())
            .join(ARCHIVE_ROOT);
        copy_official_tree(&ascii, &unicode);
        let marker = fs::read(unicode.join("zerqon-runtime.json")).unwrap();
        run_resolver_child(
            "tests::official_caches_are_reused_offline_with_platform_unicode_fallback",
            directory.path(),
        );
        let path_case = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::official_caches_are_reused_offline_with_platform_unicode_fallback",
                "--include-ignored",
                "--nocapture",
            ])
            .env("ZERQON_RUNTIME_TEST_ROOT", directory.path())
            .env("LOCALAPPDATA", directory.path().join("profile"))
            .env("PATH", unicode.join("bin"))
            .env("ZERQON_RUNTIME_TEST_PATH", "1")
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(
            path_case.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&path_case.stdout),
            String::from_utf8_lossy(&path_case.stderr)
        );
        assert_eq!(
            fs::read(unicode.join("zerqon-runtime.json")).unwrap(),
            marker
        );
    }

    #[tokio::test]
    #[ignore = "Requires the pinned official Windows/Linux JRE archive fixture"]
    async fn official_runtime_hashes_reject_tampering_missing_and_extra_files_before_execution() {
        use std::io::{Seek, SeekFrom};
        let directory = tempfile::tempdir().unwrap();
        let runtime = official_fixture(&directory.path().join("cache"));
        let asset = current_asset().unwrap();
        let started = std::time::Instant::now();
        validate_cached_files(&runtime, asset).unwrap();
        eprintln!(
            "Official {:?} first integrity check after extraction: {:?}",
            asset.platform,
            started.elapsed()
        );
        let started = std::time::Instant::now();
        validate_cached_files(&runtime, asset).unwrap();
        eprintln!(
            "Official {:?} repeated integrity check: {:?}",
            asset.platform,
            started.elapsed()
        );
        let paths = match asset.platform {
            Platform::WindowsX64 => [
                "bin/java.exe",
                "bin/java.dll",
                "bin/server/jvm.dll",
                "lib/modules",
                "conf/security/java.security",
                "legal/java.base/LICENSE",
            ],
            Platform::LinuxX64 => [
                "bin/java",
                "lib/libjli.so",
                "lib/server/libjvm.so",
                "lib/modules",
                "conf/security/java.security",
                "legal/java.base/LICENSE",
            ],
        };
        for relative in paths {
            let path = runtime.join(relative);
            let mut file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            let mut original = [0_u8; 1];
            file.read_exact(&mut original).unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(&[original[0] ^ 0x80]).unwrap();
            file.sync_all().unwrap();
            let error = validate_cached_files(&runtime, asset).unwrap_err();
            assert!(
                error.to_string().contains("contents differ"),
                "{relative}: {error}"
            );
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(&original).unwrap();
            file.sync_all().unwrap();
        }
        let notice = runtime.join("NOTICE");
        let saved_notice = fs::read(&notice).unwrap();
        fs::remove_file(&notice).unwrap();
        assert!(
            validate_cached_files(&runtime, asset)
                .unwrap_err()
                .to_string()
                .contains("missing pinned files")
        );
        fs::write(&notice, &saved_notice).unwrap();
        let injected = runtime.join("lib/unrecognized-agent.jar");
        fs::write(&injected, b"not an official runtime component").unwrap();
        assert!(validate_cached_files(&runtime, asset).is_err());
        assert_eq!(
            fs::read(&injected).unwrap(),
            b"not an official runtime component"
        );
        fs::remove_file(injected).unwrap();
        validate_cached_files(&runtime, asset).unwrap();
        let fake = cached_probe_fixture(&directory.path().join("fake"), "valid");
        let binary = runtime.join("bin").join(asset.platform.executable());
        fs::copy(fake.join("bin").join(asset.platform.executable()), &binary).unwrap();
        assert!(validated_cached_runtime(&runtime, asset).await.is_err());
        assert!(!runtime.join("executed").exists());
        assert!(binary.is_file());
    }

    #[tokio::test]
    async fn explicit_java_preserves_operator_selected_unicode_paths() {
        if let Some(root) = resolver_child_root() {
            assert!(probe_java(Path::new("java")).await.is_err());
            let cache = root.join("среда-中文");
            let executable = cache
                .join("25")
                .join(current_asset().unwrap().platform.cache_name())
                .join(ARCHIVE_ROOT)
                .join("bin")
                .join(current_asset().unwrap().platform.executable());
            let resolved = resolve_java(&executable, &root.join("unused-cache"), false)
                .await
                .unwrap();
            assert!(
                resolved
                    .canonicalize()
                    .unwrap()
                    .starts_with(cache.canonicalize().unwrap())
            );
            assert!(!root.join("profile").exists());
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        cached_probe_fixture(&directory.path().join("среда-中文"), "valid");
        run_resolver_child(
            "tests::explicit_java_preserves_operator_selected_unicode_paths",
            directory.path(),
        );
    }

    #[tokio::test]
    async fn resolver_rejects_corrupt_local_identity_before_using_fallback() {
        if let Some(root) = resolver_child_root() {
            assert!(probe_java(Path::new("java")).await.is_err());
            let error = resolve_java(Path::new("java"), &root.join("среда-中文"), true)
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                JavaRuntimeError::Invalid(_) | JavaRuntimeError::Json(_)
            ));
            return;
        }
        for contents in [b"{broken".as_slice(), br#"{"release":"unknown"}"#] {
            let directory = tempfile::tempdir().unwrap();
            let local = cached_probe_fixture(&directory.path().join("среда-中文"), "valid");
            let fallback =
                cached_probe_fixture(&directory.path().join("profile/Zerqon/cache/java"), "valid");
            fs::write(local.join("zerqon-runtime.json"), contents).unwrap();
            run_resolver_child(
                "tests::resolver_rejects_corrupt_local_identity_before_using_fallback",
                directory.path(),
            );
            assert_eq!(
                fs::read(local.join("zerqon-runtime.json")).unwrap(),
                contents
            );
            assert!(!local.join("executed").exists());
            assert!(!fallback.join("executed").exists());
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn forged_unicode_runtime_is_rejected_without_executing_or_using_fallback() {
        if let Some(root) = resolver_child_root() {
            assert!(probe_java(Path::new("java")).await.is_err());
            let error = resolve_java(Path::new("java"), &root.join("среда-中文"), false)
                .await
                .unwrap_err();
            assert!(matches!(error, JavaRuntimeError::Invalid(_)));
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let local = cached_probe_fixture(&directory.path().join("среда-中文"), "exit");
        let fallback =
            cached_probe_fixture(&directory.path().join("profile/Zerqon/cache/java"), "valid");
        let identity = fs::read(local.join("zerqon-runtime.json")).unwrap();
        run_resolver_child(
            "tests::forged_unicode_runtime_is_rejected_without_executing_or_using_fallback",
            directory.path(),
        );
        assert!(!local.join("executed").exists());
        assert!(!fallback.join("executed").exists());
        assert_eq!(
            fs::read(local.join("zerqon-runtime.json")).unwrap(),
            identity
        );
    }

    #[tokio::test]
    async fn explicit_unexpected_java_version_is_not_hidden_by_fallback() {
        if let Some(root) = resolver_child_root() {
            assert!(probe_java(Path::new("java")).await.is_err());
            let asset = current_asset().unwrap();
            let binary = root
                .join("среда-中文")
                .join("25")
                .join(asset.platform.cache_name())
                .join(ARCHIVE_ROOT)
                .join("bin")
                .join(asset.platform.executable());
            let error = resolve_java(&binary, &root.join("unused-cache"), false)
                .await
                .unwrap_err();
            assert!(matches!(error, JavaRuntimeError::Invalid(_)));
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let local = cached_probe_fixture(&directory.path().join("среда-中文"), "old");
        let fallback =
            cached_probe_fixture(&directory.path().join("profile/Zerqon/cache/java"), "valid");
        run_resolver_child(
            "tests::explicit_unexpected_java_version_is_not_hidden_by_fallback",
            directory.path(),
        );
        assert!(local.join("executed").exists());
        assert!(!fallback.join("executed").exists());
    }

    #[tokio::test]
    async fn explicit_launcher_exit_remains_an_error() {
        if let Some(root) = resolver_child_root() {
            assert!(probe_java(Path::new("java")).await.is_err());
            let asset = current_asset().unwrap();
            let binary = root
                .join("ascii")
                .join("25")
                .join(asset.platform.cache_name())
                .join(ARCHIVE_ROOT)
                .join("bin")
                .join(asset.platform.executable());
            let error = resolve_java(&binary, &root.join("unused-cache"), false)
                .await
                .unwrap_err();
            assert!(matches!(error, JavaRuntimeError::Invalid(_)));
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let local = cached_probe_fixture(&directory.path().join("ascii"), "exit");
        let fallback =
            cached_probe_fixture(&directory.path().join("profile/Zerqon/cache/java"), "valid");
        run_resolver_child(
            "tests::explicit_launcher_exit_remains_an_error",
            directory.path(),
        );
        assert!(local.join("executed").exists());
        assert!(!fallback.join("executed").exists());
    }

    #[tokio::test]
    async fn disabled_downloads_create_neither_requested_nor_fallback_cache() {
        if let Some(root) = resolver_child_root() {
            assert!(probe_java(Path::new("java")).await.is_err());
            assert!(
                resolve_java(Path::new("java"), &root.join("среда-中文"), false)
                    .await
                    .is_err()
            );
            assert!(!root.join("среда-中文").exists());
            assert!(!root.join("profile").exists());
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        run_resolver_child(
            "tests::disabled_downloads_create_neither_requested_nor_fallback_cache",
            directory.path(),
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }
    fn metadata() -> Value {
        json!({"release_name":"jdk-25.0.5+8","version_data":{"major":25,"security":5,"build":8},"binaries":[{"architecture":"x64","os":"windows","image_type":"jre","jvm_impl":"hotspot","package":{"name":"OpenJDK25U-jre_x64_windows_hotspot_25.0.5_8.zip","size":58589419u64,"checksum":"da78d54a2fcdbba1e92d5e3f07dd30236f96297891c4a21a781c3f38f5c0beae","link":"https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5%2B8/OpenJDK25U-jre_x64_windows_hotspot_25.0.5_8.zip"}}]})
    }
    #[test]
    fn metadata_is_bound_to_release_platform_size_hash_and_trusted_asset() {
        assert!(validate_metadata(&metadata(), WINDOWS).is_ok());
        for (pointer, value) in [
            ("/release_name", json!("jdk-26")),
            ("/version_data/major", json!(26)),
            ("/binaries/0/os", json!("linux")),
            ("/binaries/0/package/size", json!(1)),
            ("/binaries/0/package/checksum", json!("00")),
            (
                "/binaries/0/package/link",
                json!("https://example.com/java.zip"),
            ),
        ] {
            let mut value_original = metadata();
            *value_original.pointer_mut(pointer).unwrap() = value;
            assert!(
                validate_metadata(&value_original, WINDOWS).is_err(),
                "{pointer}"
            );
        }
    }
    #[test]
    fn archive_entries_cannot_escape_runtime_root_or_target_devices() {
        assert_eq!(
            archive_path("jdk-25.0.5+8-jre/bin/java").unwrap(),
            PathBuf::from("bin/java")
        );
        for name in [
            "/jdk-25.0.5+8-jre/bin/java",
            "jdk-25.0.5+8-jre/../secret",
            "other/bin/java",
            "jdk-25.0.5+8-jre/bin\\java",
            "jdk-25.0.5+8-jre/NUL",
            "jdk-25.0.5+8-jre/foo:stream",
        ] {
            assert!(archive_path(name).is_err(), "{name}");
        }
    }
    #[test]
    fn license_links_materialize_only_inside_the_archive_legal_directory() {
        assert_eq!(
            link_target(Path::new("legal/java.sql/LICENSE"), "../java.base/LICENSE").unwrap(),
            PathBuf::from("legal/java.base/LICENSE")
        );
        for target in [
            "../../../outside",
            "/etc/passwd",
            "C:\\secret",
            "../..//bin/java",
        ] {
            assert!(
                link_target(Path::new("legal/java.sql/LICENSE"), target).is_err(),
                "{target}"
            );
        }
    }
    #[test]
    fn probe_requires_exact_java25_and_reported_home() {
        assert_eq!(
            parse_probe("java.home = /runtime\njava.specification.version = 25\n").unwrap(),
            PathBuf::from("/runtime/bin").join(if cfg!(windows) { "java.exe" } else { "java" })
        );
        for text in [
            "java.home = /runtime\njava.specification.version = 21",
            "java.specification.version = 25",
            "java.home = /runtime\njava.specification.version = unknown",
        ] {
            assert!(parse_probe(text).is_err());
        }
    }

    #[test]
    fn redirect_hosts_require_https_exact_host_and_no_credentials() {
        for url in [
            "http://github.com/a",
            "https://github.com.evil.test/a",
            "https://user:password@github.com/a",
            "https://api.adoptium.net:8443/a",
            "https://localhost/a",
        ] {
            assert!(!trusted_url(&reqwest::Url::parse(url).unwrap()), "{url}");
        }
        assert!(trusted_url(&reqwest::Url::parse(WINDOWS.url).unwrap()));
        assert!(trusted_url(
            &reqwest::Url::parse(
                "https://release-assets.githubusercontent.com/asset?signature=test"
            )
            .unwrap()
        ));
    }

    #[test]
    fn zip_extraction_rejects_symlinks_and_case_collisions() {
        for link in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("fixture.zip");
            let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file(format!("{ARCHIVE_ROOT}/bin/java"), options)
                .unwrap();
            zip.write_all(b"binary").unwrap();
            if link {
                zip.add_symlink(
                    format!("{ARCHIVE_ROOT}/legal/link"),
                    "../../outside",
                    options,
                )
                .unwrap();
            } else {
                zip.start_file(format!("{ARCHIVE_ROOT}/bin/JAVA"), options)
                    .unwrap();
                zip.write_all(b"replacement").unwrap();
            }
            zip.finish().unwrap();
            let output = directory.path().join("out");
            fs::create_dir(&output).unwrap();
            assert!(extract_zip(&path, &output).is_err());
            assert!(!directory.path().join("outside").exists());
        }
    }

    fn tar_fixture(path: &Path, escape: bool, cycle: bool) {
        let encoder = flate2::write::GzEncoder::new(
            File::create(path).unwrap(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(7);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(
            &mut header,
            format!("{ARCHIVE_ROOT}/legal/java.base/LICENSE"),
            b"license".as_slice(),
        )
        .unwrap();
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_mode(0o777);
        link.set_cksum();
        tar.append_link(
            &mut link,
            format!("{ARCHIVE_ROOT}/legal/java.sql/LICENSE"),
            if escape {
                "../../../outside"
            } else if cycle {
                "LICENSE"
            } else {
                "../java.base/LICENSE"
            },
        )
        .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn tar_materializes_safe_license_links_and_rejects_cycles_and_traversal() {
        for (escape, cycle) in [(false, false), (true, false), (false, true)] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("fixture.tar.gz");
            tar_fixture(&path, escape, cycle);
            let output = directory.path().join("out");
            fs::create_dir(&output).unwrap();
            let result = extract_tar(&path, &output);
            if escape || cycle {
                assert!(result.is_err());
            } else {
                result.unwrap();
                let license = output.join("legal/java.sql/LICENSE");
                assert_eq!(fs::read(&license).unwrap(), b"license");
                assert!(
                    !fs::symlink_metadata(license)
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
            }
        }
    }

    #[tokio::test]
    async fn explicit_missing_java_never_triggers_download_or_changes_cache() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache");
        assert!(
            resolve_java(&directory.path().join("missing-java"), &cache, true)
                .await
                .is_err()
        );
        assert!(!cache.exists());
    }

    #[tokio::test]
    #[ignore = "Requires installed system Java25; does not download anything"]
    async fn system_java25_is_used_without_creating_cache() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache");
        let java = resolve_java(Path::new("java"), &cache, false)
            .await
            .unwrap();
        assert!(java.is_file());
        assert!(!cache.exists());
    }

    #[tokio::test]
    #[ignore = "Downloads and verifies the pinned official JRE for the current Windows/Linux x64 platform"]
    async fn official_managed_runtime_download_install_probe_and_reuse() {
        if let Some(root) = resolver_child_root() {
            assert!(probe_java(Path::new("java")).await.is_err());
            let java = resolve_java(Path::new("java"), &root.join("среда-中文-Java"), false)
                .await
                .unwrap();
            assert_eq!(probe_java(&java).await.unwrap(), java);
            assert!(
                java.canonicalize()
                    .unwrap()
                    .starts_with(root.canonicalize().unwrap())
            );
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("среда-中文-Java");
        #[cfg(windows)]
        let cache = {
            let resolved =
                windows_cache_location(&cache, Some(&directory.path().join("profile"))).unwrap();
            assert!(!cache.exists());
            assert!(resolved.starts_with(directory.path()));
            resolved
        };
        let asset = current_asset().unwrap();
        let java = install_managed(&cache, asset).await.unwrap();
        assert!(
            java.canonicalize()
                .unwrap()
                .starts_with(directory.path().canonicalize().unwrap())
        );
        assert_eq!(probe_java(&java).await.unwrap(), java);
        assert_eq!(install_managed(&cache, asset).await.unwrap(), java);
        run_resolver_child(
            "tests::official_managed_runtime_download_install_probe_and_reuse",
            directory.path(),
        );
    }

    #[cfg(windows)]
    #[test]
    fn non_ascii_runtime_cache_uses_private_fallback_without_writing_files() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("中文/cache");
        let fallback = directory.path().join("profile");
        let resolved = windows_cache_location(&cache, Some(&fallback)).unwrap();
        assert_eq!(resolved, fallback.join("Zerqon/cache/java"));
        assert!(!cache.exists());
        assert!(!fallback.exists());
        assert_eq!(
            windows_cache_location(&directory.path().join("ascii"), None).unwrap(),
            directory.path().join("ascii")
        );
        for invalid_fallback in [None, Some(Path::new("relative")), Some(cache.as_path())] {
            assert!(windows_cache_location(&cache, invalid_fallback).is_err());
        }
        assert!(!cache.exists());
    }

    #[test]
    #[ignore = "Requires verified .local/adoptium-linux25.tar.gz official Linux archive"]
    fn official_linux_archive_extracts_with_all_license_links_materialized() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".local/adoptium-linux25.tar.gz");
        let directory = tempfile::tempdir().unwrap();
        extract_tar(&path, directory.path()).unwrap();
        assert!(directory.path().join("bin/java").is_file());
        assert!(directory.path().join("lib/server/libjvm.so").is_file());
        let license = directory.path().join("legal/java.sql/LICENSE");
        assert!(license.is_file());
        assert!(
            !fs::symlink_metadata(license)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[tokio::test]
    async fn streaming_runtime_download_rejects_hash_mismatch_and_oversize() {
        for (body, size, sha256) in [
            (
                "abc",
                3,
                "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            ),
            (
                "abcd",
                3,
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 2048];
                let received = socket.read(&mut request).await.unwrap();
                assert!(received > 0);
                let response = format!("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{body}");
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            });
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            let mut target = tempfile::NamedTempFile::new().unwrap();
            assert!(
                download_archive(
                    &client,
                    &format!("http://{address}/runtime"),
                    size,
                    sha256,
                    &mut target
                )
                .await
                .is_err()
            );
        }
    }

    #[test]
    fn entry_size_limits_fail_before_creating_an_output_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("entry");
        let mut total = 0;
        assert!(write_entry(&mut b"x".as_slice(), &path, MAX_FILE + 1, &mut total, 0o644).is_err());
        assert!(!path.exists());
        total = MAX_UNPACKED;
        assert!(write_entry(&mut b"x".as_slice(), &path, 1, &mut total, 0o644).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn extraction_cancellation_does_not_copy_more_than_one_64_kib_block() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("entry");
        let mut input = std::io::Cursor::new(vec![7; 4 * 65_536]);
        let check = || {
            if path
                .metadata()
                .is_ok_and(|metadata| metadata.len() >= 65_536)
            {
                Err(JavaRuntimeError::Stopped(Stopped))
            } else {
                Ok(())
            }
        };
        let result = super::write_entry(&mut input, &path, 4 * 65_536, &mut 0, 0o644, &check);
        assert!(matches!(result, Err(JavaRuntimeError::Stopped(_))));
        assert_eq!(input.position(), 65_536);
        assert_eq!(path.metadata().unwrap().len(), 65_536);
    }

    #[test]
    fn validation_cancellation_does_not_hash_more_than_one_64_kib_block() {
        use std::cell::Cell;
        struct ObservedRead<'a> {
            input: std::io::Cursor<Vec<u8>>,
            bytes: &'a Cell<u64>,
        }
        impl Read for ObservedRead<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let count = Read::read(&mut self.input, buffer)?;
                self.bytes.set(self.bytes.get() + count as u64);
                Ok(count)
            }
        }
        let bytes = Cell::new(0);
        let mut input = ObservedRead {
            input: std::io::Cursor::new(vec![7; 4 * 65_536]),
            bytes: &bytes,
        };
        let expected = RuntimeFile {
            path: "lib/modules".into(),
            size: 4 * 65_536,
            sha256: "0".repeat(64),
            executable: false,
        };
        let check = || {
            if bytes.get() >= 65_536 {
                Err(JavaRuntimeError::Stopped(Stopped))
            } else {
                Ok(())
            }
        };
        let result = validate_runtime_contents(&mut input, &expected, &mut [0; 65_536], &check);
        assert!(matches!(result, Err(JavaRuntimeError::Stopped(_))));
        assert_eq!(bytes.get(), 65_536);
    }

    #[tokio::test]
    async fn concurrent_installers_wait_for_cache_owner_instead_of_failing_immediately() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("install.lock");
        let owner = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        owner.try_lock().unwrap();
        let waiter = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(
            acquire_install_lock(&waiter, Duration::from_millis(50))
                .await
                .is_err()
        );
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(owner);
        });
        acquire_install_lock(&waiter, Duration::from_secs(2))
            .await
            .unwrap();
        release.await.unwrap();
    }

    fn installation_fixture(directory: &Path, archive_bytes: &[u8]) -> Installation {
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("install.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        let mut archive = tempfile::NamedTempFile::new_in(directory).unwrap();
        archive.write_all(archive_bytes).unwrap();
        let stage = tempfile::Builder::new()
            .prefix("install-")
            .tempdir_in(directory)
            .unwrap();
        fs::create_dir(stage.path().join("runtime")).unwrap();
        Installation {
            archive,
            stage,
            runtime: directory.join(ARCHIVE_ROOT),
            _lock: lock,
        }
    }

    #[tokio::test]
    async fn cancelled_install_keeps_worker_files_and_lock_until_extraction_finishes() {
        let directory = tempfile::tempdir().unwrap();
        let installation = installation_fixture(directory.path(), b"invalid archive");
        let archive = installation.archive.path().to_owned();
        let stage = installation.stage.path().to_owned();
        let runtime = installation.runtime.clone();
        let waiter = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.path().join("install.lock"))
            .unwrap();
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let task = tokio::spawn(finish_installation(
            installation,
            current_asset().unwrap(),
            move |archive, output, platform| {
                started.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(5)).unwrap();
                extract_archive(archive, output, platform)
            },
        ));
        running.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let lock_held = matches!(waiter.try_lock(), Err(std::fs::TryLockError::WouldBlock));
        let archive_retained = archive.is_file();
        let stage_retained = stage.is_dir();
        release.send(()).unwrap();
        acquire_install_lock(&waiter, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(
            lock_held,
            "cancelled installer released an active worker's lock"
        );
        assert!(
            archive_retained,
            "cancelled installer removed the active archive"
        );
        assert!(
            stage_retained,
            "cancelled installer removed the active stage"
        );
        assert!(!archive.exists());
        assert!(!stage.exists());
        assert!(!runtime.exists());
    }

    #[test]
    fn cancelled_queued_install_retains_its_guards_until_the_worker_exits() {
        use std::future::Future;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let (started, running) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let blocker = runtime.spawn_blocking(move || {
            started.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        running.recv_timeout(Duration::from_secs(5)).unwrap();
        runtime.block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let installation = installation_fixture(directory.path(), b"invalid archive");
            let archive = installation.archive.path().to_owned();
            let stage = installation.stage.path().to_owned();
            let waiter = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(directory.path().join("install.lock"))
                .unwrap();
            let lifecycle = Lifecycle::new();
            let stop = lifecycle.token();
            let mut task = Box::pin(finish_installation_tracked(
                installation,
                current_asset().unwrap(),
                extract_archive,
                &lifecycle,
                &stop,
            ));
            std::future::poll_fn(|cx| {
                assert!(task.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            drop(task);
            let lock_held = matches!(waiter.try_lock(), Err(std::fs::TryLockError::WouldBlock));
            let inputs_retained = archive.is_file() && stage.is_dir();
            release.send(()).unwrap();
            blocker.await.unwrap();
            acquire_install_lock(&waiter, Duration::from_secs(5))
                .await
                .unwrap();
            assert!(lock_held, "queued installation lost its cache lock");
            assert!(inputs_retained, "queued installation lost its input guards");
            assert!(!archive.exists() && !stage.exists());
            assert!(!directory.path().join(ARCHIVE_ROOT).exists());
            lifecycle
                .drain(tokio::time::Instant::now() + Duration::from_secs(5))
                .await
                .unwrap();
        });
    }

    fn official_installation(directory: &Path) -> Installation {
        let asset = current_asset().unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(match asset.platform {
            Platform::WindowsX64 => ".local/adoptium-windows25.zip",
            Platform::LinuxX64 => ".local/adoptium-linux25.tar.gz",
        });
        let mut input = File::open(source).unwrap();
        assert_eq!(input.metadata().unwrap().len(), asset.size);
        let mut installation = installation_fixture(directory, b"");
        let mut digest = Context::new(&SHA256);
        let mut buffer = [0; 65_536];
        loop {
            let count = input.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            installation.archive.write_all(&buffer[..count]).unwrap();
            digest.update(&buffer[..count]);
        }
        assert_eq!(hex(digest.finish().as_ref()), asset.sha256);
        installation
    }

    #[tokio::test]
    #[ignore = "Requires the pinned official Windows/Linux JRE archive fixture"]
    async fn cancelled_genuine_runtime_extraction_cleans_only_after_worker_completion() {
        let directory = tempfile::tempdir().unwrap();
        let installation = official_installation(directory.path());
        let archive = installation.archive.path().to_owned();
        let stage = installation.stage.path().to_owned();
        let waiter = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.path().join("install.lock"))
            .unwrap();
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (extracted, extraction) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(finish_installation(
            installation,
            current_asset().unwrap(),
            move |archive, output, platform| {
                started.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(5)).unwrap();
                let result = extract_archive(archive, output, platform);
                extracted.send(result.is_ok()).unwrap();
                result
            },
        ));
        running.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let held = matches!(waiter.try_lock(), Err(std::fs::TryLockError::WouldBlock));
        let inputs_retained = archive.is_file() && stage.is_dir();
        release.send(()).unwrap();
        assert!(
            extraction.await.unwrap(),
            "genuine archive extraction failed after cancellation"
        );
        acquire_install_lock(&waiter, Duration::from_secs(15))
            .await
            .unwrap();
        assert!(held && inputs_retained);
        assert!(!archive.exists() && !stage.exists());
        assert!(!directory.path().join(ARCHIVE_ROOT).exists());
    }

    #[tokio::test]
    #[ignore = "Requires the pinned official Windows/Linux JRE archive fixture"]
    async fn genuine_runtime_installation_publishes_only_a_verified_cache() {
        let directory = tempfile::tempdir().unwrap();
        let installation = official_installation(directory.path());
        let archive = installation.archive.path().to_owned();
        let stage = installation.stage.path().to_owned();
        let runtime = installation.runtime.clone();
        let java = finish_installation(installation, current_asset().unwrap(), extract_archive)
            .await
            .unwrap();
        assert!(
            java.canonicalize()
                .unwrap()
                .starts_with(runtime.canonicalize().unwrap()),
            "Java executable is outside the verified runtime: {}",
            java.display()
        );
        validate_cached_files(&runtime, current_asset().unwrap()).unwrap();
        assert!(!archive.exists() && !stage.exists());
        let waiter = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.path().join("install.lock"))
            .unwrap();
        waiter.try_lock().unwrap();
    }

    #[tokio::test]
    #[ignore = "Requires the pinned official Windows/Linux JRE archive fixture"]
    async fn installation_never_replaces_an_existing_verified_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = official_fixture(directory.path());
        let marker = fs::read(runtime.join("zerqon-runtime.json")).unwrap();
        let installation = official_installation(runtime.parent().unwrap());
        let stage = installation.stage.path().to_owned();
        let archive = installation.archive.path().to_owned();
        let error = finish_installation(installation, current_asset().unwrap(), extract_archive)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("destination appeared"));
        assert_eq!(
            fs::read(runtime.join("zerqon-runtime.json")).unwrap(),
            marker
        );
        validate_cached_files(&runtime, current_asset().unwrap()).unwrap();
        assert!(!archive.exists() && !stage.exists());
    }

    #[test]
    #[ignore = "Requires the pinned official Windows/Linux JRE archive fixture"]
    fn cancelled_cached_install_retains_lock_until_queued_validation_finishes() {
        use std::future::Future;
        let directory = tempfile::tempdir().unwrap();
        let cached = official_fixture(directory.path());
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let (started, running) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let blocker = executor.spawn_blocking(move || {
            started.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        running.recv_timeout(Duration::from_secs(5)).unwrap();
        executor.block_on(async {
            let lifecycle = Lifecycle::new();
            let stop = lifecycle.token();
            let mut task = Box::pin(install_managed_tracked(
                directory.path(),
                current_asset().unwrap(),
                &lifecycle,
                &stop,
            ));
            std::future::poll_fn(|cx| {
                assert!(task.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            drop(task);
            let waiter = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(cached.parent().unwrap().join("install.lock"))
                .unwrap();
            let held = matches!(waiter.try_lock(), Err(std::fs::TryLockError::WouldBlock));
            release.send(()).unwrap();
            blocker.await.unwrap();
            acquire_install_lock(&waiter, Duration::from_secs(5))
                .await
                .unwrap();
            assert!(
                held,
                "cancelled cache validation released its install lock while queued"
            );
            lifecycle
                .drain(tokio::time::Instant::now() + Duration::from_secs(5))
                .await
                .unwrap();
        });
        validate_cached_files(&cached, current_asset().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn cancelled_probe_does_not_spawn_after_its_awaiter_is_gone() {
        let directory = tempfile::tempdir().unwrap();
        let lifecycle = Lifecycle::new();
        let stop = lifecycle.token();
        stop.request_stop();
        assert!(matches!(
            run_probe(&directory.path().join("absent-java"), &stop).await,
            Err(JavaRuntimeError::Stopped(_))
        ));
    }

    #[tokio::test]
    async fn cancelled_java_probe_reaps_its_child_before_releasing_installation_resources() {
        let directory = tempfile::tempdir().unwrap();
        let fake = cached_probe_fixture(&directory.path().join("probe"), "wait");
        let executable = fake
            .join("bin")
            .join(current_asset().unwrap().platform.executable());
        let installation = installation_fixture(directory.path(), b"archive retained for probe");
        let archive = installation.archive.path().to_owned();
        let stage = installation.stage.path().to_owned();
        let waiter = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.path().join("install.lock"))
            .unwrap();
        let lifecycle = Lifecycle::new();
        let context = lifecycle.clone();
        let task = tokio::spawn(async move {
            probe_java_with_owner_tracked(&executable, installation, &context, &context.token())
                .await
        });
        let pid: u32 = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = fs::read_to_string(fake.join("probe.pid"))
                    && let Ok(pid) = text.parse()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let child_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(fake.join("alive.lock"))
            .unwrap();
        assert!(matches!(
            child_lock.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        task.abort();
        match task.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("stalled Java probe completed before cancellation"),
        }
        lifecycle
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .unwrap();
        assert!(
            !lifecycle.token().is_stopped(),
            "caller cancellation stopped the shared lifecycle"
        );
        waiter.try_lock().unwrap();
        child_lock.try_lock().unwrap();
        #[cfg(target_os = "linux")]
        assert!(
            !Path::new("/proc").join(pid.to_string()).exists(),
            "Java child was not reaped"
        );
        #[cfg(not(target_os = "linux"))]
        let _ = pid;
        assert!(!archive.exists() && !stage.exists());
        assert!(!directory.path().join(ARCHIVE_ROOT).exists());
    }
}
