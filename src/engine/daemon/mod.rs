//! `engine::daemon` — supervising a long-lived awman daemon process.
//!
//! Layer 1 (WI 0114 F-29, decision Q1). [`DaemonSupervisor`] owns the process
//! half of a daemon — liveness, background spawn, termination — and
//! [`DaemonGuard`] owns the cross-daemon mutual exclusion that depends on it.
//! Layer 0's `data::fs::daemon_process` keeps the on-disk half: PID files,
//! `ServerMeta` sidecars, paths, unit names and plist labels.

pub mod guard;
pub mod supervisor;

pub use guard::{AcquireError, DaemonGuard, DaemonKind};
pub use supervisor::{DaemonSupervisor, Termination};
