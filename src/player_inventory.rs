use crate::{
    crafting_data::CraftingData,
    game_mode::GameMode,
    lifecycle::{JobGuard, Lifecycle, RegistrationError},
    player_storage::{PersistedPlayer, PlayerStorage, PlayerStorageError},
    players::{InventoryRevision, PacketBatch, Players},
};
use bytes::Bytes;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Notify, OwnedRwLockReadGuard, OwnedSemaphorePermit, RwLock, Semaphore};
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
    #[error("{0}")]
    Store(#[from] PlayerStorageError),
    #[error(transparent)]
    Player(#[from] crate::players::PlayerError),
    #[error("inventory worker task failed: {0}")]
    Worker(#[from] tokio::task::JoinError),
    #[error("{0}")]
    Registration(#[from] RegistrationError),
    #[error("{0}")]
    Cleanup(#[from] crate::managed_tasks::CleanupError),
}

pub struct InventoryService {
    pub gameplay: Arc<RwLock<()>>,
    catalog: Arc<ItemCatalog>,
    store: Arc<PlayerStorage>,
    players: Arc<Players>,
    sessions: Mutex<HashMap<Uuid, Arc<Session>>>,
    stripes: Arc<[Mutex<()>; 64]>,
    inflight: Arc<Semaphore>,
    capacity: usize,
    lifecycle: Lifecycle,
    crafting: Arc<CraftingData>,
}
struct Session {
    uuid: Uuid,
    id: i32,
    inventory: Mutex<Option<SessionInventory>>,
    closed: AtomicBool,
    generation: AtomicU64,
    rate: Mutex<MutationRate>,
    activity: Arc<InventoryActivity>,
}

#[derive(Clone)]
struct SessionInventory(PersistedPlayer);

impl std::ops::Deref for SessionInventory {
    type Target = PlayerInventory;

    fn deref(&self) -> &Self::Target {
        &self.0.record.inventory
    }
}

impl std::ops::DerefMut for SessionInventory {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0.record.inventory
    }
}

fn storage_result<T>(
    store: &PlayerStorage,
    result: Result<T, PlayerStorageError>,
) -> Result<T, InventoryServiceError> {
    result.map_err(|error| {
        let terminal = matches!(
            &error,
            PlayerStorageError::CorruptRecord(_)
                | PlayerStorageError::Commit(
                    zerqon_world::CommitError::Conflict
                        | zerqon_world::CommitError::Indeterminate { .. }
                )
        );
        if terminal || store.check_available().is_err() {
            crate::managed_tasks::CleanupError(format!("durable player storage failed: {error}"))
                .into()
        } else {
            error.into()
        }
    })
}

#[derive(Default)]
struct InventoryAdmission {
    closed: bool,
    owner_dropped: bool,
    active: usize,
    failure: Option<String>,
    finalization: InventoryFinalization,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum InventoryFinalization {
    #[default]
    Pending,
    Running,
    Finished,
}

#[derive(Default)]
struct InventoryActivity {
    admission: Mutex<InventoryAdmission>,
    changed: Notify,
}

struct InventoryWork {
    service: Option<Arc<InventoryService>>,
    session: Option<Arc<Session>>,
    activity: Arc<InventoryActivity>,
    permit: Option<OwnedSemaphorePermit>,
    gameplay: Option<OwnedRwLockReadGuard<()>>,
    job: Option<JobGuard>,
    finalizing: bool,
}

impl InventoryWork {
    fn admit(
        service: &Arc<InventoryService>,
        session: &Arc<Session>,
        permit: OwnedSemaphorePermit,
        gameplay: Option<OwnedRwLockReadGuard<()>>,
        opening: bool,
    ) -> Result<Self, InventoryServiceError> {
        let mut admission = session
            .activity
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if admission.closed {
            return Err(InventoryServiceError::Unavailable(
                "inventory admission is closed",
            ));
        }
        // Existing sessions may finish accepted commands while shutdown drains.
        // Their private admission gate remains the authority for continuation.
        let job = if opening {
            service.lifecycle.register("inventory worker")?
        } else {
            service.lifecycle.register_cleanup("inventory worker")?
        };
        admission.active += 1;
        Ok(Self {
            service: Some(Arc::clone(service)),
            session: Some(Arc::clone(session)),
            activity: Arc::clone(&session.activity),
            permit: Some(permit),
            gameplay,
            job: Some(job),
            finalizing: false,
        })
    }

    fn admit_finalizer(
        service: &Arc<InventoryService>,
        session: &Arc<Session>,
        permit: OwnedSemaphorePermit,
        gameplay: OwnedRwLockReadGuard<()>,
    ) -> Option<Self> {
        let mut admission = session
            .activity
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !admission.closed
            || admission.active != 0
            || admission.failure.is_some()
            || admission.finalization != InventoryFinalization::Pending
        {
            return None;
        }
        let job = match service.lifecycle.register_cleanup("inventory finalizer") {
            Ok(job) => job,
            Err(error) => {
                admission.failure =
                    Some(format!("inventory finalizer registration failed: {error}"));
                admission.finalization = InventoryFinalization::Finished;
                drop(gameplay);
                drop(permit);
                drop(admission);
                session.activity.changed.notify_waiters();
                return None;
            }
        };
        admission.active = 1;
        admission.finalization = InventoryFinalization::Running;
        Some(Self {
            service: Some(Arc::clone(service)),
            session: Some(Arc::clone(session)),
            activity: Arc::clone(&session.activity),
            permit: Some(permit),
            gameplay: Some(gameplay),
            job: Some(job),
            finalizing: true,
        })
    }

    fn finish(mut self, cleanup: Result<(), String>) {
        self.settle(cleanup);
    }

    fn settle(&mut self, cleanup: Result<(), String>) {
        drop(self.gameplay.take());
        drop(self.permit.take());
        let mut admission = self
            .activity
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Err(error) = &cleanup {
            admission.failure.get_or_insert_with(|| error.clone());
            admission.closed = true;
        }
        if self.finalizing {
            admission.finalization = InventoryFinalization::Finished;
        }
        admission.active -= 1;
        let service = self.service.take();
        let session = self.session.take();
        if admission.owner_dropped
            && admission.active == 0
            && let (Some(service), Some(session)) = (&service, &session)
        {
            retire_session(service, session);
        }
        drop(session);
        drop(service);
        // A drain already being polled must not pass the gate before the
        // lifecycle acknowledgement, even without a notification wake.
        if let Some(job) = self.job.take() {
            job.finish(cleanup);
        }
        drop(admission);
        self.activity.changed.notify_waiters();
    }
}

impl Drop for InventoryWork {
    fn drop(&mut self) {
        if self.job.is_some() {
            self.settle(Err("inventory worker ended before settling".into()));
        }
    }
}

struct OwnedInventoryWork<F> {
    operation: Option<F>,
    completion: Option<InventoryWork>,
}

impl<F> OwnedInventoryWork<F> {
    fn run<T>(mut self) -> Result<T, InventoryServiceError>
    where
        F: FnOnce() -> Result<T, InventoryServiceError>,
    {
        let finalizing = self
            .completion
            .as_ref()
            .is_some_and(|completion| completion.finalizing);
        let operation = self
            .operation
            .take()
            .expect("inventory worker owns its operation");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation));
        let (result, cleanup) = match result {
            Ok(Err(error)) if finalizing || crate::managed_tasks::is_cleanup_failure(&error) => {
                let cleanup = Err(error.to_string());
                (Err(error), cleanup)
            }
            Ok(result) => (result, Ok(())),
            Err(payload) => {
                let cause = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("non-string panic");
                let message = format!("inventory worker panicked: {cause}");
                (
                    Err(crate::managed_tasks::CleanupError(message.clone()).into()),
                    Err(message),
                )
            }
        };
        if let Some(completion) = self.completion.take() {
            completion.finish(cleanup);
        }
        result
    }
}

impl<F> Drop for OwnedInventoryWork<F> {
    fn drop(&mut self) {
        // The operation owns its original packet. Free it before acknowledging
        // even a queued worker abandoned by the executor.
        drop(self.operation.take());
        drop(self.completion.take());
    }
}

async fn inventory_worker<T, F>(
    completion: InventoryWork,
    operation: F,
) -> Result<T, InventoryServiceError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, InventoryServiceError> + Send + 'static,
{
    let work = OwnedInventoryWork {
        operation: Some(operation),
        completion: Some(completion),
    };
    tokio::task::spawn_blocking(move || work.run()).await?
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
        store: Arc<PlayerStorage>,
        players: Arc<Players>,
        capacity: usize,
        gameplay: Arc<RwLock<()>>,
        lifecycle: Lifecycle,
        crafting: Arc<CraftingData>,
    ) -> Result<Arc<Self>, InventoryServiceError> {
        if !(1..=65_536).contains(&capacity) {
            return Err(InventoryServiceError::Limit("player capacity"));
        }
        Ok(Arc::new(Self {
            gameplay,
            catalog: Arc::clone(store.catalog()),
            store,
            players,
            sessions: Mutex::new(HashMap::new()),
            stripes: Arc::new(std::array::from_fn(|_| Mutex::new(()))),
            inflight: Arc::new(Semaphore::new(capacity.min(64))),
            capacity,
            lifecycle,
            crafting,
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
            activity: Arc::new(InventoryActivity::default()),
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
        let crafting = Arc::clone(&self.crafting);
        let completion = InventoryWork::admit(self, &session, permit, None, true)?;
        inventory_worker(completion, move || {
            let _stripe = stripes[stripe(uuid)]
                .lock()
                .map_err(|_| InventoryServiceError::Unavailable("inventory I/O lock poisoned"))?;
            ensure_open(&session)?;
            let mut inventory = SessionInventory(storage_result(&store, store.load(uuid))?);
            if inventory.slots()[1..5].iter().any(Option::is_some) {
                inventory.refresh_crafting(&catalog, &crafting.recipes)?;
            }
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
        .await?;
        Ok(handle)
    }
    pub async fn give(
        self: &Arc<Self>,
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
        self: &Arc<Self>,
        session: Arc<Session>,
        operation: F,
    ) -> Result<PacketBatch, InventoryServiceError>
    where
        F: FnOnce(&mut SessionInventory, &ItemCatalog) -> Result<(), InventoryServiceError>
            + Send
            + 'static,
    {
        let gameplay = Arc::clone(&self.gameplay).read_owned().await;
        let permit = Arc::clone(&self.inflight)
            .acquire_owned()
            .await
            .map_err(|_| InventoryServiceError::Unavailable("I/O workers stopped"))?;
        let store = Arc::clone(&self.store);
        let catalog = Arc::clone(&self.catalog);
        let stripes = Arc::clone(&self.stripes);
        let players = Arc::clone(&self.players);
        let completion = InventoryWork::admit(self, &session, permit, Some(gameplay), false)?;
        inventory_worker(completion, move||{
            let _stripe=stripes[stripe(session.uuid)].lock().map_err(|_|InventoryServiceError::Unavailable("inventory I/O lock poisoned"))?;
            ensure_open(&session)?;
            let mut current=session.inventory.lock().map_err(|_|InventoryServiceError::Unavailable("inventory lock poisoned"))?;
            let mut candidate=current.as_ref().ok_or(InventoryServiceError::Unavailable("inventory is loading"))?.clone();
            operation(&mut candidate,&catalog)?;
            let (packets,equipment)=packets(session.id,&candidate,&catalog)?;
            let generation=session.generation.load(Ordering::Relaxed).checked_add(1).ok_or(InventoryServiceError::Limit("session generation exhausted"))?;
            let previous=current.as_ref().ok_or(InventoryServiceError::Unavailable("inventory is loading"))?;
            if previous.slots()!=candidate.slots()||previous.cursor()!=candidate.cursor()||previous.selected()!=candidate.selected()
                || previous.0.record.game_mode != candidate.0.record.game_mode || previous.0.record.previous_game_mode != candidate.0.record.previous_game_mode {
                session.rate.lock().map_err(|_|InventoryServiceError::Unavailable("rate limiter poisoned"))?.admit(Instant::now())?;
                candidate.0.revision = storage_result(&store, store.save(session.uuid,previous.0.revision,&candidate.0.record))?;
            }
            *current=Some(candidate);
            session.generation.store(generation,Ordering::Release);
            drop(current);
            if let Err(error)=players.set_equipment(session.uuid,session.id,equipment) { tracing::warn!(%error,uuid=%session.uuid,"Inventory committed but equipment delivery failed"); }
            Ok::<_,InventoryServiceError>(packets.with_inventory_revision(InventoryRevision{uuid:session.uuid,entity_id:session.id,generation}))
        }).await
    }
}
impl InventoryHandle {
    #[cfg(test)]
    pub(crate) fn test_admission_closed(&self) -> bool {
        self.session.activity.admission.lock().unwrap().closed
    }

    #[cfg(test)]
    pub(crate) async fn test_blocked_change(
        &self,
        started: tokio::sync::oneshot::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> Result<PacketBatch, InventoryServiceError> {
        self.service
            .change(Arc::clone(&self.session), move |inventory, _| {
                let _ = started.send(());
                release
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|_| InventoryServiceError::Unavailable("test barrier timed out"))?;
                inventory.select_hotbar(3)?;
                Ok(())
            })
            .await
    }

    pub(crate) async fn close_and_drain(&self) -> Result<(), InventoryServiceError> {
        self.session
            .activity
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .closed = true;
        loop {
            let changed = self.session.activity.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let finalize = {
                let admission = self
                    .session
                    .activity
                    .admission
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if admission.active == 0 {
                    if let Some(error) = &admission.failure {
                        return Err(crate::managed_tasks::CleanupError(error.clone()).into());
                    }
                    if admission.finalization == InventoryFinalization::Finished {
                        return Ok(());
                    }
                    admission.finalization == InventoryFinalization::Pending
                } else {
                    false
                }
            };
            if finalize {
                let gameplay = Arc::clone(&self.service.gameplay).read_owned().await;
                let permit = match Arc::clone(&self.service.inflight).acquire_owned().await {
                    Ok(permit) => permit,
                    Err(error) => {
                        let mut admission = self
                            .session
                            .activity
                            .admission
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        if admission.finalization == InventoryFinalization::Pending {
                            admission.failure = Some(format!(
                                "inventory finalizer could not acquire I/O quota: {error}"
                            ));
                            admission.finalization = InventoryFinalization::Finished;
                        }
                        drop(gameplay);
                        drop(admission);
                        self.session.activity.changed.notify_waiters();
                        continue;
                    }
                };
                let Some(completion) =
                    InventoryWork::admit_finalizer(&self.service, &self.session, permit, gameplay)
                else {
                    continue;
                };
                let session = Arc::clone(&self.session);
                let store = Arc::clone(&self.service.store);
                let catalog = Arc::clone(&self.service.catalog);
                let stripes = Arc::clone(&self.service.stripes);
                let players = Arc::clone(&self.service.players);
                // The completion owns the terminal outcome. Reading it through the
                // gate gives every closer the same result, including after abort.
                let _ = inventory_worker(completion, move || {
                    let _stripe = stripes[stripe(session.uuid)].lock().map_err(|_| {
                        InventoryServiceError::Unavailable("inventory I/O lock poisoned")
                    })?;
                    ensure_open(&session)?;
                    let mut current = session.inventory.lock().map_err(|_| {
                        InventoryServiceError::Unavailable("inventory lock poisoned")
                    })?;
                    let mut candidate = current
                        .as_ref()
                        .ok_or(InventoryServiceError::Unavailable("inventory is loading"))?
                        .clone();
                    let creative = candidate.0.record.game_mode == GameMode::Creative;
                    match candidate.close_crafting(
                        &catalog,
                        InventoryPolicy {
                            creative,
                            can_interact: true,
                            allow_drops: false,
                        },
                    ) {
                        Ok(_) | Err(zerqon_inventory::InventoryError::DroppingUnavailable) => {}
                        Err(error) => return Err(error.into()),
                    }
                    let equipment = equipment(session.id, &candidate, &catalog)?;
                    let generation = session
                        .generation
                        .load(Ordering::Relaxed)
                        .checked_add(1)
                        .ok_or(InventoryServiceError::Limit("session generation exhausted"))?;
                    // Close is durable cleanup, independent of the mutation rate.
                    // DroppingUnavailable leaves the original grid/cursor intact.
                    candidate.0.revision = storage_result(
                        &store,
                        store.save(session.uuid, candidate.0.revision, &candidate.0.record),
                    )?;
                    *current = Some(candidate);
                    session.generation.store(generation, Ordering::Release);
                    drop(current);
                    players.set_equipment(session.uuid, session.id, equipment)?;
                    Ok::<_, InventoryServiceError>(())
                })
                .await;
                continue;
            }
            changed.await;
        }
    }

    pub fn modes(&self) -> Result<(GameMode, Option<GameMode>), InventoryServiceError> {
        ensure_open(&self.session)?;
        let current = self
            .session
            .inventory
            .lock()
            .map_err(|_| InventoryServiceError::Unavailable("inventory lock poisoned"))?;
        let current = current
            .as_ref()
            .ok_or(InventoryServiceError::Unavailable("inventory is loading"))?;
        Ok((
            current.0.record.game_mode,
            current.0.record.previous_game_mode,
        ))
    }

    pub fn held_stack(
        &self,
        hand: zerqon_inventory::Hand,
    ) -> Result<zerqon_inventory::Slot, InventoryServiceError> {
        ensure_open(&self.session)?;
        let current = self
            .session
            .inventory
            .lock()
            .map_err(|_| InventoryServiceError::Unavailable("inventory lock poisoned"))?;
        let current = current
            .as_ref()
            .ok_or(InventoryServiceError::Unavailable("inventory is loading"))?;
        Ok(match hand {
            zerqon_inventory::Hand::Main => current.selected_stack().clone(),
            zerqon_inventory::Hand::Off => current.slots()[45].clone(),
        })
    }
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
                let policy = InventoryPolicy {
                    creative: inventory.0.record.game_mode == GameMode::Creative,
                    ..policy
                };
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
        let completion = InventoryWork::admit(&self.service, &session, permit, None, false)?;
        inventory_worker(completion, move || {
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
        .await
    }
    pub async fn apply_inbound_packet(
        &self,
        id: i32,
        packet: zerqon_protocol::inbound::InboundPacket,
        body_offset: usize,
        policy: InventoryPolicy,
    ) -> Result<PacketBatch, InventoryServiceError> {
        self.apply_owned_packet(id, packet, body_offset, policy)
            .await
    }

    async fn apply_owned_packet<B: std::ops::Deref<Target = [u8]> + Send + 'static>(
        &self,
        id: i32,
        packet: B,
        body_offset: usize,
        policy: InventoryPolicy,
    ) -> Result<PacketBatch, InventoryServiceError> {
        let body = packet
            .get(body_offset..)
            .ok_or(InventoryServiceError::Limit("inventory packet offset"))?;
        if body.len() > 65_536 {
            return Err(InventoryServiceError::Limit("inventory packet bytes"));
        }
        if !matches!(id, 18 | 19 | 54 | 57) {
            return Err(InventoryServiceError::Unavailable(
                "unsupported inventory packet",
            ));
        }
        let crafting = Arc::clone(&self.service.crafting);
        self.service
            .change(Arc::clone(&self.session), move |inventory, catalog| {
                let policy = InventoryPolicy {
                    creative: inventory.0.record.game_mode == GameMode::Creative,
                    ..policy
                };
                let body = packet
                    .get(body_offset..)
                    .ok_or(InventoryServiceError::Limit("inventory packet offset"))?;
                if id != 19 && !policy.can_interact {
                    return Err(zerqon_inventory::InventoryError::InteractionDenied.into());
                }
                match id {
                    19 => {
                        let mut remaining = body;
                        let container = zerqon_protocol::codec::get_varint(&mut remaining)
                            .map_err(zerqon_inventory::ItemError::from)?;
                        if !remaining.is_empty() {
                            return Err(zerqon_inventory::ItemError::Invalid(
                                "trailing container close bytes",
                            )
                            .into());
                        }
                        if container != 0 {
                            return Err(zerqon_inventory::InventoryError::WrongContainer.into());
                        }
                        inventory.close_crafting(
                            catalog,
                            InventoryPolicy {
                                allow_drops: false,
                                ..policy
                            },
                        )?;
                    }
                    57 => {
                        let request = decode_creative_slot(body, catalog, DecodeLimits::default())?;
                        inventory.creative_set_with_crafting(
                            catalog,
                            &request,
                            InventoryPolicy {
                                allow_drops: false,
                                ..policy
                            },
                            &crafting.recipes,
                        )?;
                    }
                    54 => {
                        inventory.select_hotbar(i16::from(decode_carried_slot(body)?))?;
                    }
                    18 => {
                        let request = decode_click(body, catalog, DecodeLimits::default())?;
                        inventory.click_with_crafting(
                            catalog,
                            &request,
                            InventoryPolicy {
                                allow_drops: false,
                                ..policy
                            },
                            &crafting.recipes,
                            &crafting.remainders,
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
        let mut admission = self
            .session
            .activity
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        admission.closed = true;
        admission.owner_dropped = true;
        if admission.active == 0 {
            retire_session(&self.service, &self.session);
        }
    }
}

fn retire_session(service: &InventoryService, session: &Arc<Session>) {
    session.closed.store(true, Ordering::Release);
    let mut sessions = service
        .sessions
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if sessions
        .get(&session.uuid)
        .is_some_and(|current| Arc::ptr_eq(current, session))
    {
        sessions.remove(&session.uuid);
    }
}

fn ensure_open(session: &Session) -> Result<(), InventoryServiceError> {
    if session.closed.load(Ordering::Acquire) {
        Err(InventoryServiceError::Unavailable("session is closed"))
    } else if let Some(error) = session
        .activity
        .admission
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .failure
        .clone()
    {
        Err(crate::managed_tasks::CleanupError(error).into())
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

    fn test_service(
        directory: std::path::PathBuf,
        catalog: Arc<ItemCatalog>,
        players: Arc<Players>,
        capacity: usize,
        gameplay: Arc<RwLock<()>>,
        lifecycle: Lifecycle,
        crafting: Arc<CraftingData>,
    ) -> Result<Arc<InventoryService>, InventoryServiceError> {
        let world = Arc::new(
            zerqon_world::WorldStore::open(directory.join("world"))
                .map_err(PlayerStorageError::from)?,
        );
        let store = Arc::new(PlayerStorage::new(world, catalog, GameMode::Creative)?);
        InventoryService::new(store, players, capacity, gameplay, lifecycle, crafting)
    }

    fn durable_inventory(service: &InventoryService, uuid: Uuid) -> PlayerInventory {
        service.store.load(uuid).unwrap().record.inventory
    }

    fn save_fixture(service: &InventoryService, session: &Session, state: &mut SessionInventory) {
        state.0.revision = service
            .store
            .save(session.uuid, state.0.revision, &state.0.record)
            .unwrap();
    }

    impl InventoryHandle {
        async fn apply_packet(
            &self,
            id: i32,
            body: &[u8],
            policy: InventoryPolicy,
        ) -> Result<PacketBatch, InventoryServiceError> {
            self.apply_owned_packet(id, body.to_vec(), 0, policy).await
        }
    }

    fn test_crafting_data() -> Arc<CraftingData> {
        Arc::new(CraftingData {
            recipes: zerqon_inventory::CraftingRecipes::default(),
            remainders: zerqon_inventory::CraftingRemainders::default(),
        })
    }

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

    struct WorkFixture {
        world: Arc<zerqon_world::WorldStore>,
        service: Arc<InventoryService>,
        handle: Arc<InventoryHandle>,
        _player: crate::players::PlayerHandle,
        _outbound: mpsc::Receiver<PacketBatch>,
        _directory: tempfile::TempDir,
    }

    impl WorkFixture {
        async fn new() -> Self {
            Self::with_mode(GameMode::Creative).await
        }

        async fn with_mode(mode: GameMode) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let players = Players::new(2, 64.0, 159, Arc::new(EntityIds::default()));
            let (player, outbound) = reserve(&players, Uuid::from_u128(31));
            let world =
                Arc::new(zerqon_world::WorldStore::open(directory.path().join("world")).unwrap());
            let store = Arc::new(PlayerStorage::new(Arc::clone(&world), catalog(), mode).unwrap());
            let service = InventoryService::new(
                store,
                players,
                2,
                Arc::new(RwLock::new(())),
                Lifecycle::new(),
                test_crafting_data(),
            )
            .unwrap();
            let handle = Arc::new(
                service
                    .open(Uuid::from_u128(31), player.id())
                    .await
                    .unwrap(),
            );
            Self {
                _directory: directory,
                world,
                service,
                handle,
                _player: player,
                _outbound: outbound,
            }
        }
    }

    #[tokio::test]
    async fn stored_survival_mode_cannot_be_overridden_by_caller_creative_policy() {
        let fixture = WorkFixture::with_mode(GameMode::Survival).await;
        let before = fixture
            .service
            .store
            .load(fixture.handle.session.uuid)
            .unwrap()
            .revision;
        let body = encode_creative_slot_body(
            &CreativeSlotRequest {
                slot: 36,
                stack: Some(ItemStack::new(ItemId(1), 4, ValidatedPatch::default()).unwrap()),
            },
            &fixture.service.catalog,
        )
        .unwrap();
        let result = fixture.handle.apply_packet(57, &body, policy()).await;
        assert!(
            matches!(
                result,
                Err(InventoryServiceError::Inventory(
                    zerqon_inventory::InventoryError::CreativeRequired
                ))
            ),
            "caller-supplied creative flag changed stored survival authority"
        );
        assert_eq!(
            fixture
                .service
                .store
                .load(fixture.handle.session.uuid)
                .unwrap()
                .revision,
            before
        );
        assert_eq!(fixture.handle.modes().unwrap(), (GameMode::Survival, None));
        fixture
            .service
            .give(
                fixture.handle.session.uuid,
                fixture.handle.session.id,
                "stone",
                2,
            )
            .await
            .unwrap();
        fixture
            .handle
            .apply_packet(54, &[0, 2], policy())
            .await
            .unwrap();
        assert_eq!(fixture.handle.modes().unwrap(), (GameMode::Survival, None));
        assert!(fixture.service.lifecycle.pending().is_empty());
    }

    #[tokio::test]
    async fn stored_creative_mode_and_previous_mode_survive_caller_policy_and_receipt_publication()
    {
        let fixture = WorkFixture::new().await;
        {
            let mut current = fixture.handle.session.inventory.lock().unwrap();
            let state = current.as_mut().unwrap();
            state.0.record.previous_game_mode = Some(GameMode::Survival);
            save_fixture(&fixture.service, &fixture.handle.session, state);
        }
        let before = fixture
            .service
            .store
            .load(fixture.handle.session.uuid)
            .unwrap()
            .revision;
        let body = encode_creative_slot_body(
            &CreativeSlotRequest {
                slot: 36,
                stack: Some(ItemStack::new(ItemId(1), 4, ValidatedPatch::default()).unwrap()),
            },
            &fixture.service.catalog,
        )
        .unwrap();
        fixture
            .handle
            .apply_packet(
                57,
                &body,
                InventoryPolicy {
                    creative: false,
                    ..policy()
                },
            )
            .await
            .unwrap();
        let durable = fixture
            .service
            .store
            .load(fixture.handle.session.uuid)
            .unwrap();
        assert!(durable.revision > before);
        assert_eq!(
            fixture
                .handle
                .session
                .inventory
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .0
                .revision,
            durable.revision
        );
        assert_eq!(
            fixture.handle.modes().unwrap(),
            (GameMode::Creative, Some(GameMode::Survival))
        );
        assert_eq!(durable.record.game_mode, GameMode::Creative);
        assert_eq!(durable.record.previous_game_mode, Some(GameMode::Survival));
        assert_eq!(
            durable.record.inventory.slots()[36]
                .as_ref()
                .unwrap()
                .count(),
            4
        );
    }

    #[tokio::test]
    async fn mode_only_candidates_preserve_record_revision_coherence() {
        let fixture = WorkFixture::new().await;
        let uuid = fixture.handle.session.uuid;
        let initial = fixture.service.store.load(uuid).unwrap().revision;
        fixture
            .service
            .change(Arc::clone(&fixture.handle.session), |state, _| {
                state.0.record.game_mode = GameMode::Survival;
                state.0.record.previous_game_mode = Some(GameMode::Creative);
                Ok(())
            })
            .await
            .unwrap();
        let first = fixture.service.store.load(uuid).unwrap();
        assert!(first.revision > initial);
        assert_eq!(
            fixture.handle.modes().unwrap(),
            (GameMode::Survival, Some(GameMode::Creative))
        );
        assert_eq!(
            fixture
                .handle
                .session
                .inventory
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .0
                .revision,
            first.revision
        );
        fixture
            .service
            .change(Arc::clone(&fixture.handle.session), |state, _| {
                state.0.record.previous_game_mode = None;
                Ok(())
            })
            .await
            .unwrap();
        let second = fixture.service.store.load(uuid).unwrap();
        assert!(second.revision > first.revision);
        assert_eq!(second.record.game_mode, GameMode::Survival);
        assert_eq!(second.record.previous_game_mode, None);
        assert_eq!(fixture.handle.modes().unwrap(), (GameMode::Survival, None));
        assert_eq!(
            fixture
                .handle
                .session
                .inventory
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .0
                .revision,
            second.revision
        );
    }

    #[tokio::test]
    async fn unknown_commit_error_preserves_applied_state_and_never_autosaves_stale_ram() {
        let fixture = WorkFixture::new().await;
        let uuid = fixture.handle.session.uuid;
        let store = Arc::clone(&fixture.service.store);
        let result = fixture
            .service
            .change(
                Arc::clone(&fixture.handle.session),
                move |state, catalog| {
                    let mut applied = state.0.record.clone();
                    applied.inventory.set_slot(
                        catalog,
                        36,
                        Some(ItemStack::new(ItemId(1), 40, ValidatedPatch::default()).unwrap()),
                    )?;
                    store.save(uuid, state.0.revision, &applied).unwrap();
                    // Model loss of the acknowledgement after an actual domain write.
                    // Physical redb sync-failure recovery is tested in zerqon-world.
                    storage_result(
                        &store,
                        Err(PlayerStorageError::Commit(
                            zerqon_world::CommitError::Indeterminate {
                                operation: Uuid::new_v4(),
                                cause: "controlled lost acknowledgement".into(),
                            },
                        )),
                    )
                },
            )
            .await;
        assert!(matches!(result, Err(error) if crate::managed_tasks::is_cleanup_failure(&error)));
        assert!(
            fixture
                .handle
                .session
                .inventory
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .slots()[36]
                .is_none()
        );
        let committed = fixture.service.store.load(uuid).unwrap();
        assert_eq!(
            committed.record.inventory.slots()[36]
                .as_ref()
                .unwrap()
                .count(),
            40
        );
        assert!(fixture.handle.close_and_drain().await.is_err());
        assert!(fixture.handle.full_sync().await.is_err());
        assert_eq!(
            fixture.service.store.load(uuid).unwrap().revision,
            committed.revision
        );
        assert_eq!(
            durable_inventory(&fixture.service, uuid).slots()[36]
                .as_ref()
                .unwrap()
                .count(),
            40
        );
    }

    #[tokio::test]
    async fn corrupt_canonical_player_record_is_terminal_without_overwriting_it() {
        let fixture = WorkFixture::new().await;
        let uuid = fixture.handle.session.uuid;
        let key = zerqon_world::PlayerKey::new(uuid);
        let before = fixture
            .handle
            .session
            .inventory
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .0
            .revision;
        let reads = fixture.world.snapshot_domain(&[], &[key], &[]).unwrap();
        fixture
            .world
            .commit_domain(
                Uuid::new_v4(),
                &reads,
                &[],
                &[zerqon_world::PlayerWrite {
                    key,
                    replacement: Arc::from(b"invalid player record".as_slice()),
                }],
                &[],
            )
            .unwrap();
        let result = fixture
            .service
            .give(uuid, fixture.handle.session.id, "stone", 1)
            .await;
        assert!(matches!(result, Err(error) if crate::managed_tasks::is_cleanup_failure(&error)));
        assert!(
            fixture.world.check_available().is_ok(),
            "semantic record error is distinct from a redb I/O latch"
        );
        assert!(fixture.handle.full_sync().await.is_err());
        assert!(fixture.handle.modes().is_err());
        assert!(fixture.handle.close_and_drain().await.is_err());
        assert_eq!(
            fixture
                .handle
                .session
                .inventory
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .0
                .revision,
            before
        );
        assert_eq!(
            fixture
                .world
                .snapshot_domain(&[], &[key], &[])
                .unwrap()
                .players[0]
                .data()
                .unwrap()
                .as_ref(),
            b"invalid player record"
        );
    }

    #[tokio::test]
    async fn all_inventory_writes_use_domain_and_leave_imported_legacy_bytes_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog();
        let uuid = Uuid::from_u128(123);
        let world =
            Arc::new(zerqon_world::WorldStore::open(directory.path().join("world")).unwrap());
        let legacy = crate::player_store::PlayerStore::new(
            world.root().join("playerdata"),
            Arc::clone(&catalog),
        )
        .unwrap();
        let mut old = PlayerInventory::new();
        old.set_slot(
            &catalog,
            1,
            Some(ItemStack::new(ItemId(1), 2, ValidatedPatch::default()).unwrap()),
        )
        .unwrap();
        old.set_slot(
            &catalog,
            37,
            Some(ItemStack::new(ItemId(1), 4, ValidatedPatch::default()).unwrap()),
        )
        .unwrap();
        old.set_cursor(
            &catalog,
            Some(ItemStack::new(ItemId(1), 3, ValidatedPatch::default()).unwrap()),
        )
        .unwrap();
        old.select_hotbar(1).unwrap();
        legacy.save(uuid, &old).unwrap();
        let path = world.root().join("playerdata").join(format!("{uuid}.zinv"));
        let original = std::fs::read(&path).unwrap();
        let players = Players::new(2, 64.0, 159, Arc::new(EntityIds::default()));
        let (player, _outbound) = reserve(&players, uuid);
        let store = Arc::new(PlayerStorage::new(world, catalog, GameMode::Survival).unwrap());
        let service = InventoryService::new(
            store,
            players,
            2,
            Arc::new(RwLock::new(())),
            Lifecycle::new(),
            test_crafting_data(),
        )
        .unwrap();
        let handle = service.open(uuid, player.id()).await.unwrap();
        assert_eq!(
            handle.modes().unwrap(),
            (GameMode::Creative, None),
            "legacy mode was replaced by the fresh-player default"
        );
        service.give(uuid, player.id(), "stone", 6).await.unwrap();
        handle.apply_packet(54, &[0, 2], policy()).await.unwrap();
        handle.swap_offhand(policy()).await.unwrap();
        handle.close_and_drain().await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let saved = service.store.load(uuid).unwrap();
        assert_eq!(saved.record.game_mode, GameMode::Creative);
        assert!(
            saved.record.inventory.slots()[..5]
                .iter()
                .all(Option::is_none)
        );
        assert!(saved.record.inventory.cursor().is_none());
        assert_eq!(
            saved
                .record
                .inventory
                .slots()
                .iter()
                .flatten()
                .map(|stack| usize::from(stack.count()))
                .sum::<usize>(),
            15
        );
        drop(handle);
        let reopened = service.open(uuid, player.id()).await.unwrap();
        assert_eq!(reopened.modes().unwrap(), (GameMode::Creative, None));
        assert_eq!(durable_inventory(&service, uuid).selected(), 2);
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[tokio::test]
    async fn inventory_close_returns_grid_and_cursor_before_persisting_even_when_rate_limited() {
        let fixture = WorkFixture::new().await;
        let catalog = &fixture.service.catalog;
        {
            let mut current = fixture.handle.session.inventory.lock().unwrap();
            let inventory = current.as_mut().unwrap();
            inventory
                .set_slot(
                    catalog,
                    0,
                    Some(ItemStack::new(ItemId(1), 64, ValidatedPatch::default()).unwrap()),
                )
                .unwrap();
            inventory
                .set_slot(
                    catalog,
                    1,
                    Some(ItemStack::new(ItemId(1), 2, ValidatedPatch::default()).unwrap()),
                )
                .unwrap();
            inventory
                .set_cursor(
                    catalog,
                    Some(ItemStack::new(ItemId(1), 3, ValidatedPatch::default()).unwrap()),
                )
                .unwrap();
            save_fixture(&fixture.service, &fixture.handle.session, inventory);
        }
        {
            let mut rate = fixture.handle.session.rate.lock().unwrap();
            rate.started = Instant::now();
            rate.count = 32;
        }
        fixture.handle.close_and_drain().await.unwrap();
        let saved = fixture
            .service
            .store
            .load(fixture.handle.session.uuid)
            .unwrap()
            .record
            .inventory;
        assert!(
            saved.slots()[..5].iter().all(Option::is_none),
            "close left crafting inputs in persistent grid"
        );
        assert!(saved.cursor().is_none(), "close left persistent cursor");
        assert_eq!(
            saved.slots()[36].as_ref().unwrap().count(),
            5,
            "derived result became a real returned item"
        );
        assert_eq!(fixture.handle.session.rate.lock().unwrap().count, 32);
        let before = fixture
            .service
            .store
            .load(fixture.handle.session.uuid)
            .unwrap()
            .revision;
        fixture.service.lifecycle.stop();
        fixture
            .service
            .lifecycle
            .drain(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
            .unwrap();
        let (first, second) = tokio::join!(
            fixture.handle.close_and_drain(),
            fixture.handle.close_and_drain()
        );
        first.unwrap();
        second.unwrap();
        assert_eq!(
            fixture
                .service
                .store
                .load(fixture.handle.session.uuid)
                .unwrap()
                .revision,
            before
        );
    }

    #[tokio::test]
    async fn inventory_close_preserves_overflowing_grid_and_cursor_without_inventing_drops() {
        let fixture = WorkFixture::new().await;
        let catalog = &fixture.service.catalog;
        let before = {
            let mut current = fixture.handle.session.inventory.lock().unwrap();
            let inventory = current.as_mut().unwrap();
            for slot in 9..=45 {
                inventory
                    .set_slot(
                        catalog,
                        slot,
                        Some(ItemStack::new(ItemId(1), 64, ValidatedPatch::default()).unwrap()),
                    )
                    .unwrap();
            }
            inventory
                .set_slot(
                    catalog,
                    0,
                    Some(ItemStack::new(ItemId(1), 64, ValidatedPatch::default()).unwrap()),
                )
                .unwrap();
            inventory
                .set_slot(
                    catalog,
                    1,
                    Some(ItemStack::new(ItemId(1), 2, ValidatedPatch::default()).unwrap()),
                )
                .unwrap();
            inventory
                .set_cursor(
                    catalog,
                    Some(ItemStack::new(ItemId(1), 3, ValidatedPatch::default()).unwrap()),
                )
                .unwrap();
            save_fixture(&fixture.service, &fixture.handle.session, inventory);
            inventory.clone()
        };
        fixture.handle.close_and_drain().await.unwrap();
        let saved = fixture
            .service
            .store
            .load(fixture.handle.session.uuid)
            .unwrap()
            .record
            .inventory;
        assert_eq!(&saved.slots()[1..], &before.slots()[1..]);
        assert_eq!(saved.cursor(), before.cursor());
        assert!(saved.slots()[0].is_none());
        let current = fixture.handle.session.inventory.lock().unwrap();
        let current = current.as_ref().unwrap();
        assert_eq!(current.slots(), before.slots());
        assert_eq!(current.cursor(), before.cursor());
    }

    #[tokio::test]
    async fn inventory_close_is_owned_across_caller_abort_and_blocks_reopen_until_saved() {
        let WorkFixture {
            _directory,
            world: _world,
            service,
            handle,
            _player,
            _outbound,
        } = WorkFixture::new().await;
        let session = Arc::clone(&handle.session);
        let uuid = session.uuid;
        let id = session.id;
        {
            let mut current = session.inventory.lock().unwrap();
            let inventory = current.as_mut().unwrap();
            inventory
                .set_slot(
                    &service.catalog,
                    1,
                    Some(ItemStack::new(ItemId(1), 7, ValidatedPatch::default()).unwrap()),
                )
                .unwrap();
            inventory
                .set_cursor(
                    &service.catalog,
                    Some(ItemStack::new(ItemId(1), 2, ValidatedPatch::default()).unwrap()),
                )
                .unwrap();
            save_fixture(&service, &session, inventory);
        }
        let stripes = Arc::clone(&service.stripes);
        let (started, held) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _stripe = stripes[stripe(uuid)].lock().unwrap();
            let _ = started.send(());
            let _ = released.recv_timeout(Duration::from_secs(5));
        });
        held.await.unwrap();
        let first_handle = Arc::clone(&handle);
        let first = tokio::spawn(async move { first_handle.close_and_drain().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while session.activity.admission.lock().unwrap().finalization
                != InventoryFinalization::Running
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let second_handle = Arc::clone(&handle);
        let (started, running) = tokio::sync::oneshot::channel();
        let second = tokio::spawn(async move {
            let _ = started.send(());
            second_handle.close_and_drain().await
        });
        running.await.unwrap();
        first.abort();
        second.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert!(second.await.unwrap_err().is_cancelled());
        drop(handle);
        assert_eq!(session.activity.admission.lock().unwrap().active, 1);
        assert_eq!(service.lifecycle.pending(), ["inventory finalizer"]);
        assert!(matches!(
            service.open(uuid, id).await,
            Err(InventoryServiceError::Unavailable("duplicate session"))
        ));
        release.send(()).unwrap();
        holder.join().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let changed = session.activity.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if session.activity.admission.lock().unwrap().active == 0 {
                    break;
                }
                changed.await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            session.generation.load(Ordering::Acquire),
            1,
            "concurrent close ran finalization twice"
        );
        assert!(service.lifecycle.pending().is_empty());
        let reopened = service.open(uuid, id).await.unwrap();
        let saved = service.store.load(uuid).unwrap().record.inventory;
        assert!(saved.slots()[..5].iter().all(Option::is_none));
        assert!(saved.cursor().is_none());
        assert_eq!(saved.slots()[36].as_ref().unwrap().count(), 9);
        drop(reopened);
    }

    #[tokio::test]
    async fn inventory_close_persistence_failure_is_terminal_and_replayed() {
        let fixture = WorkFixture::new().await;
        {
            let mut current = fixture.handle.session.inventory.lock().unwrap();
            current
                .as_mut()
                .unwrap()
                .set_slot(
                    &fixture.service.catalog,
                    1,
                    Some(ItemStack::new(ItemId(1), 4, ValidatedPatch::default()).unwrap()),
                )
                .unwrap();
            save_fixture(
                &fixture.service,
                &fixture.handle.session,
                current.as_mut().unwrap(),
            );
        }
        let mut winner = fixture
            .service
            .store
            .load(fixture.handle.session.uuid)
            .unwrap();
        winner.record.inventory.select_hotbar(2).unwrap();
        winner.revision = fixture
            .service
            .store
            .save(fixture.handle.session.uuid, winner.revision, &winner.record)
            .unwrap();
        let first = fixture.handle.close_and_drain().await.unwrap_err();
        assert!(crate::managed_tasks::is_cleanup_failure(&first));
        {
            let current = fixture.handle.session.inventory.lock().unwrap();
            assert_eq!(
                current.as_ref().unwrap().slots()[1]
                    .as_ref()
                    .unwrap()
                    .count(),
                4
            );
            assert!(current.as_ref().unwrap().slots()[36].is_none());
        }
        // Even repairing the stale RAM view cannot reopen an already settled close.
        *fixture.handle.session.inventory.lock().unwrap() = Some(SessionInventory(winner.clone()));
        let second = fixture.handle.close_and_drain().await.unwrap_err();
        assert_eq!(first.to_string(), second.to_string());
        assert_eq!(
            fixture
                .service
                .store
                .load(fixture.handle.session.uuid)
                .unwrap()
                .revision,
            winner.revision,
            "failed finalization was silently retried"
        );
        fixture.service.lifecycle.stop();
        assert!(matches!(
            fixture
                .service
                .lifecycle
                .drain(tokio::time::Instant::now() + Duration::from_secs(2))
                .await,
            Err(crate::lifecycle::DrainError::Cleanup { .. })
        ));
    }

    #[tokio::test]
    async fn inventory_close_waiters_recheck_finalization_after_acquiring_quota() {
        let fixture = WorkFixture::new().await;
        let quota = Arc::clone(&fixture.service.inflight)
            .acquire_many_owned(2)
            .await
            .unwrap();
        let mut first = Box::pin(fixture.handle.close_and_drain());
        let mut second = Box::pin(fixture.handle.close_and_drain());
        std::future::poll_fn(|context| {
            assert!(std::future::Future::poll(first.as_mut(), context).is_pending());
            assert!(std::future::Future::poll(second.as_mut(), context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(quota);
        let (first, second) = tokio::join!(first, second);
        first.unwrap();
        second.unwrap();
        assert_eq!(fixture.handle.session.generation.load(Ordering::Acquire), 1);
        assert!(
            fixture
                .handle
                .session
                .activity
                .admission
                .lock()
                .unwrap()
                .finalization
                == InventoryFinalization::Finished
        );
        assert_eq!(fixture.service.inflight.available_permits(), 2);
        assert!(fixture.service.lifecycle.pending().is_empty());
    }

    #[tokio::test]
    async fn inventory_close_closed_quota_records_terminal_error_and_releases_gameplay() {
        let fixture = WorkFixture::new().await;
        fixture.service.inflight.close();
        let first = fixture.handle.close_and_drain().await.unwrap_err();
        let second = fixture.handle.close_and_drain().await.unwrap_err();
        assert_eq!(first.to_string(), second.to_string());
        assert!(crate::managed_tasks::is_cleanup_failure(&first));
        assert!(
            fixture
                .handle
                .session
                .activity
                .admission
                .lock()
                .unwrap()
                .finalization
                == InventoryFinalization::Finished
        );
        assert!(fixture.service.gameplay.try_write().is_ok());
        assert!(fixture.service.lifecycle.pending().is_empty());
    }

    #[tokio::test]
    async fn inventory_close_after_lifecycle_seal_records_one_terminal_failure() {
        let fixture = WorkFixture::new().await;
        fixture
            .service
            .lifecycle
            .drain(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
            .unwrap();
        let first = fixture.handle.close_and_drain().await.unwrap_err();
        let second = fixture.handle.close_and_drain().await.unwrap_err();
        assert!(crate::managed_tasks::is_cleanup_failure(&first));
        assert_eq!(first.to_string(), second.to_string());
        assert!(
            fixture
                .handle
                .session
                .activity
                .admission
                .lock()
                .unwrap()
                .finalization
                == InventoryFinalization::Finished
        );
        assert_eq!(fixture.handle.session.generation.load(Ordering::Acquire), 0);
        assert_eq!(fixture.service.inflight.available_permits(), 2);
        assert!(fixture.service.gameplay.try_write().is_ok());
    }

    #[tokio::test]
    async fn cancelled_inventory_open_holds_session_reservation_until_worker_finishes() {
        let fixture = WorkFixture::new().await;
        let uuid = Uuid::from_u128(32);
        let (player, _outbound) = reserve(&fixture.service.players, uuid);
        let id = player.id();
        let stripes = Arc::clone(&fixture.service.stripes);
        let (started, held) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _stripe = stripes[stripe(uuid)].lock().unwrap();
            let _ = started.send(());
            let _ = released.recv_timeout(Duration::from_secs(5));
        });
        held.await.unwrap();
        let service = Arc::clone(&fixture.service);
        let caller = tokio::spawn(async move { service.open(uuid, id).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let active = fixture
                    .service
                    .sessions
                    .lock()
                    .unwrap()
                    .get(&uuid)
                    .map(|session| session.activity.admission.lock().unwrap().active)
                    .unwrap_or(0);
                if active == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        caller.abort();
        assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
        assert!(matches!(
            fixture.service.open(uuid, id).await,
            Err(InventoryServiceError::Unavailable("duplicate session"))
        ));
        fixture.service.lifecycle.stop();
        let mut draining = Box::pin(
            fixture
                .service
                .lifecycle
                .drain(tokio::time::Instant::now() + Duration::from_secs(5)),
        );
        std::future::poll_fn(|context| {
            assert!(std::future::Future::poll(draining.as_mut(), context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        release.send(()).unwrap();
        holder.join().unwrap();
        draining.await.unwrap();
        assert!(!fixture.service.sessions.lock().unwrap().contains_key(&uuid));
        assert_eq!(fixture.service.inflight.available_permits(), 2);
    }

    #[tokio::test]
    async fn cancelled_inventory_sync_stays_tracked_until_snapshot_lock_is_released() {
        let fixture = WorkFixture::new().await;
        let session = Arc::clone(&fixture.handle.session);
        let (started, held) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _inventory = session.inventory.lock().unwrap();
            let _ = started.send(());
            let _ = released.recv_timeout(Duration::from_secs(5));
        });
        held.await.unwrap();
        let handle = Arc::clone(&fixture.handle);
        let caller = tokio::spawn(async move { handle.full_sync().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture
                .handle
                .session
                .activity
                .admission
                .lock()
                .unwrap()
                .active
                == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        fixture.service.lifecycle.stop();
        let mut draining = Box::pin(fixture.handle.close_and_drain());
        std::future::poll_fn(|context| {
            assert!(std::future::Future::poll(draining.as_mut(), context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        release.send(()).unwrap();
        holder.join().unwrap();
        draining.await.unwrap();
        fixture
            .service
            .lifecycle
            .drain(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(fixture.service.inflight.available_permits(), 2);
    }

    #[tokio::test]
    async fn closed_inventory_admission_rejects_mutation_and_sync_without_new_workers() {
        let fixture = WorkFixture::new().await;
        let lifecycle = &fixture.service.lifecycle;
        let owner = lifecycle.register("test admitted session").unwrap();
        lifecycle.stop();
        fixture
            .service
            .give(
                fixture.handle.session.uuid,
                fixture.handle.session.id,
                "stone",
                1,
            )
            .await
            .unwrap();
        assert!(
            fixture
                .service
                .open(Uuid::from_u128(32), fixture.handle.session.id + 1)
                .await
                .is_err()
        );
        fixture.handle.close_and_drain().await.unwrap();
        assert!(
            fixture
                .service
                .give(
                    fixture.handle.session.uuid,
                    fixture.handle.session.id,
                    "stone",
                    1
                )
                .await
                .is_err()
        );
        assert!(fixture.handle.full_sync().await.is_err());
        assert!(
            fixture
                .handle
                .apply_packet(54, &[0, 2], policy())
                .await
                .is_err()
        );
        assert_eq!(fixture.service.inflight.available_permits(), 2);
        assert_eq!(
            fixture
                .handle
                .session
                .activity
                .admission
                .lock()
                .unwrap()
                .active,
            0
        );
        assert_eq!(lifecycle.pending(), ["test admitted session"]);
        owner.finish(Ok(()));
        lifecycle
            .drain(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn unobserved_domain_error_does_not_fail_inventory_cleanup() {
        let fixture = WorkFixture::new().await;
        let service = Arc::clone(&fixture.service);
        let session = Arc::clone(&fixture.handle.session);
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let caller = tokio::spawn(async move {
            service
                .change(session, move |_, _| {
                    let _ = started.send(());
                    released.recv_timeout(Duration::from_secs(5)).unwrap();
                    Err(zerqon_inventory::InventoryError::InteractionDenied.into())
                })
                .await
        });
        running.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        fixture.handle.close_and_drain().await.unwrap();
        assert!(
            fixture
                .service
                .store
                .load(fixture.handle.session.uuid)
                .unwrap()
                .record
                .inventory
                .slots()
                .iter()
                .all(Option::is_none)
        );
        fixture.service.lifecycle.stop();
        fixture
            .service
            .lifecycle
            .drain(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn panicking_inventory_worker_reports_cleanup_failure_and_closes_admission() {
        let fixture = WorkFixture::new().await;
        let result = fixture
            .service
            .change(Arc::clone(&fixture.handle.session), |_, _| {
                panic!("controlled inventory panic")
            })
            .await;
        assert!(matches!(result, Err(error) if crate::managed_tasks::is_cleanup_failure(&error)));
        assert!(fixture.handle.close_and_drain().await.is_err());
        assert!(fixture.handle.full_sync().await.is_err());
        assert_eq!(fixture.service.inflight.available_permits(), 2);
        assert_eq!(
            fixture
                .handle
                .session
                .activity
                .admission
                .lock()
                .unwrap()
                .active,
            0
        );
        fixture.service.lifecycle.stop();
        assert!(matches!(
            fixture
                .service
                .lifecycle
                .drain(tokio::time::Instant::now() + Duration::from_secs(2))
                .await,
            Err(crate::lifecycle::DrainError::Cleanup { .. })
        ));
    }

    #[derive(Debug, PartialEq)]
    struct WorkObservation {
        active: usize,
        job_pending: bool,
        permits: usize,
        gameplay_free: bool,
        input_bytes: usize,
    }

    struct WorkWakeProbe {
        service: Arc<InventoryService>,
        session: Arc<Session>,
        input: zerqon_protocol::inbound::ConnectionBudget,
        observed: Mutex<Option<WorkObservation>>,
        changed: Notify,
    }

    impl WorkWakeProbe {
        fn observe(&self) {
            let observation = WorkObservation {
                active: self
                    .session
                    .activity
                    .admission
                    .try_lock()
                    .map_or(usize::MAX, |admission| admission.active),
                job_pending: self
                    .service
                    .lifecycle
                    .pending()
                    .contains(&"inventory worker"),
                permits: self.service.inflight.available_permits(),
                gameplay_free: self.service.gameplay.try_write().is_ok(),
                input_bytes: self.input.snapshot().used_bytes,
            };
            self.observed.lock().unwrap().get_or_insert(observation);
            self.changed.notify_one();
        }
    }

    impl std::task::Wake for WorkWakeProbe {
        fn wake(self: Arc<Self>) {
            self.observe();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.observe();
        }
    }

    #[tokio::test]
    async fn abandoned_inventory_owner_releases_packet_and_resources_before_drain_wakes() {
        use zerqon_protocol::{
            codec::write_frame,
            inbound::{PacketBudget, read_frame_budgeted},
        };
        let fixture = WorkFixture::new().await;
        let global = PacketBudget::new(64).unwrap();
        let account = global.connection(64).unwrap();
        let mut wire = Vec::new();
        write_frame(&mut wire, &[54, 0, 3], None).await.unwrap();
        let packet = read_frame_budgeted(&mut wire.as_slice(), 64, None, &account)
            .await
            .unwrap();
        let completion = InventoryWork::admit(
            &fixture.service,
            &fixture.handle.session,
            Arc::clone(&fixture.service.inflight)
                .acquire_owned()
                .await
                .unwrap(),
            Some(Arc::clone(&fixture.service.gameplay).read_owned().await),
            false,
        )
        .unwrap();
        let work = OwnedInventoryWork {
            operation: Some(move || {
                drop(packet);
                Ok::<_, InventoryServiceError>(())
            }),
            completion: Some(completion),
        };
        let probe = Arc::new(WorkWakeProbe {
            service: Arc::clone(&fixture.service),
            session: Arc::clone(&fixture.handle.session),
            input: account,
            observed: Mutex::new(None),
            changed: Notify::new(),
        });
        let waker = std::task::Waker::from(Arc::clone(&probe));
        let mut context = std::task::Context::from_waker(&waker);
        let mut draining = Box::pin(fixture.handle.close_and_drain());
        assert!(std::future::Future::poll(draining.as_mut(), &mut context).is_pending());
        drop(work);
        assert!(draining.await.is_err());
        assert_eq!(
            *probe.observed.lock().unwrap(),
            Some(WorkObservation {
                active: 0,
                job_pending: false,
                permits: 2,
                gameplay_free: true,
                input_bytes: 0,
            })
        );
        fixture.service.lifecycle.stop();
        assert!(matches!(
            fixture
                .service
                .lifecycle
                .drain(tokio::time::Instant::now() + Duration::from_secs(2))
                .await,
            Err(crate::lifecycle::DrainError::Cleanup { .. })
        ));
    }

    struct PacketDropBarrier {
        packet: Option<zerqon_protocol::inbound::InboundPacket>,
        freed: Option<tokio::sync::oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl std::ops::Deref for PacketDropBarrier {
        type Target = [u8];
        fn deref(&self) -> &[u8] {
            self.packet.as_ref().unwrap().as_slice()
        }
    }

    impl Drop for PacketDropBarrier {
        fn drop(&mut self) {
            drop(self.packet.take());
            if let Some(freed) = self.freed.take() {
                let _ = freed.send(());
            }
            let _ = self.release.recv_timeout(Duration::from_secs(5));
        }
    }

    #[tokio::test]
    async fn packet_quota_ends_when_buffer_is_freed_and_completion_wakes_after_worker_settles() {
        use zerqon_protocol::{
            codec::write_frame,
            inbound::{PacketBudget, read_frame_budgeted},
        };
        let fixture = WorkFixture::new().await;
        let global = PacketBudget::new(64).unwrap();
        let account = global.connection(64).unwrap();
        let mut wire = Vec::new();
        write_frame(&mut wire, &[54, 0, 3], None).await.unwrap();
        let packet = read_frame_budgeted(&mut wire.as_slice(), 64, None, &account)
            .await
            .unwrap();
        let (freed, buffer_freed) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let packet = PacketDropBarrier {
            packet: Some(packet),
            freed: Some(freed),
            release: released,
        };
        let probe = Arc::new(WorkWakeProbe {
            service: Arc::clone(&fixture.service),
            session: Arc::clone(&fixture.handle.session),
            input: account.clone(),
            observed: Mutex::new(None),
            changed: Notify::new(),
        });
        let waker = std::task::Waker::from(Arc::clone(&probe));
        let mut context = std::task::Context::from_waker(&waker);
        let mut work = Box::pin(fixture.handle.apply_owned_packet(54, packet, 1, policy()));
        assert!(std::future::Future::poll(work.as_mut(), &mut context).is_pending());
        buffer_freed.await.unwrap();
        assert_eq!(account.snapshot().used_bytes, 0);
        assert_eq!(
            fixture
                .handle
                .session
                .activity
                .admission
                .lock()
                .unwrap()
                .active,
            1
        );
        assert!(
            fixture
                .service
                .lifecycle
                .pending()
                .contains(&"inventory worker")
        );
        assert_eq!(fixture.service.inflight.available_permits(), 1);
        assert!(fixture.service.gameplay.try_write().is_err());
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), probe.changed.notified())
            .await
            .unwrap();
        work.await.unwrap();
        assert_eq!(
            *probe.observed.lock().unwrap(),
            Some(WorkObservation {
                active: 0,
                job_pending: false,
                permits: 2,
                gameplay_free: true,
                input_bytes: 0,
            })
        );
        assert_eq!(
            fixture
                .service
                .store
                .load(fixture.handle.session.uuid)
                .unwrap()
                .record
                .inventory
                .selected(),
            3
        );
    }

    #[tokio::test]
    async fn rejected_inventory_frames_release_memory_without_changing_saved_data() {
        use zerqon_protocol::{
            codec::write_frame,
            inbound::{PacketBudget, read_frame_budgeted},
        };
        let directory = tempfile::tempdir().unwrap();
        let uuid = Uuid::from_u128(29);
        let players = Players::new(2, 64.0, 159, Arc::new(EntityIds::default()));
        let (player, _receiver) = reserve(&players, uuid);
        let service = test_service(
            directory.path().into(),
            catalog(),
            players,
            2,
            Arc::new(RwLock::new(())),
            Lifecycle::new(),
            test_crafting_data(),
        )
        .unwrap();
        let handle = service.open(uuid, player.id()).await.unwrap();
        let global = PacketBudget::new(64).unwrap();
        let initial_revision = service.store.load(uuid).unwrap().revision;
        let account = global.connection(64).unwrap();
        for (id, offset, selected, allowed) in [
            (255, 1, 3, true),
            (54, 4, 3, true),
            (54, 1, 9, true),
            (54, 1, 3, false),
        ] {
            let mut wire = Vec::new();
            write_frame(&mut wire, &[54, 0, selected], None)
                .await
                .unwrap();
            let packet = read_frame_budgeted(&mut wire.as_slice(), 64, None, &account)
                .await
                .unwrap();
            assert_eq!(account.snapshot().used_bytes, 3);
            let policy = InventoryPolicy {
                can_interact: allowed,
                ..policy()
            };
            assert!(
                handle
                    .apply_inbound_packet(id, packet, offset, policy)
                    .await
                    .is_err()
            );
            assert_eq!(account.snapshot().used_bytes, 0);
            assert_eq!(global.snapshot().used_bytes, 0);
            assert_eq!(service.store.load(uuid).unwrap().revision, initial_revision);
            assert_eq!(
                handle
                    .session
                    .inventory
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .selected(),
                0
            );
        }
    }

    #[tokio::test]
    async fn accepted_inventory_work_keeps_inbound_memory_after_its_caller_is_cancelled() {
        use zerqon_protocol::{
            codec::write_frame,
            inbound::{PacketBudget, read_frame_budgeted},
        };
        let directory = tempfile::tempdir().unwrap();
        let uuid = Uuid::from_u128(27);
        let players = Players::new(2, 64.0, 159, Arc::new(EntityIds::default()));
        let (player, _receiver) = reserve(&players, uuid);
        let service = test_service(
            directory.path().into(),
            catalog(),
            players,
            2,
            Arc::new(RwLock::new(())),
            Lifecycle::new(),
            test_crafting_data(),
        )
        .unwrap();
        let handle = Arc::new(service.open(uuid, player.id()).await.unwrap());
        let global = PacketBudget::new(64).unwrap();
        let account = global.connection(64).unwrap();
        let mut wire = Vec::new();
        write_frame(&mut wire, &[54, 0, 3], None).await.unwrap();
        let packet = read_frame_budgeted(&mut wire.as_slice(), 64, None, &account)
            .await
            .unwrap();
        let stripes = Arc::clone(&service.stripes);
        let (held_tx, held_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = stripes[stripe(uuid)].lock().unwrap();
            held_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        });
        held_rx.await.unwrap();
        let pending_handle = Arc::clone(&handle);
        let pending = tokio::spawn(async move {
            pending_handle
                .apply_inbound_packet(54, packet, 1, policy())
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while service.inflight.available_permits() == 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        let retained_bytes = account.snapshot().used_bytes;
        service.lifecycle.stop();
        let mut draining = Box::pin(
            service
                .lifecycle
                .drain(tokio::time::Instant::now() + Duration::from_secs(5)),
        );
        std::future::poll_fn(|context| {
            assert!(
                std::future::Future::poll(draining.as_mut(), context).is_pending(),
                "shutdown passed a blocked physical inventory worker"
            );
            std::task::Poll::Ready(())
        })
        .await;
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while service.inflight.available_permits() != 2 || account.snapshot().used_bytes != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        draining.await.unwrap();
        assert_eq!(
            service
                .store
                .load(uuid)
                .unwrap()
                .record
                .inventory
                .selected(),
            3
        );
        assert_eq!(
            retained_bytes, 3,
            "accepted worker lost the inbound packet reservation"
        );
        assert_eq!(global.snapshot().used_bytes, 0);
    }

    #[tokio::test]
    async fn inventory_changes_wait_for_an_exclusive_world_edit_without_blocking_the_executor() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog();
        let uuid = Uuid::from_u128(76);
        let players = Players::new(20, 64.0, 159, Arc::new(EntityIds::default()));
        let (player, _receiver) = reserve(&players, uuid);
        let service = test_service(
            directory.path().into(),
            catalog,
            players,
            20,
            Arc::new(RwLock::new(())),
            Lifecycle::new(),
            test_crafting_data(),
        )
        .unwrap();
        let _handle = service.open(uuid, player.id()).await.unwrap();
        let initial_revision = service.store.load(uuid).unwrap().revision;
        let lease = Arc::clone(&service.gameplay).write_owned().await;
        let pending_service = Arc::clone(&service);
        let id = player.id();
        let mut pending =
            tokio::spawn(async move { pending_service.give(uuid, id, "stone", 1).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut pending)
                .await
                .is_err()
        );
        assert_eq!(service.store.load(uuid).unwrap().revision, initial_revision);
        drop(lease);
        tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let loaded = service.store.load(uuid).unwrap().record.inventory;
        assert_eq!(loaded.slots()[36].as_ref().unwrap().count(), 1);
    }

    #[tokio::test]
    async fn writes_are_persisted_by_uuid_before_success_and_reload_restores_selection() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog();
        let uuid = Uuid::from_u128(7);
        let players = Players::new(20, 64.0, 159, Arc::new(EntityIds::default()));
        let (player, _receiver) = reserve(&players, uuid);
        let service = test_service(
            directory.path().into(),
            Arc::clone(&catalog),
            players,
            20,
            Arc::new(RwLock::new(())),
            Lifecycle::new(),
            test_crafting_data(),
        )
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
        let store = &service.store;
        let saved = store.load(uuid).unwrap().record.inventory;
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
        let service = test_service(
            directory.path().into(),
            Arc::clone(&catalog),
            players,
            20,
            Arc::new(RwLock::new(())),
            Lifecycle::new(),
            test_crafting_data(),
        )
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
        let store = &service.store;
        let mut rejected = false;
        for slot in 9..45 {
            let before = store.load(uuid).unwrap().record.inventory;
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
                    let after = store.load(uuid).unwrap().record.inventory;
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
        let mut record = store.load(uuid).unwrap();
        record.record.inventory = oversized;
        store.save(uuid, record.revision, &record.record).unwrap();
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
        let service = test_service(
            directory.path().into(),
            Arc::clone(&catalog),
            Arc::clone(&players),
            20,
            Arc::new(RwLock::new(())),
            Lifecycle::new(),
            test_crafting_data(),
        )
        .unwrap();
        let handle = service.open(uuid, player.id()).await.unwrap();
        let _other_handle = service.open(other_uuid, other.id()).await.unwrap();
        player.activate().unwrap();
        other.activate().unwrap();
        players.tracking_tick().unwrap();
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
        players.tracking_tick().unwrap();
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
        let service = test_service(
            directory.path().into(),
            Arc::clone(&catalog),
            players,
            20,
            Arc::new(RwLock::new(())),
            Lifecycle::new(),
            test_crafting_data(),
        )
        .unwrap();
        let handle = service.open(uuid, player.id()).await.unwrap();
        let initial_revision = service.store.load(uuid).unwrap().revision;
        handle.session.rate.lock().unwrap().count = 32;
        handle
            .apply_packet(18, &[0, 0, 0, 9, 0, 0, 0, 0], policy())
            .await
            .unwrap();
        assert_eq!(service.store.load(uuid).unwrap().revision, initial_revision);
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
        let saved = durable_inventory(&service, uuid);
        assert!(saved.slots()[36].is_none());
        assert_eq!(saved.slots()[45].as_ref().unwrap().count(), 5);
    }

    #[tokio::test]
    async fn retiring_session_keeps_admitted_write_and_blocks_reopen_until_completion() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog();
        let uuid = Uuid::from_u128(12);
        let players = Players::new(20, 64.0, 159, Arc::new(EntityIds::default()));
        let (player, _rx) = reserve(&players, uuid);
        let service = test_service(
            directory.path().into(),
            Arc::clone(&catalog),
            players,
            20,
            Arc::new(RwLock::new(())),
            Lifecycle::new(),
            test_crafting_data(),
        )
        .unwrap();
        let handle = service.open(uuid, player.id()).await.unwrap();
        service.give(uuid, player.id(), "stone", 1).await.unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let stripes = Arc::clone(&service.stripes);
        let blocker = std::thread::spawn(move || {
            let _guard = stripes[stripe(uuid)].lock().unwrap();
            ready_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
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
        tokio::time::timeout(Duration::from_secs(2), async {
            while handle.session.activity.admission.lock().unwrap().active == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(handle);
        let id = player.id();
        let premature = tokio::time::timeout(Duration::from_secs(2), service.open(uuid, id)).await;
        release_tx.send(()).unwrap();
        blocker.join().unwrap();
        old.await.unwrap().unwrap();
        assert!(matches!(
            premature,
            Ok(Err(InventoryServiceError::Unavailable("duplicate session")))
        ));
        let _new_handle = service.open(uuid, id).await.unwrap();
        let saved = durable_inventory(&service, uuid);
        assert_eq!(saved.slots()[36].as_ref().unwrap().count(), 21);
    }

    #[tokio::test]
    async fn overflowing_give_has_no_partial_effect() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog();
        let uuid = Uuid::from_u128(13);
        let players = Players::new(20, 64.0, 159, Arc::new(EntityIds::default()));
        let (player, _rx) = reserve(&players, uuid);
        let service = test_service(
            directory.path().into(),
            Arc::clone(&catalog),
            players,
            20,
            Arc::new(RwLock::new(())),
            Lifecycle::new(),
            test_crafting_data(),
        )
        .unwrap();
        let handle = service.open(uuid, player.id()).await.unwrap();
        let before = handle.full_sync().await.unwrap();
        let initial_revision = service.store.load(uuid).unwrap().revision;
        assert!(matches!(
            service.give(uuid, player.id(), "stone", 4096).await,
            Err(InventoryServiceError::Limit(_))
        ));
        assert_eq!(handle.full_sync().await.unwrap(), before);
        assert_eq!(service.store.load(uuid).unwrap().revision, initial_revision);
    }

    #[tokio::test]
    async fn durable_conflict_keeps_the_winner_and_closes_stale_session_admission() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog();
        let uuid = Uuid::from_u128(14);
        let players = Players::new(20, 64.0, 159, Arc::new(EntityIds::default()));
        let (player, _rx) = reserve(&players, uuid);
        let service = test_service(
            directory.path().into(),
            catalog,
            players,
            20,
            Arc::new(RwLock::new(())),
            Lifecycle::new(),
            test_crafting_data(),
        )
        .unwrap();
        let handle = service.open(uuid, player.id()).await.unwrap();
        service.give(uuid, player.id(), "stone", 1).await.unwrap();
        let old_revision = handle
            .session
            .inventory
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .0
            .revision;
        let mut winner = service.store.load(uuid).unwrap();
        winner
            .record
            .inventory
            .set_slot(
                &service.catalog,
                36,
                Some(ItemStack::new(ItemId(1), 40, ValidatedPatch::default()).unwrap()),
            )
            .unwrap();
        winner.revision = service
            .store
            .save(uuid, winner.revision, &winner.record)
            .unwrap();
        let result = service.give(uuid, player.id(), "stone", 1).await;
        assert!(matches!(result, Err(error) if crate::managed_tasks::is_cleanup_failure(&error)));
        {
            let current = handle.session.inventory.lock().unwrap();
            let current = current.as_ref().unwrap();
            assert_eq!(current.0.revision, old_revision);
            assert_eq!(current.slots()[36].as_ref().unwrap().count(), 1);
        }
        assert!(handle.full_sync().await.is_err());
        assert!(handle.modes().is_err());
        assert!(handle.held_stack(zerqon_inventory::Hand::Main).is_err());
        assert!(handle.close_and_drain().await.is_err());
        assert_eq!(service.store.load(uuid).unwrap().revision, winner.revision);
        assert_eq!(
            durable_inventory(&service, uuid).slots()[36]
                .as_ref()
                .unwrap()
                .count(),
            40
        );
        drop(handle);
        let reopened = service.open(uuid, player.id()).await.unwrap();
        assert_eq!(
            reopened
                .held_stack(zerqon_inventory::Hand::Main)
                .unwrap()
                .unwrap()
                .count(),
            40
        );
    }
}
