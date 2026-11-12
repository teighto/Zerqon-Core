use crate::{
    Result,
    drop_record::DropRecord,
    entity_packets,
    game_data::GameData,
    item_physics::Terrain,
    players::{EntityIds, Players},
    runtime::World,
};
use bytes::Bytes;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::{Mutex, OwnedMutexGuard};
use uuid::Uuid;
use zerqon_inventory::ItemCatalog;
use zerqon_world::{DropKey, DropWrite};

struct Item {
    id: i32,
    record: DropRecord,
    revision: u64,
    dirty: bool,
    cost: usize,
    collector: usize,
}
#[derive(Default)]
struct Live {
    items: BTreeMap<DropKey, Item>,
    visible: HashMap<(Uuid, i32), BTreeMap<DropKey, i32>>,
    bytes: usize,
    pickup_cursor: Option<DropKey>,
}
pub struct ItemDrops {
    world: Arc<World>,
    players: Arc<Players>,
    catalog: Arc<ItemCatalog>,
    blocks: Arc<zerqon_blocks::Catalog>,
    ids: Arc<EntityIds>,
    world_id: Uuid,
    item_type: i32,
    maximum: usize,
    memory: usize,
    live: Arc<Mutex<Live>>,
    count: AtomicUsize,
}
pub struct ItemConfig {
    pub maximum: usize,
    pub memory: usize,
    pub stop: crate::lifecycle::StopToken,
}
pub(crate) struct DropLease {
    service: Arc<ItemDrops>,
    live: OwnedMutexGuard<Live>,
    staged: Vec<(DropKey, Item)>,
}

