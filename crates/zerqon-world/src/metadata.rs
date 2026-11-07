use crate::WorldError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub const FILE_NAME: &str = "world-metadata.json";
const MAX_JSON: usize = 1024 * 1024;
const MAX_NBT: usize = 8 * 1024 * 1024;
const SAVED_DATA_PATHS: &[&str] = &[
    "data/minecraft/game_rules.dat",
    "data/minecraft/world_clocks.dat",
    "data/minecraft/weather.dat",
    "dimensions/minecraft/overworld/data/minecraft/world_border.dat",
    "dimensions/minecraft/overworld/data/minecraft/chunk_tickets.dat",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationPolicy {
    NativeFlat,
    ExistingOnly,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnPoint {
    pub dimension: String,
    pub position: [i32; 3],
    pub yaw: f32,
    pub pitch: f32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DimensionSource {
    pub dimension_type: Option<String>,
    pub generator_kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratorSource {
    pub seed: i64,
    pub generate_structures: bool,
    pub bonus_chest: bool,
    pub dimensions: BTreeMap<String, DimensionSource>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceIdentity {
    pub minecraft_version: String,
    pub data_version: i32,
    pub level_name: String,
    pub level_dat_sha256: String,
    pub generator_sha256: String,
    pub version_history: Vec<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DifficultySource {
    pub difficulty: String,
    pub hardcore: bool,
    pub locked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedDataSource {
    pub path: String,
    pub data_version: i32,
    pub sha256: String,
    pub keys: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedSource {
    pub manifest: String,
    pub manifest_sha256: String,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldMetadata {
    pub schema: u32,
    pub world_id: Uuid,
    pub source: SourceIdentity,
    pub spawn: SpawnPoint,
    pub generation_policy: GenerationPolicy,
    pub source_game_mode: u8,
    pub source_game_time: i64,
    pub source_difficulty: DifficultySource,
    pub generator: GeneratorSource,
    pub saved_data: Vec<SavedDataSource>,
    pub retained_source: Option<RetainedSource>,
}

impl WorldMetadata {
    pub fn from_vanilla(source: &Path) -> Result<Self, WorldError> {
        let source = source_root(source)?;
        let (level_bytes, level_hash) = read_gzip_nbt(&source.join("level.dat"))?;
        let level: RawLevel = decode(&level_bytes, "level.dat")?;
        if level.data.data_version != 5023
            || level.data.version.id != 5023
            || level.data.version.name != "26.3"
            || level.data.version.series != "main"
            || level.data.version.snapshot != 0
        {
            return Err(invalid("level.dat must be Minecraft 26.3 DataVersion 5023"));
        }
        let (generator_bytes, generator_hash) =
            read_gzip_nbt(&source.join("data/minecraft/world_gen_settings.dat"))?;
        let raw_generator: RawWorldGen = decode(&generator_bytes, "world_gen_settings.dat")?;
        if raw_generator.data_version != 5023 {
            return Err(invalid("world_gen_settings.dat must have DataVersion 5023"));
        }
        let mut dimensions = BTreeMap::new();
        for (name, value) in raw_generator.data.dimensions {
            let dimension_type = match value.dimension_type {
                fastnbt::Value::String(value) => Some(value),
                fastnbt::Value::Compound(_) => None,
                _ => return Err(invalid("dimension type must be an identifier or compound")),
            };
            dimensions.insert(
                name,
                DimensionSource {
                    dimension_type,
                    generator_kind: value.generator.kind,
                },
            );
        }
        let position: [i32; 3] = level
            .data
            .spawn
            .position
            .as_ref()
            .try_into()
            .map_err(|_| invalid("spawn.pos must contain three integers"))?;
        let metadata = Self {
            schema: 1,
            world_id: Uuid::new_v4(),
            source: SourceIdentity {
                minecraft_version: "26.3".into(),
                data_version: 5023,
                level_name: level.data.name,
                level_dat_sha256: level_hash,
                generator_sha256: generator_hash,
                version_history: level.data.version_history,
            },
            spawn: SpawnPoint {
                dimension: level.data.spawn.dimension,
                position,
                yaw: level.data.spawn.yaw,
                pitch: level.data.spawn.pitch,
            },
            generation_policy: GenerationPolicy::ExistingOnly,
            source_game_mode: u8::try_from(level.data.game_mode)
                .map_err(|_| invalid("invalid source game mode"))?,
            source_game_time: level.data.game_time,
            source_difficulty: DifficultySource {
                difficulty: level.data.difficulty.difficulty,
                hardcore: boolean(level.data.difficulty.hardcore)?,
                locked: boolean(level.data.difficulty.locked)?,
            },
            generator: GeneratorSource {
                seed: raw_generator.data.seed,
                generate_structures: boolean(raw_generator.data.generate_structures)?,
                bonus_chest: boolean(raw_generator.data.bonus_chest)?,
                dimensions,
            },
            saved_data: read_saved_data(&source)?,
            retained_source: None,
        };
        metadata.validate()?;
        Ok(metadata)
    }
    pub fn load(root: &Path) -> Result<Option<Self>, WorldError> {
        let path = root.join(FILE_NAME);
        reject_links(&path)?;
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let metadata: Self = serde_json::from_slice(&read_regular(&path, MAX_JSON as u64)?)
            .map_err(|error| invalid(format!("{FILE_NAME}: {error}")))?;
        metadata.validate()?;
        if let Some(retained) = &metadata.retained_source {
            let bytes = read_regular(&root.join(&retained.manifest), 16 * 1024 * 1024)?;
            if checksum(&bytes) != retained.manifest_sha256 {
                return Err(invalid("retained source manifest checksum mismatch"));
            }
        }
        Ok(Some(metadata))
    }
    pub fn write_new(&self, root: &Path) -> Result<(), WorldError> {
        self.validate()?;
        let root = source_root(root)?;
        let target = root.join(FILE_NAME);
        reject_links(&target)?;
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| invalid(error.to_string()))?;
        if bytes.len() > MAX_JSON {
            return Err(invalid("world metadata exceeds 1 MiB"));
        }
        let mut file = tempfile::NamedTempFile::new_in(&root)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(target)
            .map_err(|error| error.error)?;
        #[cfg(unix)]
        File::open(root)?.sync_all()?;
        Ok(())
    }
    fn validate(&self) -> Result<(), WorldError> {
        if self.schema != 1 || self.world_id.is_nil() {
            return Err(invalid("unsupported metadata schema or nil world UUID"));
        }
        if self.source.minecraft_version != "26.3"
            || self.source.data_version != 5023
            || !hash_valid(&self.source.level_dat_sha256)
            || !hash_valid(&self.source.generator_sha256)
        {
            return Err(invalid("invalid source world identity"));
        }
        if self.source.level_name.len() > 1024
            || self.source.version_history.len() > 256
            || self
                .source
                .version_history
                .iter()
                .any(|version| *version <= 0)
        {
            return Err(invalid("source world identity exceeds limits"));
        }
        if self.generation_policy != GenerationPolicy::ExistingOnly {
            return Err(invalid(
                "imported worlds must use existing_only generation policy",
            ));
        }
        if self.spawn.dimension != "minecraft:overworld" {
            return Err(invalid("only an overworld spawn can currently be imported"));
        }
        if self.spawn.position[0].unsigned_abs() > 29_999_984
            || self.spawn.position[2].unsigned_abs() > 29_999_984
            || !(-64..=320).contains(&self.spawn.position[1])
            || !self.spawn.yaw.is_finite()
            || !self.spawn.pitch.is_finite()
        {
            return Err(invalid(
                "source spawn is outside supported bounds or has non-finite rotation",
            ));
        }
        if self.source_game_mode > 3
            || self.source_game_time < 0
            || !matches!(
                self.source_difficulty.difficulty.as_str(),
                "peaceful" | "easy" | "normal" | "hard"
            )
        {
            return Err(invalid("invalid source game mode, time or difficulty"));
        }
        if self.generator.dimensions.is_empty() || self.generator.dimensions.len() > 64 {
            return Err(invalid("source dimension count must be 1..64"));
        }
        for (name, dimension) in &self.generator.dimensions {
            if !resource_id(name)
                || !resource_id(&dimension.generator_kind)
                || dimension
                    .dimension_type
                    .as_ref()
                    .is_some_and(|kind| !resource_id(kind))
            {
                return Err(invalid("invalid source dimension or generator identifier"));
            }
        }
        if self
            .generator
            .dimensions
            .get("minecraft:overworld")
            .and_then(|dimension| dimension.dimension_type.as_deref())
            != Some("minecraft:overworld")
        {
            return Err(invalid(
                "only the standard minecraft:overworld dimension type is currently supported",
            ));
        }
        if self.saved_data.len() > 16
            || self.saved_data.iter().any(|entry| {
                entry.data_version != 5023
                    || !hash_valid(&entry.sha256)
                    || entry.keys.len() > 4096
                    || entry.keys.iter().any(|key| key.len() > 512)
                    || !SAVED_DATA_PATHS.contains(&entry.path.as_str())
            })
            || self
                .saved_data
                .iter()
                .map(|entry| &entry.path)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.saved_data.len()
        {
            return Err(invalid("invalid saved data summary"));
        }
        if let Some(retained) = &self.retained_source
            && (retained.manifest != "source-data/manifest.json"
                || !hash_valid(&retained.manifest_sha256)
                || retained.files > 16_384
                || retained.bytes > 64 * 1024 * 1024 * 1024)
        {
            return Err(invalid("invalid retained source manifest identity"));
        }
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> WorldError {
    WorldError::Metadata(message.into())
}
fn boolean(value: i8) -> Result<bool, WorldError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid("NBT boolean must be zero or one")),
    }
}
fn hash_valid(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
pub(crate) fn checksum(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn resource_id(value: &str) -> bool {
    if value.len() > 255 {
        return false;
    }
    let Some((namespace, path)) = value.split_once(':') else {
        return false;
    };
    !namespace.is_empty()
        && !matches!(namespace, "." | "..")
        && !path.is_empty()
        && namespace.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
        })
        && path.split('/').all(|part| {
            !part.is_empty()
                && !matches!(part, "." | "..")
                && part.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
                })
        })
}

pub(crate) fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}
pub(crate) fn reject_links(path: &Path) -> Result<(), WorldError> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        if !matches!(
            component,
            std::path::Component::Normal(_) | std::path::Component::ParentDir
        ) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if is_link(&metadata) => {
                return Err(invalid(format!(
                    "source/destination path contains a link or reparse point: {}",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
pub(crate) fn source_root(path: &Path) -> Result<PathBuf, WorldError> {
    reject_links(path)?;
    let path = path.canonicalize()?;
    if !path.is_dir() {
        return Err(invalid("world path must be a directory"));
    }
    Ok(path)
}
pub(crate) fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>, WorldError> {
    reject_links(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(invalid(format!(
            "{} must be a bounded regular file",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("file grew beyond its size limit"));
    }
    Ok(bytes)
}
fn read_gzip_nbt(path: &Path) -> Result<(Vec<u8>, String), WorldError> {
    let compressed = read_regular(path, MAX_NBT as u64)?;
    let hash = checksum(&compressed);
    let mut decoder = flate2::bufread::GzDecoder::new(compressed.as_slice());
    let mut bytes = Vec::new();
    decoder
        .by_ref()
        .take(MAX_NBT as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_NBT || !decoder.into_inner().is_empty() {
        return Err(invalid(
            "NBT gzip output exceeds limit or has trailing data",
        ));
    }
    crate::anvil::validate_nbt(&bytes)
        .map_err(|error| invalid(format!("{}: {error}", path.display())))?;
    Ok((bytes, hash))
}
fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8], name: &str) -> Result<T, WorldError> {
    fastnbt::from_bytes_with_opts(bytes, fastnbt::DeOpts::new().max_seq_len(65_536))
        .map_err(|error| invalid(format!("{name}: {error}")))
}

fn read_saved_data(source: &Path) -> Result<Vec<SavedDataSource>, WorldError> {
    let mut result = Vec::new();
    for &path in SAVED_DATA_PATHS {
        match fs::symlink_metadata(source.join(path)) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
        let (bytes, sha256) = read_gzip_nbt(&source.join(path))?;
        let data: RawSavedData = decode(&bytes, path)?;
        if data.data_version != 5023 {
            return Err(invalid(format!("{path} must have DataVersion 5023")));
        }
        let fastnbt::Value::Compound(values) = data.data else {
            return Err(invalid(format!("{path} data must be a compound")));
        };
        let mut keys: Vec<_> = values.into_keys().collect();
        keys.sort();
        result.push(SavedDataSource {
            path: path.into(),
            data_version: 5023,
            sha256,
            keys,
        });
    }
    Ok(result)
}

#[derive(Deserialize)]
struct RawLevel {
    #[serde(rename = "Data")]
    data: RawLevelData,
}
#[derive(Deserialize)]
struct RawLevelData {
    #[serde(rename = "DataVersion")]
    data_version: i32,
    #[serde(rename = "Version")]
    version: RawVersion,
    #[serde(rename = "LevelName")]
    name: String,
    #[serde(default)]
    version_history: Vec<i32>,
    #[serde(rename = "GameType")]
    game_mode: i32,
    #[serde(rename = "Time")]
    game_time: i64,
    spawn: RawSpawn,
    #[serde(rename = "difficulty_settings")]
    difficulty: RawDifficulty,
}
#[derive(Deserialize)]
struct RawVersion {
    #[serde(rename = "Id")]
    id: i32,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Series")]
    series: String,
    #[serde(rename = "Snapshot")]
    snapshot: i8,
}
#[derive(Deserialize)]
struct RawSpawn {
    dimension: String,
    #[serde(rename = "pos")]
    position: fastnbt::IntArray,
    #[serde(default)]
    yaw: f32,
    #[serde(default)]
    pitch: f32,
}
#[derive(Deserialize)]
struct RawDifficulty {
    difficulty: String,
    #[serde(default)]
    hardcore: i8,
    #[serde(default)]
    locked: i8,
}
#[derive(Deserialize)]
struct RawWorldGen {
    #[serde(rename = "DataVersion")]
    data_version: i32,
    data: RawGenerator,
}
#[derive(Deserialize)]
struct RawGenerator {
    seed: i64,
    generate_structures: i8,
    bonus_chest: i8,
    dimensions: BTreeMap<String, RawDimension>,
}
#[derive(Deserialize)]
struct RawDimension {
    #[serde(rename = "type")]
    dimension_type: fastnbt::Value,
    generator: RawGeneratorKind,
}
#[derive(Deserialize)]
struct RawGeneratorKind {
    #[serde(rename = "type")]
    kind: String,
}
#[derive(Deserialize)]
struct RawSavedData {
    #[serde(rename = "DataVersion")]
    data_version: i32,
    data: fastnbt::Value,
}
