//! Typed `SandboxOption` enum and the resolved option bag the sandbox
//! backend consumes.
//!
//! Mirrors the container tier's `ContainerOption` / `ResolvedContainerOptions`
//! pattern with sandbox-paradigm-appropriate fields: no arbitrary host
//! mounts, no CPU enforcement (recorded but unused), kit/template selection
//! instead of image refs.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::engine::container::options::{
    Entrypoint, EnvLiteral, EnvVar, ModelFlagForm, OverlaySpec,
};

/// Every knob a sandbox-class agent accepts. Adding a new option is a single
/// variant and a single branch in `ResolvedSandboxOptions::ingest`.
#[derive(Debug, Clone, PartialEq)]
pub enum SandboxOption {
    /// Kit selector — which agent kit/template the sandbox boots.
    AgentId(String),
    EntrypointOverride(Entrypoint),
    WorkspaceDir(PathBuf),
    ExtraOverlay(OverlaySpec),
    EnvPassthrough(EnvVar),
    EnvLiteral(EnvLiteral),
    SeededPrompt(String),
    Interactive(bool),
    /// Deterministic per (worktree, agent) sandbox name; see WI 0090.
    SandboxName(String),
    MemoryGb(u32),
    /// Recorded but unused; the sandbox capability flags say CPU limits are
    /// unsupported.
    CpuLimit(f64),
    /// Serialized into session.json by WI 0090.
    AgentSetting {
        key: String,
        value: serde_json::Value,
    },
    /// Key/value pairs; routed to `sbx secret set` by WI 0090.
    AgentCredentials {
        env_vars: Vec<(String, String)>,
    },
    /// System prompt delivered via a file mount + CLI flag (host, container, flag).
    SystemPromptFile {
        host_path: PathBuf,
        container_path: PathBuf,
        flag: String,
    },
    /// System prompt delivered via an env var pointing to a mounted file.
    SystemPromptEnvFile {
        env_var: String,
        host_path: PathBuf,
        container_path: PathBuf,
    },
    /// System prompt delivered as inline text via a CLI flag.
    SystemPromptInline {
        flag: String,
        text: String,
    },
    DisallowedTools(Vec<String>),
    AllowedTools(Vec<String>),
    Model {
        flag: ModelFlagForm,
    },
    /// Literal per-agent mode-flag argv strings (yolo/auto/plan). Only
    /// deliverable to `kind: agent` kits, whose launch accepts agent args
    /// after the `--` delimiter; mixin kits launch through Docker's
    /// built-in template and use [`SandboxOption::PermissionMode`] instead.
    AgentModeFlags(Vec<String>),
    /// Requested permission mode (`"plan"` / `"auto"` / `"yolo"`) for mixin
    /// kits whose apply script can render a native settings equivalent
    /// (claude's `permissions.defaultMode`). Written to `session.json`.
    PermissionMode(String),
    /// Keep the sandbox after the agent exits (persistent lifecycle).
    KeepAfterExit,
    /// A user-facing note about a requested feature the sandbox runtime cannot
    /// honor (skill mounts, directory overlays, context-dir mounts, …).
    /// Surfaced as a Warning by `run_interactive` before launch, alongside the
    /// CPU-limit and withheld-env warnings. Never written to `session.json`.
    UnsupportedNote(String),
}

/// Resolved option bag — all options merged into a single struct that the
/// sandbox backend consumes.
#[derive(Debug, Clone, Default)]
pub struct ResolvedSandboxOptions {
    /// Kit selector.
    pub agent_id: String,
    pub entrypoint_override: Option<Entrypoint>,
    pub workspace_dir: PathBuf,
    pub extra_overlays: Vec<OverlaySpec>,
    pub env_passthrough: Vec<EnvVar>,
    pub env_literal: Vec<EnvLiteral>,
    pub seeded_prompt: Option<String>,
    pub interactive: bool,
    /// Deterministic per (worktree, agent); see WI 0090.
    pub sandbox_name: Option<String>,
    pub memory_gb: Option<u32>,
    /// Recorded but unused; capability flag says CPU limits unsupported.
    pub cpu_limit: Option<f64>,
    /// Serialized into session.json by WI 0090.
    pub agent_settings: HashMap<String, serde_json::Value>,
    /// Key/value pairs; routed to `sbx secret set` by WI 0090.
    pub agent_credentials: Vec<(String, String)>,
    /// (host, container, flag).
    pub system_prompt_file: Option<(PathBuf, PathBuf, String)>,
    /// (env var, host, container).
    pub system_prompt_env_file: Option<(String, PathBuf, PathBuf)>,
    /// (flag, text).
    pub system_prompt_inline: Option<(String, String)>,
    pub disallowed_tools: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub model: Option<ModelFlagForm>,
    /// Literal mode-flag argv strings; appended after `--` for `kind: agent`
    /// kits only.
    pub mode_flags: Vec<String>,
    /// Permission mode for mixins with a native settings equivalent; lands
    /// in `session.json`.
    pub permission_mode: Option<String>,
    pub keep_after_exit: bool,
    /// User-facing warnings about requested-but-unsupported features, surfaced
    /// by `run_interactive` before launch.
    pub unsupported_notes: Vec<String>,
}

impl ResolvedSandboxOptions {
    pub fn resolve(options: impl IntoIterator<Item = SandboxOption>) -> Self {
        let mut r = Self::default();
        for opt in options {
            r.ingest(opt);
        }
        r
    }

    fn ingest(&mut self, opt: SandboxOption) {
        match opt {
            SandboxOption::AgentId(v) => self.agent_id = v,
            SandboxOption::EntrypointOverride(v) => self.entrypoint_override = Some(v),
            SandboxOption::WorkspaceDir(v) => self.workspace_dir = v,
            SandboxOption::ExtraOverlay(v) => self.extra_overlays.push(v),
            SandboxOption::EnvPassthrough(v) => self.env_passthrough.push(v),
            SandboxOption::EnvLiteral(v) => self.env_literal.push(v),
            SandboxOption::SeededPrompt(v) => self.seeded_prompt = Some(v),
            SandboxOption::Interactive(v) => self.interactive = v,
            SandboxOption::SandboxName(v) => self.sandbox_name = Some(v),
            SandboxOption::MemoryGb(v) => self.memory_gb = Some(v),
            SandboxOption::CpuLimit(v) => self.cpu_limit = Some(v),
            SandboxOption::AgentSetting { key, value } => {
                self.agent_settings.insert(key, value);
            }
            SandboxOption::AgentCredentials { env_vars } => {
                self.agent_credentials.extend(env_vars);
            }
            SandboxOption::SystemPromptFile {
                host_path,
                container_path,
                flag,
            } => {
                self.system_prompt_file = Some((host_path, container_path, flag));
            }
            SandboxOption::SystemPromptEnvFile {
                env_var,
                host_path,
                container_path,
            } => {
                self.system_prompt_env_file = Some((env_var, host_path, container_path));
            }
            SandboxOption::SystemPromptInline { flag, text } => {
                self.system_prompt_inline = Some((flag, text));
            }
            SandboxOption::DisallowedTools(v) => self.disallowed_tools.extend(v),
            SandboxOption::AllowedTools(v) => self.allowed_tools.extend(v),
            SandboxOption::Model { flag } => self.model = Some(flag),
            SandboxOption::AgentModeFlags(v) => self.mode_flags.extend(v),
            SandboxOption::PermissionMode(v) => self.permission_mode = Some(v),
            SandboxOption::KeepAfterExit => self.keep_after_exit = true,
            SandboxOption::UnsupportedNote(v) => self.unsupported_notes.push(v),
        }
    }
}
