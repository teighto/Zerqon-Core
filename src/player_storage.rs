use crate::{
    game_mode::GameMode,
    player_record::PlayerRecord,
    player_store::{PlayerStore, StoreError},
};
use std::{fs, sync::Arc};
use uuid::Uuid;
use zerqon_inventory::ItemCatalog;
use zerqon_world::{
    CommitError, DomainReceipt, DomainSnapshot, PlayerKey, PlayerSnapshot, PlayerWrite, WorldError,
    WorldStore,
};

#[derive(Clone)]
pub struct PersistedPlayer {
    pub record: PlayerRecord,
    pub revision: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum PlayerStorageError {
    #[error(transparent)]
    World(#[from] WorldError),
    #[error(transparent)]
    Record(#[from] StoreError),
    #[error("stored player record failed semantic validation: {0}")]
    CorruptRecord(#[source] StoreError),
    #[error(transparent)]
    Commit(#[from] CommitError),
    #[error("invalid player storage operation: {0}")]
    Invalid(&'static str),
}

/// Active player records share the world's durable transaction domain.
/// Callers serialize each UUID's live load/save operations and retain the last
/// committed revision; conflicts must not be rebased onto a stale RAM candidate.
pub struct PlayerStorage {
    world: Arc<WorldStore>,
    catalog: Arc<ItemCatalog>,
    world_uuid: Uuid,
    default_mode: GameMode,
}

impl PlayerStorage {
    pub fn new(
        world: Arc<WorldStore>,
        catalog: Arc<ItemCatalog>,
        default_mode: GameMode,
    ) -> Result<Self, PlayerStorageError> {
        world.check_available()?;
        let world_uuid = world.uuid()?;
        Ok(Self {
            world,
            catalog,
            world_uuid,
            default_mode,
        })
    }

    pub fn catalog(&self) -> &Arc<ItemCatalog> {
        &self.catalog
    }

    pub(crate) fn same_world(&self, world: &Arc<WorldStore>) -> bool {
        Arc::ptr_eq(&self.world, world)
    }

    /// Decode an already-read participant without importing or creating a row.
    /// The enclosing domain commit verifies the snapshot's opaque store token.
    pub(crate) fn decode_existing_snapshot(
        &self,
        player: Uuid,
        snapshot: &PlayerSnapshot,
    ) -> Result<PersistedPlayer, PlayerStorageError> {
        if player.is_nil() || snapshot.key() != PlayerKey::new(player) {
            return Err(PlayerStorageError::Invalid(
                "player snapshot identity differs",
            ));
        }
        let bytes = snapshot.data().ok_or(PlayerStorageError::Invalid(
            "mixed operation requires an existing player",
        ))?;
        let record = PlayerRecord::decode(bytes, self.world_uuid, player, &self.catalog)
            .map_err(PlayerStorageError::CorruptRecord)?;
        Ok(PersistedPlayer {
            record,
            revision: snapshot.revision(),
        })
    }

    pub(crate) fn encode_write(
        &self,
        player: Uuid,
        record: &PlayerRecord,
    ) -> Result<PlayerWrite, PlayerStorageError> {
        Ok(PlayerWrite {
            key: PlayerKey::new(player),
            replacement: Arc::from(record.encode(self.world_uuid, player, &self.catalog)?),
        })
    }

    /// Inspect a terminal backend failure after an unsuccessful operation.
    pub fn check_available(&self) -> Result<(), WorldError> {
        self.world.check_available()
    }

    pub fn load(&self, player: Uuid) -> Result<PersistedPlayer, PlayerStorageError> {
        let reads = self.snapshot(player)?;
        if let Some(existing) = self.existing(player, &reads)? {
            return Ok(existing);
        }
        self.initialize(player, reads)
    }

    pub fn save(
        &self,
        player: Uuid,
        expected_revision: u64,
        record: &PlayerRecord,
    ) -> Result<u64, PlayerStorageError> {
        if expected_revision == 0 {
            return Err(PlayerStorageError::Invalid(
                "save requires a positive existing revision",
            ));
        }
        let reads = self.snapshot(player)?;
        let current = self
            .existing(player, &reads)?
            .ok_or(PlayerStorageError::Invalid(
                "save cannot initialize a missing player",
            ))?;
        if current.revision != expected_revision {
            return Err(CommitError::Conflict.into());
        }
        let replacement = record.encode(self.world_uuid, player, &self.catalog)?;
        let receipt = self.world.commit_domain(
            Uuid::new_v4(),
            &reads,
            &[],
            &[PlayerWrite {
                key: PlayerKey::new(player),
                replacement: Arc::from(replacement),
            }],
            &[],
        )?;
        // A domain commit assigns this one global revision to every after-image.
        // Publication succeeded: no fallible DB reread may turn it into a rollback.
        Ok(receipt.revision)
    }

    fn snapshot(&self, player: Uuid) -> Result<DomainSnapshot, PlayerStorageError> {
        if player.is_nil() {
            return Err(PlayerStorageError::Invalid("nil player UUID"));
        }
        // The world layer rejects a missing/corrupt row with a creation marker;
        // such an error must never be interpreted as permission to reimport.
        Ok(self
            .world
            .snapshot_domain(&[], &[PlayerKey::new(player)], &[])?)
    }

    fn existing(
        &self,
        player: Uuid,
        reads: &DomainSnapshot,
    ) -> Result<Option<PersistedPlayer>, PlayerStorageError> {
        let [snapshot] = reads.players.as_slice() else {
            return Err(PlayerStorageError::Invalid(
                "expected exactly one player snapshot",
            ));
        };
        if snapshot.key() != PlayerKey::new(player) {
            return Err(PlayerStorageError::Invalid(
                "player snapshot identity differs",
            ));
        }
        snapshot
            .data()
            .map(|bytes| {
                let record = PlayerRecord::decode(bytes, self.world_uuid, player, &self.catalog)
                    .map_err(PlayerStorageError::CorruptRecord)?;
                Ok(PersistedPlayer {
                    record,
                    revision: snapshot.revision(),
                })
            })
            .transpose()
    }

    fn initialize(
        &self,
        player: Uuid,
        reads: DomainSnapshot,
    ) -> Result<PersistedPlayer, PlayerStorageError> {
        let directory = self.world.root().join("playerdata");
        let legacy = match fs::symlink_metadata(&directory) {
            Ok(_) => {
                PlayerStore::new(directory, Arc::clone(&self.catalog))?.load_existing(player)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(StoreError::Io(error).into()),
        };
        let record = legacy.map_or_else(
            || PlayerRecord::new(self.default_mode),
            PlayerRecord::from_legacy,
        );
        let replacement = record.encode(self.world_uuid, player, &self.catalog)?;
        let result = self.world.commit_domain(
            Uuid::new_v4(),
            &reads,
            &[],
            &[PlayerWrite {
                key: PlayerKey::new(player),
                replacement: Arc::from(replacement),
            }],
            &[],
        );
        self.initialized(player, record, result)
    }

    fn initialized(
        &self,
        player: Uuid,
        record: PlayerRecord,
        result: Result<DomainReceipt, CommitError>,
    ) -> Result<PersistedPlayer, PlayerStorageError> {
        match result {
            Ok(receipt) => Ok(PersistedPlayer {
                record,
                revision: receipt.revision,
            }),
            Err(CommitError::Conflict) => {
                // A concurrent first load won. Observe it once; never recursively
                // call load(), reimport the legacy file, or retry an unknown commit.
                let winner = self.snapshot(player)?;
                self.existing(player, &winner)?
                    .ok_or_else(|| CommitError::Conflict.into())
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{game_mode::GameMode, player_record::PlayerRecord, player_store::PlayerStore};
    use std::{
        fs,
        path::PathBuf,
        sync::{Arc, Barrier},
    };
    use uuid::Uuid;
    use zerqon_inventory::{ItemDef, ItemId, ItemStack, PlayerInventory, ValidatedPatch};
    use zerqon_world::{CommitError, PlayerKey, PlayerWrite, WorldStore};

    fn catalog() -> Arc<ItemCatalog> {
        Arc::new(
            ItemCatalog::from_definitions(
                vec![ItemDef {
                    id: ItemId(1),
                    key: "minecraft:stone".into(),
                    max_stack: 64,
                    max_damage: None,
                }],
                Vec::new(),
            )
            .unwrap(),
        )
    }

    fn populated(catalog: &ItemCatalog) -> PlayerInventory {
        let mut inventory = PlayerInventory::new();
        for (slot, count) in [(1, 3), (4, 7), (36, 12)] {
            inventory
                .set_slot(
                    catalog,
                    slot,
                    Some(ItemStack::new(ItemId(1), count, ValidatedPatch::default()).unwrap()),
                )
                .unwrap();
        }
        inventory
            .set_cursor(
                catalog,
                Some(ItemStack::new(ItemId(1), 2, ValidatedPatch::default()).unwrap()),
            )
            .unwrap();
        inventory.select_hotbar(2).unwrap();
        inventory
    }

    fn legacy(
        world: &WorldStore,
        catalog: &Arc<ItemCatalog>,
        player: Uuid,
    ) -> (PathBuf, Vec<u8>, PlayerInventory) {
        let directory = world.root().join("playerdata");
        let original = populated(catalog);
        PlayerStore::new(directory.clone(), Arc::clone(catalog))
            .unwrap()
            .save(player, &original)
            .unwrap();
        let path = directory.join(format!("{player}.zinv"));
        let bytes = fs::read(&path).unwrap();
        (path, bytes, original)
    }

    #[test]
    fn mixed_helpers_do_not_initialize_and_require_exact_world_and_player_identity() {
        let directory = tempfile::tempdir().unwrap();
        let other_directory = tempfile::tempdir().unwrap();
        let world = Arc::new(WorldStore::open(directory.path()).unwrap());
        let other = Arc::new(WorldStore::open(other_directory.path()).unwrap());
        let player = Uuid::from_u128(81);
        let store = PlayerStorage::new(Arc::clone(&world), catalog(), GameMode::Creative).unwrap();
        assert!(store.same_world(&world));
        assert!(!store.same_world(&other));
        let absent = world
            .snapshot_domain(&[], &[PlayerKey::new(player)], &[])
            .unwrap();
        assert!(matches!(
            store.decode_existing_snapshot(player, &absent.players[0]),
            Err(PlayerStorageError::Invalid(_))
        ));
        assert!(
            world
                .snapshot_domain(&[], &[PlayerKey::new(player)], &[])
                .unwrap()
                .players[0]
                .data()
                .is_none()
        );
        assert!(!directory.path().join("playerdata").exists());
        let loaded = store.load(player).unwrap();
        let reads = world
            .snapshot_domain(&[], &[PlayerKey::new(player)], &[])
            .unwrap();
        assert_eq!(
            store
                .decode_existing_snapshot(player, &reads.players[0])
                .unwrap()
                .revision,
            loaded.revision
        );
        assert!(matches!(
            store.decode_existing_snapshot(Uuid::from_u128(82), &reads.players[0]),
            Err(PlayerStorageError::Invalid(_))
        ));
        let record = PlayerRecord::new(GameMode::Survival);
        let write = store.encode_write(player, &record).unwrap();
        assert_eq!(write.key, PlayerKey::new(player));
        assert_eq!(
            PlayerRecord::decode(
                &write.replacement,
                world.uuid().unwrap(),
                player,
                store.catalog()
            )
            .unwrap()
            .game_mode,
            GameMode::Survival
        );
        assert_eq!(store.load(player).unwrap().revision, loaded.revision);
        assert_eq!(
            store.load(player).unwrap().record.game_mode,
            GameMode::Creative
        );
    }

    #[test]
    fn fresh_player_uses_default_mode_and_reopens_without_creating_legacy_files() {
        let root = tempfile::tempdir().unwrap();
        let catalog = catalog();
        let player = Uuid::from_u128(1);
        let world = Arc::new(WorldStore::open(root.path()).unwrap());
        let storage =
            PlayerStorage::new(Arc::clone(&world), Arc::clone(&catalog), GameMode::Survival)
                .unwrap();
        assert!(Arc::ptr_eq(storage.catalog(), &catalog));
        let first = storage.load(player).unwrap();
        assert!(first.revision > 0);
        assert_eq!(first.record.game_mode, GameMode::Survival);
        assert_eq!(first.record.previous_game_mode, None);
        assert!(first.record.inventory.slots().iter().all(Option::is_none));
        let stored = world
            .snapshot_domain(&[], &[PlayerKey::new(player)], &[])
            .unwrap();
        PlayerRecord::decode(
            stored.players[0].data().unwrap(),
            world.uuid().unwrap(),
            player,
            &catalog,
        )
        .unwrap();
        assert!(!root.path().join("playerdata").exists());
        assert_eq!(storage.load(player).unwrap().revision, first.revision);
        drop(storage);
        drop(world);
        let world = Arc::new(WorldStore::open(root.path()).unwrap());
        let storage = PlayerStorage::new(world, catalog, GameMode::Creative).unwrap();
        let reopened = storage.load(player).unwrap();
        assert_eq!(reopened.revision, first.revision);
        assert_eq!(reopened.record.game_mode, GameMode::Survival);
    }

    #[test]
    fn legacy_import_preserves_grid_cursor_selection_and_source_bytes_as_creative() {
        let root = tempfile::tempdir().unwrap();
        let world = Arc::new(WorldStore::open(root.path()).unwrap());
        let catalog = catalog();
        let player = Uuid::from_u128(2);
        let (path, bytes, original) = legacy(&world, &catalog, player);
        let storage = PlayerStorage::new(world, catalog, GameMode::Survival).unwrap();
        let loaded = storage.load(player).unwrap();
        assert_eq!(loaded.record.game_mode, GameMode::Creative);
        assert_eq!(loaded.record.previous_game_mode, None);
        assert_eq!(loaded.record.inventory, original);
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn valid_database_record_is_authoritative_even_if_legacy_file_is_corrupt() {
        let root = tempfile::tempdir().unwrap();
        let world = Arc::new(WorldStore::open(root.path()).unwrap());
        let catalog = catalog();
        let player = Uuid::from_u128(3);
        let (path, _, _) = legacy(&world, &catalog, player);
        let storage = PlayerStorage::new(world, catalog, GameMode::Survival).unwrap();
        let first = storage.load(player).unwrap();
        fs::write(&path, b"corrupt abandoned file").unwrap();
        let loaded = storage.load(player).unwrap();
        assert_eq!(loaded.revision, first.revision);
        assert_eq!(loaded.record.inventory, first.record.inventory);
        assert_eq!(fs::read(path).unwrap(), b"corrupt abandoned file");
    }

    #[test]
    fn save_commits_record_with_expected_revision_and_stale_save_preserves_winner() {
        let root = tempfile::tempdir().unwrap();
        let world = Arc::new(WorldStore::open(root.path()).unwrap());
        let catalog = catalog();
        let player = Uuid::from_u128(4);
        let storage =
            PlayerStorage::new(Arc::clone(&world), Arc::clone(&catalog), GameMode::Creative)
                .unwrap();
        let first = storage.load(player).unwrap();
        let mut candidate = PlayerRecord::new(GameMode::Survival);
        candidate.previous_game_mode = Some(GameMode::Creative);
        candidate.inventory = populated(&catalog);
        candidate
            .inventory
            .set_slot(
                &catalog,
                0,
                Some(ItemStack::new(ItemId(1), 4, ValidatedPatch::default()).unwrap()),
            )
            .unwrap();
        let revision = storage.save(player, first.revision, &candidate).unwrap();
        assert!(revision > first.revision);
        assert!(matches!(
            storage.save(
                player,
                first.revision,
                &PlayerRecord::new(GameMode::Creative)
            ),
            Err(PlayerStorageError::Commit(CommitError::Conflict))
        ));
        let loaded = storage.load(player).unwrap();
        assert_eq!(loaded.revision, revision);
        assert_eq!(loaded.record.game_mode, GameMode::Survival);
        assert_eq!(loaded.record.previous_game_mode, Some(GameMode::Creative));
        assert_eq!(
            &loaded.record.inventory.slots()[1..],
            &candidate.inventory.slots()[1..]
        );
        assert!(loaded.record.inventory.slots()[0].is_none());
        assert_eq!(
            loaded.record.inventory.cursor(),
            candidate.inventory.cursor()
        );
        drop(storage);
        drop(world);
        let reopened = PlayerStorage::new(
            Arc::new(WorldStore::open(root.path()).unwrap()),
            catalog,
            GameMode::Creative,
        )
        .unwrap();
        assert_eq!(reopened.load(player).unwrap().revision, revision);
    }

    #[test]
    fn malformed_legacy_does_not_create_a_database_record_or_get_replaced() {
        let root = tempfile::tempdir().unwrap();
        let world = Arc::new(WorldStore::open(root.path()).unwrap());
        let player = Uuid::from_u128(5);
        let path = world.root().join(format!("playerdata/{player}.zinv"));
        fs::create_dir(path.parent().unwrap()).unwrap();
        fs::write(&path, b"malformed legacy").unwrap();
        let storage =
            PlayerStorage::new(Arc::clone(&world), catalog(), GameMode::Survival).unwrap();
        assert!(matches!(
            storage.load(player),
            Err(PlayerStorageError::Record(_))
        ));
        assert!(
            world
                .snapshot_domain(&[], &[PlayerKey::new(player)], &[])
                .unwrap()
                .players[0]
                .data()
                .is_none()
        );
        assert_eq!(fs::read(path).unwrap(), b"malformed legacy");
    }

    #[test]
    fn corrupt_semantic_database_record_never_falls_back_to_legacy_or_allows_save() {
        let root = tempfile::tempdir().unwrap();
        let world = Arc::new(WorldStore::open(root.path()).unwrap());
        let catalog = catalog();
        let player = Uuid::from_u128(6);
        let (path, bytes, _) = legacy(&world, &catalog, player);
        let key = PlayerKey::new(player);
        let before = world.snapshot_domain(&[], &[key], &[]).unwrap();
        let receipt = world
            .commit_domain(
                Uuid::new_v4(),
                &before,
                &[],
                &[PlayerWrite {
                    key,
                    replacement: Arc::from(b"corrupt record".as_slice()),
                }],
                &[],
            )
            .unwrap();
        let storage = PlayerStorage::new(Arc::clone(&world), catalog, GameMode::Survival).unwrap();
        assert!(matches!(
            storage.load(player),
            Err(PlayerStorageError::CorruptRecord(_))
        ));
        assert!(matches!(
            storage.save(
                player,
                receipt.revision,
                &PlayerRecord::new(GameMode::Creative)
            ),
            Err(PlayerStorageError::CorruptRecord(_))
        ));
        let after = world.snapshot_domain(&[], &[key], &[]).unwrap();
        assert_eq!(after.players[0].revision(), receipt.revision);
        assert_eq!(after.players[0].data().unwrap().as_ref(), b"corrupt record");
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn save_never_initializes_missing_players_or_accepts_zero_revision() {
        let root = tempfile::tempdir().unwrap();
        let world = Arc::new(WorldStore::open(root.path()).unwrap());
        let player = Uuid::from_u128(7);
        let storage =
            PlayerStorage::new(Arc::clone(&world), catalog(), GameMode::Creative).unwrap();
        for revision in [0, 1] {
            assert!(
                storage
                    .save(player, revision, &PlayerRecord::new(GameMode::Survival))
                    .is_err()
            );
        }
        assert!(
            world
                .snapshot_domain(&[], &[PlayerKey::new(player)], &[])
                .unwrap()
                .players[0]
                .data()
                .is_none()
        );
        assert!(storage.load(Uuid::nil()).is_err());
        assert!(
            storage
                .save(Uuid::nil(), 1, &PlayerRecord::new(GameMode::Creative))
                .is_err()
        );
        storage.check_available().unwrap();
    }

    #[test]
    fn concurrent_first_loads_observe_one_committed_player_revision() {
        let root = tempfile::tempdir().unwrap();
        let world = Arc::new(WorldStore::open(root.path()).unwrap());
        let storage = PlayerStorage::new(world, catalog(), GameMode::Survival).unwrap();
        let player = Uuid::from_u128(8);
        let barrier = Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let tasks = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        storage.load(player).unwrap()
                    })
                })
                .collect::<Vec<_>>();
            tasks
                .into_iter()
                .map(|task| task.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results[0].revision, results[1].revision);
        assert_eq!(results[0].record.game_mode, results[1].record.game_mode);
        assert_eq!(results[0].record.inventory, results[1].record.inventory);
    }

    #[test]
    fn first_load_cas_loser_returns_the_winners_record_without_overwriting_it() {
        let root = tempfile::tempdir().unwrap();
        let world = Arc::new(WorldStore::open(root.path()).unwrap());
        let catalog = catalog();
        let player = Uuid::from_u128(9);
        let stale = world
            .snapshot_domain(&[], &[PlayerKey::new(player)], &[])
            .unwrap();
        let winner =
            PlayerStorage::new(Arc::clone(&world), Arc::clone(&catalog), GameMode::Survival)
                .unwrap()
                .load(player)
                .unwrap();
        let loser = PlayerStorage::new(world, catalog, GameMode::Creative).unwrap();
        let observed = loser.initialize(player, stale).unwrap();
        assert_eq!(observed.revision, winner.revision);
        assert_eq!(observed.record.game_mode, GameMode::Survival);
        assert_eq!(loser.load(player).unwrap().revision, winner.revision);
    }

    #[test]
    fn indeterminate_initialization_is_not_converted_into_a_winner_read_or_retry() {
        let root = tempfile::tempdir().unwrap();
        let world = Arc::new(WorldStore::open(root.path()).unwrap());
        let storage = PlayerStorage::new(world, catalog(), GameMode::Survival).unwrap();
        let player = Uuid::from_u128(10);
        let winner = storage.load(player).unwrap();
        let operation = Uuid::from_u128(100);
        // This tests adapter error routing, not physical backend fault recovery.
        // The world crate separately injects errors at its actual commit boundary.
        let result = storage.initialized(
            player,
            PlayerRecord::new(GameMode::Creative),
            Err(CommitError::Indeterminate {
                operation,
                cause: "unknown durable outcome".into(),
            }),
        );
        assert!(
            matches!(result, Err(PlayerStorageError::Commit(CommitError::Indeterminate { operation: observed, .. })) if observed == operation)
        );
        assert_eq!(storage.load(player).unwrap().revision, winner.revision);
    }

    #[test]
    fn an_invalid_candidate_is_distinct_from_corrupt_authority_and_does_not_commit() {
        let root = tempfile::tempdir().unwrap();
        let world = Arc::new(WorldStore::open(root.path()).unwrap());
        let storage = PlayerStorage::new(world, catalog(), GameMode::Survival).unwrap();
        let player = Uuid::from_u128(11);
        let first = storage.load(player).unwrap();
        let foreign = ItemCatalog::from_definitions(
            vec![ItemDef {
                id: ItemId(999),
                key: "minecraft:foreign".into(),
                max_stack: 64,
                max_damage: None,
            }],
            Vec::new(),
        )
        .unwrap();
        let mut candidate = PlayerRecord::new(GameMode::Creative);
        candidate
            .inventory
            .set_slot(
                &foreign,
                36,
                Some(ItemStack::new(ItemId(999), 1, ValidatedPatch::default()).unwrap()),
            )
            .unwrap();
        assert!(matches!(
            storage.save(player, first.revision, &candidate),
            Err(PlayerStorageError::Record(_))
        ));
        let after = storage.load(player).unwrap();
        assert_eq!(after.revision, first.revision);
        assert_eq!(after.record.game_mode, GameMode::Survival);
        assert_eq!(after.record.inventory, first.record.inventory);
    }

    #[test]
    fn existing_database_authority_does_not_depend_on_the_legacy_directory() {
        let root = tempfile::tempdir().unwrap();
        let world = Arc::new(WorldStore::open(root.path()).unwrap());
        let catalog = catalog();
        let player = Uuid::from_u128(12);
        let first =
            PlayerStorage::new(Arc::clone(&world), Arc::clone(&catalog), GameMode::Survival)
                .unwrap()
                .load(player)
                .unwrap();
        fs::write(root.path().join("playerdata"), b"not a directory").unwrap();
        let storage = PlayerStorage::new(world, catalog, GameMode::Creative).unwrap();
        assert_eq!(storage.load(player).unwrap().revision, first.revision);
        assert!(
            storage
                .save(
                    player,
                    first.revision,
                    &PlayerRecord::new(GameMode::Creative)
                )
                .unwrap()
                > first.revision
        );
        assert_eq!(
            fs::read(root.path().join("playerdata")).unwrap(),
            b"not a directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_legacy_links_cannot_be_misread_as_a_fresh_player() {
        for link_directory in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let world = Arc::new(WorldStore::open(root.path()).unwrap());
            let player = Uuid::from_u128(13);
            let directory = root.path().join("playerdata");
            let link = if link_directory {
                directory.clone()
            } else {
                fs::create_dir(&directory).unwrap();
                directory.join(format!("{player}.zinv"))
            };
            std::os::unix::fs::symlink(root.path().join("missing"), &link).unwrap();
            let storage =
                PlayerStorage::new(Arc::clone(&world), catalog(), GameMode::Survival).unwrap();
            assert!(matches!(
                storage.load(player),
                Err(PlayerStorageError::Record(_))
            ));
            assert!(
                world
                    .snapshot_domain(&[], &[PlayerKey::new(player)], &[])
                    .unwrap()
                    .players[0]
                    .data()
                    .is_none()
            );
            assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        }
    }
}
