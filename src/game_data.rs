use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::Path,
};

use serde::Deserialize;
use zerqon_protocol::codec::{put_string, put_varint};

use zerqon_world::FlatPalette;

#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("invalid game data: {0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub struct BlockResolver {
    blocks: BTreeMap<String, Block>,
}
impl BlockResolver {
    pub fn load(path: &Path) -> Result<Self, DataError> {
        let identity: Identity = read_json(&path.join("prepared.json"))?;
        identity.validate()?;
        let blocks: BTreeMap<String, Block> =
            read_json(&path.join("generated/reports/blocks.json"))?;
        let mut ids = HashSet::new();
        for (name, block) in &blocks {
            identifier(name)?;
            if block.states.is_empty() {
                return Err(invalid("block has no states"));
            }
            if block.states.iter().filter(|state| state.default).count() != 1 {
                return Err(invalid("block requires exactly one default state"));
            }
            let mut properties = HashSet::new();
            for state in &block.states {
                if state.id >= 1_048_576
                    || !ids.insert(state.id)
                    || !properties.insert(&state.properties)
                {
                    return Err(invalid("duplicate or invalid block state"));
                }
            }
        }
        if blocks.is_empty() {
            return Err(invalid("empty block registry"));
        }
        Ok(Self { blocks })
    }
    pub fn resolve(&self, name: &str, properties: &BTreeMap<String, String>) -> Option<u32> {
        let block = self.blocks.get(name)?;
        let default = block.states.iter().find(|state| state.default)?;
        if properties.is_empty() {
            return Some(default.id);
        }
        if properties
            .keys()
            .any(|name| !default.properties.contains_key(name))
        {
            return None;
        }
        block
            .states
            .iter()
            .find(|state| {
                default.properties.iter().all(|(name, value)| {
                    state.properties.get(name) == Some(properties.get(name).unwrap_or(value))
                })
            })
            .map(|state| state.id)
    }
}

pub struct GameData {
    pub flat: FlatPalette,
    pub plains_biome: i32,
    pub overworld_dimension: i32,
    pub block_bits: u8,
    pub max_block_state: u32,
    configuration: Vec<Vec<u8>>,
    block_flags: Vec<u8>,
    entity_types: BTreeMap<String, i32>,
}

impl GameData {
    pub fn entity_type(&self, name: &str) -> Option<i32> {
        self.entity_types.get(name).copied()
    }

