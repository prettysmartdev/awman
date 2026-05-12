//! `amux` library — Layer 0–3 of the grand architecture refactor.
//!
//! `src/main.rs` (Layer 4) consumes this library to build the user-facing
//! `amux` binary. The four layers exposed below are wired together as
//! described in `aspec/architecture/2026-grand-architecture.md`:
//!
//! - [`data`] (Layer 0) — config, filesystem, session, workflow state.
//! - [`engine`] (Layer 1) — container/git/overlay/auth/agent/workflow engines.
//! - [`command`] (Layer 2) — `*Command` types, `Dispatch`, `CommandCatalogue`.
//! - [`frontend`] (Layer 3) — CLI / TUI / headless presentations of Layer 2.

#![forbid(unsafe_code)]
// Suppress dead-code warnings until oldsrc/ is deleted and the only
// binary entry point is src/main.rs.
#![allow(dead_code)]

pub mod command;
pub mod data;
pub mod engine;
pub mod frontend;

/// Process-global mutex for tests that must change the working directory.
///
/// `std::env::set_current_dir` is process-wide; tests run in parallel and can
/// step on each other when CWD changes aren't serialized. Any test that calls
/// `set_current_dir` MUST hold this lock for the duration of the CWD change
/// and restore the directory before releasing it.
#[cfg(test)]
pub static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
