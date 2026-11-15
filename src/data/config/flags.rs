//! Typed flag values shared across the layered architecture.
//!
//! Frontends (Layer 3) parse user input into `FlagConfig` and pass it down
//! through Layer 2 to Layer 0 / Layer 1. The concrete `clap` definitions live
//! in Layer 2's `Dispatch`; this file only models the shape.

use std::path::PathBuf;

/// Flag-derived overrides that Layer 0 honours when computing the effective config.
///
/// Every field is `None` when the user did not pass the corresponding flag.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FlagConfig {
    /// `--leader agent::model`, resolved as the dynamic-workflow Flag source.
    pub leader: Option<String>,
    pub prerequisites: PrerequisiteFlags,
    pub squad_prerequisites: bool,
    pub squad_task_prerequisites: Option<super::workflows::PrerequisitePolicy>,
    /// Override the working directory.
    pub working_dir: Option<PathBuf>,
    /// Override the agent name (`--agent <name>`).
    pub agent: Option<String>,
    /// Override the model (`--model <name>`).
    pub model: Option<String>,
    /// `--yolo`: enable yolo mode.
    pub yolo: Option<bool>,
    /// `--auto`: enable auto-advance for workflows.
    pub auto: Option<bool>,
    /// `--headless`: run without prompts.
    pub headless: Option<bool>,
    /// `--overlay <…>`: raw overlay specifications (parsed in higher layers).
    pub overlays_raw: Option<Vec<String>>,
    /// `--remote-session <id>`.
    pub remote_session: Option<String>,
    /// Raw `--work-item <reference>`, resolved by the command.
    pub work_item: Option<String>,
    /// `--agent-flag <ARG>` (repeatable): extra arguments for the agent this
    /// invocation launches. Replaces that agent's `agents.flags` config entry.
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
