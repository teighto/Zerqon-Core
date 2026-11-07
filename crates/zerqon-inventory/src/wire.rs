use crate::*;
use std::collections::{BTreeMap, BTreeSet};
use zerqon_protocol::codec::{get_string, get_varint, put_string, put_varint, take};

pub fn decode_creative_slot(
    mut data: &[u8],
    catalog: &ItemCatalog,
    limits: DecodeLimits,
) -> Result<CreativeSlotRequest, ItemError> {
    validate_limits(data, limits)?;
    let slot = i16::from_be_bytes(array(&mut data)?);
    let stack = read_stack(&mut data, catalog, limits, true)?;
    finished(data)?;
    Ok(CreativeSlotRequest { slot, stack })
}

pub fn decode_client_stack(
    mut data: &[u8],
    catalog: &ItemCatalog,
    limits: DecodeLimits,
) -> Result<Slot, ItemError> {
    validate_limits(data, limits)?;
    let stack = read_stack(&mut data, catalog, limits, true)?;
    finished(data)?;
    Ok(stack)
}

pub fn decode_server_stack(
    mut data: &[u8],
    catalog: &ItemCatalog,
    limits: DecodeLimits,
) -> Result<Slot, ItemError> {
    validate_limits(data, limits)?;
    let stack = read_stack(&mut data, catalog, limits, false)?;
    finished(data)?;
    Ok(stack)
}

pub fn decode_click(
    mut data: &[u8],
    catalog: &ItemCatalog,
    limits: DecodeLimits,
) -> Result<ClickRequest, ItemError> {
    validate_limits(data, limits)?;
    let container_id = get_varint(&mut data)?;
    let state_id = get_varint(&mut data)?;
    let slot = i16::from_be_bytes(array(&mut data)?);
    let button = take(&mut data, 1)?[0] as i8;
    let mode = match get_varint(&mut data)? {
        0 => ClickMode::Pickup,
        1 => ClickMode::QuickMove,
        2 => ClickMode::Swap,
        3 => ClickMode::Clone,
        4 => ClickMode::Throw,
        5 => ClickMode::QuickCraft,
        6 => ClickMode::PickupAll,
        _ => return Err(ItemError::Invalid("unknown click mode")),
    };
    let count = length(&mut data, 128, "changed slot count")?;
    let mut changed_slots = Vec::with_capacity(count);
    let mut seen = BTreeSet::new();
    for _ in 0..count {
        let index = i16::from_be_bytes(array(&mut data)?);
        if !seen.insert(index) {
            return Err(ItemError::Invalid("duplicate changed slot"));
        }
        changed_slots.push((index, read_hashed(&mut data, catalog, limits)?));
    }
    let carried = read_hashed(&mut data, catalog, limits)?;
    finished(data)?;
    Ok(ClickRequest {
        container_id,
        state_id,
        slot,
        button,
        mode,
        changed_slots,
        carried,
    })
}

pub fn decode_carried_slot(mut data: &[u8]) -> Result<u8, ItemError> {
    let slot = i16::from_be_bytes(array(&mut data)?);
    finished(data)?;
    if !(0..=8).contains(&slot) {
        return Err(ItemError::Invalid("hotbar slot must be 0..8"));
    }
    Ok(slot as u8)
}

pub fn decode_use_on(mut data: &[u8]) -> Result<UseOnRequest, ItemError> {
    let hand = match get_varint(&mut data)? {
        0 => Hand::Main,
        1 => Hand::Off,
        _ => return Err(ItemError::Invalid("invalid hand")),
    };
    let packed = i64::from_be_bytes(array(&mut data)?);
    let block = BlockPosition {
        x: (packed >> 38) as i32,
        y: (packed << 52 >> 52) as i32,
        z: (packed << 26 >> 38) as i32,
    };
    let face = match get_varint(&mut data)? {
        0 => Face::Down,
        1 => Face::Up,
        2 => Face::North,
        3 => Face::South,
        4 => Face::West,
        5 => Face::East,
        _ => return Err(ItemError::Invalid("invalid block face")),
    };
    let mut hit = [0.0; 3];
    for axis in &mut hit {
        *axis = f32::from_be_bytes(array(&mut data)?);
        if !axis.is_finite() || (f64::from(*axis) - 0.5).abs() >= 1.000_000_1 {
            return Err(ItemError::Invalid("invalid block hit coordinate"));
        }
    }
    let inside = boolean(&mut data)?;
    let world_border_hit = boolean(&mut data)?;
    let sequence = unsigned(&mut data)?;
    finished(data)?;
    Ok(UseOnRequest {
        hand,
        block,
        face,
        hit,
        inside,
        world_border_hit,
        sequence,
    })
}

