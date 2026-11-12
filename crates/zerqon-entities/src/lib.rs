use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    f64::consts::FRAC_1_SQRT_2,
};
use uuid::Uuid;
mod navigation;
mod physics;
pub use physics::{
    Movement, PhysicsError, body_bounds, clip_motion, clip_shapes, move_mob, scaled_bounds,
};

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
    pub fn step_height(self) -> f32 {
        0.6
    }
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
    #[serde(default)]
    pub age: i32,
    #[serde(default)]
    pub forced_age: i32,
    #[serde(default)]
    pub love_ticks: u16,
}

impl Mob {
    pub fn is_baby(&self) -> bool {
        self.age < 0
    }
    pub fn scale(&self) -> f64 {
        if self.is_baby() { 0.5 } else { 1.0 }
    }
    pub fn dimensions(&self) -> [f64; 2] {
        self.kind.dimensions().map(|value| value * self.scale())
    }
    pub fn can_feed(&self) -> bool {
        self.kind != MobKind::Zombie && (self.age < 0 || (self.age == 0 && self.love_ticks == 0))
    }
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
    fn water_at(&self, _x: i32, _y: i32, _z: i32) -> bool {
        false
    }
    fn block_solid(&self, x: i32, y: i32, z: i32) -> bool;
    fn block_collision(&self, x: i32, y: i32, z: i32) -> BlockCollision<'_> {
        if self.block_solid(x, y, z) {
            BlockCollision::Full
        } else {
            BlockCollision::Empty
        }
    }
    fn surface_y(&self, x: i32, z: i32) -> Option<i32>;
    fn surface_height(&self, x: i32, z: i32) -> Option<f64> {
        self.surface_y(x, z).map(f64::from)
    }
    fn loaded_at(&self, _x: i32, _z: i32) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug)]
