//! Per-command frontend trait implementations for
//! [`super::frontend::InstanceCommandFrontend`], one module per command, moved
//! from `src/frontend/tui/per_command/` (WI 0142, D2). They push typed dialog
//! requests and feed updates; they never render. Each file follows the same
//! pattern as `src/frontend/cli/per_command/`.

mod acp_frontend;
mod agent_auth;
mod agent_setup;
mod agent_trust;
mod agents;
mod attach;
mod chat;
mod clean;
mod config;
mod container_frontend;
mod daemon;
mod exec_prompt;
mod exec_workflow;
mod init;
mod mount_scope;
mod new;
mod ready;
mod remote_frontend;
mod specs;
mod squad;
mod squad_attach;
mod workflow_frontend;
mod workflow_prerequisites;
mod worktree_lifecycle;

pub use acp_frontend::{AcpPromptReceiver, AcpPromptSender, InstanceAcpFrontend};
pub use container_frontend::InstanceContainerProxy;
