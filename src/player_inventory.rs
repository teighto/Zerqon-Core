use crate::{
    player_store::PlayerStore,
    players::{InventoryRevision, PacketBatch, Players},
};
use bytes::Bytes;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
use uuid::Uuid;
use zerqon_inventory::{
    CreativeSlotRequest, DecodeLimits, InventoryPolicy, ItemCatalog, ItemId, ItemStack,
    PlayerInventory, ValidatedPatch, decode_carried_slot, decode_click, decode_creative_slot,
    encode_content_body, encode_held_slot_body, encode_server_stack,
};

pub const MAX_INVENTORY_SYNC_BYTES: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum InventoryServiceError {
    #[error("player inventory is unavailable: {0}")]
    Unavailable(&'static str),
    #[error("inventory resource limit exceeded: {0}")]
    Limit(&'static str),
    #[error("inventory mutations are rate limited; retry shortly")]
    RateLimited,
    #[error(transparent)]
    Item(#[from] zerqon_inventory::ItemError),
    #[error(transparent)]
    Inventory(#[from] zerqon_inventory::InventoryError),
    #[error(transparent)]
    Store(#[from] crate::player_store::StoreError),
    #[error(transparent)]
    Player(#[from] crate::players::PlayerError),
    #[error(transparent)]
    Worker(#[from] tokio::task::JoinError),
}

pub struct InventoryService {
    catalog: Arc<ItemCatalog>,
    store: Arc<PlayerStore>,
    players: Arc<Players>,
    sessions: Mutex<HashMap<Uuid, Arc<Session>>>,
    stripes: Arc<[Mutex<()>; 64]>,
    inflight: Arc<Semaphore>,
    capacity: usize,
}
struct Session {
    uuid: Uuid,
    id: i32,
    inventory: Mutex<Option<PlayerInventory>>,
    closed: AtomicBool,
    generation: AtomicU64,
    rate: Mutex<MutationRate>,
}
pub struct InventoryHandle {
    service: Arc<InventoryService>,
    session: Arc<Session>,
}
struct MutationRate {
    started: Instant,
    count: u32,
}
impl MutationRate {
    fn admit(&mut self, now: Instant) -> Result<(), InventoryServiceError> {
        if now.saturating_duration_since(self.started) >= Duration::from_secs(1) {
            self.started = now;
            self.count = 0;
        }
        if self.count >= 32 {
            return Err(InventoryServiceError::RateLimited);
        }
        self.count += 1;
        Ok(())
    }
}
impl InventoryService {
    pub fn new(
        directory: PathBuf,
        catalog: Arc<ItemCatalog>,
        players: Arc<Players>,
        capacity: usize,
    ) -> Result<Arc<Self>, InventoryServiceError> {
        if !(1..=65_536).contains(&capacity) {
            return Err(InventoryServiceError::Limit("player capacity"));
        }
        Ok(Arc::new(Self {
            store: Arc::new(PlayerStore::new(directory, Arc::clone(&catalog))?),
            catalog,
            players,
            sessions: Mutex::new(HashMap::new()),
            stripes: Arc::new(std::array::from_fn(|_| Mutex::new(()))),
            inflight: Arc::new(Semaphore::new(capacity.min(64))),
            capacity,
        }))
    }
    pub async fn open(
        self: &Arc<Self>,
        uuid: Uuid,
        id: i32,
    ) -> Result<InventoryHandle, InventoryServiceError> {
        if uuid.is_nil() || id <= 0 {
            return Err(InventoryServiceError::Unavailable("invalid identity"));
        }
        let session = Arc::new(Session {
            uuid,
            id,
            inventory: Mutex::new(None),
            closed: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            rate: Mutex::new(MutationRate {
                started: Instant::now(),
                count: 0,
            }),
        });
        {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| InventoryServiceError::Unavailable("session lock poisoned"))?;
            if sessions.contains_key(&uuid) {
                return Err(InventoryServiceError::Unavailable("duplicate session"));
            }
            if sessions.len() >= self.capacity {
                return Err(InventoryServiceError::Limit("player capacity"));
            }
            sessions.insert(uuid, Arc::clone(&session));
        }
        let handle = InventoryHandle {
            service: Arc::clone(self),
            session: Arc::clone(&session),
        };
        let permit = Arc::clone(&self.inflight)
            .acquire_owned()
            .await
            .map_err(|_| InventoryServiceError::Unavailable("I/O workers stopped"))?;
        let store = Arc::clone(&self.store);
        let catalog = Arc::clone(&self.catalog);
        let stripes = Arc::clone(&self.stripes);
        let players = Arc::clone(&self.players);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _stripe = stripes[stripe(uuid)]
                .lock()
                .map_err(|_| InventoryServiceError::Unavailable("inventory I/O lock poisoned"))?;
            ensure_open(&session)?;
            let inventory = store.load(uuid)?;
            let (_, equipment) = packets(id, &inventory, &catalog)?;
            ensure_open(&session)?;
            *session
                .inventory
                .lock()
                .map_err(|_| InventoryServiceError::Unavailable("inventory lock poisoned"))? =
                Some(inventory);
            players.set_equipment(uuid, id, equipment)?;
            Ok::<_, InventoryServiceError>(())
        })
        .await??;
        Ok(handle)
    }
    pub async fn give(
        &self,
        uuid: Uuid,
        id: i32,
        item: &str,
        count: u32,
    ) -> Result<(), InventoryServiceError> {
        if !(1..=4096).contains(&count) {
            return Err(InventoryServiceError::Limit("give count must be 1..4096"));
        }
        let key = if item.contains(':') {
            item.to_owned()
        } else {
            format!("minecraft:{item}")
        };
        let item = self
            .catalog
            .item_id(&key)
            .ok_or(InventoryServiceError::Unavailable(
                "unknown item registry name",
            ))?;
        let session = self
            .sessions
            .lock()
            .map_err(|_| InventoryServiceError::Unavailable("session lock poisoned"))?
            .get(&uuid)
            .filter(|session| session.id == id)
            .cloned()
            .ok_or(InventoryServiceError::Unavailable(
                "player is not connected",
            ))?;
        let batch = self
            .change(session, move |inventory, catalog| {
                grant(inventory, catalog, item, count)
            })
            .await?;
        self.players.dispatch_batches(vec![(uuid, id, batch)])?;
        Ok(())
    }
    async fn change<F>(
        &self,
        session: Arc<Session>,
        operation: F,
    ) -> Result<PacketBatch, InventoryServiceError>
    where
        F: FnOnce(&mut PlayerInventory, &ItemCatalog) -> Result<(), InventoryServiceError>
            + Send
            + 'static,
    {
        let permit = Arc::clone(&self.inflight)
            .acquire_owned()
            .await
            .map_err(|_| InventoryServiceError::Unavailable("I/O workers stopped"))?;
        let store = Arc::clone(&self.store);
        let catalog = Arc::clone(&self.catalog);
        let stripes = Arc::clone(&self.stripes);
        let players = Arc::clone(&self.players);
        tokio::task::spawn_blocking(move||{
            let _permit=permit;let _stripe=stripes[stripe(session.uuid)].lock().map_err(|_|InventoryServiceError::Unavailable("inventory I/O lock poisoned"))?;
            ensure_open(&session)?;
            let mut current=session.inventory.lock().map_err(|_|InventoryServiceError::Unavailable("inventory lock poisoned"))?;
            let mut candidate=current.as_ref().ok_or(InventoryServiceError::Unavailable("inventory is loading"))?.clone();
            operation(&mut candidate,&catalog)?;
            let (packets,equipment)=packets(session.id,&candidate,&catalog)?;
            let generation=session.generation.load(Ordering::Relaxed).checked_add(1).ok_or(InventoryServiceError::Limit("session generation exhausted"))?;
            let previous=current.as_ref().ok_or(InventoryServiceError::Unavailable("inventory is loading"))?;
            if previous.slots()!=candidate.slots()||previous.cursor()!=candidate.cursor()||previous.selected()!=candidate.selected(){
                session.rate.lock().map_err(|_|InventoryServiceError::Unavailable("rate limiter poisoned"))?.admit(Instant::now())?;
                store.save(session.uuid,&candidate)?;
            }
            *current=Some(candidate);
            session.generation.store(generation,Ordering::Release);
            drop(current);
            if let Err(error)=players.set_equipment(session.uuid,session.id,equipment) { tracing::warn!(%error,uuid=%session.uuid,"Inventory committed but equipment delivery failed"); }
            Ok::<_,InventoryServiceError>(packets.with_inventory_revision(InventoryRevision{uuid:session.uuid,entity_id:session.id,generation}))
        }).await?
    }
}
impl InventoryHandle {
    pub fn allow_outbound(&self, batch: &PacketBatch) -> bool {
        batch.inventory_revision().is_none_or(|revision| {
            revision.uuid == self.session.uuid
                && revision.entity_id == self.session.id
                && revision.generation == self.session.generation.load(Ordering::Acquire)
        })
    }
    pub async fn swap_offhand(
        &self,
        policy: InventoryPolicy,
    ) -> Result<PacketBatch, InventoryServiceError> {
        self.service
            .change(Arc::clone(&self.session), move |inventory, catalog| {
                let request = zerqon_inventory::ClickRequest {
                    container_id: 0,
                    state_id: i32::from(inventory.state_id()),
                    slot: 36 + i16::from(inventory.selected()),
                    button: 40,
                    mode: zerqon_inventory::ClickMode::Swap,
                    changed_slots: Vec::new(),
                    carried: zerqon_inventory::HashedStack::Empty,
                };
                inventory.click(
                    catalog,
                    &request,
                    InventoryPolicy {
                        allow_drops: false,
                        ..policy
                    },
                )?;
                Ok(())
            })
            .await
    }
    pub async fn full_sync(&self) -> Result<PacketBatch, InventoryServiceError> {
        let session = Arc::clone(&self.session);
        let catalog = Arc::clone(&self.service.catalog);
        let permit = Arc::clone(&self.service.inflight)
            .acquire_owned()
            .await
            .map_err(|_| InventoryServiceError::Unavailable("I/O workers stopped"))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            ensure_open(&session)?;
            let inventory = session
                .inventory
                .lock()
                .map_err(|_| InventoryServiceError::Unavailable("inventory lock poisoned"))?;
            let inventory = inventory
                .as_ref()
                .ok_or(InventoryServiceError::Unavailable("inventory is loading"))?;
            packets(session.id, inventory, &catalog).map(|(packets, _)| {
                packets.with_inventory_revision(InventoryRevision {
                    uuid: session.uuid,
                    entity_id: session.id,
                    generation: session.generation.load(Ordering::Acquire),
                })
            })
        })
        .await?
    }
    pub async fn apply_packet(
        &self,
        id: i32,
        body: &[u8],
        policy: InventoryPolicy,
    ) -> Result<PacketBatch, InventoryServiceError> {
        if body.len() > 65_536 {
            return Err(InventoryServiceError::Limit("inventory packet bytes"));
        }
        if !matches!(id, 18 | 54 | 57) {
            return Err(InventoryServiceError::Unavailable(
                "unsupported inventory packet",
            ));
        }
        let body = body.to_vec();
        self.service
            .change(Arc::clone(&self.session), move |inventory, catalog| {
                if !policy.can_interact {
                    return Err(zerqon_inventory::InventoryError::InteractionDenied.into());
                }
                match id {
                    57 => {
                        let request =
                            decode_creative_slot(&body, catalog, DecodeLimits::default())?;
                        inventory.creative_set(
                            catalog,
                            &request,
                            InventoryPolicy {
                                allow_drops: false,
                                ..policy
                            },
                        )?;
                    }
                    54 => {
                        inventory.select_hotbar(i16::from(decode_carried_slot(&body)?))?;
                    }
                    18 => {
                        let request = decode_click(&body, catalog, DecodeLimits::default())?;
                        inventory.click(
                            catalog,
                            &request,
                            InventoryPolicy {
                                allow_drops: false,
                                ..policy
                            },
                        )?;
                    }
                    _ => {
                        return Err(InventoryServiceError::Unavailable(
                            "unsupported inventory packet",
                        ));
                    }
                }
                Ok(())
            })
            .await
    }
}

impl Drop for InventoryHandle {
    fn drop(&mut self) {
        self.session.closed.store(true, Ordering::Release);
        if let Ok(mut sessions) = self.service.sessions.lock()
            && sessions
                .get(&self.session.uuid)
                .is_some_and(|session| Arc::ptr_eq(session, &self.session))
        {
            sessions.remove(&self.session.uuid);
        }
    }
}

fn ensure_open(session: &Session) -> Result<(), InventoryServiceError> {
    if session.closed.load(Ordering::Acquire) {
        Err(InventoryServiceError::Unavailable("session is closed"))
    } else {
        Ok(())
    }
}
fn stripe(uuid: Uuid) -> usize {
    let value = uuid.as_u128();
    ((value ^ (value >> 64)) as usize) % 64
}

fn packets(
    id: i32,
    inventory: &PlayerInventory,
    catalog: &ItemCatalog,
) -> Result<(PacketBatch, Bytes), InventoryServiceError> {
    let mut content = vec![18];
    content.extend(encode_content_body(
        0,
        inventory.state_id(),
        inventory.slots(),
        inventory.cursor(),
        catalog,
    )?);
    let mut held = vec![107];
    held.extend(encode_held_slot_body(inventory.selected())?);
    let equipment = equipment(id, inventory, catalog)?;
    if content.len() + held.len() + equipment.len() > MAX_INVENTORY_SYNC_BYTES {
        return Err(InventoryServiceError::Limit(
            "combined inventory snapshot exceeds 256 KiB",
        ));
    }
    Ok((
        vec![content.into(), held.into(), equipment.clone()].into(),
        equipment,
    ))
}

fn equipment(
    id: i32,
    inventory: &PlayerInventory,
    catalog: &ItemCatalog,
) -> Result<Bytes, InventoryServiceError> {
    let mut packet = vec![104];
    zerqon_protocol::codec::put_varint(id, &mut packet);
    packet.push(0x80);
    encode_server_stack(inventory.selected_stack(), catalog, &mut packet)?;
    packet.push(1);
    encode_server_stack(&inventory.slots()[45], catalog, &mut packet)?;
    Ok(packet.into())
}

fn grant(
    inventory: &mut PlayerInventory,
    catalog: &ItemCatalog,
    item: ItemId,
    count: u32,
) -> Result<(), InventoryServiceError> {
    let seed = ItemStack::new(item, 1, ValidatedPatch::default())?;
    catalog.validate_stack(&seed)?;
    let maximum = catalog.effective_max_stack(&seed)?;
    let mut remaining = count;
    let selected = 36 + usize::from(inventory.selected());
    let merge_order = std::iter::once(selected)
        .chain(std::iter::once(45))
        .chain((36..45).filter(|index| *index != selected))
        .chain(9..36);
    for index in merge_order {
        let Some(stack) = inventory.slots()[index].clone() else {
            continue;
        };
        if !catalog.same_kind(&stack, &seed)? {
            continue;
        }
        let amount = remaining.min(u32::from(maximum.saturating_sub(stack.count()))) as u8;
        if amount != 0 {
            inventory.creative_set(
                catalog,
                &CreativeSlotRequest {
                    slot: index as i16,
                    stack: Some(stack.with_count(stack.count() + amount)?),
                },
                InventoryPolicy {
                    creative: true,
                    can_interact: true,
                    allow_drops: false,
                },
            )?;
            remaining -= u32::from(amount);
        }
        if remaining == 0 {
            return Ok(());
        }
    }
    for index in (36..45).chain(9..36) {
        if inventory.slots()[index].is_none() {
            let amount = remaining.min(u32::from(maximum)) as u8;
            inventory.creative_set(
                catalog,
                &CreativeSlotRequest {
                    slot: index as i16,
                    stack: Some(seed.with_count(amount)?),
                },
                InventoryPolicy {
                    creative: true,
                    can_interact: true,
                    allow_drops: false,
                },
            )?;
            remaining -= u32::from(amount);
            if remaining == 0 {
                return Ok(());
            }
        }
    }
    Err(InventoryServiceError::Limit(
        "inventory is full; no items were given",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::players::{EntityIds, Identity};
    use std::collections::{BTreeMap, BTreeSet};
    use tokio::sync::{mpsc, watch};
    use zerqon_inventory::{
        ComponentDef, ComponentId, ComponentValue, CreativeSlotRequest, ItemDef, ItemId, ItemStack,
        PlayerInventory, ValidatedPatch, encode_creative_slot_body,
    };

    fn catalog() -> Arc<ItemCatalog> {
        Arc::new(
            ItemCatalog::from_definitions(
                vec![
                    ItemDef {
                        id: ItemId(0),
                        key: "minecraft:air".into(),
                        max_stack: 64,
                        max_damage: None,
                    },
                    ItemDef {
                        id: ItemId(1),
                        key: "minecraft:stone".into(),
                        max_stack: 64,
                        max_damage: None,
                    },
                ],
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
        )
    }
    fn reserve(
        players: &Arc<Players>,
        uuid: Uuid,
    ) -> (crate::players::PlayerHandle, mpsc::Receiver<PacketBatch>) {
        let (sender, receiver) = mpsc::channel(32);
        let (disconnect, _) = watch::channel(false);
        (
            players
                .reserve(
                    Identity {
                        uuid,
                        name: format!("Tester{}", uuid.as_u128()),
                        properties: Vec::new(),
                    },
                    sender,
                    disconnect,
                )
                .unwrap(),
            receiver,
        )
    }
    fn policy() -> InventoryPolicy {
        InventoryPolicy {
            creative: true,
            can_interact: true,
            allow_drops: false,
        }
    }

    #[tokio::test]
    async fn writes_are_persisted_by_uuid_before_success_and_reload_restores_selection() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog();
        let uuid = Uuid::from_u128(7);
        let players = Players::new(20, 64.0, 159, Arc::new(EntityIds::default()));
        let (player, _receiver) = reserve(&players, uuid);
        let service =
            InventoryService::new(directory.path().into(), Arc::clone(&catalog), players, 20)
                .unwrap();
        let handle = service.open(uuid, player.id()).await.unwrap();
        let bytes = encode_creative_slot_body(
            &CreativeSlotRequest {
                slot: 38,
                stack: Some(ItemStack::new(ItemId(1), 23, ValidatedPatch::default()).unwrap()),
            },
            &catalog,
        )
        .unwrap();
        let update = handle.apply_packet(57, &bytes, policy()).await.unwrap();
        assert!(update.iter().any(|packet| packet[0] == 18));
        assert!(update.iter().any(|packet| packet[0] == 104));
        handle.apply_packet(54, &[0, 2], policy()).await.unwrap();
        let store = PlayerStore::new(directory.path().into(), Arc::clone(&catalog)).unwrap();
        let saved = store.load(uuid).unwrap();
        assert_eq!(saved.selected(), 2);
        assert_eq!(saved.slots()[38].as_ref().unwrap().count(), 23);
        drop(handle);
        let reopened = service.open(uuid, player.id()).await.unwrap();
        let packets = reopened.full_sync().await.unwrap();
        assert!(packets.iter().any(|packet| packet.as_ref() == [107, 2]));
    }

    #[tokio::test]
    async fn aggregate_packet_budget_rejects_mutation_and_invalid_saved_reload() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog();
        let uuid = Uuid::from_u128(8);
        let players = Players::new(20, 64.0, 159, Arc::new(EntityIds::default()));
        let (player, _receiver) = reserve(&players, uuid);
        let service =
            InventoryService::new(directory.path().into(), Arc::clone(&catalog), players, 20)
                .unwrap();
        let handle = service.open(uuid, player.id()).await.unwrap();
        let text = "\0".repeat(4096);
        let patch = ValidatedPatch::new(
            BTreeMap::from([
                (ComponentId(6), ComponentValue::CustomName(text.clone())),
                (ComponentId(9), ComponentValue::ItemName(text)),
            ]),
            BTreeSet::new(),
            &catalog,
        )
        .unwrap();
        let item = Some(ItemStack::new(ItemId(1), 1, patch).unwrap());
        let store = PlayerStore::new(directory.path().into(), Arc::clone(&catalog)).unwrap();
        let mut rejected = false;
        for slot in 9..45 {
            let before = store.load(uuid).unwrap();
            let body = encode_creative_slot_body(
                &CreativeSlotRequest {
                    slot,
                    stack: item.clone(),
                },
                &catalog,
            )
            .unwrap();
            match handle.apply_packet(57, &body, policy()).await {
                Ok(_) => {}
                Err(InventoryServiceError::Limit(_)) => {
                    let after = store.load(uuid).unwrap();
                    assert_eq!(after.slots(), before.slots());
                    rejected = true;
                    break;
                }
                Err(error) => panic!("unexpected error: {error}"),
            }
        }
        assert!(
            rejected,
            "combined legal stacks must exceed snapshot budget"
        );
        drop(handle);
        let mut oversized = PlayerInventory::new();
        for slot in 1..=30 {
            oversized.set_slot(&catalog, slot, item.clone()).unwrap();
        }
        store.save(uuid, &oversized).unwrap();
        assert!(matches!(
            service.open(uuid, player.id()).await,
            Err(InventoryServiceError::Limit(_))
        ));
    }

    #[test]
    fn mutation_admission_is_bounded_and_recovers_in_the_next_window() {
        let now = Instant::now();
        let mut rate = MutationRate {
            started: now,
            count: 0,
        };
        for _ in 0..32 {
            rate.admit(now).unwrap();
        }
        assert!(matches!(
            rate.admit(now),
            Err(InventoryServiceError::RateLimited)
        ));
        rate.admit(now + Duration::from_secs(1)).unwrap();
    }

    #[tokio::test]
    async fn stale_queued_give_snapshots_are_suppressed_without_dropping_other_equipment() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog();
        let uuid = Uuid::from_u128(9);
        let other_uuid = Uuid::from_u128(10);
        let players = Players::new(20, 64.0, 159, Arc::new(EntityIds::default()));
        let (player, mut rx) = reserve(&players, uuid);
        let (other, mut other_rx) = reserve(&players, other_uuid);
        let service = InventoryService::new(
            directory.path().into(),
            Arc::clone(&catalog),
            Arc::clone(&players),
            20,
        )
        .unwrap();
        let handle = service.open(uuid, player.id()).await.unwrap();
        let _other_handle = service.open(other_uuid, other.id()).await.unwrap();
        player.activate().unwrap();
        other.activate().unwrap();
        while rx.try_recv().is_ok() {}
        while other_rx.try_recv().is_ok() {}
        service
            .give(uuid, player.id(), "minecraft:stone", 1)
            .await
            .unwrap();
        let old = rx.recv().await.unwrap();
        let body = encode_creative_slot_body(
            &CreativeSlotRequest {
                slot: 36,
                stack: Some(ItemStack::new(ItemId(1), 5, ValidatedPatch::default()).unwrap()),
            },
            &catalog,
        )
        .unwrap();
        let latest = handle.apply_packet(57, &body, policy()).await.unwrap();
        assert!(!handle.allow_outbound(&old));
        assert!(handle.allow_outbound(&latest));
        service
            .give(other_uuid, other.id(), "minecraft:stone", 2)
            .await
            .unwrap();
        let equipment = rx.recv().await.unwrap();
        assert_eq!(equipment[0][0], 104);
        assert!(handle.allow_outbound(&equipment));
        let selected = handle.apply_packet(54, &[0, 2], policy()).await.unwrap();
        assert!(!handle.allow_outbound(&latest));
        assert!(handle.allow_outbound(&selected));
    }

    #[tokio::test]
    async fn offhand_swap_uses_authoritative_selected_slot_and_noop_clicks_do_not_write() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog();
        let uuid = Uuid::from_u128(11);
        let players = Players::new(20, 64.0, 159, Arc::new(EntityIds::default()));
        let (player, _rx) = reserve(&players, uuid);
        let service =
            InventoryService::new(directory.path().into(), Arc::clone(&catalog), players, 20)
                .unwrap();
        let handle = service.open(uuid, player.id()).await.unwrap();
        handle.session.rate.lock().unwrap().count = 32;
        handle
            .apply_packet(18, &[0, 0, 0, 9, 0, 0, 0, 0], policy())
            .await
            .unwrap();
        assert!(!directory.path().join(format!("{uuid}.zinv")).exists());
        let body = encode_creative_slot_body(
            &CreativeSlotRequest {
                slot: 36,
                stack: Some(ItemStack::new(ItemId(1), 5, ValidatedPatch::default()).unwrap()),
            },
            &catalog,
        )
        .unwrap();
        assert!(matches!(
            handle.apply_packet(57, &body, policy()).await,
            Err(InventoryServiceError::RateLimited)
        ));
        handle.session.rate.lock().unwrap().count = 0;
        handle.apply_packet(57, &body, policy()).await.unwrap();
        handle.swap_offhand(policy()).await.unwrap();
        let saved = PlayerStore::new(directory.path().into(), catalog)
            .unwrap()
            .load(uuid)
            .unwrap();
        assert!(saved.slots()[36].is_none());
        assert_eq!(saved.slots()[45].as_ref().unwrap().count(), 5);
    }

    #[tokio::test]
    async fn closed_session_queued_write_cannot_overwrite_a_reopened_inventory() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog();
        let uuid = Uuid::from_u128(12);
        let players = Players::new(20, 64.0, 159, Arc::new(EntityIds::default()));
        let (player, _rx) = reserve(&players, uuid);
        let service =
            InventoryService::new(directory.path().into(), Arc::clone(&catalog), players, 20)
                .unwrap();
        let handle = service.open(uuid, player.id()).await.unwrap();
        service.give(uuid, player.id(), "stone", 1).await.unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let stripes = Arc::clone(&service.stripes);
        let blocker = std::thread::spawn(move || {
            let _guard = stripes[stripe(uuid)].lock().unwrap();
            ready_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let old_session = Arc::clone(&handle.session);
        let operation_service = Arc::clone(&service);
        let old = tokio::spawn(async move {
            operation_service
                .change(old_session, |inventory, catalog| {
                    grant(inventory, catalog, ItemId(1), 20)
                })
                .await
        });
        tokio::task::yield_now().await;
        drop(handle);
        let new_service = Arc::clone(&service);
        let id = player.id();
        let reopened = tokio::spawn(async move { new_service.open(uuid, id).await });
        release_tx.send(()).unwrap();
        blocker.join().unwrap();
        assert!(matches!(
            old.await.unwrap(),
            Err(InventoryServiceError::Unavailable(_))
        ));
        let _new_handle = reopened.await.unwrap().unwrap();
        let saved = PlayerStore::new(directory.path().into(), catalog)
            .unwrap()
            .load(uuid)
            .unwrap();
        assert_eq!(saved.slots()[36].as_ref().unwrap().count(), 1);
    }

    #[tokio::test]
    async fn overflowing_give_has_no_partial_effect() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog();
        let uuid = Uuid::from_u128(13);
        let players = Players::new(20, 64.0, 159, Arc::new(EntityIds::default()));
        let (player, _rx) = reserve(&players, uuid);
        let service =
            InventoryService::new(directory.path().into(), Arc::clone(&catalog), players, 20)
                .unwrap();
        let handle = service.open(uuid, player.id()).await.unwrap();
        let before = handle.full_sync().await.unwrap();
        assert!(matches!(
            service.give(uuid, player.id(), "stone", 4096).await,
            Err(InventoryServiceError::Limit(_))
        ));
        assert_eq!(handle.full_sync().await.unwrap(), before);
        assert!(!directory.path().join(format!("{uuid}.zinv")).exists());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn failed_atomic_write_keeps_memory_revision_and_original_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog();
        let uuid = Uuid::from_u128(14);
        let players = Players::new(20, 64.0, 159, Arc::new(EntityIds::default()));
        let (player, _rx) = reserve(&players, uuid);
        let service = InventoryService::new(directory.path().into(), catalog, players, 20).unwrap();
        let handle = service.open(uuid, player.id()).await.unwrap();
        service.give(uuid, player.id(), "stone", 1).await.unwrap();
        let before = handle.full_sync().await.unwrap();
        let path = directory.path().join(format!("{uuid}.zinv"));
        let bytes = std::fs::read(&path).unwrap();
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        assert!(matches!(
            service.give(uuid, player.id(), "stone", 1).await,
            Err(InventoryServiceError::Store(_))
        ));
        assert_eq!(handle.full_sync().await.unwrap(), before);
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}
