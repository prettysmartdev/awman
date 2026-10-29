//! Filesystem and database concerns for awman.
//!
//! Every direct file or database access in Layer 0 is encapsulated in a typed
//! object here. Higher layers consume these types; they never call
//! `std::fs::*` or `rusqlite::*` directly.

pub mod agent_dirs;
pub mod agent_trust;
pub mod api_command_log;
pub mod api_db;
pub mod api_labels;
pub mod api_paths;
pub mod auth_paths;
pub mod command_outputs;
pub mod config_file;
pub mod context_dirs;
pub mod daemon_env;
pub mod daemon_paths;
pub mod daemon_process;
pub mod data_paths;
pub mod git_metadata;
pub mod hash;
pub mod kit_paths;
pub mod log_dirs;
pub mod overlay_paths;
pub mod panic_log;
pub mod path_guard;
pub mod private_files;
pub mod remote_slug;
pub mod repo_file;
pub mod skill_dirs;
pub mod skill_library;
pub mod squad_paths;
pub mod squad_run_paths;
pub mod squad_verdict;
pub mod task_store;
pub mod workflow_dirs;

pub use agent_dirs::{AgentDirs, DefinitionScope};
pub use api_db::{CommandRecord, SessionRecord, SqliteSessionStore};
pub use api_paths::ApiPaths;
pub use auth_paths::{AgentAuthPaths, AuthPathResolver};
pub use context_dirs::ContextDirResolver;
pub use daemon_env::{
    DaemonEnvStore, EnvPersistence, EnvPersistenceSetting, FallbackReason, NoStore,
};
pub use daemon_paths::DaemonPaths;
pub use daemon_process::{DaemonProcess, ServerMeta};
pub use data_paths::DataPaths;
pub use kit_paths::SandboxKitPaths;
pub use log_dirs::WorkflowLogPaths;
pub use overlay_paths::OverlayPathResolver;
pub use panic_log::PanicLog;
pub use skill_dirs::{SkillDirs, SKILL_INTERVIEW_CONTAINER_DIR};
pub use squad_paths::{SharedSquadRunLog, SquadPaths, SquadRunLog, SquadRunLogError, SquadRunLogs};
pub use squad_run_paths::SquadRunPaths;
pub use squad_verdict::{
    read_verdict, verdict_path, RunVerdict, VerdictError, RUN_DIR_CONTAINER_PATH, VERDICT_FILE_NAME,
};
pub use task_store::{
    MountScope, Run, RunDetail, RunId, RunStatus, Task, TaskStatus, TaskStore, TaskWorkspace,
};
pub use workflow_dirs::WorkflowDirs;
pub mod phase_tools_cache;
pub mod prerequisite_grants;
pub mod ssh_keys;
