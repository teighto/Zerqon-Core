use proptest::prelude::*;
use std::{collections::BTreeMap, sync::OnceLock};
use zerqon_blocks::*;

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
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        )
        .unwrap()
}

fn context() -> Interaction<'static> {
    Interaction {
        eye: [0.5, 2.62, 2.5],
        block_range: 5.0,
        creative: true,
        bounds: Bounds::default(),
        blockers: &[],
    }
}

#[test]
fn creative_stone_placement_uses_authoritative_item_and_preserves_preconditions() {
    let clicked = Position::new(0, 0, 0);
    let old = state("minecraft:stone", &[]);
    let view = |position| {
        Some(if position == clicked {
            old
        } else {
            catalog().air()
        })
    };
    let properties = BTreeMap::new();
    let request = Placement {
        clicked,
        face: Direction::Up,
        local_hit: [0.5, 1.0, 0.5],
        facing: Direction::North,
        item: catalog().item_id("minecraft:stone").unwrap(),
        secondary_use: false,
        properties: &properties,
    };
    let plan = catalog()
        .plan_placement(&view, &context(), &request)
        .unwrap();
    assert_eq!(
        plan.changes(),
        [BlockChange {
            position: Position::new(0, 1, 0),
            expected: catalog().air(),
            replacement: old
        }]
    );
    assert!(plan.reads().contains(&(clicked, old)));
    assert!(
        plan.reads()
            .contains(&(Position::new(0, 1, 0), catalog().air()))
    );
}

#[test]
fn creative_break_restores_air_without_consuming_a_held_item() {
    let position = Position::new(0, 0, 0);
    let stone = state("minecraft:stone", &[]);
    let view = |p| {
        Some(if p == position {
            stone
        } else {
            catalog().air()
        })
    };
    let plan = catalog()
        .plan_break(&view, &context(), position, None)
        .unwrap();
    assert_eq!(
        plan.changes(),
        [BlockChange {
            position,
            expected: stone,
            replacement: catalog().air()
        }]
    );
}

#[test]
fn original_server_stair_shape_cases_match_rust() {
    type Fixture = (u32, u32, Vec<(i32, i32, u32)>);
    let cases: Vec<Fixture> =
        serde_json::from_str(include_str!("fixtures/stair-shapes.json")).unwrap();
    assert_eq!(cases.len(), 384);
    for (index, (base, expected, neighbors)) in cases.iter().enumerate() {
        let view = |position: Position| {
            Some(
                neighbors
                    .iter()
                    .find(|(x, z, _)| position == Position::new(*x, 0, *z))
                    .map_or(catalog().air(), |(_, _, state)| StateId(*state)),
            )
        };
        let actual = catalog()
            .stair_state(&view, Position::new(0, 0, 0), StateId(*base))
            .unwrap();
        assert_eq!(actual, StateId(*expected), "original fixture {index}");
    }
}

#[test]
fn logs_follow_the_clicked_face_axis() {
    let properties = BTreeMap::new();
    let clicked = Position::new(0, 0, 0);
    let view = |position| {
        Some(if position == clicked {
            state("minecraft:stone", &[])
        } else {
            catalog().air()
        })
    };
    for (face, axis) in [
        (Direction::Down, "y"),
        (Direction::Up, "y"),
        (Direction::East, "x"),
        (Direction::West, "x"),
        (Direction::North, "z"),
        (Direction::South, "z"),
    ] {
        let request = Placement {
            clicked,
            face,
            local_hit: [0.5; 3],
            facing: Direction::North,
            item: catalog().item_id("minecraft:oak_log").unwrap(),
            secondary_use: false,
            properties: &properties,
        };
        let plan = catalog()
            .plan_placement(&view, &context(), &request)
            .unwrap();
        assert_eq!(
            plan.changes()[0].replacement,
            state("minecraft:oak_log", &[("axis", axis)])
        );
    }
}

