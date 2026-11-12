use super::{BlockInteraction, State, World, block_update, fail_world, stripe, validate_chunk};
use crate::{
    Result,
    managed_tasks::CleanupError,
    player_control::SessionKey,
    player_inventory::{InventoryHandle, MixedInventoryLease},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
use tokio::sync::{OwnedRwLockWriteGuard, OwnedSemaphorePermit};
use uuid::Uuid;
use zerqon_blocks::{
    Aabb, Bounds, BuildError, Direction, Interaction, Placement, Position, StateId,
};
use zerqon_world::{
    ChunkKey, ChunkWrite, CommitError, DropKey, DropWrite, GenerationPolicy, PlayerKey,
};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Preparing,
    Committing,
    Committed,
    Published,
}

struct BlockEdit {
    key: SessionKey,
    action: BlockInteraction,
    rules: Arc<zerqon_blocks::Catalog>,
    actor_position: crate::players::Position,
    blockers: Vec<Aabb>,
    secondary_use: bool,
    loot_key: DropKey,
    #[cfg(test)]
    after_commit: Option<Box<dyn FnOnce() + Send>>,
}

pub(super) enum ItemAction {
    Throw {
        all: bool,
    },
    Pickup(DropKey),
    Packet {
        id: i32,
        body: Vec<u8>,
        can_interact: bool,
    },
}
struct ItemEdit {
    key: SessionKey,
    action: ItemAction,
    position: crate::players::Position,
    new_key: DropKey,
}
enum Operation {
    Block(BlockEdit),
    Item(ItemEdit),
}

/// Own the physical operation and all asynchronous leases outside catch_unwind.
/// Cancelling result delivery cannot release gameplay or inventory admission.
struct OwnedMixedJob {
    world: Arc<World>,
    gameplay: Option<OwnedRwLockWriteGuard<()>>,
    permit: Option<OwnedSemaphorePermit>,
    inventory: Option<MixedInventoryLease>,
    ignition: Option<crate::tnt::IgnitionReservation>,
    drops: Option<crate::item_drops::DropLease>,
    operation: Option<Operation>,
    phase: Phase,
}

impl OwnedMixedJob {
    fn run(mut self) -> Result<()> {
        let operation = self.operation.take().expect("owned mixed operation");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match operation {
            Operation::Block(operation) => operation.run(
                &self.world,
                self.inventory.as_ref().expect("owned admission"),
                &mut self.phase,
                &mut self.ignition,
                &mut self.drops,
            ),
            Operation::Item(operation) => operation.run(
                &self.world,
                self.inventory.as_ref().expect("owned admission"),
                &mut self.phase,
                self.drops.as_mut().expect("owned item lease"),
            ),
        }));
        let result = match result {
            Ok(result) => result,
            Err(payload) => {
                let cause = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("non-string panic");
                Err(CleanupError(format!("mixed world worker panicked: {cause}")).into())
            }
        };
        let terminal = result.as_ref().err().is_some_and(|error| {
            matches!(self.phase, Phase::Committing | Phase::Committed)
                || crate::managed_tasks::is_cleanup_failure(error.as_ref())
                || self.world.check_health().is_err()
        });
        let cleanup = if terminal {
            Err(format!(
                "mixed world operation failed: {}",
                result.as_ref().unwrap_err()
            ))
        } else {
            Ok(())
        };
        if result.is_ok() {
            self.phase = Phase::Published;
        }
        // Return a typed terminal error so callers cannot send rollback/sync packets.
        let result = if let Err(cause) = &cleanup {
            Err(CleanupError(cause.clone()).into())
        } else {
            result
        };
        self.settle(cleanup);
        result
    }

    fn settle(&mut self, cleanup: std::result::Result<(), String>) {
        if let Err(cause) = &cleanup {
            fail_world(&self.world.failed, &self.world.shutdown);
            if let Some(inventory) = &self.inventory {
                inventory.poison(cause);
            }
        }
        drop(self.operation.take());
        drop(self.permit.take());
        drop(self.gameplay.take());
        drop(self.ignition.take());
        drop(self.drops.take());
        if let Some(inventory) = self.inventory.take() {
            inventory.settle(cleanup);
        }
    }

    fn reject_unsubmitted(mut self) {
        drop(self.operation.take());
        drop(self.permit.take());
        drop(self.gameplay.take());
        drop(self.ignition.take());
        drop(self.drops.take());
        if let Some(inventory) = self.inventory.take() {
            inventory.reject_unsubmitted();
        }
    }
}

