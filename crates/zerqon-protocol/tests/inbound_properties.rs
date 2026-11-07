use proptest::prelude::*;
use zerqon_protocol::{
    codec,
    inbound::{PacketBudget, read_frame_budgeted},
};

fn payloads() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 1..1025),
        (any::<u8>(), 1_usize..4097).prop_map(|(byte, size)| vec![byte; size]),
    ]
}

fn compression() -> impl Strategy<Value = Option<usize>> {
    prop_oneof![
        Just(None),
        Just(Some(0)),
        Just(Some(1)),
        Just(Some(256)),
        Just(Some(8192))
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn incomplete_compression_varints_never_read_into_the_following_frame(header in prop::collection::vec(0_u8..128, 1..6)) {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        runtime.block_on(async {
            let global = PacketBudget::new(64).unwrap();
            let account = global.connection(64).unwrap();
            let mut wire = vec![header.len() as u8];
            wire.extend(header.into_iter().map(|byte| byte | 128));
            wire.extend_from_slice(&[1, 3]);
            let mut input = wire.as_slice();
            prop_assert!(read_frame_budgeted(&mut input, 64, Some(0), &account).await.is_err());
            prop_assert_eq!(input, &[1, 3]);
            prop_assert_eq!(global.snapshot().used_bytes, 0);
            prop_assert_eq!(global.snapshot().peak_bytes, 0);
            Ok(())
        })?;
    }

    #[test]
    fn framed_payload_matches_original_bytes_and_legacy_decoder(payload in payloads(), threshold in compression()) {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        runtime.block_on(async {
            let global = PacketBudget::new(16384).unwrap();
            let account = global.connection(16384).unwrap();
            let mut wire = Vec::new();
            codec::write_frame(&mut wire, &payload, threshold).await.unwrap();
            let legacy = codec::read_frame(&mut wire.as_slice(), 8192, threshold).await.unwrap();
            let mut input = wire.as_slice();
            let packet = read_frame_budgeted(&mut input, 8192, threshold, &account).await.unwrap();
            prop_assert!(input.is_empty());
            prop_assert_eq!(packet.as_slice(), payload.as_slice());
            prop_assert_eq!(packet.as_slice(), legacy.as_slice());
            prop_assert_eq!(global.snapshot().used_bytes, payload.len());
            prop_assert_eq!(account.snapshot().used_bytes, payload.len());
            prop_assert!(global.snapshot().peak_bytes >= payload.len());
            prop_assert!(global.snapshot().peak_bytes <= 16384);
            drop(packet);
            prop_assert_eq!(global.snapshot().used_bytes, 0);
            prop_assert_eq!(account.snapshot().used_bytes, 0);
            Ok(())
        })?;
    }

    #[test]
    fn every_truncated_frame_returns_all_charges(payload in payloads(), threshold in compression(), selector in any::<usize>()) {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        runtime.block_on(async {
            let global = PacketBudget::new(16384).unwrap();
            let account = global.connection(16384).unwrap();
            let mut wire = Vec::new();
            codec::write_frame(&mut wire, &payload, threshold).await.unwrap();
            let cutoff = selector % wire.len();
            prop_assert!(read_frame_budgeted(&mut &wire[..cutoff], 8192, threshold, &account).await.is_err());
            prop_assert_eq!(global.snapshot().used_bytes, 0);
            prop_assert_eq!(account.snapshot().used_bytes, 0);
            prop_assert!(global.snapshot().peak_bytes <= 16384);
            Ok(())
        })?;
    }

    #[test]
    fn lifetime_model_conserves_global_and_connection_bytes(actions in prop::collection::vec((0_usize..2, any::<bool>(), 1_usize..65), 1..81)) {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        runtime.block_on(async {
            let global = PacketBudget::new(96).unwrap();
            let accounts = [global.connection(64).unwrap(), global.connection(64).unwrap()];
            let mut packets = [Vec::new(), Vec::new()];
            let mut expected = [Vec::<usize>::new(), Vec::<usize>::new()];
            for (account, receive, size) in actions {
                if receive {
                    let local_used: usize = expected[account].iter().sum();
                    let total_used: usize = expected.iter().flatten().sum();
                    let mut wire = vec![size as u8];
                    wire.extend(std::iter::repeat_n(7, size));
                    let result = read_frame_budgeted(&mut wire.as_slice(), 128, None, &accounts[account]).await;
                    prop_assert_eq!(result.is_ok(), local_used + size <= 64 && total_used + size <= 96);
                    if let Ok(packet) = result { packets[account].push(packet); expected[account].push(size); }
                } else if !packets[account].is_empty() {
                    let index = size % packets[account].len();
                    packets[account].swap_remove(index);
                    expected[account].swap_remove(index);
                }
                prop_assert_eq!(global.snapshot().used_bytes, expected.iter().flatten().sum::<usize>());
                prop_assert!(global.snapshot().peak_bytes <= 96);
                for account in 0..2 {
                    prop_assert_eq!(accounts[account].snapshot().used_bytes, expected[account].iter().sum::<usize>());
                    prop_assert!(accounts[account].snapshot().peak_bytes <= 64);
                }
            }
            drop(packets);
            prop_assert_eq!(global.snapshot().used_bytes, 0);
            Ok(())
        })?;
    }
}
