use crate::{
    game_mode::GameMode,
    player_store::{StoreError, StoredInventory, serialize_payload},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zerqon_inventory::{ItemCatalog, PlayerInventory};

use zerqon_world::MAX_PLAYER_RECORD_BYTES as MAX_RECORD_BYTES;

#[derive(Clone)]
pub struct PlayerRecord {
    pub inventory: PlayerInventory,
    pub game_mode: GameMode,
    pub previous_game_mode: Option<GameMode>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPlayerRecord {
    schema: u32,
    world: Uuid,
    game_mode: GameMode,
    previous_game_mode: Option<GameMode>,
    inventory: StoredInventory,
}

impl PlayerRecord {
    pub fn new(game_mode: GameMode) -> Self {
        Self {
            inventory: PlayerInventory::new(),
            game_mode,
            previous_game_mode: None,
        }
    }

    pub fn from_legacy(inventory: PlayerInventory) -> Self {
        Self {
            inventory,
            game_mode: GameMode::Creative,
            previous_game_mode: None,
        }
    }

    pub fn encode(
        &self,
        world: Uuid,
        player: Uuid,
        catalog: &ItemCatalog,
    ) -> Result<Vec<u8>, StoreError> {
        if world.is_nil() || player.is_nil() {
            return Err(StoreError::Invalid("nil world or player UUID"));
        }
        serialize_payload(
            &StoredPlayerRecord {
                schema: 1,
                world,
                game_mode: self.game_mode,
                previous_game_mode: self.previous_game_mode,
                inventory: StoredInventory::capture(player, &self.inventory, catalog)?,
            },
            MAX_RECORD_BYTES,
        )
    }

    pub fn decode(
        bytes: &[u8],
        world: Uuid,
        player: Uuid,
        catalog: &ItemCatalog,
    ) -> Result<Self, StoreError> {
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(StoreError::Limit);
        }
        let stored: StoredPlayerRecord = serde_json::from_slice(bytes)?;
        if world.is_nil()
            || player.is_nil()
            || stored.world != world
            || stored.schema != 1
            || stored.inventory.schema() != 2
        {
            return Err(StoreError::Invalid(
                "player record identity or schema mismatch",
            ));
        }
        Ok(Self {
            inventory: stored.inventory.restore(player, catalog)?,
            game_mode: stored.game_mode,
            previous_game_mode: stored.previous_game_mode,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game_mode::GameMode;
    use serde_json::{Value, json};
    use std::collections::{BTreeMap, BTreeSet};
    use uuid::Uuid;
    use zerqon_inventory::{
        ComponentDef, ComponentId, ComponentValue, ItemCatalog, ItemDef, ItemId, ItemStack,
        PlayerInventory, ValidatedPatch,
    };

    fn catalog() -> ItemCatalog {
        ItemCatalog::from_definitions(
            vec![ItemDef {
                id: ItemId(1),
                key: "minecraft:stone".into(),
                max_stack: 64,
                max_damage: None,
            }],
            vec![
                ComponentDef {
                    id: ComponentId(1),
                    key: "minecraft:custom_name".into(),
                },
                ComponentDef {
                    id: ComponentId(2),
                    key: "minecraft:item_name".into(),
                },
            ],
        )
        .unwrap()
    }

    fn populated(catalog: &ItemCatalog) -> PlayerInventory {
        let mut inventory = PlayerInventory::new();
        let patch = ValidatedPatch::new(
            BTreeMap::from([(ComponentId(1), ComponentValue::CustomName("teighto".into()))]),
            BTreeSet::new(),
            catalog,
        )
        .unwrap();
        for (slot, count) in [(0, 64), (1, 3), (37, 12)] {
            inventory
                .set_slot(
                    catalog,
                    slot,
                    Some(ItemStack::new(ItemId(1), count, patch.clone()).unwrap()),
                )
                .unwrap();
        }
        inventory
            .set_cursor(catalog, Some(ItemStack::new(ItemId(1), 5, patch).unwrap()))
            .unwrap();
        inventory.select_hotbar(1).unwrap();
        inventory
    }

    fn identities() -> (Uuid, Uuid) {
        (Uuid::from_u128(1), Uuid::from_u128(2))
    }

    #[test]
    fn round_trip_preserves_owned_items_and_modes_without_persisting_the_preview() {
        let catalog = catalog();
        let (world, player) = identities();
        for mode in [GameMode::Survival, GameMode::Creative] {
            for previous in [None, Some(GameMode::Survival), Some(GameMode::Creative)] {
                let record = PlayerRecord {
                    inventory: populated(&catalog),
                    game_mode: mode,
                    previous_game_mode: previous,
                };
                let bytes = record.encode(world, player, &catalog).unwrap();
                let restored = PlayerRecord::decode(&bytes, world, player, &catalog).unwrap();
                assert_eq!(restored.game_mode, mode);
                assert_eq!(restored.previous_game_mode, previous);
                assert_eq!(
                    &restored.inventory.slots()[1..],
                    &record.inventory.slots()[1..]
                );
                assert!(restored.inventory.slots()[0].is_none());
                assert!(record.inventory.slots()[0].is_some());
                assert_eq!(restored.inventory.cursor(), record.inventory.cursor());
                assert_eq!(restored.inventory.selected(), record.inventory.selected());
            }
        }
    }

    #[test]
    fn legacy_inventory_keeps_creative_mode_independent_of_new_player_default() {
        let catalog = catalog();
        let legacy = PlayerRecord::from_legacy(populated(&catalog));
        assert_eq!(legacy.game_mode, GameMode::Creative);
        assert_eq!(legacy.previous_game_mode, None);
        let fresh = PlayerRecord::new(GameMode::Survival);
        assert_eq!(fresh.game_mode, GameMode::Survival);
        assert!(fresh.inventory.slots().iter().all(Option::is_none));
    }

    #[test]
    fn maximum_legacy_save_still_fits_the_player_record_envelope() {
        let catalog = std::sync::Arc::new(catalog());
        let (world, player) = identities();
        let mut inventory = PlayerInventory::new();
        for slot in 1..=43 {
            let (custom_length, item_length) = if slot == 43 { (3876, 4) } else { (4096, 4096) };
            let mut custom_name = "\0".repeat(custom_length);
            if slot == 43 {
                custom_name.push_str("tail");
            }
            let patch = ValidatedPatch::new(
                BTreeMap::from([
                    (ComponentId(1), ComponentValue::CustomName(custom_name)),
                    (
                        ComponentId(2),
                        ComponentValue::ItemName("\0".repeat(item_length)),
                    ),
                ]),
                BTreeSet::new(),
                &catalog,
            )
            .unwrap();
            inventory
                .set_slot(
                    &catalog,
                    slot,
                    Some(ItemStack::new(ItemId(1), 1, patch).unwrap()),
                )
                .unwrap();
        }
        let directory = tempfile::tempdir().unwrap();
        let payload_size =
            serde_json::to_vec(&StoredInventory::capture(player, &inventory, &catalog).unwrap())
                .unwrap()
                .len();
        assert!(
            payload_size <= 2 * 1024 * 1024 - 44,
            "legacy payload {payload_size}"
        );
        let legacy =
            crate::player_store::PlayerStore::new(directory.path().into(), catalog.clone())
                .unwrap();
        legacy.save(player, &inventory).unwrap();
        let source = std::fs::read_dir(directory.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let before = std::fs::read(&source).unwrap();
        assert_eq!(before.len(), 2 * 1024 * 1024);
        let record = PlayerRecord::from_legacy(legacy.load(player).unwrap());
        let encoded = record.encode(world, player, &catalog).unwrap();
        assert!(encoded.len() > 2 * 1024 * 1024);
        let decoded = PlayerRecord::decode(&encoded, world, player, &catalog).unwrap();
        assert_eq!(decoded.inventory.slots(), inventory.slots());
        assert_eq!(std::fs::read(source).unwrap(), before);
    }

    #[test]
    fn records_cannot_cross_worlds_or_players() {
        let catalog = catalog();
        let (world, player) = identities();
        let record = PlayerRecord::new(GameMode::Creative);
        let bytes = record.encode(world, player, &catalog).unwrap();
        assert!(PlayerRecord::decode(&bytes, player, player, &catalog).is_err());
        assert!(PlayerRecord::decode(&bytes, world, world, &catalog).is_err());
        assert!(record.encode(Uuid::nil(), player, &catalog).is_err());
        assert!(record.encode(world, Uuid::nil(), &catalog).is_err());
    }

    #[test]
    fn malformed_modes_identity_and_owned_result_are_rejected() {
        let catalog = catalog();
        let (world, player) = identities();
        let record = PlayerRecord::from_legacy(populated(&catalog));
        let original: Value =
            serde_json::from_slice(&record.encode(world, player, &catalog).unwrap()).unwrap();
        for (pointer, replacement) in [
            ("/schema", json!(2)),
            ("/game_mode", json!("spectator")),
            ("/previous_game_mode", json!("adventure")),
            ("/inventory/schema", json!(1)),
            ("/inventory/protocol", json!(776)),
            ("/inventory/minecraft_version", json!("1.21.11")),
            (
                "/inventory/slots/0",
                original["inventory"]["slots"][1].clone(),
            ),
            ("/inventory/slots/1/item", json!("minecraft:unknown_item")),
        ] {
            let mut invalid = original.clone();
            *invalid.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                PlayerRecord::decode(
                    &serde_json::to_vec(&invalid).unwrap(),
                    world,
                    player,
                    &catalog
                )
                .is_err(),
                "{pointer}"
            );
        }
        let mut missing_mode = original.clone();
        missing_mode.as_object_mut().unwrap().remove("game_mode");
        assert!(
            PlayerRecord::decode(
                &serde_json::to_vec(&missing_mode).unwrap(),
                world,
                player,
                &catalog
            )
            .is_err()
        );
        let mut extra_field = original;
        extra_field["unexpected"] = json!(true);
        assert!(
            PlayerRecord::decode(
                &serde_json::to_vec(&extra_field).unwrap(),
                world,
                player,
                &catalog
            )
            .is_err()
        );
    }

    #[test]
    fn record_size_and_complete_json_are_required_before_decoding() {
        let catalog = catalog();
        let (world, player) = identities();
        let bytes = PlayerRecord::new(GameMode::Survival)
            .encode(world, player, &catalog)
            .unwrap();
        assert!(PlayerRecord::decode(&bytes[..bytes.len() - 1], world, player, &catalog).is_err());
        let mut trailing = bytes;
        trailing.push(b'X');
        assert!(PlayerRecord::decode(&trailing, world, player, &catalog).is_err());
        assert!(matches!(
            PlayerRecord::decode(&vec![b' '; MAX_RECORD_BYTES + 1], world, player, &catalog),
            Err(crate::player_store::StoreError::Limit)
        ));
    }
}
