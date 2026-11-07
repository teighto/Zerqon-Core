use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};
use tokio::time::Instant;
use uuid::Uuid;

pub const MAX_NATIVE_CALLS: u64 = 128;
pub const MAX_NATIVE_CALL_BYTES: usize = 65_536;
pub const MAX_NATIVE_RESULT_BYTES: usize = 262_144;
pub const MAX_NATIVE_TOTAL_BYTES: usize = 1_048_576;
pub const MAX_QUERY_PLAYERS: usize = 1024;
pub const MAX_QUERY_WORLDS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "query", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeQuery {
    ServerSnapshot,
    OnlinePlayers,
    Player {
        uuid: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<i32>,
    },
    Worlds,
}

impl<'de> Deserialize<'de> for NativeQuery {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "query", rename_all = "snake_case", deny_unknown_fields)]
        enum WireQuery {
            ServerSnapshot {},
            OnlinePlayers {},
            Player {
                uuid: Uuid,
                #[serde(default)]
                session_id: Option<i32>,
            },
            Worlds {},
        }
        Ok(match WireQuery::deserialize(deserializer)? {
            WireQuery::ServerSnapshot {} => Self::ServerSnapshot,
            WireQuery::OnlinePlayers {} => Self::OnlinePlayers,
            WireQuery::Player { uuid, session_id } => Self::Player { uuid, session_id },
            WireQuery::Worlds {} => Self::Worlds,
        })
    }
}

