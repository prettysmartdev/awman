//! The first-party agent table — the only place in `src/engine/` that
//! branches on agent name. Adding a built-in agent is a single-file edit
//! here; any other agent is a custom definition (`.awman/agents/<name>.toml`).

use super::definition::{
    AgentDefinition, AgentImageSource, AgentOrigin, CredentialSource, InteractiveSeedDelivery,
    ModelFlagDelivery, SbxKitKind, SettingsMount, SystemPromptMode,
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
            skills_mount: Some(".claude/commands".into()),
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
            // Recent Codex runs its interactive TUI on a shared background
            // "app-server daemon" installed under `$CODEX_HOME/packages`. A
            // throwaway container has no use for one, the image has none, and
            // the host `~/.codex` mounted over `$CODEX_HOME` must not receive a
            // container-installed daemon or its pid records. Without the flag
            // the TUI exits 1 ("daemon executable not found"). `codex exec`
            // never uses the daemon and rejects the flag, so interactive only.
            interactive_only_flags: strings(&["--no-daemon"]),
            plan_flags: strings(&["--approval-mode", "plan"]),
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
            skills_mount: Some(".codex/skills".into()),
            ping_argv: Some(strings(&["codex", "exec"])),
            sandbox_auth_env_vars: strings(&["OPENAI_API_KEY"]),
            ..base("codex", "Codex")
        },
        "opencode" => AgentDefinition {
            non_interactive_flag: Some("run".into()),
            // opencode's bare command treats a positional as a project dir, so a
            // seeded prompt must go through `--prompt <text>`; a positional
            // prompt makes opencode `open()` the prompt as a path (ENAMETOOLONG).
            interactive_seed_delivery: InteractiveSeedDelivery::Flag("--prompt".into()),
            // TODO(acp): verify and wire up if/when opencode ships ACP support
            supports_acp: false,
            system_prompt_delivery: SystemPromptMode::AgentsMd,
            settings_mount: SettingsMount::Direct(".config/opencode".into()),
            skills_mount: Some(".config/opencode/commands".into()),
            ping_argv: Some(strings(&["opencode", "run"])),
            sandbox_auth_env_vars: strings(&["ANTHROPIC_API_KEY"]),
            ..base("opencode", "OpenCode")
        },
        "gemini" => AgentDefinition {
            plan_flags: strings(&["--approval-mode=plan"]),
            yolo_flags: strings(&["--yolo"]),
            auto_flags: strings(&["--approval-mode=auto_edit"]),
            // TODO(acp): verify and wire up if/when gemini ships ACP support
            supports_acp: false,
            system_prompt_delivery: SystemPromptMode::EnvFile {
                var: "GEMINI_SYSTEM_MD".into(),
            },
            settings_mount: SettingsMount::Direct(".gemini".into()),
            skills_mount: Some(".gemini/commands".into()),
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
            interactive_entrypoint: strings(&["copilot", "-i"]),
            plan_flags: strings(&["--plan"]),
            yolo_flags: strings(&["--autopilot"]),
            // TODO(acp): verify and wire up if/when copilot ships ACP support
            supports_acp: false,
            system_prompt_delivery: SystemPromptMode::EnvFile {
                var: "COPILOT_CUSTOM_INSTRUCTIONS_DIRS".into(),
            },
            settings_mount: SettingsMount::None,
            skills_mount: Some(".copilot/instructions".into()),
            ping_argv: Some(strings(&["copilot", "-p", "-i"])),
            static_env: vec![("COPILOT_OFFLINE".into(), "true".into())],
            sandbox_model_note: Some(
                "--model cannot be applied to copilot under the sandbox runtime (no \
                 mixin-safe config); use the /model slash command inside the session \
                 instead"
                    .into(),
            ),
            sandbox_auth_env_vars: strings(&["GH_TOKEN", "GITHUB_TOKEN"]),
            ..base("copilot", "GitHub Copilot CLI")
        },
        "cline" => AgentDefinition {
            non_interactive_flag: Some("task".into()),
            plan_flags: strings(&["--plan"]),
            yolo_flags: strings(&["--yolo"]),
            auto_flags: strings(&["--auto-approve-all"]),
            supports_acp: true,
            acp_entrypoint: Some(strings(&["cline", "--acp"])),
            system_prompt_delivery: SystemPromptMode::Replace,
            system_prompt_flag: Some("--system".into()),
            sbx_kit_kind: SbxKitKind::Agent,
            settings_mount: SettingsMount::Direct(".cline/data".into()),
            skills_mount: Some(".cline/skills".into()),
            ping_argv: Some(strings(&["cline", "task"])),
            ..base("cline", "Cline")
        },
        "antigravity" => AgentDefinition {
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
            // prompt as the agy executable name. Leaving plan/auto empty
            // keeps non-yolo modes a silent no-op (matches opencode).
            interactive_entrypoint: strings(&["agy"]),
            non_interactive_flag: Some("--print".into()),
            yolo_flags: strings(&["--dangerously-skip-permissions"]),
            model_flag: ModelFlagDelivery::Unsupported,
            // agy accepts `--prompt-interactive`/`-i` for an interactive seed,
            // but awman has always seeded it positionally; keep that behavior
            // until the flag form is verified end-to-end.
            interactive_seed_delivery: InteractiveSeedDelivery::Positional,
            // TODO(acp): verify and wire up if/when antigravity ships ACP support
            supports_acp: false,
            system_prompt_delivery: SystemPromptMode::AddDir {
                flag: "--add-dir".into(),
            },
            sbx_kit_kind: SbxKitKind::Agent,
            settings_mount: SettingsMount::Antigravity,
            skills_mount: Some(".gemini/antigravity-cli/skills".into()),
            credential_source: CredentialSource::AntigravityKeychainFile,
            // The ping binary is `antigravity`, NOT the `agy` interactive
            // entrypoint. That is what the pre-F-32 fallback arm produced and
            // it is preserved verbatim here; see the ping_argv doc comment.
            ping_argv: Some(strings(&["antigravity", "--print"])),
            ..base("antigravity", "Antigravity")
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::repo::LaunchMode;
    use crate::engine::container::options::{AutoMode, Entrypoint, PlanMode, YoloMode};
    use crate::engine::error::EngineError;

    use super::super::definition::RunParadigm;

    fn def(name: &str) -> AgentDefinition {
        first_party(name).unwrap_or_else(|| panic!("no first-party definition for {name}"))
    }

    #[test]
    fn exactly_the_seven_first_party_agents_resolve() {
        assert_eq!(FIRST_PARTY_AGENTS.len(), 7);
        for a in FIRST_PARTY_AGENTS {
            let d = def(a);
            assert_eq!(d.name, *a);
            assert_eq!(d.origin, AgentOrigin::FirstParty);
            assert_eq!(d.image, AgentImageSource::Download);
            assert!(!d.is_custom());
            assert!(d.scope().is_none());
            assert!(d.auth_env.is_empty(), "{a}");
            assert!(d.homepage.is_none(), "{a}");
            assert!(!d.display_name.is_empty(), "{a}");
            assert!(is_first_party(a));
        }
        // The former built-ins are asserted absent in `registry.rs`, so this
        // file carries no literal of their names (WI 0130 regression guard).
        assert!(first_party("totallymade-up").is_none());
        assert!(!is_first_party("totallymade-up"));
    }

    #[test]
    fn display_names_match_the_table() {
        let expected = [
            ("claude", "Claude Code"),
            ("codex", "Codex"),
            ("opencode", "OpenCode"),
            ("gemini", "Gemini CLI"),
            ("copilot", "GitHub Copilot CLI"),
            ("cline", "Cline"),
            ("antigravity", "Antigravity"),
        ];
        assert_eq!(expected.len(), FIRST_PARTY_AGENTS.len());
        for (agent, want) in expected {
            assert_eq!(def(agent).display_name, want, "{agent}");
        }
    }

    #[test]
    fn acp_metadata_is_consistent_for_all_agents() {
        for a in FIRST_PARTY_AGENTS {
            let d = def(a);
            assert_eq!(
                d.acp_entrypoint.is_some(),
                d.supports_acp,
                "ACP metadata invariant failed for {a}"
            );
        }
    }

    #[test]
    fn cline_has_verified_acp_entrypoint() {
        let d = def("cline");
        assert!(d.supports_acp);
        assert_eq!(d.acp_entrypoint, Some(strings(&["cline", "--acp"])));
        assert_eq!(
            d.entrypoint_for_acp().unwrap(),
            Entrypoint(vec!["cline".to_string(), "--acp".to_string()])
        );
    }

    #[test]
    fn unsupported_agents_have_no_acp_entrypoint() {
        for agent in FIRST_PARTY_AGENTS {
            if *agent == "cline" {
                continue;
            }
            let d = def(agent);
            assert!(!d.supports_acp, "unexpected ACP support for {agent}");
            assert!(d.acp_entrypoint.is_none());
            assert!(matches!(
                d.entrypoint_for_acp(),
                Err(EngineError::AcpUnsupported { .. })
            ));
        }
    }

    #[test]
    fn opencode_plan_unsupported() {
        assert!(def("opencode").plan_flags.is_empty());
    }

    #[test]
    fn opencode_interactive_seed_uses_prompt_flag() {
        assert_eq!(
            def("opencode").interactive_seed_delivery,
            InteractiveSeedDelivery::Flag("--prompt".into()),
            "opencode must seed interactive prompts via `--prompt`; a bare \
             positional is a project dir and opencode open()s it (ENAMETOOLONG)"
        );
    }

    #[test]
    fn only_opencode_uses_a_seed_flag_others_are_positional() {
        for a in FIRST_PARTY_AGENTS {
            let d = def(a);
            match *a {
                "opencode" => assert!(matches!(
                    d.interactive_seed_delivery,
                    InteractiveSeedDelivery::Flag(_)
                )),
                _ => assert_eq!(
                    d.interactive_seed_delivery,
                    InteractiveSeedDelivery::Positional,
                    "{a} must seed interactive prompts positionally"
                ),
            }
        }
    }

    #[test]
    fn codex_yolo_flag_is_dangerously_bypass_approvals_and_sandbox() {
        assert_eq!(
            def("codex").yolo_flags,
            strings(&["--dangerously-bypass-approvals-and-sandbox"]),
            "codex yolo flag must be --dangerously-bypass-approvals-and-sandbox; \
             --full-auto is exec-only, deprecated, and rejected by interactive codex"
        );
    }

    #[test]
    fn codex_auto_flag_is_sandbox_workspace_write() {
        assert_eq!(
            def("codex").auto_flags,
            strings(&["--sandbox", "workspace-write"]),
            "codex auto flags must be --sandbox workspace-write (successor to deprecated --full-auto)"
        );
    }

    #[test]
    fn antigravity_yolo_flag_is_dangerously_skip_permissions() {
        assert_eq!(
            def("antigravity").yolo_flags,
            strings(&["--dangerously-skip-permissions"]),
            "antigravity yolo flag must be --dangerously-skip-permissions"
        );
    }

    #[test]
    fn antigravity_non_interactive_flag_is_print() {
        assert_eq!(
            def("antigravity").non_interactive_flag.as_deref(),
            Some("--print"),
            "antigravity non_interactive_flag must be --print"
        );
    }

    #[test]
    fn antigravity_model_flag_unsupported_returns_err() {
        let result = def("antigravity").model_flag_for("gemini-3.5-flash");
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

    /// Interactive codex runs without the app-server daemon; `codex exec`
    /// rejects `--no-daemon`, so the non-interactive launch must not carry it.
    #[test]
    fn codex_no_daemon_is_interactive_only() {
        let d = def("codex");
        assert_eq!(d.entrypoint_for(false).0, vec!["codex", "--no-daemon"]);
        assert_eq!(d.entrypoint_for(true).0, vec!["codex", "exec"]);
    }

    #[test]
    fn only_codex_declares_interactive_only_flags() {
        for agent in FIRST_PARTY_AGENTS {
            let d = def(agent);
            if *agent != "codex" {
                assert!(d.interactive_only_flags.is_empty(), "{agent}");
                assert_eq!(
                    d.entrypoint_for(false).0,
                    d.interactive_entrypoint,
                    "{agent}"
                );
            }
        }
    }

    #[test]
    fn antigravity_interactive_entrypoint_is_agy() {
        assert_eq!(
            def("antigravity").interactive_entrypoint,
            strings(&["agy"]),
            "antigravity interactive_entrypoint must be [\"agy\"]"
        );
    }

    // ─── WI-0087: system prompt delivery ───────────────────────────────────

    #[test]
    fn codex_system_prompt_delivery_is_append_inline_with_developer_instructions() {
        let d = def("codex");
        assert_eq!(
            d.system_prompt_delivery,
            SystemPromptMode::AppendInline {
                key: "developer_instructions".into()
            }
        );
        assert_eq!(
            d.system_prompt_flag.as_deref(),
            Some("--config"),
            "codex system_prompt_flag must be --config"
        );
    }

    #[test]
    fn claude_system_prompt_delivery_is_append() {
        let d = def("claude");
        assert_eq!(d.system_prompt_delivery, SystemPromptMode::Append);
        assert_eq!(
            d.system_prompt_flag.as_deref(),
            Some("--append-system-prompt-file")
        );
    }

    #[test]
    fn cline_system_prompt_delivery_is_replace() {
        let d = def("cline");
        assert_eq!(d.system_prompt_delivery, SystemPromptMode::Replace);
        assert_eq!(d.system_prompt_flag.as_deref(), Some("--system"));
    }

    #[test]
    fn antigravity_system_prompt_delivery_is_add_dir() {
        assert_eq!(
            def("antigravity").system_prompt_delivery,
            SystemPromptMode::AddDir {
                flag: "--add-dir".into()
            }
        );
    }

    #[test]
    fn opencode_system_prompt_delivery_is_agents_md() {
        assert_eq!(
            def("opencode").system_prompt_delivery,
            SystemPromptMode::AgentsMd
        );
    }

    #[test]
    fn env_file_system_prompt_delivery_for_gemini_and_copilot() {
        assert_eq!(
            def("gemini").system_prompt_delivery,
            SystemPromptMode::EnvFile {
                var: "GEMINI_SYSTEM_MD".into()
            }
        );
        assert_eq!(
            def("copilot").system_prompt_delivery,
            SystemPromptMode::EnvFile {
                var: "COPILOT_CUSTOM_INSTRUCTIONS_DIRS".into()
            }
        );
    }

    // ─── WI-0114 F-32: the fields that absorbed the four per-agent tables ───
    //
    // Each of these asserts the WHOLE table, agent by agent, against the
    // values the pre-F-32 `match agent.as_str()` arms produced. They are the
    // regression guard: a wrong entry here is a silent behaviour change in
    // overlay mounting, credential delivery or the sanctioned host ping.

    #[test]
    fn settings_mount_table_matches_the_pre_f32_arms() {
        let expected: &[(&str, SettingsMount)] = &[
            ("claude", SettingsMount::Claude),
            ("codex", SettingsMount::Direct(".codex".into())),
            ("opencode", SettingsMount::Direct(".config/opencode".into())),
            ("gemini", SettingsMount::Direct(".gemini".into())),
            ("copilot", SettingsMount::None),
            ("cline", SettingsMount::Direct(".cline/data".into())),
            ("antigravity", SettingsMount::Antigravity),
        ];
        assert_eq!(expected.len(), FIRST_PARTY_AGENTS.len());
        for (agent, want) in expected {
            assert_eq!(
                def(agent).settings_mount,
                *want,
                "settings_mount for {agent}"
            );
        }
    }

    /// The `Direct` agents mount at the same path on host and in the
    /// container. If a future agent needs them to differ, `Direct` has to
    /// grow a second path rather than the loop growing a special case.
    #[test]
    fn direct_settings_mounts_are_relative_paths_without_a_leading_slash() {
        for agent in FIRST_PARTY_AGENTS {
            if let SettingsMount::Direct(rel) = def(agent).settings_mount {
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
            ("gemini", Some(".gemini/commands")),
            ("copilot", Some(".copilot/instructions")),
            ("cline", Some(".cline/skills")),
            ("antigravity", Some(".gemini/antigravity-cli/skills")),
        ];
        assert_eq!(expected.len(), FIRST_PARTY_AGENTS.len());
        for (agent, want) in expected {
            assert_eq!(
                def(agent).skills_mount.as_deref(),
                *want,
                "skills_mount for {agent}"
            );
        }
    }

    #[test]
    fn credential_source_table_matches_the_pre_f32_keychain_arms() {
        for agent in FIRST_PARTY_AGENTS {
            let want = match *agent {
                "claude" => CredentialSource::ClaudeKeychainOauth,
                "antigravity" => CredentialSource::AntigravityKeychainFile,
                _ => CredentialSource::None,
            };
            assert_eq!(
                def(agent).credential_source,
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
            ("gemini", &["gemini", "-p"]),
            ("copilot", &["copilot", "-p", "-i"]),
            ("cline", &["cline", "task"]),
            // Reproduces the pre-F-32 catch-all arm exactly, `agy` notwithstanding.
            ("antigravity", &["antigravity", "--print"]),
        ];
        assert_eq!(expected.len(), FIRST_PARTY_AGENTS.len());
        for (agent, want) in expected {
            assert_eq!(
                def(agent).ping_argv,
                Some(strings(want)),
                "ping_argv for {agent}"
            );
        }
    }

    /// INV-8: the greeting is the ping's only variable argument, and it is
    /// always last.
    #[test]
    fn ping_command_appends_the_greeting_as_the_only_variable_argument() {
        for agent in FIRST_PARTY_AGENTS {
            let d = def(agent);
            let argv = d.ping_argv.clone().unwrap();
            let (bin, args) = d.ping_command("hello there").unwrap();
            assert_eq!(bin, argv[0], "ping binary for {agent}");
            assert_eq!(args.last().map(String::as_str), Some("hello there"));
            assert_eq!(
                &args[..args.len() - 1],
                &argv[1..],
                "every argument but the greeting is fixed, for {agent}"
            );
        }
    }

    #[test]
    fn ping_command_is_none_without_a_ping_argv() {
        let mut d = def("cline");
        d.ping_argv = None;
        assert_eq!(d.ping_command("Hello"), None);
        d.ping_argv = Some(Vec::new());
        assert_eq!(d.ping_command("Hello"), None);
    }

    #[test]
    fn static_env_is_copilot_offline_and_nothing_else() {
        for agent in FIRST_PARTY_AGENTS {
            let d = def(agent);
            match *agent {
                "copilot" => assert_eq!(
                    d.static_env,
                    vec![("COPILOT_OFFLINE".to_string(), "true".to_string())]
                ),
                _ => assert!(
                    d.static_env.is_empty(),
                    "{agent} must declare no static env vars"
                ),
            }
        }
    }

    #[test]
    fn only_claude_supports_a_sandbox_permission_mode() {
        for agent in FIRST_PARTY_AGENTS {
            assert_eq!(
                def(agent).sandbox_permission_mode_supported,
                *agent == "claude",
                "sandbox_permission_mode_supported for {agent}"
            );
        }
    }

    #[test]
    fn only_copilot_carries_a_sandbox_model_note() {
        for agent in FIRST_PARTY_AGENTS {
            assert_eq!(
                def(agent).sandbox_model_note.is_some(),
                *agent == "copilot",
                "sandbox_model_note for {agent}"
            );
        }
        assert_eq!(
            def("copilot").sandbox_model_note.as_deref(),
            Some(
                "--model cannot be applied to copilot under the sandbox runtime (no \
                 mixin-safe config); use the /model slash command inside the session \
                 instead"
            )
        );
    }

    /// The whole `sandbox_auth_env_vars` column, verified against
    /// docs.docker.com/ai/sandboxes/security/credentials/: service ↔ env var
    /// pairs per built-in agent.
    #[test]
    fn sandbox_auth_env_vars_cover_mixin_agents_only() {
        let expected: &[(&str, &[&str])] = &[
            ("claude", &["ANTHROPIC_API_KEY"]),
            ("codex", &["OPENAI_API_KEY"]),
            ("opencode", &["ANTHROPIC_API_KEY"]),
            ("gemini", &["GEMINI_API_KEY", "GOOGLE_API_KEY"]),
            ("copilot", &["GH_TOKEN", "GITHUB_TOKEN"]),
            // Agent-kit agents are out of scope for auto-auth.
            ("cline", &[]),
            ("antigravity", &[]),
        ];
        assert_eq!(expected.len(), FIRST_PARTY_AGENTS.len());
        for (agent, want) in expected {
            assert_eq!(
                def(agent).sandbox_auth_env_vars,
                strings(want),
                "sandbox_auth_env_vars for {agent}"
            );
        }
    }

    #[test]
    fn sbx_kit_kind_is_agent_for_cline_and_antigravity_only() {
        for agent in FIRST_PARTY_AGENTS {
            let want = match *agent {
                "cline" | "antigravity" => SbxKitKind::Agent,
                _ => SbxKitKind::Mixin,
            };
            assert_eq!(def(agent).sbx_kit_kind, want, "sbx_kit_kind for {agent}");
        }
    }

    /// The whole `deprecation_note` column. Gemini is the only agent its
    /// vendor has deprecated; every other entry is `None`, so a note cannot
    /// appear for an agent without this test changing.
    #[test]
    fn only_gemini_carries_a_deprecation_note() {
        for agent in FIRST_PARTY_AGENTS {
            assert_eq!(
                def(agent).deprecation_note.is_some(),
                *agent == "gemini",
                "deprecation_note for {agent}"
            );
        }
        assert_eq!(
            def("gemini").deprecation_note.as_deref(),
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
        let d = def("codex");
        assert_eq!(
            d.mode_flags(&run_with(Some(YoloMode::Enabled), None, None)),
            vec!["--dangerously-bypass-approvals-and-sandbox"]
        );
        assert_eq!(
            d.mode_flags(&run_with(None, Some(AutoMode::Enabled), None)),
            vec!["--sandbox", "workspace-write"]
        );
        // yolo + auto together keep the yolo-first order the two option
        // builders emitted before F-32.
        assert_eq!(
            d.mode_flags(&run_with(
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
        let d = def("opencode");
        assert!(d
            .mode_flags(&run_with(Some(YoloMode::Disabled), None, None))
            .is_empty());
        assert!(d
            .mode_flags(&run_with(None, Some(AutoMode::Enabled), None))
            .is_empty());
    }

    #[test]
    fn validate_run_rejects_plan_on_an_agent_without_a_plan_flag() {
        let d = def("opencode");
        let run = run_with(None, None, Some(PlanMode::Enabled));
        for paradigm in [RunParadigm::Container, RunParadigm::Sandbox] {
            assert!(matches!(
                d.validate_run(&run, paradigm),
                Err(EngineError::PlanModeUnsupported { ref agent }) if agent == "opencode"
            ));
        }
    }

    #[test]
    fn tool_restrictions_fail_closed_on_both_paradigms() {
        for agent in ["codex", "cline", "gemini", "copilot", "opencode"] {
            for allowed in [true, false] {
                let mut run = run_with(None, None, None);
                if allowed {
                    run.allowed_tools.push("Read".into());
                } else {
                    run.disallowed_tools.push("Bash".into());
                }
                for paradigm in [RunParadigm::Container, RunParadigm::Sandbox] {
                    assert!(
                        matches!(
                            def(agent).validate_run(&run, paradigm),
                            Err(EngineError::ToolRestrictionFlagMissing { .. })
                        ),
                        "{agent} {paradigm:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn validate_run_rejects_plan_and_yolo_together_on_both_paradigms() {
        let d = def("claude");
        let run = run_with(Some(YoloMode::Enabled), None, Some(PlanMode::Enabled));
        for paradigm in [RunParadigm::Container, RunParadigm::Sandbox] {
            assert!(matches!(
                d.validate_run(&run, paradigm),
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

        let cline = def("cline");
        assert!(cline.validate_run(&run, RunParadigm::Container).is_ok());
        assert!(matches!(
            cline.validate_run(&run, RunParadigm::Sandbox),
            Err(EngineError::NotImplemented(_))
        ));

        let claude = def("claude");
        for paradigm in [RunParadigm::Container, RunParadigm::Sandbox] {
            assert!(matches!(
                claude.validate_run(&run, paradigm),
                Err(EngineError::AcpUnsupported { ref agent }) if agent == "claude"
            ));
        }
    }
}

#[cfg(test)]
mod family_coverage_tests {
    //! Every settings family and prompt mode a first-party agent can carry is
    //! realised by at least one agent, so the builtin runtime's compatibility
    //! tests cannot silently miss one.
    use super::*;

    #[test]
    fn every_settings_family_and_first_party_prompt_mode_belongs_to_some_agent() {
        let mut settings = [false; 4];
        // `Unsupported` has no first-party users; custom conversion covers it.
        let mut prompts = [false; 6];
        for agent in FIRST_PARTY_AGENTS {
            let d = first_party(agent).unwrap();
            settings[match d.settings_mount {
                SettingsMount::None => 0,
                SettingsMount::Direct(_) => 1,
                SettingsMount::Claude => 2,
                SettingsMount::Antigravity => 3,
            }] = true;
            match d.system_prompt_delivery {
                SystemPromptMode::Append => prompts[0] = true,
                SystemPromptMode::AppendInline { .. } => prompts[1] = true,
                SystemPromptMode::Replace => prompts[2] = true,
                SystemPromptMode::AgentsMd => prompts[3] = true,
                SystemPromptMode::EnvFile { .. } => prompts[4] = true,
                SystemPromptMode::AddDir { .. } => prompts[5] = true,
                SystemPromptMode::Unsupported => {
                    panic!("{agent}: no first-party agent uses Unsupported delivery")
                }
            }
        }
        assert_eq!(settings, [true; 4], "a SettingsMount family has no agent");
        assert_eq!(prompts, [true; 6], "a SystemPromptMode has no agent");
    }

    #[test]
    fn direct_settings_paths_are_relative_and_cannot_escape_home() {
        for agent in FIRST_PARTY_AGENTS {
            if let SettingsMount::Direct(relative) = first_party(agent).unwrap().settings_mount {
                let path = std::path::Path::new(&relative);
                assert!(path.is_relative(), "{agent}: {relative}");
                assert!(
                    path.components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))),
                    "{agent}: {relative}"
                );
            }
        }
    }

    #[test]
    fn prompt_delivery_parameters_are_well_formed() {
        for agent in FIRST_PARTY_AGENTS {
            match first_party(agent).unwrap().system_prompt_delivery {
                SystemPromptMode::EnvFile { var } => {
                    assert!(
                        var.chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'),
                        "{agent}: {var}"
                    );
                }
                SystemPromptMode::AddDir { flag } => {
                    assert!(flag.starts_with("--"), "{agent}: {flag}")
                }
                SystemPromptMode::AppendInline { key } => assert!(!key.is_empty(), "{agent}"),
                _ => {}
            }
        }
    }
}
