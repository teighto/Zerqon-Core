use super::{World, fail_world, generate_chunk, stripe, validate_chunk};
use crate::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, atomic::Ordering},
};
use zerqon_world::{
    ChunkKey, ChunkWrite, CommitError, GenerationPolicy, MIN_Y, WORLD_HEIGHT, WorldError,
};

pub const MAX_EDIT_BLOCKS: usize = 65_536;
pub const MAX_EDIT_CHUNKS: usize = 16;
pub const MAX_READ_CHUNKS: usize = 256;

#[derive(Debug, thiserror::Error)]
#[error("selected blocks changed before editing; retry the operation")]
pub struct BulkEditConflict;

type Edit = ([i32; 3], Option<u32>, u32);

impl World {
    pub async fn edit_blocks(&self, edits: Vec<([i32; 3], u32)>) -> Result<usize> {
        if edits.len() > MAX_EDIT_BLOCKS {
            return Err("edit exceeds the 65536 block limit".into());
        }
        self.apply_bulk(
            edits
                .into_iter()
                .map(|(position, state)| (position, None, state))
                .collect(),
        )
        .await
    }

    pub async fn edit_blocks_if(&self, edits: Vec<([i32; 3], u32, u32)>) -> Result<usize> {
        if edits.len() > MAX_EDIT_BLOCKS {
            return Err("edit exceeds the 65536 block limit".into());
        }
        self.apply_bulk(
            edits
                .into_iter()
                .map(|(position, expected, state)| (position, Some(expected), state))
                .collect(),
        )
        .await
    }

