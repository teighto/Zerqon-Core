use crate::{entity_packets, game_mode::GameMode};
use bytes::Bytes;
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicI32, Ordering},
};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

#[cfg(test)]
mod tracking_review;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InventoryRevision {
    pub uuid: Uuid,
    pub entity_id: i32,
    pub generation: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PacketBatch {
    packets: Vec<Bytes>,
    inventory_revision: Option<InventoryRevision>,
}
impl PacketBatch {
    pub fn inventory_revision(&self) -> Option<InventoryRevision> {
        self.inventory_revision
    }
    pub fn with_inventory_revision(mut self, revision: InventoryRevision) -> Self {
        self.inventory_revision = Some(revision);
        self
    }
}
impl From<Vec<Bytes>> for PacketBatch {
    fn from(packets: Vec<Bytes>) -> Self {
        Self {
            packets,
            inventory_revision: None,
        }
    }
}
impl std::ops::Deref for PacketBatch {
    type Target = Vec<Bytes>;
    fn deref(&self) -> &Self::Target {
        &self.packets
    }
}
impl IntoIterator for PacketBatch {
    type Item = Bytes;
    type IntoIter = std::vec::IntoIter<Bytes>;
    fn into_iter(self) -> Self::IntoIter {
        self.packets.into_iter()
    }
}

#[derive(Clone, Debug)]
pub struct PlayerSnapshot {
    pub uuid: Uuid,
    pub id: i32,
    pub name: String,
    pub position: Position,
}

#[derive(Clone, Debug)]
pub struct Identity {
    pub uuid: Uuid,
    pub name: String,
    pub properties: Vec<(String, String, Option<String>)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Position {
    pub xyz: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub on_ground: bool,
}
impl Default for Position {
    fn default() -> Self {
        Self {
            xyz: [0.5, 64.0, 0.5],
            yaw: 0.0,
            pitch: 0.0,
            on_ground: true,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PlayerError {
    #[error("player is already connected")]
    Duplicate,
    #[error("player capacity reached")]
    Capacity,
    #[error("invalid player position or identity")]
    Invalid,
    #[error("player registry unavailable")]
    Unavailable,
}

pub struct EntityIds(AtomicI32);
impl Default for EntityIds {
    fn default() -> Self {
        Self(AtomicI32::new(1))
    }
}
impl EntityIds {
    pub fn advance_past(&self, id: i32) -> Result<(), PlayerError> {
        if id < 0 {
            return Err(PlayerError::Invalid);
        }
        let next = id.checked_add(1).ok_or(PlayerError::Capacity)?;
        self.0.fetch_max(next, Ordering::Relaxed);
        Ok(())
    }
    pub fn next(&self) -> Result<i32, PlayerError> {
        self.0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| PlayerError::Capacity)
    }
}

struct Player {
    identity: Identity,
    id: i32,
    position: Position,
    active: bool,
    outbound: mpsc::Sender<PacketBatch>,
    disconnect: watch::Sender<bool>,
    equipment: Option<Bytes>,
    profile: Bytes,
    latency: i32,
    game_mode: GameMode,
    health: f32,
    invulnerable_until: std::time::Instant,
    spawn_revision: u64,
    sneaking: bool,
}
#[derive(Clone)]
struct ReplicatedPlayer {
    id: i32,
    position: Position,
    equipment: Option<Bytes>,
    profile: Bytes,
    latency: i32,
    health: f32,
    spawn_revision: u64,
}
struct TrackingPackets {
    spawn: PacketBatch,
    movement: [Bytes; 2],
    latency: Bytes,
}
pub struct Players {
    inner: Mutex<BTreeMap<Uuid, Player>>,
    replicated: Mutex<BTreeMap<Uuid, ReplicatedPlayer>>,
    tracking_dirty: AtomicBool,
    capacity: usize,
    radius: f64,
    player_type: i32,
    ids: Arc<EntityIds>,
}
pub struct PlayerHandle {
    players: Arc<Players>,
    uuid: Uuid,
    id: i32,
}

fn player_snapshot(player: &Player) -> PlayerSnapshot {
    PlayerSnapshot {
        uuid: player.identity.uuid,
        id: player.id,
        name: player.identity.name.clone(),
        position: player.position,
    }
}

impl Players {
    pub(crate) fn is_sneaking(&self, uuid: Uuid, id: i32) -> Result<bool, PlayerError> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| PlayerError::Unavailable)?
            .get(&uuid)
            .filter(|p| p.id == id)
            .is_some_and(|p| p.sneaking))
    }
    pub(crate) fn combat_targets(&self) -> Result<Vec<zerqon_entities::PlayerTarget>, PlayerError> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| PlayerError::Unavailable)?
            .values()
            .filter(|p| p.active && p.health > 0.0)
            .map(|p| zerqon_entities::PlayerTarget {
                id: p.id,
                position: p.position.xyz,
                attackable: p.game_mode == GameMode::Survival,
            })
            .collect())
    }

    pub(crate) fn mob_attack(
        &self,
        id: i32,
        origin: [f64; 3],
        damage: f32,
    ) -> Result<(), PlayerError> {
        if !damage.is_finite() || damage <= 0.0 || origin.iter().any(|v| !v.is_finite()) {
            return Err(PlayerError::Invalid);
        }
        let mut inner = self.inner.lock().map_err(|_| PlayerError::Unavailable)?;
        let Some(player) = inner.values_mut().find(|p| {
            p.id == id
                && p.active
                && p.health > 0.0
                && p.game_mode == GameMode::Survival
                && std::time::Instant::now() >= p.invulnerable_until
        }) else {
            return Ok(());
        };
        let dx = player.position.xyz[0] - origin[0];
        let dz = player.position.xyz[2] - origin[2];
        let length = dx.hypot(dz).max(0.01);
        let impulse = [dx / length * 0.4, 0.35, dz / length * 0.4];
        player.health = (player.health - damage).max(0.0);
        player.invulnerable_until =
            std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut packets: Vec<Bytes> = vec![
            entity_packets::health(player.health, 20, 5.0).into(),
            entity_packets::hurt_animation(id, 0.0)
                .map_err(|_| PlayerError::Invalid)?
                .into(),
            entity_packets::entity_motion(id, impulse)
                .map_err(|_| PlayerError::Invalid)?
                .into(),
        ];
        if player.health == 0.0 {
            packets.push(crate::blast_packets::combat_kill(id, "Slain by a zombie").into());
        }
        enqueue(player, packets);
        let (position, health) = (player.position, player.health);
        for observer in inner
            .values()
            .filter(|p| p.active && p.id != id && self.visible(p.position, position))
        {
            enqueue(
                observer,
                vec![
                    Bytes::from(entity_packets::mob_health(id, health)),
                    entity_packets::hurt_animation(id, 0.0)
                        .map_err(|_| PlayerError::Invalid)?
                        .into(),
                    entity_packets::entity_motion(id, impulse)
                        .map_err(|_| PlayerError::Invalid)?
                        .into(),
                ],
            );
        }
        Ok(())
    }
    pub(crate) fn is_alive(&self, uuid: Uuid, id: i32) -> Result<bool, PlayerError> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| PlayerError::Unavailable)?
            .get(&uuid)
            .is_some_and(|player| player.id == id && player.active && player.health > 0.0))
    }

    pub(crate) fn blast_targets(&self, center: [f64; 3]) -> Result<Vec<(Uuid, i32)>, PlayerError> {
        let inner = self.inner.lock().map_err(|_| PlayerError::Unavailable)?;
        Ok(inner
            .values()
            .filter(|player| {
                player.active
                    && player.health > 0.0
                    && player.game_mode == GameMode::Survival
                    && crate::blast_damage::within_blast(center, player.position.xyz)
            })
            .map(|player| (player.identity.uuid, player.id))
            .collect())
    }

    pub(crate) fn apply_blast(
        &self,
        center: [f64; 3],
        view: &crate::blast_damage::BlastView,
        targets: &[(Uuid, i32)],
    ) -> Result<(), PlayerError> {
        let mut inner = self.inner.lock().map_err(|_| PlayerError::Unavailable)?;
        let mut notifications = Vec::new();
        for &(uuid, id) in targets {
            let Some(player) = inner.get_mut(&uuid).filter(|player| {
                player.id == id
                    && player.active
                    && player.health > 0.0
                    && player.game_mode == GameMode::Survival
                    && std::time::Instant::now() >= player.invulnerable_until
            }) else {
                continue;
            };
            let Some(effect) = view.effect(
                center,
                player.position.xyz,
                [f64::from(0.6_f32), f64::from(1.8_f32)],
                f64::from(1.62_f32),
            ) else {
                continue;
            };
            player.health = (player.health - effect.damage).max(0.0);
            let mut packets: Vec<Bytes> = vec![
                entity_packets::health(player.health, 20, 5.0).into(),
                entity_packets::hurt_animation(player.id, 0.0)
                    .map_err(|_| PlayerError::Invalid)?
                    .into(),
                entity_packets::entity_motion(player.id, effect.impulse)
                    .map_err(|_| PlayerError::Invalid)?
                    .into(),
            ];
            if player.health == 0.0 {
                packets.push(crate::blast_packets::death(player.id).into());
            }
            enqueue(player, packets);
            notifications.push((player.id, player.position, player.health, effect.impulse));
        }
        for (id, position, health, impulse) in notifications {
            let packets: Vec<Bytes> = vec![
                entity_packets::mob_health(id, health).into(),
                entity_packets::hurt_animation(id, 0.0)
                    .map_err(|_| PlayerError::Invalid)?
                    .into(),
                entity_packets::entity_motion(id, impulse)
                    .map_err(|_| PlayerError::Invalid)?
                    .into(),
            ];
            for observer in inner.values().filter(|observer| {
                observer.active && observer.id != id && self.visible(observer.position, position)
            }) {
                enqueue(observer, packets.clone());
            }
        }
        Ok(())
    }

    pub fn set_equipment(&self, uuid: Uuid, id: i32, packet: Bytes) -> Result<(), PlayerError> {
        let mut inner = self.inner.lock().map_err(|_| PlayerError::Unavailable)?;
        let Some(player) = inner.get_mut(&uuid).filter(|player| player.id == id) else {
            return Ok(());
        };
        let mut header = packet.as_ref();
        if zerqon_protocol::codec::get_varint(&mut header).ok() != Some(104)
            || zerqon_protocol::codec::get_varint(&mut header).ok() != Some(id)
            || packet.len() > 256 * 1024
        {
            return Err(PlayerError::Invalid);
        }
        if player.equipment.as_ref() == Some(&packet) {
            return Ok(());
        }
        player.equipment = Some(packet);
        self.tracking_dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn tracking_tick(&self) -> Result<(), PlayerError> {
        let inner = self.inner.lock().map_err(|_| PlayerError::Unavailable)?;
        if !self.tracking_dirty.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut previous = self
            .replicated
            .lock()
            .map_err(|_| PlayerError::Unavailable)?;
        let current: BTreeMap<_, _> = inner
            .values()
            .filter(|player| player.active)
            .map(|player| {
                (
                    player.identity.uuid,
                    ReplicatedPlayer {
                        id: player.id,
                        position: player.position,
                        equipment: player.equipment.clone(),
                        profile: player.profile.clone(),
                        latency: player.latency,
                        health: player.health,
                        spawn_revision: player.spawn_revision,
                    },
                )
            })
            .collect();
        let packets: BTreeMap<_, _> = current
            .iter()
            .map(|(&uuid, player)| {
                let mut latency = vec![71, 0x10, 1];
                latency.extend_from_slice(uuid.as_bytes());
                zerqon_protocol::codec::put_varint(player.latency, &mut latency);
                (
                    uuid,
                    TrackingPackets {
                        spawn: spawn_packets(uuid, player, self.player_type),
                        movement: [
                            entity_packets::position_sync(
                                player.id,
                                player.position.xyz,
                                player.position.yaw,
                                player.position.pitch,
                                player.position.on_ground,
                            )
                            .into(),
                            entity_packets::head_rotation(player.id, player.position.yaw).into(),
                        ],
                        latency: latency.into(),
                    },
                )
            })
            .collect();
        for (&observer_uuid, observer) in &current {
            let destination = &inner[&observer_uuid];
            if *destination.disconnect.borrow() {
                continue;
            }
            let old_observer = previous
                .get(&observer_uuid)
                .filter(|old| old.id == observer.id);
            let mut batch = Vec::new();
            if let Some(old_observer) = old_observer {
                let mut removed_entities = Vec::new();
                let mut removed_profiles = Vec::new();
                for (&uuid, old) in previous.iter() {
                    let next = current.get(&uuid).filter(|next| next.id == old.id);
                    let was_visible =
                        uuid != observer_uuid && self.visible(old.position, old_observer.position);
                    if was_visible
                        && next.is_none_or(|next| {
                            !self.visible(next.position, observer.position)
                                || next.spawn_revision != old.spawn_revision
                        })
                    {
                        removed_entities.push(old.id);
                    }
                    if next.is_none() {
                        removed_profiles.push(uuid);
                    }
                }
                if !removed_entities.is_empty() {
                    batch.push(entity_packets::remove_entities(&removed_entities).into());
                }
                if !removed_profiles.is_empty() {
                    batch.push(entity_packets::player_info_remove(&removed_profiles).into());
                }
            }
            for (&uuid, player) in &current {
                let old = old_observer
                    .and_then(|_| previous.get(&uuid))
                    .filter(|old| old.id == player.id);
                let encoded = &packets[&uuid];
                if old.is_none() {
                    batch.push(player.profile.clone());
                }
                if player.latency != old.map_or(0, |old| old.latency) {
                    batch.push(encoded.latency.clone());
                }
                if uuid == observer_uuid || !self.visible(player.position, observer.position) {
                    continue;
                }
                let was_visible = old.zip(old_observer).is_some_and(|(old, observer)| {
                    old.spawn_revision == player.spawn_revision
                        && self.visible(old.position, observer.position)
                });
                if !was_visible {
                    batch.extend(encoded.spawn.iter().cloned());
                } else if let Some(old) = old {
                    if player.position != old.position {
                        batch.extend(encoded.movement.iter().cloned());
                    }
                    if player.equipment != old.equipment
                        && let Some(equipment) = &player.equipment
                    {
                        batch.push(equipment.clone());
                    }
                }
            }
            if !batch.is_empty() {
                enqueue(destination, batch);
            }
        }
        // Failed enqueues permanently disconnect their observer. Every remaining
        // observer has the same last-enqueued baseline, in its reliable FIFO.
        *previous = current;
        self.tracking_dirty.store(false, Ordering::Relaxed);
        Ok(())
    }
    pub fn snapshots(&self) -> Result<Vec<PlayerSnapshot>, PlayerError> {
        let inner = self.inner.lock().map_err(|_| PlayerError::Unavailable)?;
        Ok(inner
            .values()
            .filter(|player| player.active)
            .map(player_snapshot)
            .collect())
    }

    pub fn query_snapshot(&self, uuid: Uuid) -> Result<Option<PlayerSnapshot>, PlayerError> {
        let inner = self
            .inner
            .try_lock()
            .map_err(|_| PlayerError::Unavailable)?;
        Ok(inner
            .get(&uuid)
            .filter(|player| player.active)
            .map(player_snapshot))
    }

    pub fn snapshots_bounded(&self, limit: usize) -> Result<Vec<PlayerSnapshot>, PlayerError> {
        let inner = self
            .inner
            .try_lock()
            .map_err(|_| PlayerError::Unavailable)?;
        if inner
            .values()
            .filter(|player| player.active)
            .take(limit.saturating_add(1))
            .count()
            > limit
        {
            return Err(PlayerError::Capacity);
        }
        Ok(inner
            .values()
            .filter(|player| player.active)
            .map(player_snapshot)
            .collect())
    }
    pub fn dispatch_batches<B: Into<PacketBatch>>(
        &self,
        batches: Vec<(Uuid, i32, B)>,
    ) -> Result<(), PlayerError> {
        let inner = self.inner.lock().map_err(|_| PlayerError::Unavailable)?;
        for (uuid, id, packets) in batches {
            let packets = packets.into();
            if packets.is_empty() {
                continue;
            }
            if let Some(player) = inner
                .get(&uuid)
                .filter(|player| player.active && player.id == id)
            {
                enqueue(player, packets);
            }
        }
        Ok(())
    }
    pub fn new(capacity: usize, radius: f64, player_type: i32, ids: Arc<EntityIds>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(BTreeMap::new()),
            replicated: Mutex::new(BTreeMap::new()),
            tracking_dirty: AtomicBool::new(false),
            capacity,
            radius,
            player_type,
            ids,
        })
    }
    pub fn reserve(
        self: &Arc<Self>,
        identity: Identity,
        outbound: mpsc::Sender<PacketBatch>,
        disconnect: watch::Sender<bool>,
    ) -> Result<PlayerHandle, PlayerError> {
        if identity.name.is_empty()
            || identity.name.len() > 16
            || !identity
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || identity.uuid.is_nil()
            || identity.properties.len() > 16
        {
            return Err(PlayerError::Invalid);
        }
        let mut inner = self.inner.lock().map_err(|_| PlayerError::Unavailable)?;
        if inner.contains_key(&identity.uuid)
            || inner
                .values()
                .any(|player| player.identity.name.eq_ignore_ascii_case(&identity.name))
        {
            return Err(PlayerError::Duplicate);
        }
        if inner.len() >= self.capacity {
            return Err(PlayerError::Capacity);
        }
        let id = self.ids.next()?;
        let uuid = identity.uuid;
        let profile = profile_packet(&identity, GameMode::Creative);
        inner.insert(
            uuid,
            Player {
                identity,
                id,
                position: Position::default(),
                active: false,
                outbound,
                disconnect,
                equipment: None,
                profile,
                latency: 0,
                game_mode: GameMode::Creative,
                health: 20.0,
                invulnerable_until: std::time::Instant::now(),
                spawn_revision: 0,
                sneaking: false,
            },
        );
        Ok(PlayerHandle {
            players: Arc::clone(self),
            uuid,
            id,
        })
    }
    pub fn count(&self) -> usize {
        self.inner.lock().map_or(0, |inner| {
            inner.values().filter(|player| player.active).count()
        })
    }
    pub fn try_count(&self) -> Result<usize, PlayerError> {
        self.inner
            .try_lock()
            .map(|inner| inner.values().filter(|player| player.active).count())
            .map_err(|_| PlayerError::Unavailable)
    }
    pub fn names(&self) -> Vec<String> {
        self.inner.lock().map_or_else(
            |_| Vec::new(),
            |inner| {
                inner
                    .values()
                    .filter(|player| player.active)
                    .map(|player| player.identity.name.clone())
                    .collect()
            },
        )
    }
    pub fn broadcast(&self, packet: Bytes) -> Result<(), PlayerError> {
        let inner = self.inner.lock().map_err(|_| PlayerError::Unavailable)?;
        for player in inner.values().filter(|player| player.active) {
            enqueue(player, vec![packet.clone()]);
        }
        Ok(())
    }
    fn visible(&self, left: Position, right: Position) -> bool {
        (left.xyz[0] - right.xyz[0]).abs() <= self.radius
            && (left.xyz[2] - right.xyz[2]).abs() <= self.radius
    }
}
impl PlayerHandle {
    pub fn set_sneaking(&self, sneaking: bool) -> Result<(), PlayerError> {
        let mut inner = self
            .players
            .inner
            .lock()
            .map_err(|_| PlayerError::Unavailable)?;
        let player = inner
            .get_mut(&self.uuid)
            .filter(|p| p.id == self.id)
            .ok_or(PlayerError::Unavailable)?;
        player.sneaking = sneaking;
        Ok(())
    }
    pub fn health(&self) -> Result<f32, PlayerError> {
        self.players
            .inner
            .lock()
            .map_err(|_| PlayerError::Unavailable)?
            .get(&self.uuid)
            .filter(|player| player.id == self.id)
            .map(|player| player.health)
            .ok_or(PlayerError::Unavailable)
    }
    pub fn respawn(&self, position: Position) -> Result<bool, PlayerError> {
        if !crate::world_view::valid_position(position) {
            return Err(PlayerError::Invalid);
        }
        let mut inner = self
            .players
            .inner
            .lock()
            .map_err(|_| PlayerError::Unavailable)?;
        let player = inner
            .get_mut(&self.uuid)
            .filter(|player| player.id == self.id)
            .ok_or(PlayerError::Unavailable)?;
        if player.health > 0.0 {
            return Ok(false);
        }
        player.spawn_revision = player
            .spawn_revision
            .checked_add(1)
            .ok_or(PlayerError::Capacity)?;
        player.health = 20.0;
        player.position = position;
        player.invulnerable_until = std::time::Instant::now() + std::time::Duration::from_secs(3);
        self.players.tracking_dirty.store(true, Ordering::Relaxed);
        Ok(true)
    }
    pub fn id(&self) -> i32 {
        self.id
    }
    pub fn initialize_game_mode(&self, game_mode: GameMode) -> Result<(), PlayerError> {
        let mut inner = self
            .players
            .inner
            .lock()
            .map_err(|_| PlayerError::Unavailable)?;
        let player = inner
            .get_mut(&self.uuid)
            .filter(|player| player.id == self.id)
            .ok_or(PlayerError::Unavailable)?;
        if player.active {
            return Err(PlayerError::Invalid);
        }
        player.profile = profile_packet(&player.identity, game_mode);
        player.game_mode = game_mode;
        Ok(())
    }
    pub fn activate(&self) -> Result<(), PlayerError> {
        let mut inner = self
            .players
            .inner
            .lock()
            .map_err(|_| PlayerError::Unavailable)?;
        let player = inner
            .get_mut(&self.uuid)
            .filter(|player| player.id == self.id)
            .ok_or(PlayerError::Unavailable)?;
        if player.active {
            return Ok(());
        }
        player.active = true;
        self.players.tracking_dirty.store(true, Ordering::Relaxed);
        Ok(())
    }
    pub fn move_to(&self, position: Position) -> Result<(), PlayerError> {
        if position.xyz.iter().any(|value| !value.is_finite())
            || position.xyz[0].abs() > 30_000_000.0
            || position.xyz[2].abs() > 30_000_000.0
            || position.xyz[1].abs() > 20_000_000.0
            || !position.yaw.is_finite()
            || !position.pitch.is_finite()
        {
            return Err(PlayerError::Invalid);
        }
        let mut inner = self
            .players
            .inner
            .lock()
            .map_err(|_| PlayerError::Unavailable)?;
        let player = inner
            .get_mut(&self.uuid)
            .filter(|player| player.id == self.id)
            .ok_or(PlayerError::Unavailable)?;
        if player.position == position {
            return Ok(());
        }
        player.position = position;
        self.players.tracking_dirty.store(true, Ordering::Relaxed);
        Ok(())
    }
    pub fn latency(&self, milliseconds: i32) -> Result<(), PlayerError> {
        let mut inner = self
            .players
            .inner
            .lock()
            .map_err(|_| PlayerError::Unavailable)?;
        let player = inner
            .get_mut(&self.uuid)
            .filter(|player| player.active && player.id == self.id)
            .ok_or(PlayerError::Unavailable)?;
        let milliseconds = milliseconds.max(0);
        if player.latency != milliseconds {
            player.latency = milliseconds;
            self.players.tracking_dirty.store(true, Ordering::Relaxed);
        }
        Ok(())
    }
}

