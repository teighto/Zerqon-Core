use prost::Message;
#[cfg(any(target_os = "linux", test))]
use std::collections::BTreeMap;
use std::{io::Write, path::Path, time::Duration};

#[cfg(target_os = "linux")]
mod linux;
mod spark;

pub const SPARK_MIME: &str = "application/x-spark-sampler";
pub const MAX_STACK_DEPTH: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("native CPU profiling is supported only on Linux")]
    Unsupported,
    #[error("invalid profiler limits: {0}")]
    InvalidLimits(&'static str),
    #[error("native profiler is already running")]
    AlreadyRunning,
    #[error("native profiler received no CPU samples")]
    NoSamples,
    #[error("native profiler limit exceeded: {0}")]
    Limit(&'static str),
    #[error("native profiler backend failed: {0}")]
    Backend(String),
    #[error("native profiler worker failed")]
    Worker,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct ProfileConfig {
    pub duration: Duration,
    pub frequency_hz: u32,
    pub max_threads: usize,
    pub max_samples: usize,
    pub max_unique_stacks: usize,
    pub max_nodes: usize,
    pub max_output_bytes: usize,
}

impl Default for ProfileConfig {
    fn default() -> Self {
        Self {
            duration: Duration::from_secs(30),
            frequency_hz: 100,
            max_threads: 128,
            max_samples: 20000,
            max_unique_stacks: 4096,
            max_nodes: 65536,
            max_output_bytes: 16 * 1024 * 1024,
        }
    }
}

impl ProfileConfig {
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.duration < Duration::from_millis(100) || self.duration > Duration::from_secs(300) {
            return Err(ProfileError::InvalidLimits(
                "duration must be 100 ms to 300 s",
            ));
        }
        if !(1..=1000).contains(&self.frequency_hz) {
            return Err(ProfileError::InvalidLimits("frequency must be 1..1000 Hz"));
        }
        for (value, max) in [
            (self.max_threads, 256),
            (self.max_samples, 100000),
            (self.max_unique_stacks, 8192),
            (self.max_nodes, 131072),
        ] {
            if value == 0 || value > max {
                return Err(ProfileError::InvalidLimits(
                    "thread, sample, stack or node bound is invalid",
                ));
            }
        }
        if !(1024..=32 * 1024 * 1024).contains(&self.max_output_bytes) {
            return Err(ProfileError::InvalidLimits(
                "output limit must be 1 KiB to 32 MiB",
            ));
        }
        Ok(())
    }
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone)]
struct Sample {
    thread_id: u32,
    thread_name: String,
    frames: Vec<std::sync::Arc<str>>,
    samples: u64,
    cpu_ns: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProfileSummary {
    pub samples: u64,
    pub threads: usize,
    pub nodes: usize,
    pub estimated_cpu_ms: f64,
    pub duration_ms: u64,
    pub lost_samples: u64,
    pub truncated: bool,
    pub backend: &'static str,
}

pub struct Profile {
    data: spark::SamplerData,
    summary: ProfileSummary,
    max_output_bytes: usize,
}

impl Profile {
    pub fn summary(&self) -> &ProfileSummary {
        &self.summary
    }
    pub fn spark_bytes(&self) -> Result<Vec<u8>, ProfileError> {
        if self.data.encoded_len() > self.max_output_bytes {
            return Err(ProfileError::Limit("profile output bytes"));
        }
        Ok(self.data.encode_to_vec())
    }
    pub fn spark_gzip(&self) -> Result<Vec<u8>, ProfileError> {
        let bytes = self.spark_bytes()?;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&bytes)?;
        let result = encoder.finish()?;
        if result.len() > self.max_output_bytes {
            return Err(ProfileError::Limit("compressed profile output bytes"));
        }
        Ok(result)
    }
    pub fn write_spark(&self, path: &Path) -> Result<(), ProfileError> {
        let bytes = self.spark_bytes()?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(path)
            .map_err(|error| ProfileError::Io(error.error))?;
        Ok(())
    }
}

#[cfg(any(target_os = "linux", test))]
type NodeIndex = BTreeMap<(Option<i32>, std::sync::Arc<str>), i32>;

