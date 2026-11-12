use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use zerqon_inventory::*;

fn catalog() -> ItemCatalog {
    ItemCatalog::from_definitions(
        vec![
            ItemDef {
                id: ItemId(1),
                key: "minecraft:stone".into(),
                max_stack: 64,
                max_damage: None,
            },
            ItemDef {
                id: ItemId(2),
                key: "minecraft:oak_log".into(),
                max_stack: 64,
                max_damage: None,
            },
            ItemDef {
                id: ItemId(3),
                key: "minecraft:iron_pickaxe".into(),
                max_stack: 1,
                max_damage: Some(250),
            },
        ],
        vec![
            ComponentDef {
                id: ComponentId(1),
                key: "minecraft:custom_name".into(),
            },
            ComponentDef {
                id: ComponentId(2),
                key: "minecraft:max_stack_size".into(),
            },
            ComponentDef {
                id: ComponentId(3),
                key: "minecraft:damage".into(),
            },
        ],
    )
    .unwrap()
}

fn policy(creative: bool) -> InventoryPolicy {
    InventoryPolicy {
        creative,
        can_interact: true,
        allow_drops: true,
    }
}

fn stack(item: u32, count: u8) -> ItemStack {
    ItemStack::new(ItemId(item), count, ValidatedPatch::default()).unwrap()
}

fn fill(inventory: &mut PlayerInventory, catalog: &ItemCatalog) {
    for slot in 9..45 {
        inventory
            .set_slot(catalog, slot, Some(stack(1, 64)))
            .unwrap();
    }
}

fn owned_count(inventory: &PlayerInventory) -> u32 {
    inventory
        .slots()
        .iter()
        .skip(1)
        .flatten()
        .map(|value| u32::from(value.count()))
        .sum::<u32>()
        + inventory
            .cursor()
            .as_ref()
            .map_or(0, |value| u32::from(value.count()))
}

#[test]
fn partial_pickup_preserves_remainder_and_full_inventory_is_a_true_noop() {
    let catalog = catalog();
    let mut inventory = PlayerInventory::new();
    fill(&mut inventory, &catalog);
    inventory
        .set_slot(&catalog, 36, Some(stack(1, 63)))
        .unwrap();
    let incoming = stack(1, 3);
    let before = owned_count(&inventory);
    let result = inventory
        .insert_pickup(&catalog, &incoming, policy(false))
        .unwrap();
    assert_eq!((result.inserted, result.discarded), (1, 0));
    assert_eq!(result.remainder, Some(stack(1, 2)));
    assert_eq!(incoming.count(), 3);
    assert_eq!(owned_count(&inventory), before + 1);
    assert_eq!(
        result.effects.unwrap().changed_slots,
        vec![(36, Some(stack(1, 64)))]
    );
    let full = inventory.clone();
    let no_space = inventory
        .insert_pickup(&catalog, result.remainder.as_ref().unwrap(), policy(false))
        .unwrap();
    assert_eq!((no_space.inserted, no_space.discarded), (0, 0));
    assert_eq!(no_space.remainder, Some(stack(1, 2)));
    assert!(no_space.effects.is_none());
    assert_eq!(inventory, full);
}

