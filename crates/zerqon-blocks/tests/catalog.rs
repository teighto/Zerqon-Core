use std::{collections::BTreeMap, sync::OnceLock};
use zerqon_blocks::*;

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| Catalog::embedded().unwrap())
}

#[test]
fn mappings_follow_original_block_items_and_tool_components() {
    let redstone = catalog().item_id("minecraft:redstone").unwrap();
    let wire = catalog().item_block(redstone).unwrap().unwrap();
    assert_eq!(
        catalog().state(wire).unwrap().key(),
        "minecraft:redstone_wire"
    );
    assert!(
        catalog()
            .item_block(catalog().item_id("minecraft:water_bucket").unwrap())
            .unwrap()
            .is_none()
    );
    assert!(
        !catalog()
            .item(catalog().item_id("minecraft:oak_door").unwrap())
            .unwrap()
            .ordinary_block_item
    );
    assert_eq!(
        catalog()
            .item(catalog().item_id("minecraft:diamond_sword").unwrap())
            .unwrap()
            .can_destroy_in_creative,
        Some(false)
    );
    assert_eq!(
        catalog()
            .item(catalog().item_id("minecraft:diamond_pickaxe").unwrap())
            .unwrap()
            .can_destroy_in_creative,
        Some(true)
    );
}

#[test]
fn every_original_state_resolves_from_its_reverse_properties() {
    for id in 0..catalog().state_count() as u32 {
        let state = catalog().state(StateId(id)).unwrap();
        let properties = state
            .properties()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        assert_eq!(
            catalog().resolve(state.key(), &properties).unwrap(),
            StateId(id)
        );
    }
}

#[test]
fn collision_geometry_distinguishes_slab_halves_and_dynamic_states() {
    for (slab_type, min, max) in [
        ("bottom", 0.0, 0.5),
        ("top", 0.5, 1.0),
        ("double", 0.0, 1.0),
    ] {
        let id = catalog()
            .resolve(
                "minecraft:oak_slab",
                &BTreeMap::from([("type".into(), slab_type.into())]),
            )
            .unwrap();
        assert_eq!(
            catalog().state(id).unwrap().collision().unwrap(),
            [Aabb {
                min: [0.0, min, 0.0],
                max: [1.0, max, 1.0]
            }]
        );
    }
    let powder = catalog()
        .resolve("minecraft:powder_snow", &BTreeMap::new())
        .unwrap();
    assert!(matches!(
        catalog().state(powder).unwrap().collision(),
        Err(Error::DynamicCollision)
    ));
}

#[test]
fn corrupt_truncated_and_appended_assets_fail_integrity_validation() {
    let asset = include_bytes!("../assets/26.3.json.zlib");
    let mut corrupt = asset.to_vec();
    let middle = corrupt.len() / 2;
    corrupt[middle] ^= 1;
    assert!(Catalog::from_compressed(&corrupt).is_err());
    assert!(Catalog::from_compressed(&asset[..asset.len() - 1]).is_err());
    let mut appended = asset.to_vec();
    appended.push(0);
    assert!(Catalog::from_compressed(&appended).is_err());
    assert!(Catalog::from_compressed(&vec![0; 512 * 1024 + 1]).is_err());
}

#[test]
fn property_changes_reject_unknown_names_values_and_duplicate_changes() {
    let log = catalog()
        .resolve("minecraft:oak_log", &BTreeMap::new())
        .unwrap();
    for changes in [
        vec![("axis", "diagonal")],
        vec![("unknown", "x")],
        vec![("axis", "x"), ("axis", "z")],
    ] {
        assert!(matches!(
            catalog().with_properties(log, changes),
            Err(Error::Properties)
        ));
    }
}
