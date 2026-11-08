use proptest::prelude::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use zerqon_inventory::*;

fn catalog() -> ItemCatalog {
    ItemCatalog::from_definitions(
        [
            "air",
            "oak_log",
            "oak_planks",
            "stick",
            "stone",
            "coal",
            "honey_bottle",
            "honey_block",
            "glass_bottle",
            "paper",
        ]
        .iter()
        .enumerate()
        .map(|(index, name)| ItemDef {
            id: ItemId(index as u32),
            key: format!("minecraft:{name}"),
            max_stack: if *name == "honey_bottle" { 16 } else { 64 },
            max_damage: None,
        })
        .collect(),
        vec![ComponentDef {
            id: ComponentId(0),
            key: "minecraft:custom_name".into(),
        }],
    )
    .unwrap()
}

fn stack(catalog: &ItemCatalog, name: &str, count: u8) -> Slot {
    Some(
        ItemStack::new(
            catalog.item_id(&format!("minecraft:{name}")).unwrap(),
            count,
            ValidatedPatch::default(),
        )
        .unwrap(),
    )
}

fn recipes(catalog: &ItemCatalog) -> CraftingRecipes {
    CraftingRecipes::from_json(catalog, &BTreeMap::from([
        ("minecraft:oak_planks".into(), json!({"type":"minecraft:crafting_shapeless","ingredients":["minecraft:oak_log"],"result":{"id":"minecraft:oak_planks","count":4}})),
        ("minecraft:stick".into(), json!({"type":"minecraft:crafting_shaped","key":{"#":"minecraft:oak_planks"},"pattern":["#","#"],"result":{"id":"minecraft:stick","count":4}})),
        ("minecraft:honey_block".into(), json!({"type":"minecraft:crafting_shaped","key":{"#":"minecraft:honey_bottle"},"pattern":["##","##"],"result":{"id":"minecraft:honey_block"}})),
    ]), &BTreeMap::new()).unwrap()
}

fn remainders(catalog: &ItemCatalog) -> CraftingRemainders {
    CraftingRemainders::from_definitions(
        catalog,
        BTreeMap::from([(
            catalog.item_id("minecraft:honey_bottle").unwrap(),
            stack(catalog, "glass_bottle", 1).unwrap(),
        )]),
    )
    .unwrap()
}

fn policy(allow_drops: bool) -> InventoryPolicy {
    InventoryPolicy {
        creative: false,
        can_interact: true,
        allow_drops,
    }
}

fn click(mode: ClickMode, slot: i16, button: i8) -> ClickRequest {
    ClickRequest {
        container_id: 0,
        state_id: 0,
        slot,
        button,
        mode,
        changed_slots: Vec::new(),
        carried: HashedStack::Empty,
    }
}

fn menu(catalog: &ItemCatalog, recipes: &CraftingRecipes, grid: [Slot; 4]) -> PlayerInventory {
    let mut inventory = PlayerInventory::new();
    for (index, slot) in grid.into_iter().enumerate() {
        inventory.set_slot(catalog, index as i16 + 1, slot).unwrap();
    }
    inventory.refresh_crafting(catalog, recipes).unwrap();
    inventory
}

fn fill_main_inventory(inventory: &mut PlayerInventory, catalog: &ItemCatalog) {
    for slot in 9..=44 {
        inventory
            .set_slot(catalog, slot, stack(catalog, "stone", 64))
            .unwrap();
    }
}

fn item_total(inventory: &PlayerInventory, drops: &[ItemStack], item: ItemId) -> u32 {
    inventory
        .slots()
        .iter()
        .skip(1)
        .chain(std::iter::once(inventory.cursor()))
        .flatten()
        .chain(drops.iter())
        .filter(|stack| stack.item() == item)
        .map(|stack| u32::from(stack.count()))
        .sum()
}

#[test]
fn left_and_right_result_pickup_each_take_one_whole_batch() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let remainders = remainders(&catalog);
    for button in [0, 1] {
        let mut inventory = menu(
            &catalog,
            &recipes,
            [stack(&catalog, "oak_log", 2), None, None, None],
        );
        let revision = inventory.state_id();
        let effects = inventory
            .click_with_crafting(
                &catalog,
                &click(ClickMode::Pickup, 0, button),
                policy(true),
                &recipes,
                &remainders,
            )
            .unwrap();
        assert_eq!(inventory.cursor(), &stack(&catalog, "oak_planks", 4));
        assert_eq!(inventory.slots()[1], stack(&catalog, "oak_log", 1));
        assert_eq!(inventory.slots()[0], stack(&catalog, "oak_planks", 4));
        assert!(effects.drops.is_empty());
        assert_eq!(effects.state_id, (revision + 1) & 32767);
    }
}

