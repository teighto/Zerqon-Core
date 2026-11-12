use crate::{game_data::GameData, game_mode::GameMode};
use std::collections::HashMap;
use uuid::Uuid;
use zerqon_protocol::codec::{put_string, put_varint};
use zerqon_world::{Chunk, SECTION_COUNT, SECTION_VOLUME, Section};

#[derive(Debug, thiserror::Error)]
pub enum PacketError {
    #[error("unknown block state")]
    UnknownState,
}

pub fn known_packs() -> Vec<u8> {
    let mut out = vec![15, 1];
    for value in ["minecraft", "core", "26.3"] {
        put_string(value, &mut out);
    }
    out
}

pub fn configuration_brand(brand: &str) -> Vec<u8> {
    let mut out = vec![1];
    put_string("minecraft:brand", &mut out);
    put_string(brand, &mut out);
    out
}

pub fn login(
    entity_id: i32,
    max_players: i32,
    view_distance: i32,
    dimension_type: i32,
    game_mode: GameMode,
    previous: Option<GameMode>,
    online_mode: bool,
) -> Vec<u8> {
    let mut out = vec![50];
    out.extend_from_slice(&entity_id.to_be_bytes());
    out.extend_from_slice(&[0, 1]);
    put_string("minecraft:overworld", &mut out);
    for value in [max_players, view_distance, view_distance] {
        put_varint(value, &mut out);
    }
    out.extend_from_slice(&[0, 1, 0]);
    put_varint(dimension_type, &mut out);
    put_string("minecraft:overworld", &mut out);
    out.extend_from_slice(&0_i64.to_be_bytes());
    put_varint(i32::from(game_mode), &mut out);
    put_varint(previous.map_or(0, |mode| i32::from(mode) + 1), &mut out);
    out.extend_from_slice(&[0, 1, 0, 0, 63, u8::from(online_mode), 0]);
    out
}

pub fn position(teleport: i32, x: f64, y: f64, z: f64) -> Vec<u8> {
    position_rotated(teleport, [x, y, z], [0.0, 0.0])
}

