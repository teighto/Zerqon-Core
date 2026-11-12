use crate::{ComponentId, ComponentValue, ItemError, ItemId, ItemStack, ValidatedPatch};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::Read,
    path::Path,
};

#[derive(Debug, Clone)]
pub struct ItemDef {
    pub id: ItemId,
    pub key: String,
    pub max_stack: u8,
    pub max_damage: Option<u32>,
}
#[derive(Debug, Clone)]
pub struct ComponentDef {
    pub id: ComponentId,
    pub key: String,
}
#[derive(Debug, Clone)]
pub struct ItemCatalog {
    items: HashMap<ItemId, ItemDef>,
    item_names: HashMap<String, ItemId>,
    components: HashMap<ComponentId, ComponentDef>,
    component_names: HashMap<String, ComponentId>,
    special_click: HashSet<ItemId>,
    equippable: HashSet<ItemId>,
    defaults: HashMap<ItemId, BTreeMap<ComponentId, serde_json::Value>>,
}
impl ItemCatalog {
    pub fn load(directory: &Path) -> Result<Self, ItemError> {
        #[derive(Deserialize)]
        struct Identity {
            version: String,
            protocol: i32,
        }
        #[derive(Deserialize)]
        struct Entry {
            protocol_id: u32,
        }
        #[derive(Deserialize)]
        struct Registry {
            entries: BTreeMap<String, Entry>,
        }
        #[derive(Deserialize)]
        struct Registries {
            #[serde(rename = "minecraft:item")]
            items: Registry,
            #[serde(rename = "minecraft:data_component_type")]
            components: Registry,
        }
        #[derive(Deserialize)]
        struct Defaults {
            components: serde_json::Map<String, serde_json::Value>,
        }
        let identity: Identity =
            serde_json::from_slice(&read_bounded(&directory.join("prepared.json"), 16_384)?)?;
        if identity.version != "26.3" || identity.protocol != 777 {
            return Err(ItemError::Invalid(
                "expected Minecraft 26.3 protocol 777 item data",
            ));
        }
        let registries: Registries = serde_json::from_slice(&read_bounded(
            &directory.join("generated/reports/registries.json"),
            8 * 1024 * 1024,
        )?)?;
        if registries.items.entries.len() > 65_536 || registries.components.entries.len() > 1024 {
            return Err(ItemError::Limit("registry entries"));
        }
        let mut total_bytes = 0usize;
        let mut items = Vec::new();
        let mut special_click = HashSet::new();
        let mut equippable = HashSet::new();
        let mut item_defaults = HashMap::new();
        for (key, entry) in registries.items.entries {
            if !valid_identifier(&key) {
                return Err(ItemError::Invalid("invalid item registry key"));
            }
            let (namespace, path) = key
                .split_once(':')
                .ok_or(ItemError::Invalid("item key needs namespace"))?;
            if matches!(namespace, "." | "..") {
                return Err(ItemError::Invalid("item namespace is not a directory name"));
            }
            let file = directory
                .join("generated/reports")
                .join(namespace)
                .join("components/item")
                .join(format!("{path}.json"));
            let bytes = read_bounded(&file, 1_048_576)?;
            total_bytes = total_bytes
                .checked_add(bytes.len())
                .ok_or(ItemError::Limit("item defaults bytes"))?;
            if total_bytes > 32 * 1024 * 1024 {
                return Err(ItemError::Limit("item defaults bytes"));
            }
            let defaults: Defaults = serde_json::from_slice(&bytes)?;
            let max_stack = match defaults.components.get("minecraft:max_stack_size") {
                None => 1,
                Some(value) => value
                    .as_u64()
                    .filter(|value| (1..=99).contains(value))
                    .ok_or(ItemError::Invalid("invalid default max stack size"))?
                    as u8,
            };
            let max_damage = defaults
                .components
                .get("minecraft:max_damage")
                .map(|value| {
                    value
                        .as_u64()
                        .filter(|value| (1..=i32::MAX as u64).contains(value))
                        .map(|value| value as u32)
                        .ok_or(ItemError::Invalid("invalid default max damage"))
                })
                .transpose()?;
            let id = ItemId(entry.protocol_id);
            if defaults
                .components
                .contains_key("minecraft:bundle_contents")
            {
                special_click.insert(id);
            }
            if defaults.components.contains_key("minecraft:equippable") {
                equippable.insert(id);
            }
            item_defaults.insert(
                id,
                defaults
                    .components
                    .into_iter()
                    .filter(|(key, _)| supported_component(key))
                    .collect::<BTreeMap<_, _>>(),
            );
            items.push(ItemDef {
                id,
                key,
                max_stack,
                max_damage,
            });
        }
        let components = registries
            .components
            .entries
            .into_iter()
            .map(|(key, entry)| ComponentDef {
                id: ComponentId(entry.protocol_id),
                key,
            })
            .collect();
        let mut catalog = Self::from_definitions(items, components)?;
        catalog.special_click = special_click;
        catalog.equippable = equippable;
        for (item, values) in item_defaults {
            let defaults = values
                .into_iter()
                .map(|(key, value)| {
                    Ok((
                        catalog.component_id(&key).ok_or(ItemError::Invalid(
                            "default component missing from registry",
                        ))?,
                        value,
                    ))
                })
                .collect::<Result<_, ItemError>>()?;
            catalog.defaults.insert(item, defaults);
        }
        Ok(catalog)
    }

