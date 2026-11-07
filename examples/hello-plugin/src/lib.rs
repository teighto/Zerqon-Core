use zerqon_plugin_api::{Action, Event, LogLevel};

pub fn handle(event: Event) -> Vec<Action> {
    match event {
        Event::Enable { .. } => {
            let mut actions = vec![
                Action::Log {
                    level: LogLevel::Info,
                    message: "Hello Rust plugin enabled".into(),
                },
                Action::RegisterCommand {
                    name: "hello".into(),
                    description: "Say hello from a Rust plugin".into(),
                    permission: None,
                },
            ];
            if cfg!(all(feature = "native-read", target_arch = "wasm32")) {
                actions.push(Action::RegisterCommand {
                    name: "rwhere".into(),
                    description: "Read your player session and world through the native API".into(),
                    permission: None,
                });
            }
            actions
        }
        #[cfg(all(feature = "native-read", target_arch = "wasm32"))]
        Event::Command { command, .. } if command == "rwhere" => vec![Action::Reply {
            message: where_reply().unwrap_or_else(|error| format!("Native read failed: {error}")),
        }],
        Event::Command {
            sender, command, ..
        } if command == "hello" => vec![Action::Reply {
            message: format!("Hello, {sender}! This plugin is written in Rust."),
        }],
        Event::Disable => vec![Action::Log {
            level: LogLevel::Info,
            message: "Hello Rust plugin disabled".into(),
        }],
        _ => Vec::new(),
    }
}

#[cfg(all(feature = "native-read", target_arch = "wasm32"))]
fn where_reply() -> Result<String, Box<dyn std::error::Error>> {
    use zerqon_plugin_api::{guest::NativeReads, native::NativeSource};
    let mut reads = NativeReads::current()?;
    let (uuid, session_id) = match reads.source()? {
        NativeSource::Player { uuid, session_id } => (*uuid, *session_id),
        NativeSource::Server | NativeSource::Console => {
            return Ok("This command requires a player.".into());
        }
    };
    let player = reads
        .player(uuid, Some(session_id))?
        .ok_or("Player session is no longer active")?;
    let server = reads.server_snapshot()?;
    let world = reads
        .worlds()?
        .iter()
        .find(|world| world.uuid == player.world_uuid)
        .ok_or("Player world is unavailable")?;
    Ok(format!(
        "{}: uuid={}, session={}, world={}({}), position=({},{},{}), online={}/{}, protocol={}",
        player.name,
        player.uuid,
        player.session_id,
        world.name,
        world.uuid,
        player.position.x,
        player.position.y,
        player.position.z,
        server.online_players,
        server.max_players,
        server.protocol,
    ))
}

zerqon_plugin_api::export_plugin!(handle);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_registers_hello_command() {
        let actions = handle(Event::Enable {
            server_version: "0.0.1".into(),
        });
        assert!(actions.iter().any(
            |action| matches!(action, Action::RegisterCommand { name, .. } if name == "hello")
        ));
    }

    #[test]
    fn hello_replies_to_command_sender() {
        let actions = handle(Event::Command {
            sender: "Teighto".into(),
            sender_kind: zerqon_plugin_api::SenderKind::Player,
            operator: false,
            command: "hello".into(),
            args: vec![],
        });
        assert_eq!(
            actions,
            vec![Action::Reply {
                message: "Hello, Teighto! This plugin is written in Rust.".into()
            }]
        );
    }

    #[test]
    fn unrelated_commands_produce_no_actions() {
        assert!(
            handle(Event::Command {
                sender: "Teighto".into(),
                sender_kind: zerqon_plugin_api::SenderKind::Player,
                operator: false,
                command: "other".into(),
                args: vec![]
            })
            .is_empty()
        );
    }
}