#[test]
fn slab_side_hit_boundary_and_same_slab_merge_match_vanilla() {
    let properties = BTreeMap::new();
    let clicked = Position::new(0, 0, 0);
    for (old, face, y, want, target) in [
        (
            "stone",
            Direction::East,
            0.5,
            "bottom",
            Position::new(1, 0, 0),
        ),
        (
            "stone",
            Direction::East,
            0.5001,
            "top",
            Position::new(1, 0, 0),
        ),
        (
            "stone",
            Direction::Down,
            0.5,
            "top",
            Position::new(0, -1, 0),
        ),
        ("oak_slab", Direction::Up, 0.5, "double", clicked),
        ("oak_slab", Direction::East, 0.75, "double", clicked),
        (
            "oak_slab",
            Direction::East,
            0.5,
            "bottom",
            Position::new(1, 0, 0),
        ),
    ] {
        let original = state(&format!("minecraft:{old}"), &[]);
        let view = |position| {
            Some(if position == clicked {
                original
            } else {
                catalog().air()
            })
        };
        let request = Placement {
            clicked,
            face,
            local_hit: [0.5, y, 0.5],
            facing: Direction::North,
            item: catalog().item_id("minecraft:oak_slab").unwrap(),
            secondary_use: false,
            properties: &properties,
        };
        let plan = catalog()
            .plan_placement(&view, &context(), &request)
            .unwrap();
        assert!(
            plan.changes().iter().any(|change| change.position == target
                && change.replacement == state("minecraft:oak_slab", &[("type", want)])),
            "{old} {face:?} {y}"
        );
    }
}

#[test]
fn placing_and_breaking_stairs_recalculates_existing_neighbors() {
    let original = state("minecraft:oak_stairs", &[("facing", "north")]);
    let origin = Position::new(0, 0, 0);
    let target = Position::new(0, 0, -1);
    let floor = Position::new(0, -1, -1);
    let view = |p| {
        Some(if p == origin {
            original
        } else if p == floor {
            state("minecraft:stone", &[])
        } else {
            catalog().air()
        })
    };
    let properties = BTreeMap::new();
    let request = Placement {
        clicked: floor,
        face: Direction::Up,
        local_hit: [0.5, 1.0, 0.5],
        facing: Direction::West,
        item: catalog().item_id("minecraft:stone_stairs").unwrap(),
        secondary_use: false,
        properties: &properties,
    };
    let plan = catalog()
        .plan_placement(&view, &context(), &request)
        .unwrap();
    let shaped = state(
        "minecraft:oak_stairs",
        &[("facing", "north"), ("shape", "outer_left")],
    );
    assert!(plan.changes().contains(&BlockChange {
        position: origin,
        expected: original,
        replacement: shaped
    }));
    let changed = |p| {
        plan.changes()
            .iter()
            .find(|change| change.position == p)
            .map(|change| change.replacement)
            .or_else(|| view(p))
    };
    let removed = catalog()
        .plan_break(&changed, &context(), target, None)
        .unwrap();
    assert!(removed.changes().contains(&BlockChange {
        position: origin,
        expected: shaped,
        replacement: original
    }));
}

fn stone_request(properties: &BTreeMap<String, String>) -> Placement<'_> {
    Placement {
        clicked: Position::new(0, 0, 0),
        face: Direction::Up,
        local_hit: [0.5, 1.0, 0.5],
        facing: Direction::North,
        item: catalog().item_id("minecraft:stone").unwrap(),
        secondary_use: false,
        properties,
    }
}

fn floor(position: Position) -> Option<StateId> {
    Some(if position.y == 0 {
        state("minecraft:stone", &[])
    } else {
        catalog().air()
    })
}

#[test]
fn nonfinite_or_outside_hit_coordinates_are_rejected() {
    let properties = BTreeMap::new();
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.5001, -0.5001] {
        for axis in 0..3 {
            let mut request = stone_request(&properties);
            request.local_hit[axis] = value;
            assert!(matches!(
                catalog().plan_placement(&floor, &context(), &request),
                Err(BuildError::Invalid)
            ));
        }
    }
}

