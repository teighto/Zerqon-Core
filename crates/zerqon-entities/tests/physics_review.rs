use std::collections::BTreeMap;
use uuid::Uuid;
use zerqon_blocks::{Catalog, StateId};
use zerqon_entities::{
    BlockCollision, EntityChange, EntityWorld, MobKind, RemovalReason, WorldQuery, move_mob,
};

struct Empty;
impl WorldQuery for Empty {
    fn block_solid(&self, _: i32, _: i32, _: i32) -> bool {
        false
    }
    fn surface_y(&self, _: i32, _: i32) -> Option<i32> {
        None
    }
}

#[test]
fn a_valid_mob_at_the_lower_position_limit_is_removed_by_the_void() {
    let mut world = EntityWorld::new(7, 4).unwrap();
    world
        .spawn(
            1,
            Uuid::from_u128(1),
            MobKind::Pig,
            [0.5, -2048.0, 0.5],
            &Empty,
        )
        .unwrap();
    let changes = world.tick(&Empty, &[]);
    assert!(changes.iter().any(|change| matches!(
        change,
        EntityChange::Removed {
            id: 1,
            reason: RemovalReason::Void,
            ..
        }
    )));
    assert!(world.is_empty());
}

struct Fence {
    catalog: Catalog,
    state: StateId,
    origin: [i32; 3],
}
impl WorldQuery for Fence {
    fn block_solid(&self, x: i32, y: i32, z: i32) -> bool {
        [x, y, z] == self.origin
    }
    fn block_collision(&self, x: i32, y: i32, z: i32) -> BlockCollision<'_> {
        if [x, y, z] == self.origin {
            BlockCollision::Static(self.catalog.state(self.state).unwrap().collision().unwrap())
        } else {
            BlockCollision::Empty
        }
    }
    fn surface_y(&self, _: i32, _: i32) -> Option<i32> {
        None
    }
}

#[test]
fn fence_overhang_is_found_above_its_own_cell_at_negative_coordinates() {
    let catalog = Catalog::embedded().unwrap();
    let state = catalog
        .resolve("minecraft:oak_fence", &BTreeMap::new())
        .unwrap();
    let terrain = Fence {
        catalog,
        state,
        origin: [-1, 0, -1],
    };
    for kind in [MobKind::Pig, MobKind::Cow, MobKind::Zombie] {
        let moved = move_mob([-0.5, 1.75, -0.5], kind, [0.0, -0.5, 0.0], false, &terrain).unwrap();
        assert_eq!(moved.position[1], 1.5);
        assert!(moved.on_ground);
        let blocked =
            move_mob([0.5, 1.25, -0.5], kind, [-0.75, 0.0, 0.0], false, &terrain).unwrap();
        assert!(blocked.collided[0]);
        assert!((blocked.position[0] - (-0.375 + kind.dimensions()[0] / 2.0)).abs() < 1e-12);
    }
}

struct PausedTerrain {
    blocked_x: Option<i32>,
    unloaded: bool,
}
impl WorldQuery for PausedTerrain {
    fn block_solid(&self, _: i32, y: i32, _: i32) -> bool {
        y < 0
    }
    fn block_collision(&self, x: i32, y: i32, z: i32) -> BlockCollision<'_> {
        if !self.unloaded && self.blocked_x == Some(x) && y == 0 && z == 0 {
            BlockCollision::Unsupported
        } else if y < 0 {
            BlockCollision::Full
        } else {
            BlockCollision::Empty
        }
    }
    fn loaded_at(&self, x: i32, _: i32) -> bool {
        !(self.unloaded && self.blocked_x == Some(x))
    }
    fn surface_y(&self, _: i32, _: i32) -> Option<i32> {
        Some(0)
    }
}

#[test]
fn blocked_mobs_do_not_rewind_other_mobs_randomness_or_change_resume_order() {
    let floor = PausedTerrain {
        blocked_x: None,
        unloaded: false,
    };
    for blocked_x in [0, 8] {
        let mut actual = EntityWorld::new(0x263777, 4).unwrap();
        for (id, x) in [(1, 0.5), (2, 8.5)] {
            actual
                .spawn(
                    id,
                    Uuid::from_u128(id as u128),
                    MobKind::Pig,
                    [x, 0.0, 0.5],
                    &floor,
                )
                .unwrap();
        }
        let mut expected = EntityWorld::restore(actual.snapshot(), 4).unwrap();
        let blocked = PausedTerrain {
            blocked_x: Some(blocked_x),
            unloaded: false,
        };
        let unloaded = PausedTerrain {
            blocked_x: Some(blocked_x),
            unloaded: true,
        };
        for _ in 0..10 {
            actual.tick(&blocked, &[]);
            expected.tick(&unloaded, &[]);
            assert_eq!(actual.snapshot(), expected.snapshot());
        }
        actual.tick(&floor, &[]);
        expected.tick(&floor, &[]);
        assert_eq!(actual.snapshot(), expected.snapshot());
    }
}
