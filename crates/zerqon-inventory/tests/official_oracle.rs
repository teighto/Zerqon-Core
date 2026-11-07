use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use zerqon_inventory::*;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned()
}
fn decode_hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|bytes| u8::from_str_radix(std::str::from_utf8(bytes).unwrap(), 16).unwrap())
        .collect()
}
fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn from_json(value: &Value, catalog: &ItemCatalog) -> Slot {
    if value.is_null() {
        return None;
    }
    let added = value["added"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| {
            let component = match key.as_str() {
                "minecraft:max_stack_size" => {
                    ComponentValue::MaxStackSize(value.as_u64().unwrap() as u8)
                }
                "minecraft:max_damage" => ComponentValue::MaxDamage(value.as_u64().unwrap() as u32),
                "minecraft:damage" => ComponentValue::Damage(value.as_u64().unwrap() as u32),
                "minecraft:repair_cost" => {
                    ComponentValue::RepairCost(value.as_u64().unwrap() as u32)
                }
                "minecraft:unbreakable" => ComponentValue::Unbreakable,
                "minecraft:custom_name" => {
                    ComponentValue::CustomName(value.as_str().unwrap().into())
                }
                "minecraft:item_name" => ComponentValue::ItemName(value.as_str().unwrap().into()),
                "minecraft:item_model" => ComponentValue::ItemModel(value.as_str().unwrap().into()),
                "minecraft:rarity" => ComponentValue::Rarity(match value.as_str().unwrap() {
                    "common" => 0,
                    "uncommon" => 1,
                    "rare" => 2,
                    "epic" => 3,
                    _ => panic!("invalid reference rarity"),
                }),
                "minecraft:enchantment_glint_override" => {
                    ComponentValue::EnchantmentGlint(value.as_bool().unwrap())
                }
                "minecraft:block_state" => ComponentValue::BlockState(
                    value
                        .as_object()
                        .unwrap()
                        .iter()
                        .map(|(key, value)| (key.clone(), value.as_str().unwrap().into()))
                        .collect(),
                ),
                _ => panic!("reference contains unsupported component {key}"),
            };
            (catalog.component_id(key).unwrap(), component)
        })
        .collect::<BTreeMap<_, _>>();
    let removed = value["removed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| catalog.component_id(name.as_str().unwrap()).unwrap())
        .collect::<BTreeSet<_>>();
    Some(
        ItemStack::new(
            catalog.item_id(value["item"].as_str().unwrap()).unwrap(),
            value["count"].as_u64().unwrap() as u8,
            ValidatedPatch::new(added, removed, catalog).unwrap(),
        )
        .unwrap(),
    )
}

fn to_json(slot: &Slot, catalog: &ItemCatalog) -> Value {
    let Some(stack) = slot else {
        return Value::Null;
    };
    let added: BTreeMap<_, _> = stack
        .patch()
        .added()
        .iter()
        .map(|(id, value)| (catalog.component(*id).unwrap().key.clone(), value.to_json()))
        .collect();
    let mut removed: Vec<_> = stack
        .patch()
        .removed()
        .iter()
        .map(|id| catalog.component(*id).unwrap().key.clone())
        .collect();
    removed.sort();
    json!({"item":catalog.item(stack.item()).unwrap().key,"count":stack.count(),"added":added,"removed":removed})
}

fn hashed_json(stack: &HashedStack, catalog: &ItemCatalog) -> Value {
    match stack {
        HashedStack::Empty => Value::Null,
        HashedStack::Item {
            item,
            count,
            added,
            removed,
        } => {
            let added: BTreeMap<_, _> = added
                .iter()
                .map(|(id, value)| (catalog.component(*id).unwrap().key.clone(), value))
                .collect();
            let mut removed: Vec<_> = removed
                .iter()
                .map(|id| catalog.component(*id).unwrap().key.clone())
                .collect();
            removed.sort();
            json!({"item":catalog.item(*item).unwrap().key,"count":count,"added":added,"removed":removed})
        }
    }
}

#[test]
#[ignore = "requires official 26.3 reports and scripts/test-inventory-codec.py --export .cache/inventory-oracle/official.json --export-default-items"]
fn all_official_default_items_and_component_fixtures_match_and_export_for_java() {
    let root = root();
    let data = std::env::var_os("ZERQON_TEST_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(".cache/vanilla/26.3"));
    let catalog = ItemCatalog::load(&data).unwrap();
    assert_eq!(catalog.item_count(), 1658);
    assert_eq!(catalog.component_count(), 122);
    let mut fixtures: Value = serde_json::from_slice(
        &std::fs::read(root.join(".cache/inventory-oracle/official.json"))
            .expect("run official inventory oracle export"),
    )
    .unwrap();
    let mut checked = 0;
    for fixture in fixtures["fixtures"].as_array_mut().unwrap() {
        let name = fixture["name"].as_str().unwrap().to_owned();
        let bytes = decode_hex(fixture["body_hex"].as_str().unwrap());
        let expected = &fixture["expected"];
        let output = match fixture["kind"].as_str().unwrap() {
            "creative_slot" => {
                let request = decode_creative_slot(&bytes, &catalog, DecodeLimits::default())
                    .unwrap_or_else(|error| panic!("{name}: {error}"));
                assert_eq!(
                    json!({"slot":request.slot,"stack":to_json(&request.stack,&catalog)}),
                    *expected,
                    "{name}"
                );
                Some(encode_creative_slot_body(&request, &catalog).unwrap())
            }
            "server_stack" => {
                let slot = decode_server_stack(&bytes, &catalog, DecodeLimits::default())
                    .unwrap_or_else(|error| panic!("{name}: {error}"));
                assert_eq!(to_json(&slot, &catalog), *expected, "{name}");
                let mut output = Vec::new();
                encode_server_stack(&slot, &catalog, &mut output).unwrap();
                Some(output)
            }
            "slot" => Some(
                encode_slot_body(
                    expected["container_id"].as_i64().unwrap() as i32,
                    expected["state_id"].as_u64().unwrap() as u16,
                    expected["slot"].as_i64().unwrap() as i16,
                    &from_json(&expected["stack"], &catalog),
                    &catalog,
                )
                .unwrap(),
            ),
            "content" => {
                let slots: Vec<_> = expected["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|value| from_json(value, &catalog))
                    .collect();
                Some(
                    encode_content_body(
                        expected["container_id"].as_i64().unwrap() as i32,
                        expected["state_id"].as_u64().unwrap() as u16,
                        &slots,
                        &from_json(&expected["carried"], &catalog),
                        &catalog,
                    )
                    .unwrap(),
                )
            }
            "cursor" => Some(
                encode_cursor_body(&from_json(&expected["stack"], &catalog), &catalog).unwrap(),
            ),
            "player_inventory" => Some(
                encode_player_inventory_body(
                    expected["slot"].as_u64().unwrap() as u8,
                    &from_json(&expected["stack"], &catalog),
                    &catalog,
                )
                .unwrap(),
            ),
            "carried_slot" => {
                assert_eq!(
                    decode_carried_slot(&bytes).unwrap() as u64,
                    expected["slot"].as_u64().unwrap(),
                    "{name}"
                );
                None
            }
            "held_slot" => {
                Some(encode_held_slot_body(expected["slot"].as_u64().unwrap() as u8).unwrap())
            }
            "use_item_on" => {
                let request = decode_use_on(&bytes).unwrap();
                let actual = json!({"hand":request.hand as u8,"position":[request.block.x,request.block.y,request.block.z],"face":request.face as u8,"hit":request.hit,"inside":request.inside,"world_border_hit":request.world_border_hit,"sequence":request.sequence});
                assert_eq!(actual, *expected, "{name}");
                None
            }
            "container_click" => {
                let request = decode_click(&bytes, &catalog, DecodeLimits::default()).unwrap();
                let changes: Vec<_> = request
                    .changed_slots
                    .iter()
                    .map(|(slot, stack)| json!({"slot":slot,"stack":hashed_json(stack,&catalog)}))
                    .collect();
                let actual = json!({"container_id":request.container_id,"state_id":request.state_id,"slot":request.slot,"button":request.button,"mode":request.mode as u8,"changed_slots":changes,"carried":hashed_json(&request.carried,&catalog)});
                assert_eq!(actual, *expected, "{name}");
                None
            }
            kind => panic!("Unhandled oracle kind {kind}"),
        };
        if let Some(output) = output {
            assert_eq!(output, bytes, "{name}");
            fixture["body_hex"] = Value::String(encode_hex(&output));
        }
        checked += 1;
    }
    assert!(checked >= 1692);
    std::fs::write(
        root.join(".cache/inventory-rust-fixtures.json"),
        serde_json::to_vec_pretty(&fixtures).unwrap(),
    )
    .unwrap();
}

#[test]
#[ignore = "requires official Minecraft 26.3 data"]
fn official_item_behaviors_are_not_silently_treated_as_generic_clicks() {
    let catalog = ItemCatalog::load(&root().join(".cache/vanilla/26.3")).unwrap();
    let stack = |name| {
        ItemStack::new(catalog.item_id(name).unwrap(), 1, ValidatedPatch::default()).unwrap()
    };
    assert!(
        !catalog
            .supports_generic_click(&stack("minecraft:bundle"))
            .unwrap()
    );
    assert!(
        !catalog
            .supports_generic_quick_move(&stack("minecraft:shield"))
            .unwrap()
    );
    assert!(
        catalog
            .supports_generic_click(&stack("minecraft:shield"))
            .unwrap()
    );
    assert!(
        catalog
            .supports_generic_quick_move(&stack("minecraft:stone"))
            .unwrap()
    );
}
