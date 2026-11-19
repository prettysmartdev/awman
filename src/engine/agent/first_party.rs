//! The first-party agent table — the only place in `src/engine/` that
//! branches on agent name. Adding a built-in agent is a single-file edit
//! here; any other agent is a custom definition (`.awman/agents/<name>.toml`).

use super::definition::{
    AgentDefinition, AgentImageSource, AgentOrigin, AgentOutputFormat, CliMajorRequirement,
    CredentialSource, InteractiveModelFallback, InteractiveSeedDelivery, ModeRefusal, ModeSelector,
    ModelFlagDelivery, NativePlanKind, NativePlanSpec, NativeSettingsPatch, PlanConfigGuard,
    SbxKitKind, SelectorArity, SessionStorageDir, SettingsMount, SystemPromptMode,
};

/// The eight built-in agents. Same slice as the Layer 0 constant so Layer 0
/// (discovery, reserved stems) and Layer 1 agree without an import in the
/// wrong direction.
pub const FIRST_PARTY_AGENTS: &[&str] =
    crate::data::config::agent_definition::FIRST_PARTY_AGENT_NAMES;

/// `true` when `name` is a built-in agent.
pub fn is_first_party(name: &str) -> bool {
    FIRST_PARTY_AGENTS.contains(&name)
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| s.to_string()).collect()
}

/// A native plan mapping delivered by the same argv on interactive and
/// headless stdio launches, with no ACP mode.
fn argv_plan(kind: NativePlanKind, argv: &[&str]) -> Option<NativePlanSpec> {
    Some(NativePlanSpec {
        kind,
        interactive_argv: Some(strings(argv)),
        interactive_settings: None,
        headless_argv: Some(strings(argv)),
        acp_mode_id: None,
    })
}

/// A value selector; `plan_values` are the duplicates of awman's own plan
/// mapping (dropped once), any other value is a conflict.
fn value_selector(flags: &[&str], plan_values: &[&str]) -> ModeSelector {
    ModeSelector {
        flags: strings(flags),
        arity: SelectorArity::Value,
        plan_values: strings(plan_values),
        value_prefix: None,
    }
}

/// A switch that always contradicts plan.
fn conflicting_switch(flags: &[&str]) -> ModeSelector {
    ModeSelector {
        flags: strings(flags),
        arity: SelectorArity::Bare,
        plan_values: Vec::new(),
        value_prefix: None,
    }
}

/// A switch that duplicates awman's own plan mapping (dropped once).
fn duplicate_switch(flags: &[&str]) -> ModeSelector {
    ModeSelector {
        flags: strings(flags),
        arity: SelectorArity::Bare,
        plan_values: strings(flags),
        value_prefix: None,
    }
}

/// The fields every first-party entry shares; each arm overrides the rest.
fn base(name: &str, display_name: &str) -> AgentDefinition {
    AgentDefinition {
        automation_adapter: automation_adapter(name),
        profile_state: profile_spec(name),
        native_resume: resume_spec(name),
        name: name.to_string(),
        display_name: display_name.to_string(),
        homepage: None,
        origin: AgentOrigin::FirstParty,
        image: AgentImageSource::Download,
        interactive_entrypoint: vec![name.to_string()],
        non_interactive_flag: None,
        headless_prompt_in_argv: false,
        output_format: AgentOutputFormat::Text,
        interactive_only_flags: Vec::new(),
        native_plan: None,
        mode_selectors: Vec::new(),
        plan_env_drop: Vec::new(),
        plan_config_guard: PlanConfigGuard::None,
        headless_prompt_flag: None,
        yolo_flags: Vec::new(),
        auto_flags: Vec::new(),
        disallowed_tools_flag: None,
        allowed_tools_flag: None,
        model_flag: ModelFlagDelivery::SpaceArg,
        interactive_seed_delivery: InteractiveSeedDelivery::Positional,
        supports_stdin_injection: false,
        supports_acp: false,
        acp_entrypoint: None,
        system_prompt_delivery: SystemPromptMode::Unsupported,
        system_prompt_flag: None,
        sbx_kit_kind: SbxKitKind::Mixin,
        settings_mount: SettingsMount::None,
        extra_settings_mounts: Vec::new(),
        session_storage_dirs: Vec::new(),
        // First-party session state already lives under HOME (Claude's
        // ~/.claude, Codex's ~/.codex and private sqlite mount, XDG for
        // opencode). Project configuration must stay visible, read-only.
        read_only_workspace_redirects: Vec::new(),
        skills_mount: None,
        credential_source: CredentialSource::None,
        ping_argv: None,
        static_env: Vec::new(),
        auth_env: Vec::new(),
        sandbox_permission_mode_supported: false,
        sandbox_model_note: None,
        sandbox_auth_env_vars: Vec::new(),
        deprecation_note: None,
        strict_modes: false,
        requires_private_session: false,
        mode_refusals: Vec::new(),
        headless_only_flags: Vec::new(),
        headless_text_flags: Vec::new(),
        interactive_model_fallback: None,
        cli_version: None,
        launch_note: None,
        sandbox_auth_note: None,
    }
}

