//! `CommandCatalogue` completeness and dispatch invariant tests.
//!
//! Parity matrix items 1–22 require the binary as a subprocess.
//! This file verifies the catalogue itself (path lookup, alias resolution,
//! flag enumeration) which is a pure in-memory check.

use awman::command::dispatch::catalogue::CommandCatalogue;

// ─── Top-level command coverage ──────────────────────────────────────────────

fn top_level_names() -> Vec<&'static str> {
    CommandCatalogue::get()
        .root()
        .subcommands
        .iter()
        .map(|s| s.name)
        .collect()
}

#[test]
fn catalogue_has_init_command() {
    assert!(top_level_names().contains(&"init"));
}

#[test]
fn catalogue_has_ready_command() {
    assert!(top_level_names().contains(&"ready"));
}

#[test]
fn catalogue_has_chat_command() {
    assert!(top_level_names().contains(&"chat"));
}

#[test]
fn catalogue_has_specs_command() {
    assert!(top_level_names().contains(&"specs"));
}

#[test]
fn catalogue_has_agents_command() {
    assert!(top_level_names().contains(&"agents"));
}

#[test]
fn catalogue_has_config_command() {
    assert!(top_level_names().contains(&"config"));
}

#[test]
fn catalogue_has_exec_command() {
    assert!(top_level_names().contains(&"exec"));
}

#[test]
fn catalogue_has_api_command() {
    assert!(top_level_names().contains(&"api"));
}

#[test]
fn catalogue_has_remote_command() {
    assert!(top_level_names().contains(&"remote"));
}

#[test]
fn catalogue_has_new_command() {
    assert!(top_level_names().contains(&"new"));
}

// ─── Subcommand coverage ─────────────────────────────────────────────────────

fn subcommand_names(parent: &str) -> Vec<&'static str> {
    CommandCatalogue::get()
        .lookup(&[parent])
        .expect("parent command must exist")
        .subcommands
        .iter()
        .map(|s| s.name)
        .collect()
}

#[test]
fn specs_has_amend_subcommand() {
    let names = subcommand_names("specs");
    assert!(names.contains(&"amend"), "missing 'amend': {names:?}");
}

#[test]
fn config_has_show_get_set_subcommands() {
    let names = subcommand_names("config");
    assert!(names.contains(&"show"));
    assert!(names.contains(&"get"));
    assert!(names.contains(&"set"));
}

#[test]
fn exec_has_prompt_and_workflow_subcommands() {
    let names = subcommand_names("exec");
    assert!(names.contains(&"prompt"));
    assert!(names.contains(&"workflow"));
}

#[test]
fn api_has_start_kill_logs_status_subcommands() {
    let names = subcommand_names("api");
    assert!(names.contains(&"start"));
    assert!(names.contains(&"kill"));
    assert!(names.contains(&"logs"));
    assert!(names.contains(&"status"));
}

#[test]
fn remote_has_exec_and_session_subcommands() {
    let names = subcommand_names("remote");
    assert!(names.contains(&"exec"));
    assert!(names.contains(&"session"));
}

#[test]
fn new_has_spec_workflow_skill_subcommands() {
    let names = subcommand_names("new");
    assert!(names.contains(&"spec"));
    assert!(names.contains(&"workflow"));
    assert!(names.contains(&"skill"));
}

#[test]
fn remote_session_has_start_and_kill_subcommands() {
    let cat = CommandCatalogue::get();
    let remote = cat.lookup(&["remote"]).unwrap();
    let session = remote.find_subcommand("session").unwrap();
    let sub_names: Vec<&str> = session.subcommands.iter().map(|s| s.name).collect();
    assert!(sub_names.contains(&"start"));
    assert!(sub_names.contains(&"kill"));
}

// ─── Flag enumeration ─────────────────────────────────────────────────────────

#[test]
fn init_has_agent_flag() {
    let cat = CommandCatalogue::get();
    let init = cat.lookup(&["init"]).unwrap();
    assert!(
        init.find_flag("agent").is_some(),
        "init must have --agent flag"
    );
}

