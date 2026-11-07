use crate::{Aabb, Error};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::Read,
};

const ASSET: &[u8] = include_bytes!("../assets/26.3.json.zlib");
const CHECKSUM: &str = include_str!("../assets/26.3.json.sha256");
const SERVER_SHA256: &str = "a362163eec5d1612d520772bc16e5b39c09e3b234fdc045f56bf544284ee8ae6";
const MAX_COMPRESSED: usize = 512 * 1024;
const MAX_JSON: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StateId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Unsupported,
    Simple,
    Pillar,
    Slab,
    Stairs,
    SnowyGround,
}

#[derive(Debug)]
struct Property {
    name: String,
    values: Vec<String>,
    radix: u32,
}

#[derive(Debug)]
struct Block {
    key: String,
    default: StateId,
    kind: BlockKind,
    flags: u8,
    item: u32,
    properties: Vec<Property>,
    states: Vec<StateId>,
}

#[derive(Debug, Clone, Copy)]
struct StateData {
    block: u16,
    shape: u16,
    properties: u32,
    flags: u8,
}

#[derive(Debug)]
pub struct Item {
    pub key: String,
    block: Option<u16>,
    pub ordinary_block_item: bool,
    pub can_destroy_in_creative: Option<bool>,
}

#[derive(Debug)]
pub struct Catalog {
    blocks: Vec<Block>,
    block_keys: HashMap<String, u16>,
    states: Vec<StateData>,
    items: Vec<Item>,
    item_keys: HashMap<String, u32>,
    shapes: Vec<Box<[Aabb]>>,
    air: StateId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCatalog {
    schema: u32,
    minecraft: String,
    protocol: u32,
    server_sha256: String,
    blocks: Vec<RawBlock>,
    states: Vec<(u16, u32, u16, u8)>,
    items: Vec<(String, i32, bool, Option<bool>)>,
    shapes: Vec<Vec<[f64; 6]>>,
}

type RawBlock = (String, u32, u8, u8, u32, Vec<(String, Vec<String>)>);

impl Catalog {
    pub fn embedded() -> Result<Self, Error> {
        Self::from_compressed(ASSET)
    }

    pub fn from_compressed(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_COMPRESSED {
            return Err(Error::Data("block asset size"));
        }
        let digest: String = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if digest != CHECKSUM.trim() {
            return Err(Error::Data("block asset checksum or size"));
        }
        let mut decoder = flate2::read::ZlibDecoder::new(bytes);
        let mut json = Vec::new();
        decoder
            .by_ref()
            .take(MAX_JSON as u64 + 1)
            .read_to_end(&mut json)?;
        if json.len() > MAX_JSON || decoder.total_in() != bytes.len() as u64 {
            return Err(Error::Data("block asset expansion or trailing bytes"));
        }
        Self::validate(serde_json::from_slice(&json)?)
    }