#[test]
fn pickup_merges_selected_then_offhand_then_main_without_touching_menu_contents() {
    let catalog = catalog();
    let mut inventory = PlayerInventory::new();
    fill(&mut inventory, &catalog);
    inventory.select_hotbar(4).unwrap();
    for (slot, count) in [(40, 60), (45, 62), (36, 63), (9, 63)] {
        inventory
            .set_slot(&catalog, slot, Some(stack(1, count)))
            .unwrap();
    }
    inventory.set_slot(&catalog, 0, Some(stack(2, 64))).unwrap();
    inventory.set_slot(&catalog, 1, Some(stack(2, 3))).unwrap();
    inventory.set_slot(&catalog, 5, Some(stack(2, 5))).unwrap();
    inventory.set_cursor(&catalog, Some(stack(1, 11))).unwrap();
    let result = inventory
        .insert_pickup(&catalog, &stack(1, 7), policy(false))
        .unwrap();
    assert_eq!((result.inserted, result.discarded), (7, 0));
    assert!(result.remainder.is_none());
    for slot in [40, 45, 36] {
        assert_eq!(inventory.slots()[slot], Some(stack(1, 64)));
    }
    assert_eq!(inventory.slots()[9], Some(stack(1, 63)));
    assert_eq!(inventory.slots()[0], Some(stack(2, 64)));
    assert_eq!(inventory.slots()[1], Some(stack(2, 3)));
    assert_eq!(inventory.slots()[5], Some(stack(2, 5)));
    assert_eq!(inventory.cursor(), &Some(stack(1, 11)));
    assert_eq!(inventory.selected(), 4);
}

#[test]
fn pickup_uses_hotbar_then_main_empty_slots_but_not_empty_offhand() {
    let catalog = catalog();
    let mut inventory = PlayerInventory::new();
    fill(&mut inventory, &catalog);
    inventory.set_slot(&catalog, 37, None).unwrap();
    inventory.set_slot(&catalog, 9, None).unwrap();
    let incoming = stack(2, 7);
    let result = inventory
        .insert_pickup(&catalog, &incoming, policy(false))
        .unwrap();
    assert_eq!(result.inserted, 7);
    assert_eq!(inventory.slots()[37], Some(incoming));
    assert!(inventory.slots()[9].is_none());
    assert!(inventory.slots()[45].is_none());
    inventory.set_slot(&catalog, 9, Some(stack(1, 64))).unwrap();
    let before = inventory.clone();
    let result = inventory
        .insert_pickup(&catalog, &stack(1, 1), policy(false))
        .unwrap();
    assert_eq!(result.inserted, 0);
    assert_eq!(inventory, before);
}

#[test]
fn damaged_tools_keep_components_in_the_first_empty_slot() {
    let catalog = catalog();
    let patch = ValidatedPatch::new(
        BTreeMap::from([
            (ComponentId(1), ComponentValue::CustomName("teighto".into())),
            (ComponentId(3), ComponentValue::Damage(17)),
        ]),
        BTreeSet::new(),
        &catalog,
    )
    .unwrap();
    let tool = ItemStack::new(ItemId(3), 1, patch).unwrap();
    let mut inventory = PlayerInventory::new();
    inventory.set_slot(&catalog, 36, Some(stack(3, 1))).unwrap();
    let result = inventory
        .insert_pickup(&catalog, &tool, policy(false))
        .unwrap();
    assert!(result.remainder.is_none());
    assert_eq!(inventory.slots()[36], Some(stack(3, 1)));
    assert_eq!(inventory.slots()[37], Some(tool));
}

#[test]
fn pickup_matches_components_and_respects_the_actual_ninety_nine_stack_limit() {
    let catalog = catalog();
    let named = ItemStack::new(
        ItemId(1),
        7,
        ValidatedPatch::new(
            BTreeMap::from([(ComponentId(1), ComponentValue::CustomName("teighto".into()))]),
            BTreeSet::new(),
            &catalog,
        )
        .unwrap(),
    )
    .unwrap();
    let mut inventory = PlayerInventory::new();
    inventory
        .set_slot(&catalog, 36, Some(stack(1, 63)))
        .unwrap();
    inventory
        .insert_pickup(&catalog, &named, policy(false))
        .unwrap();
    assert_eq!(inventory.slots()[36], Some(stack(1, 63)));
    assert_eq!(inventory.slots()[37], Some(named));

    let large = ItemStack::new(
        ItemId(1),
        98,
        ValidatedPatch::new(
            BTreeMap::from([(ComponentId(2), ComponentValue::MaxStackSize(99))]),
            BTreeSet::new(),
            &catalog,
        )
        .unwrap(),
    )
    .unwrap();
    for slot in 9..45 {
        inventory
            .set_slot(&catalog, slot, Some(stack(2, 64)))
            .unwrap();
    }
    inventory
        .set_slot(&catalog, 36, Some(large.clone()))
        .unwrap();
    inventory
        .set_slot(&catalog, 45, Some(large.clone()))
        .unwrap();
    let result = inventory
        .insert_pickup(&catalog, &large.with_count(20).unwrap(), policy(false))
        .unwrap();
    assert_eq!((result.inserted, result.discarded), (2, 0));
    assert_eq!(result.remainder, Some(large.with_count(18).unwrap()));
    assert_eq!(inventory.slots()[36], Some(large.with_count(99).unwrap()));
    assert_eq!(inventory.slots()[45], Some(large.with_count(99).unwrap()));
}

