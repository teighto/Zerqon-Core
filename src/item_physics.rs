use crate::{Result, drop_record::DropRecord};
use std::{collections::HashMap, sync::Arc};
use zerqon_blocks::{Aabb, Catalog, Position, StateId};
use zerqon_world::{Chunk, MIN_Y, WORLD_HEIGHT};

pub struct Terrain {
    pub chunks: HashMap<(i32, i32), Arc<Chunk>>,
    pub blocks: Arc<Catalog>,
}

impl Terrain {
    fn state(&self, x: i32, y: i32, z: i32) -> Option<zerqon_blocks::State<'_>> {
        if !(MIN_Y..MIN_Y + WORLD_HEIGHT as i32).contains(&y) {
            return self.blocks.state(self.blocks.air()).ok();
        }
        let state = self
            .chunks
            .get(&(x.div_euclid(16), z.div_euclid(16)))?
            .get_block(x.rem_euclid(16) as u8, y, z.rem_euclid(16) as u8)?;
        self.blocks.state(StateId(state)).ok()
    }
    pub fn step(
        &self,
        record: &mut DropRecord,
        fire_resistant: bool,
        scratch: &mut Vec<Aabb>,
    ) -> Result<bool> {
        let [x, y, z] = record.position.map(|v| v.floor() as i32);
        let Some(center) = self.state(x, y, z) else {
            return Ok(false);
        };
        let mut next = record.clone();
        let level = center
            .property("level")
            .and_then(|value| value.parse::<u8>().ok())
            .unwrap_or(0);
        let fluid_height = if level >= 8
            || self
                .state(x, y + 1, z)
                .is_some_and(|state| state.key() == center.key())
        {
            1.0
        } else {
            f64::from(8 - level) / 9.0
        };
        if matches!(center.key(), "minecraft:water" | "minecraft:lava")
            && f64::from(y) + fluid_height - next.position[1] > f64::from(0.1_f32)
        {
            let friction = if center.key() == "minecraft:water" {
                0.99_f32
            } else {
                0.95_f32
            };
            next.velocity[0] *= f64::from(friction);
            next.velocity[2] *= f64::from(friction);
            if next.velocity[1] < 0.06 {
                next.velocity[1] += f64::from(0.0005_f32);
            }
            if center.key() == "minecraft:lava" && !fire_resistant && !record.invulnerable {
                next.health = next.health.saturating_sub(4);
                next.fire_ticks = 300;
            }
        } else if !next.no_gravity {
            next.velocity[1] -= 0.04;
        }
        if matches!(center.key(), "minecraft:fire" | "minecraft:soul_fire") && !fire_resistant {
            next.fire_ticks = 300;
        }
        if fire_resistant {
            next.fire_ticks = 0;
        } else if next.fire_ticks > 0 {
            next.fire_ticks -= 1;
            if next.fire_ticks % 20 == 0 && !next.invulnerable {
                next.health = next.health.saturating_sub(1);
            }
        }
        let bounds = Aabb {
            min: [
                next.position[0] - 0.125,
                next.position[1],
                next.position[2] - 0.125,
            ],
            max: [
                next.position[0] + 0.125,
                next.position[1] + 0.25,
                next.position[2] + 0.125,
            ],
        };
        let velocity = next.velocity.map(|v| v.clamp(-4.0, 4.0));
        let lo: [i32; 3] = std::array::from_fn(|axis| {
            (bounds.min[axis] + velocity[axis].min(0.0)).floor() as i32 - 1
        });
        let hi: [i32; 3] = std::array::from_fn(|axis| {
            (bounds.max[axis] + velocity[axis].max(0.0)).ceil() as i32 + 1
        });
        scratch.clear();
        for x in lo[0]..hi[0] {
            for y in lo[1]..hi[1] {
                for z in lo[2]..hi[2] {
                    let Some(state) = self.state(x, y, z) else {
                        return Ok(false);
                    };
                    let Ok(shapes) = state.collision() else {
                        return Ok(false);
                    };
                    if scratch.len() + shapes.len() > 4096 {
                        return Ok(false);
                    }
                    scratch.extend(
                        shapes
                            .iter()
                            .map(|shape| shape.translated(Position::new(x, y, z))),
                    );
                }
            }
        }
        if scratch.iter().any(|shape| bounds.intersects(*shape)) {
            for offset in [
                [0.0, 0.1, 0.0],
                [0.2, 0.0, 0.0],
                [-0.2, 0.0, 0.0],
                [0.0, 0.0, 0.2],
                [0.0, 0.0, -0.2],
            ] {
                let moved = Aabb {
                    min: std::array::from_fn(|a| bounds.min[a] + offset[a]),
                    max: std::array::from_fn(|a| bounds.max[a] + offset[a]),
                };
                if !scratch.iter().any(|shape| moved.intersects(*shape)) {
                    for (p, d) in next.position.iter_mut().zip(offset) {
                        *p += d;
                    }
                    next.velocity = [0.0, 0.1, 0.0];
                    *record = next;
                    return Ok(true);
                }
            }
            next.position[1] += 0.1;
            if next.age != i16::MIN {
                next.age = next.age.saturating_add(1);
            }
            if next.pickup_delay > 0 && next.pickup_delay != i16::MAX {
                next.pickup_delay -= 1;
            }
            *record = next;
            return Ok(true);
        }
        let movement = zerqon_entities::clip_motion(bounds, velocity, scratch)?;
        next.on_ground = velocity[1] < 0.0 && movement[1] != velocity[1];
        for axis in 0..3 {
            next.position[axis] += movement[axis];
            if movement[axis] != velocity[axis] {
                next.velocity[axis] = 0.0;
            }
        }
        for axis in [0, 2] {
            let clamped = next.position[axis].clamp(-30_000_000.0, 30_000_000.0);
            if clamped != next.position[axis] {
                next.position[axis] = clamped;
                next.velocity[axis] = 0.0;
            }
        }
        let ground = if next.on_ground {
            match self
                .state(x, (next.position[1] - 0.01).floor() as i32, z)
                .map(|s| s.key())
            {
                Some("minecraft:ice" | "minecraft:packed_ice" | "minecraft:frosted_ice") => {
                    0.98_f32
                }
                Some("minecraft:blue_ice") => 0.989_f32,
                Some("minecraft:slime_block") => 0.8_f32,
                _ => 0.6_f32,
            }
        } else {
            1.0
        };
        next.velocity[0] *= f64::from(0.98_f32 * ground);
        next.velocity[2] *= f64::from(0.98_f32 * ground);
        next.velocity[1] *= f64::from(0.98_f32);
        if next.on_ground && next.velocity[1] < 0.0 {
            next.velocity[1] *= -0.5;
        }
        if next.age != i16::MIN {
            next.age = next.age.saturating_add(1);
        }
        if next.pickup_delay > 0 && next.pickup_delay != i16::MAX {
            next.pickup_delay -= 1;
        }
        *record = next;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_freefall_ground_friction_and_unloaded_pause() {
        let blocks = Arc::new(Catalog::embedded().unwrap());
        let id = |name| {
            blocks
                .resolve(name, &std::collections::BTreeMap::new())
                .unwrap()
                .0
        };
        let chunk = Chunk::flat(
            0,
            0,
            zerqon_world::FlatPalette {
                air: id("minecraft:air"),
                bedrock: id("minecraft:bedrock"),
                stone: id("minecraft:stone"),
                dirt: id("minecraft:dirt"),
                grass: id("minecraft:grass_block"),
            },
        );
        let mut terrain = Terrain {
            chunks: HashMap::from([((0, 0), Arc::new(chunk))]),
            blocks,
        };
        let stack = zerqon_inventory::ItemStack::new(
            zerqon_inventory::ItemId(1),
            7,
            zerqon_inventory::ValidatedPatch::default(),
        )
        .unwrap();
        let mut item = DropRecord::new(stack, [2.5, 70.0, 2.5], [0.1, 0.2, -0.05], [0.0; 2]);
        item.pickup_delay = 40;
        assert!(terrain.step(&mut item, false, &mut Vec::new()).unwrap());
        assert_eq!(item.position, [2.6, 70.16, 2.45]);
        assert_eq!(
            item.velocity,
            [
                0.09800000190734864,
                0.1568000030517578,
                -0.04900000095367432
            ]
        );
        assert_eq!((item.age, item.pickup_delay), (1, 39));
        item.position = [2.5, 64.0, 2.5];
        item.velocity = [0.1, -0.1, 0.0];
        terrain.step(&mut item, false, &mut Vec::new()).unwrap();
        assert_eq!(item.position[1], 64.0);
        assert!(item.on_ground);
        assert_eq!(item.velocity[0], 0.05880000591278076);
        terrain.chunks.clear();
        let before = item.clone();
        assert!(!terrain.step(&mut item, false, &mut Vec::new()).unwrap());
        assert_eq!(item, before);
    }
}
