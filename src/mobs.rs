use crate::{
    Result, entity_packets,
    game_data::GameData,
    mob_store,
    players::{EntityIds, PacketBatch, PlayerSnapshot, Players},
    runtime::World,
};
use bytes::Bytes;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;
use zerqon_entities::{EntityChange, EntityWorld, Mob, MobKind, PlayerTarget, WorldQuery};
use zerqon_world::{Chunk, MIN_Y, WORLD_HEIGHT};

pub struct MobConfig {
    pub path: PathBuf,
    pub capacity: usize,
    pub natural_spawning: bool,
    pub visibility_radius: f64,
}
pub struct Mobs {
    config: MobConfig,
    world: Arc<World>,
    players: Arc<Players>,
    data: Arc<GameData>,
    ids: Arc<EntityIds>,
    live: Arc<Mutex<LiveMobs>>,
    serial: tokio::sync::Mutex<()>,
}
type Observer = (Uuid, i32);
type Dispatch = Vec<(Uuid, i32, PacketBatch)>;
struct LiveMobs {
    engine: EntityWorld,
    visible: HashMap<Observer, BTreeSet<i32>>,
    last_yaw: BTreeMap<i32, f32>,
    last_attack: HashMap<Observer, Instant>,
    dirty: bool,
}

impl Mobs {
    pub async fn open(
        config: MobConfig,
        world: Arc<World>,
        players: Arc<Players>,
        data: Arc<GameData>,
        ids: Arc<EntityIds>,
    ) -> Result<Arc<Self>> {
        for kind in [MobKind::Pig, MobKind::Cow, MobKind::Zombie] {
            data.entity_type(kind.identifier())
                .ok_or("missing mob entity type in prepared data")?;
        }
        let path = config.path.clone();
        let capacity = config.capacity;
        let engine = world
            .execute(move || {
                Ok(mob_store::load(&path, capacity)?.unwrap_or(EntityWorld::new(0, capacity)?))
            })
            .await?;
        if let Some(id) = engine.entities().map(|mob| mob.id).max() {
            ids.advance_past(id)?;
        }
        Ok(Arc::new(Self {
            config,
            world,
            players,
            data,
            ids,
            live: Arc::new(Mutex::new(LiveMobs {
                engine,
                visible: HashMap::new(),
                last_yaw: BTreeMap::new(),
                last_attack: HashMap::new(),
                dirty: false,
            })),
            serial: tokio::sync::Mutex::new(()),
        }))
    }

    pub async fn tick(&self, tick: u64) -> Result<()> {
        let _serial = self.serial.lock().await;
        let observers = self.players.snapshots()?;
        if observers.is_empty() {
            {
                let mut live = self.live.lock().map_err(|_| "mob state lock poisoned")?;
                live.visible.clear();
                live.last_attack.clear();
            }
            return self.flush_locked().await;
        }
        let natural = self.config.natural_spawning && tick.is_multiple_of(20);
        let no_work = {
            let live = self.live.lock().map_err(|_| "mob state lock poisoned")?;
            live.engine.is_empty() && !live.dirty && live.visible.values().all(BTreeSet::is_empty)
        };
        if no_work && !natural {
            return Ok(());
        }
        let live = Arc::clone(&self.live);
        let data = Arc::clone(&self.data);
        let world = Arc::clone(&self.world);
        let ids = Arc::clone(&self.ids);
        let radius = self.config.visibility_radius;
        let path = self.config.path.clone();
        let capacity = self.config.capacity;
        let (batches, save_error) = self
            .world
            .execute(move || {
                let view = ChunkView::new(world.cached_chunks()?, Arc::clone(&data));
                let targets: Vec<_> = observers
                    .iter()
                    .map(|player| PlayerTarget {
                        id: player.id,
                        position: player.position.xyz,
                        attackable: false,
                    })
                    .collect();
                let mut live = live.lock().map_err(|_| "mob state lock poisoned")?;
                let mut changes = live.engine.tick(&view, &targets);
                let mut spawned = false;
                if natural && live.engine.len() < capacity {
                    let id = ids.next()?;
                    if let Some(change) =
                        live.engine
                            .try_natural_spawn(&view, &targets, false, || (id, Uuid::new_v4()))?
                    {
                        changes.push(change);
                        spawned = true;
                    }
                }
                live.dirty = true;
                let save_error = if spawned || tick.is_multiple_of(100) {
                    match mob_store::save(&path, &live.engine.snapshot()) {
                        Ok(()) => {
                            live.dirty = false;
                            None
                        }
                        Err(error) => Some(error),
                    }
                } else {
                    None
                };
                Ok((
                    live.batches(&observers, &changes, &data, radius)?,
                    save_error,
                ))
            })
            .await?;
        self.players.dispatch_batches(batches)?;
        if let Some(error) = save_error {
            return Err(error.into());
        }
        Ok(())
    }

