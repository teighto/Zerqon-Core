use crate::{
    operator_store::OperatorStore,
    players::{PlayerError, PlayerSnapshot, Players},
    plugin_services::{
        MAX_QUERY_PLAYERS, NativeCallContext, NativeErrorCode, NativeEventContext, NativePlayer,
        NativePosition, NativeQuery, NativeQueryError, NativeQueryFuture, NativeQueryService,
        NativeQueryValue, NativeWorld, ServerSnapshot,
    },
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use tokio::{sync::watch, time::Instant};

pub struct ServerQueries {
    pub players: Arc<Players>,
    pub tick: Arc<AtomicU64>,
    pub max_players: u32,
    pub shutdown: watch::Receiver<bool>,
    pub operators: Arc<Mutex<OperatorStore>>,
    pub worlds: Arc<[NativeWorld]>,
}

impl ServerQueries {
    fn player_world(&self) -> Result<uuid::Uuid, NativeQueryError> {
        match self.worlds.as_ref() {
            [world] if world.valid() => Ok(world.uuid),
            [] => Err(NativeQueryError::new(
                NativeErrorCode::Unavailable,
                "World is unavailable",
            )),
            _ => Err(NativeQueryError::new(
                NativeErrorCode::Unsupported,
                "Player world mapping requires one active world",
            )),
        }
    }

    pub fn player_context(
        &self,
        uuid: uuid::Uuid,
        session_id: i32,
    ) -> Result<NativeEventContext, NativeQueryError> {
        let snapshot = self
            .players
            .query_snapshot(uuid)
            .map_err(player_error)?
            .filter(|player| player.id == session_id)
            .ok_or_else(|| {
                NativeQueryError::new(
                    NativeErrorCode::StaleSession,
                    "Player session is no longer active",
                )
            })?;
        let world_uuid = self.player_world()?;
        let operators = self.operators.try_lock().map_err(|_| {
            NativeQueryError::new(
                NativeErrorCode::Unavailable,
                "Operator registry is unavailable",
            )
        })?;
        Ok(NativeEventContext {
            player: native_player(snapshot, world_uuid, &operators),
        })
    }
}

fn player_error(error: PlayerError) -> NativeQueryError {
    match error {
        PlayerError::Capacity => NativeQueryError::new(
            NativeErrorCode::Capacity,
            "Online player query exceeds its capacity",
        ),
        _ => NativeQueryError::new(
            NativeErrorCode::Unavailable,
            "Player registry is unavailable",
        ),
    }
}

fn native_player(
    snapshot: PlayerSnapshot,
    world_uuid: uuid::Uuid,
    operators: &OperatorStore,
) -> NativePlayer {
    NativePlayer {
        uuid: snapshot.uuid,
        session_id: snapshot.id,
        entity_id: snapshot.id,
        name: snapshot.name,
        world_uuid,
        position: NativePosition {
            x: snapshot.position.xyz[0],
            y: snapshot.position.xyz[1],
            z: snapshot.position.xyz[2],
            yaw: snapshot.position.yaw,
            pitch: snapshot.position.pitch,
        },
        operator: operators.is_operator(snapshot.uuid),
        online: true,
    }
}

impl NativeQueryService for ServerQueries {
    fn query(&self, context: NativeCallContext, query: NativeQuery) -> NativeQueryFuture<'_> {
        Box::pin(async move {
            if context.deadline <= Instant::now() {
                return Err(NativeQueryError::new(
                    NativeErrorCode::DeadlineExceeded,
                    "Native query deadline expired",
                ));
            }
            if *self.shutdown.borrow() && matches!(query, NativeQuery::ServerSnapshot) {
                return Err(NativeQueryError::new(
                    NativeErrorCode::Unavailable,
                    "Server is stopping",
                ));
            }
            if context.plugin.is_empty() || context.parent_id == 0 || context.call_id == 0 {
                return Err(NativeQueryError::new(
                    NativeErrorCode::InvalidArgument,
                    "Invalid native call identity",
                ));
            }
            if !query.valid() {
                return Err(NativeQueryError::new(
                    NativeErrorCode::InvalidArgument,
                    "Invalid native query arguments",
                ));
            }
            if matches!(
                query,
                NativeQuery::OnlinePlayers | NativeQuery::Player { .. }
            ) {
                self.player_world()?;
            }
            loop {
                if Instant::now() >= context.deadline {
                    return Err(NativeQueryError::new(
                        NativeErrorCode::DeadlineExceeded,
                        "Native query deadline expired",
                    ));
                }
                if *self.shutdown.borrow() && matches!(query, NativeQuery::ServerSnapshot) {
                    return Err(NativeQueryError::new(
                        NativeErrorCode::Unavailable,
                        "Server is stopping",
                    ));
                }
                match self.query_once(query.clone()) {
                    Err(error) if error.code == NativeErrorCode::Unavailable => {
                        tokio::time::sleep_until(
                            context
                                .deadline
                                .min(Instant::now() + std::time::Duration::from_millis(1)),
                        )
                        .await;
                    }
                    Ok(_) if Instant::now() >= context.deadline => {
                        return Err(NativeQueryError::new(
                            NativeErrorCode::DeadlineExceeded,
                            "Native query deadline expired",
                        ));
                    }
                    result => return result,
                }
            }
        })
    }
}