#[test]
fn cursor_must_have_room_for_the_entire_result_batch() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let remainders = remainders(&catalog);
    for (before, after, logs) in [(60, 64, 1), (62, 62, 2), (64, 64, 2)] {
        let mut inventory = menu(
            &catalog,
            &recipes,
            [stack(&catalog, "oak_log", 2), None, None, None],
        );
        inventory
            .set_cursor(&catalog, stack(&catalog, "oak_planks", before))
            .unwrap();
        inventory
            .click_with_crafting(
                &catalog,
                &click(ClickMode::Pickup, 0, 0),
                policy(true),
                &recipes,
                &remainders,
            )
            .unwrap();
        assert_eq!(inventory.cursor(), &stack(&catalog, "oak_planks", after));
        assert_eq!(inventory.slots()[1], stack(&catalog, "oak_log", logs));
    }
}

// These additional guards were checked against the pinned original server in
// .cache/crafting-menu-oracle/probe-td9z8qlq/trace.json. They are separate from
// the frozen 34-case differential corpus below.
#[test]
fn occupied_cursor_blocks_result_clone_and_throw_but_allows_hotbar_swap() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let remainders = remainders(&catalog);
    for (mode, button, creative, remaining_logs, hotbar_count) in [
        (ClickMode::Clone, 2, true, 2, None),
        (ClickMode::Throw, 0, false, 2, None),
        (ClickMode::Throw, 1, false, 2, None),
        (ClickMode::Swap, 0, false, 1, Some(4)),
    ] {
        let mut inventory = menu(
            &catalog,
            &recipes,
            [stack(&catalog, "oak_log", 2), None, None, None],
        );
        inventory
            .set_cursor(&catalog, stack(&catalog, "stone", 1))
            .unwrap();
        let effects = inventory
            .click_with_crafting(
                &catalog,
                &click(mode, 0, button),
                InventoryPolicy {
                    creative,
                    ..policy(true)
                },
                &recipes,
                &remainders,
            )
            .unwrap();
        assert_eq!(inventory.cursor(), &stack(&catalog, "stone", 1), "{mode:?}");
        assert_eq!(
            inventory.slots()[1],
            stack(&catalog, "oak_log", remaining_logs),
            "{mode:?}"
        );
        assert_eq!(
            inventory.slots()[0],
            stack(&catalog, "oak_planks", 4),
            "{mode:?}"
        );
        assert_eq!(
            inventory.slots()[36],
            hotbar_count.and_then(|count| stack(&catalog, "oak_planks", count)),
            "{mode:?}"
        );
        assert!(effects.drops.is_empty(), "{mode:?}");
    }
}

#[test]
fn pickup_all_on_an_empty_result_collects_real_grid_and_inventory_items() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let remainders = remainders(&catalog);
    let mut inventory = menu(
        &catalog,
        &recipes,
        [
            stack(&catalog, "oak_planks", 3),
            stack(&catalog, "paper", 1),
            None,
            None,
        ],
    );
    inventory
        .set_slot(&catalog, 10, stack(&catalog, "oak_planks", 7))
        .unwrap();
    inventory
        .set_cursor(&catalog, stack(&catalog, "oak_planks", 1))
        .unwrap();
    assert!(inventory.slots()[0].is_none());
    let effects = inventory
        .click_with_crafting(
            &catalog,
            &click(ClickMode::PickupAll, 0, 0),
            policy(true),
            &recipes,
            &remainders,
        )
        .unwrap();
    assert_eq!(inventory.cursor(), &stack(&catalog, "oak_planks", 11));
    assert_eq!(inventory.slots()[2], stack(&catalog, "paper", 1));
    assert!(inventory.slots()[0].is_none());
    assert!(inventory.slots()[1].is_none());
    assert!(inventory.slots()[10].is_none());
    assert!(effects.drops.is_empty());
}

#[test]
fn quick_move_of_a_grid_input_uses_main_inventory_and_clears_preview() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let remainders = remainders(&catalog);
    let mut inventory = menu(
        &catalog,
        &recipes,
        [stack(&catalog, "oak_log", 2), None, None, None],
    );
    let effects = inventory
        .click_with_crafting(
            &catalog,
            &click(ClickMode::QuickMove, 1, 0),
            policy(true),
            &recipes,
            &remainders,
        )
        .unwrap();
    let mut expected: [Slot; MENU_SLOT_COUNT] = std::array::from_fn(|_| None);
    expected[9] = stack(&catalog, "oak_log", 2);
    assert_eq!(inventory.slots(), &expected);
    assert!(inventory.cursor().is_none());
    assert!(effects.drops.is_empty());
}

