use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use zerqon_profiler::{ProfileConfig, ProfileSession};

#[inline(never)]
fn measured_cpu_work(mut value: u64) -> u64 {
    for _ in 0..20000 {
        value = std::hint::black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
    }
    value
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let path = arguments
        .next()
        .ok_or("usage: capture OUTPUT.sparkprofile [MAX_SAMPLES]")?;
    let max_samples = arguments
        .next()
        .map(|arg| {
            arg.to_str()
                .ok_or("invalid sample bound")
                .and_then(|arg| arg.parse::<usize>().map_err(|_| "invalid sample bound"))
        })
        .transpose()?
        .unwrap_or(20000);
    let running = Arc::new(AtomicBool::new(true));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let mut workers = Vec::new();
    for index in 0..2 {
        let running = Arc::clone(&running);
        let ready = ready_tx.clone();
        workers.push(
            thread::Builder::new()
                .name(format!("zerqon-cpu-{index}"))
                .spawn(move || {
                    ready.send(()).unwrap();
                    let mut value = 7;
                    while running.load(Ordering::Relaxed) {
                        value = measured_cpu_work(value);
                    }
                    std::hint::black_box(value)
                })?,
        );
    }
    ready_rx.recv()?;
    ready_rx.recv()?;
    let session = ProfileSession::start(ProfileConfig {
        duration: Duration::from_secs(2),
        max_samples,
        ..ProfileConfig::default()
    });
    let result = match session {
        Ok(session) => {
            while !session.is_finished() {
                thread::sleep(Duration::from_millis(10));
            }
            session.finish()
        }
        Err(error) => Err(error),
    };
    running.store(false, Ordering::Relaxed);
    for worker in workers {
        worker.join().map_err(|_| "workload thread failed")?;
    }
    let profile = result?;
    profile.write_spark(std::path::Path::new(&path))?;
    println!("{}", serde_json::to_string_pretty(profile.summary())?);
    Ok(())
}