#[test]
fn init_agent_flag_is_free_form_and_names_first_party_agents() {
    use awman::command::dispatch::catalogue::FlagKind;
    let cat = CommandCatalogue::get();
    let init = cat.lookup(&["init"]).unwrap();
    let flag = init.find_flag("agent").unwrap();
    // Custom agents are valid values, so the flag cannot be a closed enum;
    // `init` validates the name against the agent registry instead.
    assert!(
        matches!(flag.kind, FlagKind::String),
        "--agent should be a String"
    );
    for agent in awman::engine::agent::first_party::FIRST_PARTY_AGENTS {
        assert!(
            flag.help.contains(agent),
            "--agent help should name {agent:?}: {}",
            flag.help
        );
    }
    assert!(flag.help.contains(".awman/agents/"));
}

#[test]
fn ready_has_build_flag() {
    let cat = CommandCatalogue::get();
    let ready = cat.lookup(&["ready"]).unwrap();
    assert!(ready.find_flag("build").is_some());
}

#[test]
fn ready_has_no_cache_flag() {
    let cat = CommandCatalogue::get();
    let ready = cat.lookup(&["ready"]).unwrap();
    assert!(ready.find_flag("no-cache").is_some());
}

#[test]
fn ready_has_json_flag() {
    let cat = CommandCatalogue::get();
    let ready = cat.lookup(&["ready"]).unwrap();
    assert!(
        ready.find_flag("json").is_some(),
        "ready must have --json flag for machine-readable output"
    );
}

#[test]
fn exec_workflow_has_yolo_flag() {
    let cat = CommandCatalogue::get();
    let wf = cat.lookup(&["exec", "workflow"]).unwrap();
    assert!(wf.find_flag("yolo").is_some());
}

#[test]
fn exec_workflow_has_wf_alias() {
    let cat = CommandCatalogue::get();
    let wf = cat.lookup(&["exec", "workflow"]).unwrap();
    assert!(
        wf.aliases.contains(&"wf"),
        "`exec workflow` must have 'wf' alias"
    );
}

#[test]
fn api_start_has_port_flag() {
    let cat = CommandCatalogue::get();
    let start = cat.lookup(&["api", "start"]).unwrap();
    assert!(start.find_flag("port").is_some());
}

#[test]
fn api_start_has_background_flag() {
    let cat = CommandCatalogue::get();
    let start = cat.lookup(&["api", "start"]).unwrap();
    assert!(start.find_flag("background").is_some());
}

#[test]
fn api_start_has_refresh_key_flag() {
    let cat = CommandCatalogue::get();
    let start = cat.lookup(&["api", "start"]).unwrap();
    assert!(start.find_flag("refresh-key").is_some());
}

#[test]
fn api_start_has_dangerously_skip_auth_flag() {
    let cat = CommandCatalogue::get();
    let start = cat.lookup(&["api", "start"]).unwrap();
    assert!(start.find_flag("dangerously-skip-auth").is_some());
}

#[test]
fn new_workflow_has_format_flag() {
    let cat = CommandCatalogue::get();
    let wf = cat.lookup(&["new", "workflow"]).unwrap();
    assert!(wf.find_flag("format").is_some());
}

#[test]
fn new_workflow_has_global_flag() {
    let cat = CommandCatalogue::get();
    let wf = cat.lookup(&["new", "workflow"]).unwrap();
    assert!(wf.find_flag("global").is_some());
}

#[test]
fn new_skill_has_global_flag() {
    let cat = CommandCatalogue::get();
    let skill = cat.lookup(&["new", "skill"]).unwrap();
    assert!(skill.find_flag("global").is_some());
}

#[test]
fn status_has_watch_flag() {
    let cat = CommandCatalogue::get();
    let status = cat.lookup(&["agents", "status"]).unwrap();
    assert!(status.find_flag("watch").is_some());
}

#[test]
fn lookup_nonexistent_command_returns_none() {
    let cat = CommandCatalogue::get();
    assert!(cat.lookup(&["nonexistent"]).is_none());
}

#[test]
fn lookup_deeply_nested_path() {
    let cat = CommandCatalogue::get();
    assert!(cat.lookup(&["remote", "session", "start"]).is_some());
    assert!(cat.lookup(&["remote", "session", "kill"]).is_some());
}