#[test]
fn remainders_merge_other_hotbar_stacks_before_main_after_priority_slots_are_full() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let remainders = remainders(&catalog);
    let mut inventory = menu(
        &catalog,
        &recipes,
        std::array::from_fn(|_| stack(&catalog, "honey_bottle", 2)),
    );
    inventory.select_hotbar(2).unwrap();
    for (slot, count) in [(38, 64), (45, 64), (36, 60), (9, 60)] {
        inventory
            .set_slot(&catalog, slot, stack(&catalog, "glass_bottle", count))
            .unwrap();
    }
    let effects = inventory
        .click_with_crafting(
            &catalog,
            &click(ClickMode::Pickup, 0, 0),
            policy(true),
            &recipes,
            &remainders,
        )
        .unwrap();
    assert_eq!(inventory.slots()[36], stack(&catalog, "glass_bottle", 64));
    assert_eq!(inventory.slots()[9], stack(&catalog, "glass_bottle", 60));
    assert_eq!(inventory.slots()[38], stack(&catalog, "glass_bottle", 64));
    assert_eq!(inventory.slots()[45], stack(&catalog, "glass_bottle", 64));
    assert_eq!(inventory.cursor(), &stack(&catalog, "honey_block", 1));
    for slot in 1..=4 {
        assert_eq!(inventory.slots()[slot], stack(&catalog, "honey_bottle", 1));
    }
    assert_eq!(inventory.slots()[0], stack(&catalog, "honey_block", 1));
    assert!(effects.drops.is_empty());
}

#[test]
fn partial_shift_capacity_places_two_drops_two_and_consumes_one_input() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let remainders = remainders(&catalog);
    let mut inventory = menu(
        &catalog,
        &recipes,
        [stack(&catalog, "oak_log", 2), None, None, None],
    );
    fill_main_inventory(&mut inventory, &catalog);
    inventory
        .set_slot(&catalog, 44, stack(&catalog, "oak_planks", 62))
        .unwrap();
    let effects = inventory
        .click_with_crafting(
            &catalog,
            &click(ClickMode::QuickMove, 0, 0),
            policy(true),
            &recipes,
            &remainders,
        )
        .unwrap();
    assert_eq!(inventory.slots()[44], stack(&catalog, "oak_planks", 64));
    assert_eq!(inventory.slots()[1], stack(&catalog, "oak_log", 1));
    assert_eq!(inventory.slots()[0], stack(&catalog, "oak_planks", 4));
    assert_eq!(
        effects.drops,
        vec![stack(&catalog, "oak_planks", 2).unwrap()]
    );
}

#[test]
fn forbidden_partial_shift_drop_rolls_back_the_entire_snapshot() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let remainders = remainders(&catalog);
    let mut inventory = menu(
        &catalog,
        &recipes,
        [stack(&catalog, "oak_log", 2), None, None, None],
    );
    fill_main_inventory(&mut inventory, &catalog);
    inventory
        .set_slot(&catalog, 44, stack(&catalog, "oak_planks", 62))
        .unwrap();
    let before = inventory.clone();
    assert!(matches!(
        inventory.click_with_crafting(
            &catalog,
            &click(ClickMode::QuickMove, 0, 0),
            policy(false),
            &recipes,
            &remainders
        ),
        Err(InventoryError::DroppingUnavailable)
    ));
    assert_eq!(inventory, before);
}

#[test]
fn forbidden_remainder_drop_rolls_back_result_and_inputs() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let remainders = remainders(&catalog);
    let mut inventory = menu(
        &catalog,
        &recipes,
        std::array::from_fn(|_| stack(&catalog, "honey_bottle", 2)),
    );
    fill_main_inventory(&mut inventory, &catalog);
    let before = inventory.clone();
    assert!(matches!(
        inventory.click_with_crafting(
            &catalog,
            &click(ClickMode::Pickup, 0, 0),
            policy(false),
            &recipes,
            &remainders
        ),
        Err(InventoryError::DroppingUnavailable)
    ));
    assert_eq!(inventory, before);
}

#[test]
fn a_late_shift_drop_rolls_back_the_already_completed_first_batch() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let remainders = remainders(&catalog);
    let mut inventory = menu(
        &catalog,
        &recipes,
        [stack(&catalog, "oak_log", 3), None, None, None],
    );
    fill_main_inventory(&mut inventory, &catalog);
    inventory
        .set_slot(&catalog, 44, stack(&catalog, "oak_planks", 58))
        .unwrap();
    let before = inventory.clone();
    assert!(matches!(
        inventory.click_with_crafting(
            &catalog,
            &click(ClickMode::QuickMove, 0, 0),
            policy(false),
            &recipes,
            &remainders
        ),
        Err(InventoryError::DroppingUnavailable)
    ));
    assert_eq!(inventory, before);
}

#[test]
fn a_late_remainder_failure_rolls_back_all_prior_shift_batches() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let remainders = remainders(&catalog);
    let mut inventory = menu(
        &catalog,
        &recipes,
        std::array::from_fn(|_| stack(&catalog, "honey_bottle", 3)),
    );
    fill_main_inventory(&mut inventory, &catalog);
    inventory
        .set_slot(&catalog, 44, stack(&catalog, "honey_block", 62))
        .unwrap();
    inventory
        .set_slot(&catalog, 36, stack(&catalog, "glass_bottle", 60))
        .unwrap();
    let before = inventory.clone();
    assert!(matches!(
        inventory.click_with_crafting(
            &catalog,
            &click(ClickMode::QuickMove, 0, 0),
            policy(false),
            &recipes,
            &remainders
        ),
        Err(InventoryError::DroppingUnavailable)
    ));
    assert_eq!(inventory, before);
}

