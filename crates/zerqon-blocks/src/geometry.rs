#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl Position {
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    pub fn relative(self, direction: Direction) -> Option<Self> {
        let [x, y, z] = direction.offset();
        Some(Self {
            x: self.x.checked_add(x)?,
            y: self.y.checked_add(y)?,
            z: self.z.checked_add(z)?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Down,
    Up,
    North,
    South,
    West,
    East,
}

impl Direction {
    pub const ALL: [Self; 6] = [
        Self::Down,
        Self::Up,
        Self::North,
        Self::South,
        Self::West,
        Self::East,
    ];
    pub const HORIZONTAL: [Self; 4] = [Self::North, Self::South, Self::West, Self::East];

    pub fn from_id(id: u8) -> Option<Self> {
        Self::ALL.get(usize::from(id)).copied()
    }

    pub fn horizontal(self) -> bool {
        !matches!(self, Self::Up | Self::Down)
    }

    pub fn opposite(self) -> Self {
        match self {
            Self::Down => Self::Up,
            Self::Up => Self::Down,
            Self::North => Self::South,
            Self::South => Self::North,
            Self::West => Self::East,
            Self::East => Self::West,
        }
    }

    pub fn counter_clockwise(self) -> Option<Self> {
        match self {
            Self::North => Some(Self::West),
            Self::West => Some(Self::South),
            Self::South => Some(Self::East),
            Self::East => Some(Self::North),
            _ => None,
        }
    }

    pub fn axis(self) -> &'static str {
        match self {
            Self::Down | Self::Up => "y",
            Self::North | Self::South => "z",
            Self::West | Self::East => "x",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Up => "up",
            Self::North => "north",
            Self::South => "south",
            Self::West => "west",
            Self::East => "east",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|direction| direction.name() == value)
    }

    fn offset(self) -> [i32; 3] {
        match self {
            Self::Down => [0, -1, 0],
            Self::Up => [0, 1, 0],
            Self::North => [0, 0, -1],
            Self::South => [0, 0, 1],
            Self::West => [-1, 0, 0],
            Self::East => [1, 0, 0],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

impl Aabb {
    pub fn valid(self) -> bool {
        (0..3).all(|axis| {
            self.min[axis].is_finite()
                && self.max[axis].is_finite()
                && self.min[axis] < self.max[axis]
        })
    }

    pub fn translated(self, position: Position) -> Self {
        let offset = [
            f64::from(position.x),
            f64::from(position.y),
            f64::from(position.z),
        ];
        Self {
            min: std::array::from_fn(|axis| self.min[axis] + offset[axis]),
            max: std::array::from_fn(|axis| self.max[axis] + offset[axis]),
        }
    }

    pub fn intersects(self, other: Self) -> bool {
        (0..3).all(|axis| self.min[axis] < other.max[axis] && self.max[axis] > other.min[axis])
    }
}