#[cfg(any(target_os = "linux", test))]
fn export_samples(
    samples: Vec<Sample>,
    start_ms: i64,
    duration: Duration,
    lost: u64,
    truncated: bool,
    config: &ProfileConfig,
) -> Result<Profile, ProfileError> {
    if samples.is_empty() {
        return Err(ProfileError::NoSamples);
    }
    let mut threads: BTreeMap<u32, spark::Thread> = BTreeMap::new();
    let mut indexes: BTreeMap<u32, NodeIndex> = BTreeMap::new();
    let mut nodes = 0usize;
    let mut count = 0_u64;
    let mut cpu_ns = 0_u64;
    for sample in samples {
        if sample.frames.is_empty()
            || sample.frames.len() > MAX_STACK_DEPTH
            || sample.samples == 0
            || sample.cpu_ns == 0
        {
            return Err(ProfileError::Backend("invalid CPU sample".into()));
        }
        count = count
            .checked_add(sample.samples)
            .ok_or(ProfileError::Limit("sample counter"))?;
        cpu_ns = cpu_ns
            .checked_add(sample.cpu_ns)
            .ok_or(ProfileError::Limit("CPU time counter"))?;
        if count > config.max_samples as u64 {
            return Err(ProfileError::Limit("sample count"));
        }
        if !threads.contains_key(&sample.thread_id) && threads.len() >= config.max_threads {
            return Err(ProfileError::Limit("thread count"));
        }
        let thread = threads
            .entry(sample.thread_id)
            .or_insert_with(|| spark::Thread {
                name: format!("{} [tid {}]", sample.thread_name, sample.thread_id),
                children: Vec::new(),
                times: vec![0.0],
                children_refs: Vec::new(),
            });
        let index = indexes.entry(sample.thread_id).or_default();
        let weight = sample.cpu_ns as f64 / 1_000_000.0;
        thread.times[0] += weight;
        let mut parent = None;
        for frame in sample.frames {
            if frame.len() > 256 {
                return Err(ProfileError::Limit("symbol length"));
            }
            let key = (parent, frame.clone());
            let node = if let Some(&id) = index.get(&key) {
                id
            } else {
                if nodes >= config.max_nodes {
                    return Err(ProfileError::Limit("stack tree nodes"));
                }
                let id = thread.children.len() as i32;
                thread.children.push(spark::Node {
                    class_name: "native".into(),
                    method_name: frame.to_string(),
                    parent_line: 0,
                    line: 0,
                    times: vec![0.0],
                    children_refs: Vec::new(),
                });
                if let Some(parent) = parent {
                    thread.children[parent as usize].children_refs.push(id);
                } else {
                    thread.children_refs.push(id);
                }
                index.insert(key, id);
                nodes += 1;
                id
            };
            thread.children[node as usize].times[0] += weight;
            parent = Some(node);
        }
    }
    let duration_ms =
        u64::try_from(duration.as_millis()).map_err(|_| ProfileError::Limit("duration"))?;
    let end_ms = start_ms
        .checked_add(duration_ms as i64)
        .ok_or(ProfileError::Limit("timestamp"))?;
    let window =
        i32::try_from(start_ms / 60000).map_err(|_| ProfileError::Limit("window timestamp"))?;
    let summary = ProfileSummary {
        samples: count,
        threads: threads.len(),
        nodes,
        estimated_cpu_ms: cpu_ns as f64 / 1_000_000.0,
        duration_ms,
        lost_samples: lost,
        truncated,
        backend: "Linux perf_event_open CPU-clock",
    };
    let ids = threads.keys().map(|&id| i64::from(id)).collect();
    let metadata=spark::Metadata{
        start_time:start_ms,end_time:end_ms,interval:(1_000_000/config.frequency_hz) as i32,
        thread_dumper:Some(spark::ThreadDumper{kind:1,ids}),aggregator:Some(spark::Aggregator{kind:0,thread_grouper:0}),
        comment:"Native Rust CPU samples. One aggregated window. No Java stacks, GC metrics, or tick timing attribution.".into(),
        platform:Some(spark::Platform{kind:0,name:"Zerqon".into(),version:env!("CARGO_PKG_VERSION").into(),minecraft_version:"26.3".into(),data_version:2,brand:"Zerqon".into()}),
        extra:BTreeMap::from([
            ("profiler_backend".into(),summary.backend.into()),("sample_count".into(),count.to_string()),
            ("lost_samples".into(),lost.to_string()),("truncated".into(),truncated.to_string()),
            ("thread_coverage".into(),"Threads present when capture started".into()),
            ("engine_note".into(),"Engine enum 2 is native Rust; not Java or async-profiler".into()),
            ("spark_schema".into(),"lucko/spark@7ffae3d7ceb69df95480fd03e3f89d979c780fd8".into()),
        ]),mode:0,engine:2,engine_version:"perf-event2 0.7.4".into(),
    };
    let data = spark::SamplerData {
        metadata: Some(metadata),
        threads: threads.into_values().collect(),
        time_windows: vec![window],
        windows: BTreeMap::from([(
            window,
            spark::Window {
                start_time: start_ms,
                end_time: end_ms,
                duration: duration_ms as i32,
            },
        )]),
    };
    let profile = Profile {
        data,
        summary,
        max_output_bytes: config.max_output_bytes,
    };
    if profile.data.encoded_len() > config.max_output_bytes {
        return Err(ProfileError::Limit("profile output bytes"));
    }
    Ok(profile)
}

pub struct ProfileSession {
    stop: std::sync::mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<Result<Profile, ProfileError>>>,
}