#[test]
fn synthetic_self_returning_remainders_cannot_make_repeating_craft_unbounded() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    // Deliberately synthetic definitions exercise the public constructor's
    // bounded menu contract; no vanilla item has this remainder mapping.
    let remainders = CraftingRemainders::from_definitions(
        &catalog,
        BTreeMap::from([(
            catalog.item_id("minecraft:oak_log").unwrap(),
            stack(&catalog, "oak_log", 1).unwrap(),
        )]),
    )
    .unwrap();
    for request in [
        click(ClickMode::QuickMove, 0, 0),
        click(ClickMode::Throw, 0, 1),
    ] {
        let mut inventory = menu(
            &catalog,
            &recipes,
            [stack(&catalog, "oak_log", 2), None, None, None],
        );
        let before = inventory.clone();
        assert!(matches!(
            inventory.click_with_crafting(&catalog, &request, policy(true), &recipes, &remainders),
            Err(InventoryError::CraftingLimit)
        ));
        assert_eq!(inventory, before);
    }
}

#[test]
fn rejected_permissions_and_packet_metadata_preserve_an_active_drag() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let remainders = remainders(&catalog);
    let mut inventory = PlayerInventory::new();
    inventory
        .set_cursor(&catalog, stack(&catalog, "oak_planks", 4))
        .unwrap();
    inventory
        .click_with_crafting(
            &catalog,
            &click(ClickMode::QuickCraft, -999, 0),
            policy(true),
            &recipes,
            &remainders,
        )
        .unwrap();
    inventory
        .click_with_crafting(
            &catalog,
            &click(ClickMode::QuickCraft, 1, 1),
            policy(true),
            &recipes,
            &remainders,
        )
        .unwrap();
    let before = inventory.clone();
    let ordinary = click(ClickMode::Pickup, 0, 0);
    let mut cases = vec![(
        ordinary.clone(),
        InventoryPolicy {
            can_interact: false,
            ..policy(true)
        },
    )];
    let mut wrong_container = ordinary.clone();
    wrong_container.container_id = 1;
    cases.push((wrong_container, policy(true)));
    let mut wrong_state = ordinary.clone();
    wrong_state.state_id = -1;
    cases.push((wrong_state, policy(true)));
    let mut duplicate_prediction = ordinary.clone();
    duplicate_prediction.changed_slots = vec![(1, HashedStack::Empty), (1, HashedStack::Empty)];
    cases.push((duplicate_prediction, policy(true)));
    let mut bad_prediction = ordinary;
    bad_prediction.changed_slots = vec![(46, HashedStack::Empty)];
    cases.push((bad_prediction, policy(true)));
    cases.push((click(ClickMode::Pickup, 0, 2), policy(true)));
    for (request, policy) in cases {
        assert!(
            inventory
                .click_with_crafting(&catalog, &request, policy, &recipes, &remainders)
                .is_err()
        );
        assert_eq!(inventory, before);
    }
    inventory
        .click_with_crafting(
            &catalog,
            &click(ClickMode::QuickCraft, 3, 1),
            policy(true),
            &recipes,
            &remainders,
        )
        .unwrap();
    inventory
        .click_with_crafting(
            &catalog,
            &click(ClickMode::QuickCraft, -999, 2),
            policy(true),
            &recipes,
            &remainders,
        )
        .unwrap();
    assert_eq!(inventory.slots()[1], stack(&catalog, "oak_planks", 2));
    assert_eq!(inventory.slots()[3], stack(&catalog, "oak_planks", 2));
    assert_eq!(inventory.slots()[0], stack(&catalog, "stick", 4));
    assert!(inventory.cursor().is_none());
}

#[test]
fn remainder_definitions_reject_unknown_sources_outputs_and_oversized_stacks() {
    let catalog = catalog();
    let honey = catalog.item_id("minecraft:honey_bottle").unwrap();
    for definitions in [
        BTreeMap::from([(ItemId(999), stack(&catalog, "glass_bottle", 1).unwrap())]),
        BTreeMap::from([(ItemId(0), stack(&catalog, "glass_bottle", 1).unwrap())]),
        BTreeMap::from([(
            honey,
            ItemStack::new(ItemId(999), 1, ValidatedPatch::default()).unwrap(),
        )]),
        BTreeMap::from([(honey, stack(&catalog, "glass_bottle", 65).unwrap())]),
    ] {
        assert!(CraftingRemainders::from_definitions(&catalog, definitions).is_err());
    }
}