impl Drop for OwnedMixedJob {
    fn drop(&mut self) {
        if self.inventory.is_some() {
            self.settle(Err(
                "accepted mixed world operation ended before settling".into()
            ));
        }
    }
}

pub(super) async fn edit_block(
    state: &State,
    uuid: Uuid,
    entity_id: i32,
    inventory: &InventoryHandle,
    action: BlockInteraction,
) -> Result<()> {
    let job = prepare(state, uuid, entity_id, inventory, action).await?;
    submit(job).await
}

pub(super) async fn edit_item(
    state: &State,
    uuid: Uuid,
    entity_id: i32,
    inventory: Option<&InventoryHandle>,
    action: ItemAction,
) -> Result<()> {
    let world = Arc::clone(state.world.as_ref().ok_or("World unavailable")?);
    let finishing = matches!(&action, ItemAction::Packet { id: 19, .. });
    let service = state.drops.as_ref().ok_or("Item entities unavailable")?;
    let drops = if finishing {
        service.reserve_cleanup().await?
    } else {
        service.reserve().await?
    };
    if let ItemAction::Pickup(key) = &action
        && drops.item(*key).is_none()
    {
        return Ok(());
    }
    let gameplay = Arc::clone(&state.gameplay).write_owned().await;
    if finishing {
        world.check_health()?;
    } else {
        world.check_available()?;
    }
    if !finishing && !state.players.is_alive(uuid, entity_id)? {
        return if matches!(&action, ItemAction::Pickup(_)) {
            Ok(())
        } else {
            Err("Player is dead or no longer connected".into())
        };
    }
    let position = state
        .players
        .query_snapshot(uuid)?
        .filter(|p| p.id == entity_id)
        .ok_or("Player session ended")?
        .position;
    let permit = Arc::clone(&world.inflight).acquire_owned().await?;
    let key = SessionKey {
        uuid,
        session_id: entity_id,
    };
    let inventory = if let Some(inventory) = inventory {
        let permit = inventory.reserve_mixed_io().await?;
        inventory.admit_mixed(key, permit)?
    } else {
        let Some(lease) = state
            .inventories
            .as_ref()
            .ok_or("Inventory unavailable")?
            .reserve_passive(uuid, entity_id)
            .await?
        else {
            return Ok(());
        };
        lease
    };
    if !inventory.same_world(&world.store) {
        inventory.reject_unsubmitted();
        return Err("Inventory belongs to another world".into());
    }
    submit(OwnedMixedJob {
        world,
        gameplay: Some(gameplay),
        permit: Some(permit),
        inventory: Some(inventory),
        ignition: None,
        drops: Some(drops),
        operation: Some(Operation::Item(ItemEdit {
            key,
            action,
            position,
            new_key: DropKey::new(Uuid::new_v4()),
        })),
        phase: Phase::Preparing,
    })
    .await
}

