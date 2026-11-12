use crate::Result;
use serde::Deserialize;
use std::{collections::BTreeMap, sync::OnceLock};
use zerqon_inventory::{ItemCatalog, ItemStack, Slot, ValidatedPatch};

#[derive(Deserialize)]
struct Definition {
    hardness: f32,
    requires_tool: bool,
    tool: Option<String>,
    #[serde(default)]
    incorrect: u8,
}
static BLOCKS: OnceLock<BTreeMap<String, Definition>> = OnceLock::new();

pub struct Profile {
    pub ticks: u32,
    pub drop: Slot,
    pub wear: bool,
}

pub fn profile(block: &str, held: &Slot, catalog: &ItemCatalog) -> Result<Profile> {
    let blocks = BLOCKS.get_or_init(|| {
        serde_json::from_str(include_str!(
            "../crates/zerqon-blocks/assets/mining-26.3.json"
        ))
        .expect("embedded official mining definitions")
    });
    let definition = blocks.get(block).ok_or("Unknown block mining properties")?;
    if definition.hardness < 0.0 {
        return Err("This block cannot be mined in Survival".into());
    }
    let tool = held
        .as_ref()
        .map(|stack| catalog.item(stack.item()))
        .transpose()?;
    let name = tool.map_or("", |tool| {
        tool.key.strip_prefix("minecraft:").unwrap_or(&tool.key)
    });
    let material = [
        "wooden",
        "golden",
        "stone",
        "copper",
        "iron",
        "diamond",
        "netherite",
    ]
    .iter()
    .position(|prefix| name.starts_with(&format!("{prefix}_")));
    let matches = definition
        .tool
        .as_ref()
        .is_some_and(|kind| name.ends_with(&format!("_{kind}")));
    let correct = matches && material.is_some_and(|tier| definition.incorrect & (1 << tier) == 0);
    let harvest = !definition.requires_tool || correct;
    let speed = if matches {
        material.map_or(1.0, |tier| [2.0, 12.0, 4.0, 5.0, 6.0, 8.0, 9.0][tier])
    } else {
        1.0
    };
    let ticks = if definition.hardness == 0.0 {
        0
    } else {
        (definition.hardness * f32::from(if harvest { 30u8 } else { 100u8 }) / speed).ceil() as u32
    };
    let key = block.strip_prefix("minecraft:").unwrap_or(block);
    let drop = if !harvest
        || key.ends_with("_leaves")
        || matches!(
            key,
            "glass" | "glass_pane" | "ice" | "fire" | "tall_grass" | "short_grass"
        ) {
        None
    } else {
        let key = match key {
            "stone" => "cobblestone",
            "deepslate" => "cobbled_deepslate",
            "grass_block" | "podzol" | "mycelium" | "dirt_path" => "dirt",
            "coal_ore" | "deepslate_coal_ore" => "coal",
            "iron_ore" | "deepslate_iron_ore" => "raw_iron",
            "copper_ore" | "deepslate_copper_ore" => "raw_copper",
            "gold_ore" | "deepslate_gold_ore" => "raw_gold",
            "diamond_ore" | "deepslate_diamond_ore" => "diamond",
            "emerald_ore" | "deepslate_emerald_ore" => "emerald",
            "redstone_ore" | "deepslate_redstone_ore" => "redstone",
            "lapis_ore" | "deepslate_lapis_ore" => "lapis_lazuli",
            other => other,
        };
        catalog
            .item_id(&format!("minecraft:{key}"))
            .map(|id| ItemStack::new(id, 1, ValidatedPatch::default()))
            .transpose()?
    };
    Ok(Profile {
        ticks,
        drop,
        wear: definition.hardness > 0.0 && tool.is_some_and(|tool| tool.max_damage.is_some()),
    })
}
