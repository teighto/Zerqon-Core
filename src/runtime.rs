use crate::{
    Result,
    auth::Authenticator,
    config::{Properties, Settings},
    game_data::GameData,
    java_plugins::{JavaPluginConfig, JavaPlugins},
    mobs::{MobConfig, Mobs},
    player_inventory::InventoryService,
    players::{EntityIds, Players},
    server::TickStats,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{RwLock, Semaphore, watch};
use uuid::Uuid;
use zerqon_inventory::ItemCatalog;
use zerqon_plugin_api::{Action, Event, LogLevel, SenderKind};
use zerqon_plugins::{Limits, PluginManager};
use zerqon_world::{Chunk, ChunkCache, ChunkWorkers, GenerationPolicy, SpawnPoint, WorldStore};

#[cfg(test)]
mod review_tests;

#[derive(Clone, Copy)]
pub enum CommandSource<'a> {
    Console,
    Player { uuid: Uuid, name: &'a str },
}

pub enum BlockInteraction {
    Place(zerqon_inventory::UseOnRequest),
    Break(zerqon_inventory::PlayerActionRequest),
}
impl<'a> CommandSource<'a> {
    pub fn name(self) -> &'a str {
        match self {
            Self::Console => "CONSOLE",
            Self::Player { name, .. } => name,
        }
    }
}

pub struct State {
    pub properties: Properties,
    pub settings: Settings,
    pub stats: Mutex<TickStats>,
    pub tick: Arc<AtomicU64>,
    pub gameplay: Arc<RwLock<()>>,
    pub data: Option<Arc<GameData>>,
    pub world: Option<Arc<World>>,
    pub items: Option<Arc<ItemCatalog>>,
    pub blocks: Option<Arc<zerqon_blocks::Catalog>>,
    pub inventories: Option<Arc<InventoryService>>,
    pub mobs: Option<Arc<Mobs>>,
    pub auth: Option<Authenticator>,
    pub players: Arc<Players>,
    pub queries: Arc<crate::server_queries::ServerQueries>,
    pub packet_budget: zerqon_protocol::inbound::PacketBudget,
    pub slots: Arc<Semaphore>,
    pub plugins: Arc<Mutex<PluginManager>>,
    pub commands: Mutex<HashMap<String, (String, Option<String>)>>,
    pub operators: Arc<Mutex<crate::operator_store::OperatorStore>>,
    pub profiler: crate::profiling::ProfilerService,
    pub shutdown: watch::Sender<bool>,
    pub java: Option<JavaPlugins>,
    pub updates: Option<watch::Receiver<Option<crate::updates::UpdateAvailable>>>,
    plugin_slots: Arc<Semaphore>,
}

