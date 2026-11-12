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
use zerqon_entities::{
    BlockCollision, EntityChange, EntityWorld, Mob, MobKind, PlayerTarget, WorldQuery,
};
use zerqon_world::{Chunk, MIN_Y, WORLD_HEIGHT};

pub struct MobConfig {
    pub path: PathBuf,
    pub capacity: usize,
    pub natural_spawning: bool,
    pub visibility_radius: f64,
}
pub struct Mobs {
    config: MobConfig,
    gameplay: Arc<tokio::sync::RwLock<()>>,
    world: Arc<World>,
    players: Arc<Players>,
    data: Arc<GameData>,
    blocks: Arc<zerqon_blocks::Catalog>,
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
    pub(crate) fn blast_targets(&self, center: [f64; 3]) -> Result<Vec<(i32, Uuid)>> {
        let live = self.live.lock().map_err(|_| "mob state lock poisoned")?;
        Ok(live
            .engine
            .entities()
            .filter(|mob| {
                mob.health > 0.0 && crate::blast_damage::within_blast(center, mob.position)
            })
            .map(|mob| (mob.id, mob.uuid))
            .collect())
    }

    pub(crate) async fn apply_blast(
        &self,
        center: [f64; 3],
        view: Arc<crate::blast_damage::BlastView>,
        targets: Vec<(i32, Uuid)>,
    ) -> Result<()> {
        if targets.is_empty() {
            return Ok(());
        }
        let _serial = self.serial.lock().await;
        let gameplay = Arc::clone(&self.gameplay).read_owned().await;
        let observers = self.players.snapshots()?;
        let live = Arc::clone(&self.live);
        let data = Arc::clone(&self.data);
        let path = self.config.path.clone();
        let capacity = self.config.capacity;
        let radius = self.config.visibility_radius;
        let batches = self
            .world
            .execute(move || {
                let _gameplay = gameplay;
                let mut live = live.lock().map_err(|_| "mob state lock poisoned")?;
                let mut effects = Vec::new();
                for (id, uuid) in targets {
                    let Some(mob) = live
                        .engine
                        .get(id)
                        .filter(|mob| mob.uuid == uuid && mob.health > 0.0)
                    else {
                        continue;
                    };
                    let dimensions = mob.dimensions();
                    if let Some(effect) =
                        view.effect(center, mob.position, dimensions, dimensions[1] * 0.85)
                    {
                        effects.push((id, effect));
                    }
                }
                if effects.is_empty() {
                    return Ok(Vec::new());
                }
                let before = live.engine.snapshot();
                let mut changes = Vec::new();
                for (id, effect) in effects {
                    changes.extend(live.engine.damage_with_impulse(
                        id,
                        effect.damage,
                        effect.impulse,
                    )?);
                }
                persist_mutation(&mut live, &path, before, capacity)?;
                live.batches(&observers, &changes, &data, radius)
            })
            .await?;
        self.players.dispatch_batches(batches)?;
        Ok(())
    }