#[test]
fn preview_and_predictions_cannot_supply_authoritative_result_items() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let remainders = remainders(&catalog);
    let mut inventory = PlayerInventory::new();
    inventory
        .set_slot(&catalog, 1, stack(&catalog, "oak_log", 2))
        .unwrap();
    inventory
        .set_slot(&catalog, 0, stack(&catalog, "stone", 64))
        .unwrap();
    let forged = HashedStack::Item {
        item: catalog.item_id("minecraft:stone").unwrap(),
        count: 64,
        added: BTreeMap::new(),
        removed: BTreeSet::new(),
    };
    let mut request = click(ClickMode::Pickup, 0, 0);
    request.changed_slots.push((0, forged.clone()));
    request.carried = forged;
    let effects = inventory
        .click_with_crafting(&catalog, &request, policy(true), &recipes, &remainders)
        .unwrap();
    assert_eq!(inventory.cursor(), &stack(&catalog, "oak_planks", 4));
    assert_eq!(inventory.slots()[1], stack(&catalog, "oak_log", 1));
    assert_eq!(inventory.slots()[0], stack(&catalog, "oak_planks", 4));
    assert_eq!(
        item_total(
            &inventory,
            &effects.drops,
            catalog.item_id("minecraft:stone").unwrap()
        ),
        0
    );
}

#[test]
fn refresh_discards_a_stale_virtual_result_without_returning_it() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let mut inventory = PlayerInventory::new();
    inventory
        .set_slot(&catalog, 0, stack(&catalog, "stone", 64))
        .unwrap();
    let effects = inventory.refresh_crafting(&catalog, &recipes).unwrap();
    assert!(inventory.slots().iter().all(Option::is_none));
    assert!(inventory.cursor().is_none());
    assert!(effects.drops.is_empty());
}

#[test]
fn creative_grid_edits_refresh_preview_but_result_slot_remains_read_only() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let creative = InventoryPolicy {
        creative: true,
        ..policy(true)
    };
    let mut inventory = PlayerInventory::new();
    let revision = inventory.state_id();
    inventory
        .creative_set_with_crafting(
            &catalog,
            &CreativeSlotRequest {
                slot: 1,
                stack: stack(&catalog, "oak_log", 2),
            },
            creative,
            &recipes,
        )
        .unwrap();
    assert_eq!(inventory.slots()[0], stack(&catalog, "oak_planks", 4));
    assert_eq!(inventory.state_id(), (revision + 1) & 32767);
    let before = inventory.clone();
    assert!(
        inventory
            .creative_set_with_crafting(
                &catalog,
                &CreativeSlotRequest {
                    slot: 0,
                    stack: stack(&catalog, "stone", 64)
                },
                creative,
                &recipes
            )
            .is_err()
    );
    assert_eq!(inventory, before);
    inventory
        .creative_set_with_crafting(
            &catalog,
            &CreativeSlotRequest {
                slot: 1,
                stack: None,
            },
            creative,
            &recipes,
        )
        .unwrap();
    assert!(inventory.slots()[0].is_none());
}

#[test]
fn repeated_stale_state_consumes_a_new_batch_each_time_and_never_mints_after_exhaustion() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let remainders = remainders(&catalog);
    let mut inventory = menu(
        &catalog,
        &recipes,
        [stack(&catalog, "oak_log", 2), None, None, None],
    );
    let mut request = click(ClickMode::Pickup, 0, 0);
    request.state_id = i32::from(inventory.state_id());
    for expected in [4, 8, 8] {
        inventory
            .click_with_crafting(&catalog, &request, policy(true), &recipes, &remainders)
            .unwrap();
        assert_eq!(inventory.cursor(), &stack(&catalog, "oak_planks", expected));
    }
    assert!(inventory.slots()[0].is_none());
    assert!(inventory.slots()[1].is_none());
}

#[test]
fn grid_pickup_and_drag_refresh_the_derived_result() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let remainders = remainders(&catalog);
    let mut inventory = PlayerInventory::new();
    inventory
        .set_cursor(&catalog, stack(&catalog, "oak_planks", 4))
        .unwrap();
    for (slot, button) in [(-999, 0), (1, 1), (3, 1), (-999, 2)] {
        inventory
            .click_with_crafting(
                &catalog,
                &click(ClickMode::QuickCraft, slot, button),
                policy(true),
                &recipes,
                &remainders,
            )
            .unwrap();
    }
    assert_eq!(inventory.slots()[1], stack(&catalog, "oak_planks", 2));
    assert_eq!(inventory.slots()[3], stack(&catalog, "oak_planks", 2));
    assert_eq!(inventory.slots()[0], stack(&catalog, "stick", 4));
    assert!(inventory.cursor().is_none());
    inventory
        .click_with_crafting(
            &catalog,
            &click(ClickMode::Pickup, 1, 0),
            policy(true),
            &recipes,
            &remainders,
        )
        .unwrap();
    assert_eq!(inventory.cursor(), &stack(&catalog, "oak_planks", 2));
    assert!(inventory.slots()[0].is_none());
}

