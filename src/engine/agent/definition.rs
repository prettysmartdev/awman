//! `AgentDefinition` — the one per-agent description every consumer reads.
//!
//! First-party agents build it in Rust (`first_party.rs`); custom agents
//! convert a validated `.awman/agents/<name>.toml` into the same struct.
//! Option builders, overlays, sbx and launch policy never care which.

use std::path::PathBuf;

pub use crate::data::agent_output_format::AgentOutputFormat;
pub use crate::data::fs::agent_dirs::DefinitionScope;

pub use crate::data::agent_policy::{NativePlanKind, NativeStartMode};
use crate::data::config::agents::LaunchMode;
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
    /// Env var pointing to a directory containing AGENTS.md.
    EnvDir { var: String },
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

/// A private host directory mounted for one agent execution. The directory
/// follows the container execution's lifetime, rather than the engine's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStorageDir {
    /// Destination relative to the container's effective HOME.
    pub home_relative_path: String,
    /// Optional environment variable set to the absolute destination.
    pub env_var: Option<String>,
    /// Optional CLI flag and config key for agents whose config file takes
    /// precedence over the environment variable. Emitted after user flags.
    pub config_override: Option<(String, String)>,
    /// Copy `$HOME/<path>` from the host into the private directory before
    /// launch. Regular files and directories only; symlinks, `node_modules`
    /// and `.git` are skipped; nothing is ever copied back. `None` starts
    /// empty.
    pub seed_from_home: Option<String>,
}

/// A breaking CLI major required by an agent adapter; exact builds are never gated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliMajorRequirement {
    /// Binary executed inside the image as `<binary> --version`. Equals
    /// `interactive_entrypoint[0]`.
    pub binary: String,
    /// Major the image must report (1 for opencode, 2 for opencode-v2).
    pub required_major: u64,
}

/// An interactive `--model` launch for an agent whose full-screen command has
/// no model option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveModelFallback {
    /// Inserted directly after `interactive_entrypoint` (opencode-v2: `mini`).
    pub subcommand: String,
    /// Shown to the user once, because the interface they get is different.
    pub notice: String,
    /// `false`: combining the fallback with yolo/auto/plan is refused.
    pub allows_mode_flags: bool,
}

/// Why a requested permission mode is refused for an agent whose
/// `strict_modes` is set and which declares no flags for it. The text is shown
/// verbatim as [`EngineError::AgentModeUnsupported`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeRefusal {
    /// `"yolo"` or `"auto"`.
    pub mode: &'static str,
    pub reason: String,
}

/// State an agent would otherwise write under the workspace, redirected to
/// its writable home for a read-only launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOnlyWorkspaceRedirect {
    pub env_var: String,
    /// Relative to the effective container HOME; only normal components.
    pub home_relative_path: String,
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
/// This only governs interactive delivery. Headless runs use the
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

/// How an agent starts in its native plan (or read-only) mode on each
/// launch route (WI 0070). Replaces the single `plan_flags` array, which
/// could not say that one route has no delivery while another does.
///
/// A route field of `None` means that route cannot deliver the mode and a
/// requested `--plan` is refused there; it never falls back to another
/// route's delivery or to prompt instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePlanSpec {
    pub kind: NativePlanKind,
    /// Tokens added to the mode-flag region on an interactive stdio launch.
    pub interactive_argv: Option<Vec<String>>,
    /// Settings-file delivery for an interactive stdio launch with no argv.
    pub interactive_settings: Option<NativeSettingsPatch>,
    /// Tokens added to the mode-flag region on a headless stdio launch.
    pub headless_argv: Option<Vec<String>>,
    /// ACP session mode id selected and acknowledged before the first prompt.
    pub acp_mode_id: Option<String>,
}

/// A private settings file patch that selects a native starting mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSettingsPatch {
    /// File relative to the effective guest HOME. It must lie inside one of
    /// the definition's `session_storage_dirs` (execution-private, never a
    /// host directory).
    pub home_relative_file: String,
    /// Shallow JSON object merged into that file (keys replace).
    pub patch: serde_json::Value,
}

/// Arity of a provider mode/sandbox selector flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectorArity {
    /// A switch (`--yolo`).
    Bare,
    /// Takes the next token, or `=value` (`--sandbox read-only`).
    Value,
}

/// A provider flag that selects a starting mode or sandbox, checked in the
/// user's agent flags when `--plan` is requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeSelector {
    /// Spellings of the same option (`["--sandbox", "-s"]`).
    pub flags: Vec<String>,
    pub arity: SelectorArity,
    /// `Value`: values that agree with this definition's plan mapping (a
    /// duplicate, dropped once). `Bare`: non-empty marks the switch itself a
    /// duplicate; empty means it always contradicts plan.
    pub plan_values: Vec<String>,
    /// `Value`: only values starting with this prefix are selectors (codex
    /// `--config sandbox_mode=…`). `None` matches any value.
    pub value_prefix: Option<String>,
}

/// Configuration a definition verifies before `--plan` may rely on a named
/// built-in Plan agent. Selected by the definition, never by a name match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlanConfigGuard {
    #[default]
    None,
    OpenCodeV1,
    OpenCodeV2,
}

/// The launch route a run takes, as far as native plan delivery cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanRoute {
    Interactive,
    InteractiveModelFallback,
    Headless,
    Acp,
    SandboxAgentKit,
    SandboxMixin,
}

impl PlanRoute {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::InteractiveModelFallback => "interactive (model fallback)",
            Self::Headless => "headless",
            Self::Acp => "acp",
            Self::SandboxAgentKit => "sandbox agent kit",
            Self::SandboxMixin => "sandbox mixin kit",
        }
    }
}