    pub async fn synchronize(&self) -> Result<()> {
        let _serial = self.serial.lock().await;
        let observers = self.players.snapshots()?;
        let live = Arc::clone(&self.live);
        let data = Arc::clone(&self.data);
        let radius = self.config.visibility_radius;
        let batches = self
            .world
            .execute(move || {
                live.lock().map_err(|_| "mob state lock poisoned")?.batches(
                    &observers,
                    &[],
                    &data,
                    radius,
                )
            })
            .await?;
        self.players.dispatch_batches(batches)?;
        Ok(())
    }

    pub async fn summon(&self, kind: MobKind, position: [f64; 3]) -> Result<i32> {
        if position.iter().any(|value| !value.is_finite())
            || position[0].abs() > 29_999_984.0
            || position[2].abs() > 29_999_984.0
            || !(-64.0..=318.0).contains(&position[1])
        {
            return Err("invalid summon coordinates".into());
        }
        let _serial = self.serial.lock().await;
        let cx = (position[0].floor() as i32).div_euclid(16);
        let cz = (position[2].floor() as i32).div_euclid(16);
        let mut chunks = self.world.cached_chunks()?;
        for x in cx - 1..=cx + 1 {
            for z in cz - 1..=cz + 1 {
                chunks.push(self.world.chunk(x, z).await?);
            }
        }
        let observers = self.players.snapshots()?;
        let live = Arc::clone(&self.live);
        let data = Arc::clone(&self.data);
        let id = self.ids.next()?;
        let path = self.config.path.clone();
        let capacity = self.config.capacity;
        let radius = self.config.visibility_radius;
        let batches = self
            .world
            .execute(move || {
                let view = ChunkView::new(chunks, Arc::clone(&data));
                let mut live = live.lock().map_err(|_| "mob state lock poisoned")?;
                let before = live.engine.snapshot();
                let change = live
                    .engine
                    .spawn(id, Uuid::new_v4(), kind, position, &view)?;
                persist_mutation(&mut live, &path, before, capacity)?;
                live.batches(&observers, &[change], &data, radius)
            })
            .await?;
        self.players.dispatch_batches(batches)?;
        Ok(id)
    }

    pub async fn attack(&self, uuid: Uuid, player_id: i32, target: i32) -> Result<()> {
        let _serial = self.serial.lock().await;
        let observers = self.players.snapshots()?;
        let attacker = observers
            .iter()
            .find(|player| player.uuid == uuid && player.id == player_id)
            .ok_or("attacker is no longer connected")?
            .position
            .xyz;
        let live = Arc::clone(&self.live);
        let data = Arc::clone(&self.data);
        let world = Arc::clone(&self.world);
        let path = self.config.path.clone();
        let capacity = self.config.capacity;
        let radius = self.config.visibility_radius;
        let batches = self
            .world
            .execute(move || {
                let view = ChunkView::new(world.cached_chunks()?, Arc::clone(&data));
                let mut live = live.lock().map_err(|_| "mob state lock poisoned")?;
                let observer = (uuid, player_id);
                if live
                    .last_attack
                    .get(&observer)
                    .is_some_and(|last| last.elapsed() < Duration::from_millis(500))
                    || !live
                        .visible
                        .get(&observer)
                        .is_some_and(|ids| ids.contains(&target))
                    || !live.engine.reachable_from(target, attacker, &view)
                {
                    return Ok(Vec::new());
                }
                let before = live.engine.snapshot();
                let changes = live.engine.damage(target, 4.0)?;
                persist_mutation(&mut live, &path, before, capacity)?;
                live.last_attack.insert(observer, Instant::now());
                live.batches(&observers, &changes, &data, radius)
            })
            .await?;
        self.players.dispatch_batches(batches)?;
        Ok(())
    }

