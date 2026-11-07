use std::sync::Arc;
use zerqon_protocol::{
    codec,
    inbound::{BudgetConfigError, BudgetScope, PacketBudget, ReadFrameError, read_frame_budgeted},
};

const ZLIB_ABC: &[u8] = &[
    0x78, 0x9c, 0x4b, 0x4c, 0x4a, 0x06, 0x00, 0x02, 0x4d, 0x01, 0x27,
];

fn compressed_abc() -> Vec<u8> {
    [vec![12, 3], ZLIB_ABC.to_vec()].concat()
}

#[tokio::test]
async fn packet_keeps_both_reservations_until_the_last_shared_owner_is_dropped() {
    let budget = PacketBudget::new(8).unwrap();
    let connection = budget.connection(8).unwrap();
    let packet = read_frame_budgeted(&mut &[4, 7, 1, 2, 3][..], 64, None, &connection)
        .await
        .unwrap();
    assert_eq!(packet.as_slice(), &[7, 1, 2, 3]);
    assert_eq!(budget.snapshot().used_bytes, 4);
    assert_eq!(connection.snapshot().used_bytes, 4);
    let first = Arc::new(packet);
    let last = Arc::clone(&first);
    drop(first);
    assert_eq!(budget.snapshot().used_bytes, 4);
    drop(last);
    assert_eq!(budget.snapshot().used_bytes, 0);
    assert_eq!(connection.snapshot().used_bytes, 0);
}

#[tokio::test]
async fn quota_rejection_does_not_read_the_advertised_payload() {
    let budget = PacketBudget::new(3).unwrap();
    let connection = budget.connection(3).unwrap();
    let mut input = &[4, 7, 1, 2, 3][..];
    assert!(
        read_frame_budgeted(&mut input, 64, None, &connection)
            .await
            .is_err()
    );
    assert_eq!(input, &[7, 1, 2, 3]);
    assert_eq!(budget.snapshot().used_bytes, 0);
    assert_eq!(connection.snapshot().used_bytes, 0);
}

#[tokio::test]
async fn legacy_client_codec_stays_usable_without_a_budget() {
    let packet = codec::read_frame(&mut &[4, 7, 1, 2, 3][..], 64, None)
        .await
        .unwrap();
    assert_eq!(packet, [7, 1, 2, 3]);
}

#[test]
fn invalid_limits_fail_before_creating_an_account() {
    assert!(matches!(PacketBudget::new(0), Err(BudgetConfigError::Zero)));
    assert!(matches!(
        PacketBudget::new(usize::MAX),
        Err(BudgetConfigError::Overflow)
    ));
    let global = PacketBudget::new(64).unwrap();
    assert!(matches!(global.connection(0), Err(BudgetConfigError::Zero)));
    assert!(matches!(
        global.connection(65),
        Err(BudgetConfigError::ConnectionExceedsGlobal)
    ));
    assert!(matches!(
        global.connection(usize::MAX),
        Err(BudgetConfigError::Overflow)
    ));
    assert_eq!(global.snapshot().used_bytes, 0);
}

#[tokio::test]
async fn compressed_buffers_are_reserved_together_then_only_payload_stays_charged() {
    let global = PacketBudget::new(16).unwrap();
    let connection = global.connection(14).unwrap();
    let packet = read_frame_budgeted(&mut compressed_abc().as_slice(), 64, Some(0), &connection)
        .await
        .unwrap();
    assert_eq!(packet.as_slice(), b"abc");
    assert_eq!(global.snapshot().peak_bytes, 14);
    assert_eq!(connection.snapshot().peak_bytes, 14);
    assert_eq!(global.snapshot().used_bytes, 3);
    assert_eq!(connection.snapshot().used_bytes, 3);
    drop(packet);
    assert_eq!(global.snapshot().used_bytes, 0);
}