#[test]
fn creative_overflow_is_explicit_and_does_not_fake_an_inventory_change() {
    let catalog = catalog();
    let mut inventory = PlayerInventory::new();
    fill(&mut inventory, &catalog);
    let before = inventory.clone();
    let result = inventory
        .insert_pickup(&catalog, &stack(2, 7), policy(true))
        .unwrap();
    assert_eq!((result.inserted, result.discarded), (0, 7));
    assert!(result.remainder.is_none());
    assert!(result.effects.is_none());
    assert_eq!(inventory, before);
    inventory
        .set_slot(&catalog, 36, Some(stack(1, 63)))
        .unwrap();
    let result = inventory
        .insert_pickup(&catalog, &stack(1, 3), policy(true))
        .unwrap();
    assert_eq!((result.inserted, result.discarded), (1, 2));
    assert!(result.remainder.is_none());
    assert!(result.effects.is_some());
    assert_eq!(inventory.slots()[36], Some(stack(1, 64)));
}

#[test]
fn q_and_drop_all_split_selected_stack_even_with_a_carried_item() {
    let catalog = catalog();
    let patch = ValidatedPatch::new(
        BTreeMap::from([(ComponentId(1), ComponentValue::CustomName("teighto".into()))]),
        BTreeSet::new(),
        &catalog,
    )
    .unwrap();
    let item = ItemStack::new(ItemId(1), 3, patch).unwrap();
    let mut inventory = PlayerInventory::new();
    inventory.select_hotbar(2).unwrap();
    inventory
        .set_slot(&catalog, 38, Some(item.clone()))
        .unwrap();
    inventory.set_cursor(&catalog, Some(stack(2, 5))).unwrap();
    let first = inventory
        .drop_selected(&catalog, false, policy(false))
        .unwrap()
        .unwrap();
    assert_eq!(first.drops, vec![item.with_count(1).unwrap()]);
    assert_eq!(inventory.slots()[38], Some(item.with_count(2).unwrap()));
    assert_eq!(inventory.cursor(), &Some(stack(2, 5)));
    let all = inventory
        .drop_selected(&catalog, true, policy(true))
        .unwrap()
        .unwrap();
    assert_eq!(all.drops, vec![item.with_count(2).unwrap()]);
    assert!(inventory.slots()[38].is_none());
    assert_eq!(inventory.selected(), 2);
    let empty = inventory.clone();
    assert!(
        inventory
            .drop_selected(&catalog, true, policy(false))
            .unwrap()
            .is_none()
    );
    assert_eq!(inventory, empty);
}

#[test]
fn rejected_collection_and_dropping_leave_all_owned_and_transient_state_unchanged() {
    let catalog = catalog();
    let mut inventory = PlayerInventory::new();
    inventory.set_slot(&catalog, 36, Some(stack(1, 3))).unwrap();
    for request_policy in [
        InventoryPolicy {
            can_interact: false,
            ..policy(false)
        },
        InventoryPolicy {
            allow_drops: false,
            ..policy(false)
        },
    ] {
        let before = inventory.clone();
        assert!(
            inventory
                .drop_selected(&catalog, false, request_policy)
                .is_err()
        );
        assert_eq!(inventory, before);
    }
    let before = inventory.clone();
    assert!(
        inventory
            .insert_pickup(
                &catalog,
                &stack(1, 2),
                InventoryPolicy {
                    can_interact: false,
                    ..policy(false)
                }
            )
            .is_err()
    );
    assert_eq!(inventory, before);
    assert!(
        inventory
            .insert_pickup(&catalog, &stack(999, 2), policy(false))
            .is_err()
    );
    assert_eq!(inventory, before);
}