    pub fn load(path: &Path) -> Result<Self, DataError> {
        let prepared: Identity = read_json(&path.join("prepared.json"))?;
        prepared.validate()?;
        let registry_data: RegistryFile = read_json(&path.join("registries-sync.json"))?;
        registry_data.identity.validate()?;
        let tags: TagsFile = read_json(&path.join("tags-sync.json"))?;
        let blocks: BTreeMap<String, Block> =
            read_json(&path.join("generated/reports/blocks.json"))?;
        let builtins: BTreeMap<String, BuiltinRegistry> =
            read_json(&path.join("generated/reports/registries.json"))?;
        let entity_registry = builtins
            .get("minecraft:entity_type")
            .ok_or_else(|| invalid("missing entity type registry"))?;
        let mut entity_types = BTreeMap::new();
        let mut entity_ids = HashSet::new();
        for (name, entry) in &entity_registry.entries {
            identifier(name)?;
            if !(0..1_048_576).contains(&entry.protocol_id) || !entity_ids.insert(entry.protocol_id)
            {
                return Err(invalid("invalid or duplicate entity type ID"));
            }
            entity_types.insert(name.clone(), entry.protocol_id);
        }
        if !entity_types.contains_key("minecraft:player") {
            return Err(invalid("missing player entity type"));
        }
        let default = |name: &str| -> Result<u32, DataError> {
            let states = &blocks
                .get(name)
                .ok_or_else(|| invalid(format!("missing {name}")))?
                .states;
            let mut defaults = states.iter().filter(|state| state.default);
            let value = defaults
                .next()
                .ok_or_else(|| invalid(format!("missing default for {name}")))?;
            if defaults.next().is_some() {
                return Err(invalid(format!("duplicate default for {name}")));
            }
            Ok(value.id)
        };
        let flat = FlatPalette {
            air: default("minecraft:air")?,
            bedrock: default("minecraft:bedrock")?,
            stone: default("minecraft:stone")?,
            dirt: default("minecraft:dirt")?,
            grass: default("minecraft:grass_block")?,
        };
        let max_block_state = blocks
            .values()
            .flat_map(|block| &block.states)
            .map(|state| state.id)
            .max()
            .ok_or_else(|| invalid("empty block registry"))?;
        if max_block_state >= 1_048_576 {
            return Err(invalid("block state ID exceeds supported bound"));
        }
        let mut block_flags = vec![0; max_block_state as usize + 1];
        for (name, block) in &blocks {
            let air = matches!(
                name.as_str(),
                "minecraft:air" | "minecraft:cave_air" | "minecraft:void_air"
            );
            let fluid = matches!(
                name.as_str(),
                "minecraft:water"
                    | "minecraft:lava"
                    | "minecraft:bubble_column"
                    | "minecraft:kelp"
                    | "minecraft:kelp_plant"
                    | "minecraft:seagrass"
                    | "minecraft:tall_seagrass"
            );
            for state in &block.states {
                let flags = &mut block_flags[state.id as usize];
                if *flags != 0 {
                    return Err(invalid(format!("duplicate block state ID {}", state.id)));
                }
                *flags = 1
                    | (u8::from(air) << 1)
                    | (u8::from(
                        fluid
                            || state
                                .properties
                                .get("waterlogged")
                                .is_some_and(|value| value == "true"),
                    ) << 2);
            }
        }
        let plains_biome = registry_id(
            &registry_data.registries,
            "minecraft:worldgen/biome",
            "minecraft:plains",
        )?;
        let overworld_dimension = registry_id(
            &registry_data.registries,
            "minecraft:dimension_type",
            "minecraft:overworld",
        )?;
        let configuration = configuration_packets(&registry_data.registries, &tags.registries)?;
        Ok(Self {
            flat,
            plains_biome,
            overworld_dimension,
            block_bits: (u32::BITS - max_block_state.leading_zeros()).max(1) as u8,
            max_block_state,
            configuration,
            block_flags,
            entity_types,
        })
    }

    pub fn configuration_packets(&self) -> &[Vec<u8>] {
        &self.configuration
    }

    pub fn is_valid_state(&self, state: u32) -> bool {
        state <= self.max_block_state
            && self
                .block_flags
                .get(state as usize)
                .is_some_and(|flags| flags & 1 != 0)
    }

    pub fn is_air(&self, state: u32) -> bool {
        self.block_flags
            .get(state as usize)
            .is_some_and(|flags| flags & 2 != 0)
    }

    pub fn has_fluid(&self, state: u32) -> bool {
        self.block_flags
            .get(state as usize)
            .is_some_and(|flags| flags & 4 != 0)
    }
}

#[derive(Deserialize)]
struct Identity {
    version: String,
    protocol: i32,
}

#[derive(Deserialize)]
struct BuiltinRegistry {
    entries: BTreeMap<String, BuiltinEntry>,
}
#[derive(Deserialize)]
struct BuiltinEntry {
    protocol_id: i32,
}

