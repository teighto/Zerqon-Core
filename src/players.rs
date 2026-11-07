use crate::entity_packets;
use bytes::Bytes;
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI32, Ordering},
};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

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
}
pub struct Players {
    inner: Mutex<BTreeMap<Uuid, Player>>,
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

impl Players {
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
        player.equipment = Some(packet.clone());
        let position = player.position;
        if !player.active {
            return Ok(());
        }
        for other in inner
            .values()
            .filter(|other| other.active && other.identity.uuid != uuid)
        {
            if self.visible(position, other.position) {
                enqueue(other, vec![packet.clone()]);
            }
        }
        Ok(())
    }
    pub fn snapshots(&self) -> Result<Vec<PlayerSnapshot>, PlayerError> {
        let inner = self.inner.lock().map_err(|_| PlayerError::Unavailable)?;
        Ok(inner
            .values()
            .filter(|player| player.active)
            .map(|player| PlayerSnapshot {
                uuid: player.identity.uuid,
                id: player.id,
                name: player.identity.name.clone(),
                position: player.position,
            })
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
            },
        );
        Ok(PlayerHandle {
            players: Arc::clone(self),
            uuid,
            id,
        })
    }
    pub fn count(&self) -> usize {
        self.try_count().unwrap_or(0)
    }
    pub fn try_count(&self) -> Result<usize, PlayerError> {
        self.inner
            .lock()
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
    pub fn id(&self) -> i32 {
        self.id
    }
    pub fn activate(&self) -> Result<(), PlayerError> {
        let mut inner = self
            .players
            .inner
            .lock()
            .map_err(|_| PlayerError::Unavailable)?;
        let player = inner.get_mut(&self.uuid).ok_or(PlayerError::Unavailable)?;
        if player.active {
            return Ok(());
        }
        player.active = true;
        let player = inner.get(&self.uuid).ok_or(PlayerError::Unavailable)?;
        let profile = profile_packet(&player.identity);
        let mut own = vec![profile.clone()];
        for other in inner
            .values()
            .filter(|other| other.active && other.identity.uuid != self.uuid)
        {
            let mut remote = vec![profile.clone()];
            own.push(profile_packet(&other.identity));
            if self.players.visible(player.position, other.position) {
                remote.extend(spawn_packets(player, self.players.player_type));
                own.extend(spawn_packets(other, self.players.player_type));
            }
            enqueue(other, remote);
        }
        enqueue(player, own);
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
        let player = inner.get_mut(&self.uuid).ok_or(PlayerError::Unavailable)?;
        let previous = player.position;
        player.position = position;
        if !player.active {
            return Ok(());
        }
        let player = inner.get(&self.uuid).ok_or(PlayerError::Unavailable)?;
        let movement = Bytes::from(entity_packets::position_sync(
            player.id,
            position.xyz,
            position.yaw,
            position.pitch,
            position.on_ground,
        ));
        let rotation = Bytes::from(entity_packets::head_rotation(player.id, position.yaw));
        let mut own = Vec::new();
        for other in inner
            .values()
            .filter(|other| other.active && other.identity.uuid != self.uuid)
        {
            let was_visible = self.players.visible(previous, other.position);
            let visible = self.players.visible(position, other.position);
            match (was_visible, visible) {
                (false, true) => {
                    enqueue(other, spawn_packets(player, self.players.player_type));
                    own.extend(spawn_packets(other, self.players.player_type));
                }
                (true, false) => {
                    enqueue(
                        other,
                        vec![entity_packets::remove_entities(&[player.id]).into()],
                    );
                    own.push(entity_packets::remove_entities(&[other.id]).into());
                }
                (true, true) => enqueue(other, vec![movement.clone(), rotation.clone()]),
                (false, false) => {}
            }
        }
        if !own.is_empty() {
            enqueue(player, own);
        }
        Ok(())
    }
    pub fn latency(&self, milliseconds: i32) -> Result<(), PlayerError> {
        let inner = self
            .players
            .inner
            .lock()
            .map_err(|_| PlayerError::Unavailable)?;
        if !inner.get(&self.uuid).is_some_and(|player| player.active) {
            return Err(PlayerError::Unavailable);
        }
        let mut packet = vec![71, 0x10, 1];
        packet.extend_from_slice(self.uuid.as_bytes());
        zerqon_protocol::codec::put_varint(milliseconds.max(0), &mut packet);
        let packet = Bytes::from(packet);
        for player in inner.values().filter(|player| player.active) {
            enqueue(player, vec![packet.clone()]);
        }
        Ok(())
    }
}

