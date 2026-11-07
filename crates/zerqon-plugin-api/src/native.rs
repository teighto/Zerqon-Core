use serde::{Deserialize, Serialize};
use std::{fmt, marker::PhantomData};
use uuid::Uuid;

pub const READ_API_VERSION: i32 = 1;
pub const CAP_CONTEXT: u64 = 1;
pub const CAP_SERVER_SNAPSHOT: u64 = 1 << 1;
pub const CAP_ONLINE_PLAYERS: u64 = 1 << 2;
pub const CAP_PLAYER: u64 = 1 << 3;
pub const CAP_WORLDS: u64 = 1 << 4;
pub const READ_CAPABILITIES: u64 =
    CAP_CONTEXT | CAP_SERVER_SNAPSHOT | CAP_ONLINE_PLAYERS | CAP_PLAYER | CAP_WORLDS;
pub const MAX_NATIVE_CONTEXT_BYTES: usize = 256;
pub const MAX_NATIVE_TOTAL_REQUEST_BYTES: usize = 1_048_576;
pub const MAX_NATIVE_CONTEXT_COPY_BYTES: usize = 65_536;
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
    pub fn capability(&self) -> u64 {
        match self {
            Self::ServerSnapshot => CAP_SERVER_SNAPSHOT,
            Self::OnlinePlayers => CAP_ONLINE_PLAYERS,
            Self::Player { .. } => CAP_PLAYER,
            Self::Worlds => CAP_WORLDS,
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, NativeDecodeError> {
        let query: Self = decode_bounded(bytes, MAX_NATIVE_CALL_BYTES)?;
        if !query.valid() {
            return Err(NativeDecodeError::InvalidValue);
        }
        Ok(query)
    }

    pub fn valid(&self) -> bool {
        match self {
            Self::Player { uuid, session_id } => {
                !uuid.is_nil() && session_id.is_none_or(|session| session > 0)
            }
            _ => true,
        }
    }
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

impl<'de> Deserialize<'de> for NativeQueryValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ValueVisitor;
        impl<'de> serde::de::Visitor<'de> for ValueVisitor {
            type Value = NativeQueryValue;

            fn expecting(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
                output.write_str("a native query result object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                use serde::de::Error;
                let key = map
                    .next_key::<String>()?
                    .ok_or_else(|| A::Error::custom("empty native result"))?;
                let value = match key.as_str() {
                    "players" => NativeQueryValue::OnlinePlayers {
                        players: map
                            .next_value_seed(BoundedSequence::<NativePlayer, MAX_QUERY_PLAYERS>(
                                PhantomData,
                            ))?,
                    },
                    "player" => NativeQueryValue::Player {
                        player: map.next_value()?,
                    },
                    "worlds" => NativeQueryValue::Worlds {
                        worlds: map
                            .next_value_seed(BoundedSequence::<NativeWorld, MAX_QUERY_WORLDS>(
                                PhantomData,
                            ))?,
                    },
                    "server_version" | "minecraft_version" | "protocol" | "max_players"
                    | "online_players" | "tick" => {
                        let snapshot = ServerSnapshot::deserialize(
                            serde::de::value::MapAccessDeserializer::new(PrefixedMap {
                                first_key: Some(key),
                                remaining: map,
                            }),
                        )?;
                        return Ok(NativeQueryValue::ServerSnapshot(snapshot));
                    }
                    _ => return Err(A::Error::custom("unknown native result field")),
                };
                if map.next_key::<serde::de::IgnoredAny>()?.is_some() {
                    return Err(A::Error::custom(
                        "unexpected or duplicate native result field",
                    ));
                }
                Ok(value)
            }
        }
        deserializer.deserialize_map(ValueVisitor)
    }
}

struct PrefixedMap<A> {
    first_key: Option<String>,
    remaining: A,
}

impl<'de, A: serde::de::MapAccess<'de>> serde::de::MapAccess<'de> for PrefixedMap<A> {
    type Error = A::Error;

    fn next_key_seed<K: serde::de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        use serde::de::IntoDeserializer;
        if let Some(key) = self.first_key.take() {
            return seed.deserialize(key.into_deserializer()).map(Some);
        }
        self.remaining.next_key_seed(seed)
    }

    fn next_value_seed<V: serde::de::DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        self.remaining.next_value_seed(seed)
    }
}

