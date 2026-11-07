include!("../src/java_plugins.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;
    use tokio::io::BufReader;

    #[test]
    fn stderr_decoder_preserves_unicode_at_every_fragment_boundary() {
        let text = "Привет 🌍 — 你好, café!";
        for width in 1..=text.len() {
            let mut decoder = StderrDecoder::default();
            let mut decoded = String::new();
            for fragment in text.as_bytes().chunks(width) {
                decoded.push_str(&decoder.push(fragment));
            }
            decoded.push_str(&decoder.finish());
            assert_eq!(decoded, text, "fragment width {width}");
        }
        for split in 0..=text.len() {
            let mut decoder = StderrDecoder::default();
            let mut decoded = decoder.push(&text.as_bytes()[..split]);
            decoded.push_str(&decoder.push(&text.as_bytes()[split..]));
            decoded.push_str(&decoder.finish());
            assert_eq!(decoded, text, "split {split}");
        }
        let text = format!("{}🌍Привет", "x".repeat(4095));
        let mut decoder = StderrDecoder::default();
        let mut decoded = decoder.push(&text.as_bytes()[..4096]);
        decoded.push_str(&decoder.push(&text.as_bytes()[4096..]));
        decoded.push_str(&decoder.finish());
        assert_eq!(decoded, text);
    }

    #[test]
    fn stderr_decoder_handles_malformed_bytes_and_emits_one_truncated_eof_replacement() {
        let mut decoder = StderrDecoder::default();
        let mut decoded = String::new();
        for fragment in [b"A\xe2".as_slice(), b"\x82B\xff\xf0", b"\x9f"] {
            decoded.push_str(&decoder.push(fragment));
        }
        decoded.push_str(&decoder.finish());
        assert_eq!(decoded, "A�B��");
        assert_eq!(decoder.finish(), "");
        let mut decoder = StderrDecoder::default();
        for byte in [0xf0, 0x9f, 0x8c] {
            assert_eq!(decoder.push(&[byte]), "");
            assert!(decoder.pending_len <= 3);
        }
        assert_eq!(decoder.push(&[0x8d]), "🌍");
        assert_eq!(decoder.pending_len, 0);
        let mut decoder = StderrDecoder::default();
        assert_eq!(decoder.push(b"\xe2\x28\xa1"), "�(�");
        assert_eq!(decoder.push(b"ok"), "ok");
        assert_eq!(decoder.finish(), "");
    }

    fn fixture_jar() -> &'static std::path::Path {
        static FIXTURE: OnceLock<tempfile::TempDir> = OnceLock::new();
        let root = FIXTURE.get_or_init(|| {
            let root = tempfile::tempdir().unwrap();
            let classes = root.path().join("classes");
            std::fs::create_dir(&classes).unwrap();
            let status = std::process::Command::new("javac")
                .args(["--release", "25", "-d"])
                .arg(&classes)
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/java-host/PluginHost.java"
                ))
                .status()
                .unwrap();
            assert!(status.success());
            let java = std::process::Command::new("java")
                .arg("-XshowSettings:properties")
                .arg("-version")
                .output()
                .unwrap();
            let properties = String::from_utf8_lossy(&java.stderr);
            let home = properties
                .lines()
                .find_map(|line| line.trim().strip_prefix("java.home = "))
                .unwrap();
            let jar =
                PathBuf::from(home)
                    .join("bin")
                    .join(if cfg!(windows) { "jar.exe" } else { "jar" });
            let status = std::process::Command::new(jar)
                .arg("--create")
                .arg("--file")
                .arg(root.path().join("fixture.jar"))
                .arg("-C")
                .arg(classes)
                .arg(".")
                .status()
                .unwrap();
            assert!(status.success());
            root
        });
        root.path()
    }

    fn config(mode: &str, directory: &std::path::Path) -> JavaPluginConfig {
        JavaPluginConfig {
            java: "java".into(),
            host_jar: fixture_jar().join("fixture.jar"),
            gson_jar: fixture_jar().join("fixture.jar"),
            extra_classpath: Vec::new(),
            directory: directory.join(format!("java-fixture-{mode}")),
            server_version: "0.0.1".into(),
            request_timeout: Duration::from_secs(5),
            startup_timeout: Duration::from_secs(15),
            queue_capacity: 4,
        }
    }

    #[tokio::test]
    async fn java_subprocess_loads_dispatches_and_shuts_down() {
        let directory = tempfile::tempdir().unwrap();
        let mut plugins = JavaPlugins::spawn(config("echo", directory.path()))
            .await
            .unwrap();
        let startup = plugins.take_startup_results();
        assert_eq!(startup[0].name, "fixture");
        assert!(
            matches!(&startup[0].actions[0], Action::RegisterCommand { name, .. } if name == "jhello")
        );
        let result = plugins
            .dispatch(
                Event::Command {
                    sender: "console".into(),
                    sender_kind: zerqon_plugin_api::SenderKind::Console,
                    operator: false,
                    command: "jhello".into(),
                    args: vec![],
                },
                Some("fixture".into()),
            )
            .await
            .unwrap();
        assert_eq!(
            result[0].actions,
            [Action::Reply {
                message: "received".into()
            }]
        );
        assert!(plugins.list().await.unwrap()[0].enabled);
        assert!(!plugins.shutdown().await.unwrap()[0].enabled);
        assert!(plugins.list().await.is_err());
    }

    #[tokio::test]
    async fn java_host_stdio_is_utf8_and_protocol_preserves_cyrillic() {
        let directory = tempfile::tempdir().unwrap();
        let plugins = JavaPlugins::spawn(config("charset", directory.path()))
            .await
            .unwrap();
        let results = plugins
            .dispatch(Event::Tick { tick: 1 }, None)
            .await
            .unwrap();
        assert_eq!(
            results[0].actions,
            [Action::Reply {
                message: "Привет из Java".into()
            }]
        );
        plugins.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalid_host_replies_or_eof_stop_the_host() {
        for mode in ["oversize", "wrong_id", "bad_json", "eof", "bad_action"] {
            let directory = tempfile::tempdir().unwrap();
            let plugins = JavaPlugins::spawn(config(mode, directory.path()))
                .await
                .unwrap();
            let result = plugins.dispatch(Event::Tick { tick: 1 }, None).await;
            assert!(
                matches!(
                    result,
                    Err(JavaPluginError::Protocol(_)) | Err(JavaPluginError::Stopped)
                ),
                "mode={mode}, result={result:?}"
            );
            assert!(plugins.list().await.is_err(), "mode={mode}");
        }
    }

    #[tokio::test]
    async fn timed_out_callback_terminates_host_and_rejects_further_work() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = config("timeout", directory.path());
        settings.request_timeout = Duration::from_millis(150);
        let plugins = JavaPlugins::spawn(settings).await.unwrap();
        let started = Instant::now();
        assert!(matches!(
            plugins.dispatch(Event::Tick { tick: 0 }, None).await,
            Err(JavaPluginError::Timeout)
        ));
        assert!(plugins.list().await.is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn saturated_request_queue_rejects_new_requests_immediately() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = config("timeout", directory.path());
        settings.request_timeout = Duration::from_millis(300);
        settings.queue_capacity = 1;
        let plugins = JavaPlugins::spawn(settings).await.unwrap();
        let (first, second, third) = tokio::join!(
            plugins.dispatch(Event::Tick { tick: 1 }, None),
            plugins.dispatch(Event::Tick { tick: 2 }, None),
            plugins.dispatch(Event::Tick { tick: 3 }, None)
        );
        assert!(matches!(first, Err(JavaPluginError::Timeout)));
        assert!(matches!(second, Err(JavaPluginError::QueueFull)));
        assert!(matches!(third, Err(JavaPluginError::QueueFull)));
    }

    #[tokio::test]
    async fn startup_timeout_is_enforced_before_host_is_exposed() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = config("startup_timeout", directory.path());
        settings.startup_timeout = Duration::from_millis(500);
        let started = Instant::now();
        assert!(matches!(
            JavaPlugins::spawn(settings).await,
            Err(JavaPluginError::Timeout)
        ));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn dropping_bridge_terminates_the_actual_jvm() {
        let directory = tempfile::tempdir().unwrap();
        let plugins = JavaPlugins::spawn(config("pid", directory.path()))
            .await
            .unwrap();
        let result = plugins
            .dispatch(Event::Tick { tick: 1 }, None)
            .await
            .unwrap();
        let Action::Reply { message } = &result[0].actions[0] else {
            panic!("missing PID reply")
        };
        let pid: u32 = message.parse().unwrap();
        drop(plugins);
        let started = Instant::now();
        loop {
            let alive = if cfg!(windows) {
                let output = std::process::Command::new("tasklist")
                    .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
                    .output()
                    .unwrap();
                String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
            } else {
                std::process::Command::new("kill")
                    .args(["-0", &pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .unwrap()
                    .success()
            };
            if !alive {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "JVM {pid} survived bridge drop"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn stderr_flood_is_drained_without_blocking_protocol_responses() {
        let directory = tempfile::tempdir().unwrap();
        let plugins = JavaPlugins::spawn(config("stderr", directory.path()))
            .await
            .unwrap();
        let result = plugins
            .dispatch(Event::Tick { tick: 1 }, None)
            .await
            .unwrap();
        assert_eq!(
            result[0].actions,
            [Action::Reply {
                message: "received".into()
            }]
        );
        plugins.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn oversized_request_is_rejected_without_poisoning_host() {
        let directory = tempfile::tempdir().unwrap();
        let plugins = JavaPlugins::spawn(config("echo", directory.path()))
            .await
            .unwrap();
        let result = plugins
            .dispatch(
                Event::Command {
                    sender: "console".into(),
                    sender_kind: zerqon_plugin_api::SenderKind::Console,
                    operator: false,
                    command: "jhello".into(),
                    args: vec!["x".repeat(65_536)],
                },
                None,
            )
            .await;
        assert!(matches!(result, Err(JavaPluginError::RequestTooLarge)));
        assert!(plugins.list().await.unwrap()[0].enabled);
        plugins.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "Requires Java 25 and python scripts/build-java-plugins.py; run cargo test --test java_plugins -- --ignored"]
    async fn real_java_host_loads_and_executes_compiled_plugin() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let directory = tempfile::tempdir().unwrap();
        let plugin = root.join("dist/hello-java-plugin-0.0.1.jar");
        assert!(
            plugin.is_file(),
            "Run python scripts/build-java-plugins.py before Java integration tests"
        );
        std::fs::copy(plugin, directory.path().join("hello.jar")).unwrap();
        let settings = JavaPluginConfig {
            java: "java".into(),
            host_jar: root.join("libraries/zerqon-java-host-0.0.1.jar"),
            gson_jar: root.join("libraries/gson-2.13.2.jar"),
            extra_classpath: Vec::new(),
            directory: directory.path().to_owned(),
            server_version: "0.0.1".into(),
            request_timeout: Duration::from_secs(5),
            startup_timeout: Duration::from_secs(15),
            queue_capacity: 4,
        };
        let mut plugins = JavaPlugins::spawn(settings).await.unwrap();
        let startup = plugins.take_startup_results();
        let owner = startup.iter().find(|plugin| plugin.actions.iter().any(|action| matches!(action, Action::RegisterCommand { name, .. } if name == "jhello"))).unwrap();
        let result = plugins
            .dispatch(
                Event::Command {
                    sender: "RustBridge".into(),
                    sender_kind: zerqon_plugin_api::SenderKind::Console,
                    operator: false,
                    command: "jhello".into(),
                    args: vec![],
                },
                Some(owner.name.clone()),
            )
            .await
            .unwrap();
        assert!(result.iter().flat_map(|plugin| &plugin.actions).any(|action| matches!(action, Action::Reply { message } if message == "Hello, RustBridge! This plugin uses the Zerqon Java API.")));
        plugins.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn bounded_reader_preserves_buffered_lines_and_crlf() {
        let mut reader = BufReader::new(&b"first\r\nsecond\n"[..]);
        assert_eq!(read_line_limited(&mut reader, 10).await.unwrap(), b"first");
        assert_eq!(read_line_limited(&mut reader, 10).await.unwrap(), b"second");
        assert!(matches!(
            read_line_limited(&mut reader, 10).await,
            Err(JavaPluginError::Stopped)
        ));
    }

    #[test]
    fn extra_classpath_is_bounded_canonical_and_precedes_legacy_gson() {
        let directory = tempfile::tempdir().unwrap();
        let host = directory.path().join("host.jar");
        let gson = directory.path().join("gson.jar");
        let paper = directory.path().join("paper.jar");
        for path in [&host, &gson, &paper] {
            std::fs::write(path, []).unwrap();
        }
        let mut settings = JavaPluginConfig {
            java: "java".into(),
            host_jar: host.clone(),
            gson_jar: gson.clone(),
            extra_classpath: vec![paper.clone()],
            directory: directory.path().into(),
            server_version: "0.0.1".into(),
            request_timeout: Duration::from_secs(2),
            startup_timeout: Duration::from_secs(2),
            queue_capacity: 1,
        };
        let paths: Vec<_> = std::env::split_paths(&java_classpath(&settings).unwrap()).collect();
        assert_eq!(
            paths,
            [&host, &paper, &gson].map(|path| std::fs::canonicalize(path).unwrap())
        );
        settings.extra_classpath = vec![paper.clone(); 65];
        assert!(java_classpath(&settings).is_err());
        settings.extra_classpath = vec![directory.path().into()];
        assert!(java_classpath(&settings).is_err());
        settings.extra_classpath = vec![directory.path().join("missing.jar")];
        assert!(java_classpath(&settings).is_err());
        let oversized = std::fs::File::create(&paper).unwrap();
        oversized.set_len(40 * 1024 * 1024 + 1).unwrap();
        settings.extra_classpath = vec![paper];
        assert!(java_classpath(&settings).is_err());
    }

    #[tokio::test]
    #[ignore = "Requires Java25 and python scripts/build-java-plugins.py"]
    async fn rust_bridge_loads_original_paper_api_plugin_and_enforces_permissions() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let directory = tempfile::tempdir().unwrap();
        let plugin_dir = directory.path().join("plugins/java-plugins");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::copy(
            root.join("target/paper-bridge/paper-probe.jar"),
            plugin_dir.join("paper.jar"),
        )
        .unwrap();
        let mut extra: Vec<_> = std::fs::read_dir(root.join("libraries/paper"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "jar"))
            .collect();
        extra.sort();
        let settings = JavaPluginConfig {
            java: "java".into(),
            host_jar: root.join("libraries/zerqon-java-host-0.0.1.jar"),
            gson_jar: root.join("libraries/gson-2.13.2.jar"),
            extra_classpath: extra,
            directory: plugin_dir,
            server_version: "0.0.1".into(),
            request_timeout: Duration::from_secs(5),
            startup_timeout: Duration::from_secs(15),
            queue_capacity: 4,
        };
        let mut plugins = JavaPlugins::spawn(settings).await.unwrap();
        let startup = plugins.take_startup_results();
        assert!(startup[0].enabled, "{startup:?}");
        assert!(startup[0].actions.iter().any(|action| matches!(action, Action::RegisterCommand { name, permission: Some(permission), .. } if name == "probe" && permission == "zerqon.probe")));
        for tick in 1..=3 {
            plugins.dispatch(Event::Tick { tick }, None).await.unwrap();
        }
        let reply = plugins
            .dispatch(
                Event::Command {
                    sender: "NativeOperator".into(),
                    sender_kind: zerqon_plugin_api::SenderKind::Player,
                    operator: true,
                    command: "p".into(),
                    args: Vec::new(),
                },
                Some("paper".into()),
            )
            .await
            .unwrap();
        assert_eq!(
            reply[0].actions,
            [Action::Reply {
                message: "hello:2:1:NativeOperator:p".into()
            }]
        );
        let denied = plugins
            .dispatch(
                Event::Command {
                    sender: "CONSOLE".into(),
                    sender_kind: zerqon_plugin_api::SenderKind::Player,
                    operator: false,
                    command: "p".into(),
                    args: Vec::new(),
                },
                Some("paper".into()),
            )
            .await
            .unwrap();
        assert_eq!(
            denied[0].actions,
            [Action::Reply {
                message: "Permission denied".into()
            }]
        );
        assert!(
            std::fs::read_to_string(directory.path().join("config/PaperProbe/config.yml"))
                .unwrap()
                .contains("enabled: true")
        );
        assert!(!plugins.shutdown().await.unwrap()[0].enabled);
    }

    #[tokio::test]
    async fn bounded_reader_rejects_oversized_and_unterminated_lines() {
        let mut reader = BufReader::new(&b"123456789\n"[..]);
        assert!(matches!(
            read_line_limited(&mut reader, 8).await,
            Err(JavaPluginError::Protocol(_))
        ));
        let mut reader = BufReader::new(&b"unterminated"[..]);
        assert!(matches!(
            read_line_limited(&mut reader, 64).await,
            Err(JavaPluginError::Protocol(_))
        ));
    }
}
