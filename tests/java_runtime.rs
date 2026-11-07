include!("../src/java_runtime.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
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
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("среда-Java");
        let asset = current_asset().unwrap();
        let java = install_managed(&cache, asset).await.unwrap();
        assert!(
            java.canonicalize()
                .unwrap()
                .starts_with(directory.path().canonicalize().unwrap())
        );
        assert_eq!(probe_java(&java).await.unwrap(), java);
        assert_eq!(install_managed(&cache, asset).await.unwrap(), java);
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