impl State {
    pub async fn edit_block(
        &self,
        uuid: Uuid,
        entity_id: i32,
        inventory: &crate::player_inventory::InventoryHandle,
        action: BlockInteraction,
    ) -> Result<()> {
        use zerqon_blocks::{
            Aabb, Bounds, BuildError, Direction, Interaction, Placement, Position, StateId,
        };
        use zerqon_world::{ChunkKey, ChunkWrite, CommitError};
        if *self.shutdown.borrow() {
            return Err("server is stopping".into());
        }
        let world = self.world.as_ref().ok_or("world unavailable")?;
        let rules = Arc::clone(self.blocks.as_ref().ok_or("block rules unavailable")?);
        let gameplay = Arc::clone(&self.gameplay).write_owned().await;
        let observers = self.players.snapshots()?;
        let actor = observers
            .iter()
            .find(|player| player.uuid == uuid && player.id == entity_id)
            .ok_or("player session is no longer active")?;
        let actor_position = actor.position;
        let (clicked, hand) = match &action {
            BlockInteraction::Place(request) => (request.block, request.hand),
            BlockInteraction::Break(request) => (request.block, zerqon_inventory::Hand::Main),
        };
        let target = Position::new(clicked.x, clicked.y, clicked.z);
        let held = inventory.held_stack(hand)?;
        let near = [
            f64::from(clicked.x),
            f64::from(clicked.y),
            f64::from(clicked.z),
        ];
        let mut blockers: Vec<Aabb> = observers
            .iter()
            .filter_map(|player| {
                let xyz = player.position.xyz;
                let bounds = Aabb {
                    min: [xyz[0] - 0.3, xyz[1], xyz[2] - 0.3],
                    max: [xyz[0] + 0.3, xyz[1] + 1.8, xyz[2] + 0.3],
                };
                (0..3)
                    .all(|axis| {
                        bounds.max[axis] >= near[axis] - 3.0 && bounds.min[axis] <= near[axis] + 4.0
                    })
                    .then_some(bounds)
            })
            .collect();
        if let Some(mobs) = &self.mobs {
            blockers.extend(mobs.blockers_near(near)?);
        }
        if blockers.len() > 1024 {
            return Err("too many nearby entities for block interaction".into());
        }
        let store = Arc::clone(&world.store);
        let cache = Arc::clone(&world.cache);
        let data = Arc::clone(&world.data);
        let stripes = Arc::clone(&world.stripes);
        let players = Arc::clone(&self.players);
        let shutdown = self.shutdown.clone();
        let permit = Arc::clone(&world.inflight).acquire_owned().await?;
        let ticket = world.workers.try_submit(move || {
            Ok((|| -> Result<()> {
                let _gameplay = gameplay;
                if *shutdown.borrow() {
                    return Err("server is stopping".into());
                }
                store.check_available()?;
                let center = ChunkKey::new(clicked.x.div_euclid(16), clicked.z.div_euclid(16));
                let keys: Vec<_> = (-1..=1)
                    .flat_map(|x| (-1..=1).map(move |z| ChunkKey::new(center.x + x, center.z + z)))
                    .collect();
                let lock_ids: BTreeSet<_> = keys.iter().map(|key| stripe(key.x, key.z)).collect();
                let _locks: Vec<_> = lock_ids
                    .iter()
                    .map(|id| stripes[*id].lock().map_err(|_| "chunk lock poisoned"))
                    .collect::<std::result::Result<_, _>>()?;
                let snapshots = store.snapshot(&keys)?;
                let chunks: BTreeMap<_, _> = snapshots
                    .iter()
                    .filter_map(|snapshot| {
                        let key = snapshot.key();
                        snapshot
                            .chunk()
                            .cloned()
                            .or_else(|| {
                                (store.generation_policy() == GenerationPolicy::NativeFlat)
                                    .then(|| Arc::new(Chunk::flat(key.x, key.z, data.flat)))
                            })
                            .map(|chunk| (key, chunk))
                    })
                    .collect();
                for chunk in chunks.values() {
                    if let Err(error) = validate_chunk(chunk, &data) {
                        let _ = shutdown.send(true);
                        return Err(error.into());
                    }
                }
                let view = |position: Position| {
                    chunks
                        .get(&ChunkKey::new(
                            position.x.div_euclid(16),
                            position.z.div_euclid(16),
                        ))
                        .and_then(|chunk| {
                            chunk.get_block(
                                position.x.rem_euclid(16) as u8,
                                position.y,
                                position.z.rem_euclid(16) as u8,
                            )
                        })
                        .map(StateId)
                };
                let interaction = Interaction {
                    eye: [
                        actor_position.xyz[0],
                        actor_position.xyz[1] + 1.62,
                        actor_position.xyz[2],
                    ],
                    block_range: 5.0,
                    creative: true,
                    bounds: Bounds::default(),
                    blockers: &blockers,
                };
                let correction_face = match &action {
                    BlockInteraction::Place(request) => Direction::from_id(request.face as u8),
                    BlockInteraction::Break(_) => None,
                };
                let plan = (|| -> std::result::Result<zerqon_blocks::EditPlan, BuildError> {
                    match action {
                        BlockInteraction::Place(request) => {
                            let held = held.as_ref().ok_or(BuildError::UnsupportedItem)?;
                            let properties = held
                                .patch()
                                .added()
                                .values()
                                .find_map(|value| match value {
                                    zerqon_inventory::ComponentValue::BlockState(value) => {
                                        Some(value.clone())
                                    }
                                    _ => None,
                                })
                                .unwrap_or_default();
                            let facing = match ((actor_position.yaw as f64 / 90.0 + 0.5).floor()
                                as i64)
                                .rem_euclid(4)
                            {
                                0 => Direction::South,
                                1 => Direction::West,
                                2 => Direction::North,
                                _ => Direction::East,
                            };
                            rules.plan_placement(
                                &view,
                                &interaction,
                                &Placement {
                                    clicked: target,
                                    face: Direction::from_id(request.face as u8)
                                        .ok_or(BuildError::Invalid)?,
                                    local_hit: request.hit,
                                    facing,
                                    item: held.item().0,
                                    secondary_use: false,
                                    properties: &properties,
                                },
                            )
                        }
                        BlockInteraction::Break(_) => rules.plan_break(
                            &view,
                            &interaction,
                            target,
                            held.as_ref().map(|stack| stack.item().0),
                        ),
                    }
                })();
                let plan = match plan {
                    Ok(plan) => plan,
                    Err(error) => {
                        let corrections = std::iter::once(target)
                            .chain(correction_face.and_then(|face| target.relative(face)))
                            .filter_map(|position| {
                                view(position).map(|state| {
                                    bytes::Bytes::from(block_update(
                                        position.x, position.y, position.z, state.0,
                                    ))
                                })
                            })
                            .collect::<Vec<_>>();
                        players.dispatch_batches(vec![(uuid, entity_id, corrections)])?;
                        return Err(error.into());
                    }
                };
                if plan.changes().is_empty() {
                    return Ok(());
                }
                let read_keys: BTreeSet<_> = plan
                    .reads()
                    .iter()
                    .map(|(position, _)| {
                        ChunkKey::new(position.x.div_euclid(16), position.z.div_euclid(16))
                    })
                    .collect();
                let reads: Vec<_> = snapshots
                    .into_iter()
                    .filter(|snapshot| read_keys.contains(&snapshot.key()))
                    .collect();
                let mut changed = BTreeMap::new();
                for change in plan.changes() {
                    let key = ChunkKey::new(
                        change.position.x.div_euclid(16),
                        change.position.z.div_euclid(16),
                    );
                    let chunk = changed.entry(key).or_insert_with(|| {
                        (**chunks
                            .get(&key)
                            .expect("planner read set includes every changed chunk"))
                        .clone()
                    });
                    chunk.set_block(
                        change.position.x.rem_euclid(16) as u8,
                        change.position.y,
                        change.position.z.rem_euclid(16) as u8,
                        change.replacement.0,
                    )?;
                }
                let writes: Vec<_> = changed
                    .into_iter()
                    .map(|(key, chunk)| ChunkWrite {
                        key,
                        replacement: Arc::new(chunk),
                    })
                    .collect();
                let updates: Vec<_> = plan
                    .changes()
                    .iter()
                    .map(|change| {
                        bytes::Bytes::from(block_update(
                            change.position.x,
                            change.position.y,
                            change.position.z,
                            change.replacement.0,
                        ))
                    })
                    .collect();
                match store.commit(Uuid::new_v4(), &reads, &writes) {
                    Ok(_) => {}
                    Err(error @ CommitError::Indeterminate { .. }) => {
                        let _ = shutdown.send(true);
                        return Err(error.into());
                    }
                    Err(error) => return Err(error.into()),
                }
                match cache.lock() {
                    Ok(mut cache) => {
                        for write in writes {
                            cache.insert(write.replacement);
                        }
                    }
                    Err(_) => {
                        let _ = shutdown.send(true);
                        return Err("world edit committed but cache publication failed".into());
                    }
                }
                for packet in updates {
                    if let Err(error) = players.broadcast(packet) {
                        let _ = shutdown.send(true);
                        return Err(error.into());
                    }
                }
                Ok(())
            })())
        })?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            ticket.wait()
        })
        .await???;
        Ok(())
    }

    pub async fn new(
        directory: &Path,
        properties: Properties,
        settings: Settings,
        status_only: bool,
        shutdown: watch::Sender<bool>,
        updates: Option<watch::Receiver<Option<crate::updates::UpdateAvailable>>>,
    ) -> Result<Arc<Self>> {
        let gameplay = Arc::new(RwLock::new(()));
        let data = if status_only {
            None
        } else {
            Some(Arc::new(GameData::load(
                &directory.join(&settings.data_directory),
            )?))
        };
        let items = if status_only {
            None
        } else {
            let path = directory.join(&settings.data_directory);
            Some(Arc::new(
                tokio::task::spawn_blocking(move || ItemCatalog::load(&path)).await??,
            ))
        };
        let auth = if properties.online_mode && !status_only {
            Some(Authenticator::new().await?)
        } else {
            None
        };
        let mut plugins = PluginManager::new(Limits {
            fuel_per_event: settings.plugin_fuel,
            memory_bytes: settings.plugin_memory_mib * 1024 * 1024,
            ..Limits::default()
        })?;
        let loaded = if status_only {
            Vec::new()
        } else {
            plugins.load_directory(&directory.join("plugins/rust-plugins"))?
        };
        let operators = crate::operator_store::OperatorStore::load(&directory.join("ops.json"))?;
        tracing::info!(
            count = operators.entries().len(),
            "Loaded UUID operator records"
        );
        let operators = Arc::new(Mutex::new(operators));
        let entity_ids = Arc::new(EntityIds::default());
        let players = Players::new(
            properties.max_players,
            f64::from(properties.view_distance) * 16.0,
            data.as_ref()
                .and_then(|data| data.entity_type("minecraft:player"))
                .unwrap_or(0),
            Arc::clone(&entity_ids),
        );
        let world = data
            .as_ref()
            .map(|data| {
                World::new(
                    &directory.join(&properties.level_name),
                    &settings,
                    Arc::clone(data),
                    Arc::clone(&gameplay),
                    Arc::clone(&players),
                    shutdown.clone(),
                )
            })
            .transpose()?
            .map(Arc::new);
        let blocks = if status_only {
            None
        } else {
            Some(Arc::new(zerqon_blocks::Catalog::embedded()?))
        };
        let inventories = items
            .as_ref()
            .map(|catalog| {
                InventoryService::new(
                    directory.join(&properties.level_name).join("playerdata"),
                    Arc::clone(catalog),
                    Arc::clone(&players),
                    properties.max_players,
                    Arc::clone(&gameplay),
                )
            })
            .transpose()?;
        let mobs = match (&world, &data, &blocks) {
            (Some(world), Some(data), Some(blocks)) => Some(
                Mobs::open(
                    MobConfig {
                        path: directory.join(&properties.level_name).join("entities.zqe"),
                        capacity: settings.max_entities,
                        natural_spawning: settings.spawn_mobs,
                        visibility_radius: f64::from(properties.view_distance) * 16.0,
                    },
                    Arc::clone(world),
                    Arc::clone(&players),
                    Arc::clone(data),
                    Arc::clone(blocks),
                    entity_ids,
                    Arc::clone(&gameplay),
                )
                .await?,
            ),
            _ => None,
        };
        let tick = Arc::new(AtomicU64::new(0));
        let worlds: Arc<[crate::plugin_services::NativeWorld]> = world
            .as_ref()
            .map(|world| world.descriptor(&properties.level_name))
            .transpose()?
            .into_iter()
            .collect::<Vec<_>>()
            .into();
        let queries = Arc::new(crate::server_queries::ServerQueries {
            players: Arc::clone(&players),
            tick: Arc::clone(&tick),
            max_players: u32::try_from(properties.max_players)?,
            shutdown: shutdown.subscribe(),
            operators: Arc::clone(&operators),
            worlds,
        });
        let java_present = !status_only
            && std::fs::read_dir(directory.join("plugins/java-plugins"))?
                .filter_map(std::result::Result::ok)
                .any(|entry| entry.path().extension().is_some_and(|ext| ext == "jar"));
        let mut java = if settings.java_plugins && java_present {
            Some(
                JavaPlugins::spawn_with_services(
                    JavaPluginConfig {
                        java: settings.java_executable.clone().into(),
                        host_jar: directory.join("libraries/zerqon-java-host-0.0.1.jar"),
                        gson_jar: directory.join("libraries/gson-2.13.2.jar"),
                        extra_classpath: crate::paper_libraries::classpath(
                            &directory.join("libraries/paper"),
                        )?,
                        directory: directory.join("plugins/java-plugins"),
                        server_version: env!("CARGO_PKG_VERSION").into(),
                        request_timeout: Duration::from_secs(2),
                        startup_timeout: Duration::from_secs(20),
                        queue_capacity: 32,
                    },
                    queries.clone(),
                )
                .await?,
            )
        } else {
            None
        };
        if java_present && !settings.java_plugins {
            tracing::warn!("Java plugins are disabled in server.settings");
        }
        let java_loaded = java
            .as_mut()
            .map(JavaPlugins::take_startup_results)
            .unwrap_or_default();
        let state = Arc::new(Self {
            slots: Arc::new(Semaphore::new(properties.max_players)),
            packet_budget: zerqon_protocol::inbound::PacketBudget::new(
                settings.packet_memory_mib * 1024 * 1024,
            )?,
            properties,
            settings,
            stats: Mutex::new(TickStats::new()),
            tick,
            gameplay,
            data,
            world,
            items,
            blocks,
            inventories,
            mobs,
            auth,
            players,
            queries,
            plugins: Arc::new(Mutex::new(plugins)),
            commands: Mutex::new(HashMap::new()),
            operators,
            profiler: crate::profiling::ProfilerService::new(
                directory.to_owned(),
                zerqon_profiler::ProfileConfig::default(),
            )?,
            shutdown,
            java,
            updates,
            plugin_slots: Arc::new(Semaphore::new(32)),
        });
        if let Some(items) = &state.items {
            tracing::info!(
                items = items.item_count(),
                components = items.component_count(),
                "Loaded item catalog"
            );
        }
        for loaded in loaded {
            let name = loaded
                .path
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("invalid");
            match loaded.result {
                Ok(actions) => {
                    state.apply_actions(name, actions);
                    tracing::info!("Loaded Rust plugin {name}");
                }
                Err(error) => tracing::error!(%error,plugin=name,"Plugin failed to load"),
            }
        }
        for result in java_loaded {
            if let Some(error) = result.error {
                tracing::error!(plugin=result.name,%error,"Java plugin failed to load");
            } else {
                state.apply_actions(&format!("java:{}", result.name), result.actions);
                tracing::info!(plugin = result.name, "Loaded Java plugin");
            }
        }
        Ok(state)
    }

    pub fn online(&self) -> usize {
        self.players.count()
    }
    pub fn report(&self) -> String {
        self.stats.lock().map_or_else(
            |_| "Tick metrics unavailable".into(),
            |stats| stats.report(),
        )
    }
    pub fn is_operator(&self, uuid: Uuid) -> bool {
        self.operators.lock().is_ok_and(|ops| ops.is_operator(uuid))
    }
    fn permission_level(&self, source: CommandSource<'_>) -> u8 {
        match source {
            CommandSource::Console => 4,
            CommandSource::Player { uuid, .. } => {
                self.operators.lock().map_or(0, |ops| ops.level(uuid))
            }
        }
    }
    pub fn update_operator(&self, name: &str, add: bool) -> Result<bool> {
        let online = self
            .players
            .snapshots()?
            .into_iter()
            .find(|player| player.name.eq_ignore_ascii_case(name));
        let mut operators = self
            .operators
            .lock()
            .map_err(|_| "operator lock poisoned")?;
        if add {
            let player =
                online.ok_or("The player must be connected before receiving operator access")?;
            Ok(operators.grant(player.uuid, &player.name, 4)?)
        } else {
            let uuid = Uuid::parse_str(name)
                .ok()
                .or_else(|| operators.lookup_by_name(name).map(|entry| entry.uuid))
                .or_else(|| online.map(|player| player.uuid))
                .ok_or("Operator not found")?;
            Ok(operators.revoke(uuid)?)
        }
    }
    fn apply_actions(&self, plugin: &str, actions: Vec<Action>) -> Vec<String> {
        let mut replies = Vec::new();
        for action in actions {
            match action {
                Action::Log { level, message } => match level {
                    LogLevel::Info => tracing::info!(%plugin,"{message}"),
                    LogLevel::Warn => tracing::warn!(%plugin,"{message}"),
                    LogLevel::Error => tracing::error!(%plugin,"{message}"),
                },
                Action::Reply { message } => replies.push(message),
                Action::RegisterCommand {
                    name, permission, ..
                } => {
                    if let Ok(mut commands) = self.commands.lock() {
                        if is_builtin(&name) || commands.contains_key(&name) {
                            tracing::warn!(%plugin,%name,"Command name already registered");
                        } else {
                            commands.insert(name, (plugin.to_owned(), permission));
                        }
                    }
                }
            }
        }
        replies
    }

    pub async fn event(&self, event: Event) -> Result<()> {
        self.event_with_context(event, None).await
    }

    async fn player_context(
        &self,
        uuid: Uuid,
        session: Option<i32>,
    ) -> Result<crate::plugin_services::NativeEventContext> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        loop {
            let attempt = match session {
                Some(session) => self.queries.player_context(uuid, session),
                None => match self.players.query_snapshot(uuid) {
                    Ok(Some(player)) => self.queries.player_context(uuid, player.id),
                    Ok(None) => Err(crate::plugin_services::NativeQueryError::new(
                        crate::plugin_services::NativeErrorCode::StaleSession,
                        "Player is not connected",
                    )),
                    Err(_) => Err(crate::plugin_services::NativeQueryError::new(
                        crate::plugin_services::NativeErrorCode::Unavailable,
                        "Player registry is busy",
                    )),
                },
            };
            match attempt {
                Ok(context) => return Ok(context),
                Err(error)
                    if error.code == crate::plugin_services::NativeErrorCode::Unavailable
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub async fn player_event(
        &self,
        uuid: Uuid,
        session: i32,
        name: &str,
        joining: bool,
    ) -> Result<()> {
        let event = if joining {
            Event::PlayerJoin {
                uuid: uuid.to_string(),
                name: name.into(),
            }
        } else {
            Event::PlayerQuit {
                uuid: uuid.to_string(),
                name: name.into(),
            }
        };
        let context = if self.java.is_some() {
            match self.player_context(uuid, Some(session)).await {
                Ok(context) => Some(context),
                Err(error) if !joining => {
                    let java_error = format!("Java player quit context: {error}");
                    return match self.dispatch_rust_event(event).await {
                        Ok(()) => Err(java_error.into()),
                        Err(rust_error) => {
                            Err(format!("{java_error}; Rust player quit: {rust_error}").into())
                        }
                    };
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        self.event_with_context(event, context).await
    }

    async fn event_with_context(
        &self,
        event: Event,
        context: Option<crate::plugin_services::NativeEventContext>,
    ) -> Result<()> {
        if let Event::Tick { tick } = &event {
            self.tick.fetch_max(*tick, Ordering::Release);
        }
        let disabling = matches!(event, Event::Disable);
        let mut failures = Vec::new();
        if let Some(java) = &self.java {
            let results = if disabling {
                java.shutdown().await
            } else if let Some(context) = context {
                java.dispatch_with_context(event.clone(), None, context)
                    .await
            } else {
                java.dispatch(event.clone(), None).await
            };
            match results {
                Ok(results) => {
                    for result in results {
                        if let Some(error) = result.error {
                            tracing::error!(plugin=result.name,%error,"Java plugin callback failed");
                            if disabling {
                                failures.push(format!("Java plugin {}: {error}", result.name));
                            }
                        }
                        self.apply_actions(&format!("java:{}", result.name), result.actions);
                    }
                }
                Err(error) => {
                    tracing::warn!(%error,"Java plugin event failed");
                    if disabling {
                        failures.push(format!("Java host: {error}"));
                    }
                }
            }
        }
        if let Err(error) = self.dispatch_rust_event(event).await {
            failures.push(error.to_string());
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; ").into())
        }
    }

    async fn dispatch_rust_event(&self, event: Event) -> Result<()> {
        let disabling = matches!(event, Event::Disable);
        let mut failures = Vec::new();
        let permit = Arc::clone(&self.plugin_slots).acquire_owned().await?;
        let plugins = Arc::clone(&self.plugins);
        let dispatched = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut plugins = plugins.lock().map_err(|_| "plugin lock poisoned")?;
            let results = if disabling {
                plugins
                    .list()
                    .into_iter()
                    .map(|plugin| zerqon_plugins::DispatchResult {
                        name: plugin.name.clone(),
                        result: plugins.unload(&plugin.name),
                    })
                    .collect()
            } else {
                plugins.dispatch_all(&event)
            };
            Ok::<_, &str>(results)
        })
        .await;
        match dispatched {
            Ok(Ok(results)) => {
                for result in results {
                    match result.result {
                        Ok(actions) => {
                            self.apply_actions(&result.name, actions);
                        }
                        Err(error) => {
                            tracing::error!(%error,plugin=result.name,"Rust plugin callback failed");
                            if disabling {
                                failures.push(format!("Rust plugin {}: {error}", result.name));
                            }
                        }
                    }
                }
            }
            Ok(Err(error)) => failures.push(error.into()),
            Err(error) => failures.push(error.to_string()),
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; ").into())
        }
    }

    pub async fn command(&self, source: CommandSource<'_>, input: &str) -> Result<Vec<String>> {
        let sender = source.name();
        let level = self.permission_level(source);
        let authorized = level > 0;
        let mut words = input.trim_start_matches('/').split_whitespace();
        let Some(name) = words.next() else {
            return Ok(vec![]);
        };
        let name = name.to_ascii_lowercase();
        let args: Vec<String> = words.map(str::to_owned).collect();
        match name.as_str(){
            "tps"=>Ok(vec![self.report()]),
            "spark"=>Ok(self.profiler.execute(&args,level>=4).await?),
            "list"=>Ok(vec![format!("Players ({}): {}",self.online(),self.players.names().join(", "))]),
            "version"=>Ok(vec![format!("Zerqon {} | Minecraft 26.3 | teighto",env!("CARGO_PKG_VERSION"))]),
            "help"=>Ok(vec!["Commands: /help /version /tps /tpsbar /plugins; operator: /give player item [count], /summon pig|cow|zombie [x y z], /setblock x y z state, /stop".into()]),
            "plugins"=>{let rust=self.plugins.lock().map_err(|_|"plugin lock poisoned")?.list().iter().map(|plugin|format!("{} ({})",plugin.name,if plugin.enabled{"enabled"}else{"disabled"})).collect::<Vec<_>>().join(", ");let java=if let Some(java)=&self.java{java.list().await?.iter().map(|plugin|format!("{} ({})",plugin.name,if plugin.enabled{"enabled"}else{"disabled"})).collect::<Vec<_>>().join(", ")}else{"disabled".into()};Ok(vec![format!("Rust: {rust}; Java: {java}")])},
            "stop"=>{if level<4{return Ok(vec!["Permission denied".into()]);}let _=self.shutdown.send(true);Ok(vec!["Stopping Zerqon".into()])},
            "setblock"=>{
                if level<2{return Ok(vec!["Permission denied".into()]);}
                if args.len()!=4{return Ok(vec!["Usage: /setblock x y z state-id".into()]);}
                let x=args[0].parse::<i32>()?;let y=args[1].parse::<i32>()?;let z=args[2].parse::<i32>()?;let block=args[3].parse::<u32>()?;
                let world=self.world.as_ref().ok_or("world unavailable")?;
                world.set_block(x,y,z,block).await?;
                Ok(vec![format!("Block set at {x} {y} {z}")])
            },
            "give"=>{
                if level<2{return Ok(vec!["Permission denied".into()]);}
                if !(2..=3).contains(&args.len()){return Ok(vec!["Usage: /give player minecraft:item [count 1..4096]".into()]);}
                let count=if args.len()==3{args[2].parse::<u32>()?}else{1};
                let player=self.players.snapshots()?.into_iter().find(|player|player.name.eq_ignore_ascii_case(&args[0])).ok_or("Player is not connected")?;
                self.inventories.as_ref().ok_or("Inventory service unavailable")?.give(player.uuid,player.id,&args[1],count).await?;
                Ok(vec![format!("Gave {count} {} to {}",args[1],player.name)])
            },
            "summon"=>{
                if level<2{return Ok(vec!["Permission denied".into()]);}
                if args.len()!=1&&args.len()!=4{return Ok(vec!["Usage: /summon pig|cow|zombie [x y z]".into()]);}
                let kind=match args[0].strip_prefix("minecraft:").unwrap_or(&args[0]){"pig"=>zerqon_entities::MobKind::Pig,"cow"=>zerqon_entities::MobKind::Cow,"zombie"=>zerqon_entities::MobKind::Zombie,_=>return Ok(vec!["Supported mobs: pig, cow, zombie".into()])};
                let position=if args.len()==4{[args[1].parse::<f64>()?,args[2].parse::<f64>()?,args[3].parse::<f64>()?]}else{match source {
                    CommandSource::Player{uuid,..}=>self.players.query_snapshot(uuid)?.ok_or("Player is not connected")?.position.xyz,
                    CommandSource::Console=>{let spawn=self.world.as_ref().ok_or("World unavailable")?.spawn();[f64::from(spawn.position[0])+0.5,f64::from(spawn.position[1]),f64::from(spawn.position[2])+0.5]},
                }};
                let mobs=self.mobs.as_ref().ok_or("mob simulation unavailable")?;
                let id=mobs.summon(kind,position).await?;
                Ok(vec![format!("Summoned {} (entity {id})",kind.identifier())])
            },
            _=>{
                let registered=self.commands.lock().map_err(|_|"command lock poisoned")?.get(&name).cloned();
                let Some((plugin,permission))=registered else{return Ok(vec!["Unknown command; use /help".into()]);};
                if permission.is_some()&&!authorized&&!plugin.starts_with("java:"){return Ok(vec!["Permission denied".into()]);}
                let mut event=Event::Command{sender:sender.into(),operator:authorized,sender_kind:if matches!(source,CommandSource::Console){SenderKind::Console}else{SenderKind::Player},command:name,args};
                if let Some(name)=plugin.strip_prefix("java:"){let java=self.java.as_ref().ok_or("Java plugins unavailable")?;let results=match source {
                    CommandSource::Player{uuid,..}=>{
                        let context=self.player_context(uuid,None).await?;
                        if let Event::Command {operator,..}=&mut event { *operator=context.player.operator; }
                        java.dispatch_with_context(event,Some(name.into()),context).await?
                    },
                    CommandSource::Console=>java.dispatch(event,Some(name.into())).await?,
                };let mut replies=Vec::new();for result in results{if let Some(error)=result.error{return Err(error.into());}replies.extend(self.apply_actions(&plugin,result.actions));}return Ok(replies);}
                let permit=Arc::clone(&self.plugin_slots).acquire_owned().await?;
                let plugins=Arc::clone(&self.plugins);let plugin_name=plugin.clone();
                let actions=tokio::task::spawn_blocking(move||{let _permit=permit;plugins.lock().map_err(|_|"plugin lock poisoned")?.dispatch(&plugin_name,&event).map_err(|error|error.to_string())}).await?.map_err(|error|format!("plugin {plugin}: {error}"))?;
                Ok(self.apply_actions(&plugin,actions))
            }
        }
    }
}

fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "help"
            | "list"
            | "version"
            | "tps"
            | "tpsbar"
            | "plugins"
            | "stop"
            | "setblock"
            | "spark"
            | "summon"
            | "give"
            | "op"
            | "deop"
    )
}

