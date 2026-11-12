//! Initial native terrain generator. Deterministic for a seed, but not a vanilla seed replica.
use crate::{Chunk, MIN_Y, SECTION_VOLUME, Section, WORLD_HEIGHT};

pub const NORMAL_SEA_LEVEL: i32 = 62;

/// Registry IDs resolved by the application; generation never assumes numeric IDs.
#[derive(Clone, Copy, Debug)]
pub struct NormalPalette {
    pub air: u32,
    pub bedrock: u32,
    pub stone: u32,
    pub deepslate: u32,
    pub dirt: u32,
    pub grass: u32,
    pub sand: u32,
    pub sandstone: u32,
    pub gravel: u32,
    pub water: u32,
    pub lava: u32,
    /// Full snow block used as the cold-biome surface.
    pub snow: u32,
    pub oak_log: u32,
    pub oak_leaves: u32,
    pub spruce_log: u32,
    pub spruce_leaves: u32,
    pub coal_ore: u32,
    pub iron_ore: u32,
    pub copper_ore: u32,
    pub gold_ore: u32,
    pub redstone_ore: u32,
    pub diamond_ore: u32,
    pub plains: i32,
    pub forest: i32,
    pub desert: i32,
    pub snowy_plains: i32,
    pub ocean: i32,
    pub river: i32,
    pub mountains: i32,
}

/// Immutable and cheap to copy into bounded worker jobs.
#[derive(Clone, Copy, Debug)]
pub struct NormalGenerator {
    seed: u64,
    palette: NormalPalette,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Biome {
    Plains,
    Forest,
    Desert,
    Snow,
    Ocean,
    River,
    Mountains,
}

#[derive(Clone, Copy)]
struct Column {
    height: i32,
    biome: Biome,
}

#[derive(Clone, Copy)]
struct Tree {
    x: i64,
    z: i64,
    ground: i32,
    height: i32,
    spruce: bool,
}

impl NormalGenerator {
    pub fn new(seed: u64, palette: NormalPalette) -> Self {
        Self { seed, palette }
    }

    /// Nominal solid surface, excluding water and vegetation.
    pub fn surface_height(&self, world_x: i64, world_z: i64) -> i32 {
        self.column(world_x, world_z).height
    }

    /// Sample each quart cell at world block coordinates (`quart * 4`).
    /// Chunk stores blocks only; the client biome palette is supplied by the caller.
    pub fn biome_at(&self, world_x: i64, world_z: i64) -> i32 {
        match self.column(world_x, world_z).biome {
            Biome::Plains => self.palette.plains,
            Biome::Forest => self.palette.forest,
            Biome::Desert => self.palette.desert,
            Biome::Snow => self.palette.snowy_plains,
            Biome::Ocean => self.palette.ocean,
            Biome::River => self.palette.river,
            Biome::Mountains => self.palette.mountains,
        }
    }

    pub fn generate(&self, chunk_x: i32, chunk_z: i32) -> Chunk {
        let origin_x = i64::from(chunk_x) * 16;
        let origin_z = i64::from(chunk_z) * 16;
        let columns: [Column; 256] = std::array::from_fn(|index| {
            self.column(
                origin_x + (index % 16) as i64,
                origin_z + (index / 16) as i64,
            )
        });
        // One bounded scratch buffer avoids per-block palette growth/repacking.
        let mut blocks = vec![self.palette.air; WORLD_HEIGHT * 256];
        for (index, column) in columns.iter().copied().enumerate() {
            let x = origin_x + (index % 16) as i64;
            let z = origin_z + (index / 16) as i64;
            for y in MIN_Y..=column.height.max(NORMAL_SEA_LEVEL) {
                blocks[(y - MIN_Y) as usize * 256 + index] = self.terrain(x, y, z, column);
            }
        }
        // Global anchors include neighboring cells: crowns do not stop at chunk edges.
        for cell_x in (origin_x - 3).div_euclid(6)..=(origin_x + 18).div_euclid(6) {
            for cell_z in (origin_z - 3).div_euclid(6)..=(origin_z + 18).div_euclid(6) {
                if let Some(tree) = self.tree(cell_x, cell_z) {
                    self.place_tree(&mut blocks, origin_x, origin_z, tree);
                }
            }
        }
        let sections = std::array::from_fn(|section| {
            let offset = section * SECTION_VOLUME;
            let states = &blocks[offset..offset + SECTION_VOLUME];
            if states.iter().all(|&state| state == self.palette.air) {
                Section::uniform(self.palette.air)
            } else {
                Section::from_fn(|index| states[index])
            }
        });
        Chunk {
            x: chunk_x,
            z: chunk_z,
            sections,
        }
    }