/// Drive the real catalogue and ChatCommand until the recording runtime's
/// launch boundary. Stopping there keeps the test daemon-free while checking
/// the actual container options assembled across all three layers.
#[tokio::test]
async fn chat_custom_pi_dispatches_expected_container_options() {
    use crate::helpers::{custom_agents::*, IsolatedEnv};
    use awman::command::commands::api_server::event_bus::EventBus;
    use awman::command::dispatch::Dispatch;
    use awman::engine::agent::AgentEngine;
    use awman::engine::agent_runtime::ResolvedAgentOptions;
    use awman::engine::container::options::ModelFlagForm;
    use awman::frontend::api::command_frontend::ApiDispatchFrontend;
    use std::sync::Arc;

    // auth.env forwards only variables that exist on the host. This unique
    // synthetic value is never shared with a real runtime or host agent.
    struct TokenGuard(Option<std::ffi::OsString>);
    impl Drop for TokenGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => std::env::set_var("WI0130_TOKEN", value),
                None => std::env::remove_var("WI0130_TOKEN"),
            }
        }
    }
    let _token = TokenGuard(std::env::var_os("WI0130_TOKEN"));
    std::env::set_var("WI0130_TOKEN", "synthetic-test-token");

    for (mode, expected) in [
        ("--yolo", "--reckless"),
        ("--auto", "--automatic"),
        ("--plan", "--planning"),
    ] {
        let fixture = IsolatedEnv::new();
        write_definition(fixture.git_root.path(), "pi", INSTALL);
        std::fs::create_dir_all(fixture.home_dir.path().join(".pi")).unwrap();
        std::fs::write(
            fixture.git_root.path().join("Dockerfile.dev"),
            "FROM scratch\n",
        )
        .unwrap();
        let session = fixture.open_session();
        approve(&session, "pi");
        let runtime = Arc::new(RecordingRuntime::default());
        let mut engines = fixture.engines(false);
        engines.runtime = runtime.clone();
        engines.agent_engine = Arc::new(AgentEngine::new(
            engines.overlay_engine.clone(),
            runtime.clone(),
        ));
        let bus = EventBus::new(32);
        let argv = [
            "--agent",
            "pi",
            "--model",
            "test-model",
            mode,
            "--overlay",
            "context(repo)",
        ]
        .map(String::from);
        let frontend = ApiDispatchFrontend::new("chat", &argv, bus.sender());
        let dispatch = Dispatch::new(
            frontend,
            Arc::new(tokio::sync::RwLock::new(session)),
            engines,
        );
        let error = dispatch.run_command(&["chat"]).await.unwrap_err();
        assert!(
            error.to_string().contains("WI0130 recorded launch"),
            "{error}"
        );
        let options = runtime.options.lock().unwrap();
        assert_eq!(options.len(), 1);
        let ResolvedAgentOptions::Container(options) = &options[0] else {
            panic!("chat should produce container options")
        };
        assert_eq!(
            options.entrypoint.as_ref().unwrap().0,
            ["pi", "--interactive", "run"]
        );
        // The command catalogue's --auto shorthand also enables yolo.
        let expected = if mode == "--auto" {
            vec!["--reckless", expected]
        } else {
            vec![expected]
        };
        assert_eq!(options.agent_mode_flags, expected);
        assert_eq!(
            options.model,
            Some(ModelFlagForm::Shorthand("--model=test-model".into()))
        );
        let (flag, prompt) = options
            .system_prompt_inline
            .as_ref()
            .expect("context overlay delivers system prompt");
        assert_eq!(flag, "--instructions");
        assert!(prompt.contains("/awman/context/repo"), "{prompt}");
        assert!(options.overlays.iter().any(|overlay| overlay.host_path
            == fixture.home_dir.path().join(".pi")
            && overlay.container_path == std::path::Path::new("/root/.pi")));
        assert!(options
            .env_passthrough
            .iter()
            .any(|variable| variable.0 == "WI0130_TOKEN"));
        assert!(options
            .env_literal
            .iter()
            .any(|variable| variable.key == "PI_STATIC" && variable.value == "fixture"));
    }
}