    async fn apply_bulk(&self, edits: Vec<Edit>) -> Result<usize> {
        self.check_available()?;
        let mut grouped = BTreeMap::<ChunkKey, Vec<Edit>>::new();
        let mut positions = BTreeSet::new();
        for edit @ (position, expected, replacement) in edits {
            validate_position(position)?;
            if !self.data.is_valid_state(replacement)
                || expected.is_some_and(|state| !self.data.is_valid_state(state))
            {
                return Err("invalid block state in edit".into());
            }
            if !positions.insert(position) {
                return Err("duplicate block position in edit".into());
            }
            grouped.entry(key(position)).or_default().push(edit);
            if grouped.len() > MAX_EDIT_CHUNKS {
                return Err("edit exceeds the 16 chunk transaction limit".into());
            }
        }
        if grouped.is_empty() {
            return Ok(0);
        }
        let gameplay = Arc::clone(&self.gameplay).write_owned().await;
        let permit = Arc::clone(&self.inflight).acquire_owned().await?;
        let store = Arc::clone(&self.store);
        let cache = Arc::clone(&self.cache);
        let data = Arc::clone(&self.data);
        let generator = self.generator;
        let stripes = Arc::clone(&self.stripes);
        let players = Arc::clone(&self.players);
        let failed = Arc::clone(&self.failed);
        let shutdown = self.shutdown.clone();
        let ticket = self.workers.try_submit(move || {
            let result = (|| -> Result<usize> {
                let _gameplay = gameplay;
                store.check_available()?;
                if failed.load(Ordering::Acquire) {
                    return Err(WorldError::StorageUnavailable.into());
                }
                if *shutdown.borrow() {
                    return Err(crate::lifecycle::Stopped.into());
                }
                let keys: Vec<_> = grouped.keys().copied().collect();
                let stripe_ids: BTreeSet<_> = keys.iter().map(|key| stripe(key.x, key.z)).collect();
                let _guards: Vec<_> = stripe_ids
                    .iter()
                    .map(|index| stripes[*index].lock().map_err(|_| "chunk lock poisoned"))
                    .collect::<std::result::Result<_, _>>()?;
                let recipients = players.snapshots()?;
                let reads = store.snapshot(&keys)?;
                let mut writes = Vec::new();
                let mut changed = Vec::new();
                for snapshot in &reads {
                    let key = snapshot.key();
                    let mut chunk = match snapshot.chunk() {
                        Some(chunk) => (**chunk).clone(),
                        None if store.generation_policy() == GenerationPolicy::ExistingOnly => {
                            return Err(
                                WorldError::MissingImportedChunk { x: key.x, z: key.z }.into()
                            );
                        }
                        None => generate_chunk(key.x, key.z, &data, generator),
                    };
                    validate_chunk(&chunk, &data)?;
                    let before = changed.len();
                    for &([x, y, z], expected, replacement) in &grouped[&key] {
                        let old = chunk
                            .get_block(x.rem_euclid(16) as u8, y, z.rem_euclid(16) as u8)
                            .ok_or(WorldError::OutOfBounds)?;
                        if expected.is_some_and(|expected| old != expected) {
                            return Err(BulkEditConflict.into());
                        }
                        if old != replacement {
                            chunk.set_block(
                                x.rem_euclid(16) as u8,
                                y,
                                z.rem_euclid(16) as u8,
                                replacement,
                            )?;
                            changed.push(([x, y, z], replacement));
                        }
                    }
                    if changed.len() != before {
                        writes.push(ChunkWrite {
                            key,
                            replacement: Arc::new(chunk),
                        });
                    }
                }
                if changed.is_empty() {
                    return Ok(0);
                }
                let packets = crate::edit_packets::section_updates(&changed);
                let mut publication = BulkPublication {
                    failed: Arc::clone(&failed),
                    shutdown: shutdown.clone(),
                    committed: false,
                };
                match store.commit(uuid::Uuid::new_v4(), &reads, &writes) {
                    Ok(_) => publication.committed = true,
                    Err(error @ CommitError::Indeterminate { .. }) => {
                        fail_world(&failed, &shutdown);
                        return Err(error.into());
                    }
                    Err(error) => {
                        if store.check_available().is_err() {
                            fail_world(&failed, &shutdown);
                        }
                        return Err(error.into());
                    }
                }
                {
                    let mut cache = cache
                        .lock()
                        .map_err(|_| "edit committed but cache publication failed")?;
                    for write in writes {
                        cache.insert(write.replacement);
                    }
                }
                players.dispatch_batches(
                    recipients
                        .into_iter()
                        .map(|player| (player.uuid, player.id, packets.clone()))
                        .collect(),
                )?;
                publication.committed = false;
                Ok(changed.len())
            })();
            Ok(result)
        })?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            ticket.wait()
        })
        .await??
    }

    pub async fn read_blocks(&self, positions: Vec<[i32; 3]>) -> Result<Vec<u32>> {
        self.check_available()?;
        if positions.len() > MAX_EDIT_BLOCKS {
            return Err("block read exceeds the 65536 position limit".into());
        }
        let mut keys = BTreeSet::new();
        for &position in &positions {
            validate_position(position)?;
            keys.insert(key(position));
            if keys.len() > MAX_READ_CHUNKS {
                return Err("block read exceeds the 256 chunk limit".into());
            }
        }
        let gameplay = Arc::clone(&self.gameplay).read_owned().await;
        let store = Arc::clone(&self.store);
        let cache = Arc::clone(&self.cache);
        let data = Arc::clone(&self.data);
        let generator = self.generator;
        let stripes = Arc::clone(&self.stripes);
        let shutdown = self.shutdown.clone();
        self.execute(move || {
            let _gameplay = gameplay;
            if *shutdown.borrow() {
                return Err(crate::lifecycle::Stopped.into());
            }
            let stripe_ids: BTreeSet<_> = keys.iter().map(|key| stripe(key.x, key.z)).collect();
            let _guards: Vec<_> = stripe_ids
                .iter()
                .map(|index| stripes[*index].lock().map_err(|_| "chunk lock poisoned"))
                .collect::<std::result::Result<_, _>>()?;
            store.check_available()?;
            let mut chunks = BTreeMap::new();
            for key in keys {
                let cached = cache
                    .lock()
                    .map_err(|_| "chunk cache lock poisoned")?
                    .get(key.x, key.z);
                let chunk = match cached {
                    Some(chunk) => chunk,
                    None => Arc::new(
                        store
                            .load(key.x, key.z)?
                            .unwrap_or_else(|| generate_chunk(key.x, key.z, &data, generator)),
                    ),
                };
                validate_chunk(&chunk, &data)?;
                chunks.insert(key, chunk);
            }
            positions
                .into_iter()
                .map(|position @ [x, y, z]| {
                    chunks[&key(position)]
                        .get_block(x.rem_euclid(16) as u8, y, z.rem_euclid(16) as u8)
                        .ok_or_else(|| WorldError::OutOfBounds.into())
                })
                .collect()
        })
        .await
    }
}

