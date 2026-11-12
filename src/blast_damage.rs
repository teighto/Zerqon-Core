use crate::{Result, mobs::Mobs, players::Players, runtime::World};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::RwLock;
use uuid::Uuid;
use zerqon_blocks::{Aabb, Catalog, Position, StateId};
use zerqon_world::{Chunk, MIN_Y, WORLD_HEIGHT};

const RADIUS: f64 = 8.0;

#[derive(Clone, Copy, Debug)]
pub(crate) struct BlastEffect {
    pub damage: f32,
    pub impulse: [f64; 3],
}

pub(crate) struct BlastView {
    chunks: BTreeMap<(i32, i32), Arc<Chunk>>,
    blocks: Arc<Catalog>,
}

pub struct PreparedBlast {
    center: [f64; 3],
    view: Option<Arc<BlastView>>,
    players: Vec<(Uuid, i32)>,
    mobs: Vec<(i32, Uuid)>,
}

pub struct BlastDamage {
    world: Arc<World>,
    players: Arc<Players>,
    mobs: Option<Arc<Mobs>>,
    blocks: Arc<Catalog>,
    gameplay: Arc<RwLock<()>>,
}

impl BlastDamage {
    pub fn new(
        world: Arc<World>,
        players: Arc<Players>,
        mobs: Option<Arc<Mobs>>,
        blocks: Arc<Catalog>,
        gameplay: Arc<RwLock<()>>,
    ) -> Self {
        Self {
            world,
            players,
            mobs,
            blocks,
            gameplay,
        }
    }

    pub async fn prepare(&self, center: [f64; 3]) -> Result<PreparedBlast> {
        if center
            .iter()
            .any(|value| !value.is_finite() || value.abs() > 30_000_016.0)
        {
            return Err("invalid explosion center".into());
        }
        let _gameplay = Arc::clone(&self.gameplay).read_owned().await;
        self.world.check_available()?;
        let players = self.players.blast_targets(center)?;
        let mobs = self
            .mobs
            .as_ref()
            .map(|mobs| mobs.blast_targets(center))
            .transpose()?
            .unwrap_or_default();
        let view = if players.is_empty() && mobs.is_empty() {
            None
        } else {
            let min_x = ((center[0] - RADIUS - 1.0).floor() as i32).div_euclid(16);
            let max_x = ((center[0] + RADIUS + 1.0).floor() as i32).div_euclid(16);
            let min_z = ((center[2] - RADIUS - 1.0).floor() as i32).div_euclid(16);
            let max_z = ((center[2] + RADIUS + 1.0).floor() as i32).div_euclid(16);
            let mut chunks = BTreeMap::new();
            for x in min_x..=max_x {
                for z in min_z..=max_z {
                    match self.world.chunk(x, z).await {
                        Ok(chunk) => {
                            chunks.insert((x, z), chunk);
                        }
                        Err(error) if World::missing_imported_chunk(error.as_ref()) => {}
                        Err(error) => return Err(error),
                    }
                }
            }
            Some(Arc::new(BlastView {
                chunks,
                blocks: Arc::clone(&self.blocks),
            }))
        };
        Ok(PreparedBlast {
            center,
            view,
            players,
            mobs,
        })
    }

    pub async fn apply(&self, blast: PreparedBlast) -> Result<()> {
        let Some(view) = blast.view else {
            return Ok(());
        };
        if let Some(mobs) = &self.mobs {
            mobs.apply_blast(blast.center, Arc::clone(&view), blast.mobs)
                .await?;
        }
        let players = Arc::clone(&self.players);
        let gameplay = Arc::clone(&self.gameplay).read_owned().await;
        self.world
            .execute(move || {
                let _gameplay = gameplay;
                players.apply_blast(blast.center, &view, &blast.players)?;
                Ok(())
            })
            .await
    }
}

pub(crate) fn within_blast(center: [f64; 3], feet: [f64; 3]) -> bool {
    center
        .iter()
        .zip(feet)
        .map(|(a, b)| (a - b).powi(2))
        .sum::<f64>()
        <= RADIUS * RADIUS
}

