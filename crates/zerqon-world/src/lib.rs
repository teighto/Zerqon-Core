pub mod anvil;
mod cache;
mod chunk;
pub mod metadata;
mod section;
mod source_data;
mod storage;
mod transaction;
mod workers;

pub use cache::ChunkCache;
pub use chunk::{Chunk, FlatPalette, MIN_Y, SECTION_COUNT, WORLD_HEIGHT};
pub use metadata::{GenerationPolicy, SpawnPoint, WorldMetadata};
pub use section::{SECTION_VOLUME, Section};
pub use storage::WorldStore;
pub use transaction::{
    ChunkKey, ChunkSnapshot, ChunkWrite, CommitError, CommitReceipt, CommittedChunk,
};
pub use workers::{ChunkTicket, ChunkWorkers};

#[derive(Debug, thiserror::Error)]
pub enum WorldError {
    #[error("world operation cancelled")]
    Cancelled,
    #[error("world storage: {0}")]
    Storage(String),
    #[error("world storage requires reopening after an uncertain commit")]
    StorageUnavailable,
    #[error("world commit {operation} has an uncertain outcome: {cause}")]
    CommitIndeterminate {
        operation: uuid::Uuid,
        cause: String,
    },
    #[error("block coordinates are outside the chunk")]
    OutOfBounds,
    #[error("invalid chunk data: {0}")]
    Corrupt(&'static str),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("world is already open in another process")]
    Locked,
    #[error(
        "a vanilla world is present; import into a new directory instead of opening it as Zerqon storage"
    )]
    ForeignWorld,
    #[error("invalid world metadata: {0}")]
    Metadata(String),
    #[error("imported world has no chunk at {x}, {z}; generation is disabled")]
    MissingImportedChunk { x: i32, z: i32 },
    #[error("invalid worker or cache limits")]
    InvalidLimits,
    #[error("world worker queue is full")]
    QueueFull,
    #[error("world workers have stopped")]
    WorkersStopped,
    #[error("world worker job panicked")]
    WorkerPanicked,
    #[error("world worker result timed out")]
    Timeout,
}
