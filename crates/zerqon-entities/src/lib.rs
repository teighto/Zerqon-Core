use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    f64::consts::FRAC_1_SQRT_2,
};
use uuid::Uuid;
mod physics;

pub const TICKS_PER_SECOND: u32 = 20;
const MAX_ENTITIES: usize = 10_000;
const BORDER: f64 = 29_999_984.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobKind {
    Pig,
    Cow,
    Zombie,
}

impl MobKind {
    pub fn dimensions(self) -> [f64; 2] {
        match self {
            Self::Pig => [f64::from(0.9_f32), f64::from(0.9_f32)],
            Self::Cow => [f64::from(0.9_f32), f64::from(1.4_f32)],
            Self::Zombie => [f64::from(0.6_f32), f64::from(1.95_f32)],
        }
    }
    pub fn maximum_health(self) -> f32 {
        if self == Self::Zombie { 20.0 } else { 10.0 }
    }
    pub fn identifier(self) -> &'static str {
        match self {
            Self::Pig => "minecraft:pig",
            Self::Cow => "minecraft:cow",
            Self::Zombie => "minecraft:zombie",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mob {
    pub id: i32,
    pub uuid: Uuid,
    pub kind: MobKind,
    #[serde(rename = "position_bits", with = "array_bits")]
    pub position: [f64; 3],
    #[serde(rename = "velocity_bits", with = "array_bits")]
    pub velocity: [f64; 3],
    #[serde(rename = "yaw_bits", with = "float_bits")]
    pub yaw: f32,
    #[serde(rename = "pitch_bits", with = "float_bits")]
    pub pitch: f32,
    #[serde(rename = "health_bits", with = "float_bits")]
    pub health: f32,
    pub on_ground: bool,
    #[serde(rename = "wander_target_bits", with = "array_bits")]
    pub wander_target: [f64; 2],
    pub wander_ticks: u16,
    pub attack_cooldown: u16,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema: u32,
    pub tick: u64,
    pub random_state: u64,
    pub entities: Vec<Mob>,
}

#[derive(Clone, Copy, Debug)]
pub struct PlayerTarget {
    pub id: i32,
    pub position: [f64; 3],
    pub attackable: bool,
}

pub trait WorldQuery {
    fn block_solid(&self, x: i32, y: i32, z: i32) -> bool;
    fn surface_y(&self, x: i32, z: i32) -> Option<i32>;
    fn loaded_at(&self, _x: i32, _z: i32) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemovalReason {
    Killed,
    Void,
    Explicit,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EntityChange {
    Spawned(Mob),
    Moved {
        id: i32,
        position: [f64; 3],
        yaw: f32,
        pitch: f32,
        on_ground: bool,
    },
    Health {
        id: i32,
        health: f32,
    },
    Removed {
        id: i32,
        uuid: Uuid,
        reason: RemovalReason,
    },
    AttackPlayer {
        attacker: i32,
        player: i32,
        damage: f32,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum EntityError {
    #[error("invalid entity input: {0}")]
    Invalid(&'static str),
    #[error("entity capacity reached")]
    Capacity,
    #[error("entity identity already exists")]
    Duplicate,
    #[error("entity would intersect a solid block or another entity")]
    Collision,
    #[error("entity not found")]
    Missing,
}

pub struct EntityWorld {
    capacity: usize,
    tick: u64,
    random_state: u64,
    entities: BTreeMap<i32, Mob>,
    identities: HashSet<Uuid>,
    buckets: BTreeMap<(i32, i32), BTreeSet<i32>>,
}
impl EntityWorld {
    pub fn new(seed: u64, capacity: usize) -> Result<Self, EntityError> {
        if !(1..=MAX_ENTITIES).contains(&capacity) {
            return Err(EntityError::Invalid("capacity must be 1..=10000"));
        }
        Ok(Self {
            capacity,
            tick: 0,
            random_state: seed,
            entities: BTreeMap::new(),
            identities: HashSet::new(),
            buckets: BTreeMap::new(),
        })
    }
    pub fn spawn(
        &mut self,
        id: i32,
        uuid: Uuid,
        kind: MobKind,
        position: [f64; 3],
        world: &impl WorldQuery,
    ) -> Result<EntityChange, EntityError> {
        if id <= 0 || !valid_position(position) {
            return Err(EntityError::Invalid("identity or position"));
        }
        if self.entities.contains_key(&id) || self.identities.contains(&uuid) {
            return Err(EntityError::Duplicate);
        }
        if self.len() == self.capacity {
            return Err(EntityError::Capacity);
        }
        if !self.spawn_space(position, kind, world) {
            return Err(EntityError::Collision);
        }
        let mob = Mob {
            id,
            uuid,
            kind,
            position,
            velocity: [0.0; 3],
            yaw: 0.0,
            pitch: 0.0,
            health: kind.maximum_health(),
            on_ground: false,
            wander_target: [position[0], position[2]],
            wander_ticks: 0,
            attack_cooldown: 0,
        };
        self.insert(mob.clone());
        Ok(EntityChange::Spawned(mob))
    }
    pub fn tick(&mut self, world: &impl WorldQuery, players: &[PlayerTarget]) -> Vec<EntityChange> {
        self.tick = self.tick.wrapping_add(1);
        let targets = player_index(players);
        let mut changes = Vec::new();
        let mut void = Vec::new();
        let mut moved_buckets = Vec::new();
        for mob in self.entities.values_mut() {
            if !world.loaded_at(
                mob.position[0].floor() as i32,
                mob.position[2].floor() as i32,
            ) {
                continue;
            }
            let old = (mob.position, mob.yaw, mob.on_ground);
            mob.attack_cooldown = mob.attack_cooldown.saturating_sub(1);
            let target = if mob.kind == MobKind::Zombie {
                nearest_player(mob.position, &targets)
            } else {
                None
            };
            let goal = if let Some(player) = target {
                [player.position[0], player.position[2]]
            } else {
                if mob.wander_ticks == 0 || distance_xz(mob.position, mob.wander_target) < 0.2 {
                    let direction = (random(&mut self.random_state) % 8) as usize;
                    let distance = 2.0 + (random(&mut self.random_state) % 7) as f64;
                    let [x, z] = DIRECTIONS[direction];
                    mob.wander_target = [
                        (mob.position[0] + x * distance).clamp(-BORDER, BORDER),
                        (mob.position[2] + z * distance).clamp(-BORDER, BORDER),
                    ];
                    mob.wander_ticks = 60 + (random(&mut self.random_state) % 60) as u16;
                }
                mob.wander_ticks = mob.wander_ticks.saturating_sub(1);
                mob.wander_target
            };
            let dx = goal[0] - mob.position[0];
            let dz = goal[1] - mob.position[2];
            let length = dx.hypot(dz);
            let speed = if mob.kind == MobKind::Zombie {
                0.115
            } else {
                0.075
            };
            let move_horizontal = length > if target.is_some() { 1.3 } else { 0.1 };
            mob.velocity[0] = if move_horizontal {
                dx / length * speed
            } else {
                0.0
            };
            mob.velocity[2] = if move_horizontal {
                dz / length * speed
            } else {
                0.0
            };
            if move_horizontal {
                mob.yaw = (-dx).atan2(dz).to_degrees() as f32;
            }
            mob.velocity[1] = (mob.velocity[1] - 0.08).max(-3.92);
            if !physics::overlaps(mob.position, mob.kind, world) {
                let movement = mob.velocity;
                let (next, hit) = physics::move_body(mob.position, mob.kind, movement, world);
                mob.position = next;
                mob.on_ground = movement[1] < 0.0 && hit[1];
                for (axis, collided) in hit.into_iter().enumerate() {
                    if collided {
                        mob.velocity[axis] = 0.0;
                    }
                }
                mob.velocity[1] *= 0.98;
                if hit[0] || hit[2] {
                    mob.wander_ticks = 0;
                }
            } else {
                mob.velocity = [0.0; 3];
                mob.on_ground = false;
            }
            if bucket(old.0) != bucket(mob.position) {
                moved_buckets.push((mob.id, bucket(old.0), bucket(mob.position)));
            }
            if !valid_position(mob.position) || mob.position[1] < -80.0 {
                void.push(mob.id);
                continue;
            }
            if let Some(player) = target {
                let mut eye = mob.position;
                eye[1] += mob.kind.dimensions()[1] * 0.85;
                let mut target_eye = player.position;
                target_eye[1] += 1.62;
                if mob.attack_cooldown == 0
                    && distance_squared(mob.position, player.position) <= 3.24
                    && line_clear(eye, target_eye, world)
                {
                    changes.push(EntityChange::AttackPlayer {
                        attacker: mob.id,
                        player: player.id,
                        damage: 3.0,
                    });
                    mob.attack_cooldown = 20;
                }
            }
            if old != (mob.position, mob.yaw, mob.on_ground) {
                changes.push(EntityChange::Moved {
                    id: mob.id,
                    position: mob.position,
                    yaw: mob.yaw,
                    pitch: mob.pitch,
                    on_ground: mob.on_ground,
                });
            }
        }
        for (id, old, new) in moved_buckets {
            self.unindex(id, old);
            self.buckets.entry(new).or_default().insert(id);
        }
        for id in void {
            if let Ok(change) = self.remove_with_reason(id, RemovalReason::Void) {
                changes.push(change);
            }
        }
        changes
    }
    pub fn damage(&mut self, id: i32, amount: f32) -> Result<Vec<EntityChange>, EntityError> {
        if !amount.is_finite() || amount <= 0.0 {
            return Err(EntityError::Invalid("damage must be finite and positive"));
        }
        let mob = self.entities.get_mut(&id).ok_or(EntityError::Missing)?;
        mob.health = (mob.health - amount).max(0.0);
        let mut changes = vec![EntityChange::Health {
            id,
            health: mob.health,
        }];
        if mob.health == 0.0 {
            changes.push(self.remove_with_reason(id, RemovalReason::Killed)?);
        }
        Ok(changes)
    }
    pub fn reachable_from(&self, id: i32, position: [f64; 3], world: &impl WorldQuery) -> bool {
        let Some(mob) = self.get(id) else {
            return false;
        };
        if !valid_position(position) || distance_squared(position, mob.position) > 9.0 {
            return false;
        }
        let mut eye = position;
        eye[1] += 1.62;
        let mut target = mob.position;
        target[1] += mob.kind.dimensions()[1] * 0.5;
        line_clear(eye, target, world)
    }
    pub fn remove(&mut self, id: i32) -> Result<EntityChange, EntityError> {
        self.remove_with_reason(id, RemovalReason::Explicit)
    }
    pub fn get(&self, id: i32) -> Option<&Mob> {
        self.entities.get(&id)
    }
    pub fn entities(&self) -> impl Iterator<Item = &Mob> {
        self.entities.values()
    }
    pub fn len(&self) -> usize {
        self.entities.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }
    pub fn nearby(&self, position: [f64; 3], radius: f64) -> Result<Vec<&Mob>, EntityError> {
        if !valid_position(position) || !radius.is_finite() || !(0.0..=128.0).contains(&radius) {
            return Err(EntityError::Invalid("query position or radius"));
        }
        let min = bucket([position[0] - radius, position[1], position[2] - radius]);
        let max = bucket([position[0] + radius, position[1], position[2] + radius]);
        let mut found = Vec::new();
        for x in min.0..=max.0 {
            for z in min.1..=max.1 {
                if let Some(ids) = self.buckets.get(&(x, z)) {
                    for id in ids {
                        if let Some(mob) = self.entities.get(id)
                            && distance_squared(position, mob.position) <= radius * radius
                        {
                            found.push(mob);
                        }
                    }
                }
            }
        }
        found.sort_by_key(|mob| mob.id);
        Ok(found)
    }
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            schema: 1,
            tick: self.tick,
            random_state: self.random_state,
            entities: self.entities.values().cloned().collect(),
        }
    }
    pub fn restore(snapshot: Snapshot, capacity: usize) -> Result<Self, EntityError> {
        if snapshot.schema != 1 {
            return Err(EntityError::Invalid("unsupported entity snapshot schema"));
        }
        let mut world = Self::new(snapshot.random_state, capacity)?;
        if snapshot.entities.len() > capacity {
            return Err(EntityError::Capacity);
        }
        for mob in snapshot.entities {
            if mob.id <= 0
                || !valid_position(mob.position)
                || !mob.velocity.iter().all(|v| v.is_finite() && v.abs() <= 4.0)
                || !mob.yaw.is_finite()
                || !mob.pitch.is_finite()
                || !mob.health.is_finite()
                || mob.health <= 0.0
                || mob.health > mob.kind.maximum_health()
                || !mob
                    .wander_target
                    .iter()
                    .all(|v| v.is_finite() && v.abs() <= BORDER)
                || mob.wander_ticks > 120
                || mob.attack_cooldown > 20
            {
                return Err(EntityError::Invalid("invalid entity snapshot state"));
            }
            if world.entities.contains_key(&mob.id) || world.identities.contains(&mob.uuid) {
                return Err(EntityError::Duplicate);
            }
            world.insert(mob);
        }
        world.tick = snapshot.tick;
        Ok(world)
    }
    pub fn try_natural_spawn(
        &mut self,
        world: &impl WorldQuery,
        players: &[PlayerTarget],
        hostile_allowed: bool,
        mut identity: impl FnMut() -> (i32, Uuid),
    ) -> Result<Option<EntityChange>, EntityError> {
        if self.len() == self.capacity {
            return Ok(None);
        }
        let mut players: Vec<_> = players
            .iter()
            .filter(|player| player.id > 0 && valid_position(player.position))
            .collect();
        players.sort_by_key(|player| player.id);
        if players.is_empty() {
            return Ok(None);
        }
        for _ in 0..8 {
            let player = players[(random(&mut self.random_state) % players.len() as u64) as usize];
            let x = player.position[0].floor() as i32
                + (random(&mut self.random_state) % 97) as i32
                - 48;
            let z = player.position[2].floor() as i32
                + (random(&mut self.random_state) % 97) as i32
                - 48;
            let Some(y) = world.surface_y(x, z) else {
                continue;
            };
            let position = [f64::from(x) + 0.5, f64::from(y), f64::from(z) + 0.5];
            if !valid_position(position)
                || players
                    .iter()
                    .any(|player| distance_squared(position, player.position) < 24.0 * 24.0)
                || !world.block_solid(x, y.saturating_sub(1), z)
            {
                continue;
            }
            let kind = match random(&mut self.random_state) % if hostile_allowed { 3 } else { 2 } {
                0 => MobKind::Pig,
                1 => MobKind::Cow,
                _ => MobKind::Zombie,
            };
            if !self.spawn_space(position, kind, world) {
                continue;
            }
            let (id, uuid) = identity();
            return self.spawn(id, uuid, kind, position, world).map(Some);
        }
        Ok(None)
    }
    fn spawn_space(&self, position: [f64; 3], kind: MobKind, world: &impl WorldQuery) -> bool {
        if physics::overlaps(position, kind, world) {
            return false;
        }
        self.nearby(position, 4.0).is_ok_and(|near| {
            near.into_iter()
                .all(|other| !physics::bodies_overlap(position, kind, other.position, other.kind))
        })
    }
    fn insert(&mut self, mob: Mob) {
        self.buckets
            .entry(bucket(mob.position))
            .or_default()
            .insert(mob.id);
        self.identities.insert(mob.uuid);
        self.entities.insert(mob.id, mob);
    }
    fn unindex(&mut self, id: i32, at: (i32, i32)) {
        if let Some(ids) = self.buckets.get_mut(&at) {
            ids.remove(&id);
            if ids.is_empty() {
                self.buckets.remove(&at);
            }
        }
    }
    fn remove_with_reason(
        &mut self,
        id: i32,
        reason: RemovalReason,
    ) -> Result<EntityChange, EntityError> {
        let mob = self.entities.remove(&id).ok_or(EntityError::Missing)?;
        self.identities.remove(&mob.uuid);
        self.unindex(id, bucket(mob.position));
        Ok(EntityChange::Removed {
            id,
            uuid: mob.uuid,
            reason,
        })
    }
}

const DIRECTIONS: [[f64; 2]; 8] = [
    [1.0, 0.0],
    [FRAC_1_SQRT_2, FRAC_1_SQRT_2],
    [0.0, 1.0],
    [-FRAC_1_SQRT_2, FRAC_1_SQRT_2],
    [-1.0, 0.0],
    [-FRAC_1_SQRT_2, -FRAC_1_SQRT_2],
    [0.0, -1.0],
    [FRAC_1_SQRT_2, -FRAC_1_SQRT_2],
];

fn random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e3779b97f4a7c15);
    let mut value = *state;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}
fn valid_position(position: [f64; 3]) -> bool {
    position.iter().all(|value| value.is_finite())
        && position[0].abs() <= BORDER
        && position[2].abs() <= BORDER
        && (-2048.0..=2048.0).contains(&position[1])
}
fn bucket(position: [f64; 3]) -> (i32, i32) {
    (
        (position[0].floor() as i32).div_euclid(16),
        (position[2].floor() as i32).div_euclid(16),
    )
}
fn distance_squared(first: [f64; 3], second: [f64; 3]) -> f64 {
    first
        .into_iter()
        .zip(second)
        .map(|(a, b)| (a - b) * (a - b))
        .sum()
}
fn distance_xz(position: [f64; 3], target: [f64; 2]) -> f64 {
    (position[0] - target[0]).hypot(position[2] - target[1])
}
fn player_index(players: &[PlayerTarget]) -> BTreeMap<(i32, i32), Vec<&PlayerTarget>> {
    let mut result = BTreeMap::new();
    for player in players {
        if player.id > 0 && player.attackable && valid_position(player.position) {
            result
                .entry(bucket(player.position))
                .or_insert_with(Vec::new)
                .push(player);
        }
    }
    result
}
fn nearest_player<'a>(
    position: [f64; 3],
    players: &BTreeMap<(i32, i32), Vec<&'a PlayerTarget>>,
) -> Option<&'a PlayerTarget> {
    let at = bucket(position);
    let mut nearest = None;
    let mut best = 16.0 * 16.0;
    for x in at.0 - 1..=at.0 + 1 {
        for z in at.1 - 1..=at.1 + 1 {
            if let Some(candidates) = players.get(&(x, z)) {
                for &player in candidates {
                    let distance = distance_squared(position, player.position);
                    if distance < best
                        || (distance == best
                            && nearest
                                .is_none_or(|previous: &PlayerTarget| player.id < previous.id))
                    {
                        best = distance;
                        nearest = Some(player);
                    }
                }
            }
        }
    }
    nearest
}
fn line_clear(first: [f64; 3], second: [f64; 3], world: &impl WorldQuery) -> bool {
    let min: [i32; 3] = std::array::from_fn(|axis| first[axis].min(second[axis]).floor() as i32);
    let max: [i32; 3] = std::array::from_fn(|axis| first[axis].max(second[axis]).floor() as i32);
    for x in min[0]..=max[0] {
        for y in min[1]..=max[1] {
            for z in min[2]..=max[2] {
                if !world.block_solid(x, y, z) {
                    continue;
                }
                let block = [f64::from(x), f64::from(y), f64::from(z)];
                let mut enter = 0.0_f64;
                let mut leave = 1.0_f64;
                for axis in 0..3 {
                    let movement = second[axis] - first[axis];
                    if movement == 0.0 {
                        if first[axis] < block[axis] || first[axis] > block[axis] + 1.0 {
                            leave = -1.0;
                            break;
                        }
                    } else {
                        let first_hit = (block[axis] - first[axis]) / movement;
                        let last_hit = (block[axis] + 1.0 - first[axis]) / movement;
                        enter = enter.max(first_hit.min(last_hit));
                        leave = leave.min(first_hit.max(last_hit));
                    }
                }
                if enter < leave {
                    return false;
                }
            }
        }
    }
    true
}

mod array_bits {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<S: Serializer, const N: usize>(
        value: &[f64; N],
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        [u64; N]: Serialize,
    {
        value.map(f64::to_bits).serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        deserializer: D,
    ) -> Result<[f64; N], D::Error>
    where
        [u64; N]: Deserialize<'de>,
    {
        Ok(<[u64; N]>::deserialize(deserializer)?.map(f64::from_bits))
    }
}
mod float_bits {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<S: Serializer>(value: &f32, serializer: S) -> Result<S::Ok, S::Error> {
        value.to_bits().serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f32, D::Error> {
        Ok(f32::from_bits(u32::deserialize(deserializer)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn line_of_sight_does_not_skip_thin_corner_intersections() {
        struct Corner;
        impl WorldQuery for Corner {
            fn block_solid(&self, x: i32, y: i32, z: i32) -> bool {
                [x, y, z] == [0, 0, 1]
            }
            fn surface_y(&self, _x: i32, _z: i32) -> Option<i32> {
                None
            }
        }
        assert!(!line_clear([0.4, 0.5, 0.4], [1.6, 0.5, 1.601], &Corner));
    }
}
