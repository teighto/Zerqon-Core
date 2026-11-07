use crate::{Aabb, BlockKind, Catalog, Direction, Error, Position, State, StateId};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub trait BlockView {
    /// Read a stable snapshot; unavailable chunks must return `None`.
    fn state_at(&self, position: Position) -> Option<StateId>;
}

impl<F: Fn(Position) -> Option<StateId>> BlockView for F {
    fn state_at(&self, position: Position) -> Option<StateId> {
        self(position)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    pub min: Position,
    pub max: Position,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            min: Position::new(-30_000_000, -64, -30_000_000),
            max: Position::new(30_000_000, 319, 30_000_000),
        }
    }
}

pub struct Interaction<'a> {
    pub eye: [f64; 3],
    pub block_range: f64,
    pub creative: bool,
    pub bounds: Bounds,
    pub blockers: &'a [Aabb],
}

pub struct Placement<'a> {
    pub clicked: Position,
    pub face: Direction,
    pub local_hit: [f32; 3],
    pub facing: Direction,
    pub item: u32,
    pub secondary_use: bool,
    pub properties: &'a BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockChange {
    pub position: Position,
    pub expected: StateId,
    pub replacement: StateId,
}

#[derive(Debug)]
pub struct EditPlan {
    reads: Vec<(Position, StateId)>,
    changes: Vec<BlockChange>,
}

impl EditPlan {
    /// Every value must still match when the caller commits under world locks.
    pub fn reads(&self) -> &[(Position, StateId)] {
        &self.reads
    }
    pub fn changes(&self) -> &[BlockChange] {
        &self.changes
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("invalid block interaction")]
    Invalid,
    #[error("survival block interactions are not implemented")]
    Survival,
    #[error("block is outside interaction range")]
    OutOfReach,
    #[error("block is outside world bounds")]
    OutOfBounds,
    #[error("required block is not loaded: {0:?}")]
    Missing(Position),
    #[error("block placement is not implemented for this item")]
    UnsupportedItem,
    #[error("block behavior is not implemented at {0:?}")]
    UnsupportedBlock(Position),
    #[error("block interaction is not implemented at {0:?}")]
    Interactive(Position),
    #[error("fluid block updates are not implemented at {0:?}")]
    Fluid(Position),
    #[error("block cannot be replaced at {0:?}")]
    Occupied(Position),
    #[error("placement intersects an entity at {0:?}")]
    Obstructed(Position),
    #[error("held item cannot destroy blocks in creative mode")]
    ToolDenied,
    #[error("block-state component effect is not implemented")]
    UnsupportedProperty,
    #[error(transparent)]
    Data(#[from] Error),
}

impl Catalog {
    /// Compute creative edits from server-owned inventory and interaction state.
    /// The caller must validate authorization, actor and inventory changes again before commit.
    pub fn plan_placement(
        &self,
        view: &impl BlockView,
        interaction: &Interaction<'_>,
        request: &Placement<'_>,
    ) -> Result<EditPlan, BuildError> {
        interaction.validate(request.clicked)?;
        if !request.facing.horizontal()
            || request
                .local_hit
                .iter()
                .any(|value| !value.is_finite() || (f64::from(*value) - 0.5).abs() >= 1.0000001)
            || request.properties.len() > 16
        {
            return Err(BuildError::Invalid);
        }
        let item = self.item(request.item)?;
        let default = self
            .item_block(request.item)?
            .filter(|_| item.ordinary_block_item)
            .ok_or(BuildError::UnsupportedItem)?;
        let kind = self.state(default)?.kind();
        if kind == BlockKind::Unsupported {
            return Err(BuildError::UnsupportedItem);
        }
        let mut reads = Reads::new(self, view, interaction.bounds);
        let clicked = reads.get(request.clicked)?;
        dry(clicked, request.clicked)?;
        if clicked.interactive() && !request.secondary_use {
            return Err(BuildError::Interactive(request.clicked));
        }
        let target = if can_replace(clicked, request, true)? {
            request.clicked
        } else {
            request
                .clicked
                .relative(request.face)
                .ok_or(BuildError::OutOfBounds)?
        };
        if !interaction.bounds.contains(target) {
            return Err(BuildError::OutOfBounds);
        }
        let previous = reads.get(target)?;
        dry(previous, target)?;
        if !can_replace(previous, request, target == request.clicked)? {
            return Err(BuildError::Occupied(target));
        }
        mutation_supported(previous, target)?;
        let top = request.face == Direction::Down
            || (request.face.horizontal()
                && f64::from(request.clicked.y) + f64::from(request.local_hit[1])
                    - f64::from(target.y)
                    > 0.5);
        let mut replacement = match kind {
            BlockKind::Simple => default,
            BlockKind::Pillar => self.with_properties(default, [("axis", request.face.axis())])?,
            BlockKind::Slab => {
                let slab_type = if previous.key() == self.state(default)?.key() {
                    "double"
                } else if top {
                    "top"
                } else {
                    "bottom"
                };
                self.with_properties(default, [("type", slab_type), ("waterlogged", "false")])?
            }
            BlockKind::Stairs => self.with_properties(
                default,
                [
                    ("facing", request.facing.name()),
                    ("half", if top { "top" } else { "bottom" }),
                    ("waterlogged", "false"),
                ],
            )?,
            BlockKind::SnowyGround => {
                let above = reads.get(
                    target
                        .relative(Direction::Up)
                        .ok_or(BuildError::OutOfBounds)?,
                )?;
                self.snowy_neighbor_state(default, Direction::Up, above.id())?
            }
            BlockKind::Unsupported => return Err(BuildError::UnsupportedItem),
        };
        if request
            .properties
            .iter()
            .any(|(key, value)| key == "shape" || (key == "waterlogged" && value != "false"))
        {
            return Err(BuildError::UnsupportedProperty);
        }
        replacement = self.with_properties(
            replacement,
            request
                .properties
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )?;
        mutation_supported(self.state(replacement)?, target)?;
        reads.set(target, replacement)?;
        if kind == BlockKind::Stairs {
            replacement = stairs(&mut reads, target, replacement)?;
            reads.set(target, replacement)?;
        }
        update_neighbors(&mut reads, target)?;
        let plan = reads.finish();
        self.validate_collision(&plan, interaction.blockers)?;
        Ok(plan)
    }