#[tokio::test]
async fn failed_global_reservation_does_not_hold_or_inflate_the_local_account() {
    let global = PacketBudget::new(16).unwrap();
    let first = global.connection(16).unwrap();
    let second = global.connection(16).unwrap();
    let held = read_frame_budgeted(&mut &[3, 7, 1, 2][..], 64, None, &first)
        .await
        .unwrap();
    let wire = compressed_abc();
    let mut input = wire.as_slice();
    assert!(matches!(
        read_frame_budgeted(&mut input, 64, Some(0), &second).await,
        Err(ReadFrameError::Budget {
            scope: BudgetScope::Global,
            requested: 14,
            limit: 16
        })
    ));
    assert_eq!(input, ZLIB_ABC);
    assert_eq!(global.snapshot().used_bytes, 3);
    assert_eq!(global.snapshot().peak_bytes, 3);
    assert_eq!(global.snapshot().rejections, 1);
    assert_eq!(second.snapshot().used_bytes, 0);
    assert_eq!(second.snapshot().peak_bytes, 0);
    drop(held);
    let packet = read_frame_budgeted(&mut wire.as_slice(), 64, Some(0), &second)
        .await
        .unwrap();
    assert_eq!(packet.as_slice(), b"abc");
}

#[tokio::test]
async fn cloned_connection_handles_share_the_same_local_limit() {
    let global = PacketBudget::new(32).unwrap();
    let connection = global.connection(4).unwrap();
    let clone = connection.clone();
    let held = read_frame_budgeted(&mut &[3, 7, 1, 2][..], 64, None, &connection)
        .await
        .unwrap();
    assert!(matches!(
        read_frame_budgeted(&mut &[2, 7, 1][..], 64, None, &clone).await,
        Err(ReadFrameError::Budget {
            scope: BudgetScope::Connection,
            requested: 2,
            limit: 4
        })
    ));
    assert_eq!(global.snapshot().used_bytes, 3);
    assert_eq!(clone.snapshot().used_bytes, 3);
    assert_eq!(connection.snapshot().rejections, 1);
    assert_eq!(global.snapshot().rejections, 0);
    drop(held);
    assert_eq!(clone.snapshot().used_bytes, 0);
}

#[tokio::test]
async fn uncompressed_packet_inside_compression_framing_has_one_payload_charge() {
    let global = PacketBudget::new(3).unwrap();
    let connection = global.connection(3).unwrap();
    let packet = read_frame_budgeted(&mut &[4, 0, 7, 1, 2][..], 64, Some(256), &connection)
        .await
        .unwrap();
    assert_eq!(packet.as_slice(), &[7, 1, 2]);
    assert_eq!(global.snapshot().peak_bytes, 3);
    drop(packet);
    assert_eq!(connection.snapshot().used_bytes, 0);
}

#[tokio::test]
async fn malformed_short_compression_header_cannot_consume_the_next_frame() {
    let global = PacketBudget::new(64).unwrap();
    let connection = global.connection(64).unwrap();
    let mut input = &[1, 128, 1, 3][..];
    assert!(matches!(
        read_frame_budgeted(&mut input, 64, Some(0), &connection).await,
        Err(ReadFrameError::Protocol(codec::Error::Invalid(_)))
    ));
    assert_eq!(input, &[1, 3]);
    assert_eq!(global.snapshot().peak_bytes, 0);
}

#[tokio::test]
async fn oversized_invalid_and_overflowed_headers_reject_before_any_body_charge() {
    let global = PacketBudget::new(64).unwrap();
    let connection = global.connection(64).unwrap();
    for (wire, compression) in [
        (vec![0], None),
        (vec![127], None),
        (vec![128, 128, 128, 0], None),
        (vec![5, 255, 255, 255, 255, 127], Some(0)),
        (vec![5, 255, 255, 255, 255, 15], Some(0)),
        (vec![5, 255, 255, 255, 255, 7], Some(0)),
        (vec![1, 0], Some(256)),
        (vec![1, 3], Some(0)),
        (vec![2, 0, 1], Some(0)),
    ] {
        assert!(
            read_frame_budgeted(&mut wire.as_slice(), 64, compression, &connection)
                .await
                .is_err(),
            "{wire:?}"
        );
        assert_eq!(global.snapshot().used_bytes, 0);
        assert_eq!(global.snapshot().peak_bytes, 0);
    }
}

