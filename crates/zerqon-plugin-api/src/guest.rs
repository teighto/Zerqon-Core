use crate::{Action, Event, MAX_EVENT_BYTES};

#[link(wasm_import_module = "zerqon")]
unsafe extern "C" {
    fn event_len() -> i32;
    fn event_read(pointer: *mut u8, length: i32) -> i32;
    fn emit(pointer: *const u8, length: i32) -> i32;
}

/// Decodes one host event and emits the handler's actions through ABI version 1.
pub fn dispatch(handler: fn(Event) -> Vec<Action>) -> i32 {
    // SAFETY: event_len takes no pointers; the Zerqon host returns a bounded length.
    let len = unsafe { event_len() };
    if len <= 0 || len as usize > MAX_EVENT_BYTES {
        return 1;
    }
    let mut bytes = vec![0_u8; len as usize];
    // SAFETY: the host writes exactly len bytes to this live, uniquely borrowed buffer.
    if unsafe { event_read(bytes.as_mut_ptr(), len) } != len {
        return 2;
    }
    let Ok(event) = serde_json::from_slice::<Event>(&bytes) else {
        return 3;
    };
    for action in handler(event) {
        let Ok(bytes) = serde_json::to_vec(&action) else {
            return 4;
        };
        if bytes.len() > MAX_EVENT_BYTES {
            return 5;
        }
        // SAFETY: emit only reads the live buffer during this synchronous host call.
        if unsafe { emit(bytes.as_ptr(), bytes.len() as i32) } != 0 {
            return 6;
        }
    }
    0
}

#[cfg(feature = "native-read")]
pub use native_read::NativeReads;

#[cfg(feature = "native-read")]
mod native_read {
    use crate::native::{
        CAP_CONTEXT, MAX_NATIVE_CALL_BYTES, MAX_NATIVE_CALLS, MAX_NATIVE_CONTEXT_BYTES,
        MAX_NATIVE_RESULT_BYTES, MAX_NATIVE_TOTAL_BYTES, MAX_NATIVE_TOTAL_REQUEST_BYTES,
        NativeCallbackContext, NativePlayer, NativeQuery, NativeQueryResponse, NativeQueryValue,
        NativeReadError, NativeSource, NativeWorld, READ_API_VERSION, ServerSnapshot,
    };
    use std::collections::BTreeMap;
    use uuid::Uuid;

    #[link(wasm_import_module = "zerqon_read_v1")]
    unsafe extern "C" {
        fn capabilities(version: i32) -> i64;
        fn callback_token() -> i64;
        fn context_len(token: i64) -> i32;
        fn context_read(token: i64, pointer: *mut u8, length: i32) -> i32;
        fn query(token: i64, pointer: *const u8, length: i32) -> i32;
        fn result_len(token: i64, handle: i32) -> i32;
        fn result_read(token: i64, handle: i32, pointer: *mut u8, length: i32) -> i32;
    }

    /// Read access and cached snapshots for one host callback.
    ///
    /// Native queries may suspend this synchronous handler. Access cannot be reused
    /// in another callback; transport failures invalidate it for the rest of this one.
    pub struct NativeReads {
        token: i64,
        capabilities: u64,
        context: NativeCallbackContext,
        calls: u64,
        request_bytes: usize,
        result_bytes: usize,
        poisoned: bool,
        online: Option<Vec<NativePlayer>>,
        worlds: Option<Vec<NativeWorld>>,
        players: BTreeMap<(Uuid, Option<i32>), Option<NativePlayer>>,
    }

    impl NativeReads {
        pub fn current() -> Result<Self, NativeReadError> {
            // SAFETY: capability and token imports accept no guest memory.
            let (capabilities, token) =
                unsafe { (capabilities(READ_API_VERSION), callback_token()) };
            if capabilities < 0 {
                return Err(NativeReadError::Protocol);
            }
            if capabilities as u64 & CAP_CONTEXT == 0 {
                return Err(NativeReadError::Unsupported);
            }
            if token <= 0 {
                return Err(NativeReadError::InactiveCallback);
            }
            // SAFETY: the active token is a scalar supplied by the host.
            let len = unsafe { context_len(token) };
            let mut bytes = bounded_buffer(len, MAX_NATIVE_CONTEXT_BYTES)?;
            // SAFETY: this uniquely owned buffer has exactly the checked host length.
            if unsafe { context_read(token, bytes.as_mut_ptr(), len) } != len {
                return Err(NativeReadError::Protocol);
            }
            let context = NativeCallbackContext::decode(&bytes).map_err(NativeReadError::Decode)?;
            let reads = Self {
                token,
                capabilities: capabilities as u64,
                context,
                calls: 0,
                request_bytes: 0,
                result_bytes: 0,
                poisoned: false,
                online: None,
                worlds: None,
                players: BTreeMap::new(),
            };
            reads.check_active()?;
            Ok(reads)
        }

        pub fn capabilities(&self) -> u64 {
            self.capabilities
        }

        pub fn source(&self) -> Result<&NativeSource, NativeReadError> {
            self.check_active()?;
            Ok(&self.context.source)
        }

        pub fn server_snapshot(&mut self) -> Result<ServerSnapshot, NativeReadError> {
            match self.query(NativeQuery::ServerSnapshot)? {
                NativeQueryValue::ServerSnapshot(snapshot) => Ok(snapshot),
                _ => Err(NativeReadError::Protocol),
            }
        }