    pub fn plan_break(
        &self,
        view: &impl BlockView,
        interaction: &Interaction<'_>,
        position: Position,
        held_item: Option<u32>,
    ) -> Result<EditPlan, BuildError> {
        interaction.validate(position)?;
        if let Some(item) = held_item {
            match self.item(item)?.can_destroy_in_creative {
                Some(true) => {}
                Some(false) => return Err(BuildError::ToolDenied),
                None => return Err(BuildError::UnsupportedItem),
            }
        }
        let mut reads = Reads::new(self, view, interaction.bounds);
        let previous = reads.get(position)?;
        mutation_supported(previous, position)?;
        if previous.is_air() {
            return Ok(reads.finish());
        }
        reads.set(position, self.air())?;
        update_neighbors(&mut reads, position)?;
        let plan = reads.finish();
        self.validate_collision(&plan, interaction.blockers)?;
        Ok(plan)
    }

    pub fn stair_state(
        &self,
        view: &impl BlockView,
        position: Position,
        state: StateId,
    ) -> Result<StateId, BuildError> {
        let bounds = Bounds {
            min: Position::new(-30_000_000, -2048, -30_000_000),
            max: Position::new(30_000_000, 2047, 30_000_000),
        };
        if !bounds.contains(position) {
            return Err(BuildError::OutOfBounds);
        }
        let mut reads = Reads::new(self, view, bounds);
        stairs(&mut reads, position, state)
    }

    pub fn snowy_neighbor_state(
        &self,
        state: StateId,
        direction: Direction,
        neighbor: StateId,
    ) -> Result<StateId, Error> {
        if self.state(state)?.kind() != BlockKind::SnowyGround {
            return Err(Error::Properties);
        }
        let neighbor = self.state(neighbor)?;
        if direction != Direction::Up {
            return Ok(state);
        }
        self.with_properties(
            state,
            [(
                "snowy",
                if neighbor.snow_cover() {
                    "true"
                } else {
                    "false"
                },
            )],
        )
    }

