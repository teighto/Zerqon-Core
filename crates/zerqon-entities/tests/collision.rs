use std::{collections::BTreeMap, sync::OnceLock};
use uuid::Uuid;
use zerqon_blocks::{Aabb, Catalog, StateId};
use zerqon_entities::{
    BlockCollision, EntityChange, EntityWorld, MobKind, PhysicsError, WorldQuery, move_mob,
};

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| Catalog::embedded().unwrap())
}
fn state(key: &str, properties: &[(&str, &str)]) -> StateId {
    catalog()
        .resolve(
            key,
            &properties
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
        .unwrap()
}

struct Terrain {
    blocks: BTreeMap<[i32; 3], StateId>,
    missing: Option<[i32; 3]>,
    unsupported: Option<[i32; 3]>,
    floor: bool,
}
impl Default for Terrain {
    fn default() -> Self {
        Self {
            blocks: BTreeMap::new(),
            missing: None,
            unsupported: None,
            floor: true,
        }
    }
}
impl WorldQuery for Terrain {
    fn block_solid(&self, x: i32, y: i32, z: i32) -> bool {
        !matches!(self.block_collision(x, y, z), BlockCollision::Empty)
    }
    fn block_collision(&self, x: i32, y: i32, z: i32) -> BlockCollision<'_> {
        if self.missing == Some([x, y, z]) {
            return BlockCollision::Unavailable;
        }
        if self.unsupported == Some([x, y, z]) {
            return BlockCollision::Unsupported;
        }
        if let Some(id) = self.blocks.get(&[x, y, z]) {
            let state = catalog().state(*id).unwrap();
            if state.has_fluid() {
                return BlockCollision::Unsupported;
            }
            return match state.collision() {
                Ok(boxes) => BlockCollision::Static(boxes),
                Err(_) => BlockCollision::Unsupported,
            };
        }
        if y < 0 && self.floor {
            BlockCollision::Full
        } else {
            BlockCollision::Empty
        }
    }
    fn surface_y(&self, _x: i32, _z: i32) -> Option<i32> {
        Some(0)
    }
}

#[test]
fn each_mob_lands_on_the_actual_slab_top_without_tunneling() {
    for kind in [MobKind::Pig, MobKind::Cow, MobKind::Zombie] {
        for (slab_type, height) in [("bottom", 0.5), ("top", 1.0), ("double", 1.0)] {
            let world = Terrain {
                blocks: BTreeMap::from([(
                    [0, 0, 0],
                    state("minecraft:oak_slab", &[("type", slab_type)]),
                )]),
                ..Terrain::default()
            };
            let result =
                move_mob([0.5, 12.0, 0.5], kind, [0.0, -20.0, 0.0], false, &world).unwrap();
            assert_eq!(result.position[1], height);
            assert!(result.on_ground);
            assert!(result.collided[1]);
        }
    }
}

#[test]
fn grounded_mobs_step_half_a_block_but_cannot_step_full_blocks() {
    for kind in [MobKind::Pig, MobKind::Cow, MobKind::Zombie] {
        let half = kind.dimensions()[0] / 2.0;
        for (slab_type, expected_height) in [("bottom", 0.5), ("top", 0.0)] {
            let world = Terrain {
                blocks: BTreeMap::from([(
                    [1, 0, 0],
                    state("minecraft:oak_slab", &[("type", slab_type)]),
                )]),
                ..Terrain::default()
            };
            let result = move_mob([0.5, 0.0, 0.5], kind, [0.75, -0.08, 0.0], true, &world).unwrap();
            assert_eq!(result.position[1], expected_height);
            if slab_type == "bottom" {
                assert_eq!(result.position[0], 1.25);
            } else {
                assert_eq!(result.position[0], 1.0 - half);
            }
        }
    }
}

