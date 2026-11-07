use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};
use tokio::time::Instant;

pub const MAX_NATIVE_CALLS: u64 = 128;
pub const MAX_NATIVE_CALL_BYTES: usize = 65_536;
pub const MAX_NATIVE_RESULT_BYTES: usize = 262_144;
pub const MAX_NATIVE_TOTAL_BYTES: usize = 1_048_576;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "query", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeQuery {
    ServerSnapshot,
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
    Pin<Box<dyn Future<Output = Result<ServerSnapshot, NativeQueryError>> + Send + 'a>>;

pub trait NativeQueryService: Send + Sync {
    /// Queries must yield promptly and release their work when this future is dropped.
    fn query(&self, context: NativeCallContext, query: NativeQuery) -> NativeQueryFuture<'_>;
}