pub fn position_rotated(teleport: i32, xyz: [f64; 3], rotation: [f32; 2]) -> Vec<u8> {
    let mut out = vec![73];
    put_varint(teleport, &mut out);
    for value in [xyz[0], xyz[1], xyz[2], 0.0, 0.0, 0.0] {
        out.extend_from_slice(&value.to_be_bytes());
    }
    for value in rotation {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out.extend_from_slice(&[0; 4]);
    out
}

pub fn abilities(game_mode: GameMode, flying: bool) -> Vec<u8> {
    abilities_with_flight_speed(
        game_mode,
        flying,
        crate::flight_speed::FlightSpeed::default(),
    )
}

pub fn abilities_with_flight_speed(
    game_mode: GameMode,
    flying: bool,
    speed: crate::flight_speed::FlightSpeed,
) -> Vec<u8> {
    let flags = if game_mode == GameMode::Creative {
        0x0d | (u8::from(flying) << 1)
    } else {
        0
    };
    let mut out = vec![65, flags];
    out.extend_from_slice(&speed.wire().to_be_bytes());
    out.extend_from_slice(&0.1_f32.to_be_bytes());
    out
}

pub(crate) fn text_component(text: &str, out: &mut Vec<u8>) {
    let mut encoded = Vec::with_capacity(text.len().min(65535));
    for character in text.chars() {
        let mut utf16 = [0_u16; 2];
        let mut bytes = [0_u8; 6];
        let mut count = 0;
        for &unit in character.encode_utf16(&mut utf16).iter() {
            if unit > 0 && unit <= 0x7f {
                bytes[count] = unit as u8;
                count += 1;
            } else if unit <= 0x7ff {
                bytes[count] = 0xc0 | (unit >> 6) as u8;
                bytes[count + 1] = 0x80 | (unit & 0x3f) as u8;
                count += 2;
            } else {
                bytes[count] = 0xe0 | (unit >> 12) as u8;
                bytes[count + 1] = 0x80 | ((unit >> 6) & 0x3f) as u8;
                bytes[count + 2] = 0x80 | (unit & 0x3f) as u8;
                count += 3;
            }
        }
        if encoded.len() + count > u16::MAX as usize {
            break;
        }
        encoded.extend_from_slice(&bytes[..count]);
    }
    out.push(8);
    out.extend_from_slice(&(encoded.len() as u16).to_be_bytes());
    out.extend_from_slice(&encoded);
}

pub fn system_chat(text: &str) -> Vec<u8> {
    let mut out = vec![124];
    text_component(text, &mut out);
    out.push(0);
    out
}

fn boss_packet(id: Uuid, operation: u8) -> Vec<u8> {
    let mut out = vec![9];
    out.extend_from_slice(id.as_bytes());
    out.push(operation);
    out
}

fn progress(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

pub fn bossbar_add(id: Uuid, text: &str, value: f32) -> Vec<u8> {
    let mut out = boss_packet(id, 0);
    text_component(text, &mut out);
    out.extend_from_slice(&progress(value).to_be_bytes());
    out.extend_from_slice(&[3, 0, 0]);
    out
}

pub fn bossbar_update(id: Uuid, text: &str, value: f32) -> Vec<Vec<u8>> {
    let mut title = boss_packet(id, 3);
    text_component(text, &mut title);
    let mut health = boss_packet(id, 2);
    health.extend_from_slice(&progress(value).to_be_bytes());
    vec![title, health]
}

pub fn bossbar_remove(id: Uuid) -> Vec<u8> {
    boss_packet(id, 1)
}

pub fn chunk(chunk: &Chunk, data: &GameData) -> Result<Vec<u8>, PacketError> {
    chunk_with_generator(chunk, data, None)
}

pub fn chunk_with_generator(
    chunk: &Chunk,
    data: &GameData,
    generator: Option<zerqon_world::NormalGenerator>,
) -> Result<Vec<u8>, PacketError> {
    let mut biome_data = Vec::new();
    if let Some(generator) = generator {
        let mut palette = Vec::new();
        let indices: [u32; 16] = std::array::from_fn(|index| {
            let biome = generator.biome_at(
                i64::from(chunk.x) * 16 + (index % 4) as i64 * 4,
                i64::from(chunk.z) * 16 + (index / 4) as i64 * 4,
            );
            if let Some(index) = palette.iter().position(|id| *id == biome) {
                index as u32
            } else {
                palette.push(biome);
                (palette.len() - 1) as u32
            }
        });
        if palette.len() == 1 {
            biome_data.push(0);
            put_varint(palette[0], &mut biome_data);
        } else {
            // The seven native biome kinds fit the protocol's 1..=3 bit palette.
            let bits = (usize::BITS - (palette.len() - 1).leading_zeros()) as u8;
            biome_data.push(bits);
            put_varint(palette.len() as i32, &mut biome_data);
            for biome in palette {
                put_varint(biome, &mut biome_data);
            }
            // PalettedContainer order is X, then Z, then Y (4*4*4 quart cells).
            pack_values(
                (0..64).map(|index| indices[index % 16]),
                bits,
                &mut biome_data,
            );
        }
    } else {
        biome_data.push(0);
        put_varint(data.plains_biome, &mut biome_data);
    }
    let mut section_data = Vec::with_capacity(8192);
    let mut heights = [0_u32; 256];
    for (section_index, section) in chunk.sections().iter().enumerate() {
        let mut non_air = 0_u16;
        let mut fluids = 0_u16;
        if let Some(state) = section.uniform_state() {
            if !data.is_valid_state(state) {
                return Err(PacketError::UnknownState);
            }
            if !data.is_air(state) {
                non_air = SECTION_VOLUME as u16;
                heights.fill(((section_index + 1) * 16) as u32);
            }
            if data.has_fluid(state) {
                fluids = SECTION_VOLUME as u16;
            }
        } else {
            for (index, state) in section.iter().enumerate() {
                if !data.is_valid_state(state) {
                    return Err(PacketError::UnknownState);
                }
                if !data.is_air(state) {
                    non_air += 1;
                    heights[index % 256] = (section_index * 16 + index / 256 + 1) as u32;
                }
                fluids += u16::from(data.has_fluid(state));
            }
        }
        section_data.extend_from_slice(&non_air.to_be_bytes());
        section_data.extend_from_slice(&fluids.to_be_bytes());
        encode_section(section, data.block_bits, &mut section_data);
        section_data.extend_from_slice(&biome_data);
    }
    let mut out = vec![46];
    out.extend_from_slice(&chunk.x.to_be_bytes());
    out.extend_from_slice(&chunk.z.to_be_bytes());
    out.push(2);
    for kind in [1, 4] {
        out.push(kind);
        put_varint(37, &mut out);
        pack_values(heights.iter().copied(), 9, &mut out);
    }
    put_varint(section_data.len() as i32, &mut out);
    out.extend_from_slice(&section_data);
    out.push(0);
    encode_skylight(&heights, &mut out);
    Ok(out)
}

fn encode_section(section: &Section, global_bits: u8, out: &mut Vec<u8>) {
    if let Some(state) = section.uniform_state() {
        out.push(0);
        put_varint(state as i32, out);
        return;
    }
    let mut palette = Vec::new();
    let mut lookup = HashMap::new();
    let indices: Vec<u32> = section
        .iter()
        .map(|state| {
            *lookup.entry(state).or_insert_with(|| {
                let index = palette.len() as u32;
                palette.push(state);
                index
            })
        })
        .collect();
    if palette.len() == 1 {
        out.push(0);
        put_varint(palette[0] as i32, out);
        return;
    }
    if palette.len() <= 256 {
        let bits = ((usize::BITS - (palette.len() - 1).leading_zeros()) as u8).max(4);
        out.push(bits);
        put_varint(palette.len() as i32, out);
        for state in palette {
            put_varint(state as i32, out);
        }
        pack_values(indices.into_iter(), bits, out);
    } else {
        out.push(global_bits);
        pack_values(section.iter(), global_bits, out);
    }
}

fn pack_values(values: impl Iterator<Item = u32>, bits: u8, out: &mut Vec<u8>) {
    let per_word = 64 / usize::from(bits);
    let mut word = 0_u64;
    let mut used = 0;
    for value in values {
        word |= u64::from(value) << (used * usize::from(bits));
        used += 1;
        if used == per_word {
            out.extend_from_slice(&word.to_be_bytes());
            word = 0;
            used = 0;
        }
    }
    if used != 0 {
        out.extend_from_slice(&word.to_be_bytes());
    }
}

fn bitset(value: u64, out: &mut Vec<u8>) {
    let length = (u64::BITS - value.leading_zeros()).div_ceil(8) as usize;
    put_varint(length as i32, out);
    out.extend_from_slice(&value.to_le_bytes()[..length]);
}

fn encode_skylight(heights: &[u32; 256], out: &mut Vec<u8>) {
    let mut sky_mask = 0_u64;
    let mut empty_sky = 0_u64;
    let mut layers = Vec::new();
    for section in 0..SECTION_COUNT + 2 {
        let mut layer = [0_u8; SECTION_VOLUME / 2];
        for index in 0..SECTION_VOLUME {
            let y = (section as i32 - 1) * 16 + (index / 256) as i32;
            if y >= heights[index % 256] as i32 {
                layer[index / 2] |= 15 << ((index % 2) * 4);
            }
        }
        if layer.iter().all(|byte| *byte == 0) {
            empty_sky |= 1 << section;
        } else {
            sky_mask |= 1 << section;
            layers.push(layer);
        }
    }
    bitset(sky_mask, out);
    bitset(0, out);
    bitset(empty_sky, out);
    bitset((1 << (SECTION_COUNT + 2)) - 1, out);
    put_varint(layers.len() as i32, out);
    for layer in layers {
        put_varint(layer.len() as i32, out);
        out.extend_from_slice(&layer);
    }
    out.push(0);
}

pub fn chunk_center(x: i32, z: i32) -> Vec<u8> {
    let mut out = vec![96];
    put_varint(x, &mut out);
    put_varint(z, &mut out);
    out
}

pub fn chunk_batch_start() -> Vec<u8> {
    vec![12]
}
pub fn chunk_batch_finished(count: i32) -> Vec<u8> {
    let mut out = vec![11];
    put_varint(count, &mut out);
    out
}

pub fn default_spawn(x: i32, y: i32, z: i32) -> Vec<u8> {
    world_spawn("minecraft:overworld", [x, y, z], [0.0, 0.0])
}

pub fn world_spawn(dimension: &str, [x, y, z]: [i32; 3], rotation: [f32; 2]) -> Vec<u8> {
    let mut out = vec![99];
    put_string(dimension, &mut out);
    let packed =
        ((x as u64 & 0x3ffffff) << 38) | ((z as u64 & 0x3ffffff) << 12) | (y as u64 & 0xfff);
    out.extend_from_slice(&packed.to_be_bytes());
    for value in rotation {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out
}

pub fn game_event(event: u8, param: f32) -> Vec<u8> {
    let mut out = vec![39, event];
    out.extend_from_slice(&param.to_be_bytes());
    out
}

pub fn command_tree(commands: &[&str]) -> Vec<u8> {
    let mut out = vec![16];
    put_varint((commands.len() * 2 + 1) as i32, &mut out);
    out.push(0);
    put_varint(commands.len() as i32, &mut out);
    for index in 0..commands.len() {
        put_varint((index * 2 + 1) as i32, &mut out);
    }
    for (index, command) in commands.iter().enumerate() {
        out.extend_from_slice(&[5, 1]);
        put_varint((index * 2 + 2) as i32, &mut out);
        put_string(command, &mut out);
        out.extend_from_slice(&[6, 0]);
        put_string("args", &mut out);
        out.extend_from_slice(&[5, 2]);
    }
    out.push(0);
    out
}
