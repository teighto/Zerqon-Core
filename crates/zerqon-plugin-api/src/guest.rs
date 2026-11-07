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
