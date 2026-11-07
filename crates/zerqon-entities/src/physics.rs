use crate::{BlockCollision, MobKind, WorldQuery};
use zerqon_blocks::{Aabb, Position};

const EPSILON: f64 = 1e-7;
const MAX_COLLIDERS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PhysicsError {
    #[error("invalid body or movement")]
    Invalid,
    #[error("collision query exceeds its resource limit")]
    Limit,
    #[error("terrain is unavailable at {0:?}")]
    Unavailable([i32; 3]),
    #[error("dynamic geometry or fluid physics is unsupported at {0:?}")]
    Unsupported([i32; 3]),
    #[error("body already intersects terrain")]
    Intersecting,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Movement {
    pub position: [f64; 3],
    pub displacement: [f64; 3],
    pub collided: [bool; 3],
    pub on_ground: bool,
}

#[derive(Default)]
pub(crate) struct Scratch {
    ordinary: ShapeBuffer,
    stepping: ShapeBuffer,
    heights: Vec<f32>,
}

#[derive(Default)]
struct ShapeBuffer {
    boxes: Vec<Aabb>,
    ends: Vec<usize>,
}
impl ShapeBuffer {
    fn groups(&self) -> impl Iterator<Item = &[Aabb]> + Clone {
        self.ends.iter().scan(0, move |start, &end| {
            let group = &self.boxes[*start..end];
            *start = end;
            Some(group)
        })
    }
}

pub fn body_bounds(position: [f64; 3], kind: MobKind) -> Aabb {
    let [width, height] = kind.dimensions();
    let half = width / 2.0;
    Aabb {
        min: [position[0] - half, position[1], position[2] - half],
        max: [position[0] + half, position[1] + height, position[2] + half],
    }
}

fn translated(bounds: Aabb, movement: [f64; 3]) -> Aabb {
    Aabb {
        min: std::array::from_fn(|axis| bounds.min[axis] + movement[axis]),
        max: std::array::from_fn(|axis| bounds.max[axis] + movement[axis]),
    }
}

fn swept(bounds: Aabb, movement: [f64; 3]) -> Aabb {
    Aabb {
        min: std::array::from_fn(|axis| bounds.min[axis] + movement[axis].min(0.0)),
        max: std::array::from_fn(|axis| bounds.max[axis] + movement[axis].max(0.0)),
    }
}

fn intersects(first: Aabb, second: Aabb) -> bool {
    (0..3).all(|axis| {
        first.max[axis] - EPSILON > second.min[axis] && first.min[axis] + EPSILON < second.max[axis]
    })
}

fn collect(
    bounds: Aabb,
    world: &impl WorldQuery,
    output: &mut ShapeBuffer,
) -> Result<(), PhysicsError> {
    output.boxes.clear();
    output.ends.clear();
    if !bounds.valid()
        || bounds
            .min
            .iter()
            .chain(&bounds.max)
            .any(|value| value.abs() > 30_000_016.0)
    {
        return Err(PhysicsError::Invalid);
    }
    let start: [i32; 3] = std::array::from_fn(|axis| bounds.min[axis].floor() as i32 - 1);
    let end: [i32; 3] = std::array::from_fn(|axis| bounds.max[axis].ceil() as i32 + 1);
    let volume: i64 = (0..3)
        .map(|axis| i64::from(end[axis] - start[axis]))
        .product();
    if volume > 4096 {
        return Err(PhysicsError::Limit);
    }
    for x in start[0]..end[0] {
        for y in start[1]..end[1] {
            for z in start[2]..end[2] {
                let position = Position::new(x, y, z);
                let full = Aabb {
                    min: [f64::from(x), f64::from(y), f64::from(z)],
                    max: [f64::from(x) + 1.0, f64::from(y) + 1.0, f64::from(z) + 1.0],
                };
                match world.block_collision(x, y, z) {
                    BlockCollision::Empty => {}
                    BlockCollision::Full => {
                        if intersects(bounds, full) {
                            if output.boxes.len() == MAX_COLLIDERS {
                                return Err(PhysicsError::Limit);
                            }
                            output.boxes.push(full);
                            output.ends.push(output.boxes.len());
                        }
                    }
                    BlockCollision::Static(boxes) => {
                        if boxes.len() > 64 {
                            return Err(PhysicsError::Limit);
                        }
                        let start = output.boxes.len();
                        let mut touches = false;
                        for shape in boxes {
                            if shape
                                .min
                                .iter()
                                .chain(&shape.max)
                                .any(|value| !value.is_finite() || !(-0.25..=1.5).contains(value))
                                || !shape.valid()
                            {
                                return Err(PhysicsError::Invalid);
                            }
                            let shape = shape.translated(position);
                            touches |= intersects(bounds, shape);
                            if output.boxes.len() == MAX_COLLIDERS {
                                return Err(PhysicsError::Limit);
                            }
                            output.boxes.push(shape);
                        }
                        if touches {
                            output.ends.push(output.boxes.len());
                        } else {
                            output.boxes.truncate(start);
                        }
                    }
                    BlockCollision::Unavailable => {
                        return Err(PhysicsError::Unavailable([x, y, z]));
                    }
                    BlockCollision::Unsupported => {
                        return Err(PhysicsError::Unsupported([x, y, z]));
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn try_overlaps(
    position: [f64; 3],
    kind: MobKind,
    world: &impl WorldQuery,
) -> Result<bool, PhysicsError> {
    let bounds = body_bounds(position, kind);
    let mut boxes = ShapeBuffer::default();
    collect(bounds, world, &mut boxes)?;
    Ok(boxes
        .boxes
        .into_iter()
        .any(|shape| intersects(bounds, shape)))
}

#[cfg(test)]
fn overlaps(position: [f64; 3], kind: MobKind, world: &impl WorldQuery) -> bool {
    try_overlaps(position, kind, world).unwrap()
}

pub(crate) fn bodies_overlap(
    first: [f64; 3],
    first_kind: MobKind,
    second: [f64; 3],
    second_kind: MobKind,
) -> bool {
    body_bounds(first, first_kind).intersects(body_bounds(second, second_kind))
}

/// Clip a displacement against static world-space boxes in the 26.3 axis order.
pub fn clip_motion(
    bounds: Aabb,
    requested: [f64; 3],
    colliders: &[Aabb],
) -> Result<[f64; 3], PhysicsError> {
    if !bounds.valid()
        || requested
            .iter()
            .any(|value| !value.is_finite() || value.abs() > 32.0)
        || colliders.len() > MAX_COLLIDERS
        || colliders.iter().any(|shape| !shape.valid())
    {
        return Err(PhysicsError::Invalid);
    }
    Ok(clip(
        bounds,
        requested,
        colliders.iter().map(std::slice::from_ref),
    ))
}

pub fn clip_shapes(
    bounds: Aabb,
    requested: [f64; 3],
    shapes: &[&[Aabb]],
) -> Result<[f64; 3], PhysicsError> {
    if !bounds.valid()
        || requested
            .iter()
            .any(|value| !value.is_finite() || value.abs() > 32.0)
        || shapes.len() > MAX_COLLIDERS
        || shapes.iter().map(|group| group.len()).sum::<usize>() > MAX_COLLIDERS
        || shapes
            .iter()
            .flat_map(|group| group.iter())
            .any(|shape| !shape.valid())
    {
        return Err(PhysicsError::Invalid);
    }
    Ok(clip(
        bounds,
        requested,
        shapes.iter().copied().filter(|group| !group.is_empty()),
    ))
}

fn clip<'a>(
    bounds: Aabb,
    requested: [f64; 3],
    colliders: impl Iterator<Item = &'a [Aabb]> + Clone,
) -> [f64; 3] {
    if colliders.clone().next().is_none() {
        return requested;
    }
    let order = if requested[0].abs() < requested[2].abs() {
        [1, 2, 0]
    } else {
        [1, 0, 2]
    };
    let mut moved = [0.0; 3];
    for axis in order {
        let mut amount = requested[axis];
        if amount.abs() < EPSILON {
            continue;
        }
        let current = translated(bounds, moved);
        for group in colliders.clone() {
            if amount.abs() < EPSILON {
                amount = 0.0;
                break;
            }
            let positive = amount > 0.0;
            for obstacle in group {
                if (0..3).any(|other| {
                    other != axis
                        && (current.max[other] - EPSILON <= obstacle.min[other]
                            || current.min[other] + EPSILON >= obstacle.max[other])
                }) {
                    continue;
                }
                if positive {
                    let gap = obstacle.min[axis] - current.max[axis];
                    if gap >= -EPSILON {
                        amount = amount.min(gap);
                    }
                } else {
                    let gap = obstacle.max[axis] - current.min[axis];
                    if gap <= EPSILON {
                        amount = amount.max(gap);
                    }
                }
            }
        }
        moved[axis] = amount;
    }
    moved
}

pub fn move_mob(
    position: [f64; 3],
    kind: MobKind,
    requested: [f64; 3],
    was_on_ground: bool,
    world: &impl WorldQuery,
) -> Result<Movement, PhysicsError> {
    move_with_scratch(
        position,
        kind,
        requested,
        was_on_ground,
        world,
        &mut Scratch::default(),
    )
}

pub(crate) fn move_with_scratch(
    position: [f64; 3],
    kind: MobKind,
    requested: [f64; 3],
    was_on_ground: bool,
    world: &impl WorldQuery,
    scratch: &mut Scratch,
) -> Result<Movement, PhysicsError> {
    if !crate::valid_position(position)
        || requested
            .iter()
            .any(|value| !value.is_finite() || value.abs() > 32.0)
    {
        return Err(PhysicsError::Invalid);
    }
    let bounds = body_bounds(position, kind);
    collect(swept(bounds, requested), world, &mut scratch.ordinary)?;
    if scratch
        .ordinary
        .boxes
        .iter()
        .any(|shape| intersects(bounds, *shape))
    {
        return Err(PhysicsError::Intersecting);
    }
    let mut allowed = clip(bounds, requested, scratch.ordinary.groups());
    let falling = allowed[1] != requested[1] && requested[1] < 0.0;
    if (falling || was_on_ground) && (allowed[0] != requested[0] || allowed[2] != requested[2]) {
        let step_bounds = if falling {
            translated(bounds, [0.0, allowed[1], 0.0])
        } else {
            bounds
        };
        let max_step = kind.step_height();
        let mut area = swept(
            step_bounds,
            [requested[0], f64::from(max_step), requested[2]],
        );
        if !falling {
            area.min[1] -= f64::from(1e-5_f32);
        }
        collect(area, world, &mut scratch.stepping)?;
        scratch.heights.clear();
        for shape in &scratch.stepping.boxes {
            for y in [shape.min[1], shape.max[1]] {
                let height = (y - step_bounds.min[1]) as f32;
                if height >= 0.0
                    && height <= max_step
                    && height != allowed[1] as f32
                    && !scratch.heights.contains(&height)
                {
                    scratch.heights.push(height);
                }
            }
        }
        scratch.heights.sort_by(f32::total_cmp);
        for height in &scratch.heights {
            let candidate = clip(
                step_bounds,
                [requested[0], f64::from(*height), requested[2]],
                scratch.stepping.groups(),
            );
            if candidate[0] * candidate[0] + candidate[2] * candidate[2]
                > allowed[0] * allowed[0] + allowed[2] * allowed[2]
            {
                allowed = [
                    candidate[0],
                    candidate[1] - (bounds.min[1] - step_bounds.min[1]),
                    candidate[2],
                ];
                break;
            }
        }
    }
    let position = std::array::from_fn(|axis| position[axis] + allowed[axis]);
    if !crate::valid_position(position) {
        return Err(PhysicsError::Invalid);
    }
    let collided = std::array::from_fn(|axis| allowed[axis] != requested[axis]);
    Ok(Movement {
        position,
        displacement: allowed,
        collided,
        on_ground: requested[1] < 0.0 && collided[1],
    })
}

#[cfg(test)]
fn move_body(
    position: [f64; 3],
    kind: MobKind,
    requested: [f64; 3],
    world: &impl WorldQuery,
) -> ([f64; 3], [bool; 3]) {
    let bounds = body_bounds(position, kind);
    let mut boxes = ShapeBuffer::default();
    collect(swept(bounds, requested), world, &mut boxes).unwrap();
    let allowed = clip(bounds, requested, boxes.groups());
    (
        std::array::from_fn(|axis| position[axis] + allowed[axis]),
        std::array::from_fn(|axis| allowed[axis] != requested[axis]),
    )
}

pub(crate) fn line_clear(first: [f64; 3], second: [f64; 3], world: &impl WorldQuery) -> bool {
    let area = Aabb {
        min: std::array::from_fn(|axis| first[axis].min(second[axis]) - 2.0 * EPSILON),
        max: std::array::from_fn(|axis| first[axis].max(second[axis]) + 2.0 * EPSILON),
    };
    let mut boxes = ShapeBuffer::default();
    if collect(area, world, &mut boxes).is_err() {
        return false;
    }
    for bounds in boxes.boxes {
        let mut enter = 0.0_f64;
        let mut leave = 1.0_f64;
        for axis in 0..3 {
            let movement = second[axis] - first[axis];
            if movement == 0.0 {
                if first[axis] < bounds.min[axis] || first[axis] > bounds.max[axis] {
                    leave = -1.0;
                    break;
                }
            } else {
                let a = (bounds.min[axis] - first[axis]) / movement;
                let b = (bounds.max[axis] - first[axis]) / movement;
                enter = enter.max(a.min(b));
                leave = leave.min(a.max(b));
            }
        }
        if enter < leave {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    struct Slab;
    impl WorldQuery for Slab {
        fn block_solid(&self, x: i32, y: i32, z: i32) -> bool {
            (x, y, z) == (0, 0, 0)
        }
        fn block_collision(&self, x: i32, y: i32, z: i32) -> crate::BlockCollision<'_> {
            static BOXES: [zerqon_blocks::Aabb; 1] = [zerqon_blocks::Aabb {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 0.5, 1.0],
            }];
            if (x, y, z) == (0, 0, 0) {
                crate::BlockCollision::Static(&BOXES)
            } else {
                crate::BlockCollision::Empty
            }
        }
        fn surface_y(&self, _x: i32, _z: i32) -> Option<i32> {
            None
        }
    }

    #[test]
    fn bottom_slab_landing_stops_at_half_height_and_free_space_above_is_valid() {
        let (position, hit) = move_body([0.5, 2.0, 0.5], MobKind::Pig, [0.0, -4.0, 0.0], &Slab);
        assert_eq!(position[1], 0.5);
        assert!(hit[1]);
        assert!(!overlaps([0.5, 0.5, 0.5], MobKind::Pig, &Slab));
    }
    struct Blocks(Vec<[i32; 3]>);
    impl WorldQuery for Blocks {
        fn block_solid(&self, x: i32, y: i32, z: i32) -> bool {
            self.0.contains(&[x, y, z])
        }
        fn surface_y(&self, _x: i32, _z: i32) -> Option<i32> {
            None
        }
    }
    #[test]
    fn swept_wall_stops_fast_movement_and_keeps_free_axis() {
        let blocks = Blocks(
            (0..3)
                .flat_map(|y| (0..6).map(move |z| [2, y, z]))
                .collect(),
        );
        let (position, hit) = move_body([0.5, 0.0, 0.5], MobKind::Pig, [5.0, 0.0, 2.0], &blocks);
        assert_eq!(position[0], 2.0 - f64::from(0.9_f32) / 2.0);
        assert_eq!(position[2], 2.5);
        assert_eq!(hit, [true, false, false]);
        assert!(!overlaps(position, MobKind::Pig, &blocks));
    }
    #[test]
    fn floor_and_ceiling_clip_vertical_motion_without_drift() {
        let blocks = Blocks(vec![[0, -1, 0], [0, 3, 0]]);
        let (landed, hit) = move_body([0.5, 2.0, 0.5], MobKind::Pig, [0.0, -5.0, 0.0], &blocks);
        assert_eq!(landed[1], 0.0);
        assert!(hit[1]);
        let (ceiling, hit) = move_body([0.5, 0.0, 0.5], MobKind::Pig, [0.0, 5.0, 0.0], &blocks);
        assert_eq!(ceiling[1], 3.0 - f64::from(0.9_f32));
        assert!(hit[1]);
        assert!(!overlaps(ceiling, MobKind::Pig, &blocks));
    }

    #[test]
    fn negative_wall_and_internal_corner_stop_at_the_first_contact() {
        let wall = Blocks(vec![[-2, 0, 0], [-2, 1, 0]]);
        let (position, hit) = move_body([0.5, 0.0, 0.5], MobKind::Cow, [-5.0, 0.0, 0.0], &wall);
        assert_eq!(position[0], -1.0 + f64::from(0.9_f32) / 2.0);
        assert!(hit[0]);
        let corner = Blocks(
            (0..3)
                .flat_map(|y| (0..4).flat_map(move |at| [[2, y, at], [at, y, 2]]))
                .collect(),
        );
        let (position, hit) = move_body([0.5, 0.0, 0.5], MobKind::Cow, [5.0, 0.0, 5.0], &corner);
        assert_eq!(hit, [true, false, true]);
        assert!(!overlaps(position, MobKind::Cow, &corner));
    }

    #[test]
    fn leaving_a_ledge_allows_falling_on_the_next_tick() {
        let floor = Blocks(vec![[0, -1, 0]]);
        let (position, _) = move_body([0.5, 0.0, 0.5], MobKind::Pig, [2.0, -0.08, 0.0], &floor);
        assert_eq!(position, [2.5, 0.0, 0.5]);
        let (position, hit) = move_body(position, MobKind::Pig, [0.0, -0.08, 0.0], &floor);
        assert_eq!(position[1], -0.08);
        assert!(!hit[1]);
    }
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn empty_space_preserves_displacement(x in -50_i32..50,z in -50_i32..50,dx in -5.0_f64..5.0,dy in -5.0_f64..5.0,dz in -5.0_f64..5.0){
            let position=[x as f64,10.0,z as f64];
            let (moved,hit)=move_body(position,MobKind::Zombie,[dx,dy,dz],&Blocks(Vec::new()));
            prop_assert_eq!(hit,[false;3]);
            prop_assert!((moved[0]-position[0]-dx).abs()<1e-12);
            prop_assert!((moved[1]-position[1]-dy).abs()<1e-12);
            prop_assert!((moved[2]-position[2]-dz).abs()<1e-12);
        }
        #[test]
        fn translating_voxels_and_body_preserves_collision(offset_x in -100_i32..100,offset_z in -100_i32..100,dx in 0.0_f64..6.0,dz in -0.1_f64..0.1){
            let blocks=Blocks(vec![[2,0,0],[2,1,0]]);
            let shifted=Blocks(vec![[2+offset_x,0,offset_z],[2+offset_x,1,offset_z]]);
            let (reference,hit)=move_body([0.5,0.0,0.5],MobKind::Cow,[dx,0.0,dz],&blocks);
            let (moved,shift_hit)=move_body([0.5+offset_x as f64,0.0,0.5+offset_z as f64],MobKind::Cow,[dx,0.0,dz],&shifted);
            prop_assert_eq!(hit,shift_hit);
            prop_assert!((moved[0]-offset_x as f64-reference[0]).abs()<1e-12);
            prop_assert!((moved[2]-offset_z as f64-reference[2]).abs()<1e-12);
            prop_assert!(!overlaps(moved,MobKind::Cow,&shifted));
        }
    }
}
