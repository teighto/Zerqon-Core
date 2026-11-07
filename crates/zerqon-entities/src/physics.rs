use crate::{MobKind, WorldQuery};

fn bounds(position: [f64; 3], kind: MobKind) -> ([f64; 3], [f64; 3]) {
    let [width, height] = kind.dimensions();
    let half = width / 2.0;
    (
        [position[0] - half, position[1], position[2] - half],
        [position[0] + half, position[1] + height, position[2] + half],
    )
}

pub(crate) fn overlaps(position: [f64; 3], kind: MobKind, world: &impl WorldQuery) -> bool {
    let (min, max) = bounds(position, kind);
    for x in min[0].floor() as i32..max[0].ceil() as i32 {
        for y in min[1].floor() as i32..max[1].ceil() as i32 {
            for z in min[2].floor() as i32..max[2].ceil() as i32 {
                if world.block_solid(x, y, z) {
                    return true;
                }
            }
        }
    }
    false
}

pub(crate) fn bodies_overlap(
    first: [f64; 3],
    first_kind: MobKind,
    second: [f64; 3],
    second_kind: MobKind,
) -> bool {
    let (first_min, first_max) = bounds(first, first_kind);
    let (second_min, second_max) = bounds(second, second_kind);
    (0..3).all(|axis| first_min[axis] < second_max[axis] && first_max[axis] > second_min[axis])
}

pub(crate) fn move_body(
    mut position: [f64; 3],
    kind: MobKind,
    movement: [f64; 3],
    world: &impl WorldQuery,
) -> ([f64; 3], [bool; 3]) {
    let mut hit = [false; 3];
    for axis in [1, 0, 2] {
        let requested = movement[axis];
        if requested == 0.0 {
            continue;
        }
        let (min, max) = bounds(position, kind);
        let mut swept_min = min;
        let mut swept_max = max;
        swept_min[axis] += requested.min(0.0);
        swept_max[axis] += requested.max(0.0);
        let start: [i32; 3] =
            std::array::from_fn(|index| swept_min[index].floor() as i32 - i32::from(index == axis));
        let end: [i32; 3] = std::array::from_fn(|index| {
            if index == axis {
                swept_max[index].floor() as i32
            } else {
                swept_max[index].ceil() as i32 - 1
            }
        });
        let mut allowed = requested;
        for x in start[0]..=end[0] {
            for y in start[1]..=end[1] {
                for z in start[2]..=end[2] {
                    if !world.block_solid(x, y, z) {
                        continue;
                    }
                    let block = [f64::from(x), f64::from(y), f64::from(z)];
                    if (0..3).any(|index| {
                        index != axis
                            && (max[index] <= block[index] || min[index] >= block[index] + 1.0)
                    }) {
                        continue;
                    }
                    if requested > 0.0 && max[axis] <= block[axis] {
                        let gap = block[axis] - max[axis];
                        if gap <= allowed {
                            allowed = gap;
                            hit[axis] = true;
                        }
                    } else if requested < 0.0 && min[axis] >= block[axis] + 1.0 {
                        let gap = block[axis] + 1.0 - min[axis];
                        if gap >= allowed {
                            allowed = gap;
                            hit[axis] = true;
                        }
                    }
                }
            }
        }
        position[axis] += allowed;
    }
    (position, hit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
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
