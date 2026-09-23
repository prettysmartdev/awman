//! Per-agent translation matrix — the only place in `src/engine/` that
//! branches on agent name. Adding a new agent is a single-file edit here.

use crate::data::config::repo::LaunchMode;
use crate::engine::container::options::{AutoMode, Entrypoint, ModelFlagForm, PlanMode, YoloMode};
use crate::engine::error::EngineError;

/// Supported agent names — derived from the legacy `Agent` enum in
/// `oldsrc/cli.rs`.
pub const SUPPORTED_AGENTS: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "maki",
    "gemini",
    "copilot",
    "crush",
    "cline",
    "antigravity",
];

/// How awman injects the combined context system prompt into an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemPromptMode {
    /// Append to default system prompt by mounting the prompt as a file and
    /// referencing it with a CLI flag (e.g. claude `--append-system-prompt-file <path>`).
    Append,
    /// Append to default system prompt by passing it inline as `<key>=<text>`
    /// to a CLI flag (e.g. codex `--config developer_instructions=<text>`).
    AppendInline { key: &'static str },
    /// Replace default system prompt with inline text (destructive; preamble
    /// prepended). Used by agents like cline `--system <text>`.
    Replace,
    /// File-based: write AGENTS.md into context dir.
    AgentsMd,
    /// Env var pointing to a file.
    EnvFile { var: &'static str },
    /// Extra workspace dir flag (agy --add-dir) + AGENTS.md.
    AddDir { flag: &'static str },
    /// Not supported.
    Unsupported,
}

/// Per-agent metadata used by `AgentEngine::build_options`.
#[derive(Debug, Clone)]
pub struct AgentMatrix {
    pub agent: &'static str,
    /// Bare interactive entrypoint (e.g. `["claude"]`, `["copilot", "-i"]`).
    pub interactive_entrypoint: Vec<&'static str>,
    /// Print/non-interactive entrypoint suffix (e.g. `--print` for Claude).
    pub non_interactive_flag: Option<&'static str>,
    /// Whether plan mode is supported and which flag to emit.
    pub plan_flag: Option<&'static [&'static str]>,
    /// Yolo flag (e.g. `--dangerously-skip-permissions`). `None` means yolo
    /// silently equates to no permission flags.
    pub yolo_flag: Option<&'static str>,
    /// Auto flag (e.g. `--permission-mode auto`).
    pub auto_flag: Option<&'static [&'static str]>,
    /// Disallowed-tools flag name (e.g. `--disallowedTools`).
    pub disallowed_tools_flag: Option<&'static str>,
    /// Allowed-tools flag name (e.g. `--allowedTools`).
    pub allowed_tools_flag: Option<&'static str>,
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
    /// Currently `false` for every shipped agent. Set to `true` once an agent
    /// CLI is verified to accept a newline-terminated prompt on its existing
    /// stdin without losing state. The wiring on the Docker side keeps the
    /// spawned subprocess's stdin alive for re-injection.
    pub supports_stdin_injection: bool,
    /// Whether this agent's CLI can run in ACP (Agent Client Protocol) mode.
    pub supports_acp: bool,
    /// Entrypoint used when launching in ACP mode. `None` when ACP is not
    /// supported by this agent.
    pub acp_entrypoint: Option<Vec<&'static str>>,
    /// How context system prompts are delivered to this agent.
    pub system_prompt_delivery: SystemPromptMode,
    /// CLI flag for system prompt delivery (e.g. `--append-system-prompt-file`).
    pub system_prompt_flag: Option<&'static str>,
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
    pub skills_mount: Option<&'static str>,
    /// Which host credential scheme, if any, awman passes through for this
    /// agent. Consumed by `engine::auth::keychain`.
    pub credential_source: CredentialSource,
    /// Full argv of the sanctioned host-side ready ping, **excluding** the
    /// trailing greeting, which the pinger appends. Element 0 is the binary.
    ///
    /// Note that antigravity's ping binary is `antigravity`, not the `agy`
    /// binary named by `interactive_entrypoint`. That is what the pre-F-32
    /// catch-all arm in `ready::ping_command` produced, and F-32 is a
    /// behaviour-preserving change, so it is reproduced exactly. It means the
    /// antigravity ping can only ever report `NotInstalled` on a host where
    /// the binary is called `agy`; correcting it is a behaviour change and
    /// needs its own work item.
    pub ping_argv: &'static [&'static str],
    /// Environment variables always set for this agent, on every paradigm.
    pub static_env: &'static [(&'static str, &'static str)],
    /// Whether a mixin-kit launch under a sandbox-class runtime can deliver
    /// plan/yolo/auto as a settings-file permission mode that awman renders
    /// (claude's `permissions.defaultMode`). `false` means the request is
    /// reported as an unsupported note instead.
    pub sandbox_permission_mode_supported: bool,
    /// Note surfaced when `--model` is requested under a sandbox-class
    /// runtime and the agent's built-in template has no mixin-safe model
    /// config. `None` for agents whose model flag survives the sandbox path.
    pub sandbox_model_note: Option<&'static str>,
    /// Allowlist of provider auth env vars accepted via `env(VAR)` overlays
    /// for launch-time auto-auth under a sandbox-class runtime. Empty for
    /// agent-kit agents, which do not participate.
    ///
    /// Each var satisfies two constraints, both verified against the Docker
    /// Sandboxes credentials docs: it maps to an sbx well-known service
    /// (`engine::auth::service_for_credential`), and the agent's base kit
    /// routes that service through the host proxy (the kit template's
    /// `network.allowedDomains` / `environment.proxyManaged`).
    pub sandbox_auth_env_vars: &'static [&'static str],
    /// Warning surfaced once per launch for an agent its vendor has
    /// deprecated. `None` for every supported agent.
    pub deprecation_note: Option<&'static str>,
}

