#![allow(dead_code)]

#[path = "../src/entity_packets.rs"]
mod entity_packets;
#[path = "../src/game_data.rs"]
mod game_data;
#[path = "../src/game_mode.rs"]
mod game_mode;
#[path = "../src/packets.rs"]
mod packets;

use game_data::GameData;
use game_mode::GameMode;
use serde_json::json;
use std::{fs, path::Path};
use zerqon_protocol::codec::{get_string, get_varint, take};

fn fixture(path: &Path) {
    fs::create_dir_all(path.join("generated/reports")).unwrap();
    fs::write(path.join("generated/reports/registries.json"),r#"{"minecraft:entity_type":{"entries":{"minecraft:player":{"protocol_id":159},"minecraft:pig":{"protocol_id":101}}}}"#).unwrap();
    fs::write(
        path.join("prepared.json"),
        r#"{"version":"26.3","protocol":777}"#,
    )
    .unwrap();
    fs::write(path.join("registries-sync.json"), json!({"version":"26.3","protocol":777,"registries":[
        {"name":"minecraft:dimension_type","entries":[{"id":"minecraft:the_end","data":{}},{"id":"minecraft:overworld","data":{}}]},
        {"name":"minecraft:worldgen/biome","entries":[{"id":"minecraft:plains","data":{}}]}
    ]}).to_string()).unwrap();
    fs::write(path.join("tags-sync.json"), r#"{"registries":[{"name":"minecraft:worldgen/biome","tags":[{"name":"minecraft:is_overworld","ids":[0]}]}]}"#).unwrap();
    fs::write(
        path.join("generated/reports/blocks.json"),
        json!({
            "minecraft:air":{"states":[{"id":0,"default":true}]},
            "minecraft:stone":{"states":[{"id":257,"default":true}]},
            "minecraft:dirt":{"states":[{"id":258,"default":true}]},
        "minecraft:grass_block":{"states":[{"id":259,"default":true,"properties":{"snowy":"false"}},{"id":260,"properties":{"snowy":"true"}}]},
            "minecraft:bedrock":{"states":[{"id":261,"default":true}]}
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn login_reports_authentication_separately_from_secure_chat_enforcement() {
    for online in [false, true] {
        let packet = packets::login(7, 20, 4, 1, GameMode::Creative, None, online);
        assert_eq!(&packet[packet.len() - 2..], &[u8::from(online), 0]);
    }
}

#[test]
fn login_preserves_current_and_optional_previous_game_modes() {
    for mode in [GameMode::Creative, GameMode::Survival] {
        for previous in [None, Some(GameMode::Creative), Some(GameMode::Survival)] {
            let packet = packets::login(7, 20, 4, 1, mode, previous, false);
            let mut body = packet.as_slice();
            assert_eq!(get_varint(&mut body).unwrap(), 50);
            assert_eq!(take(&mut body, 4).unwrap(), 7_i32.to_be_bytes());
            assert_eq!(take(&mut body, 2).unwrap(), &[0, 1]);
            assert_eq!(get_string(&mut body, 64).unwrap(), "minecraft:overworld");
            for expected in [20, 4, 4] {
                assert_eq!(get_varint(&mut body).unwrap(), expected);
            }
            assert_eq!(take(&mut body, 3).unwrap(), &[0, 1, 0]);
            assert_eq!(get_varint(&mut body).unwrap(), 1);
            assert_eq!(get_string(&mut body, 64).unwrap(), "minecraft:overworld");
            assert_eq!(take(&mut body, 8).unwrap(), 0_i64.to_be_bytes());
            assert_eq!(get_varint(&mut body).unwrap(), i32::from(mode));
            assert_eq!(
                get_varint(&mut body).unwrap(),
                previous.map_or(0, |mode| i32::from(mode) + 1)
            );
            assert_eq!(body, &[0, 1, 0, 0, 63, 0, 0]);
        }
    }
}

#[test]
fn survival_abilities_never_grant_creative_privileges_or_flight() {
    for (mode, flying, flags) in [
        (GameMode::Creative, false, 0x0d),
        (GameMode::Creative, true, 0x0f),
        (GameMode::Survival, false, 0),
        (GameMode::Survival, true, 0),
    ] {
        assert_eq!(
            packets::abilities(mode, flying),
            [65, flags, 0x3d, 0x4c, 0xcc, 0xcd, 0x3d, 0xcc, 0xcc, 0xcd]
        );
    }
}

#[test]
fn configuration_brand_encodes_default_and_unicode_names_as_protocol_strings() {
    for brand in ["zerqon", "teighto Привет 😀"] {
        let packet = packets::configuration_brand(brand);
        let mut body = packet.as_slice();
        assert_eq!(get_varint(&mut body).unwrap(), 1);
        assert_eq!(get_string(&mut body, 32767).unwrap(), "minecraft:brand");
        assert_eq!(get_string(&mut body, 32767).unwrap(), brand);
        assert!(body.is_empty());
    }
}

#[test]
fn loads_registry_ids_and_defaults_from_prepared_data() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let data = GameData::load(dir.path()).unwrap();
    assert_eq!(data.flat.stone, 257);
    assert_eq!(data.flat.grass, 259);
    assert_eq!(data.overworld_dimension, 1);
    assert_eq!(data.plains_biome, 0);
    assert_eq!(data.block_bits, 9);
    assert_eq!(data.entity_type("minecraft:player"), Some(159));
    assert_eq!(data.entity_type("minecraft:pig"), Some(101));
    assert_eq!(data.entity_type("minecraft:absent"), None);
    let frames = data.configuration_packets();
    assert_eq!(frames.first().unwrap()[0], 13);
    assert_eq!(frames.last().unwrap(), &[3]);
    let mut registry = frames[1].as_slice();
    assert_eq!(get_varint(&mut registry).unwrap(), 7);
    assert_eq!(
        get_string(&mut registry, 32767).unwrap(),
        "minecraft:dimension_type"
    );
    assert_eq!(get_varint(&mut registry).unwrap(), 2);
    assert_eq!(
        get_string(&mut registry, 32767).unwrap(),
        "minecraft:the_end"
    );
    assert_eq!(take(&mut registry, 1).unwrap(), &[0]);
    assert_eq!(
        get_string(&mut registry, 32767).unwrap(),
        "minecraft:overworld"
    );
    assert_eq!(registry, &[0]);
}

#[test]
fn import_resolver_uses_defaults_and_rejects_unknown_states() {
    use std::collections::BTreeMap;
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let resolver = game_data::BlockResolver::load(dir.path()).unwrap();
    assert_eq!(
        resolver.resolve("minecraft:stone", &BTreeMap::new()),
        Some(257)
    );
    assert_eq!(
        resolver.resolve(
            "minecraft:grass_block",
            &BTreeMap::from([("snowy".into(), "true".into())])
        ),
        Some(260)
    );
    assert_eq!(
        resolver.resolve("minecraft:grass_block", &BTreeMap::new()),
        Some(259)
    );
    assert_eq!(
        resolver.resolve(
            "minecraft:grass_block",
            &BTreeMap::from([("snowy".into(), "invalid".into())])
        ),
        None
    );
    assert_eq!(resolver.resolve("minecraft:absent", &BTreeMap::new()), None);
    assert_eq!(
        resolver.resolve(
            "minecraft:grass_block",
            &BTreeMap::from([("absent".into(), "true".into())])
        ),
        None
    );
}

#[test]
fn import_resolver_merges_only_known_partial_properties() {
    use std::collections::BTreeMap;
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let path = dir.path().join("generated/reports/blocks.json");
    let mut blocks: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    blocks["minecraft:oak_stairs"] = json!({"states":[
        {"id":1000,"default":true,"properties":{"facing":"north","half":"bottom"}},
        {"id":1001,"properties":{"facing":"east","half":"bottom"}},
        {"id":1002,"properties":{"facing":"east","half":"top"}}
    ]});
    fs::write(path, serde_json::to_vec(&blocks).unwrap()).unwrap();
    let resolver = game_data::BlockResolver::load(dir.path()).unwrap();
    assert_eq!(
        resolver.resolve(
            "minecraft:oak_stairs",
            &BTreeMap::from([("facing".into(), "east".into())])
        ),
        Some(1001)
    );
    assert_eq!(
        resolver.resolve(
            "minecraft:oak_stairs",
            &BTreeMap::from([
                ("facing".into(), "east".into()),
                ("half".into(), "top".into())
            ])
        ),
        Some(1002)
    );
}

#[test]
fn refuses_mismatched_data_and_missing_defaults() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    fs::write(
        dir.path().join("prepared.json"),
        r#"{"version":"1.21.11","protocol":774}"#,
    )
    .unwrap();
    assert!(GameData::load(dir.path()).is_err());
    fixture(dir.path());
    fs::write(dir.path().join("generated/reports/blocks.json"), "{}").unwrap();
    assert!(GameData::load(dir.path()).is_err());
}

#[test]
fn water_and_waterlogged_blocks_both_contribute_to_section_fluid_counts() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let path = dir.path().join("generated/reports/blocks.json");
    let mut blocks: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    blocks["minecraft:water"] = json!({"states":[{"id":263,"default":true}]});
    blocks["minecraft:oak_fence"] =
        json!({"states":[{"id":264,"default":true,"properties":{"waterlogged":"true"}}]});
    fs::write(path, serde_json::to_vec(&blocks).unwrap()).unwrap();
    let data = GameData::load(dir.path()).unwrap();
    assert!(data.has_fluid(263) && data.has_fluid(264));
    let mut chunk = zerqon_world::Chunk::flat(0, 0, data.flat);
    chunk.set_block(0, -64, 0, 263).unwrap();
    chunk.set_block(1, -64, 0, 264).unwrap();
    let encoded = packets::chunk(&chunk, &data).unwrap();
    let mut body = encoded.as_slice();
    assert_eq!(get_varint(&mut body).unwrap(), 46);
    take(&mut body, 8).unwrap();
    for _ in 0..get_varint(&mut body).unwrap() {
        get_varint(&mut body).unwrap();
        let words = get_varint(&mut body).unwrap() as usize;
        take(&mut body, words * 8).unwrap();
    }
    let length = get_varint(&mut body).unwrap() as usize;
    let mut sections = take(&mut body, length).unwrap();
    assert_eq!(
        u16::from_be_bytes(take(&mut sections, 2).unwrap().try_into().unwrap()),
        4096
    );
    assert_eq!(
        u16::from_be_bytes(take(&mut sections, 2).unwrap().try_into().unwrap()),
        2
    );
}

#[test]
fn unknown_saved_block_states_are_rejected_before_transmission() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let data = GameData::load(dir.path()).unwrap();
    let mut chunk = zerqon_world::Chunk::flat(0, 0, data.flat);
    chunk.set_block(0, 64, 0, 262).unwrap();
    assert!(packets::chunk(&chunk, &data).is_err());
    chunk.set_block(0, 64, 0, 100).unwrap();
    assert!(packets::chunk(&chunk, &data).is_err());
}

#[test]
fn position_packet_has_26_3_velocity_and_relative_fields() {
    let out = packets::position(5, 1.0, 64.0, -2.0);
    assert_eq!(out.len(), 62);
    assert_eq!(&out[..2], &[73, 5]);
    assert_eq!(&out[2..10], &1.0_f64.to_be_bytes());
    assert_eq!(&out[10..18], &64.0_f64.to_be_bytes());
    assert_eq!(&out[18..26], &(-2.0_f64).to_be_bytes());
    assert_eq!(&out[26..], &[0; 36]);
}

#[test]
fn chat_uses_network_nbt_with_java_modified_utf8() {
    assert_eq!(
        packets::system_chat("A\0😀"),
        [124, 8, 0, 9, 65, 192, 128, 237, 160, 189, 237, 184, 128, 0]
    );
}

#[test]
fn flat_chunk_has_24_sections_and_correct_packed_blocks() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let data = GameData::load(dir.path()).unwrap();
    let chunk = zerqon_world::Chunk::flat(-3, 5, data.flat);
    let encoded = packets::chunk(&chunk, &data).unwrap();
    let mut body = encoded.as_slice();
    assert_eq!(get_varint(&mut body).unwrap(), 46);
    assert_eq!(
        i32::from_be_bytes(take(&mut body, 4).unwrap().try_into().unwrap()),
        -3
    );
    assert_eq!(
        i32::from_be_bytes(take(&mut body, 4).unwrap().try_into().unwrap()),
        5
    );
    let heightmaps = get_varint(&mut body).unwrap();
    for _ in 0..heightmaps {
        get_varint(&mut body).unwrap();
        let words = get_varint(&mut body).unwrap() as usize;
        take(&mut body, words * 8).unwrap();
    }
    let size = get_varint(&mut body).unwrap() as usize;
    let mut sections = take(&mut body, size).unwrap();
    for section in 0..24 {
        let non_air = u16::from_be_bytes(take(&mut sections, 2).unwrap().try_into().unwrap());
        assert_eq!(non_air, if section < 8 { 4096 } else { 0 });
        assert_eq!(take(&mut sections, 2).unwrap(), &[0, 0]);
        let bits = take(&mut sections, 1).unwrap()[0];
        if bits == 0 {
            let state = get_varint(&mut sections).unwrap();
            assert_eq!(state, if section < 8 { 257 } else { 0 });
        } else {
            assert_eq!(bits, 4);
            let length = get_varint(&mut sections).unwrap();
            let palette = (0..length)
                .map(|_| get_varint(&mut sections).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                palette,
                if section == 0 {
                    vec![261, 257]
                } else {
                    vec![257, 258, 259]
                }
            );
            let packed = take(&mut sections, 2048).unwrap();
            assert_eq!(u64::from_be_bytes(packed[0..8].try_into().unwrap()), 0);
            let last = u64::from_be_bytes(packed[2040..].try_into().unwrap());
            assert_eq!(
                last,
                if section == 0 {
                    0x1111111111111111
                } else {
                    0x2222222222222222
                }
            );
        }
        assert_eq!(take(&mut sections, 1).unwrap(), &[0]);
        assert_eq!(get_varint(&mut sections).unwrap(), 0);
    }
    assert!(sections.is_empty());
    assert_eq!(get_varint(&mut body).unwrap(), 0);
    for expected in [
        &[0, 254, 255, 3][..],
        &[][..],
        &[255, 1][..],
        &[255, 255, 255, 3][..],
    ] {
        let bytes = get_varint(&mut body).unwrap() as usize;
        assert_eq!(take(&mut body, bytes).unwrap(), expected);
    }
    let sky_arrays = get_varint(&mut body).unwrap();
    for _ in 0..sky_arrays {
        assert_eq!(get_varint(&mut body).unwrap(), 2048);
        take(&mut body, 2048).unwrap();
    }
    assert_eq!(get_varint(&mut body).unwrap(), 0);
    assert!(body.is_empty());
}

#[test]
fn bossbar_update_uses_separate_title_and_health_operations() {
    let id = uuid::Uuid::nil();
    let updates = packets::bossbar_update(id, "TPS", 0.5);
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0][17], 3);
    assert_eq!(updates[1][17], 2);
    assert_eq!(&updates[1][18..], &0.5_f32.to_be_bytes());
}

