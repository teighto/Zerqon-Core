use uuid::Uuid;
use zerqon_entities::{EntityChange, EntityWorld, MobKind, PlayerTarget, WorldQuery};
struct Terrain;
impl WorldQuery for Terrain {
    fn block_solid(&self, x: i32, y: i32, z: i32) -> bool {
        y < 64 || (x == 0 && (64..66).contains(&y) && (-2..=2).contains(&z))
    }
    fn surface_y(&self, _: i32, _: i32) -> Option<i32> {
        Some(64)
    }
}
#[test]
fn zombie_routes_around_wall_and_animals_follow_food_without_attacking() {
    let mut world = EntityWorld::new(42, 16).unwrap();
    world
        .spawn(
            1,
            Uuid::from_u128(1),
            MobKind::Zombie,
            [-4.5, 64.0, 0.5],
            &Terrain,
        )
        .unwrap();
    let target = PlayerTarget {
        id: 10,
        position: [4.5, 64.0, 0.5],
        attackable: true,
    };
    let mut attacked = false;
    let mut detoured = false;
    for _ in 0..320 {
        for change in world.tick(&Terrain, &[target]) {
            attacked |= matches!(change, EntityChange::AttackPlayer { player: 10, .. });
        }
        let mob = world.get(1).unwrap();
        detoured |= mob.position[2].abs() > 2.8;
        assert!(
            !(mob.position[0] > -0.3
                && mob.position[0] < 1.3
                && mob.position[2].abs() < 2.7
                && mob.position[1] < 65.9)
        );
    }
    assert!(
        detoured && attacked,
        "zombie failed to navigate and attack: {:?}",
        world.get(1).unwrap().position
    );
    world
        .spawn(
            2,
            Uuid::from_u128(2),
            MobKind::Cow,
            [4.5, 64.0, 5.5],
            &Terrain,
        )
        .unwrap();
    let target = PlayerTarget {
        attackable: false,
        ..target
    };
    let before = world.get(2).unwrap().position[2];
    for _ in 0..100 {
        assert!(
            world
                .tick_with_food(&Terrain, &[target], &[(MobKind::Cow, target)])
                .iter()
                .all(|change| !matches!(change, EntityChange::AttackPlayer { .. }))
        );
    }
    assert!(world.get(2).unwrap().position[2] < before - 2.0);
}