pub fn block_update(x: i32, y: i32, z: i32, state: u32) -> Vec<u8> {
    let mut packet = vec![8];
    let position = ((i64::from(x) & 0x3ffffff) << 38)
        | ((i64::from(z) & 0x3ffffff) << 12)
        | (i64::from(y) & 0xfff);
    packet.extend_from_slice(&position.to_be_bytes());
    zerqon_protocol::codec::put_varint(state as i32, &mut packet);
    packet
}

pub struct World {
    players: Arc<Players>,
    shutdown: watch::Sender<bool>,
    gameplay: Arc<RwLock<()>>,
    store: Arc<WorldStore>,
    cache: Arc<Mutex<ChunkCache>>,
    workers: ChunkWorkers,
    inflight: Arc<Semaphore>,
    data: Arc<GameData>,
    stripes: Arc<[Mutex<()>; 64]>,
}
impl World {
    pub fn descriptor(&self, name: &str) -> Result<crate::plugin_services::NativeWorld> {
        use crate::plugin_services::{NativeEnvironment, NativePosition, NativeWorld};
        let spawn = self.spawn();
        let descriptor = NativeWorld {
            uuid: self.store.uuid()?,
            name: name.into(),
            key: spawn.dimension.clone(),
            dimension: spawn.dimension,
            environment: NativeEnvironment::Normal,
            min_height: zerqon_world::MIN_Y,
            max_height: zerqon_world::MIN_Y + zerqon_world::WORLD_HEIGHT as i32,
            spawn: NativePosition {
                x: f64::from(spawn.position[0]),
                y: f64::from(spawn.position[1]),
                z: f64::from(spawn.position[2]),
                yaw: spawn.yaw,
                pitch: spawn.pitch,
            },
        };
        if !descriptor.valid() {
            return Err("invalid native world descriptor".into());
        }
        Ok(descriptor)
    }
    pub fn check_available(&self) -> Result<()> {
        if *self.shutdown.borrow() {
            return Err(zerqon_world::WorldError::StorageUnavailable.into());
        }
        self.store.check_available()?;
        Ok(())
    }
    pub fn spawn(&self) -> SpawnPoint {
        self.store.metadata().map_or_else(
            || SpawnPoint {
                dimension: "minecraft:overworld".into(),
                position: [0, 64, 0],
                yaw: 0.0,
                pitch: 0.0,
            },
            |metadata| metadata.spawn.clone(),
        )
    }
    pub fn existing_only(&self) -> bool {
        self.store.generation_policy() == GenerationPolicy::ExistingOnly
    }
    pub fn missing_imported_chunk(error: &(dyn std::error::Error + 'static)) -> bool {
        matches!(
            error.downcast_ref::<zerqon_world::WorldError>(),
            Some(zerqon_world::WorldError::MissingImportedChunk { .. })
        )
    }
    pub async fn execute<T: Send + 'static>(
        &self,
        job: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let permit = Arc::clone(&self.inflight).acquire_owned().await?;
        let ticket = self.workers.try_submit(move || Ok(job()))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            ticket.wait()
        })
        .await??
    }
    pub fn cached_chunks(&self) -> Result<Vec<Arc<Chunk>>> {
        self.check_available()?;
        Ok(self
            .cache
            .lock()
            .map_err(|_| "chunk cache lock poisoned")?
            .snapshot())
    }
    pub async fn chunk(&self, x: i32, z: i32) -> Result<Arc<Chunk>> {
        self.check_available()?;
        let store = Arc::clone(&self.store);
        let cache = Arc::clone(&self.cache);
        let data = Arc::clone(&self.data);
        let stripes = Arc::clone(&self.stripes);
        self.execute(move || {
            let _guard = stripes[stripe(x, z)]
                .lock()
                .map_err(|_| "chunk lock poisoned")?;
            store.check_available()?;
            let cached = cache.lock().map_err(|_| "cache lock poisoned")?.get(x, z);
            if let Some(chunk) = cached {
                return Ok(chunk);
            }
            let chunk = Arc::new(
                store
                    .load(x, z)?
                    .unwrap_or_else(|| Chunk::flat(x, z, data.flat)),
            );
            validate_chunk(&chunk, &data)?;
            cache
                .lock()
                .map_err(|_| "cache lock poisoned")?
                .insert(Arc::clone(&chunk));
            Ok(chunk)
        })
        .await
    }
    fn new(
        path: &Path,
        settings: &Settings,
        data: Arc<GameData>,
        gameplay: Arc<RwLock<()>>,
        players: Arc<Players>,
        shutdown: watch::Sender<bool>,
    ) -> Result<Self> {
        let store = WorldStore::open(path)?;
        if let Some(metadata) = store.metadata() {
            let [x, _, z] = metadata.spawn.position;
            let spawn = store
                .load(x.div_euclid(16), z.div_euclid(16))?
                .ok_or("imported spawn chunk is unavailable")?;
            validate_chunk(&spawn, &data)?;
        }
        Ok(Self {
            players,
            shutdown,
            gameplay,
            store: Arc::new(store),
            cache: Arc::new(Mutex::new(ChunkCache::with_limits(
                settings.chunk_cache_capacity,
                256 * 1024 * 1024,
            )?)),
            workers: ChunkWorkers::new(settings.world_threads, settings.work_queue_capacity)?,
            inflight: Arc::new(Semaphore::new(settings.work_queue_capacity)),
            data,
            stripes: Arc::new(std::array::from_fn(|_| Mutex::new(()))),
        })
    }
    pub async fn packet(&self, x: i32, z: i32) -> Result<Vec<u8>> {
        self.check_available()?;
        let permit = Arc::clone(&self.inflight).acquire_owned().await?;
        let store = Arc::clone(&self.store);
        let cache = Arc::clone(&self.cache);
        let data = Arc::clone(&self.data);
        let stripes = Arc::clone(&self.stripes);
        let ticket = self.workers.try_submit(move || {
            let _guard = stripes[stripe(x, z)]
                .lock()
                .map_err(|_| zerqon_world::WorldError::Corrupt("chunk lock poisoned"))?;
            store.check_available()?;
            let cached = cache
                .lock()
                .map_err(|_| zerqon_world::WorldError::Corrupt("cache lock poisoned"))?
                .get(x, z);
            let chunk = if let Some(chunk) = cached {
                chunk
            } else {
                let chunk = Arc::new(
                    store
                        .load(x, z)?
                        .unwrap_or_else(|| Chunk::flat(x, z, data.flat)),
                );
                validate_chunk(&chunk, &data)?;
                cache
                    .lock()
                    .map_err(|_| zerqon_world::WorldError::Corrupt("cache lock poisoned"))?
                    .insert(Arc::clone(&chunk));
                chunk
            };
            crate::packets::chunk(&chunk, &data)
                .map_err(|_| zerqon_world::WorldError::Corrupt("unknown block state"))
        })?;
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            ticket.wait_timeout(Duration::from_secs(30))
        })
        .await??;
        Ok(result)
    }
    pub async fn set_block(&self, x: i32, y: i32, z: i32, block: u32) -> Result<()> {
        let gameplay = Arc::clone(&self.gameplay).write_owned().await;
        if x.unsigned_abs() > 30_000_000
            || z.unsigned_abs() > 30_000_000
            || !self.data.is_valid_state(block)
        {
            return Err("invalid block position or state".into());
        }
        let players = Arc::clone(&self.players);
        let shutdown = self.shutdown.clone();
        let permit = Arc::clone(&self.inflight).acquire_owned().await?;
        let store = Arc::clone(&self.store);
        let cache = Arc::clone(&self.cache);
        let data = Arc::clone(&self.data);
        let stripes = Arc::clone(&self.stripes);
        let ticket = self.workers.try_submit(move || {
            let _gameplay = gameplay;
            if *shutdown.borrow() {
                return Err(zerqon_world::WorldError::StorageUnavailable);
            }
            let cx = x.div_euclid(16);
            let cz = z.div_euclid(16);
            let _guard = stripes[stripe(cx, cz)]
                .lock()
                .map_err(|_| zerqon_world::WorldError::Corrupt("chunk lock poisoned"))?;
            let cached = cache
                .lock()
                .map_err(|_| zerqon_world::WorldError::Corrupt("cache lock poisoned"))?
                .get(cx, cz);
            let mut chunk = if let Some(chunk) = cached {
                (*chunk).clone()
            } else {
                store
                    .load(cx, cz)?
                    .unwrap_or_else(|| Chunk::flat(cx, cz, data.flat))
            };
            validate_chunk(&chunk, &data)?;
            chunk.set_block(x.rem_euclid(16) as u8, y, z.rem_euclid(16) as u8, block)?;
            if let Err(error) = store.save(&chunk) {
                if store.check_available().is_err() {
                    let _ = shutdown.send(true);
                }
                return Err(error);
            }
            match cache.lock() {
                Ok(mut cache) => {
                    cache.insert(Arc::new(chunk));
                }
                Err(_) => {
                    let _ = shutdown.send(true);
                    return Err(zerqon_world::WorldError::StorageUnavailable);
                }
            }
            if players
                .broadcast(block_update(x, y, z, block).into())
                .is_err()
            {
                let _ = shutdown.send(true);
                return Err(zerqon_world::WorldError::StorageUnavailable);
            }
            Ok(())
        })?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            ticket.wait()
        })
        .await??;
        Ok(())
    }
}
fn validate_chunk(
    chunk: &Chunk,
    data: &GameData,
) -> std::result::Result<(), zerqon_world::WorldError> {
    for section in chunk.sections() {
        if let Some(state) = section.uniform_state() {
            if !data.is_valid_state(state) {
                return Err(zerqon_world::WorldError::Corrupt("unknown block state"));
            }
        } else if section.iter().any(|state| !data.is_valid_state(state)) {
            return Err(zerqon_world::WorldError::Corrupt("unknown block state"));
        }
    }
    Ok(())
}
fn stripe(x: i32, z: i32) -> usize {
    ((x as u32).wrapping_mul(31) ^ (z as u32)) as usize % 64
}

