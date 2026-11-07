use serde::{Deserialize, Serialize};

#[cfg(feature = "native-types")]
pub mod native;

pub const ABI_VERSION: u32 = 1;
pub const MAX_EVENT_BYTES: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderKind {
    Console,
    Player,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Enable {
        server_version: String,
    },
    Disable,
    Command {
        sender: String,
        sender_kind: SenderKind,
        #[serde(default)]
        operator: bool,
        command: String,
        args: Vec<String>,
    },
    Tick {
        tick: u64,
    },
    PlayerJoin {
        uuid: String,
        name: String,
    },
    PlayerQuit {
        uuid: String,
        name: String,
    },
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
        enum WireEvent {
            Enable {
                server_version: String,
            },
            Disable {},
            Command {
                sender: String,
                sender_kind: SenderKind,
                #[serde(default)]
                operator: bool,
                command: String,
                args: Vec<String>,
            },
            Tick {
                tick: u64,
            },
            PlayerJoin {
                uuid: String,
                name: String,
            },
            PlayerQuit {
                uuid: String,
                name: String,
            },
        }
        Ok(match WireEvent::deserialize(deserializer)? {
            WireEvent::Enable { server_version } => Self::Enable { server_version },
            WireEvent::Disable {} => Self::Disable,
            WireEvent::Command {
                sender,
                sender_kind,
                operator,
                command,
                args,
            } => Self::Command {
                sender,
                sender_kind,
                operator,
                command,
                args,
            },
            WireEvent::Tick { tick } => Self::Tick { tick },
            WireEvent::PlayerJoin { uuid, name } => Self::PlayerJoin { uuid, name },
            WireEvent::PlayerQuit { uuid, name } => Self::PlayerQuit { uuid, name },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Log {
        level: LogLevel,
        message: String,
    },
    Reply {
        message: String,
    },
    RegisterCommand {
        name: String,
        description: String,
        permission: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
pub mod guest;

#[macro_export]
macro_rules! export_plugin {
    ($handler:path) => {
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn zerqon_abi_version() -> u32 {
            $crate::ABI_VERSION
        }

        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn zerqon_on_event() -> i32 {
            $crate::guest::dispatch($handler)
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disable_rejects_unknown_fields_without_changing_abi_one_wire_shape() {
        assert_eq!(
            serde_json::to_string(&Event::Disable).unwrap(),
            r#"{"event":"disable"}"#
        );
        assert!(serde_json::from_str::<Event>(r#"{"event":"disable","operator":true}"#).is_err());
        assert!(serde_json::from_str::<Event>(r#"{"event":"disable","event":"enable"}"#).is_err());
        assert_eq!(
            serde_json::from_str::<Event>(r#"{"event":"disable"}"#).unwrap(),
            Event::Disable
        );
    }

    #[test]
    fn command_requires_explicit_sender_kind_and_keeps_player_console_distinct() {
        let json = r#"{"event":"command","sender":"CONSOLE","sender_kind":"player","command":"admin","args":[]}"#;
        let event: Event = serde_json::from_str(json).unwrap();
        assert_eq!(
            serde_json::to_value(event).unwrap()["sender_kind"],
            "player"
        );
        assert!(
            serde_json::from_str::<Event>(
                r#"{"event":"command","sender":"CONSOLE","command":"admin","args":[]}"#
            )
            .is_err()
        );
    }
}
