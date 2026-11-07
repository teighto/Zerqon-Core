use proptest::prelude::*;
use zerqon_world::{Chunk, FlatPalette, Section};

const PALETTE: FlatPalette = FlatPalette {
    air: 7,
    bedrock: 12,
    stone: 55,
    dirt: 91,
    grass: 108,
};

#[test]
fn uniform_fast_path_cannot_hide_a_mixed_block() {
    let mut section = Section::uniform(313);
    assert_eq!(section.uniform_state(), Some(313));
    section.set(231, 314).unwrap();
    assert_eq!(section.uniform_state(), None);
    assert_eq!(section.get(231), Some(314));
}

#[test]
fn single_mutation_preserves_other_blocks() {
    let mut section = Section::uniform(7);
    assert_eq!(section.set(4095, 123).unwrap(), 7);
    assert_eq!(section.get(4095), Some(123));
    assert_eq!(section.get(4094), Some(7));
    assert!(section.heap_bytes() < 2048);
}

#[test]
fn flat_terrain_uses_supplied_registry_values_at_exact_heights() {
    let chunk = Chunk::flat(-4, 23, PALETTE);
    for (y, expected) in [
        (-64, 12),
        (-63, 55),
        (59, 55),
        (60, 91),
        (62, 91),
        (63, 108),
        (64, 7),
        (319, 7),
    ] {
        assert_eq!(chunk.get_block(15, y, 15), Some(expected), "y={y}");
    }
}

#[test]
fn chunk_mutations_respect_section_boundaries() {
    let mut chunk = Chunk::flat(0, 0, PALETTE);
    for (x, y, z, value) in [
        (0, -64, 0, 213),
        (15, -49, 15, 444),
        (0, -48, 0, 555),
        (15, 319, 15, 666),
    ] {
        chunk.set_block(x, y, z, value).unwrap();
        assert_eq!(chunk.get_block(x, y, z), Some(value));
    }
    assert_eq!(chunk.get_block(14, 319, 15), Some(7));
    assert!(chunk.set_block(16, 64, 0, 2).is_err());
    assert_eq!(chunk.get_block(0, i32::MIN, 0), None);
}

#[test]
fn repeated_unique_replacements_do_not_grow_palette_without_bound() {
    let mut section = Section::uniform(0);
    for state in 1..=9000 {
        section.set(2048, state).unwrap();
    }
    assert_eq!(section.get(2048), Some(9000));
    assert_eq!(section.get(2047), Some(0));
    assert!(section.heap_bytes() <= 32 * 1024);
}

#[test]
fn full_palette_growth_preserves_every_position() {
    let mut section = Section::uniform(0);
    for index in 0..4096 {
        section.set(index, index as u32).unwrap();
    }
    section.set(0, u32::MAX).unwrap();
    assert_eq!(section.get(0), Some(u32::MAX));
    for index in 1..4096 {
        assert_eq!(section.get(index), Some(index as u32));
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn compact_storage_matches_independent_dense_model(
        initial in any::<u32>(),
        updates in prop::collection::vec((0usize..4096, any::<u32>()), 1..180)
    ) {
        let mut section = Section::uniform(initial);
        let mut dense = vec![initial; 4096];
        for (index, state) in updates {
            prop_assert_eq!(section.set(index, state).unwrap(), dense[index]);
            dense[index] = state;
        }
        prop_assert!(section.iter().eq(dense));
    }
}
