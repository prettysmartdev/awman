//! Filesystem and database concerns for awman.
//!
//! Every direct file or database access in Layer 0 is encapsulated in a typed
//! object here. Higher layers consume these types; they never call
//! `std::fs::*` or `rusqlite::*` directly.

pub mod api_db;
pub mod api_paths;
pub mod api_process;
pub mod auth_paths;
pub mod overlay_paths;
pub mod skill_dirs;
pub mod workflow_dirs;
pub mod workflow_state;

pub use api_db::{CommandRecord, SessionRecord, SqliteSessionStore};
pub use api_paths::ApiPaths;
pub use auth_paths::{AgentAuthPaths, AuthPathResolver};
pub use overlay_paths::OverlayPathResolver;
pub use skill_dirs::SkillDirs;
pub use workflow_dirs::WorkflowDirs;
pub use workflow_state::WorkflowStateStore;