impl ItemEdit {
    fn run(
        self,
        world: &World,
        inventory: &MixedInventoryLease,
        phase: &mut Phase,
        drops: &mut crate::item_drops::DropLease,
    ) -> Result<()> {
        if matches!(&self.action, ItemAction::Packet { id: 19, .. }) {
            world.check_health()?;
        } else {
            world.check_available()?;
        }
        let io = inventory.lock_io()?;
        let mut player = io.lock_player()?;
        let existing_key = if let ItemAction::Pickup(key) = &self.action {
            Some(*key)
        } else {
            None
        };
        let mut reads = world.store.snapshot_domain(
            &[],
            &[PlayerKey::new(self.key.uuid)],
            existing_key.as_slice(),
        )?;
        player.check_snapshot(&reads.players[0])?;
        let mut next = player.current().record.clone();
        let policy = zerqon_inventory::InventoryPolicy {
            creative: next.game_mode == crate::game_mode::GameMode::Creative,
            can_interact: true,
            allow_drops: true,
        };
        let mut remainder = None;
        let mut amount = 0;
        let mut writes = Vec::new();
        let new_stacks = match &self.action {
            ItemAction::Throw { all } => {
                let Some(effects) =
                    next.inventory
                        .drop_selected(inventory.catalog(), *all, policy)?
                else {
                    let revision = player.current().revision;
                    return Ok(player.prepare_after(next)?.publish(revision).enqueue()?);
                };
                effects.drops
            }
            ItemAction::Packet {
                id,
                body,
                can_interact,
            } => inventory.apply_packet(&mut next, *id, body, *can_interact)?,
            ItemAction::Pickup(key) => {
                let Some((_, mut record, revision)) = drops.item(*key) else {
                    return Ok(());
                };
                if revision != reads.drops[0].revision() {
                    return Err(CleanupError(
                        "Item pickup revision differs from live simulation".into(),
                    )
                    .into());
                }
                if !crate::item_drops::pickup_allowed(&record, self.key.uuid, self.position.xyz) {
                    return Ok(());
                }
                let result =
                    next.inventory
                        .insert_pickup(inventory.catalog(), &record.stack, policy)?;
                if result.inserted == 0 && result.discarded == 0 {
                    return Ok(());
                }
                amount = i32::from(result.inserted) + i32::from(result.discarded);
                remainder = result.remainder.map(|stack| {
                    record.stack = stack;
                    record
                });
                writes.push(DropWrite {
                    key: *key,
                    replacement: remainder
                        .as_ref()
                        .map(|record| drops.encode(*key, record))
                        .transpose()?,
                });
                Vec::new()
            }
        };
        if new_stacks.len() >= zerqon_world::MAX_DOMAIN_WRITES {
            return Err("Too many dropped stacks in one action".into());
        }
        if !new_stacks.is_empty() {
            let keys: Vec<_> = (0..new_stacks.len())
                .map(|i| {
                    if i == 0 {
                        self.new_key
                    } else {
                        DropKey::new(Uuid::new_v4())
                    }
                })
                .collect();
            reads = world
                .store
                .snapshot_domain(&[], &[PlayerKey::new(self.key.uuid)], &keys)?;
            player.check_snapshot(&reads.players[0])?;
            if reads.drops.iter().any(|read| read.revision() != 0) {
                return Err("New item identity already exists".into());
            }
            for (key, stack) in keys.into_iter().zip(new_stacks) {
                writes.push(drops.stage(
                    key,
                    crate::item_drops::thrown_record(key, stack, self.position, self.key.uuid),
                )?);
            }
        }
        let prepared = player.prepare_after(next)?;
        *phase = Phase::Committing;
        let receipt = match world.store.commit_domain(
            Uuid::new_v4(),
            &reads,
            &[],
            std::slice::from_ref(prepared.write()),
            &writes,
        ) {
            Ok(receipt) => {
                *phase = Phase::Committed;
                receipt
            }
            Err(error @ (CommitError::Conflict | CommitError::Indeterminate { .. })) => {
                return Err(CleanupError(format!("Item transaction failed: {error}")).into());
            }
            Err(error) => {
                *phase = Phase::Preparing;
                return Err(error.into());
            }
        };
        prepared.publish(receipt.revision).enqueue()?;
        if let Some(key) = existing_key {
            drops.publish_pickup(
                key,
                remainder,
                receipt.revision,
                self.key.session_id,
                amount,
            )?;
        } else if !writes.is_empty() {
            drops.publish_new(receipt.revision)?;
        }
        Ok(())
    }
}

async fn submit(job: OwnedMixedJob) -> Result<()> {
    let world = Arc::clone(&job.world);
    // try_submit destroys the closure on rejection. Keep ownership recoverable
    // until acceptance, so QueueFull is ordinary rejection rather than abandonment.
    let handoff = Arc::new(Mutex::new(Some(job)));
    let worker_handoff = Arc::clone(&handoff);
    let ticket = match world.workers.try_submit(move || {
        let job = worker_handoff
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .expect("single mixed worker owner");
        Ok(job.run())
    }) {
        Ok(ticket) => ticket,
        Err(error) => {
            if let Some(job) = handoff
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                job.reject_unsubmitted();
            }
            return Err(error.into());
        }
    };
    drop(handoff);
    // The waiter owns only result delivery; physical leases stay in the worker.
    tokio::task::spawn_blocking(move || ticket.wait()).await???;
    Ok(())
}

