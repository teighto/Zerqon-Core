use crate::{ItemCatalog, ItemError};
use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU8,
    sync::Arc,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ItemId(pub u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ComponentId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentValue {
    MaxStackSize(u8),
    MaxDamage(u32),
    Damage(u32),
    RepairCost(u32),
    Unbreakable,
    CustomName(String),
    ItemName(String),
    ItemModel(String),
    Rarity(u8),
    EnchantmentGlint(bool),
    BlockState(BTreeMap<String, String>),
}

impl ComponentValue {
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::MaxStackSize(value) => serde_json::json!(value),
            Self::MaxDamage(value) | Self::Damage(value) | Self::RepairCost(value) => {
                serde_json::json!(value)
            }
            Self::Unbreakable => serde_json::json!({}),
            Self::CustomName(value) | Self::ItemName(value) | Self::ItemModel(value) => {
                serde_json::json!(value)
            }
            Self::Rarity(value) => serde_json::json!(match value {
                0 => "common",
                1 => "uncommon",
                2 => "rare",
                3 => "epic",
                _ => "invalid",
            }),
            Self::EnchantmentGlint(value) => serde_json::json!(value),
            Self::BlockState(value) => serde_json::json!(value),
        }
    }
    pub fn key(&self) -> &'static str {
        match self {
            Self::MaxStackSize(_) => "minecraft:max_stack_size",
            Self::MaxDamage(_) => "minecraft:max_damage",
            Self::Damage(_) => "minecraft:damage",
            Self::RepairCost(_) => "minecraft:repair_cost",
            Self::Unbreakable => "minecraft:unbreakable",
            Self::CustomName(_) => "minecraft:custom_name",
            Self::ItemName(_) => "minecraft:item_name",
            Self::ItemModel(_) => "minecraft:item_model",
            Self::Rarity(_) => "minecraft:rarity",
            Self::EnchantmentGlint(_) => "minecraft:enchantment_glint_override",
            Self::BlockState(_) => "minecraft:block_state",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidatedPatch {
    pub(crate) added: BTreeMap<ComponentId, ComponentValue>,
    pub(crate) removed: BTreeSet<ComponentId>,
}
impl ValidatedPatch {
    pub fn new(
        added: BTreeMap<ComponentId, ComponentValue>,
        removed: BTreeSet<ComponentId>,
        catalog: &ItemCatalog,
    ) -> Result<Self, ItemError> {
        let patch = Self { added, removed };
        catalog.validate_patch(&patch)?;
        Ok(patch)
    }
    pub fn added(&self) -> &BTreeMap<ComponentId, ComponentValue> {
        &self.added
    }
    pub fn removed(&self) -> &BTreeSet<ComponentId> {
        &self.removed
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemStack {
    item: ItemId,
    count: NonZeroU8,
    patch: Arc<ValidatedPatch>,
}
impl ItemStack {
    pub fn new(item: ItemId, count: u8, patch: ValidatedPatch) -> Result<Self, ItemError> {
        if count > 99 {
            return Err(ItemError::Invalid("count must be 1..99"));
        }
        let count = NonZeroU8::new(count)
            .ok_or(ItemError::Invalid("nonempty stack requires positive count"))?;
        Ok(Self {
            item,
            count,
            patch: Arc::new(patch),
        })
    }
    pub fn item(&self) -> ItemId {
        self.item
    }
    pub fn count(&self) -> u8 {
        self.count.get()
    }
    pub fn patch(&self) -> &ValidatedPatch {
        &self.patch
    }
    pub fn with_count(&self, count: u8) -> Result<Self, ItemError> {
        if !(1..=99).contains(&count) {
            return Err(ItemError::Invalid("count must be 1..99"));
        }
        let count = NonZeroU8::new(count)
            .ok_or(ItemError::Invalid("nonempty stack requires positive count"))?;
        Ok(Self {
            item: self.item,
            count,
            patch: Arc::clone(&self.patch),
        })
    }
    pub fn same_kind(&self, other: &Self) -> bool {
        self.item == other.item && self.patch == other.patch
    }
}
pub type Slot = Option<ItemStack>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashedStack {
    Empty,
    Item {
        item: ItemId,
        count: u8,
        added: BTreeMap<ComponentId, i32>,
        removed: BTreeSet<ComponentId>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickMode {
    Pickup,
    QuickMove,
    Swap,
    Clone,
    Throw,
    QuickCraft,
    PickupAll,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickRequest {
    pub container_id: i32,
    pub state_id: i32,
    pub slot: i16,
    pub button: i8,
    pub mode: ClickMode,
    pub changed_slots: Vec<(i16, HashedStack)>,
    pub carried: HashedStack,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreativeSlotRequest {
    pub slot: i16,
    pub stack: Slot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hand {
    Main,
    Off,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Face {
    Down,
    Up,
    North,
    South,
    West,
    East,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockPosition {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerAction {
    StartDestroyBlock,
    ChangeDestroyDirection,
    AbortDestroyBlock,
    StopDestroyBlock,
    DropAllItems,
    DropItem,
    ReleaseUseItem,
    SwapItemWithOffhand,
    Stab,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerActionRequest {
    pub action: PlayerAction,
    pub block: BlockPosition,
    pub face: Face,
    pub sequence: u32,
}
#[derive(Debug, Clone, PartialEq)]
pub struct UseOnRequest {
    pub hand: Hand,
    pub block: BlockPosition,
    pub face: Face,
    pub hit: [f32; 3],
    pub inside: bool,
    pub world_border_hit: bool,
    pub sequence: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct DecodeLimits {
    pub max_packet_bytes: usize,
    pub max_components: usize,
    pub max_component_bytes: usize,
    pub max_text_bytes: usize,
}
impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_packet_bytes: 65_536,
            max_components: 256,
            max_component_bytes: 16_384,
            max_text_bytes: 4096,
        }
    }
}
