#![allow(unused_imports)]
//! Layer 0: data
//!
//! This layer owns every data definition, config concern, filesystem access,
//! and database concern. No business logic, no container interaction, no git
//! operations, no workflow execution, no command logic, and no frontend code
//! is permitted at this layer. See `aspec/architecture/2026-grand-architecture.md`.

pub mod config;
pub mod error;
pub mod execution_event;
pub mod fs;
pub mod image_tags;
pub mod migration;
pub mod network;
pub mod ready_phase;
pub mod ready_summary;
pub mod repo_dockerfile_paths;
pub mod session;
pub mod session_manager;
pub mod session_setup_event;
pub mod step_status;
pub mod templates;
pub mod workflow_dag;
pub mod workflow_definition;
pub mod workflow_prompt_template;
pub mod workflow_state;
pub mod workflow_state_store;
pub mod worktree_paths;

pub use error::DataError;
pub use fs::api_db::{CommandResult, WorkerId};
pub use image_tags::{agent_image_tag, project_image_tag, repo_hash};
pub use repo_dockerfile_paths::RepoDockerfilePaths;
pub use session::{
    AgentName, CommandInvocation, CommandStatus, ContainerHandle, GitRootResolver, Session,
    SessionId, SessionLogEntry, SessionLogKind, SessionState, SessionType, StepStatus,
    WorkflowInvocation, WorkflowStepRecord,
};
pub use session_manager::{InMemorySessionStore, SessionManager, SessionStore};
pub use workflow_dag::{detect_cycle, validate_references, WorkflowDag};
pub use workflow_definition::{detect_format, Workflow, WorkflowFormat, WorkflowStep};
pub use workflow_state::{
    PhaseStepState, PhaseStepStatus, StepState, WorkflowState, WorkflowStepInfo,
    WORKFLOW_STATE_SCHEMA_VERSION,
};
pub use workflow_state_store::WorkflowStateStore as EngineWorkflowStateStore;
pub use worktree_paths::{worktree_branch_name, worktree_branch_name_for_workflow, WorktreePaths};