impl BlastView {
    pub(crate) fn effect(
        &self,
        center: [f64; 3],
        feet: [f64; 3],
        dimensions: [f64; 2],
        eye_height: f64,
    ) -> Option<BlastEffect> {
        if !within_blast(center, feet) {
            return None;
        }
        let half = dimensions[0] * 0.5;
        let bounds = Aabb {
            min: [feet[0] - half, feet[1], feet[2] - half],
            max: [feet[0] + half, feet[1] + dimensions[1], feet[2] + half],
        };
        let exposure = seen_percent(bounds, |sample| self.clear_ray(sample, center));
        Some(effect_from_exposure(center, feet, eye_height, exposure))
    }

    fn clear_ray(&self, start: [f64; 3], end: [f64; 3]) -> bool {
        let delta: [f64; 3] = std::array::from_fn(|axis| end[axis] - start[axis]);
        let mut cell: [i32; 3] = start.map(|value| value.floor() as i32);
        let last: [i32; 3] = end.map(|value| value.floor() as i32);
        let step: [i32; 3] = delta.map(|value| {
            if value > 0.0 {
                1
            } else if value < 0.0 {
                -1
            } else {
                0
            }
        });
        let advance: [f64; 3] = delta.map(|value| {
            if value == 0.0 {
                f64::INFINITY
            } else {
                1.0 / value.abs()
            }
        });
        let mut next: [f64; 3] = std::array::from_fn(|axis| {
            if step[axis] == 0 {
                return f64::INFINITY;
            }
            let edge = f64::from(cell[axis] + i32::from(step[axis] > 0));
            (edge - start[axis]) / delta[axis]
        });
        for _ in 0..64 {
            if self.blocks_ray(cell, start, end)
                || self.blocks_ray([cell[0], cell[1] - 1, cell[2]], start, end)
            {
                return false;
            }
            if cell == last {
                return true;
            }
            let axis = (0..3)
                .min_by(|&a, &b| next[a].total_cmp(&next[b]))
                .unwrap_or(0);
            if next[axis] > 1.0 {
                return true;
            }
            cell[axis] += step[axis];
            next[axis] += advance[axis];
        }
        false
    }

    fn blocks_ray(&self, [x, y, z]: [i32; 3], start: [f64; 3], end: [f64; 3]) -> bool {
        if !(MIN_Y..MIN_Y + WORLD_HEIGHT as i32).contains(&y) {
            return false;
        }
        let Some(chunk) = self.chunks.get(&(x.div_euclid(16), z.div_euclid(16))) else {
            return true;
        };
        let Some(state) = chunk.get_block(x.rem_euclid(16) as u8, y, z.rem_euclid(16) as u8) else {
            return true;
        };
        let Ok(state) = self.blocks.state(StateId(state)) else {
            return true;
        };
        let Ok(shapes) = state.collision() else {
            return true;
        };
        shapes
            .iter()
            .any(|shape| segment_intersects(start, end, shape.translated(Position::new(x, y, z))))
    }
}

fn effect_from_exposure(
    center: [f64; 3],
    feet: [f64; 3],
    eye_height: f64,
    exposure: f32,
) -> BlastEffect {
    let distance = center
        .iter()
        .zip(feet)
        .map(|(a, b)| (a - b).powi(2))
        .sum::<f64>()
        .sqrt()
        / RADIUS;
    let impact = (1.0 - distance).max(0.0) * f64::from(exposure);
    let damage = ((impact * impact + impact) / 2.0 * 7.0 * RADIUS + 1.0) as f32;
    let mut direction = std::array::from_fn::<_, 3, _>(|axis| feet[axis] - center[axis]);
    direction[1] += eye_height;
    let length = direction
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    let impulse = if length > 1e-12 {
        direction.map(|value| value / length * impact)
    } else {
        [0.0; 3]
    };
    BlastEffect { damage, impulse }
}

