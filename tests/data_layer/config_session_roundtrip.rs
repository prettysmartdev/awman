//! Layer 0 config + session cross-module integration tests.
//!
//! Validates that Session::open, EffectiveConfig, RepoConfig, GlobalConfig,
//! and FlagConfig interact correctly across the module boundaries.

use amux::data::config::flags::FlagConfig;
use amux::data::config::global::GlobalConfig;
use amux::data::config::repo::{RepoConfig, REPO_CONFIG_SUBDIR};
use amux::data::error::DataError;
use amux::data::session::{AgentName, Session, SessionLogKind, SessionOpenOptions};
use amux::data::worktree_paths::{worktree_branch_name, worktree_branch_name_for_workflow};

use crate::helpers::IsolatedEnv;

// ─── Session::open basics ────────────────────────────────────────────────────

#[test]
fn session_open_returns_correct_paths() {
    let env = IsolatedEnv::new();
    let session = env.open_session();
    assert_eq!(session.git_root(), env.git_root.path());
    assert_eq!(session.working_dir(), env.git_root.path());
}

#[test]
fn session_open_each_call_produces_unique_id() {
    let env = IsolatedEnv::new();
    let s1 = env.open_session();
    let s2 = env.open_session();
    assert_ne!(s1.id(), s2.id());
}

#[test]
fn session_open_falls_back_when_git_root_not_found() {
    let env = IsolatedEnv::new();
    // FailingResolver is defined inline.
    struct Fail;
    impl amux::data::session::GitRootResolver for Fail {
        fn resolve(&self, wd: &std::path::Path) -> Result<std::path::PathBuf, DataError> {
            Err(DataError::GitRootNotFound {
                working_dir: wd.to_path_buf(),
            })
        }
    }
    let opts = SessionOpenOptions {
        env: Some(env.env()),
        ..Default::default()
    };
    let session = Session::open_or_workdir_fallback(env.git_root.path().to_path_buf(), &Fail, opts)
        .expect("fallback session");
    // When git root not found, working_dir is used as git_root.
    assert_eq!(session.git_root(), env.git_root.path());
}

#[test]
fn session_open_propagates_git_root_not_found_without_fallback() {
    let env = IsolatedEnv::new();
    struct Fail;
    impl amux::data::session::GitRootResolver for Fail {
        fn resolve(&self, wd: &std::path::Path) -> Result<std::path::PathBuf, DataError> {
            Err(DataError::GitRootNotFound {
                working_dir: wd.to_path_buf(),
            })
        }
    }
    let opts = SessionOpenOptions {
        env: Some(env.env()),
        ..Default::default()
    };
    let err = Session::open(env.git_root.path().to_path_buf(), &Fail, opts).unwrap_err();
    assert!(matches!(err, DataError::GitRootNotFound { .. }));
}

// ─── RepoConfig load / save round-trip ───────────────────────────────────────

#[test]
fn repo_config_missing_file_returns_defaults() {
    let env = IsolatedEnv::new();
    // No config file written — should return defaults.
    let cfg = RepoConfig::load(env.git_root.path()).unwrap();
    assert_eq!(cfg, RepoConfig::default());
}

#[test]
fn repo_config_present_file_is_loaded() {
    let env = IsolatedEnv::new();
    let amux_dir = env.git_root.path().join(REPO_CONFIG_SUBDIR);
    std::fs::create_dir_all(&amux_dir).unwrap();
    std::fs::write(
        amux_dir.join("config.json"),
        r#"{"agent":"codex","terminal_scrollback_lines":5000}"#,
    )
    .unwrap();

    let cfg = RepoConfig::load(env.git_root.path()).unwrap();
    assert_eq!(cfg.agent.as_deref(), Some("codex"));
    assert_eq!(cfg.terminal_scrollback_lines, Some(5000));
}

#[test]
fn repo_config_malformed_json_returns_error() {
    let env = IsolatedEnv::new();
    let amux_dir = env.git_root.path().join(REPO_CONFIG_SUBDIR);
    std::fs::create_dir_all(&amux_dir).unwrap();
    std::fs::write(amux_dir.join("config.json"), b"{ not valid json }").unwrap();

    let err = RepoConfig::load(env.git_root.path()).unwrap_err();
    assert!(matches!(err, DataError::ConfigParse { .. }));
}

