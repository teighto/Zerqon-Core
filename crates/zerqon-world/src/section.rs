use crate::WorldError;
use std::collections::HashMap;

pub const SECTION_VOLUME: usize = 4096;

#[derive(Clone, Debug)]
pub struct Section {
    storage: Storage,
}

#[derive(Clone, Debug)]
enum Storage {
    Uniform(u32),
    Indirect {
        palette: Vec<u32>,
        bits: u8,
        words: Vec<u64>,
    },
}

impl Section {
    pub fn uniform(state: u32) -> Self {
        Self {
            storage: Storage::Uniform(state),
        }
    }

    pub fn uniform_state(&self) -> Option<u32> {
        match self.storage {
            Storage::Uniform(state) => Some(state),
            Storage::Indirect { .. } => None,
        }
    }

    pub fn from_states(states: &[u32]) -> Result<Self, WorldError> {
        if states.len() != SECTION_VOLUME {
            return Err(WorldError::Corrupt("section must have 4096 blocks"));
        }
        Ok(Self::from_fn(|index| states[index]))
    }

    pub fn from_fn(mut state_at: impl FnMut(usize) -> u32) -> Self {
        let mut palette = Vec::new();
        let mut ids = HashMap::new();
        let indices: Vec<usize> = (0..SECTION_VOLUME)
            .map(|index| {
                let state = state_at(index);
                *ids.entry(state).or_insert_with(|| {
                    palette.push(state);
                    palette.len() - 1
                })
            })
            .collect();
        if palette.len() == 1 {
            return Self::uniform(palette[0]);
        }
        let bits = required_bits(palette.len());
        let mut words = vec![0; word_count(bits)];
        for (index, id) in indices.into_iter().enumerate() {
            set_id(&mut words, bits, index, id);
        }
        Self {
            storage: Storage::Indirect {
                palette,
                bits,
                words,
            },
        }
    }

    pub fn get(&self, index: usize) -> Option<u32> {
        if index >= SECTION_VOLUME {
            return None;
        }
        Some(match &self.storage {
            Storage::Uniform(state) => *state,
            Storage::Indirect {
                palette,
                bits,
                words,
            } => palette[get_id(words, *bits, index)],
        })
    }

    pub fn set(&mut self, index: usize, state: u32) -> Result<u32, WorldError> {
        let old = self.get(index).ok_or(WorldError::OutOfBounds)?;
        if old == state {
            return Ok(old);
        }
        match &mut self.storage {
            Storage::Uniform(previous) => {
                let mut words = vec![0; word_count(1)];
                set_id(&mut words, 1, index, 1);
                self.storage = Storage::Indirect {
                    palette: vec![*previous, state],
                    bits: 1,
                    words,
                };
            }
            Storage::Indirect {
                palette,
                bits,
                words,
            } => {
                if let Some(id) = palette.iter().position(|candidate| *candidate == state) {
                    set_id(words, *bits, index, id);
                } else if palette.len() == SECTION_VOLUME {
                    let mut values: Vec<u32> = self.iter().collect();
                    values[index] = state;
                    *self = Self::from_states(&values)?;
                } else {
                    let id = palette.len();
                    palette.push(state);
                    let next_bits = required_bits(palette.len());
                    if next_bits > *bits {
                        let mut repacked = vec![0; word_count(next_bits)];
                        for index in 0..SECTION_VOLUME {
                            set_id(&mut repacked, next_bits, index, get_id(words, *bits, index));
                        }
                        *words = repacked;
                        *bits = next_bits;
                    }
                    set_id(words, *bits, index, id);
                }
            }
        }
        Ok(old)
    }

    pub fn heap_bytes(&self) -> usize {
        match &self.storage {
            Storage::Uniform(_) => 0,
            Storage::Indirect { palette, words, .. } => {
                palette.capacity() * 4 + words.capacity() * 8
            }
        }
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = u32> + '_ {
        (0..SECTION_VOLUME).map(|index| match &self.storage {
            Storage::Uniform(state) => *state,
            Storage::Indirect {
                palette,
                bits,
                words,
            } => palette[get_id(words, *bits, index)],
        })
    }

    pub(crate) fn palette_data(&self) -> (&[u32], u8, &[u64]) {
        match &self.storage {
            Storage::Uniform(state) => (std::slice::from_ref(state), 0, &[]),
            Storage::Indirect {
                palette,
                bits,
                words,
            } => (palette, *bits, words),
        }
    }

    pub(crate) fn from_palette(
        palette: Vec<u32>,
        bits: u8,
        words: Vec<u64>,
    ) -> Result<Self, WorldError> {
        if palette.is_empty() || palette.len() > SECTION_VOLUME {
            return Err(WorldError::Corrupt("invalid section palette length"));
        }
        if palette.len() == 1 {
            if bits != 0 || !words.is_empty() {
                return Err(WorldError::Corrupt("uniform section contains indices"));
            }
            return Ok(Self::uniform(palette[0]));
        }
        if bits != required_bits(palette.len()) || words.len() != word_count(bits) {
            return Err(WorldError::Corrupt("invalid section packing"));
        }
        if (0..SECTION_VOLUME).any(|index| get_id(&words, bits, index) >= palette.len()) {
            return Err(WorldError::Corrupt("palette index is out of range"));
        }
        Ok(Self {
            storage: Storage::Indirect {
                palette,
                bits,
                words,
            },
        })
    }
}

fn required_bits(palette_len: usize) -> u8 {
    (usize::BITS - (palette_len - 1).leading_zeros()) as u8
}
fn word_count(bits: u8) -> usize {
    SECTION_VOLUME.div_ceil(64 / bits as usize)
}
fn get_id(words: &[u64], bits: u8, index: usize) -> usize {
    let per_word = 64 / bits as usize;
    ((words[index / per_word] >> ((index % per_word) * bits as usize)) & ((1_u64 << bits) - 1))
        as usize
}
fn set_id(words: &mut [u64], bits: u8, index: usize, id: usize) {
    let per_word = 64 / bits as usize;
    let shift = (index % per_word) * bits as usize;
    let mask = ((1_u64 << bits) - 1) << shift;
    words[index / per_word] = (words[index / per_word] & !mask) | ((id as u64) << shift);
}
