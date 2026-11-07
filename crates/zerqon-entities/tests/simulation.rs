use proptest::prelude::*;
use uuid::Uuid;
use zerqon_entities::{EntityChange, EntityWorld, MobKind, PlayerTarget, WorldQuery};

struct Floor;
impl WorldQuery for Floor {
    fn block_solid(&self, _x: i32, y: i32, _z: i32) -> bool {
        y < 64
    }
    fn surface_y(&self, _x: i32, _z: i32) -> Option<i32> {
        Some(64)
    }
}

fn spawn(world: &mut EntityWorld, id: i32, kind: MobKind, position: [f64; 3]) {
    world
        .spawn(id, Uuid::from_u128(id as u128), kind, position, &Floor)
        .unwrap();
}

#[test]
fn falling_mob_lands_without_floor_drift() {
    let mut world = EntityWorld::new(7, 8).unwrap();
    spawn(&mut world, 1, MobKind::Cow, [0.5, 70.0, 0.5]);
    world.tick(&Floor, &[]);
    assert!(world.get(1).unwrap().position[1] < 70.0);
    for _ in 0..1000 {
        world.tick(&Floor, &[]);
    }
    let cow = world.get(1).unwrap();
    assert_eq!(cow.position[1], 64.0);
    assert!(cow.on_ground);
    assert_eq!(cow.velocity[1], 0.0);
}

#[test]
fn capacity_identity_and_bad_positions_fail_without_mutation() {
    let mut world = EntityWorld::new(7, 1).unwrap();
    assert!(
        world
            .spawn(
                1,
                Uuid::from_u128(1),
                MobKind::Pig,
                [f64::NAN, 64.0, 0.0],
                &Floor
            )
            .is_err()
    );
    assert!(
        world
            .spawn(
                1,
                Uuid::from_u128(1),
                MobKind::Pig,
                [0.0, 63.5, 0.0],
                &Floor
            )
            .is_err()
    );
    assert!(world.is_empty());
    spawn(&mut world, 1, MobKind::Pig, [0.0, 64.0, 0.0]);
    let snapshot = world.snapshot();
    assert!(
        world
            .spawn(
                2,
                Uuid::from_u128(2),
                MobKind::Cow,
                [3.0, 64.0, 0.0],
                &Floor
            )
            .is_err()
    );
    assert_eq!(world.snapshot(), snapshot);
}

#[test]
fn damage_updates_health_then_removes_dead_entity_and_index() {
    let mut world = EntityWorld::new(7, 8).unwrap();
    spawn(&mut world, 1, MobKind::Pig, [0.0, 64.0, 0.0]);
    assert!(world.damage(1, f32::NAN).is_err());
    let changes = world.damage(1, 3.0).unwrap();
    assert_eq!(changes, [EntityChange::Health { id: 1, health: 7.0 }]);
    let changes = world.damage(1, 7.0).unwrap();
    assert!(matches!(
        changes.last(),
        Some(EntityChange::Removed { id: 1, .. })
    ));
    assert!(world.get(1).is_none());
    assert!(world.nearby([0.0, 64.0, 0.0], 5.0).unwrap().is_empty());
}

#[test]
fn zombie_pursues_only_attackable_players_and_respects_attack_cooldown() {
    let mut world = EntityWorld::new(4, 8).unwrap();
    spawn(&mut world, 1, MobKind::Zombie, [0.0, 64.0, 0.0]);
    let target = PlayerTarget {
        id: 90,
        position: [5.0, 64.0, 0.0],
        attackable: true,
    };
    for _ in 0..20 {
        world.tick(&Floor, &[target]);
    }
    assert!(world.get(1).unwrap().position[0] > 1.0);
    let target = PlayerTarget {
        id: 90,
        position: world.get(1).unwrap().position,
        attackable: true,
    };
    let mut hits = 0;
    for _ in 0..40 {
        hits += world
            .tick(&Floor, &[target])
            .iter()
            .filter(|change| matches!(change, EntityChange::AttackPlayer { player: 90, .. }))
            .count();
    }
    assert_eq!(hits, 2);
    assert!(
        !world
            .tick(
                &Floor,
                &[PlayerTarget {
                    attackable: false,
                    ..target
                }]
            )
            .iter()
            .any(|change| matches!(change, EntityChange::AttackPlayer { .. }))
    );
}

#[test]
fn natural_spawning_is_seeded_bounded_and_away_from_players() {
    let players = [PlayerTarget {
        id: 90,
        position: [0.0, 64.0, 0.0],
        attackable: true,
    }];
    let mut first = EntityWorld::new(123, 3).unwrap();
    let mut second = EntityWorld::new(123, 3).unwrap();
    for id in 1..=20 {
        let a = first
            .try_natural_spawn(&Floor, &players, false, || {
                (id, Uuid::from_u128(id as u128))
            })
            .unwrap();
        let b = second
            .try_natural_spawn(&Floor, &players, false, || {
                (id, Uuid::from_u128(id as u128))
            })
            .unwrap();
        assert_eq!(a, b);
    }
    assert_eq!(first.len(), 3);
    for mob in first.entities() {
        assert_ne!(mob.kind, MobKind::Zombie);
        assert!(mob.position[0].hypot(mob.position[2]) >= 24.0);
    }
}