    fn column(&self, x: i64, z: i64) -> Column {
        let continental = self.noise2(x, z, 720.0, 1);
        let hills = self.noise2(x, z, 170.0, 2) * 11.0
            + self.noise2(x, z, 57.0, 3) * 5.0
            + self.noise2(x, z, 19.0, 4) * 1.7;
        let mountain_field = self.noise2(x, z, 310.0, 5);
        let mountain = ((mountain_field - 0.1) * 1.7).clamp(0.0, 1.0);
        let ridge = 1.0 - self.noise2(x, z, 94.0, 6).abs();
        let mut height =
            68.0 + continental * 43.0 + hills + mountain * mountain * (38.0 + ridge * 76.0);
        let river_distance = self.noise2(x, z, 260.0, 7).abs();
        let river = river_distance < 0.047;
        if river {
            let bank = smooth((river_distance / 0.047).clamp(0.0, 1.0));
            let strength = smooth(((132.0 - height) / 30.0).clamp(0.0, 1.0))
                * smooth(((continental + 0.28) / 0.1).clamp(0.0, 1.0));
            height = lerp(height, 57.0, (1.0 - bank) * strength);
        }
        let height = height.round().clamp(23.0, 226.0) as i32;
        let temperature = self.noise2(x, z, 470.0, 8);
        let moisture = self.noise2(x, z, 360.0, 9);
        let biome = if river && height <= NORMAL_SEA_LEVEL + 1 {
            Biome::River
        } else if height < NORMAL_SEA_LEVEL - 1 {
            Biome::Ocean
        } else if temperature < -0.22 || height > 142 {
            Biome::Snow
        } else if mountain > 0.4 && height > 100 {
            Biome::Mountains
        } else if temperature > 0.22 && moisture < 0.12 {
            Biome::Desert
        } else if moisture > 0.02 {
            Biome::Forest
        } else {
            Biome::Plains
        };
        Column { height, biome }
    }

    fn terrain(&self, x: i64, y: i32, z: i64, column: Column) -> u32 {
        let p = self.palette;
        if y == MIN_Y
            || (y < MIN_Y + 4 && hash(self.seed ^ 10, x, i64::from(y), z) % 4 > (y - MIN_Y) as u64)
        {
            return p.bedrock;
        }
        if y > column.height {
            return p.water;
        }
        if y < column.height - 5 && y > MIN_Y + 4 && self.cave(x, y, z) {
            return if y < -54 { p.lava } else { p.air };
        }
        let depth = column.height - y;
        if depth == 0 {
            return match column.biome {
                Biome::Desert => p.sand,
                Biome::Ocean | Biome::River => {
                    if column.height >= 54 {
                        p.sand
                    } else {
                        p.gravel
                    }
                }
                Biome::Snow => p.snow,
                Biome::Mountains if column.height > 120 => p.stone,
                _ if column.height <= NORMAL_SEA_LEVEL + 1 => p.sand,
                _ => p.grass,
            };
        }
        if depth <= 3 {
            return match column.biome {
                Biome::Desert => {
                    if depth < 3 {
                        p.sand
                    } else {
                        p.sandstone
                    }
                }
                Biome::Ocean | Biome::River => p.gravel,
                Biome::Mountains if column.height > 120 => p.stone,
                _ => p.dirt,
            };
        }
        if let Some(ore) = self.ore(x, y, z) {
            return ore;
        }
        if y < 0 { p.deepslate } else { p.stone }
    }

    fn cave(&self, x: i64, y: i32, z: i64) -> bool {
        let a = self.noise3(x as f64 / 36.0, f64::from(y) / 24.0, z as f64 / 36.0, 11);
        let b = self.noise3(x as f64 / 27.0, f64::from(y) / 19.0, z as f64 / 27.0, 12);
        a * a + b * b < 0.010 || (y < 35 && a > 0.64)
    }