impl ServerQueries {
    fn query_once(&self, query: NativeQuery) -> Result<NativeQueryValue, NativeQueryError> {
        match query {
            NativeQuery::ServerSnapshot => {
                let online = self.players.try_count().map_err(|_| {
                    NativeQueryError::new(
                        NativeErrorCode::Unavailable,
                        "Player registry is unavailable",
                    )
                })?;
                Ok(NativeQueryValue::ServerSnapshot(ServerSnapshot {
                    server_version: env!("CARGO_PKG_VERSION").into(),
                    minecraft_version: "26.3".into(),
                    protocol: 777,
                    max_players: self.max_players,
                    online_players: u32::try_from(online).map_err(|_| {
                        NativeQueryError::new(
                            NativeErrorCode::Capacity,
                            "Player count exceeds the query limit",
                        )
                    })?,
                    tick: self.tick.load(Ordering::Acquire),
                }))
            }
            NativeQuery::OnlinePlayers => {
                let world_uuid = self.player_world()?;
                let snapshots = self
                    .players
                    .snapshots_bounded(MAX_QUERY_PLAYERS)
                    .map_err(player_error)?;
                let operators = self.operators.try_lock().map_err(|_| {
                    NativeQueryError::new(
                        NativeErrorCode::Unavailable,
                        "Operator registry is unavailable",
                    )
                })?;
                Ok(NativeQueryValue::OnlinePlayers {
                    players: snapshots
                        .into_iter()
                        .map(|snapshot| native_player(snapshot, world_uuid, &operators))
                        .collect(),
                })
            }
            NativeQuery::Player { uuid, session_id } => {
                let world_uuid = self.player_world()?;
                let snapshot = self
                    .players
                    .query_snapshot(uuid)
                    .map_err(player_error)?
                    .filter(|snapshot| session_id.is_none_or(|session| session == snapshot.id));
                let operators = self.operators.try_lock().map_err(|_| {
                    NativeQueryError::new(
                        NativeErrorCode::Unavailable,
                        "Operator registry is unavailable",
                    )
                })?;
                Ok(NativeQueryValue::Player {
                    player: snapshot
                        .map(|snapshot| native_player(snapshot, world_uuid, &operators)),
                })
            }
            NativeQuery::Worlds => {
                if self.worlds.len() > crate::plugin_services::MAX_QUERY_WORLDS {
                    return Err(NativeQueryError::new(
                        NativeErrorCode::Capacity,
                        "World query exceeds its capacity",
                    ));
                }
                Ok(NativeQueryValue::Worlds {
                    worlds: self.worlds.to_vec(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::players::{EntityIds, Identity};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    fn server(value: NativeQueryValue) -> ServerSnapshot {
        let NativeQueryValue::ServerSnapshot(snapshot) = value else {
            panic!("expected server snapshot")
        };
        snapshot
    }

    fn operators() -> Arc<Mutex<OperatorStore>> {
        let directory = tempfile::tempdir().unwrap();
        Arc::new(Mutex::new(
            OperatorStore::load(&directory.path().join("ops.json")).unwrap(),
        ))
    }

    fn context() -> NativeCallContext {
        NativeCallContext {
            plugin: "teighto-query-test".into(),
            parent_id: 1,
            call_id: 1,
            deadline: Instant::now() + std::time::Duration::from_secs(2),
        }
    }

    #[tokio::test]
    async fn read_models_follow_real_positions_operators_disconnect_and_reconnect() {
        let directory = tempfile::tempdir().unwrap();
        let players = Players::new(4, 64.0, 159, Arc::new(EntityIds::default()));
        let (shutdown, receiver) = watch::channel(false);
        let world = NativeWorld {
            uuid: Uuid::from_u128(2),
            name: "actual-world".into(),
            key: "minecraft:overworld".into(),
            dimension: "minecraft:overworld".into(),
            environment: crate::plugin_services::NativeEnvironment::Normal,
            min_height: -64,
            max_height: 320,
            spawn: NativePosition {
                x: 0.5,
                y: 64.0,
                z: 0.5,
                yaw: 0.0,
                pitch: 0.0,
            },
        };
        let service = ServerQueries {
            players: Arc::clone(&players),
            tick: Arc::new(AtomicU64::new(0)),
            max_players: 4,
            shutdown: receiver,
            operators: Arc::new(Mutex::new(
                OperatorStore::load(&directory.path().join("ops.json")).unwrap(),
            )),
            worlds: Arc::from([world.clone()]),
        };
        let (sender, _outbound) = mpsc::channel(32);
        let (disconnect, _disconnected) = watch::channel(false);
        let uuid = Uuid::from_u128(1);
        let player = players
            .reserve(
                Identity {
                    uuid,
                    name: "CONSOLE".into(),
                    properties: Vec::new(),
                },
                sender,
                disconnect,
            )
            .unwrap();
        assert_eq!(
            service
                .query(context(), NativeQuery::OnlinePlayers)
                .await
                .unwrap(),
            NativeQueryValue::OnlinePlayers {
                players: Vec::new()
            }
        );
        player
            .move_to(crate::players::Position {
                xyz: [-3.25, 80.0, 34.0],
                yaw: 42.0,
                pitch: 7.0,
                on_ground: true,
            })
            .unwrap();
        player.activate().unwrap();
        let first_id = player.id();
        let query = NativeQuery::Player {
            uuid,
            session_id: Some(first_id),
        };
        let NativeQueryValue::Player {
            player: Some(snapshot),
        } = service.query(context(), query.clone()).await.unwrap()
        else {
            panic!("missing active player")
        };
        assert_eq!(
            (
                snapshot.uuid,
                snapshot.name.as_str(),
                snapshot.session_id,
                snapshot.entity_id,
                snapshot.world_uuid
            ),
            (uuid, "CONSOLE", first_id, first_id, world.uuid)
        );
        assert_eq!(
            snapshot.position,
            NativePosition {
                x: -3.25,
                y: 80.0,
                z: 34.0,
                yaw: 42.0,
                pitch: 7.0
            }
        );
        assert!(snapshot.online && !snapshot.operator);
        service
            .operators
            .lock()
            .unwrap()
            .grant(uuid, "CONSOLE", 4)
            .unwrap();
        assert!(
            service
                .player_context(uuid, first_id)
                .unwrap()
                .player
                .operator
        );
        shutdown.send_replace(true);
        assert_eq!(
            service.query(context(), NativeQuery::Worlds).await.unwrap(),
            NativeQueryValue::Worlds {
                worlds: vec![world]
            }
        );
        assert!(
            service
                .query(context(), query.clone())
                .await
                .unwrap()
                .valid_for(&query)
        );
        drop(player);
        assert_eq!(
            service.query(context(), query.clone()).await.unwrap(),
            NativeQueryValue::Player { player: None }
        );
        let (sender, _outbound) = mpsc::channel(32);
        let (disconnect, _disconnected) = watch::channel(false);
        let player = players
            .reserve(
                Identity {
                    uuid,
                    name: "CONSOLE".into(),
                    properties: Vec::new(),
                },
                sender,
                disconnect,
            )
            .unwrap();
        player.activate().unwrap();
        assert_ne!(player.id(), first_id);
        assert_eq!(
            service.query(context(), query).await.unwrap(),
            NativeQueryValue::Player { player: None }
        );
        assert_eq!(
            service.player_context(uuid, first_id).unwrap_err().code,
            NativeErrorCode::StaleSession
        );
        assert_eq!(
            service
                .player_context(uuid, player.id())
                .unwrap()
                .player
                .entity_id,
            player.id()
        );
    }

    #[tokio::test]
    async fn queries_follow_actual_player_activation_disconnect_ticks_and_shutdown() {
        let players = Players::new(4, 64.0, 159, Arc::new(EntityIds::default()));
        let (shutdown, receiver) = watch::channel(false);
        let service = ServerQueries {
            players: Arc::clone(&players),
            tick: Arc::new(AtomicU64::new(0)),
            max_players: 4,
            shutdown: receiver,
            operators: operators(),
            worlds: Arc::from([]),
        };
        let (sender, _outbound) = mpsc::channel(8);
        let (disconnect, _disconnected) = watch::channel(false);
        let player = players
            .reserve(
                Identity {
                    uuid: Uuid::from_u128(7),
                    name: "teighto".into(),
                    properties: Vec::new(),
                },
                sender,
                disconnect,
            )
            .unwrap();
        assert_eq!(
            server(
                service
                    .query(context(), NativeQuery::ServerSnapshot)
                    .await
                    .unwrap()
            )
            .online_players,
            0
        );
        player.activate().unwrap();
        service.tick.store(37, Ordering::Release);
        let snapshot = server(
            service
                .query(context(), NativeQuery::ServerSnapshot)
                .await
                .unwrap(),
        );
        assert_eq!(
            (snapshot.online_players, snapshot.max_players, snapshot.tick),
            (1, 4, 37)
        );
        drop(player);
        assert_eq!(
            server(
                service
                    .query(context(), NativeQuery::ServerSnapshot)
                    .await
                    .unwrap()
            )
            .online_players,
            0
        );
        shutdown.send_replace(true);
        assert_eq!(
            service
                .query(context(), NativeQuery::ServerSnapshot)
                .await
                .unwrap_err()
                .code,
            NativeErrorCode::Unavailable
        );
    }

    #[tokio::test]
    async fn expired_queries_are_rejected_before_serving_a_snapshot() {
        let (_shutdown, receiver) = watch::channel(false);
        let service = ServerQueries {
            players: Players::new(4, 64.0, 159, Arc::new(EntityIds::default())),
            tick: Arc::new(AtomicU64::new(0)),
            max_players: 4,
            shutdown: receiver,
            operators: operators(),
            worlds: Arc::from([]),
        };
        let mut context = context();
        context.deadline = Instant::now();
        assert_eq!(
            service
                .query(context, NativeQuery::ServerSnapshot)
                .await
                .unwrap_err()
                .code,
            NativeErrorCode::DeadlineExceeded
        );
    }

    #[tokio::test]
    async fn read_queries_do_not_block_the_executor_behind_operator_disk_writes() {
        let directory = tempfile::tempdir().unwrap();
        let (_shutdown, receiver) = watch::channel(false);
        let operators = Arc::new(Mutex::new(
            OperatorStore::load(&directory.path().join("ops.json")).unwrap(),
        ));
        let service = ServerQueries {
            players: Players::new(4, 64.0, 159, Arc::new(EntityIds::default())),
            tick: Arc::new(AtomicU64::new(0)),
            max_players: 4,
            shutdown: receiver,
            operators: Arc::clone(&operators),
            worlds: Arc::from([NativeWorld {
                uuid: Uuid::from_u128(2),
                name: "world".into(),
                key: "minecraft:overworld".into(),
                dimension: "minecraft:overworld".into(),
                environment: crate::plugin_services::NativeEnvironment::Normal,
                min_height: -64,
                max_height: 320,
                spawn: NativePosition {
                    x: 0.5,
                    y: 64.0,
                    z: 0.5,
                    yaw: 0.0,
                    pitch: 0.0,
                },
            }]),
        };
        let (started, running) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _held = operators.lock().unwrap();
            started.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(400));
        });
        running.recv().unwrap();
        let began = Instant::now();
        let mut bounded = context();
        bounded.deadline = Instant::now() + Duration::from_millis(50);
        let result = service.query(bounded, NativeQuery::OnlinePlayers).await;
        let elapsed = began.elapsed();
        holder.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(200),
            "native query blocked for {elapsed:?}"
        );
        assert_eq!(result.unwrap_err().code, NativeErrorCode::DeadlineExceeded);
    }

    #[tokio::test]
    #[ignore = "requires built Java25 host and original Paper read-model fixture"]
    async fn paper_command_survives_transient_operator_contention_in_native_query() {
        use crate::java_plugins::{JavaPluginConfig, JavaPlugins};
        use crate::plugin_services::NativeEnvironment;
        use std::io::Write;
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let directory = tempfile::tempdir().unwrap();
        let plugin_dir = directory.path().join("plugins/java-plugins");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let mut jar =
            zip::ZipWriter::new(std::fs::File::create(plugin_dir.join("busy.jar")).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        jar.start_file("plugin.yml", options).unwrap();
        jar.write_all(b"name: BusyProbe\nversion: 1\nmain: io.github.teighto.zerqon.fixtures.ReadOnlyPaperProbe\napi-version: '26.3'\ncommands:\n  read: {}\npermissions:\n  native.public:\n    default: true\n  native.operator:\n    default: op\n").unwrap();
        let class = "io/github/teighto/zerqon/fixtures/ReadOnlyPaperProbe.class";
        jar.start_file(class, options).unwrap();
        jar.write_all(
            &std::fs::read(root.join("target/paper-bridge/fixtures").join(class)).unwrap(),
        )
        .unwrap();
        jar.finish().unwrap();
        let players = Players::new(4, 64.0, 159, Arc::new(EntityIds::default()));
        let (outbound, _packets) = mpsc::channel(32);
        let (disconnect, _disconnected) = watch::channel(false);
        let uuid = Uuid::from_u128(7);
        let player = players
            .reserve(
                Identity {
                    uuid,
                    name: "BusyPlayer".into(),
                    properties: Vec::new(),
                },
                outbound,
                disconnect,
            )
            .unwrap();
        player.activate().unwrap();
        let (_shutdown, receiver) = watch::channel(false);
        let operators = Arc::new(Mutex::new(
            OperatorStore::load(&directory.path().join("ops.json")).unwrap(),
        ));
        let service = Arc::new(ServerQueries {
            players,
            tick: Arc::new(AtomicU64::new(1)),
            max_players: 4,
            shutdown: receiver,
            operators: Arc::clone(&operators),
            worlds: Arc::from([NativeWorld {
                uuid: Uuid::from_u128(2),
                name: "world".into(),
                key: "minecraft:overworld".into(),
                dimension: "minecraft:overworld".into(),
                environment: NativeEnvironment::Normal,
                min_height: -64,
                max_height: 320,
                spawn: NativePosition {
                    x: 0.0,
                    y: 64.0,
                    z: 0.0,
                    yaw: 0.0,
                    pitch: 0.0,
                },
            }]),
        });
        let context = service.player_context(uuid, player.id()).unwrap();
        struct Observed {
            service: Arc<ServerQueries>,
            called: Arc<tokio::sync::Notify>,
        }
        impl NativeQueryService for Observed {
            fn query(
                &self,
                context: NativeCallContext,
                query: NativeQuery,
            ) -> NativeQueryFuture<'_> {
                if matches!(query, NativeQuery::OnlinePlayers) {
                    self.called.notify_one();
                }
                self.service.query(context, query)
            }
        }
        let called = Arc::new(tokio::sync::Notify::new());
        let mut extra: Vec<_> = std::fs::read_dir(root.join("libraries/paper"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "jar"))
            .collect();
        extra.sort();
        let plugins = Arc::new(
            JavaPlugins::spawn_with_services(
                JavaPluginConfig {
                    java: "java".into(),
                    host_jar: root.join("libraries/zerqon-java-host-0.0.1.jar"),
                    gson_jar: root.join("libraries/gson-2.13.2.jar"),
                    extra_classpath: extra,
                    directory: plugin_dir,
                    server_version: "0.0.1".into(),
                    request_timeout: Duration::from_secs(3),
                    startup_timeout: Duration::from_secs(15),
                    queue_capacity: 4,
                },
                Arc::new(Observed {
                    service,
                    called: Arc::clone(&called),
                }),
            )
            .await
            .unwrap(),
        );
        let (locked, ready) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = operators.lock().unwrap();
            locked.send(()).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(3));
        });
        ready.await.unwrap();
        let dispatch = Arc::clone(&plugins);
        let mut request = tokio::spawn(async move {
            dispatch
                .dispatch_with_context(
                    zerqon_plugin_api::Event::Command {
                        sender: "BusyPlayer".into(),
                        sender_kind: zerqon_plugin_api::SenderKind::Player,
                        operator: false,
                        command: "read".into(),
                        args: Vec::new(),
                    },
                    Some("busy".into()),
                    context,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), called.notified())
            .await
            .unwrap();
        let early = tokio::time::timeout(Duration::from_millis(15), &mut request).await;
        release.send(()).unwrap();
        holder.join().unwrap();
        assert!(
            early.is_err(),
            "transient busy failed the Paper command immediately: {early:?}"
        );
        let response = request.await.unwrap().unwrap();
        assert!(response[0].enabled, "{response:?}");
        assert!(response[0].error.is_none(), "{response:?}");
        assert!(
            matches!(&response[0].actions[0], zerqon_plugin_api::Action::Reply {message} if message.contains("BusyPlayer:true:false:true:false"))
        );
        assert!(plugins.list().await.unwrap()[0].enabled);
        plugins.shutdown().await.unwrap();
    }
}