    pub fn blockers_near(&self, position: [f64; 3]) -> Result<Vec<zerqon_blocks::Aabb>> {
        let live = self.live.lock().map_err(|_| "mob state lock poisoned")?;
        let mut blockers = Vec::new();
        for mob in live.engine.entities().filter(|mob| mob.health > 0.0) {
            let bounds = zerqon_entities::scaled_bounds(mob.position, mob.kind, mob.scale());
            if (0..3).all(|axis| {
                bounds.max[axis] >= position[axis] - 3.0 && bounds.min[axis] <= position[axis] + 4.0
            }) {
                if blockers.len() == 1024 {
                    return Err("too many nearby entities for block interaction".into());
                }
                blockers.push(bounds);
            }
        }
        Ok(blockers)
    }
    pub async fn open(
        config: MobConfig,
        world: Arc<World>,
        players: Arc<Players>,
        data: Arc<GameData>,
        blocks: Arc<zerqon_blocks::Catalog>,
        ids: Arc<EntityIds>,
        gameplay: Arc<tokio::sync::RwLock<()>>,
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
            gameplay,
            world,
            players,
            data,
            blocks,
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

    #[cfg(test)]
    pub async fn tick(&self, tick: u64) -> Result<()> {
        self.tick_with_food(tick, Vec::new()).await
    }

    pub async fn tick_with_food(
        &self,
        tick: u64,
        food: Vec<(MobKind, PlayerTarget)>,
    ) -> Result<()> {
        let _serial = self.serial.lock().await;
        let gameplay = Arc::clone(&self.gameplay).read_owned().await;
        let observers = self.players.snapshots()?;
        if observers.is_empty() {
            {
                let mut live = self.live.lock().map_err(|_| "mob state lock poisoned")?;
                live.visible.clear();
                live.last_attack.clear();
            }
            return self.flush_locked().await;
        }
        let natural = self.config.natural_spawning && tick.is_multiple_of(200);
        let no_work = {
            let live = self.live.lock().map_err(|_| "mob state lock poisoned")?;
            live.engine.is_empty() && !live.dirty && live.visible.values().all(BTreeSet::is_empty)
        };
        if no_work && !natural {
            return Ok(());
        }
        let live = Arc::clone(&self.live);
        let data = Arc::clone(&self.data);
        let blocks = Arc::clone(&self.blocks);
        let world = Arc::clone(&self.world);
        let ids = Arc::clone(&self.ids);
        let radius = self.config.visibility_radius;
        let path = self.config.path.clone();
        let capacity = self.config.capacity;
        let targets = self.players.combat_targets()?;
        let players = Arc::clone(&self.players);
        let (batches, save_error) = self
            .world
            .execute(move || {
                let _gameplay = gameplay;
                let view = ChunkView::new(world.cached_chunks()?, blocks);
                let mut live = live.lock().map_err(|_| "mob state lock poisoned")?;
                let mut changes = live.engine.tick_with_food(&view, &targets, &food);
                for change in &changes {
                    if let EntityChange::AttackPlayer {
                        attacker,
                        player,
                        damage,
                    } = change
                        && let Some(mob) = live.engine.get(*attacker)
                    {
                        players.mob_attack(*player, mob.position, *damage)?;
                    }
                }
                for change in &changes {
                    if let EntityChange::PhysicsBlocked { id, reason } = change {
                        tracing::debug!(id,%reason,"Mob physics paused");
                    }
                }
                let mut spawned = false;
                let pairs = live.engine.breeding_pairs();
                if !pairs.is_empty() {
                    let before = live.engine.snapshot();
                    let mut births = Vec::new();
                    for (first, second) in pairs {
                        if let Some(child) =
                            live.engine
                                .breed(first, second, ids.next()?, Uuid::new_v4(), &view)?
                        {
                            births.push(child);
                        }
                    }
                    if !births.is_empty() {
                        persist_mutation(&mut live, &path, before, capacity)?;
                        changes.extend(births);
                    }
                }
                if natural && live.engine.len() < capacity {
                    let id = ids.next()?;
                    if let Some(change) = live.engine.try_natural_spawn(
                        &view,
                        &targets,
                        (tick % 24000) >= 13000,
                        || (id, Uuid::new_v4()),
                    )? {
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
        let gameplay = Arc::clone(&self.gameplay).read_owned().await;
        let cx = (position[0].floor() as i32).div_euclid(16);
        let cz = (position[2].floor() as i32).div_euclid(16);
        let mut chunks = self.world.cached_chunks()?;
        for x in cx - 1..=cx + 1 {
            for z in cz - 1..=cz + 1 {
                match self.world.chunk(x, z).await {
                    Ok(chunk) => chunks.push(chunk),
                    Err(error) if World::missing_imported_chunk(error.as_ref()) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        let observers = self.players.snapshots()?;
        let live = Arc::clone(&self.live);
        let data = Arc::clone(&self.data);
        let id = self.ids.next()?;
        let blocks = Arc::clone(&self.blocks);
        let path = self.config.path.clone();
        let capacity = self.config.capacity;
        let radius = self.config.visibility_radius;
        let batches = self
            .world
            .execute(move || {
                let _gameplay = gameplay;
                let view = ChunkView::new(chunks, blocks);
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
        let gameplay = Arc::clone(&self.gameplay).read_owned().await;
        let observers = self.players.snapshots()?;
        let attacker = observers
            .iter()
            .find(|player| player.uuid == uuid && player.id == player_id)
            .ok_or("attacker is no longer connected")?
            .position
            .xyz;
        let live = Arc::clone(&self.live);
        let data = Arc::clone(&self.data);
        let blocks = Arc::clone(&self.blocks);
        let world = Arc::clone(&self.world);
        let path = self.config.path.clone();
        let capacity = self.config.capacity;
        let radius = self.config.visibility_radius;
        let batches = self
            .world
            .execute(move || {
                let _gameplay = gameplay;
                let view = ChunkView::new(world.cached_chunks()?, blocks);
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
                let changes = live.engine.damage_with_knockback(target, 4.0, attacker)?;
                persist_mutation(&mut live, &path, before, capacity)?;
                live.last_attack.insert(observer, Instant::now());
                live.batches(&observers, &changes, &data, radius)
            })
            .await?;
        self.players.dispatch_batches(batches)?;
        Ok(())
    }

    pub async fn interact(
        &self,
        uuid: Uuid,
        player_id: i32,
        target: i32,
        hand: zerqon_inventory::Hand,
        inventory: &crate::player_inventory::InventoryHandle,
    ) -> Result<bool> {
        let _serial = self.serial.lock().await;
        let Some(stack) = inventory.held_stack(hand)? else {
            return Ok(false);
        };
        let item = inventory.item_key(&stack)?;
        let gameplay = Arc::clone(&self.gameplay).read_owned().await;
        if !self.players.is_alive(uuid, player_id)? {
            return Ok(false);
        }
        let actor = self
            .players
            .query_snapshot(uuid)?
            .filter(|p| p.id == player_id)
            .ok_or("Player is no longer connected")?;
        let view = ChunkView::new(self.world.cached_chunks()?, Arc::clone(&self.blocks));
        let mob_id = {
            let live = self.live.lock().map_err(|_| "mob lock poisoned")?;
            let Some(mob) = live.engine.get(target) else {
                return Ok(false);
            };
            let food = match mob.kind {
                MobKind::Cow => item == "minecraft:wheat",
                MobKind::Pig => matches!(
                    item.as_str(),
                    "minecraft:carrot" | "minecraft:potato" | "minecraft:beetroot"
                ),
                _ => false,
            };
            if !food
                || !mob.can_feed()
                || !live
                    .engine
                    .reachable_from(target, actor.position.xyz, &view)
            {
                return Ok(false);
            }
            mob.uuid
        };
        drop(gameplay);
        inventory.consume_hand(hand, stack).await?;
        let gameplay = Arc::clone(&self.gameplay).read_owned().await;
        let live = Arc::clone(&self.live);
        let path = self.config.path.clone();
        let capacity = self.config.capacity;
        let data = Arc::clone(&self.data);
        let observers = self.players.snapshots()?;
        let radius = self.config.visibility_radius;
        let batches = self
            .world
            .execute(move || {
                let _gameplay = gameplay;
                let mut live = live.lock().map_err(|_| "mob lock poisoned")?;
                if !live.engine.get(target).is_some_and(|m| m.uuid == mob_id) {
                    return Err("Animal is no longer available".into());
                }
                let before = live.engine.snapshot();
                let changes = live.engine.feed(target)?;
                persist_mutation(&mut live, &path, before, capacity)?;
                live.batches(&observers, &changes, &data, radius)
            })
            .await?;
        self.players.dispatch_batches(batches)?;
        Ok(true)
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
                if let EntityChange::Love { id } = change
                    && previous.contains(id)
                {
                    batch.push(entity_packets::entity_event(*id, 18).into());
                }
                if let EntityChange::AgeChanged { id, baby } = change
                    && previous.contains(id)
                {
                    batch.push(entity_packets::animal_age(*id, *baby).into());
                }
                if let EntityChange::Health { id, health } = change
                    && previous.contains(id)
                {
                    batch.push(Bytes::from(entity_packets::mob_health(*id, *health)));
                    // Original non-player LivingEntity.getHurtDir() returns zero.
                    batch.push(entity_packets::hurt_animation(*id, 0.0)?.into());
                    if let Some(mob) = self.engine.get(*id) {
                        batch.push(entity_packets::entity_motion(*id, mob.velocity)?.into());
                    }
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
                dispatch.push((player.uuid, player.id, batch.into()));
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
    let mut packets: Vec<Bytes> = vec![
        entity_packets::add_entity_with_motion(
            mob.id,
            mob.uuid,
            kind,
            mob.position,
            mob.velocity,
            mob.yaw,
            mob.pitch,
        )?
        .into(),
        entity_packets::mob_health(mob.id, mob.health).into(),
    ];
    if mob.kind != MobKind::Zombie {
        packets.push(entity_packets::animal_age(mob.id, mob.is_baby()).into());
    }
    Ok(packets.into())
}

struct ChunkView {
    chunks: HashMap<(i32, i32), Arc<Chunk>>,
    blocks: Arc<zerqon_blocks::Catalog>,
}
impl ChunkView {
    fn new(chunks: Vec<Arc<Chunk>>, blocks: Arc<zerqon_blocks::Catalog>) -> Self {
        Self {
            chunks: chunks
                .into_iter()
                .map(|chunk| ((chunk.x, chunk.z), chunk))
                .collect(),
            blocks,
        }
    }
}
impl WorldQuery for ChunkView {
    fn water_at(&self, x: i32, y: i32, z: i32) -> bool {
        self.chunks
            .get(&(x.div_euclid(16), z.div_euclid(16)))
            .and_then(|c| c.get_block(x.rem_euclid(16) as u8, y, z.rem_euclid(16) as u8))
            .and_then(|id| self.blocks.state(zerqon_blocks::StateId(id)).ok())
            .is_some_and(|state| state.key() == "minecraft:water")
    }
    fn loaded_at(&self, x: i32, z: i32) -> bool {
        self.chunks
            .contains_key(&(x.div_euclid(16), z.div_euclid(16)))
    }
    fn block_solid(&self, x: i32, y: i32, z: i32) -> bool {
        match self.block_collision(x, y, z) {
            BlockCollision::Empty => false,
            BlockCollision::Static(boxes) => !boxes.is_empty(),
            _ => true,
        }
    }
    fn block_collision(&self, x: i32, y: i32, z: i32) -> BlockCollision<'_> {
        if !(MIN_Y..MIN_Y + WORLD_HEIGHT as i32).contains(&y) {
            return BlockCollision::Empty;
        }
        let Some(chunk) = self.chunks.get(&(x.div_euclid(16), z.div_euclid(16))) else {
            return BlockCollision::Unavailable;
        };
        let Some(state) = chunk.get_block(x.rem_euclid(16) as u8, y, z.rem_euclid(16) as u8) else {
            return BlockCollision::Unavailable;
        };
        let Ok(state) = self.blocks.state(zerqon_blocks::StateId(state)) else {
            return BlockCollision::Unsupported;
        };
        if state.has_fluid() && state.key() != "minecraft:water" {
            return BlockCollision::Unsupported;
        }
        match state.collision() {
            Ok([]) => BlockCollision::Empty,
            Ok(boxes) => BlockCollision::Static(boxes),
            Err(_) => BlockCollision::Unsupported,
        }
    }
    fn surface_y(&self, x: i32, z: i32) -> Option<i32> {
        self.surface_height(x, z).map(|height| height.ceil() as i32)
    }
    fn surface_height(&self, x: i32, z: i32) -> Option<f64> {
        if !self.loaded_at(x, z) {
            return None;
        }
        for y in (MIN_Y..MIN_Y + WORLD_HEIGHT as i32).rev() {
            match self.block_collision(x, y, z) {
                BlockCollision::Empty => {}
                BlockCollision::Full => return Some(f64::from(y) + 1.0),
                BlockCollision::Static(boxes) => {
                    if let Some(top) = boxes
                        .iter()
                        .filter(|bounds| {
                            bounds.min[0] <= 0.5
                                && bounds.max[0] >= 0.5
                                && bounds.min[2] <= 0.5
                                && bounds.max[2] >= 0.5
                        })
                        .map(|bounds| bounds.max[1])
                        .max_by(f64::total_cmp)
                    {
                        return Some(f64::from(y) + top);
                    }
                }
                BlockCollision::Unavailable | BlockCollision::Unsupported => return None,
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn shape_view(state: &str, properties: &[(&str, &str)]) -> ChunkView {
        static BLOCKS: std::sync::OnceLock<Arc<zerqon_blocks::Catalog>> =
            std::sync::OnceLock::new();
        let blocks = Arc::clone(
            BLOCKS.get_or_init(|| Arc::new(zerqon_blocks::Catalog::embedded().unwrap())),
        );
        let id = |name: &str| blocks.resolve(name, &BTreeMap::new()).unwrap().0;
        let palette = zerqon_world::FlatPalette {
            air: id("minecraft:air"),
            bedrock: id("minecraft:bedrock"),
            stone: id("minecraft:stone"),
            dirt: id("minecraft:dirt"),
            grass: id("minecraft:grass_block"),
        };
        let mut chunks = Vec::new();
        for x in -1..=1 {
            for z in -1..=1 {
                chunks.push(Arc::new(Chunk::flat(x, z, palette)));
            }
        }
        let mut center = Chunk::flat(0, 0, palette);
        let properties = properties
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        center
            .set_block(1, 64, 1, blocks.resolve(state, &properties).unwrap().0)
            .unwrap();
        chunks.push(Arc::new(center));
        ChunkView::new(chunks, blocks)
    }

    #[test]
    fn chunk_view_uses_fractional_slab_surface_and_real_mob_collision() {
        let view = shape_view("minecraft:oak_slab", &[]);
        assert_eq!(view.surface_height(1, 1), Some(64.5));
        let mut engine = EntityWorld::new(1, 8).unwrap();
        engine
            .spawn(1, Uuid::from_u128(1), MobKind::Pig, [1.5, 64.5, 1.5], &view)
            .unwrap();
        engine.tick(&view, &[]);
        assert_eq!(engine.get(1).unwrap().position[1], 64.5);
        assert!(engine.get(1).unwrap().on_ground);
    }

    #[test]
    fn chunk_view_refuses_fluid_physics_and_reports_unavailable_chunks() {
        let view = shape_view("minecraft:oak_fence", &[("waterlogged", "true")]);
        assert!(matches!(
            view.block_collision(1, 64, 1),
            BlockCollision::Unsupported
        ));
        assert_eq!(view.surface_height(1, 1), None);
        assert!(matches!(
            view.block_collision(128, 64, 128),
            BlockCollision::Unavailable
        ));
        assert!(matches!(
            view.block_collision(128, -65, 128),
            BlockCollision::Empty
        ));
    }
    #[tokio::test]
    #[ignore = "requires prepared official Minecraft 26.3 data"]
    async fn summon_inside_imported_chunk_does_not_require_unrelated_neighbors() {
        use crate::{
            config::{self, Properties, Settings},
            game_data::BlockResolver,
            runtime::State,
        };
        let directory = tempfile::tempdir().unwrap();
        config::initialize(directory.path()).unwrap();
        let source = directory.path().join("source");
        std::fs::create_dir_all(source.join("dimensions/minecraft/overworld/region")).unwrap();
        std::fs::create_dir_all(source.join("data/minecraft")).unwrap();
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("crates/zerqon-world/tests/fixtures");
        for (from, to) in [
            ("metadata-level.dat", "level.dat"),
            (
                "metadata-world-gen-settings.dat",
                "data/minecraft/world_gen_settings.dat",
            ),
            ("metadata-game-rules.dat", "data/minecraft/game_rules.dat"),
            (
                "negative-3.mca",
                "dimensions/minecraft/overworld/region/r.-1.-2.mca",
            ),
        ] {
            std::fs::copy(fixtures.join(from), source.join(to)).unwrap();
        }
        let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/vanilla/26.3");
        let resolver = BlockResolver::load(&data).unwrap();
        zerqon_world::anvil::import_world_blocks(
            &source,
            &directory.path().join("world"),
            &|name, properties| resolver.resolve(name, properties),
        )
        .unwrap();
        let settings = Settings {
            data_directory: data.to_string_lossy().into_owned(),
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
        let mobs = state.mobs.as_ref().unwrap();
        let id = mobs
            .summon(MobKind::Pig, [-8.5, 70.0, -520.5])
            .await
            .unwrap();
        assert_eq!(
            mobs.live.lock().unwrap().engine.get(id).unwrap().position,
            [-8.5, 70.0, -520.5]
        );
        let world = state.world.as_ref().unwrap();
        assert!(World::missing_imported_chunk(
            world.chunk(-2, -34).await.unwrap_err().as_ref()
        ));
    }
    #[tokio::test]
    #[ignore = "requires prepared 26.3 data and owned real vanilla flat-world fixture"]
    async fn console_summon_defaults_to_imported_world_spawn_height() {
        use crate::{
            config::{self, Properties, Settings},
            game_data::BlockResolver,
            runtime::{CommandSource, State},
        };
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let data = root.join(".cache/vanilla/26.3");
        let source = root.join(".local/vanilla-world-filled/world");
        let directory = tempfile::tempdir().unwrap();
        config::initialize(directory.path()).unwrap();
        let resolver = BlockResolver::load(&data).unwrap();
        zerqon_world::anvil::import_world_blocks(
            &source,
            &directory.path().join("world"),
            &|name, properties| resolver.resolve(name, properties),
        )
        .unwrap();
        let settings = Settings {
            data_directory: data.to_string_lossy().into_owned(),
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
        let spawn = state.world.as_ref().unwrap().spawn();
        assert_eq!(spawn.position[1], -60);
        let replies = state
            .command(CommandSource::Console, "summon pig")
            .await
            .unwrap();
        assert!(
            replies
                .iter()
                .any(|reply| reply.contains("Summoned minecraft:pig")),
            "{replies:?}"
        );
        let live = state.mobs.as_ref().unwrap().live.lock().unwrap();
        let pig = live.engine.entities().next().unwrap();
        assert_eq!(
            pig.position,
            [
                f64::from(spawn.position[0]) + 0.5,
                -60.0,
                f64::from(spawn.position[2]) + 0.5
            ]
        );
    }
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