/// How awman passes an agent's host settings directory into a container.
///
/// Five of the nine agents take the plain `Direct` strategy: mount
/// `$HOME/<path>` at `<container_home>/<path>` when the host directory
/// exists. Claude and antigravity stage a sanitized or keychain-seeded copy
/// instead, and those two strategies stay explicit in `OverlayEngine`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsMount {
    /// No host settings overlay for this agent (copilot, maki).
    None,
    /// Mount `$HOME/<path>` read-write at `<container_home>/<path>` when the
    /// host directory exists.
    Direct(&'static str),
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

#[derive(Debug, Clone, Copy)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractiveSeedDelivery {
    /// Append the prompt as the final positional argv argument.
    Positional,
    /// Deliver the prompt as a flag pair: `<flag> <text>`.
    Flag(&'static str),
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

impl AgentMatrix {
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
        if matches!(run.plan, Some(PlanMode::Enabled)) && self.plan_flag.is_none() {
            return Err(EngineError::PlanModeUnsupported {
                agent: self.agent.to_string(),
            });
        }
        // Every launch path funnels through an option builder, so this single
        // guard keeps an agent that does not speak ACP out of a container with
        // JSON-RPC framing regardless of any Layer 2 pre-flight decision.
        if matches!(run.launch_mode, LaunchMode::Acp) {
            if !self.supports_acp {
                return Err(EngineError::AcpUnsupported {
                    agent: self.agent.to_string(),
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

    /// Resolve the requested plan/yolo/auto modes into this agent's literal
    /// argv flags, in the fixed yolo → auto → plan order both option builders
    /// used before F-32. Empty when the agent declares no flag for any
    /// requested mode (a silent no-op, by design — see `yolo_flag`).
    pub fn mode_flags(&self, run: &super::AgentRunOptions) -> Vec<String> {
        let mut flags = Vec::new();
        if matches!(run.yolo, Some(YoloMode::Enabled)) {
            if let Some(flag) = self.yolo_flag {
                flags.push(flag.to_string());
            }
        }
        if matches!(run.auto, Some(AutoMode::Enabled)) {
            if let Some(f) = self.auto_flag {
                flags.extend(f.iter().map(|s| s.to_string()));
            }
        }
        if matches!(run.plan, Some(PlanMode::Enabled)) {
            if let Some(f) = self.plan_flag {
                flags.extend(f.iter().map(|s| s.to_string()));
            }
        }
        flags
    }

    /// The sanctioned host ping's `(binary, argv)` for this agent, with the
    /// greeting appended as the final argument. Pure and side-effect free so
    /// it can be asserted in tests (INV-8).
    pub fn ping_command(&self, greeting: &str) -> (String, Vec<String>) {
        let (bin, prefix) = self
            .ping_argv
            .split_first()
            .expect("every agent declares a ping binary");
        let mut args: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
        args.push(greeting.to_string());
        (bin.to_string(), args)
    }
}

/// Lookup the matrix entry for a known agent name.
pub fn matrix_for(agent: &str) -> Result<AgentMatrix, EngineError> {
    Ok(match agent {
        "claude" => AgentMatrix {
            agent: "claude",
            interactive_entrypoint: vec!["claude"],
            non_interactive_flag: Some("--print"),
            plan_flag: Some(&["--permission-mode", "plan"]),
            yolo_flag: Some("--dangerously-skip-permissions"),
            auto_flag: Some(&["--permission-mode", "auto"]),
            disallowed_tools_flag: Some("--disallowedTools"),
            allowed_tools_flag: Some("--allowedTools"),
            model_flag: ModelFlagDelivery::SpaceArg,
            interactive_seed_delivery: InteractiveSeedDelivery::Positional,
            supports_stdin_injection: false,
            // TODO(acp): verify and wire up if/when claude ships ACP support
            supports_acp: false,
            acp_entrypoint: None,
            system_prompt_delivery: SystemPromptMode::Append,
            system_prompt_flag: Some("--append-system-prompt-file"),
            sbx_kit_kind: SbxKitKind::Mixin,
            settings_mount: SettingsMount::Claude,
            skills_mount: Some(".claude/commands"),
            credential_source: CredentialSource::ClaudeKeychainOauth,
            ping_argv: &["claude", "--model", "haiku", "--print"],
            static_env: &[],
            sandbox_permission_mode_supported: true,
            sandbox_model_note: None,
            sandbox_auth_env_vars: &["ANTHROPIC_API_KEY"],
            deprecation_note: None,
        },
        "codex" => AgentMatrix {
            agent: "codex",
            interactive_entrypoint: vec!["codex"],
            non_interactive_flag: Some("exec"),
            plan_flag: Some(&["--approval-mode", "plan"]),
            // `--full-auto` is exec-only (and deprecated); the global flag
            // below is accepted by both interactive `codex` and `codex exec`.
            yolo_flag: Some("--dangerously-bypass-approvals-and-sandbox"),
            // Successor to the deprecated `--full-auto` preset: writes inside
            // the workspace are auto-approved, escalations still prompt.
            auto_flag: Some(&["--sandbox", "workspace-write"]),
            disallowed_tools_flag: None,
            allowed_tools_flag: None,
            model_flag: ModelFlagDelivery::SpaceArg,
            interactive_seed_delivery: InteractiveSeedDelivery::Positional,
            supports_stdin_injection: false,
            // TODO(acp): verify and wire up if/when codex ships ACP support
            supports_acp: false,
            acp_entrypoint: None,
            // codex takes the system prompt as `--config developer_instructions=<text>`.
            system_prompt_delivery: SystemPromptMode::AppendInline {
                key: "developer_instructions",
            },
            system_prompt_flag: Some("--config"),
            sbx_kit_kind: SbxKitKind::Mixin,
            settings_mount: SettingsMount::Direct(".codex"),
            skills_mount: Some(".codex/skills"),
            credential_source: CredentialSource::None,
            ping_argv: &["codex", "exec"],
            static_env: &[],
            sandbox_permission_mode_supported: false,
            sandbox_model_note: None,
            sandbox_auth_env_vars: &["OPENAI_API_KEY"],
            deprecation_note: None,
        },
        "opencode" => AgentMatrix {
            agent: "opencode",
            interactive_entrypoint: vec!["opencode"],
            non_interactive_flag: Some("run"),
            plan_flag: None,
            yolo_flag: None,
            auto_flag: None,
            disallowed_tools_flag: None,
            allowed_tools_flag: None,
            model_flag: ModelFlagDelivery::SpaceArg,
            // opencode's bare command treats a positional as a project dir, so a
            // seeded prompt must go through `--prompt <text>`; a positional
            // prompt makes opencode `open()` the prompt as a path (ENAMETOOLONG).
            interactive_seed_delivery: InteractiveSeedDelivery::Flag("--prompt"),
            supports_stdin_injection: false,
            // TODO(acp): verify and wire up if/when opencode ships ACP support
            supports_acp: false,
            acp_entrypoint: None,
            system_prompt_delivery: SystemPromptMode::AgentsMd,
            system_prompt_flag: None,
            sbx_kit_kind: SbxKitKind::Mixin,
            settings_mount: SettingsMount::Direct(".config/opencode"),
            skills_mount: Some(".config/opencode/commands"),
            credential_source: CredentialSource::None,
            ping_argv: &["opencode", "run"],
            static_env: &[],
            sandbox_permission_mode_supported: false,
            sandbox_model_note: None,
            sandbox_auth_env_vars: &["ANTHROPIC_API_KEY"],
            deprecation_note: None,
        },
        "maki" => AgentMatrix {
            agent: "maki",
            interactive_entrypoint: vec!["maki"],
            non_interactive_flag: None,
            plan_flag: None,
            yolo_flag: Some("--yolo"),
            auto_flag: None,
            disallowed_tools_flag: None,
            allowed_tools_flag: None,
            model_flag: ModelFlagDelivery::SpaceArg,
            interactive_seed_delivery: InteractiveSeedDelivery::Positional,
            supports_stdin_injection: false,
            // TODO(acp): verify and wire up if/when maki ships ACP support
            supports_acp: false,
            acp_entrypoint: None,
            system_prompt_delivery: SystemPromptMode::Unsupported,
            system_prompt_flag: None,
            sbx_kit_kind: SbxKitKind::Agent,
            settings_mount: SettingsMount::None,
            skills_mount: None,
            credential_source: CredentialSource::None,
            ping_argv: &["maki", "--print"],
            static_env: &[],
            sandbox_permission_mode_supported: false,
            sandbox_model_note: None,
            sandbox_auth_env_vars: &[],
            deprecation_note: None,
        },
        "gemini" => AgentMatrix {
            agent: "gemini",
            interactive_entrypoint: vec!["gemini"],
            non_interactive_flag: None,
            plan_flag: Some(&["--approval-mode=plan"]),
            yolo_flag: Some("--yolo"),
            auto_flag: Some(&["--approval-mode=auto_edit"]),
            disallowed_tools_flag: None,
            allowed_tools_flag: None,
            model_flag: ModelFlagDelivery::SpaceArg,
            interactive_seed_delivery: InteractiveSeedDelivery::Positional,
            supports_stdin_injection: false,
            // TODO(acp): verify and wire up if/when gemini ships ACP support
            supports_acp: false,
            acp_entrypoint: None,
            system_prompt_delivery: SystemPromptMode::EnvFile {
                var: "GEMINI_SYSTEM_MD",
            },
            system_prompt_flag: None,
            sbx_kit_kind: SbxKitKind::Mixin,
            settings_mount: SettingsMount::Direct(".gemini"),
            skills_mount: Some(".gemini/commands"),
            credential_source: CredentialSource::None,
            ping_argv: &["gemini", "-p"],
            static_env: &[],
            sandbox_permission_mode_supported: false,
            sandbox_model_note: None,
            sandbox_auth_env_vars: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            deprecation_note: Some(
                "The 'gemini' agent is deprecated by Google. Migrate to \
                 'antigravity' — run 'awman chat --agent antigravity' (or \
                 'awman config set agent antigravity' to change your default).",
            ),
        },
        "copilot" => AgentMatrix {
            agent: "copilot",
            interactive_entrypoint: vec!["copilot", "-i"],
            non_interactive_flag: None,
            plan_flag: Some(&["--plan"]),
            yolo_flag: Some("--autopilot"),
            auto_flag: None,
            disallowed_tools_flag: None,
            allowed_tools_flag: None,
            model_flag: ModelFlagDelivery::SpaceArg,
            interactive_seed_delivery: InteractiveSeedDelivery::Positional,
            supports_stdin_injection: false,
            // TODO(acp): verify and wire up if/when copilot ships ACP support
            supports_acp: false,
            acp_entrypoint: None,
            system_prompt_delivery: SystemPromptMode::EnvFile {
                var: "COPILOT_CUSTOM_INSTRUCTIONS_DIRS",
            },
            system_prompt_flag: None,
            sbx_kit_kind: SbxKitKind::Mixin,
            settings_mount: SettingsMount::None,
            skills_mount: Some(".copilot/instructions"),
            credential_source: CredentialSource::None,
            ping_argv: &["copilot", "-p", "-i"],
            static_env: &[("COPILOT_OFFLINE", "true")],
            sandbox_permission_mode_supported: false,
            sandbox_model_note: Some(
                "--model cannot be applied to copilot under the sandbox runtime (no \
                 mixin-safe config); use the /model slash command inside the session \
                 instead",
            ),
            sandbox_auth_env_vars: &["GH_TOKEN", "GITHUB_TOKEN"],
            deprecation_note: None,
        },
        "crush" => AgentMatrix {
            agent: "crush",
            interactive_entrypoint: vec!["crush"],
            non_interactive_flag: Some("run"),
            plan_flag: None,
            yolo_flag: Some("--yolo"),
            auto_flag: None,
            disallowed_tools_flag: None,
            allowed_tools_flag: None,
            model_flag: ModelFlagDelivery::SpaceArg,
            interactive_seed_delivery: InteractiveSeedDelivery::Positional,
            supports_stdin_injection: false,
            // TODO(acp): verify and wire up if/when crush ships ACP support
            supports_acp: false,
            acp_entrypoint: None,
            system_prompt_delivery: SystemPromptMode::Unsupported,
            system_prompt_flag: None,
            sbx_kit_kind: SbxKitKind::Agent,
            settings_mount: SettingsMount::Direct(".config/crush"),
            skills_mount: Some(".config/crush/commands"),
            credential_source: CredentialSource::None,
            ping_argv: &["crush", "run"],
            static_env: &[],
            sandbox_permission_mode_supported: false,
            sandbox_model_note: None,
            sandbox_auth_env_vars: &[],
            deprecation_note: None,
        },
        "cline" => AgentMatrix {
            agent: "cline",
            interactive_entrypoint: vec!["cline"],
            non_interactive_flag: Some("task"),
            plan_flag: Some(&["--plan"]),
            yolo_flag: Some("--yolo"),
            auto_flag: Some(&["--auto-approve-all"]),
            disallowed_tools_flag: None,
            allowed_tools_flag: None,
            model_flag: ModelFlagDelivery::SpaceArg,
            interactive_seed_delivery: InteractiveSeedDelivery::Positional,
            supports_stdin_injection: false,
            supports_acp: true,
            acp_entrypoint: Some(vec!["cline", "--acp"]),
            system_prompt_delivery: SystemPromptMode::Replace,
            system_prompt_flag: Some("--system"),
            sbx_kit_kind: SbxKitKind::Agent,
            settings_mount: SettingsMount::Direct(".cline/data"),
            skills_mount: Some(".cline/skills"),
            credential_source: CredentialSource::None,
            ping_argv: &["cline", "task"],
            static_env: &[],
            sandbox_permission_mode_supported: false,
            sandbox_model_note: None,
            sandbox_auth_env_vars: &[],
            deprecation_note: None,
        },
        "antigravity" => AgentMatrix {
            // Verified against `agy --help` (v1.0.x). Flags actually accepted:
            //   --print / -p / --prompt           (non-interactive)
            //   --prompt-interactive / -i         (interactive seed)
            //   --dangerously-skip-permissions    (yolo)
            //   --print-timeout                   (default 5m, not surfaced here)
            //   --continue / --conversation       (session resume, not wired)
            //   --add-dir                         (extra workspace dirs)
            //   --log-file, --sandbox
            // There is **no** `--approval-mode` / `--plan` / `--auto-edit`
            // CLI flag — those are settings.json (`toolPermission`) values
            // surfaced through agy's interactive `/...` slash commands.
            // Don't emit them; the binary just dumps `--help` and treats the
            // prompt as the agy executable name. Leaving plan/auto as `None`
            // keeps non-yolo modes a silent no-op (matches opencode/maki).
            agent: "antigravity",
            interactive_entrypoint: vec!["agy"],
            non_interactive_flag: Some("--print"),
            plan_flag: None,
            yolo_flag: Some("--dangerously-skip-permissions"),
            auto_flag: None,
            disallowed_tools_flag: None,
            allowed_tools_flag: None,
            model_flag: ModelFlagDelivery::Unsupported,
            // agy accepts `--prompt-interactive`/`-i` for an interactive seed,
            // but awman has always seeded it positionally; keep that behavior
            // until the flag form is verified end-to-end.
            interactive_seed_delivery: InteractiveSeedDelivery::Positional,
            supports_stdin_injection: false,
            // TODO(acp): verify and wire up if/when antigravity ships ACP support
            supports_acp: false,
            acp_entrypoint: None,
            system_prompt_delivery: SystemPromptMode::AddDir { flag: "--add-dir" },
            system_prompt_flag: None,
            sbx_kit_kind: SbxKitKind::Agent,
            settings_mount: SettingsMount::Antigravity,
            skills_mount: Some(".gemini/antigravity-cli/skills"),
            credential_source: CredentialSource::AntigravityKeychainFile,
            // The ping binary is `antigravity`, NOT the `agy` interactive
            // entrypoint. That is what the pre-F-32 fallback arm produced and
            // it is preserved verbatim here; see the ping_argv doc comment.
            ping_argv: &["antigravity", "--print"],
            static_env: &[],
            sandbox_permission_mode_supported: false,
            sandbox_model_note: None,
            sandbox_auth_env_vars: &[],
            deprecation_note: None,
        },
        other => {
            return Err(EngineError::Other(format!(
                "unknown agent '{other}'; supported: {}",
                SUPPORTED_AGENTS.join(", ")
            )))
        }
    })
}

/// Build the entrypoint with optional non-interactive shape.
pub fn entrypoint_for(matrix: &AgentMatrix, non_interactive: bool) -> Entrypoint {
    let mut parts: Vec<String> = matrix
        .interactive_entrypoint
        .iter()
        .map(|s| s.to_string())
        .collect();
    if non_interactive {
        if let Some(flag) = matrix.non_interactive_flag {
            // For agents like Codex (`codex exec`) the "flag" is actually a
            // subcommand inserted after the binary; for Claude it's `--print`
            // appended after the args. Both append-at-end shapes work here
            // because the seeded prompt is positional.
            parts.push(flag.to_string());
        }
    }
    Entrypoint(parts)
}

/// Build the entrypoint used for an ACP launch.
pub fn entrypoint_for_acp(matrix: &AgentMatrix) -> Result<Entrypoint, EngineError> {
    let parts = matrix
        .acp_entrypoint
        .as_ref()
        .ok_or_else(|| EngineError::AcpUnsupported {
            agent: matrix.agent.to_string(),
        })?
        .iter()
        .map(|s| s.to_string())
        .collect();
    Ok(Entrypoint(parts))
}

/// Translate a model name into the matrix-specific flag form.
pub fn model_flag_for(matrix: &AgentMatrix, model: &str) -> Result<ModelFlagForm, EngineError> {
    match matrix.model_flag {
        ModelFlagDelivery::SpaceArg => Ok(ModelFlagForm::Argument(model.to_string())),
        // `--model=NAME` is one self-contained argv token — Shorthand, not
        // Argument (the backends prepend `--model` to Argument values).
        ModelFlagDelivery::EqArg => Ok(ModelFlagForm::Shorthand(format!("--model={model}"))),
        ModelFlagDelivery::Unsupported => Err(EngineError::Other(format!(
            "agent '{}' does not support a model flag",
            matrix.agent
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_supports_all_agents() {
        for a in SUPPORTED_AGENTS {
            let matrix = matrix_for(a).expect("matrix missing for agent");
            assert_eq!(
                matrix.acp_entrypoint.is_some(),
                matrix.supports_acp,
                "ACP metadata invariant failed for {a}"
            );
        }
    }

    #[test]
    fn cline_has_verified_acp_entrypoint() {
        let matrix = matrix_for("cline").unwrap();
        assert!(matrix.supports_acp);
        assert_eq!(matrix.acp_entrypoint, Some(vec!["cline", "--acp"]));
        assert_eq!(
            entrypoint_for_acp(&matrix).unwrap(),
            Entrypoint(vec!["cline".to_string(), "--acp".to_string()])
        );
    }

    #[test]
    fn unsupported_agents_have_no_acp_entrypoint() {
        for agent in SUPPORTED_AGENTS {
            if *agent == "cline" {
                continue;
            }
            let matrix = matrix_for(agent).unwrap();
            assert!(!matrix.supports_acp, "unexpected ACP support for {agent}");
            assert!(matrix.acp_entrypoint.is_none());
            assert!(matches!(
                entrypoint_for_acp(&matrix),
                Err(EngineError::AcpUnsupported { .. })
            ));
        }
    }

    #[test]
    fn unknown_agent_errors() {
        assert!(matrix_for("totallymade-up").is_err());
    }

    #[test]
    fn opencode_plan_unsupported() {
        let m = matrix_for("opencode").unwrap();
        assert!(m.plan_flag.is_none());
    }

    #[test]
    fn opencode_interactive_seed_uses_prompt_flag() {
        let m = matrix_for("opencode").unwrap();
        assert_eq!(
            m.interactive_seed_delivery,
            InteractiveSeedDelivery::Flag("--prompt"),
            "opencode must seed interactive prompts via `--prompt`; a bare \
             positional is a project dir and opencode open()s it (ENAMETOOLONG)"
        );
    }

    #[test]
    fn only_opencode_uses_a_seed_flag_others_are_positional() {
        for a in SUPPORTED_AGENTS {
            let m = matrix_for(a).unwrap();
            match a {
                &"opencode" => assert!(matches!(
                    m.interactive_seed_delivery,
                    InteractiveSeedDelivery::Flag(_)
                )),
                _ => assert_eq!(
                    m.interactive_seed_delivery,
                    InteractiveSeedDelivery::Positional,
                    "{a} must seed interactive prompts positionally"
                ),
            }
        }
    }

    #[test]
    fn codex_yolo_flag_is_dangerously_bypass_approvals_and_sandbox() {
        let m = matrix_for("codex").unwrap();
        assert_eq!(
            m.yolo_flag,
            Some("--dangerously-bypass-approvals-and-sandbox"),
            "codex yolo_flag must be --dangerously-bypass-approvals-and-sandbox; \
             --full-auto is exec-only, deprecated, and rejected by interactive codex"
        );
    }

    #[test]
    fn codex_auto_flag_is_sandbox_workspace_write() {
        let m = matrix_for("codex").unwrap();
        assert_eq!(
            m.auto_flag,
            Some(&["--sandbox", "workspace-write"][..]),
            "codex auto_flag must be --sandbox workspace-write (successor to deprecated --full-auto)"
        );
    }

    #[test]
    fn antigravity_yolo_flag_is_dangerously_skip_permissions() {
        let m = matrix_for("antigravity").unwrap();
        assert_eq!(
            m.yolo_flag,
            Some("--dangerously-skip-permissions"),
            "antigravity yolo_flag must be --dangerously-skip-permissions"
        );
    }

    #[test]
    fn antigravity_non_interactive_flag_is_print() {
        let m = matrix_for("antigravity").unwrap();
        assert_eq!(
            m.non_interactive_flag,
            Some("--print"),
            "antigravity non_interactive_flag must be --print"
        );
    }

    #[test]
    fn antigravity_model_flag_unsupported_returns_err() {
        let m = matrix_for("antigravity").unwrap();
        let result = model_flag_for(&m, "gemini-3.5-flash");
        assert!(
            result.is_err(),
            "model_flag_for antigravity must return Err (Unsupported); got {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("antigravity"),
            "error must name the agent; got: {msg}"
        );
        assert!(
            msg.contains("does not support a model flag"),
            "error must say 'does not support a model flag'; got: {msg}"
        );
    }

    #[test]
    fn antigravity_interactive_entrypoint_is_agy() {
        let m = matrix_for("antigravity").unwrap();
        assert_eq!(
            m.interactive_entrypoint,
            vec!["agy"],
            "antigravity interactive_entrypoint must be [\"agy\"]"
        );
    }

    // ─── WI-0087: system prompt delivery matrix ────────────────────────────────

    #[test]
    fn all_supported_agents_have_valid_system_prompt_delivery() {
        for agent in SUPPORTED_AGENTS {
            let m = matrix_for(agent).expect("matrix must exist for all SUPPORTED_AGENTS");
            // Just constructing the matrix (above) validates no panic.
            // Additionally verify the delivery is a valid variant by matching it.
            let _valid = match &m.system_prompt_delivery {
                SystemPromptMode::Append
                | SystemPromptMode::AppendInline { .. }
                | SystemPromptMode::Replace
                | SystemPromptMode::AgentsMd
                | SystemPromptMode::EnvFile { .. }
                | SystemPromptMode::AddDir { .. }
                | SystemPromptMode::Unsupported => true,
            };
        }
    }

    #[test]
    fn codex_system_prompt_delivery_is_append_inline_with_developer_instructions() {
        let m = matrix_for("codex").unwrap();
        assert!(
            matches!(
                m.system_prompt_delivery,
                SystemPromptMode::AppendInline {
                    key: "developer_instructions"
                }
            ),
            "codex must use AppendInline {{ key: developer_instructions }}; got {:?}",
            m.system_prompt_delivery
        );
        assert_eq!(
            m.system_prompt_flag,
            Some("--config"),
            "codex system_prompt_flag must be --config"
        );
    }

    #[test]
    fn claude_system_prompt_delivery_is_append() {
        let m = matrix_for("claude").unwrap();
        assert_eq!(
            m.system_prompt_delivery,
            SystemPromptMode::Append,
            "claude must use Append delivery"
        );
        assert_eq!(
            m.system_prompt_flag,
            Some("--append-system-prompt-file"),
            "claude system_prompt_flag must be --append-system-prompt-file"
        );
    }

    #[test]
    fn maki_system_prompt_delivery_is_unsupported() {
        let m = matrix_for("maki").unwrap();
        assert_eq!(
            m.system_prompt_delivery,
            SystemPromptMode::Unsupported,
            "maki must use Unsupported delivery"
        );
        assert!(
            m.system_prompt_flag.is_none(),
            "maki must have no system_prompt_flag"
        );
    }

    #[test]
    fn cline_system_prompt_delivery_is_replace() {
        let m = matrix_for("cline").unwrap();
        assert_eq!(
            m.system_prompt_delivery,
            SystemPromptMode::Replace,
            "cline must use Replace delivery"
        );
        assert_eq!(
            m.system_prompt_flag,
            Some("--system"),
            "cline system_prompt_flag must be --system"
        );
    }

    #[test]
    fn crush_system_prompt_delivery_is_unsupported() {
        let m = matrix_for("crush").unwrap();
        assert_eq!(
            m.system_prompt_delivery,
            SystemPromptMode::Unsupported,
            "crush must use Unsupported delivery"
        );
    }

    #[test]
    fn antigravity_system_prompt_delivery_is_add_dir() {
        let m = matrix_for("antigravity").unwrap();
        assert!(
            matches!(
                m.system_prompt_delivery,
                SystemPromptMode::AddDir { flag: "--add-dir" }
            ),
            "antigravity must use AddDir {{ flag: \"--add-dir\" }}; got {:?}",
            m.system_prompt_delivery
        );
    }

    #[test]
    fn opencode_system_prompt_delivery_is_agents_md() {
        let m = matrix_for("opencode").unwrap();
        assert_eq!(
            m.system_prompt_delivery,
            SystemPromptMode::AgentsMd,
            "opencode must use AgentsMd delivery"
        );
    }

    // ─── WI-0114 F-32: the fields that absorbed the four per-agent tables ───
    //
    // Each of these asserts the WHOLE table, agent by agent, against the
    // values the pre-F-32 `match agent.as_str()` arms produced. They are the
    // regression guard the work item asks for: a wrong entry here is a silent
    // behaviour change in overlay mounting, credential delivery or the
    // sanctioned host ping.

    #[test]
    fn settings_mount_table_matches_the_pre_f32_arms() {
        let expected: &[(&str, SettingsMount)] = &[
            ("claude", SettingsMount::Claude),
            ("codex", SettingsMount::Direct(".codex")),
            ("opencode", SettingsMount::Direct(".config/opencode")),
            ("maki", SettingsMount::None),
            ("gemini", SettingsMount::Direct(".gemini")),
            ("copilot", SettingsMount::None),
            ("crush", SettingsMount::Direct(".config/crush")),
            ("cline", SettingsMount::Direct(".cline/data")),
            ("antigravity", SettingsMount::Antigravity),
        ];
        assert_eq!(expected.len(), SUPPORTED_AGENTS.len());
        for (agent, want) in expected {
            assert_eq!(
                matrix_for(agent).unwrap().settings_mount,
                *want,
                "settings_mount for {agent}"
            );
        }
    }

    /// The five `Direct` agents mount at the same path on host and in the
    /// container. That equality is what let the five near-identical arms
    /// collapse into one; if a future agent needs them to differ, `Direct`
    /// has to grow a second path rather than the loop growing a special case.
    #[test]
    fn direct_settings_mounts_are_relative_paths_without_a_leading_slash() {
        for agent in SUPPORTED_AGENTS {
            if let SettingsMount::Direct(rel) = matrix_for(agent).unwrap().settings_mount {
                assert!(
                    !rel.starts_with('/') && !rel.is_empty(),
                    "{agent}'s Direct settings mount must be relative to $HOME; got {rel:?}"
                );
            }
        }
    }

    #[test]
    fn skills_mount_table_matches_the_pre_f32_arms() {
        let expected: &[(&str, Option<&str>)] = &[
            ("claude", Some(".claude/commands")),
            ("codex", Some(".codex/skills")),
            ("opencode", Some(".config/opencode/commands")),
            ("maki", None),
            ("gemini", Some(".gemini/commands")),
            ("copilot", Some(".copilot/instructions")),
            ("crush", Some(".config/crush/commands")),
            ("cline", Some(".cline/skills")),
            ("antigravity", Some(".gemini/antigravity-cli/skills")),
        ];
        assert_eq!(expected.len(), SUPPORTED_AGENTS.len());
        for (agent, want) in expected {
            assert_eq!(
                matrix_for(agent).unwrap().skills_mount,
                *want,
                "skills_mount for {agent}"
            );
        }
    }

    #[test]
    fn credential_source_table_matches_the_pre_f32_keychain_arms() {
        for agent in SUPPORTED_AGENTS {
            let want = match *agent {
                "claude" => CredentialSource::ClaudeKeychainOauth,
                "antigravity" => CredentialSource::AntigravityKeychainFile,
                _ => CredentialSource::None,
            };
            assert_eq!(
                matrix_for(agent).unwrap().credential_source,
                want,
                "credential_source for {agent}"
            );
        }
    }

    #[test]
    fn ping_argv_table_matches_the_pre_f32_ready_arms() {
        let expected: &[(&str, &[&str])] = &[
            // The cheapest Claude model is pinned so the refresh ping never
            // uses the account default (which may be a premium model).
            ("claude", &["claude", "--model", "haiku", "--print"]),
            ("codex", &["codex", "exec"]),
            ("opencode", &["opencode", "run"]),
            ("maki", &["maki", "--print"]),
            ("gemini", &["gemini", "-p"]),
            ("copilot", &["copilot", "-p", "-i"]),
            ("crush", &["crush", "run"]),
            ("cline", &["cline", "task"]),
            // Reproduces the pre-F-32 catch-all arm exactly, `agy` notwithstanding.
            ("antigravity", &["antigravity", "--print"]),
        ];
        assert_eq!(expected.len(), SUPPORTED_AGENTS.len());
        for (agent, want) in expected {
            assert_eq!(
                matrix_for(agent).unwrap().ping_argv,
                *want,
                "ping_argv for {agent}"
            );
        }
    }

    /// INV-8: the greeting is the ping's only variable argument, and it is
    /// always last.
    #[test]
    fn ping_command_appends_the_greeting_as_the_only_variable_argument() {
        for agent in SUPPORTED_AGENTS {
            let m = matrix_for(agent).unwrap();
            let (bin, args) = m.ping_command("hello there");
            assert_eq!(bin, m.ping_argv[0], "ping binary for {agent}");
            assert_eq!(args.last().map(String::as_str), Some("hello there"));
            assert_eq!(
                &args[..args.len() - 1],
                &m.ping_argv[1..]
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()[..],
                "every argument but the greeting is fixed, for {agent}"
            );
        }
    }

    #[test]
    fn static_env_is_copilot_offline_and_nothing_else() {
        for agent in SUPPORTED_AGENTS {
            let m = matrix_for(agent).unwrap();
            match *agent {
                "copilot" => assert_eq!(m.static_env, &[("COPILOT_OFFLINE", "true")]),
                _ => assert!(
                    m.static_env.is_empty(),
                    "{agent} must declare no static env vars"
                ),
            }
        }
    }

    #[test]
    fn only_claude_supports_a_sandbox_permission_mode() {
        for agent in SUPPORTED_AGENTS {
            let m = matrix_for(agent).unwrap();
            assert_eq!(
                m.sandbox_permission_mode_supported,
                *agent == "claude",
                "sandbox_permission_mode_supported for {agent}"
            );
        }
    }

    #[test]
    fn only_copilot_carries_a_sandbox_model_note() {
        for agent in SUPPORTED_AGENTS {
            let m = matrix_for(agent).unwrap();
            assert_eq!(
                m.sandbox_model_note.is_some(),
                *agent == "copilot",
                "sandbox_model_note for {agent}"
            );
        }
        assert_eq!(
            matrix_for("copilot").unwrap().sandbox_model_note,
            Some(
                "--model cannot be applied to copilot under the sandbox runtime (no \
                 mixin-safe config); use the /model slash command inside the session \
                 instead"
            )
        );
    }

    /// The whole `sandbox_auth_env_vars` column, verified against
    /// docs.docker.com/ai/sandboxes/security/credentials/: service ↔ env var
    /// pairs per built-in agent. Moved here from a second `match agent` in
    /// `sandbox/dsbx/auth.rs` (F-32 / midpoint finding 21).
    #[test]
    fn sandbox_auth_env_vars_cover_mixin_agents_only() {
        let expected: &[(&str, &[&str])] = &[
            ("claude", &["ANTHROPIC_API_KEY"]),
            ("codex", &["OPENAI_API_KEY"]),
            ("opencode", &["ANTHROPIC_API_KEY"]),
            // Agent-kit agents are out of scope for auto-auth.
            ("maki", &[]),
            ("gemini", &["GEMINI_API_KEY", "GOOGLE_API_KEY"]),
            ("copilot", &["GH_TOKEN", "GITHUB_TOKEN"]),
            ("crush", &[]),
            ("cline", &[]),
            ("antigravity", &[]),
        ];
        assert_eq!(expected.len(), SUPPORTED_AGENTS.len());
        for (agent, want) in expected {
            assert_eq!(
                matrix_for(agent).unwrap().sandbox_auth_env_vars,
                *want,
                "sandbox_auth_env_vars for {agent}"
            );
        }
    }

    /// The whole `deprecation_note` column. Gemini is the only agent its
    /// vendor has deprecated; every other entry is `None`, so a note cannot
    /// appear for an agent without this test changing.
    #[test]
    fn only_gemini_carries_a_deprecation_note() {
        for agent in SUPPORTED_AGENTS {
            let m = matrix_for(agent).unwrap();
            assert_eq!(
                m.deprecation_note.is_some(),
                *agent == "gemini",
                "deprecation_note for {agent}"
            );
        }
        assert_eq!(
            matrix_for("gemini").unwrap().deprecation_note,
            Some(
                "The 'gemini' agent is deprecated by Google. Migrate to \
                 'antigravity' — run 'awman chat --agent antigravity' (or \
                 'awman config set agent antigravity' to change your default)."
            )
        );
    }

    // ─── WI-0114 F-32 step 3: shared validation and mode-flag resolution ────

    fn run_with(
        yolo: Option<YoloMode>,
        auto: Option<AutoMode>,
        plan: Option<PlanMode>,
    ) -> super::super::AgentRunOptions {
        super::super::AgentRunOptions {
            yolo,
            auto,
            plan,
            ..Default::default()
        }
    }

    #[test]
    fn mode_flags_emit_yolo_then_auto_then_plan() {
        let m = matrix_for("codex").unwrap();
        assert_eq!(
            m.mode_flags(&run_with(Some(YoloMode::Enabled), None, None)),
            vec!["--dangerously-bypass-approvals-and-sandbox"]
        );
        assert_eq!(
            m.mode_flags(&run_with(None, Some(AutoMode::Enabled), None)),
            vec!["--sandbox", "workspace-write"]
        );
        // yolo + auto together keep the yolo-first order the two option
        // builders emitted before F-32.
        assert_eq!(
            m.mode_flags(&run_with(
                Some(YoloMode::Enabled),
                Some(AutoMode::Enabled),
                None
            )),
            vec![
                "--dangerously-bypass-approvals-and-sandbox",
                "--sandbox",
                "workspace-write"
            ]
        );
    }

    #[test]
    fn mode_flags_are_empty_when_the_agent_declares_no_flag() {
        let m = matrix_for("opencode").unwrap();
        assert!(m
            .mode_flags(&run_with(Some(YoloMode::Disabled), None, None))
            .is_empty());
        assert!(m
            .mode_flags(&run_with(None, Some(AutoMode::Enabled), None))
            .is_empty());
    }

    #[test]
    fn validate_run_rejects_plan_on_an_agent_without_a_plan_flag() {
        let m = matrix_for("opencode").unwrap();
        let run = run_with(None, None, Some(PlanMode::Enabled));
        for paradigm in [RunParadigm::Container, RunParadigm::Sandbox] {
            assert!(matches!(
                m.validate_run(&run, paradigm),
                Err(EngineError::PlanModeUnsupported { ref agent }) if agent == "opencode"
            ));
        }
    }

    #[test]
    fn validate_run_rejects_plan_and_yolo_together_on_both_paradigms() {
        let m = matrix_for("claude").unwrap();
        let run = run_with(Some(YoloMode::Enabled), None, Some(PlanMode::Enabled));
        for paradigm in [RunParadigm::Container, RunParadigm::Sandbox] {
            assert!(matches!(
                m.validate_run(&run, paradigm),
                Err(EngineError::ConflictingOptions(_))
            ));
        }
    }

    /// The one deliberate asymmetry: an ACP-capable agent passes on the
    /// container paradigm and is refused as `NotImplemented` on the sandbox
    /// paradigm, while an ACP-incapable agent is `AcpUnsupported` on both.
    #[test]
    fn validate_run_acp_is_container_only() {
        let run = super::super::AgentRunOptions {
            launch_mode: LaunchMode::Acp,
            ..Default::default()
        };

        let cline = matrix_for("cline").unwrap();
        assert!(cline.validate_run(&run, RunParadigm::Container).is_ok());
        assert!(matches!(
            cline.validate_run(&run, RunParadigm::Sandbox),
            Err(EngineError::NotImplemented(_))
        ));

        let claude = matrix_for("claude").unwrap();
        for paradigm in [RunParadigm::Container, RunParadigm::Sandbox] {
            assert!(matches!(
                claude.validate_run(&run, paradigm),
                Err(EngineError::AcpUnsupported { ref agent }) if agent == "claude"
            ));
        }
    }
}