    pub fn from_definitions(
        items: Vec<ItemDef>,
        components: Vec<ComponentDef>,
    ) -> Result<Self, ItemError> {
        if items.is_empty() || items.len() > 65_536 || components.len() > 1024 {
            return Err(ItemError::Limit("registry entries"));
        }
        let mut result = Self {
            items: HashMap::new(),
            item_names: HashMap::new(),
            components: HashMap::new(),
            component_names: HashMap::new(),
            special_click: HashSet::new(),
            equippable: HashSet::new(),
            defaults: HashMap::new(),
        };
        for item in items {
            if item.id.0 > 65_535
                || !valid_identifier(&item.key)
                || !(1..=99).contains(&item.max_stack)
                || item.max_damage.is_some_and(|damage| {
                    damage == 0 || damage > i32::MAX as u32 || item.max_stack > 1
                })
            {
                return Err(ItemError::Invalid("invalid item definition"));
            }
            if result.items.contains_key(&item.id) || result.item_names.contains_key(&item.key) {
                return Err(ItemError::Invalid("duplicate item definition"));
            }
            result.item_names.insert(item.key.clone(), item.id);
            result.items.insert(item.id, item);
        }
        for component in components {
            if component.id.0 > 65_535 || !valid_identifier(&component.key) {
                return Err(ItemError::Invalid("invalid component definition"));
            }
            if result.components.contains_key(&component.id)
                || result.component_names.contains_key(&component.key)
            {
                return Err(ItemError::Invalid("duplicate component definition"));
            }
            result
                .component_names
                .insert(component.key.clone(), component.id);
            result.components.insert(component.id, component);
        }
        for item in result.items.values() {
            let mut defaults = BTreeMap::new();
            if let Some(id) = result.component_id("minecraft:max_stack_size") {
                defaults.insert(id, serde_json::json!(item.max_stack));
            }
            if let (Some(id), Some(damage)) =
                (result.component_id("minecraft:max_damage"), item.max_damage)
            {
                defaults.insert(id, serde_json::json!(damage));
            }
            result.defaults.insert(item.id, defaults);
        }
        Ok(result)
    }

    pub fn item(&self, id: ItemId) -> Result<&ItemDef, ItemError> {
        self.items.get(&id).ok_or(ItemError::UnknownItem(id.0))
    }
    pub fn item_id(&self, key: &str) -> Option<ItemId> {
        self.item_names.get(key).copied()
    }
    pub fn component(&self, id: ComponentId) -> Result<&ComponentDef, ItemError> {
        self.components
            .get(&id)
            .ok_or(ItemError::UnknownComponent(id.0))
    }
    pub fn component_id(&self, key: &str) -> Option<ComponentId> {
        self.component_names.get(key).copied()
    }
    pub fn item_count(&self) -> usize {
        self.items.len()
    }
    pub fn component_count(&self) -> usize {
        self.components.len()
    }

    pub fn effective_component(
        &self,
        stack: &ItemStack,
        key: &str,
    ) -> Result<Option<serde_json::Value>, ItemError> {
        self.validate_stack(stack)?;
        let Some(id) = self.component_id(key) else {
            return Ok(None);
        };
        if stack.patch().removed.contains(&id) {
            return Ok(None);
        }
        Ok(stack
            .patch()
            .added
            .get(&id)
            .map(ComponentValue::to_json)
            .or_else(|| {
                self.defaults
                    .get(&stack.item())
                    .and_then(|defaults| defaults.get(&id))
                    .cloned()
            }))
    }

    pub fn validate_stack(&self, stack: &ItemStack) -> Result<(), ItemError> {
        let item = self.item(stack.item())?;
        if item.key == "minecraft:air" {
            return Err(ItemError::Invalid("nonempty air stack"));
        }
        self.validate_patch(stack.patch())?;
        let maximum = self.effective_max_stack(stack)?;
        if stack.count() > maximum {
            return Err(ItemError::Invalid("stack count exceeds effective maximum"));
        }
        let max_damage = match self.component_id("minecraft:max_damage") {
            Some(id) if stack.patch().removed.contains(&id) => None,
            Some(id) => match stack.patch().added.get(&id) {
                Some(ComponentValue::MaxDamage(value)) => Some(*value),
                _ => item.max_damage,
            },
            None => item.max_damage,
        };
        if max_damage.is_some() && maximum > 1 {
            return Err(ItemError::Invalid(
                "item cannot be damageable and stackable",
            ));
        }
        Ok(())
    }