struct BoundedSequence<T, const MAX: usize>(PhantomData<T>);

impl<'de, T: Deserialize<'de>, const MAX: usize> serde::de::DeserializeSeed<'de>
    for BoundedSequence<T, MAX>
{
    type Value = Vec<T>;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Vec<T>, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de, T: Deserialize<'de>, const MAX: usize> serde::de::Visitor<'de>
    for BoundedSequence<T, MAX>
{
    type Value = Vec<T>;

    fn expecting(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(output, "at most {MAX} native entries")
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut sequence: A) -> Result<Vec<T>, A::Error> {
        use serde::de::Error;
        if sequence.size_hint().is_some_and(|size| size > MAX) {
            return Err(A::Error::custom("native list exceeds its entry limit"));
        }
        let mut values = Vec::new();
        while values.len() < MAX {
            let Some(value) = sequence.next_element()? else {
                return Ok(values);
            };
            if values.len() == values.capacity() {
                let capacity = values.capacity().saturating_mul(2).max(4).min(MAX);
                values
                    .try_reserve_exact(capacity - values.len())
                    .map_err(|_| A::Error::custom("native list allocation failed"))?;
            }
            values.push(value);
        }
        sequence.next_element_seed(RejectExtraEntry)?;
        Ok(values)
    }
}

struct RejectExtraEntry;

impl<'de> serde::de::DeserializeSeed<'de> for RejectExtraEntry {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, _: D) -> Result<(), D::Error> {
        Err(serde::de::Error::custom(
            "native list exceeds its entry limit",
        ))
    }
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
    pub fn matches_event(&self, event: &crate::Event) -> bool {
        use crate::{Event, SenderKind};
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

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

impl fmt::Display for NativeQueryError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str(&self.message)
    }
}

