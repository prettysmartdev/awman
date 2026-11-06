//! `yoloDisallowedTools` is read from the repo and global config files on disk
//! and reaches a `--yolo` launch's container options; an agent that cannot
//! enforce it is refused rather than launched without the restriction.

use awman::data::config::env::EnvSnapshot;
use awman::data::fs::data_paths::AWMAN_HOME;
use awman::data::session::{AgentName, Session, SessionOpenOptions, StaticGitRootResolver};
use awman::engine::agent::{AgentEngine, AgentRunOptions};
use awman::engine::container::options::{ContainerOption, YoloMode};
use awman::engine::error::EngineError;
use awman::engine::overlay::OverlayEngine;

use std::sync::Arc;

/// A repo at `<tmp>/repo` with an optional repo config, and a global config
/// home at `<tmp>/home` with an optional global config.
fn open_session(tmp: &std::path::Path, repo: Option<&str>, global: Option<&str>) -> Session {
    let root = tmp.join("repo");
    std::fs::create_dir_all(root.join(".awman")).unwrap();
    if let Some(json) = repo {
        std::fs::write(root.join(".awman/config.json"), json).unwrap();
    }
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    if let Some(json) = global {
        std::fs::write(home.join("config.json"), json).unwrap();
    }
    let env = EnvSnapshot::with_overrides([(AWMAN_HOME, home.to_str().unwrap())]);
    let resolver = StaticGitRootResolver::new(&root);
    let opts = SessionOpenOptions {
        flags: Default::default(),
        env: Some(env),
        available_agents: None,
        overrides: Default::default(),
    };
    Session::open(root, &resolver, opts).expect("Session::open")
}

fn engine(tmp: &std::path::Path) -> AgentEngine {
    let overlay = OverlayEngine::with_auth_resolver(
        awman::data::fs::auth_paths::AuthPathResolver::at_home(tmp.join("auth-home")),
    )
    .with_secret_files_provider(Arc::new(|_| Vec::new()));
    AgentEngine::new(
        Arc::new(overlay),
        Arc::new(awman::engine::container::ContainerRuntime::docker()),
    )
}

fn yolo() -> AgentRunOptions {
    AgentRunOptions {
        yolo: Some(YoloMode::Enabled),
        ..Default::default()
    }
}

fn disallowed(opts: &[ContainerOption]) -> Vec<String> {
    opts.iter()
        .filter_map(|o| match o {
            ContainerOption::DisallowedTools(v) => Some(v.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

#[test]
fn global_yolo_disallowed_tools_reach_a_claude_yolo_launch() {
    let tmp = tempfile::tempdir().unwrap();
    let session = open_session(
        tmp.path(),
        None,
        Some(r#"{"agents":{"yolo":{"disallowedTools":["Bash", "computer"]}}}"#),
    );
    let claude = AgentName::new("claude").unwrap();
    let opts = engine(tmp.path())
        .build_options(&session, &claude, &yolo())
        .unwrap();
    assert_eq!(disallowed(&opts), vec!["Bash", "computer"]);
    assert!(opts
        .iter()
        .any(|o| matches!(o, ContainerOption::DisallowedToolsFlag(f) if f == "--disallowedTools")));
}

#[test]
fn repo_yolo_disallowed_tools_replace_the_global_list() {
    let tmp = tempfile::tempdir().unwrap();
    let session = open_session(
        tmp.path(),
        Some(r#"{"agents":{"yolo":{"disallowedTools":["WebFetch"]}}}"#),
        Some(r#"{"agents":{"yolo":{"disallowedTools":["Bash"]}}}"#),
    );
    let claude = AgentName::new("claude").unwrap();
    let opts = engine(tmp.path())
        .build_options(&session, &claude, &yolo())
        .unwrap();
    assert_eq!(disallowed(&opts), vec!["WebFetch"]);
}

#[test]
fn yolo_launch_of_an_agent_without_a_disallowed_tools_flag_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let session = open_session(
        tmp.path(),
        None,
        Some(r#"{"agents":{"yolo":{"disallowedTools":["Bash"]}}}"#),
    );
    let engine = engine(tmp.path());
    for agent in ["codex", "gemini", "opencode", "copilot", "cline"] {
        let name = AgentName::new(agent).unwrap();
        let err = engine.build_options(&session, &name, &yolo()).unwrap_err();
        assert!(
            matches!(err, EngineError::YoloDisallowedToolsUnsupported { .. }),
            "{agent}: {err:?}"
        );
    }
    // An empty repo list turns the global one off for this repo.
    let tmp = tempfile::tempdir().unwrap();
    let session = open_session(
        tmp.path(),
        Some(r#"{"agents":{"yolo":{"disallowedTools":[]}}}"#),
        Some(r#"{"agents":{"yolo":{"disallowedTools":["Bash"]}}}"#),
    );
    let codex = AgentName::new("codex").unwrap();
    engine.build_options(&session, &codex, &yolo()).unwrap();
}
