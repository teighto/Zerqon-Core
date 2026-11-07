use zerqon_plugin_api::{Action, Event, LogLevel};

pub fn handle(event: Event) -> Vec<Action> {
    match event {
        Event::Enable { .. } => vec![
            Action::Log {
                level: LogLevel::Info,
                message: "Hello Rust plugin enabled".into(),
            },
            Action::RegisterCommand {
                name: "hello".into(),
                description: "Say hello from a Rust plugin".into(),
                permission: None,
            },
        ],
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