    fn validate(raw: RawCatalog) -> Result<Self, Error> {
        if raw.schema != 1
            || raw.minecraft != "26.3"
            || raw.protocol != 777
            || raw.server_sha256 != SERVER_SHA256
        {
            return Err(Error::Data("block asset version"));
        }
        if raw.blocks.len() != 1286
            || raw.states.len() != 35723
            || raw.items.len() != 1658
            || raw.shapes.is_empty()
            || raw.shapes.len() > 4096
        {
            return Err(Error::Data("block asset registry counts"));
        }
        let mut shapes = Vec::with_capacity(raw.shapes.len());
        for boxes in raw.shapes {
            if boxes.len() > 64 {
                return Err(Error::Data("collision box count"));
            }
            let mut shape = Vec::with_capacity(boxes.len());
            for coordinates in boxes {
                let bounds = Aabb {
                    min: [coordinates[0], coordinates[1], coordinates[2]],
                    max: [coordinates[3], coordinates[4], coordinates[5]],
                };
                if !bounds.valid() || coordinates.iter().any(|value| value.abs() > 4.0) {
                    return Err(Error::Data("collision coordinates"));
                }
                shape.push(bounds);
            }
            shapes.push(shape.into_boxed_slice());
        }
        let mut blocks = Vec::with_capacity(raw.blocks.len());
        let mut block_keys = HashMap::new();
        let mut total_states = 0usize;
        for (key, default, family, flags, item, definitions) in raw.blocks {
            if !identifier(&key) || flags > 63 || item >= 1658 || definitions.len() > 16 {
                return Err(Error::Data("block definition"));
            }
            if block_keys
                .insert(key.clone(), blocks.len() as u16)
                .is_some()
            {
                return Err(Error::Data("duplicate block"));
            }
            let kind = match family {
                0 => BlockKind::Unsupported,
                1 => BlockKind::Simple,
                2 => BlockKind::Pillar,
                3 => BlockKind::Slab,
                4 => BlockKind::Stairs,
                5 => BlockKind::SnowyGround,
                _ => return Err(Error::Data("block family")),
            };
            let mut radix = 1u32;
            let mut properties = Vec::new();
            let mut names = HashSet::new();
            for (name, values) in definitions {
                if !token(&name)
                    || !names.insert(name.clone())
                    || values.is_empty()
                    || values.len() > 64
                    || values.iter().any(|value| !token(value))
                    || values.iter().collect::<HashSet<_>>().len() != values.len()
                {
                    return Err(Error::Data("block property"));
                }
                let next = radix
                    .checked_mul(values.len() as u32)
                    .filter(|count| *count <= 35_723)
                    .ok_or(Error::Data("block state product"))?;
                properties.push(Property {
                    name,
                    values,
                    radix,
                });
                radix = next;
            }
            total_states += radix as usize;
            if total_states > 35_723 {
                return Err(Error::Data("block state allocation"));
            }
            blocks.push(Block {
                key,
                default: StateId(default),
                kind,
                flags,
                item,
                properties,
                states: vec![StateId(u32::MAX); radix as usize],
            });
        }
        let mut states = Vec::with_capacity(raw.states.len());
        for (block, properties, shape, flags) in raw.states {
            let definition = blocks
                .get_mut(usize::from(block))
                .ok_or(Error::Data("state block index"))?;
            let target = definition
                .states
                .get_mut(properties as usize)
                .ok_or(Error::Data("state property code"))?;
            if *target != StateId(u32::MAX) || usize::from(shape) >= shapes.len() || flags > 31 {
                return Err(Error::Data("duplicate state, shape or flags"));
            }
            *target = StateId(states.len() as u32);
            states.push(StateData {
                block,
                shape,
                properties,
                flags,
            });
        }
        for (index, block) in blocks.iter().enumerate() {
            if block.states.contains(&StateId(u32::MAX))
                || states
                    .get(block.default.0 as usize)
                    .is_none_or(|state| usize::from(state.block) != index)
            {
                return Err(Error::Data("incomplete states or invalid default"));
            }
        }
        let mut items = Vec::with_capacity(raw.items.len());
        let mut item_keys = HashMap::new();
        for (key, block, ordinary_block_item, can_destroy_in_creative) in raw.items {
            if !identifier(&key)
                || block < -1
                || block >= blocks.len() as i32
                || (ordinary_block_item && block < 0)
                || item_keys.insert(key.clone(), items.len() as u32).is_some()
            {
                return Err(Error::Data("item mapping"));
            }
            items.push(Item {
                key,
                block: (block >= 0).then_some(block as u16),
                ordinary_block_item,
                can_destroy_in_creative,
            });
        }
        let air = blocks
            .get(usize::from(
                *block_keys
                    .get("minecraft:air")
                    .ok_or(Error::Data("missing air"))?,
            ))
            .ok_or(Error::Data("air block"))?
            .default;
        let catalog = Self {
            blocks,
            block_keys,
            states,
            items,
            item_keys,
            shapes,
            air,
        };
        if !catalog.state(air)?.is_air() {
            return Err(Error::Data("air state"));
        }
        Ok(catalog)
    }

    pub fn air(&self) -> StateId {
        self.air
    }
    pub fn state_count(&self) -> usize {
        self.states.len()
    }
    pub fn item_count(&self) -> usize {
        self.items.len()
    }
    pub fn state(&self, id: StateId) -> Result<State<'_>, Error> {
        let data = self
            .states
            .get(id.0 as usize)
            .ok_or(Error::UnknownState(id.0))?;
        Ok(State {
            catalog: self,
            id,
            data,
        })
    }
    pub fn item(&self, id: u32) -> Result<&Item, Error> {
        self.items.get(id as usize).ok_or(Error::UnknownItem(id))
    }
    pub fn item_id(&self, key: &str) -> Option<u32> {
        self.item_keys.get(key).copied()
    }
    pub fn item_block(&self, item: u32) -> Result<Option<StateId>, Error> {
        Ok(self
            .item(item)?
            .block
            .map(|block| self.blocks[usize::from(block)].default))
    }
    pub fn resolve(
        &self,
        key: &str,
        properties: &BTreeMap<String, String>,
    ) -> Result<StateId, Error> {
        let index = *self.block_keys.get(key).ok_or(Error::Properties)?;
        self.with_properties(
            self.blocks[usize::from(index)].default,
            properties
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
    }
    pub fn with_properties<'a>(
        &self,
        state: StateId,
        properties: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<StateId, Error> {
        let current = self.state(state)?;
        let block = current.block();
        let mut code = current.data.properties;
        let mut names = HashSet::new();
        for (key, value) in properties {
            if !names.insert(key) {
                return Err(Error::Properties);
            }
            let property = block
                .properties
                .iter()
                .find(|property| property.name == key)
                .ok_or(Error::Properties)?;
            let index = property
                .values
                .iter()
                .position(|candidate| candidate == value)
                .ok_or(Error::Properties)? as u32;
            let old = code / property.radix % property.values.len() as u32;
            code = code - old * property.radix + index * property.radix;
        }
        block
            .states
            .get(code as usize)
            .copied()
            .ok_or(Error::Properties)
    }
}

#[derive(Clone, Copy)]
pub struct State<'a> {
    catalog: &'a Catalog,
    id: StateId,
    data: &'a StateData,
}