#[test]
fn duplicate_ids_and_uuids_do_not_replace_existing_entities() {
    let mut world = EntityWorld::new(1, 8).unwrap();
    spawn(&mut world, 1, MobKind::Pig, [0.0, 64.0, 0.0]);
    let before = world.snapshot();
    assert!(
        world
            .spawn(
                1,
                Uuid::from_u128(2),
                MobKind::Cow,
                [4.0, 64.0, 0.0],
                &Floor
            )
            .is_err()
    );
    assert!(
        world
            .spawn(
                2,
                Uuid::from_u128(1),
                MobKind::Cow,
                [4.0, 64.0, 0.0],
                &Floor
            )
            .is_err()
    );
    assert_eq!(world.snapshot(), before);
}

#[test]
fn unloaded_entities_keep_their_physics_and_ai_state() {
    struct Unloaded;
    impl WorldQuery for Unloaded {
        fn loaded_at(&self, _x: i32, _z: i32) -> bool {
            false
        }
        fn block_solid(&self, _x: i32, _y: i32, _z: i32) -> bool {
            panic!("unloaded terrain must not be queried")
        }
        fn surface_y(&self, _x: i32, _z: i32) -> Option<i32> {
            None
        }
    }
    let mut world = EntityWorld::new(3, 8).unwrap();
    spawn(&mut world, 1, MobKind::Cow, [0.0, 70.0, 0.0]);
    let before = world.get(1).unwrap().clone();
    let rng = world.snapshot().random_state;
    for _ in 0..20 {
        assert!(world.tick(&Unloaded, &[]).is_empty());
    }
    assert_eq!(world.get(1), Some(&before));
    assert_eq!(world.snapshot().random_state, rng);
}

#[test]
fn player_attack_reach_is_bounded_and_rejects_nonfinite_positions() {
    let mut world = EntityWorld::new(3, 8).unwrap();
    spawn(&mut world, 1, MobKind::Pig, [0.0, 64.0, 0.0]);
    assert!(world.reachable_from(1, [2.0, 64.0, 0.0], &Floor));
    assert!(!world.reachable_from(1, [4.0, 64.0, 0.0], &Floor));
    assert!(!world.reachable_from(1, [f64::NAN, 64.0, 0.0], &Floor));
}

#[test]
fn corrupted_snapshots_are_refused_and_valid_state_is_preserved() {
    let mut world = EntityWorld::new(3, 8).unwrap();
    spawn(&mut world, 1, MobKind::Cow, [0.0, 64.0, 0.0]);
    let snapshot = world.snapshot();
    let mut invalid = snapshot.clone();
    invalid.entities[0].position[0] = f64::INFINITY;
    assert!(EntityWorld::restore(invalid, 8).is_err());
    let mut invalid = snapshot.clone();
    invalid.entities[0].health = 0.0;
    assert!(EntityWorld::restore(invalid, 8).is_err());
    let mut invalid = snapshot.clone();
    invalid.entities[0].velocity[1] = 1000.0;
    assert!(EntityWorld::restore(invalid, 8).is_err());
    let mut invalid = snapshot.clone();
    invalid.entities.push(invalid.entities[0].clone());
    assert!(EntityWorld::restore(invalid, 8).is_err());
    let mut invalid = snapshot.clone();
    invalid.schema = 999;
    assert!(EntityWorld::restore(invalid, 8).is_err());
    assert_eq!(
        EntityWorld::restore(snapshot.clone(), 8)
            .unwrap()
            .snapshot(),
        snapshot
    );
    assert!(world.nearby([0.0, 64.0, 0.0], f64::INFINITY).is_err());
    assert!(world.nearby([0.0, 64.0, 0.0], 129.0).is_err());
}

#[test]
fn zombie_cannot_attack_through_a_solid_wall() {
    struct Wall;
    impl WorldQuery for Wall {
        fn block_solid(&self, x: i32, y: i32, _z: i32) -> bool {
            y < 64 || (x == 1 && (64..68).contains(&y))
        }
        fn surface_y(&self, _x: i32, _z: i32) -> Option<i32> {
            Some(64)
        }
    }
    let mut world = EntityWorld::new(3, 8).unwrap();
    world
        .spawn(
            1,
            Uuid::from_u128(1),
            MobKind::Zombie,
            [0.0, 64.0, 0.0],
            &Wall,
        )
        .unwrap();
    let player = PlayerTarget {
        id: 90,
        position: [2.0, 64.0, 0.0],
        attackable: true,
    };
    for _ in 0..100 {
        assert!(
            !world
                .tick(&Wall, &[player])
                .iter()
                .any(|event| matches!(event, EntityChange::AttackPlayer { .. }))
        );
        assert!(world.get(1).unwrap().position[0] <= 1.0 - f64::from(0.6_f32) / 2.0);
    }
}