    pub fn effective_max_stack(&self, stack: &ItemStack) -> Result<u8, ItemError> {
        let item = self.item(stack.item())?;
        match self.component_id("minecraft:max_stack_size") {
            Some(id) if stack.patch().removed.contains(&id) => Ok(1),
            Some(id) => match stack.patch().added.get(&id) {
                Some(ComponentValue::MaxStackSize(value)) => Ok(*value),
                _ => Ok(item.max_stack),
            },
            None => Ok(item.max_stack),
        }
    }

    pub fn supports_generic_click(&self, stack: &ItemStack) -> Result<bool, ItemError> {
        self.validate_stack(stack)?;
        Ok(!self.special_click.contains(&stack.item()))
    }
    pub fn supports_generic_quick_move(&self, stack: &ItemStack) -> Result<bool, ItemError> {
        Ok(self.supports_generic_click(stack)? && !self.equippable.contains(&stack.item()))
    }

    pub fn same_kind(&self, left: &ItemStack, right: &ItemStack) -> Result<bool, ItemError> {
        self.validate_stack(left)?;
        self.validate_stack(right)?;
        if left.item() != right.item() {
            return Ok(false);
        }
        if left.same_kind(right) {
            return Ok(true);
        }
        let ids: std::collections::BTreeSet<_> = left
            .patch()
            .added
            .keys()
            .chain(&left.patch().removed)
            .chain(right.patch().added.keys())
            .chain(&right.patch().removed)
            .copied()
            .collect();
        for id in ids {
            let value = |stack: &ItemStack| {
                if stack.patch().removed.contains(&id) {
                    None
                } else if let Some(value) = stack.patch().added.get(&id) {
                    Some(value.to_json())
                } else {
                    self.defaults
                        .get(&stack.item())
                        .and_then(|defaults| defaults.get(&id))
                        .cloned()
                }
            };
            if value(left) != value(right) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) fn validate_patch(&self, patch: &ValidatedPatch) -> Result<(), ItemError> {
        if patch.added.len() + patch.removed.len() > self.component_count().min(256) {
            return Err(ItemError::Limit("item component count"));
        }
        for (id, value) in &patch.added {
            let definition = self.component(*id)?;
            if !supported_component(&definition.key) {
                return Err(ItemError::UnsupportedComponent(definition.key.clone()));
            }
            if definition.key != value.key() || patch.removed.contains(id) {
                return Err(ItemError::Invalid(
                    "component type mismatch or add/remove overlap",
                ));
            }
            validate_value(value)?;
        }
        for id in &patch.removed {
            let definition = self.component(*id)?;
            if !supported_component(&definition.key) {
                return Err(ItemError::UnsupportedComponent(definition.key.clone()));
            }
        }
        Ok(())
    }
}

pub(crate) fn supported_component(key: &str) -> bool {
    matches!(
        key,
        "minecraft:max_stack_size"
            | "minecraft:max_damage"
            | "minecraft:damage"
            | "minecraft:repair_cost"
            | "minecraft:unbreakable"
            | "minecraft:custom_name"
            | "minecraft:item_name"
            | "minecraft:item_model"
            | "minecraft:rarity"
            | "minecraft:enchantment_glint_override"
            | "minecraft:block_state"
    )
}

pub(crate) fn valid_identifier(key: &str) -> bool {
    let Some((namespace, path)) = key.split_once(':') else {
        return false;
    };
    !namespace.is_empty()
        && !path.is_empty()
        && key.len() <= 256
        && namespace.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
        })
        && path.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"/_.-".contains(&byte)
        })
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

fn validate_value(value: &ComponentValue) -> Result<(), ItemError> {
    let valid = match value {
        ComponentValue::MaxStackSize(value) => (1..=99).contains(value),
        ComponentValue::MaxDamage(value) => (1..=i32::MAX as u32).contains(value),
        ComponentValue::Damage(value) | ComponentValue::RepairCost(value) => {
            *value <= i32::MAX as u32
        }
        ComponentValue::CustomName(value) | ComponentValue::ItemName(value) => value.len() <= 4096,
        ComponentValue::ItemModel(value) => valid_identifier(value),
        ComponentValue::Rarity(value) => *value <= 3,
        ComponentValue::Unbreakable | ComponentValue::EnchantmentGlint(_) => true,
        ComponentValue::BlockState(values) => {
            values.len() <= 32
                && values.iter().all(|(name, value)| {
                    !name.is_empty()
                        && name.len() <= 64
                        && value.len() <= 64
                        && !value.is_empty()
                        && name.is_ascii()
                        && value.is_ascii()
                })
        }
    };
    if valid {
        Ok(())
    } else {
        Err(ItemError::Invalid(
            "component value outside supported domain",
        ))
    }
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, ItemError> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(ItemError::Limit("game data file"));
    }
    Ok(bytes)
}
