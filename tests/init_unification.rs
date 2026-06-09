/// Integration and regression tests for work item 0051 — init unification.
///
/// These tests verify that:
/// 1. The CLI full path (`CliInitQa` + mock launcher) produces the expected files and summary.
/// 2. A TUI-equivalent path (`PresetInitQa` + same mock launcher) produces identical outcomes.
/// 3. The two surfaces cannot diverge: both exercise the same `execute()` function with
///    only their Q&A adapter as the distinguishing variable.
/// 4. Regression guards: `ask_work_items_setup` is offered in both paths, declining it
///    does not panic or leave a missing summary row, and audit is executed inline (not
///    deferred via the old `pending_init_run_audit` / `check_init_continuation` mechanism).
use awman::cli::Agent;
use awman::commands::init_flow::{self, CliInitQa, InitContainerLauncher, InitParams, InitQa};
use awman::commands::output::OutputSink;
use awman::commands::ready::StepStatus;
use awman::config::WorkItemsConfig;
use awman::runtime::AgentRuntime;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::sync::mpsc::unbounded_channel;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn setup_empty_git_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    tmp
}

// ── Mock runtime ──────────────────────────────────────────────────────────────

/// Minimal `AgentRuntime` stub. `execute()` only calls `name()` and `is_available()`
/// directly; all container operations are delegated to the launcher.
struct MockRuntime {
    available: bool,
}