impl NativeQuery {
    pub fn valid(&self) -> bool {
        match self {
            Self::Player { uuid, session_id } => {
                !uuid.is_nil() && session_id.is_none_or(|session| session > 0)
            }
            _ => true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct NativeCallContext {
    pub plugin: String,
    pub parent_id: u64,
    pub call_id: u64,
    pub deadline: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSnapshot {
    pub server_version: String,
    pub minecraft_version: String,
    pub protocol: i32,
    pub max_players: u32,
    pub online_players: u32,
    pub tick: u64,
}

impl ServerSnapshot {
    pub fn valid(&self) -> bool {
        [&self.server_version, &self.minecraft_version]
            .iter()
            .all(|value| {
                !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
            })
            && self.protocol >= 0
            && (1..=1_000_000).contains(&self.max_players)
            && self.online_players <= self.max_players
            && self.tick <= i64::MAX as u64
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativePosition {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub pitch: f32,
}

impl NativePosition {
    pub fn valid(&self) -> bool {
        [self.x, self.y, self.z]
            .iter()
            .all(|value| value.is_finite())
            && self.x.abs() <= 30_000_000.0
            && self.z.abs() <= 30_000_000.0
            && self.y.abs() <= 20_000_000.0
            && self.yaw.is_finite()
            && self.pitch.is_finite()
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativePlayer {
    pub uuid: Uuid,
    pub session_id: i32,
    pub entity_id: i32,
    pub name: String,
    pub world_uuid: Uuid,
    pub position: NativePosition,
    pub operator: bool,
    pub online: bool,
}

impl NativePlayer {
    pub fn valid(&self) -> bool {
        !self.uuid.is_nil()
            && self.session_id > 0
            && self.entity_id > 0
            && !self.world_uuid.is_nil()
            && (1..=16).contains(&self.name.len())
            && self
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            && self.position.valid()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeEnvironment {
    Normal,
    Nether,
    TheEnd,
    Custom,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeWorld {
    pub uuid: Uuid,
    pub name: String,
    pub key: String,
    pub dimension: String,
    pub environment: NativeEnvironment,
    pub min_height: i32,
    pub max_height: i32,
    pub spawn: NativePosition,
}

impl NativeWorld {
    pub fn valid(&self) -> bool {
        !self.uuid.is_nil()
            && !self.name.is_empty()
            && self.name.len() <= 128
            && !self.name.chars().any(char::is_control)
            && native_key(&self.key)
            && native_key(&self.dimension)
            && self.min_height < self.max_height
            && self.min_height >= -20_000_000
            && self.max_height <= 20_000_000
            && self.min_height % 16 == 0
            && self.max_height % 16 == 0
            && self.spawn.valid()
    }
}

fn native_key(value: &str) -> bool {
    value.len() <= 256
        && value.split_once(':').is_some_and(|(namespace, path)| {
            !namespace.is_empty()
                && !path.is_empty()
                && namespace.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
                })
                && path.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"/_.-".contains(&byte)
                })
        })
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum NativeQueryValue {
    ServerSnapshot(ServerSnapshot),
    OnlinePlayers { players: Vec<NativePlayer> },
    Player { player: Option<NativePlayer> },
    Worlds { worlds: Vec<NativeWorld> },
}

impl NativeQueryValue {
    pub fn valid_for(&self, query: &NativeQuery) -> bool {
        match (self, query) {
            (Self::ServerSnapshot(snapshot), NativeQuery::ServerSnapshot) => snapshot.valid(),
            (Self::OnlinePlayers { players }, NativeQuery::OnlinePlayers) => {
                let mut uuids = std::collections::HashSet::new();
                let mut sessions = std::collections::HashSet::new();
                let mut entities = std::collections::HashSet::new();
                players.len() <= MAX_QUERY_PLAYERS
                    && players.iter().all(|player| {
                        player.valid()
                            && player.online
                            && uuids.insert(player.uuid)
                            && sessions.insert(player.session_id)
                            && entities.insert(player.entity_id)
                    })
            }
            (Self::Player { player }, NativeQuery::Player { uuid, session_id }) => {
                player.as_ref().is_none_or(|player| {
                    player.valid()
                        && player.online
                        && player.uuid == *uuid
                        && session_id.is_none_or(|session| session == player.session_id)
                })
            }
            (Self::Worlds { worlds }, NativeQuery::Worlds) => {
                let mut uuids = std::collections::HashSet::new();
                let mut keys = std::collections::HashSet::new();
                worlds.len() <= MAX_QUERY_WORLDS
                    && worlds.iter().all(|world| {
                        world.valid() && uuids.insert(world.uuid) && keys.insert(&world.key)
                    })
            }
            _ => false,
        }
    }
}

impl From<ServerSnapshot> for NativeQueryValue {
    fn from(value: ServerSnapshot) -> Self {
        Self::ServerSnapshot(value)
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEventContext {
    pub player: NativePlayer,
}

impl NativeEventContext {
    pub fn matches_event(&self, event: &zerqon_plugin_api::Event) -> bool {
        use zerqon_plugin_api::{Event, SenderKind};
        if !self.player.valid() {
            return false;
        }
        match event {
            Event::Command {
                sender,
                sender_kind: SenderKind::Player,
                operator,
                ..
            } => {
                self.player.online
                    && self.player.name == *sender
                    && self.player.operator == *operator
            }
            Event::PlayerJoin { uuid, name } => {
                self.player.online
                    && self.player.name == *name
                    && Uuid::parse_str(uuid).ok() == Some(self.player.uuid)
            }
            Event::PlayerQuit { uuid, name } => {
                self.player.online
                    && self.player.name == *name
                    && Uuid::parse_str(uuid).ok() == Some(self.player.uuid)
            }
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeErrorCode {
    InvalidArgument,
    NotFound,
    StaleSession,
    Unavailable,
    Unsupported,
    Capacity,
    Cancelled,
    DeadlineExceeded,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, thiserror::Error)]
#[serde(deny_unknown_fields)]
#[error("{message}")]
pub struct NativeQueryError {
    pub code: NativeErrorCode,
    pub message: String,
}

impl NativeQueryError {
    pub fn new(code: NativeErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn valid(&self) -> bool {
        !self.message.is_empty()
            && self.message.len() <= 1024
            && !self.message.chars().any(char::is_control)
    }
}

pub type NativeQueryFuture<'a> =
    Pin<Box<dyn Future<Output = Result<NativeQueryValue, NativeQueryError>> + Send + 'a>>;

pub trait NativeQueryService: Send + Sync {
    /// Queries must yield promptly and release their work when this future is dropped.
    fn query(&self, context: NativeCallContext, query: NativeQuery) -> NativeQueryFuture<'_>;
}
