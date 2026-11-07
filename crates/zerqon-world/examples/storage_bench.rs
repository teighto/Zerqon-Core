#[cfg(unix)]
use std::fs::File;
use std::{io::Write, sync::Arc, time::Instant};
use uuid::Uuid;
use zerqon_world::{Chunk, ChunkKey, ChunkWrite, FlatPalette, WorldStore};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).map_or("transaction", String::as_str);
    let iterations = args.get(2).map_or(Ok(100usize), |value| value.parse())?;
    let count = args.get(3).map_or(Ok(4usize), |value| value.parse())?;
    if !matches!(mode, "transaction" | "legacy")
        || !(1..=10000).contains(&iterations)
        || !(1..=16).contains(&count)
    {
        return Err(
            "usage: storage_bench [transaction|legacy] [1..10000 iterations] [1..16 chunks]".into(),
        );
    }
    let root = tempfile::tempdir()?;
    let palette = FlatPalette {
        air: 0,
        bedrock: 1,
        stone: 2,
        dirt: 3,
        grass: 4,
    };
    let keys: Vec<_> = (0..count).map(|x| ChunkKey::new(x as i32, 0)).collect();
    let store = if mode == "transaction" {
        Some(WorldStore::open(root.path())?)
    } else {
        None
    };
    let mut timings = Vec::new();
    for iteration in 0..iterations {
        let chunks: Vec<_> = keys
            .iter()
            .map(|key| {
                let mut chunk = Chunk::flat(key.x, key.z, palette);
                chunk.set_block(0, 64, 0, iteration as u32 + 100).unwrap();
                chunk
            })
            .collect();
        let start = Instant::now();
        if let Some(store) = &store {
            let snapshots = store.snapshot(&keys)?;
            let writes = chunks
                .into_iter()
                .map(|chunk| ChunkWrite {
                    key: ChunkKey::new(chunk.x, chunk.z),
                    replacement: Arc::new(chunk),
                })
                .collect::<Vec<_>>();
            store.commit(Uuid::new_v4(), &snapshots, &writes)?;
        } else {
            for chunk in chunks {
                let path = root.path().join(format!("c.{}.{}.zqc", chunk.x, chunk.z));
                if path.exists() {
                    Chunk::from_bytes(&std::fs::read(&path)?)?;
                }
                let mut temporary = tempfile::NamedTempFile::new_in(root.path())?;
                temporary.write_all(&chunk.to_bytes())?;
                temporary.as_file().sync_all()?;
                temporary.persist(path)?;
                #[cfg(unix)]
                File::open(root.path())?.sync_all()?;
            }
        }
        timings.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    if let Some(store) = &store {
        for key in &keys {
            assert_eq!(
                store.load(key.x, key.z)?.unwrap().get_block(0, 64, 0),
                Some(iterations as u32 + 99)
            );
        }
    }
    drop(store);
    let bytes: u64 = std::fs::read_dir(root.path())?
        .map(|entry| Ok::<_, std::io::Error>(entry?.metadata()?.len()))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .sum();
    timings.sort_by(f64::total_cmp);
    let percentile = |p: f64| timings[((timings.len() - 1) as f64 * p).round() as usize];
    println!(
        "{}",
        serde_json::json!({"mode":mode,"iterations":iterations,"chunks":count,"p50_ms":percentile(0.5),"p95_ms":percentile(0.95),"p99_ms":percentile(0.99),"files_bytes":bytes})
    );
    Ok(())
}