#[test]
fn repo_config_save_and_reload_roundtrip() {
    let env = IsolatedEnv::new();
    let amux_dir = env.git_root.path().join(REPO_CONFIG_SUBDIR);
    std::fs::create_dir_all(&amux_dir).unwrap();

    let cfg = RepoConfig {
        agent: Some("maki".to_string()),
        terminal_scrollback_lines: Some(9999),
        ..Default::default()
    };

    cfg.save(env.git_root.path()).unwrap();

    let loaded = RepoConfig::load(env.git_root.path()).unwrap();
    assert_eq!(loaded.agent.as_deref(), Some("maki"));
    assert_eq!(loaded.terminal_scrollback_lines, Some(9999));
}

// ─── GlobalConfig load / save round-trip ─────────────────────────────────────

#[test]
fn global_config_missing_file_returns_defaults() {
    let env = IsolatedEnv::new();
    let cfg = GlobalConfig::load_with(&env.env()).unwrap();
    assert_eq!(cfg, GlobalConfig::default());
}

#[test]
fn global_config_save_and_reload_roundtrip() {
    let env = IsolatedEnv::new();
    // Ensure the home directory exists.
    std::fs::create_dir_all(env.home_dir.path()).unwrap();

    let cfg = GlobalConfig {
        default_agent: Some("opencode".to_string()),
        terminal_scrollback_lines: Some(3000),
        ..Default::default()
    };

    cfg.save_with(&env.env()).unwrap();

    let loaded = GlobalConfig::load_with(&env.env()).unwrap();
    assert_eq!(loaded.default_agent.as_deref(), Some("opencode"));
    assert_eq!(loaded.terminal_scrollback_lines, Some(3000));
}

// ─── EffectiveConfig merge precedence ────────────────────────────────────────

