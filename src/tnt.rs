use crate::{
    Result,
    blast_damage::BlastDamage,
    entity_packets,
    game_data::GameData,
    players::{EntityIds, PacketBatch, PlayerSnapshot, Players},
    runtime::{BulkEditConflict, World},
    tnt_packets,
    tnt_physics::{Body, Terrain},
};
use bytes::Bytes;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};
use tokio::sync::{Mutex, OwnedMutexGuard};
use uuid::Uuid;

const MAX_ACTIVE: usize = 4096;
const FUSE: u16 = 80;
const RANGE: f64 = 64.0;
type Observer = (Uuid, i32);

pub(crate) struct IgnitionReservation {
    service: Arc<TntService>,
    live: OwnedMutexGuard<Live>,
    prepared: Option<Primed>,
}

impl IgnitionReservation {
    pub(crate) fn stage(&mut self, position: [i32; 3]) -> Result<()> {
        if self.live.active.len() >= MAX_ACTIVE {
            return Err("TNT active limit reached".into());
        }
        self.prepared = Some(self.service.primed(position, FUSE)?);
        Ok(())
    }

    pub(crate) fn publish(mut self) -> Result<()> {
        let entity = self
            .prepared
            .take()
            .ok_or("TNT ignition was not prepared")?;
        self.live.active.insert(entity.id, entity);
        self.service.counters(&self.live);
        self.service.synchronize(&mut self.live, false)
    }
}

#[derive(Clone)]
struct Primed {
    id: i32,
    uuid: Uuid,
    body: Body,
    fuse: u16,
    paused: bool,
}
impl Primed {
    fn position(&self) -> [f64; 3] {
        self.body.position
    }
}

#[derive(Default)]
struct Live {
    active: BTreeMap<i32, Primed>,
    visible: HashMap<Observer, BTreeSet<i32>>,
    load_cursor: usize,
}

/// Bounded TNT physics and terrain explosions.
pub struct TntService {
    world: Arc<World>,
    players: Arc<Players>,
    data: Arc<GameData>,
    ids: Arc<EntityIds>,
    blocks: Arc<zerqon_blocks::Catalog>,
    damage: Arc<BlastDamage>,
    type_id: i32,
    explosions_per_tick: usize,
    tnt: u32,
    protected: BTreeSet<u32>,
    live: Arc<Mutex<Live>>,
    active: AtomicUsize,
    ready: AtomicUsize,
    detonated: AtomicU64,
    destroyed: AtomicU64,
    paused: AtomicUsize,
}

impl TntService {
    pub fn new(
        world: Arc<World>,
        players: Arc<Players>,
        data: Arc<GameData>,
        ids: Arc<EntityIds>,
        explosions_per_tick: usize,
        blocks: Arc<zerqon_blocks::Catalog>,
        damage: Arc<BlastDamage>,
    ) -> Result<Self> {
        if !(1..=MAX_ACTIVE).contains(&explosions_per_tick) {
            return Err("TNT explosions per tick must be 1..4096".into());
        }
        let tnt = data
            .default_block_state("minecraft:tnt")
            .ok_or("TNT state missing from registry")?;
        let type_id = data
            .entity_type("minecraft:tnt")
            .ok_or("TNT entity missing from registry")?;
        let protected = [
            "bedrock",
            "obsidian",
            "crying_obsidian",
            "barrier",
            "end_portal",
            "end_portal_frame",
            "end_gateway",
            "reinforced_deepslate",
            "command_block",
            "chain_command_block",
            "repeating_command_block",
            "structure_block",
            "jigsaw",
        ]
        .into_iter()
        .filter_map(|name| data.default_block_state(&format!("minecraft:{name}")))
        .collect();
        Ok(Self {
            world,
            players,
            data,
            ids,
            blocks,
            damage,
            type_id,
            explosions_per_tick,
            tnt,
            protected,
            live: Arc::new(Mutex::new(Live::default())),
            active: AtomicUsize::new(0),
            ready: AtomicUsize::new(0),
            detonated: AtomicU64::new(0),
            destroyed: AtomicU64::new(0),
            paused: AtomicUsize::new(0),
        })
    }

    pub fn status(&self) -> String {
        format!(
            "TNT: active={} ready={} detonated={} per_tick={} blocks_changed={} paused={} capacity={MAX_ACTIVE}",
            self.active.load(Ordering::Relaxed),
            self.ready.load(Ordering::Relaxed),
            self.detonated.load(Ordering::Relaxed),
            self.explosions_per_tick,
            self.destroyed.load(Ordering::Relaxed),
            self.paused.load(Ordering::Relaxed)
        )
    }

