use uuid::Uuid;
use zerqon_entities::{EntityChange, EntityWorld, MobKind, Snapshot, WorldQuery};

struct Floor;
impl WorldQuery for Floor {
    fn block_solid(&self, _: i32, y: i32, _: i32) -> bool {
        y < 64
    }
    fn surface_y(&self, _: i32, _: i32) -> Option<i32> {
        Some(64)
    }
}

struct LowRoom;
impl WorldQuery for LowRoom {
    fn block_solid(&self, _: i32, y: i32, _: i32) -> bool {
        !(64..65).contains(&y)
    }
    fn surface_y(&self, _: i32, _: i32) -> Option<i32> {
        Some(64)
    }
}

#[test]
fn breeding_persists_age_cooldowns_growth_and_loads_legacy_adults() {
    let mut world = EntityWorld::new(42, 8).unwrap();
    for (id, x) in [(1, 0.5), (2, 2.5)] {
        world
            .spawn(
                id,
                Uuid::from_u128(id as u128),
                MobKind::Cow,
                [x, 64.0, 0.5],
                &Floor,
            )
            .unwrap();
    }
    let mut legacy = serde_json::to_value(world.snapshot()).unwrap();
    legacy["schema"] = 1.into();
    for mob in legacy["entities"].as_array_mut().unwrap() {
        for key in ["age", "forced_age", "love_ticks"] {
            mob.as_object_mut().unwrap().remove(key);
        }
    }
    let loaded = EntityWorld::restore(serde_json::from_value(legacy).unwrap(), 8).unwrap();
    assert!(loaded.entities().all(|mob| mob.age == 0 && mob.can_feed()));
    world.damage(1, 4.0).unwrap();
    assert!(matches!(
        world.feed(1).unwrap()[0],
        EntityChange::Love { id: 1 }
    ));
    assert_eq!(
        world.get(1).unwrap().health,
        6.0,
        "food must not heal adult animals"
    );
    assert!(world.feed(1).is_err());
    world.feed(2).unwrap();
    assert!(
        world
            .breed(1, 2, 3, Uuid::from_u128(3), &Floor)
            .unwrap()
            .is_none()
    );
    for _ in 0..60 {
        world.tick(&Floor, &[]);
    }
    let (a, b) = world.breeding_pairs()[0];
    assert!(matches!(
        world.breed(a, b, 3, Uuid::from_u128(3), &Floor).unwrap(),
        Some(EntityChange::Spawned(_))
    ));
    assert_eq!(world.get(1).unwrap().age, 6000);
    assert_eq!(world.get(2).unwrap().love_ticks, 0);
    assert!(world.feed(1).is_err());
    let child = world.get(3).unwrap();
    assert!(child.is_baby());
    assert_eq!(child.age, -24000);
    assert_eq!(
        child.dimensions(),
        MobKind::Cow.dimensions().map(|value| value * 0.5)
    );
    world.feed(3).unwrap();
    assert_eq!(world.get(3).unwrap().age, -21600);
    assert_eq!(world.get(3).unwrap().forced_age, 2400);
    let saved: Snapshot =
        serde_json::from_slice(&serde_json::to_vec(&world.snapshot()).unwrap()).unwrap();
    let mut restored = EntityWorld::restore(saved.clone(), 8).unwrap();
    assert_eq!(restored.snapshot(), saved);
    assert!(restored.breeding_pairs().is_empty());
    let mut baby_only = restored.snapshot();
    baby_only.entities.retain(|mob| mob.id == 3);
    let start = baby_only.entities[0].position;
    let mut small = EntityWorld::restore(baby_only, 8).unwrap();
    let lure = zerqon_entities::PlayerTarget {
        id: 99,
        position: [start[0] + 5.0, 64.0, start[2]],
        attackable: false,
    };
    for _ in 0..100 {
        let changes = small.tick_with_food(&LowRoom, &[lure], &[(MobKind::Cow, lure)]);
        assert!(
            !changes
                .iter()
                .any(|change| matches!(change, EntityChange::PhysicsBlocked { id: 3, .. })),
            "baby collision used adult dimensions"
        );
    }
    assert!(
        small.get(3).unwrap().position[0] > start[0] + 2.0,
        "baby could not navigate a low passage"
    );
    let mut growth = restored.snapshot();
    growth.entities.iter_mut().find(|m| m.id == 3).unwrap().age = -1;
    restored = EntityWorld::restore(growth, 8).unwrap();
    assert!(
        restored
            .tick(&Floor, &[])
            .iter()
            .any(|change| matches!(change, EntityChange::AgeChanged { id: 3, baby: false }))
    );
    assert!(!restored.get(3).unwrap().is_baby());
    let mut invalid = restored.snapshot();
    invalid.entities[0].love_ticks = 601;
    assert!(EntityWorld::restore(invalid, 8).is_err());
}
