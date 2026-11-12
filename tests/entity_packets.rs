#[path = "../src/entity_packets.rs"]
mod entity_packets;
#[path = "../src/game_mode.rs"]
mod game_mode;

use uuid::Uuid;
use zerqon_inventory::{
    ComponentDef, ComponentId, ComponentValue, ItemCatalog, ItemDef, ItemId, ItemStack,
    ValidatedPatch,
};
use zerqon_protocol::codec::{get_string, get_varint, take};

fn item_catalog() -> ItemCatalog {
    ItemCatalog::from_definitions(
        vec![ItemDef {
            id: ItemId(1),
            key: "minecraft:stone".into(),
            max_stack: 64,
            max_damage: None,
        }],
        vec![ComponentDef {
            id: ComponentId(6),
            key: "minecraft:custom_name".into(),
        }],
    )
    .unwrap()
}

fn original_drop_velocity() -> Vec<u8> {
    let line = include_str!("../tools/entity-wire/26.3.tsv")
        .lines()
        .find(|line| line.starts_with("ordinary_drop\t"))
        .unwrap();
    line.split('\t')
        .nth(4)
        .unwrap()
        .as_bytes()
        .chunks_exact(2)
        .map(|part| u8::from_str_radix(std::str::from_utf8(part).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn living_entity_feedback_matches_original_26_3_wire_fields() {
    assert_eq!(
        entity_packets::hurt_animation(300, 0.0).unwrap(),
        [43, 172, 2, 0, 0, 0, 0]
    );
    assert!(entity_packets::hurt_animation(300, f32::NAN).is_err());
    assert_eq!(entity_packets::entity_event(300, 18), [34, 0, 0, 1, 44, 18]);
    assert_eq!(
        entity_packets::animal_age(300, true),
        [101, 172, 2, 16, 8, 1, 255]
    );
    assert_eq!(
        entity_packets::no_gravity(300, false),
        [101, 172, 2, 5, 8, 0, 255]
    );
}

#[test]
fn player_info_can_initialize_survival_mode() {
    let packet = entity_packets::player_info_add(
        Uuid::from_u128(42),
        "TestPlayer",
        true,
        game_mode::GameMode::Survival,
    );
    let mut body = &packet[19..];
    assert_eq!(get_string(&mut body, 16).unwrap(), "TestPlayer");
    assert_eq!(get_varint(&mut body).unwrap(), 0);
    assert_eq!(get_varint(&mut body).unwrap(), 0);
    assert_eq!(body, &[1, 0, 1]);
}

#[test]
fn player_info_initializes_tab_profile_listing_and_creative_mode() {
    let uuid = Uuid::from_u128(42);
    let packet =
        entity_packets::player_info_add(uuid, "TestPlayer", true, game_mode::GameMode::Creative);
    let mut body = packet.as_slice();
    assert_eq!(get_varint(&mut body).unwrap(), 71);
    assert_eq!(take(&mut body, 1).unwrap(), &[0x9d]);
    assert_eq!(get_varint(&mut body).unwrap(), 1);
    assert_eq!(take(&mut body, 16).unwrap(), uuid.as_bytes());
    assert_eq!(get_string(&mut body, 16).unwrap(), "TestPlayer");
    assert_eq!(body, &[0, 1, 1, 0, 1]);
}

#[test]
fn authenticated_properties_keep_their_values_and_optional_signatures() {
    let packet = entity_packets::player_info_add_properties(
        Uuid::nil(),
        "TestPlayer",
        true,
        &[
            ("textures", "texture-value", Some("signed-value")),
            ("extra", "value", None),
        ],
        game_mode::GameMode::Creative,
    );
    let mut body = &packet[19..];
    assert_eq!(get_string(&mut body, 16).unwrap(), "TestPlayer");
    assert_eq!(get_varint(&mut body).unwrap(), 2);
    for (name, value, signature) in [
        ("textures", "texture-value", Some("signed-value")),
        ("extra", "value", None),
    ] {
        assert_eq!(get_string(&mut body, 64).unwrap(), name);
        assert_eq!(get_string(&mut body, 32767).unwrap(), value);
        assert_eq!(
            take(&mut body, 1).unwrap(),
            &[u8::from(signature.is_some())]
        );
        if let Some(signature) = signature {
            assert_eq!(get_string(&mut body, 1024).unwrap(), signature);
        }
    }
    assert_eq!(body, &[1, 1, 0, 1]);
}

#[test]
fn add_entity_uses_26_3_zero_velocity_before_packed_rotations() {
    let packet = entity_packets::add_entity(300, Uuid::nil(), 159, [1.0, 64.0, -2.0], 90.0, -45.0);
    let mut body = packet.as_slice();
    assert_eq!(get_varint(&mut body).unwrap(), 1);
    assert_eq!(get_varint(&mut body).unwrap(), 300);
    assert_eq!(take(&mut body, 16).unwrap(), &[0; 16]);
    assert_eq!(get_varint(&mut body).unwrap(), 159);
    assert_eq!(take(&mut body, 8).unwrap(), 1.0_f64.to_be_bytes());
    assert_eq!(take(&mut body, 8).unwrap(), 64.0_f64.to_be_bytes());
    assert_eq!(take(&mut body, 8).unwrap(), (-2.0_f64).to_be_bytes());
    assert_eq!(body, &[0, 224, 64, 64, 0]);
}

#[test]
fn position_sync_contains_linear_path_and_no_old_velocity_fields() {
    let packet = entity_packets::position_sync(7, [1.0, 64.0, -2.0], 90.0, -45.0, true);
    assert_eq!(packet.len(), 36);
    assert_eq!(&packet[..3], &[35, 7, 0]);
    assert_eq!(&packet[27..31], &90.0_f32.to_be_bytes());
    assert_eq!(&packet[31..35], &(-45.0_f32).to_be_bytes());
    assert_eq!(packet[35], 1);
}

#[test]
fn metadata_head_rotation_and_health_match_wire_fixtures() {
    assert_eq!(
        entity_packets::player_metadata(7, 127),
        [101, 7, 16, 0, 127, 255]
    );
    assert_eq!(entity_packets::head_rotation(7, -0.1), [85, 7, 255]);
    assert_eq!(
        entity_packets::health(20.0, 20, 5.0),
        [106, 65, 160, 0, 0, 20, 64, 160, 0, 0]
    );
    assert_eq!(
        entity_packets::mob_health(300, 6.0),
        [101, 172, 2, 9, 3, 64, 192, 0, 0, 255]
    );
}

#[test]
fn removal_encodes_all_identifiers() {
    assert_eq!(
        entity_packets::remove_entities(&[1, 300]),
        [78, 2, 1, 172, 2]
    );
    let first = Uuid::from_u128(7);
    let second = Uuid::from_u128(8);
    let packet = entity_packets::player_info_remove(&[first, second]);
    assert_eq!(&packet[..2], &[70, 2]);
    assert_eq!(&packet[2..18], first.as_bytes());
    assert_eq!(&packet[18..], second.as_bytes());
}

#[test]
fn moving_spawn_and_motion_use_the_original_lp_bytes_in_their_distinct_layouts() {
    let uuid = Uuid::from_u128(42);
    let packet = entity_packets::add_entity_with_motion(
        300,
        uuid,
        72,
        [1.0, 64.0, -2.0],
        [0.1, 0.2, -0.05],
        90.0,
        -45.0,
    )
    .unwrap();
    let mut body = packet.as_slice();
    assert_eq!(get_varint(&mut body).unwrap(), 1);
    assert_eq!(get_varint(&mut body).unwrap(), 300);
    assert_eq!(take(&mut body, 16).unwrap(), uuid.as_bytes());
    assert_eq!(get_varint(&mut body).unwrap(), 72);
    for expected in [1.0_f64, 64.0, -2.0] {
        assert_eq!(
            f64::from_be_bytes(take(&mut body, 8).unwrap().try_into().unwrap()),
            expected
        );
    }
    let velocity = original_drop_velocity();
    assert_eq!(take(&mut body, velocity.len()).unwrap(), velocity);
    assert_eq!(body, &[224, 64, 64, 0]);
    let motion = entity_packets::entity_motion(300, [0.1, 0.2, -0.05]).unwrap();
    assert_eq!(&motion[..3], &[103, 172, 2]);
    assert_eq!(&motion[3..], original_drop_velocity());
}

#[test]
fn checked_packets_keep_existing_zero_motion_and_linear_position_wire_bytes() {
    let uuid = Uuid::nil();
    assert_eq!(
        entity_packets::add_entity_with_motion(
            300,
            uuid,
            72,
            [1.0, 64.0, -2.0],
            [0.0; 3],
            90.0,
            -45.0
        )
        .unwrap(),
        entity_packets::add_entity(300, uuid, 72, [1.0, 64.0, -2.0], 90.0, -45.0)
    );
    assert_eq!(
        entity_packets::try_position_sync(300, [1.0, 64.0, -2.0], 90.0, -45.0, true).unwrap(),
        entity_packets::position_sync(300, [1.0, 64.0, -2.0], 90.0, -45.0, true)
    );
    assert_eq!(
        entity_packets::entity_motion(300, [0.0; 3]).unwrap(),
        [103, 172, 2, 0]
    );
}

#[test]
fn item_metadata_uses_server_stack_encoding_and_collect_has_three_varints() {
    let catalog = item_catalog();
    let stone = ItemStack::new(ItemId(1), 7, ValidatedPatch::default()).unwrap();
    assert_eq!(
        entity_packets::item_metadata(300, &Some(stone), &catalog).unwrap(),
        [101, 172, 2, 8, 7, 7, 1, 0, 0, 255]
    );
    assert_eq!(
        entity_packets::item_metadata(300, &None, &catalog).unwrap(),
        [101, 172, 2, 8, 7, 0, 255]
    );
    let patch = ValidatedPatch::new(
        std::collections::BTreeMap::from([(
            ComponentId(6),
            ComponentValue::CustomName("teighto".into()),
        )]),
        std::collections::BTreeSet::new(),
        &catalog,
    )
    .unwrap();
    let named = ItemStack::new(ItemId(1), 3, patch).unwrap();
    assert_eq!(
        entity_packets::item_metadata(300, &Some(named), &catalog).unwrap(),
        [
            101, 172, 2, 8, 7, 3, 1, 1, 0, 6, 8, 0, 7, b't', b'e', b'i', b'g', b'h', b't', b'o',
            255
        ]
    );
    assert_eq!(
        entity_packets::take_item_entity(300, 301, 7).unwrap(),
        [127, 172, 2, 173, 2, 7]
    );
}

#[test]
fn live_packet_constructors_reject_nonfinite_state_and_invalid_item_stacks() {
    for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for axis in 0..3 {
            let mut vector = [0.0; 3];
            vector[axis] = invalid;
            assert!(
                entity_packets::add_entity_with_motion(
                    1,
                    Uuid::nil(),
                    72,
                    vector,
                    [0.0; 3],
                    0.0,
                    0.0
                )
                .is_err()
            );
            assert!(
                entity_packets::add_entity_with_motion(
                    1,
                    Uuid::nil(),
                    72,
                    [0.0; 3],
                    vector,
                    0.0,
                    0.0
                )
                .is_err()
            );
            assert!(entity_packets::try_position_sync(1, vector, 0.0, 0.0, false).is_err());
            assert!(entity_packets::entity_motion(1, vector).is_err());
        }
    }
    for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        for (yaw, pitch) in [(invalid, 0.0), (0.0, invalid)] {
            assert!(
                entity_packets::add_entity_with_motion(
                    1,
                    Uuid::nil(),
                    72,
                    [0.0; 3],
                    [0.0; 3],
                    yaw,
                    pitch
                )
                .is_err()
            );
            assert!(entity_packets::try_position_sync(1, [0.0; 3], yaw, pitch, false).is_err());
        }
    }
    let catalog = item_catalog();
    for stack in [
        ItemStack::new(ItemId(1), 65, ValidatedPatch::default()).unwrap(),
        ItemStack::new(ItemId(999), 1, ValidatedPatch::default()).unwrap(),
    ] {
        assert!(entity_packets::item_metadata(300, &Some(stack), &catalog).is_err());
    }
    for count in [0, -1, i32::MIN] {
        assert!(entity_packets::take_item_entity(300, 301, count).is_err());
    }
    for id in [0, -1, i32::MIN] {
        assert!(
            entity_packets::add_entity_with_motion(
                id,
                Uuid::nil(),
                72,
                [0.0; 3],
                [0.0; 3],
                0.0,
                0.0
            )
            .is_err()
        );
        assert!(entity_packets::entity_motion(id, [0.0; 3]).is_err());
        assert!(entity_packets::item_metadata(id, &None, &catalog).is_err());
        assert!(entity_packets::try_position_sync(id, [0.0; 3], 0.0, 0.0, false).is_err());
        assert!(entity_packets::take_item_entity(id, 301, 1).is_err());
        assert!(entity_packets::take_item_entity(300, id, 1).is_err());
    }
    assert!(
        entity_packets::add_entity_with_motion(1, Uuid::nil(), -1, [0.0; 3], [0.0; 3], 0.0, 0.0)
            .is_err()
    );
}