#[test]
fn server_pickup_and_q_preserve_an_active_drag() {
    let catalog = catalog();
    let mut inventory = PlayerInventory::new();
    inventory.set_slot(&catalog, 36, Some(stack(2, 3))).unwrap();
    inventory.set_cursor(&catalog, Some(stack(1, 8))).unwrap();
    let drag = |slot, button| ClickRequest {
        container_id: 0,
        state_id: 0,
        slot,
        button,
        mode: ClickMode::QuickCraft,
        changed_slots: Vec::new(),
        carried: HashedStack::Empty,
    };
    inventory
        .click(&catalog, &drag(-999, 0), policy(false))
        .unwrap();
    inventory
        .click(&catalog, &drag(9, 1), policy(false))
        .unwrap();
    inventory
        .insert_pickup(&catalog, &stack(2, 2), policy(false))
        .unwrap();
    inventory
        .drop_selected(&catalog, false, policy(false))
        .unwrap();
    inventory
        .click(&catalog, &drag(-999, 2), policy(false))
        .unwrap();
    assert_eq!(inventory.slots()[9], Some(stack(1, 8)));
    assert_eq!(inventory.slots()[36], Some(stack(2, 4)));
    assert!(inventory.cursor().is_none());
}

proptest! {
    #[test]
    fn pickup_conserves_items_or_accounts_for_creative_discard(
        counts in prop_oneof![
            prop::collection::vec(0_u8..=64, 36),
            prop::collection::vec(63_u8..=64, 36),
            Just(vec![64_u8; 36]),
        ],
        offhand in prop_oneof![Just(0_u8), Just(64_u8), 1_u8..=63],
        selected in 0_u8..9,
        incoming in 1_u8..=64,
        creative in any::<bool>(),
    ) {
        let catalog = catalog();
        let mut inventory = PlayerInventory::new();
        for (slot, count) in (36..45).chain(9..36).zip(counts.iter().copied()) {
            if count != 0 { inventory.set_slot(&catalog, slot, Some(stack(1, count))).unwrap(); }
        }
        if offhand != 0 { inventory.set_slot(&catalog, 45, Some(stack(1, offhand))).unwrap(); }
        inventory.select_hotbar(i16::from(selected)).unwrap();
        let capacity: u32 = counts.iter().map(|count| u32::from(64-count)).sum::<u32>()
            + if offhand == 0 { 0 } else { u32::from(64-offhand) };
        let before = inventory.clone();
        let result = inventory.insert_pickup(&catalog, &stack(1, incoming), policy(creative)).unwrap();
        let remainder = result.remainder.as_ref().map_or(0, |value| u32::from(value.count()));
        prop_assert_eq!(u32::from(result.inserted), capacity.min(u32::from(incoming)));
        prop_assert_eq!(owned_count(&inventory) + remainder + u32::from(result.discarded), owned_count(&before) + u32::from(incoming));
        prop_assert_eq!(result.effects.is_some(), result.inserted != 0);
        prop_assert_eq!(inventory.selected(), selected);
        prop_assert!(inventory.slots().iter().flatten().all(|value| value.count() <= 64));
        if !creative { prop_assert_eq!(result.discarded, 0); }
        if let Some(remaining) = result.remainder {
            let saturated = inventory.clone();
            let again = inventory.insert_pickup(&catalog, &remaining, policy(false)).unwrap();
            prop_assert_eq!(again.inserted, 0);
            prop_assert!(again.effects.is_none());
            prop_assert_eq!(&inventory, &saturated);
        }
    }
}
