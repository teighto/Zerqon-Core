use std::collections::BTreeMap;
use zerqon_blocks::{Bounds, Catalog, Direction, Interaction, Placement, Position, StateId};

#[test]
fn normal_building_neighbors_doors_torches_and_waterlogged_shapes() {
    let data = Catalog::embedded().unwrap();
    let air = data.air();
    let at = Position::new(0, 64, 0);
    let state = |key: &str| data.resolve(key, &BTreeMap::new()).unwrap();
    let mut world = BTreeMap::from([
        (Position::new(0, 63, 0), state("minecraft:stone")),
        (Position::new(1, 64, 0), state("minecraft:water")),
        (Position::new(-1, 64, 0), state("minecraft:chest")),
    ]);
    let apply = |world: &mut BTreeMap<Position, StateId>, plan: zerqon_blocks::EditPlan| {
        for change in plan.changes() {
            world.insert(change.position, change.replacement);
        }
    };
    let context = Interaction {
        eye: [0.5, 65.62, 3.5],
        block_range: 5.0,
        creative: false,
        bounds: Bounds::default(),
        blockers: &[],
    };
    let properties = BTreeMap::new();
    let mut request = Placement {
        clicked: Position::new(0, 63, 0),
        face: Direction::Up,
        local_hit: [0.5, 1.0, 0.5],
        facing: Direction::North,
        item: data.item_id("minecraft:stone").unwrap(),
        secondary_use: false,
        properties: &properties,
    };
    let plan = data
        .plan_placement(
            &|p| Some(*world.get(&p).unwrap_or(&air)),
            &context,
            &request,
        )
        .unwrap();
    assert_eq!(plan.changes().len(), 1);
    apply(&mut world, plan);
    assert_eq!(world[&at], state("minecraft:stone"));
    world.insert(at, air);
    request.item = data.item_id("minecraft:oak_door").unwrap();
    let plan = data
        .plan_placement(
            &|p| Some(*world.get(&p).unwrap_or(&air)),
            &context,
            &request,
        )
        .unwrap();
    assert_eq!(plan.changes().len(), 2);
    apply(&mut world, plan);
    assert_eq!(
        data.state(world[&at]).unwrap().property("half"),
        Some("lower")
    );
    let upper = at.relative(Direction::Up).unwrap();
    assert_eq!(
        data.state(world[&upper]).unwrap().property("half"),
        Some("upper")
    );
    let plan = data
        .plan_use(&|p| Some(*world.get(&p).unwrap_or(&air)), &context, upper)
        .unwrap()
        .unwrap();
    apply(&mut world, plan);
    assert_eq!(
        data.state(world[&at]).unwrap().property("open"),
        Some("true")
    );
    assert_eq!(
        data.state(world[&upper]).unwrap().property("open"),
        Some("true")
    );
    let plan = data
        .plan_break(
            &|p| Some(*world.get(&p).unwrap_or(&air)),
            &context,
            at,
            None,
        )
        .unwrap();
    assert_eq!(plan.changes().len(), 2);
    apply(&mut world, plan);
    request.item = data.item_id("minecraft:torch").unwrap();
    let plan = data
        .plan_placement(
            &|p| Some(*world.get(&p).unwrap_or(&air)),
            &context,
            &request,
        )
        .unwrap();
    apply(&mut world, plan);
    assert_eq!(world[&at], state("minecraft:torch"));
    world.insert(at, state("minecraft:water"));
    request.item = data.item_id("minecraft:oak_slab").unwrap();
    let plan = data
        .plan_placement(
            &|p| Some(*world.get(&p).unwrap_or(&air)),
            &context,
            &request,
        )
        .unwrap();
    apply(&mut world, plan);
    assert_eq!(
        data.state(world[&at]).unwrap().property("waterlogged"),
        Some("true")
    );
    let plan = data
        .plan_break(
            &|p| Some(*world.get(&p).unwrap_or(&air)),
            &context,
            at,
            None,
        )
        .unwrap();
    apply(&mut world, plan);
    assert_eq!(world[&at], state("minecraft:water"));
}