#[test]
fn range_uses_eye_to_block_box_and_a_strict_boundary() {
    let properties = BTreeMap::new();
    let request = stone_request(&properties);
    let mut interaction = context();
    interaction.eye = [7.0, 0.5, 0.5];
    assert!(matches!(
        catalog().plan_placement(&floor, &interaction, &request),
        Err(BuildError::OutOfReach)
    ));
    interaction.eye[0] = 6.999;
    assert!(
        catalog()
            .plan_placement(&floor, &interaction, &request)
            .is_ok()
    );
    interaction.eye[0] = f64::NAN;
    assert!(matches!(
        catalog().plan_placement(&floor, &interaction, &request),
        Err(BuildError::Invalid)
    ));
}

#[test]
fn missing_chunks_and_occupied_targets_do_not_produce_edits() {
    let properties = BTreeMap::new();
    let request = stone_request(&properties);
    let target = Position::new(0, 1, 0);
    let missing = |position| {
        if position == target {
            None
        } else {
            floor(position)
        }
    };
    assert!(
        matches!(catalog().plan_placement(&missing,&context(),&request),Err(BuildError::Missing(p)) if p==target)
    );
    let occupied = |_| Some(state("minecraft:stone", &[]));
    assert!(
        matches!(catalog().plan_placement(&occupied,&context(),&request),Err(BuildError::Occupied(p)) if p==target)
    );
    let missing_neighbor = |position| {
        if position == Position::new(1, 1, 0) {
            None
        } else {
            floor(position)
        }
    };
    assert!(matches!(
        catalog().plan_placement(&missing_neighbor, &context(), &request),
        Err(BuildError::Missing(_))
    ));
}

#[test]
fn slab_collision_allows_touching_but_rejects_intersection_with_the_placed_half() {
    let properties = BTreeMap::new();
    let mut request = stone_request(&properties);
    request.item = catalog().item_id("minecraft:oak_slab").unwrap();
    let touching = [Aabb {
        min: [0.2, 1.5, 0.2],
        max: [0.8, 3.0, 0.8],
    }];
    let mut interaction = context();
    interaction.blockers = &touching;
    assert!(
        catalog()
            .plan_placement(&floor, &interaction, &request)
            .is_ok()
    );
    let intersects = [Aabb {
        min: [0.2, 1.49, 0.2],
        max: [0.8, 3.0, 0.8],
    }];
    interaction.blockers = &intersects;
    assert!(matches!(
        catalog().plan_placement(&floor, &interaction, &request),
        Err(BuildError::Obstructed(_))
    ));
}

#[test]
fn unsupported_items_fluids_entities_and_neighbor_effects_fail_explicitly() {
    let properties = BTreeMap::new();
    for key in [
        "minecraft:oak_door",
        "minecraft:oak_sign",
        "minecraft:water_bucket",
        "minecraft:sand",
    ] {
        let mut request = stone_request(&properties);
        request.item = catalog().item_id(key).unwrap();
        assert!(
            matches!(
                catalog().plan_placement(&floor, &context(), &request),
                Err(BuildError::UnsupportedItem)
            ),
            "{key}"
        );
    }
    let request = stone_request(&properties);
    for key in [
        "minecraft:water",
        "minecraft:chest",
        "minecraft:redstone_wire",
        "minecraft:powder_snow",
    ] {
        let unsupported = state(key, &[]);
        let view = |position| {
            if position == Position::new(1, 1, 0) {
                Some(unsupported)
            } else {
                floor(position)
            }
        };
        assert!(
            matches!(
                catalog().plan_placement(&view, &context(), &request),
                Err(BuildError::Fluid(_) | BuildError::UnsupportedBlock(_))
            ),
            "{key}"
        );
    }
    let chest = state("minecraft:chest", &[]);
    let view = |position| {
        Some(if position == request.clicked {
            chest
        } else {
            catalog().air()
        })
    };
    assert!(matches!(
        catalog().plan_placement(&view, &context(), &request),
        Err(BuildError::Interactive(_))
    ));
}