impl Drop for PlayerHandle {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.players.inner.lock()
            && inner
                .get(&self.uuid)
                .is_some_and(|player| player.id == self.id)
        {
            inner.remove(&self.uuid);
            self.players.tracking_dirty.store(true, Ordering::Relaxed);
        }
    }
}
fn enqueue(player: &Player, batch: impl Into<PacketBatch>) {
    if *player.disconnect.borrow() {
        return;
    }
    if player.outbound.try_send(batch.into()).is_err() {
        player.disconnect.send_replace(true);
    }
}
fn profile_packet(identity: &Identity, game_mode: GameMode) -> Bytes {
    if identity.properties.is_empty() {
        return entity_packets::player_info_add(identity.uuid, &identity.name, true, game_mode)
            .into();
    }
    let properties: Vec<_> = identity
        .properties
        .iter()
        .map(|(name, value, signature)| (name.as_str(), value.as_str(), signature.as_deref()))
        .collect();
    entity_packets::player_info_add_properties(
        identity.uuid,
        &identity.name,
        true,
        &properties,
        game_mode,
    )
    .into()
}
fn spawn_packets(uuid: Uuid, player: &ReplicatedPlayer, kind: i32) -> PacketBatch {
    let mut packets = vec![
        entity_packets::add_entity(
            player.id,
            uuid,
            kind,
            player.position.xyz,
            player.position.yaw,
            player.position.pitch,
        )
        .into(),
        entity_packets::player_metadata(player.id, 127).into(),
    ];
    if player.health != 20.0 || player.spawn_revision > 0 {
        packets.push(entity_packets::mob_health(player.id, player.health).into());
    }
    if let Some(equipment) = &player.equipment {
        packets.push(equipment.clone());
    }
    packets.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerqon_protocol::codec::get_varint;
    type Peer = (
        PlayerHandle,
        mpsc::Receiver<PacketBatch>,
        watch::Receiver<bool>,
    );
    fn connect(players: &Arc<Players>, number: u128, name: &str) -> Peer {
        let (sender, receiver) = mpsc::channel(16);
        let (disconnect, closed) = watch::channel(false);
        let handle = players
            .reserve(
                Identity {
                    uuid: Uuid::from_u128(number),
                    name: name.into(),
                    properties: vec![],
                },
                sender,
                disconnect,
            )
            .unwrap();
        (handle, receiver, closed)
    }
    fn drain(receiver: &mut mpsc::Receiver<PacketBatch>) -> Vec<Bytes> {
        let mut packets = Vec::new();
        while let Ok(batch) = receiver.try_recv() {
            packets.extend(batch);
        }
        packets
    }

    #[test]
    fn initial_mode_is_sent_to_self_and_later_observers_and_cannot_change_after_activation() {
        fn mode(packets: &[Bytes], uuid: Uuid) -> i32 {
            let packet = packets
                .iter()
                .find(|packet| {
                    packet.first() == Some(&71)
                        && packet.get(1) == Some(&0x9d)
                        && packet.get(3..19) == Some(uuid.as_bytes().as_slice())
                })
                .unwrap();
            let mut body = &packet[19..];
            zerqon_protocol::codec::get_string(&mut body, 16).unwrap();
            assert_eq!(get_varint(&mut body).unwrap(), 0);
            get_varint(&mut body).unwrap()
        }
        let players = Players::new(2, 64.0, 159, Arc::new(EntityIds::default()));
        let (one, mut first, _) = connect(&players, 1, "One");
        one.initialize_game_mode(GameMode::Survival).unwrap();
        one.activate().unwrap();
        assert!(one.initialize_game_mode(GameMode::Creative).is_err());
        players.tracking_tick().unwrap();
        assert_eq!(mode(&drain(&mut first), Uuid::from_u128(1)), 0);
        let (two, mut second, _) = connect(&players, 2, "Two");
        two.activate().unwrap();
        players.tracking_tick().unwrap();
        let observed = drain(&mut second);
        assert_eq!(mode(&observed, Uuid::from_u128(1)), 0);
        assert_eq!(mode(&observed, Uuid::from_u128(2)), 1);
    }

    #[test]
    fn movement_burst_does_not_fill_an_observers_queue() {
        let players = Players::new(2, 64.0, 159, Arc::new(EntityIds::default()));
        let (one, mut rx, closed) = connect(&players, 1, "One");
        let (two, mut rx2, _) = connect(&players, 2, "Two");
        one.activate().unwrap();
        two.activate().unwrap();
        players.tracking_tick().unwrap();
        drain(&mut rx);
        drain(&mut rx2);
        for index in 0..1000 {
            two.move_to(Position {
                xyz: [2.0 + f64::from(index) / 1000.0, 64.0, 3.0],
                ..Position::default()
            })
            .unwrap();
        }
        assert!(
            !*closed.borrow(),
            "one sender overflowed an ordinary observer"
        );
        assert!(rx.is_empty(), "movement must wait for the tracking tick");
        assert_eq!(
            players
                .query_snapshot(Uuid::from_u128(2))
                .unwrap()
                .unwrap()
                .position
                .xyz,
            [2.999, 64.0, 3.0]
        );
        players.tracking_tick().unwrap();
        assert_eq!(rx.len(), 1);
        assert_eq!(
            drain(&mut rx),
            [
                Bytes::from(entity_packets::position_sync(
                    two.id(),
                    [2.999, 64.0, 3.0],
                    0.0,
                    0.0,
                    true
                )),
                Bytes::from(entity_packets::head_rotation(two.id(), 0.0))
            ]
        );
        players.tracking_tick().unwrap();
        assert!(rx.is_empty());
    }

    #[test]
    fn latency_burst_does_not_fill_an_observers_queue() {
        let players = Players::new(2, 64.0, 159, Arc::new(EntityIds::default()));
        let (one, mut rx, closed) = connect(&players, 1, "One");
        one.activate().unwrap();
        players.tracking_tick().unwrap();
        drain(&mut rx);
        for latency in 0..1000 {
            one.latency(latency).unwrap();
        }
        assert!(!*closed.borrow());
        assert!(rx.is_empty());
        players.tracking_tick().unwrap();
        let packets = drain(&mut rx);
        assert_eq!(packets.len(), 1);
        assert_eq!(&packets[0][..3], &[71, 0x10, 1]);
        assert_eq!(get_varint(&mut &packets[0][19..]).unwrap(), 999);
    }

    #[test]
    fn simultaneous_movement_uses_both_old_and_current_observer_positions() {
        let players = Players::new(2, 16.0, 159, Arc::new(EntityIds::default()));
        let (one, mut rx, _) = connect(&players, 1, "One");
        let (two, mut rx2, _) = connect(&players, 2, "Two");
        one.activate().unwrap();
        two.activate().unwrap();
        players.tracking_tick().unwrap();
        drain(&mut rx);
        drain(&mut rx2);
        for player in [&one, &two] {
            player
                .move_to(Position {
                    xyz: [100.0, 64.0, 0.0],
                    ..Position::default()
                })
                .unwrap();
        }
        players.tracking_tick().unwrap();
        for (receiver, expected) in [(&mut rx, two.id()), (&mut rx2, one.id())] {
            let packets = drain(receiver);
            assert_eq!(
                packets.iter().map(|packet| packet[0]).collect::<Vec<_>>(),
                [35, 85]
            );
            assert_eq!(get_varint(&mut &packets[0][1..]).unwrap(), expected);
        }
        two.move_to(Position::default()).unwrap();
        two.move_to(Position {
            xyz: [100.0, 64.0, 0.0],
            ..Position::default()
        })
        .unwrap();
        players.tracking_tick().unwrap();
        assert!(
            rx.is_empty() && rx2.is_empty(),
            "intermediate visibility must not flicker"
        );
    }

    #[test]
    fn activation_movement_and_leave_before_a_tick_do_not_publish_ghost_entities() {
        let players = Players::new(3, 16.0, 159, Arc::new(EntityIds::default()));
        let (one, mut rx, _) = connect(&players, 1, "One");
        one.activate().unwrap();
        players.tracking_tick().unwrap();
        drain(&mut rx);
        let (short, _rx2, _) = connect(&players, 2, "Short");
        short.activate().unwrap();
        drop(short);
        let (new, mut rx3, _) = connect(&players, 3, "New");
        let position = Position {
            xyz: [-8.5, -60.0, 3.5],
            yaw: 90.0,
            ..Position::default()
        };
        new.move_to(position).unwrap();
        new.activate().unwrap();
        one.move_to(Position {
            xyz: [-8.5, -60.0, 4.5],
            ..Position::default()
        })
        .unwrap();
        players.tracking_tick().unwrap();
        let packets = drain(&mut rx);
        assert_eq!(
            packets.iter().map(|packet| packet[0]).collect::<Vec<_>>(),
            [71, 1, 101]
        );
        assert_eq!(
            packets[1],
            entity_packets::add_entity(new.id(), Uuid::from_u128(3), 159, position.xyz, 90.0, 0.0)
        );
        let own = drain(&mut rx3);
        assert_eq!(own.iter().filter(|p| p[0] == 1).count(), 1);
        assert!(!own.iter().any(|p| matches!(p[0], 35 | 70 | 78)));
    }

    #[test]
    fn reconnect_removes_the_old_generation_before_publishing_the_new_one() {
        let players = Players::new(2, 16.0, 159, Arc::new(EntityIds::default()));
        let (one, mut rx, _) = connect(&players, 1, "One");
        let (two, mut rx2, _) = connect(&players, 2, "Two");
        one.activate().unwrap();
        two.activate().unwrap();
        players.tracking_tick().unwrap();
        drain(&mut rx);
        drain(&mut rx2);
        let stale = PlayerHandle {
            players: Arc::clone(&players),
            uuid: Uuid::from_u128(2),
            id: two.id(),
        };
        let old_id = two.id();
        drop(two);
        let (new, mut new_rx, _) = connect(&players, 2, "Two");
        new.activate().unwrap();
        let equipment = Bytes::from(vec![104, new.id() as u8, 0x80, 0, 1, 0]);
        players
            .set_equipment(Uuid::from_u128(2), new.id(), equipment.clone())
            .unwrap();
        assert_eq!(stale.activate(), Err(PlayerError::Unavailable));
        assert_eq!(stale.latency(500), Err(PlayerError::Unavailable));
        assert_eq!(
            stale.move_to(Position::default()),
            Err(PlayerError::Unavailable)
        );
        drop(stale);
        assert_eq!(
            players
                .query_snapshot(Uuid::from_u128(2))
                .unwrap()
                .unwrap()
                .id,
            new.id()
        );
        players.tracking_tick().unwrap();
        let packets = drain(&mut rx);
        assert_eq!(
            packets.iter().map(|p| p[0]).collect::<Vec<_>>(),
            [78, 70, 71, 1, 101, 104]
        );
        assert_eq!(packets[0], entity_packets::remove_entities(&[old_id]));
        assert_eq!(
            packets[1],
            entity_packets::player_info_remove(&[Uuid::from_u128(2)])
        );
        assert_eq!(get_varint(&mut &packets[3][1..]).unwrap(), new.id());
        assert_eq!(packets[5], equipment);
        assert!(!drain(&mut new_rx).iter().any(|p| matches!(p[0], 70 | 78)));
    }

    #[test]
    fn undelivered_tracking_batches_keep_reconnect_transitions_in_fifo_order() {
        let players = Players::new(2, 16.0, 159, Arc::new(EntityIds::default()));
        let (one, mut rx, closed) = connect(&players, 1, "One");
        let (two, _rx2, _) = connect(&players, 2, "Two");
        one.activate().unwrap();
        two.activate().unwrap();
        players.tracking_tick().unwrap();
        let old_id = two.id();
        drop(two);
        let (new, _rx3, _) = connect(&players, 2, "Two");
        new.activate().unwrap();
        players.tracking_tick().unwrap();
        assert_eq!(rx.len(), 2);
        assert!(!*closed.borrow());
        let initial = rx.try_recv().unwrap();
        let update = rx.try_recv().unwrap();
        assert!(initial.inventory_revision().is_none() && update.inventory_revision().is_none());
        let initial_spawn = initial.iter().find(|p| p[0] == 1).unwrap();
        assert_eq!(get_varint(&mut &initial_spawn[1..]).unwrap(), old_id);
        assert_eq!(update[0], entity_packets::remove_entities(&[old_id]));
        let next_spawn = update.iter().find(|p| p[0] == 1).unwrap();
        assert_eq!(get_varint(&mut &next_spawn[1..]).unwrap(), new.id());
    }

    #[test]
    fn hidden_equipment_updates_coalesce_and_reentry_keeps_tab_membership() {
        let players = Players::new(2, 16.0, 159, Arc::new(EntityIds::default()));
        let (one, mut rx, _) = connect(&players, 1, "One");
        let (two, mut rx2, _) = connect(&players, 2, "Two");
        one.activate().unwrap();
        two.activate().unwrap();
        players.tracking_tick().unwrap();
        drain(&mut rx);
        drain(&mut rx2);
        two.move_to(Position {
            xyz: [100.0, 64.0, 0.0],
            ..Position::default()
        })
        .unwrap();
        players.tracking_tick().unwrap();
        assert_eq!(
            drain(&mut rx),
            [Bytes::from(entity_packets::remove_entities(&[two.id()]))]
        );
        drain(&mut rx2);
        let mut latest = Bytes::new();
        for count in 1..=64 {
            latest = vec![104, two.id() as u8, 0x80, count, 1, 0, 0].into();
            players
                .set_equipment(Uuid::from_u128(2), two.id(), latest.clone())
                .unwrap();
        }
        players.tracking_tick().unwrap();
        assert!(rx.is_empty());
        two.move_to(Position::default()).unwrap();
        players.tracking_tick().unwrap();
        let packets = drain(&mut rx);
        assert_eq!(
            packets.iter().map(|p| p[0]).collect::<Vec<_>>(),
            [1, 101, 104]
        );
        assert_eq!(packets[2], latest);
    }

    #[test]
    fn tracking_preserves_reliable_inventory_and_block_batches_in_fifo_order() {
        let players = Players::new(2, 64.0, 159, Arc::new(EntityIds::default()));
        let (one, mut rx, _) = connect(&players, 1, "One");
        let (two, mut rx2, _) = connect(&players, 2, "Two");
        one.activate().unwrap();
        two.activate().unwrap();
        players.tracking_tick().unwrap();
        drain(&mut rx);
        drain(&mut rx2);
        let revision = InventoryRevision {
            uuid: Uuid::from_u128(1),
            entity_id: one.id(),
            generation: 11,
        };
        let inventory =
            PacketBatch::from(vec![Bytes::from_static(&[18, 0])]).with_inventory_revision(revision);
        let acknowledgement = PacketBatch::from(vec![Bytes::from_static(&[4, 17])]);
        players
            .dispatch_batches(vec![(revision.uuid, one.id(), inventory.clone())])
            .unwrap();
        for index in 0..1000 {
            two.move_to(Position {
                xyz: [2.0 + f64::from(index) / 1000.0, 64.0, 3.0],
                ..Position::default()
            })
            .unwrap();
        }
        players
            .dispatch_batches(vec![(revision.uuid, one.id(), acknowledgement.clone())])
            .unwrap();
        players.tracking_tick().unwrap();
        assert_eq!(rx.try_recv().unwrap(), inventory);
        assert_eq!(rx.try_recv().unwrap(), acknowledgement);
        let tracking = rx.try_recv().unwrap();
        assert!(tracking.inventory_revision().is_none());
        assert_eq!(tracking.iter().map(|p| p[0]).collect::<Vec<_>>(), [35, 85]);
        assert!(rx.is_empty());
    }

    #[test]
    fn a_genuinely_slow_observer_still_disconnects_without_affecting_other_observers() {
        let players = Players::new(3, 64.0, 159, Arc::new(EntityIds::default()));
        let (one, mut rx, slow_closed) = connect(&players, 1, "Slow");
        let (two, mut rx2, fast_closed) = connect(&players, 2, "Fast");
        let (three, mut rx3, _) = connect(&players, 3, "Mover");
        for player in [&one, &two, &three] {
            player.activate().unwrap();
        }
        players.tracking_tick().unwrap();
        drain(&mut rx);
        drain(&mut rx2);
        drain(&mut rx3);
        for index in 0..17 {
            three
                .move_to(Position {
                    xyz: [2.0 + f64::from(index) / 100.0, 64.0, 3.0],
                    ..Position::default()
                })
                .unwrap();
            players.tracking_tick().unwrap();
            assert_eq!(rx2.len(), 1);
            drain(&mut rx2);
            drain(&mut rx3);
        }
        assert!(*slow_closed.borrow());
        assert!(!*fast_closed.borrow());
        assert_eq!(rx.len(), 16);
        assert_eq!(
            players.count(),
            3,
            "disconnect notification is separate from player lifetime"
        );
        drain(&mut rx);
        three.move_to(Position::default()).unwrap();
        players.tracking_tick().unwrap();
        assert!(
            rx.is_empty(),
            "a failed observer cannot reuse the global baseline"
        );
        assert!(!rx2.is_empty());
    }

    #[test]
    fn joining_sixty_four_players_waits_for_one_tracking_batch() {
        let players = Players::new(64, 64.0, 159, Arc::new(EntityIds::default()));
        let mut peers: Vec<_> = (1..=64)
            .map(|id| {
                let peer = connect(&players, id, &format!("P{id}"));
                peer.0.activate().unwrap();
                peer
            })
            .collect();
        assert!(peers.iter().all(|(_, _, closed)| !*closed.borrow()));
        assert!(peers.iter().all(|(_, receiver, _)| receiver.is_empty()));
        players.tracking_tick().unwrap();
        for (_, receiver, closed) in &mut peers {
            assert!(!*closed.borrow());
            assert_eq!(receiver.len(), 1);
            let packets = drain(receiver);
            assert_eq!(packets.iter().filter(|p| p[0] == 71).count(), 64);
            assert_eq!(packets.iter().filter(|p| p[0] == 1).count(), 63);
        }
    }

    #[test]
    #[ignore = "manual optimized-profile tracking resource measurement"]
    fn measure_sixty_four_player_tracking_resource_cost() {
        let players = Players::new(64, 64.0, 159, Arc::new(EntityIds::default()));
        let mut peers: Vec<_> = (1..=64)
            .map(|id| {
                let peer = connect(&players, id, &format!("P{id}"));
                peer.0.activate().unwrap();
                peer
            })
            .collect();
        players.tracking_tick().unwrap();
        for (_, receiver, _) in &mut peers {
            drain(receiver);
        }
        let mut times = Vec::new();
        let mut payload_bytes = 0;
        let mut references = 0;
        let mut shared_payloads = 0;
        for tick in 0..220 {
            for (index, (handle, _, _)) in peers.iter().enumerate() {
                handle
                    .move_to(Position {
                        xyz: [
                            (index % 8) as f64 * 4.0 + f64::from(tick % 2) / 10.0,
                            64.0,
                            (index / 8) as f64 * 4.0,
                        ],
                        ..Position::default()
                    })
                    .unwrap();
            }
            let start = std::time::Instant::now();
            players.tracking_tick().unwrap();
            if tick >= 20 {
                times.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            let mut batches = Vec::new();
            for (_, receiver, closed) in &mut peers {
                assert!(!*closed.borrow());
                assert_eq!(receiver.len(), 1);
                batches.push(receiver.try_recv().unwrap());
            }
            if tick == 219 {
                payload_bytes = batches
                    .iter()
                    .flat_map(|batch| batch.iter())
                    .map(Bytes::len)
                    .sum::<usize>();
                references = batches.iter().map(|batch| batch.len()).sum::<usize>();
                shared_payloads = batches
                    .iter()
                    .flat_map(|batch| batch.iter())
                    .map(|packet| (packet.as_ptr(), packet.len()))
                    .collect::<std::collections::BTreeSet<_>>()
                    .len();
            }
        }
        times.sort_by(f64::total_cmp);
        assert_eq!(players.replicated.lock().unwrap().len(), 64);
        assert_eq!(references, 64 * 63 * 2);
        assert_eq!(shared_payloads, 128);
        println!(
            "players=64 measured_ticks=200 tracking_median_ms={:.6} tracking_p95_ms={:.6} retained_snapshot_entries=64 retained_record_bytes={} queued_batches_per_tick=64 packet_references={} unique_payloads={} payload_bytes_per_tick={}",
            times[100],
            times[189],
            std::mem::size_of::<ReplicatedPlayer>(),
            references,
            shared_payloads,
            payload_bytes
        );
    }

    #[test]
    fn bounded_queries_exclude_inactive_sessions_and_track_reconnect_identity() {
        let players = Players::new(4, 64.0, 159, Arc::new(EntityIds::default()));
        let uuid = Uuid::from_u128(1);
        let (first, _rx, _) = connect(&players, 1, "One");
        assert!(players.query_snapshot(uuid).unwrap().is_none());
        assert!(players.snapshots_bounded(0).unwrap().is_empty());
        first.activate().unwrap();
        let first_id = first.id();
        assert_eq!(players.query_snapshot(uuid).unwrap().unwrap().id, first_id);
        assert!(matches!(
            players.snapshots_bounded(0),
            Err(PlayerError::Capacity)
        ));
        assert_eq!(players.snapshots_bounded(1).unwrap().len(), 1);
        drop(first);
        assert!(players.query_snapshot(uuid).unwrap().is_none());
        let (second, _rx, _) = connect(&players, 1, "One");
        second.activate().unwrap();
        assert_ne!(second.id(), first_id);
        assert_eq!(
            players.query_snapshot(uuid).unwrap().unwrap().id,
            second.id()
        );
    }

    #[test]
    fn native_registry_reads_report_busy_instead_of_waiting_on_a_mutex() {
        let players = Players::new(4, 64.0, 159, Arc::new(EntityIds::default()));
        let held = players.inner.lock().unwrap();
        assert!(matches!(players.try_count(), Err(PlayerError::Unavailable)));
        assert!(matches!(
            players.query_snapshot(Uuid::from_u128(1)),
            Err(PlayerError::Unavailable)
        ));
        assert!(matches!(
            players.snapshots_bounded(4),
            Err(PlayerError::Unavailable)
        ));
        drop(held);
        assert_eq!(players.try_count().unwrap(), 0);
    }

    #[test]
    fn equipment_cache_is_applied_before_activation_and_on_visibility_reentry() {
        let players = Players::new(20, 32.0, 159, Arc::new(EntityIds::default()));
        let (first, mut first_rx, _) = connect(&players, 1, "One");
        let equipment = Bytes::from(vec![104, first.id() as u8, 0x80, 0, 1, 0]);
        players
            .set_equipment(Uuid::from_u128(1), first.id(), equipment.clone())
            .unwrap();
        first.activate().unwrap();
        players.tracking_tick().unwrap();
        drain(&mut first_rx);
        let (second, mut second_rx, _) = connect(&players, 2, "Two");
        second.activate().unwrap();
        players.tracking_tick().unwrap();
        assert!(drain(&mut second_rx).contains(&equipment));
        second
            .move_to(Position {
                xyz: [100.0, 64.0, 0.0],
                ..Position::default()
            })
            .unwrap();
        players.tracking_tick().unwrap();
        drain(&mut second_rx);
        second.move_to(Position::default()).unwrap();
        players.tracking_tick().unwrap();
        assert!(drain(&mut second_rx).contains(&equipment));
        let wrong = Bytes::from_static(&[104, 0]);
        players
            .set_equipment(Uuid::from_u128(1), first.id() + 100, wrong.clone())
            .unwrap();
        assert!(!drain(&mut second_rx).contains(&wrong));
    }

    #[test]
    fn mob_batches_use_active_snapshots_and_connection_generation() {
        let ids = Arc::new(EntityIds::default());
        ids.advance_past(100).unwrap();
        let players = Players::new(20, 64.0, 159, ids);
        let (first, mut rx, _) = connect(&players, 1, "One");
        first.activate().unwrap();
        players.tracking_tick().unwrap();
        drain(&mut rx);
        assert_eq!(first.id(), 101);
        let snapshot = players.snapshots().unwrap();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].name, "One");
        players
            .dispatch_batches(vec![(
                snapshot[0].uuid,
                first.id() + 1,
                vec![Bytes::from_static(&[1])],
            )])
            .unwrap();
        assert!(drain(&mut rx).is_empty());
        players
            .dispatch_batches(vec![(
                snapshot[0].uuid,
                first.id(),
                vec![Bytes::from_static(&[2]), Bytes::from_static(&[3])],
            )])
            .unwrap();
        assert_eq!(
            drain(&mut rx),
            [Bytes::from_static(&[2]), Bytes::from_static(&[3])]
        );
    }
    #[test]
    fn joining_players_receive_tab_entries_and_distinct_entity_ids() {
        let players = Players::new(20, 64.0, 159, Arc::new(EntityIds::default()));
        let (first, mut first_rx, _) = connect(&players, 1, "One");
        first.activate().unwrap();
        players.tracking_tick().unwrap();
        drain(&mut first_rx);
        let (second, mut second_rx, _) = connect(&players, 2, "Two");
        second.activate().unwrap();
        players.tracking_tick().unwrap();
        assert_ne!(first.id(), second.id());
        assert_eq!(players.count(), 2);
        let first_packets = drain(&mut first_rx);
        assert!(
            first_packets
                .iter()
                .any(|p| p[0] == 71 && p.windows(3).any(|s| s == b"Two"))
        );
        assert!(
            first_packets
                .iter()
                .any(|p| p[0] == 1 && get_varint(&mut &p[1..]).unwrap() == second.id())
        );
        let second_packets = drain(&mut second_rx);
        assert!(
            second_packets
                .iter()
                .any(|p| p[0] == 71 && p.windows(3).any(|s| s == b"One"))
        );
    }
    #[test]
    fn movement_and_leaving_update_other_players() {
        let players = Players::new(20, 64.0, 159, Arc::new(EntityIds::default()));
        let (first, mut first_rx, _) = connect(&players, 1, "One");
        first.activate().unwrap();
        let (second, mut second_rx, _) = connect(&players, 2, "Two");
        second.activate().unwrap();
        players.tracking_tick().unwrap();
        drain(&mut first_rx);
        drain(&mut second_rx);
        second
            .move_to(Position {
                xyz: [2.0, 64.0, 3.0],
                yaw: 90.0,
                ..Position::default()
            })
            .unwrap();
        players.tracking_tick().unwrap();
        assert!(drain(&mut first_rx).iter().any(|p| p[0] == 35));
        drop(second);
        players.tracking_tick().unwrap();
        let packets = drain(&mut first_rx);
        assert!(packets.iter().any(|p| p[0] == 70));
        assert!(packets.iter().any(|p| p[0] == 78));
        assert_eq!(players.count(), 1);
    }
    #[test]
    fn view_distance_removes_and_respawns_entities_without_removing_tab() {
        let players = Players::new(20, 16.0, 159, Arc::new(EntityIds::default()));
        let (one, mut rx, _) = connect(&players, 1, "One");
        one.activate().unwrap();
        let (two, mut rx2, _) = connect(&players, 2, "Two");
        two.activate().unwrap();
        players.tracking_tick().unwrap();
        drain(&mut rx);
        drain(&mut rx2);
        two.move_to(Position {
            xyz: [100.0, 64.0, 0.0],
            ..Position::default()
        })
        .unwrap();
        players.tracking_tick().unwrap();
        let packets = drain(&mut rx);
        assert!(packets.iter().any(|p| p[0] == 78));
        assert!(!packets.iter().any(|p| p[0] == 70));
        drain(&mut rx2);
        two.move_to(Position::default()).unwrap();
        players.tracking_tick().unwrap();
        assert!(drain(&mut rx).iter().any(|p| p[0] == 1));
        assert!(drain(&mut rx2).iter().any(|p| p[0] == 1));
    }
    #[test]
    fn duplicate_identity_and_capacity_are_rejected_without_kicking_original() {
        let players = Players::new(1, 64.0, 159, Arc::new(EntityIds::default()));
        let (one, _, closed) = connect(&players, 1, "One");
        let (sender, _) = mpsc::channel(1);
        let (stop, _) = watch::channel(false);
        assert!(matches!(
            players.reserve(
                Identity {
                    uuid: Uuid::from_u128(1),
                    name: "One".into(),
                    properties: vec![]
                },
                sender.clone(),
                stop.clone()
            ),
            Err(PlayerError::Duplicate)
        ));
        assert!(matches!(
            players.reserve(
                Identity {
                    uuid: Uuid::from_u128(2),
                    name: "Two".into(),
                    properties: vec![]
                },
                sender,
                stop
            ),
            Err(PlayerError::Capacity)
        ));
        assert!(!*closed.borrow());
        drop(one);
    }
    #[test]
    fn invalid_positions_cannot_poison_a_later_snapshot() {
        let players = Players::new(2, 64.0, 159, Arc::new(EntityIds::default()));
        let (one, _, _) = connect(&players, 1, "One");
        for coordinate in [f64::NAN, f64::INFINITY, 30_000_001.0] {
            assert_eq!(
                one.move_to(Position {
                    xyz: [coordinate, 64.0, 0.0],
                    ..Position::default()
                }),
                Err(PlayerError::Invalid)
            );
        }
    }
}