/// The definition of a built-in agent. `None` for any other name, including
/// a former built-in that is now an example custom definition.
/// Pure: no I/O.
pub fn first_party(name: &str) -> Option<AgentDefinition> {
    Some(match name {
        "claude" => AgentDefinition {
            non_interactive_flag: Some("--print".into()),
            output_format: AgentOutputFormat::ClaudeStreamJson,
            native_plan: argv_plan(NativePlanKind::Plan, &["--permission-mode", "plan"]),
            mode_selectors: vec![
                value_selector(&["--permission-mode"], &["plan"]),
                conflicting_switch(&["--dangerously-skip-permissions"]),
            ],
            yolo_flags: strings(&["--dangerously-skip-permissions"]),
            auto_flags: strings(&["--permission-mode", "auto"]),
            disallowed_tools_flag: Some("--disallowedTools".into()),
            allowed_tools_flag: Some("--allowedTools".into()),
            // TODO(acp): verify and wire up if/when claude ships ACP support
            supports_acp: false,
            system_prompt_delivery: SystemPromptMode::Append,
            system_prompt_flag: Some("--append-system-prompt-file".into()),
            settings_mount: SettingsMount::Claude,
            skills_mount: Some(".claude/skills".into()),
            credential_source: CredentialSource::ClaudeKeychainOauth,
            // The cheapest Claude model is pinned so the refresh ping never
            // uses the account default (which may be a premium model).
            ping_argv: Some(strings(&["claude", "--model", "haiku", "--print"])),
            sandbox_permission_mode_supported: true,
            sandbox_auth_env_vars: strings(&["ANTHROPIC_API_KEY"]),
            ..base("claude", "Claude Code")
        },
        "codex" => AgentDefinition {
            non_interactive_flag: Some("exec".into()),
            output_format: AgentOutputFormat::CodexJson,
            // Recent Codex runs its interactive TUI on a shared background
            // "app-server daemon" installed under `$CODEX_HOME/packages`. A
            // throwaway container has no use for one, the image has none, and
            // the host `~/.codex` mounted over `$CODEX_HOME` must not receive a
            // container-installed daemon or its pid records. Without the flag
            // the TUI exits 1 ("daemon executable not found"). `codex exec`
            // never uses the daemon and rejects the flag, so interactive only.
            interactive_only_flags: strings(&["--no-daemon"]),
            // Codex has no startup flag for its semantic /plan command; its
            // supported equivalent is the native read-only sandbox policy, on
            // both `codex` and `codex exec`. In-session `/permissions` changes
            // stay available; awman adds no lock.
            native_plan: argv_plan(NativePlanKind::ReadOnly, &["--sandbox", "read-only"]),
            mode_selectors: vec![
                value_selector(&["--sandbox", "-s"], &["read-only"]),
                ModeSelector {
                    flags: strings(&["--config", "-c"]),
                    arity: SelectorArity::Value,
                    plan_values: strings(&["sandbox_mode=read-only", "sandbox_mode=\"read-only\""]),
                    value_prefix: Some("sandbox_mode=".into()),
                },
                conflicting_switch(&["--full-auto"]),
                conflicting_switch(&["--dangerously-bypass-approvals-and-sandbox"]),
                conflicting_switch(&["--yolo"]),
            ],
            // `--full-auto` is exec-only (and deprecated); the global flag
            // below is accepted by both interactive `codex` and `codex exec`.
            yolo_flags: strings(&["--dangerously-bypass-approvals-and-sandbox"]),
            // Successor to the deprecated `--full-auto` preset: writes inside
            // the workspace are auto-approved, escalations still prompt.
            auto_flags: strings(&["--sandbox", "workspace-write"]),
            // TODO(acp): verify and wire up if/when codex ships ACP support
            supports_acp: false,
            // codex takes the system prompt as `--config developer_instructions=<text>`.
            system_prompt_delivery: SystemPromptMode::AppendInline {
                key: "developer_instructions".into(),
            },
            system_prompt_flag: Some("--config".into()),
            settings_mount: SettingsMount::Direct(".codex".into()),
            session_storage_dirs: vec![SessionStorageDir {
                home_relative_path: ".codex-sqlite".into(),
                env_var: Some("CODEX_SQLITE_HOME".into()),
                config_override: Some(("--config".into(), "sqlite_home".into())),
                seed_from_home: None,
            }],
            skills_mount: Some(".codex/skills".into()),
            // The host ping runs in a private empty directory, outside any
            // Git repository. `codex exec` otherwise rejects that cwd before
            // it can check authentication.
            ping_argv: Some(strings(&["codex", "exec", "--skip-git-repo-check"])),
            sandbox_auth_env_vars: strings(&["OPENAI_API_KEY"]),
            ..base("codex", "Codex")
        },
        "opencode" => AgentDefinition {
            output_format: AgentOutputFormat::OpenCodeJson,
            non_interactive_flag: Some("run".into()),
            headless_prompt_in_argv: true,
            auto_flags: strings(&["--auto"]),
            // The built-in Plan agent on the TUI and `run`; ACP selects the
            // advertised `plan` session mode. Tab/agent switch to Build later
            // is the provider's own transition.
            native_plan: Some(NativePlanSpec {
                kind: NativePlanKind::Plan,
                interactive_argv: Some(strings(&["--agent", "plan"])),
                interactive_settings: None,
                headless_argv: Some(strings(&["--agent", "plan"])),
                acp_mode_id: Some("plan".into()),
            }),
            mode_selectors: vec![value_selector(&["--agent"], &["plan"])],
            plan_env_drop: strings(&["OPENCODE_PERMISSION"]),
            plan_config_guard: PlanConfigGuard::OpenCodeV1,
            // opencode's bare command treats a positional as a project dir, so a
            // seeded prompt must go through `--prompt <text>`; a positional
            // prompt makes opencode `open()` the prompt as a path (ENAMETOOLONG).
            interactive_seed_delivery: InteractiveSeedDelivery::Flag("--prompt".into()),
            supports_acp: true,
            acp_entrypoint: Some(strings(&["opencode", "acp"])),
            system_prompt_delivery: SystemPromptMode::AgentsMd,
            settings_mount: SettingsMount::Direct(".config/opencode".into()),
            extra_settings_mounts: strings(&[".local/share/opencode"]),
            skills_mount: Some(".config/opencode/skills".into()),
            // A host `opencode` cannot say which major it is (and a host 2.x
            // binary starts a shared background service), so readiness is
            // proven inside the image by `cli_version` instead.
            ping_argv: None,
            // A mixin kit cannot guarantee the major or the headless
            // subcommand; awman owns the kit.
            sbx_kit_kind: SbxKitKind::Agent,
            sandbox_auth_env_vars: strings(&["ANTHROPIC_API_KEY"]),
            cli_version: Some(CliMajorRequirement {
                binary: "opencode".into(),
                required_major: 1,
            }),
            ..base("opencode", "OpenCode")
        },
        // OpenCode 2.x. Same `opencode` binary name as 1.x, its own image.
        "opencode-v2" => AgentDefinition {
            output_format: AgentOutputFormat::OpenCodeV2Json,
            // Without `--standalone` V2 discovers a shared background service
            // whose credentials and sessions may belong to another launch.
            interactive_entrypoint: strings(&["opencode", "--standalone"]),
            non_interactive_flag: Some("run".into()),
            // `run` reads a non-TTY stdin message (verified on 2.3.2).
            headless_prompt_in_argv: false,
            // V2 `--auto` approves everything not explicitly denied: that is
            // awman's yolo, not awman's auto.
            yolo_flags: strings(&["--auto"]),
            auto_flags: Vec::new(),
            // Headless: `run --agent plan`. The full TUI has no agent flag, so
            // its private copy of ~/.config/opencode (seeded below, never the
            // host directory) gets `default_agent: "plan"`; the Plan profile
            // guard verifies nothing outranks it. `mini` (the --model
            // fallback) has no plan option and stays refused.
            native_plan: Some(NativePlanSpec {
                kind: NativePlanKind::Plan,
                interactive_argv: None,
                interactive_settings: Some(NativeSettingsPatch {
                    home_relative_file: ".config/opencode/opencode.json".into(),
                    patch: serde_json::json!({ "default_agent": "plan" }),
                }),
                headless_argv: Some(strings(&["--agent", "plan"])),
                acp_mode_id: None,
            }),
            mode_selectors: vec![
                value_selector(&["--agent"], &["plan"]),
                conflicting_switch(&["--auto"]),
            ],
            plan_env_drop: strings(&["OPENCODE_PERMISSION"]),
            plan_config_guard: PlanConfigGuard::OpenCodeV2,
            strict_modes: true,
            requires_private_session: true,
            mode_refusals: vec![ModeRefusal {
                mode: "auto",
                reason: "agent 'opencode-v2' has no equivalent of --auto: OpenCode 2.x either \
                         asks for each permission or, with its own --auto, approves everything \
                         not explicitly denied. Use --yolo for that behaviour, or launch without \
                         a mode flag and set permissions in opencode.json."
                    .into(),
            }],
            interactive_seed_delivery: InteractiveSeedDelivery::Flag("--prompt".into()),
            interactive_model_fallback: Some(InteractiveModelFallback {
                subcommand: "mini".into(),
                notice: "OpenCode 2.x has no --model option on its full-screen interface, so \
                         this session uses the minimal interface (`opencode mini`). Launch \
                         without --model for the full-screen interface and pick a model inside \
                         the session."
                    .into(),
                allows_mode_flags: false,
            }),
            supports_acp: false,
            system_prompt_delivery: SystemPromptMode::AgentsMd,
            // Never the live host directory: a private, per-execution copy.
            settings_mount: SettingsMount::None,
            session_storage_dirs: vec![
                SessionStorageDir {
                    home_relative_path: ".config/opencode".into(),
                    env_var: None,
                    config_override: None,
                    seed_from_home: Some(".config/opencode".into()),
                },
                // `opencode.db` (saved credentials) lives here; starts empty.
                SessionStorageDir {
                    home_relative_path: ".local/share/opencode".into(),
                    env_var: None,
                    config_override: None,
                    seed_from_home: None,
                },
                SessionStorageDir {
                    home_relative_path: ".local/state/opencode".into(),
                    env_var: None,
                    config_override: None,
                    seed_from_home: None,
                },
            ],
            skills_mount: Some(".config/opencode/skills".into()),
            ping_argv: None,
            sbx_kit_kind: SbxKitKind::Agent,
            sandbox_auth_env_vars: strings(&["ANTHROPIC_API_KEY"]),
            cli_version: Some(CliMajorRequirement {
                binary: "opencode".into(),
                required_major: 2,
            }),
            launch_note: Some(
                "opencode-v2 runs OpenCode 2.x with a private server and a private copy of \
                 ~/.config/opencode. Changes made inside the session, including logins, are \
                 discarded when it ends and never reach the configuration OpenCode 1.x reads. \
                 1.x plugins and the 1.x `plugin` setting do not load in 2.x. Provide provider \
                 keys with env(), for example env(ANTHROPIC_API_KEY)."
                    .into(),
            ),
            ..base("opencode-v2", "OpenCode 2")
        },
        "cursor" => AgentDefinition {
            interactive_entrypoint: strings(&["cursor-agent"]),
            non_interactive_flag: Some("--print".into()),
            // Answers workspace trust (headless runs otherwise exit 1). This
            // is not `--force`.
            headless_only_flags: strings(&["--trust"]),
            // Terminal runs print readable text; structured runs replace this
            // with `--output-format stream-json` (container options).
            headless_text_flags: strings(&["--output-format", "text"]),
            headless_prompt_in_argv: true,
            output_format: AgentOutputFormat::CursorStreamJson,
            // `--force` only ever on an explicit --yolo.
            yolo_flags: strings(&["--force"]),
            auto_flags: Vec::new(),
            // `--mode plan` on both cursor-agent and `--print`. Shift+Tab mode
            // rotation later is the provider's own transition; `--trust`
            // (headless) and `--force` (explicit yolo only) are unchanged.
            native_plan: argv_plan(NativePlanKind::Plan, &["--mode", "plan"]),
            mode_selectors: vec![
                value_selector(&["--mode"], &["plan"]),
                conflicting_switch(&["--force", "-f"]),
                conflicting_switch(&["--yolo"]),
            ],
            strict_modes: true,
            requires_private_session: true,
            mode_refusals: vec![ModeRefusal {
                mode: "auto",
                reason: "agent 'cursor' has no verified equivalent of --auto. Launch without a \
                         mode flag to use Cursor's own allow-list approvals, or use --yolo to \
                         approve everything (--force)."
                    .into(),
            }],
            model_flag: ModelFlagDelivery::SpaceArg,
            interactive_seed_delivery: InteractiveSeedDelivery::Positional,
            supports_acp: false,
            system_prompt_delivery: SystemPromptMode::AgentsMd,
            // No host ~/.cursor and no host auth.json reach the container.
            settings_mount: SettingsMount::None,
            session_storage_dirs: vec![SessionStorageDir {
                home_relative_path: ".cursor".into(),
                env_var: Some("CURSOR_CONFIG_DIR".into()),
                config_override: None,
                seed_from_home: None,
            }],
            skills_mount: Some(".cursor/skills".into()),
            credential_source: CredentialSource::None,
            // CURSOR_API_KEY only when the user names it with env()/a secret.
            auth_env: Vec::new(),
            ping_argv: None,
            sbx_kit_kind: SbxKitKind::Agent,
            sandbox_auth_env_vars: Vec::new(),
            sandbox_auth_note: Some(
                "CURSOR_API_KEY cannot be delivered to 'cursor' under the sandbox runtime yet; \
                 use runtime.kind \
                 docker, apple or builtin with env(CURSOR_API_KEY)"
                    .into(),
            ),
            ..base("cursor", "Cursor Agent")
        },
        "copilot" => AgentDefinition {
            output_format: AgentOutputFormat::CopilotJson,
            // `-p` takes the next token as the prompt, so it is delivered
            // directly before the prompt (`copilot --plan … -p PROMPT`) rather
            // than in the entrypoint, where mode flags would follow it.
            non_interactive_flag: None,
            headless_prompt_in_argv: true,
            headless_prompt_flag: Some("-p".into()),
            interactive_seed_delivery: InteractiveSeedDelivery::Flag("-i".into()),
            native_plan: argv_plan(NativePlanKind::Plan, &["--plan"]),
            // COPILOT_PLAN_THEN_AUTOPILOT really starts in plan, so it is kept.
            mode_selectors: vec![
                value_selector(&["--mode"], &["plan"]),
                duplicate_switch(&["--plan"]),
                conflicting_switch(&["--autopilot"]),
                conflicting_switch(&["--yolo"]),
                conflicting_switch(&["--allow-all"]),
            ],
            yolo_flags: strings(&["--yolo"]),
            // TODO(acp): verify and wire up if/when copilot ships ACP support
            supports_acp: false,
            system_prompt_delivery: SystemPromptMode::EnvDir {
                var: "COPILOT_CUSTOM_INSTRUCTIONS_DIRS".into(),
            },
            settings_mount: SettingsMount::Direct(".copilot".into()),
            skills_mount: Some(".copilot/skills".into()),
            ping_argv: Some(strings(&["copilot", "-p"])),
            sandbox_model_note: Some(
                "--model cannot be applied to copilot under the sandbox runtime (no \
                 mixin-safe config); use the /model slash command inside the session \
                 instead"
                    .into(),
            ),
            sandbox_auth_env_vars: strings(&["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"]),
            ..base("copilot", "GitHub Copilot CLI")
        },
        "cline" => AgentDefinition {
            output_format: AgentOutputFormat::ClineJson,
            // A positional prompt starts a one-shot run. A seeded interactive
            // prompt therefore needs `--tui`, and `task` is not a subcommand.
            non_interactive_flag: None,
            headless_prompt_in_argv: true,
            interactive_only_flags: strings(&["--tui"]),
            // TUI and one-shot `--plan`; ACP selects the advertised `plan` mode.
            native_plan: Some(NativePlanSpec {
                kind: NativePlanKind::Plan,
                interactive_argv: Some(strings(&["--plan"])),
                interactive_settings: None,
                headless_argv: Some(strings(&["--plan"])),
                acp_mode_id: Some("plan".into()),
            }),
            mode_selectors: vec![
                duplicate_switch(&["--plan", "-p"]),
                conflicting_switch(&["--act", "-a"]),
                conflicting_switch(&["--yolo", "-y"]),
                value_selector(&["--auto-approve"], &[]),
            ],
            yolo_flags: strings(&["--yolo"]),
            auto_flags: strings(&["--auto-approve", "true"]),
            supports_acp: true,
            acp_entrypoint: Some(strings(&["cline", "--acp"])),
            system_prompt_delivery: SystemPromptMode::Replace,
            system_prompt_flag: Some("--system".into()),
            sbx_kit_kind: SbxKitKind::Agent,
            settings_mount: SettingsMount::Direct(".cline/data".into()),
            skills_mount: Some(".cline/skills".into()),
            ping_argv: Some(strings(&["cline", "--json"])),
            ..base("cline", "Cline")
        },
        "antigravity" => AgentDefinition {
            output_format: AgentOutputFormat::AntigravityStreamJson,
            // Antigravity CLI exposes startup execution modes and models.
            interactive_entrypoint: strings(&["agy"]),
            non_interactive_flag: Some("--print".into()),
            headless_prompt_in_argv: true,
            // Planning steers the agent; shell permission rules stay separate.
            native_plan: argv_plan(NativePlanKind::Plan, &["--mode", "plan"]),
            mode_selectors: vec![
                value_selector(&["--mode"], &["plan"]),
                conflicting_switch(&["--dangerously-skip-permissions"]),
            ],
            yolo_flags: strings(&["--dangerously-skip-permissions"]),
            auto_flags: strings(&["--mode", "accept-edits"]),
            interactive_seed_delivery: InteractiveSeedDelivery::Flag("--prompt-interactive".into()),
            // TODO(acp): verify and wire up if/when antigravity ships ACP support
            supports_acp: false,
            system_prompt_delivery: SystemPromptMode::AddDir {
                flag: "--add-dir".into(),
            },
            sbx_kit_kind: SbxKitKind::Agent,
            settings_mount: SettingsMount::Antigravity,
            skills_mount: Some(".gemini/antigravity-cli/skills".into()),
            credential_source: CredentialSource::AntigravityKeychainFile,
            ping_argv: Some(strings(&["agy", "--print"])),
            ..base("antigravity", "Antigravity")
        },
        _ => return None,
    })
}

