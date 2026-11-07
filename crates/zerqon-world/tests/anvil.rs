use std::collections::BTreeMap;
use zerqon_world::anvil::RegionReader;
use zerqon_world::anvil::{ImportError, import_world_blocks};

fn resolve(name: &str, properties: &BTreeMap<String, String>) -> Option<u32> {
    match (
        name,
        properties.get("axis").map(String::as_str),
        properties.len(),
    ) {
        ("minecraft:air", None, 0) => Some(0),
        ("minecraft:stone", None, 0) => Some(77),
        ("minecraft:oak_log", Some("x"), 1) => Some(100),
        ("minecraft:oak_log", Some("y"), 1) => Some(101),
        _ => None,
    }
}

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/negative-3.mca")
}
fn region_with_payload(payload: &[u8], compression: u8) -> Vec<u8> {
    let sectors = (payload.len() + 5).div_ceil(4096);
    assert!(sectors <= 255);
    let mut region = vec![0; 8192 + sectors * 4096];
    region[4092..4096].copy_from_slice(&((2u32 << 8) | sectors as u32).to_be_bytes());
    region[8192..8196].copy_from_slice(&((payload.len() + 1) as u32).to_be_bytes());
    region[8196] = compression;
    region[8197..8197 + payload.len()].copy_from_slice(payload);
    region
}
fn read_bytes(bytes: &[u8]) -> Result<Option<zerqon_world::anvil::ImportedChunk>, ImportError> {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("region.mca");
    std::fs::write(&path, bytes).unwrap();
    RegionReader::open(&path, -1, -2)?.read_chunk(31, 31, &resolve)
}

#[test]
fn rejects_overlapping_outside_and_truncated_sector_allocations() {
    let original = std::fs::read(fixture_path()).unwrap();
    let mut overlap = original.clone();
    overlap[..4].copy_from_slice(&original[4092..4096]);
    assert!(read_bytes(&overlap).is_err());
    let mut header_sector = original.clone();
    header_sector[4092..4096].copy_from_slice(&257u32.to_be_bytes());
    assert!(read_bytes(&header_sector).is_err());
    let mut outside = original.clone();
    outside[4092..4096].copy_from_slice(&((10u32 << 8) | 1).to_be_bytes());
    assert!(read_bytes(&outside).is_err());
    assert!(read_bytes(&original[..original.len() - 1]).is_err());
    let mut bad_length = original;
    bad_length[8192..8196].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(read_bytes(&bad_length).is_err());
}

#[test]
fn rejects_unsupported_compression_external_chunks_and_decompression_bombs() {
    let nbt = include_bytes!("fixtures/negative-chunk.nbt");
    for method in [0, 4, 129, 130] {
        assert!(read_bytes(&region_with_payload(nbt, method)).is_err());
    }
    use std::io::Write;
    let mut compressor =
        flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    compressor.write_all(&vec![0; 8 * 1024 * 1024 + 1]).unwrap();
    assert!(read_bytes(&region_with_payload(&compressor.finish().unwrap(), 2)).is_err());
}

#[test]
fn rejects_other_versions_unknown_states_and_wrong_chunk_coordinates() {
    let nbt = include_bytes!("fixtures/negative-chunk.nbt");
    let mut old = nbt.to_vec();
    let offset = old
        .windows(11)
        .position(|bytes| bytes == b"DataVersion")
        .unwrap()
        + 11;
    old[offset..offset + 4].copy_from_slice(&4189i32.to_be_bytes());
    assert!(matches!(
        read_bytes(&region_with_payload(&old, 3)),
        Err(ImportError::Version(4189))
    ));
    let mut wrong = nbt.to_vec();
    let offset = wrong.windows(4).position(|bytes| bytes == b"xPos").unwrap() + 4;
    wrong[offset..offset + 4].copy_from_slice(&0i32.to_be_bytes());
    assert!(read_bytes(&region_with_payload(&wrong, 3)).is_err());
    let mut region = RegionReader::open(&fixture_path(), -1, -2).unwrap();
    assert!(matches!(
        region.read_chunk(31, 31, &|_, _| None),
        Err(ImportError::UnknownState { .. })
    ));
    let mut unknown_properties = nbt.to_vec();
    let property = unknown_properties
        .windows(4)
        .position(|bytes| bytes == b"axis")
        .unwrap()
        + 4
        + 2;
    unknown_properties[property] = b'z';
    assert!(
        matches!(read_bytes(&region_with_payload(&unknown_properties,3)),Err(ImportError::UnknownState{name,..}) if name=="minecraft:oak_log")
    );
}

#[test]
fn rejects_malformed_nbt_before_deserializing_it() {
    let original = include_bytes!("fixtures/negative-chunk.nbt");
    let mut trailing = original.to_vec();
    trailing.push(0);
    assert!(read_bytes(&region_with_payload(&trailing, 3)).is_err());
    assert!(read_bytes(&region_with_payload(&original[..original.len() - 1], 3)).is_err());
    let mut nested = vec![10, 0, 0];
    for _ in 0..100 {
        nested.extend_from_slice(&[10, 0, 1, b'x']);
    }
    nested.extend(vec![0; 101]);
    assert!(read_bytes(&region_with_payload(&nested, 3)).is_err());
    let mut bad_index = original.to_vec();
    let offset = bad_index
        .windows(4)
        .position(|bytes| bytes == b"data")
        .unwrap()
        + 4
        + 4;
    bad_index[offset + 7] = 15;
    assert!(read_bytes(&region_with_payload(&bad_index, 3)).is_err());
}