#[test]
fn low_headroom_blocks_step_up_and_preserves_the_free_axis() {
    let world = Terrain {
        blocks: BTreeMap::from([
            ([1, 0, 0], state("minecraft:oak_slab", &[])),
            ([0, 1, 0], state("minecraft:stone", &[])),
            ([1, 1, 0], state("minecraft:stone", &[])),
        ]),
        ..Terrain::default()
    };
    let result = move_mob(
        [0.5, 0.0, 0.5],
        MobKind::Pig,
        [0.75, -0.08, 0.2],
        true,
        &world,
    )
    .unwrap();
    assert_eq!(result.position[1], 0.0);
    assert!(result.collided[0]);
    assert_eq!(result.position[2], 0.7);
}

#[test]
fn stair_treads_are_climbed_in_two_half_steps_at_negative_coordinates() {
    let world = Terrain {
        blocks: BTreeMap::from([(
            [-2, 0, -3],
            state("minecraft:oak_stairs", &[("facing", "east")]),
        )]),
        ..Terrain::default()
    };
    let first = move_mob(
        [-2.5, 0.0, -2.5],
        MobKind::Pig,
        [0.4, -0.08, 0.0],
        true,
        &world,
    )
    .unwrap();
    assert_eq!(first.position[1], 0.5);
    let second = move_mob(
        first.position,
        MobKind::Pig,
        [0.4, -0.08, 0.0],
        true,
        &world,
    )
    .unwrap();
    assert_eq!(second.position[1], 1.0);
    assert!((second.position[0] + 1.7).abs() < 1e-12);
}

#[test]
fn incomplete_or_dynamic_neighbor_geometry_cannot_be_treated_as_empty() {
    for world in [
        Terrain {
            missing: Some([1, 0, 0]),
            ..Terrain::default()
        },
        Terrain {
            unsupported: Some([1, 0, 0]),
            ..Terrain::default()
        },
    ] {
        assert!(matches!(
            move_mob(
                [0.5, 0.0, 0.5],
                MobKind::Pig,
                [0.01, 0.0, 0.0],
                false,
                &world
            ),
            Err(PhysicsError::Unavailable(_) | PhysicsError::Unsupported(_))
        ));
    }
}

#[test]
fn unsupported_physics_preserves_entity_and_random_state_and_reports_once() {
    let mut engine = EntityWorld::new(7, 8).unwrap();
    engine
        .spawn(
            1,
            Uuid::from_u128(1),
            MobKind::Pig,
            [0.5, 0.0, 0.5],
            &Terrain::default(),
        )
        .unwrap();
    let before = engine.get(1).unwrap().clone();
    let rng = engine.snapshot().random_state;
    let world = Terrain {
        unsupported: Some([0, 0, 0]),
        ..Terrain::default()
    };
    assert!(matches!(
        engine.tick(&world, &[]).as_slice(),
        [EntityChange::PhysicsBlocked { id: 1, .. }]
    ));
    assert!(engine.tick(&world, &[]).is_empty());
    assert_eq!(engine.get(1), Some(&before));
    assert_eq!(engine.snapshot().random_state, rng);
}

#[test]
fn invalid_collision_shapes_and_motion_are_rejected_before_unbounded_work() {
    let body = zerqon_entities::body_bounds([0.0, 1.0, 0.0], MobKind::Pig);
    assert!(zerqon_entities::clip_motion(body, [f64::NAN, 0.0, 0.0], &[]).is_err());
    assert!(zerqon_entities::clip_motion(body, [33.0, 0.0, 0.0], &[]).is_err());
    assert!(
        zerqon_entities::clip_motion(
            body,
            [0.0, -1.0, 0.0],
            &[Aabb {
                min: [f64::NAN; 3],
                max: [1.0; 3]
            }]
        )
        .is_err()
    );
}

