use crate::game_mode::GameMode;
use uuid::Uuid;
use zerqon_inventory::{ItemCatalog, ItemError, Slot, encode_server_stack};
use zerqon_protocol::{
    codec::{put_string, put_varint},
    put_lp_vec3,
};

#[derive(Debug, thiserror::Error)]
pub enum EntityPacketError {
    #[error("invalid entity packet state: {0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Item(#[from] ItemError),
}

pub fn player_info_add(uuid: Uuid, name: &str, listed: bool, game_mode: GameMode) -> Vec<u8> {
    player_info_add_properties(uuid, name, listed, &[], game_mode)
}

pub fn player_info_add_properties(
    uuid: Uuid,
    name: &str,
    listed: bool,
    properties: &[(&str, &str, Option<&str>)],
    game_mode: GameMode,
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
    put_varint(i32::from(game_mode), &mut packet);
    packet.extend_from_slice(&[u8::from(listed), 0, 1]);
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
    let mut packet = entity_header(id, uuid, type_id, position);
    packet.extend_from_slice(&[0, angle(pitch), angle(yaw), angle(yaw), 0]);
    packet
}

pub fn add_entity_with_motion(
    id: i32,
    uuid: Uuid,
    type_id: i32,
    position: [f64; 3],
    velocity: [f64; 3],
    yaw: f32,
    pitch: f32,
) -> Result<Vec<u8>, EntityPacketError> {
    validate_transform(id, position, yaw, pitch)?;
    if type_id < 0 {
        return Err(EntityPacketError::Invalid("negative entity type"));
    }
    validate_velocity(velocity)?;
    let mut packet = entity_header(id, uuid, type_id, position);
    put_lp_vec3(velocity, &mut packet);
    packet.extend_from_slice(&[angle(pitch), angle(yaw), angle(yaw), 0]);
    Ok(packet)
}

fn entity_header(id: i32, uuid: Uuid, type_id: i32, position: [f64; 3]) -> Vec<u8> {
    let mut packet = vec![1];
    put_varint(id, &mut packet);
    packet.extend_from_slice(uuid.as_bytes());
    put_varint(type_id, &mut packet);
    for value in position {
        packet.extend_from_slice(&value.to_be_bytes());
    }
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

pub fn try_position_sync(
    id: i32,
    position: [f64; 3],
    yaw: f32,
    pitch: f32,
    on_ground: bool,
) -> Result<Vec<u8>, EntityPacketError> {
    validate_transform(id, position, yaw, pitch)?;
    Ok(position_sync(id, position, yaw, pitch, on_ground))
}

pub fn entity_motion(id: i32, velocity: [f64; 3]) -> Result<Vec<u8>, EntityPacketError> {
    validate_entity_id(id)?;
    validate_velocity(velocity)?;
    let mut packet = vec![103];
    put_varint(id, &mut packet);
    put_lp_vec3(velocity, &mut packet);
    Ok(packet)
}

pub fn hurt_animation(id: i32, yaw: f32) -> Result<Vec<u8>, EntityPacketError> {
    validate_entity_id(id)?;
    if !yaw.is_finite() {
        return Err(EntityPacketError::Invalid("nonfinite hurt direction"));
    }
    let mut packet = vec![43];
    put_varint(id, &mut packet);
    packet.extend_from_slice(&yaw.to_be_bytes());
    Ok(packet)
}

pub fn item_metadata(
    id: i32,
    stack: &Slot,
    catalog: &ItemCatalog,
) -> Result<Vec<u8>, EntityPacketError> {
    validate_entity_id(id)?;
    let mut packet = vec![101];
    put_varint(id, &mut packet);
    packet.extend_from_slice(&[8, 7]);
    encode_server_stack(stack, catalog, &mut packet)?;
    packet.push(255);
    Ok(packet)
}

pub fn take_item_entity(
    item_id: i32,
    collector_id: i32,
    amount: i32,
) -> Result<Vec<u8>, EntityPacketError> {
    validate_entity_id(item_id)?;
    validate_entity_id(collector_id)?;
    if amount <= 0 {
        return Err(EntityPacketError::Invalid("nonpositive collected amount"));
    }
    let mut packet = vec![127];
    for value in [item_id, collector_id, amount] {
        put_varint(value, &mut packet);
    }
    Ok(packet)
}

fn validate_entity_id(id: i32) -> Result<(), EntityPacketError> {
    if id <= 0 {
        Err(EntityPacketError::Invalid("nonpositive entity ID"))
    } else {
        Ok(())
    }
}

fn validate_velocity(velocity: [f64; 3]) -> Result<(), EntityPacketError> {
    if velocity.iter().any(|value| !value.is_finite()) {
        Err(EntityPacketError::Invalid("nonfinite velocity"))
    } else {
        Ok(())
    }
}

fn validate_transform(
    id: i32,
    position: [f64; 3],
    yaw: f32,
    pitch: f32,
) -> Result<(), EntityPacketError> {
    validate_entity_id(id)?;
    if position.iter().any(|value| !value.is_finite()) || !yaw.is_finite() || !pitch.is_finite() {
        Err(EntityPacketError::Invalid("nonfinite position or rotation"))
    } else {
        Ok(())
    }
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

pub fn entity_event(id: i32, event: u8) -> Vec<u8> {
    let mut packet = vec![34];
    packet.extend_from_slice(&id.to_be_bytes());
    packet.push(event);
    packet
}

pub fn animal_age(id: i32, baby: bool) -> Vec<u8> {
    let mut packet = vec![101];
    put_varint(id, &mut packet);
    packet.extend_from_slice(&[16, 8, u8::from(baby), 255]);
    packet
}
pub fn no_gravity(id: i32, value: bool) -> Vec<u8> {
    let mut packet = vec![101];
    put_varint(id, &mut packet);
    packet.extend_from_slice(&[5, 8, u8::from(value), 255]);
    packet
}