/// What one launch must do to start in the native plan mode.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NativePlanDelivery {
    /// Tokens for the mode-flag region (empty for settings/ACP/mixin delivery).
    pub argv: Vec<String>,
    /// Private settings patch (opencode-v2 full TUI).
    pub settings: Option<NativeSettingsPatch>,
    /// ACP session mode to select before the first prompt.
    pub acp_mode_id: Option<String>,
    /// Mixin kit: render the plan permission mode in `session.json`.
    pub sandbox_permission_mode: bool,
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
    /// The automation adapter this definition selects (WI 0070). The sole
    /// source of provider-specific lifecycle/prompt mechanics; `None`
    /// declares automation and native resume unsupported.
    pub automation_adapter: Option<crate::data::automation::AutomationAdapterKind>,
    pub profile_state: Option<super::profile_state::ProfileStateSpec>,
    pub native_resume: Option<NativeResumeSpec>,
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
    /// Print/headless entrypoint suffix (e.g. `--print` for Claude).
    pub non_interactive_flag: Option<String>,
    /// Headless invocation takes the prompt as the argument after its mode
    /// flag rather than reading plain text from stdin.
    pub headless_prompt_in_argv: bool,
    /// The structured output mode a headless run can print in
    /// (WI 0137 §4). A capability of the table, not a name match: under the
    /// API an agent with a structured format runs in that mode (claude
    /// `--output-format stream-json --verbose`, codex `exec --json`) and the
    /// matching `engine::agent_output` translator turns the stream into
    /// `step_activity` events, output lines and the final message. `Text`
    /// for every other agent and for a custom definition that declares none.
    pub output_format: AgentOutputFormat,
    /// Flags appended to the entrypoint only for an interactive (non-`-n`)
    /// launch — for options the agent's headless mode rejects (codex
    /// `--no-daemon`, which `codex exec` refuses). Not used for ACP launches.
    pub interactive_only_flags: Vec<String>,
    /// Native plan/read-only starting mode per launch route. `None` means
    /// `--plan` is unsupported on every route (`PlanModeUnsupported`).
    pub native_plan: Option<NativePlanSpec>,
    /// Provider selectors checked in the user's agent flags when `--plan` is
    /// requested (`AgentModeConflict`, or a duplicate dropped once).
    pub mode_selectors: Vec<ModeSelector>,
    /// Environment names not passed to a `--plan` launch because they would
    /// select an incompatible starting mode in the guest.
    pub plan_env_drop: Vec<String>,
    /// Configuration a `--plan` launch must verify first.
    pub plan_config_guard: PlanConfigGuard,
    /// Headless prompt delivered as `<flag> <prompt>` at the end of argv
    /// (copilot `-p`), so mode flags never land between the flag and its
    /// value.
    pub headless_prompt_flag: Option<String>,
    /// Yolo flags (e.g. `--dangerously-skip-permissions`). Empty means yolo
    /// silently equates to no permission flags, unless `strict_modes` is set.
    pub yolo_flags: Vec<String>,
    /// Auto flags (e.g. `--permission-mode auto`). Empty is a silent no-op,
    /// unless `strict_modes` is set.
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
    /// Additional host directories mounted at the same relative location in
    /// the guest (for agents that split configuration and credentials).
    pub extra_settings_mounts: Vec<String>,
    /// Private writable directories mounted for one execution.
    pub session_storage_dirs: Vec<SessionStorageDir>,
    pub read_only_workspace_redirects: Vec<ReadOnlyWorkspaceRedirect>,
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
    /// A requested `--yolo` or `--auto` with no flags is
    /// [`EngineError::AgentModeUnsupported`] rather than a silent no-op.
    pub strict_modes: bool,
    /// Requires disposable per-execution state and credential scope.
    pub requires_private_session: bool,
    /// Agent-specific refusal text for a strict mode it has no flags for.
    /// A mode without an entry gets a generic sentence.
    pub mode_refusals: Vec<ModeRefusal>,
    /// Appended right after the headless form on every headless, non-ACP
    /// launch (cursor `--trust`).
    pub headless_only_flags: Vec<String>,
    /// Appended on a headless, non-ACP launch that is not using a
    /// structured output format (cursor `--output-format text`).
    pub headless_text_flags: Vec<String>,
    /// Interactive `--model` launches use this subcommand instead of the
    /// full-screen entrypoint (opencode-v2 `mini`).
    pub interactive_model_fallback: Option<InteractiveModelFallback>,
    /// In-image version proof (ready probe and launch check).
    pub cli_version: Option<CliMajorRequirement>,
    /// Informational note surfaced once per launch.
    pub launch_note: Option<String>,
    /// Note surfaced on every sandbox-runtime launch.
    pub sandbox_auth_note: Option<String>,
}

