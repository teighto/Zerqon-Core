use proptest::prelude::*;
use zerqon_inventory::*;
use zerqon_protocol::codec::put_varint;

fn hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn original_26_3_action_fixtures_include_the_inserted_direction_change() {
    for (body, action) in [
        ("00fffffbc000021fc4058101", PlayerAction::StartDestroyBlock),
        (
            "01fffffbc000021fc4058101",
            PlayerAction::ChangeDestroyDirection,
        ),
        ("02fffffbc000021fc4058101", PlayerAction::AbortDestroyBlock),
        ("03fffffbc000021fc4058101", PlayerAction::StopDestroyBlock),
        ("04fffffbc000021fc4058101", PlayerAction::DropAllItems),
        ("05fffffbc000021fc4058101", PlayerAction::DropItem),
        ("06fffffbc000021fc4058101", PlayerAction::ReleaseUseItem),
        (
            "07fffffbc000021fc4058101",
            PlayerAction::SwapItemWithOffhand,
        ),
        ("08fffffbc000021fc4058101", PlayerAction::Stab),
    ] {
        assert_eq!(
            decode_player_action(&hex(body)).unwrap(),
            PlayerActionRequest {
                action,
                block: BlockPosition {
                    x: -17,
                    y: -60,
                    z: 33
                },
                face: Face::East,
                sequence: 129,
            }
        );
    }
}

#[test]
fn original_use_on_fixtures_preserve_hands_faces_and_both_booleans() {
    for (body, hand, face, inside, border) in [
        (
            "00000000bfffffd040003e8000003f0000003f400000000011",
            Hand::Main,
            Face::Down,
            false,
            false,
        ),
        (
            "00000000bfffffd040013e8000003f0000003f400000010011",
            Hand::Main,
            Face::Up,
            true,
            false,
        ),
        (
            "00000000bfffffd040023e8000003f0000003f400000000111",
            Hand::Main,
            Face::North,
            false,
            true,
        ),
        (
            "01000000bfffffd040033e8000003f0000003f400000010111",
            Hand::Off,
            Face::South,
            true,
            true,
        ),
        (
            "01000000bfffffd040043e8000003f0000003f400000000011",
            Hand::Off,
            Face::West,
            false,
            false,
        ),
        (
            "01000000bfffffd040053e8000003f0000003f400000010111",
            Hand::Off,
            Face::East,
            true,
            true,
        ),
    ] {
        assert_eq!(
            decode_use_on(&hex(body)).unwrap(),
            UseOnRequest {
                hand,
                block: BlockPosition { x: 2, y: 64, z: -3 },
                face,
                hit: [0.25, 0.5, 0.75],
                inside,
                world_border_hit: border,
                sequence: 17,
            }
        );
    }
}

#[test]
fn player_action_rejects_invalid_ordinals_directions_sequences_and_body_lengths() {
    let valid = hex("00fffffbc000021fc4058101");
    for end in 0..valid.len() {
        assert!(
            decode_player_action(&valid[..end]).is_err(),
            "truncation {end}"
        );
    }
    for action in [9, 127, i32::MAX, -1] {
        let mut bytes = Vec::new();
        put_varint(action, &mut bytes);
        bytes.extend_from_slice(&valid[1..]);
        assert!(decode_player_action(&bytes).is_err(), "action {action}");
    }
    for direction in [6, 127, 128, 255] {
        let mut bytes = valid.clone();
        bytes[9] = direction;
        assert!(
            decode_player_action(&bytes).is_err(),
            "direction {direction}"
        );
    }
    let mut negative = valid[..10].to_vec();
    put_varint(-1, &mut negative);
    assert!(decode_player_action(&negative).is_err());
    for field in [0, 10] {
        let mut bytes = valid[..field].to_vec();
        bytes.extend([0x80; 6]);
        bytes.extend_from_slice(&valid[field..]);
        assert!(
            decode_player_action(&bytes).is_err(),
            "overlong field {field}"
        );
    }
    let mut trailing = valid;
    trailing.push(0);
    assert!(decode_player_action(&trailing).is_err());
}

