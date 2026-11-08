use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    error::Error,
    fs::{self, File},
    hint::black_box,
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use uuid::Uuid;
use zerqon_world::{
    Chunk, ChunkTicket, ChunkWorkers, GenerationPolicy, WorldError, WorldMetadata, WorldStore,
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
const MAX_DOCUMENT: u64 = 8 * 1024 * 1024;
const MAX_CHUNKS: usize = 4096;
const MAX_LOADS: usize = 10_000_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    schema: u32,
    minecraft: String,
    data_version: i32,
    world_id: Uuid,
    seed: i64,
    generator_kind: String,
    generator_settings: String,
    level_dat_sha256: String,
    generator_sha256: String,
    retained_manifest_sha256: String,
    chunks: Vec<SelectedChunk>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectedChunk {
    x: i32,
    z: i32,
    encoded_bytes: u64,
    encoded_sha256: String,
}

#[derive(Deserialize)]
struct GeneratorFile {
    #[serde(rename = "DataVersion")]
    data_version: i32,
    data: GeneratorData,
}

#[derive(Deserialize)]
struct GeneratorData {
    seed: i64,
    dimensions: BTreeMap<String, DimensionData>,
}

#[derive(Deserialize)]
struct DimensionData {
    generator: GeneratorDataSource,
}

#[derive(Deserialize)]
struct GeneratorDataSource {
    #[serde(rename = "type")]
    kind: String,
    settings: fastnbt::Value,
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn read_regular(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > maximum {
        return Err(format!("not a bounded regular file: {}", path.display()).into());
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err("input grew beyond its bound".into());
    }
    Ok(bytes)
}

fn require_prepared_store(root: &Path) -> Result<()> {
    if !root.is_dir() {
        return Err("benchmark world must already exist".into());
    }
    for name in [
        "chunks.redb",
        "storage-format.json",
        "IMPORT_INCOMPLETE",
        "world-metadata.json",
    ] {
        let metadata = fs::symlink_metadata(root.join(name))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() == 0 {
            return Err(format!(
                "prepare/migrate a dedicated benchmark store before running: {name}"
            )
            .into());
        }
    }
    Ok(())
}

fn validate_provenance(root: &Path, selection: &Selection) -> Result<WorldMetadata> {
    if selection.schema != 1
        || selection.minecraft != "26.3"
        || selection.data_version != 5023
        || selection.world_id.is_nil()
        || selection.generator_kind != "minecraft:noise"
        || selection.generator_settings != "minecraft:overworld"
        || selection.chunks.is_empty()
        || selection.chunks.len() > MAX_CHUNKS
    {
        return Err(
            "selection must describe a bounded Minecraft 26.3 normal Overworld dataset".into(),
        );
    }
    for value in [
        &selection.level_dat_sha256,
        &selection.generator_sha256,
        &selection.retained_manifest_sha256,
    ] {
        if !valid_digest(value) {
            return Err("invalid provenance digest".into());
        }
    }
    let mut coordinates = BTreeSet::new();
    for chunk in &selection.chunks {
        if !coordinates.insert((chunk.x, chunk.z))
            || !(26..=1024 * 1024).contains(&chunk.encoded_bytes)
            || !valid_digest(&chunk.encoded_sha256)
        {
            return Err("selection contains a duplicate or invalid chunk record".into());
        }
    }
    let metadata =
        WorldMetadata::load(root)?.ok_or("benchmark requires imported world metadata")?;
    if metadata.generation_policy != GenerationPolicy::ExistingOnly
        || metadata.world_id != selection.world_id
        || metadata.source.minecraft_version != selection.minecraft
        || metadata.source.data_version != selection.data_version
        || metadata.generator.seed != selection.seed
        || metadata.source.level_dat_sha256 != selection.level_dat_sha256
        || metadata.source.generator_sha256 != selection.generator_sha256
        || metadata
            .retained_source
            .as_ref()
            .map(|value| &value.manifest_sha256)
            != Some(&selection.retained_manifest_sha256)
        || metadata
            .generator
            .dimensions
            .get("minecraft:overworld")
            .map(|value| value.generator_kind.as_str())
            != Some("minecraft:noise")
    {
        return Err("selection provenance differs from the imported ExistingOnly world".into());
    }
    let retained = root.join("source-data/files");
    let level = read_regular(&retained.join("level.dat"), MAX_DOCUMENT)?;
    if digest(&level) != selection.level_dat_sha256 {
        return Err("retained level.dat changed".into());
    }
    let compressed = read_regular(
        &retained.join("data/minecraft/world_gen_settings.dat"),
        MAX_DOCUMENT,
    )?;
    if digest(&compressed) != selection.generator_sha256 {
        return Err("retained generator changed".into());
    }
    let mut decoder = flate2::bufread::GzDecoder::new(compressed.as_slice());
    let mut bytes = Vec::new();
    decoder
        .by_ref()
        .take(MAX_DOCUMENT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DOCUMENT || !decoder.into_inner().is_empty() {
        return Err("invalid generator gzip size/tail".into());
    }
    let generator: GeneratorFile =
        fastnbt::from_bytes_with_opts(&bytes, fastnbt::DeOpts::new().max_seq_len(65_536))?;
    let overworld = generator
        .data
        .dimensions
        .get("minecraft:overworld")
        .ok_or("missing overworld generator")?;
    if generator.data_version != 5023
        || generator.data.seed != selection.seed
        || overworld.generator.kind != "minecraft:noise"
        || !matches!(&overworld.generator.settings, fastnbt::Value::String(value) if value == "minecraft:overworld")
    {
        return Err("saved generator differs from the selected normal Overworld preset".into());
    }
    Ok(metadata)
}

fn verify_all(store: &WorldStore, selection: &Selection) -> Result<()> {
    for expected in &selection.chunks {
        let chunk = store
            .load(expected.x, expected.z)?
            .ok_or_else(|| format!("missing selected chunk ({}, {})", expected.x, expected.z))?;
        let bytes = chunk.to_bytes();
        if chunk.x != expected.x
            || chunk.z != expected.z
            || bytes.len() as u64 != expected.encoded_bytes
            || digest(&bytes) != expected.encoded_sha256
        {
            return Err(format!(
                "chunk data differs from frozen selection: ({}, {})",
                expected.x, expected.z
            )
            .into());
        }
    }
    Ok(())
}

#[derive(Default)]
struct Totals {
    loads: u64,
    logical_bytes: u64,
    decoded_payload_bytes_sum: u64,
    max_decoded_chunk_bytes: u64,
}

struct Loaded {
    logical_bytes: u64,
    decoded_bytes: u64,
}

fn collect(ticket: ChunkTicket<Loaded>, totals: &mut Totals) -> Result<()> {
    let loaded = ticket.wait()?;
    totals.loads += 1;
    totals.logical_bytes += loaded.logical_bytes;
    totals.decoded_payload_bytes_sum += loaded.decoded_bytes;
    totals.max_decoded_chunk_bytes = totals.max_decoded_chunk_bytes.max(loaded.decoded_bytes);
    Ok(())
}

fn load_passes(
    store: &Arc<WorldStore>,
    workers: &ChunkWorkers,
    selection: &Selection,
    passes: usize,
    window: usize,
) -> Result<Totals> {
    let mut pending = VecDeque::with_capacity(window);
    let mut totals = Totals::default();
    for _ in 0..passes {
        for expected in &selection.chunks {
            let (x, z, logical_bytes) = (expected.x, expected.z, expected.encoded_bytes);
            loop {
                let store = Arc::clone(store);
                match workers.try_submit(move || {
                    let chunk = store.load(x, z)?.ok_or(WorldError::Corrupt(
                        "benchmark selection is missing a chunk",
                    ))?;
                    if chunk.x != x || chunk.z != z {
                        return Err(WorldError::Corrupt("benchmark chunk coordinates differ"));
                    }
                    let decoded_bytes =
                        std::mem::size_of::<Chunk>() as u64 + chunk.heap_bytes() as u64;
                    black_box(&chunk);
                    drop(chunk);
                    Ok(Loaded {
                        logical_bytes,
                        decoded_bytes,
                    })
                }) {
                    Ok(ticket) => {
                        pending.push_back(ticket);
                        break;
                    }
                    Err(WorldError::QueueFull) => {
                        collect(
                            pending
                                .pop_front()
                                .ok_or("queue full with no submitted work")?,
                            &mut totals,
                        )?;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            if pending.len() >= window {
                collect(pending.pop_front().unwrap(), &mut totals)?;
            }
        }
    }
    while let Some(ticket) = pending.pop_front() {
        collect(ticket, &mut totals)?;
    }
    Ok(totals)
}

fn signal(expected: &str) -> Result<()> {
    let mut line = String::new();
    std::io::stdin().lock().take(32).read_line(&mut line)?;
    if line.trim_end() != expected {
        return Err(format!("expected driver signal {expected}").into());
    }
    Ok(())
}

fn emit(value: serde_json::Value) -> Result<()> {
    println!("{value}");
    std::io::stdout().flush()?;
    Ok(())
}

fn main() -> Result<()> {
    if cfg!(debug_assertions) {
        return Err("build this benchmark with --release".into());
    }
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args
        .first()
        .is_some_and(|argument| argument == "--prepare-store")
    {
        if args.len() != 3 {
            return Err("usage: ordinary_load_bench --prepare-store WORLD SELECTION.json".into());
        }
        let root = PathBuf::from(&args[1]).canonicalize()?;
        let selection: Selection =
            serde_json::from_slice(&read_regular(Path::new(&args[2]), MAX_DOCUMENT)?)?;
        validate_provenance(&root, &selection)?;
        let store = WorldStore::open(&root)?;
        verify_all(&store, &selection)?;
        emit(
            json!({"event":"prepared", "selected_chunks":selection.chunks.len(), "world_id":store.uuid()?, "generation_policy":"existing_only", "timed":false}),
        )?;
        return Ok(());
    }
    if !(4..=5).contains(&args.len()) || args.get(4).is_some_and(|value| value != "--coordinated") {
        return Err(
            "usage: ordinary_load_bench WORLD SELECTION.json WORKERS PASSES [--coordinated]".into(),
        );
    }
    let root = PathBuf::from(&args[0]).canonicalize()?;
    let selection_bytes = read_regular(Path::new(&args[1]), MAX_DOCUMENT)?;
    let selection_hash = digest(&selection_bytes);
    let selection: Selection = serde_json::from_slice(&selection_bytes)?;
    let threads = args[2].parse::<usize>()?;
    let passes = args[3].parse::<usize>()?;
    if !(1..=256).contains(&threads)
        || !(1..=10_000).contains(&passes)
        || selection
            .chunks
            .len()
            .checked_mul(passes)
            .is_none_or(|value| value > MAX_LOADS)
    {
        return Err("worker/pass/workload limits exceeded".into());
    }
    require_prepared_store(&root)?;
    let metadata = validate_provenance(&root, &selection)?;
    let store = Arc::new(WorldStore::open(&root)?);
    if store.generation_policy() != GenerationPolicy::ExistingOnly
        || store.uuid()? != selection.world_id
    {
        return Err("opened store differs from the selected imported world".into());
    }
    verify_all(&store, &selection)?;
    let workers = ChunkWorkers::new(threads, threads * 2)?;
    let window = threads * 3;
    let _warmup = load_passes(&store, &workers, &selection, 1, window)?;
    let provenance = json!({
        "minecraft":"26.3", "data_version":5023, "seed":selection.seed,
        "world_id":selection.world_id, "generator_kind":selection.generator_kind,
        "generator_settings":selection.generator_settings, "selection_sha256":selection_hash,
        "retained_manifest_sha256":selection.retained_manifest_sha256,
        "source_level_dat_sha256":metadata.source.level_dat_sha256,
        "source_generator_sha256":metadata.source.generator_sha256,
        "reported_build_commit":option_env!("ZERQON_BENCH_COMMIT"),
        "crate_version":env!("CARGO_PKG_VERSION"), "debug_assertions":cfg!(debug_assertions),
    });
    let coordinated = args.len() == 5;
    if coordinated {
        emit(
            json!({"event":"ready", "mode":"warm_imported_block_read_decode", "workers":threads, "selected_chunks":selection.chunks.len(), "passes":passes, "queue_capacity":threads*2, "inflight_limit":window, "provenance":provenance}),
        )?;
        signal("go")?;
    }
    let start = Instant::now();
    let totals = load_passes(&store, &workers, &selection, passes, window)?;
    let elapsed = start.elapsed().as_secs_f64();
    if totals.loads != (selection.chunks.len() * passes) as u64 || elapsed == 0.0 {
        return Err("incomplete workload or insufficient timer resolution".into());
    }
    let measured = json!({
        "event":"measured", "mode":"warm_imported_block_read_decode", "workers":threads,
        "selected_chunks":selection.chunks.len(), "passes":passes, "completed_loads":totals.loads,
        "logical_encoded_bytes":totals.logical_bytes, "timed_loop_wall_seconds":elapsed,
        "chunks_per_second":totals.loads as f64/elapsed,
        "logical_encoded_bytes_per_second":totals.logical_bytes as f64/elapsed,
        "decoded_chunk_bytes_max":totals.max_decoded_chunk_bytes,
        "decoded_payload_bytes_sum":totals.decoded_payload_bytes_sum,
        "queue_capacity":threads*2, "inflight_limit":window,
        "cache_state":"explicit untimed warmup; redb 16 MiB; OS cache uncontrolled",
        "memory_definition":"decoded payload+struct sizes exclude allocator, temporary decode copies, DB cache and process RSS",
        "provenance":provenance,
    });
    emit(measured)?;
    if coordinated {
        signal("verify")?;
    }
    verify_all(&store, &selection)?;
    workers.shutdown()?;
    emit(
        json!({"event":"verified", "all_selected_chunks_match":true, "selection_sha256":selection_hash}),
    )?;
    if coordinated {
        signal("exit")?;
    }
    Ok(())
}
