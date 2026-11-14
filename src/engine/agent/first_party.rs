//! The first-party agent table — the only place in `src/engine/` that
//! branches on agent name. Adding a built-in agent is a single-file edit
//! here; any other agent is a custom definition (`.awman/agents/<name>.toml`).

use super::definition::{
    AgentDefinition, AgentImageSource, AgentOrigin, AgentOutputFormat, CredentialSource,
    InteractiveSeedDelivery, ModelFlagDelivery, SbxKitKind, SessionStorageDir, SettingsMount,
    SystemPromptMode,
};

/// The seven built-in agents. Same slice as the Layer 0 constant so Layer 0
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

/// The fields every first-party entry shares; each arm overrides the rest.
fn base(name: &str, display_name: &str) -> AgentDefinition {
    AgentDefinition {
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
        plan_flags: Vec::new(),
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
        skills_mount: None,
        credential_source: CredentialSource::None,
        ping_argv: None,
        static_env: Vec::new(),
        auth_env: Vec::new(),
        sandbox_permission_mode_supported: false,
        sandbox_model_note: None,
        sandbox_auth_env_vars: Vec::new(),
        deprecation_note: None,
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
            plan_flags: strings(&["--permission-mode", "plan"]),
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
            // Codex exposes Plan through its interactive /plan command, not a
            // startup flag. Reject awman's --plan instead of passing an
            // unknown --approval-mode option.
            plan_flags: Vec::new(),
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
            non_interactive_flag: Some("run".into()),
            headless_prompt_in_argv: true,
            auto_flags: strings(&["--auto"]),
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
            ping_argv: Some(strings(&["opencode", "run"])),
            sandbox_auth_env_vars: strings(&["ANTHROPIC_API_KEY"]),
            ..base("opencode", "OpenCode")
        },
        "gemini" => AgentDefinition {
            non_interactive_flag: Some("--prompt".into()),
            headless_prompt_in_argv: true,
            interactive_seed_delivery: InteractiveSeedDelivery::Flag("--prompt-interactive".into()),
            plan_flags: strings(&["--approval-mode=plan"]),
            yolo_flags: strings(&["--approval-mode=yolo"]),
            auto_flags: strings(&["--approval-mode=auto_edit"]),
            supports_acp: true,
            acp_entrypoint: Some(strings(&["gemini", "--experimental-acp"])),
            system_prompt_delivery: SystemPromptMode::EnvFile {
                var: "GEMINI_SYSTEM_MD".into(),
            },
            settings_mount: SettingsMount::Direct(".gemini".into()),
            skills_mount: Some(".gemini/skills".into()),
            ping_argv: Some(strings(&["gemini", "-p"])),
            sandbox_auth_env_vars: strings(&["GEMINI_API_KEY", "GOOGLE_API_KEY"]),
            deprecation_note: Some(
                "The 'gemini' agent is deprecated by Google. Migrate to \
                 'antigravity' — run 'awman chat --agent antigravity' (or \
                 'awman config set agent antigravity' to change your default)."
                    .into(),
            ),
            ..base("gemini", "Gemini CLI")
        },
        "copilot" => AgentDefinition {
            non_interactive_flag: Some("-p".into()),
            headless_prompt_in_argv: true,
            interactive_seed_delivery: InteractiveSeedDelivery::Flag("-i".into()),
            plan_flags: strings(&["--plan"]),
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
            // A positional prompt starts a one-shot run. A seeded interactive
            // prompt therefore needs `--tui`, and `task` is not a subcommand.
            non_interactive_flag: None,
            headless_prompt_in_argv: true,
            interactive_only_flags: strings(&["--tui"]),
            plan_flags: strings(&["--plan"]),
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
            // Antigravity CLI exposes startup execution modes and models.
            interactive_entrypoint: strings(&["agy"]),
            non_interactive_flag: Some("--print".into()),
            headless_prompt_in_argv: true,
            plan_flags: strings(&["--mode", "plan"]),
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