        pub fn online_players(&mut self) -> Result<&[NativePlayer], NativeReadError> {
            self.check_query(&NativeQuery::OnlinePlayers)?;
            if self.online.is_none() {
                match self.query(NativeQuery::OnlinePlayers)? {
                    NativeQueryValue::OnlinePlayers { players } => self.online = Some(players),
                    _ => return Err(NativeReadError::Protocol),
                }
            }
            self.online.as_deref().ok_or(NativeReadError::Protocol)
        }

        pub fn player(
            &mut self,
            uuid: Uuid,
            session_id: Option<i32>,
        ) -> Result<Option<NativePlayer>, NativeReadError> {
            let request = NativeQuery::Player { uuid, session_id };
            self.check_query(&request)?;
            if let Some(player) = self.players.get(&(uuid, session_id)) {
                return Ok(player.clone());
            }
            if let Some(player) = self.online.as_ref().and_then(|players| {
                players.iter().find(|player| {
                    player.uuid == uuid && session_id.is_none_or(|id| id == player.session_id)
                })
            }) {
                return Ok(Some(player.clone()));
            }
            match self.query(request)? {
                NativeQueryValue::Player { player } => {
                    self.players.insert((uuid, session_id), player.clone());
                    Ok(player)
                }
                _ => Err(NativeReadError::Protocol),
            }
        }

        pub fn worlds(&mut self) -> Result<&[NativeWorld], NativeReadError> {
            self.check_query(&NativeQuery::Worlds)?;
            if self.worlds.is_none() {
                match self.query(NativeQuery::Worlds)? {
                    NativeQueryValue::Worlds { worlds } => self.worlds = Some(worlds),
                    _ => return Err(NativeReadError::Protocol),
                }
            }
            self.worlds.as_deref().ok_or(NativeReadError::Protocol)
        }

        fn check_active(&self) -> Result<(), NativeReadError> {
            if self.poisoned {
                return Err(NativeReadError::Protocol);
            }
            // SAFETY: the token import does not access guest memory.
            if unsafe { callback_token() } != self.token {
                return Err(NativeReadError::StaleCallback);
            }
            Ok(())
        }

        fn check_query(&self, request: &NativeQuery) -> Result<(), NativeReadError> {
            self.check_active()?;
            if self.capabilities & request.capability() == 0 {
                return Err(NativeReadError::Unsupported);
            }
            if !request.valid() {
                return Err(NativeReadError::InvalidRequest);
            }
            Ok(())
        }

        fn query(&mut self, request: NativeQuery) -> Result<NativeQueryValue, NativeReadError> {
            self.check_query(&request)?;
            if self.calls >= MAX_NATIVE_CALLS {
                return Err(NativeReadError::Capacity);
            }
            let bytes =
                serde_json::to_vec(&request).map_err(|_| NativeReadError::InvalidRequest)?;
            if bytes.len() > MAX_NATIVE_CALL_BYTES
                || bytes.len() > MAX_NATIVE_TOTAL_REQUEST_BYTES.saturating_sub(self.request_bytes)
            {
                return Err(NativeReadError::Capacity);
            }
            self.calls += 1;
            self.request_bytes += bytes.len();
            // SAFETY: the host copies this live request before suspending its import.
            let handle = unsafe { query(self.token, bytes.as_ptr(), bytes.len() as i32) };
            let response = self.read_response(&request, handle);
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    self.poisoned = true;
                    return Err(error);
                }
            };
            match response {
                NativeQueryResponse::Success { value } => Ok(value),
                NativeQueryResponse::Error { error } => Err(NativeReadError::Service(error)),
            }
        }

        fn read_response(
            &mut self,
            request: &NativeQuery,
            handle: i32,
        ) -> Result<NativeQueryResponse, NativeReadError> {
            self.check_active()?;
            if handle <= 0 || handle as u64 > MAX_NATIVE_CALLS {
                return Err(NativeReadError::Protocol);
            }
            // SAFETY: the host validates this callback token and its scalar result handle.
            let len = unsafe { result_len(self.token, handle) };
            if len <= 0 {
                return Err(NativeReadError::Protocol);
            }
            if len as usize > MAX_NATIVE_TOTAL_BYTES.saturating_sub(self.result_bytes) {
                return Err(NativeReadError::Capacity);
            }
            let mut bytes = bounded_buffer(len, MAX_NATIVE_RESULT_BYTES)?;
            self.result_bytes += bytes.len();
            // SAFETY: only this exact checked buffer is writable for the duration of the import.
            if unsafe { result_read(self.token, handle, bytes.as_mut_ptr(), len) } != len {
                return Err(NativeReadError::Protocol);
            }
            self.check_active()?;
            NativeQueryResponse::decode_for(request, &bytes).map_err(NativeReadError::Decode)
        }
    }

    fn bounded_buffer(len: i32, limit: usize) -> Result<Vec<u8>, NativeReadError> {
        if len <= 0 {
            return Err(NativeReadError::Protocol);
        }
        let len = len as usize;
        if len > limit {
            return Err(NativeReadError::Capacity);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| NativeReadError::Capacity)?;
        bytes.resize(len, 0);
        Ok(bytes)
    }
}
