use uuid::Uuid;
use zerqon_protocol::codec::{put_string, put_varint};

pub fn player_info_add(uuid: Uuid, name: &str, listed: bool) -> Vec<u8> {
    player_info_add_properties(uuid, name, listed, &[])
}

pub fn player_info_add_properties(
    uuid: Uuid,
    name: &str,
    listed: bool,
    properties: &[(&str, &str, Option<&str>)],
) -> Vec<u8> {
    let mut packet = vec![71, 0x9d, 1];
    packet.extend_from_slice(uuid.as_bytes());
    put_string(name, &mut packet);
    put_varint(properties.len() as i32, &mut packet);
    for &(name, value, signature) in properties {
        put_string(name, &mut packet);
        put_string(value, &mut packet);
        packet.push(u8::from(signature.is_some()));
        if let Some(signature) = signature {
            put_string(signature, &mut packet);
        }
    }
    packet.extend_from_slice(&[1, u8::from(listed), 0, 1]);
    packet
}

pub fn player_info_remove(players: &[Uuid]) -> Vec<u8> {
    let mut packet = vec![70];
    put_varint(players.len() as i32, &mut packet);
    for player in players {
        packet.extend_from_slice(player.as_bytes());
    }
    packet
}

pub fn add_entity(
    id: i32,
    uuid: Uuid,
    type_id: i32,
    position: [f64; 3],
    yaw: f32,
    pitch: f32,
) -> Vec<u8> {
    let mut packet = vec![1];
    put_varint(id, &mut packet);
    packet.extend_from_slice(uuid.as_bytes());
    put_varint(type_id, &mut packet);
    for value in position {
        packet.extend_from_slice(&value.to_be_bytes());
    }
    packet.extend_from_slice(&[0, angle(pitch), angle(yaw), angle(yaw), 0]);
    packet
}

pub fn position_sync(
    id: i32,
    position: [f64; 3],
    yaw: f32,
    pitch: f32,
    on_ground: bool,
) -> Vec<u8> {
    let mut packet = vec![35];
    put_varint(id, &mut packet);
    packet.push(0);
    for value in position {
        packet.extend_from_slice(&value.to_be_bytes());
    }
    packet.extend_from_slice(&yaw.to_be_bytes());
    packet.extend_from_slice(&pitch.to_be_bytes());
    packet.push(u8::from(on_ground));
    packet
}

pub fn head_rotation(id: i32, yaw: f32) -> Vec<u8> {
    let mut packet = vec![85];
    put_varint(id, &mut packet);
    packet.push(angle(yaw));
    packet
}

fn angle(degrees: f32) -> u8 {
    (degrees * 256.0 / 360.0).floor() as i32 as u8
}

pub fn remove_entities(ids: &[i32]) -> Vec<u8> {
    let mut packet = vec![78];
    put_varint(ids.len() as i32, &mut packet);
    for &id in ids {
        put_varint(id, &mut packet);
    }
    packet
}

pub fn player_metadata(id: i32, skin_layers: u8) -> Vec<u8> {
    let mut packet = vec![101];
    put_varint(id, &mut packet);
    packet.extend_from_slice(&[16, 0, skin_layers & 0x7f, 255]);
    packet
}

pub fn health(health: f32, food: i32, saturation: f32) -> Vec<u8> {
    let mut packet = vec![106];
    packet.extend_from_slice(&health.to_be_bytes());
    put_varint(food, &mut packet);
    packet.extend_from_slice(&saturation.to_be_bytes());
    packet
}

pub fn mob_health(id: i32, health: f32) -> Vec<u8> {
    let mut packet = vec![101];
    put_varint(id, &mut packet);
    packet.extend_from_slice(&[9, 3]);
    packet.extend_from_slice(&health.to_be_bytes());
    packet.push(255);
    packet
}
