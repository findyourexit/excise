mod arena;
mod error;
mod identity_store;
mod memory;
mod types;

pub use arena::Arena;
pub use error::ModelError;
#[cfg(any(feature = "fuzzing", feature = "internal"))]
pub use identity_store::{IdentityRecord, IdentityStore, SESSION_PREFIX};
pub use memory::{DEFAULT_PROCESS_MIB, MIN_PROCESS_MIB, MemoryBudget, detected_memory_limit_mib};
pub use types::{
    ByteBounds, EntrySnapshot, Node, NodeId, NodeKind, NodeMetrics, NodeState, SyntheticKind,
    UnscannedReason,
};
