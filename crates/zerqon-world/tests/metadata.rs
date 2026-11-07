use std::io::{Read, Write};
use std::{fs, path::Path};
use zerqon_world::{Chunk, FlatPalette, WorldError, WorldStore};
use zerqon_world::{GenerationPolicy, WorldMetadata};

fn write_fixture(root: &Path) {
    fs::create_dir_all(root.join("data/minecraft")).unwrap();
    fs::write(
        root.join("level.dat"),
        include_bytes!("fixtures/metadata-level.dat"),
    )
    .unwrap();
    fs::write(
        root.join("data/minecraft/world_gen_settings.dat"),
        include_bytes!("fixtures/metadata-world-gen-settings.dat"),
    )
    .unwrap();
    fs::write(
        root.join("data/minecraft/game_rules.dat"),
        include_bytes!("fixtures/metadata-game-rules.dat"),
    )
    .unwrap();
}

#[test]
fn independent_nbt_fixture_preserves_spawn_identity_and_generator_policy() {
    let root = tempfile::tempdir().unwrap();
    write_fixture(root.path());
    let metadata = WorldMetadata::from_vanilla(root.path()).unwrap();
    assert_eq!(metadata.spawn.position, [-1, -60, -513]);
    assert_eq!(metadata.spawn.dimension, "minecraft:overworld");
    assert_eq!((metadata.spawn.yaw, metadata.spawn.pitch), (45.0, -12.5));
    assert_eq!(metadata.generation_policy, GenerationPolicy::ExistingOnly);
    assert!(!metadata.world_id.is_nil());
    assert_eq!(metadata.source.data_version, 5023);
    assert_eq!(metadata.source.minecraft_version, "26.3");
    assert_eq!(metadata.source_game_mode, 0);
    assert_eq!(metadata.source_game_time, 1234);
    assert_eq!(metadata.generator.seed, -12345);
    assert_eq!(
        metadata.generator.dimensions["minecraft:overworld"].generator_kind,
        "minecraft:flat"
    );
    let destination = tempfile::tempdir().unwrap();
    metadata.write_new(destination.path()).unwrap();
    let loaded = WorldMetadata::load(destination.path()).unwrap().unwrap();
    assert_eq!(loaded, metadata);
    assert!(metadata.write_new(destination.path()).is_err());
}

#[test]
fn invalid_saved_metadata_cannot_silently_enable_flat_generation() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("world-metadata.json"), b"{}").unwrap();
    assert!(WorldMetadata::load(directory.path()).is_err());
}

fn mutate_nbt(path: &Path, change: impl FnOnce(&mut fastnbt::Value)) {
    let compressed = fs::read(path).unwrap();
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(compressed.as_slice())
        .read_to_end(&mut bytes)
        .unwrap();
    let mut value: fastnbt::Value = fastnbt::from_bytes(&bytes).unwrap();
    change(&mut value);
    let encoded = fastnbt::to_bytes(&value).unwrap();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&encoded).unwrap();
    fs::write(path, gzip.finish().unwrap()).unwrap();
}
fn compound(value: &mut fastnbt::Value) -> &mut std::collections::HashMap<String, fastnbt::Value> {
    let fastnbt::Value::Compound(value) = value else {
        panic!("fixture compound")
    };
    value
}

#[test]
fn non_overworld_nonfinite_and_out_of_bounds_spawn_are_rejected() {
    for case in 0..4 {
        let root = tempfile::tempdir().unwrap();
        write_fixture(root.path());
        mutate_nbt(&root.path().join("level.dat"), |value| {
            let data = compound(compound(value).get_mut("Data").unwrap());
            let spawn = compound(data.get_mut("spawn").unwrap());
            match case {
                0 => {
                    spawn.insert(
                        "dimension".into(),
                        fastnbt::Value::String("minecraft:the_nether".into()),
                    );
                }
                1 => {
                    spawn.insert(
                        "pos".into(),
                        fastnbt::Value::IntArray(fastnbt::IntArray::new(vec![0, 10000, 0])),
                    );
                }
                2 => {
                    spawn.insert("yaw".into(), fastnbt::Value::Float(f32::NAN));
                }
                _ => {
                    spawn.insert(
                        "pos".into(),
                        fastnbt::Value::IntArray(fastnbt::IntArray::new(vec![i32::MIN, 0, 0])),
                    );
                }
            }
        });
        assert!(
            WorldMetadata::from_vanilla(root.path()).is_err(),
            "case {case}"
        );
    }
}