fn seen_percent(bounds: Aabb, mut clear: impl FnMut([f64; 3]) -> bool) -> f32 {
    let step: [f64; 3] =
        std::array::from_fn(|axis| 1.0 / ((bounds.max[axis] - bounds.min[axis]) * 2.0 + 1.0));
    let offset = [
        (1.0 - (1.0 / step[0]).floor() * step[0]) / 2.0,
        0.0,
        (1.0 - (1.0 / step[2]).floor() * step[2]) / 2.0,
    ];
    let (mut seen, mut total) = (0u32, 0u32);
    let mut x = 0.0;
    while x <= 1.0 {
        let mut y = 0.0;
        while y <= 1.0 {
            let mut z = 0.0;
            while z <= 1.0 {
                let fraction = [x, y, z];
                let sample = std::array::from_fn(|axis| {
                    bounds.min[axis]
                        + fraction[axis] * (bounds.max[axis] - bounds.min[axis])
                        + offset[axis]
                });
                seen += u32::from(clear(sample));
                total += 1;
                z += step[2];
            }
            y += step[1];
        }
        x += step[0];
    }
    seen as f32 / total as f32
}

fn segment_intersects(start: [f64; 3], end: [f64; 3], bounds: Aabb) -> bool {
    let (mut near, mut far) = (0.0f64, 1.0f64);
    for axis in 0..3 {
        let direction = end[axis] - start[axis];
        if direction.abs() < 1e-12 {
            if start[axis] < bounds.min[axis] - 1e-7 || start[axis] > bounds.max[axis] + 1e-7 {
                return false;
            }
        } else {
            let first = (bounds.min[axis] - start[axis]) / direction;
            let second = (bounds.max[axis] - start[axis]) / direction;
            near = near.max(first.min(second));
            far = far.min(first.max(second));
            if near > far {
                return false;
            }
        }
    }
    far > 1e-7 && near < 1.0 - 1e-7
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposure_damage_and_knockback_obey_radius_and_partial_cover() {
        let center = [0.0, 64.0, 0.0];
        let feet = [4.0, 64.0, 0.0];
        let open = effect_from_exposure(center, feet, 1.62, 1.0);
        let shielded = effect_from_exposure(center, feet, 1.62, 0.0);
        assert_eq!(open.damage, 22.0);
        assert_eq!(
            effect_from_exposure(center, [7.0, 64.0, 0.0], 1.62, 1.0).damage,
            4.9375
        );
        assert_eq!(shielded.damage, 1.0);
        assert_eq!(shielded.impulse, [0.0; 3]);
        assert!(open.impulse[0] > 0.0 && open.impulse[1] > 0.0);
        assert!(!within_blast(center, [8.001, 64.0, 0.0]));
        let bounds = Aabb {
            min: [0.0; 3],
            max: [1.0; 3],
        };
        assert_eq!(seen_percent(bounds, |_| true), 1.0);
        assert!(segment_intersects(
            [-1.0, 0.5, 0.5],
            [2.0, 0.5, 0.5],
            bounds
        ));
        assert!(segment_intersects(
            [-1.0, 1.0, 0.5],
            [2.0, 1.0, 0.5],
            bounds
        ));
        assert!(!segment_intersects(
            [-1.0, 1.0001, 0.5],
            [2.0, 1.0001, 0.5],
            bounds
        ));
    }

    #[test]
    fn collision_cover_and_player_death_respawn_preserve_creative_immunity() {
        use crate::{
            game_mode::GameMode,
            players::{EntityIds, Identity, Position},
        };
        let blocks = Arc::new(Catalog::embedded().unwrap());
        let air = blocks.air().0;
        let palette = zerqon_world::FlatPalette {
            air,
            bedrock: air,
            stone: air,
            dirt: air,
            grass: air,
        };
        let mut chunks = BTreeMap::new();
        for x in -1..=1 {
            for z in -1..=1 {
                chunks.insert((x, z), Arc::new(Chunk::flat(x, z, palette)));
            }
        }
        let mut view = BlastView {
            chunks,
            blocks: Arc::clone(&blocks),
        };
        let center = [0.0, 64.0, 0.0];
        let feet = [4.0, 64.0, 0.0];
        assert_eq!(
            view.effect(center, feet, [0.6, 1.8], 1.62).unwrap().damage,
            22.0
        );
        let stone = blocks
            .resolve("minecraft:stone", &BTreeMap::new())
            .unwrap()
            .0;
        for y in 63..=67 {
            for z in -2i32..=2 {
                Arc::make_mut(view.chunks.get_mut(&(0, z.div_euclid(16))).unwrap())
                    .set_block(2, y, z.rem_euclid(16) as u8, stone)
                    .unwrap();
            }
        }
        assert_eq!(
            view.effect(center, feet, [0.6, 1.8], 1.62).unwrap().damage,
            1.0
        );
        for y in 63..=67 {
            for z in -2i32..=2 {
                Arc::make_mut(view.chunks.get_mut(&(0, z.div_euclid(16))).unwrap())
                    .set_block(2, y, z.rem_euclid(16) as u8, air)
                    .unwrap();
            }
        }
        let players = Players::new(2, 64.0, 0, Arc::new(EntityIds::default()));
        let mut handles = Vec::new();
        let mut receivers = Vec::new();
        for (uuid, name, mode) in [
            (Uuid::from_u128(1), "Survivor", GameMode::Survival),
            (Uuid::from_u128(2), "Creative", GameMode::Creative),
        ] {
            let (sender, receiver) = tokio::sync::mpsc::channel(32);
            let (disconnect, _) = tokio::sync::watch::channel(false);
            let handle = players
                .reserve(
                    Identity {
                        uuid,
                        name: name.into(),
                        properties: Vec::new(),
                    },
                    sender,
                    disconnect,
                )
                .unwrap();
            handle.initialize_game_mode(mode).unwrap();
            handle
                .move_to(Position {
                    xyz: feet,
                    ..Position::default()
                })
                .unwrap();
            handle.activate().unwrap();
            handles.push(handle);
            receivers.push(receiver);
        }
        players.tracking_tick().unwrap();
        for receiver in &mut receivers {
            while receiver.try_recv().is_ok() {}
        }
        let targets = players.blast_targets(center).unwrap();
        assert_eq!(targets.len(), 1);
        players.apply_blast(center, &view, &targets).unwrap();
        assert_eq!(handles[0].health().unwrap(), 0.0);
        assert_eq!(handles[1].health().unwrap(), 20.0);
        assert!(
            receivers[0]
                .try_recv()
                .unwrap()
                .iter()
                .any(|packet| packet[0] == 69)
        );
        assert!(
            receivers[0].try_recv().is_err(),
            "self death must not leave stale health metadata in a later batch"
        );
        while receivers[1].try_recv().is_ok() {}
        assert!(handles[0].respawn(Position::default()).unwrap());
        assert_eq!(handles[0].health().unwrap(), 20.0);
        players.tracking_tick().unwrap();
        let mut observer_packets = Vec::new();
        while let Ok(batch) = receivers[1].try_recv() {
            observer_packets.extend(batch);
        }
        let removed = observer_packets
            .iter()
            .position(|packet| packet[0] == 78)
            .unwrap();
        let added = observer_packets
            .iter()
            .position(|packet| packet[0] == 1)
            .unwrap();
        assert!(removed < added);
        let healthy = crate::entity_packets::mob_health(handles[0].id(), 20.0);
        assert!(
            observer_packets
                .iter()
                .any(|packet| packet.as_ref() == healthy)
        );
        assert!(
            !observer_packets.iter().any(|packet| packet[0] == 70),
            "respawn must not remove TAB identity"
        );
        players.apply_blast(center, &view, &targets).unwrap();
        assert_eq!(
            handles[0].health().unwrap(),
            20.0,
            "respawn protection must reject immediate repeat blasts"
        );
        assert!(!handles[1].respawn(Position::default()).unwrap());
    }
}