    fn validate_collision(&self, plan: &EditPlan, blockers: &[Aabb]) -> Result<(), BuildError> {
        for change in plan.changes() {
            let replacement = self.state(change.replacement)?.collision()?;
            if self.state(change.expected)?.collision()? == replacement {
                continue;
            }
            for shape in replacement {
                let bounds = shape.translated(change.position);
                if blockers.iter().any(|blocker| bounds.intersects(*blocker)) {
                    return Err(BuildError::Obstructed(change.position));
                }
            }
        }
        Ok(())
    }
}

impl Bounds {
    fn valid(self) -> bool {
        self.min.x >= -30_000_000
            && self.max.x <= 30_000_000
            && self.min.z >= -30_000_000
            && self.max.z <= 30_000_000
            && self.min.y >= -2048
            && self.max.y <= 2047
            && self.min.x <= self.max.x
            && self.min.y <= self.max.y
            && self.min.z <= self.max.z
    }

    fn contains(self, position: Position) -> bool {
        (self.min.x..=self.max.x).contains(&position.x)
            && (self.min.y..=self.max.y).contains(&position.y)
            && (self.min.z..=self.max.z).contains(&position.z)
    }
}

impl Interaction<'_> {
    fn validate(&self, target: Position) -> Result<(), BuildError> {
        if !self.creative {
            return Err(BuildError::Survival);
        }
        if !self.bounds.valid()
            || !self.block_range.is_finite()
            || !(0.0..=64.0).contains(&self.block_range)
            || self
                .eye
                .iter()
                .any(|value| !value.is_finite() || value.abs() > 30_000_064.0)
            || self.blockers.len() > 1024
            || self.blockers.iter().any(|bounds| !bounds.valid())
        {
            return Err(BuildError::Invalid);
        }
        if !self.bounds.contains(target) {
            return Err(BuildError::OutOfBounds);
        }
        let min = [
            f64::from(target.x),
            f64::from(target.y),
            f64::from(target.z),
        ];
        let distance: f64 = (0..3)
            .map(|axis| {
                let delta = self.eye[axis] - self.eye[axis].clamp(min[axis], min[axis] + 1.0);
                delta * delta
            })
            .sum();
        if distance >= (self.block_range + 1.0).powi(2) {
            return Err(BuildError::OutOfReach);
        }
        Ok(())
    }
}

struct Reads<'a, V> {
    catalog: &'a Catalog,
    view: &'a V,
    bounds: Bounds,
    original: BTreeMap<Position, StateId>,
    changes: BTreeMap<Position, StateId>,
}

impl<'a, V: BlockView> Reads<'a, V> {
    fn new(catalog: &'a Catalog, view: &'a V, bounds: Bounds) -> Self {
        Self {
            catalog,
            view,
            bounds,
            original: BTreeMap::new(),
            changes: BTreeMap::new(),
        }
    }

    fn get(&mut self, position: Position) -> Result<State<'a>, BuildError> {
        if position.x.unsigned_abs() > 30_000_000 || position.z.unsigned_abs() > 30_000_000 {
            return Err(BuildError::OutOfBounds);
        }
        if position.y < self.bounds.min.y || position.y > self.bounds.max.y {
            return Ok(self.catalog.state(self.catalog.air())?);
        }
        let value = if let Some(state) = self
            .changes
            .get(&position)
            .or_else(|| self.original.get(&position))
        {
            *state
        } else {
            if self.original.len() >= 64 {
                return Err(BuildError::Invalid);
            }
            let value = self
                .view
                .state_at(position)
                .ok_or(BuildError::Missing(position))?;
            self.original.insert(position, value);
            value
        };
        Ok(self.catalog.state(value)?)
    }

    fn set(&mut self, position: Position, state: StateId) -> Result<(), BuildError> {
        if !self.bounds.contains(position) {
            return Err(BuildError::OutOfBounds);
        }
        self.get(position)?;
        self.changes.insert(position, state);
        Ok(())
    }

    fn finish(self) -> EditPlan {
        let changes = self
            .changes
            .into_iter()
            .filter_map(|(position, replacement)| {
                let expected = *self.original.get(&position)?;
                (expected != replacement).then_some(BlockChange {
                    position,
                    expected,
                    replacement,
                })
            })
            .collect();
        EditPlan {
            reads: self.original.into_iter().collect(),
            changes,
        }
    }
}

fn dry(state: State<'_>, position: Position) -> Result<(), BuildError> {
    if state.has_fluid() {
        return Err(BuildError::Fluid(position));
    }
    if state.dynamic_collision() {
        return Err(BuildError::UnsupportedBlock(position));
    }
    Ok(())
}