#[test]
fn command_tree_has_executable_literals_and_greedy_arguments() {
    let out = packets::command_tree(&["tps", "spark"]);
    let mut body = out.as_slice();
    assert_eq!(get_varint(&mut body).unwrap(), 16);
    assert_eq!(get_varint(&mut body).unwrap(), 5);
    assert_eq!(take(&mut body, 1).unwrap(), &[0]);
    assert_eq!(get_varint(&mut body).unwrap(), 2);
    assert_eq!(get_varint(&mut body).unwrap(), 1);
    assert_eq!(get_varint(&mut body).unwrap(), 3);
    for name in ["tps", "spark"] {
        assert_eq!(take(&mut body, 1).unwrap(), &[5]);
        assert_eq!(get_varint(&mut body).unwrap(), 1);
        get_varint(&mut body).unwrap();
        assert_eq!(get_string(&mut body, 64).unwrap(), name);
        assert_eq!(take(&mut body, 1).unwrap(), &[6]);
        assert_eq!(get_varint(&mut body).unwrap(), 0);
        assert_eq!(get_string(&mut body, 64).unwrap(), "args");
        assert_eq!(get_varint(&mut body).unwrap(), 5);
        assert_eq!(get_varint(&mut body).unwrap(), 2);
    }
    assert_eq!(body, &[0]);
}

