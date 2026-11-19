//! Layer 2 contracts. Shared scenarios accept an explicit backend.

mod admission;
mod agent_permissions;
mod agent_version_policy;
mod automation_admission;
mod automation_observers;
mod builtin;
pub mod catalogue;
mod config;
mod credentials;
mod daemon;
mod differential;
mod draft_sync;
mod edit_artifacts;
mod failures;
mod misplaced_flags;
mod models;
mod native_plan;
mod prepare_only;
mod read_only_workspace;
mod relay;
mod retired_agents;
mod scaffolding;
mod scenarios;
mod secrets;
mod support;
mod wait_failures;
mod work_item_edit;
mod work_item_source;
mod workflow;
mod workflow_context;
mod worktree_naming;

mod native_resume;
mod settings_profiles;

mod credential_rotation;
