use std::{hint::black_box, time::Instant};
use zerqon_protocol::{
    codec,
    inbound::{PacketBudget, read_frame_budgeted},
};

#[test]
#[ignore = "manual optimized-profile packet budget cost measurement"]
fn measure_small_packet_budget_cost() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        for (size, compression, iterations) in [(9, None, 100_000), (9, Some(256), 100_000), (34, Some(256), 100_000), (128, Some(0), 20_000)] {
            let payload = vec![7; size];
            let mut wire = Vec::new();
            codec::write_frame(&mut wire, &payload, compression).await.unwrap();
            let budget = PacketBudget::new(64 * 1024 * 1024).unwrap();
            let connection = budget.connection(2 * 1024 * 1024).unwrap();
            for _ in 0..1000 {
                black_box(codec::read_frame(&mut wire.as_slice(), 65_536, compression).await.unwrap());
                black_box(read_frame_budgeted(&mut wire.as_slice(), 65_536, compression, &connection).await.unwrap());
            }
            let mut legacy_samples = Vec::new();
            let mut budget_samples = Vec::new();
            for round in 0..7 {
                for budgeted in if round % 2 == 0 { [false, true] } else { [true, false] } {
                    let started = Instant::now();
                    for _ in 0..iterations {
                        let mut input = black_box(wire.as_slice());
                        if budgeted {
                            let packet = read_frame_budgeted(&mut input, 65_536, compression, &connection).await.unwrap();
                            black_box(packet.as_slice());
                        } else {
                            let packet = codec::read_frame(&mut input, 65_536, compression).await.unwrap();
                            black_box(packet.as_slice());
                        }
                    }
                    let ns = started.elapsed().as_secs_f64() * 1_000_000_000.0 / f64::from(iterations);
                    if budgeted { budget_samples.push(ns); } else { legacy_samples.push(ns); }
                    assert_eq!(budget.snapshot().used_bytes, 0);
                }
            }
            legacy_samples.sort_by(f64::total_cmp);
            budget_samples.sort_by(f64::total_cmp);
            println!("payload_bytes={size} compression={compression:?} iterations={iterations} samples=7 legacy_median_ns={:.3} budget_median_ns={:.3} overhead_ns={:.3} peak_reserved_bytes={}", legacy_samples[3], budget_samples[3], budget_samples[3]-legacy_samples[3], budget.snapshot().peak_bytes);
        }
    });
}