    pub async fn ignite(&self, positions: Vec<[i32; 3]>) -> Result<usize> {
        let mut live = self
            .live
            .try_lock()
            .map_err(|_| "TNT service is busy; retry")?;
        self.world.check_available()?;
        if positions.len() > 65_536 {
            return Err("TNT selection exceeds 65536 blocks".into());
        }
        let positions: Vec<_> = positions
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let states = self.world.read_blocks(positions.clone()).await?;
        let selected: Vec<_> = positions
            .into_iter()
            .zip(states)
            .filter_map(|(position, state)| (state == self.tnt).then_some(position))
            .collect();
        if selected.len() > MAX_ACTIVE.saturating_sub(live.active.len()) {
            return Err(format!(
                "TNT active limit is {MAX_ACTIVE}; selected {}, already active {}",
                selected.len(),
                live.active.len()
            )
            .into());
        }
        let prepared: Vec<_> = selected
            .iter()
            .copied()
            .map(|block| self.primed(block, FUSE))
            .collect::<Result<_>>()?;
        if prepared.is_empty() {
            return Ok(0);
        }
        self.world
            .edit_blocks_if(
                selected
                    .into_iter()
                    .map(|position| (position, self.tnt, self.data.flat.air))
                    .collect(),
            )
            .await?;
        let count = prepared.len();
        for entity in prepared {
            live.active.insert(entity.id, entity);
        }
        self.counters(&live);
        self.synchronize(&mut live, false)?;
        Ok(count)
    }

    pub(crate) async fn reserve_ignition(self: &Arc<Self>) -> Result<IgnitionReservation> {
        self.world.check_available()?;
        Ok(IgnitionReservation {
            service: Arc::clone(self),
            live: Arc::clone(&self.live).lock_owned().await,
            prepared: None,
        })
    }

    pub async fn cancel(&self) -> Result<usize> {
        let mut live = self.live.lock().await;
        let count = live.active.len();
        live.active.clear();
        self.paused.store(0, Ordering::Relaxed);
        self.counters(&live);
        self.synchronize(&mut live, false)?;
        Ok(count)
    }

    pub async fn tick(&self, tick: u64) -> Result<()> {
        let mut live = self.live.lock().await;
        if live.active.is_empty() && live.visible.is_empty() {
            return Ok(());
        }
        self.world.check_available()?;
        self.advance(&mut live).await?;
        let due: Vec<_> = live
            .active
            .values()
            .filter(|entity| entity.fuse == 0 && !entity.paused)
            .take(self.explosions_per_tick)
            .cloned()
            .collect();
        let mut effects = Vec::with_capacity(due.len());
        for entity in due {
            if let Some(changed) = self.detonate(&mut live, &entity).await? {
                let packet = Bytes::from(tnt_packets::explosion(
                    entity.body.explosion_center(),
                    changed,
                ));
                effects.push((entity, packet));
            }
        }
        self.counters(&live);
        let batches = self
            .players
            .snapshots()?
            .into_iter()
            .filter_map(|player| {
                let packets: Vec<_> = effects
                    .iter()
                    .filter(|(entity, _)| nearby(&player, entity))
                    .map(|(_, packet)| packet.clone())
                    .collect();
                (!packets.is_empty()).then_some((
                    player.uuid,
                    player.id,
                    PacketBatch::from(packets),
                ))
            })
            .collect();
        self.players.dispatch_batches(batches)?;
        self.synchronize(&mut live, tick.is_multiple_of(10))
    }

    fn primed(&self, block: [i32; 3], fuse: u16) -> Result<Primed> {
        let uuid = Uuid::new_v4();
        let random =
            ((uuid.as_u128() as u64 & 0x3fff_ffff_ffff_ffff) >> 9) as f64 / (1u64 << 53) as f64;
        let angle = random * f64::from(std::f32::consts::TAU);
        Ok(Primed {
            id: self.ids.next()?,
            uuid,
            body: Body {
                position: [
                    f64::from(block[0]) + 0.5,
                    f64::from(block[1]),
                    f64::from(block[2]) + 0.5,
                ],
                velocity: [-angle.sin() * 0.02, f64::from(0.2f32), -angle.cos() * 0.02],
                on_ground: false,
            },
            fuse,
            paused: false,
        })
    }

