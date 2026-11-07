#[path = "../src/java_launch.rs"]
mod java_launch;
#[path = "../src/plugin_services.rs"]
mod plugin_services;

include!("../src/java_plugins.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::java_launch::classpath_manifest;
    use crate::plugin_services::ServerSnapshot;
    use std::sync::OnceLock;
    use tokio::io::BufReader;

    impl JavaPlugins {
        async fn spawn(config: JavaPluginConfig) -> Result<Self, JavaPluginError> {
            Self::spawn_internal(config, None).await
        }
    }

    struct SnapshotService;
    impl plugin_services::NativeQueryService for SnapshotService {
        fn query(
            &self,
            context: plugin_services::NativeCallContext,
            query: plugin_services::NativeQuery,
        ) -> plugin_services::NativeQueryFuture<'_> {
            assert_eq!(context.plugin, "fixture");
            assert!(context.parent_id > 0 && context.call_id > 0);
            assert!(context.deadline > Instant::now());
            assert_eq!(query, plugin_services::NativeQuery::ServerSnapshot);
            Box::pin(async {
                Ok(plugin_services::ServerSnapshot {
                    server_version: "0.0.1".into(),
                    minecraft_version: "26.3".into(),
                    protocol: 777,
                    max_players: 64,
                    online_players: 2,
                    tick: 123,
                }
                .into())
            })
        }
    }

    fn snapshot() -> plugin_services::ServerSnapshot {
        plugin_services::ServerSnapshot {
            server_version: "0.0.1".into(),
            minecraft_version: "26.3".into(),
            protocol: 777,
            max_players: 64,
            online_players: 2,
            tick: 123,
        }
    }

    struct ObservedService {
        calls: std::sync::atomic::AtomicUsize,
        dropped: Arc<std::sync::atomic::AtomicUsize>,
        delay: Duration,
        result: Result<ServerSnapshot, NativeQueryError>,
        contexts: std::sync::Mutex<Vec<(String, u64, u64)>>,
    }

    impl ObservedService {
        fn new(delay: Duration, result: Result<ServerSnapshot, NativeQueryError>) -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                dropped: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                delay,
                result,
                contexts: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    impl NativeQueryService for ObservedService {
        fn query(
            &self,
            context: NativeCallContext,
            _: NativeQuery,
        ) -> plugin_services::NativeQueryFuture<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.contexts.lock().unwrap().push((
                context.plugin,
                context.parent_id,
                context.call_id,
            ));
            struct QueryDrop(Arc<std::sync::atomic::AtomicUsize>);
            impl Drop for QueryDrop {
                fn drop(&mut self) {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
            }
            let guard = QueryDrop(Arc::clone(&self.dropped));
            Box::pin(async move {
                let _guard = guard;
                tokio::time::sleep(self.delay).await;
                self.result.clone().map(NativeQueryValue::from)
            })
        }
    }

    fn native_config(mode: &str, directory: &std::path::Path) -> JavaPluginConfig {
        let settings = config(mode, directory);
        std::fs::create_dir_all(&settings.directory).unwrap();
        std::fs::copy(&settings.host_jar, settings.directory.join("fixture.jar")).unwrap();
        settings
    }

    #[tokio::test]
    async fn oversized_valid_player_query_returns_capacity_without_killing_the_host() {
        use crate::plugin_services::{MAX_QUERY_PLAYERS, NativePlayer, NativePosition};
        struct LargePlayers;
        impl NativeQueryService for LargePlayers {
            fn query(
                &self,
                _: NativeCallContext,
                _: NativeQuery,
            ) -> plugin_services::NativeQueryFuture<'_> {
                Box::pin(async {
                    Ok(NativeQueryValue::OnlinePlayers {
                        players: (1..=MAX_QUERY_PLAYERS)
                            .map(|index| NativePlayer {
                                uuid: uuid::Uuid::from_u128(index as u128),
                                session_id: index as i32,
                                entity_id: index as i32,
                                name: format!("Player{index:010}"),
                                world_uuid: uuid::Uuid::from_u128(99999),
                                position: NativePosition {
                                    x: 29999999.125,
                                    y: 9999.875,
                                    z: -29999999.625,
                                    yaw: 89.125,
                                    pitch: -81.875,
                                },
                                operator: false,
                                online: true,
                            })
                            .collect(),
                    })
                })
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let plugins = JavaPlugins::spawn_with_services(
            native_config("native_players", directory.path()),
            Arc::new(LargePlayers),
        )
        .await
        .unwrap();
        let reply = plugins
            .dispatch(Event::Tick { tick: 1 }, None)
            .await
            .unwrap();
        assert_eq!(
            reply[0].actions,
            [Action::Reply {
                message: "native-error".into()
            }]
        );
        assert!(plugins.list().await.unwrap()[0].enabled);
        plugins.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalid_player_event_context_is_rejected_before_host_dispatch() {
        use crate::plugin_services::{NativePlayer, NativePosition};
        let directory = tempfile::tempdir().unwrap();
        let plugins = JavaPlugins::spawn_with_services(
            native_config("echo", directory.path()),
            Arc::new(SnapshotService),
        )
        .await
        .unwrap();
        let context = NativeEventContext {
            player: NativePlayer {
                uuid: uuid::Uuid::from_u128(1),
                session_id: 7,
                entity_id: 7,
                name: "CONSOLE".into(),
                world_uuid: uuid::Uuid::from_u128(2),
                position: NativePosition {
                    x: 0.5,
                    y: 64.0,
                    z: 0.5,
                    yaw: 0.0,
                    pitch: 0.0,
                },
                operator: false,
                online: true,
            },
        };
        assert!(matches!(
            plugins
                .dispatch_with_context(native_command("read"), None, context.clone())
                .await,
            Err(JavaPluginError::Configuration(_))
        ));
        let command = Event::Command {
            sender: "CONSOLE".into(),
            sender_kind: zerqon_plugin_api::SenderKind::Player,
            operator: false,
            command: "read".into(),
            args: Vec::new(),
        };
        assert_eq!(
            plugins
                .dispatch_with_context(command, None, context)
                .await
                .unwrap()[0]
                .actions,
            [Action::Reply {
                message: "received".into()
            }]
        );
        assert!(plugins.list().await.unwrap()[0].enabled);
        plugins.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn negotiated_native_query_roundtrip_preserves_parent_response() {
        let directory = tempfile::tempdir().unwrap();
        let settings = native_config("native", directory.path());
        let plugins =
            JavaPlugins::spawn_with_services(settings, std::sync::Arc::new(SnapshotService))
                .await
                .unwrap();
        let response = plugins
            .dispatch(Event::Tick { tick: 5 }, Some("fixture".into()))
            .await
            .unwrap();
        assert_eq!(
            response[0].actions,
            [Action::Reply {
                message: "snapshot:123:2".into()
            }]
        );
        assert!(plugins.list().await.unwrap()[0].enabled);
        plugins.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn legacy_host_rejects_unsolicited_native_queries() {
        let directory = tempfile::tempdir().unwrap();
        let plugins = JavaPlugins::spawn(config("native", directory.path()))
            .await
            .unwrap();
        assert!(matches!(
            plugins.dispatch(Event::Tick { tick: 1 }, None).await,
            Err(JavaPluginError::Protocol(_))
        ));
        assert!(plugins.list().await.is_err());
    }

    #[tokio::test]
    async fn native_host_rejects_hostile_frames_before_calling_services() {
        for mode in [
            "native_wrong_parent",
            "native_wrong_id",
            "native_unknown_plugin",
            "native_unknown_query",
            "native_unknown_field",
            "native_duplicate_field",
            "native_oversize",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let service = ObservedService::new(Duration::ZERO, Ok(snapshot()));
            let plugins = JavaPlugins::spawn_with_services(
                native_config(mode, directory.path()),
                service.clone(),
            )
            .await
            .unwrap();
            assert!(
                matches!(
                    plugins
                        .dispatch(Event::Tick { tick: 1 }, Some("fixture".into()))
                        .await,
                    Err(JavaPluginError::Protocol(_))
                ),
                "{mode}"
            );
            assert_eq!(service.calls.load(Ordering::SeqCst), 0, "{mode}");
            assert!(plugins.list().await.is_err(), "{mode}");
        }
    }

    #[tokio::test]
    async fn native_host_rejects_duplicate_and_excessive_calls_without_reexecution() {
        for (mode, expected) in [("native_duplicate", 1), ("native_limit", 128)] {
            let directory = tempfile::tempdir().unwrap();
            let service = ObservedService::new(Duration::ZERO, Ok(snapshot()));
            let plugins = JavaPlugins::spawn_with_services(
                native_config(mode, directory.path()),
                service.clone(),
            )
            .await
            .unwrap();
            assert!(
                matches!(
                    plugins.dispatch(Event::Tick { tick: 1 }, None).await,
                    Err(JavaPluginError::Protocol(_))
                ),
                "{mode}"
            );
            assert_eq!(service.calls.load(Ordering::SeqCst), expected);
            assert_eq!(service.dropped.load(Ordering::SeqCst), expected);
        }
    }

    #[tokio::test]
    async fn native_queries_are_forbidden_in_list_requests() {
        let directory = tempfile::tempdir().unwrap();
        let service = ObservedService::new(Duration::ZERO, Ok(snapshot()));
        let plugins = JavaPlugins::spawn_with_services(
            native_config("native_list", directory.path()),
            service.clone(),
        )
        .await
        .unwrap();
        assert!(matches!(
            plugins.list().await,
            Err(JavaPluginError::Protocol(_))
        ));
        assert_eq!(service.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn native_service_errors_and_subdeadlines_do_not_poison_the_host() {
        for (delay, result) in [
            (
                Duration::ZERO,
                Err(NativeQueryError::new(
                    NativeErrorCode::Unavailable,
                    "Unavailable",
                )),
            ),
            (Duration::from_secs(30), Ok(snapshot())),
            (
                Duration::ZERO,
                Ok(ServerSnapshot {
                    online_players: 65,
                    ..snapshot()
                }),
            ),
            (
                Duration::ZERO,
                Err(NativeQueryError::new(
                    NativeErrorCode::Internal,
                    "x".repeat(4096),
                )),
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let service = ObservedService::new(delay, result);
            let plugins = JavaPlugins::spawn_with_services(
                native_config("native_error", directory.path()),
                service.clone(),
            )
            .await
            .unwrap();
            let result = plugins
                .dispatch(Event::Tick { tick: 1 }, None)
                .await
                .unwrap();
            assert_eq!(
                result[0].actions,
                [Action::Reply {
                    message: "native-error".into()
                }]
            );
            assert_eq!(service.calls.load(Ordering::SeqCst), 1);
            assert_eq!(service.dropped.load(Ordering::SeqCst), 1);
            assert!(plugins.list().await.unwrap()[0].enabled);
            plugins.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn native_queries_share_the_original_parent_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let service = ObservedService::new(Duration::from_millis(150), Ok(snapshot()));
        let mut settings = native_config("native_two", directory.path());
        settings.request_timeout = Duration::from_millis(240);
        let plugins = JavaPlugins::spawn_with_services(settings, service.clone())
            .await
            .unwrap();
        let started = Instant::now();
        assert!(matches!(
            plugins.dispatch(Event::Tick { tick: 1 }, None).await,
            Err(JavaPluginError::Timeout)
        ));
        assert!(plugins.list().await.is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(service.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cancelling_inflight_native_query_releases_service_future_and_stops_host() {
        let directory = tempfile::tempdir().unwrap();
        let service = ObservedService::new(Duration::from_secs(30), Ok(snapshot()));
        let plugins = Arc::new(
            JavaPlugins::spawn_with_services(
                native_config("native", directory.path()),
                service.clone(),
            )
            .await
            .unwrap(),
        );
        let requested = Arc::clone(&plugins);
        let request =
            tokio::spawn(async move { requested.dispatch(Event::Tick { tick: 1 }, None).await });
        let started = Instant::now();
        while service.calls.load(Ordering::SeqCst) == 0 {
            assert!(started.elapsed() < Duration::from_secs(3));
            tokio::task::yield_now().await;
        }
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        while service.dropped.load(Ordering::SeqCst) == 0 {
            assert!(started.elapsed() < Duration::from_secs(3));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(plugins.list().await.is_err());
    }

    #[tokio::test]
    async fn native_protocol_negotiation_rejects_legacy_mismatch_and_stalled_hosts() {
        for mode in [
            "no-protocol",
            "bad-protocol",
            "bad-capability",
            "native-before-hello",
            "stalled-protocol",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let service = ObservedService::new(Duration::ZERO, Ok(snapshot()));
            let mut settings = native_config("native", directory.path());
            let host = directory.path().join(format!("{mode}.jar"));
            std::fs::copy(&settings.host_jar, &host).unwrap();
            settings.host_jar = host;
            settings.startup_timeout = Duration::from_millis(500);
            let result = JavaPlugins::spawn_with_services(settings, service.clone()).await;
            if mode == "stalled-protocol" {
                assert!(matches!(result, Err(JavaPluginError::Timeout)), "{mode}");
            } else {
                assert!(
                    matches!(result, Err(JavaPluginError::Protocol(_))),
                    "{mode}"
                );
            }
            assert_eq!(service.calls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn disabled_plugins_remain_listable_but_cannot_query_native_services() {
        let directory = tempfile::tempdir().unwrap();
        let service = ObservedService::new(Duration::ZERO, Ok(snapshot()));
        let plugins = JavaPlugins::spawn_with_services(
            native_config("native_disabled", directory.path()),
            service.clone(),
        )
        .await
        .unwrap();
        assert!(
            !plugins
                .dispatch(Event::Tick { tick: 1 }, None)
                .await
                .unwrap()[0]
                .enabled
        );
        assert!(!plugins.list().await.unwrap()[0].enabled);
        assert!(matches!(
            plugins.dispatch(Event::Tick { tick: 2 }, None).await,
            Err(JavaPluginError::Protocol(_))
        ));
        assert_eq!(service.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn service_snapshots_and_errors_reject_invalid_values() {
        let mut value = snapshot();
        for invalid in ["", "line\nbreak"] {
            value.server_version = invalid.into();
            assert!(!value.valid());
        }
        value = snapshot();
        value.minecraft_version = "😀".repeat(33);
        assert!(!value.valid());
        value = snapshot();
        value.max_players = 0;
        assert!(!value.valid());
        value = snapshot();
        value.online_players = value.max_players + 1;
        assert!(!value.valid());
        value = snapshot();
        value.tick = i64::MAX as u64 + 1;
        assert!(!value.valid());
        assert!(!NativeQueryError::new(NativeErrorCode::Internal, "bad\u{0}error").valid());
        assert!(!NativeQueryError::new(NativeErrorCode::Internal, "x".repeat(1025)).valid());
        assert!(NativeQueryError::new(NativeErrorCode::Unavailable, "Не готово").valid());
    }

    fn real_native_config(directory: &std::path::Path) -> JavaPluginConfig {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        JavaPluginConfig {
            java: "java".into(),
            host_jar: root.join("libraries/zerqon-java-host-0.0.1.jar"),
            gson_jar: root.join("libraries/gson-2.13.2.jar"),
            extra_classpath: Vec::new(),
            directory: directory.to_owned(),
            server_version: "0.0.1".into(),
            request_timeout: Duration::from_secs(5),
            startup_timeout: Duration::from_secs(15),
            queue_capacity: 4,
        }
    }

    fn native_command(command: &str) -> Event {
        Event::Command {
            sender: "NativeOperator".into(),
            sender_kind: zerqon_plugin_api::SenderKind::Console,
            operator: true,
            command: command.into(),
            args: Vec::new(),
        }
    }

    #[tokio::test]
    #[ignore = "Requires python scripts/build-java-plugins.py"]
    async fn actual_java_sdk_queries_rust_services_and_restricts_callback_threads() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let directory = tempfile::tempdir().unwrap();
        std::fs::copy(
            root.join("target/java-plugins/native-query-plugin.jar"),
            directory.path().join("native.jar"),
        )
        .unwrap();
        let service = ObservedService::new(Duration::ZERO, Ok(snapshot()));
        let plugins =
            JavaPlugins::spawn_with_services(real_native_config(directory.path()), service.clone())
                .await
                .unwrap();
        for command in ["native", "twice"] {
            let reply = plugins
                .dispatch(native_command(command), Some("native".into()))
                .await
                .unwrap();
            assert!(reply[0].enabled, "{reply:?}");
            assert_eq!(
                reply[0].actions,
                [Action::Reply {
                    message: "0.0.1:26.3:777:64:2:123".into()
                }]
            );
        }
        let reply = plugins
            .dispatch(native_command("thread"), Some("native".into()))
            .await
            .unwrap();
        assert_eq!(
            reply[0].actions,
            [Action::Reply {
                message: "thread rejected".into()
            }]
        );
        assert_eq!(service.calls.load(Ordering::SeqCst), 3);
        let contexts = service.contexts.lock().unwrap().clone();
        assert_eq!(
            contexts
                .iter()
                .map(|(plugin, _, call)| (plugin.as_str(), *call))
                .collect::<Vec<_>>(),
            [("native", 1), ("native", 1), ("native", 2)]
        );
        assert_ne!(contexts[0].1, contexts[1].1);
        assert_eq!(contexts[1].1, contexts[2].1);
        plugins.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "Requires python scripts/build-java-plugins.py"]
    async fn actual_java_sdk_receives_typed_service_errors_without_losing_the_session() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let directory = tempfile::tempdir().unwrap();
        std::fs::copy(
            root.join("target/java-plugins/native-query-plugin.jar"),
            directory.path().join("native.jar"),
        )
        .unwrap();
        let service = ObservedService::new(
            Duration::ZERO,
            Err(NativeQueryError::new(
                NativeErrorCode::Unavailable,
                "Not ready",
            )),
        );
        let plugins =
            JavaPlugins::spawn_with_services(real_native_config(directory.path()), service.clone())
                .await
                .unwrap();
        let reply = plugins
            .dispatch(native_command("native"), Some("native".into()))
            .await
            .unwrap();
        assert!(reply[0].enabled);
        assert_eq!(
            reply[0].actions,
            [Action::Reply {
                message: "UNAVAILABLE".into()
            }]
        );
        assert!(plugins.list().await.unwrap()[0].enabled);
        plugins.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "Requires python scripts/build-java-plugins.py"]
    async fn actual_java_lifecycle_queries_preserve_file_identity_and_parent_budgets() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let directory = tempfile::tempdir().unwrap();
        for name in ["one", "two"] {
            std::fs::copy(
                root.join("target/java-plugins/native-lifecycle-plugin.jar"),
                directory.path().join(format!("{name}.jar")),
            )
            .unwrap();
        }
        let service = ObservedService::new(Duration::ZERO, Ok(snapshot()));
        let mut plugins =
            JavaPlugins::spawn_with_services(real_native_config(directory.path()), service.clone())
                .await
                .unwrap();
        assert_eq!(plugins.take_startup_results().len(), 2);
        assert_eq!(
            plugins
                .dispatch(Event::Tick { tick: 1 }, None)
                .await
                .unwrap()
                .len(),
            2
        );
        assert_eq!(plugins.shutdown().await.unwrap().len(), 2);
        let contexts = service.contexts.lock().unwrap().clone();
        assert_eq!(contexts.len(), 6);
        for pair in contexts.chunks(2) {
            assert_eq!(pair[0].0, "one");
            assert_eq!(pair[1].0, "two");
            assert_eq!(pair[0].2, 1);
            assert_eq!(pair[1].2, 2);
            assert_eq!(pair[0].1, pair[1].1);
        }
        assert!(contexts[0].1 < contexts[2].1 && contexts[2].1 < contexts[4].1);
    }

    #[tokio::test]
    #[ignore = "Requires python scripts/build-java-plugins.py"]
    async fn actual_paper_callbacks_keep_native_identity_through_nested_dispatch() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let directory = tempfile::tempdir().unwrap();
        for (filename, metadata) in [("file-a", "NativeA"), ("file-b", "NativeB")] {
            let mut jar = zip::ZipWriter::new(
                std::fs::File::create(directory.path().join(format!("{filename}.jar"))).unwrap(),
            );
            let options = zip::write::SimpleFileOptions::default();
            jar.start_file("plugin.yml", options).unwrap();
            std::io::Write::write_all(&mut jar, format!("name: {metadata}\nversion: 1\nmain: io.github.teighto.zerqon.fixtures.NativePaperProbe\napi-version: '26.3'\ncommands:\n  nativepaper: {{}}\n").as_bytes()).unwrap();
            for class in [
                "NativePaperProbe.class",
                "NativePaperProbe$ProbeEvent.class",
            ] {
                let path = format!("io/github/teighto/zerqon/fixtures/{class}");
                jar.start_file(&path, options).unwrap();
                std::io::Write::write_all(
                    &mut jar,
                    &std::fs::read(root.join("target/paper-bridge/fixtures").join(path)).unwrap(),
                )
                .unwrap();
            }
            jar.finish().unwrap();
        }
        let mut settings = real_native_config(directory.path());
        settings.extra_classpath = std::fs::read_dir(root.join("libraries/paper"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "jar"))
            .collect();
        settings.extra_classpath.sort();
        let service = ObservedService::new(Duration::ZERO, Ok(snapshot()));
        let mut plugins = JavaPlugins::spawn_with_services(settings, service.clone())
            .await
            .unwrap();
        assert!(
            plugins
                .take_startup_results()
                .iter()
                .all(|result| result.enabled)
        );
        assert_eq!(service.calls.load(Ordering::SeqCst), 4);
        plugins
            .dispatch(Event::Tick { tick: 1 }, None)
            .await
            .unwrap();
        let mut event = native_command("nativepaper");
        if let Event::Command { args, .. } = &mut event {
            *args = vec!["nested".into(), "nativeb:nativepaper".into()];
        }
        let before = service.calls.load(Ordering::SeqCst);
        let response = plugins
            .dispatch(event, Some("file-a".into()))
            .await
            .unwrap();
        assert!(response[0].enabled, "{response:?}");
        assert_eq!(
            response[0].actions,
            [
                Action::Reply {
                    message: "tick=123".into()
                },
                Action::Reply {
                    message: "tick=123".into()
                }
            ]
        );
        let nested = service.contexts.lock().unwrap()[before..].to_vec();
        assert_eq!(
            nested
                .iter()
                .map(|(plugin, _, call)| (plugin.as_str(), *call))
                .collect::<Vec<_>>(),
            [("file-b", 1), ("file-b", 2), ("file-a", 3)]
        );
        assert!(nested.iter().all(|(_, parent, _)| *parent == nested[0].1));
        assert_eq!(plugins.shutdown().await.unwrap().len(), 2);
    }

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
            let source = root.path().join("PluginHost.java");
            std::fs::copy(
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/java-host/PluginHost.java"
                ),
                &source,
            )
            .unwrap();
            let status = std::process::Command::new("javac")
                .args(["--release", "25", "-encoding", "UTF-8", "-d"])
                .arg(&classes)
                .arg(source)
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

    fn classpath_artifacts(host: &std::path::Path) -> Vec<PathBuf> {
        let host = std::fs::canonicalize(host).unwrap();
        let url = reqwest::Url::from_file_path(&host).unwrap();
        let probe = java_launch::manifest_jar(&[host], None).unwrap();
        let directory = std::path::Path::new(&probe.argument)
            .parent()
            .unwrap()
            .to_owned();
        drop(probe);
        std::fs::read_dir(directory)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                if !entry.file_type().ok()?.is_file()
                    || !entry
                        .file_name()
                        .to_str()?
                        .starts_with("zerqon-java-classpath-")
                {
                    return None;
                }
                let mut archive =
                    zip::ZipArchive::new(std::fs::File::open(entry.path()).ok()?).ok()?;
                let manifest = archive.by_name("META-INF/MANIFEST.MF").ok()?;
                let mut text = String::new();
                std::io::Read::read_to_string(
                    &mut std::io::Read::take(manifest, 131_072),
                    &mut text,
                )
                .ok()?;
                text.replace("\r\n ", "")
                    .contains(url.as_str())
                    .then(|| entry.path())
            })
            .collect()
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
    async fn java_host_loads_from_chinese_classpath_and_plugin_directory() {
        let root = tempfile::tempdir().unwrap();
        let libraries = root.path().join("目录 空间😀");
        std::fs::create_dir(&libraries).unwrap();
        let host = libraries.join("主机.jar");
        let dependency = libraries.join("依赖.jar");
        std::fs::copy(fixture_jar().join("fixture.jar"), &host).unwrap();
        std::fs::copy(fixture_jar().join("fixture.jar"), &dependency).unwrap();
        let mut settings = config("echo", &root.path().join("插件目录"));
        settings.host_jar = host;
        settings.gson_jar = dependency.clone();
        settings.extra_classpath = vec![dependency];
        let mut plugins = JavaPlugins::spawn(settings).await.unwrap();
        assert!(plugins.take_startup_results()[0].enabled);
        let response = plugins
            .dispatch(Event::Tick { tick: 1 }, Some("fixture".into()))
            .await
            .unwrap();
        assert_eq!(
            response[0].actions,
            [Action::Reply {
                message: "received".into()
            }]
        );
        assert!(!plugins.shutdown().await.unwrap()[0].enabled);
    }

    #[tokio::test]
    #[ignore = "requires built Java25 host and hello plugin: python scripts/build-java-plugins.py"]
    async fn real_host_and_plugin_load_under_chinese_paths_with_unicode_replies() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().join("目录 空间😀");
        let plugins = base.join("插件");
        std::fs::create_dir_all(&plugins).unwrap();
        let host = base.join("主机.jar");
        let gson = base.join("依赖.jar");
        std::fs::copy(root.join("libraries/zerqon-java-host-0.0.1.jar"), &host).unwrap();
        std::fs::copy(root.join("libraries/gson-2.13.2.jar"), &gson).unwrap();
        std::fs::copy(
            root.join("dist/hello-java-plugin-0.0.1.jar"),
            plugins.join("hello.jar"),
        )
        .unwrap();
        let settings = JavaPluginConfig {
            java: "java".into(),
            host_jar: host,
            gson_jar: gson,
            extra_classpath: Vec::new(),
            directory: plugins,
            server_version: "0.0.1".into(),
            request_timeout: Duration::from_secs(5),
            startup_timeout: Duration::from_secs(15),
            queue_capacity: 4,
        };
        let mut host = JavaPlugins::spawn(settings).await.unwrap();
        assert!(host.take_startup_results()[0].enabled);
        let result = host
            .dispatch(
                Event::Command {
                    sender: "你好😀".into(),
                    sender_kind: zerqon_plugin_api::SenderKind::Player,
                    operator: false,
                    command: "jhello".into(),
                    args: Vec::new(),
                },
                Some("hello".into()),
            )
            .await
            .unwrap();
        assert_eq!(
            result[0].actions,
            [Action::Reply {
                message: "Hello, 你好😀! This plugin uses the Zerqon Java API.".into()
            }]
        );
        host.shutdown().await.unwrap();
    }

    #[test]
    fn manifest_classpath_escapes_unicode_and_header_characters_preserving_order() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("目录 # 空间.jar");
        let second = directory.path().join("two%.jar");
        std::fs::write(&first, []).unwrap();
        std::fs::write(&second, []).unwrap();
        let paths = [
            std::fs::canonicalize(first).unwrap(),
            std::fs::canonicalize(second).unwrap(),
        ];
        let bytes = classpath_manifest(&paths, None).unwrap();
        assert!(bytes.is_ascii());
        let folded = String::from_utf8(bytes).unwrap();
        assert!(folded.split("\r\n").all(|line| line.len() <= 70));
        let joined = folded.replace("\r\n ", "");
        let value = joined
            .lines()
            .find_map(|line| line.strip_prefix("Class-Path: "))
            .unwrap();
        let urls: Vec<_> = value
            .split(' ')
            .map(|url| reqwest::Url::parse(url).unwrap())
            .collect();
        assert_eq!(urls.len(), 2);
        for (index, url) in urls.iter().enumerate() {
            assert_eq!(
                std::fs::canonicalize(url.to_file_path().unwrap()).unwrap(),
                paths[index]
            );
        }
        assert!(value.contains("%23") && value.contains("%25") && value.contains("%E7"));
    }

    #[test]
    fn manifest_classpath_rejects_relative_paths_and_oversized_headers() {
        assert!(matches!(
            classpath_manifest(&[PathBuf::from("relative.jar")], None),
            Err(JavaLaunchError::Configuration(_))
        ));
        let directory = tempfile::tempdir().unwrap();
        let oversized = directory.path().join(format!("{}.jar", "a".repeat(65_536)));
        assert!(matches!(
            classpath_manifest(&[oversized], None),
            Err(JavaLaunchError::Configuration(
                "classpath manifest exceeds 64 KiB"
            ))
        ));
    }

    #[test]
    fn executable_manifest_rejects_header_injection_and_folds_main_class() {
        for value in [
            "",
            "1Main",
            "a..Main",
            ".Main",
            "a/Main",
            "Main\r\nClass-Path: evil.jar",
        ] {
            assert!(matches!(
                classpath_manifest(&[], Some(value)),
                Err(JavaLaunchError::Configuration("invalid main class"))
            ));
        }
        let main = format!("org.example.{}$Nested", "LongMain".repeat(20));
        let manifest = String::from_utf8(classpath_manifest(&[], Some(&main)).unwrap()).unwrap();
        assert!(manifest.split("\r\n").all(|line| line.len() <= 70));
        assert!(
            manifest
                .replace("\r\n ", "")
                .contains(&format!("Main-Class: {main}\r\n"))
        );
    }

    #[test]
    fn manifest_classpath_artifact_is_valid_and_removed_when_guard_drops() {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().join(if cfg!(windows) {
            "separator;path"
        } else {
            "separator:path"
        });
        std::fs::create_dir(&base).unwrap();
        let jar = base.join("host.jar");
        std::fs::write(&jar, []).unwrap();
        let settings = JavaPluginConfig {
            java: "java".into(),
            host_jar: jar.clone(),
            gson_jar: jar,
            extra_classpath: Vec::new(),
            directory: base,
            server_version: "0.0.1".into(),
            request_timeout: Duration::from_secs(5),
            startup_timeout: Duration::from_secs(15),
            queue_capacity: 4,
        };
        let classpath = java_classpath(&settings).unwrap();
        let artifact = PathBuf::from(&classpath.argument);
        assert!(artifact.is_file());
        {
            let mut archive =
                zip::ZipArchive::new(std::fs::File::open(&artifact).unwrap()).unwrap();
            assert_eq!(archive.len(), 1);
            let mut manifest = archive.by_name("META-INF/MANIFEST.MF").unwrap();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut manifest, &mut bytes).unwrap();
            assert!(bytes.starts_with(b"Manifest-Version: 1.0\r\nClass-Path: file:"));
            assert!(bytes.ends_with(b"\r\n\r\n"));
        }
        drop(classpath);
        assert!(!artifact.exists());
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
        let unicode_host = directory.path().join("主机😀.jar");
        std::fs::copy(&settings.host_jar, &unicode_host).unwrap();
        settings.host_jar = unicode_host.clone();
        settings.startup_timeout = Duration::from_millis(500);
        let started = Instant::now();
        assert!(matches!(
            JavaPlugins::spawn(settings).await,
            Err(JavaPluginError::Timeout)
        ));
        assert!(started.elapsed() < Duration::from_secs(3));
        while !classpath_artifacts(&unicode_host).is_empty() {
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "manifest survived startup timeout"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn dropping_bridge_terminates_the_actual_jvm() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = config("pid", directory.path());
        let unicode_host = directory.path().join("主机😀.jar");
        std::fs::copy(&settings.host_jar, &unicode_host).unwrap();
        settings.host_jar = unicode_host.clone();
        let plugins = JavaPlugins::spawn(settings).await.unwrap();
        let artifacts = classpath_artifacts(&unicode_host);
        assert_eq!(artifacts.len(), 1);
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
            if !alive && artifacts.iter().all(|path| !path.exists()) {
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
    async fn dropping_bridge_cancels_pending_callback_before_removing_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = config("timeout", directory.path());
        let unicode_host = directory.path().join("主机😀.jar");
        std::fs::copy(&settings.host_jar, &unicode_host).unwrap();
        settings.host_jar = unicode_host.clone();
        settings.request_timeout = Duration::from_secs(30);
        let plugins = JavaPlugins::spawn(settings).await.unwrap();
        let artifacts = classpath_artifacts(&unicode_host);
        assert_eq!(artifacts.len(), 1);
        {
            let request = plugins.dispatch(Event::Tick { tick: 0 }, None);
            tokio::pin!(request);
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut request)
                    .await
                    .is_err()
            );
        }
        let started = Instant::now();
        drop(plugins);
        while artifacts.iter().any(|path| path.exists()) {
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "manifest survived cancellation of a pending callback"
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