async fn prepare(
    state: &State,
    uuid: Uuid,
    entity_id: i32,
    inventory: &InventoryHandle,
    action: BlockInteraction,
) -> Result<OwnedMixedJob> {
    let world = Arc::clone(state.world.as_ref().ok_or("world unavailable")?);
    world.check_available()?;
    let rules = Arc::clone(state.blocks.as_ref().ok_or("block rules unavailable")?);
    let drops = if matches!(&action, BlockInteraction::Mined { .. }) {
        Some(
            state
                .drops
                .as_ref()
                .ok_or("Item entities unavailable")?
                .reserve()
                .await?,
        )
    } else {
        None
    };
    let ignition = if matches!(&action, BlockInteraction::Place(_)) {
        match &state.tnt {
            Some(tnt) => Some(tnt.reserve_ignition().await?),
            None => None,
        }
    } else {
        None
    };
    let gameplay = Arc::clone(&state.gameplay).write_owned().await;
    let permit = Arc::clone(&world.inflight).acquire_owned().await?;
    let inventory_permit = inventory.reserve_mixed_io().await?;
    world.check_available()?;
    if !state.players.is_alive(uuid, entity_id)? {
        return Err("Player is dead or no longer connected".into());
    }
    let observers = state.players.snapshots()?;
    let actor_position = observers
        .iter()
        .find(|player| player.uuid == uuid && player.id == entity_id)
        .ok_or("player session is no longer active")?
        .position;
    let secondary_use = state.players.is_sneaking(uuid, entity_id)?;
    let clicked = match &action {
        BlockInteraction::Place(request) => request.block,
        BlockInteraction::Break(request) | BlockInteraction::Mined { request, .. } => request.block,
    };
    let near = [
        f64::from(clicked.x),
        f64::from(clicked.y),
        f64::from(clicked.z),
    ];
    let mut blockers: Vec<Aabb> = observers
        .iter()
        .filter_map(|player| {
            let xyz = player.position.xyz;
            let bounds = Aabb {
                min: [xyz[0] - 0.3, xyz[1], xyz[2] - 0.3],
                max: [xyz[0] + 0.3, xyz[1] + 1.8, xyz[2] + 0.3],
            };
            (0..3)
                .all(|axis| {
                    bounds.max[axis] >= near[axis] - 3.0 && bounds.min[axis] <= near[axis] + 4.0
                })
                .then_some(bounds)
        })
        .collect();
    if let Some(mobs) = &state.mobs {
        blockers.extend(mobs.blockers_near(near)?);
    }
    if blockers.len() > 1024 {
        return Err("too many nearby entities for block interaction".into());
    }
    let key = SessionKey {
        uuid,
        session_id: entity_id,
    };
    let lease = inventory.admit_mixed(key, inventory_permit)?;
    if !lease.same_world(&world.store) {
        lease.reject_unsubmitted();
        return Err("inventory belongs to a different world store".into());
    }
    Ok(OwnedMixedJob {
        world,
        gameplay: Some(gameplay),
        permit: Some(permit),
        inventory: Some(lease),
        ignition,
        drops,
        operation: Some(Operation::Block(BlockEdit {
            key,
            action,
            rules,
            actor_position,
            blockers,
            secondary_use,
            loot_key: DropKey::new(Uuid::new_v4()),
            #[cfg(test)]
            after_commit: None,
        })),
        phase: Phase::Preparing,
    })
}