fn mutation_supported(state: State<'_>, position: Position) -> Result<(), BuildError> {
    dry(state, position)?;
    if !state.is_air()
        && (state.kind() == BlockKind::Unsupported
            || state.has_block_entity()
            || state.game_master())
    {
        return Err(BuildError::UnsupportedBlock(position));
    }
    Ok(())
}

fn can_replace(
    state: State<'_>,
    request: &Placement<'_>,
    replacing_clicked: bool,
) -> Result<bool, BuildError> {
    if state.kind() == BlockKind::Slab {
        if state.item() != request.item || state.property("type") == Some("double") {
            return Ok(false);
        }
        if !replacing_clicked {
            return Ok(true);
        }
        let above = request.local_hit[1] > 0.5;
        return Ok(if state.property("type") == Some("bottom") {
            request.face == Direction::Up || (request.face.horizontal() && above)
        } else {
            request.face == Direction::Down || (request.face.horizontal() && !above)
        });
    }
    if state.specialized_replacement() {
        return Err(BuildError::UnsupportedBlock(request.clicked));
    }
    Ok(state.replaceable() && state.item() != request.item)
}

fn update_neighbors<V: BlockView>(
    reads: &mut Reads<'_, V>,
    position: Position,
) -> Result<(), BuildError> {
    let mut pending = VecDeque::from([position]);
    let mut visited = BTreeSet::new();
    while let Some(position) = pending.pop_front() {
        if !visited.insert(position) {
            continue;
        }
        for direction in Direction::ALL {
            let neighbor = position
                .relative(direction)
                .ok_or(BuildError::OutOfBounds)?;
            let state = reads.get(neighbor)?;
            dry(state, neighbor)?;
            if state.kind() == BlockKind::Unsupported
                && !state.is_air()
                && (state.neighbor_effects() || state.has_block_entity())
            {
                return Err(BuildError::UnsupportedBlock(neighbor));
            }
            if direction.horizontal() && state.kind() == BlockKind::Stairs {
                let updated = stairs(reads, neighbor, state.id())?;
                if updated != state.id() {
                    reads.set(neighbor, updated)?;
                    pending.push_back(neighbor);
                }
            }
            if state.kind() == BlockKind::SnowyGround {
                let changed = reads.get(position)?;
                let updated = reads.catalog.snowy_neighbor_state(
                    state.id(),
                    direction.opposite(),
                    changed.id(),
                )?;
                if updated != state.id() {
                    reads.set(neighbor, updated)?;
                    pending.push_back(neighbor);
                }
            }
        }
    }
    Ok(())
}

fn stairs<V: BlockView>(
    reads: &mut Reads<'_, V>,
    position: Position,
    id: StateId,
) -> Result<StateId, BuildError> {
    let state = reads.catalog.state(id)?;
    if !state.stair_like() {
        return Err(BuildError::UnsupportedBlock(position));
    }
    let facing = state
        .property("facing")
        .and_then(Direction::parse)
        .filter(|direction| direction.horizontal())
        .ok_or(Error::Properties)?;
    let half = state.property("half").ok_or(Error::Properties)?;
    for (front, direction) in [(true, facing), (false, facing.opposite())] {
        let neighbor = reads.get(
            position
                .relative(direction)
                .ok_or(BuildError::OutOfBounds)?,
        )?;
        if !neighbor.stair_like() || neighbor.property("half") != Some(half) {
            continue;
        }
        let other = neighbor
            .property("facing")
            .and_then(Direction::parse)
            .ok_or(Error::Properties)?;
        if other.axis() == facing.axis() {
            continue;
        }
        let side = if front { other.opposite() } else { other };
        let blocker = reads.get(position.relative(side).ok_or(BuildError::OutOfBounds)?)?;
        if blocker.stair_like()
            && blocker.property("facing") == Some(facing.name())
            && blocker.property("half") == Some(half)
        {
            continue;
        }
        let left = Some(other) == facing.counter_clockwise();
        let shape = match (front, left) {
            (true, true) => "outer_left",
            (true, false) => "outer_right",
            (false, true) => "inner_left",
            (false, false) => "inner_right",
        };
        return Ok(reads.catalog.with_properties(id, [("shape", shape)])?);
    }
    Ok(reads.catalog.with_properties(id, [("shape", "straight")])?)
}
