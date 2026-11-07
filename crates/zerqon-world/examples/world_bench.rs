use std::collections::VecDeque;
use std::hint::black_box;
use std::time::Instant;
use zerqon_world::{Chunk, ChunkWorkers, FlatPalette, SECTION_COUNT, SECTION_VOLUME, WorldError};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let count = arguments
        .next()
        .unwrap_or_else(|| "10000".into())
        .parse::<usize>()?;
    let threads = arguments
        .next()
        .unwrap_or_else(|| "4".into())
        .parse::<usize>()?;
    if !(1..=1_000_000).contains(&count)
        || !(1..=256).contains(&threads)
        || arguments.next().is_some()
    {
        return Err("usage: world_bench [chunks:1..1000000] [threads:1..256]".into());
    }
    let palette = FlatPalette {
        air: 0,
        bedrock: 1,
        stone: 2,
        dirt: 3,
        grass: 4,
    };
    for x in 0..100 {
        black_box(Chunk::flat(x, 0, palette));
    }

    let started = Instant::now();
    for x in 0..count {
        black_box(Chunk::flat(x as i32, 0, palette));
    }
    let serial = started.elapsed().as_secs_f64();

    let workers = ChunkWorkers::new(threads, threads * 2)?;
    let started = Instant::now();
    let mut pending: VecDeque<zerqon_world::ChunkTicket> = VecDeque::new();
    for x in 0..count {
        loop {
            match workers.try_generate(x as i32, 0, palette) {
                Ok(ticket) => {
                    pending.push_back(ticket);
                    break;
                }
                Err(WorldError::QueueFull) => {
                    if let Some(ticket) = pending.pop_front() {
                        black_box(ticket.wait()?);
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        if pending.len() >= threads * 3
            && let Some(ticket) = pending.pop_front()
        {
            black_box(ticket.wait()?);
        }
    }
    while let Some(ticket) = pending.pop_front() {
        black_box(ticket.wait()?);
    }
    let parallel = started.elapsed().as_secs_f64();
    workers.shutdown()?;

    let chunk = Chunk::flat(0, 0, palette);
    let encoded = chunk.to_bytes();
    let started = Instant::now();
    for _ in 0..count {
        black_box(Chunk::from_bytes(black_box(&encoded))?);
    }
    let decode = started.elapsed().as_secs_f64();

    println!("synthetic_flat_chunks={count} worker_threads={threads}");
    println!(
        "serial_generation_seconds={serial:.6} chunks_per_second={:.0}",
        count as f64 / serial
    );
    println!(
        "parallel_generation_seconds={parallel:.6} chunks_per_second={:.0}",
        count as f64 / parallel
    );
    println!(
        "decode_seconds={decode:.6} chunks_per_second={:.0}",
        count as f64 / decode
    );
    println!(
        "flat_section_heap_bytes={} flat_chunk_struct_bytes={} encoded_bytes={} dense_block_array_bytes={}",
        chunk.heap_bytes(),
        std::mem::size_of::<Chunk>(),
        encoded.len(),
        SECTION_COUNT * SECTION_VOLUME * 4
    );
    println!(
        "Memory figures exclude allocator overhead, queues, cache and process RSS. This is not a gameplay benchmark."
    );
    Ok(())
}