    fn ore(&self, x: i64, y: i32, z: i64) -> Option<u32> {
        let y = i64::from(y);
        // Veins are global spheres, including centers in neighboring lattice cells.
        // Looking only at the current cell would cut a vein at chunk-aligned borders.
        for cell_x in (x - 2).div_euclid(8)..=(x + 2).div_euclid(8) {
            for cell_y in (y - 2).div_euclid(8)..=(y + 2).div_euclid(8) {
                for cell_z in (z - 2).div_euclid(8)..=(z + 2).div_euclid(8) {
                    let cell = hash(self.seed ^ 13, cell_x, cell_y, cell_z);
                    let p = self.palette;
                    let ore = match cell % 1000 {
                        0..=109 if (0..=150).contains(&y) => p.coal_ore,
                        110..=179 if y <= 100 => p.iron_ore,
                        180..=219 if (0..=72).contains(&y) => p.copper_ore,
                        220..=234 if y < 32 => p.gold_ore,
                        235..=259 if y < 0 => p.redstone_ore,
                        260..=269 if y < -16 => p.diamond_ore,
                        _ => continue,
                    };
                    let dx = x - (cell_x * 8 + ((cell >> 16) & 7) as i64);
                    let dy = y - (cell_y * 8 + ((cell >> 19) & 7) as i64);
                    let dz = z - (cell_z * 8 + ((cell >> 22) & 7) as i64);
                    if dx * dx + dy * dy + dz * dz <= 6 {
                        return Some(ore);
                    }
                }
            }
        }
        None
    }

    fn tree(&self, cell_x: i64, cell_z: i64) -> Option<Tree> {
        let bits = hash(self.seed ^ 14, cell_x, 0, cell_z);
        let x = cell_x * 6 + 1 + ((bits >> 8) % 4) as i64;
        let z = cell_z * 6 + 1 + ((bits >> 16) % 4) as i64;
        let column = self.column(x, z);
        let chance = match column.biome {
            Biome::Forest => 62,
            Biome::Plains => 7,
            Biome::Snow if column.height < 139 => 28,
            _ => return None,
        };
        if bits % 100 >= chance || column.height <= NORMAL_SEA_LEVEL + 1 {
            return None;
        }
        if [(x - 2, z), (x + 2, z), (x, z - 2), (x, z + 2)]
            .iter()
            .any(|&(x, z)| (self.column(x, z).height - column.height).abs() > 3)
        {
            return None;
        }
        let spruce = column.biome == Biome::Snow;
        Some(Tree {
            x,
            z,
            ground: column.height,
            height: if spruce {
                7 + ((bits >> 24) % 3) as i32
            } else {
                4 + ((bits >> 24) % 3) as i32
            },
            spruce,
        })
    }

    fn place_tree(&self, blocks: &mut [u32], origin_x: i64, origin_z: i64, tree: Tree) {
        let p = self.palette;
        let (log, leaves) = if tree.spruce {
            (p.spruce_log, p.spruce_leaves)
        } else {
            (p.oak_log, p.oak_leaves)
        };
        let mut put = |x: i64, y: i32, z: i64, state: u32, trunk: bool| {
            let x = x - origin_x;
            let z = z - origin_z;
            if !(0..16).contains(&x)
                || !(0..16).contains(&z)
                || !(MIN_Y..MIN_Y + WORLD_HEIGHT as i32).contains(&y)
            {
                return;
            }
            let index = (y - MIN_Y) as usize * 256 + z as usize * 16 + x as usize;
            if blocks[index] == p.air
                || (trunk && (blocks[index] == p.oak_leaves || blocks[index] == p.spruce_leaves))
            {
                blocks[index] = state;
            }
        };
        for y in 1..=tree.height {
            put(tree.x, tree.ground + y, tree.z, log, true);
        }
        let top = tree.ground + tree.height;
        let layers = if tree.spruce { 6 } else { 3 };
        for down in 0..=layers {
            let radius: i64 = if tree.spruce {
                (1 + down / 2).min(3)
            } else if down >= 1 {
                2
            } else {
                1
            };
            for dx in -radius..=radius {
                for dz in -radius..=radius {
                    if dx.abs() == radius && dz.abs() == radius && radius > 1 {
                        continue;
                    }
                    put(
                        tree.x + dx,
                        top + 1 - down as i32,
                        tree.z + dz,
                        leaves,
                        false,
                    );
                }
            }
        }
    }

    fn noise2(&self, x: i64, z: i64, scale: f64, salt: u64) -> f64 {
        self.noise3(x as f64 / scale, 0.0, z as f64 / scale, salt)
    }