pub fn encode_server_stack(
    stack: &Slot,
    catalog: &ItemCatalog,
    output: &mut Vec<u8>,
) -> Result<(), ItemError> {
    write_stack(stack, catalog, output, false)
}
pub fn encode_client_stack(
    stack: &Slot,
    catalog: &ItemCatalog,
    output: &mut Vec<u8>,
) -> Result<(), ItemError> {
    write_stack(stack, catalog, output, true)
}

pub fn encode_creative_slot_body(
    request: &CreativeSlotRequest,
    catalog: &ItemCatalog,
) -> Result<Vec<u8>, ItemError> {
    let mut out = request.slot.to_be_bytes().to_vec();
    encode_client_stack(&request.stack, catalog, &mut out)?;
    Ok(out)
}

pub fn encode_content_body(
    container: i32,
    state: u16,
    items: &[Slot],
    cursor: &Slot,
    catalog: &ItemCatalog,
) -> Result<Vec<u8>, ItemError> {
    if items.len() > 128 || state > 32767 {
        return Err(ItemError::Limit("container content"));
    }
    let mut out = Vec::new();
    put_varint(container, &mut out);
    put_varint(i32::from(state), &mut out);
    put_varint(items.len() as i32, &mut out);
    for item in items {
        encode_server_stack(item, catalog, &mut out)?;
    }
    encode_server_stack(cursor, catalog, &mut out)?;
    if out.len() > 2_097_151 {
        return Err(ItemError::Limit("container packet bytes"));
    }
    Ok(out)
}

pub fn encode_slot_body(
    container: i32,
    state: u16,
    slot: i16,
    item: &Slot,
    catalog: &ItemCatalog,
) -> Result<Vec<u8>, ItemError> {
    if state > 32767 {
        return Err(ItemError::Invalid("state ID"));
    }
    let mut out = Vec::new();
    put_varint(container, &mut out);
    put_varint(i32::from(state), &mut out);
    out.extend_from_slice(&slot.to_be_bytes());
    encode_server_stack(item, catalog, &mut out)?;
    Ok(out)
}

pub fn encode_cursor_body(item: &Slot, catalog: &ItemCatalog) -> Result<Vec<u8>, ItemError> {
    let mut out = Vec::new();
    encode_server_stack(item, catalog, &mut out)?;
    Ok(out)
}
pub fn encode_held_slot_body(slot: u8) -> Result<Vec<u8>, ItemError> {
    if slot > 8 {
        return Err(ItemError::Invalid("hotbar slot"));
    }
    Ok(vec![slot])
}
pub fn encode_player_inventory_body(
    slot: u8,
    item: &Slot,
    catalog: &ItemCatalog,
) -> Result<Vec<u8>, ItemError> {
    if slot > 42 {
        return Err(ItemError::Invalid("inventory index"));
    }
    let mut out = Vec::new();
    put_varint(i32::from(slot), &mut out);
    encode_server_stack(item, catalog, &mut out)?;
    Ok(out)
}

