//! Shared context storage and attempt-pinned command records (Layer 0).
pub mod file;
pub mod invocation_pin;
pub mod key;
pub mod path;
pub mod snapshot;
pub mod staging;
pub mod store;
pub use file::ContextFileMeta;
pub use invocation_pin::{CommandInvocationPins, InvocationPin};
pub use key::{ContextKey, ContextKeyError};
pub use path::{ContextPathError, ContextRelPath};
pub use snapshot::{ChangeKind, CommandContextRecord, ContextChange, ContextSnapshot};
pub use staging::{StagedFile, StagingError};
pub use store::{ContextLimits, ContextStoreError, Precondition, RepoContextStore, WriteOutcome};
