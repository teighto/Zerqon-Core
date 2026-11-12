use crate::{
    Result,
    runtime::{CommandSource, State},
};
use std::{collections::HashMap, sync::Mutex};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

pub const PLUGIN_NAME: &str = "ZerqonEdit";
const MAX_BLOCKS: usize = crate::runtime::MAX_EDIT_BLOCKS;
const MAX_CHUNKS: u64 = crate::runtime::MAX_EDIT_CHUNKS as u64;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum Owner {
    Console,
    Player(Uuid, i32),
}

impl From<CommandSource<'_>> for Owner {
    fn from(source: CommandSource<'_>) -> Self {
        match source {
            CommandSource::Console => Self::Console,
            CommandSource::Player {
                uuid, session_id, ..
            } => Self::Player(uuid, session_id),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Selection {
    corners: [Option<[i32; 3]>; 2],
}

#[derive(Clone, Copy)]
struct Region {
    min: [i32; 3],
    max: [i32; 3],
    volume: usize,
}

impl Selection {
    fn region(self) -> Result<Region> {
        let [Some(first), Some(second)] = self.corners else {
            return Err("Select both corners with //pos1 and //pos2".into());
        };
        let min = std::array::from_fn(|axis| first[axis].min(second[axis]));
        let max = std::array::from_fn(|axis| first[axis].max(second[axis]));
        let volume = (0..3)
            .try_fold(1u64, |volume, axis| {
                volume.checked_mul((i64::from(max[axis]) - i64::from(min[axis]) + 1) as u64)
            })
            .ok_or("Selection is too large")?;
        let chunks = (i64::from(max[0].div_euclid(16)) - i64::from(min[0].div_euclid(16)) + 1)
            * (i64::from(max[2].div_euclid(16)) - i64::from(min[2].div_euclid(16)) + 1);
        if volume > MAX_BLOCKS as u64 || chunks > MAX_CHUNKS as i64 {
            return Err("Selection limit: 65536 blocks across at most 16 chunks".into());
        }
        Ok(Region {
            min,
            max,
            volume: volume as usize,
        })
    }
}

impl Region {
    fn positions(self) -> Vec<[i32; 3]> {
        let mut positions = Vec::with_capacity(self.volume);
        for x in self.min[0]..=self.max[0] {
            for z in self.min[2]..=self.max[2] {
                for y in self.min[1]..=self.max[1] {
                    positions.push([x, y, z]);
                }
            }
        }
        positions
    }
}

pub struct WorldEditPlugin {
    selections: Mutex<HashMap<Owner, Selection>>,
    capacity: usize,
    edits: AsyncMutex<()>,
}

impl WorldEditPlugin {
    pub fn new(max_players: usize) -> Self {
        Self {
            selections: Mutex::new(HashMap::new()),
            capacity: max_players.saturating_add(1),
            edits: AsyncMutex::new(()),
        }
    }

    pub fn unregister(&self, uuid: Uuid, session_id: i32) {
        self.selections
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&Owner::Player(uuid, session_id));
    }

    fn selection(&self, source: CommandSource<'_>) -> Result<Selection> {
        self.selections
            .lock()
            .map_err(|_| "Editor selection service unavailable")?
            .get(&Owner::from(source))
            .copied()
            .ok_or_else(|| "Select both corners with //pos1 and //pos2".into())
    }

    pub fn selection_positions(&self, source: CommandSource<'_>) -> Result<Vec<[i32; 3]>> {
        Ok(self.selection(source)?.region()?.positions())
    }

    fn set_corner(
        &self,
        source: CommandSource<'_>,
        corner: usize,
        position: [i32; 3],
    ) -> Result<()> {
        validate_position(position)?;
        let owner = Owner::from(source);
        let mut selections = self
            .selections
            .lock()
            .map_err(|_| "Editor selection service unavailable")?;
        if !selections.contains_key(&owner) && selections.len() >= self.capacity {
            return Err("Editor selection capacity reached".into());
        }
        selections.entry(owner).or_default().corners[corner] = Some(position);
        Ok(())
    }

    pub async fn execute(
        &self,
        state: &State,
        source: CommandSource<'_>,
        command: &str,
        args: &[String],
    ) -> Result<Vec<String>> {
        let player = match source {
            CommandSource::Console => None,
            CommandSource::Player {
                uuid, session_id, ..
            } => Some(
                state
                    .players
                    .query_snapshot(uuid)?
                    .filter(|player| player.id == session_id)
                    .ok_or("Player is no longer connected")?,
            ),
        };
        match command {
            "pos1" | "pos2" => {
                let position = if args.is_empty() {
                    let player = player.ok_or("Console usage: //pos1 x y z or //pos2 x y z")?;
                    player.position.xyz.map(|value| value.floor() as i32)
                } else if args.len() == 3 {
                    [
                        args[0].parse::<i32>()?,
                        args[1].parse::<i32>()?,
                        args[2].parse::<i32>()?,
                    ]
                } else {
                    return Ok(vec![format!("Usage: //{command} [x y z]")]);
                };
                let corner = usize::from(command == "pos2");
                self.set_corner(source, corner, position)?;
                Ok(vec![format!(
                    "Position {}: {} {} {}",
                    corner + 1,
                    position[0],
                    position[1],
                    position[2]
                )])
            }
            "desel" => {
                if !args.is_empty() {
                    return Ok(vec!["Usage: //desel".into()]);
                }
                self.selections
                    .lock()
                    .map_err(|_| "Editor selection service unavailable")?
                    .remove(&Owner::from(source));
                Ok(vec!["Selection cleared".into()])
            }
            "size" => {
                if !args.is_empty() {
                    return Ok(vec!["Usage: //size".into()]);
                }
                let region = self.selection(source)?.region()?;
                Ok(vec![format!(
                    "Selection: {} blocks; {} {} {} to {} {} {}",
                    region.volume,
                    region.min[0],
                    region.min[1],
                    region.min[2],
                    region.max[0],
                    region.max[1],
                    region.max[2]
                )])
            }
            "set" | "replace" => {
                if args.len() != if command == "set" { 1 } else { 2 } {
                    return Ok(vec![
                        if command == "set" {
                            "Usage: //set block"
                        } else {
                            "Usage: //replace from-block to-block"
                        }
                        .into(),
                    ]);
                }
                let data = state.data.as_ref().ok_or("Game data unavailable")?;
                let resolve = |name: &str| -> Result<u32> {
                    let key = if name.contains(':') {
                        name.to_owned()
                    } else {
                        format!("minecraft:{name}")
                    };
                    data.default_block_state(&key)
                        .ok_or_else(|| format!("Unknown block: {name}").into())
                };
                let first = resolve(&args[0])?;
                let replacement = if command == "replace" {
                    resolve(&args[1])?
                } else {
                    first
                };
                let _edit = self
                    .edits
                    .try_lock()
                    .map_err(|_| "Another editor operation is running; retry after it finishes")?;
                let positions = self.selection_positions(source)?;
                let world = state.world.as_ref().ok_or("World unavailable")?;
                let changed = if command == "replace" {
                    let current = world.read_blocks(positions.clone()).await?;
                    let edits = positions
                        .into_iter()
                        .zip(current)
                        .filter_map(|(position, state)| {
                            (state == first).then_some((position, first, replacement))
                        })
                        .collect();
                    world.edit_blocks_if(edits).await?
                } else {
                    let edits = positions
                        .into_iter()
                        .map(|position| (position, replacement))
                        .collect();
                    world.edit_blocks(edits).await?
                };
                Ok(vec![format!("Changed {changed} blocks")])
            }
            _ => Err("Unknown editor command".into()),
        }
    }
}

fn validate_position([x, y, z]: [i32; 3]) -> Result<()> {
    if x.unsigned_abs() > 30_000_000
        || z.unsigned_abs() > 30_000_000
        || !(zerqon_world::MIN_Y..zerqon_world::MIN_Y + zerqon_world::WORLD_HEIGHT as i32)
            .contains(&y)
    {
        return Err("Selection position is outside the world bounds".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selections_are_inclusive_bounded_and_isolated_by_session() {
        let plugin = WorldEditPlugin::new(1);
        let uuid = Uuid::from_u128(1);
        let source = CommandSource::Player {
            uuid,
            session_id: 1,
            name: "teighto",
        };
        plugin.set_corner(source, 0, [-1, 64, -1]).unwrap();
        plugin.set_corner(source, 1, [0, 65, 0]).unwrap();
        let positions = plugin.selection_positions(source).unwrap();
        assert_eq!(positions.len(), 8);
        assert!(positions.contains(&[-1, 64, -1]) && positions.contains(&[0, 65, 0]));
        let reconnected = CommandSource::Player {
            uuid,
            session_id: 2,
            name: "teighto",
        };
        assert!(plugin.selection_positions(reconnected).is_err());
        plugin.unregister(uuid, 2);
        assert_eq!(plugin.selection_positions(source).unwrap().len(), 8);
        plugin
            .set_corner(source, 1, [30_000_000, 319, 30_000_000])
            .unwrap();
        assert!(plugin.selection_positions(source).is_err());
        assert!(plugin.set_corner(source, 1, [0, 320, 0]).is_err());
        plugin.set_corner(source, 0, [0, 64, 0]).unwrap();
        plugin.set_corner(source, 1, [256, 64, 0]).unwrap();
        assert!(
            plugin.selection_positions(source).is_err(),
            "17 chunks exceed the limit despite low volume"
        );
        plugin.set_corner(source, 1, [15, 319, 15]).unwrap();
        assert_eq!(
            plugin.selection_positions(source).unwrap().len(),
            MAX_BLOCKS
        );
        plugin.set_corner(source, 0, [0, 63, 0]).unwrap();
        assert!(plugin.selection_positions(source).is_err());
        plugin.unregister(uuid, 1);
        assert!(plugin.selection_positions(source).is_err());
    }
}
