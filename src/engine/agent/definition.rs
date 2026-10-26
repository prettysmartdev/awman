//! `AgentDefinition` — the one per-agent description every consumer reads.
//!
//! First-party agents build it in Rust (`first_party.rs`); custom agents
//! convert a validated `.awman/agents/<name>.toml` into the same struct.
//! Option builders, overlays, sbx and launch policy never care which.

use std::path::PathBuf;

pub use crate::data::fs::agent_dirs::DefinitionScope;

use crate::data::config::repo::LaunchMode;
use crate::engine::container::options::{AutoMode, Entrypoint, ModelFlagForm, PlanMode, YoloMode};
use crate::engine::error::EngineError;

/// Where a definition came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentOrigin {
    /// Built into awman (`first_party.rs`).
    FirstParty,
    /// Loaded from a custom TOML definition.
    Custom {
        scope: DefinitionScope,
        path: PathBuf,
        /// Content hash of the definition (the trust key).
        sha256: String,
    },
}

/// The install recipe of a custom agent's rendered Dockerfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSpec {
    pub needs_node: bool,
    pub apt: Vec<String>,
    pub install: Vec<String>,
    /// Binary the rendered Dockerfile smoke-tests.
    pub binary: String,
}

/// How the agent's `.awman/Dockerfile.<name>` is produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentImageSource {
    /// First-party: downloaded from GitHub, falling back to the bundled copy.
    Download,
    /// Custom install steps rendered into the shared template.
    Rendered { spec: ImageSpec },
    /// Custom escape hatch: the user's own Dockerfile (absolute path).
    UserDockerfile { path: PathBuf },
}

/// How awman injects the combined context system prompt into an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemPromptMode {
    /// Append to default system prompt by mounting the prompt as a file and
    /// referencing it with a CLI flag (e.g. claude `--append-system-prompt-file <path>`).
    Append,
    /// Append to default system prompt by passing it inline as `<key>=<text>`
    /// to a CLI flag (e.g. codex `--config developer_instructions=<text>`).
    AppendInline { key: String },
    /// Replace default system prompt with inline text (destructive; preamble
    /// prepended). Used by agents like cline `--system <text>`.
    Replace,
    /// File-based: write AGENTS.md into context dir.
    AgentsMd,
    /// Env var pointing to a file.
    EnvFile { var: String },
    /// Extra workspace dir flag (agy --add-dir) + AGENTS.md.
    AddDir { flag: String },
    /// Not supported.
    Unsupported,
}

/// How awman passes an agent's host settings directory into a container.
///
/// Most agents take the plain `Direct` strategy: mount `$HOME/<path>` at
/// `<container_home>/<path>` when the host directory exists. Claude and
/// antigravity stage a sanitized or keychain-seeded copy instead, and those
/// two strategies stay explicit in `OverlayEngine`. A custom definition can
/// only ever produce `None` or `Direct`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsMount {
    /// No host settings overlay for this agent (copilot).
    None,
    /// Mount `$HOME/<path>` read-write at `<container_home>/<path>` when the
    /// host directory exists.
    Direct(String),
    /// Claude's strategy: a sanitized `~/.claude.json` (synthesized when
    /// absent) plus a sanitized `~/.claude` settings dir that also carries
    /// the refreshable credential file.
    Claude,
    /// Antigravity's strategy: a staged `~/.gemini` seeded with the OAuth
    /// token read from the host keychain, synthesized when the host dir is
    /// absent but a keychain token exists.
    Antigravity,
}

/// Which host credential scheme awman reads for an agent.
///
/// Each variant names one concrete scheme, not a generic shape: adding an
/// agent that authenticates the same way as an existing one means teaching
/// that variant to carry the differing parameters, not reusing it blind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// No host credential awman passes through.
    None,
    /// Claude's keychain OAuth token: delivered as the
    /// `CLAUDE_CODE_OAUTH_TOKEN` env pair, and on container-class runtimes as
    /// a refreshable credential file staged inside the settings overlay.
    ClaudeKeychainOauth,
    /// Antigravity's keychain OAuth token, delivered only as a file planted
    /// inside the staged settings directory.
    AntigravityKeychainFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFlagDelivery {
    /// `--model NAME`
    SpaceArg,
    /// `--model=NAME`
    EqArg,
    /// Not supported.
    Unsupported,
}