#[test]
fn waterlogged_fence_retains_geometry_but_fluid_simulation_is_explicitly_unsupported() {
    let dry = state("minecraft:oak_fence", &[("waterlogged", "false")]);
    let wet = state("minecraft:oak_fence", &[("waterlogged", "true")]);
    assert!(
        !catalog()
            .state(wet)
            .unwrap()
            .collision()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        catalog().state(dry).unwrap().collision().unwrap(),
        catalog().state(wet).unwrap().collision().unwrap()
    );
    let world = Terrain {
        blocks: BTreeMap::from([([0, 0, 0], wet)]),
        ..Terrain::default()
    };
    assert!(matches!(
        move_mob(
            [0.5, 2.0, 0.5],
            MobKind::Pig,
            [0.0, -3.0, 0.0],
            false,
            &world
        ),
        Err(PhysicsError::Unsupported(_))
    ));
}

#[test]
fn original_entity_collide_and_static_clipper_match_all_fixture_scenes() {
    #[derive(serde::Deserialize)]
    struct Fixture {
        cases: Vec<Case>,
    }
    #[derive(serde::Deserialize)]
    struct Case {
        name: String,
        kind: usize,
        step: f32,
        floor: bool,
        grounded: bool,
        position: [f64; 3],
        movement: [f64; 3],
        blocks: Vec<[i32; 4]>,
        expected: [f64; 3],
        clip_expected: [f64; 3],
    }
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/collision-26.3.json")).unwrap();
    assert_eq!(fixture.cases.len(), 438);
    for case in fixture.cases {
        let kind = [MobKind::Pig, MobKind::Cow, MobKind::Zombie][case.kind];
        assert_eq!(kind.step_height(), case.step);
        let mut shapes = Vec::new();
        if case.floor {
            shapes.push(vec![Aabb {
                min: [-64.0, -32.0, -64.0],
                max: [64.0, 0.0, 64.0],
            }]);
        }
        for [x, y, z, id] in &case.blocks {
            shapes.push(
                catalog()
                    .state(StateId(*id as u32))
                    .unwrap()
                    .collision()
                    .unwrap()
                    .iter()
                    .map(|shape| shape.translated(zerqon_blocks::Position::new(*x, *y, *z)))
                    .collect::<Vec<_>>(),
            );
        }
        let blocks = case
            .blocks
            .into_iter()
            .map(|[x, y, z, id]| ([x, y, z], StateId(id as u32)))
            .collect();
        let world = Terrain {
            blocks,
            floor: case.floor,
            ..Terrain::default()
        };
        let actual = move_mob(case.position, kind, case.movement, case.grounded, &world)
            .unwrap_or_else(|error| panic!("{}:{error}", case.name));
        for axis in 0..3 {
            assert!(
                (actual.displacement[axis] - case.expected[axis]).abs() < 1e-10,
                "{} axis{axis}: {:?} != {:?}",
                case.name,
                actual.displacement,
                case.expected
            );
        }
        let actual = zerqon_entities::clip_shapes(
            zerqon_entities::body_bounds(case.position, kind),
            case.movement,
            &shapes.iter().map(Vec::as_slice).collect::<Vec<_>>(),
        )
        .unwrap();
        for axis in 0..3 {
            assert!(
                (actual[axis] - case.clip_expected[axis]).abs() < 1e-10,
                "{} rawclip axis{axis}: {:?} != {:?}",
                case.name,
                actual,
                case.clip_expected
            );
        }
    }
}

#[test]
fn natural_spawning_requires_actual_support_at_the_reported_height() {
    struct Empty;
    impl WorldQuery for Empty {
        fn block_solid(&self, _x: i32, _y: i32, _z: i32) -> bool {
            false
        }
        fn surface_y(&self, _x: i32, _z: i32) -> Option<i32> {
            Some(64)
        }
    }
    let mut engine = EntityWorld::new(123, 8).unwrap();
    let players = [zerqon_entities::PlayerTarget {
        id: 90,
        position: [0.0, 64.0, 0.0],
        attackable: false,
    }];
    for _ in 0..8 {
        assert!(
            engine
                .try_natural_spawn(&Empty, &players, false, || panic!(
                    "unsupported spawn allocated an identity"
                ))
                .unwrap()
                .is_none()
        );
    }
}