impl std::error::Error for NativeQueryError {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeSource {
    Server,
    Console,
    Player { uuid: Uuid, session_id: i32 },
}

impl<'de> Deserialize<'de> for NativeSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
        enum WireSource {
            Server {},
            Console {},
            Player { uuid: Uuid, session_id: i32 },
        }
        Ok(match WireSource::deserialize(deserializer)? {
            WireSource::Server {} => Self::Server,
            WireSource::Console {} => Self::Console,
            WireSource::Player { uuid, session_id } => Self::Player { uuid, session_id },
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeCallbackContext {
    pub source: NativeSource,
}

impl NativeCallbackContext {
    pub fn valid(&self) -> bool {
        match self.source {
            NativeSource::Server | NativeSource::Console => true,
            NativeSource::Player { uuid, session_id } => !uuid.is_nil() && session_id > 0,
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, NativeDecodeError> {
        let context: Self = decode_bounded(bytes, MAX_NATIVE_CONTEXT_BYTES)?;
        if !context.valid() {
            return Err(NativeDecodeError::InvalidValue);
        }
        Ok(context)
    }

    pub fn matches_event(&self, event: &crate::Event) -> bool {
        use crate::{Event, SenderKind};
        if !self.valid() {
            return false;
        }
        match (&self.source, event) {
            (NativeSource::Server, Event::Enable { .. } | Event::Disable | Event::Tick { .. })
            | (
                NativeSource::Console,
                Event::Command {
                    sender_kind: SenderKind::Console,
                    ..
                },
            )
            | (
                NativeSource::Player { .. },
                Event::Command {
                    sender_kind: SenderKind::Player,
                    ..
                },
            ) => true,
            (
                NativeSource::Player { uuid, .. },
                Event::PlayerJoin {
                    uuid: event_uuid, ..
                }
                | Event::PlayerQuit {
                    uuid: event_uuid, ..
                },
            ) => Uuid::parse_str(event_uuid).ok() == Some(*uuid),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum NativeQueryResponse {
    Success { value: NativeQueryValue },
    Error { error: NativeQueryError },
}

impl Serialize for NativeQueryResponse {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut fields = serializer.serialize_struct("NativeQueryResponse", 2)?;
        match self {
            Self::Success { value } => {
                fields.serialize_field("ok", &true)?;
                fields.serialize_field("value", value)?;
            }
            Self::Error { error } => {
                fields.serialize_field("ok", &false)?;
                fields.serialize_field("error", error)?;
            }
        }
        fields.end()
    }
}

impl<'de> Deserialize<'de> for NativeQueryResponse {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ResponseVisitor;
        impl<'de> serde::de::Visitor<'de> for ResponseVisitor {
            type Value = NativeQueryResponse;

            fn expecting(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
                output.write_str("a native query response with an explicit boolean status")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                use serde::de::Error;
                let (mut ok, mut value, mut error) = (None, None, None);
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "ok" if ok.is_none() => ok = Some(map.next_value::<bool>()?),
                        "value" if value.is_none() => value = Some(map.next_value()?),
                        "error" if error.is_none() => error = Some(map.next_value()?),
                        _ => {
                            return Err(A::Error::custom("unexpected or duplicate response field"));
                        }
                    }
                }
                match (ok, value, error) {
                    (Some(true), Some(value), None) => Ok(NativeQueryResponse::Success { value }),
                    (Some(false), None, Some(error)) => Ok(NativeQueryResponse::Error { error }),
                    _ => Err(A::Error::custom(
                        "native response status and fields do not match",
                    )),
                }
            }
        }
        deserializer.deserialize_map(ResponseVisitor)
    }
}

impl NativeQueryResponse {
    pub fn valid_for(&self, query: &NativeQuery) -> bool {
        match self {
            Self::Success { value } => query.valid() && value.valid_for(query),
            Self::Error { error } => error.valid(),
        }
    }

    pub fn decode_for(query: &NativeQuery, bytes: &[u8]) -> Result<Self, NativeDecodeError> {
        let response: Self = decode_bounded(bytes, MAX_NATIVE_RESULT_BYTES)?;
        if !response.valid_for(query) {
            return Err(NativeDecodeError::InvalidValue);
        }
        Ok(response)
    }
}

#[derive(Debug)]
pub enum NativeDecodeError {
    TooLarge,
    Malformed(serde_json::Error),
    InvalidValue,
}

impl fmt::Display for NativeDecodeError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => output.write_str("native payload exceeds its byte limit"),
            Self::Malformed(error) => write!(output, "invalid native payload: {error}"),
            Self::InvalidValue => output.write_str("native payload violates its query contract"),
        }
    }
}

impl std::error::Error for NativeDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Malformed(error) => Some(error),
            _ => None,
        }
    }
}

fn decode_bounded<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    limit: usize,
) -> Result<T, NativeDecodeError> {
    if bytes.len() > limit {
        return Err(NativeDecodeError::TooLarge);
    }
    serde_json::from_slice(bytes).map_err(NativeDecodeError::Malformed)
}

#[derive(Debug)]
pub enum NativeReadError {
    Unsupported,
    InactiveCallback,
    StaleCallback,
    InvalidRequest,
    Capacity,
    Protocol,
    Decode(NativeDecodeError),
    Service(NativeQueryError),
}

impl fmt::Display for NativeReadError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => output.write_str("native read capability is unavailable"),
            Self::InactiveCallback => output.write_str("no active native callback"),
            Self::StaleCallback => output.write_str("native access belongs to an expired callback"),
            Self::InvalidRequest => output.write_str("invalid native read request"),
            Self::Capacity => output.write_str("native read capacity exceeded"),
            Self::Protocol => output.write_str("native read transport is no longer valid"),
            Self::Decode(error) => error.fmt(output),
            Self::Service(error) => error.fmt(output),
        }
    }
}

impl std::error::Error for NativeReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decode(error) => Some(error),
            Self::Service(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
