use std::{collections::HashMap, sync::Arc};
use zerqon_blocks::{Aabb, Catalog, Position, StateId};
use zerqon_entities::{PhysicsError, clip_motion};
use zerqon_world::{Chunk, MIN_Y, WORLD_HEIGHT};

pub const SIZE: f64 = 0.98_f32 as f64;
const GRAVITY: f64 = 0.04;
const DRAG: f64 = 0.98_f32 as f64;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Body {
    pub position: [f64; 3],
    pub velocity: [f64; 3],
    pub on_ground: bool,
}

impl Body {
    pub fn bounds(self) -> Aabb {
        Aabb {
            min: [
                self.position[0] - SIZE / 2.0,
                self.position[1],
                self.position[2] - SIZE / 2.0,
            ],
            max: [
                self.position[0] + SIZE / 2.0,
                self.position[1] + SIZE,
                self.position[2] + SIZE / 2.0,
            ],
        }
    }

    pub fn requested(self) -> [f64; 3] {
        [
            self.velocity[0],
            self.velocity[1] - GRAVITY,
            self.velocity[2],
        ]
    }

    pub fn swept(self) -> Aabb {
        let bounds = self.bounds();
        let velocity = self.requested();
        Aabb {
            min: std::array::from_fn(|axis| bounds.min[axis] + velocity[axis].min(0.0)),
            max: std::array::from_fn(|axis| bounds.max[axis] + velocity[axis].max(0.0)),
        }
    }

    pub fn explosion_center(self) -> [f64; 3] {
        [
            self.position[0],
            self.position[1] + SIZE * 0.0625,
            self.position[2],
        ]
    }
}

pub struct Terrain {
    pub chunks: HashMap<(i32, i32), Arc<Chunk>>,
    pub catalog: Arc<Catalog>,
}

impl Terrain {
    pub fn step(&self, body: &mut Body, scratch: &mut Vec<Aabb>) -> Result<(), PhysicsError> {
        scratch.clear();
        let swept = body.swept();
        let minimum = swept.min.map(|value| value.floor() as i32 - 1);
        let maximum = swept.max.map(|value| value.ceil() as i32 + 1);
        if !swept.valid()
            || swept
                .min
                .iter()
                .chain(&swept.max)
                .any(|value| value.abs() > 30_000_016.0)
            || (0..3)
                .map(|axis| i64::from(maximum[axis] - minimum[axis]))
                .product::<i64>()
                > 4096
        {
            return Err(PhysicsError::Limit);
        }
        for x in minimum[0]..maximum[0] {
            for z in minimum[2]..maximum[2] {
                for y in minimum[1]..maximum[1] {
                    if !(MIN_Y..MIN_Y + WORLD_HEIGHT as i32).contains(&y) {
                        continue;
                    }
                    let chunk = self
                        .chunks
                        .get(&(x.div_euclid(16), z.div_euclid(16)))
                        .ok_or(PhysicsError::Unavailable([x, y, z]))?;
                    let state = chunk
                        .get_block(x.rem_euclid(16) as u8, y, z.rem_euclid(16) as u8)
                        .ok_or(PhysicsError::Unavailable([x, y, z]))?;
                    let state = self
                        .catalog
                        .state(StateId(state))
                        .map_err(|_| PhysicsError::Unsupported([x, y, z]))?;
                    for shape in state
                        .collision()
                        .map_err(|_| PhysicsError::Unsupported([x, y, z]))?
                    {
                        if scratch.len() == 4096 {
                            return Err(PhysicsError::Limit);
                        }
                        scratch.push(shape.translated(Position::new(x, y, z)));
                    }
                }
            }
        }
        step(body, scratch)
    }
}

pub fn step(body: &mut Body, colliders: &[Aabb]) -> Result<(), PhysicsError> {
    let requested = body.requested();
    let displacement = clip_motion(body.bounds(), requested, colliders)?;
    let mut velocity = requested;
    for axis in 0..3 {
        if displacement[axis] != requested[axis] {
            velocity[axis] = 0.0;
        }
    }
    let on_ground = requested[1] < 0.0 && displacement[1] != requested[1];
    for value in &mut velocity {
        *value *= DRAG;
    }
    if on_ground {
        velocity[0] *= 0.7;
        velocity[1] *= -0.5;
        velocity[2] *= 0.7;
    }
    body.position = std::array::from_fn(|axis| body.position[axis] + displacement[axis]);
    body.velocity = velocity;
    body.on_ground = on_ground;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tnt_falls_with_original_drag_and_stops_on_floor_and_wall() {
        let floor = Aabb {
            min: [-16.0, 63.0, -16.0],
            max: [16.0, 64.0, 16.0],
        };
        let mut original = Body {
            position: [0.5, 65.0, 0.5],
            velocity: [0.02, 0.2_f32 as f64, 0.0],
            on_ground: false,
        };
        for _ in 0..12 {
            step(&mut original, &[floor]).unwrap();
        }
        assert_eq!(
            original.position,
            [0.7152832983024712, 64.25059389157599, 0.5]
        );
        assert_eq!(
            original.velocity,
            [0.015694338140153623, -0.26501188914507445, 0.0]
        );
        step(&mut original, &[floor]).unwrap();
        assert_eq!(original.position, [0.7309776364426248, 64.0, 0.5]);
        assert_eq!(original.velocity, [0.010766316173687406, -0.0, 0.0]);
        assert!(original.on_ground);
        let mut body = Body {
            position: [0.5, 72.0, 0.5],
            velocity: [0.0, 0.2_f32 as f64, -0.02],
            on_ground: false,
        };
        step(&mut body, &[floor]).unwrap();
        assert_eq!(body.position[1], 72.16000000298023);
        assert_eq!(body.velocity[1], (0.2_f32 as f64 - 0.04) * DRAG);
        for _ in 0..79 {
            step(&mut body, &[floor]).unwrap();
        }
        assert_eq!(body.position[1], 64.0);
        assert!(body.on_ground);
        assert_eq!(body.velocity[1], -0.0);
        let wall = Aabb {
            min: [1.0, 63.0, -16.0],
            max: [2.0, 80.0, 16.0],
        };
        body.position = [0.5, 64.0, 0.5];
        body.velocity = [0.4, 0.0, 0.0];
        step(&mut body, &[floor, wall]).unwrap();
        assert!((body.position[0] - (1.0 - SIZE / 2.0)).abs() < 1e-12);
        assert_eq!(body.velocity[0], 0.0);
    }

    #[test]
    fn unavailable_terrain_does_not_advance_body_or_velocity() {
        let terrain = Terrain {
            chunks: HashMap::new(),
            catalog: Arc::new(Catalog::embedded().unwrap()),
        };
        let mut body = Body {
            position: [-0.5, 72.0, 0.5],
            velocity: [0.0, -0.3, 0.0],
            on_ground: false,
        };
        let before = body;
        assert!(matches!(
            terrain.step(&mut body, &mut Vec::new()),
            Err(PhysicsError::Unavailable(_))
        ));
        assert_eq!(body, before);
    }
}
