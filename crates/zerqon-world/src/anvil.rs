use crate::{Chunk, SECTION_COUNT, Section};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

const SECTOR: usize = 4096;
const MAX_NBT: usize = 8 * 1024 * 1024;
pub const DATA_VERSION: i32 = 5023;

#[derive(Debug, Default, serde::Serialize)]
pub struct ImportReport {
    pub minecraft_version: String,
    pub data_version: i32,
    pub regions: usize,
    pub chunks: usize,
    pub incomplete_chunks_skipped: usize,
    pub block_entities_not_imported: usize,
    pub embedded_entities_not_imported: usize,
    pub block_ticks_not_imported: usize,
    pub fluid_ticks_not_imported: usize,
    pub biome_sections_not_imported: usize,
    pub light_sections_not_imported: usize,
    pub unsupported_root_fields: std::collections::BTreeSet<String>,
    pub source_artifacts_not_imported: Vec<String>,
    pub full_world_import: bool,
    pub world_id: Option<uuid::Uuid>,
    pub applied: Vec<String>,
    pub retained: Option<crate::metadata::RetainedSource>,
    pub not_supported: Vec<String>,
}

pub fn import_world_blocks(
    source: &Path,
    destination: &Path,
    resolver: &impl Fn(&str, &BTreeMap<String, String>) -> Option<u32>,
) -> Result<ImportReport, ImportError> {
    import_world_blocks_cancellable(source, destination, resolver, &|| false)
}