/// How a seeded initial prompt is delivered when launching in *interactive*
/// mode.
///
/// Most agents accept the prompt as a trailing positional argument to the bare
/// interactive entrypoint (e.g. `claude "<prompt>"`). A few cannot: opencode's
/// bare command treats a positional as a *project directory* (`opencode
/// [project]`), so passing a prompt there makes opencode `open()` the prompt as
/// a path — which fails with `ENAMETOOLONG` for any real prompt and crashes the
/// container. Those agents declare a dedicated flag instead (opencode
/// `--prompt <text>`).
///
/// This only governs interactive delivery. Non-interactive runs use the
/// agent's `non_interactive_flag` entrypoint shape (e.g. `opencode run`) and
/// receive the prompt over stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractiveSeedDelivery {
    /// Append the prompt as the final positional argv argument.
    Positional,
    /// Deliver the prompt as a flag pair: `<flag> <text>`.
    Flag(String),
}

/// Which Docker Sandbox kit kind awman emits for this agent.
///
/// Consulted only by the sbx kit emitter and launcher (`src/engine/sandbox/
/// dsbx/`); the Docker and Apple runtimes ignore it. `Mixin` extends one of
/// Docker's published built-in agent templates; `Agent` installs the agent
/// itself on top of the generic shell template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SbxKitKind {
    /// Extends a Docker built-in template (`kind: mixin`).
    Mixin,
    /// Full agent spec extending the shell template (`kind: agent`).
    Agent,
}

/// Which option-builder is asking. The two paradigms share every validation
/// rule except ACP, which is container-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunParadigm {
    Container,
    Sandbox,
}

/// Per-agent metadata used by `AgentEngine::build_options` and every other
/// agent-aware consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinition {
    /// Agent name (`claude`, or a custom definition's file stem).
    pub name: String,
    /// Human-readable name shown in ready/status/TUI.
    pub display_name: String,
    pub homepage: Option<String>,
    pub origin: AgentOrigin,
    /// How `.awman/Dockerfile.<name>` is produced.
    pub image: AgentImageSource,
    /// Bare interactive entrypoint (e.g. `["claude"]`, `["copilot", "-i"]`).
    pub interactive_entrypoint: Vec<String>,
    /// Print/non-interactive entrypoint suffix (e.g. `--print` for Claude).
    pub non_interactive_flag: Option<String>,
    /// Flags appended to the entrypoint only for an interactive (non-`-n`)
    /// launch — for options the agent's non-interactive mode rejects (codex
    /// `--no-daemon`, which `codex exec` refuses). Not used for ACP launches.
    pub interactive_only_flags: Vec<String>,
    /// Plan-mode flags. Empty means plan mode is unsupported
    /// (`PlanModeUnsupported`).
    pub plan_flags: Vec<String>,
    /// Yolo flags (e.g. `--dangerously-skip-permissions`). Empty means yolo
    /// silently equates to no permission flags.
    pub yolo_flags: Vec<String>,
    /// Auto flags (e.g. `--permission-mode auto`). Empty is a silent no-op.
    pub auto_flags: Vec<String>,
    /// Disallowed-tools flag name (e.g. `--disallowedTools`).
    pub disallowed_tools_flag: Option<String>,
    /// Allowed-tools flag name (e.g. `--allowedTools`).
    pub allowed_tools_flag: Option<String>,
    /// How model is delivered (`--model NAME` for most).
    pub model_flag: ModelFlagDelivery,
    /// How a seeded prompt is delivered in interactive mode (positional for
    /// most; a dedicated flag for agents whose bare positional means something
    /// else — e.g. opencode `--prompt`).
    pub interactive_seed_delivery: InteractiveSeedDelivery,
    /// Whether the agent supports mid-session prompt injection over its
    /// already-running container's stdin. Used by the workflow engine to
    /// decide between reusing a long-lived container (when `true`) and
    /// spinning up a fresh one per step (when `false`).
    ///
    /// Currently `false` for every shipped agent, and always `false` for a
    /// custom one. Set to `true` once an agent CLI is verified to accept a
    /// newline-terminated prompt on its existing stdin without losing state.
    pub supports_stdin_injection: bool,
    /// Whether this agent's CLI can run in ACP (Agent Client Protocol) mode.
    pub supports_acp: bool,
    /// Entrypoint used when launching in ACP mode. `None` when ACP is not
    /// supported by this agent.
    pub acp_entrypoint: Option<Vec<String>>,
    /// How context system prompts are delivered to this agent.
    pub system_prompt_delivery: SystemPromptMode,
    /// CLI flag for system prompt delivery (e.g. `--append-system-prompt-file`).
    pub system_prompt_flag: Option<String>,
    /// Which Docker Sandbox kit kind awman emits for this agent. Consulted
    /// only by the sbx runtime (`src/engine/sandbox/dsbx/`); other runtimes
    /// ignore it.
    pub sbx_kit_kind: SbxKitKind,
    /// How the host agent's settings/credential directory is passed into a
    /// container. Consumed by `OverlayEngine::agent_settings_overlays_*`.
    pub settings_mount: SettingsMount,
    /// Where the global skills directory is mounted inside the container,
    /// relative to the container `$HOME`. `None` means the agent has no known
    /// skills directory and the skills overlay is skipped with a warning.
    pub skills_mount: Option<String>,
    /// Which host credential scheme, if any, awman passes through for this
    /// agent. Consumed by `engine::auth::keychain`. Always `None` for a
    /// custom definition.
    pub credential_source: CredentialSource,
    /// Full argv of the sanctioned host-side ready ping, **excluding** the
    /// trailing greeting, which the pinger appends. Element 0 is the binary.
    /// `Some` for every first-party agent; for a custom agent it is its
    /// `[ping] command`, and `None` means `awman ready` skips the host check.
    ///
    /// Note that antigravity's ping binary is `antigravity`, not the `agy`
    /// binary named by `interactive_entrypoint`. That is what the pre-F-32
    /// catch-all arm in `ready::ping_command` produced, and F-32 is a
    /// behaviour-preserving change, so it is reproduced exactly. It means the
    /// antigravity ping can only ever report `NotInstalled` on a host where
    /// the binary is called `agy`; correcting it is a behaviour change and
    /// needs its own work item.
    pub ping_argv: Option<Vec<String>>,
    /// Environment variables always set for this agent, on every paradigm.
    pub static_env: Vec<(String, String)>,
    /// Host env vars passed through when set (a custom definition's
    /// `auth.env`). Empty for every first-party agent.
    pub auth_env: Vec<String>,
    /// Whether a mixin-kit launch under a sandbox-class runtime can deliver
    /// plan/yolo/auto as a settings-file permission mode that awman renders
    /// (claude's `permissions.defaultMode`). `false` means the request is
    /// reported as an unsupported note instead.
    pub sandbox_permission_mode_supported: bool,
    /// Note surfaced when `--model` is requested under a sandbox-class
    /// runtime and the agent's built-in template has no mixin-safe model
    /// config. `None` for agents whose model flag survives the sandbox path.
    pub sandbox_model_note: Option<String>,
    /// Allowlist of provider auth env vars accepted via `env(VAR)` overlays
    /// for launch-time auto-auth under a sandbox-class runtime. Empty for
    /// agent-kit agents, which do not participate.
    ///
    /// Each var satisfies two constraints, both verified against the Docker
    /// Sandboxes credentials docs: it maps to an sbx well-known service
    /// (`engine::auth::service_for_credential`), and the agent's base kit
    /// routes that service through the host proxy (the kit template's
    /// `network.allowedDomains` / `environment.proxyManaged`).
    pub sandbox_auth_env_vars: Vec<String>,
    /// Warning surfaced once per launch for an agent its vendor has
    /// deprecated. `None` for every supported agent.
    pub deprecation_note: Option<String>,
}