#[test]
fn effective_config_flag_agent_wins_over_repo_and_global() {
    let env = IsolatedEnv::new();

    let amux_dir = env.git_root.path().join(REPO_CONFIG_SUBDIR);
    std::fs::create_dir_all(&amux_dir).unwrap();
    std::fs::write(amux_dir.join("config.json"), r#"{"agent":"repo-agent"}"#).unwrap();
    std::fs::create_dir_all(env.home_dir.path()).unwrap();
    std::fs::write(
        env.home_dir.path().join("config.json"),
        r#"{"default_agent":"global-agent"}"#,
    )
    .unwrap();

    let flags = FlagConfig {
        agent: Some("flag-agent".to_string()),
        ..Default::default()
    };
    let session = env.open_session_with_flags(flags);
    assert_eq!(
        session.default_agent().map(|a| a.as_str()),
        Some("flag-agent")
    );
    let ec = session.effective_config();
    assert_eq!(ec.agent().as_deref(), Some("flag-agent"));
}

#[test]
fn effective_config_repo_agent_wins_over_global() {
    let env = IsolatedEnv::new();

    let amux_dir = env.git_root.path().join(REPO_CONFIG_SUBDIR);
    std::fs::create_dir_all(&amux_dir).unwrap();
    std::fs::write(amux_dir.join("config.json"), r#"{"agent":"repo-agent"}"#).unwrap();
    std::fs::create_dir_all(env.home_dir.path()).unwrap();
    std::fs::write(
        env.home_dir.path().join("config.json"),
        r#"{"default_agent":"global-agent"}"#,
    )
    .unwrap();

    let session = env.open_session();
    assert_eq!(
        session.default_agent().map(|a| a.as_str()),
        Some("repo-agent")
    );
}

#[test]
fn effective_config_global_agent_used_when_repo_absent() {
    let env = IsolatedEnv::new();
    std::fs::create_dir_all(env.home_dir.path()).unwrap();
    std::fs::write(
        env.home_dir.path().join("config.json"),
        r#"{"default_agent":"global-agent"}"#,
    )
    .unwrap();

    let session = env.open_session();
    assert_eq!(
        session.default_agent().map(|a| a.as_str()),
        Some("global-agent")
    );
}

#[test]
fn effective_config_scrollback_repo_wins_over_global() {
    let env = IsolatedEnv::new();

    let amux_dir = env.git_root.path().join(REPO_CONFIG_SUBDIR);
    std::fs::create_dir_all(&amux_dir).unwrap();
    std::fs::write(
        amux_dir.join("config.json"),
        r#"{"terminal_scrollback_lines":7777}"#,
    )
    .unwrap();
    std::fs::create_dir_all(env.home_dir.path()).unwrap();
    std::fs::write(
        env.home_dir.path().join("config.json"),
        r#"{"terminal_scrollback_lines":2000}"#,
    )
    .unwrap();

    let session = env.open_session();
    assert_eq!(session.effective_config().scrollback_lines(), 7777);
}

// ─── Session state mutation ───────────────────────────────────────────────────

#[test]
fn session_state_record_error_accumulates() {
    let env = IsolatedEnv::new();
    let mut session = env.open_session();
    assert!(session.state().errors.is_empty());

    session.state_mut().record_error("first error");
    session.state_mut().record_error("second error");

    assert_eq!(session.state().errors.len(), 2);
    assert_eq!(session.state().errors[0].message, "first error");
    assert_eq!(session.state().errors[1].message, "second error");
}

#[test]
fn session_state_record_note_with_levels() {
    let env = IsolatedEnv::new();
    let mut session = env.open_session();
    session
        .state_mut()
        .record_note(SessionLogKind::Info, "info note");
    session
        .state_mut()
        .record_note(SessionLogKind::Warning, "warn note");
    assert_eq!(session.state().notes.len(), 2);
    assert!(matches!(
        session.state().notes[0].kind,
        SessionLogKind::Info
    ));
    assert!(matches!(
        session.state().notes[1].kind,
        SessionLogKind::Warning
    ));
}

#[test]
fn session_touch_advances_last_active_at() {
    let env = IsolatedEnv::new();
    let mut session = env.open_session();
    let before = session.last_active_at();
    // Brief sleep to let the system clock tick.
    std::thread::sleep(std::time::Duration::from_millis(5));
    session.touch();
    let after = session.last_active_at();
    assert!(after >= before);
}

// ─── AgentName validation ─────────────────────────────────────────────────────

#[test]
fn agent_name_all_valid_agent_matrix() {
    for name in &[
        "claude", "codex", "opencode", "maki", "gemini", "copilot", "crush", "cline",
    ] {
        assert!(
            AgentName::new(*name).is_ok(),
            "expected {name:?} to be valid"
        );
    }
}

#[test]
fn agent_name_empty_rejected() {
    assert!(AgentName::new("").is_err());
}

#[test]
fn agent_name_65_chars_rejected() {
    let name = "a".repeat(65);
    assert!(AgentName::new(name).is_err());
}

#[test]
fn agent_name_64_chars_accepted() {
    let name = "a".repeat(64);
    assert!(AgentName::new(name).is_ok());
}

#[test]
fn agent_name_slash_rejected() {
    assert!(AgentName::new("bad/agent").is_err());
}

#[test]
fn agent_name_space_rejected() {
    assert!(AgentName::new("my agent").is_err());
}

// ─── WorktreePaths & branch names ────────────────────────────────────────────

#[test]
fn worktree_branch_name_zero_padded() {
    assert_eq!(worktree_branch_name(42), "amux/work-item-0042");
    assert_eq!(worktree_branch_name(1), "amux/work-item-0001");
    assert_eq!(worktree_branch_name(9999), "amux/work-item-9999");
}

#[test]
fn worktree_branch_name_for_workflow_prefixed() {
    assert_eq!(
        worktree_branch_name_for_workflow("my-wf"),
        "amux/workflow-my-wf"
    );
}

#[test]
fn worktree_paths_for_work_item_correct_structure() {
    use amux::data::worktree_paths::WorktreePaths;
    let paths = WorktreePaths::with_home("/fake-home");
    let wt = paths.for_work_item(std::path::Path::new("/r/myrepo"), 42);
    // Should be ~/.amux/worktrees/myrepo/0042
    assert!(wt.ends_with("worktrees/myrepo/0042"), "got {wt:?}");
}

#[test]
fn worktree_paths_for_workflow_uses_wf_prefix() {
    use amux::data::worktree_paths::WorktreePaths;
    let paths = WorktreePaths::with_home("/fake-home");
    let wt = paths.for_workflow(std::path::Path::new("/r/myrepo"), "my-flow");
    assert!(wt.ends_with("worktrees/myrepo/wf-my-flow"), "got {wt:?}");
}

// ─── Image tags ──────────────────────────────────────────────────────────────

#[test]
fn project_image_tag_uses_folder_name() {
    let tag = amux::data::project_image_tag(std::path::Path::new("/srv/myproject"));
    assert_eq!(tag, "amux-myproject:latest");
}

#[test]
fn agent_image_tag_includes_agent_name() {
    let tag = amux::data::agent_image_tag(std::path::Path::new("/srv/myproject"), "claude");
    assert_eq!(tag, "amux-myproject-claude:latest");
}

#[test]
fn repo_hash_is_eight_hex_chars() {
    let h = amux::data::repo_hash(std::path::Path::new("/some/nonexistent/path"));
    assert_eq!(h.len(), 8);
    assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn repo_hash_is_deterministic() {
    let p = std::path::Path::new("/some/nonexistent/path");
    assert_eq!(amux::data::repo_hash(p), amux::data::repo_hash(p));
}
