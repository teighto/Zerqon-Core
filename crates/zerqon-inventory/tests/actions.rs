use proptest::prelude::*;
use zerqon_inventory::{
    ClickMode, ClickRequest, CreativeSlotRequest, HashedStack, InventoryError, InventoryPolicy,
    ItemCatalog, ItemDef, ItemId, ItemStack, PlayerInventory, Slot, ValidatedPatch,
};

fn catalog() -> ItemCatalog {
    ItemCatalog::from_definitions(
        vec![
            ItemDef {
                id: ItemId(0),
                key: "minecraft:air".into(),
                max_stack: 64,
                max_damage: None,
            },
            ItemDef {
                id: ItemId(1),
                key: "minecraft:stone".into(),
                max_stack: 64,
                max_damage: None,
            },
            ItemDef {
                id: ItemId(2),
                key: "minecraft:iron_sword".into(),
                max_stack: 1,
                max_damage: Some(100),
            },
        ],
        Vec::new(),
    )
    .unwrap()
}

fn stack(count: u8) -> Slot {
    Some(ItemStack::new(ItemId(1), count, ValidatedPatch::default()).unwrap())
}
fn count(slot: &Slot) -> u8 {
    slot.as_ref().map_or(0, ItemStack::count)
}
fn policy() -> InventoryPolicy {
    InventoryPolicy {
        creative: false,
        can_interact: true,
        allow_drops: true,
    }
}
fn creative() -> InventoryPolicy {
    InventoryPolicy {
        creative: true,
        ..policy()
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
fn total(inventory: &PlayerInventory) -> u32 {
    inventory
        .slots()
        .iter()
        .chain(std::iter::once(inventory.cursor()))
        .map(|slot| u32::from(count(slot)))
        .sum()
}

#[test]
fn trusted_setters_validate_before_mutation() {
    let c = catalog();
    let mut inventory = PlayerInventory::new();
    inventory.set_slot(&c, 36, stack(3)).unwrap();
    assert_eq!(count(inventory.selected_stack()), 3);
    let before = inventory.clone();
    assert!(inventory.set_slot(&c, 46, stack(1)).is_err());
    assert!(inventory.set_cursor(&c, stack(65)).is_err());
    assert_eq!(inventory, before);
    inventory.select_hotbar(8).unwrap();
    assert_eq!(inventory.selected(), 8);
    assert!(inventory.select_hotbar(9).is_err());
    assert_eq!(inventory.selected(), 8);
}

#[test]
fn creative_updates_require_permission_and_effective_item_limits() {
    let c = catalog();
    let mut inventory = PlayerInventory::new();
    let request = CreativeSlotRequest {
        slot: 36,
        stack: stack(64),
    };
    assert!(matches!(
        inventory.creative_set(&c, &request, policy()),
        Err(InventoryError::CreativeRequired)
    ));
    inventory.creative_set(&c, &request, creative()).unwrap();
    assert_eq!(count(&inventory.slots()[36]), 64);
    let before = inventory.clone();
    assert!(
        inventory
            .creative_set(
                &c,
                &CreativeSlotRequest {
                    slot: 0,
                    stack: stack(1)
                },
                creative()
            )
            .is_err()
    );
    assert!(
        inventory
            .creative_set(
                &c,
                &CreativeSlotRequest {
                    slot: 36,
                    stack: stack(65)
                },
                creative()
            )
            .is_err()
    );
    assert_eq!(inventory, before);
}

#[test]
fn pickup_splits_places_merges_and_swaps_using_server_state() {
    let c = catalog();
    let mut inventory = PlayerInventory::new();
    inventory.set_slot(&c, 9, stack(9)).unwrap();
    inventory
        .click(&c, &click(ClickMode::Pickup, 9, 1), policy())
        .unwrap();
    assert_eq!(
        (count(&inventory.slots()[9]), count(inventory.cursor())),
        (4, 5)
    );
    inventory
        .click(&c, &click(ClickMode::Pickup, 10, 1), policy())
        .unwrap();
    assert_eq!(
        (count(&inventory.slots()[10]), count(inventory.cursor())),
        (1, 4)
    );
    inventory.set_slot(&c, 11, stack(63)).unwrap();
    inventory
        .click(&c, &click(ClickMode::Pickup, 11, 0), policy())
        .unwrap();
    assert_eq!(
        (count(&inventory.slots()[11]), count(inventory.cursor())),
        (64, 3)
    );
    let sword = Some(ItemStack::new(ItemId(2), 1, ValidatedPatch::default()).unwrap());
    inventory.set_slot(&c, 12, sword.clone()).unwrap();
    inventory
        .click(&c, &click(ClickMode::Pickup, 12, 1), policy())
        .unwrap();
    assert_eq!(inventory.cursor(), &sword);
    assert_eq!(count(&inventory.slots()[12]), 3);
}

#[test]
fn quick_move_merges_before_using_empty_slots_and_swap_supports_offhand() {
    let c = catalog();
    let mut inventory = PlayerInventory::new();
    inventory.set_slot(&c, 9, stack(10)).unwrap();
    inventory.set_slot(&c, 37, stack(60)).unwrap();
    inventory
        .click(&c, &click(ClickMode::QuickMove, 9, 0), policy())
        .unwrap();
    assert_eq!(
        (
            count(&inventory.slots()[9]),
            count(&inventory.slots()[36]),
            count(&inventory.slots()[37])
        ),
        (0, 6, 64)
    );
    inventory
        .click(&c, &click(ClickMode::Swap, 37, 40), policy())
        .unwrap();
    assert_eq!(
        (count(&inventory.slots()[37]), count(&inventory.slots()[45])),
        (0, 64)
    );
}

#[test]
fn drops_are_explicit_effects_and_disabled_drop_paths_do_not_lose_items() {
    let c = catalog();
    let mut inventory = PlayerInventory::new();
    inventory.set_slot(&c, 9, stack(8)).unwrap();
    let before = inventory.clone();
    assert!(matches!(
        inventory.click(
            &c,
            &click(ClickMode::Throw, 9, 0),
            InventoryPolicy {
                allow_drops: false,
                ..policy()
            }
        ),
        Err(InventoryError::DroppingUnavailable)
    ));
    assert_eq!(inventory, before);
    let effect = inventory
        .click(&c, &click(ClickMode::Throw, 9, 0), policy())
        .unwrap();
    assert_eq!(effect.drops[0].count(), 1);
    assert_eq!(count(&inventory.slots()[9]), 7);
    inventory
        .click(&c, &click(ClickMode::Pickup, 9, 0), policy())
        .unwrap();
    let effect = inventory
        .click(&c, &click(ClickMode::Pickup, -999, 0), policy())
        .unwrap();
    assert_eq!(effect.drops[0].count(), 7);
    assert!(inventory.cursor().is_none());
}

#[test]
fn drag_distributes_evenly_once_per_distinct_slot_and_cannot_mint_survival_items() {
    let c = catalog();
    let mut inventory = PlayerInventory::new();
    inventory.set_cursor(&c, stack(10)).unwrap();
    inventory
        .click(&c, &click(ClickMode::QuickCraft, -999, 0), policy())
        .unwrap();
    for slot in [9, 10, 10, 11] {
        inventory
            .click(&c, &click(ClickMode::QuickCraft, slot, 1), policy())
            .unwrap();
    }
    inventory
        .click(&c, &click(ClickMode::QuickCraft, -999, 2), policy())
        .unwrap();
    assert_eq!(
        [
            count(&inventory.slots()[9]),
            count(&inventory.slots()[10]),
            count(&inventory.slots()[11]),
            count(inventory.cursor())
        ],
        [3, 3, 3, 1]
    );
    let before = inventory.clone();
    assert!(
        inventory
            .click(&c, &click(ClickMode::QuickCraft, -999, 8), policy())
            .is_err()
    );
    assert_eq!(inventory, before);
}

#[test]
fn pickup_all_fills_cursor_from_partial_stacks_before_full_stacks() {
    let c = catalog();
    let mut inventory = PlayerInventory::new();
    inventory.set_cursor(&c, stack(1)).unwrap();
    inventory.set_slot(&c, 10, stack(64)).unwrap();
    inventory.set_slot(&c, 11, stack(7)).unwrap();
    inventory
        .click(&c, &click(ClickMode::PickupAll, 9, 0), policy())
        .unwrap();
    assert_eq!(
        (
            count(inventory.cursor()),
            count(&inventory.slots()[10]),
            count(&inventory.slots()[11])
        ),
        (64, 8, 0)
    );
}

#[test]
fn pickup_all_reports_matching_crafting_or_armor_items_it_cannot_handle() {
    let c = catalog();
    let mut inventory = PlayerInventory::new();
    inventory.set_slot(&c, 1, stack(3)).unwrap();
    inventory.set_cursor(&c, stack(1)).unwrap();
    let before = inventory.clone();
    assert!(matches!(
        inventory.click(&c, &click(ClickMode::PickupAll, 9, 0), policy()),
        Err(InventoryError::UnsupportedSlot(1))
    ));
    assert_eq!(inventory, before);
}

#[test]
fn invalid_context_and_unsupported_slots_leave_every_field_unchanged() {
    let c = catalog();
    let mut inventory = PlayerInventory::new();
    inventory.set_slot(&c, 9, stack(3)).unwrap();
    for request in [
        ClickRequest {
            container_id: 2,
            ..click(ClickMode::Pickup, 9, 0)
        },
        ClickRequest {
            state_id: -1,
            ..click(ClickMode::Pickup, 9, 0)
        },
        click(ClickMode::Pickup, 5, 0),
        click(ClickMode::Pickup, 46, 0),
        click(ClickMode::Pickup, 9, 5),
    ] {
        let before = inventory.clone();
        assert!(inventory.click(&c, &request, policy()).is_err());
        assert_eq!(inventory, before);
    }
    let before = inventory.clone();
    assert!(
        inventory
            .click(
                &c,
                &click(ClickMode::Pickup, 9, 0),
                InventoryPolicy {
                    can_interact: false,
                    ..policy()
                }
            )
            .is_err()
    );
    assert_eq!(inventory, before);
}

#[test]
fn stale_state_and_client_predictions_do_not_supply_authoritative_items() {
    use std::collections::{BTreeMap, BTreeSet};
    let c = catalog();
    let mut inventory = PlayerInventory::new();
    inventory.set_slot(&c, 9, stack(3)).unwrap();
    let mut expected = inventory.clone();
    let actual_request = click(ClickMode::Pickup, 9, 0);
    expected.click(&c, &actual_request, policy()).unwrap();
    let mut forged = actual_request;
    forged.state_id = 12345;
    forged.changed_slots = vec![(
        9,
        HashedStack::Item {
            item: ItemId(2),
            count: 99,
            added: BTreeMap::new(),
            removed: BTreeSet::new(),
        },
    )];
    forged.carried = HashedStack::Item {
        item: ItemId(1),
        count: 64,
        added: BTreeMap::new(),
        removed: BTreeSet::new(),
    };
    let effects = inventory.click(&c, &forged, policy()).unwrap();
    assert_eq!(inventory, expected);
    assert!(effects.force_full_sync);
    assert_eq!(count(inventory.cursor()), 3);
}

#[test]
fn duplicate_or_out_of_range_predictions_do_not_change_inventory() {
    let c = catalog();
    let mut inventory = PlayerInventory::new();
    inventory.set_slot(&c, 9, stack(3)).unwrap();
    for changes in [
        vec![(9, HashedStack::Empty), (9, HashedStack::Empty)],
        vec![(46, HashedStack::Empty)],
        vec![(-1, HashedStack::Empty)],
        vec![(9, HashedStack::Empty); 129],
    ] {
        let before = inventory.clone();
        let request = ClickRequest {
            changed_slots: changes,
            ..click(ClickMode::Pickup, 9, 0)
        };
        assert!(matches!(
            inventory.click(&c, &request, policy()),
            Err(InventoryError::InvalidPrediction)
        ));
        assert_eq!(inventory, before);
    }
}

#[test]
fn right_drag_places_one_per_target_and_bad_sequence_has_no_partial_effect() {
    let c = catalog();
    let mut inventory = PlayerInventory::new();
    inventory.set_cursor(&c, stack(7)).unwrap();
    let before = inventory.clone();
    assert!(
        inventory
            .click(&c, &click(ClickMode::QuickCraft, 9, 5), policy())
            .is_err()
    );
    assert_eq!(inventory, before);
    inventory
        .click(&c, &click(ClickMode::QuickCraft, -999, 4), policy())
        .unwrap();
    for slot in [9, 10, 11] {
        inventory
            .click(&c, &click(ClickMode::QuickCraft, slot, 5), policy())
            .unwrap();
    }
    let before = inventory.clone();
    assert!(
        inventory
            .click(&c, &click(ClickMode::QuickCraft, -999, 2), policy())
            .is_err()
    );
    assert_eq!(inventory, before);
    inventory
        .click(&c, &click(ClickMode::QuickCraft, -999, 6), policy())
        .unwrap();
    assert_eq!(
        [
            count(&inventory.slots()[9]),
            count(&inventory.slots()[10]),
            count(&inventory.slots()[11]),
            count(inventory.cursor())
        ],
        [1, 1, 1, 4]
    );
}

#[test]
fn creative_clone_and_middle_drag_have_explicit_creation_semantics() {
    let c = catalog();
    let mut inventory = PlayerInventory::new();
    inventory.set_slot(&c, 9, stack(3)).unwrap();
    let before = inventory.clone();
    assert!(matches!(
        inventory.click(&c, &click(ClickMode::Clone, 9, 2), policy()),
        Err(InventoryError::CreativeRequired)
    ));
    assert_eq!(inventory, before);
    inventory
        .click(&c, &click(ClickMode::Clone, 9, 2), creative())
        .unwrap();
    assert_eq!(count(inventory.cursor()), 64);
    inventory
        .click(&c, &click(ClickMode::QuickCraft, -999, 8), creative())
        .unwrap();
    for slot in [10, 11] {
        inventory
            .click(&c, &click(ClickMode::QuickCraft, slot, 9), creative())
            .unwrap();
    }
    inventory
        .click(&c, &click(ClickMode::QuickCraft, -999, 10), creative())
        .unwrap();
    assert_eq!(
        (
            count(&inventory.slots()[10]),
            count(&inventory.slots()[11]),
            count(inventory.cursor())
        ),
        (64, 64, 0)
    );
}

#[test]
fn creative_permission_checks_are_not_bypassed_by_an_existing_drag() {
    let c = catalog();
    let mut inventory = PlayerInventory::new();
    inventory.set_cursor(&c, stack(3)).unwrap();
    inventory
        .click(&c, &click(ClickMode::QuickCraft, -999, 0), policy())
        .unwrap();
    let before = inventory.clone();
    assert!(matches!(
        inventory.click(&c, &click(ClickMode::Clone, 9, 2), policy()),
        Err(InventoryError::CreativeRequired)
    ));
    assert_eq!(inventory, before);
}

#[test]
fn single_target_drag_uses_the_matching_pickup_button() {
    let c = catalog();
    for (kind, expected_slot, expected_cursor) in [(0, 7, 0), (1, 1, 6), (2, 0, 7)] {
        let mut inventory = PlayerInventory::new();
        inventory.set_cursor(&c, stack(7)).unwrap();
        for (slot, stage) in [(-999, 0), (9, 1), (-999, 2)] {
            inventory
                .click(
                    &c,
                    &click(ClickMode::QuickCraft, slot, kind * 4 + stage),
                    creative(),
                )
                .unwrap();
        }
        assert_eq!(
            (count(&inventory.slots()[9]), count(inventory.cursor())),
            (expected_slot, expected_cursor)
        );
    }
}

#[test]
fn state_revision_wraps_at_the_vanilla_fifteen_bit_boundary() {
    let c = catalog();
    let mut inventory = PlayerInventory::new();
    for _ in 0..32767 {
        inventory
            .creative_set(
                &c,
                &CreativeSlotRequest {
                    slot: 9,
                    stack: None,
                },
                creative(),
            )
            .unwrap();
    }
    assert_eq!(inventory.state_id(), 32767);
    inventory
        .creative_set(
            &c,
            &CreativeSlotRequest {
                slot: 9,
                stack: None,
            },
            creative(),
        )
        .unwrap();
    assert_eq!(inventory.state_id(), 0);
}

#[test]
fn explicit_default_component_patch_merges_with_plain_stack() {
    use std::collections::{BTreeMap, BTreeSet};
    use zerqon_inventory::{ComponentDef, ComponentId, ComponentValue};
    let c = ItemCatalog::from_definitions(
        vec![ItemDef {
            id: ItemId(1),
            key: "minecraft:stone".into(),
            max_stack: 64,
            max_damage: None,
        }],
        vec![ComponentDef {
            id: ComponentId(1),
            key: "minecraft:max_stack_size".into(),
        }],
    )
    .unwrap();
    let explicit = ValidatedPatch::new(
        BTreeMap::from([(ComponentId(1), ComponentValue::MaxStackSize(64))]),
        BTreeSet::new(),
        &c,
    )
    .unwrap();
    let mut inventory = PlayerInventory::new();
    inventory.set_slot(&c, 9, stack(62)).unwrap();
    inventory
        .set_cursor(&c, Some(ItemStack::new(ItemId(1), 2, explicit).unwrap()))
        .unwrap();
    inventory
        .click(&c, &click(ClickMode::Pickup, 9, 0), policy())
        .unwrap();
    assert_eq!(count(&inventory.slots()[9]), 64);
    assert!(inventory.cursor().is_none());
}

#[test]
#[ignore = "requires official generated 26.3 item reports"]
fn real_catalog_rejects_unimplemented_bundle_hooks_and_armor_autoequip() {
    let c = ItemCatalog::load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.cache/vanilla/26.3"),
    )
    .unwrap();
    let mut inventory = PlayerInventory::new();
    let bundle = Some(
        ItemStack::new(
            c.item_id("minecraft:bundle").unwrap(),
            1,
            ValidatedPatch::default(),
        )
        .unwrap(),
    );
    inventory.set_slot(&c, 9, bundle).unwrap();
    let before = inventory.clone();
    assert!(matches!(
        inventory.click(&c, &click(ClickMode::Pickup, 9, 0), policy()),
        Err(InventoryError::UnsupportedItemBehavior)
    ));
    assert_eq!(inventory, before);
    let shield = Some(
        ItemStack::new(
            c.item_id("minecraft:shield").unwrap(),
            1,
            ValidatedPatch::default(),
        )
        .unwrap(),
    );
    inventory.set_slot(&c, 9, shield).unwrap();
    let before = inventory.clone();
    assert!(matches!(
        inventory.click(&c, &click(ClickMode::QuickMove, 9, 0), policy()),
        Err(InventoryError::UnsupportedItemBehavior)
    ));
    assert_eq!(inventory, before);
}

proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/actions.proptest-regressions")))), ..ProptestConfig::default() })]
    #[test]
    fn survival_pickup_sequences_conserve_total_count_and_enforce_limits(initial in 1_u8..=64, operations in prop::collection::vec((9_i16..=45, 0_i8..=1), 0..100)) {
        let c = catalog(); let mut inventory = PlayerInventory::new(); inventory.set_slot(&c, 9, stack(initial)).unwrap();
        for (slot, button) in operations { inventory.click(&c, &click(ClickMode::Pickup, slot, button), policy()).unwrap(); prop_assert_eq!(total(&inventory), u32::from(initial)); prop_assert!(inventory.slots().iter().chain(std::iter::once(inventory.cursor())).all(|slot|count(slot)<=64)); }
    }

    #[test]
    fn mixed_survival_operations_conserve_items_including_explicit_drops(initial in prop::collection::vec(0_u8..=64, 1..=12), operations in prop::collection::vec((0_u8..=6, 9_i16..=45, 0_u8..=10), 1..150)) {
        let c = catalog(); let mut inventory = PlayerInventory::new();
        for (index, quantity) in initial.into_iter().enumerate() { if quantity != 0 { inventory.set_slot(&c, 9 + index as i16, stack(quantity)).unwrap(); } }
        let expected = total(&inventory); let mut dropped = 0_u32;
        for (mode, slot, input) in operations {
            let (mode, button, slot) = match mode {
                0 => (ClickMode::Pickup, (input % 2) as i8, if input == 10 { -999 } else { slot }),
                1 => (ClickMode::QuickMove, (input % 2) as i8, slot),
                2 => (ClickMode::Swap, if input == 10 { 40 } else { (input % 9) as i8 }, slot),
                3 => (ClickMode::Throw, (input % 2) as i8, slot),
                4 => (ClickMode::PickupAll, (input % 2) as i8, slot),
                5 => (ClickMode::QuickCraft, (input % 7) as i8, if input & 3 == 1 { slot } else { -999 }),
                _ => (ClickMode::Clone, 2, slot),
            };
            let before = inventory.clone();
            match inventory.click(&c, &click(mode, slot, button), policy()) {
                Ok(effects) => dropped += effects.drops.iter().map(|stack|u32::from(stack.count())).sum::<u32>(),
                Err(_) => prop_assert_eq!(&inventory, &before),
            }
            prop_assert_eq!(total(&inventory) + dropped, expected);
            prop_assert!(inventory.slots().iter().chain(std::iter::once(inventory.cursor())).all(|slot|count(slot)<=64));
            prop_assert!(inventory.state_id() <= 32767);
        }
    }
}
