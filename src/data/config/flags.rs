//! Typed flag values shared across the layered architecture.
//!
//! Frontends (Layer 3) parse user input into `FlagConfig` and pass it down
//! through Layer 2 to Layer 0 / Layer 1. The concrete `clap` definitions live
//! in Layer 2's `Dispatch`; this file only models the shape.

use std::path::PathBuf;
use std::time::Duration;

use crate::data::config::repo::LaunchMode;

/// Flag-derived overrides that Layer 0 honours when computing the effective config.
///
/// Every field is `None` when the user did not pass the corresponding flag.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FlagConfig {
    pub prerequisites: PrerequisiteFlags,
    pub squad_prerequisites: bool,
    pub squad_task_prerequisites: Option<super::workflows::PrerequisitePolicy>,
    /// Override the working directory.
    pub working_dir: Option<PathBuf>,
    /// Override the agent name (`--agent <name>`).
    pub agent: Option<String>,
    /// Override the model (`--model <name>`).
    pub model: Option<String>,
    /// `--launch-mode <stdio|acp>`.
    pub launch_mode: Option<LaunchMode>,
    /// Override scrollback line count.
    pub terminal_scrollback_lines: Option<usize>,
    /// Override agent-stuck timeout.
    pub agent_stuck_timeout: Option<Duration>,
    /// `--yolo`: enable yolo mode.
    pub yolo: Option<bool>,
    /// `--auto`: enable auto-advance for workflows.
    pub auto: Option<bool>,
    /// `--non-interactive`: force non-interactive behavior.
    pub non_interactive: Option<bool>,
    /// `--yolo-disallowed-tool` / `--yolo-disallowed-tools`: tool denylist for yolo mode.
    pub yolo_disallowed_tools: Option<Vec<String>>,
    /// `--overlay <…>`: raw overlay specifications (parsed in higher layers).
    pub overlays_raw: Option<Vec<String>>,
    /// `--remote-addr <addr>`.
    pub remote_addr: Option<String>,
    /// `--remote-session <id>`.
    pub remote_session: Option<String>,
    /// `--api-key <key>`.
    pub api_key: Option<String>,
    /// `--work-item <N>`.
    pub work_item: Option<u32>,
    /// `--max-concurrent <N>`: cap on concurrently-running workflow steps.
    pub max_concurrent_agents: Option<usize>,
    /// `--agent-flag <ARG>` (repeatable): extra arguments for the agent this
    /// invocation launches. Replaces that agent's `agentFlags` config entry.
    pub agent_flags: Option<Vec<String>>,
}

impl FlagConfig {
    /// Construct an empty flag set (all fields `None`).
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityFlag {
    Host,
    Fixed { name: String, email: String },
    Deny,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshFlag {
    Key(PathBuf),
    All,
    Deny,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenFlag {
    Env(String),
    Acquire,
    Secret(String),
    Deny,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrerequisiteFlags {
    pub identity: Option<IdentityFlag>,
    pub ssh: Option<SshFlag>,
    pub token: Option<TokenFlag>,
    pub unresolved: Option<super::workflows::UnresolvedMode>,
    pub check_only: bool,
}
impl PrerequisiteFlags {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
    pub fn widens(&self) -> Vec<&'static str> {
        let mut flags = vec![];
        if self
            .identity
            .as_ref()
            .is_some_and(|f| !matches!(f, IdentityFlag::Deny))
        {
            flags.push("--prerequisite-identity");
        }
        if self
            .ssh
            .as_ref()
            .is_some_and(|f| !matches!(f, SshFlag::Deny))
        {
            flags.push("--prerequisite-ssh");
        }
        if self
            .token
            .as_ref()
            .is_some_and(|f| !matches!(f, TokenFlag::Deny))
        {
            flags.push("--prerequisite-token");
        }
        flags
    }
}
