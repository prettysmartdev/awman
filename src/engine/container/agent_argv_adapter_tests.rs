//! WI 0157 argv contract for the version-specific built-in adapters
//! (`opencode` 1.x, `opencode-v2`, `cursor`). Every row of the design's argv
//! tables is pinned here, through the same pure option builder
//! (`launch_argv_options`) and argv tail (`AgentArgv`) every container backend
//! uses, so V2/Cursor work cannot drift V1.

use super::AgentArgv;
use crate::data::config::agents::LaunchMode;
use crate::engine::agent::definition::RunParadigm;
use crate::engine::agent::first_party::first_party;
use crate::engine::agent::{launch_argv_options, AgentRunOptions};
use crate::engine::container::options::{AutoMode, PlanMode, ResolvedContainerOptions, YoloMode};
use crate::engine::error::EngineError;

const SEED: &str = "fix the build";

#[derive(Clone, Copy, Default)]
struct Shape {
    headless: bool,
    seed: bool,
    model: Option<&'static str>,
    yolo: bool,
    auto: bool,
    plan: bool,
    structured: bool,
    acp: bool,
}

fn run(shape: Shape) -> AgentRunOptions {
    AgentRunOptions {
        non_interactive: shape.headless,
        initial_prompt: shape.seed.then(|| SEED.to_string()),
        model: shape.model.map(str::to_string),
        yolo: shape.yolo.then_some(YoloMode::Enabled),
        yolo_authorized: true,
        auto: shape.auto.then_some(AutoMode::Enabled),
        plan: shape.plan.then_some(PlanMode::Enabled),
        structured_output: shape.structured,
        launch_mode: if shape.acp {
            LaunchMode::Acp
        } else {
            LaunchMode::default()
        },
        ..Default::default()
    }
}

/// Full argv (entrypoint + tail) and the resolved output format, or the
/// refusal `validate_run` produces before anything is prepared.
fn launch(
    agent: &str,
    run: &AgentRunOptions,
) -> Result<(Vec<String>, ResolvedContainerOptions), EngineError> {
    let def = first_party(agent).unwrap_or_else(|| panic!("{agent} is first-party"));
    def.validate_run(run, RunParadigm::Container)?;
    let resolved = ResolvedContainerOptions::resolve(launch_argv_options(&def, run)?)?;
    let entrypoint = resolved.entrypoint.clone().expect("entrypoint").0;
    let argv = AgentArgv::from_options(&resolved)?.append_to(entrypoint);
    Ok((argv, resolved))
}

fn argv(agent: &str, shape: Shape) -> Vec<String> {
    launch(agent, &run(shape))
        .unwrap_or_else(|e| panic!("{agent}: unexpected refusal: {e}"))
        .0
}

fn refusal(agent: &str, shape: Shape) -> EngineError {
    match launch(agent, &run(shape)) {
        Ok((argv, _)) => panic!("{agent}: expected a refusal, launched {argv:?}"),
        Err(e) => e,
    }
}

fn s(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| p.to_string()).collect()
}

// ── opencode (1.x): unchanged behaviour ────────────────────────────────────