    fn noise3(&self, x: f64, y: f64, z: f64, salt: u64) -> f64 {
        let ix = x.floor() as i64;
        let iy = y.floor() as i64;
        let iz = z.floor() as i64;
        let tx = smooth(x - ix as f64);
        let ty = smooth(y - iy as f64);
        let tz = smooth(z - iz as f64);
        let value = |dx, dy, dz| {
            let bits = hash(
                self.seed
                    .wrapping_add(salt.wrapping_mul(0x9e3779b97f4a7c15)),
                ix + dx,
                iy + dy,
                iz + dz,
            );
            (bits >> 11) as f64 * (2.0 / ((1_u64 << 53) as f64)) - 1.0
        };
        let low = lerp(
            lerp(value(0, 0, 0), value(1, 0, 0), tx),
            lerp(value(0, 0, 1), value(1, 0, 1), tx),
            tz,
        );
        if ty == 0.0 {
            return low;
        }
        let high = lerp(
            lerp(value(0, 1, 0), value(1, 1, 0), tx),
            lerp(value(0, 1, 1), value(1, 1, 1), tx),
            tz,
        );
        lerp(low, high, ty)
    }
}

fn smooth(value: f64) -> f64 {
    value * value * (3.0 - 2.0 * value)
}
fn lerp(a: f64, b: f64, value: f64) -> f64 {
    a + (b - a) * value
}
fn hash(seed: u64, x: i64, y: i64, z: i64) -> u64 {
    let mut value = seed
        ^ (x as u64).wrapping_mul(0x9e3779b97f4a7c15)
        ^ (y as u64).wrapping_mul(0xbf58476d1ce4e5b9)
        ^ (z as u64).wrapping_mul(0x94d049bb133111eb);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> NormalPalette {
        NormalPalette {
            air: 0,
            bedrock: 1,
            stone: 2,
            deepslate: 3,
            dirt: 4,
            grass: 5,
            sand: 6,
            sandstone: 7,
            gravel: 8,
            water: 9,
            lava: 10,
            snow: 11,
            oak_log: 12,
            oak_leaves: 13,
            spruce_log: 14,
            spruce_leaves: 15,
            coal_ore: 16,
            iron_ore: 17,
            copper_ore: 18,
            gold_ore: 19,
            redstone_ore: 20,
            diamond_ore: 21,
            plains: 0,
            forest: 1,
            desert: 2,
            snowy_plains: 3,
            ocean: 4,
            river: 5,
            mountains: 6,
        }
    }

    #[test]
    fn deterministic_terrain_and_tree_canopies_cross_negative_chunk_edges() {
        let generator = NormalGenerator::new(1337, palette());
        let first = generator.generate(-1, 0);
        assert_eq!(first.to_bytes(), generator.generate(-1, 0).to_bytes());
        let second = generator.generate(0, 0);
        for z in 0..16 {
            let left = generator.surface_height(-1, z);
            let right = generator.surface_height(0, z);
            assert!((left - right).abs() <= 8);
            assert_ne!(first.get_block(15, left, z as u8), Some(palette().air));
            assert_ne!(second.get_block(0, right, z as u8), Some(palette().air));
            assert_eq!(first.get_block(15, MIN_Y, z as u8), Some(palette().bedrock));
        }
        assert!(first.sections()[crate::SECTION_COUNT - 1].uniform_state() == Some(palette().air));
        assert!(first.heap_bytes() < WORLD_HEIGHT * 256 * 4 / 3);
        assert!(
            (-128..=128)
                .step_by(8)
                .any(|x| generator.surface_height(x, 0)
                    != NormalGenerator::new(1338, palette()).surface_height(x, 0))
        );
        let tree = (-50..=0)
            .flat_map(|x| (-50..=50).map(move |z| (x, z)))
            .filter_map(|(x, z)| generator.tree(x, z))
            .find(|tree| tree.x.rem_euclid(16) == 14)
            .expect("bounded region has a crown crossing a negative chunk edge");
        let left_x = tree.x.div_euclid(16) as i32;
        let chunk_z = tree.z.div_euclid(16) as i32;
        let left = generator.generate(left_x, chunk_z);
        let right = generator.generate(left_x + 1, chunk_z);
        let expected = if tree.spruce {
            palette().spruce_leaves
        } else {
            palette().oak_leaves
        };
        let y = tree.ground + tree.height - 1;
        let z = tree.z.rem_euclid(16) as u8;
        assert_eq!(left.get_block(15, y, z), Some(expected));
        assert_eq!(right.get_block(0, y, z), Some(expected));
    }
}