#[test]
fn consuming_a_named_input_preserves_the_remaining_input_patch() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let remainders = remainders(&catalog);
    let patch = ValidatedPatch::new(
        BTreeMap::from([(
            ComponentId(0),
            ComponentValue::CustomName("Saved log".into()),
        )]),
        BTreeSet::new(),
        &catalog,
    )
    .unwrap();
    let named = ItemStack::new(catalog.item_id("minecraft:oak_log").unwrap(), 2, patch).unwrap();
    let mut inventory = menu(&catalog, &recipes, [Some(named.clone()), None, None, None]);
    inventory
        .click_with_crafting(
            &catalog,
            &click(ClickMode::Pickup, 0, 0),
            policy(true),
            &recipes,
            &remainders,
        )
        .unwrap();
    assert_eq!(inventory.slots()[1], Some(named.with_count(1).unwrap()));
    assert_eq!(inventory.cursor(), &stack(&catalog, "oak_planks", 4));
}

#[test]
fn close_with_forbidden_drops_is_atomic_and_with_drops_never_returns_virtual_output() {
    let catalog = catalog();
    let recipes = recipes(&catalog);
    let mut inventory = menu(
        &catalog,
        &recipes,
        [stack(&catalog, "oak_log", 2), None, None, None],
    );
    fill_main_inventory(&mut inventory, &catalog);
    inventory
        .set_cursor(&catalog, stack(&catalog, "stone", 3))
        .unwrap();
    let before = inventory.clone();
    assert!(matches!(
        inventory.close_crafting(&catalog, policy(false)),
        Err(InventoryError::DroppingUnavailable)
    ));
    assert_eq!(inventory, before);
    let effects = inventory.close_crafting(&catalog, policy(true)).unwrap();
    assert!(inventory.slots()[..5].iter().all(Option::is_none));
    assert!(inventory.cursor().is_none());
    assert_eq!(
        item_total(
            &inventory,
            &effects.drops,
            catalog.item_id("minecraft:oak_log").unwrap()
        ),
        2
    );
    assert_eq!(
        item_total(
            &inventory,
            &effects.drops,
            catalog.item_id("minecraft:oak_planks").unwrap()
        ),
        0
    );
    assert_eq!(
        item_total(
            &inventory,
            &effects.drops,
            catalog.item_id("minecraft:stone").unwrap()
        ),
        36 * 64 + 3
    );
}

proptest! {
    #[test]
    fn shift_crafting_conserves_recipe_weight_across_partial_capacity_and_drops(
        logs in 1u8..=64,
        room in 0u8..=64,
    ) {
        let catalog = catalog();
        let recipes = recipes(&catalog);
        let remainders = remainders(&catalog);
        let log_id = catalog.item_id("minecraft:oak_log").unwrap();
        let plank_id = catalog.item_id("minecraft:oak_planks").unwrap();
        let mut inventory = menu(&catalog,&recipes,[stack(&catalog,"oak_log",logs),None,None,None]);
        fill_main_inventory(&mut inventory,&catalog);
        inventory.set_slot(&catalog,44,if room==64 {None} else {stack(&catalog,"oak_planks",64-room)}).unwrap();
        let original_weight = 4*item_total(&inventory,&[],log_id)+item_total(&inventory,&[],plank_id);
        let effects = inventory.click_with_crafting(&catalog,&click(ClickMode::QuickMove,0,0),policy(true),&recipes,&remainders).unwrap();
        prop_assert_eq!(4*item_total(&inventory,&effects.drops,log_id)+item_total(&inventory,&effects.drops,plank_id),original_weight);
        for stack in inventory.slots().iter().chain(std::iter::once(inventory.cursor())).flatten().chain(effects.drops.iter()) {
            prop_assert!(catalog.validate_stack(stack).is_ok());
        }
    }

    #[test]
    fn repeated_stick_crafting_conserves_recipe_weight_without_counting_virtual_output(
        ingredients in 1u8..=64,
        buttons in prop::collection::vec(0i8..=1,0..80),
    ) {
        let catalog = catalog();
        let recipes = recipes(&catalog);
        let remainders = remainders(&catalog);
        let planks = catalog.item_id("minecraft:oak_planks").unwrap();
        let sticks = catalog.item_id("minecraft:stick").unwrap();
        let mut inventory = menu(&catalog,&recipes,[stack(&catalog,"oak_planks",ingredients),None,stack(&catalog,"oak_planks",ingredients),None]);
        let initial_weight = 4*u32::from(ingredients);
        for button in buttons {
            let effects = inventory.click_with_crafting(&catalog,&click(ClickMode::Pickup,0,button),policy(false),&recipes,&remainders).unwrap();
            prop_assert!(effects.drops.is_empty());
            let remaining_weight = 2*item_total(&inventory,&[],planks)+item_total(&inventory,&[],sticks);
            prop_assert_eq!(remaining_weight,initial_weight);
            for stack in inventory.slots().iter().chain(std::iter::once(inventory.cursor())).flatten() {
                prop_assert!(catalog.validate_stack(stack).is_ok());
            }
        }
    }

    #[test]
    fn honey_crafting_preserves_contents_and_containers_or_rolls_back_atomically(
        amounts in prop::array::uniform4(1u8..=16),
        glass_count in 1u8..=64,
        allow_drops in any::<bool>(),
    ) {
        let catalog = catalog();
        let recipes = recipes(&catalog);
        let remainders = remainders(&catalog);
        let mut inventory = menu(&catalog,&recipes,amounts.map(|count|stack(&catalog,"honey_bottle",count)));
        fill_main_inventory(&mut inventory,&catalog);
        inventory.set_slot(&catalog,36,stack(&catalog,"glass_bottle",glass_count)).unwrap();
        let before = inventory.clone();
        let bottles = catalog.item_id("minecraft:honey_bottle").unwrap();
        let blocks = catalog.item_id("minecraft:honey_block").unwrap();
        let glasses = catalog.item_id("minecraft:glass_bottle").unwrap();
        let original_honey = item_total(&before,&[],bottles);
        let original_containers = original_honey+u32::from(glass_count);
        match inventory.click_with_crafting(&catalog,&click(ClickMode::Pickup,0,0),policy(allow_drops),&recipes,&remainders) {
            Ok(effects) => {
                prop_assert_eq!(item_total(&inventory,&effects.drops,bottles)+4*item_total(&inventory,&effects.drops,blocks),original_honey);
                prop_assert_eq!(item_total(&inventory,&effects.drops,bottles)+item_total(&inventory,&effects.drops,glasses),original_containers);
                prop_assert_eq!(item_total(&inventory,&effects.drops,blocks),1);
                if !allow_drops { prop_assert!(effects.drops.is_empty()); }
            }
            Err(InventoryError::DroppingUnavailable) if !allow_drops => prop_assert_eq!(inventory,before),
            other => prop_assert!(false,"unexpected craft outcome: {other:?}"),
        }
    }
}