impl AgentDefinition {
    /// `true` for a definition loaded from a custom TOML file.
    pub fn is_custom(&self) -> bool {
        !matches!(self.origin, AgentOrigin::FirstParty)
    }

    /// The custom definition's scope; `None` for a first-party agent.
    pub fn scope(&self) -> Option<DefinitionScope> {
        match &self.origin {
            AgentOrigin::FirstParty => None,
            AgentOrigin::Custom { scope, .. } => Some(*scope),
        }
    }

    /// Up-front validation shared by `AgentEngine::build_options_with_credentials`
    /// and `AgentEngine::build_sandbox_options`, so a run rejected on one
    /// paradigm is rejected identically on the other.
    ///
    /// The single asymmetry is ACP: a sandbox-class runtime carries no
    /// ACP-piping option, so an ACP launch that clears the `supports_acp`
    /// check is still refused as `NotImplemented` (WI 0089 convention).
    pub fn validate_run(
        &self,
        run: &super::AgentRunOptions,
        paradigm: RunParadigm,
    ) -> Result<(), EngineError> {
        self.validate_tool_restrictions(&run.allowed_tools, &run.disallowed_tools, paradigm)?;
        if matches!(run.plan, Some(PlanMode::Enabled)) && self.plan_flags.is_empty() {
            return Err(EngineError::PlanModeUnsupported {
                agent: self.name.clone(),
            });
        }
        // Every launch path funnels through an option builder, so this single
        // guard keeps an agent that does not speak ACP out of a container with
        // JSON-RPC framing regardless of any Layer 2 pre-flight decision.
        if matches!(run.launch_mode, LaunchMode::Acp) {
            if !self.supports_acp {
                return Err(EngineError::AcpUnsupported {
                    agent: self.name.clone(),
                });
            }
            if matches!(paradigm, RunParadigm::Sandbox) {
                return Err(EngineError::NotImplemented(
                    "ACP launch mode is not supported by sandbox-class runtimes",
                ));
            }
        }
        if matches!(run.plan, Some(PlanMode::Enabled))
            && matches!(run.yolo, Some(YoloMode::Enabled))
        {
            return Err(EngineError::ConflictingOptions(
                "plan and yolo modes are mutually exclusive".into(),
            ));
        }
        Ok(())
    }