#[test]
fn creative_tool_restrictions_and_survival_are_not_silently_ignored() {
    let sword = catalog().item_id("minecraft:diamond_sword").unwrap();
    assert!(matches!(
        catalog().plan_break(&floor, &context(), Position::new(0, 0, 0), Some(sword)),
        Err(BuildError::ToolDenied)
    ));
    let mut interaction = context();
    interaction.creative = false;
    assert!(matches!(
        catalog().plan_break(&floor, &interaction, Position::new(0, 0, 0), None),
        Err(BuildError::Survival)
    ));
    assert!(
        catalog()
            .plan_break(&floor, &context(), Position::new(0, 0, 0), Some(u32::MAX))
            .is_err()
    );
}

#[test]
fn supported_item_properties_override_orientation_but_cannot_enable_fluid_effects() {
    let mut properties = BTreeMap::from([("axis".into(), "x".into())]);
    let mut request = stone_request(&properties);
    request.item = catalog().item_id("minecraft:oak_log").unwrap();
    let plan = catalog()
        .plan_placement(&floor, &context(), &request)
        .unwrap();
    assert_eq!(
        catalog()
            .state(plan.changes()[0].replacement)
            .unwrap()
            .property("axis"),
        Some("x")
    );
    properties = BTreeMap::from([("waterlogged".into(), "true".into())]);
    request = stone_request(&properties);
    request.item = catalog().item_id("minecraft:oak_slab").unwrap();
    assert!(matches!(
        catalog().plan_placement(&floor, &context(), &request),
        Err(BuildError::UnsupportedProperty)
    ));
}

#[test]
fn build_height_checks_the_destination_and_skips_neighbors_outside_the_dimension() {
    let properties = BTreeMap::new();
    let mut request = stone_request(&properties);
    request.clicked.y = 318;
    let view = |position: Position| {
        if !(-64..=319).contains(&position.y) {
            None
        } else {
            Some(if position.y == 318 {
                state("minecraft:stone", &[])
            } else {
                catalog().air()
            })
        }
    };
    let mut interaction = context();
    interaction.eye = [0.5, 319.0, 2.5];
    let plan = catalog()
        .plan_placement(&view, &interaction, &request)
        .unwrap();
    assert_eq!(plan.changes()[0].position.y, 319);
    request.clicked.y = 319;
    let ceiling = |_| Some(state("minecraft:stone", &[]));
    assert!(matches!(
        catalog().plan_placement(&ceiling, &interaction, &request),
        Err(BuildError::OutOfBounds)
    ));
}

proptest! {
    #[test]
    fn translated_builds_preserve_states_and_guarded_read_sets(dx in -20000i32..20000, dy in -32i32..250, dz in -20000i32..20000) {
        let properties=BTreeMap::new();
        let request=stone_request(&properties);
        let original=catalog().plan_placement(&floor,&context(),&request).unwrap();
        let translate=|position: Position| Position::new(position.x+dx,position.y+dy,position.z+dz);
        let translated_view=|position: Position| floor(Position::new(position.x-dx,position.y-dy,position.z-dz));
        let mut interaction=context();
        interaction.eye=[interaction.eye[0]+f64::from(dx),interaction.eye[1]+f64::from(dy),interaction.eye[2]+f64::from(dz)];
        let translated_request=Placement {clicked:translate(request.clicked),..request};
        let result=catalog().plan_placement(&translated_view,&interaction,&translated_request).unwrap();
        let expected:Vec<_>=original.changes().iter().map(|change|BlockChange {position:translate(change.position),..*change}).collect();
        prop_assert_eq!(result.changes(),expected);
        let reads:Vec<_>=original.reads().iter().map(|(position,state)|(translate(*position),*state)).collect();
        prop_assert_eq!(result.reads(),reads);
    }
}

