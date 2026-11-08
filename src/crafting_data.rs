use aws_lc_rs::digest::{SHA256, digest};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use zerqon_inventory::{
    CraftingRecipes, CraftingRemainders, ItemCatalog, ItemId, ItemStack, ValidatedPatch,
};

const REMAINDERS: &[u8] = include_bytes!("../tools/crafting-data/26.3.json");
const REMAINDERS_SHA256: &str = "bc627336ec9e71cbe791e524d9ce057c7641530d5c43449c17d2d3fee7ffd797";

pub struct CraftingData {
    pub recipes: CraftingRecipes,
    pub remainders: CraftingRemainders,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemainderFacts {
    schema: u32,
    minecraft: String,
    protocol: i32,
    data_version: i32,
    bundle_sha1: String,
    server_sha256: String,
    item_count: usize,
    remainders: Vec<(String, String, u8)>,
}

impl CraftingData {
    pub fn load(directory: &Path, catalog: &ItemCatalog) -> Result<Self, CraftingDataError> {
        let recipes = crate::bootstrap::load_prepared_recipes(directory)?;
        let sync =
            crate::bootstrap::read_json(&directory.join("tags-sync.json"), 16 * 1024 * 1024)?;
        let tags = item_tags(catalog, &sync)?;
        let checksum = digest(&SHA256, REMAINDERS)
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if checksum != REMAINDERS_SHA256 {
            return Err(CraftingDataError::Invalid(
                "embedded remainder checksum differs",
            ));
        }
        let facts: RemainderFacts = serde_json::from_slice(REMAINDERS)?;
        if facts.schema != 1
            || facts.minecraft != "26.3"
            || facts.protocol != 777
            || facts.data_version != 5023
            || facts.bundle_sha1 != crate::bootstrap::SERVER_SHA1
            || facts.server_sha256
                != "a362163eec5d1612d520772bc16e5b39c09e3b234fdc045f56bf544284ee8ae6"
            || facts.item_count != catalog.item_count()
        {
            return Err(CraftingDataError::Invalid(
                "embedded remainder identity differs from the item catalog",
            ));
        }
        Ok(Self {
            recipes: CraftingRecipes::from_json(catalog, &recipes, &tags)?,
            remainders: compile_remainders(catalog, &facts.remainders)?,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CraftingDataError {
    #[error("invalid crafting data: {0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Item(#[from] zerqon_inventory::ItemError),
    #[error(transparent)]
    Recipes(#[from] zerqon_inventory::CraftingError),
    #[error(transparent)]
    Bootstrap(#[from] crate::bootstrap::BootstrapError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

fn item_tags(
    catalog: &ItemCatalog,
    sync: &Value,
) -> Result<BTreeMap<String, Value>, CraftingDataError> {
    let registries = sync["registries"]
        .as_array()
        .filter(|values| values.len() <= 1024)
        .ok_or(CraftingDataError::Invalid(
            "missing or oversized tag registries",
        ))?;
    let mut matches = registries
        .iter()
        .filter(|registry| registry["name"] == "minecraft:item");
    let registry = matches
        .next()
        .ok_or(CraftingDataError::Invalid("missing item tags"))?;
    if matches.next().is_some() {
        return Err(CraftingDataError::Invalid("duplicate item tag registry"));
    }
    let tags = registry["tags"]
        .as_array()
        .filter(|tags| tags.len() <= 4096)
        .ok_or(CraftingDataError::Invalid("invalid item tag count"))?;
    let mut total = 0usize;
    let mut result = BTreeMap::new();
    for tag in tags {
        let name = tag["name"]
            .as_str()
            .filter(|name| name.len() <= 256)
            .ok_or(CraftingDataError::Invalid("invalid item tag name"))?;
        let ids = tag["ids"]
            .as_array()
            .ok_or(CraftingDataError::Invalid("invalid item tag members"))?;
        total = total
            .checked_add(ids.len())
            .filter(|total| *total <= 65_536)
            .ok_or(CraftingDataError::Invalid("too many item tag members"))?;
        let mut seen = BTreeSet::new();
        let mut members = Vec::with_capacity(ids.len());
        for value in ids {
            let id = value
                .as_u64()
                .and_then(|value| u16::try_from(value).ok())
                .ok_or(CraftingDataError::Invalid("invalid item tag ID"))?;
            if !seen.insert(id) {
                return Err(CraftingDataError::Invalid("duplicate item tag member"));
            }
            members.push(catalog.item(ItemId(u32::from(id)))?.key.clone());
        }
        if result
            .insert(name.to_owned(), serde_json::json!({"values":members}))
            .is_some()
        {
            return Err(CraftingDataError::Invalid("duplicate item tag"));
        }
    }
    Ok(result)
}

fn compile_remainders(
    catalog: &ItemCatalog,
    rows: &[(String, String, u8)],
) -> Result<CraftingRemainders, CraftingDataError> {
    if rows.len() > catalog.item_count() {
        return Err(CraftingDataError::Invalid("too many crafting remainders"));
    }
    let mut definitions = BTreeMap::new();
    for (source, target, count) in rows {
        let source = catalog
            .item_id(source)
            .ok_or(CraftingDataError::Invalid("unknown remainder input"))?;
        let target = catalog
            .item_id(target)
            .ok_or(CraftingDataError::Invalid("unknown remainder output"))?;
        let stack = ItemStack::new(target, *count, ValidatedPatch::default())?;
        if definitions.insert(source, stack).is_some() {
            return Err(CraftingDataError::Invalid("duplicate crafting remainder"));
        }
    }
    Ok(CraftingRemainders::from_definitions(catalog, definitions)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zerqon_inventory::{ItemDef, ItemId};

    fn catalog() -> ItemCatalog {
        ItemCatalog::from_definitions(
            [
                (1, "minecraft:honey_bottle", 16),
                (2, "minecraft:glass_bottle", 64),
                (3, "minecraft:oak_log", 64),
            ]
            .into_iter()
            .map(|(id, key, max_stack)| ItemDef {
                id: ItemId(id),
                key: key.into(),
                max_stack,
                max_damage: None,
            })
            .collect(),
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    #[ignore = "requires official 26.3 data with prepared recipes"]
    fn prepared_real_catalog_builds_matching_recipes_and_original_remainders() {
        let directory = std::env::var_os("ZERQON_TEST_DATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/vanilla/26.3"));
        let catalog = ItemCatalog::load(&directory).unwrap();
        let data = CraftingData::load(&directory, &catalog).unwrap();
        assert_eq!(data.recipes.report().supported.len(), 478);
        assert_eq!(data.recipes.report().excluded.len(), 1564);
        let log = catalog.item_id("minecraft:oak_log").unwrap();
        let grid = [
            Some(ItemStack::new(log, 2, ValidatedPatch::default()).unwrap()),
            None,
            None,
            None,
        ];
        let preview = data.recipes.find_match(&grid, &catalog).unwrap().unwrap();
        assert_eq!(
            catalog.item(preview.output.item()).unwrap().key,
            "minecraft:oak_planks"
        );
        assert_eq!(preview.output.count(), 4);
        let honey = catalog.item_id("minecraft:honey_bottle").unwrap();
        let remainder = data.remainders.get(honey).unwrap();
        assert_eq!(
            catalog.item(remainder.item()).unwrap().key,
            "minecraft:glass_bottle"
        );
        assert_eq!(remainder.count(), 1);
    }

    #[test]
    fn synchronized_item_tags_resolve_authoritative_registry_ids() {
        let catalog = catalog();
        let sync = json!({"registries":[
            {"name":"minecraft:block","tags":[{"name":"minecraft:unrelated","ids":[9999]}]},
            {"name":"minecraft:item","tags":[{"name":"minecraft:logs","ids":[3]}]}
        ]});
        let tags = item_tags(&catalog, &sync).unwrap();
        assert_eq!(
            tags,
            BTreeMap::from([(
                "minecraft:logs".into(),
                json!({"values":["minecraft:oak_log"]})
            )])
        );
    }

    #[test]
    fn invalid_or_duplicate_item_tag_bindings_are_rejected() {
        let catalog = catalog();
        for tags in [
            json!([{"name":"minecraft:logs","ids":[999]}]),
            json!([{"name":"minecraft:logs","ids":[-1]}]),
            json!([{"name":"minecraft:logs","ids":[3.0]}]),
            json!([{"name":"minecraft:logs","ids":[3,3]}]),
            json!([{"name":"minecraft:logs","ids":[3]},{"name":"minecraft:logs","ids":[3]}]),
        ] {
            assert!(
                item_tags(
                    &catalog,
                    &json!({"registries":[{"name":"minecraft:item","tags":tags}]})
                )
                .is_err()
            );
        }
        let duplicate = json!({"registries":[{"name":"minecraft:item","tags":[]},{"name":"minecraft:item","tags":[]}]});
        assert!(item_tags(&catalog, &duplicate).is_err());
        assert!(item_tags(&catalog, &json!({"registries":[]})).is_err());
    }

    #[test]
    fn item_remainders_use_typed_default_output_without_input_components() {
        let catalog = catalog();
        let remainders = compile_remainders(
            &catalog,
            &[(
                "minecraft:honey_bottle".into(),
                "minecraft:glass_bottle".into(),
                1,
            )],
        )
        .unwrap();
        let output = remainders.get(ItemId(1)).unwrap();
        assert_eq!(output.item(), ItemId(2));
        assert_eq!(output.count(), 1);
        assert!(remainders.get(ItemId(3)).is_none());
    }

    #[test]
    fn remainder_definitions_reject_unknown_items_duplicates_and_invalid_counts() {
        let catalog = catalog();
        for rows in [
            vec![(
                "minecraft:missing".into(),
                "minecraft:glass_bottle".into(),
                1,
            )],
            vec![(
                "minecraft:honey_bottle".into(),
                "minecraft:missing".into(),
                1,
            )],
            vec![(
                "minecraft:honey_bottle".into(),
                "minecraft:glass_bottle".into(),
                0,
            )],
            vec![(
                "minecraft:honey_bottle".into(),
                "minecraft:glass_bottle".into(),
                65,
            )],
            vec![
                (
                    "minecraft:honey_bottle".into(),
                    "minecraft:glass_bottle".into(),
                    1,
                ),
                (
                    "minecraft:honey_bottle".into(),
                    "minecraft:glass_bottle".into(),
                    1,
                ),
            ],
        ] {
            assert!(compile_remainders(&catalog, &rows).is_err());
        }
    }
}