#[tokio::test]
async fn workflow_mixed_agents_refuses_untrusted_before_start_and_deleted_definition_is_unknown() {
    use crate::helpers::{custom_agents::*, IsolatedEnv};
    use awman::command::commands::api_server::event_bus::EventBus;
    use awman::command::dispatch::Dispatch;
    use awman::engine::agent::AgentEngine;
    use awman::frontend::api::command_frontend::ApiDispatchFrontend;
    use std::sync::Arc;

    for deleted in [false, true] {
        let fixture = IsolatedEnv::new();
        let root = fixture.git_root.path();
        write_definition(root, "pi", INSTALL);
        write_definition(root, "approved", INSTALL);
        let session = fixture.open_session();
        approve(&session, "approved");
        let workflow = root.join("mixed.toml");
        std::fs::write(&workflow, "[[steps]]\nname = 'first'\nagent = 'claude'\nprompt = 'first'\n[[steps]]\nname = 'approved'\nagent = 'approved'\nprompt = 'second'\n[[steps]]\nname = 'custom'\nagent = 'pi'\nprompt = 'third'\n").unwrap();
        if deleted {
            std::fs::remove_file(root.join(".awman/agents/pi.toml")).unwrap();
            std::fs::write(
                root.join(".awman/Dockerfile.pi"),
                "# awman-generated from repo:deleted\nFROM scratch\n",
            )
            .unwrap();
        }
        let runtime = Arc::new(RecordingRuntime::default());
        let mut engines = fixture.engines(false);
        engines.runtime = runtime.clone();
        engines.agent_engine = Arc::new(AgentEngine::new(
            engines.overlay_engine.clone(),
            runtime.clone(),
        ));
        let bus = EventBus::new(64);
        let argv = vec![workflow.display().to_string(), "--non-interactive".into()];
        let frontend = ApiDispatchFrontend::new("exec workflow", &argv, bus.sender());
        let dispatch = Dispatch::new(
            frontend,
            Arc::new(tokio::sync::RwLock::new(session)),
            engines,
        );
        let error = dispatch
            .run_command(&["exec", "workflow"])
            .await
            .unwrap_err();
        if deleted {
            assert!(
                error.to_string().contains("unknown agent 'pi'"),
                "{error:?}"
            );
            assert_eq!(error.exit_code(), 4);
        } else {
            assert!(
                error.to_string().contains("has not been approved"),
                "{error:?}"
            );
            assert!(
                error.to_string().contains("awman ready --agent pi"),
                "{error:?}"
            );
        }
        assert!(
            runtime.options.lock().unwrap().is_empty(),
            "preflight must finish before a container is created"
        );
        assert!(
            runtime.builder.builds.lock().unwrap().is_empty(),
            "preflight must finish before images are built"
        );
    }
}

#[tokio::test]
async fn ready_custom_crush_sbx_command_reports_unsupported_as_failed_step() {
    use crate::helpers::{custom_agents::*, IsolatedEnv};
    use awman::command::commands::api_server::event_bus::EventBus;
    use awman::command::dispatch::Dispatch;
    use awman::engine::agent::AgentEngine;
    use awman::frontend::api::command_frontend::ApiDispatchFrontend;
    use std::sync::Arc;
    let fixture = IsolatedEnv::new();
    write_definition(
        fixture.git_root.path(),
        "crush",
        include_str!("../../skills/create-custom-agent/examples/crush.toml"),
    );
    let session = fixture.open_session();
    approve(&session, "crush");
    let runtime = Arc::new(RecordingRuntime::sandbox());
    let mut engines = fixture.engines(false);
    engines.runtime = runtime.clone();
    engines.agent_engine = Arc::new(AgentEngine::new(
        engines.overlay_engine.clone(),
        runtime.clone(),
    ));
    let bus = EventBus::new(64);
    let frontend =
        ApiDispatchFrontend::new("ready", &["--agent".into(), "crush".into()], bus.sender());
    let dispatch = Dispatch::new(
        frontend,
        Arc::new(tokio::sync::RwLock::new(session)),
        engines,
    );
    let error = dispatch.run_command(&["ready"]).await.unwrap_err();
    assert!(
        matches!(
            error,
            awman::command::error::CommandError::Engine(
                awman::engine::error::EngineError::CustomAgentRuntimeUnsupported { .. }
            )
        ),
        "{error:?}"
    );
    assert!(runtime.builder.builds.lock().unwrap().is_empty());
    assert!(runtime.options.lock().unwrap().is_empty());
    assert!(!fixture.home_dir.path().join("kits").exists());
}