    pub async fn flush(&self) -> Result<()> {
        let _serial = self.serial.lock().await;
        self.flush_locked().await
    }
    async fn flush_locked(&self) -> Result<()> {
        if !self
            .live
            .lock()
            .map_err(|_| "mob state lock poisoned")?
            .dirty
        {
            return Ok(());
        }
        let live = Arc::clone(&self.live);
        let path = self.config.path.clone();
        self.world
            .execute(move || {
                let mut live = live.lock().map_err(|_| "mob state lock poisoned")?;
                if live.dirty {
                    mob_store::save(&path, &live.engine.snapshot())?;
                    live.dirty = false;
                }
                Ok(())
            })
            .await
    }
}

impl LiveMobs {
    fn batches(
        &mut self,
        observers: &[PlayerSnapshot],
        changes: &[EntityChange],
        data: &GameData,
        radius: f64,
    ) -> Result<Dispatch> {
        let mobs: Vec<_> = self.engine.entities().cloned().collect();
        let moved: HashSet<_> = changes
            .iter()
            .filter_map(|change| {
                if let EntityChange::Moved { id, .. } = change {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect();
        let active: HashSet<_> = observers
            .iter()
            .map(|player| (player.uuid, player.id))
            .collect();
        self.visible.retain(|observer, _| active.contains(observer));
        self.last_attack
            .retain(|observer, _| active.contains(observer));
        let mut dispatch = Vec::new();
        for player in observers {
            let observer = (player.uuid, player.id);
            let previous = self.visible.remove(&observer).unwrap_or_default();
            let current: BTreeSet<_> = mobs
                .iter()
                .filter(|mob| {
                    (mob.position[0] - player.position.xyz[0]).abs() <= radius
                        && (mob.position[2] - player.position.xyz[2]).abs() <= radius
                })
                .map(|mob| mob.id)
                .collect();
            let mut batch = Vec::new();
            for change in changes {
                if let EntityChange::Health { id, health } = change
                    && previous.contains(id)
                {
                    batch.push(Bytes::from(entity_packets::mob_health(*id, *health)));
                }
            }
            let removed: Vec<_> = previous.difference(&current).copied().collect();
            if !removed.is_empty() {
                batch.push(entity_packets::remove_entities(&removed).into());
            }
            for mob in mobs.iter().filter(|mob| current.contains(&mob.id)) {
                if !previous.contains(&mob.id) {
                    batch.extend(spawn_packets(mob, data)?);
                } else if moved.contains(&mob.id) {
                    batch.push(
                        entity_packets::position_sync(
                            mob.id,
                            mob.position,
                            mob.yaw,
                            mob.pitch,
                            mob.on_ground,
                        )
                        .into(),
                    );
                    if self
                        .last_yaw
                        .get(&mob.id)
                        .is_none_or(|yaw| yaw.to_bits() != mob.yaw.to_bits())
                    {
                        batch.push(entity_packets::head_rotation(mob.id, mob.yaw).into());
                    }
                }
            }
            self.visible.insert(observer, current);
            if !batch.is_empty() {
                dispatch.push((player.uuid, player.id, batch));
            }
        }
        self.last_yaw = mobs.iter().map(|mob| (mob.id, mob.yaw)).collect();
        Ok(dispatch)
    }
}

fn persist_mutation(
    live: &mut LiveMobs,
    path: &std::path::Path,
    before: zerqon_entities::Snapshot,
    capacity: usize,
) -> Result<()> {
    match mob_store::save(path, &live.engine.snapshot()) {
        Ok(()) => live.dirty = false,
        Err(error) if error.was_committed() => {
            live.dirty = true;
            tracing::warn!(%error,"Mob snapshot commit needs a durability retry");
        }
        Err(error) => {
            live.engine = EntityWorld::restore(before, capacity)?;
            return Err(error.into());
        }
    }
    Ok(())
}

fn spawn_packets(mob: &Mob, data: &GameData) -> Result<PacketBatch> {
    let kind = data
        .entity_type(mob.kind.identifier())
        .ok_or("missing mob entity type")?;
    Ok(vec![
        entity_packets::add_entity(mob.id, mob.uuid, kind, mob.position, mob.yaw, mob.pitch).into(),
        entity_packets::mob_health(mob.id, mob.health).into(),
    ])
}

struct ChunkView {
    chunks: HashMap<(i32, i32), Arc<Chunk>>,
    data: Arc<GameData>,
}
impl ChunkView {
    fn new(chunks: Vec<Arc<Chunk>>, data: Arc<GameData>) -> Self {
        Self {
            chunks: chunks
                .into_iter()
                .map(|chunk| ((chunk.x, chunk.z), chunk))
                .collect(),
            data,
        }
    }
}
impl WorldQuery for ChunkView {
    fn loaded_at(&self, x: i32, z: i32) -> bool {
        self.chunks
            .contains_key(&(x.div_euclid(16), z.div_euclid(16)))
    }
    fn block_solid(&self, x: i32, y: i32, z: i32) -> bool {
        let Some(chunk) = self.chunks.get(&(x.div_euclid(16), z.div_euclid(16))) else {
            return true;
        };
        if !(MIN_Y..MIN_Y + WORLD_HEIGHT as i32).contains(&y) {
            return false;
        }
        let Some(state) = chunk.get_block(x.rem_euclid(16) as u8, y, z.rem_euclid(16) as u8) else {
            return true;
        };
        !self.data.is_air(state) && !self.data.is_fluid_block(state)
    }
    fn surface_y(&self, x: i32, z: i32) -> Option<i32> {
        if !self.loaded_at(x, z) {
            return None;
        }
        (MIN_Y..MIN_Y + WORLD_HEIGHT as i32)
            .rev()
            .find(|y| self.block_solid(x, *y, z))
            .map(|y| y + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires prepared official Minecraft 26.3 data"]
    async fn failed_tick_save_still_removes_dead_mob_and_retries_dirty_snapshot() {
        use crate::{
            config::{self, Properties, Settings},
            players::Identity,
            runtime::State,
        };
        let directory = tempfile::tempdir().unwrap();
        config::initialize(directory.path()).unwrap();
        let settings = Settings {
            data_directory: std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(".cache/vanilla/26.3")
                .to_string_lossy()
                .into_owned(),
            java_plugins: false,
            update_check: false,
            spawn_mobs: false,
            ..Settings::default()
        };
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let state = State::new(
            directory.path(),
            Properties::parse("online-mode=false\nview-distance=1\n").unwrap(),
            settings,
            false,
            shutdown,
            None,
        )
        .await
        .unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(32);
        let (disconnect, _) = tokio::sync::watch::channel(false);
        let player = state
            .players
            .reserve(
                Identity {
                    uuid: Uuid::from_u128(1),
                    name: "Tester".into(),
                    properties: vec![],
                },
                sender,
                disconnect,
            )
            .unwrap();
        player.activate().unwrap();
        let mobs = state.mobs.as_ref().unwrap();
        let id = mobs.summon(MobKind::Pig, [2.5, 64.0, 0.5]).await.unwrap();
        while receiver.try_recv().is_ok() {}
        {
            let mut live = mobs.live.lock().unwrap();
            let mut snapshot = live.engine.snapshot();
            snapshot.entities[0].position[1] = -79.99;
            snapshot.entities[0].velocity[1] = -0.1;
            live.engine = EntityWorld::restore(snapshot, 128).unwrap();
        }
        std::fs::write(&mobs.config.path, b"corrupt snapshot").unwrap();
        assert!(mobs.tick(100).await.is_err());
        let packets: Vec<_> = std::iter::from_fn(|| receiver.try_recv().ok())
            .flatten()
            .collect();
        assert!(
            packets
                .iter()
                .any(|packet| packet.as_ref() == entity_packets::remove_entities(&[id])),
            "death removal was swallowed by snapshot save failure"
        );
        assert!(mobs.live.lock().unwrap().dirty);
        assert!(
            mobs.tick(200).await.is_err(),
            "empty engine skipped retrying dirty snapshot"
        );
        assert_eq!(
            std::fs::read(&mobs.config.path).unwrap(),
            b"corrupt snapshot"
        );
    }
}
