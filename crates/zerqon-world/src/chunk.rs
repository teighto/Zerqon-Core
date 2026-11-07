use crate::{Section, WorldError};

pub const MIN_Y: i32 = -64;
pub const SECTION_COUNT: usize = 24;
pub const WORLD_HEIGHT: usize = SECTION_COUNT * 16;

#[derive(Clone, Copy, Debug)]
pub struct FlatPalette {
    pub air: u32,
    pub bedrock: u32,
    pub stone: u32,
    pub dirt: u32,
    pub grass: u32,
}

#[derive(Clone, Debug)]
pub struct Chunk {
    pub x: i32,
    pub z: i32,
    pub(crate) sections: [Section; SECTION_COUNT],
}

impl Chunk {
    pub fn flat(x: i32, z: i32, palette: FlatPalette) -> Self {
        let mut sections = std::array::from_fn(|index| {
            Section::uniform(if index < 8 {
                palette.stone
            } else {
                palette.air
            })
        });
        sections[0] = Section::from_fn(|index| {
            if index < 256 {
                palette.bedrock
            } else {
                palette.stone
            }
        });
        sections[7] = Section::from_fn(|index| match index / 256 {
            0..=11 => palette.stone,
            12..=14 => palette.dirt,
            _ => palette.grass,
        });
        Self { x, z, sections }
    }
    pub fn sections(&self) -> &[Section; SECTION_COUNT] {
        &self.sections
    }
    pub fn get_block(&self, x: u8, y: i32, z: u8) -> Option<u32> {
        let (section, index) = coordinates(x, y, z)?;
        self.sections[section].get(index)
    }
    pub fn set_block(&mut self, x: u8, y: i32, z: u8, state: u32) -> Result<u32, WorldError> {
        let (section, index) = coordinates(x, y, z).ok_or(WorldError::OutOfBounds)?;
        self.sections[section].set(index, state)
    }
    pub fn heap_bytes(&self) -> usize {
        self.sections.iter().map(Section::heap_bytes).sum()
    }
}

fn coordinates(x: u8, y: i32, z: u8) -> Option<(usize, usize)> {
    if x > 15 || z > 15 || !(MIN_Y..MIN_Y + WORLD_HEIGHT as i32).contains(&y) {
        return None;
    }
    let y = (y - MIN_Y) as usize;
    Some((y / 16, (y % 16) * 256 + z as usize * 16 + x as usize))
}