#[test]
fn player_action_keeps_signed_position_extremes_and_maximum_sequence() {
    for (packed, block) in [
        (
            "ffffffffffffffff",
            BlockPosition {
                x: -1,
                y: -1,
                z: -1,
            },
        ),
        (
            "8000000000000000",
            BlockPosition {
                x: -33_554_432,
                y: 0,
                z: 0,
            },
        ),
        (
            "7fffffc000000000",
            BlockPosition {
                x: 33_554_431,
                y: 0,
                z: 0,
            },
        ),
        (
            "0000002000000000",
            BlockPosition {
                x: 0,
                y: 0,
                z: -33_554_432,
            },
        ),
        (
            "0000001ffffff000",
            BlockPosition {
                x: 0,
                y: 0,
                z: 33_554_431,
            },
        ),
        (
            "0000000000000800",
            BlockPosition {
                x: 0,
                y: -2048,
                z: 0,
            },
        ),
        (
            "00000000000007ff",
            BlockPosition {
                x: 0,
                y: 2047,
                z: 0,
            },
        ),
    ] {
        let decoded = decode_player_action(&hex(&format!("00{packed}00ffffffff07"))).unwrap();
        assert_eq!(decoded.block, block);
        assert_eq!(decoded.face, Face::Down);
        assert_eq!(decoded.sequence, i32::MAX as u32);
    }
}

#[test]
fn use_on_hit_bounds_follow_vanilla_tolerance_instead_of_unit_cube() {
    let valid = hex("00000000bfffffd040013e8000003f0000003f400000000011");
    for axis in 0..3 {
        for value in [-0.5_f32, 0.0, 1.0, 1.5, f32::from_bits(0xbf00_0001)] {
            let mut bytes = valid.clone();
            bytes[10 + axis * 4..14 + axis * 4].copy_from_slice(&value.to_be_bytes());
            assert_eq!(decode_use_on(&bytes).unwrap().hit[axis], value);
        }
        for value in [
            f32::from_bits(0xbf00_0002),
            f32::from_bits(0x3fc0_0001),
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            let mut bytes = valid.clone();
            bytes[10 + axis * 4..14 + axis * 4].copy_from_slice(&value.to_be_bytes());
            assert!(decode_use_on(&bytes).is_err(), "axis {axis}, value {value}");
        }
    }
}

#[test]
fn use_on_rejects_malformed_metadata_and_trailing_or_truncated_bodies() {
    let valid = hex("00000000bfffffd040013e8000003f0000003f400000000011");
    for end in 0..valid.len() {
        assert!(decode_use_on(&valid[..end]).is_err(), "truncation {end}");
    }
    for (offset, value) in [(0, 2), (9, 6), (22, 2), (23, 255)] {
        let mut bytes = valid.clone();
        bytes[offset] = value;
        assert!(decode_use_on(&bytes).is_err(), "offset {offset}");
    }
    let mut negative = valid[..24].to_vec();
    put_varint(-1, &mut negative);
    assert!(decode_use_on(&negative).is_err());
    let mut trailing = valid;
    trailing.push(0);
    assert!(decode_use_on(&trailing).is_err());
}

proptest! {
    #[test]
    fn packed_positions_preserve_all_signed_coordinate_bits(
        x in -33_554_432_i32..=33_554_431,
        y in -2048_i32..=2047,
        z in -33_554_432_i32..=33_554_431,
        sequence in 0_i32..=i32::MAX,
        direction in 0_u8..6,
    ) {
        let packed = (((i64::from(x) & 0x3ff_ffff) << 38)
            | ((i64::from(z) & 0x3ff_ffff) << 12)
            | (i64::from(y) & 0xfff)).to_be_bytes();
        let mut action = vec![1];
        action.extend(packed);
        action.push(direction);
        put_varint(sequence, &mut action);
        let decoded = decode_player_action(&action).unwrap();
        prop_assert_eq!(decoded.block, BlockPosition { x, y, z });
        prop_assert_eq!(decoded.action, PlayerAction::ChangeDestroyDirection);
        prop_assert_eq!(decoded.face, [Face::Down, Face::Up, Face::North, Face::South, Face::West, Face::East][usize::from(direction)]);
        prop_assert_eq!(decoded.sequence, sequence as u32);
        let mut placement = vec![0];
        placement.extend(packed);
        placement.push(direction);
        for _ in 0..3 { placement.extend(0.5_f32.to_be_bytes()); }
        placement.extend([0, 1]);
        put_varint(sequence, &mut placement);
        prop_assert_eq!(decode_use_on(&placement).unwrap().block, decoded.block);
    }
}
