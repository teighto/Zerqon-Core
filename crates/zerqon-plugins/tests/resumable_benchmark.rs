use std::time::{Duration, Instant};
use zerqon_plugin_api::{
    Event,
    native::{NativeCallbackContext, NativeSource, READ_CAPABILITIES},
};
use zerqon_plugins::{
    Limits, PluginError,
    resumable::{CallbackControl, InvocationStep, ResumableHost},
};

fn module(memory_pages: u32, handler: &str) -> Vec<u8> {
    wat::parse_str(format!(
        r#"(module
        (memory (export "memory") {memory_pages})
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (func (export "zerqon_on_event") (result i32) {handler} i32.const 0))"#
    ))
    .unwrap()
}

#[test]
#[ignore = "manual optimized-profile resumable segment latency measurement"]
fn measure_resumable_cpu_segment_latency() {
    let limits = Limits {
        memory_bytes: 1024 * 1024 * 1024,
        fuel_per_event: 100_000_000,
        ..Limits::default()
    };
    for (name, wasm, completes) in [
        ("loop", module(1, "(loop $again br $again)"), false),
        (
            "memory-fill-1gib",
            module(
                16_384,
                "i32.const 0 i32.const 7 i32.const 1073741824 memory.fill",
            ),
            true,
        ),
        (
            "memory-grow-1gib",
            module(1, "i32.const 16383 memory.grow drop"),
            true,
        ),
    ] {
        let mut samples = Vec::new();
        for _ in 0..3 {
            let host = ResumableHost::new(limits.clone(), 50_000).unwrap();
            let load = Instant::now();
            let plugin = host.instantiate(name, &wasm, READ_CAPABILITIES).unwrap();
            let load_ms = load.elapsed().as_secs_f64() * 1000.0;
            let prepared = plugin
                .prepare(
                    &Event::Enable {
                        server_version: "0.0.1".into(),
                    },
                    &NativeCallbackContext {
                        source: NativeSource::Server,
                    },
                )
                .unwrap();
            let mut call = plugin
                .begin(
                    prepared,
                    CallbackControl::new(Instant::now() + Duration::from_secs(30)),
                    Box::new(()),
                )
                .unwrap();
            let mut segments = 0;
            loop {
                let start = Instant::now();
                let result = call.run();
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
                segments += 1;
                match result {
                    Ok(InvocationStep::Quantum(next)) => call = next,
                    Ok(InvocationStep::Finished(done)) => {
                        assert!(completes);
                        let (_, output) = done.commit().unwrap();
                        assert!(output.stats().fuel_consumed <= limits.fuel_per_event);
                        break;
                    }
                    Err(PluginError::FuelExhausted) => {
                        assert!(!completes);
                        break;
                    }
                    _ => panic!("unexpected benchmark outcome"),
                }
            }
            println!("scenario={name} load_ms={load_ms:.3} segments={segments}");
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "scenario={name} samples={} segment_median_ms={:.3} segment_p95_ms={:.3} segment_max_ms={:.3} quantum=50000 total_fuel=100000000 max_linear_bytes=1073741824",
            samples.len(),
            samples[samples.len() / 2],
            samples[(samples.len() * 95 / 100).min(samples.len() - 1)],
            samples[samples.len() - 1]
        );
    }
}