impl ProfileSession {
    pub fn start(config: ProfileConfig) -> Result<Self, ProfileError> {
        config.validate()?;
        #[cfg(target_os = "linux")]
        {
            linux::start(config)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(ProfileError::Unsupported)
        }
    }
    pub fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }
    pub fn finish(mut self) -> Result<Profile, ProfileError> {
        let _ = self.stop.send(());
        self.worker
            .take()
            .ok_or(ProfileError::Worker)?
            .join()
            .map_err(|_| ProfileError::Worker)?
    }
}

impl Drop for ProfileSession {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unbounded_or_zero_sampling_limits() {
        for config in [
            ProfileConfig {
                duration: Duration::ZERO,
                ..ProfileConfig::default()
            },
            ProfileConfig {
                duration: Duration::from_secs(301),
                ..ProfileConfig::default()
            },
            ProfileConfig {
                frequency_hz: 1001,
                ..ProfileConfig::default()
            },
            ProfileConfig {
                max_samples: 0,
                ..ProfileConfig::default()
            },
            ProfileConfig {
                max_threads: 257,
                ..ProfileConfig::default()
            },
        ] {
            assert!(config.validate().is_err());
        }
    }

    fn sampled_profile() -> Profile {
        export_samples(
            vec![
                Sample {
                    thread_id: 7,
                    thread_name: "zerqon-world".into(),
                    frames: vec!["root".into(), "work".into()],
                    samples: 3,
                    cpu_ns: 30_000_000,
                },
                Sample {
                    thread_id: 7,
                    thread_name: "zerqon-world".into(),
                    frames: vec!["root".into(), "other".into()],
                    samples: 2,
                    cpu_ns: 20_000_000,
                },
            ],
            1_700_000_000_000,
            Duration::from_secs(1),
            0,
            false,
            &ProfileConfig::default(),
        )
        .unwrap()
    }

    #[test]
    fn spark_tree_contains_actual_sample_weights_and_valid_flat_references() {
        let profile = sampled_profile();
        let bytes = profile.spark_bytes().unwrap();
        let decoded = spark::SamplerData::decode(bytes.as_slice()).unwrap();
        let thread = &decoded.threads[0];
        assert_eq!(thread.times, vec![50.0]);
        assert_eq!(profile.summary.samples, 5);
        let root = &thread.children[thread.children_refs[0] as usize];
        assert_eq!(root.method_name, "root");
        assert_eq!(root.times, vec![50.0]);
        let mut leaves: Vec<_> = root
            .children_refs
            .iter()
            .map(|&id| {
                (
                    &thread.children[id as usize].method_name,
                    thread.children[id as usize].times[0] as u64,
                )
            })
            .collect();
        leaves.sort();
        assert_eq!(
            leaves,
            vec![(&"other".to_string(), 20), (&"work".to_string(), 30)]
        );
        let metadata = decoded.metadata.unwrap();
        assert_eq!(metadata.engine, 2);
        assert!(!metadata.extra.is_empty());
    }

    #[test]
    fn file_export_preserves_existing_profiles_and_gzip_matches_raw() {
        use std::io::Read;
        let profile = sampled_profile();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.sparkprofile");
        profile.write_spark(&path).unwrap();
        assert!(profile.write_spark(&path).is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            profile.spark_bytes().unwrap()
        );
        let gzip = profile.spark_gzip().unwrap();
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(gzip.as_slice())
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, profile.spark_bytes().unwrap());
    }
    #[test]
    fn output_size_cap_is_applied_before_serialization() {
        let mut profile = sampled_profile();
        profile.max_output_bytes = 1;
        assert!(matches!(profile.spark_bytes(), Err(ProfileError::Limit(_))));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn unsupported_os_never_returns_a_fake_cpu_profile() {
        assert!(matches!(
            ProfileSession::start(ProfileConfig::default()),
            Err(ProfileError::Unsupported)
        ));
    }

    #[test]
    fn tree_node_and_sample_limits_reject_incomplete_exports() {
        let sample = Sample {
            thread_id: 1,
            thread_name: "worker".into(),
            frames: vec!["root".into(), "child".into()],
            samples: 2,
            cpu_ns: 20000,
        };
        for config in [
            ProfileConfig {
                max_nodes: 1,
                ..ProfileConfig::default()
            },
            ProfileConfig {
                max_samples: 1,
                ..ProfileConfig::default()
            },
        ] {
            assert!(matches!(
                export_samples(
                    vec![sample.clone()],
                    1000,
                    Duration::from_secs(1),
                    0,
                    false,
                    &config
                ),
                Err(ProfileError::Limit(_))
            ));
        }
    }

    #[test]
    fn empty_samples_are_not_reported_as_a_successful_profile() {
        assert!(matches!(
            export_samples(
                Vec::new(),
                1000,
                Duration::from_secs(1),
                0,
                false,
                &ProfileConfig::default()
            ),
            Err(ProfileError::NoSamples)
        ));
    }
}