#[cfg(test)]
mod authorization_tests {
    use super::*;
    #[tokio::test]
    async fn a_reused_operator_name_does_not_authorize_another_uuid() {
        let directory = tempfile::tempdir().unwrap();
        crate::config::initialize(directory.path()).unwrap();
        std::fs::write(directory.path().join("ops.json"),serde_json::to_vec(&serde_json::json!([{"uuid":Uuid::from_u128(1),"name":"Admin","level":4,"bypassesPlayerLimit":false}])).unwrap()).unwrap();
        let (shutdown, receiver) = watch::channel(false);
        let state = State::new(
            directory.path(),
            Properties::parse("online-mode=false").unwrap(),
            Settings::default(),
            true,
            shutdown,
            None,
        )
        .await
        .unwrap();
        let reply = state
            .command(
                CommandSource::Player {
                    uuid: Uuid::from_u128(2),
                    name: "Admin",
                },
                "stop",
            )
            .await
            .unwrap();
        assert_eq!(reply, vec!["Permission denied"]);
        assert!(!*receiver.borrow());
    }

    #[tokio::test]
    async fn operator_rights_follow_uuid_after_a_name_change() {
        let directory = tempfile::tempdir().unwrap();
        crate::config::initialize(directory.path()).unwrap();
        let mut operators =
            crate::operator_store::OperatorStore::load(&directory.path().join("ops.json")).unwrap();
        operators.grant(Uuid::from_u128(1), "OldName", 4).unwrap();
        let (shutdown, receiver) = watch::channel(false);
        let state = State::new(
            directory.path(),
            Properties::parse("online-mode=false").unwrap(),
            Settings::default(),
            true,
            shutdown,
            None,
        )
        .await
        .unwrap();
        state
            .command(
                CommandSource::Player {
                    uuid: Uuid::from_u128(1),
                    name: "NewName",
                },
                "stop",
            )
            .await
            .unwrap();
        assert!(*receiver.borrow());
    }

    #[tokio::test]
    async fn low_level_operator_cannot_stop_server_or_start_profiler() {
        let directory = tempfile::tempdir().unwrap();
        crate::config::initialize(directory.path()).unwrap();
        let mut operators =
            crate::operator_store::OperatorStore::load(&directory.path().join("ops.json")).unwrap();
        operators.grant(Uuid::from_u128(1), "Builder", 2).unwrap();
        let (shutdown, receiver) = watch::channel(false);
        let state = State::new(
            directory.path(),
            Properties::parse("online-mode=false").unwrap(),
            Settings::default(),
            true,
            shutdown,
            None,
        )
        .await
        .unwrap();
        let source = CommandSource::Player {
            uuid: Uuid::from_u128(1),
            name: "Builder",
        };
        assert_eq!(
            state.command(source, "stop").await.unwrap(),
            vec!["Permission denied"]
        );
        assert_eq!(
            state
                .command(source, "spark profiler start")
                .await
                .unwrap_err()
                .to_string(),
            "Permission denied"
        );
        assert!(!*receiver.borrow());
        assert!(state.is_operator(Uuid::from_u128(1)));
    }
}
