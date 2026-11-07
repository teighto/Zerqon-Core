use proptest::prelude::*;
use zerqon_protocol::{TeleportAck, decode_teleport_ack, encode_teleport_ack};

fn fixtures() -> [(TeleportAck, &'static str); 3] {
    [
        (
            TeleportAck {
                id: 1,
                xyz: [0.5, 64.0, 0.5],
                rotation: [0.0, 0.0],
            },
            "013fe000000000000040500000000000003fe00000000000000000000000000000",
        ),
        (
            TeleportAck {
                id: 129,
                xyz: [-17.25, -60.0, 33.5],
                rotation: [180.0, -45.5],
            },
            "8101c031400000000000c04e0000000000004040c0000000000043340000c2360000",
        ),
        (
            TeleportAck {
                id: i32::MAX,
                xyz: [30_000_000.0, -20_000_000.0, -30_000_000.0],
                rotation: [720.25, 89.5],
            },
            "ffffffff07417c9c3800000000c17312d000000000c17c9c38000000004434100042b30000",
        ),
    ]
}

fn hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn original_26_3_codec_fixtures_include_all_position_and_rotation_fields() {
    for (expected, body) in fixtures() {
        assert_eq!(decode_teleport_ack(&hex(body)).unwrap(), expected);
        assert_eq!(encode_teleport_ack(&expected).unwrap(), hex(body));
    }
}

#[test]
fn legacy_id_only_truncated_and_trailing_acks_are_rejected() {
    for (_, body) in fixtures() {
        let body = hex(body);
        for end in 0..body.len() {
            assert!(
                decode_teleport_ack(&body[..end]).is_err(),
                "truncation {end}"
            );
        }
        let mut trailing = body;
        trailing.push(0);
        assert!(decode_teleport_ack(&trailing).is_err());
    }
    for short in [&[1][..], &[0x81, 1][..], &[0xff, 0xff, 0xff, 0xff, 7][..]] {
        assert!(decode_teleport_ack(short).is_err());
    }
}

#[test]
fn teleport_values_reject_invalid_ids_nonfinite_numbers_and_out_of_bounds_positions() {
    let original = hex(fixtures()[0].1);
    for id in [0, -1, i32::MIN] {
        let mut invalid = Vec::new();
        zerqon_protocol::codec::put_varint(id, &mut invalid);
        invalid.extend_from_slice(&original[1..]);
        assert!(decode_teleport_ack(&invalid).is_err());
        let mut ack = fixtures()[0].0;
        ack.id = id;
        assert!(encode_teleport_ack(&ack).is_err());
    }
    for id in [&[0x80; 6][..], &[0xff, 0xff, 0xff, 0xff, 0x10][..]] {
        let mut invalid = id.to_vec();
        invalid.extend_from_slice(&original[1..]);
        assert!(decode_teleport_ack(&invalid).is_err());
    }
    for axis in 0..3 {
        let bound = if axis == 1 {
            20_000_000.0_f64
        } else {
            30_000_000.0
        };
        for value in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            bound + 0.25,
            -bound - 0.25,
        ] {
            let mut invalid = original.clone();
            invalid[1 + axis * 8..9 + axis * 8].copy_from_slice(&value.to_be_bytes());
            assert!(
                decode_teleport_ack(&invalid).is_err(),
                "axis {axis}, value {value}"
            );
            let mut ack = fixtures()[0].0;
            ack.xyz[axis] = value;
            assert!(encode_teleport_ack(&ack).is_err());
        }
    }
    for axis in 0..2 {
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut invalid = original.clone();
            invalid[25 + axis * 4..29 + axis * 4].copy_from_slice(&value.to_be_bytes());
            assert!(decode_teleport_ack(&invalid).is_err());
            let mut ack = fixtures()[0].0;
            ack.rotation[axis] = value;
            assert!(encode_teleport_ack(&ack).is_err());
        }
    }
}

#[test]
fn finite_rotations_and_signed_zero_are_preserved_without_clamping() {
    let ack = TeleportAck {
        id: 7,
        xyz: [-0.0, 0.0, -0.0],
        rotation: [f32::MAX, -f32::MAX],
    };
    let decoded = decode_teleport_ack(&encode_teleport_ack(&ack).unwrap()).unwrap();
    assert_eq!(decoded.xyz.map(f64::to_bits), ack.xyz.map(f64::to_bits));
    assert_eq!(
        decoded.rotation.map(f32::to_bits),
        ack.rotation.map(f32::to_bits)
    );
}

proptest! {
    #[test]
    fn teleport_fields_roundtrip_with_full_precision(
        id in 1_i32..=i32::MAX,
        x in -30_000_000_f64..=30_000_000_f64,
        y in -20_000_000_f64..=20_000_000_f64,
        z in -30_000_000_f64..=30_000_000_f64,
        yaw in -1_000_000_f32..=1_000_000_f32,
        pitch in -1_000_000_f32..=1_000_000_f32,
    ) {
        let ack = TeleportAck { id, xyz: [x, y, z], rotation: [yaw, pitch] };
        let bytes = encode_teleport_ack(&ack).unwrap();
        prop_assert!((33..=37).contains(&bytes.len()));
        let decoded = decode_teleport_ack(&bytes).unwrap();
        prop_assert_eq!(decoded.id, ack.id);
        prop_assert_eq!(decoded.xyz.map(f64::to_bits), ack.xyz.map(f64::to_bits));
        prop_assert_eq!(decoded.rotation.map(f32::to_bits), ack.rotation.map(f32::to_bits));
    }
}