#[tokio::test]
async fn checksum_truncation_extra_output_and_trailing_input_return_all_reservations() {
    let global = PacketBudget::new(128).unwrap();
    let connection = global.connection(128).unwrap();
    let mut wrong_checksum = compressed_abc();
    *wrong_checksum.last_mut().unwrap() ^= 1;
    let mut too_much_output = compressed_abc();
    too_much_output[1] = 2;
    let mut too_little_output = compressed_abc();
    too_little_output[1] = 4;
    let mut trailing = compressed_abc();
    trailing[0] += 1;
    trailing.push(0);
    let mut wires = vec![wrong_checksum, too_much_output, too_little_output, trailing];
    for length in 0..ZLIB_ABC.len() {
        wires.push([vec![(length + 1) as u8, 3], ZLIB_ABC[..length].to_vec()].concat());
    }
    for wire in wires {
        assert!(
            read_frame_budgeted(&mut wire.as_slice(), 64, Some(0), &connection)
                .await
                .is_err(),
            "accepted malformed zlib {wire:?}"
        );
        assert_eq!(global.snapshot().used_bytes, 0);
        assert_eq!(connection.snapshot().used_bytes, 0);
    }
}

#[tokio::test]
async fn two_consecutive_frames_remain_aligned_after_success() {
    let global = PacketBudget::new(32).unwrap();
    let connection = global.connection(32).unwrap();
    let wire = [compressed_abc(), compressed_abc()].concat();
    let mut input = wire.as_slice();
    let first = read_frame_budgeted(&mut input, 64, Some(0), &connection)
        .await
        .unwrap();
    let second = read_frame_budgeted(&mut input, 64, Some(0), &connection)
        .await
        .unwrap();
    assert!(input.is_empty());
    assert_eq!(first.as_slice(), b"abc");
    assert_eq!(second.as_slice(), b"abc");
    assert_eq!(global.snapshot().used_bytes, 6);
}

#[tokio::test]
async fn noncanonical_zero_varint_keeps_existing_compression_semantics() {
    let global = PacketBudget::new(3).unwrap();
    let connection = global.connection(3).unwrap();
    let wire = [5, 128, 0, 7, 1, 2];
    let legacy = codec::read_frame(&mut wire.as_slice(), 64, Some(256))
        .await
        .unwrap();
    let packet = read_frame_budgeted(&mut wire.as_slice(), 64, Some(256), &connection)
        .await
        .unwrap();
    assert_eq!(packet.as_slice(), &[7, 1, 2]);
    assert_eq!(packet.as_slice(), legacy);
    assert_eq!(global.snapshot().peak_bytes, 3);
}

#[tokio::test]
async fn caller_packet_limit_is_enforced_at_and_above_the_server_boundary() {
    let global = PacketBudget::new(65_536).unwrap();
    let connection = global.connection(65_536).unwrap();
    let mut wire = vec![0x80, 0x80, 4];
    wire.extend(std::iter::repeat_n(7, 65_536));
    let packet = read_frame_budgeted(&mut wire.as_slice(), 65_536, None, &connection)
        .await
        .unwrap();
    assert_eq!(packet.len(), 65_536);
    assert_eq!(global.snapshot().used_bytes, 65_536);
    drop(packet);
    assert!(matches!(
        read_frame_budgeted(&mut &[0x81, 0x80, 4][..], 65_536, None, &connection).await,
        Err(ReadFrameError::Protocol(codec::Error::Invalid(_)))
    ));
    assert_eq!(global.snapshot().used_bytes, 0);
}
