//! Layer 1 daemon features that live in the engine crate.
//!
//! `squad` and `worker` features are Layer 2 wrappers in
//! `command::daemon_runtime::features`; `engine` is a Layer 1 lifecycle
//! primitive because it owns only reusable runtime mechanics (idle tracking,
//! executor share, drain/stop plumbing) and reaches Layer 2 through the
//! [`engine::EngineHost`] trait it defines (WI 0142 Part A §1).
pub mod engine;
