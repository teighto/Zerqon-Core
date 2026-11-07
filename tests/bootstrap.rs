include!("../src/bootstrap.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn pinned_download_verification_checks_length_and_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        std::fs::write(&path, b"abc").unwrap();
        assert!(verify_file(&path, 3, "a9993e364706816aba3e25717850c26c9cd0d89d").unwrap());
        assert!(!verify_file(&path, 4, "a9993e364706816aba3e25717850c26c9cd0d89d").unwrap());
        assert!(!verify_file(&path, 3, SERVER_SHA1).unwrap());
    }
    #[test]
    fn tags_resolve_nested_optional_and_duplicate_entries_in_source_order() {
        let definitions = BTreeMap::from([
            (
                "minecraft:first".into(),
                vec![json!("stone"), json!("minecraft:dirt"), json!("stone")],
            ),
            (
                "minecraft:second".into(),
                vec![
                    json!("#first"),
                    json!({"id":"missing","required":false}),
                    json!("minecraft:water"),
                ],
            ),
        ]);
        let ids = BTreeMap::from([
            ("minecraft:stone".into(), 2),
            ("minecraft:dirt".into(), 5),
            ("minecraft:water".into(), 8),
        ]);
        let tags = resolve_tags(&definitions, &ids).unwrap();
        assert_eq!(tags["minecraft:second"], [2, 5, 8]);
    }
    #[test]
    fn tags_reject_cycles_and_missing_required_references() {
        let ids = BTreeMap::new();
        for definitions in [
            BTreeMap::from([("minecraft:a".into(), vec![json!("#a")])]),
            BTreeMap::from([("minecraft:a".into(), vec![json!("missing")])]),
        ] {
            assert!(resolve_tags(&definitions, &ids).is_err());
        }
    }
    #[test]
    fn archive_paths_cannot_escape_or_target_windows_devices() {
        for name in [
            "/data/file.json",
            "../file.json",
            "data/../file.json",
            "data\\file.json",
            "C:/file.json",
            "data/NUL.json",
            "data/a:stream.json",
            "data/file.",
        ] {
            assert!(safe_archive_path(name).is_err(), "{name}");
        }
        assert_eq!(
            safe_archive_path("data/minecraft/worldgen/biome/plains.json").unwrap(),
            ["data", "minecraft", "worldgen", "biome", "plains.json"]
        );
    }

    async fn serve_once(bytes: Vec<u8>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket.write_all(&bytes).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        format!("http://{address}/fixture")
    }

    #[tokio::test]
    async fn download_verifies_stream_before_publishing_and_preserves_existing_files() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("server.jar");
        let sha = "a9993e364706816aba3e25717850c26c9cd0d89d";
        let url = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc".to_vec(),
        )
        .await;
        download(&destination, &url, 3, sha).await.unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"abc");
        let url = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nxyz".to_vec(),
        )
        .await;
        assert!(download(&destination, &url, 3, sha).await.is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"abc");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn download_rejects_chunked_oversize_truncation_and_http_errors() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("server.jar");
        let sha = "a9993e364706816aba3e25717850c26c9cd0d89d";
        for response in [b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\nabcd\r\n0\r\n\r\n".as_slice(),b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nab",b"HTTP/1.1 404 Not Found\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc"]{
            let url=serve_once(response.to_vec()).await;assert!(download(&destination,&url,3,sha).await.is_err());assert!(!destination.exists());
        }
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn registry_order_and_tags_match_minecraft_relative_path_then_namespace_order() {
        let data = BTreeMap::from([
            (
                "zeta/worldgen/biome/a.json".into(),
                json!({"temperature":1}),
            ),
            (
                "alpha/worldgen/biome/z.json".into(),
                json!({"temperature":2}),
            ),
            (
                "minecraft/tags/worldgen/biome/test.json".into(),
                json!({"values":["zeta:a","alpha:z"]}),
            ),
        ]);
        let registries = collect_registries(&data).unwrap();
        let biomes = registries
            .iter()
            .find(|registry| registry["name"] == "minecraft:worldgen/biome")
            .unwrap();
        assert_eq!(biomes["entries"][0]["id"], "zeta:a");
        assert_eq!(biomes["entries"][1]["id"], "alpha:z");
        let tags = collect_tags(&data, &json!({}), &registries).unwrap();
        assert_eq!(tags[0]["tags"][0]["ids"], json!([0, 1]));
    }

    #[test]
    fn matching_identity_does_not_make_malformed_protocol_reports_a_valid_cache() {
        let directory = tempfile::tempdir().unwrap();
        let reports = directory.path().join("generated/reports");
        fs::create_dir_all(&reports).unwrap();
        fs::write(directory.path().join("prepared.json"),serde_json::to_vec(&json!({"version":VERSION,"protocol":PROTOCOL,"server_sha1":SERVER_SHA1,"world_version":5023,"java_major":25,"registries":32,"entries":0,"tag_registries":1,"tags":0})).unwrap()).unwrap();
        for name in REPORTS {
            fs::write(reports.join(name), b"{\"garbage\":true}").unwrap();
        }
        fs::write(directory.path().join("registries-sync.json"),serde_json::to_vec(&json!({"version":VERSION,"protocol":PROTOCOL,"registries":REGISTRIES.iter().map(|name|json!({"name":format!("minecraft:{name}"),"entries":[]})).collect::<Vec<_>>()})).unwrap()).unwrap();
        fs::write(
            directory.path().join("tags-sync.json"),
            b"{\"registries\":[{\"name\":\"minecraft:block\",\"tags\":[]}]}",
        )
        .unwrap();
        assert!(!cache_ready(directory.path()).unwrap());
    }

    #[tokio::test]
    #[ignore = "Requires official cached .cache/vanilla/26.3/server.jar, Python-reference outputs and Java25; no Python is invoked by bootstrap"]
    async fn native_prepare_matches_reference_and_reuses_cache_without_java() {
        let reference = Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/vanilla/26.3");
        let directory = tempfile::tempdir().unwrap();
        fs::copy(
            reference.join("server.jar"),
            directory.path().join("server.jar"),
        )
        .unwrap();
        let prepared = Bootstrap::prepare(directory.path(), Path::new("java"))
            .await
            .unwrap();
        assert!(!prepared.reused);
        assert_eq!(prepared.directory, directory.path().canonicalize().unwrap());
        for name in ["prepared.json", "registries-sync.json", "tags-sync.json"] {
            assert_eq!(
                read_json(&directory.path().join(name), 16 * 1024 * 1024).unwrap(),
                read_json(&reference.join(name), 16 * 1024 * 1024).unwrap(),
                "{name}"
            );
        }
        for name in REPORTS {
            assert_eq!(
                read_json(
                    &directory.path().join("generated/reports").join(name),
                    16 * 1024 * 1024
                )
                .unwrap(),
                read_json(
                    &reference.join("generated/reports").join(name),
                    16 * 1024 * 1024
                )
                .unwrap(),
                "{name}"
            );
        }
        let reused = Bootstrap::prepare(directory.path(), Path::new("nonexistent-java-runtime"))
            .await
            .unwrap();
        assert!(reused.reused);
    }

    #[tokio::test]
    #[ignore = "Downloads the pinned 62 MB official server over HTTPS and requires Java25"]
    async fn native_first_boot_downloads_and_prepares_official_data() {
        let directory = tempfile::tempdir().unwrap();
        let prepared = Bootstrap::prepare(directory.path(), Path::new("java"))
            .await
            .unwrap();
        assert!(!prepared.reused);
        assert!(
            verify_file(
                &directory.path().join("server.jar"),
                SERVER_BYTES,
                SERVER_SHA1
            )
            .unwrap()
        );
        assert!(cache_ready(directory.path()).unwrap());
    }

    #[tokio::test]
    #[ignore = "Requires existing verified .cache/vanilla/26.3; exercises concurrent read-only cache reuse"]
    async fn concurrent_servers_can_reuse_the_same_verified_cache() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/vanilla/26.3");
        let (first, second) = tokio::join!(
            Bootstrap::prepare(&directory, Path::new("nonexistent-java")),
            Bootstrap::prepare(&directory, Path::new("nonexistent-java"))
        );
        assert!(first.unwrap().reused);
        assert!(second.unwrap().reused);
    }

    #[tokio::test]
    async fn cache_lock_waits_for_owner_release_and_times_out_when_owner_stays() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("prepare.lock");
        let first = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&path)
            .unwrap();
        first.try_lock().unwrap();
        let second = fs::OpenOptions::new()
            .write(true)
            .read(true)
            .open(&path)
            .unwrap();
        assert!(
            acquire_prepare_lock(&second, Duration::from_millis(50))
                .await
                .is_err()
        );
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(first);
        });
        acquire_prepare_lock(&second, Duration::from_secs(2))
            .await
            .unwrap();
        release.await.unwrap();
    }

    #[tokio::test]
    async fn report_generator_runs_with_utf8_stdout_stderr_and_unicode_output_path() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("bootstrap=debug")
            .with_test_writer()
            .try_init();
        let directory = tempfile::tempdir().unwrap();
        let java = java_runtime(Path::new("java")).await.unwrap();
        let bin = java.parent().unwrap();
        let classes = directory.path().join("classes");
        fs::create_dir(&classes).unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/java-bootstrap/ReportCharsetProbe.java");
        let compiled =
            std::process::Command::new(bin.join(if cfg!(windows) { "javac.exe" } else { "javac" }))
                .args(["--release", "25", "-encoding", "UTF-8", "-d"])
                .arg(&classes)
                .arg(source)
                .output()
                .unwrap();
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let jar = directory.path().join("generator.jar");
        let packaged =
            std::process::Command::new(bin.join(if cfg!(windows) { "jar.exe" } else { "jar" }))
                .args(["--create", "--file"])
                .arg(&jar)
                .args(["--main-class", "ReportCharsetProbe", "-C"])
                .arg(classes)
                .arg(".")
                .output()
                .unwrap();
        assert!(
            packaged.status.success(),
            "{}",
            String::from_utf8_lossy(&packaged.stderr)
        );
        let stage = directory.path().join("данные-Minecraft");
        fs::create_dir(&stage).unwrap();
        generate_reports(&java, &jar, &stage).await.unwrap();
        assert_eq!(
            fs::read_to_string(stage.join("generated/charset-result.txt")).unwrap(),
            "Привет из генератора"
        );
    }
}
