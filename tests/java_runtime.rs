include!("../src/java_runtime.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::OnceLock;

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
    async fn offline_resolver_preserves_working_unicode_cache() {
        if let Some(root) = resolver_child_root() {
            assert!(probe_java(Path::new("java")).await.is_err());
            let cache = root.join("среда-中文");
            let resolved = resolve_java(Path::new("java"), &cache, false)
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
            "tests::offline_resolver_preserves_working_unicode_cache",
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
    async fn unicode_launcher_exit_uses_existing_fallback_without_download() {
        if let Some(root) = resolver_child_root() {
            assert!(probe_java(Path::new("java")).await.is_err());
            let resolved = resolve_java(Path::new("java"), &root.join("среда-中文"), false)
                .await
                .unwrap();
            assert!(
                resolved
                    .canonicalize()
                    .unwrap()
                    .starts_with(root.join("profile").canonicalize().unwrap())
            );
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let local = cached_probe_fixture(&directory.path().join("среда-中文"), "exit");
        let fallback =
            cached_probe_fixture(&directory.path().join("profile/Zerqon/cache/java"), "valid");
        let identity = fs::read(local.join("zerqon-runtime.json")).unwrap();
        run_resolver_child(
            "tests::unicode_launcher_exit_uses_existing_fallback_without_download",
            directory.path(),
        );
        assert!(local.join("executed").exists());
        assert!(fallback.join("executed").exists());
        assert_eq!(
            fs::read(local.join("zerqon-runtime.json")).unwrap(),
            identity
        );
    }

    #[tokio::test]
    async fn unexpected_java_version_is_not_hidden_by_fallback() {
        if let Some(root) = resolver_child_root() {
            assert!(probe_java(Path::new("java")).await.is_err());
            let error = resolve_java(Path::new("java"), &root.join("среда-中文"), false)
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
            "tests::unexpected_java_version_is_not_hidden_by_fallback",
            directory.path(),
        );
        assert!(local.join("executed").exists());
        assert!(!fallback.join("executed").exists());
    }

    #[tokio::test]
    async fn ascii_launcher_exit_remains_an_error() {
        if let Some(root) = resolver_child_root() {
            assert!(probe_java(Path::new("java")).await.is_err());
            let error = resolve_java(Path::new("java"), &root.join("ascii"), false)
                .await
                .unwrap_err();
            assert!(matches!(error, JavaRuntimeError::ProbeExit(_)));
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let local = cached_probe_fixture(&directory.path().join("ascii"), "exit");
        let fallback =
            cached_probe_fixture(&directory.path().join("profile/Zerqon/cache/java"), "valid");
        run_resolver_child(
            "tests::ascii_launcher_exit_remains_an_error",
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
}
