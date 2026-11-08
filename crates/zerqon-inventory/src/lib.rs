mod catalog;
mod crafting;
mod inventory;
mod types;
mod wire;

pub use catalog::{ComponentDef, ItemCatalog, ItemDef};
pub use crafting::*;
pub use inventory::*;
pub use types::*;
pub use wire::*;

#[derive(Debug, thiserror::Error)]
pub enum ItemError {
    #[error("invalid inventory data: {0}")]
    Invalid(&'static str),
    #[error("inventory resource limit exceeded: {0}")]
    Limit(&'static str),
    #[error("unsupported item component: {0}")]
    UnsupportedComponent(String),
    #[error("unknown item ID: {0}")]
    UnknownItem(u32),
    #[error("unknown component ID: {0}")]
    UnknownComponent(u32),
    #[error(transparent)]
    Wire(#[from] zerqon_protocol::codec::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
