use std::{future::Future, pin::Pin};
use tokio::time::Instant;
pub use zerqon_plugin_api::native::{
    MAX_NATIVE_CALL_BYTES, MAX_NATIVE_CALLS, MAX_NATIVE_TOTAL_BYTES, MAX_QUERY_PLAYERS,
    MAX_QUERY_WORLDS, NativeEnvironment, NativeErrorCode, NativeEventContext, NativePlayer,
    NativePosition, NativeQuery, NativeQueryError, NativeQueryValue, NativeWorld, ServerSnapshot,
};

#[derive(Clone, Debug)]
pub struct NativeCallContext {
    pub plugin: String,
    pub parent_id: u64,
    pub call_id: u64,
    pub deadline: Instant,
}

pub type NativeQueryFuture<'a> =
    Pin<Box<dyn Future<Output = Result<NativeQueryValue, NativeQueryError>> + Send + 'a>>;

pub trait NativeQueryService: Send + Sync {
    /// The future owns all accepted work until it settles, including error paths.
    /// It must not detach work that survives its return. Runtime teardown also requires drop safety.
    fn query(&self, context: NativeCallContext, query: NativeQuery) -> NativeQueryFuture<'_>;
}