/// `key = 'value'` → `key=value`: a TOML-style config assignment with the
/// spaces around `=` and one pair of matching quotes removed, so equivalent
/// spellings of a provider config selector compare equal.
fn normalise_config_assignment(text: &str) -> String {
    fn unquote(value: &str) -> &str {
        for quote in ['"', '\''] {
            if let Some(inner) = value
                .strip_prefix(quote)
                .and_then(|rest| rest.strip_suffix(quote))
            {
                return inner;
            }
        }
        value
    }
    match text.split_once('=') {
        Some((key, value)) => format!("{}={}", key.trim(), unquote(value.trim())),
        None => text.trim().to_string(),
    }
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
        if matches!(run.plan, Some(PlanMode::Enabled)) && self.native_plan.is_none() {
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
        if matches!(paradigm, RunParadigm::Sandbox) && self.requires_private_session {
            return Err(EngineError::Config(format!("agent '{}' requires per-execution state and credential isolation unavailable on Docker Sandboxes; select a container runtime", self.name)));
        }
        // Until the pinned CLI's end-of-options behaviour has runtime proof,
        // refuse option-shaped prompt operands on both paradigms.
        if self.output_format
            == crate::data::agent_output_format::AgentOutputFormat::CursorStreamJson
            && run
                .initial_prompt
                .as_deref()
                .is_some_and(|p| p.trim_start().starts_with('-'))
        {
            return Err(EngineError::Config(
                "Cursor prompts must not start with '-'; prefix the prompt with a sentence".into(),
            ));
        }
        let yolo = matches!(run.yolo, Some(YoloMode::Enabled));
        if self.strict_modes && yolo && !run.yolo_authorized {
            return Err(EngineError::Config(format!("agent '{}' requires explicit --yolo authorization; unattended defaults do not authorize this mode", self.name)));
        }
        let auto = matches!(run.auto, Some(AutoMode::Enabled));
        let plan = matches!(run.plan, Some(PlanMode::Enabled));
        // A strict agent refuses a mode it has no flags for instead of
        // launching with weaker (or stronger) permissions than requested.
        // When both yolo and auto are requested yolo wins, as documented, so
        // auto is only checked on its own.
        if self.strict_modes {
            if yolo && self.yolo_flags.is_empty() {
                return Err(self.mode_refusal("yolo"));
            }
            if !yolo && auto && self.auto_flags.is_empty() {
                return Err(self.mode_refusal("auto"));
            }
        }
        if !run.non_interactive && !matches!(run.launch_mode, LaunchMode::Acp) {
            if let (Some(_), Some(fallback)) = (&run.model, &self.interactive_model_fallback) {
                let mode = if yolo {
                    Some("yolo")
                } else if auto {
                    Some("auto")
                } else if plan {
                    Some("plan")
                } else {
                    None
                };
                if let (Some(mode), false) = (mode, fallback.allows_mode_flags) {
                    return Err(self.model_fallback_refusal(mode, fallback));
                }
            }
        }
        // Each route proves its own delivery; a requested plan that this
        // route cannot deliver is refused here, before anything is prepared.
        self.native_plan_delivery(run, paradigm)?;
        Ok(())
    }

    /// PR #71's refusal of `--model` with a mode flag on an interactive
    /// launch that takes the model fallback subcommand. Text is a regression
    /// anchor.
    fn model_fallback_refusal(
        &self,
        mode: &'static str,
        fallback: &InteractiveModelFallback,
    ) -> EngineError {
        let lacks = if mode == "plan" {
            "has no plan option"
        } else {
            "has no auto-approve option"
        };
        let binary = self
            .interactive_entrypoint
            .first()
            .map(String::as_str)
            .unwrap_or(self.name.as_str());
        EngineError::AgentModeUnsupported {
            agent: self.name.clone(),
            mode,
            reason: format!(
                "agent '{}' cannot combine --model with --{mode} in an interactive \
                 session: the interface that accepts a model ({binary} {}) {lacks}. \
                 Drop one of the two, or run headless (exec prompt), where both are \
                 supported.",
                self.name, fallback.subcommand
            ),
        }
    }

    /// Which route this run takes for native plan delivery. Pure.
    pub fn plan_route(&self, run: &super::AgentRunOptions, paradigm: RunParadigm) -> PlanRoute {
        if matches!(run.launch_mode, LaunchMode::Acp) {
            return PlanRoute::Acp;
        }
        if paradigm == RunParadigm::Sandbox {
            return match self.sbx_kit_kind {
                SbxKitKind::Agent => PlanRoute::SandboxAgentKit,
                SbxKitKind::Mixin => PlanRoute::SandboxMixin,
            };
        }
        if self.uses_model_fallback(run.non_interactive, run.model.is_some()) {
            return PlanRoute::InteractiveModelFallback;
        }
        if run.non_interactive {
            PlanRoute::Headless
        } else {
            PlanRoute::Interactive
        }
    }

    /// The native starting mode this definition reports for a delivered
    /// `--plan`: the spec's kind, or `Default` when it has none.
    pub fn native_start_mode(&self) -> NativeStartMode {
        self.native_plan
            .as_ref()
            .map(|spec| spec.kind.into())
            .unwrap_or_default()
    }

    /// What this run must do to start in the native plan mode, or why its
    /// route cannot. `Ok(None)` when `--plan` is not requested. Pure.
    pub fn native_plan_delivery(
        &self,
        run: &super::AgentRunOptions,
        paradigm: RunParadigm,
    ) -> Result<Option<NativePlanDelivery>, EngineError> {
        if !matches!(run.plan, Some(PlanMode::Enabled)) {
            return Ok(None);
        }
        let Some(spec) = &self.native_plan else {
            return Err(EngineError::PlanModeUnsupported {
                agent: self.name.clone(),
            });
        };
        let route = self.plan_route(run, paradigm);
        let refuse = |reason: &str| EngineError::NativePlanRouteUnsupported {
            agent: self.name.clone(),
            route: route.as_str(),
            reason: reason.to_string(),
        };
        let stdio = |headless: bool| -> Result<NativePlanDelivery, EngineError> {
            if headless {
                let argv = spec.headless_argv.clone().ok_or_else(|| {
                    refuse("the agent declares no headless plan mode; run it interactively or launch without --plan")
                })?;
                return Ok(NativePlanDelivery {
                    argv,
                    ..Default::default()
                });
            }
            if spec.interactive_argv.is_none() && spec.interactive_settings.is_none() {
                return Err(refuse(
                    "the agent declares no interactive plan mode; run it headless (exec prompt --headless) or launch without --plan",
                ));
            }
            Ok(NativePlanDelivery {
                argv: spec.interactive_argv.clone().unwrap_or_default(),
                settings: spec.interactive_settings.clone(),
                ..Default::default()
            })
        };
        let delivery = match route {
            PlanRoute::Acp => {
                let id = spec.acp_mode_id.clone().ok_or_else(|| {
                    refuse("ACP launch has no declared plan mode; use agents.launchMode=stdio or launch without --plan")
                })?;
                NativePlanDelivery {
                    acp_mode_id: Some(id),
                    ..Default::default()
                }
            }
            PlanRoute::InteractiveModelFallback => {
                let fallback = self
                    .interactive_model_fallback
                    .as_ref()
                    .expect("model fallback route has a fallback");
                if !fallback.allows_mode_flags {
                    return Err(self.model_fallback_refusal("plan", fallback));
                }
                stdio(false)?
            }
            PlanRoute::Interactive => stdio(false)?,
            PlanRoute::Headless => stdio(true)?,
            PlanRoute::SandboxAgentKit => {
                let delivery = stdio(run.non_interactive)?;
                if delivery.settings.is_some() {
                    return Err(refuse(
                        "the plan mode is delivered through private settings, which a sandbox kit cannot stage; select a container runtime",
                    ));
                }
                delivery
            }
            PlanRoute::SandboxMixin => {
                if !self.sandbox_permission_mode_supported {
                    return Err(refuse(
                        "the built-in template's launch command takes no flags and the agent has no settings equivalent awman manages; select a container runtime or launch without --plan",
                    ));
                }
                NativePlanDelivery {
                    sandbox_permission_mode: true,
                    ..Default::default()
                }
            }
        };
        Ok(Some(delivery))
    }

    /// Refuse user agent flags that contradict a requested plan, and drop a
    /// matching duplicate selector once. No-op when plan is not requested.
    ///
    /// Only whole tokens are inspected: a selector spelling, its next-token
    /// value or its `=value` form. Prose inside a token is never scanned.
    /// `origin` names where the list came from (`--agent-flag` or
    /// `agents.flags`).
    pub fn reconcile_agent_flags_for_plan(
        &self,
        run: &super::AgentRunOptions,
        agent_flags: Vec<String>,
        origin: &'static str,
    ) -> Result<Vec<String>, EngineError> {
        if !matches!(run.plan, Some(PlanMode::Enabled)) {
            return Ok(agent_flags);
        }
        let conflict = |selector: String, reason: &str| EngineError::AgentModeConflict {
            agent: self.name.clone(),
            selector,
            origin,
            reason: reason.to_string(),
        };
        // Generic rule: awman's own --yolo / --auto spelled through agent flags.
        for (mode, sequence) in [("--yolo", &self.yolo_flags), ("--auto", &self.auto_flags)] {
            if !sequence.is_empty()
                && agent_flags
                    .windows(sequence.len())
                    .any(|w| w == sequence.as_slice())
            {
                return Err(conflict(
                    sequence.join(" "),
                    &format!(
                        "it is how awman delivers {mode}, which cannot be combined with --plan; remove it from the agent flags"
                    ),
                ));
            }
        }
        let mut kept = Vec::with_capacity(agent_flags.len());
        let mut i = 0;
        while i < agent_flags.len() {
            let token = &agent_flags[i];
            let matched = self.mode_selectors.iter().find_map(|selector| {
                selector.flags.iter().find_map(|flag| {
                    if token == flag {
                        return Some((selector, flag, None));
                    }
                    let rest = token.strip_prefix(flag.as_str())?;
                    if let Some(value) = rest.strip_prefix('=') {
                        return Some((selector, flag, Some(value.to_string())));
                    }
                    // A short value flag also takes its value attached
                    // (`-sdanger-full-access`), as clap-style parsers accept.
                    let short = flag.len() == 2 && flag.starts_with('-') && flag != "--";
                    (short && selector.arity == SelectorArity::Value && !rest.is_empty())
                        .then(|| (selector, flag, Some(rest.to_string())))
                })
            });
            let Some((selector, flag, inline)) = matched else {
                kept.push(token.clone());
                i += 1;
                continue;
            };
            match selector.arity {
                SelectorArity::Bare => {
                    if selector.plan_values.is_empty() || inline.is_some() {
                        return Err(conflict(
                            token.clone(),
                            "it selects a different starting mode or permission policy; remove it or launch without --plan",
                        ));
                    }
                    // A duplicate of awman's own plan selector: dropped once.
                    i += 1;
                }
                SelectorArity::Value => {
                    let (value, consumed) = match inline {
                        Some(value) => (Some(value), 1),
                        None => (agent_flags.get(i + 1).cloned(), 2),
                    };
                    let Some(value) = value else {
                        return Err(conflict(
                            token.clone(),
                            "it has no value, so awman cannot verify it agrees with --plan",
                        ));
                    };
                    // A config-key selector (`-c sandbox_mode=...`) is
                    // compared in its normalised form: TOML allows spaces
                    // around `=` and a quoted value.
                    let (value, plan_values) = match selector.value_prefix.as_deref() {
                        Some(_) => (
                            normalise_config_assignment(&value),
                            selector
                                .plan_values
                                .iter()
                                .map(|v| normalise_config_assignment(v))
                                .collect(),
                        ),
                        None => (value, selector.plan_values.clone()),
                    };
                    let is_selector = selector
                        .value_prefix
                        .as_deref()
                        .is_none_or(|prefix| value.starts_with(prefix));
                    if !is_selector {
                        kept.extend(agent_flags[i..i + consumed].iter().cloned());
                    } else if !plan_values.contains(&value) {
                        return Err(conflict(
                            format!("{flag} {value}"),
                            "it selects a different starting mode or sandbox; remove it or launch without --plan",
                        ));
                    }
                    i += consumed;
                }
            }
        }
        Ok(kept)
    }

    fn mode_refusal(&self, mode: &'static str) -> EngineError {
        let reason = self
            .mode_refusals
            .iter()
            .find(|r| r.mode == mode)
            .map(|r| r.reason.clone())
            .unwrap_or_else(|| {
                format!(
                    "agent '{}' has no equivalent of --{mode}; launch without it",
                    self.name
                )
            });
        EngineError::AgentModeUnsupported {
            agent: self.name.clone(),
            mode,
            reason,
        }
    }

    /// `true` when this run will be delivered in the agent's structured
    /// output format (so `headless_text_flags` must not also be applied).
    /// Mirrors the condition `build_options` uses to emit
    /// `ContainerOption::AgentOutputFormat`.
    pub fn uses_structured_output(&self, run: &super::AgentRunOptions) -> bool {
        run.structured_output
            && run.non_interactive
            && !matches!(run.launch_mode, LaunchMode::Acp)
            && self.output_format.is_structured()
    }

    /// `true` when this run takes the interactive model fallback subcommand.
    fn uses_model_fallback(&self, non_interactive: bool, model_requested: bool) -> bool {
        !non_interactive && model_requested && self.interactive_model_fallback.is_some()
    }

    /// `Some(notice)` when this run uses the interactive model fallback.
    pub fn interactive_model_notice(&self, run: &super::AgentRunOptions) -> Option<&str> {
        if matches!(run.launch_mode, LaunchMode::Acp) {
            return None;
        }
        if self.uses_model_fallback(run.non_interactive, run.model.is_some()) {
            self.interactive_model_fallback
                .as_ref()
                .map(|f| f.notice.as_str())
        } else {
            None
        }
    }

    /// The entrypoint for a non-ACP launch: `interactive_entrypoint`, then
    /// either the interactive model fallback subcommand (interactive with a
    /// model), or the headless form plus `headless_only_flags` (and
    /// `headless_text_flags` unless a structured format is in use), or the
    /// `interactive_only_flags`. Every option builder (container and sbx)
    /// derives its launch shape from here so they cannot drift.
    pub fn entrypoint_for_run(&self, run: &super::AgentRunOptions) -> Entrypoint {
        Entrypoint(self.launch_shape(
            run.non_interactive,
            run.model.is_some(),
            self.uses_structured_output(run),
        ))
    }

    /// The raw parts behind [`Self::entrypoint_for_run`], for callers (the
    /// sbx tail) that hold resolved options rather than `AgentRunOptions`.
    pub fn launch_shape(
        &self,
        non_interactive: bool,
        model_requested: bool,
        structured: bool,
    ) -> Vec<String> {
        let mut parts = self.interactive_entrypoint.clone();
        if self.uses_model_fallback(non_interactive, model_requested) {
            if let Some(fallback) = &self.interactive_model_fallback {
                parts.push(fallback.subcommand.clone());
            }
            return parts;
        }
        if non_interactive {
            if let Some(flag) = &self.non_interactive_flag {
                parts.push(flag.clone());
            }
            parts.extend(self.headless_only_flags.iter().cloned());
            if !structured {
                parts.extend(self.headless_text_flags.iter().cloned());
            }
        } else {
            parts.extend(self.interactive_only_flags.iter().cloned());
        }
        parts
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
    /// requested mode (a silent no-op, by design — see `yolo_flags`). The
    /// plan part is the argv of this route's [`Self::native_plan_delivery`]
    /// (empty for a settings, ACP or mixin delivery).
    pub fn mode_flags(
        &self,
        run: &super::AgentRunOptions,
        paradigm: RunParadigm,
    ) -> Result<Vec<String>, EngineError> {
        let mut flags = Vec::new();
        if matches!(run.yolo, Some(YoloMode::Enabled)) {
            flags.extend(self.yolo_flags.iter().cloned());
        }
        if matches!(run.auto, Some(AutoMode::Enabled)) {
            flags.extend(self.auto_flags.iter().cloned());
        }
        if let Some(delivery) = self.native_plan_delivery(run, paradigm)? {
            flags.extend(delivery.argv);
        }
        Ok(flags)
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
    /// beyond the bare entrypoint and the headless form. A user
    /// `agentFlags` entry naming one of them is dropped from that launch
    /// rather than passed twice.
    pub fn entrypoint_managed_flags(&self, non_interactive: bool) -> &[String] {
        if non_interactive {
            &self.headless_only_flags
        } else {
            &self.interactive_only_flags
        }
    }

    /// Build the entrypoint with optional headless shape.
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

#[cfg(test)]
mod native_plan_tests {
    //! WI 0070: native plan delivery per agent and route, the conflict
    //! rules, custom route keys and the OpenCode Plan-profile guard. Pure.
    use super::*;
    use crate::engine::agent::first_party::{first_party, FIRST_PARTY_AGENTS};
    use crate::engine::agent::opencode_plan_guard::{
        check_opencode_plan, OpenCodeConfigSource, SourceKind, SourceScope,
    };
    use crate::engine::agent::{launch_argv_options, AgentRunOptions};
    use crate::engine::container::options::ResolvedContainerOptions;
    use crate::engine::container::AgentArgv;

    fn s(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    fn def(name: &str) -> AgentDefinition {
        first_party(name).unwrap_or_else(|| panic!("{name} is first-party"))
    }

    fn plan_run(headless: bool) -> AgentRunOptions {
        AgentRunOptions {
            plan: Some(PlanMode::Enabled),
            non_interactive: headless,
            ..Default::default()
        }
    }

    #[derive(Debug, PartialEq)]
    enum Expect {
        Argv(Vec<String>),
        Settings(&'static str, serde_json::Value),
        Acp(&'static str),
        PermissionMode,
        RouteRefused,
        ModelFallbackRefused,
    }

    fn outcome(agent: &str, run: &AgentRunOptions, paradigm: RunParadigm) -> Expect {
        match def(agent).native_plan_delivery(run, paradigm) {
            Ok(Some(d)) if d.sandbox_permission_mode => Expect::PermissionMode,
            Ok(Some(d)) if d.acp_mode_id.is_some() => {
                Expect::Acp(if d.acp_mode_id.as_deref() == Some("plan") {
                    "plan"
                } else {
                    "other"
                })
            }
            Ok(Some(d)) if d.settings.is_some() => {
                let settings = d.settings.unwrap();
                assert!(d.argv.is_empty(), "{agent}: settings delivery has no argv");
                Expect::Settings(
                    if settings.home_relative_file == ".config/opencode/opencode.json" {
                        ".config/opencode/opencode.json"
                    } else {
                        "other"
                    },
                    settings.patch,
                )
            }
            Ok(Some(d)) => Expect::Argv(d.argv),
            Ok(None) => panic!("{agent}: plan requested but no delivery"),
            Err(EngineError::NativePlanRouteUnsupported { agent: a, .. }) => {
                assert_eq!(a, agent);
                Expect::RouteRefused
            }
            Err(EngineError::AgentModeUnsupported { mode: "plan", .. }) => {
                Expect::ModelFallbackRefused
            }
            Err(other) => panic!("{agent}: unexpected {other:?}"),
        }
    }

    /// The design's per-agent table (§A.4.1), every route, for all eight.
    #[test]
    fn every_built_in_delivers_plan_on_its_declared_routes() {
        use Expect::*;
        let argv = |parts: &[&str]| Argv(s(parts));
        let v2_settings = || {
            Settings(
                ".config/opencode/opencode.json",
                serde_json::json!({"default_agent": "plan"}),
            )
        };
        // (agent, interactive, headless, interactive+model, acp, sandbox interactive, sandbox headless)
        let table: Vec<(&str, Expect, Expect, Expect, Expect, Expect, Expect)> = vec![
            (
                "claude",
                argv(&["--permission-mode", "plan"]),
                argv(&["--permission-mode", "plan"]),
                argv(&["--permission-mode", "plan"]),
                RouteRefused,
                PermissionMode,
                PermissionMode,
            ),
            (
                "codex",
                argv(&["--sandbox", "read-only"]),
                argv(&["--sandbox", "read-only"]),
                argv(&["--sandbox", "read-only"]),
                RouteRefused,
                RouteRefused,
                RouteRefused,
            ),
            (
                "opencode",
                argv(&["--agent", "plan"]),
                argv(&["--agent", "plan"]),
                argv(&["--agent", "plan"]),
                Acp("plan"),
                argv(&["--agent", "plan"]),
                argv(&["--agent", "plan"]),
            ),
            (
                "opencode-v2",
                v2_settings(),
                argv(&["--agent", "plan"]),
                ModelFallbackRefused,
                RouteRefused,
                RouteRefused,
                argv(&["--agent", "plan"]),
            ),
            (
                "cursor",
                argv(&["--mode", "plan"]),
                argv(&["--mode", "plan"]),
                argv(&["--mode", "plan"]),
                RouteRefused,
                argv(&["--mode", "plan"]),
                argv(&["--mode", "plan"]),
            ),
            (
                "copilot",
                argv(&["--plan"]),
                argv(&["--plan"]),
                argv(&["--plan"]),
                RouteRefused,
                RouteRefused,
                RouteRefused,
            ),
            (
                "cline",
                argv(&["--plan"]),
                argv(&["--plan"]),
                argv(&["--plan"]),
                Acp("plan"),
                argv(&["--plan"]),
                argv(&["--plan"]),
            ),
            (
                "antigravity",
                argv(&["--mode", "plan"]),
                argv(&["--mode", "plan"]),
                argv(&["--mode", "plan"]),
                RouteRefused,
                argv(&["--mode", "plan"]),
                argv(&["--mode", "plan"]),
            ),
        ];
        assert_eq!(table.len(), FIRST_PARTY_AGENTS.len());
        for (agent, interactive, headless, model, acp, sbx_i, sbx_h) in table {
            assert_eq!(
                outcome(agent, &plan_run(false), RunParadigm::Container),
                interactive,
                "{agent} interactive"
            );
            assert_eq!(
                outcome(agent, &plan_run(true), RunParadigm::Container),
                headless,
                "{agent} headless"
            );
            let mut with_model = plan_run(false);
            with_model.model = Some("m".into());
            assert_eq!(
                outcome(agent, &with_model, RunParadigm::Container),
                model,
                "{agent} interactive --model"
            );
            let mut over_acp = plan_run(false);
            over_acp.launch_mode = LaunchMode::Acp;
            assert_eq!(
                outcome(agent, &over_acp, RunParadigm::Container),
                acp,
                "{agent} acp"
            );
            assert_eq!(
                outcome(agent, &plan_run(false), RunParadigm::Sandbox),
                sbx_i,
                "{agent} sandbox interactive"
            );
            assert_eq!(
                outcome(agent, &plan_run(true), RunParadigm::Sandbox),
                sbx_h,
                "{agent} sandbox headless"
            );
        }
    }

    #[test]
    fn kinds_and_start_modes_are_reported_per_agent() {
        for agent in FIRST_PARTY_AGENTS {
            let expected = if *agent == "codex" {
                NativeStartMode::ReadOnly
            } else {
                NativeStartMode::Plan
            };
            assert_eq!(def(agent).native_start_mode(), expected, "{agent}");
        }
    }

    #[test]
    fn no_plan_means_no_delivery_and_no_refusal() {
        for agent in FIRST_PARTY_AGENTS {
            for headless in [false, true] {
                let run = AgentRunOptions {
                    non_interactive: headless,
                    ..Default::default()
                };
                assert_eq!(
                    def(agent)
                        .native_plan_delivery(&run, RunParadigm::Sandbox)
                        .unwrap(),
                    None
                );
                assert!(def(agent)
                    .mode_flags(&run, RunParadigm::Container)
                    .unwrap()
                    .is_empty());
            }
        }
    }

    #[test]
    fn validate_run_refuses_routes_before_launch_and_keeps_pr71_anchors() {
        // ACP on an agent without ACP: the existing refusal comes first.
        let mut acp = plan_run(false);
        acp.launch_mode = LaunchMode::Acp;
        assert!(matches!(
            def("claude").validate_run(&acp, RunParadigm::Container),
            Err(EngineError::AcpUnsupported { .. })
        ));
        // Docker Sandboxes keep refusing private-session agents.
        for agent in ["opencode-v2", "cursor"] {
            let err = def(agent)
                .validate_run(&plan_run(true), RunParadigm::Sandbox)
                .unwrap_err();
            assert!(
                err.to_string().contains("requires per-execution state"),
                "{err}"
            );
        }
        // PR #71's --model + mode refusal text is unchanged.
        let mut model = plan_run(false);
        model.model = Some("m".into());
        let err = def("opencode-v2")
            .validate_run(&model, RunParadigm::Container)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot combine --model with --plan"),
            "{err}"
        );
        assert!(err.to_string().contains("opencode mini"), "{err}");
        assert!(err.is_native_capability_unavailable());
        // A mixin without a managed route is refused, never noted.
        let err = def("codex")
            .validate_run(&plan_run(false), RunParadigm::Sandbox)
            .unwrap_err();
        assert!(
            matches!(err, EngineError::NativePlanRouteUnsupported { .. }),
            "{err:?}"
        );
        assert!(err.is_native_capability_unavailable());
        // plan + yolo stays a conflict.
        let mut both = plan_run(false);
        both.yolo = Some(YoloMode::Enabled);
        both.yolo_authorized = true;
        assert!(matches!(
            def("claude").validate_run(&both, RunParadigm::Container),
            Err(EngineError::ConflictingOptions(_))
        ));
    }

    #[test]
    fn mode_flags_keep_the_yolo_auto_plan_order() {
        let d = def("antigravity");
        let run = AgentRunOptions {
            auto: Some(AutoMode::Enabled),
            plan: Some(PlanMode::Enabled),
            ..Default::default()
        };
        assert_eq!(
            d.mode_flags(&run, RunParadigm::Container).unwrap(),
            s(&["--mode", "accept-edits", "--mode", "plan"])
        );
        let yolo = AgentRunOptions {
            yolo: Some(YoloMode::Enabled),
            ..Default::default()
        };
        assert_eq!(
            d.mode_flags(&yolo, RunParadigm::Container).unwrap(),
            s(&["--dangerously-skip-permissions"])
        );
    }

    fn full_argv(agent: &str, run: &AgentRunOptions) -> Vec<String> {
        let d = def(agent);
        d.validate_run(run, RunParadigm::Container).unwrap();
        let resolved =
            ResolvedContainerOptions::resolve(launch_argv_options(&d, run).unwrap()).unwrap();
        let entrypoint = resolved.entrypoint.clone().unwrap().0;
        AgentArgv::from_options(&resolved)
            .unwrap()
            .append_to(entrypoint)
    }

    #[test]
    fn copilot_headless_puts_p_directly_before_the_prompt() {
        let mut run = plan_run(true);
        run.initial_prompt = Some("review".into());
        assert_eq!(
            full_argv("copilot", &run),
            s(&["copilot", "--plan", "-p", "review"])
        );
        let yolo = AgentRunOptions {
            yolo: Some(YoloMode::Enabled),
            non_interactive: true,
            initial_prompt: Some("review".into()),
            ..Default::default()
        };
        assert_eq!(
            full_argv("copilot", &yolo),
            s(&["copilot", "--yolo", "-p", "review"])
        );
        let mut interactive = plan_run(false);
        interactive.initial_prompt = Some("review".into());
        assert_eq!(
            full_argv("copilot", &interactive),
            s(&["copilot", "--plan", "-i", "review"])
        );
    }

    #[test]
    fn plan_argv_lands_in_the_mode_region_for_every_argv_route() {
        let mut run = plan_run(true);
        run.initial_prompt = Some("go".into());
        assert_eq!(
            full_argv("codex", &run),
            s(&["codex", "exec", "--sandbox", "read-only"])
        );
        assert_eq!(
            full_argv("opencode-v2", &run),
            s(&["opencode", "--standalone", "run", "--agent", "plan"])
        );
        assert_eq!(
            full_argv("cursor", &run),
            s(&[
                "cursor-agent",
                "--print",
                "--trust",
                "--output-format",
                "text",
                "--mode",
                "plan",
                "go"
            ])
        );
        let mut interactive = plan_run(false);
        interactive.initial_prompt = Some("go".into());
        assert_eq!(
            full_argv("codex", &interactive),
            s(&["codex", "--no-daemon", "--sandbox", "read-only", "go"])
        );
        assert_eq!(
            full_argv("opencode", &interactive),
            s(&["opencode", "--agent", "plan", "--prompt", "go"])
        );
        // opencode-v2's full TUI gets no plan argv (settings delivery).
        assert_eq!(
            full_argv("opencode-v2", &interactive),
            s(&["opencode", "--standalone", "--prompt", "go"])
        );
        assert_eq!(
            full_argv("cline", &interactive),
            s(&["cline", "--tui", "--plan", "go"])
        );
    }

    fn reconcile(agent: &str, flags: &[&str]) -> Result<Vec<String>, EngineError> {
        def(agent).reconcile_agent_flags_for_plan(&plan_run(false), s(flags), "--agent-flag")
    }

    fn conflicts(agent: &str, flags: &[&str]) {
        match reconcile(agent, flags) {
            Err(EngineError::AgentModeConflict {
                agent: a, origin, ..
            }) => {
                assert_eq!(a, agent);
                assert_eq!(origin, "--agent-flag");
            }
            other => panic!("{agent} {flags:?}: expected a conflict, got {other:?}"),
        }
    }

    #[test]
    fn contradicting_selectors_are_refused_per_agent() {
        let table: &[(&str, &[&[&str]])] = &[
            (
                "claude",
                &[
                    &["--permission-mode", "acceptEdits"],
                    &["--permission-mode=default"],
                    &["--dangerously-skip-permissions"],
                    &["--permission-mode"],
                ],
            ),
            (
                "codex",
                &[
                    &["--sandbox", "workspace-write"],
                    &["-s", "danger-full-access"],
                    &["--sandbox=workspace-write"],
                    &["--config", "sandbox_mode=workspace-write"],
                    &["-c", "sandbox_mode=danger-full-access"],
                    // Attached short values and spaced/quoted config keys.
                    &["-sdanger-full-access"],
                    &["-c", "sandbox_mode = 'danger-full-access'"],
                    &["--config", "sandbox_mode = \"workspace-write\""],
                    &["-csandbox_mode=workspace-write"],
                    &["--full-auto"],
                    &["--dangerously-bypass-approvals-and-sandbox"],
                    &["--yolo"],
                ],
            ),
            (
                "opencode",
                &[&["--agent", "build"], &["--agent=build"], &["--auto"]],
            ),
            ("opencode-v2", &[&["--agent", "build"], &["--auto"]]),
            (
                "cursor",
                &[&["--mode", "ask"], &["--force"], &["-f"], &["--yolo"]],
            ),
            (
                "copilot",
                &[
                    &["--mode", "autopilot"],
                    &["--autopilot"],
                    &["--yolo"],
                    &["--allow-all"],
                ],
            ),
            (
                "cline",
                &[
                    &["--act"],
                    &["-a"],
                    &["--yolo"],
                    &["-y"],
                    &["--auto-approve", "false"],
                    &["--auto-approve", "true"],
                ],
            ),
            (
                "antigravity",
                &[
                    &["--mode", "accept-edits"],
                    &["--mode", "fast"],
                    &["--dangerously-skip-permissions"],
                ],
            ),
        ];
        assert_eq!(table.len(), FIRST_PARTY_AGENTS.len());
        for (agent, cases) in table {
            for flags in *cases {
                conflicts(agent, flags);
            }
        }
    }

    #[test]
    fn matching_duplicates_are_normalised_once_and_other_flags_kept() {
        let table: &[(&str, &[&str], &[&str])] = &[
            (
                "claude",
                &["--verbose", "--permission-mode", "plan"],
                &["--verbose"],
            ),
            ("claude", &["--permission-mode=plan"], &[]),
            (
                "codex",
                &["--sandbox", "read-only", "-c", "model=o3"],
                &["-c", "model=o3"],
            ),
            (
                "codex",
                &["-sread-only", "-c", "sandbox_mode = 'read-only'"],
                &[],
            ),
            ("codex", &["--config", "sandbox_mode=\"read-only\""], &[]),
            ("codex", &["-c", "sandbox_mode=read-only"], &[]),
            (
                "opencode",
                &["--agent", "plan", "--print-logs"],
                &["--print-logs"],
            ),
            ("cursor", &["--mode=plan"], &[]),
            ("copilot", &["--plan", "--mode", "plan"], &[]),
            ("cline", &["--plan", "-p"], &[]),
            ("antigravity", &["--mode", "plan"], &[]),
        ];
        for (agent, input, kept) in table {
            assert_eq!(
                reconcile(agent, input).unwrap(),
                s(kept),
                "{agent} {input:?}"
            );
        }
    }

    #[test]
    fn prose_inside_a_token_is_never_scanned_and_plan_off_is_a_no_op() {
        assert_eq!(
            reconcile(
                "claude",
                &[
                    "--append-system-prompt",
                    "never use --dangerously-skip-permissions"
                ]
            )
            .unwrap(),
            s(&[
                "--append-system-prompt",
                "never use --dangerously-skip-permissions"
            ])
        );
        let off = AgentRunOptions::default();
        assert_eq!(
            def("claude")
                .reconcile_agent_flags_for_plan(
                    &off,
                    s(&["--dangerously-skip-permissions"]),
                    "agents.flags"
                )
                .unwrap(),
            s(&["--dangerously-skip-permissions"])
        );
    }

    #[test]
    fn the_generic_rule_refuses_awman_yolo_or_auto_spelled_as_agent_flags() {
        let mut custom = def("claude");
        custom.mode_selectors.clear();
        custom.yolo_flags = s(&["--trust-all"]);
        custom.auto_flags = s(&["--approve", "edits"]);
        for flags in [&["--trust-all"][..], &["x", "--approve", "edits"][..]] {
            assert!(matches!(
                custom.reconcile_agent_flags_for_plan(&plan_run(false), s(flags), "agents.flags"),
                Err(EngineError::AgentModeConflict {
                    origin: "agents.flags",
                    ..
                })
            ));
        }
        // A partial sequence is not awman's --auto.
        assert_eq!(
            custom
                .reconcile_agent_flags_for_plan(
                    &plan_run(false),
                    s(&["--approve", "all"]),
                    "agents.flags"
                )
                .unwrap(),
            s(&["--approve", "all"])
        );
    }

    fn custom_modes(toml_modes: &str) -> Option<NativePlanSpec> {
        let modes: crate::data::config::agent_definition::ModesTable =
            toml::from_str(toml_modes).unwrap();
        crate::engine::agent::custom::native_plan_spec(&modes)
    }

    #[test]
    fn custom_routes_inherit_plan_unless_explicitly_empty() {
        let inherit = custom_modes("plan = [\"--plan\"]").unwrap();
        assert_eq!(inherit.kind, NativePlanKind::Custom);
        assert_eq!(inherit.interactive_argv, Some(s(&["--plan"])));
        assert_eq!(inherit.headless_argv, Some(s(&["--plan"])));
        assert_eq!(inherit.acp_mode_id, None);

        let split = custom_modes(
            "plan = [\"--plan\"]\nplan_headless = []\nplan_interactive = [\"--mode\", \"plan\"]\nplan_kind = \"plan\"",
        )
        .unwrap();
        assert_eq!(split.kind, NativePlanKind::Plan);
        assert_eq!(split.interactive_argv, Some(s(&["--mode", "plan"])));
        assert_eq!(split.headless_argv, None, "explicitly empty = unsupported");

        let acp_only = custom_modes("plan_acp_mode = \"architect\"").unwrap();
        assert_eq!(acp_only.interactive_argv, None);
        assert_eq!(acp_only.headless_argv, None);
        assert_eq!(acp_only.acp_mode_id.as_deref(), Some("architect"));

        assert_eq!(custom_modes(""), None);
        assert_eq!(
            custom_modes("plan_interactive = []\nplan_headless = []"),
            None
        );
    }

    #[test]
    fn a_custom_agent_never_infers_acp_or_a_route_it_did_not_declare() {
        let mut custom = def("cline");
        custom.native_plan = custom_modes("plan = [\"--plan\"]\nplan_headless = []");
        let mut acp = plan_run(false);
        acp.launch_mode = LaunchMode::Acp;
        assert!(matches!(
            custom.native_plan_delivery(&acp, RunParadigm::Container),
            Err(EngineError::NativePlanRouteUnsupported { route: "acp", .. })
        ));
        assert!(matches!(
            custom.native_plan_delivery(&plan_run(true), RunParadigm::Container),
            Err(EngineError::NativePlanRouteUnsupported {
                route: "headless",
                ..
            })
        ));
        custom.native_plan = None;
        assert!(matches!(
            custom.native_plan_delivery(&plan_run(false), RunParadigm::Container),
            Err(EngineError::PlanModeUnsupported { .. })
        ));
        // Mount-only needs no native plan.
        assert_eq!(
            custom
                .native_plan_delivery(&AgentRunOptions::default(), RunParadigm::Container)
                .unwrap(),
            None
        );
    }

    fn json(origin: &str, scope: SourceScope, text: &str) -> OpenCodeConfigSource {
        OpenCodeConfigSource {
            origin: origin.into(),
            kind: SourceKind::Json,
            scope,
            text: text.into(),
        }
    }

    fn guard_conflict(
        guard: PlanConfigGuard,
        sources: &[OpenCodeConfigSource],
        settings: bool,
    ) -> String {
        match check_opencode_plan(guard, "opencode", sources, settings) {
            Err(EngineError::AgentModeConflict {
                selector,
                origin: "settings",
                ..
            }) => selector,
            other => panic!("expected a conflict, got {other:?}"),
        }
    }

    /// WI 0070 review finding 3: v2 configures agents under the plural
    /// `agents` object, so the v2 guard inspects it in every source kind
    /// (JSON, JSONC, the seeded private copy, inline environment); the 1.x
    /// schema has no such table.
    #[test]
    fn the_opencode_v2_guard_inspects_the_plural_agents_table() {
        use PlanConfigGuard::{OpenCodeV1, OpenCodeV2};
        let replaced = "{\"agents\":{\"plan\":{\"mode\":\"primary\",\"system\":\"Act and edit \
                        immediately\",\"permissions\":[{\"action\":\"*\",\"resource\":\"*\",\
                        \"effect\":\"allow\"}]}}}";
        for (origin, scope, text) in [
            ("project opencode.json", SourceScope::Project, replaced),
            (
                "project .opencode/opencode.jsonc",
                SourceScope::Project,
                "{ // v2\n \"agents\": { \"plan\": { \"disable\": true, }, }, }",
            ),
            (
                "private global opencode.json",
                SourceScope::PrivateGlobal,
                replaced,
            ),
            (
                "OPENCODE_CONFIG_CONTENT",
                SourceScope::Environment,
                replaced,
            ),
        ] {
            let source = json(origin, scope, text);
            for settings in [false, true] {
                let selector = guard_conflict(OpenCodeV2, std::slice::from_ref(&source), settings);
                assert!(selector.contains("agents.plan"), "{origin}: {selector}");
            }
        }
        let cosmetic = json(
            "project opencode.json",
            SourceScope::Project,
            "{\"agents\":{\"plan\":{\"model\":\"m\",\"color\":\"#000\"},\"build\":{\"system\":\"x\"}}}",
        );
        assert!(
            check_opencode_plan(OpenCodeV2, "opencode-v2", &[cosmetic], false)
                .unwrap()
                .is_empty()
        );
        guard_conflict(
            OpenCodeV2,
            &[json(
                "x",
                SourceScope::Project,
                "{\"agents\":{\"plan\":true}}",
            )],
            false,
        );
        // 1.x has no plural table; its singular one stays inspected.
        assert!(check_opencode_plan(
            OpenCodeV1,
            "opencode",
            &[json("x", SourceScope::Project, replaced)],
            false
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn the_opencode_guard_allows_cosmetic_plan_overrides_only() {
        use PlanConfigGuard::{OpenCodeV1, OpenCodeV2};
        let ok = json(
            "project opencode.json",
            SourceScope::Project,
            "{ // comment\n \"agent\": {\"plan\": {\"model\": \"x\", \"temperature\": 0.1, \"top_p\": 1, \"color\": \"#fff\", \"description\": \"d\", \"variant\": \"v\",}}}",
        );
        for guard in [OpenCodeV1, OpenCodeV2] {
            assert_eq!(
                check_opencode_plan(guard, "opencode", std::slice::from_ref(&ok), true).unwrap(),
                Vec::<String>::new()
            );
            for (table, key) in [
                ("agent", "disable"),
                ("agent", "permission"),
                ("mode", "tools"),
                ("agent", "prompt"),
            ] {
                let bad = json(
                    "project opencode.json",
                    SourceScope::Project,
                    &format!("{{\"{table}\": {{\"plan\": {{\"{key}\": true}}}}}}"),
                );
                let selector = guard_conflict(guard, &[bad], false);
                assert!(
                    selector.ends_with(&format!("{table}.plan.{key}")),
                    "{selector}"
                );
            }
            let replaced = json(
                "global opencode.json",
                SourceScope::Global,
                "{\"agent\": {\"plan\": false}}",
            );
            guard_conflict(guard, &[replaced], false);
            let md = OpenCodeConfigSource {
                origin: "project agents/plan.md".into(),
                kind: SourceKind::MarkdownAgent,
                scope: SourceScope::Project,
                text: "---\n---\nbuild things".into(),
            };
            assert_eq!(
                guard_conflict(guard, &[md], false),
                "project agents/plan.md"
            );
            guard_conflict(
                guard,
                &[json(
                    "project opencode.jsonc",
                    SourceScope::Project,
                    "{ not json",
                )],
                false,
            );
            guard_conflict(guard, &[json("x", SourceScope::Project, "[1, 2]")], false);
        }
        assert!(check_opencode_plan(
            PlanConfigGuard::None,
            "claude",
            &[json("x", SourceScope::Project, "nonsense")],
            true
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn opencode_v2_default_agent_is_overridden_privately_or_refused_when_it_outranks() {
        use PlanConfigGuard::{OpenCodeV1, OpenCodeV2};
        let private = json(
            "private global opencode.json",
            SourceScope::PrivateGlobal,
            "{\"default_agent\": \"build\"}",
        );
        let notices = check_opencode_plan(
            OpenCodeV2,
            "opencode-v2",
            std::slice::from_ref(&private),
            true,
        )
        .unwrap();
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].contains("overrode default_agent=\"build\""),
            "{notices:?}"
        );
        for scope in [SourceScope::Project, SourceScope::Environment] {
            let outranks = json(
                "OPENCODE_CONFIG_CONTENT",
                scope,
                "{\"default_agent\": \"build\"}",
            );
            let selector = guard_conflict(OpenCodeV2, std::slice::from_ref(&outranks), true);
            assert!(selector.contains("default_agent"), "{selector}");
            // Headless v2 and 1.x select Plan with an explicit --agent plan.
            assert!(check_opencode_plan(
                OpenCodeV2,
                "opencode-v2",
                std::slice::from_ref(&outranks),
                false
            )
            .unwrap()
            .is_empty());
            assert!(
                check_opencode_plan(OpenCodeV1, "opencode", &[outranks], true)
                    .unwrap()
                    .is_empty()
            );
        }
        let already = json(
            "project opencode.json",
            SourceScope::Project,
            "{\"default_agent\": \"plan\"}",
        );
        assert!(
            check_opencode_plan(OpenCodeV2, "opencode-v2", &[already], true)
                .unwrap()
                .is_empty()
        );
    }
}

/// A definition's exact conversation selector; no latest/picker fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumePosition {
    Options,
    SubcommandTail,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdForm {
    Separate,
    Equals(String),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeSelector {
    pub before_id: Vec<String>,
    pub form: IdForm,
    pub after_id: Vec<String>,
    pub position: ResumePosition,
    pub no_auto_continue: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeResumeSpec {
    pub interactive: Option<ResumeSelector>,
    pub headless: Option<ResumeSelector>,
    pub model_fallback: Option<ResumeSelector>,
}
impl AgentDefinition {
    pub fn resume_selector(
        &self,
        run: &super::AgentRunOptions,
    ) -> Result<&ResumeSelector, EngineError> {
        let unavailable = || EngineError::NativeResumeUnsupported {
            agent: self.name.clone(),
            reason: "exact native resume is unavailable for this launch route".into(),
        };
        if run.launch_mode == crate::data::config::agents::LaunchMode::Acp {
            return Err(unavailable());
        }
        let spec = self.native_resume.as_ref().ok_or_else(unavailable)?;
        let selector = if self.uses_model_fallback(run.non_interactive, run.model.is_some()) {
            &spec.model_fallback
        } else if run.non_interactive {
            &spec.headless
        } else {
            &spec.interactive
        };
        selector.as_ref().ok_or_else(unavailable)
    }
    pub fn native_resume_tokens(
        &self,
        run: &super::AgentRunOptions,
    ) -> Result<Option<(Vec<String>, ResumePosition)>, EngineError> {
        let Some(id) = &run.native_session else {
            return Ok(None);
        };
        let selector = self.resume_selector(run)?;
        if id.as_str().starts_with('-') && selector.form == IdForm::Separate {
            return Err(EngineError::Config(
                "native session id starting with '-' requires an equals selector".into(),
            ));
        }
        let mut tokens = selector.before_id.clone();
        tokens.push(match &selector.form {
            IdForm::Separate => id.as_str().into(),
            IdForm::Equals(flag) => format!("{flag}={}", id.as_str()),
        });
        tokens.extend(selector.after_id.clone());
        Ok(Some((tokens, selector.position.clone())))
    }
    pub fn verify_resume_flags(
        &self,
        run: &super::AgentRunOptions,
        flags: &[String],
    ) -> Result<(), EngineError> {
        if run.native_session.is_some()
            && flags.iter().any(|s| {
                matches!(
                    s.split('=').next().unwrap_or(s),
                    "-r" | "-c"
                        | "-s"
                        | "--resume"
                        | "--continue"
                        | "--session"
                        | "--id"
                        | "--conversation"
                        | "--fork-session"
                        | "resume"
                        | "--last"
                )
            })
        {
            return Err(EngineError::Config("--agent-session already selects the conversation; remove provider resume selectors from agent flags".into()));
        }
        Ok(())
    }
}