impl BlockEdit {
    fn run(
        self,
        world: &World,
        inventory: &MixedInventoryLease,
        phase: &mut Phase,
        ignition: &mut Option<crate::tnt::IgnitionReservation>,
        drops: &mut Option<crate::item_drops::DropLease>,
    ) -> Result<()> {
        world.check_available()?;
        let (clicked, hand) = match &self.action {
            BlockInteraction::Place(request) => (request.block, request.hand),
            BlockInteraction::Break(request) | BlockInteraction::Mined { request, .. } => {
                (request.block, zerqon_inventory::Hand::Main)
            }
        };
        let target = Position::new(clicked.x, clicked.y, clicked.z);
        let center = ChunkKey::new(clicked.x.div_euclid(16), clicked.z.div_euclid(16));
        let keys: Vec<_> = (-1..=1)
            .flat_map(|x| (-1..=1).map(move |z| ChunkKey::new(center.x + x, center.z + z)))
            .collect();
        let io = inventory.lock_io()?;
        let lock_ids: BTreeSet<_> = keys.iter().map(|key| stripe(key.x, key.z)).collect();
        let _locks: Vec<_> = lock_ids
            .iter()
            .map(|id| {
                world.stripes[*id]
                    .lock()
                    .map_err(|_| CleanupError("chunk lock poisoned".into()))
            })
            .collect::<std::result::Result<_, _>>()?;
        let mut player = io.lock_player()?;
        let mut reads = world
            .store
            .snapshot_domain(
                &keys,
                &[PlayerKey::new(self.key.uuid)],
                if drops.is_some() {
                    std::slice::from_ref(&self.loot_key)
                } else {
                    &[]
                },
            )
            .map_err(|error| CleanupError(format!("mixed world snapshot failed: {error}")))?;
        player.check_snapshot(&reads.players[0])?;
        let record = &player.current().record;
        let held = match hand {
            zerqon_inventory::Hand::Main => record.inventory.selected_stack(),
            zerqon_inventory::Hand::Off => &record.inventory.slots()[45],
        };
        let chunks: BTreeMap<_, _> = reads
            .chunks
            .iter()
            .filter_map(|snapshot| {
                let key = snapshot.key();
                snapshot
                    .chunk()
                    .cloned()
                    .or_else(|| {
                        (world.store.generation_policy() == GenerationPolicy::NativeFlat)
                            .then(|| Arc::new(world.generate(key.x, key.z)))
                    })
                    .map(|chunk| (key, chunk))
            })
            .collect();
        for chunk in chunks.values() {
            validate_chunk(chunk, &world.data).map_err(|error| {
                CleanupError(format!("mixed world chunk failed validation: {error}"))
            })?;
        }
        let view = |position: Position| {
            chunks
                .get(&ChunkKey::new(
                    position.x.div_euclid(16),
                    position.z.div_euclid(16),
                ))
                .and_then(|chunk| {
                    chunk.get_block(
                        position.x.rem_euclid(16) as u8,
                        position.y,
                        position.z.rem_euclid(16) as u8,
                    )
                })
                .map(StateId)
        };
        let interaction = Interaction {
            eye: [
                self.actor_position.xyz[0],
                self.actor_position.xyz[1] + 1.62,
                self.actor_position.xyz[2],
            ],
            block_range: 5.0,
            creative: record.game_mode == crate::game_mode::GameMode::Creative,
            bounds: Bounds::default(),
            blockers: &self.blockers,
        };
        let correction_face = match &self.action {
            BlockInteraction::Place(request) => Direction::from_id(request.face as u8),
            BlockInteraction::Break(_) | BlockInteraction::Mined { .. } => None,
        };
        let mut ignite_tnt = false;
        let mut used_block = false;
        let plan = (|| -> std::result::Result<zerqon_blocks::EditPlan, BuildError> {
            if let BlockInteraction::Mined {
                expected,
                held: original,
                ..
            } = &self.action
                && (view(target) != Some(StateId(*expected)) || original != held)
            {
                return Err(BuildError::Invalid);
            }
            if matches!(&self.action, BlockInteraction::Break(_)) && !interaction.creative {
                return Err(BuildError::Survival);
            }
            match &self.action {
                BlockInteraction::Place(request) => {
                    if !self.secondary_use
                        && let Some(plan) = self.rules.plan_use(&view, &interaction, target)?
                    {
                        used_block = true;
                        return Ok(plan);
                    }
                    let Some(held) = held.as_ref() else {
                        return Ok(zerqon_blocks::EditPlan::unchanged());
                    };
                    if crate::ignition_item::is_igniter(inventory.catalog(), held)
                        && view(target)
                            .and_then(|state| self.rules.state(state).ok())
                            .is_some_and(|state| state.key() == "minecraft:tnt")
                    {
                        if Direction::from_id(request.face as u8).is_none()
                            || request.hit.iter().any(|value| {
                                !value.is_finite() || (f64::from(*value) - 0.5).abs() >= 1.0000001
                            })
                        {
                            return Err(BuildError::Invalid);
                        }
                        let plan = self.rules.plan_ignite_tnt(&view, &interaction, target)?;
                        ignite_tnt = true;
                        return Ok(plan);
                    }
                    let properties = held
                        .patch()
                        .added()
                        .values()
                        .find_map(|value| match value {
                            zerqon_inventory::ComponentValue::BlockState(value) => {
                                Some(value.clone())
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    let facing = match ((self.actor_position.yaw as f64 / 90.0 + 0.5).floor()
                        as i64)
                        .rem_euclid(4)
                    {
                        0 => Direction::South,
                        1 => Direction::West,
                        2 => Direction::North,
                        _ => Direction::East,
                    };
                    self.rules.plan_placement(
                        &view,
                        &interaction,
                        &Placement {
                            clicked: target,
                            face: Direction::from_id(request.face as u8)
                                .ok_or(BuildError::Invalid)?,
                            local_hit: request.hit,
                            facing,
                            item: held.item().0,
                            secondary_use: self.secondary_use,
                            properties: &properties,
                        },
                    )
                }
                BlockInteraction::Break(_) | BlockInteraction::Mined { .. } => {
                    self.rules.plan_break(
                        &view,
                        &interaction,
                        target,
                        held.as_ref().map(|stack| stack.item().0),
                    )
                }
            }
        })();
        let plan = match plan {
            Ok(plan) => plan,
            Err(error) => {
                let corrections = std::iter::once(target)
                    .chain(correction_face.and_then(|face| target.relative(face)))
                    .filter_map(|position| {
                        view(position).map(|state| {
                            bytes::Bytes::from(block_update(
                                position.x, position.y, position.z, state.0,
                            ))
                        })
                    })
                    .collect::<Vec<_>>();
                world.players.dispatch_batches(vec![(
                    self.key.uuid,
                    self.key.session_id,
                    corrections,
                )])?;
                return Err(error.into());
            }
        };
        if plan.changes().is_empty() {
            return Ok(());
        }
        let mut drop_writes = Vec::new();
        let after = if ignite_tnt {
            ignition
                .as_mut()
                .ok_or("TNT simulation unavailable")?
                .stage([target.x, target.y, target.z])?;
            let old = held.as_ref().ok_or("ignition item disappeared")?;
            let replacement = crate::ignition_item::after_ignition(
                inventory.catalog(),
                old,
                interaction.creative,
            )?;
            if &replacement != held {
                let mut next = record.clone();
                let slot = match hand {
                    zerqon_inventory::Hand::Main => 36 + i16::from(next.inventory.selected()),
                    zerqon_inventory::Hand::Off => 45,
                };
                next.inventory.creative_set(
                    inventory.catalog(),
                    &zerqon_inventory::CreativeSlotRequest {
                        slot,
                        stack: replacement,
                    },
                    zerqon_inventory::InventoryPolicy {
                        creative: true,
                        can_interact: true,
                        allow_drops: false,
                    },
                )?;
                Some(next)
            } else {
                None
            }
        } else if !interaction.creative && !used_block {
            let mut next = record.clone();
            let slot = match hand {
                zerqon_inventory::Hand::Main => 36 + i16::from(next.inventory.selected()),
                zerqon_inventory::Hand::Off => 45,
            };
            let replacement = match &self.action {
                BlockInteraction::Place(_) => {
                    let stack = held.as_ref().ok_or("No held block")?;
                    if stack.count() > 1 {
                        Some(stack.with_count(stack.count() - 1)?)
                    } else {
                        None
                    }
                }
                BlockInteraction::Mined { expected, .. } => {
                    let profile = crate::mining::profile(
                        self.rules.state(StateId(*expected))?.key(),
                        held,
                        inventory.catalog(),
                    )?;
                    let replacement = if profile.wear {
                        held.as_ref()
                            .map(|stack| {
                                crate::ignition_item::wear_tool(inventory.catalog(), stack)
                            })
                            .transpose()?
                            .flatten()
                    } else {
                        held.clone()
                    };
                    if let Some(drop) = profile.drop {
                        if reads.drops.iter().any(|read| read.revision() != 0) {
                            return Err("New item identity already exists".into());
                        }
                        let record = crate::item_drops::mined_record(
                            self.loot_key,
                            drop,
                            [target.x, target.y, target.z],
                        );
                        drop_writes.push(
                            drops
                                .as_mut()
                                .ok_or("Item entity reservation missing")?
                                .stage(self.loot_key, record)?,
                        );
                    }
                    if replacement == *held {
                        next.inventory.slots()[slot as usize].clone()
                    } else {
                        replacement
                    }
                }
                BlockInteraction::Break(_) => {
                    return Err("Survival mining requires a completed dig".into());
                }
            };
            next.inventory.creative_set(
                inventory.catalog(),
                &zerqon_inventory::CreativeSlotRequest {
                    slot,
                    stack: replacement,
                },
                zerqon_inventory::InventoryPolicy {
                    creative: true,
                    can_interact: true,
                    allow_drops: false,
                },
            )?;
            Some(next)
        } else {
            None
        };
        let prepared_player = after
            .map(|record| player.prepare_after(record))
            .transpose()?;
        let read_keys: BTreeSet<_> = plan
            .reads()
            .iter()
            .map(|(position, _)| {
                ChunkKey::new(position.x.div_euclid(16), position.z.div_euclid(16))
            })
            .collect();
        reads
            .chunks
            .retain(|snapshot| read_keys.contains(&snapshot.key()));
        let mut changed = BTreeMap::new();
        for change in plan.changes() {
            let key = ChunkKey::new(
                change.position.x.div_euclid(16),
                change.position.z.div_euclid(16),
            );
            let chunk = changed.entry(key).or_insert_with(|| {
                (**chunks
                    .get(&key)
                    .expect("planner read set includes every changed chunk"))
                .clone()
            });
            chunk.set_block(
                change.position.x.rem_euclid(16) as u8,
                change.position.y,
                change.position.z.rem_euclid(16) as u8,
                change.replacement.0,
            )?;
        }
        let writes: Vec<_> = changed
            .into_iter()
            .map(|(key, chunk)| ChunkWrite {
                key,
                replacement: Arc::new(chunk),
            })
            .collect();
        let updates: Vec<_> = plan
            .changes()
            .iter()
            .map(|change| {
                bytes::Bytes::from(block_update(
                    change.position.x,
                    change.position.y,
                    change.position.z,
                    change.replacement.0,
                ))
            })
            .collect();
        *phase = Phase::Committing;
        let player_writes = prepared_player
            .as_ref()
            .map(|change| std::slice::from_ref(change.write()))
            .unwrap_or(&[]);
        let receipt = match world.store.commit_domain(
            Uuid::new_v4(),
            &reads,
            &writes,
            player_writes,
            &drop_writes,
        ) {
            Ok(receipt) => {
                *phase = Phase::Committed;
                receipt
            }
            Err(error @ (CommitError::Conflict | CommitError::Indeterminate { .. })) => {
                return Err(CleanupError(format!("mixed world commit failed: {error}")).into());
            }
            Err(error) => {
                *phase = Phase::Preparing;
                return Err(error.into());
            }
        };
        let inventory_publication = prepared_player.map(|change| change.publish(receipt.revision));
        #[cfg(test)]
        if let Some(after_commit) = self.after_commit {
            after_commit();
        }
        {
            let mut cache = world
                .cache
                .lock()
                .map_err(|_| "world edit committed but cache publication failed")?;
            for write in writes {
                cache.insert(write.replacement);
            }
        }
        // Every in-memory projection is now ready; only now may success updates escape.
        for packet in updates {
            world.players.broadcast(packet)?;
        }
        if let Some(publication) = inventory_publication {
            publication.enqueue()?;
        }
        if ignite_tnt {
            ignition
                .take()
                .ok_or("TNT ignition reservation disappeared")?
                .publish()?;
        }
        if !drop_writes.is_empty() {
            drops
                .as_mut()
                .ok_or("Committed item reservation disappeared")?
                .publish_new(receipt.revision)?;
        }
        Ok(())
    }
}