pub fn import_world_blocks_cancellable(
    source: &Path,
    destination: &Path,
    resolver: &impl Fn(&str, &BTreeMap<String, String>) -> Option<u32>,
    cancelled: &impl Fn() -> bool,
) -> Result<ImportReport, ImportError> {
    let check = || {
        if cancelled() {
            Err(crate::WorldError::Cancelled)
        } else {
            Ok(())
        }
    };
    check()?;
    let snapshot = crate::source_data::SourceSnapshot::open(source, cancelled)?;
    let source = snapshot.root().to_owned();
    let mut world_metadata = crate::WorldMetadata::from_vanilla(&source)?;
    check()?;
    crate::metadata::reject_links(destination)?;
    if destination.try_exists()? || std::fs::symlink_metadata(destination).is_ok() {
        return Err(invalid("destination must not already exist"));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| invalid("destination needs a parent directory"))?;
    let parent = crate::metadata::source_root(if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    })?;
    if parent.starts_with(&source) {
        return Err(invalid("destination must be outside the source world"));
    }
    let region = source.join("region");
    let modern = source.join("dimensions/minecraft/overworld/region");
    let regions = match (region.is_dir(), modern.is_dir()) {
        (true, false) => region,
        (false, true) => modern,
        (true, true) => {
            return Err(invalid(
                "both legacy and modern overworld region directories exist",
            ));
        }
        _ => return Err(invalid("overworld region directory was not found")),
    };
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&regions)? {
        check()?;
        let entry = entry?;
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "mca")
        {
            if !entry.file_type()?.is_file() {
                return Err(invalid("region entries must be regular files"));
            }
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| invalid("region filename is not UTF-8"))?;
            let parts: Vec<_> = name.split('.').collect();
            if parts.len() != 4 || parts[0] != "r" || parts[3] != "mca" {
                return Err(invalid("invalid region filename"));
            }
            let x = parts[1]
                .parse::<i32>()
                .map_err(|_| invalid("invalid region X"))?;
            let z = parts[2]
                .parse::<i32>()
                .map_err(|_| invalid("invalid region Z"))?;
            if files.len() >= 65_536 {
                return Err(invalid("world exceeds 65536 region import limit"));
            }
            files.push((x, z, entry.path()));
        }
    }
    if files.is_empty() {
        return Err(invalid("source has no overworld region files"));
    }
    files.sort_by_key(|entry| (entry.0, entry.1));
    if files
        .windows(2)
        .any(|pair| (pair[0].0, pair[0].1) == (pair[1].0, pair[1].1))
    {
        return Err(invalid("duplicate region coordinates"));
    }
    check()?;
    std::fs::create_dir(destination)?;
    let marker = destination.join("IMPORT_INCOMPLETE");
    use std::io::Write;
    let mut marker_file = match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&marker)
    {
        Ok(file) => file,
        Err(error) => {
            if let Err(cleanup) = std::fs::remove_dir(destination) {
                return Err(invalid(format!(
                    "cannot create import marker ({error}); empty destination cleanup failed ({cleanup})"
                )));
            }
            return Err(error.into());
        }
    };
    marker_file.write_all(b"World import in progress; do not remove this marker. Original source data is unchanged.\n")?;
    marker_file.sync_all()?;
    drop(marker_file);
    #[cfg(unix)]
    {
        File::open(destination)?.sync_all()?;
        File::open(&parent)?.sync_all()?;
    }
    let store = crate::WorldStore::open_import(destination)?;
    world_metadata.retained_source =
        Some(snapshot.retain_into(destination, &world_metadata, cancelled)?);
    let retained_root = destination.join("source-data/files");
    let mut report = ImportReport {
        minecraft_version: "26.3".into(),
        data_version: DATA_VERSION,
        full_world_import: false,
        world_id: Some(world_metadata.world_id),
        applied: vec![
            "chunk.block_states".into(),
            "world_metadata.identity".into(),
            "world_metadata.spawn".into(),
            "generation_policy.existing_only".into(),
        ],
        retained: world_metadata.retained_source.clone(),
        not_supported: vec![
            "source_game_mode_execution".into(),
            "source_game_time_execution".into(),
            "source_difficulty_execution".into(),
            "source_game_rules_and_clocks_execution".into(),
            "source_generator_execution".into(),
            "non_overworld_chunks".into(),
            "biomes_and_lighting".into(),
            "block_entity_behavior_and_inventories".into(),
            "scheduled_ticks".into(),
            "vanilla_entities_and_playerdata".into(),
            "datapack_execution".into(),
        ],
        ..ImportReport::default()
    };
    for artifact in [
        "level.dat",
        "level.dat_old",
        "entities",
        "poi",
        "playerdata",
        "players",
        "data",
        "datapacks",
        "DIM-1",
        "DIM1",
    ] {
        if source.join(artifact).exists() {
            report.source_artifacts_not_imported.push(artifact.into());
        }
    }
    let dimensions = match std::fs::read_dir(source.join("dimensions")) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    for namespace in dimensions.into_iter().flatten() {
        check()?;
        let namespace = namespace?;
        if !namespace.file_type()?.is_dir() {
            continue;
        }
        for dimension in std::fs::read_dir(namespace.path())? {
            check()?;
            let dimension = dimension?;
            if namespace.file_name() == "minecraft"
                && dimension.file_name() == "overworld"
                && dimension.file_type()?.is_dir()
            {
                for child in std::fs::read_dir(dimension.path())? {
                    check()?;
                    let child = child?;
                    if child.file_name() != "region" {
                        report.source_artifacts_not_imported.push(
                            child
                                .path()
                                .strip_prefix(&source)
                                .map_err(|_| invalid("dimension path escaped source"))?
                                .to_string_lossy()
                                .into_owned(),
                        );
                        if report.source_artifacts_not_imported.len() > 1024 {
                            return Err(invalid("too many unsupported world artifacts"));
                        }
                    }
                }
            } else {
                report.source_artifacts_not_imported.push(
                    dimension
                        .path()
                        .strip_prefix(&source)
                        .map_err(|_| invalid("dimension path escaped source"))?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            if report.source_artifacts_not_imported.len() > 1024 {
                return Err(invalid("too many unsupported world artifacts"));
            }
        }
    }
    for (x, z, path) in files {
        check()?;
        let relative = path
            .strip_prefix(&source)
            .map_err(|_| invalid("region escaped source world"))?;
        let mut region = RegionReader::open(&retained_root.join(relative), x, z)?;
        for local_z in 0..32 {
            for local_x in 0..32 {
                check()?;
                let imported = match region.read_chunk(local_x, local_z, resolver) {
                    Ok(value) => value,
                    Err(ImportError::IncompleteChunk(_)) => {
                        report.incomplete_chunks_skipped += 1;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if let Some(imported) = imported {
                    check()?;
                    store.save(&imported.chunk)?;
                    report.chunks += 1;
                    report.block_entities_not_imported += imported.metadata.block_entities;
                    report.embedded_entities_not_imported += imported.metadata.entities;
                    report.block_ticks_not_imported += imported.metadata.block_ticks;
                    report.fluid_ticks_not_imported += imported.metadata.fluid_ticks;
                    report.biome_sections_not_imported += imported.metadata.biome_sections;
                    report.light_sections_not_imported += imported.metadata.light_sections;
                    report
                        .unsupported_root_fields
                        .extend(imported.metadata.unsupported_root_fields);
                }
            }
        }
        report.regions += 1;
    }
    if report.chunks == 0 {
        return Err(invalid("source contains no completed overworld chunks"));
    }
    check()?;
    world_metadata.write_new(destination)?;
    let bytes = serde_json::to_vec_pretty(&report)
        .map_err(|error| invalid(format!("report serialization: {error}")))?;
    let mut report_file = tempfile::NamedTempFile::new_in(destination)?;
    report_file.write_all(&bytes)?;
    report_file.as_file().sync_all()?;
    report_file
        .persist_noclobber(destination.join("import-report.json"))
        .map_err(|error| error.error)?;
    check()?;
    std::fs::remove_file(marker)?;
    #[cfg(unix)]
    File::open(destination)?.sync_all()?;
    Ok(report)
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("invalid Anvil world: {0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("unsupported chunk DataVersion {0}; expected Minecraft 26.3 ({DATA_VERSION})")]
    Version(i32),
    #[error("chunk generation status {0} is incomplete")]
    IncompleteChunk(String),
    #[error("unknown block state {name} with properties {properties:?}")]
    UnknownState {
        name: String,
        properties: BTreeMap<String, String>,
    },
    #[error(transparent)]
    World(#[from] crate::WorldError),
}

pub struct ImportedChunk {
    pub chunk: Chunk,
    pub metadata: ChunkMetadata,
}
#[derive(Debug, Default, serde::Serialize)]
pub struct ChunkMetadata {
    pub block_entities: usize,
    pub entities: usize,
    pub block_ticks: usize,
    pub fluid_ticks: usize,
    pub biome_sections: usize,
    pub light_sections: usize,
    pub unsupported_root_fields: Vec<String>,
}
pub struct RegionReader {
    file: File,
    locations: [u32; 1024],
    region_x: i32,
    region_z: i32,
}
impl RegionReader {
    pub fn open(path: &Path, region_x: i32, region_z: i32) -> Result<Self, ImportError> {
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        if length < 8192 || length % SECTOR as u64 != 0 {
            return Err(invalid("region is truncated or not sector aligned"));
        }
        let mut header = [0u8; 8192];
        file.read_exact(&mut header)?;
        let mut locations = [0; 1024];
        let mut ranges = Vec::new();
        for (index, location) in locations.iter_mut().enumerate() {
            *location = u32::from_be_bytes(
                header[index * 4..index * 4 + 4]
                    .try_into()
                    .map_err(|_| invalid("invalid location table"))?,
            );
            let offset = *location >> 8;
            let count = *location & 255;
            if offset == 0 && count == 0 {
                continue;
            }
            if offset < 2 || count == 0 || u64::from(offset + count) * SECTOR as u64 > length {
                return Err(invalid("chunk sector allocation outside region"));
            }
            ranges.push((offset, offset + count));
        }
        ranges.sort_unstable();
        if ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
            return Err(invalid("overlapping chunk sectors"));
        }
        region_x
            .checked_mul(32)
            .and_then(|x| x.checked_add(31))
            .ok_or_else(|| invalid("region coordinates overflow"))?;
        region_z
            .checked_mul(32)
            .and_then(|z| z.checked_add(31))
            .ok_or_else(|| invalid("region coordinates overflow"))?;
        Ok(Self {
            file,
            locations,
            region_x,
            region_z,
        })
    }

    pub fn read_chunk(
        &mut self,
        local_x: u8,
        local_z: u8,
        resolver: &impl Fn(&str, &BTreeMap<String, String>) -> Option<u32>,
    ) -> Result<Option<ImportedChunk>, ImportError> {
        if local_x >= 32 || local_z >= 32 {
            return Err(invalid("local region coordinates must be 0..31"));
        }
        let location = self.locations[local_z as usize * 32 + local_x as usize];
        if location == 0 {
            return Ok(None);
        }
        self.file
            .seek(SeekFrom::Start(u64::from(location >> 8) * SECTOR as u64))?;
        let mut prefix = [0; 5];
        self.file.read_exact(&mut prefix)?;
        let length = u32::from_be_bytes(
            prefix[..4]
                .try_into()
                .map_err(|_| invalid("invalid chunk length"))?,
        ) as usize;
        if length < 1 || length > (location as usize & 255) * SECTOR - 4 {
            return Err(invalid("chunk length exceeds allocated sectors"));
        }
        let mut payload = vec![0; length - 1];
        self.file.read_exact(&mut payload)?;
        let mut nbt = Vec::new();
        match prefix[4] {
            1 => {
                let mut decoder = flate2::bufread::GzDecoder::new(payload.as_slice());
                decode_limited(&mut decoder, &mut nbt)?;
                if !decoder.into_inner().is_empty() {
                    return Err(invalid("trailing gzip data"));
                }
            }
            2 => {
                let mut decoder = flate2::bufread::ZlibDecoder::new(payload.as_slice());
                decode_limited(&mut decoder, &mut nbt)?;
                if !decoder.into_inner().is_empty() {
                    return Err(invalid("trailing zlib data"));
                }
            }
            3 => nbt = payload,
            code if code & 128 != 0 => {
                return Err(invalid("external .mcc chunks are not supported"));
            }
            code => return Err(invalid(format!("compression type {code} is not supported"))),
        }
        let root_fields = validate_nbt(&nbt)?;
        let raw: RawChunk =
            fastnbt::from_bytes_with_opts(&nbt, fastnbt::DeOpts::new().max_seq_len(65_536))
                .map_err(|error| invalid(format!("NBT decode: {error}")))?;
        if raw.data_version != DATA_VERSION {
            return Err(ImportError::Version(raw.data_version));
        }
        if raw.x_pos != self.region_x * 32 + i32::from(local_x)
            || raw.z_pos != self.region_z * 32 + i32::from(local_z)
        {
            return Err(invalid("chunk coordinates do not match region slot"));
        }
        if raw.status != "minecraft:full" && raw.status != "full" {
            let status = raw.status.strip_prefix("minecraft:").unwrap_or(&raw.status);
            if !matches!(
                status,
                "empty"
                    | "structure_starts"
                    | "structure_references"
                    | "biomes"
                    | "noise"
                    | "terrain"
                    | "surface"
                    | "carvers"
                    | "features"
                    | "initialize_light"
                    | "light"
                    | "spawn"
            ) {
                return Err(invalid("unknown chunk generation status"));
            }
            return Err(ImportError::IncompleteChunk(raw.status));
        }
        if raw.y_pos != -4 {
            return Err(invalid("only overworld height -64..319 is supported"));
        }
        let air = resolver("minecraft:air", &BTreeMap::new()).ok_or_else(|| {
            ImportError::UnknownState {
                name: "minecraft:air".into(),
                properties: BTreeMap::new(),
            }
        })?;
        let mut chunk = Chunk {
            x: raw.x_pos,
            z: raw.z_pos,
            sections: std::array::from_fn(|_| Section::uniform(air)),
        };
        let mut metadata = ChunkMetadata {
            block_entities: raw.block_entities.len(),
            entities: raw.entities.len(),
            block_ticks: raw.block_ticks.len(),
            fluid_ticks: raw.fluid_ticks.len(),
            unsupported_root_fields: root_fields
                .into_iter()
                .filter(|name| {
                    !matches!(
                        name.as_str(),
                        "DataVersion" | "xPos" | "zPos" | "yPos" | "Status" | "sections"
                    )
                })
                .collect(),
            ..ChunkMetadata::default()
        };
        let mut seen = HashSet::new();
        for section in raw.sections {
            if !seen.insert(section.y) {
                return Err(invalid("duplicate section Y"));
            }
            metadata.biome_sections += usize::from(section.biomes.is_some());
            metadata.light_sections +=
                usize::from(section.block_light.is_some() || section.sky_light.is_some());
            let Some(states) = section.block_states else {
                continue;
            };
            let index = i32::from(section.y) + 4;
            if !(0..SECTION_COUNT as i32).contains(&index) {
                return Err(invalid("block section outside supported height"));
            }
            if states.palette.is_empty() || states.palette.len() > 4096 {
                return Err(invalid("block palette size outside 1..4096"));
            }
            let resolved: Vec<u32> = states
                .palette
                .into_iter()
                .map(|entry| {
                    let entry = palette_entry(entry)?;
                    resolver(&entry.name, &entry.properties).ok_or(ImportError::UnknownState {
                        name: entry.name,
                        properties: entry.properties,
                    })
                })
                .collect::<Result<_, _>>()?;
            let packed = states.data.as_deref().unwrap_or(&[]);
            if resolved.len() == 1 {
                if !packed.is_empty() {
                    return Err(invalid("singleton palette contains packed data"));
                }
                chunk.sections[index as usize] = Section::uniform(resolved[0]);
                continue;
            }
            let bits = (usize::BITS - (resolved.len() - 1).leading_zeros()).max(4) as usize;
            let per_word = 64 / bits;
            if packed.len() != 4096usize.div_ceil(per_word) {
                return Err(invalid("packed block state array length mismatch"));
            }
            let mask = (1u64 << bits) - 1;
            let mut values = Vec::with_capacity(4096);
            for block in 0..4096 {
                let id = ((packed[block / per_word] as u64 >> ((block % per_word) * bits)) & mask)
                    as usize;
                values.push(
                    *resolved
                        .get(id)
                        .ok_or_else(|| invalid("block palette index out of bounds"))?,
                );
            }
            chunk.sections[index as usize] = Section::from_states(&values)?;
        }
        Ok(Some(ImportedChunk { chunk, metadata }))
    }
}

fn invalid(message: impl Into<String>) -> ImportError {
    ImportError::Invalid(message.into())
}
fn decode_limited(reader: &mut impl Read, output: &mut Vec<u8>) -> Result<(), ImportError> {
    reader.take(MAX_NBT as u64 + 1).read_to_end(output)?;
    if output.len() > MAX_NBT {
        return Err(invalid("decompressed chunk exceeds 8 MiB"));
    }
    Ok(())
}

#[derive(Deserialize)]
struct RawChunk {
    #[serde(rename = "DataVersion")]
    data_version: i32,
    #[serde(rename = "xPos")]
    x_pos: i32,
    #[serde(rename = "zPos")]
    z_pos: i32,
    #[serde(rename = "yPos")]
    y_pos: i32,
    #[serde(rename = "Status")]
    status: String,
    sections: Vec<RawSection>,
    #[serde(default)]
    block_entities: Vec<fastnbt::Value>,
    #[serde(default)]
    entities: Vec<fastnbt::Value>,
    #[serde(default)]
    block_ticks: Vec<fastnbt::Value>,
    #[serde(default)]
    fluid_ticks: Vec<fastnbt::Value>,
}
#[derive(Deserialize)]
struct RawSection {
    #[serde(rename = "Y")]
    y: i8,
    block_states: Option<RawStates>,
    biomes: Option<fastnbt::Value>,
    #[serde(rename = "BlockLight")]
    block_light: Option<fastnbt::ByteArray>,
    #[serde(rename = "SkyLight")]
    sky_light: Option<fastnbt::ByteArray>,
}
#[derive(Deserialize)]
struct RawStates {
    palette: Vec<fastnbt::Value>,
    data: Option<fastnbt::LongArray>,
}
struct PaletteEntry {
    name: String,
    properties: BTreeMap<String, String>,
}

fn palette_entry(value: fastnbt::Value) -> Result<PaletteEntry, ImportError> {
    use fastnbt::Value;
    match value {
        Value::String(name) => Ok(PaletteEntry {
            name,
            properties: BTreeMap::new(),
        }),
        Value::Compound(mut object) => {
            if object.len() == 1 && object.contains_key("") {
                return palette_entry(
                    object
                        .remove("")
                        .ok_or_else(|| invalid("empty palette wrapper"))?,
                );
            }
            let name = match object.remove("id").or_else(|| object.remove("Name")) {
                Some(Value::String(name)) => name,
                _ => return Err(invalid("block palette entry needs a string id")),
            };
            let properties = match object
                .remove("properties")
                .or_else(|| object.remove("Properties"))
            {
                Some(Value::Compound(properties)) => properties
                    .into_iter()
                    .map(|(key, value)| match value {
                        Value::String(value) => Ok((key, value)),
                        _ => Err(invalid("block property must be a string")),
                    })
                    .collect::<Result<_, _>>()?,
                None => BTreeMap::new(),
                _ => return Err(invalid("block properties must be a compound")),
            };
            if !object.is_empty() {
                return Err(invalid("unknown block palette fields"));
            }
            Ok(PaletteEntry { name, properties })
        }
        _ => Err(invalid("unsupported block palette entry")),
    }
}

pub(crate) fn validate_nbt(data: &[u8]) -> Result<Vec<String>, ImportError> {
    if data.len() > MAX_NBT {
        return Err(invalid("NBT exceeds 8 MiB"));
    }
    let mut scan = NbtScan {
        data,
        position: 0,
        nodes: 0,
        root_fields: Vec::new(),
    };
    if scan.byte()? != 10 {
        return Err(invalid("NBT root must be a compound"));
    }
    scan.string()?;
    scan.payload(10, 0)?;
    if scan.position != data.len() {
        return Err(invalid("trailing NBT data"));
    }
    Ok(scan.root_fields)
}
struct NbtScan<'a> {
    data: &'a [u8],
    position: usize,
    nodes: usize,
    root_fields: Vec<String>,
}
impl<'a> NbtScan<'a> {
    fn bytes(&mut self, count: usize) -> Result<&'a [u8], ImportError> {
        let end = self
            .position
            .checked_add(count)
            .ok_or_else(|| invalid("NBT length overflow"))?;
        let data = self
            .data
            .get(self.position..end)
            .ok_or_else(|| invalid("truncated NBT"))?;
        self.position = end;
        Ok(data)
    }
    fn byte(&mut self) -> Result<u8, ImportError> {
        Ok(self.bytes(1)?[0])
    }
    fn string(&mut self) -> Result<&'a [u8], ImportError> {
        let length = u16::from_be_bytes(
            self.bytes(2)?
                .try_into()
                .map_err(|_| invalid("NBT string length"))?,
        ) as usize;
        self.bytes(length)
    }
    fn count(&mut self) -> Result<usize, ImportError> {
        let value = i32::from_be_bytes(
            self.bytes(4)?
                .try_into()
                .map_err(|_| invalid("NBT array length"))?,
        );
        usize::try_from(value).map_err(|_| invalid("negative NBT array length"))
    }
    fn payload(&mut self, tag: u8, depth: usize) -> Result<(), ImportError> {
        self.nodes += 1;
        if depth > 64 || self.nodes > 65_536 {
            return Err(invalid("NBT nesting or tag count limit exceeded"));
        }
        match tag {
            1 => {
                self.bytes(1)?;
            }
            2 => {
                self.bytes(2)?;
            }
            3 | 5 => {
                self.bytes(4)?;
            }
            4 | 6 => {
                self.bytes(8)?;
            }
            8 => {
                self.string()?;
            }
            7 | 11 | 12 => {
                let count = self.count()?;
                let width = match tag {
                    7 => 1,
                    11 => 4,
                    _ => 8,
                };
                let size = count
                    .checked_mul(width)
                    .ok_or_else(|| invalid("NBT array size overflow"))?;
                self.bytes(size)?;
            }
            9 => {
                let element = self.byte()?;
                let count = self.count()?;
                if count > 65_536 || (element == 0 && count != 0) || element > 12 {
                    return Err(invalid("invalid or excessive NBT list"));
                }
                for _ in 0..count {
                    self.payload(element, depth + 1)?;
                }
            }
            10 => {
                let mut names = HashSet::new();
                loop {
                    let child = self.byte()?;
                    if child == 0 {
                        break;
                    }
                    let name = self.string()?;
                    if !names.insert(name) {
                        return Err(invalid("duplicate NBT compound key"));
                    }
                    if depth == 0 {
                        self.root_fields
                            .push(String::from_utf8_lossy(name).into_owned());
                    }
                    self.payload(child, depth + 1)?;
                }
            }
            _ => return Err(invalid("invalid NBT tag")),
        }
        Ok(())
    }
}