#[test]
fn block_import_preserves_original_and_reports_unimported_world_data() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir_all(source.join("dimensions/minecraft/overworld/region")).unwrap();
    let path = source.join("dimensions/minecraft/overworld/region/r.-1.-2.mca");
    let original = std::fs::read(fixture_path()).unwrap();
    std::fs::write(&path, &original).unwrap();
    std::fs::write(source.join("level.dat"), "preserve me").unwrap();
    let destination = root.path().join("new-world");
    let report = import_world_blocks(&source, &destination, &resolve).unwrap();
    assert_eq!(report.chunks, 1);
    assert_eq!(report.block_entities_not_imported, 1);
    assert!(!report.full_world_import);
    assert!(
        report
            .source_artifacts_not_imported
            .contains(&"level.dat".into())
    );
    assert!(!destination.join("IMPORT_INCOMPLETE").exists());
    let store = zerqon_world::WorldStore::open(&destination).unwrap();
    assert_eq!(
        store.load(-1, -33).unwrap().unwrap().get_block(15, -64, 0),
        Some(101)
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(
        std::fs::read_to_string(source.join("level.dat")).unwrap(),
        "preserve me"
    );
    assert!(import_world_blocks(&source, &destination, &resolve).is_err());
    assert!(import_world_blocks(&source, &source.join("nested"), &resolve).is_err());
}

#[test]
fn failed_import_keeps_an_explicit_incomplete_marker() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir_all(source.join("region")).unwrap();
    std::fs::copy(fixture_path(), source.join("region/r.-1.-2.mca")).unwrap();
    let destination = root.path().join("failed");
    assert!(import_world_blocks(&source, &destination, &|_, _| None).is_err());
    assert!(destination.join("IMPORT_INCOMPLETE").is_file());
    assert!(!destination.join("import-report.json").exists());
    assert!(zerqon_world::WorldStore::open(&destination).is_err());
}

#[test]
#[ignore = "Requires generated 26.3 data and python scripts/generate-test-world.py fixture in .local/vanilla-world-filled"]
fn imports_real_vanilla_263_world_and_preserves_explicit_blocks() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let blocks: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join(".cache/vanilla/26.3/generated/reports/blocks.json")).unwrap(),
    )
    .unwrap();
    let resolver = |name: &str, properties: &BTreeMap<String, String>| -> Option<u32> {
        blocks
            .get(name)?
            .get("states")?
            .as_array()?
            .iter()
            .find(|state| {
                let candidate: BTreeMap<String, String> = state
                    .get("properties")
                    .map(|value| serde_json::from_value(value.clone()).unwrap())
                    .unwrap_or_default();
                if properties.is_empty() {
                    state
                        .get("default")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false)
                } else {
                    &candidate == properties
                }
            })?
            .get("id")?
            .as_u64()
            .and_then(|id| u32::try_from(id).ok())
    };
    let destination = tempfile::tempdir().unwrap();
    let world = destination.path().join("world");
    let report = import_world_blocks(
        &root.join(".local/vanilla-world-filled/world"),
        &world,
        &resolver,
    )
    .unwrap();
    assert!(report.chunks >= 2);
    println!(
        "vanilla fixture imported {} full chunks from {} regions; skipped {} unfinished chunks; omitted {} biome sections and {} light sections",
        report.chunks,
        report.regions,
        report.incomplete_chunks_skipped,
        report.biome_sections_not_imported,
        report.light_sections_not_imported
    );
    let store = zerqon_world::WorldStore::open(&world).unwrap();
    assert_eq!(
        store.load(0, 0).unwrap().unwrap().get_block(0, 70, 0),
        resolver("minecraft:diamond_block", &BTreeMap::new())
    );
    assert_eq!(
        store.load(-1, -33).unwrap().unwrap().get_block(15, 70, 15),
        resolver(
            "minecraft:oak_log",
            &BTreeMap::from([("axis".into(), "x".into())])
        )
    );
}

#[test]
fn imports_independent_python_fixture_with_all_three_compressions() {
    for method in 1..=3 {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/fixtures/negative-{method}.mca"));
        let mut region = RegionReader::open(&path, -1, -2).unwrap();
        assert!(region.read_chunk(0, 0, &resolve).unwrap().is_none());
        let imported = region.read_chunk(31, 31, &resolve).unwrap().unwrap();
        assert_eq!((imported.chunk.x, imported.chunk.z), (-1, -33));
        for (x, y, z, state) in [
            (0, -64, 0, 77),
            (15, -64, 0, 101),
            (0, -63, 0, 101),
            (15, -49, 15, 77),
            (1, -64, 0, 0),
            (0, 0, 0, 100),
            (15, 15, 15, 100),
            (0, 16, 0, 0),
            (0, 319, 0, 0),
        ] {
            assert_eq!(
                imported.chunk.get_block(x, y, z),
                Some(state),
                "compression{method}, ({x},{y},{z})"
            );
        }
        assert_eq!(imported.metadata.block_entities, 1);
        assert_eq!(imported.metadata.block_ticks, 1);
        assert_eq!(imported.metadata.biome_sections, 3);
    }
}
