pub mod attach;
pub mod commands;
pub mod daemon;
pub mod daemon_runtime;
pub mod env_sync;
pub mod evaluation;
pub mod gateway;
pub mod runtime_guard;
pub mod supervisor;

/// The squad key-setup snippet moved to Layer 1 with `SquadSupervisor` (WI
/// 0113 F-02) because `SquadKeyState::Minted` carries the rendered text. It
/// is re-exported here so Layer 2 and Layer 3 keep their existing path.
pub use crate::engine::squad::key_setup;