#[test]
fn generator_dimension_paths_types_and_versions_fail_closed() {
    for case in 0..3 {
        let root = tempfile::tempdir().unwrap();
        write_fixture(root.path());
        mutate_nbt(
            &root.path().join("data/minecraft/world_gen_settings.dat"),
            |value| {
                let root = compound(value);
                if case == 0 {
                    root.insert("DataVersion".into(), fastnbt::Value::Int(4189));
                    return;
                }
                let dims = compound(
                    compound(root.get_mut("data").unwrap())
                        .get_mut("dimensions")
                        .unwrap(),
                );
                if case == 1 {
                    let value = dims.remove("minecraft:the_nether").unwrap();
                    dims.insert("minecraft:../../outside".into(), value);
                } else {
                    compound(dims.get_mut("minecraft:overworld").unwrap()).insert(
                        "type".into(),
                        fastnbt::Value::String("minecraft:the_nether".into()),
                    );
                }
            },
        );
        assert!(WorldMetadata::from_vanilla(root.path()).is_err());
    }
}

#[test]
fn dimension_namespace_cannot_name_a_parent_directory() {
    let root = tempfile::tempdir().unwrap();
    write_fixture(root.path());
    mutate_nbt(
        &root.path().join("data/minecraft/world_gen_settings.dat"),
        |value| {
            let dimensions = compound(
                compound(compound(value).get_mut("data").unwrap())
                    .get_mut("dimensions")
                    .unwrap(),
            );
            let dimension = dimensions.remove("minecraft:the_nether").unwrap();
            dimensions.insert("..:outside".into(), dimension);
        },
    );
    assert!(WorldMetadata::from_vanilla(root.path()).is_err());
}

#[test]
fn source_nbt_and_native_json_have_bounded_and_strict_reads() {
    let root = tempfile::tempdir().unwrap();
    write_fixture(root.path());
    fs::write(root.path().join("level.dat"), b"invalid gzip").unwrap();
    assert!(WorldMetadata::from_vanilla(root.path()).is_err());
    write_fixture(root.path());
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&vec![0; 8 * 1024 * 1024 + 1]).unwrap();
    fs::write(root.path().join("level.dat"), gzip.finish().unwrap()).unwrap();
    assert!(WorldMetadata::from_vanilla(root.path()).is_err());
    fs::write(
        root.path().join("world-metadata.json"),
        vec![b' '; 1024 * 1024 + 1],
    )
    .unwrap();
    assert!(WorldMetadata::load(root.path()).is_err());
}

#[test]
fn altered_native_policy_and_missing_old_import_metadata_never_enable_flat() {
    let source = tempfile::tempdir().unwrap();
    write_fixture(source.path());
    let metadata = WorldMetadata::from_vanilla(source.path()).unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut json = serde_json::to_value(&metadata).unwrap();
    json["generation_policy"] = serde_json::json!("native_flat");
    fs::write(
        root.path().join("world-metadata.json"),
        serde_json::to_vec(&json).unwrap(),
    )
    .unwrap();
    assert!(WorldStore::open(root.path()).is_err());
    let old = tempfile::tempdir().unwrap();
    fs::write(old.path().join("import-report.json"), b"{}").unwrap();
    assert!(matches!(
        WorldStore::open(old.path()),
        Err(WorldError::Metadata(_))
    ));
    let legacy = tempfile::tempdir().unwrap();
    let store = WorldStore::open(legacy.path()).unwrap();
    assert_eq!(store.generation_policy(), GenerationPolicy::NativeFlat);
    assert!(store.load(0, 0).unwrap().is_none());
    store
        .save(&Chunk::flat(
            0,
            0,
            FlatPalette {
                air: 0,
                bedrock: 1,
                stone: 2,
                dirt: 3,
                grass: 4,
            },
        ))
        .unwrap();
    assert!(store.load(0, 0).unwrap().is_some());
}