fn validate_limits(data: &[u8], limits: DecodeLimits) -> Result<(), ItemError> {
    if !(1..=2_097_151).contains(&limits.max_packet_bytes)
        || limits.max_components > 256
        || !(1..=65_536).contains(&limits.max_component_bytes)
        || limits.max_text_bytes > 4096
    {
        return Err(ItemError::Invalid("decoder limits"));
    }
    if data.len() > limits.max_packet_bytes {
        return Err(ItemError::Limit("packet bytes"));
    }
    Ok(())
}
fn finished(data: &[u8]) -> Result<(), ItemError> {
    if data.is_empty() {
        Ok(())
    } else {
        Err(ItemError::Invalid("trailing packet or component bytes"))
    }
}
fn array<const N: usize>(data: &mut &[u8]) -> Result<[u8; N], ItemError> {
    let mut value = [0; N];
    value.copy_from_slice(take(data, N)?);
    Ok(value)
}
fn unsigned(data: &mut &[u8]) -> Result<u32, ItemError> {
    u32::try_from(get_varint(data)?).map_err(|_| ItemError::Invalid("negative VarInt"))
}
fn length(data: &mut &[u8], maximum: usize, name: &'static str) -> Result<usize, ItemError> {
    let length = unsigned(data)? as usize;
    if length > maximum {
        Err(ItemError::Limit(name))
    } else {
        Ok(length)
    }
}
fn boolean(data: &mut &[u8]) -> Result<bool, ItemError> {
    match take(data, 1)?[0] {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(ItemError::Invalid("noncanonical boolean")),
    }
}

fn read_stack(
    data: &mut &[u8],
    catalog: &ItemCatalog,
    limits: DecodeLimits,
    delimited: bool,
) -> Result<Slot, ItemError> {
    let count = unsigned(data)?;
    if count == 0 {
        return Ok(None);
    }
    if count > 99 {
        return Err(ItemError::Invalid("stack count must be 1..99"));
    }
    let item = ItemId(unsigned(data)?);
    catalog.item(item)?;
    let max_components = catalog.component_count().min(limits.max_components);
    let additions = length(data, max_components, "component count")?;
    let removals = length(
        data,
        max_components.saturating_sub(additions),
        "component count",
    )?;
    let mut added = BTreeMap::new();
    let mut removed = BTreeSet::new();
    for _ in 0..additions {
        let id = ComponentId(unsigned(data)?);
        let definition = catalog.component(id)?;
        if added.contains_key(&id) {
            return Err(ItemError::Invalid("duplicate added component"));
        }
        if !crate::catalog::supported_component(&definition.key) {
            return Err(ItemError::UnsupportedComponent(definition.key.clone()));
        }
        let value = if delimited {
            let size = length(data, limits.max_component_bytes, "component bytes")?;
            let mut payload = take(data, size)?;
            let value = read_component(&definition.key, &mut payload, limits)?;
            finished(payload)?;
            value
        } else {
            let length = data.len().min(limits.max_component_bytes);
            let limited = length < data.len();
            let mut payload = &data[..length];
            let value = read_component(&definition.key, &mut payload, limits).map_err(|error| {
                if limited
                    && matches!(
                        &error,
                        ItemError::Wire(zerqon_protocol::codec::Error::Invalid("truncated packet"))
                    )
                {
                    ItemError::Limit("component bytes")
                } else {
                    error
                }
            })?;
            *data = &data[length - payload.len()..];
            value
        };
        added.insert(id, value);
    }
    for _ in 0..removals {
        let id = ComponentId(unsigned(data)?);
        catalog.component(id)?;
        if added.contains_key(&id) || !removed.insert(id) {
            return Err(ItemError::Invalid(
                "duplicate or conflicting removed component",
            ));
        }
    }
    let patch = ValidatedPatch::new(added, removed, catalog)?;
    let stack = ItemStack::new(item, count as u8, patch)?;
    catalog.validate_stack(&stack)?;
    Ok(Some(stack))
}