impl Drop for PlayerHandle {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.players.inner.lock()
            && let Some(player) = inner.remove(&self.uuid).filter(|player| player.active)
        {
            let tab = Bytes::from(entity_packets::player_info_remove(&[self.uuid]));
            let removed = Bytes::from(entity_packets::remove_entities(&[player.id]));
            for other in inner.values().filter(|other| other.active) {
                let mut batch = vec![tab.clone()];
                if self.players.visible(player.position, other.position) {
                    batch.push(removed.clone());
                }
                enqueue(other, batch);
            }
        }
    }
}
fn enqueue(player: &Player, batch: impl Into<PacketBatch>) {
    if player.outbound.try_send(batch.into()).is_err() {
        let _ = player.disconnect.send(true);
    }
}
fn profile_packet(identity: &Identity) -> Bytes {
    if identity.properties.is_empty() {
        return entity_packets::player_info_add(identity.uuid, &identity.name, true).into();
    }
    let properties: Vec<_> = identity
        .properties
        .iter()
        .map(|(name, value, signature)| (name.as_str(), value.as_str(), signature.as_deref()))
        .collect();
    entity_packets::player_info_add_properties(identity.uuid, &identity.name, true, &properties)
        .into()
}
fn spawn_packets(player: &Player, kind: i32) -> PacketBatch {
    let mut packets = vec![
        entity_packets::add_entity(
            player.id,
            player.identity.uuid,
            kind,
            player.position.xyz,
            player.position.yaw,
            player.position.pitch,
        )
        .into(),
        entity_packets::player_metadata(player.id, 127).into(),
    ];
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
    fn equipment_cache_is_applied_before_activation_and_on_visibility_reentry() {
        let players = Players::new(20, 32.0, 159, Arc::new(EntityIds::default()));
        let (first, mut first_rx, _) = connect(&players, 1, "One");
        let equipment = Bytes::from(vec![104, first.id() as u8, 0x80, 0, 1, 0]);
        players
            .set_equipment(Uuid::from_u128(1), first.id(), equipment.clone())
            .unwrap();
        first.activate().unwrap();
        drain(&mut first_rx);
        let (second, mut second_rx, _) = connect(&players, 2, "Two");
        second.activate().unwrap();
        assert!(drain(&mut second_rx).contains(&equipment));
        second
            .move_to(Position {
                xyz: [100.0, 64.0, 0.0],
                ..Position::default()
            })
            .unwrap();
        drain(&mut second_rx);
        second.move_to(Position::default()).unwrap();
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
        drain(&mut first_rx);
        let (second, mut second_rx, _) = connect(&players, 2, "Two");
        second.activate().unwrap();
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
        drain(&mut first_rx);
        drain(&mut second_rx);
        second
            .move_to(Position {
                xyz: [2.0, 64.0, 3.0],
                yaw: 90.0,
                ..Position::default()
            })
            .unwrap();
        assert!(drain(&mut first_rx).iter().any(|p| p[0] == 35));
        drop(second);
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
        drain(&mut rx);
        drain(&mut rx2);
        two.move_to(Position {
            xyz: [100.0, 64.0, 0.0],
            ..Position::default()
        })
        .unwrap();
        let packets = drain(&mut rx);
        assert!(packets.iter().any(|p| p[0] == 78));
        assert!(!packets.iter().any(|p| p[0] == 70));
        drain(&mut rx2);
        two.move_to(Position::default()).unwrap();
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