#[test]
fn secondary_stair_changes_cannot_leave_unsupported_fence_connections_stale() {
    let origin = Position::new(0, 0, 0);
    let floor = Position::new(0, -1, 1);
    let fence = Position::new(-1, 0, 0);
    let view = |position| {
        Some(if position == origin {
            state("minecraft:oak_stairs", &[("facing", "north")])
        } else if position == floor {
            state("minecraft:stone", &[])
        } else if position == fence {
            state("minecraft:oak_fence", &[])
        } else {
            catalog().air()
        })
    };
    let properties = BTreeMap::new();
    let request = Placement {
        clicked: floor,
        face: Direction::Up,
        local_hit: [0.5, 1.0, 0.5],
        facing: Direction::West,
        item: catalog().item_id("minecraft:stone_stairs").unwrap(),
        secondary_use: false,
        properties: &properties,
    };
    assert!(
        matches!(catalog().plan_placement(&view,&context(),&request),Err(BuildError::UnsupportedBlock(position))if position==fence)
    );
}

#[test]
fn stair_geometry_uses_supplied_neighbors_above_the_default_overworld_height() {
    let position = Position::new(0, 400, 0);
    let base = state("minecraft:oak_stairs", &[("facing", "north")]);
    let neighbor = state("minecraft:stone_stairs", &[("facing", "west")]);
    let view = |p| {
        Some(if p == Position::new(0, 400, -1) {
            neighbor
        } else {
            catalog().air()
        })
    };
    assert_eq!(
        catalog().stair_state(&view, position, base).unwrap(),
        state(
            "minecraft:oak_stairs",
            &[("facing", "north"), ("shape", "outer_left")]
        )
    );
}

#[test]
fn debug_stick_mining_cannot_fall_back_to_ordinary_block_destruction() {
    let item = catalog().item_id("minecraft:debug_stick").unwrap();
    assert!(matches!(
        catalog().plan_break(&floor, &context(), Position::new(0, 0, 0), Some(item)),
        Err(BuildError::UnsupportedItem)
    ));
}

#[test]
fn ordinary_building_on_native_grass_updates_snowy_neighbors() {
    let properties = BTreeMap::new();
    for ground in [
        "minecraft:grass_block",
        "minecraft:podzol",
        "minecraft:mycelium",
    ] {
        let old = state(ground, &[("snowy", "true")]);
        let clicked = Position::new(0, 0, 0);
        let view = |p: Position| Some(if p.y == 0 { old } else { catalog().air() });
        let request = stone_request(&properties);
        let plan = catalog()
            .plan_placement(&view, &context(), &request)
            .unwrap();
        assert!(
            plan.changes().contains(&BlockChange {
                position: clicked,
                expected: old,
                replacement: state(ground, &[("snowy", "false")])
            }),
            "{ground}"
        );
        assert!(
            plan.changes()
                .iter()
                .any(|change| change.position == Position::new(0, 1, 0)
                    && change.replacement == state("minecraft:stone", &[]))
        );
    }
}

#[test]
fn snow_block_placement_and_removal_recompute_the_ground_below() {
    let properties = BTreeMap::new();
    let mut request = stone_request(&properties);
    request.item = catalog().item_id("minecraft:snow_block").unwrap();
    let ground = state("minecraft:grass_block", &[]);
    let snowy = state("minecraft:grass_block", &[("snowy", "true")]);
    let view = |p: Position| Some(if p.y == 0 { ground } else { catalog().air() });
    let plan = catalog()
        .plan_placement(&view, &context(), &request)
        .unwrap();
    assert!(plan.changes().contains(&BlockChange {
        position: Position::new(0, 0, 0),
        expected: ground,
        replacement: snowy
    }));
    let after = |p| {
        plan.changes()
            .iter()
            .find(|change| change.position == p)
            .map(|change| change.replacement)
            .or_else(|| view(p))
    };
    let removed = catalog()
        .plan_break(&after, &context(), Position::new(0, 1, 0), None)
        .unwrap();
    assert!(removed.changes().contains(&BlockChange {
        position: Position::new(0, 0, 0),
        expected: snowy,
        replacement: ground
    }));
}