pub enum BlockCollision<'a> {
    Empty,
    Full,
    Static(&'a [zerqon_blocks::Aabb]),
    Unavailable,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemovalReason {
    Killed,
    Void,
    Explicit,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EntityChange {
    PhysicsBlocked {
        id: i32,
        reason: PhysicsError,
    },
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
    Love {
        id: i32,
    },
    AgeChanged {
        id: i32,
        baby: bool,
    },
    GrowthFed {
        id: i32,
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
    #[error(transparent)]
    Physics(#[from] PhysicsError),
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
    physics: physics::Scratch,
    blocked: BTreeMap<i32, PhysicsError>,
    brains: BTreeMap<i32, navigation::Brain>,
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
            physics: physics::Scratch::default(),
            blocked: BTreeMap::new(),
            brains: BTreeMap::new(),
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
        if !self.spawn_space(position, kind, world)? {
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
            age: 0,
            forced_age: 0,
            love_ticks: 0,
        };
        self.insert(mob.clone());
        Ok(EntityChange::Spawned(mob))
    }
    pub fn tick(&mut self, world: &impl WorldQuery, players: &[PlayerTarget]) -> Vec<EntityChange> {
        self.tick_with_food(world, players, &[])
    }

    pub fn tick_with_food(
        &mut self,
        world: &impl WorldQuery,
        players: &[PlayerTarget],
        food: &[(MobKind, PlayerTarget)],
    ) -> Vec<EntityChange> {
        self.tick = self.tick.wrapping_add(1);
        let targets = player_index(players);
        let mut changes = Vec::new();
        let mut void = Vec::new();
        let mut moved_buckets = Vec::new();
        let mut path_budget = 8usize;
        let mut neighbors = BTreeMap::<(i32, i32), Vec<(i32, [f64; 3])>>::new();
        let mut mates = BTreeMap::<i32, (i32, [f64; 3])>::new();
        let mut parents = BTreeMap::<i32, [f64; 3]>::new();
        for mob in self
            .entities
            .values()
            .filter(|mob| mob.is_baby() || (mob.age == 0 && mob.love_ticks > 0))
        {
            let at = bucket(mob.position);
            let mut best: Option<(i32, [f64; 3], f64)> = None;
            for x in at.0 - 1..=at.0 + 1 {
                for z in at.1 - 1..=at.1 + 1 {
                    if let Some(ids) = self.buckets.get(&(x, z)) {
                        for id in ids {
                            let other = &self.entities[id];
                            if other.id == mob.id
                                || other.kind != mob.kind
                                || other.is_baby()
                                || (!mob.is_baby() && (other.age != 0 || other.love_ticks == 0))
                            {
                                continue;
                            }
                            let distance = distance_squared(mob.position, other.position);
                            if distance < 64.0
                                && best.is_none_or(|(_, _, previous)| distance < previous)
                            {
                                best = Some((other.id, other.position, distance));
                            }
                        }
                    }
                }
            }
            if let Some((id, position, _)) = best {
                if mob.is_baby() {
                    parents.insert(mob.id, position);
                } else {
                    mates.insert(mob.id, (id, position));
                }
            }
        }
        for mob in self.entities.values() {
            neighbors
                .entry(bucket(mob.position))
                .or_default()
                .push((mob.id, mob.position));
        }
        for mob in self.entities.values_mut() {
            if !world.loaded_at(
                mob.position[0].floor() as i32,
                mob.position[2].floor() as i32,
            ) {
                continue;
            }
            if mob.position[1] < -80.0 {
                void.push(mob.id);
                continue;
            }
            let old = (mob.position, mob.yaw, mob.on_ground);
            let was_baby = mob.is_baby();
            if mob.age < 0 {
                mob.age += 1;
            } else if mob.age > 0 {
                mob.age -= 1;
            }
            if was_baby && !mob.is_baby() {
                changes.push(EntityChange::AgeChanged {
                    id: mob.id,
                    baby: false,
                });
            }
            if mob.age != 0 {
                mob.love_ticks = 0;
            } else if mob.love_ticks > 0 {
                mob.love_ticks -= 1;
                if mob.love_ticks > 0 && mob.love_ticks.is_multiple_of(10) {
                    changes.push(EntityChange::Love { id: mob.id });
                }
            }
            let before = mob.clone();
            let random_before = self.random_state;
            let brain = self.brains.entry(mob.id).or_default();
            mob.attack_cooldown = mob.attack_cooldown.saturating_sub(1);
            let target = if mob.kind == MobKind::Zombie {
                nearest_player(mob.position, &targets)
            } else {
                food.iter()
                    .filter(|(kind, player)| {
                        *kind == mob.kind && distance_squared(mob.position, player.position) < 100.0
                    })
                    .min_by(|(_, a), (_, b)| {
                        distance_squared(mob.position, a.position)
                            .total_cmp(&distance_squared(mob.position, b.position))
                    })
                    .map(|(_, player)| player)
            };
            let panic = self.tick < brain.panic_until;
            let mate = mates
                .get(&mob.id)
                .filter(|_| mob.age == 0 && mob.love_ticks > 0 && !panic);
            if let Some(&(partner, _)) = mate {
                if brain.partner == Some(partner) {
                    brain.courtship = brain.courtship.saturating_add(1).min(60);
                } else {
                    brain.partner = Some(partner);
                    brain.courtship = 1;
                }
            } else {
                brain.partner = None;
                brain.courtship = 0;
            }
            let animal_goal = mate.map(|(_, position)| *position).or_else(|| {
                if target.is_none() {
                    parents.get(&mob.id).copied()
                } else {
                    None
                }
            });
            let goal = if panic {
                let dx = mob.position[0] - brain.flee_from[0];
                let dz = mob.position[2] - brain.flee_from[2];
                let length = dx.hypot(dz).max(0.1);
                [
                    mob.position[0] + dx / length * 6.0,
                    mob.position[1],
                    mob.position[2] + dz / length * 6.0,
                ]
            } else if let Some(position) = animal_goal {
                position
            } else if let Some(player) = target {
                player.position
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
                    if random(&mut self.random_state).is_multiple_of(3) {
                        brain.rest_until = self.tick + 20 + (random(&mut self.random_state) % 40);
                    }
                }
                mob.wander_ticks = mob.wander_ticks.saturating_sub(1);
                [mob.wander_target[0], mob.position[1], mob.wander_target[1]]
            };
            let goal_cell = [
                goal[0].floor() as i32,
                goal[1].floor() as i32,
                goal[2].floor() as i32,
            ];
            if path_budget > 0
                && self.tick >= brain.next_path
                && (brain.goal != Some(goal_cell) || brain.path.is_empty())
            {
                brain.path = navigation::route(world, mob.kind, mob.scale(), mob.position, goal);
                brain.goal = Some(goal_cell);
                brain.next_path = self.tick + 20 + mob.id as u64 % 10;
                path_budget -= 1;
            }
            while brain.path.front().is_some_and(|point| {
                (point[0] - mob.position[0]).hypot(point[2] - mob.position[2]) < 0.2
                    && (point[1] - mob.position[1]).abs() < 0.6
            }) {
                brain.path.pop_front();
            }
            let waypoint = brain.path.front().copied().unwrap_or(mob.position);
            let dx = waypoint[0] - mob.position[0];
            let dz = waypoint[2] - mob.position[2];
            let length = dx.hypot(dz);
            let speed = if panic {
                0.15
            } else if mob.kind == MobKind::Zombie {
                0.115
            } else {
                0.075
            };
            let close = !panic
                && animal_goal.map_or_else(
                    || {
                        target.is_some_and(|player| {
                            distance_squared(mob.position, player.position) < 2.25
                        })
                    },
                    |position| {
                        distance_squared(mob.position, position)
                            < if mob.is_baby() { 9.0 } else { 2.25 }
                    },
                );
            let move_horizontal = length > 0.05
                && !close
                && (panic
                    || target.is_some()
                    || animal_goal.is_some()
                    || self.tick >= brain.rest_until);
            // Preserve a stronger external impulse instead of erasing it with
            // this simple velocity-driven wander/target controller.
            let knockback = mob.velocity[0].hypot(mob.velocity[2]) > speed * 1.01;
            if !knockback {
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
            }
            if move_horizontal {
                mob.yaw = (-dx).atan2(dz).to_degrees() as f32;
            }
            if !knockback {
                let at = bucket(mob.position);
                for x in at.0 - 1..=at.0 + 1 {
                    for z in at.1 - 1..=at.1 + 1 {
                        if let Some(near) = neighbors.get(&(x, z)) {
                            for &(id, position) in near {
                                if id == mob.id || (position[1] - mob.position[1]).abs() > 1.5 {
                                    continue;
                                }
                                let dx = mob.position[0] - position[0];
                                let dz = mob.position[2] - position[2];
                                let distance = dx.hypot(dz);
                                if distance < 0.9 {
                                    let [x, z] = if distance > 0.01 {
                                        [dx / distance, dz / distance]
                                    } else {
                                        DIRECTIONS[mob.id as usize % 8]
                                    };
                                    mob.velocity[0] += x * 0.025;
                                    mob.velocity[2] += z * 0.025;
                                }
                            }
                        }
                    }
                }
            }
            if mob.on_ground && move_horizontal && waypoint[1] > mob.position[1] + 0.6 {
                mob.velocity[1] = 0.42;
            }
            if world.water_at(
                mob.position[0].floor() as i32,
                (mob.position[1] + 0.4).floor() as i32,
                mob.position[2].floor() as i32,
            ) {
                mob.velocity[1] = (mob.velocity[1] + 0.12).min(0.22);
                mob.velocity[0] *= 0.6;
                mob.velocity[2] *= 0.6;
            }
            mob.velocity[1] = (mob.velocity[1] - 0.08).max(-3.92);
            match physics::move_scaled_with_scratch(
                mob.position,
                mob.kind,
                mob.scale(),
                mob.velocity,
                mob.on_ground,
                world,
                &mut self.physics,
            ) {
                Ok(movement) => {
                    self.blocked.remove(&mob.id);
                    let hit = movement.collided;
                    mob.position = movement.position;
                    mob.on_ground = movement.on_ground;
                    for (axis, collided) in hit.into_iter().enumerate() {
                        if collided {
                            mob.velocity[axis] = 0.0;
                        }
                    }
                    mob.velocity[1] *= 0.98;
                    if knockback {
                        mob.velocity[0] *= 0.91;
                        mob.velocity[2] *= 0.91;
                    }
                    if hit[0] || hit[2] {
                        brain.path.clear();
                        brain.next_path = self.tick + 5;
                    }
                }
                Err(reason) => {
                    *mob = before;
                    self.random_state = random_before;
                    if reason == PhysicsError::Intersecting
                        && let Some(position) = (1..=4).find_map(|step| {
                            let p = [
                                mob.position[0],
                                mob.position[1] + f64::from(step) * 0.5,
                                mob.position[2],
                            ];
                            physics::move_scaled_with_scratch(
                                p,
                                mob.kind,
                                mob.scale(),
                                [0.0, 0.0, 0.0],
                                false,
                                world,
                                &mut self.physics,
                            )
                            .ok()
                            .map(|_| p)
                        })
                    {
                        mob.position = position;
                        mob.velocity = [0.0; 3];
                        changes.push(EntityChange::Moved {
                            id: mob.id,
                            position,
                            yaw: mob.yaw,
                            pitch: mob.pitch,
                            on_ground: false,
                        });
                    }
                    if self.blocked.insert(mob.id, reason) != Some(reason) {
                        changes.push(EntityChange::PhysicsBlocked { id: mob.id, reason });
                    }
                    continue;
                }
            }
            if bucket(old.0) != bucket(mob.position) {
                moved_buckets.push((mob.id, bucket(old.0), bucket(mob.position)));
            }
            if !valid_position(mob.position) || mob.position[1] < -80.0 {
                void.push(mob.id);
                continue;
            }
            if let Some(player) =
                target.filter(|player| mob.kind == MobKind::Zombie && player.attackable)
            {
                let mut eye = mob.position;
                eye[1] += mob.dimensions()[1] * 0.85;
                let mut target_eye = player.position;
                target_eye[1] += 1.62;
                if mob.attack_cooldown == 0
                    && distance_squared(mob.position, player.position) <= 3.24
                    && physics::line_clear(eye, target_eye, world)
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
        mob.love_ticks = 0;
        let mut changes = vec![EntityChange::Health {
            id,
            health: mob.health,
        }];
        if mob.health == 0.0 {
            changes.push(self.remove_with_reason(id, RemovalReason::Killed)?);
        }
        Ok(changes)
    }
    pub fn feed(&mut self, id: i32) -> Result<Vec<EntityChange>, EntityError> {
        let mob = self.entities.get_mut(&id).ok_or(EntityError::Missing)?;
        if !mob.can_feed() {
            return Err(EntityError::Invalid("animal cannot eat now"));
        }
        if mob.is_baby() {
            let growth = (((-mob.age / 20) as f32 * 0.1) as i32) * 20;
            let previous = mob.age;
            mob.age = (mob.age + growth).min(0);
            mob.forced_age = (mob.forced_age + mob.age - previous).min(24000);
            let mut changes = vec![EntityChange::GrowthFed { id }];
            if mob.age == 0 {
                mob.age = mob.forced_age;
                changes.push(EntityChange::AgeChanged { id, baby: false });
            }
            Ok(changes)
        } else {
            mob.love_ticks = 600;
            if let Some(brain) = self.brains.get_mut(&id) {
                brain.next_path = 0;
                brain.path.clear();
            }
            Ok(vec![EntityChange::Love { id }])
        }
    }

    pub fn breeding_pairs(&self) -> Vec<(i32, i32)> {
        let mut used = HashSet::new();
        let mut result = Vec::new();
        for (&id, brain) in &self.brains {
            let Some(partner) = brain.partner.filter(|_| brain.courtship >= 60) else {
                continue;
            };
            if used.contains(&id) || used.contains(&partner) {
                continue;
            }
            let (Some(a), Some(b)) = (self.entities.get(&id), self.entities.get(&partner)) else {
                continue;
            };
            if a.age == 0
                && b.age == 0
                && a.love_ticks > 0
                && b.love_ticks > 0
                && a.kind == b.kind
                && distance_squared(a.position, b.position) < 9.0
            {
                used.insert(id);
                used.insert(partner);
                result.push((id, partner));
            }
        }
        result
    }

    pub fn breed(
        &mut self,
        first: i32,
        second: i32,
        id: i32,
        uuid: Uuid,
        world: &impl WorldQuery,
    ) -> Result<Option<EntityChange>, EntityError> {
        if self.entities.len() >= self.capacity {
            return Ok(None);
        }
        if id <= 0
            || self.entities.contains_key(&id)
            || self.identities.contains(&uuid)
            || first == second
        {
            return Err(EntityError::Invalid("offspring identity"));
        }
        let a = self.entities.get(&first).ok_or(EntityError::Missing)?;
        let b = self.entities.get(&second).ok_or(EntityError::Missing)?;
        if !self
            .brains
            .get(&first)
            .is_some_and(|brain| brain.partner == Some(second) && brain.courtship >= 60)
        {
            return Ok(None);
        }
        if a.kind == MobKind::Zombie
            || a.kind != b.kind
            || a.age != 0
            || b.age != 0
            || a.love_ticks == 0
            || b.love_ticks == 0
            || distance_squared(a.position, b.position) >= 9.0
        {
            return Ok(None);
        }
        let position = a.position;
        if physics::move_scaled_with_scratch(
            position,
            a.kind,
            0.5,
            [0.0; 3],
            false,
            world,
            &mut self.physics,
        )
        .is_err()
        {
            return Ok(None);
        }
        let child = Mob {
            id,
            uuid,
            kind: a.kind,
            position,
            velocity: [0.0; 3],
            yaw: 0.0,
            pitch: 0.0,
            health: a.kind.maximum_health(),
            on_ground: a.on_ground,
            wander_target: [position[0], position[2]],
            wander_ticks: 0,
            attack_cooldown: 0,
            age: -24000,
            forced_age: 0,
            love_ticks: 0,
        };
        for parent in [first, second] {
            let mob = self.entities.get_mut(&parent).ok_or(EntityError::Missing)?;
            mob.age = 6000;
            mob.love_ticks = 0;
            if let Some(brain) = self.brains.get_mut(&parent) {
                brain.partner = None;
                brain.courtship = 0;
            }
        }
        self.insert(child.clone());
        Ok(Some(EntityChange::Spawned(child)))
    }

    /// Apply the core's player hit and default living-entity knockback together.
    pub fn damage_with_knockback(
        &mut self,
        id: i32,
        amount: f32,
        attacker: [f64; 3],
    ) -> Result<Vec<EntityChange>, EntityError> {
        if !valid_position(attacker) {
            return Err(EntityError::Invalid("invalid attacker position"));
        }
        let changes = self.damage(id, amount)?;
        if let Some(mob) = self.entities.get_mut(&id) {
            if mob.kind != MobKind::Zombie {
                let brain = self.brains.entry(id).or_default();
                brain.panic_until = self.tick + 80;
                brain.flee_from = attacker;
                brain.path.clear();
                brain.next_path = 0;
            }
            let dx = mob.position[0] - attacker[0];
            let dz = mob.position[2] - attacker[2];
            let length = dx.hypot(dz);
            let [away_x, away_z] = if length > 0.0032 {
                [dx / length, dz / length]
            } else {
                DIRECTIONS[(random(&mut self.random_state) % 8) as usize]
            };
            mob.velocity[0] = mob.velocity[0] * 0.5 + away_x * 0.4;
            mob.velocity[2] = mob.velocity[2] * 0.5 + away_z * 0.4;
            if mob.on_ground {
                mob.velocity[1] = (mob.velocity[1] * 0.5 + 0.4).min(0.4);
            }
        }
        Ok(changes)
    }
    pub fn damage_with_impulse(
        &mut self,
        id: i32,
        amount: f32,
        impulse: [f64; 3],
    ) -> Result<Vec<EntityChange>, EntityError> {
        if impulse
            .iter()
            .any(|value| !value.is_finite() || value.abs() > 4.0)
        {
            return Err(EntityError::Invalid("invalid explosion impulse"));
        }
        let changes = self.damage(id, amount)?;
        if let Some(mob) = self.entities.get_mut(&id) {
            for (velocity, impulse) in mob.velocity.iter_mut().zip(impulse) {
                *velocity = (*velocity + impulse).clamp(-4.0, 4.0);
            }
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
        target[1] += mob.dimensions()[1] * 0.5;
        physics::line_clear(eye, target, world)
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
            schema: 2,
            tick: self.tick,
            random_state: self.random_state,
            entities: self.entities.values().cloned().collect(),
        }
    }
    pub fn restore(snapshot: Snapshot, capacity: usize) -> Result<Self, EntityError> {
        if !matches!(snapshot.schema, 1 | 2) {
            return Err(EntityError::Invalid("unsupported entity snapshot schema"));
        }
        let mut world = Self::new(snapshot.random_state, capacity)?;
        if snapshot.entities.len() > capacity {
            return Err(EntityError::Capacity);
        }
        for mob in snapshot.entities {
            if snapshot.schema == 1 && (mob.age != 0 || mob.forced_age != 0 || mob.love_ticks != 0)
            {
                return Err(EntityError::Invalid(
                    "legacy snapshot contains breeding state",
                ));
            }
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
                || !(-24000..=24000).contains(&mob.age)
                || !(0..=24000).contains(&mob.forced_age)
                || mob.love_ticks > 600
                || (mob.age != 0 && mob.love_ticks != 0)
                || (mob.kind == MobKind::Zombie
                    && (mob.age != 0 || mob.forced_age != 0 || mob.love_ticks != 0))
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
            let Some(y) = world.surface_height(x, z) else {
                continue;
            };
            if world.water_at(x, y.floor() as i32, z) {
                continue;
            }
            let position = [f64::from(x) + 0.5, y, f64::from(z) + 0.5];
            if self.nearby(position, 32.0)?.len() >= 8 {
                continue;
            }
            if !valid_position(position)
                || players
                    .iter()
                    .any(|player| distance_squared(position, player.position) < 24.0 * 24.0)
            {
                continue;
            }
            let kind = match random(&mut self.random_state) % if hostile_allowed { 3 } else { 2 } {
                0 => MobKind::Pig,
                1 => MobKind::Cow,
                _ => MobKind::Zombie,
            };
            match self.spawn_space(position, kind, world) {
                Ok(true) => {}
                Ok(false) | Err(EntityError::Physics(_)) => continue,
                Err(error) => return Err(error),
            }
            if !physics::move_with_scratch(
                position,
                kind,
                [0.0, -0.001, 0.0],
                false,
                world,
                &mut self.physics,
            )
            .is_ok_and(|movement| movement.on_ground)
            {
                continue;
            }
            let (id, uuid) = identity();
            return self.spawn(id, uuid, kind, position, world).map(Some);
        }
        Ok(None)
    }
    fn spawn_space(
        &self,
        position: [f64; 3],
        kind: MobKind,
        world: &impl WorldQuery,
    ) -> Result<bool, EntityError> {
        if physics::try_overlaps(position, kind, world)? {
            return Ok(false);
        }
        Ok(self
            .nearby(position, 4.0)?
            .into_iter()
            .all(|other| !physics::bodies_overlap(position, kind, other.position, other.kind)))
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
        self.blocked.remove(&id);
        self.brains.remove(&id);
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
#[cfg(test)]
fn line_clear(first: [f64; 3], second: [f64; 3], world: &impl WorldQuery) -> bool {
    physics::line_clear(first, second, world)
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