#[test]
fn opencode_v1_argv_is_pinned() {
    let a = "opencode";
    assert_eq!(argv(a, Shape::default()), s(&["opencode"]));
    assert_eq!(
        argv(
            a,
            Shape {
                seed: true,
                ..Default::default()
            }
        ),
        s(&["opencode", "--prompt", SEED])
    );
    assert_eq!(
        argv(
            a,
            Shape {
                seed: true,
                model: Some("anthropic/claude-sonnet-4"),
                auto: true,
                ..Default::default()
            }
        ),
        s(&[
            "opencode",
            "--auto",
            "--model",
            "anthropic/claude-sonnet-4",
            "--prompt",
            SEED
        ])
    );
    assert_eq!(
        argv(
            a,
            Shape {
                headless: true,
                seed: true,
                ..Default::default()
            }
        ),
        s(&["opencode", "run", SEED])
    );
    assert_eq!(
        argv(
            a,
            Shape {
                headless: true,
                seed: true,
                model: Some("anthropic/claude-sonnet-4"),
                auto: true,
                ..Default::default()
            }
        ),
        s(&[
            "opencode",
            "run",
            "--auto",
            "--model",
            "anthropic/claude-sonnet-4",
            SEED
        ])
    );
    assert_eq!(
        argv(
            a,
            Shape {
                acp: true,
                ..Default::default()
            }
        ),
        s(&["opencode", "acp"])
    );
    // --yolo stays the historical silent no-op (strict_modes is false).
    assert_eq!(
        argv(
            a,
            Shape {
                yolo: true,
                seed: true,
                ..Default::default()
            }
        ),
        s(&["opencode", "--prompt", SEED])
    );
    assert!(matches!(
        refusal(
            a,
            Shape {
                plan: true,
                ..Default::default()
            }
        ),
        EngineError::PlanModeUnsupported { .. }
    ));
    let mut tools = run(Shape::default());
    tools.allowed_tools = s(&["Read"]);
    assert!(matches!(
        launch(a, &tools).unwrap_err(),
        EngineError::ToolRestrictionFlagMissing { .. }
    ));
}

// ── opencode-v2 ────────────────────────────────────────────────────────────