#[test]
fn personal_crafting_matches_every_original_server_menu_trace() {
    let oracle: Value = serde_json::from_str(include_str!(
        "../../../tools/crafting-menu-oracle/oracle.json"
    ))
    .unwrap();
    assert_eq!(oracle["minecraft"], "26.3");
    assert_eq!(oracle["protocol"], 777);
    assert_eq!(oracle["method"], "original_server_tcp");
    let recipe_oracle: Value =
        serde_json::from_str(include_str!("../../../tools/crafting-data/oracle.json")).unwrap();
    assert_eq!(oracle["server_sha256"], recipe_oracle["server_sha256"]);
    assert_eq!(oracle["bundle_sha1"], recipe_oracle["bundle_sha1"]);
    let catalog = original_catalog(&oracle, &recipe_oracle);
    let raw_recipes = recipe_oracle["recipes"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(id, value)| (id.clone(), value.clone()))
        .collect();
    let tags = recipe_oracle["tags"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(id, value)| (id.clone(), json!({"values":value})))
        .collect();
    let recipes = CraftingRecipes::from_json(&catalog, &raw_recipes, &tags).unwrap();
    let remainder_facts: Value =
        serde_json::from_str(include_str!("../../../tools/crafting-data/26.3.json")).unwrap();
    assert_eq!(oracle["server_sha256"], remainder_facts["server_sha256"]);
    let definitions = remainder_facts["remainders"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| {
            let source = catalog.item_id(row[0].as_str().unwrap())?;
            let target = catalog
                .item_id(row[1].as_str().unwrap())
                .expect("reference catalog must include every applicable remainder");
            let count = u8::try_from(row[2].as_u64().unwrap()).unwrap();
            Some((
                source,
                ItemStack::new(target, count, ValidatedPatch::default()).unwrap(),
            ))
        })
        .collect();
    let remainders = CraftingRemainders::from_definitions(&catalog, definitions).unwrap();
    let cases = oracle["cases"].as_array().unwrap();
    assert!(
        cases.len() >= 25,
        "original corpus unexpectedly lost its baseline traces"
    );
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let creative = match case["gamemode"].as_str().unwrap() {
            "survival" => false,
            "creative" => true,
            other => panic!("unexpected original mode {other}"),
        };
        let mut inventory = PlayerInventory::new();
        let slots = case["before"]["slots"].as_array().unwrap();
        assert_eq!(slots.len(), MENU_SLOT_COUNT, "{name}");
        for (index, slot) in slots.iter().enumerate().skip(1) {
            inventory
                .set_slot(&catalog, index as i16, original_stack(slot, &catalog))
                .unwrap();
        }
        inventory
            .set_cursor(
                &catalog,
                original_stack(&case["before"]["cursor"], &catalog),
            )
            .unwrap();
        if let Some(selected) = case["before"]["selected"].as_u64() {
            inventory
                .select_hotbar(i16::try_from(selected).unwrap())
                .unwrap();
        }
        inventory.refresh_crafting(&catalog, &recipes).unwrap();
        assert_original_snapshot(&inventory, &case["before"], &catalog, name);
        let stale_state = i32::from(inventory.state_id());
        let actions = case["actions"].as_array().unwrap();
        let after = case["after"].as_array().unwrap();
        assert_eq!(actions.len(), after.len(), "{name}");
        let mut dropped = BTreeMap::<String, u32>::new();
        for (action, expected) in actions.iter().zip(after) {
            let policy = InventoryPolicy {
                creative,
                ..policy(true)
            };
            let effects = if action["kind"] == "close" {
                inventory.close_crafting(&catalog, policy)
            } else {
                assert_eq!(action["kind"], "click", "{name}");
                let mode = match action["mode"].as_u64().unwrap() {
                    0 => ClickMode::Pickup,
                    1 => ClickMode::QuickMove,
                    2 => ClickMode::Swap,
                    3 => ClickMode::Clone,
                    4 => ClickMode::Throw,
                    5 => ClickMode::QuickCraft,
                    6 => ClickMode::PickupAll,
                    other => panic!("unknown original click mode {other}"),
                };
                let mut request = click(
                    mode,
                    i16::try_from(action["slot"].as_i64().unwrap()).unwrap(),
                    i8::try_from(action["button"].as_i64().unwrap()).unwrap(),
                );
                request.state_id = match action["state_policy"].as_str().unwrap() {
                    "same_stale" => stale_state,
                    "current" => i32::from(inventory.state_id()),
                    other => panic!("unknown original state policy {other}"),
                };
                inventory.click_with_crafting(&catalog, &request, policy, &recipes, &remainders)
            }
            .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_original_snapshot(&inventory, expected, &catalog, name);
            for stack in effects.drops {
                assert_eq!(
                    stack.patch(),
                    &ValidatedPatch::default(),
                    "{name}: unexpected dropped patch"
                );
                *dropped
                    .entry(catalog.item(stack.item()).unwrap().key.clone())
                    .or_default() += u32::from(stack.count());
            }
        }
        let expected_dropped: BTreeMap<_, _> = case["dropped"]
            .as_array()
            .unwrap()
            .iter()
            .map(|stack| {
                assert_eq!(stack["components"], json!({}), "{name}");
                (
                    stack["item"].as_str().unwrap().to_owned(),
                    u32::try_from(stack["count"].as_u64().unwrap()).unwrap(),
                )
            })
            .collect();
        assert_eq!(dropped, expected_dropped, "{name}: dropped items");
    }
}

