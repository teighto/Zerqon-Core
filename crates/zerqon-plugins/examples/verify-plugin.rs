use std::error::Error;
use zerqon_plugin_api::{Action, Event};
use zerqon_plugins::{Limits, PluginManager};

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("usage: verify-plugin <hello-plugin.wasm>")?;
    let bytes = std::fs::read(path)?;
    let mut manager = PluginManager::new(Limits::default())?;
    let enable = manager.load("hello", &bytes)?;
    if !enable
        .iter()
        .any(|action| matches!(action, Action::RegisterCommand { name, .. } if name == "hello"))
    {
        return Err("plugin did not register the hello command".into());
    }
    for tick in 0..1000 {
        if !manager.dispatch("hello", &Event::Tick { tick })?.is_empty() {
            return Err("tick unexpectedly produced actions".into());
        }
    }
    let actions = manager.dispatch(
        "hello",
        &Event::Command {
            sender: "Teighto".into(),
            sender_kind: zerqon_plugin_api::SenderKind::Player,
            operator: false,
            command: "hello".into(),
            args: Vec::new(),
        },
    )?;
    if actions
        != vec![Action::Reply {
            message: "Hello, Teighto! This plugin is written in Rust.".into(),
        }]
    {
        return Err("unexpected command response".into());
    }
    if manager.unload("hello")?.len() != 1 || !manager.list().is_empty() {
        return Err("plugin was not cleanly unloaded".into());
    }
    println!("Rust WASM plugin: enable, command, 1000 ticks and disable passed");
    Ok(())
}