    async fn advance(&self, live: &mut Live) -> Result<()> {
        let keys: Vec<_> = live
            .active
            .values()
            .flat_map(|entity| {
                let swept = entity.body.swept();
                let min = [
                    (swept.min[0].floor() as i32 - 1).div_euclid(16),
                    (swept.min[2].floor() as i32 - 1).div_euclid(16),
                ];
                let max = [
                    (swept.max[0].ceil() as i32).div_euclid(16),
                    (swept.max[2].ceil() as i32).div_euclid(16),
                ];
                (min[0]..=max[0]).flat_map(move |x| (min[1]..=max[1]).map(move |z| (x, z)))
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let gameplay = self.world.gameplay_read().await;
        self.world.check_available()?;
        let mut chunks: HashMap<_, _> = self
            .world
            .cached_chunks()?
            .into_iter()
            .filter(|chunk| keys.binary_search(&(chunk.x, chunk.z)).is_ok())
            .map(|chunk| ((chunk.x, chunk.z), chunk))
            .collect();
        let mut loaded = Vec::new();
        if !keys.is_empty() {
            for offset in 0..keys.len().min(256) {
                let (x, z) = keys[(live.load_cursor + offset) % keys.len()];
                if !chunks.contains_key(&(x, z)) {
                    match self.world.chunk(x, z).await {
                        Ok(chunk) => loaded.push(chunk),
                        Err(error) if World::missing_imported_chunk(error.as_ref()) => {}
                        Err(error) => return Err(error),
                    }
                }
            }
            live.load_cursor = (live.load_cursor + 256) % keys.len();
        }
        chunks.extend(loaded.into_iter().map(|chunk| ((chunk.x, chunk.z), chunk)));
        let terrain = Terrain {
            chunks,
            catalog: Arc::clone(&self.blocks),
        };
        let bodies: Vec<_> = live
            .active
            .values()
            .map(|entity| (entity.id, entity.body))
            .collect();
        let motions = self
            .world
            .execute(move || {
                let _gameplay = gameplay;
                let mut scratch = Vec::new();
                bodies
                    .into_iter()
                    .map(
                        |(id, mut body)| match terrain.step(&mut body, &mut scratch) {
                            Ok(()) => Ok((id, Some(body))),
                            Err(
                                zerqon_entities::PhysicsError::Unavailable(_)
                                | zerqon_entities::PhysicsError::Unsupported(_)
                                | zerqon_entities::PhysicsError::Limit,
                            ) => Ok((id, None)),
                            Err(error) => Err(error.into()),
                        },
                    )
                    .collect::<Result<Vec<_>>>()
            })
            .await?;
        let mut paused = 0;
        for (id, motion) in motions {
            if let Some(entity) = live.active.get_mut(&id) {
                if let Some(body) = motion {
                    entity.body = body;
                    entity.fuse = entity.fuse.saturating_sub(1);
                    entity.paused = false;
                } else {
                    paused += 1;
                    entity.paused = true;
                }
            }
        }
        live.active
            .retain(|_, entity| entity.body.position[1] >= f64::from(zerqon_world::MIN_Y - 64));
        self.paused.store(paused, Ordering::Relaxed);
        Ok(())
    }

    async fn detonate(&self, live: &mut Live, entity: &Primed) -> Result<Option<usize>> {
        let center = entity.body.explosion_center();
        let block = center.map(|coordinate| coordinate.floor() as i32);
        let positions = blast_positions(block);
        let states = self.world.read_blocks(positions.clone()).await?;
        let snapshot: BTreeMap<_, _> = positions.into_iter().zip(states).collect();
        let mut edits = Vec::new();
        let mut chain = Vec::new();
        let free = MAX_ACTIVE
            .saturating_sub(live.active.len())
            .saturating_add(1);
        for (&position, &state) in &snapshot {
            edits.push((position, state, state));
            if self.data.is_air(state)
                || self.shields(state)
                || !ray_clear(block, position, &snapshot, |state| self.shields(state))
            {
                continue;
            }
            if state == self.tnt {
                if chain.len() == free {
                    continue;
                }
                let fuse = 10
                    + ((position[0] as u32).wrapping_mul(31)
                        ^ (position[1] as u32).wrapping_mul(17)
                        ^ position[2] as u32)
                        % 20;
                chain.push(self.primed(position, fuse as u16)?);
            }
            if let Some(edit) = edits.last_mut() {
                edit.2 = self.data.flat.air;
            }
        }
        let damage = self.damage.prepare(center).await?;
        let changed = if edits.is_empty() {
            0
        } else {
            match self.world.edit_blocks_if(edits).await {
                Ok(changed) => changed,
                Err(error) if error.downcast_ref::<BulkEditConflict>().is_some() => {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            }
        };
        live.active.remove(&entity.id);
        for primed in chain {
            live.active.insert(primed.id, primed);
        }
        self.detonated.fetch_add(1, Ordering::Relaxed);
        self.destroyed.fetch_add(changed as u64, Ordering::Relaxed);
        self.damage.apply(damage).await?;
        Ok(Some(changed))
    }

    fn shields(&self, state: u32) -> bool {
        self.data.has_fluid(state) || self.protected.contains(&state)
    }

    fn counters(&self, live: &Live) {
        self.active.store(live.active.len(), Ordering::Relaxed);
        self.ready.store(
            live.active
                .values()
                .filter(|entity| entity.fuse == 0)
                .count(),
            Ordering::Relaxed,
        );
    }

    fn synchronize(&self, live: &mut Live, refresh_fuses: bool) -> Result<()> {
        let players = self.players.snapshots()?;
        let observers: BTreeSet<_> = players
            .iter()
            .map(|player| (player.uuid, player.id))
            .collect();
        live.visible
            .retain(|observer, _| observers.contains(observer));
        let mut batches = Vec::new();
        for player in players {
            let current: BTreeSet<_> = live
                .active
                .values()
                .filter(|entity| nearby(&player, entity))
                .map(|entity| entity.id)
                .collect();
            let previous = live.visible.entry((player.uuid, player.id)).or_default();
            let mut packets: Vec<Bytes> = Vec::new();
            let removed: Vec<_> = previous.difference(&current).copied().collect();
            if !removed.is_empty() {
                packets.push(entity_packets::remove_entities(&removed).into());
            }
            for id in &current {
                let entity = &live.active[id];
                if !previous.contains(id) {
                    packets.push(
                        entity_packets::add_entity_with_motion(
                            entity.id,
                            entity.uuid,
                            self.type_id,
                            entity.position(),
                            entity.body.velocity,
                            0.0,
                            0.0,
                        )?
                        .into(),
                    );
                    packets.push(tnt_packets::metadata(entity.id, entity.fuse.max(20)).into());
                } else if refresh_fuses {
                    packets.push(
                        entity_packets::position_sync(
                            entity.id,
                            entity.position(),
                            0.0,
                            0.0,
                            entity.body.on_ground,
                        )
                        .into(),
                    );
                    packets.push(
                        entity_packets::entity_motion(entity.id, entity.body.velocity)?.into(),
                    );
                    packets.push(tnt_packets::metadata(entity.id, entity.fuse.max(20)).into());
                }
            }
            *previous = current;
            if !packets.is_empty() {
                batches.push((player.uuid, player.id, PacketBatch::from(packets)));
            }
        }
        self.players.dispatch_batches(batches)?;
        Ok(())
    }
}

fn nearby(player: &PlayerSnapshot, entity: &Primed) -> bool {
    (player.position.xyz[0] - entity.body.position[0]).abs() <= RANGE
        && (player.position.xyz[2] - entity.body.position[2]).abs() <= RANGE
}

fn blast_positions(center: [i32; 3]) -> Vec<[i32; 3]> {
    let mut positions = Vec::with_capacity(257);
    for dx in -4i32..=4 {
        for dy in -4i32..=4 {
            for dz in -4i32..=4 {
                if dx * dx + dy * dy + dz * dz > 16 {
                    continue;
                }
                let position = [center[0] + dx, center[1] + dy, center[2] + dz];
                if position[0].unsigned_abs() <= 30_000_000
                    && position[2].unsigned_abs() <= 30_000_000
                    && (-64..320).contains(&position[1])
                {
                    positions.push(position);
                }
            }
        }
    }
    positions
}

fn ray_clear(
    center: [i32; 3],
    target: [i32; 3],
    blocks: &BTreeMap<[i32; 3], u32>,
    blocked: impl Fn(u32) -> bool,
) -> bool {
    let delta = std::array::from_fn::<_, 3, _>(|axis| target[axis] - center[axis]);
    let steps = delta.iter().map(|value| value.abs()).max().unwrap_or(0) * 4;
    (0..=steps).all(|step| {
        let position = std::array::from_fn(|axis| {
            (f64::from(center[axis])
                + 0.5
                + f64::from(delta[axis]) * f64::from(step) / f64::from(steps.max(1)))
            .floor() as i32
        });
        blocks.get(&position).is_none_or(|state| !blocked(*state))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blast_is_bounded_and_water_or_protected_blocks_shield_the_ray() {
        let center = [0, 64, 0];
        let positions = blast_positions(center);
        assert_eq!(positions.len(), 257);
        assert!(positions.contains(&[-4, 64, 0]));
        assert!(!positions.contains(&[4, 65, 0]));
        assert!(
            blast_positions([30_000_000, -64, -30_000_000])
                .iter()
                .all(|position| position[0] <= 30_000_000
                    && position[2] >= -30_000_000
                    && position[1] >= -64)
        );
        let blocks = BTreeMap::from([([1, 64, 0], 7), ([2, 64, 0], 1)]);
        assert!(!ray_clear(center, [2, 64, 0], &blocks, |state| state == 7));
        assert!(ray_clear(center, [-2, 64, 0], &blocks, |state| state == 7));
    }
}