impl ItemDrops {
    pub async fn open(
        world: Arc<World>,
        players: Arc<Players>,
        data: Arc<GameData>,
        catalog: Arc<ItemCatalog>,
        blocks: Arc<zerqon_blocks::Catalog>,
        ids: Arc<EntityIds>,
        config: ItemConfig,
    ) -> Result<Arc<Self>> {
        let ItemConfig {
            maximum,
            memory,
            stop,
        } = config;
        let world_id = world.storage().uuid()?;
        let item_type = data
            .entity_type("minecraft:item")
            .ok_or("Missing item entity registry")?;
        let storage = world.storage();
        let item_ids = Arc::clone(&ids);
        let items = Arc::clone(&catalog);
        let live=world.execute(move|| {
            let mut live=Live::default();let mut cursor=None;
            loop {
                stop.check()?;
                let page=storage.scan_drops(cursor,16)?;
                if page.is_empty(){break;}
                for snapshot in &page {
                    cursor=Some(snapshot.key());
                    if let Some(data)=snapshot.data() {
                        let cost=data.len().saturating_mul(2).saturating_add(4096);
                        if live.items.len()>=maximum||live.bytes.saturating_add(cost)>memory {return Err("Saved item entities exceed configured count or memory limits; raise item limits without deleting the world".into());}
                        let stored=zerqon_world::DropData{chunk:snapshot.chunk().ok_or("Live item has no chunk index")?,data:Arc::clone(data)};
                        let record=DropRecord::decode(&stored,world_id,snapshot.key(),&items)?;
                        live.items.insert(snapshot.key(),Item{id:item_ids.next()?,record,revision:snapshot.revision(),dirty:false,cost,collector:0});live.bytes+=cost;
                    }
                }
            }
            Ok(live)
        }).await?;
        let count = live.items.len();
        Ok(Arc::new(Self {
            world,
            players,
            catalog,
            blocks,
            ids,
            world_id,
            item_type,
            maximum,
            memory,
            live: Arc::new(Mutex::new(live)),
            count: AtomicUsize::new(count),
        }))
    }
    pub fn count(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }
    pub(crate) async fn reserve(self: &Arc<Self>) -> Result<DropLease> {
        self.world.check_available()?;
        self.reserve_cleanup().await
    }
    pub(crate) async fn reserve_cleanup(self: &Arc<Self>) -> Result<DropLease> {
        self.world.check_health()?;
        Ok(DropLease {
            service: Arc::clone(self),
            live: Arc::clone(&self.live).lock_owned().await,
            staged: Vec::new(),
        })
    }
    pub async fn tick(self: &Arc<Self>, tick: u64) -> Result<Vec<(Uuid, i32, DropKey)>> {
        let live = Arc::clone(&self.live).lock_owned().await;
        if live.items.is_empty() && live.visible.is_empty() {
            return Ok(Vec::new());
        }
        let gameplay = self.world.gameplay_read().await;
        let service = Arc::clone(self);
        self.world
            .execute(move || {
                let _gameplay = gameplay;
                let mut live = live;
                service.world.check_available()?;
                let observers = service.players.snapshots()?;
                if observers.is_empty() {
                    live.visible.clear();
                    return Ok(Vec::new());
                }
                let terrain = Terrain {
                    chunks: service
                        .world
                        .cached_chunks()?
                        .into_iter()
                        .map(|c| ((c.x, c.z), c))
                        .collect(),
                    blocks: Arc::clone(&service.blocks),
                };
                let mut scratch = Vec::new();
                let mut expired = Vec::new();
                let mut moved_cells = BTreeSet::new();
                for (&key, item) in &mut live.items {
                    let previous = item.record.position.map(|value| value.floor() as i32);
                    let resistant = service
                        .catalog
                        .effective_component(&item.record.stack, "minecraft:damage_resistant")?
                        .is_some_and(|value| value.to_string().contains("is_fire"));
                    if terrain.step(&mut item.record, resistant, &mut scratch)? {
                        item.dirty = true;
                        if previous != item.record.position.map(|value| value.floor() as i32) {
                            moved_cells.insert(key);
                        }
                    }
                    if item.record.age >= 6000
                        || item.record.health <= 0
                        || item.record.position[1] < f64::from(zerqon_world::MIN_Y - 64)
                    {
                        expired.push((key, None));
                    }
                }
                service.save_changes(&mut live, expired)?;
                let mut changed = BTreeSet::new();
                if tick.is_multiple_of(40) || (tick.is_multiple_of(2) && !moved_cells.is_empty()) {
                    let mut cells = BTreeMap::<[i32; 3], Vec<DropKey>>::new();
                    for (&key, item) in &live.items {
                        cells
                            .entry(item.record.position.map(|x| x.floor() as i32))
                            .or_default()
                            .push(key);
                    }
                    let mut merged = BTreeSet::new();
                    let mut pairs = Vec::new();
                    for (&key, item) in &live.items {
                        if merged.contains(&key)
                            || item.record.pickup_delay == i16::MAX
                            || item.record.age == i16::MIN
                            || (!tick.is_multiple_of(40) && !moved_cells.contains(&key))
                        {
                            continue;
                        }
                        let cell = item.record.position.map(|x| x.floor() as i32);
                        'search: for x in cell[0] - 1..=cell[0] + 1 {
                            for y in cell[1] - 1..=cell[1] + 1 {
                                for z in cell[2] - 1..=cell[2] + 1 {
                                    if let Some(keys) = cells.get(&[x, y, z]) {
                                        for &other in keys {
                                            if other == key || merged.contains(&other) {
                                                continue;
                                            }
                                            let candidate = &live.items[&other];
                                            if candidate.record.pickup_delay == i16::MAX
                                                || candidate.record.age == i16::MIN
                                                || candidate.record.owner != item.record.owner
                                                || !service.catalog.same_kind(
                                                    &item.record.stack,
                                                    &candidate.record.stack,
                                                )?
                                                || (0..3).any(|axis| {
                                                    (item.record.position[axis]
                                                        - candidate.record.position[axis])
                                                        .abs()
                                                        >= if axis == 1 { 0.25 } else { 0.75 }
                                                })
                                            {
                                                continue;
                                            }
                                            if u16::from(item.record.stack.count())
                                                + u16::from(candidate.record.stack.count())
                                                > u16::from(
                                                    service
                                                        .catalog
                                                        .effective_max_stack(&item.record.stack)?,
                                                )
                                            {
                                                continue;
                                            }
                                            merged.insert(key);
                                            merged.insert(other);
                                            changed.insert(key);
                                            changed.insert(other);
                                            pairs.push((key, other));
                                            break 'search;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let mut merge_batch = Vec::new();
                    let mut merge_bytes = 0usize;
                    for (key, other) in pairs {
                        let first = &live.items[&key];
                        let (receiver, donor) = if first.record.stack.count()
                            >= live.items[&other].record.stack.count()
                        {
                            (key, other)
                        } else {
                            (other, key)
                        };
                        let mut a = live.items[&receiver].record.clone();
                        let mut b = live.items[&donor].record.clone();
                        let total = u16::from(a.stack.count()) + u16::from(b.stack.count());
                        let received = total.min(64) as u8;
                        a.stack = a.stack.with_count(received)?;
                        a.pickup_delay = a.pickup_delay.max(b.pickup_delay);
                        a.age = a.age.min(b.age);
                        let remainder = total - u16::from(received);
                        let b = if remainder == 0 {
                            None
                        } else {
                            b.stack = b.stack.with_count(remainder as u8)?;
                            Some(b)
                        };
                        let mut pair = Vec::new();
                        let mut size = 0;
                        for (key, record) in [(receiver, Some(a)), (donor, b)] {
                            let replacement = record
                                .as_ref()
                                .map(|record| {
                                    record.encode(service.world_id, key, &service.catalog)
                                })
                                .transpose()?;
                            size += replacement.as_ref().map_or(0, |data| data.data.len());
                            pair.push((DropWrite { key, replacement }, record));
                        }
                        if !merge_batch.is_empty()
                            && (merge_batch.len() + 2 > 256 || merge_bytes + size > 8 * 1024 * 1024)
                        {
                            service.commit_batch(&mut live, std::mem::take(&mut merge_batch))?;
                            merge_bytes = 0;
                        }
                        merge_bytes += size;
                        merge_batch.extend(pair);
                    }
                    if !merge_batch.is_empty() {
                        service.commit_batch(&mut live, merge_batch)?;
                    }
                }
                if tick.is_multiple_of(100) {
                    service.save_dirty(&mut live)?;
                }
                service.synchronize(&mut live, &changed, tick.is_multiple_of(20))?;
                let mut pickups = Vec::new();
                let alive: BTreeSet<_> = service
                    .players
                    .combat_targets()?
                    .into_iter()
                    .map(|p| p.id)
                    .collect();
                let cursor = live.pickup_cursor;
                let keys: Vec<_> = live
                    .items
                    .keys()
                    .filter(|key| cursor.is_none_or(|cursor| **key > cursor))
                    .chain(
                        live.items
                            .keys()
                            .filter(|key| cursor.is_some_and(|cursor| **key <= cursor)),
                    )
                    .copied()
                    .collect();
                for key in keys {
                    live.pickup_cursor = Some(key);
                    let item = live.items.get_mut(&key).ok_or("Pickup item disappeared")?;
                    if item.record.pickup_delay > 0 {
                        continue;
                    }
                    for offset in 0..observers.len() {
                        let index = (item.collector + offset) % observers.len();
                        let player = &observers[index];
                        if alive.contains(&player.id)
                            && pickup_allowed(&item.record, player.uuid, player.position.xyz)
                        {
                            pickups.push((player.uuid, player.id, key));
                            item.collector = (index + 1) % observers.len();
                            break;
                        }
                    }
                    if pickups.len() == 32 {
                        break;
                    }
                }
                Ok(pickups)
            })
            .await
    }
    pub async fn flush(self: &Arc<Self>) -> Result<()> {
        let live = Arc::clone(&self.live).lock_owned().await;
        let service = Arc::clone(self);
        self.world
            .execute(move || {
                let mut live = live;
                service.world.check_health()?;
                service.save_dirty(&mut live)
            })
            .await
    }
    fn save_dirty(&self, live: &mut Live) -> Result<()> {
        let records = live
            .items
            .iter()
            .filter(|(_, item)| item.dirty)
            .map(|(&key, item)| (key, Some(item.record.clone())))
            .collect();
        self.save_changes(live, records)
    }
    fn save_changes(
        &self,
        live: &mut Live,
        changes: Vec<(DropKey, Option<DropRecord>)>,
    ) -> Result<()> {
        let mut batch = Vec::new();
        let mut bytes = 0usize;
        for (key, record) in changes {
            let replacement = record
                .as_ref()
                .map(|record| record.encode(self.world_id, key, &self.catalog))
                .transpose()?;
            let size = replacement.as_ref().map_or(0, |data| data.data.len());
            if !batch.is_empty() && (batch.len() >= 256 || bytes + size > 8 * 1024 * 1024) {
                self.commit_batch(live, std::mem::take(&mut batch))?;
                bytes = 0;
            }
            bytes += size;
            batch.push((DropWrite { key, replacement }, record));
        }
        if !batch.is_empty() {
            self.commit_batch(live, batch)?;
        }
        Ok(())
    }
    fn commit_batch(
        &self,
        live: &mut Live,
        batch: Vec<(DropWrite, Option<DropRecord>)>,
    ) -> Result<()> {
        let storage = self.world.storage();
        let keys: Vec<_> = batch.iter().map(|(write, _)| write.key).collect();
        let reads = storage.snapshot_domain(&[], &[], &keys)?;
        if reads.drops.iter().any(|read| {
            live.items
                .get(&read.key())
                .is_none_or(|item| item.revision != read.revision())
        }) {
            self.world.stop_after_storage_failure();
            return Err("item records changed outside the item simulation".into());
        }
        let writes: Vec<_> = batch
            .iter()
            .map(|(write, _)| DropWrite {
                key: write.key,
                replacement: write.replacement.clone(),
            })
            .collect();
        let receipt = storage
            .commit_domain(Uuid::new_v4(), &reads, &[], &[], &writes)
            .inspect_err(|_| self.world.stop_after_storage_failure())?;
        for (write, record) in batch {
            if let Some(record) = record {
                let item = live
                    .items
                    .get_mut(&write.key)
                    .ok_or("committed item disappeared")?;
                item.record = record;
                item.revision = receipt.revision;
                item.dirty = false;
            } else if let Some(item) = live.items.remove(&write.key) {
                live.bytes = live.bytes.saturating_sub(item.cost);
            }
        }
        self.count.store(live.items.len(), Ordering::Relaxed);
        Ok(())
    }
    fn synchronize(
        &self,
        live: &mut Live,
        changed: &BTreeSet<DropKey>,
        movement: bool,
    ) -> Result<()> {
        let players = self.players.snapshots()?;
        let active: BTreeSet<_> = players.iter().map(|p| (p.uuid, p.id)).collect();
        live.visible.retain(|key, _| active.contains(key));
        let mut dispatch = Vec::new();
        for player in players {
            let current: BTreeMap<_, _> = live
                .items
                .iter()
                .filter(|(_, item)| {
                    (item.record.position[0] - player.position.xyz[0]).abs() <= 96.0
                        && (item.record.position[2] - player.position.xyz[2]).abs() <= 96.0
                })
                .map(|(&key, item)| (key, item.id))
                .collect();
            let previous = live.visible.entry((player.uuid, player.id)).or_default();
            let mut packets: Vec<Bytes> = Vec::new();
            let removed: Vec<_> = previous
                .iter()
                .filter(|(key, _)| !current.contains_key(key))
                .map(|(_, id)| *id)
                .collect();
            if !removed.is_empty() {
                packets.push(entity_packets::remove_entities(&removed).into());
            }
            for (&key, &id) in &current {
                let item = &live.items[&key];
                if !previous.contains_key(&key) {
                    packets.push(
                        entity_packets::add_entity_with_motion(
                            id,
                            key.uuid,
                            self.item_type,
                            item.record.position,
                            item.record.velocity,
                            item.record.rotation[0],
                            item.record.rotation[1],
                        )?
                        .into(),
                    );
                    packets.push(
                        entity_packets::item_metadata(
                            id,
                            &Some(item.record.stack.clone()),
                            &self.catalog,
                        )?
                        .into(),
                    );
                    if item.record.no_gravity {
                        packets.push(entity_packets::no_gravity(id, true).into());
                    }
                } else {
                    if changed.contains(&key) {
                        packets.push(
                            entity_packets::item_metadata(
                                id,
                                &Some(item.record.stack.clone()),
                                &self.catalog,
                            )?
                            .into(),
                        );
                    }
                    if movement {
                        packets.push(
                            entity_packets::try_position_sync(
                                id,
                                item.record.position,
                                item.record.rotation[0],
                                item.record.rotation[1],
                                item.record.on_ground,
                            )?
                            .into(),
                        );
                        packets
                            .push(entity_packets::entity_motion(id, item.record.velocity)?.into());
                    }
                }
            }
            *previous = current;
            if !packets.is_empty() {
                dispatch.push((player.uuid, player.id, packets));
            }
        }
        self.players.dispatch_batches(dispatch)?;
        Ok(())
    }
}

impl DropLease {
    pub fn item(&self, key: DropKey) -> Option<(i32, DropRecord, u64)> {
        self.live
            .items
            .get(&key)
            .map(|item| (item.id, item.record.clone(), item.revision))
    }
    pub fn stage(&mut self, key: DropKey, record: DropRecord) -> Result<DropWrite> {
        if self.live.items.contains_key(&key) || self.staged.iter().any(|(k, _)| *k == key) {
            return Err("duplicate item identity".into());
        }
        let data = record.encode(self.service.world_id, key, &self.service.catalog)?;
        let cost = data.data.len().saturating_mul(2).saturating_add(4096);
        if self.live.items.len() + self.staged.len() >= self.service.maximum
            || self.live.bytes + self.staged.iter().map(|(_, item)| item.cost).sum::<usize>() + cost
                > self.service.memory
        {
            return Err("Item entity capacity reached".into());
        }
        self.staged.push((
            key,
            Item {
                id: self.service.ids.next()?,
                record,
                revision: 0,
                dirty: false,
                cost,
                collector: 0,
            },
        ));
        Ok(DropWrite {
            key,
            replacement: Some(data),
        })
    }
    pub fn publish_new(&mut self, revision: u64) -> Result<()> {
        for (key, mut item) in self.staged.drain(..) {
            item.revision = revision;
            self.live.bytes += item.cost;
            self.live.items.insert(key, item);
        }
        self.service
            .count
            .store(self.live.items.len(), Ordering::Relaxed);
        self.service
            .synchronize(&mut self.live, &BTreeSet::new(), false)
    }
    pub fn encode(&self, key: DropKey, record: &DropRecord) -> Result<zerqon_world::DropData> {
        Ok(record.encode(self.service.world_id, key, &self.service.catalog)?)
    }
    pub fn publish_pickup(
        &mut self,
        key: DropKey,
        remainder: Option<DropRecord>,
        revision: u64,
        collector: i32,
        amount: i32,
    ) -> Result<()> {
        if let Some(record) = remainder {
            let item = self
                .live
                .items
                .get_mut(&key)
                .ok_or("picked item disappeared")?;
            item.record = record;
            item.revision = revision;
            item.dirty = false;
        } else if let Some(item) = self.live.items.remove(&key) {
            self.live.bytes = self.live.bytes.saturating_sub(item.cost);
            let packet = Bytes::from(entity_packets::take_item_entity(
                item.id, collector, amount,
            )?);
            let recipients = self
                .live
                .visible
                .iter()
                .filter(|(_, items)| items.contains_key(&key))
                .map(|(&(uuid, id), _)| (uuid, id, vec![packet.clone()]))
                .collect();
            self.service.players.dispatch_batches(recipients)?;
        }
        self.service
            .count
            .store(self.live.items.len(), Ordering::Relaxed);
        self.service
            .synchronize(&mut self.live, &BTreeSet::from([key]), false)
    }
}

pub(crate) fn pickup_allowed(record: &DropRecord, player: Uuid, position: [f64; 3]) -> bool {
    record.pickup_delay <= 0
        && record.owner.is_none_or(|owner| owner == player)
        && (record.position[0] - position[0]).abs() < 1.425
        && (record.position[2] - position[2]).abs() < 1.425
        && record.position[1] > position[1] - 0.75
        && record.position[1] < position[1] + 2.3
}

fn sampler(key: DropKey) -> impl FnMut() -> f64 {
    let value = key.uuid.as_u128();
    let mut seed = value as u64 ^ (value >> 64) as u64;
    move || {
        seed = seed.wrapping_add(0x9e3779b97f4a7c15);
        let mut n = seed;
        n = (n ^ (n >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        n = (n ^ (n >> 27)).wrapping_mul(0x94d049bb133111eb);
        n ^= n >> 31;
        (n >> 11) as f64 / (1u64 << 53) as f64
    }
}
pub(crate) fn mined_record(
    key: DropKey,
    stack: zerqon_inventory::ItemStack,
    block: [i32; 3],
) -> DropRecord {
    let mut random = sampler(key);
    let position = block.map(|axis| f64::from(axis) + 0.25 + random() * 0.5);
    let mut record = DropRecord::new(
        stack,
        position,
        [random() * 0.2 - 0.1, 0.2, random() * 0.2 - 0.1],
        [(random() * 360.0) as f32, 0.0],
    );
    record.pickup_delay = 10;
    record
}
pub(crate) fn thrown_record(
    key: DropKey,
    stack: zerqon_inventory::ItemStack,
    position: crate::players::Position,
    thrower: Uuid,
) -> DropRecord {
    let mut random = sampler(key);
    let yaw = f64::from(position.yaw).to_radians();
    let pitch = f64::from(position.pitch).to_radians();
    let angle = random() * std::f64::consts::TAU;
    let noise = random() * 0.02;
    let velocity = [
        -yaw.sin() * pitch.cos() * f64::from(0.3_f32) + angle.cos() * noise,
        -pitch.sin() * f64::from(0.3_f32) + f64::from(0.1_f32),
        yaw.cos() * pitch.cos() * f64::from(0.3_f32) + angle.sin() * noise,
    ];
    let mut record = DropRecord::new(
        stack,
        [
            position.xyz[0],
            position.xyz[1] + f64::from(1.62_f32) - f64::from(0.3_f32),
            position.xyz[2],
        ],
        velocity,
        [position.yaw, position.pitch],
    );
    record.pickup_delay = 40;
    record.thrower = Some(thrower);
    record
}
