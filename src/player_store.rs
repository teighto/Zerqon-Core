use aws_lc_rs::digest::{Context, SHA256};
use serde::{
    Deserialize, Serialize,
    de::{self, SeqAccess, Visitor},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;
use zerqon_inventory::{
    ComponentValue, ItemCatalog, ItemStack, MENU_SLOT_COUNT, PlayerInventory, Slot, ValidatedPatch,
};

const MAGIC: &[u8; 8] = b"ZINV\0\0\0\x01";
const HEADER_BYTES: usize = 44;
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("invalid player inventory file: {0}")]
    Invalid(&'static str),
    #[error("player inventory exceeds its size limit")]
    Limit,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Item(#[from] zerqon_inventory::ItemError),
    #[error(transparent)]
    Inventory(#[from] zerqon_inventory::InventoryError),
}

pub struct PlayerStore {
    directory: PathBuf,
    catalog: Arc<ItemCatalog>,
}

impl PlayerStore {
    pub fn new(directory: PathBuf, catalog: Arc<ItemCatalog>) -> Result<Self, StoreError> {
        reject_inventory_links(&directory)?;
        fs::create_dir_all(&directory)?;
        reject_inventory_links(&directory)?;
        if !directory.is_dir() {
            return Err(StoreError::Invalid("playerdata path is not a directory"));
        }
        let directory = directory.canonicalize()?;
        reject_inventory_links(&directory)?;
        Ok(Self { directory, catalog })
    }

    #[cfg(test)]
    pub fn load(&self, uuid: Uuid) -> Result<PlayerInventory, StoreError> {
        Ok(self.load_existing(uuid)?.unwrap_or_default())
    }

    pub fn load_existing(&self, uuid: Uuid) -> Result<Option<PlayerInventory>, StoreError> {
        let path = self.path(uuid)?;
        let Some(bytes) = read_file(&path)? else {
            return Ok(None);
        };
        decode_file(&bytes)?.restore(uuid, &self.catalog).map(Some)
    }

    /// The caller serializes load/save operations for each UUID.
    #[cfg(test)]
    pub fn save(&self, uuid: Uuid, inventory: &PlayerInventory) -> Result<(), StoreError> {
        let path = self.path(uuid)?;
        self.load(uuid)?;
        let data = StoredInventory::capture(uuid, inventory, &self.catalog)?;
        let payload = serialize_bounded(&data)?;
        reject_inventory_links(&path)?;
        #[cfg(unix)]
        let directory = File::open(&self.directory)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)?;
        reject_inventory_links(temporary.path())?;
        write_file(temporary.as_file_mut(), &payload)?;
        temporary.as_file().sync_all()?;
        reject_inventory_links(&path)?;
        temporary
            .persist(path)
            .map_err(|error| StoreError::Io(error.error))?;
        // After replacement the new inventory is committed; reporting failure would roll RAM back.
        #[cfg(unix)]
        if let Err(error) = directory.sync_all() {
            tracing::warn!(%error, %uuid, "Player inventory committed, but directory durability could not be confirmed");
        }
        Ok(())
    }

    fn path(&self, uuid: Uuid) -> Result<PathBuf, StoreError> {
        if uuid.is_nil() {
            return Err(StoreError::Invalid("nil player UUID"));
        }
        let path = self.directory.join(format!("{uuid}.zinv"));
        reject_inventory_links(&path)?;
        Ok(path)
    }
}

fn reject_inventory_links(path: &Path) -> Result<(), StoreError> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        if !matches!(
            component,
            std::path::Component::Normal(_) | std::path::Component::ParentDir
        ) {
            continue;
        }
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        #[cfg(windows)]
        let linked = {
            use std::os::windows::fs::MetadataExt;
            metadata.file_attributes() & 0x400 != 0
        };
        #[cfg(not(windows))]
        let linked = metadata.file_type().is_symlink();
        if linked {
            return Err(StoreError::Invalid(
                "inventory ancestry contains a link or reparse point",
            ));
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredInventory {
    schema: u32,
    minecraft_version: String,
    protocol: u32,
    uuid: Uuid,
    selected: u8,
    #[serde(deserialize_with = "slots")]
    slots: Vec<Option<StoredStack>>,
    cursor: Option<StoredStack>,
}

impl StoredInventory {
    pub(crate) fn schema(&self) -> u32 {
        self.schema
    }

    pub(crate) fn capture(
        uuid: Uuid,
        inventory: &PlayerInventory,
        catalog: &ItemCatalog,
    ) -> Result<Self, StoreError> {
        if uuid.is_nil() {
            return Err(StoreError::Invalid("nil player UUID"));
        }
        Ok(Self {
            schema: 2,
            minecraft_version: "26.3".into(),
            protocol: 777,
            uuid,
            selected: inventory.selected(),
            slots: inventory
                .slots()
                .iter()
                .enumerate()
                .map(|(index, slot)| {
                    if index == 0 {
                        Ok(None)
                    } else {
                        encode_stack(slot, catalog)
                    }
                })
                .collect::<Result<_, _>>()?,
            cursor: encode_stack(inventory.cursor(), catalog)?,
        })
    }

    pub(crate) fn restore(
        self,
        uuid: Uuid,
        catalog: &ItemCatalog,
    ) -> Result<PlayerInventory, StoreError> {
        if !matches!(self.schema, 1 | 2) || self.minecraft_version != "26.3" || self.protocol != 777
        {
            return Err(StoreError::Invalid(
                "unsupported inventory schema or Minecraft version",
            ));
        }
        if uuid.is_nil() || self.uuid != uuid {
            return Err(StoreError::Invalid(
                "embedded UUID differs from player identity",
            ));
        }
        if self.slots.len() != MENU_SLOT_COUNT {
            return Err(StoreError::Invalid(
                "inventory must have exactly 46 menu slots",
            ));
        }
        if self.slots[0].is_some() {
            return Err(StoreError::Invalid(
                "saved crafting result requires explicit recovery; original file preserved",
            ));
        }
        let mut inventory = PlayerInventory::new();
        for (index, stack) in self.slots.into_iter().enumerate() {
            inventory.set_slot(catalog, index as i16, decode_stack(stack, catalog)?)?;
        }
        inventory.set_cursor(catalog, decode_stack(self.cursor, catalog)?)?;
        inventory.select_hotbar(i16::from(self.selected))?;
        Ok(inventory)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredStack {
    item: String,
    count: u8,
    #[serde(deserialize_with = "components")]
    added: Vec<StoredAdded>,
    #[serde(deserialize_with = "removals")]
    removed: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAdded {
    component: String,
    value: StoredComponent,
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum StoredComponent {
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
    BlockState(#[serde(deserialize_with = "properties")] BTreeMap<String, String>),
}

impl From<&ComponentValue> for StoredComponent {
    fn from(value: &ComponentValue) -> Self {
        match value {
            ComponentValue::MaxStackSize(value) => Self::MaxStackSize(*value),
            ComponentValue::MaxDamage(value) => Self::MaxDamage(*value),
            ComponentValue::Damage(value) => Self::Damage(*value),
            ComponentValue::RepairCost(value) => Self::RepairCost(*value),
            ComponentValue::Unbreakable => Self::Unbreakable,
            ComponentValue::CustomName(value) => Self::CustomName(value.clone()),
            ComponentValue::ItemName(value) => Self::ItemName(value.clone()),
            ComponentValue::ItemModel(value) => Self::ItemModel(value.clone()),
            ComponentValue::Rarity(value) => Self::Rarity(*value),
            ComponentValue::EnchantmentGlint(value) => Self::EnchantmentGlint(*value),
            ComponentValue::BlockState(value) => Self::BlockState(value.clone()),
        }
    }
}
impl From<StoredComponent> for ComponentValue {
    fn from(value: StoredComponent) -> Self {
        match value {
            StoredComponent::MaxStackSize(value) => Self::MaxStackSize(value),
            StoredComponent::MaxDamage(value) => Self::MaxDamage(value),
            StoredComponent::Damage(value) => Self::Damage(value),
            StoredComponent::RepairCost(value) => Self::RepairCost(value),
            StoredComponent::Unbreakable => Self::Unbreakable,
            StoredComponent::CustomName(value) => Self::CustomName(value),
            StoredComponent::ItemName(value) => Self::ItemName(value),
            StoredComponent::ItemModel(value) => Self::ItemModel(value),
            StoredComponent::Rarity(value) => Self::Rarity(value),
            StoredComponent::EnchantmentGlint(value) => Self::EnchantmentGlint(value),
            StoredComponent::BlockState(value) => Self::BlockState(value),
        }
    }
}

pub(crate) fn encode_stack(
    slot: &Slot,
    catalog: &ItemCatalog,
) -> Result<Option<StoredStack>, StoreError> {
    let Some(stack) = slot else {
        return Ok(None);
    };
    catalog.validate_stack(stack)?;
    let mut added = stack
        .patch()
        .added()
        .iter()
        .map(|(id, value)| {
            Ok(StoredAdded {
                component: catalog.component(*id)?.key.clone(),
                value: value.into(),
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    added.sort_by(|left, right| left.component.cmp(&right.component));
    let mut removed = stack
        .patch()
        .removed()
        .iter()
        .map(|id| Ok(catalog.component(*id)?.key.clone()))
        .collect::<Result<Vec<_>, StoreError>>()?;
    removed.sort();
    Ok(Some(StoredStack {
        item: catalog.item(stack.item())?.key.clone(),
        count: stack.count(),
        added,
        removed,
    }))
}

pub(crate) fn decode_stack(
    slot: Option<StoredStack>,
    catalog: &ItemCatalog,
) -> Result<Slot, StoreError> {
    let Some(stack) = slot else {
        return Ok(None);
    };
    if stack.added.len() + stack.removed.len() > 256 {
        return Err(StoreError::Limit);
    }
    let item = catalog
        .item_id(&stack.item)
        .ok_or(StoreError::Invalid("unknown saved item identifier"))?;
    let mut added = BTreeMap::new();
    for component in stack.added {
        let id = catalog
            .component_id(&component.component)
            .ok_or(StoreError::Invalid("unknown saved component identifier"))?;
        if added.insert(id, component.value.into()).is_some() {
            return Err(StoreError::Invalid("duplicate added component"));
        }
    }
    let mut removed = BTreeSet::new();
    for component in stack.removed {
        let id = catalog
            .component_id(&component)
            .ok_or(StoreError::Invalid("unknown saved component identifier"))?;
        if !removed.insert(id) {
            return Err(StoreError::Invalid("duplicate removed component"));
        }
    }
    let patch = ValidatedPatch::new(added, removed, catalog)?;
    let stack = ItemStack::new(item, stack.count, patch)?;
    catalog.validate_stack(&stack)?;
    Ok(Some(stack))
}

fn slots<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Option<StoredStack>>, D::Error> {
    bounded_vec::<_, _, 46>(deserializer)
}
fn components<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<StoredAdded>, D::Error> {
    bounded_vec::<_, _, 256>(deserializer)
}
fn removals<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    bounded_vec::<_, _, 256>(deserializer)
}

fn bounded_vec<'de, D, T, const MAX: usize>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T, const MAX: usize>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const MAX: usize> Visitor<'de> for Bounded<T, MAX> {
        type Value = Vec<T>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "at most {MAX} entries")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = sequence.next_element()? {
                if values.len() == MAX {
                    return Err(de::Error::custom("collection exceeds inventory bound"));
                }
                values.push(value);
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Bounded::<T, MAX>(std::marker::PhantomData))
}

fn properties<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    struct Properties;
    impl<'de> Visitor<'de> for Properties {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("at most 32 unique block properties")
        }
        fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut properties = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, String>()? {
                if properties.len() == 32 || properties.insert(key, value).is_some() {
                    return Err(de::Error::custom("duplicate or excessive block properties"));
                }
            }
            Ok(properties)
        }
    }
    deserializer.deserialize_map(Properties)
}

fn read_file(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    reject_inventory_links(path)?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() {
        return Err(StoreError::Invalid("inventory path must be a regular file"));
    }
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(StoreError::Limit);
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    reject_inventory_links(path)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(StoreError::Limit);
    }
    Ok(Some(bytes))
}

fn decode_file(bytes: &[u8]) -> Result<StoredInventory, StoreError> {
    if bytes.len() < HEADER_BYTES || &bytes[..8] != MAGIC {
        return Err(StoreError::Invalid("header"));
    }
    let length = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    if length != bytes.len() - HEADER_BYTES {
        return Err(StoreError::Invalid("payload length"));
    }
    let payload = &bytes[HEADER_BYTES..];
    let mut digest = Context::new(&SHA256);
    digest.update(&bytes[..12]);
    digest.update(payload);
    if digest.finish().as_ref() != &bytes[12..HEADER_BYTES] {
        return Err(StoreError::Invalid("checksum"));
    }
    Ok(serde_json::from_slice(payload)?)
}

#[cfg(test)]
fn serialize_bounded(data: &StoredInventory) -> Result<Vec<u8>, StoreError> {
    serialize_payload(data, MAX_FILE_BYTES - HEADER_BYTES)
}

pub(crate) fn serialize_payload(
    data: &impl Serialize,
    limit: usize,
) -> Result<Vec<u8>, StoreError> {
    struct BoundedWriter {
        bytes: Vec<u8>,
        failed: bool,
        limit: usize,
    }
    impl Write for BoundedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                self.failed = true;
                return Err(io::Error::other("inventory payload limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = BoundedWriter {
        bytes: Vec::new(),
        failed: false,
        limit,
    };
    let result = serde_json::to_writer(&mut writer, data);
    if writer.failed {
        return Err(StoreError::Limit);
    }
    result?;
    Ok(writer.bytes)
}

#[cfg(test)]
fn write_file(writer: &mut impl Write, payload: &[u8]) -> Result<(), StoreError> {
    if payload.len() > MAX_FILE_BYTES - HEADER_BYTES {
        return Err(StoreError::Limit);
    }
    let length = (payload.len() as u32).to_be_bytes();
    let mut digest = Context::new(&SHA256);
    digest.update(MAGIC);
    digest.update(&length);
    digest.update(payload);
    writer.write_all(MAGIC)?;
    writer.write_all(&length)?;
    writer.write_all(digest.finish().as_ref())?;
    writer.write_all(payload)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn directory_link(target: &Path, path: &Path) {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let script = format!(
                "New-Item -ItemType Junction -Path '{}' -Target '{}' | Out-Null",
                path.display().to_string().replace('\'', "''"),
                target.display().to_string().replace('\'', "''"),
            );
            let result = std::process::Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .creation_flags(0x08000000)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, path).unwrap();
    }

    #[test]
    fn linked_store_root_or_ancestor_is_rejected_before_creating_files() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("original"), b"preserve").unwrap();
        let alias = root.path().join("alias");
        directory_link(&outside, &alias);
        let catalog = catalog(1, 6);
        for path in [alias.clone(), alias.join("nested/playerdata")] {
            assert!(PlayerStore::new(path, Arc::clone(&catalog)).is_err());
        }
        assert_eq!(fs::read(outside.join("original")).unwrap(), b"preserve");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    }

    #[test]
    fn directory_substitution_after_open_cannot_read_or_replace_external_inventory() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("playerdata");
        let outside = root.path().join("outside");
        let retained = root.path().join("retained");
        let catalog = catalog(1, 6);
        let store = PlayerStore::new(directory.clone(), Arc::clone(&catalog)).unwrap();
        let external = PlayerStore::new(outside.clone(), Arc::clone(&catalog)).unwrap();
        let player = Uuid::from_u128(12);
        store.save(player, &inventory(&catalog)).unwrap();
        external.save(player, &PlayerInventory::new()).unwrap();
        let filename = format!("{player}.zinv");
        let original = fs::read(directory.join(&filename)).unwrap();
        let external_original = fs::read(outside.join(&filename)).unwrap();
        fs::rename(&directory, &retained).unwrap();
        directory_link(&outside, &directory);
        assert!(store.load(player).is_err());
        assert!(store.save(player, &inventory(&catalog)).is_err());
        assert_eq!(
            fs::read(outside.join(&filename)).unwrap(),
            external_original
        );
        assert_eq!(fs::read(retained.join(filename)).unwrap(), original);
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    }
    use zerqon_inventory::{
        ComponentDef, ComponentId, ComponentValue, ItemDef, ItemId, ItemStack, ValidatedPatch,
    };

    fn catalog(item_id: u32, component_id: u32) -> Arc<ItemCatalog> {
        Arc::new(
            ItemCatalog::from_definitions(
                vec![ItemDef {
                    id: ItemId(item_id),
                    key: "minecraft:stone".into(),
                    max_stack: 64,
                    max_damage: None,
                }],
                vec![ComponentDef {
                    id: ComponentId(component_id),
                    key: "minecraft:custom_name".into(),
                }],
            )
            .unwrap(),
        )
    }

    fn inventory(catalog: &ItemCatalog) -> PlayerInventory {
        let name = catalog.component_id("minecraft:custom_name").unwrap();
        let patch = ValidatedPatch::new(
            BTreeMap::from([(name, ComponentValue::CustomName("Привет 😀".into()))]),
            BTreeSet::new(),
            catalog,
        )
        .unwrap();
        let stack = ItemStack::new(catalog.item_id("minecraft:stone").unwrap(), 17, patch).unwrap();
        let mut inventory = PlayerInventory::new();
        inventory
            .set_slot(catalog, 36, Some(stack.clone()))
            .unwrap();
        inventory
            .set_cursor(catalog, Some(stack.with_count(3).unwrap()))
            .unwrap();
        inventory.select_hotbar(4).unwrap();
        inventory
    }

    #[test]
    fn absent_legacy_file_is_distinct_from_a_saved_empty_inventory() {
        let root = tempfile::tempdir().unwrap();
        let store = PlayerStore::new(root.path().into(), catalog(1, 6)).unwrap();
        let player = Uuid::from_u128(1);
        assert!(store.load_existing(player).unwrap().is_none());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        store.save(player, &PlayerInventory::new()).unwrap();
        let path = root.path().join(format!("{player}.zinv"));
        let original = std::fs::read(&path).unwrap();
        assert_eq!(
            store.load_existing(player).unwrap(),
            Some(PlayerInventory::new())
        );
        assert_eq!(std::fs::read(path).unwrap(), original);
        assert!(store.load_existing(Uuid::nil()).is_err());
    }

    #[test]
    fn stores_named_items_cursor_and_selection_across_catalog_numeric_id_changes() {
        let root = tempfile::tempdir().unwrap();
        let old_catalog = catalog(1, 6);
        let store =
            PlayerStore::new(root.path().join("playerdata"), Arc::clone(&old_catalog)).unwrap();
        let player = Uuid::from_u128(1);
        let source = inventory(&old_catalog);
        store.save(player, &source).unwrap();
        let raw = std::fs::read(root.path().join(format!("playerdata/{player}.zinv"))).unwrap();
        assert!(
            raw.windows(b"minecraft:stone".len())
                .any(|bytes| bytes == b"minecraft:stone")
        );
        let new_catalog = catalog(12, 41);
        let reopened =
            PlayerStore::new(root.path().join("playerdata"), Arc::clone(&new_catalog)).unwrap();
        let restored = reopened.load(player).unwrap();
        assert_eq!(restored, inventory(&new_catalog));
    }

    #[test]
    fn derived_crafting_result_is_not_persisted_as_an_owned_item() {
        let root = tempfile::tempdir().unwrap();
        let catalog = catalog(1, 6);
        let store = PlayerStore::new(root.path().into(), Arc::clone(&catalog)).unwrap();
        let uuid = Uuid::from_u128(51);
        let mut source = inventory(&catalog);
        source
            .set_slot(
                &catalog,
                1,
                Some(ItemStack::new(ItemId(1), 2, ValidatedPatch::default()).unwrap()),
            )
            .unwrap();
        source
            .set_slot(
                &catalog,
                0,
                Some(ItemStack::new(ItemId(1), 4, ValidatedPatch::default()).unwrap()),
            )
            .unwrap();
        store.save(uuid, &source).unwrap();
        let restored = store.load(uuid).unwrap();
        assert!(restored.slots()[0].is_none());
        assert_eq!(&restored.slots()[1..], &source.slots()[1..]);
        assert_eq!(restored.cursor(), source.cursor());
        assert_eq!(source.slots()[0].as_ref().unwrap().count(), 4);
        let stored = decode_file(&fs::read(store.path(uuid).unwrap()).unwrap()).unwrap();
        assert_eq!(stored.schema, 2);
        assert!(stored.slots[0].is_none());
    }

    #[test]
    fn legacy_grid_and_cursor_survive_upgrade_but_ambiguous_result_is_preserved() {
        for has_result in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let catalog = catalog(1, 6);
            let store = PlayerStore::new(root.path().into(), Arc::clone(&catalog)).unwrap();
            let uuid = Uuid::from_u128(52);
            let mut original = inventory(&catalog);
            original
                .set_slot(
                    &catalog,
                    1,
                    Some(ItemStack::new(ItemId(1), 2, ValidatedPatch::default()).unwrap()),
                )
                .unwrap();
            if has_result {
                original
                    .set_slot(
                        &catalog,
                        0,
                        Some(ItemStack::new(ItemId(1), 4, ValidatedPatch::default()).unwrap()),
                    )
                    .unwrap();
            }
            let legacy = StoredInventory {
                schema: 1,
                minecraft_version: "26.3".into(),
                protocol: 777,
                uuid,
                selected: original.selected(),
                slots: original
                    .slots()
                    .iter()
                    .map(|slot| encode_stack(slot, &catalog).unwrap())
                    .collect(),
                cursor: encode_stack(original.cursor(), &catalog).unwrap(),
            };
            let path = store.path(uuid).unwrap();
            write_file(
                &mut File::create(&path).unwrap(),
                &serialize_bounded(&legacy).unwrap(),
            )
            .unwrap();
            let before = fs::read(&path).unwrap();
            if has_result {
                assert!(store.load(uuid).is_err());
                assert!(store.save(uuid, &PlayerInventory::new()).is_err());
                assert_eq!(fs::read(path).unwrap(), before);
            } else {
                let restored = store.load(uuid).unwrap();
                assert_eq!(restored, original);
                store.save(uuid, &restored).unwrap();
                assert_eq!(decode_file(&fs::read(path).unwrap()).unwrap().schema, 2);
                assert_eq!(store.load(uuid).unwrap(), original);
            }
        }
    }

    #[test]
    fn every_supported_component_and_removed_key_roundtrips_as_typed_data() {
        let root = tempfile::tempdir().unwrap();
        let values = vec![
            ComponentValue::MaxStackSize(1),
            ComponentValue::MaxDamage(1000),
            ComponentValue::Damage(7),
            ComponentValue::RepairCost(12),
            ComponentValue::Unbreakable,
            ComponentValue::CustomName("custom".into()),
            ComponentValue::ItemName("item".into()),
            ComponentValue::ItemModel("minecraft:diamond".into()),
            ComponentValue::Rarity(3),
            ComponentValue::EnchantmentGlint(true),
            ComponentValue::BlockState(BTreeMap::from([("axis".into(), "x".into())])),
        ];
        let definitions = values
            .iter()
            .enumerate()
            .map(|(index, value)| ComponentDef {
                id: ComponentId(index as u32),
                key: value.key().into(),
            })
            .collect();
        let catalog = Arc::new(
            ItemCatalog::from_definitions(
                vec![ItemDef {
                    id: ItemId(1),
                    key: "minecraft:diamond_sword".into(),
                    max_stack: 1,
                    max_damage: Some(100),
                }],
                definitions,
            )
            .unwrap(),
        );
        let added = values
            .into_iter()
            .enumerate()
            .map(|(index, value)| (ComponentId(index as u32), value))
            .collect();
        let patch = ValidatedPatch::new(added, BTreeSet::new(), &catalog).unwrap();
        let stack = ItemStack::new(ItemId(1), 1, patch).unwrap();
        let removed = ValidatedPatch::new(
            BTreeMap::new(),
            BTreeSet::from([catalog.component_id("minecraft:custom_name").unwrap()]),
            &catalog,
        )
        .unwrap();
        let mut inventory = PlayerInventory::new();
        inventory.set_slot(&catalog, 36, Some(stack)).unwrap();
        inventory
            .set_slot(
                &catalog,
                37,
                Some(ItemStack::new(ItemId(1), 1, removed).unwrap()),
            )
            .unwrap();
        let store = PlayerStore::new(root.path().into(), catalog).unwrap();
        store.save(Uuid::from_u128(12), &inventory).unwrap();
        assert_eq!(store.load(Uuid::from_u128(12)).unwrap(), inventory);
    }

    #[test]
    fn reconnection_resets_protocol_revision_without_losing_inventory_state() {
        use zerqon_inventory::{CreativeSlotRequest, InventoryPolicy};
        let root = tempfile::tempdir().unwrap();
        let catalog = catalog(1, 6);
        let store = PlayerStore::new(root.path().into(), Arc::clone(&catalog)).unwrap();
        let mut original = inventory(&catalog);
        original
            .creative_set(
                &catalog,
                &CreativeSlotRequest {
                    slot: 37,
                    stack: Some(ItemStack::new(ItemId(1), 2, ValidatedPatch::default()).unwrap()),
                },
                InventoryPolicy {
                    creative: true,
                    ..InventoryPolicy::default()
                },
            )
            .unwrap();
        assert_ne!(original.state_id(), 0);
        let player = Uuid::from_u128(13);
        store.save(player, &original).unwrap();
        let restored = store.load(player).unwrap();
        assert_eq!(restored.state_id(), 0);
        assert_eq!(restored.slots(), original.slots());
        assert_eq!(restored.cursor(), original.cursor());
        assert_eq!(restored.selected(), original.selected());
    }

    #[test]
    fn missing_file_is_fresh_but_nil_identity_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let catalog = catalog(1, 6);
        let store = PlayerStore::new(root.path().into(), catalog).unwrap();
        assert_eq!(
            store.load(Uuid::from_u128(2)).unwrap(),
            PlayerInventory::new()
        );
        assert!(store.load(Uuid::nil()).is_err());
        assert!(store.save(Uuid::nil(), &PlayerInventory::new()).is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn corruption_is_rejected_and_not_overwritten_by_save() {
        let root = tempfile::tempdir().unwrap();
        let catalog = catalog(1, 6);
        let store = PlayerStore::new(root.path().into(), Arc::clone(&catalog)).unwrap();
        let player = Uuid::from_u128(3);
        let path = root.path().join(format!("{player}.zinv"));
        std::fs::write(&path, b"corrupt existing player inventory").unwrap();
        assert!(store.load(player).is_err());
        assert!(store.save(player, &inventory(&catalog)).is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"corrupt existing player inventory"
        );
    }

    #[test]
    fn checksum_and_embedded_player_identity_are_verified() {
        let root = tempfile::tempdir().unwrap();
        let catalog = catalog(1, 6);
        let store = PlayerStore::new(root.path().into(), Arc::clone(&catalog)).unwrap();
        let player = Uuid::from_u128(4);
        let other = Uuid::from_u128(5);
        let path = root.path().join(format!("{player}.zinv"));
        store.save(player, &inventory(&catalog)).unwrap();
        let original = std::fs::read(&path).unwrap();
        std::fs::write(root.path().join(format!("{other}.zinv")), &original).unwrap();
        assert!(store.load(other).is_err());
        let mut corrupt = original;
        let end = corrupt.len() - 1;
        corrupt[end] ^= 1;
        std::fs::write(&path, &corrupt).unwrap();
        assert!(store.load(player).is_err());
        assert!(store.save(player, &PlayerInventory::new()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), corrupt);
    }

    #[test]
    fn checksum_valid_but_semantically_invalid_records_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let catalog = catalog(1, 6);
        let store = PlayerStore::new(root.path().into(), Arc::clone(&catalog)).unwrap();
        let player = Uuid::from_u128(7);
        let path = root.path().join(format!("{player}.zinv"));
        store.save(player, &inventory(&catalog)).unwrap();
        let original = std::fs::read(&path).unwrap();
        let original: serde_json::Value =
            serde_json::from_slice(&original[HEADER_BYTES..]).unwrap();
        for case in 0..10 {
            let mut data = original.clone();
            match case {
                0 => data["schema"] = serde_json::json!(3),
                1 => data["minecraft_version"] = serde_json::json!("26.2"),
                2 => data["selected"] = serde_json::json!(9),
                3 => data["slots"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::Value::Null),
                4 => data["slots"][36]["item"] = serde_json::json!("minecraft:unknown_item"),
                5 => data["slots"][36]["count"] = serde_json::json!(65),
                6 => {
                    data["slots"][36]["added"][0]["component"] =
                        serde_json::json!("minecraft:unknown_component")
                }
                7 => {
                    data["slots"][36]["added"][0]["value"] =
                        serde_json::json!({"type":"max_stack_size","value":64})
                }
                8 => {
                    data["slots"][36]["added"] = serde_json::json!([]);
                    data["slots"][36]["removed"] =
                        serde_json::json!(["minecraft:custom_name", "minecraft:custom_name"]);
                }
                9 => {
                    data["unknown_field"] = serde_json::json!(true);
                }
                _ => unreachable!(),
            }
            let payload = serde_json::to_vec(&data).unwrap();
            let mut bytes = Vec::new();
            write_file(&mut bytes, &payload).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            assert!(store.load(player).is_err(), "case {case}");
            assert!(
                store.save(player, &PlayerInventory::new()).is_err(),
                "case {case}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn oversized_and_trailing_inventory_files_are_rejected_without_replacement() {
        let root = tempfile::tempdir().unwrap();
        let catalog = catalog(1, 6);
        let store = PlayerStore::new(root.path().into(), Arc::clone(&catalog)).unwrap();
        let player = Uuid::from_u128(8);
        let path = root.path().join(format!("{player}.zinv"));
        store.save(player, &inventory(&catalog)).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0);
        std::fs::write(&path, &bytes).unwrap();
        assert!(store.load(player).is_err());
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(MAX_FILE_BYTES as u64 + 1).unwrap();
        drop(file);
        assert!(matches!(store.load(player), Err(StoreError::Limit)));
        assert!(matches!(
            store.save(player, &PlayerInventory::new()),
            Err(StoreError::Limit)
        ));
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            MAX_FILE_BYTES as u64 + 1
        );
    }

    #[test]
    fn invalid_new_inventory_does_not_replace_valid_existing_state() {
        let root = tempfile::tempdir().unwrap();
        let catalog = catalog(1, 6);
        let store = PlayerStore::new(root.path().into(), Arc::clone(&catalog)).unwrap();
        let player = Uuid::from_u128(9);
        let path = root.path().join(format!("{player}.zinv"));
        store.save(player, &inventory(&catalog)).unwrap();
        let original = std::fs::read(&path).unwrap();
        let foreign_catalog = ItemCatalog::from_definitions(
            vec![ItemDef {
                id: ItemId(999),
                key: "minecraft:another".into(),
                max_stack: 64,
                max_damage: None,
            }],
            vec![],
        )
        .unwrap();
        let mut candidate = PlayerInventory::new();
        candidate
            .set_slot(
                &foreign_catalog,
                36,
                Some(ItemStack::new(ItemId(999), 1, ValidatedPatch::default()).unwrap()),
            )
            .unwrap();
        assert!(store.save(player, &candidate).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn serialization_limit_is_checked_before_creating_or_replacing_the_save_file() {
        let root = tempfile::tempdir().unwrap();
        let catalog = Arc::new(
            ItemCatalog::from_definitions(
                vec![ItemDef {
                    id: ItemId(1),
                    key: "minecraft:stone".into(),
                    max_stack: 64,
                    max_damage: None,
                }],
                vec![
                    ComponentDef {
                        id: ComponentId(6),
                        key: "minecraft:custom_name".into(),
                    },
                    ComponentDef {
                        id: ComponentId(9),
                        key: "minecraft:item_name".into(),
                    },
                ],
            )
            .unwrap(),
        );
        let store = PlayerStore::new(root.path().into(), Arc::clone(&catalog)).unwrap();
        let player = Uuid::from_u128(10);
        let path = root.path().join(format!("{player}.zinv"));
        store.save(player, &PlayerInventory::new()).unwrap();
        let original = std::fs::read(&path).unwrap();
        let patch = ValidatedPatch::new(
            BTreeMap::from([
                (
                    ComponentId(6),
                    ComponentValue::CustomName("\0".repeat(4096)),
                ),
                (ComponentId(9), ComponentValue::ItemName("\0".repeat(4096))),
            ]),
            BTreeSet::new(),
            &catalog,
        )
        .unwrap();
        let stack = ItemStack::new(ItemId(1), 1, patch).unwrap();
        let mut candidate = PlayerInventory::new();
        for slot in 0..46 {
            candidate
                .set_slot(&catalog, slot, Some(stack.clone()))
                .unwrap();
        }
        candidate.set_cursor(&catalog, Some(stack)).unwrap();
        assert!(matches!(
            store.save(player, &candidate),
            Err(StoreError::Limit)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_inventory_aliases_are_not_followed_or_replaced() {
        let root = tempfile::tempdir().unwrap();
        let catalog = catalog(1, 6);
        let store = PlayerStore::new(root.path().join("playerdata"), catalog).unwrap();
        let player = Uuid::from_u128(11);
        let path = root.path().join(format!("playerdata/{player}.zinv"));
        let outside = root.path().join("outside.zinv");
        std::fs::write(&outside, b"preserve outside").unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(store.load(player).is_err());
        assert!(store.save(player, &PlayerInventory::new()).is_err());
        assert!(
            std::fs::symlink_metadata(path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(outside).unwrap(), b"preserve outside");
    }

    #[cfg(windows)]
    #[test]
    fn failed_atomic_replace_preserves_previous_file_and_cleans_temporary_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = tempfile::tempdir().unwrap();
        let catalog = catalog(1, 6);
        let store = PlayerStore::new(root.path().into(), Arc::clone(&catalog)).unwrap();
        let player = Uuid::from_u128(6);
        let path = root.path().join(format!("{player}.zinv"));
        store.save(player, &inventory(&catalog)).unwrap();
        let original = std::fs::read(&path).unwrap();
        let _locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        assert!(store.save(player, &PlayerInventory::new()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