#[test]
fn opencode_v2_argv_rows() {
    let a = "opencode-v2";
    assert_eq!(argv(a, Shape::default()), s(&["opencode", "--standalone"]));
    assert_eq!(
        argv(
            a,
            Shape {
                seed: true,
                ..Default::default()
            }
        ),
        s(&["opencode", "--standalone", "--prompt", SEED])
    );
    assert_eq!(
        argv(
            a,
            Shape {
                seed: true,
                yolo: true,
                ..Default::default()
            }
        ),
        s(&["opencode", "--standalone", "--auto", "--prompt", SEED])
    );
    // Interactive --model takes the `mini` interface (with a notice).
    assert_eq!(
        argv(
            a,
            Shape {
                model: Some("anthropic/claude-sonnet-4"),
                ..Default::default()
            }
        ),
        s(&[
            "opencode",
            "--standalone",
            "mini",
            "--model",
            "anthropic/claude-sonnet-4"
        ])
    );
    assert_eq!(
        argv(
            a,
            Shape {
                seed: true,
                model: Some("anthropic/claude-sonnet-4"),
                ..Default::default()
            }
        ),
        s(&[
            "opencode",
            "--standalone",
            "mini",
            "--model",
            "anthropic/claude-sonnet-4",
            "--prompt",
            SEED
        ])
    );
    // Headless: `run`, prompt on stdin (never in argv).
    let (headless, resolved) = launch(
        a,
        &run(Shape {
            headless: true,
            seed: true,
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(headless, s(&["opencode", "--standalone", "run"]));
    assert_eq!(resolved.seeded_prompt.as_deref(), Some(SEED));
    assert!(!resolved.headless_prompt_in_argv);
    assert_eq!(
        argv(
            a,
            Shape {
                headless: true,
                seed: true,
                model: Some("anthropic/claude-sonnet-4"),
                yolo: true,
                ..Default::default()
            }
        ),
        s(&[
            "opencode",
            "--standalone",
            "run",
            "--auto",
            "--model",
            "anthropic/claude-sonnet-4"
        ])
    );
    // No prompt: launches, and the CLI itself reports the missing message.
    assert_eq!(
        argv(
            a,
            Shape {
                headless: true,
                ..Default::default()
            }
        ),
        s(&["opencode", "--standalone", "run"])
    );
}

#[test]
fn opencode_v2_model_notice_only_for_the_interactive_fallback() {
    let def = first_party("opencode-v2").unwrap();
    let notice = def
        .interactive_model_notice(&run(Shape {
            model: Some("a/b"),
            ..Default::default()
        }))
        .expect("interactive --model must tell the user the interface changed");
    assert!(notice.contains("opencode mini"), "{notice}");
    for shape in [
        Shape::default(),
        Shape {
            seed: true,
            ..Default::default()
        },
        Shape {
            headless: true,
            model: Some("a/b"),
            ..Default::default()
        },
    ] {
        assert_eq!(def.interactive_model_notice(&run(shape)), None);
    }
    // No other agent has a fallback.
    for agent in ["opencode", "cursor", "claude"] {
        assert_eq!(
            first_party(agent)
                .unwrap()
                .interactive_model_notice(&run(Shape {
                    model: Some("a/b"),
                    ..Default::default()
                })),
            None
        );
    }
}

#[test]
fn opencode_v2_refusals() {
    let a = "opencode-v2";
    let auto = refusal(
        a,
        Shape {
            auto: true,
            ..Default::default()
        },
    );
    assert!(
        matches!(
            &auto,
            EngineError::AgentModeUnsupported { mode: "auto", .. }
        ),
        "{auto:?}"
    );
    assert!(auto.to_string().contains("Use --yolo"), "{auto}");
    // Interactive --model + a mode: `mini` has no --auto.
    let combo = refusal(
        a,
        Shape {
            model: Some("a/b"),
            yolo: true,
            ..Default::default()
        },
    );
    assert!(matches!(
        &combo,
        EngineError::AgentModeUnsupported { mode: "yolo", .. }
    ));
    assert!(
        combo
            .to_string()
            .contains("cannot combine --model with --yolo")
            && combo.to_string().contains("opencode mini"),
        "{combo}"
    );
    assert!(matches!(
        refusal(
            a,
            Shape {
                plan: true,
                ..Default::default()
            }
        ),
        EngineError::PlanModeUnsupported { .. }
    ));
    assert!(matches!(
        refusal(
            a,
            Shape {
                acp: true,
                ..Default::default()
            }
        ),
        EngineError::AcpUnsupported { .. }
    ));
    for (allowed, disallowed) in [(s(&["Read"]), vec![]), (vec![], s(&["Bash"]))] {
        let mut r = run(Shape::default());
        r.allowed_tools = allowed;
        r.disallowed_tools = disallowed;
        assert!(matches!(
            launch(a, &r).unwrap_err(),
            EngineError::ToolRestrictionFlagMissing { .. }
        ));
    }
}

#[test]
fn opencode_v2_standalone_precedes_every_subcommand_and_user_flags() {
    let def = first_party("opencode-v2").unwrap();
    for headless in [false, true] {
        for model in [None, Some("a/b")] {
            for seed in [false, true] {
                for yolo in [false, true] {
                    let shape = Shape {
                        headless,
                        model,
                        seed,
                        yolo,
                        ..Default::default()
                    };
                    let Ok((argv, _)) = launch("opencode-v2", &run(shape)) else {
                        continue; // the refused interactive model+yolo row
                    };
                    assert_eq!(
                        &argv[..2],
                        &s(&["opencode", "--standalone"])[..],
                        "{argv:?}"
                    );
                    for sub in ["run", "mini"] {
                        if let Some(i) = argv.iter().position(|a| a == sub) {
                            assert!(i > 1, "{argv:?}");
                        }
                    }
                }
            }
        }
    }
    // The sandbox kit entrypoint is the same prefix.
    assert_eq!(def.interactive_entrypoint, s(&["opencode", "--standalone"]));
}

// ── cursor ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_argv_rows() {
    let a = "cursor";
    let base = ["cursor-agent"];
    assert_eq!(argv(a, Shape::default()), s(&base));
    assert_eq!(
        argv(
            a,
            Shape {
                seed: true,
                model: Some("gpt-5"),
                ..Default::default()
            }
        ),
        s(&["cursor-agent", "--model", "gpt-5", SEED])
    );
    assert_eq!(
        argv(
            a,
            Shape {
                seed: true,
                yolo: true,
                ..Default::default()
            }
        ),
        s(&["cursor-agent", "--force", SEED])
    );
    let (text, resolved) = launch(
        a,
        &run(Shape {
            headless: true,
            seed: true,
            model: Some("gpt-5"),
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(
        text,
        s(&[
            "cursor-agent",
            "--print",
            "--trust",
            "--output-format",
            "text",
            "--model",
            "gpt-5",
            SEED
        ])
    );
    assert_eq!(
        resolved.agent_output_format,
        crate::data::agent_output_format::AgentOutputFormat::Text
    );
    let (structured, resolved) = launch(
        a,
        &run(Shape {
            headless: true,
            seed: true,
            structured: true,
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(
        structured,
        s(&[
            "cursor-agent",
            "--print",
            "--trust",
            "--output-format",
            "stream-json",
            SEED
        ])
    );
    assert_eq!(
        resolved.agent_output_format,
        crate::data::agent_output_format::AgentOutputFormat::CursorStreamJson
    );
    assert_eq!(
        argv(
            a,
            Shape {
                headless: true,
                seed: true,
                yolo: true,
                ..Default::default()
            }
        ),
        s(&[
            "cursor-agent",
            "--print",
            "--trust",
            "--output-format",
            "text",
            "--force",
            SEED
        ])
    );
    // structured_output on an interactive launch never changes the format.
    assert_eq!(
        argv(
            a,
            Shape {
                structured: true,
                ..Default::default()
            }
        ),
        s(&base)
    );
}

#[test]
fn cursor_has_exactly_one_output_format_on_every_headless_shape() {
    for structured in [false, true] {
        for yolo in [false, true] {
            for model in [None, Some("m")] {
                let argv = argv(
                    "cursor",
                    Shape {
                        headless: true,
                        seed: true,
                        structured,
                        yolo,
                        model,
                        ..Default::default()
                    },
                );
                assert_eq!(
                    argv.iter().filter(|a| *a == "--output-format").count(),
                    1,
                    "{argv:?}"
                );
            }
        }
    }
}

#[test]
fn cursor_refusals() {
    let a = "cursor";
    let auto = refusal(
        a,
        Shape {
            auto: true,
            ..Default::default()
        },
    );
    assert!(matches!(
        &auto,
        EngineError::AgentModeUnsupported { mode: "auto", .. }
    ));
    assert!(
        auto.to_string()
            .contains("no verified equivalent of --auto"),
        "{auto}"
    );
    assert!(matches!(
        refusal(
            a,
            Shape {
                plan: true,
                headless: true,
                ..Default::default()
            }
        ),
        EngineError::PlanModeUnsupported { .. }
    ));
    assert!(matches!(
        refusal(
            a,
            Shape {
                acp: true,
                ..Default::default()
            }
        ),
        EngineError::AcpUnsupported { .. }
    ));
    let mut r = run(Shape {
        headless: true,
        ..Default::default()
    });
    r.disallowed_tools = s(&["Shell"]);
    assert!(matches!(
        launch(a, &r).unwrap_err(),
        EngineError::ToolRestrictionFlagMissing { .. }
    ));
}

#[test]
fn cursor_force_appears_only_on_an_explicit_yolo() {
    let forced = |argv: &[String]| {
        argv.iter()
            .any(|a| a == "--force" || a == "-f" || a == "--yolo")
    };
    for headless in [false, true] {
        for model in [None, Some("m")] {
            for structured in [false, true] {
                for seed in [false, true] {
                    let shape = Shape {
                        headless,
                        model,
                        structured,
                        seed,
                        ..Default::default()
                    };
                    assert!(
                        !forced(&argv("cursor", shape)),
                        "{headless} {model:?} {structured} {seed}"
                    );
                    let yolo = argv(
                        "cursor",
                        Shape {
                            yolo: true,
                            ..shape
                        },
                    );
                    assert_eq!(
                        yolo.iter().filter(|a| *a == "--force").count(),
                        1,
                        "{yolo:?}"
                    );
                }
            }
        }
    }
    let def = first_party("cursor").unwrap();
    assert!(def.headless_only_flags.iter().all(|f| f != "--force"));
    assert_eq!(def.yolo_flags, s(&["--force"]));
}

#[test]
fn cursor_api_key_never_reaches_argv_or_literal_env() {
    use crate::data::secret::{CommandSecrets, SecretString};
    let secret = "test-secret-cursor-key-0157";
    let mut r = run(Shape {
        headless: true,
        seed: true,
        ..Default::default()
    });
    r.env_passthrough = Some(s(&["CURSOR_API_KEY"]));
    r.command_secrets = Some(std::sync::Arc::new(
        CommandSecrets::new([("CURSOR_API_KEY".to_string(), SecretString::new(secret))]).unwrap(),
    ));
    let (argv, resolved) = launch("cursor", &r).unwrap();
    assert!(
        argv.iter().all(|a| !a.contains(secret) && a != "--api-key"),
        "{argv:?}"
    );
    assert!(resolved
        .env_literal
        .iter()
        .all(|e| !e.value.contains(secret)));
    let def = first_party("cursor").unwrap();
    assert!(def.static_env.iter().all(|(k, _)| k != "CURSOR_API_KEY"));
    assert!(
        def.auth_env.is_empty(),
        "CURSOR_API_KEY only through an explicit env()/secret"
    );
    // The secret is delivered as a private override by the session builder.
    assert_eq!(r.secret_env().len(), 1);
}

// ── cross-agent invariants ─────────────────────────────────────────────────

#[test]
fn ready_probe_and_launch_use_the_same_executable() {
    for agent in crate::engine::agent::first_party::FIRST_PARTY_AGENTS {
        let Some(def) = first_party(agent) else {
            continue; // catalogue and table may be briefly out of step
        };
        if let Some(requirement) = &def.cli_version {
            assert_eq!(
                Some(&requirement.binary),
                def.interactive_entrypoint.first(),
                "{agent}"
            );
            assert!(
                def.ping_argv.is_none(),
                "{agent}: a host ping could satisfy readiness"
            );
        }
    }
    for agent in [
        "claude",
        "codex",
        "cursor",
        "copilot",
        "cline",
        "antigravity",
    ] {
        assert!(first_party(agent).unwrap().cli_version.is_none(), "{agent}");
    }
    for agent in ["opencode", "opencode-v2"] {
        assert!(first_party(agent).unwrap().cli_version.is_some(), "{agent}");
    }
}

#[test]
fn the_two_opencode_majors_are_distinct_definitions() {
    let v1 = first_party("opencode").unwrap();
    let v2 = first_party("opencode-v2").unwrap();
    assert_ne!(v1.interactive_entrypoint, v2.interactive_entrypoint);
    assert_eq!(v1.cli_version.as_ref().unwrap().required_major, 1);
    assert_eq!(v2.cli_version.as_ref().unwrap().required_major, 2);
    assert_ne!(v1.session_storage_dirs, v2.session_storage_dirs);
    // V2 never mounts the live host config or data (SQLite) directory.
    assert_eq!(v2.settings_mount, crate::engine::agent::SettingsMount::None);
    assert!(v2.extra_settings_mounts.is_empty());
    let paths: Vec<_> = v2
        .session_storage_dirs
        .iter()
        .map(|d| (d.home_relative_path.as_str(), d.seed_from_home.as_deref()))
        .collect();
    assert_eq!(
        paths,
        vec![
            (".config/opencode", Some(".config/opencode")),
            (".local/share/opencode", None),
            (".local/state/opencode", None),
        ]
    );
    assert_eq!(v2.skills_mount.as_deref(), Some(".config/opencode/skills"));
    assert!(v2.launch_note.is_some());
    assert_eq!(
        v2.system_prompt_delivery,
        crate::engine::agent::SystemPromptMode::AgentsMd
    );
}

#[test]
fn cursor_state_is_private_and_rules_stay_in_the_workspace() {
    let def = first_party("cursor").unwrap();
    assert_eq!(
        def.settings_mount,
        crate::engine::agent::SettingsMount::None
    );
    assert!(def.extra_settings_mounts.is_empty());
    assert_eq!(def.session_storage_dirs.len(), 1);
    let dir = &def.session_storage_dirs[0];
    assert_eq!(dir.home_relative_path, ".cursor");
    assert_eq!(dir.env_var.as_deref(), Some("CURSOR_CONFIG_DIR"));
    assert_eq!(dir.seed_from_home, None, "no host Cursor state is copied");
    assert_eq!(def.skills_mount.as_deref(), Some(".cursor/skills"));
    assert_eq!(
        def.system_prompt_delivery,
        crate::engine::agent::SystemPromptMode::AgentsMd
    );
    assert!(def.sandbox_auth_note.is_some());
}

#[test]
fn gemini_is_not_a_built_in_definition_and_antigravity_is_unchanged() {
    assert!(first_party("gemini").is_none());
    let agy = first_party("antigravity").unwrap();
    assert_eq!(agy.interactive_entrypoint, s(&["agy"]));
    assert_eq!(
        agy.settings_mount,
        crate::engine::agent::SettingsMount::Antigravity
    );
    assert_eq!(
        agy.skills_mount.as_deref(),
        Some(".gemini/antigravity-cli/skills")
    );
    assert_eq!(
        agy.credential_source,
        crate::engine::agent::CredentialSource::AntigravityKeychainFile
    );
    assert!(agy.cli_version.is_none() && !agy.strict_modes);
}

#[test]
fn every_first_party_agent_without_a_pin_keeps_lenient_modes() {
    // Strict refusal is opt-in per definition; existing agents keep their
    // silent-no-op behaviour for flagless modes.
    for agent in [
        "claude",
        "codex",
        "opencode",
        "copilot",
        "cline",
        "antigravity",
    ] {
        assert!(!first_party(agent).unwrap().strict_modes, "{agent}");
    }
}

#[test]
fn automation_defaults_do_not_authorize_strict_agent_yolo() {
    for agent in ["cursor", "opencode-v2"] {
        let def = first_party(agent).unwrap();
        let run = AgentRunOptions {
            yolo: Some(YoloMode::Enabled),
            non_interactive: true,
            ..Default::default()
        };
        assert!(
            def.validate_run(&run, RunParadigm::Container).is_err(),
            "{agent}"
        );
    }
}

#[test]
fn cursor_dash_leading_prompt_is_refused_in_both_modes() {
    let def = first_party("cursor").unwrap();
    for non_interactive in [false, true] {
        for prompt in ["--force", "-p", "- bullet"] {
            let run = AgentRunOptions {
                initial_prompt: Some(prompt.into()),
                non_interactive,
                ..Default::default()
            };
            assert!(def.validate_run(&run, RunParadigm::Container).is_err());
        }
    }
}

#[test]
fn private_state_agents_refuse_shared_sandboxes() {
    for agent in ["cursor", "opencode-v2"] {
        let def = first_party(agent).unwrap();
        assert!(def
            .validate_run(&AgentRunOptions::default(), RunParadigm::Sandbox)
            .is_err());
    }
}

#[test]
fn env_grant_values_are_frozen_with_their_redaction_before_launch() {
    use crate::data::config::env::EnvSnapshot;
    use crate::engine::container::options::{ContainerOption, EnvVar};
    let before = EnvSnapshot::from_full_env(
        [("CURSOR_API_KEY".into(), "synthetic-selected-value".into())].into(),
    );
    let mut options = ResolvedContainerOptions::resolve_in(
        [ContainerOption::EnvPassthrough(EnvVar(
            "CURSOR_API_KEY".into(),
        ))],
        &before,
    )
    .unwrap();
    options.host_env = Some(EnvSnapshot::from_full_env(
        [("CURSOR_API_KEY".into(), "synthetic-later-value".into())].into(),
    ));
    assert_eq!(
        options.resolved_env_grants.as_ref().unwrap()[0].1.expose(),
        "synthetic-selected-value"
    );
    assert_eq!(
        options.output_redaction.redact("synthetic-selected-value"),
        "***"
    );
    assert!(!format!("{options:?}").contains("synthetic-selected-value"));
}
