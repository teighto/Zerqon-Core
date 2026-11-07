mod catalog;
mod geometry;
mod placement;

pub use catalog::{BlockKind, Catalog, Item, State, StateId};
pub use geometry::{Aabb, Direction, Position};
pub use placement::{BlockChange, BlockView, Bounds, BuildError, EditPlan, Interaction, Placement};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid block metadata: {0}")]
    Data(&'static str),
    #[error("unknown block state: {0}")]
    UnknownState(u32),
    #[error("unknown item: {0}")]
    UnknownItem(u32),
    #[error("unknown block or invalid block properties")]
    Properties,
    #[error("dynamic block collision is unsupported")]
    DynamicCollision,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_official_catalog_loads_without_external_runtime() {
        assert!(super::Catalog::embedded().is_ok());
    }
}
