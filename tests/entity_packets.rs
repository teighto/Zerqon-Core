#[path = "../src/entity_packets.rs"]
mod entity_packets;
#[path = "../src/game_mode.rs"]
mod game_mode;

use uuid::Uuid;
use zerqon_protocol::codec::{get_string, get_varint, take};

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