struct BulkPublication {
    failed: Arc<std::sync::atomic::AtomicBool>,
    shutdown: tokio::sync::watch::Sender<bool>,
    committed: bool,
}
impl Drop for BulkPublication {
    fn drop(&mut self) {
        if self.committed {
            fail_world(&self.failed, &self.shutdown);
        }
    }
}

fn key([x, _, z]: [i32; 3]) -> ChunkKey {
    ChunkKey::new(x.div_euclid(16), z.div_euclid(16))
}
fn validate_position([x, y, z]: [i32; 3]) -> Result<()> {
    if x.unsigned_abs() > 30_000_000
        || z.unsigned_abs() > 30_000_000
        || !(MIN_Y..MIN_Y + WORLD_HEIGHT as i32).contains(&y)
    {
        return Err("block position is outside the world".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Settings,
        game_data::GameData,
        lifecycle::Lifecycle,
        players::{EntityIds, Players},
    };
    use std::path::Path;
    use tokio::sync::{RwLock, watch};
    use zerqon_world::WorldStore;

    #[tokio::test]
    #[ignore = "requires prepared official Minecraft 26.3 reports"]
    async fn bulk_edits_persist_and_reject_entire_stale_or_invalid_selection() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let data = Arc::new(GameData::load(&root.join(".cache/vanilla/26.3")).unwrap());
        let palette = data.flat;
        let settings = Settings {
            world_threads: 1,
            work_queue_capacity: 4,
            ..Settings::default()
        };
        let (shutdown, _) = watch::channel(false);
        let world_path = temporary.path().join("world");
        let world = World::new(
            &world_path,
            &settings,
            data,
            Arc::new(RwLock::new(())),
            Players::new(1, 64.0, 0, Arc::new(EntityIds::default())),
            shutdown,
            Lifecycle::new(),
            None,
        )
        .unwrap();
        let first = [-1, 64, -1];
        let second = [16, 64, 0];
        assert_eq!(
            world
                .edit_blocks(vec![(first, palette.grass), (second, palette.stone)])
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            world
                .read_blocks(vec![second, first, second])
                .await
                .unwrap(),
            vec![palette.stone, palette.grass, palette.stone]
        );
        let stale = world
            .edit_blocks_if(vec![
                (first, palette.grass, palette.dirt),
                (second, palette.air, palette.dirt),
            ])
            .await
            .unwrap_err();
        assert!(stale.downcast_ref::<BulkEditConflict>().is_some());
        assert!(
            world
                .edit_blocks(vec![(first, palette.dirt), ([0, 320, 0], palette.dirt)])
                .await
                .is_err()
        );
        assert!(
            world
                .edit_blocks(vec![(first, palette.dirt), (first, palette.stone)])
                .await
                .is_err()
        );
        assert!(
            world
                .edit_blocks((0..17).map(|x| ([x * 16, 64, 0], palette.dirt)).collect())
                .await
                .is_err()
        );
        assert_eq!(
            world.read_blocks(vec![first, second]).await.unwrap(),
            vec![palette.grass, palette.stone]
        );
        drop(world);
        let store = WorldStore::open(&world_path).unwrap();
        assert_eq!(
            store.load(-1, -1).unwrap().unwrap().get_block(15, 64, 15),
            Some(palette.grass)
        );
        assert_eq!(
            store.load(1, 0).unwrap().unwrap().get_block(0, 64, 0),
            Some(palette.stone)
        );
    }
}
