use serde_json::{Value, json};
use zerqon_inventory::*;
use zerqon_protocol::{
    decode_command, decode_keepalive, decode_player_abilities, decode_teleport_ack,
    encode_teleport_ack,
};

fn bytes(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn catalog() -> ItemCatalog {
    ItemCatalog::from_definitions(
        vec![ItemDef {
            id: ItemId(1),
            key: "minecraft:stone".into(),
            max_stack: 64,
            max_damage: None,
        }],
        vec![
            ComponentDef {
                id: ComponentId(1),
                key: "minecraft:max_stack_size".into(),
            },
            ComponentDef {
                id: ComponentId(6),
                key: "minecraft:custom_name".into(),
            },
        ],
    )
    .unwrap()
}

fn hashed(stack: &HashedStack, catalog: &ItemCatalog) -> Value {
    match stack {
        HashedStack::Empty => Value::Null,
        HashedStack::Item {
            item,
            count,
            added,
            removed,
        } => {
            let added: serde_json::Map<String, Value> = added
                .iter()
                .map(|(key, value)| (catalog.component(*key).unwrap().key.clone(), json!(value)))
                .collect();
            let removed: Vec<_> = removed
                .iter()
                .map(|key| catalog.component(*key).unwrap().key.clone())
                .collect();
            json!({"item":catalog.item(*item).unwrap().key,"count":count,"added":added,"removed":removed})
        }
    }
}

fn decode(
    kind: &str,
    body: &[u8],
    catalog: &ItemCatalog,
) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(match kind {
        "accept_teleportation" => {
            let packet = decode_teleport_ack(body)?;
            json!({"id":packet.id,"position":packet.xyz,"rotation":packet.rotation})
        }
        "keep_alive" => json!({"nonce":decode_keepalive(body)?}),
        "player_abilities" => json!({"flying":decode_player_abilities(body)?}),
        "chat_command" | "chat_command_signed" => {
            json!({"command":decode_command(body,kind=="chat_command_signed")?})
        }
        "set_carried_item" => json!({"slot":decode_carried_slot(body)?}),
        "set_creative_mode_slot" => {
            let packet = decode_creative_slot(body, catalog, DecodeLimits::default())?;
            let stack=packet.stack.map_or(Value::Null,|stack|json!({"item":catalog.item(stack.item()).unwrap().key,"count":stack.count(),"added":{},"removed":[]}));
            json!({"slot":packet.slot,"stack":stack})
        }
        "container_click" => {
            let packet = decode_click(body, catalog, DecodeLimits::default())?;
            let changed: Vec<_> = packet
                .changed_slots
                .iter()
                .map(|(slot, stack)| json!({"slot":slot,"stack":hashed(stack,catalog)}))
                .collect();
            json!({"container_id":packet.container_id,"state_id":packet.state_id,"slot":packet.slot,"button":packet.button,"mode":packet.mode as u8,"changed_slots":changed,"carried":hashed(&packet.carried,catalog)})
        }
        "player_action" => {
            let packet = decode_player_action(body)?;
            json!({"action":packet.action as u8,"block":[f64::from(packet.block.x),f64::from(packet.block.y),f64::from(packet.block.z)],"face":packet.face as u8,"sequence":packet.sequence})
        }
        "use_item_on" => {
            let packet = decode_use_on(body)?;
            json!({"hand":packet.hand as u8,"position":[packet.block.x,packet.block.y,packet.block.z],"face":packet.face as u8,"hit":packet.hit,"inside":packet.inside,"world_border_hit":packet.world_border_hit,"sequence":packet.sequence})
        }
        _ => return Err("No shared Rust decoder for this kind".into()),
    })
}

#[test]
fn original_client_packet_goldens_match_shared_rust_decoders() {
    let document: Value =
        serde_json::from_str(include_str!("client_packet_oracle/fixtures.json")).unwrap();
    let catalog = catalog();
    let mut checked = 0;
    for entry in document["cases"].as_array().unwrap() {
        let kind = entry["kind"].as_str().unwrap();
        if kind.starts_with("move_player_") || kind == "attack" {
            continue;
        }
        let body = bytes(entry["body"].as_str().unwrap());
        let actual = decode(kind, &body, &catalog).unwrap();
        let expected = if kind == "chat_command_signed" {
            json!({"command":entry["fields"]["command"]})
        } else {
            entry["fields"].clone()
        };
        assert_eq!(actual, expected, "{}", entry["name"]);
        if kind == "accept_teleportation" {
            assert_eq!(
                encode_teleport_ack(&decode_teleport_ack(&body).unwrap()).unwrap(),
                body
            );
        }
        for end in 0..body.len() {
            assert!(
                decode(kind, &body[..end], &catalog).is_err(),
                "{} prefix{end}",
                entry["name"]
            );
        }
        let mut appended = body;
        appended.push(0);
        assert!(
            decode(kind, &appended, &catalog).is_err(),
            "{} trailing",
            entry["name"]
        );
        checked += 1;
    }
    assert_eq!(checked, 80);
}