impl<'a> State<'a> {
    fn block(self) -> &'a Block {
        &self.catalog.blocks[usize::from(self.data.block)]
    }
    pub fn id(self) -> StateId {
        self.id
    }
    pub fn key(self) -> &'a str {
        &self.block().key
    }
    pub fn kind(self) -> BlockKind {
        self.block().kind
    }
    pub fn item(self) -> u32 {
        self.block().item
    }
    pub fn is_air(self) -> bool {
        self.data.flags & 1 != 0
    }
    pub fn replaceable(self) -> bool {
        self.data.flags & 2 != 0
    }
    pub fn has_fluid(self) -> bool {
        self.data.flags & 4 != 0
    }
    pub fn has_block_entity(self) -> bool {
        self.data.flags & 8 != 0
    }
    pub fn dynamic_collision(self) -> bool {
        self.data.flags & 16 != 0
    }
    pub fn stair_like(self) -> bool {
        self.block().flags & 1 != 0
    }
    pub fn interactive(self) -> bool {
        self.block().flags & 2 != 0
    }
    pub fn specialized_replacement(self) -> bool {
        self.block().flags & 4 != 0
    }
    pub fn neighbor_effects(self) -> bool {
        self.block().flags & 8 != 0
    }
    pub fn game_master(self) -> bool {
        self.block().flags & 16 != 0
    }
    pub fn snow_cover(self) -> bool {
        self.block().flags & 32 != 0
    }
    pub fn properties(self) -> impl Iterator<Item = (&'a str, &'a str)> {
        self.block().properties.iter().map(move |property| {
            let index = self.data.properties / property.radix % property.values.len() as u32;
            (
                property.name.as_str(),
                property.values[index as usize].as_str(),
            )
        })
    }
    pub fn property(self, name: &str) -> Option<&'a str> {
        self.properties()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
    }
    pub fn collision(self) -> Result<&'a [Aabb], Error> {
        if self.dynamic_collision() {
            return Err(Error::DynamicCollision);
        }
        Ok(&self.catalog.shapes[usize::from(self.data.shape)])
    }
}

fn token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn identifier(value: &str) -> bool {
    value.strip_prefix("minecraft:").is_some_and(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw() -> RawCatalog {
        let mut bytes = Vec::new();
        flate2::read::ZlibDecoder::new(ASSET)
            .read_to_end(&mut bytes)
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn metadata_validation_rejects_wrong_version_and_incomplete_state_tables() {
        let mut data = raw();
        data.protocol = 776;
        assert!(Catalog::validate(data).is_err());
        let mut data = raw();
        data.states.pop();
        assert!(Catalog::validate(data).is_err());
        let mut data = raw();
        data.states[1] = data.states[0];
        assert!(Catalog::validate(data).is_err());
        let mut data = raw();
        data.blocks[0].1 = u32::MAX;
        assert!(Catalog::validate(data).is_err());
    }

    #[test]
    fn metadata_validation_rejects_invalid_geometry_properties_and_item_references() {
        let mut data = raw();
        data.shapes[1][0][0] = f64::NAN;
        assert!(Catalog::validate(data).is_err());
        let mut data = raw();
        data.states[0].2 = u16::MAX;
        assert!(Catalog::validate(data).is_err());
        let mut data = raw();
        data.items[0].1 = i32::MAX;
        assert!(Catalog::validate(data).is_err());
        let mut data = raw();
        data.blocks[0].5 = vec![("axis".into(), vec!["x".into(); 65])];
        assert!(Catalog::validate(data).is_err());
    }
}