/// Each first-party definition selects its own automation adapter.
fn automation_adapter(name: &str) -> Option<crate::data::automation::AutomationAdapterKind> {
    use crate::data::automation::AutomationAdapterKind as A;
    Some(match name {
        "claude" => A::Claude,
        "codex" => A::Codex,
        "opencode" => A::Opencode,
        "opencode-v2" => A::OpencodeV2,
        "cursor" => A::Cursor,
        "copilot" => A::Copilot,
        "cline" => A::Cline,
        "antigravity" => A::Antigravity,
        _ => return None,
    })
}

fn resume_spec(name: &str) -> Option<super::definition::NativeResumeSpec> {
    use super::definition::{IdForm, NativeResumeSpec, ResumePosition, ResumeSelector};
    let (flag, position, equals) = match name {
        "claude" | "cursor" => ("--resume", ResumePosition::Options, false),
        "codex" => ("resume", ResumePosition::SubcommandTail, false),
        "opencode" | "opencode-v2" => ("--session", ResumePosition::Options, false),
        "copilot" => ("--resume", ResumePosition::Options, true),
        "cline" => ("--id", ResumePosition::Options, false),
        "antigravity" => ("--conversation", ResumePosition::Options, false),
        _ => return None,
    };
    let interactive = ResumeSelector {
        before_id: if equals { vec![] } else { strings(&[flag]) },
        form: if equals {
            IdForm::Equals(flag.into())
        } else {
            IdForm::Separate
        },
        after_id: vec![],
        position,
        no_auto_continue: false,
    };
    let mut headless = interactive.clone();
    if name == "codex" {
        headless.after_id.push("-".into());
    }
    Some(NativeResumeSpec {
        interactive: Some(interactive.clone()),
        headless: Some(headless),
        model_fallback: (name == "opencode-v2").then_some(interactive),
    })
}
fn profile_spec(name: &str) -> Option<super::profile_state::ProfileStateSpec> {
    use super::profile_state::{ProfileRoot, ProfileStateSpec, SeedRule, SeedSanitizer::*};
    type ProfileRow = (
        Vec<(&'static str, bool, Option<&'static str>)>,
        Vec<(&'static str, super::profile_state::SeedSanitizer)>,
        Vec<&'static str>,
        Vec<&'static str>,
        bool,
    );
    let (roots, settings, history, exclude, db): ProfileRow = match name {
        "claude" => (
            vec![(".claude", false, None), (".claude.json", true, None)],
            vec![
                (".claude.json", ClaudeConfig),
                (".claude/settings.json", ClaudeSettings),
            ],
            vec![
                ".claude/projects",
                ".claude/todos",
                ".claude/plans",
                ".claude/history.jsonl",
            ],
            vec![
                ".claude/statsig",
                ".claude/shell-snapshots",
                ".claude/ide",
                ".claude/debug",
            ],
            false,
        ),
        "codex" => (
            vec![
                (".codex", false, None),
                (".codex-sqlite", false, Some("CODEX_SQLITE_HOME")),
            ],
            vec![(".codex/config.toml", CodexToml)],
            vec![".codex/sessions", ".codex/history.jsonl"],
            vec![
                ".codex-sqlite",
                ".codex/packages",
                ".codex/log",
                ".codex/tmp",
            ],
            false,
        ),
        "opencode" | "opencode-v2" => (
            vec![
                (".config/opencode", false, None),
                (".local/share/opencode", false, None),
                (".local/state/opencode", false, None),
            ],
            vec![
                (".config/opencode/opencode.json", OpenCodeJson),
                (".config/opencode/opencode.jsonc", OpenCodeJson),
            ],
            if name == "opencode" {
                vec![".local/share/opencode/storage"]
            } else {
                vec![]
            },
            vec![
                ".local/share/opencode/log",
                ".local/share/opencode/bin",
                ".local/share/opencode/snapshot",
                ".config/opencode/node_modules",
            ],
            name == "opencode-v2",
        ),
        "cursor" => (
            vec![(".cursor", false, Some("CURSOR_CONFIG_DIR"))],
            vec![(".cursor/cli-config.json", CursorCliConfig)],
            vec![".cursor/chats"],
            vec![".cursor/logs", ".cursor/extensions"],
            false,
        ),
        "copilot" => (
            vec![(".copilot", false, None)],
            vec![(".copilot/config.json", CopilotConfig)],
            vec![".copilot/session-state", ".copilot/history-session-state"],
            vec![".copilot/logs", ".copilot/pkg"],
            false,
        ),
        "cline" => (
            vec![(".cline/data", false, None)],
            vec![(".cline/data/globalState.json", ClineSettings)],
            vec![".cline/data/tasks", ".cline/data/state/taskHistory.json"],
            vec![".cline/data/logs"],
            false,
        ),
        "antigravity" => (
            vec![(".gemini", false, None)],
            vec![(".gemini/settings.json", AntigravitySettings)],
            vec![
                ".gemini/antigravity-cli/conversations",
                ".gemini/antigravity-cli/brain",
            ],
            vec![".gemini/tmp"],
            false,
        ),
        _ => return None,
    };
    Some(ProfileStateSpec {
        allow_leading_dash: name == "copilot",
        identity: match name {
            "claude" => super::profile_state::HistoryIdentity::Claude,
            "codex" => super::profile_state::HistoryIdentity::Codex,
            "opencode" | "opencode-v2" => super::profile_state::HistoryIdentity::OpenCode,
            "cursor" => super::profile_state::HistoryIdentity::Cursor,
            "copilot" => super::profile_state::HistoryIdentity::Copilot,
            "cline" => super::profile_state::HistoryIdentity::Cline,
            _ => super::profile_state::HistoryIdentity::Antigravity,
        },
        roots: roots
            .into_iter()
            .map(|(r, f, e)| ProfileRoot {
                home_relative: r.into(),
                file: f,
                env_var: e.map(str::to_owned),
            })
            .collect(),
        settings: settings
            .into_iter()
            .map(|(p, s)| SeedRule {
                path: p.into(),
                sanitizer: s,
            })
            .collect(),
        history: strings(&history),
        exclude: strings(&exclude)
            .into_iter()
            .chain(strings(match name {
                "claude" => &[".claude/.credentials.json"],
                "codex" => &[".codex/auth.json"],
                "opencode" | "opencode-v2" => &[
                    ".local/share/opencode/auth.json",
                    ".local/share/opencode/mcp-auth.json",
                ],
                "cursor" => &[".cursor/auth.json"],
                "cline" => &[".cline/data/secrets.json"],
                "antigravity" => &[
                    ".gemini/oauth_creds.json",
                    ".gemini/google_accounts.json",
                    ".gemini/antigravity-cli/antigravity-oauth-token",
                ],
                _ => &[],
            }))
            .collect(),
        exclude_basename_prefixes: match name {
            "cline" => vec![(".cline/data".into(), "secrets".into())],
            "antigravity" => vec![(".gemini".into(), "credentials".into())],
            "opencode" | "opencode-v2" => {
                vec![(".local/share/opencode".into(), "opencode.db".into())]
            }
            _ => vec![],
        },
        database_history: db,
    })
}