impl Identity {
    fn validate(&self) -> Result<(), DataError> {
        if self.version != "26.3" || self.protocol != 777 {
            return Err(invalid(
                "expected Minecraft 26.3 protocol 777; rerun scripts/prepare-data.py",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct RegistryFile {
    #[serde(flatten)]
    identity: Identity,
    registries: Vec<Registry>,
}
#[derive(Deserialize)]
struct Registry {
    name: String,
    entries: Vec<Entry>,
}
#[derive(Deserialize)]
struct Entry {
    id: String,
}
#[derive(Deserialize)]
struct TagsFile {
    registries: Vec<RegistryTags>,
}
#[derive(Deserialize)]
struct RegistryTags {
    name: String,
    tags: Vec<Tag>,
}
#[derive(Deserialize)]
struct Tag {
    name: String,
    ids: Vec<i32>,
}
#[derive(Deserialize)]
struct Block {
    states: Vec<State>,
}
#[derive(Deserialize)]
struct State {
    id: u32,
    #[serde(default)]
    default: bool,
    #[serde(default)]
    properties: BTreeMap<String, String>,
}

fn invalid(message: impl Into<String>) -> DataError {
    DataError::Invalid(message.into())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, DataError> {
    if fs::metadata(path)?.len() > 32 * 1024 * 1024 {
        return Err(invalid(format!("{} exceeds 32 MiB", path.display())));
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn identifier(value: &str) -> Result<(), DataError> {
    let Some((namespace, path)) = value.split_once(':') else {
        return Err(invalid("identifier requires a namespace"));
    };
    if value.len() > 256
        || namespace.is_empty()
        || path.is_empty()
        || !namespace.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-.".contains(&byte)
        })
        || !path.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-./".contains(&byte)
        })
    {
        return Err(invalid(format!("invalid registry identifier {value}")));
    }
    Ok(())
}

fn registry_id(registries: &[Registry], registry: &str, name: &str) -> Result<i32, DataError> {
    let value = registries
        .iter()
        .find(|value| value.name == registry)
        .ok_or_else(|| invalid(format!("missing registry {registry}")))?;
    value
        .entries
        .iter()
        .position(|entry| entry.id == name)
        .map(|index| index as i32)
        .ok_or_else(|| invalid(format!("missing {name} in {registry}")))
}

fn configuration_packets(
    registries: &[Registry],
    tag_registries: &[RegistryTags],
) -> Result<Vec<Vec<u8>>, DataError> {
    if registries.len() > 1024 || tag_registries.len() > 1024 {
        return Err(invalid("too many registries"));
    }
    let mut features = vec![13, 1];
    put_string("minecraft:vanilla", &mut features);
    let mut packets = vec![features];
    let mut names = HashSet::new();
    for registry in registries {
        identifier(&registry.name)?;
        if !names.insert(&registry.name) || registry.entries.len() > 65536 {
            return Err(invalid("duplicate or oversized registry"));
        }
        let mut packet = vec![7];
        put_string(&registry.name, &mut packet);
        put_varint(registry.entries.len() as i32, &mut packet);
        let mut entries = HashSet::new();
        for entry in &registry.entries {
            identifier(&entry.id)?;
            if !entries.insert(&entry.id) {
                return Err(invalid("duplicate registry entry"));
            }
            put_string(&entry.id, &mut packet);
            packet.push(0);
        }
        if packet.len() > 2_097_151 {
            return Err(invalid("registry packet exceeds frame limit"));
        }
        packets.push(packet);
    }
    let mut tags_packet = vec![14];
    put_varint(tag_registries.len() as i32, &mut tags_packet);
    let mut names = HashSet::new();
    for registry in tag_registries {
        identifier(&registry.name)?;
        if !names.insert(&registry.name) || registry.tags.len() > 65536 {
            return Err(invalid("duplicate or oversized tag registry"));
        }
        put_string(&registry.name, &mut tags_packet);
        put_varint(registry.tags.len() as i32, &mut tags_packet);
        let mut tags = HashSet::new();
        for tag in &registry.tags {
            identifier(&tag.name)?;
            if !tags.insert(&tag.name)
                || tag.ids.len() > 1_048_576
                || tag.ids.iter().any(|id| *id < 0)
            {
                return Err(invalid("invalid tag members"));
            }
            put_string(&tag.name, &mut tags_packet);
            put_varint(tag.ids.len() as i32, &mut tags_packet);
            for &id in &tag.ids {
                put_varint(id, &mut tags_packet);
            }
        }
    }
    if tags_packet.len() > 2_097_151 {
        return Err(invalid("tag packet exceeds frame limit"));
    }
    packets.push(tags_packet);
    packets.push(vec![3]);
    Ok(packets)
}