#[test]
fn original_snowy_neighbor_fixtures_match_all_directions_and_tag_members() {
    let fixtures: Vec<(u32, u8, u32, u32)> =
        serde_json::from_str(include_str!("fixtures/snowy-neighbors.json")).unwrap();
    assert_eq!(fixtures.len(), 180);
    for (before, direction, neighbor, expected) in fixtures {
        assert_eq!(
            catalog()
                .snowy_neighbor_state(
                    StateId(before),
                    Direction::from_id(direction).unwrap(),
                    StateId(neighbor)
                )
                .unwrap(),
            StateId(expected)
        );
    }
    let snow = state("minecraft:snow", &[]);
    let stone = state("minecraft:stone", &[]);
    let ground = state("minecraft:podzol", &[]);
    assert_eq!(
        catalog()
            .state(
                catalog()
                    .snowy_neighbor_state(ground, Direction::Up, snow)
                    .unwrap()
            )
            .unwrap()
            .property("snowy"),
        Some("true")
    );
    assert_eq!(
        catalog()
            .state(
                catalog()
                    .snowy_neighbor_state(ground, Direction::Up, stone)
                    .unwrap()
            )
            .unwrap()
            .property("snowy"),
        Some("false")
    );
}

#[test]
fn cosmetic_snow_updates_do_not_reject_existing_entity_overlap() {
    let properties = BTreeMap::new();
    let request = stone_request(&properties);
    let ground = state("minecraft:grass_block", &[("snowy", "true")]);
    let view = |p: Position| Some(if p.y == 0 { ground } else { catalog().air() });
    let blockers = [Aabb {
        min: [0.2, 0.2, 0.2],
        max: [0.8, 0.8, 0.8],
    }];
    let mut interaction = context();
    interaction.blockers = &blockers;
    assert!(
        catalog()
            .plan_placement(&view, &interaction, &request)
            .is_ok()
    );
}

#[test]
fn horizontal_snowy_ground_updates_preserve_existing_flag() {
    let properties = BTreeMap::new();
    let request = stone_request(&properties);
    let neighbor = Position::new(1, 1, 0);
    let ground = state("minecraft:mycelium", &[("snowy", "true")]);
    let view = |p| {
        if p == neighbor {
            Some(ground)
        } else {
            floor(p)
        }
    };
    let plan = catalog()
        .plan_placement(&view, &context(), &request)
        .unwrap();
    assert!(
        !plan
            .changes()
            .iter()
            .any(|change| change.position == neighbor)
    );
}

#[test]
fn placing_snowy_ground_reads_above_and_uses_its_snow_tag() {
    let properties = BTreeMap::new();
    for key in [
        "minecraft:grass_block",
        "minecraft:podzol",
        "minecraft:mycelium",
    ] {
        let above = Position::new(1, 1, 0);
        let target = Position::new(1, 0, 0);
        for (cover, want) in [
            ("minecraft:snow_block", "true"),
            ("minecraft:stone", "false"),
            ("minecraft:air", "false"),
        ] {
            let view = |p| {
                Some(if p == Position::new(0, 0, 0) {
                    state("minecraft:stone", &[])
                } else if p == above {
                    state(cover, &[])
                } else {
                    catalog().air()
                })
            };
            let mut request = stone_request(&properties);
            request.face = Direction::East;
            request.item = catalog().item_id(key).unwrap();
            let plan = catalog()
                .plan_placement(&view, &context(), &request)
                .unwrap();
            assert!(plan.changes().iter().any(|change| change.position == target
                && change.replacement == state(key, &[("snowy", want)])));
            assert!(plan.reads().contains(&(above, state(cover, &[]))));
        }
        let missing = |position| {
            if position == above {
                None
            } else {
                Some(if position == Position::new(0, 0, 0) {
                    state("minecraft:stone", &[])
                } else {
                    catalog().air()
                })
            }
        };
        let mut request = stone_request(&properties);
        request.face = Direction::East;
        request.item = catalog().item_id(key).unwrap();
        assert!(
            matches!(catalog().plan_placement(&missing,&context(),&request),Err(BuildError::Missing(position))if position==above)
        );
    }
}
