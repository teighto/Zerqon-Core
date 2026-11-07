use crate::{
    MAX_STACK_DEPTH, Profile, ProfileConfig, ProfileError, ProfileSession, Sample, export_samples,
};
use perf_event::{Builder, SampleFlag, Sampler, data::Record, events::Software};
use std::{
    collections::BTreeMap,
    io::Read,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const RING_BYTES: usize = 65536;
static ACTIVE: AtomicBool = AtomicBool::new(false);

struct Active;
impl Drop for Active {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::Release);
    }
}

pub(super) fn start(config: ProfileConfig) -> Result<ProfileSession, ProfileError> {
    ACTIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| ProfileError::AlreadyRunning)?;
    let active = Active;
    let mut samplers = Vec::new();
    let mut names = BTreeMap::new();
    let mut tids: Vec<u32> = std::fs::read_dir("/proc/self/task")?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().parse::<u32>().ok()))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    tids.sort_unstable();
    if tids.len() > config.max_threads {
        return Err(ProfileError::Limit(
            "thread count; narrow server worker count before profiling",
        ));
    }
    let period = 1_000_000_000 / u64::from(config.frequency_hz);
    for tid in tids {
        let mut builder = Builder::new(Software::CPU_CLOCK);
        builder
            .observe_pid(tid as i32)
            .sample_period(period)
            .sample(SampleFlag::IP | SampleFlag::TID | SampleFlag::CALLCHAIN | SampleFlag::PERIOD)
            .sample_max_stack(MAX_STACK_DEPTH as u16)
            .enabled(false);
        let counter = match builder.build() {
            Ok(counter) => counter,
            Err(error) if error.raw_os_error() == Some(3) => continue,
            Err(error) => {
                return Err(ProfileError::Backend(format!(
                    "perf_event_open denied or unavailable: {error}; no OS permissions were changed"
                )));
            }
        };
        let sampler = counter.sampled(RING_BYTES)?;
        let mut name = String::new();
        if let Ok(file) = std::fs::File::open(format!("/proc/self/task/{tid}/comm")) {
            let _ = file.take(64).read_to_string(&mut name);
        }
        let name = name
            .trim()
            .chars()
            .map(|c| if c.is_control() { '?' } else { c })
            .collect();
        names.insert(tid, name);
        samplers.push(sampler);
    }
    let (stop, receiver) = mpsc::channel();
    let worker = thread::Builder::new()
        .name("zerqon-profiler".into())
        .spawn(move || {
            let _active = active;
            capture(samplers, names, config, receiver)
        })?;
    Ok(ProfileSession {
        stop,
        worker: Some(worker),
    })
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Stack {
    tid: u32,
    frames: Vec<u64>,
}

fn capture(
    mut samplers: Vec<Sampler>,
    names: BTreeMap<u32, String>,
    config: ProfileConfig,
    stop: mpsc::Receiver<()>,
) -> Result<Profile, ProfileError> {
    let start_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ProfileError::Backend("clock is before Unix epoch".into()))?
            .as_millis(),
    )
    .map_err(|_| ProfileError::Limit("timestamp"))?;
    for sampler in &mut samplers {
        sampler.as_counter_mut().enable()?;
    }
    let began = Instant::now();
    let mut raw: BTreeMap<Stack, (u64, u64)> = BTreeMap::new();
    let mut count = 0usize;
    let mut lost = 0u64;
    let mut truncated = false;
    'capture: loop {
        if began.elapsed() >= config.duration || stop.try_recv().is_ok() {
            break;
        }
        for sampler in &mut samplers {
            while let Some(record) = sampler.next_record() {
                match record.parse_record().map_err(|error| {
                    ProfileError::Backend(format!("invalid kernel sample: {error}"))
                })? {
                    Record::Sample(sample) => {
                        let tid = sample.tid().ok_or_else(|| {
                            ProfileError::Backend("missing sample thread ID".into())
                        })?;
                        let mut frames: Vec<u64> = sample
                            .callchain()
                            .unwrap_or(&[])
                            .iter()
                            .copied()
                            .filter(|&ip| ip != 0 && ip < u64::MAX - 4095)
                            .take(MAX_STACK_DEPTH)
                            .collect();
                        if frames.is_empty()
                            && let Some(ip) = sample.ip()
                        {
                            frames.push(ip);
                        }
                        if frames.is_empty() {
                            lost = lost.saturating_add(1);
                            continue;
                        }
                        frames.reverse();
                        let key = Stack { tid, frames };
                        if !raw.contains_key(&key) && raw.len() >= config.max_unique_stacks {
                            truncated = true;
                            break 'capture;
                        }
                        let value = raw.entry(key).or_default();
                        value.0 += 1;
                        value.1 = value.1.saturating_add(
                            sample
                                .period()
                                .unwrap_or(1_000_000_000 / u64::from(config.frequency_hz)),
                        );
                        count += 1;
                        if count >= config.max_samples {
                            truncated = true;
                            break 'capture;
                        }
                    }
                    Record::Lost(record) => lost = lost.saturating_add(record.lost),
                    _ => {}
                }
                if began.elapsed() >= config.duration {
                    break 'capture;
                }
            }
        }
        match stop.recv_timeout(Duration::from_millis(5)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    let duration = began.elapsed();
    for sampler in &mut samplers {
        sampler.as_counter_mut().disable()?;
    }
    drop(samplers);
    let mut symbols = BTreeMap::new();
    let mut samples = Vec::with_capacity(raw.len());
    for (stack, (count, cpu_ns)) in raw {
        let mut frames = Vec::with_capacity(stack.frames.len());
        for ip in stack.frames {
            if !symbols.contains_key(&ip) && symbols.len() >= config.max_nodes {
                return Err(ProfileError::Limit("symbol cache"));
            }
            let name: &mut std::sync::Arc<str> =
                symbols.entry(ip).or_insert_with(|| symbol(ip).into());
            frames.push(name.clone());
        }
        samples.push(Sample {
            thread_id: stack.tid,
            thread_name: names
                .get(&stack.tid)
                .cloned()
                .unwrap_or_else(|| "native-thread".into()),
            frames,
            samples: count,
            cpu_ns,
        });
    }
    export_samples(samples, start_ms, duration, lost, truncated, &config)
}

fn symbol(ip: u64) -> String {
    let mut name = None;
    backtrace::resolve(ip as *mut std::ffi::c_void, |resolved| {
        if let Some(symbol) = resolved.name() {
            name = Some(symbol.to_string());
        }
    });
    let mut name = name.unwrap_or_else(|| format!("0x{ip:x}"));
    if name.len() > 256 {
        let mut end = 256;
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
    }
    name
}
