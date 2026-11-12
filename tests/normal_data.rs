#![allow(dead_code)]

#[path = "../src/entity_packets.rs"]
mod entity_packets;
#[path = "../src/flight_speed.rs"]
mod flight_speed;
#[path = "../src/game_data.rs"]
mod game_data;
#[path = "../src/game_mode.rs"]
mod game_mode;
#[path = "../src/packets.rs"]
mod packets;

use std::path::Path;
use zerqon_protocol::codec::{get_varint, take};

#[test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
fn normal_palette_and_chunk_biomes_use_prepared_registry_ids() {
    let data = game_data::GameData::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/vanilla/26.3"),
    )
    .unwrap();
    let palette = data
        .normal
        .expect("official registry contains the normal palette");
    let generator = zerqon_world::NormalGenerator::new(0, palette);
    let (x, z) = (-32..=32)
        .flat_map(|x| (-32..=32).map(move |z| (x, z)))
        .find(|&(x, z)| {
            let first = generator.biome_at(i64::from(x) * 16, i64::from(z) * 16);
            (0..16).any(|index| {
                generator.biome_at(
                    i64::from(x) * 16 + index % 4 * 4,
                    i64::from(z) * 16 + index / 4 * 4,
                ) != first
            })
        })
        .expect("bounded terrain region crosses a biome boundary");
    let chunk = generator.generate(x, z);
    let encoded = packets::chunk_with_generator(&chunk, &data, Some(generator)).unwrap();
    let mut body = encoded.as_slice();
    assert_eq!(get_varint(&mut body).unwrap(), 46);
    assert_eq!(take(&mut body, 4).unwrap(), x.to_be_bytes());
    assert_eq!(take(&mut body, 4).unwrap(), z.to_be_bytes());
    for _ in 0..get_varint(&mut body).unwrap() {
        get_varint(&mut body).unwrap();
        let words = get_varint(&mut body).unwrap() as usize;
        take(&mut body, words * 8).unwrap();
    }
    let length = get_varint(&mut body).unwrap() as usize;
    let mut sections = take(&mut body, length).unwrap();
    for _ in 0..24 {
        take(&mut sections, 4).unwrap();
        let block_bits = take(&mut sections, 1).unwrap()[0] as usize;
        if block_bits == 0 {
            get_varint(&mut sections).unwrap();
        } else {
            if block_bits <= 8 {
                for _ in 0..get_varint(&mut sections).unwrap() {
                    get_varint(&mut sections).unwrap();
                }
            }
            take(&mut sections, 4096_usize.div_ceil(64 / block_bits) * 8).unwrap();
        }
        let bits = take(&mut sections, 1).unwrap()[0] as usize;
        assert!((1..=3).contains(&bits));
        let size = get_varint(&mut sections).unwrap();
        let biomes: Vec<_> = (0..size)
            .map(|_| get_varint(&mut sections).unwrap())
            .collect();
        let per_word = 64 / bits;
        let words = take(&mut sections, 64_usize.div_ceil(per_word) * 8).unwrap();
        for index in 0..64 {
            let word = u64::from_be_bytes(words[index / per_word * 8..][..8].try_into().unwrap());
            let palette_index = (word >> (index % per_word * bits)) & ((1 << bits) - 1);
            assert_eq!(
                biomes[palette_index as usize],
                generator.biome_at(
                    i64::from(x) * 16 + (index % 4) as i64 * 4,
                    i64::from(z) * 16 + (index / 4 % 4) as i64 * 4,
                )
            );
        }
    }
    assert!(sections.is_empty());
}
