//! Filesystem and database concerns for amux.
//!
//! Every direct file or database access in Layer 0 is encapsulated in a typed
//! object here. Higher layers consume these types; they never call
//! `std::fs::*` or `rusqlite::*` directly.

pub mod auth_paths;
pub mod headless_db;
pub mod headless_paths;
pub mod headless_process;
pub mod overlay_paths;
pub mod skill_dirs;
pub mod workflow_dirs;
pub mod workflow_state;

pub use auth_paths::{AgentAuthPaths, AuthPathResolver};
pub use headless_db::{CommandRecord, SessionRecord, SqliteSessionStore};
pub use headless_paths::HeadlessPaths;
pub use overlay_paths::OverlayPathResolver;
pub use skill_dirs::SkillDirs;
pub use workflow_dirs::WorkflowDirs;
pub use workflow_state::WorkflowStateStore;