fn original_catalog(menu: &Value, recipes: &Value) -> ItemCatalog {
    let mut items = recipes["items"].as_object().unwrap().clone();
    for (key, definition) in menu["items"].as_object().unwrap() {
        if let Some(previous) = items.insert(key.clone(), definition.clone()) {
            assert_eq!(
                previous, *definition,
                "official item definitions disagree: {key}"
            );
        }
    }
    let items = items
        .into_iter()
        .map(|(key, value)| ItemDef {
            id: ItemId(u32::try_from(value["id"].as_u64().unwrap()).unwrap()),
            key,
            max_stack: u8::try_from(value["max_stack"].as_u64().unwrap()).unwrap(),
            max_damage: value["max_damage"]
                .as_u64()
                .map(|value| u32::try_from(value).unwrap()),
        })
        .collect();
    let components = recipes["components"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, id)| ComponentDef {
            id: ComponentId(u32::try_from(id.as_u64().unwrap()).unwrap()),
            key: key.clone(),
        })
        .collect();
    ItemCatalog::from_definitions(items, components).unwrap()
}

fn original_stack(value: &Value, catalog: &ItemCatalog) -> Slot {
    if value.is_null() {
        return None;
    }
    assert_eq!(
        value["components"],
        json!({}),
        "menu oracle adapter does not silently discard component patches"
    );
    let item = catalog.item_id(value["item"].as_str().unwrap()).unwrap();
    let count = u8::try_from(value["count"].as_u64().unwrap()).unwrap();
    let stack = ItemStack::new(item, count, ValidatedPatch::default()).unwrap();
    catalog.validate_stack(&stack).unwrap();
    Some(stack)
}

fn assert_original_snapshot(
    inventory: &PlayerInventory,
    expected: &Value,
    catalog: &ItemCatalog,
    name: &str,
) {
    let expected_slots = expected["slots"].as_array().unwrap();
    assert_eq!(expected_slots.len(), MENU_SLOT_COUNT, "{name}");
    let expected_slots: [Slot; MENU_SLOT_COUNT] =
        std::array::from_fn(|index| original_stack(&expected_slots[index], catalog));
    assert_eq!(inventory.slots(), &expected_slots, "{name}: slots");
    assert_eq!(
        inventory.cursor(),
        &original_stack(&expected["cursor"], catalog),
        "{name}: cursor"
    );
    if let Some(selected) = expected["selected"].as_u64() {
        assert_eq!(
            u64::from(inventory.selected()),
            selected,
            "{name}: selected hotbar"
        );
    }
}