fn read_component(
    key: &str,
    data: &mut &[u8],
    limits: DecodeLimits,
) -> Result<ComponentValue, ItemError> {
    Ok(match key {
        "minecraft:max_stack_size" => ComponentValue::MaxStackSize(
            u8::try_from(unsigned(data)?).map_err(|_| ItemError::Invalid("max stack size"))?,
        ),
        "minecraft:max_damage" => ComponentValue::MaxDamage(unsigned(data)?),
        "minecraft:damage" => ComponentValue::Damage(unsigned(data)?),
        "minecraft:repair_cost" => ComponentValue::RepairCost(unsigned(data)?),
        "minecraft:unbreakable" => ComponentValue::Unbreakable,
        "minecraft:custom_name" => {
            ComponentValue::CustomName(read_plain_text(data, limits.max_text_bytes)?)
        }
        "minecraft:item_name" => {
            ComponentValue::ItemName(read_plain_text(data, limits.max_text_bytes)?)
        }
        "minecraft:item_model" => {
            let value = get_string(data, 256)?;
            let value = if value.contains(':') {
                value.to_owned()
            } else {
                format!("minecraft:{value}")
            };
            ComponentValue::ItemModel(value)
        }
        "minecraft:rarity" => ComponentValue::Rarity(
            u8::try_from(unsigned(data)?).map_err(|_| ItemError::Invalid("rarity"))?,
        ),
        "minecraft:enchantment_glint_override" => ComponentValue::EnchantmentGlint(boolean(data)?),
        "minecraft:block_state" => {
            let count = length(data, 32, "block properties")?;
            let mut properties = BTreeMap::new();
            for _ in 0..count {
                let name = get_string(data, 64)?.to_owned();
                let value = get_string(data, 64)?.to_owned();
                if properties.insert(name, value).is_some() {
                    return Err(ItemError::Invalid("duplicate block property"));
                }
            }
            ComponentValue::BlockState(properties)
        }
        _ => return Err(ItemError::UnsupportedComponent(key.into())),
    })
}

fn read_hashed(
    data: &mut &[u8],
    catalog: &ItemCatalog,
    limits: DecodeLimits,
) -> Result<HashedStack, ItemError> {
    if !boolean(data)? {
        return Ok(HashedStack::Empty);
    }
    let item = ItemId(unsigned(data)?);
    if catalog.item(item)?.key == "minecraft:air" {
        return Err(ItemError::Invalid("nonempty predicted air"));
    }
    let count = unsigned(data)?;
    if !(1..=99).contains(&count) {
        return Err(ItemError::Invalid("predicted stack count"));
    }
    let maximum = catalog.component_count().min(limits.max_components);
    let count_added = length(data, maximum, "hashed components")?;
    let mut added = BTreeMap::new();
    for _ in 0..count_added {
        let id = ComponentId(unsigned(data)?);
        catalog.component(id)?;
        let hash = i32::from_be_bytes(array(data)?);
        if added.insert(id, hash).is_some() {
            return Err(ItemError::Invalid("duplicate component hash"));
        }
    }
    let count_removed = length(
        data,
        maximum.saturating_sub(count_added),
        "hashed components",
    )?;
    let mut removed = BTreeSet::new();
    for _ in 0..count_removed {
        let id = ComponentId(unsigned(data)?);
        catalog.component(id)?;
        if added.contains_key(&id) || !removed.insert(id) {
            return Err(ItemError::Invalid(
                "duplicate or conflicting component hash",
            ));
        }
    }
    Ok(HashedStack::Item {
        item,
        count: count as u8,
        added,
        removed,
    })
}

fn write_stack(
    stack: &Slot,
    catalog: &ItemCatalog,
    output: &mut Vec<u8>,
    delimited: bool,
) -> Result<(), ItemError> {
    let Some(stack) = stack else {
        output.push(0);
        return Ok(());
    };
    catalog.validate_stack(stack)?;
    let mut out = Vec::new();
    put_varint(i32::from(stack.count()), &mut out);
    put_varint(stack.item().0 as i32, &mut out);
    put_varint(stack.patch().added.len() as i32, &mut out);
    put_varint(stack.patch().removed.len() as i32, &mut out);
    for (id, component) in &stack.patch().added {
        put_varint(id.0 as i32, &mut out);
        let mut payload = Vec::new();
        write_component(component, &mut payload)?;
        if delimited {
            put_varint(payload.len() as i32, &mut out);
        }
        out.extend_from_slice(&payload);
    }
    for id in &stack.patch().removed {
        put_varint(id.0 as i32, &mut out);
    }
    output.extend_from_slice(&out);
    Ok(())
}

