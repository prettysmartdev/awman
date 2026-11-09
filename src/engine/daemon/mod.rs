//! Single daemon process, shared core, fair executor and feature supervision.
pub mod core;
pub mod executor;
pub mod feature;
pub mod feature_supervisor;
pub mod guard;
pub mod supervisor;
pub use guard::{AcquireError, DaemonClaim, DaemonGuard};
pub use supervisor::{DaemonSupervisor, Termination};

// WI 0142: the `engine` daemon feature (Layer 1 lifecycle + the `EngineHost` hook Layer 2 implements).
pub mod features;