#[test]
fn nearest_player_ties_use_identity_not_input_order() {
    let mut first = EntityWorld::new(3, 8).unwrap();
    spawn(&mut first, 1, MobKind::Zombie, [0.0, 64.0, 0.0]);
    let mut second = EntityWorld::restore(first.snapshot(), 8).unwrap();
    let players = [
        PlayerTarget {
            id: 90,
            position: [5.0, 64.0, 0.0],
            attackable: true,
        },
        PlayerTarget {
            id: 89,
            position: [-5.0, 64.0, 0.0],
            attackable: true,
        },
    ];
    assert_eq!(
        first.tick(&Floor, &players),
        second.tick(&Floor, &[players[1], players[0]])
    );
    assert!(first.get(1).unwrap().position[0] < 0.0);
}

#[test]
fn natural_spawn_does_not_allocate_identity_when_capped_or_without_players() {
    let mut world = EntityWorld::new(0, 1).unwrap();
    assert!(
        world
            .try_natural_spawn(&Floor, &[], true, || panic!(
                "identity must not be allocated"
            ))
            .unwrap()
            .is_none()
    );
    spawn(&mut world, 1, MobKind::Pig, [0.0, 64.0, 0.0]);
    let players = [PlayerTarget {
        id: 90,
        position: [0.0, 64.0, 0.0],
        attackable: true,
    }];
    assert!(
        world
            .try_natural_spawn(&Floor, &players, true, || panic!(
                "identity must not be allocated"
            ))
            .unwrap()
            .is_none()
    );
}

#[test]
#[ignore = "manual optimized-profile throughput measurement"]
fn measure_one_thousand_entities_on_flat_ground() {
    let mut world = EntityWorld::new(12345, 1000).unwrap();
    for id in 1..=1000 {
        spawn(
            &mut world,
            id,
            if id % 3 == 0 {
                MobKind::Zombie
            } else {
                MobKind::Pig
            },
            [
                ((id - 1) % 50) as f64 * 2.0,
                64.0,
                ((id - 1) / 50) as f64 * 2.0,
            ],
        );
    }
    for _ in 0..20 {
        world.tick(&Floor, &[]);
    }
    let start = std::time::Instant::now();
    let mut changes = 0;
    for _ in 0..200 {
        changes += world.tick(&Floor, &[]).len();
    }
    let elapsed = start.elapsed();
    assert_eq!(world.len(), 1000);
    println!(
        "entities=1000 ticks=200 elapsed_ms={:.3} mean_tick_ms={:.3} changes={} mob_struct_bytes={} snapshot_json_bytes={}",
        elapsed.as_secs_f64() * 1000.0,
        elapsed.as_secs_f64() * 5.0,
        changes,
        std::mem::size_of::<zerqon_entities::Mob>(),
        serde_json::to_vec(&world.snapshot()).unwrap().len()
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    #[test]
    fn snapshot_restore_continues_identically(seed in any::<u64>(),ticks in 1_u32..60){
        let mut original=EntityWorld::new(seed,8).unwrap();
        spawn(&mut original,1,MobKind::Pig,[-17.0,64.0,-1.0]);
        spawn(&mut original,2,MobKind::Cow,[4.0,66.0,4.0]);
        for _ in 0..ticks{original.tick(&Floor,&[]);}
        let encoded=serde_json::to_vec(&original.snapshot()).unwrap();
        let mut restored=EntityWorld::restore(serde_json::from_slice(&encoded).unwrap(),8).unwrap();
        for _ in 0..40{prop_assert_eq!(original.tick(&Floor,&[]),restored.tick(&Floor,&[]));}
        prop_assert_eq!(original.snapshot(),restored.snapshot());
    }
    #[test]
    fn spatial_query_matches_linear_distance(seed in any::<u64>(),center_x in -40_i32..40,center_z in -40_i32..40,radius in 0_u32..40){
        let mut world=EntityWorld::new(seed,40).unwrap();
        for id in 1..=32{spawn(&mut world,id,MobKind::Pig,[(id-16) as f64*2.0,64.0,((id*7)%13) as f64*2.0]);}
        for _ in 0..10{world.tick(&Floor,&[]);}
        let center=[center_x as f64,64.0,center_z as f64];
        let mut expected=world.entities().filter(|mob|mob.position.iter().zip(center).map(|(a,b)|(a-b).powi(2)).sum::<f64>()<=(radius as f64).powi(2)).map(|mob|mob.id).collect::<Vec<_>>();
        let mut actual=world.nearby(center,radius as f64).unwrap().iter().map(|mob|mob.id).collect::<Vec<_>>();
        actual.sort();expected.sort();prop_assert_eq!(actual,expected);
    }
}