fn write_component(value: &ComponentValue, out: &mut Vec<u8>) -> Result<(), ItemError> {
    match value {
        ComponentValue::MaxStackSize(value) | ComponentValue::Rarity(value) => {
            put_varint(i32::from(*value), out)
        }
        ComponentValue::MaxDamage(value)
        | ComponentValue::Damage(value)
        | ComponentValue::RepairCost(value) => put_varint(*value as i32, out),
        ComponentValue::Unbreakable => {}
        ComponentValue::EnchantmentGlint(value) => out.push(u8::from(*value)),
        ComponentValue::CustomName(text) | ComponentValue::ItemName(text) => {
            write_plain_text(text, out)?
        }
        ComponentValue::ItemModel(value) => put_string(value, out),
        ComponentValue::BlockState(values) => {
            put_varint(values.len() as i32, out);
            for (name, value) in values {
                put_string(name, out);
                put_string(value, out);
            }
        }
    }
    Ok(())
}

fn read_plain_text(data: &mut &[u8], limit: usize) -> Result<String, ItemError> {
    if take(data, 1)?[0] != 8 {
        return Err(ItemError::UnsupportedComponent(
            "rich text component; only plain NBT strings are implemented".into(),
        ));
    }
    let size = usize::from(u16::from_be_bytes(array(data)?));
    if size > limit.saturating_mul(2) {
        return Err(ItemError::Limit("text component bytes"));
    }
    let mut bytes = take(data, size)?;
    let mut units = Vec::with_capacity(size);
    while !bytes.is_empty() {
        let first = take(&mut bytes, 1)?[0];
        let value = match first {
            1..=0x7f => u16::from(first),
            0xc0..=0xdf => {
                let second = take(&mut bytes, 1)?[0];
                if second & 0xc0 != 0x80 {
                    return Err(ItemError::Invalid("modified UTF-8 continuation"));
                }
                let value = (u16::from(first & 0x1f) << 6) | u16::from(second & 0x3f);
                if value != 0 && value < 0x80 {
                    return Err(ItemError::Invalid("overlong modified UTF-8"));
                }
                value
            }
            0xe0..=0xef => {
                let tail = take(&mut bytes, 2)?;
                if tail[0] & 0xc0 != 0x80 || tail[1] & 0xc0 != 0x80 {
                    return Err(ItemError::Invalid("modified UTF-8 continuation"));
                }
                let value = (u16::from(first & 15) << 12)
                    | (u16::from(tail[0] & 63) << 6)
                    | u16::from(tail[1] & 63);
                if value < 0x800 {
                    return Err(ItemError::Invalid("overlong modified UTF-8"));
                }
                value
            }
            _ => return Err(ItemError::Invalid("invalid modified UTF-8")),
        };
        units.push(value);
    }
    let text =
        String::from_utf16(&units).map_err(|_| ItemError::Invalid("unpaired text surrogate"))?;
    if text.len() > limit {
        return Err(ItemError::Limit("text component bytes"));
    }
    Ok(text)
}

fn write_plain_text(text: &str, out: &mut Vec<u8>) -> Result<(), ItemError> {
    let mut encoded = Vec::new();
    for value in text.encode_utf16() {
        if (1..=0x7f).contains(&value) {
            encoded.push(value as u8);
        } else if value <= 0x7ff {
            encoded.extend_from_slice(&[(0xc0 | (value >> 6)) as u8, (0x80 | (value & 63)) as u8]);
        } else {
            encoded.extend_from_slice(&[
                (0xe0 | (value >> 12)) as u8,
                (0x80 | ((value >> 6) & 63)) as u8,
                (0x80 | (value & 63)) as u8,
            ]);
        }
    }
    let length =
        u16::try_from(encoded.len()).map_err(|_| ItemError::Limit("text component bytes"))?;
    out.push(8);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&encoded);
    Ok(())
}