    /// Sandbox apply scripts deliver tool permissions only for Claude. Check
    /// delivery before provisioning, including direct backend option callers.
    pub(crate) fn validate_tool_restrictions(
        &self,
        allowed: &[String],
        disallowed: &[String],
        paradigm: RunParadigm,
    ) -> Result<(), EngineError> {
        use crate::engine::error::ToolRestriction;
        for (tools, flag, restriction) in [
            (
                disallowed,
                &self.disallowed_tools_flag,
                ToolRestriction::Disallowed,
            ),
            (allowed, &self.allowed_tools_flag, ToolRestriction::Allowed),
        ] {
            // Name-based on purpose: first-party names are reserved (a custom
            // definition named `claude` is skipped at discovery and rejected
            // by `awman new agent`), so only the built-in claude matches here.
            if !tools.is_empty()
                && (flag.is_none() || (paradigm == RunParadigm::Sandbox && self.name != "claude"))
            {
                return Err(EngineError::ToolRestrictionFlagMissing { restriction });
            }
        }
        Ok(())
    }

    /// Resolve the requested plan/yolo/auto modes into this agent's literal
    /// argv flags, in the fixed yolo → auto → plan order both option builders
    /// used before F-32. Empty when the agent declares no flag for any
    /// requested mode (a silent no-op, by design — see `yolo_flags`).
    pub fn mode_flags(&self, run: &super::AgentRunOptions) -> Vec<String> {
        let mut flags = Vec::new();
        if matches!(run.yolo, Some(YoloMode::Enabled)) {
            flags.extend(self.yolo_flags.iter().cloned());
        }
        if matches!(run.auto, Some(AutoMode::Enabled)) {
            flags.extend(self.auto_flags.iter().cloned());
        }
        if matches!(run.plan, Some(PlanMode::Enabled)) {
            flags.extend(self.plan_flags.iter().cloned());
        }
        flags
    }

    /// The sanctioned host ping's `(binary, argv)` for this agent, with the
    /// greeting appended as the final argument. Pure and side-effect free so
    /// it can be asserted in tests (INV-8). `None` when the definition
    /// declares no host ping (or an empty one).
    pub fn ping_command(&self, greeting: &str) -> Option<(String, Vec<String>)> {
        let (bin, prefix) = self.ping_argv.as_deref()?.split_first()?;
        let mut args = prefix.to_vec();
        args.push(greeting.to_string());
        Some((bin.clone(), args))
    }

    /// The flags [`Self::entrypoint_for`] itself adds for this launch shape,
    /// beyond the bare entrypoint and the non-interactive form. A user
    /// `agentFlags` entry naming one of them is dropped from that launch
    /// rather than passed twice.
    pub fn entrypoint_managed_flags(&self, non_interactive: bool) -> &[String] {
        if non_interactive {
            &[]
        } else {
            &self.interactive_only_flags
        }
    }

    /// Build the entrypoint with optional non-interactive shape.
    pub fn entrypoint_for(&self, non_interactive: bool) -> Entrypoint {
        let mut parts = self.interactive_entrypoint.clone();
        if non_interactive {
            if let Some(flag) = &self.non_interactive_flag {
                // For agents like Codex (`codex exec`) the "flag" is actually a
                // subcommand inserted after the binary; for Claude it's `--print`
                // appended after the args. Both append-at-end shapes work here
                // because the seeded prompt is positional.
                parts.push(flag.clone());
            }
        }
        parts.extend(
            self.entrypoint_managed_flags(non_interactive)
                .iter()
                .cloned(),
        );
        Entrypoint(parts)
    }

    /// Build the entrypoint used for an ACP launch.
    pub fn entrypoint_for_acp(&self) -> Result<Entrypoint, EngineError> {
        let parts = self
            .acp_entrypoint
            .clone()
            .ok_or_else(|| EngineError::AcpUnsupported {
                agent: self.name.clone(),
            })?;
        Ok(Entrypoint(parts))
    }

    /// Translate a model name into the agent-specific flag form.
    pub fn model_flag_for(&self, model: &str) -> Result<ModelFlagForm, EngineError> {
        match self.model_flag {
            ModelFlagDelivery::SpaceArg => Ok(ModelFlagForm::Argument(model.to_string())),
            // `--model=NAME` is one self-contained argv token — Shorthand, not
            // Argument (the backends prepend `--model` to Argument values).
            ModelFlagDelivery::EqArg => Ok(ModelFlagForm::Shorthand(format!("--model={model}"))),
            ModelFlagDelivery::Unsupported => Err(EngineError::Other(format!(
                "agent '{}' does not support a model flag",
                self.name
            ))),
        }
    }
}