#[test]
#[ignore = "requires locally prepared official Minecraft 26.3 data"]
fn export_vanilla_decoder_fixtures() {
    let data_path = std::env::var_os("ZERQON_TEST_DATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| ".cache/vanilla/26.3".into());
    let data = GameData::load(&data_path).unwrap();
    let output = Path::new(".cache/protocol-fixtures");
    fs::create_dir_all(output).unwrap();
    let resolver = game_data::BlockResolver::load(&data_path).unwrap();
    let mut resolved = Vec::new();
    for (name, properties) in [
        ("minecraft:stone", std::collections::BTreeMap::new()),
        ("minecraft:grass_block", std::collections::BTreeMap::new()),
        (
            "minecraft:grass_block",
            std::collections::BTreeMap::from([("snowy".into(), "true".into())]),
        ),
        (
            "minecraft:oak_stairs",
            std::collections::BTreeMap::from([("facing".into(), "east".into())]),
        ),
        (
            "minecraft:oak_log",
            std::collections::BTreeMap::from([("axis".into(), "z".into())]),
        ),
    ] {
        resolved.push(json!({"name":name,"properties":properties,"state":resolver.resolve(name,&properties).unwrap()}));
    }
    fs::write(
        output.join("block-resolver.json"),
        serde_json::to_vec(&resolved).unwrap(),
    )
    .unwrap();
    let id = uuid::Uuid::nil();
    let chunk = zerqon_world::Chunk::flat(-3, 5, data.flat);
    let mut fixtures = vec![
        ("known_packs".to_owned(), packets::known_packs()),
        (
            "brand_default".into(),
            packets::configuration_brand("zerqon"),
        ),
        (
            "brand_unicode".into(),
            packets::configuration_brand("teighto Привет 😀"),
        ),
        (
            "login".into(),
            packets::login(
                7,
                20,
                4,
                data.overworld_dimension,
                GameMode::Creative,
                None,
                false,
            ),
        ),
        (
            "login_online".into(),
            packets::login(
                7,
                20,
                4,
                data.overworld_dimension,
                GameMode::Creative,
                None,
                true,
            ),
        ),
        (
            "login_survival".into(),
            packets::login(
                7,
                20,
                4,
                data.overworld_dimension,
                GameMode::Survival,
                None,
                false,
            ),
        ),
        (
            "login_survival_previous_creative".into(),
            packets::login(
                7,
                20,
                4,
                data.overworld_dimension,
                GameMode::Survival,
                Some(GameMode::Creative),
                false,
            ),
        ),
        (
            "login_creative_previous_survival".into(),
            packets::login(
                7,
                20,
                4,
                data.overworld_dimension,
                GameMode::Creative,
                Some(GameMode::Survival),
                false,
            ),
        ),
        ("position".into(), packets::position(5, 1.0, 64.0, -2.0)),
        (
            "abilities".into(),
            packets::abilities(GameMode::Creative, false),
        ),
        (
            "abilities_creative_flying".into(),
            packets::abilities(GameMode::Creative, true),
        ),
        (
            "abilities_survival".into(),
            packets::abilities(GameMode::Survival, false),
        ),
        (
            "abilities_survival_requested_flight".into(),
            packets::abilities(GameMode::Survival, true),
        ),
        ("chat".into(), packets::system_chat("Zerqon Привет 😀")),
        ("boss_add".into(), packets::bossbar_add(id, "TPS 20", 1.0)),
        ("boss_remove".into(), packets::bossbar_remove(id)),
        ("chunk".into(), packets::chunk(&chunk, &data).unwrap()),
        ("center".into(), packets::chunk_center(-3, 5)),
        ("batch_start".into(), packets::chunk_batch_start()),
        ("batch_finish".into(), packets::chunk_batch_finished(1)),
        ("spawn".into(), packets::default_spawn(0, 64, 0)),
        ("event".into(), packets::game_event(13, 0.0)),
        (
            "commands".into(),
            packets::command_tree(&["tps", "tpsbar", "spark"]),
        ),
        (
            "player_info_add".into(),
            entity_packets::player_info_add(id, "TestPlayer", true, GameMode::Creative),
        ),
        (
            "player_info_survival".into(),
            entity_packets::player_info_add(id, "TestPlayer", true, GameMode::Survival),
        ),
        (
            "player_info_properties".into(),
            entity_packets::player_info_add_properties(
                id,
                "TestPlayer",
                true,
                &[("textures", "texture-value", Some("signed-value"))],
                GameMode::Creative,
            ),
        ),
        (
            "player_info_survival_properties".into(),
            entity_packets::player_info_add_properties(
                id,
                "TestPlayer",
                true,
                &[("textures", "texture-value", Some("signed-value"))],
                GameMode::Survival,
            ),
        ),
        (
            "player_info_remove".into(),
            entity_packets::player_info_remove(&[id]),
        ),
        (
            "entity_player".into(),
            entity_packets::add_entity(
                300,
                id,
                data.entity_type("minecraft:player").unwrap(),
                [1.0, 64.0, -2.0],
                90.0,
                -45.0,
            ),
        ),
        (
            "entity_pig".into(),
            entity_packets::add_entity(
                301,
                id,
                data.entity_type("minecraft:pig").unwrap(),
                [1.0, 64.0, -2.0],
                90.0,
                -45.0,
            ),
        ),
        (
            "entity_position".into(),
            entity_packets::position_sync(300, [2.0, 64.0, -3.0], 45.0, -30.0, true),
        ),
        (
            "entity_head".into(),
            entity_packets::head_rotation(300, 90.0),
        ),
        (
            "entity_remove".into(),
            entity_packets::remove_entities(&[300, 301]),
        ),
        (
            "entity_metadata".into(),
            entity_packets::player_metadata(300, 127),
        ),
        (
            "player_health".into(),
            entity_packets::health(20.0, 20, 5.0),
        ),
        ("mob_health".into(), entity_packets::mob_health(300, 6.0)),
        ("attack_request".into(), vec![1, 172, 2]),
    ];
    for (index, packet) in packets::bossbar_update(id, "TPS 10", 0.5)
        .into_iter()
        .enumerate()
    {
        fixtures.push((format!("boss_update_{index}"), packet));
    }
    for (index, packet) in data.configuration_packets().iter().enumerate() {
        fixtures.push((format!("configuration_{index:02}"), packet.clone()));
    }
    let mut manifest = Vec::new();
    for (name, packet) in fixtures {
        fs::write(output.join(format!("{name}.bin")), packet).unwrap();
        manifest.push(name);
    }
    fs::write(
        output.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}
