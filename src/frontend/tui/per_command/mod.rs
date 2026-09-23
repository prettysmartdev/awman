//! Per-command frontend trait implementations for the TUI.
//!
//! Each file implements a single per-command frontend trait on
//! `TuiCommandFrontend`, following the same pattern as
//! `src/frontend/cli/per_command/`.

mod acp_frontend;
mod agent_auth;
mod agent_setup;
mod api_server;
mod chat;
mod clean;
mod config;
mod container_frontend;
mod exec_prompt;
mod exec_workflow;
mod init;
mod mount_scope;
mod new;
mod ready;
mod remote_frontend;
mod specs;
mod squad;
mod status;
mod workflow_frontend;
mod worktree_lifecycle;

pub use acp_frontend::{AcpPromptReceiver, AcpPromptSender, TuiAcpFrontend};
pub use container_frontend::TuiContainerProxy;
pub(crate) use squad::key_setup_dialog;