impl AgentRuntime for MockRuntime {
    fn is_available(&self) -> bool {
        self.available
    }
    fn name(&self) -> &'static str {
        "mock"
    }
    fn cli_binary(&self) -> &'static str {
        "mock"
    }
    fn check_socket(&self) -> anyhow::Result<PathBuf> {
        Ok(PathBuf::from("/mock/socket"))
    }
    fn build_image_streaming(
        &self,
        _tag: &str,
        _dockerfile: &Path,
        _context: &Path,
        _no_cache: bool,
        _on_line: &mut dyn FnMut(&str),
    ) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn image_exists(&self, _tag: &str) -> bool {
        false
    }
    fn run_container(
        &self,
        _image: &str,
        _host_path: &str,
        _entrypoint: &[&str],
        _env_vars: &[(String, String)],
        _host_settings: Option<&awman::runtime::HostSettings>,
        _allow_docker: bool,
        _container_name: Option<&str>,
        _ssh_dir: Option<&Path>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn run_container_captured(
        &self,
        _image: &str,
        _host_path: &str,
        _entrypoint: &[&str],
        _env_vars: &[(String, String)],
        _host_settings: Option<&awman::runtime::HostSettings>,
        _allow_docker: bool,
        _container_name: Option<&str>,
        _ssh_dir: Option<&Path>,
    ) -> anyhow::Result<(String, String)> {
        Ok((String::new(), String::new()))
    }
    fn run_container_at_path(
        &self,
        _image: &str,
        _host_path: &str,
        _container_path: &str,
        _working_dir: &str,
        _entrypoint: &[&str],
        _env_vars: &[(String, String)],
        _host_settings: Option<&awman::runtime::HostSettings>,
        _allow_docker: bool,
        _container_name: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn run_container_captured_at_path(
        &self,
        _image: &str,
        _host_path: &str,
        _container_path: &str,
        _working_dir: &str,
        _entrypoint: &[&str],
        _env_vars: &[(String, String)],
        _host_settings: Option<&awman::runtime::HostSettings>,
        _allow_docker: bool,
    ) -> anyhow::Result<(String, String)> {
        Ok((String::new(), String::new()))
    }
    fn run_container_detached(
        &self,
        _image: &str,
        _host_path: &str,
        _container_path: &str,
        _working_dir: &str,
        _container_name: Option<&str>,
        _env_vars: Vec<(String, String)>,
        _allow_docker: bool,
        _host_settings: Option<&awman::runtime::HostSettings>,
    ) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn start_container(&self, _container_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn stop_container(&self, _container_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn remove_container(&self, _container_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn is_container_running(&self, _container_id: &str) -> bool {
        false
    }
    fn find_stopped_container(
        &self,
        _name: &str,
        _image: &str,
    ) -> Option<awman::runtime::StoppedContainerInfo> {
        None
    }
    fn list_running_containers_by_prefix(&self, _prefix: &str) -> Vec<String> {
        vec![]
    }
    fn list_running_containers_with_ids_by_prefix(&self, _prefix: &str) -> Vec<(String, String)> {
        vec![]
    }
    fn get_container_workspace_mount(&self, _container_name: &str) -> Option<String> {
        None
    }
    fn query_container_stats(&self, _name: &str) -> Option<awman::runtime::AgentStats> {
        None
    }
    fn build_run_args_pty(
        &self,
        _image: &str,
        _host_path: &str,
        _entrypoint: &[&str],
        _env_vars: &[(String, String)],
        _host_settings: Option<&awman::runtime::HostSettings>,
        _allow_docker: bool,
        _container_name: Option<&str>,
        _ssh_dir: Option<&Path>,
    ) -> Vec<String> {
        vec![]
    }
    fn build_run_args_pty_display(
        &self,
        _image: &str,
        _host_path: &str,
        _entrypoint: &[&str],
        _env_vars: &[(String, String)],
        _host_settings: Option<&awman::runtime::HostSettings>,
        _allow_docker: bool,
        _container_name: Option<&str>,
        _ssh_dir: Option<&Path>,
    ) -> Vec<String> {
        vec![]
    }
    fn build_run_args_pty_at_path(
        &self,
        _image: &str,
        _host_path: &str,
        _container_path: &str,
        _working_dir: &str,
        _entrypoint: &[&str],
        _env_vars: &[(String, String)],
        _host_settings: Option<&awman::runtime::HostSettings>,
        _allow_docker: bool,
        _container_name: Option<&str>,
    ) -> Vec<String> {
        vec![]
    }
    fn build_exec_args_pty(
        &self,
        _container_id: &str,
        _working_dir: &str,
        _entrypoint: &[&str],
        _env_vars: &[(String, String)],
    ) -> Vec<String> {
        vec![]
    }
    fn build_run_args_display(
        &self,
        _image: &str,
        _host_path: &str,
        _entrypoint: &[&str],
        _env_vars: &[(String, String)],
        _host_settings: Option<&awman::runtime::HostSettings>,
        _allow_docker: bool,
        _container_name: Option<&str>,
        _ssh_dir: Option<&Path>,
    ) -> Vec<String> {
        vec![]
    }
}

// ── Mock container launcher ───────────────────────────────────────────────────

/// `InitContainerLauncher` stub that records calls and returns `Ok(())` without Docker.
struct MockLauncher {
    build_tags: Mutex<Vec<String>>,
    audit_agents: Mutex<Vec<String>>,
}

impl MockLauncher {
    fn new() -> Self {
        Self {
            build_tags: Mutex::new(vec![]),
            audit_agents: Mutex::new(vec![]),
        }
    }
    fn run_audit_call_count(&self) -> usize {
        self.audit_agents.lock().unwrap().len()
    }
}

impl InitContainerLauncher for MockLauncher {
    fn build_image(
        &self,
        tag: &str,
        _dockerfile: &Path,
        _context: &Path,
        _sink: &OutputSink,
    ) -> anyhow::Result<()> {
        self.build_tags.lock().unwrap().push(tag.to_string());
        Ok(())
    }
    fn run_audit(&self, agent: Agent, _cwd: &Path, _sink: &OutputSink) -> anyhow::Result<()> {
        self.audit_agents
            .lock()
            .unwrap()
            .push(agent.as_str().to_string());
        Ok(())
    }
}

// ── TUI-equivalent Q&A adapter ────────────────────────────────────────────────

/// Mirrors `TuiInitQa`: returns pre-collected answers without any I/O or blocking.
/// Used in integration tests to stand in for the private TUI struct.
struct PresetInitQa {
    replace_aspec: bool,
    run_audit: bool,
    work_items: Option<WorkItemsConfig>,
}

impl PresetInitQa {
    fn new(replace_aspec: bool, run_audit: bool, work_items: Option<WorkItemsConfig>) -> Self {
        Self { replace_aspec, run_audit, work_items }
    }
}

impl InitQa for PresetInitQa {
    fn ask_replace_aspec(&mut self) -> anyhow::Result<bool> {
        Ok(self.replace_aspec)
    }
    fn ask_run_audit(&mut self) -> anyhow::Result<bool> {
        Ok(self.run_audit)
    }
    fn ask_work_items_setup(&mut self) -> anyhow::Result<Option<WorkItemsConfig>> {
        Ok(self.work_items.take())
    }
}

// ── Integration: CLI full path ────────────────────────────────────────────────

/// Full CLI init path. `CliInitQa` uses a `Channel` sink which causes every `ask_yes_no`
/// call to return false (empty string → "no"). This exercises the CLI adapter without
/// requiring stdin or the `#[cfg(test)]`-only `mock_input` helper.
#[tokio::test]
async fn cli_init_full_path_writes_config_and_dockerfiles() {
    let tmp = setup_empty_git_repo();
    let root = tmp.path();

    // Channel sink: CliInitQa.ask_run_audit() → false, ask_work_items_setup() → None.
    let (tx, _rx) = unbounded_channel();
    let sink = OutputSink::Channel(tx);
    let runtime = Arc::new(MockRuntime { available: false });
    let mut qa = CliInitQa::new(root, sink.clone());
    let launcher = MockLauncher::new();
    let params = InitParams {
        agent: Agent::Claude,
        aspec: false,
        git_root: root.to_path_buf(),
    };

    let summary = init_flow::execute(params, &mut qa, &launcher, &sink, runtime)
        .await
        .unwrap();

    assert!(
        root.join("Dockerfile.dev").exists(),
        "Dockerfile.dev must be created"
    );
    assert!(
        root.join(".awman").join("Dockerfile.claude").exists(),
        ".awman/Dockerfile.claude must be created"
    );
    assert!(
        root.join(".awman").join("config.json").exists(),
        ".awman/config.json must be created"
    );
    assert!(
        matches!(summary.config, StepStatus::Ok(_)),
        "config stage must be Ok: {:?}",
        summary.config
    );
    assert!(
        matches!(summary.dockerfile, StepStatus::Ok(_)),
        "dockerfile stage must be Ok: {:?}",
        summary.dockerfile
    );
}

#[tokio::test]
async fn cli_init_full_path_no_stage_remains_pending() {
    let tmp = setup_empty_git_repo();
    let root = tmp.path();

    let (tx, _rx) = unbounded_channel();
    let sink = OutputSink::Channel(tx);
    let runtime = Arc::new(MockRuntime { available: false });
    let mut qa = CliInitQa::new(root, sink.clone());
    let launcher = MockLauncher::new();
    let params = InitParams {
        agent: Agent::Codex,
        aspec: false,
        git_root: root.to_path_buf(),
    };

    let summary = init_flow::execute(params, &mut qa, &launcher, &sink, runtime)
        .await
        .unwrap();

    assert_ne!(summary.config, StepStatus::Pending, "config must not be Pending");
    assert_ne!(summary.aspec_folder, StepStatus::Pending, "aspec_folder must not be Pending");
    assert_ne!(summary.dockerfile, StepStatus::Pending, "dockerfile must not be Pending");
    assert_ne!(summary.audit, StepStatus::Pending, "audit must not be Pending");
    assert_ne!(summary.image_build, StepStatus::Pending, "image_build must not be Pending");
    assert_ne!(
        summary.work_items_setup,
        StepStatus::Pending,
        "work_items_setup must not be Pending"
    );
}

/// `CliInitQa` with a Channel sink defaults all prompts to "no" so `ask_work_items_setup`
/// returns `None`. This test verifies the Skipped path and that the config is not corrupted.
#[tokio::test]
async fn cli_init_full_path_work_items_not_configured_leaves_config_unchanged() {
    let tmp = setup_empty_git_repo();
    let root = tmp.path();

    let (tx, _rx) = unbounded_channel();
    let sink = OutputSink::Channel(tx);
    let runtime = Arc::new(MockRuntime { available: false });
    let mut qa = CliInitQa::new(root, sink.clone());
    let launcher = MockLauncher::new();
    let params = InitParams {
        agent: Agent::Claude,
        aspec: false,
        git_root: root.to_path_buf(),
    };

    let summary = init_flow::execute(params, &mut qa, &launcher, &sink, runtime)
        .await
        .unwrap();

    let loaded = awman::config::load_repo_config(root).unwrap();
    assert!(
        loaded.work_items.as_ref().and_then(|w| w.dir.as_ref()).is_none(),
        "work_items.dir must remain absent when the offer was declined"
    );
    assert!(
        matches!(summary.work_items_setup, StepStatus::Skipped(_)),
        "work_items_setup must be Skipped when not configured: {:?}",
        summary.work_items_setup
    );
}

// ── Integration: TUI full path ────────────────────────────────────────────────

/// Full TUI-equivalent path: `PresetInitQa` returns answers immediately (no I/O).
/// File outcomes must be identical to the CLI path.
#[tokio::test]
async fn tui_equiv_init_full_path_writes_config_and_dockerfiles() {
    let tmp = setup_empty_git_repo();
    let root = tmp.path();

    let (tx, _rx) = unbounded_channel();
    let sink = OutputSink::Channel(tx);
    let runtime = Arc::new(MockRuntime { available: false });
    let mut qa = PresetInitQa::new(false, false, None);
    let launcher = MockLauncher::new();
    let params = InitParams {
        agent: Agent::Claude,
        aspec: false,
        git_root: root.to_path_buf(),
    };

    let summary = init_flow::execute(params, &mut qa, &launcher, &sink, runtime)
        .await
        .unwrap();

    assert!(
        root.join("Dockerfile.dev").exists(),
        "Dockerfile.dev must be created by TUI-equivalent path"
    );
    assert!(
        root.join(".awman").join("Dockerfile.claude").exists(),
        ".awman/Dockerfile.claude must be created by TUI-equivalent path"
    );
    assert!(
        root.join(".awman").join("config.json").exists(),
        ".awman/config.json must be created by TUI-equivalent path"
    );
    assert!(
        matches!(summary.config, StepStatus::Ok(_)),
        "config must be Ok: {:?}",
        summary.config
    );
    assert!(
        matches!(summary.dockerfile, StepStatus::Ok(_)),
        "dockerfile must be Ok: {:?}",
        summary.dockerfile
    );
}

/// TUI-equivalent path with work-items accepted and a dir provided.
#[tokio::test]
async fn tui_equiv_init_full_path_work_items_written_to_config() {
    let tmp = setup_empty_git_repo();
    let root = tmp.path();

    let (tx, _rx) = unbounded_channel();
    let sink = OutputSink::Channel(tx);
    let runtime = Arc::new(MockRuntime { available: false });
    let wi = WorkItemsConfig { dir: Some("tasks".into()), template: None };
    let mut qa = PresetInitQa::new(false, false, Some(wi));
    let launcher = MockLauncher::new();
    let params = InitParams {
        agent: Agent::Claude,
        aspec: false,
        git_root: root.to_path_buf(),
    };

    let summary = init_flow::execute(params, &mut qa, &launcher, &sink, runtime)
        .await
        .unwrap();

    let loaded = awman::config::load_repo_config(root).unwrap();
    assert_eq!(
        loaded.work_items.as_ref().and_then(|w| w.dir.as_deref()),
        Some("tasks"),
        "work_items.dir must be persisted after TUI-path acceptance"
    );
    assert!(
        matches!(summary.work_items_setup, StepStatus::Ok(_)),
        "work_items_setup must be Ok: {:?}",
        summary.work_items_setup
    );
}

// ── Integration: CLI and TUI produce identical file outcomes ──────────────────

/// The two init surfaces must write exactly the same files when given equivalent answers.
/// This is the structural guarantee referenced in work item 0051.
#[tokio::test]
async fn cli_and_tui_paths_produce_identical_file_outcomes() {
    let cli_tmp = setup_empty_git_repo();
    let tui_tmp = setup_empty_git_repo();
    let cli_root = cli_tmp.path();
    let tui_root = tui_tmp.path();

    // CLI path — Channel sink defaults all prompts to "no" (audit=false, work_items=None).
    let (cli_tx, _cli_rx) = unbounded_channel();
    let cli_sink = OutputSink::Channel(cli_tx);
    let mut cli_qa = CliInitQa::new(cli_root, cli_sink.clone());
    let cli_launcher = MockLauncher::new();
    let cli_params = InitParams {
        agent: Agent::Claude,
        aspec: false,
        git_root: cli_root.to_path_buf(),
    };
    let cli_summary = init_flow::execute(
        cli_params,
        &mut cli_qa,
        &cli_launcher,
        &cli_sink,
        Arc::new(MockRuntime { available: false }),
    )
    .await
    .unwrap();

    // TUI-equivalent path — PresetInitQa with equivalent answers (audit=false, work_items=None).
    let (tui_tx, _tui_rx) = unbounded_channel();
    let tui_sink = OutputSink::Channel(tui_tx);
    let mut tui_qa = PresetInitQa::new(false, false, None);
    let tui_launcher = MockLauncher::new();
    let tui_params = InitParams {
        agent: Agent::Claude,
        aspec: false,
        git_root: tui_root.to_path_buf(),
    };
    let tui_summary = init_flow::execute(
        tui_params,
        &mut tui_qa,
        &tui_launcher,
        &tui_sink,
        Arc::new(MockRuntime { available: false }),
    )
    .await
    .unwrap();

    // Both must produce the same files.
    for file in &["Dockerfile.dev", ".awman/config.json", ".awman/Dockerfile.claude"] {
        assert!(
            cli_root.join(file).exists(),
            "CLI path must produce {file}"
        );
        assert!(
            tui_root.join(file).exists(),
            "TUI path must produce {file}"
        );
    }

    // Both must have the same stage status discriminants.
    assert_eq!(
        std::mem::discriminant(&cli_summary.config),
        std::mem::discriminant(&tui_summary.config),
        "config status must match: CLI={:?} TUI={:?}",
        cli_summary.config,
        tui_summary.config
    );
    assert_eq!(
        std::mem::discriminant(&cli_summary.dockerfile),
        std::mem::discriminant(&tui_summary.dockerfile),
        "dockerfile status must match: CLI={:?} TUI={:?}",
        cli_summary.dockerfile,
        tui_summary.dockerfile
    );
    assert_eq!(
        std::mem::discriminant(&cli_summary.audit),
        std::mem::discriminant(&tui_summary.audit),
        "audit status must match: CLI={:?} TUI={:?}",
        cli_summary.audit,
        tui_summary.audit
    );
    assert_eq!(
        std::mem::discriminant(&cli_summary.image_build),
        std::mem::discriminant(&tui_summary.image_build),
        "image_build status must match: CLI={:?} TUI={:?}",
        cli_summary.image_build,
        tui_summary.image_build
    );
    assert_eq!(
        std::mem::discriminant(&cli_summary.work_items_setup),
        std::mem::discriminant(&tui_summary.work_items_setup),
        "work_items_setup status must match: CLI={:?} TUI={:?}",
        cli_summary.work_items_setup,
        tui_summary.work_items_setup
    );
}

// ── Regression: work-items offered in both paths ─────────────────────────────

#[tokio::test]
async fn regression_work_items_offered_when_not_configured() {
    // `ask_work_items_setup` must be invoked by `execute()` — was previously CLI-only
    // (gated by the `supports_color()` hack). This test uses a minimal InitQa that
    // tracks the call.
    struct CallTracker {
        work_items_called: bool,
    }
    impl InitQa for CallTracker {
        fn ask_replace_aspec(&mut self) -> anyhow::Result<bool> { Ok(false) }
        fn ask_run_audit(&mut self) -> anyhow::Result<bool> { Ok(false) }
        fn ask_work_items_setup(&mut self) -> anyhow::Result<Option<WorkItemsConfig>> {
            self.work_items_called = true;
            Ok(None)
        }
    }

    let tmp = setup_empty_git_repo();
    let root = tmp.path();
    let (tx, _rx) = unbounded_channel();
    let sink = OutputSink::Channel(tx);
    let runtime = Arc::new(MockRuntime { available: false });
    let mut qa = CallTracker { work_items_called: false };
    let launcher = MockLauncher::new();
    let params = InitParams {
        agent: Agent::Claude,
        aspec: false,
        git_root: root.to_path_buf(),
    };

    let _ = init_flow::execute(params, &mut qa, &launcher, &sink, runtime)
        .await
        .unwrap();

    assert!(
        qa.work_items_called,
        "ask_work_items_setup must be called via execute() — \
         was previously CLI-only (gated by supports_color() hack)"
    );
}

#[tokio::test]
async fn regression_declining_work_items_no_panic_and_complete_summary() {
    let tmp = setup_empty_git_repo();
    let root = tmp.path();
    let (tx, _rx) = unbounded_channel();
    let sink = OutputSink::Channel(tx);
    let runtime = Arc::new(MockRuntime { available: false });
    let mut qa = PresetInitQa::new(false, false, None); // work_items=None → declined
    let launcher = MockLauncher::new();
    let params = InitParams {
        agent: Agent::Claude,
        aspec: false,
        git_root: root.to_path_buf(),
    };

    let summary = init_flow::execute(params, &mut qa, &launcher, &sink, runtime)
        .await
        .unwrap();

    assert_ne!(
        summary.work_items_setup,
        StepStatus::Pending,
        "work_items_setup must not be Pending when user declines"
    );
    assert!(
        matches!(summary.work_items_setup, StepStatus::Skipped(_)),
        "declining work-items must yield Skipped, not {:?}",
        summary.work_items_setup
    );
}

// ── Regression: audit runs inline (pending_init_run_audit removed) ────────────

/// The old TUI path deferred audit via `pending_init_run_audit` + `check_init_continuation()`.
/// The unified path calls `launcher.run_audit()` inside `execute()`.
/// If this count is 0 after `execute()` returns, audit was deferred — regression.
#[tokio::test]
async fn regression_audit_runs_inline_not_deferred() {
    let tmp = setup_empty_git_repo();
    let root = tmp.path();
    std::fs::write(root.join("Dockerfile.dev"), "FROM ubuntu:22.04\n").unwrap();

    let (tx, _rx) = unbounded_channel();
    let sink = OutputSink::Channel(tx);
    let runtime = Arc::new(MockRuntime { available: true });
    let mut qa = PresetInitQa::new(false, true, None); // run_audit=true
    let launcher = MockLauncher::new();
    let params = InitParams {
        agent: Agent::Claude,
        aspec: false,
        git_root: root.to_path_buf(),
    };

    let summary = init_flow::execute(params, &mut qa, &launcher, &sink, runtime)
        .await
        .unwrap();

    assert_eq!(
        launcher.run_audit_call_count(),
        1,
        "run_audit must be called exactly once, inline within execute() — \
         pending_init_run_audit / check_init_continuation deferral path must not exist"
    );
    assert!(
        matches!(summary.audit, StepStatus::Ok(_)),
        "audit must be Ok when launcher succeeds: {:?}",
        summary.audit
    );
}

/// Regression with a different agent: confirms the inline audit path works for all agents.
#[tokio::test]
async fn regression_pending_init_run_audit_field_absent_for_codex_agent() {
    let tmp = setup_empty_git_repo();
    let root = tmp.path();
    std::fs::write(root.join("Dockerfile.dev"), "FROM ubuntu:22.04\n").unwrap();

    let (tx, _rx) = unbounded_channel();
    let sink = OutputSink::Channel(tx);
    let runtime = Arc::new(MockRuntime { available: true });
    let mut qa = PresetInitQa::new(false, true, None);
    let launcher = MockLauncher::new();
    let params = InitParams {
        agent: Agent::Codex,
        aspec: false,
        git_root: root.to_path_buf(),
    };

    let _ = init_flow::execute(params, &mut qa, &launcher, &sink, runtime)
        .await
        .unwrap();

    assert!(
        launcher.run_audit_call_count() > 0,
        "audit must execute inline for Agent::Codex — \
         pending_init_run_audit deferral must not exist"
    );
}
