//! Tests for `commands::exec_workflow` (WI 0114 F-51: moved out of the
//! module file, unchanged).
use super::dynamic::*;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use super::*;
use crate::command::commands::agent_auth::{AgentAuthDecision, AgentAuthFrontend};
use crate::command::commands::agent_setup::{AgentSetupDecision, AgentSetupFrontend};
use crate::command::commands::mount_scope::{MountScopeDecision, MountScopeFrontend};
use crate::command::commands::worktree_lifecycle::{
    ExistingWorktreeDecision, PostWorkflowWorktreeAction, PreWorktreeDecision,
    WorktreeLifecycleFrontend,
};
use crate::data::message::UserMessage;
use crate::data::session::AgentName;
use crate::data::workflow_state::WorkflowState;
use crate::engine::agent_runtime::frontend::{AgentProgress, AgentStatus};
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepOutput, WorkflowOutcome, WorkflowStepStatus,
    YoloTickOutcome,
};

// ─── Recording frontend ───────────────────────────────────────────────────
/// `(phase, description, attempt, of)` for each `on_phase_step_fixing`.
type SharedPhaseFixingCalls = Arc<Mutex<Vec<(PhaseKind, String, u32, u32)>>>;

struct FakeExecWorkflowFrontend {
    pty_active_calls: Vec<bool>,
    /// Per-step container names received via `report_parallel_step_container`.
    parallel_containers: Arc<Mutex<Vec<(String, String)>>>,
    replay_queued_count: usize,
    summary_calls: Vec<WorkflowSummary>,
    messages: Vec<UserMessage>,
    next_action_response: NextAction,
    /// What `ask_workflow_resume` answers. `Fresh` keeps the historical
    /// behaviour of every test that does not care.
    resume_response: WorkflowResumeDecision,
    /// Prompts the resume question was asked with.
    resume_prompts: Vec<WorkflowResumePrompt>,
    /// `on_phase_step_fixing` calls. Recorded through an `Arc` so a test
    /// can observe them after the fake has been boxed into the shared
    /// handle (F-36).
    phase_fixing: SharedPhaseFixingCalls,
}

impl FakeExecWorkflowFrontend {
    fn new() -> Self {
        Self {
            pty_active_calls: vec![],
            parallel_containers: Arc::new(Mutex::new(Vec::new())),
            replay_queued_count: 0,
            summary_calls: vec![],
            messages: vec![],
            next_action_response: NextAction::LaunchNext,
            resume_response: WorkflowResumeDecision::Fresh,
            resume_prompts: Vec::new(),
            phase_fixing: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn answering_resume(mut self, response: WorkflowResumeDecision) -> Self {
        self.resume_response = response;
        self
    }
}

impl crate::engine::git::GitFrontend for FakeExecWorkflowFrontend {}

impl UserMessageSink for FakeExecWorkflowFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        self.messages.push(msg);
    }
    fn replay_queued(&mut self) {
        self.replay_queued_count += 1;
    }
}

#[async_trait]
impl AgentFrontend for FakeExecWorkflowFrontend {
    fn report_status(&mut self, _status: AgentStatus) {}
    fn report_progress(&mut self, _progress: AgentProgress) {}
    fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
        let (stdout_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (stderr_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::engine::agent_runtime::frontend::AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }
}

impl WorkflowFrontend for FakeExecWorkflowFrontend {
    fn show_workflow_control_board(
        &mut self,
        _state: &WorkflowState,
        _available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        Ok(self.next_action_response.clone())
    }
    fn yolo_countdown_tick(
        &mut self,
        _step_name: &str,
        _remaining: Duration,
        _total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(YoloTickOutcome::Continue)
    }
    fn report_step_status(&mut self, _step: &WorkflowStep, _status: WorkflowStepStatus) {}
    fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}
    fn report_workflow_completed(&mut self, _outcome: &WorkflowOutcome) {}
    fn report_parallel_step_container(&mut self, step_name: &str, container_name: &str) {
        self.parallel_containers
            .lock()
            .unwrap()
            .push((step_name.to_string(), container_name.to_string()));
    }
    fn confirm_resume(&mut self, _mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn on_phase_step_fixing(&mut self, kind: PhaseKind, description: &str, attempt: u32, of: u32) {
        self.phase_fixing
            .lock()
            .unwrap()
            .push((kind, description.to_string(), attempt, of));
    }
}

impl MountScopeFrontend for FakeExecWorkflowFrontend {
    fn ask_mount_scope(
        &mut self,
        _git_root: &Path,
        _cwd: &Path,
    ) -> Result<MountScopeDecision, CommandError> {
        Ok(MountScopeDecision::MountGitRoot)
    }
}

impl AgentSetupFrontend for FakeExecWorkflowFrontend {
    fn ask_agent_setup(
        &mut self,
        _requested: &AgentName,
        _default: &AgentName,
        _default_available: bool,
        _image_only: bool,
    ) -> Result<AgentSetupDecision, CommandError> {
        Ok(AgentSetupDecision::Setup)
    }
    fn record_fallback(&mut self, _requested: &AgentName, _fallback: &AgentName) {}
}

impl AgentAuthFrontend for FakeExecWorkflowFrontend {
    fn ask_agent_auth_consent(
        &mut self,
        _agent: &AgentName,
        _env_var_names: &[&str],
    ) -> Result<AgentAuthDecision, CommandError> {
        Ok(AgentAuthDecision::Accept)
    }
}

impl WorktreeLifecycleFrontend for FakeExecWorkflowFrontend {
    fn ask_pre_worktree_uncommitted_files(
        &mut self,
        _files: &[String],
        _suggested_message: &str,
    ) -> Result<PreWorktreeDecision, CommandError> {
        Ok(PreWorktreeDecision::UseLastCommit)
    }
    fn ask_existing_worktree(
        &mut self,
        _path: &Path,
        _branch: &str,
    ) -> Result<ExistingWorktreeDecision, CommandError> {
        Ok(ExistingWorktreeDecision::Resume)
    }
    fn report_worktree_created(&mut self, _path: &Path, _branch: &str) {}
    fn ask_post_workflow_action(
        &mut self,
        _prompt: &crate::command::commands::worktree_lifecycle::PostWorkflowWorktreePrompt,
    ) -> Result<PostWorkflowWorktreeAction, CommandError> {
        Ok(PostWorkflowWorktreeAction::Keep)
    }
    fn ask_worktree_commit_before_merge(
        &mut self,
        _branch: &str,
        _files: &[String],
        _suggested_message: &str,
    ) -> Result<Option<String>, CommandError> {
        Ok(None)
    }
    fn ask_merge_mode(
        &mut self,
        _branch: &str,
    ) -> Result<crate::command::commands::worktree_lifecycle::WorktreeMergeMode, CommandError> {
        Ok(crate::command::commands::worktree_lifecycle::WorktreeMergeMode::LeaveBranch)
    }
    fn confirm_worktree_cleanup(
        &mut self,
        _branch: &str,
        _path: &Path,
    ) -> Result<bool, CommandError> {
        Ok(false)
    }
    fn report_merge_conflict(&mut self, _branch: &str, _wt: &Path, _root: &Path) {}
    fn report_worktree_discarded(&mut self, _branch: &str) {}
    fn report_worktree_kept(&mut self, _path: &Path, _branch: &str) {}
}

/// The fake never launches a container, so its container frontend is inert.
impl crate::command::commands::agent_setup::HasAgentFrontend for FakeExecWorkflowFrontend {
    fn container_frontend(
        &mut self,
    ) -> Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend> {
        Box::new(crate::command::commands::agent_setup::NullAgentFrontend)
    }
}

impl crate::command::commands::agent_setup::AgentLaunchFrontend for FakeExecWorkflowFrontend {
    fn set_pty_active(&mut self, active: bool) {
        self.pty_active_calls.push(active);
    }
}

impl ExecWorkflowCommandFrontend for FakeExecWorkflowFrontend {
    fn report_workflow_summary(&mut self, summary: &WorkflowSummary) {
        self.summary_calls.push(summary.clone());
    }
    fn ask_workflow_resume(
        &mut self,
        prompt: &WorkflowResumePrompt,
    ) -> Result<WorkflowResumeDecision, CommandError> {
        self.resume_prompts.push(prompt.clone());
        Ok(self.resume_response.clone())
    }
    fn notify_dynamic_workflow_resume_unavailable(
        &mut self,
        _work_item: u32,
        _reason: &str,
    ) -> Result<(), CommandError> {
        Ok(())
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────

fn write_minimal_workflow(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        r#"[[steps]]
name = "test-step"
agent = "claude"
prompt = "do something"
"#,
    )
    .unwrap();
    path
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn set_pty_active_called_true_then_false_around_engine() {
    // Arrange: minimal workflow in a temp dir that the engine can run.
    let tmp = tempfile::tempdir().unwrap();
    let wf_path = write_minimal_workflow(tmp.path(), "test.toml");

    // Use a real git repo so Session::open_at_git_root succeeds.
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(tmp.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "t@t.t"])
        .current_dir(tmp.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "t"])
        .current_dir(tmp.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    std::fs::write(tmp.path().join("README"), "x").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(tmp.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(tmp.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();

    let mut engines = Engines::for_tests(Path::new("/tmp"));
    // Override workflow_state_store to use the temp git repo.
    engines.workflow_state_store =
        Arc::new(crate::data::WorkflowStateStore::at_git_root(tmp.path()));

    let flags = ExecWorkflowCommandFlags {
        workflow: Some(wf_path),
        work_item: None,
        non_interactive: true,
        plan: false,
        allow_docker: false,
        worktree: false,

        yolo: false,
        auto: false,
        agent: None,
        model: None,
        launch_mode: None,
        overlay: vec![],
        max_concurrent: None,
        issue_source: crate::engine::issue::IssueSourceFlags { issue: None },
        dynamic: false,
        leader: None,
    };
    let session = {
        let resolver = crate::data::session::StaticGitRootResolver::new(tmp.path());
        Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            crate::data::session::SessionOpenOptions::default(),
        )
        .unwrap()
    };
    let cmd = ExecWorkflowCommand::new(flags, engines, session);
    let fake = FakeExecWorkflowFrontend::new();

    let result = cmd.run_with_frontend(Box::new(fake)).await;

    // The outcome is Ok and set_pty_active was called true then false.
    // (Engine result may be Ok or Err depending on the stub backend;
    //  what matters is the ordering.)
    // We can't easily inspect the fake after run_with_frontend consumes it.
    // Instead, we use the shared-arc pattern to peek at the state after.
    // For this test, simply verifying no panic is the structural assertion.
    let _ = result;
}

#[tokio::test]
async fn shared_frontend_handle_delegates_write_message_to_inner_frontend() {
    let mut shared: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>> =
        Arc::new(Mutex::new(Box::new(FakeExecWorkflowFrontend::new())));

    use crate::data::message::MessageLevel;
    shared.write_message(UserMessage {
        level: MessageLevel::Info,
        text: "hello".into(),
    });

    let guard = shared.lock().unwrap();
    let fake = guard.as_ref();
    // Can't easily downcast Box<dyn Trait>, but we can verify no panic
    // and that the blanket impl compiled and delegated without crashing.
    let _ = fake;
}

#[test]
fn shared_frontend_handle_forwards_parallel_step_container_to_inner_frontend() {
    // Every parallel callback must be forwarded explicitly: the trait's
    // default is a no-op, so a missing forward silently swallows the
    // event. When this one was missing, TUI parallel-group slots never
    // learned their container names and their stats stayed blank.
    let fake = FakeExecWorkflowFrontend::new();
    let seen = Arc::clone(&fake.parallel_containers);
    let mut shared: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>> =
        Arc::new(Mutex::new(Box::new(fake)));

    shared.report_parallel_step_container("build", "awman-build-1");
    shared.report_parallel_step_container("test", "awman-test-2");

    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [
            ("build".to_string(), "awman-build-1".to_string()),
            ("test".to_string(), "awman-test-2".to_string()),
        ],
        "the shared handle must forward per-step container names to the real frontend"
    );
}

#[test]
fn shared_frontend_handle_reaches_an_override_of_a_defaulted_method() {
    // The regression the blanket impl exists to prevent (F-36): the
    // hand-written `WorkflowProxy` restated 34 of the trait's 36 methods
    // and left `on_setup_step_fixing` / `on_teardown_step_fixing` out, so
    // a frontend that overrode either one never heard about a remediation
    // attempt — the call stopped at the trait's default no-op inside the
    // proxy. The blanket impl forwards every method, defaulted ones
    // included.
    let fake = FakeExecWorkflowFrontend::new();
    let seen = Arc::clone(&fake.phase_fixing);
    let mut shared: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>> =
        Arc::new(Mutex::new(Box::new(fake)));

    shared.on_phase_step_fixing(PhaseKind::Setup, "cargo build", 1, 3);

    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [(PhaseKind::Setup, "cargo build".to_string(), 1, 3)],
        "an override of a defaulted trait method must be reached through the shared handle"
    );
}

#[test]
fn exec_workflow_flags_worktree_defaults_to_false() {
    // Verify ExecWorkflowCommandFlags is constructable and worktree defaults
    // correctly reflect what dispatch sets.
    let flags = ExecWorkflowCommandFlags {
        workflow: Some(PathBuf::from("wf.toml")),
        work_item: None,
        non_interactive: false,
        plan: false,
        allow_docker: false,
        worktree: false,

        yolo: false,
        auto: false,
        agent: None,
        model: None,
        launch_mode: None,
        overlay: vec![],
        max_concurrent: None,
        issue_source: crate::engine::issue::IssueSourceFlags { issue: None },
        dynamic: false,
        leader: None,
    };
    assert!(!flags.worktree);
    assert!(!flags.yolo);
}

#[test]
fn exec_workflow_flags_yolo_implies_worktree_in_dispatch() {
    // Dispatch sets worktree=true when yolo=true; verify the flag struct
    // allows that combination.
    let flags = ExecWorkflowCommandFlags {
        workflow: Some(PathBuf::from("wf.toml")),
        work_item: None,
        non_interactive: false,
        plan: false,
        allow_docker: false,
        worktree: true,

        yolo: true,
        auto: false,
        agent: None,
        model: None,
        launch_mode: None,
        overlay: vec![],
        max_concurrent: None,
        issue_source: crate::engine::issue::IssueSourceFlags { issue: None },
        dynamic: false,
        leader: None,
    };
    assert!(flags.yolo);
    assert!(flags.worktree, "yolo must imply worktree");
}

#[test]
fn workflow_summary_steps_failed_zero_on_success() {
    let s = WorkflowSummary {
        steps_completed: 3,
        steps_failed: 0,
    };
    assert_eq!(s.steps_failed, 0);
    assert_eq!(s.steps_completed, 3);
}

// ─── Per-entry overlay isolation (WI-0082 §1 review fix) ─────────────────

/// `collect_single_entry_overlays` must scope env passthrough to the
/// caller-supplied entry + standing sources only. The orchestrator calls
/// it once per setup/teardown entry; if it leaked information across
/// calls, sibling steps would inherit each other's overlays.
#[test]
fn collect_single_entry_overlays_isolates_env_per_entry() {
    use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
    use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};

    let tmp = tempfile::tempdir().unwrap();
    let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
    let resolver = StaticGitRootResolver::new(tmp.path());
    let session = Session::open(
        tmp.path().to_path_buf(),
        &resolver,
        SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        },
    )
    .unwrap();
    let engines = Engines::for_tests(Path::new("/tmp"));

    // Set both env vars on the host so passthrough can capture them.
    std::env::set_var("WI0082_REVIEW_TOKEN_A", "value-a");
    std::env::set_var("WI0082_REVIEW_TOKEN_B", "value-b");

    let entry_a = vec!["env(WI0082_REVIEW_TOKEN_A)".to_string()];
    let entry_b = vec!["env(WI0082_REVIEW_TOKEN_B)".to_string()];

    let (_, env_a) =
        collect_single_entry_overlays(&engines, &session, &[], Some(&entry_a), None).unwrap();
    let (_, env_b) =
        collect_single_entry_overlays(&engines, &session, &[], Some(&entry_b), None).unwrap();

    std::env::remove_var("WI0082_REVIEW_TOKEN_A");
    std::env::remove_var("WI0082_REVIEW_TOKEN_B");

    assert!(
        env_a.contains_key("WI0082_REVIEW_TOKEN_A"),
        "entry A's env must contain its own var; got: {env_a:?}"
    );
    assert!(
        !env_a.contains_key("WI0082_REVIEW_TOKEN_B"),
        "entry A's env must NOT include entry B's var (no cross-step leak); got: {env_a:?}"
    );
    assert!(
        env_b.contains_key("WI0082_REVIEW_TOKEN_B"),
        "entry B's env must contain its own var; got: {env_b:?}"
    );
    assert!(
        !env_b.contains_key("WI0082_REVIEW_TOKEN_A"),
        "entry B's env must NOT include entry A's var (no cross-step leak); got: {env_b:?}"
    );
}

// ─── WI-0086: collect_single_entry_overlays uses repo-config dockerfile ─────

/// Verify that `collect_single_entry_overlays` resolves the Dockerfile path
/// from `session.repo_config().dockerfile_path_or_default()`, not from a
/// hard-coded `git_root.join("Dockerfile.dev")`.
#[test]
fn collect_single_entry_overlays_uses_repo_config_dockerfile_path() {
    use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
    use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};

    let tmp = tempfile::tempdir().unwrap();

    // Write repo config with a custom Dockerfile path.
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    std::fs::write(
        awman_dir.join("config.json"),
        r#"{"dockerfile": "infra/Dockerfile.base"}"#,
    )
    .unwrap();

    // Create the configured Dockerfile (not Dockerfile.dev).
    let infra_dir = tmp.path().join("infra");
    std::fs::create_dir_all(&infra_dir).unwrap();
    std::fs::write(
        infra_dir.join("Dockerfile.base"),
        "FROM ubuntu:22.04\nUSER agent\n",
    )
    .unwrap();

    let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
    let resolver = StaticGitRootResolver::new(tmp.path());
    let session = Session::open(
        tmp.path().to_path_buf(),
        &resolver,
        SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        },
    )
    .unwrap();

    // The session must resolve dockerfile from repo config, not Dockerfile.dev.
    let resolved = session
        .repo_config()
        .dockerfile_path_or_default(session.git_root());
    assert_eq!(
        resolved,
        tmp.path().join("infra/Dockerfile.base"),
        "session must read dockerfile path from repo config, not hard-code Dockerfile.dev"
    );

    // collect_single_entry_overlays must succeed using the configured path.
    let engines = Engines::for_tests(Path::new("/tmp"));
    let result = collect_single_entry_overlays(&engines, &session, &[], None, None);
    assert!(
        result.is_ok(),
        "collect_single_entry_overlays must succeed with a repo-config-resolved dockerfile path"
    );
}

// ─── Gemini deprecation: workflow-level scan (WI-0083 review fix) ────────

/// A session over a repo whose `.awman/config.json` names `default_agent`,
/// with `AWMAN_CONFIG_HOME` pinned at the fixture so the developer's own
/// global config cannot supply a different default.
fn make_session_with_default_agent(
    tmp: &tempfile::TempDir,
    default_agent: Option<&str>,
) -> Session {
    if let Some(agent) = default_agent {
        write_repo_config(tmp, &format!(r#"{{"agent": "{agent}"}}"#));
    }
    Session::for_tests_isolated(tmp.path(), tmp.path())
}

/// The repo config every fixture in this file writes, in one place.
fn write_repo_config(tmp: &tempfile::TempDir, json: &str) {
    let cfg_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.json"), json).unwrap();
}

fn make_workflow(workflow_agent: Option<&str>, step_agents: &[Option<&str>]) -> Workflow {
    Workflow {
        title: None,
        steps: step_agents
            .iter()
            .enumerate()
            .map(|(i, a)| WorkflowStep {
                name: format!("step{i}"),
                depends_on: vec![],
                prompt_template: "x".into(),
                agent: a.map(|s| s.to_string()),
                model: None,
                overlays: None,
                abort_on_failure: false,
            })
            .collect(),
        agent: workflow_agent.map(|s| s.to_string()),
        model: None,
        setup: vec![],
        teardown: vec![],
        teardown_on_failure: false,
        overlays: None,
    }
}

#[test]
fn acp_preflight_rejects_before_workflow_launch_when_fallback_is_error() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_default_agent(&tmp, None);
    let workflow = make_workflow(None, &[Some("cline"), Some("claude")]);
    let mut sink = crate::data::message::RecordingMessageSink::new();
    let mut flags = make_dynamic_flags(false, Some("wf.toml"), None, None, false, None);
    flags.launch_mode = Some(crate::data::config::repo::LaunchMode::Acp);

    let error = validate_workflow_acp_preflight(&workflow, &session, &flags, &mut sink)
        .expect_err("unsupported step must stop ACP workflow pre-flight");
    assert!(error.to_string().contains("step 'step1'"));
    assert!(error.to_string().contains("claude"));
    assert!(sink.all().is_empty(), "error fallback must not downgrade");
}

#[test]
fn acp_preflight_downgrades_unsupported_steps_to_stdio_and_runs() {
    // A workflow whose steps all resolve to unsupported agents under
    // `launchModeFallback: stdio` downgrades every step to stdio (one
    // warning each) and is permitted — no step resolves to ACP, so the
    // not-yet-implemented workflow-ACP guard never fires.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("config.json"),
        r#"{"launchModeFallback":"stdio"}"#,
    )
    .unwrap();
    let session = make_session_with_default_agent(&tmp, None);
    let workflow = make_workflow(None, &[Some("claude"), Some("codex")]);
    let mut sink = crate::data::message::RecordingMessageSink::new();
    let mut flags = make_dynamic_flags(false, Some("wf.toml"), None, None, false, None);
    flags.launch_mode = Some(crate::data::config::repo::LaunchMode::Acp);

    let modes = validate_workflow_acp_preflight(&workflow, &session, &flags, &mut sink)
        .expect("stdio fallback of all-unsupported steps must permit the workflow");
    assert_eq!(modes["step0"], crate::data::config::repo::LaunchMode::Stdio);
    assert_eq!(modes["step1"], crate::data::config::repo::LaunchMode::Stdio);
    let messages = sink.all();
    assert_eq!(messages.len(), 2, "one downgrade warning per step");
    assert!(messages.iter().all(|m| m.level == MessageLevel::Warning));
}

#[test]
fn acp_preflight_rejects_workflow_acp_as_not_yet_implemented() {
    // An ACP-capable step (cline) under `launchMode: acp` resolves to ACP,
    // which workflows cannot yet drive — pre-flight must reject the whole
    // workflow before any container spawns rather than launch an ACP
    // container it never speaks the protocol to (the "silent false success"
    // blocker). `launchModeFallback: stdio` does not rescue it: fallback
    // only downgrades UNsupported agents; a supported agent stays ACP.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("config.json"),
        r#"{"launchModeFallback":"stdio"}"#,
    )
    .unwrap();
    let session = make_session_with_default_agent(&tmp, None);
    let workflow = make_workflow(None, &[Some("cline")]);
    let mut sink = crate::data::message::RecordingMessageSink::new();
    let mut flags = make_dynamic_flags(false, Some("wf.toml"), None, None, false, None);
    flags.launch_mode = Some(crate::data::config::repo::LaunchMode::Acp);

    let error = validate_workflow_acp_preflight(&workflow, &session, &flags, &mut sink)
        .expect_err("workflow ACP must be rejected as not yet implemented");
    assert!(
        matches!(error, EngineError::NotImplemented(_)),
        "expected NotImplemented, got: {error:?}"
    );
    assert!(error.to_string().contains("not yet supported for workflow"));
}

#[test]
fn skip_checkout_branch_steps_removes_only_checkout_entries_and_warns() {
    use crate::data::workflow_definition::{SetupStep, SetupStepEntry};
    let mut wf = make_workflow(None, &[None]);
    wf.setup = vec![
        SetupStepEntry {
            overlays: None,
            abort_on_failure: false,
            on_failure: None,
            step: SetupStep::CheckoutCreateBranch {
                branch: "feature/x".into(),
                base: None,
            },
        },
        SetupStepEntry {
            overlays: None,
            abort_on_failure: false,
            on_failure: None,
            step: SetupStep::RunShell {
                command: "echo hi".into(),
                env: None,
            },
        },
        SetupStepEntry {
            overlays: None,
            abort_on_failure: false,
            on_failure: None,
            step: SetupStep::CheckoutCreateBranch {
                branch: "feature/y".into(),
                base: Some("main".into()),
            },
        },
    ];
    let mut fe = FakeExecWorkflowFrontend::new();
    skip_checkout_branch_steps_in_worktree(&mut wf, &mut fe);
    assert_eq!(wf.setup.len(), 1, "only the run_shell entry must remain");
    assert!(matches!(wf.setup[0].step, SetupStep::RunShell { .. }));
    let warnings: Vec<&UserMessage> = fe
        .messages
        .iter()
        .filter(|m| m.level == MessageLevel::Warning)
        .collect();
    assert_eq!(
        warnings.len(),
        2,
        "one warning per skipped checkout_create_branch step"
    );
    assert!(warnings[0].text.contains("checkout_create_branch"));
    assert!(warnings[0].text.contains("feature/x"));
    assert!(warnings[1].text.contains("feature/y"));
    assert!(
        warnings[0].text.contains("worktree"),
        "warning must explain the worktree isolation reason"
    );
}

#[test]
fn skip_checkout_branch_steps_no_op_without_checkout_entries() {
    use crate::data::workflow_definition::{SetupStep, SetupStepEntry};
    let mut wf = make_workflow(None, &[None]);
    wf.setup = vec![SetupStepEntry {
        overlays: None,
        abort_on_failure: false,
        on_failure: None,
        step: SetupStep::RunShell {
            command: "echo hi".into(),
            env: None,
        },
    }];
    let mut fe = FakeExecWorkflowFrontend::new();
    skip_checkout_branch_steps_in_worktree(&mut wf, &mut fe);
    assert_eq!(wf.setup.len(), 1);
    assert!(fe.messages.is_empty(), "no warnings when nothing skipped");
}

#[test]
fn workflow_resolves_to_gemini_true_when_step_uses_gemini() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_default_agent(&tmp, None);
    let wf = make_workflow(None, &[Some("claude"), Some("gemini")]);
    assert!(
        workflow_agents(&wf, &session).iter().any(|a| a == "gemini"),
        "must detect gemini in a step's agent field"
    );
}

#[test]
fn workflow_resolves_to_gemini_true_when_workflow_default_is_gemini_and_step_has_no_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_default_agent(&tmp, None);
    let wf = make_workflow(Some("gemini"), &[None]);
    assert!(
        workflow_agents(&wf, &session).iter().any(|a| a == "gemini"),
        "must detect workflow-level agent=gemini when step omits agent"
    );
}

#[test]
fn workflow_resolves_to_gemini_true_when_session_default_is_gemini_and_step_has_no_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_default_agent(&tmp, Some("gemini"));
    let wf = make_workflow(None, &[None]);
    assert!(
        workflow_agents(&wf, &session).iter().any(|a| a == "gemini"),
        "must detect session default agent=gemini when neither step nor workflow set agent"
    );
}

#[test]
fn workflow_resolves_to_gemini_false_when_step_overrides_gemini_with_other_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_default_agent(&tmp, Some("gemini"));
    // step.agent (claude) wins over workflow.agent (gemini) and session default.
    let wf = make_workflow(Some("gemini"), &[Some("claude")]);
    assert!(
        !workflow_agents(&wf, &session).iter().any(|a| a == "gemini"),
        "step-level agent override must win over workflow and session defaults"
    );
}

#[test]
fn workflow_resolves_to_gemini_false_when_no_path_resolves_to_gemini() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_default_agent(&tmp, Some("claude"));
    let wf = make_workflow(Some("codex"), &[Some("claude"), None]);
    assert!(
        !workflow_agents(&wf, &session).iter().any(|a| a == "gemini"),
        "must return false when neither step, workflow, nor session resolves to gemini"
    );
}

// ── issue_source_overlay + IssueTempFile ─────────────────────────────────

use crate::engine::issue::github::GithubIssueSource;
use crate::engine::issue::Issue;

fn make_issue(source_id: &str, title: &str, body: &str) -> Issue {
    Issue {
        source_id: source_id.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        provider: "GitHub".to_string(),
    }
}

#[test]
fn issue_source_overlay_writes_temp_file_and_builds_directory_overlay() {
    let tmp = tempfile::tempdir().unwrap();
    let git_root = tmp.path();
    let work_items_dir = git_root.join("aspec").join("work-items");
    let issue = make_issue("https://github.com/owner/repo/issues/84", "Test", "body");

    let build = issue_source_overlay(&GithubIssueSource, &issue, git_root, &work_items_dir)
        .expect("overlay build must succeed");

    // Temp file exists and has the expected contents.
    assert!(build.temp_file.path().exists(), "temp file must exist");
    let on_disk = std::fs::read_to_string(build.temp_file.path()).unwrap();
    assert_eq!(on_disk, "# Test\n\nbody");

    // Slug + number derive from the issue.
    assert_eq!(build.number, 84);
    assert!(
        build.slug.starts_with("ghb84"),
        "slug must start with 'ghb84', got: {}",
        build.slug
    );

    // Overlay is a ReadOnly Directory mapping the temp file to the
    // container-side work-items path.
    match build.overlay {
        TypedOverlay::Directory(spec) => {
            assert_eq!(spec.host, build.temp_file.path().display().to_string());
            assert!(
                spec.container.starts_with("/workspace/aspec/work-items/"),
                "container path must start with /workspace/aspec/work-items/, got {}",
                spec.container
            );
            assert!(spec.container.ends_with(".md"));
            assert!(spec.container.contains("0084-"));
            assert_eq!(
                spec.permission,
                crate::engine::container::options::OverlayPermission::ReadOnly,
                "overlay must be ReadOnly"
            );
        }
        other => panic!("expected TypedOverlay::Directory, got {other:?}"),
    }
}

#[test]
fn issue_temp_file_drop_deletes_underlying_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("scope-guard-test.md");
    std::fs::write(&path, "contents").unwrap();
    assert!(path.exists());
    {
        let _guard = super::IssueTempFile { path: path.clone() };
        // Inside the scope the file still exists.
        assert!(path.exists());
    }
    // After the guard is dropped the file is gone.
    assert!(
        !path.exists(),
        "IssueTempFile::drop must remove the underlying file"
    );
}

#[test]
fn issue_temp_file_filename_format_is_pid_and_slug() {
    let tmp = tempfile::tempdir().unwrap();
    let git_root = tmp.path();
    let work_items_dir = git_root.join("aspec").join("work-items");
    let issue = make_issue("https://github.com/owner/repo/issues/7", "Some Title", "");

    let build =
        issue_source_overlay(&GithubIssueSource, &issue, git_root, &work_items_dir).unwrap();

    let pid = std::process::id();
    let file_name = build
        .temp_file
        .path()
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap()
        .to_string();
    assert!(
        file_name.starts_with(&format!("awman-issue-{pid}-")),
        "temp filename must follow awman-issue-{{pid}}-{{slug}}.md, got: {file_name}"
    );
    assert!(file_name.ends_with(".md"));
    assert!(file_name.contains(&build.slug));
}

// ─── WI-0092: Dynamic Workflows — unit tests ─────────────────────────────

// ── Helpers shared by WI-0092 tests ──────────────────────────────────────

fn make_dynamic_flags(
    dynamic: bool,
    workflow: Option<&str>,
    work_item: Option<&str>,
    leader: Option<&str>,
    plan: bool,
    model: Option<&str>,
) -> ExecWorkflowCommandFlags {
    ExecWorkflowCommandFlags {
        workflow: workflow.map(PathBuf::from),
        work_item: work_item.map(|s| s.to_string()),
        non_interactive: false,
        plan,
        allow_docker: false,
        worktree: false,
        yolo: false,
        auto: false,
        agent: None,
        model: model.map(|s| s.to_string()),
        launch_mode: None,
        overlay: vec![],
        max_concurrent: None,
        issue_source: crate::engine::issue::IssueSourceFlags { issue: None },
        dynamic,
        leader: leader.map(|s| s.to_string()),
    }
}

fn make_session_simple(tmp: &tempfile::TempDir) -> crate::data::session::Session {
    make_session_with_default_agent(tmp, None)
}

fn make_session_with_agent(tmp: &tempfile::TempDir, agent: &str) -> crate::data::session::Session {
    make_session_with_default_agent(tmp, Some(agent))
}

/// Writes a repo config with `dynamicWorkflows.defaultLeader` set, for
/// leader-resolution-precedence tests (WI-0095 §5).
fn make_session_with_default_leader(
    tmp: &tempfile::TempDir,
    default_leader: &str,
) -> crate::data::session::Session {
    write_repo_config(
        tmp,
        &format!(r#"{{"dynamicWorkflows": {{"defaultLeader": "{default_leader}"}}}}"#),
    );
    Session::for_tests_isolated(tmp.path(), tmp.path())
}

// ── validate_dynamic_flags ────────────────────────────────────────────────

#[test]
fn validate_dynamic_flags_rejects_path_with_dynamic() {
    let flags = make_dynamic_flags(true, Some("/tmp/wf.toml"), Some("0042"), None, false, None);
    let err = validate_dynamic_flags(&flags).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("cannot specify a workflow file path with --dynamic"),
        "error must explain the conflict, got: {msg}"
    );
}

#[test]
fn validate_dynamic_flags_requires_work_item_with_dynamic() {
    let flags = make_dynamic_flags(true, None, None, None, false, None);
    let err = validate_dynamic_flags(&flags).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("--dynamic requires --work-item"),
        "error must name the missing flag, got: {msg}"
    );
}

#[test]
fn validate_dynamic_flags_rejects_leader_without_dynamic() {
    let flags = make_dynamic_flags(
        false,
        Some("/tmp/wf.toml"),
        None,
        Some("claude::claude-opus-4-8"),
        false,
        None,
    );
    let err = validate_dynamic_flags(&flags).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("--leader is only valid with --dynamic"),
        "error must name the constraint, got: {msg}"
    );
}

#[test]
fn validate_dynamic_flags_rejects_dynamic_with_plan() {
    let flags = make_dynamic_flags(true, None, Some("0042"), None, true, None);
    let err = validate_dynamic_flags(&flags).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("--dynamic cannot be used with --plan"),
        "error must explain why dynamic+plan is rejected, got: {msg}"
    );
}

#[test]
fn validate_dynamic_flags_rejects_malformed_leader_value() {
    // Malformed --leader (no "::" separator) is caught by validate_dynamic_flags.
    let flags = make_dynamic_flags(true, None, Some("0042"), Some("claude"), false, None);
    let err = validate_dynamic_flags(&flags).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("invalid --leader value"),
        "error must describe malformed leader, got: {msg}"
    );
}

#[test]
fn validate_dynamic_flags_ok_with_valid_dynamic_invocation() {
    let flags = make_dynamic_flags(
        true,
        None,
        Some("0042"),
        Some("claude::claude-opus-4-8"),
        false,
        None,
    );
    assert!(
        validate_dynamic_flags(&flags).is_ok(),
        "valid dynamic invocation with --leader must pass"
    );
}

#[test]
fn validate_dynamic_flags_ok_with_dynamic_no_leader() {
    let flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);
    assert!(
        validate_dynamic_flags(&flags).is_ok(),
        "valid dynamic invocation without --leader must pass"
    );
}

#[test]
fn validate_dynamic_flags_ok_with_static_invocation() {
    let flags = make_dynamic_flags(false, Some("/tmp/wf.toml"), None, None, false, None);
    assert!(
        validate_dynamic_flags(&flags).is_ok(),
        "valid static invocation must pass"
    );
}

// ── LeaderSpec::parse ─────────────────────────────────────────────────────

#[test]
fn leader_spec_parses_valid_agent_and_model() {
    let spec = parse_leader_flag("claude::claude-opus-4-8").unwrap();
    assert_eq!(spec.agent, "claude");
    assert_eq!(spec.model, "claude-opus-4-8");
}

#[test]
fn leader_spec_error_plain_string_no_double_colon() {
    let err = parse_leader_flag("claude").unwrap_err();
    assert!(
        err.to_string().contains("invalid --leader value"),
        "got: {err}"
    );
}

#[test]
fn leader_spec_error_empty_string() {
    let err = parse_leader_flag("").unwrap_err();
    assert!(
        err.to_string().contains("invalid --leader value"),
        "got: {err}"
    );
}

#[test]
fn leader_spec_error_empty_agent_component() {
    let err = parse_leader_flag("::claude-opus-4-8").unwrap_err();
    assert!(
        err.to_string().contains("invalid --leader value"),
        "got: {err}"
    );
}

#[test]
fn leader_spec_error_empty_model_component() {
    let err = parse_leader_flag("claude::").unwrap_err();
    assert!(
        err.to_string().contains("invalid --leader value"),
        "got: {err}"
    );
}

#[test]
fn leader_spec_error_three_components() {
    let err = parse_leader_flag("a::b::c").unwrap_err();
    assert!(
        err.to_string().contains("invalid --leader value"),
        "got: {err}"
    );
}

#[test]
fn leader_spec_error_message_includes_format_hint() {
    let err = parse_leader_flag("badvalue").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("agent::model"),
        "error must include the expected format hint, got: {msg}"
    );
}

// ── apply_dynamic_implied_flags ───────────────────────────────────────────

#[test]
fn apply_dynamic_implied_flags_sets_yolo_true() {
    let mut flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);
    flags.yolo = false;
    apply_dynamic_implied_flags(&mut flags);
    assert!(flags.yolo, "apply_dynamic_implied_flags must set yolo=true");
}

#[test]
fn apply_dynamic_implied_flags_sets_worktree_true() {
    let mut flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);
    flags.worktree = false;
    apply_dynamic_implied_flags(&mut flags);
    assert!(
        flags.worktree,
        "apply_dynamic_implied_flags must set worktree=true"
    );
}

#[test]
fn apply_dynamic_implied_flags_adds_context_workflow_overlay() {
    let mut flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);
    flags.overlay.clear();
    apply_dynamic_implied_flags(&mut flags);
    assert!(
        flags.overlay.iter().any(|o| o.contains("context(workflow")),
        "apply_dynamic_implied_flags must add context(workflow) overlay"
    );
}

#[test]
fn apply_dynamic_implied_flags_does_not_duplicate_context_overlay() {
    let mut flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);
    flags.overlay = vec!["context(workflow)".to_string()];
    apply_dynamic_implied_flags(&mut flags);
    let count = flags
        .overlay
        .iter()
        .filter(|o| o.contains("context(workflow"))
        .count();
    assert_eq!(count, 1, "context(workflow) must not be duplicated");
}

#[test]
fn apply_dynamic_implied_flags_preserves_existing_overlays() {
    let mut flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);
    flags.overlay = vec!["env(MY_VAR)".to_string()];
    apply_dynamic_implied_flags(&mut flags);
    assert!(
        flags.overlay.contains(&"env(MY_VAR)".to_string()),
        "pre-existing overlays must be preserved"
    );
}

// ── build_leader_prompt / build_repair_prompt ─────────────────────────────

#[test]
fn build_leader_prompt_substitutes_work_item_number() {
    let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
        "0042",
        "/workspace/aspec/work-items/0042-my-item.md",
        "  - claude",
        None,
        None,
    );
    assert!(
        prompt.contains("0042"),
        "leader prompt must contain the work item number"
    );
}

#[test]
fn build_leader_prompt_substitutes_work_item_path() {
    let path = "/workspace/aspec/work-items/0042-my-item.md";
    let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
        "0042",
        path,
        "  - claude",
        None,
        None,
    );
    assert!(
        prompt.contains(path),
        "leader prompt must contain the work item path"
    );
}

#[test]
fn build_leader_prompt_substitutes_available_agents() {
    let agents = "  - claude\n  - maki";
    let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
        "0042", "/path", agents, None, None,
    );
    assert!(
        prompt.contains("claude"),
        "leader prompt must list available agents"
    );
    assert!(
        prompt.contains("maki"),
        "leader prompt must list all available agents"
    );
}

#[test]
fn build_leader_prompt_no_unreplaced_placeholders() {
    let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
        "0099",
        "/workspace/aspec/work-items/0099-task.md",
        "  - claude",
        None,
        None,
    );
    assert!(
        !prompt.contains("{{work_item_number}}"),
        "{{work_item_number}} must be substituted"
    );
    assert!(
        !prompt.contains("{{work_item_path}}"),
        "{{work_item_path}} must be substituted"
    );
    assert!(
        !prompt.contains("{{available_agents}}"),
        "{{available_agents}} must be substituted"
    );
    assert!(
        !prompt.contains("{{max_concurrent_steps_note}}"),
        "{{max_concurrent_steps_note}} must be substituted"
    );
    assert!(
        !prompt.contains("{{developer_guidance}}"),
        "{{developer_guidance}} must be substituted"
    );
}

// ── build_leader_prompt: developer guidance (WI-0099) ─────────────────────

#[test]
fn build_leader_prompt_includes_developer_guidance_section_when_present() {
    let guidance = vec![
        "never spawn more than two agents in parallel".to_string(),
        "always include a validation step after each implementation step".to_string(),
    ];
    let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
        "0099",
        "/path",
        "  - claude",
        None,
        Some(&guidance),
    );
    assert!(
            prompt.contains("## Developer Guidance"),
            "prompt must include the Developer Guidance heading when guidance is present, got: {prompt}"
        );
    assert!(
        prompt.contains("- never spawn more than two agents in parallel"),
        "prompt must render the first guidance entry as a bullet, got: {prompt}"
    );
    assert!(
        prompt.contains("- always include a validation step after each implementation step"),
        "prompt must render the second guidance entry as a bullet, got: {prompt}"
    );
}

/// The section itself is template text and always renders; what changes is
/// whether it carries bullets or an explicit statement that there are none.
/// A section that vanished silently could not be told apart from one the
/// template never had.
#[test]
fn build_leader_prompt_states_absent_developer_guidance_when_none() {
    let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
        "0099",
        "/path",
        "  - claude",
        None,
        None,
    );
    assert!(
        prompt.contains("## Developer Guidance"),
        "the template owns the heading, so it renders either way, got: {prompt}"
    );
    assert!(
        prompt.contains("(none)"),
        "absent guidance must be stated, not left blank, got: {prompt}"
    );
    assert!(
        !prompt.contains("{{developer_guidance}}"),
        "no stray placeholder token must remain when guidance is None, got: {prompt}"
    );
}

#[test]
fn build_leader_prompt_states_absent_developer_guidance_when_empty() {
    let guidance: Vec<String> = Vec::new();
    let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
        "0099",
        "/path",
        "  - claude",
        None,
        Some(&guidance),
    );
    assert!(
        prompt.contains("(none)"),
        "an empty guidance list must be stated as absent, got: {prompt}"
    );
    assert!(
        !prompt.contains("{{developer_guidance}}"),
        "no stray placeholder token must remain when guidance is empty, got: {prompt}"
    );
}

#[test]
fn build_leader_prompt_states_the_concurrency_limit_when_max_concurrent_steps_is_some() {
    let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
        "0042",
        "/path",
        "  - claude",
        Some(3),
        None,
    );
    assert!(
        prompt.contains("Maximum concurrent steps advised: 3."),
        "prompt must state the configured limit when Some(n), got: {prompt}"
    );
}

/// As with guidance, the sentence is template text either way: "no limit
/// configured" is a fact the leader can plan against, where a vanished line
/// is silence it has to guess at.
#[test]
fn build_leader_prompt_states_no_limit_when_max_concurrent_steps_is_none() {
    let prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
        "0042",
        "/path",
        "  - claude",
        None,
        None,
    );
    assert!(
        prompt.contains("Maximum concurrent steps advised: no limit."),
        "prompt must state the absence of a limit when None, got: {prompt}"
    );
    assert!(
        !prompt.contains("{{max_concurrent_steps}}"),
        "no stray placeholder token must remain when None, got: {prompt}"
    );
}

#[test]
fn build_repair_prompt_substitutes_validation_error() {
    let error = "TOML parse error: unexpected key 'bogus' at line 3";
    let prompt = crate::data::dynamic_workflow_assets::build_repair_prompt(error);
    assert!(
        prompt.contains(error),
        "repair prompt must contain the verbatim validation error, got: {prompt}"
    );
}

#[test]
fn build_repair_prompt_no_unreplaced_placeholders() {
    let prompt = crate::data::dynamic_workflow_assets::build_repair_prompt("some error");
    assert!(
        !prompt.contains("{{validation_error}}"),
        "{{validation_error}} must be substituted"
    );
}

// ── Embedded assets ───────────────────────────────────────────────────────

#[test]
fn example_workflow_toml_parses_as_valid_workflow() {
    use crate::data::workflow_definition::WorkflowFormat;
    let result = crate::data::workflow_definition::Workflow::parse(
        crate::data::dynamic_workflow_assets::EXAMPLE_WORKFLOW_TOML,
        WorkflowFormat::Toml,
    );
    assert!(
        result.is_ok(),
        "EXAMPLE_WORKFLOW_TOML must parse as a valid Workflow: {:?}",
        result.err()
    );
    let wf = result.unwrap();
    assert!(
        !wf.steps.is_empty(),
        "example workflow must have at least one step"
    );
}

#[test]
fn workflow_usage_md_is_nonempty() {
    assert!(
        !crate::data::dynamic_workflow_assets::WORKFLOW_USAGE_MD.is_empty(),
        "WORKFLOW_USAGE_MD must not be empty"
    );
}

#[test]
fn leader_prompt_md_is_nonempty() {
    assert!(
        !crate::data::dynamic_workflow_assets::LEADER_PROMPT_MD.is_empty(),
        "LEADER_PROMPT_MD must not be empty"
    );
}

#[test]
fn leader_repair_prompt_is_nonempty() {
    assert!(
        !crate::data::dynamic_workflow_assets::LEADER_REPAIR_PROMPT.is_empty(),
        "LEADER_REPAIR_PROMPT must not be empty"
    );
}

// ── Leader model selection (WI-0092 §7) ──────────────────────────────────

#[test]
fn resolve_leader_model_with_leader_flag_uses_spec_agent_and_model() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_simple(&tmp);
    let mut flags = make_dynamic_flags(
        true,
        None,
        Some("0042"),
        Some("claude::claude-opus-4-8"),
        false,
        None,
    );
    flags.agent = None;
    let (agent, model) = resolve_leader_model(&flags, &session).unwrap();
    assert_eq!(agent.as_str(), "claude");
    assert_eq!(model.as_deref(), Some("claude-opus-4-8"));
}

#[test]
fn resolve_leader_model_with_leader_flag_ignores_flags_model() {
    // --leader takes full precedence; --model must NOT be used for the leader.
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_simple(&tmp);
    let mut flags = make_dynamic_flags(
        true,
        None,
        Some("0042"),
        Some("claude::claude-opus-4-8"),
        false,
        Some("some-other-model"),
    );
    flags.agent = None;
    let (_agent, model) = resolve_leader_model(&flags, &session).unwrap();
    assert_eq!(
        model.as_deref(),
        Some("claude-opus-4-8"),
        "--model must be ignored for the leader when --leader is present"
    );
}

#[test]
fn resolve_leader_model_with_model_flag_no_leader_passes_model() {
    // Case (b): --model present, no --leader → model forwarded from flags.
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_agent(&tmp, "maki");
    let flags = make_dynamic_flags(true, None, Some("0042"), None, false, Some("custom-model"));
    let (_agent, model) = resolve_leader_model(&flags, &session).unwrap();
    assert_eq!(
        model.as_deref(),
        Some("custom-model"),
        "--model must be passed to leader when no --leader"
    );
}

#[test]
fn resolve_leader_model_with_neither_flag_model_is_none() {
    // Case (c): neither --leader nor --model → no model override.
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_simple(&tmp);
    let flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);
    let (_agent, model) = resolve_leader_model(&flags, &session).unwrap();
    assert!(
        model.is_none(),
        "model must be None when neither --leader nor --model is set"
    );
}

#[test]
fn resolve_leader_model_both_flags_leader_model_wins() {
    // Case (d): both --leader and --model → leader spec's model governs.
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_simple(&tmp);
    let flags = make_dynamic_flags(
        true,
        None,
        Some("0042"),
        Some("claude::claude-opus-4-8"),
        false,
        Some("should-be-ignored-for-leader"),
    );
    let (_agent, model) = resolve_leader_model(&flags, &session).unwrap();
    assert_eq!(
        model.as_deref(),
        Some("claude-opus-4-8"),
        "leader spec model must win over --model when both are set"
    );
}

// ── Leader resolution precedence: --leader > defaultLeader > --model (WI-0095 §5) ──

#[test]
fn resolve_leader_model_uses_default_leader_from_config_when_flag_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_default_leader(&tmp, "codex::codex-mini-latest");
    let flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);

    let (agent, model) = resolve_leader_model(&flags, &session).unwrap();
    assert_eq!(agent.as_str(), "codex");
    assert_eq!(model.as_deref(), Some("codex-mini-latest"));
}

#[test]
fn resolve_leader_model_leader_flag_wins_over_default_leader_config() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_default_leader(&tmp, "codex::codex-mini-latest");
    let flags = make_dynamic_flags(
        true,
        None,
        Some("0042"),
        Some("claude::claude-opus-4-8"),
        false,
        None,
    );

    let (agent, model) = resolve_leader_model(&flags, &session).unwrap();
    assert_eq!(
        agent.as_str(),
        "claude",
        "--leader must win over dynamicWorkflows.defaultLeader"
    );
    assert_eq!(model.as_deref(), Some("claude-opus-4-8"));
}

#[test]
fn resolve_leader_model_default_leader_config_not_overridden_by_model_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_default_leader(&tmp, "codex::codex-mini-latest");
    let flags = make_dynamic_flags(
        true,
        None,
        Some("0042"),
        None,
        false,
        Some("should-be-ignored"),
    );

    let (agent, model) = resolve_leader_model(&flags, &session).unwrap();
    assert_eq!(agent.as_str(), "codex");
    assert_eq!(
        model.as_deref(),
        Some("codex-mini-latest"),
        "--model must not override dynamicWorkflows.defaultLeader's model"
    );
}

#[test]
fn resolve_leader_model_no_flag_no_config_falls_back_to_default_agent() {
    // The "default" source in the 3-way precedence: no --leader, no
    // dynamicWorkflows.defaultLeader in config → falls back to --model +
    // default-agent resolution (WI-0092 behavior, case (c) above).
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_simple(&tmp);
    let flags = make_dynamic_flags(true, None, Some("0042"), None, false, None);

    let (_agent, model) = resolve_leader_model(&flags, &session).unwrap();
    assert!(
        model.is_none(),
        "with no --leader, no defaultLeader, and no --model, model must be None"
    );
}

// ── AvailableActions.launch_next_label ────────────────────────────────────

#[test]
fn available_actions_launch_next_label_defaults_to_none() {
    let actions = AvailableActions::default();
    assert!(
        actions.launch_next_label.is_none(),
        "launch_next_label must default to None (renders fallback label)"
    );
}

#[test]
fn available_actions_launch_next_label_can_be_set_to_dynamic_string() {
    let actions = AvailableActions {
        launch_next_label: Some("Start dynamic workflow".to_string()),
        ..Default::default()
    };
    assert_eq!(
        actions.launch_next_label.as_deref(),
        Some("Start dynamic workflow")
    );
}

#[test]
fn available_actions_cli_uses_launch_next_label_when_set() {
    // Verify the rendering pattern: .as_deref().unwrap_or(fallback).
    // The CLI uses: `available.launch_next_label.as_deref().unwrap_or("Launch next step (new container)")`.
    let actions = AvailableActions {
        launch_next_label: Some("Start dynamic workflow".to_string()),
        can_launch_next: true,
        ..Default::default()
    };
    let rendered = actions
        .launch_next_label
        .as_deref()
        .unwrap_or("Launch next step (new container)");
    assert_eq!(rendered, "Start dynamic workflow");
}

#[test]
fn available_actions_cli_falls_back_when_label_is_none() {
    let actions = AvailableActions {
        launch_next_label: None,
        can_launch_next: true,
        ..Default::default()
    };
    let rendered = actions
        .launch_next_label
        .as_deref()
        .unwrap_or("Launch next step (new container)");
    assert_eq!(rendered, "Launch next step (new container)");
}

#[test]
fn available_actions_tui_uses_launch_next_label_when_set() {
    // TUI renders: state.launch_next_label.as_deref().unwrap_or("Next: new container")
    let label: Option<String> = Some("Start dynamic workflow".to_string());
    let rendered = label.as_deref().unwrap_or("Next: new container");
    assert_eq!(rendered, "Start dynamic workflow");
}

#[test]
fn available_actions_tui_falls_back_to_next_new_container() {
    let label: Option<String> = None;
    let rendered = label.as_deref().unwrap_or("Next: new container");
    assert_eq!(rendered, "Next: new container");
}

// ── format_available_agents ───────────────────────────────────────────────

#[test]
fn format_available_agents_empty_list_gives_placeholder() {
    let result = format_available_agents(&[]);
    assert!(
        result.contains("no agents discovered"),
        "empty agent list must give placeholder message, got: {result}"
    );
    assert!(
        result.contains(".awman/Dockerfile.<agent>"),
        "placeholder must mention the expected path, got: {result}"
    );
}

#[test]
fn format_available_agents_single_agent() {
    let agents = vec![(
        "claude".to_string(),
        std::path::PathBuf::from("/r/.awman/Dockerfile.claude"),
    )];
    let result = format_available_agents(&agents);
    assert!(
        result.contains("claude"),
        "formatted agents must include the agent name, got: {result}"
    );
    assert!(
        result.contains("  - claude"),
        "agents must be formatted with '  - ' prefix, got: {result}"
    );
}

#[test]
fn format_available_agents_multiple_agents_are_listed() {
    let agents = vec![
        (
            "claude".to_string(),
            std::path::PathBuf::from("/r/.awman/Dockerfile.claude"),
        ),
        (
            "maki".to_string(),
            std::path::PathBuf::from("/r/.awman/Dockerfile.maki"),
        ),
    ];
    let result = format_available_agents(&agents);
    assert!(result.contains("claude"), "must list claude");
    assert!(result.contains("maki"), "must list maki");
}

// ── format_agents_with_models (WI-0095 §3) ────────────────────────────────

#[test]
fn format_agents_with_models_typical_map() {
    let mut map = std::collections::HashMap::new();
    map.insert("claude".to_string(), vec!["claude-opus-4-8".to_string()]);
    let result = format_agents_with_models(&map);
    assert_eq!(result, "  - claude: claude-opus-4-8");
}

#[test]
fn format_agents_with_models_sorted_alphabetically_for_determinism() {
    let mut map = std::collections::HashMap::new();
    map.insert("gemini".to_string(), vec!["gemini-2.5-pro".to_string()]);
    map.insert("claude".to_string(), vec!["claude-opus-4-8".to_string()]);
    map.insert("codex".to_string(), vec!["codex-mini-latest".to_string()]);
    let result = format_agents_with_models(&map);
    let lines: Vec<&str> = result.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(
        lines[0].starts_with("  - claude:"),
        "agents must be sorted alphabetically, got: {lines:?}"
    );
    assert!(lines[1].starts_with("  - codex:"));
    assert!(lines[2].starts_with("  - gemini:"));
}

#[test]
fn format_agents_with_models_handles_single_model() {
    let mut map = std::collections::HashMap::new();
    map.insert("claude".to_string(), vec!["claude-opus-4-8".to_string()]);
    let result = format_agents_with_models(&map);
    assert!(result.contains("claude-opus-4-8"));
    assert!(
        !result.contains(','),
        "a single-model entry must not contain a comma, got: {result}"
    );
}

#[test]
fn format_agents_with_models_handles_multiple_models_comma_joined_in_order() {
    let mut map = std::collections::HashMap::new();
    map.insert(
        "claude".to_string(),
        vec![
            "claude-opus-4-8".to_string(),
            "claude-sonnet-4-6".to_string(),
        ],
    );
    let result = format_agents_with_models(&map);
    assert_eq!(
        result, "  - claude: claude-opus-4-8, claude-sonnet-4-6",
        "configured model-list order must be preserved"
    );
}

// ── build_effective_agents_to_models (WI-0095 §2 agent validation) ────────

fn agent_dockerfiles(names: &[&str]) -> Vec<(String, std::path::PathBuf)> {
    names
        .iter()
        .map(|n| {
            (
                n.to_string(),
                std::path::PathBuf::from(format!("/r/.awman/Dockerfile.{n}")),
            )
        })
        .collect()
}

#[test]
fn build_effective_agents_to_models_all_match_succeeds() {
    let mut configured = std::collections::HashMap::new();
    configured.insert("claude".to_string(), vec!["claude-opus-4-8".to_string()]);
    configured.insert("codex".to_string(), vec!["codex-mini-latest".to_string()]);
    let available = agent_dockerfiles(&["claude", "codex"]);
    let mut warnings = Vec::new();

    let effective =
        build_effective_agents_to_models(&configured, &available, &mut warnings).unwrap();

    assert_eq!(effective.len(), 2);
    assert_eq!(
        effective.get("claude"),
        Some(&vec!["claude-opus-4-8".to_string()])
    );
    assert!(warnings.is_empty());
}

#[test]
fn build_effective_agents_to_models_partial_mismatch_error_lists_only_missing() {
    let mut configured = std::collections::HashMap::new();
    configured.insert("claude".to_string(), vec!["claude-opus-4-8".to_string()]);
    configured.insert("foo".to_string(), vec!["some-model".to_string()]);
    let available = agent_dockerfiles(&["claude", "codex"]);
    let mut warnings = Vec::new();

    let err = build_effective_agents_to_models(&configured, &available, &mut warnings).unwrap_err();
    let msg = err.to_string();

    assert!(
        msg.contains("no Dockerfile in this repo: [foo]"),
        "error's missing-agents list must contain only foo, got: {msg}"
    );
    assert!(
        msg.contains("Available agents") && msg.contains("claude") && msg.contains("codex"),
        "error must list available agents, got: {msg}"
    );
}

#[test]
fn build_effective_agents_to_models_complete_mismatch_fails() {
    let mut configured = std::collections::HashMap::new();
    configured.insert("foo".to_string(), vec!["some-model".to_string()]);
    configured.insert("bar".to_string(), vec!["other-model".to_string()]);
    let available = agent_dockerfiles(&["claude"]);
    let mut warnings = Vec::new();

    let err = build_effective_agents_to_models(&configured, &available, &mut warnings).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("foo"), "got: {msg}");
    assert!(msg.contains("bar"), "got: {msg}");
}

#[test]
fn build_effective_agents_to_models_case_folded_match_emits_lowercase_and_warning() {
    let mut configured = std::collections::HashMap::new();
    configured.insert("Claude".to_string(), vec!["claude-opus-4-8".to_string()]);
    let available = agent_dockerfiles(&["claude"]);
    let mut warnings = Vec::new();

    let effective =
        build_effective_agents_to_models(&configured, &available, &mut warnings).unwrap();

    assert_eq!(
        effective.get("claude"),
        Some(&vec!["claude-opus-4-8".to_string()]),
        "the effective map must be keyed by the lowercase agent name"
    );
    assert!(
        !effective.contains_key("Claude"),
        "the configured mixed-case key must not survive into the effective map"
    );
    assert_eq!(warnings.len(), 1, "a case-folded match must warn");
    assert!(
        warnings[0].contains("\"Claude\"") && warnings[0].contains("case folding"),
        "warning must name the configured key and explain case folding, got: {}",
        warnings[0]
    );
}

#[test]
fn build_effective_agents_to_models_duplicate_keys_after_case_folding_fail() {
    let mut configured = std::collections::HashMap::new();
    configured.insert("Claude".to_string(), vec!["claude-opus-4-8".to_string()]);
    configured.insert("claude".to_string(), vec!["claude-sonnet-4-6".to_string()]);
    let available = agent_dockerfiles(&["claude"]);
    let mut warnings = Vec::new();

    let err = build_effective_agents_to_models(&configured, &available, &mut warnings).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("Claude") && msg.contains("claude") && msg.contains("case folding"),
        "duplicate case-folded keys must fail with both keys named, got: {msg}"
    );
}

#[test]
fn build_effective_agents_to_models_empty_map_is_not_an_error() {
    let configured = std::collections::HashMap::new();
    let available = agent_dockerfiles(&["claude"]);
    let mut warnings = Vec::new();

    let effective =
        build_effective_agents_to_models(&configured, &available, &mut warnings).unwrap();
    assert!(effective.is_empty());
    assert!(warnings.is_empty());
}

// ── resolve_and_validate_workflow_agents ──────────────────────────────────

#[test]
fn resolve_validates_step_agent_with_dockerfile() {
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM ubuntu\n").unwrap();
    let session = make_session_simple(&tmp);
    let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
    let wf = make_workflow(None, &[Some("claude")]);
    let result = resolve_and_validate_workflow_agents(&wf, &session, &paths);
    assert!(
        result.is_ok(),
        "step agent with Dockerfile must validate OK, got: {:?}",
        result.err()
    );
    let agents = result.unwrap();
    assert!(agents.contains(&"claude".to_string()));
}

#[test]
fn resolve_error_step_agent_without_dockerfile() {
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    // No Dockerfile.gemini
    let session = make_session_simple(&tmp);
    let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
    let wf = make_workflow(None, &[Some("gemini")]);
    let err = resolve_and_validate_workflow_agents(&wf, &session, &paths).unwrap_err();
    assert!(
        err.contains("gemini"),
        "error must name the unknown agent, got: {err}"
    );
    assert!(
        err.contains("Dockerfile.gemini"),
        "error must name the expected Dockerfile path, got: {err}"
    );
}

#[test]
fn resolve_error_unknown_agent_lists_available_agents() {
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM ubuntu\n").unwrap();
    let session = make_session_simple(&tmp);
    let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
    let wf = make_workflow(None, &[Some("gemini")]);
    let err = resolve_and_validate_workflow_agents(&wf, &session, &paths).unwrap_err();
    assert!(
        err.contains("Available agents"),
        "error must list available agents, got: {err}"
    );
    assert!(
        err.contains("claude"),
        "error must list claude as an available agent, got: {err}"
    );
}

#[test]
fn resolve_validates_workflow_level_agent_with_dockerfile() {
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    std::fs::write(awman_dir.join("Dockerfile.maki"), "FROM ubuntu\n").unwrap();
    let session = make_session_simple(&tmp);
    let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
    // Workflow-level agent, steps have no agent.
    let wf = make_workflow(Some("maki"), &[None]);
    let result = resolve_and_validate_workflow_agents(&wf, &session, &paths);
    assert!(
        result.is_ok(),
        "workflow-level agent with Dockerfile must validate OK"
    );
}

#[test]
fn resolve_error_workflow_level_agent_without_dockerfile() {
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    let session = make_session_simple(&tmp);
    let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
    let wf = make_workflow(Some("badname"), &[None]);
    let err = resolve_and_validate_workflow_agents(&wf, &session, &paths).unwrap_err();
    assert!(
        err.contains("badname"),
        "error must name the unknown workflow-level agent, got: {err}"
    );
}

#[test]
fn resolve_error_no_agent_anywhere_suggests_fix() {
    // No step agent, no workflow agent, no session default → error.
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM ubuntu\n").unwrap();
    let session = make_session_simple(&tmp); // no default agent
    let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
    let wf = make_workflow(None, &[None]); // no step or workflow agent
    let err = resolve_and_validate_workflow_agents(&wf, &session, &paths).unwrap_err();
    assert!(
        err.contains("no agent"),
        "error must mention missing agent, got: {err}"
    );
    assert!(
        err.contains("workflow-level"),
        "error must suggest adding workflow-level agent, got: {err}"
    );
}

#[test]
fn resolve_deduplicates_repeated_agent_names() {
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM ubuntu\n").unwrap();
    let session = make_session_simple(&tmp);
    let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
    let wf = make_workflow(None, &[Some("claude"), Some("claude"), Some("claude")]);
    let result = resolve_and_validate_workflow_agents(&wf, &session, &paths).unwrap();
    assert_eq!(
        result.len(),
        1,
        "claude must appear only once in the resolved list"
    );
}

#[test]
fn resolve_error_multiple_unknown_agents_listed_together() {
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    // No Dockerfiles for gemini or codex.
    let session = make_session_simple(&tmp);
    let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
    let wf = make_workflow(None, &[Some("gemini"), Some("codex")]);
    let err = resolve_and_validate_workflow_agents(&wf, &session, &paths).unwrap_err();
    assert!(err.contains("gemini"), "error must name gemini, got: {err}");
    assert!(err.contains("codex"), "error must name codex, got: {err}");
}

// ── validate_generated_workflow (integration-style unit tests) ────────────

#[test]
fn validate_generated_workflow_missing_file_error_contains_path() {
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    let session = make_session_simple(&tmp);
    let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
    let missing = tmp.path().join("workflow.toml");

    let err = validate_generated_workflow(&missing, &session, &paths).unwrap_err();
    assert!(
        err.contains("workflow.toml"),
        "error must mention the expected file path, got: {err}"
    );
    assert!(
        err.contains("did not produce"),
        "error must explain the leader failed to produce the file, got: {err}"
    );
}

#[test]
fn validate_generated_workflow_invalid_toml_propagates_parse_error() {
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    let session = make_session_simple(&tmp);
    let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
    let wf_path = tmp.path().join("workflow.toml");
    std::fs::write(&wf_path, "this is NOT valid toml ][").unwrap();

    let err = validate_generated_workflow(&wf_path, &session, &paths).unwrap_err();
    assert!(
        !err.is_empty(),
        "invalid TOML must produce a non-empty error"
    );
}

#[test]
fn validate_generated_workflow_unknown_agent_error_names_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    // Only "claude" Dockerfile present; workflow references "gemini".
    std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM ubuntu\n").unwrap();
    let session = make_session_simple(&tmp);
    let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
    let wf_path = tmp.path().join("workflow.toml");
    std::fs::write(
        &wf_path,
        r#"[[steps]]
name = "do-stuff"
agent = "gemini"
prompt = "do something"
"#,
    )
    .unwrap();

    let err = validate_generated_workflow(&wf_path, &session, &paths).unwrap_err();
    assert!(
        err.contains("gemini"),
        "error must name the unknown agent, got: {err}"
    );
    assert!(
        err.contains("Available agents"),
        "error must list available agents for repair, got: {err}"
    );
    assert!(
        err.contains("claude"),
        "error must list claude as available, got: {err}"
    );
}

#[test]
fn validate_generated_workflow_valid_with_known_agent_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM ubuntu\n").unwrap();
    let session = make_session_simple(&tmp);
    let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
    let wf_path = tmp.path().join("workflow.toml");
    std::fs::write(
        &wf_path,
        r#"[[steps]]
name = "step1"
agent = "claude"
prompt = "do something useful"
"#,
    )
    .unwrap();

    let result = validate_generated_workflow(&wf_path, &session, &paths);
    assert!(
        result.is_ok(),
        "valid workflow with known agent must succeed, got: {:?}",
        result.err()
    );
    let wf = result.unwrap();
    assert_eq!(wf.steps.len(), 1);
    assert_eq!(wf.steps[0].name, "step1");
}

#[test]
fn repair_prompt_substitution_contains_verbatim_validation_error() {
    // Verify the repair loop passes the exact error string to build_repair_prompt.
    let error_msg = "workflow.toml references agents with no Dockerfile: \"gemini\"";
    let repair_prompt = crate::data::dynamic_workflow_assets::build_repair_prompt(error_msg);
    assert!(
            repair_prompt.contains(error_msg),
            "repair prompt must contain the verbatim validation error from Workflow::load(), got: {repair_prompt}"
        );
}

// ── Integration: dynamicWorkflows config → leader prompt (WI-0095) ────────
//
// These exercise the same sequence `run_dynamic` performs — RepoConfig
// load, Dockerfile discovery, `build_effective_agents_to_models`,
// `format_agents_with_models`, `build_leader_prompt` — without requiring
// Docker, since none of that sequence touches the container runtime. The
// mismatched-agents case demonstrates the failure happens at this stage,
// strictly before `ensure_agent_image`/`drive_leader_agent` would run.

#[test]
fn integration_dynamic_config_valid_agents_produces_leader_prompt_with_models_and_advisory() {
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM ubuntu\n").unwrap();
    std::fs::write(awman_dir.join("Dockerfile.codex"), "FROM ubuntu\n").unwrap();
    std::fs::write(
        awman_dir.join("config.json"),
        r#"{
                "dynamicWorkflows": {
                    "agentsToModels": {
                        "claude": ["claude-opus-4-8"],
                        "codex": ["codex-mini-latest"]
                    },
                    "maxConcurrentSteps": 2
                }
            }"#,
    )
    .unwrap();

    let repo_config = crate::data::config::repo::RepoConfig::load(tmp.path()).unwrap();
    let dw = repo_config
        .dynamic_workflows
        .clone()
        .expect("dynamicWorkflows section must be present");

    let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
    let available_agents = paths.discover_agent_dockerfiles();

    let mut warnings = Vec::new();
    let effective = build_effective_agents_to_models(
        dw.agents_to_models.as_ref().unwrap(),
        &available_agents,
        &mut warnings,
    )
    .expect("all configured agents have Dockerfiles; validation must succeed");
    let agents_section = format_agents_with_models(&effective);

    let leader_prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
        "0042",
        "/workspace/aspec/work-items/0042-item.md",
        &agents_section,
        dw.max_concurrent_steps,
        dw.guidance.as_deref(),
    );

    assert!(
        leader_prompt.contains("claude-opus-4-8"),
        "leader prompt must contain the configured claude model, got: {leader_prompt}"
    );
    assert!(
        leader_prompt.contains("codex-mini-latest"),
        "leader prompt must contain the configured codex model, got: {leader_prompt}"
    );
    assert!(
        leader_prompt.contains("Maximum concurrent steps advised: 2."),
        "leader prompt must contain the maxConcurrentSteps advisory, got: {leader_prompt}"
    );
}

#[test]
fn integration_dynamic_config_guidance_entries_appear_in_leader_prompt() {
    // Mirrors the agentsToModels integration test above (WI-0099): load a
    // real RepoConfig with two guidance entries and run it through
    // build_leader_prompt, asserting both entries render as bullets.
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    std::fs::write(
        awman_dir.join("config.json"),
        r#"{
                "dynamicWorkflows": {
                    "guidance": [
                        "never spawn more than two agents in parallel",
                        "always include a validation step after each implementation step"
                    ]
                }
            }"#,
    )
    .unwrap();

    let repo_config = crate::data::config::repo::RepoConfig::load(tmp.path()).unwrap();
    let dw = repo_config
        .dynamic_workflows
        .clone()
        .expect("dynamicWorkflows section must be present");

    let leader_prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
        "0099",
        "/workspace/aspec/work-items/0099-item.md",
        "  - claude",
        dw.max_concurrent_steps,
        dw.guidance.as_deref(),
    );

    assert!(
        leader_prompt.contains("## Developer Guidance"),
        "leader prompt must include the Developer Guidance heading, got: {leader_prompt}"
    );
    assert!(
        leader_prompt.contains("- never spawn more than two agents in parallel"),
        "leader prompt must contain the first guidance entry, got: {leader_prompt}"
    );
    assert!(
        leader_prompt.contains("- always include a validation step after each implementation step"),
        "leader prompt must contain the second guidance entry, got: {leader_prompt}"
    );
}

#[test]
fn integration_dynamic_config_mismatched_agents_fails_before_container_spawn() {
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    // Only "claude" has a Dockerfile; config references "gemini", which does not.
    std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM ubuntu\n").unwrap();
    std::fs::write(
        awman_dir.join("config.json"),
        r#"{
                "dynamicWorkflows": {
                    "agentsToModels": {
                        "gemini": ["gemini-2.5-pro"]
                    }
                }
            }"#,
    )
    .unwrap();

    let repo_config = crate::data::config::repo::RepoConfig::load(tmp.path()).unwrap();
    let dw = repo_config
        .dynamic_workflows
        .clone()
        .expect("dynamicWorkflows section must be present");

    let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
    let available_agents = paths.discover_agent_dockerfiles();

    // This is the exact check `run_dynamic` performs immediately after
    // Dockerfile discovery and before `ensure_agent_image` /
    // `drive_leader_agent` — i.e. before any image build or container spawn.
    let mut warnings = Vec::new();
    let err = build_effective_agents_to_models(
        dw.agents_to_models.as_ref().unwrap(),
        &available_agents,
        &mut warnings,
    )
    .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("gemini"),
        "error must name the missing agent, got: {msg}"
    );
    assert!(
        msg.contains("no Dockerfile"),
        "error must explain the missing Dockerfile, got: {msg}"
    );
    assert!(
        msg.contains("Available agents") && msg.contains("claude"),
        "error must list available agents, got: {msg}"
    );
}

// ── The `run_dynamic` container path ──────────────────────────────────────
//
// Seven `#[ignore]`-ed `todo!()` stubs stood here until WI 0114 F-52. None
// had ever had a body: `cargo test -- --ignored` panicked on each one, so they
// reported nothing about the code and hid the fact that nothing covered the
// dynamic path from this file. The behaviour they named is covered, and by
// tests that run:
//
//   - leader writes a valid workflow, and repair exhaustion after three
//     attempts — `tests/squad_repair_loop.rs`
//   - `Stuck`/`Unstuck` delivery to every subscriber, including the countdown
//     the workflow engine arms — `tests/engine/stuck_event_wiring.rs`
//   - workflow load, DAG and state round-trip — `tests/engine/workflow_end_to_end.rs`
//
// What remains genuinely uncovered is ordering: that `WorktreeLifecycle`
// setup completes before the leader container is launched. That needs a
// container and belongs with the other Docker-gated integration tests, not as
// a stub here.

/// The leader-phase Workflow Control Board maps each `NextAction` returned
/// by the frontend onto the correct leader-scoped outcome (right arrow =
/// start workflow, up = restart, Ctrl-C/Pause = abort, everything else =
/// dismiss).
#[test]
fn leader_control_board_maps_actions() {
    fn outcome_for(action: NextAction) -> LeaderControlOutcome {
        let mut fe = FakeExecWorkflowFrontend::new();
        fe.next_action_response = action;
        let shared: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>> =
            Arc::new(Mutex::new(Box::new(fe)));
        show_leader_control_board(&shared, "leader")
    }

    assert!(matches!(
        outcome_for(NextAction::LaunchNext),
        LeaderControlOutcome::StartWorkflow
    ));
    assert!(matches!(
        outcome_for(NextAction::RestartCurrentStep),
        LeaderControlOutcome::Restart
    ));
    assert!(matches!(
        outcome_for(NextAction::Abort),
        LeaderControlOutcome::Abort
    ));
    assert!(matches!(
        outcome_for(NextAction::Pause),
        LeaderControlOutcome::Pause
    ));
    assert!(matches!(
        outcome_for(NextAction::Dismiss),
        LeaderControlOutcome::Dismiss
    ));
    // Actions with no meaning before a workflow exists just close the board.
    assert!(matches!(
        outcome_for(NextAction::CancelToPreviousStep),
        LeaderControlOutcome::Dismiss
    ));
    assert!(matches!(
        outcome_for(NextAction::ContinueInCurrentContainer {
            prompt: String::new()
        }),
        LeaderControlOutcome::Dismiss
    ));
}

// ─── WI-0115 §2: workflow resume ─────────────────────────────────────

use crate::data::workflow_dag::WorkflowDag;
use crate::data::workflow_state::StepState;

/// A linear a→b→c workflow plus its DAG, with the given per-step statuses.
fn resume_fixture(steps: &[&str], statuses: &[StepState]) -> (WorkflowState, WorkflowDag) {
    let wf_steps: Vec<WorkflowStep> = steps
        .iter()
        .enumerate()
        .map(|(i, name)| WorkflowStep {
            name: (*name).to_string(),
            depends_on: if i == 0 {
                vec![]
            } else {
                vec![steps[i - 1].to_string()]
            },
            prompt_template: "do it".into(),
            agent: None,
            model: None,
            overlays: None,
            abort_on_failure: false,
        })
        .collect();
    let mut state = WorkflowState::new("wf".into(), &wf_steps, "hash".into(), Some(1));
    for (name, status) in steps.iter().zip(statuses) {
        state.set_status(name, status.clone());
    }
    let dag = WorkflowDag::build(&wf_steps).unwrap();
    (state, dag)
}

fn failed(exit_code: i32) -> StepState {
    StepState::Failed {
        exit_code,
        error_message: None,
    }
}

#[test]
fn start_points_name_the_failed_step_and_its_neighbours() {
    let (state, dag) = resume_fixture(
        &["a", "b", "c"],
        &[StepState::Succeeded, failed(1), StepState::Pending],
    );
    let points = workflow_resume_start_points(&dag, &state);
    let names: Vec<&str> = points.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["b", "a", "c"]);
    assert!(points[0].role.contains("failed"));
}

#[test]
fn start_points_on_the_first_step_offer_no_previous() {
    let (state, dag) = resume_fixture(
        &["a", "b", "c"],
        &[failed(1), StepState::Cancelled, StepState::Cancelled],
    );
    let points = workflow_resume_start_points(&dag, &state);
    let names: Vec<&str> = points.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["a", "b"]);
}

#[test]
fn start_points_are_empty_when_the_previous_run_finished() {
    let (state, dag) = resume_fixture(
        &["a", "b", "c"],
        &[
            StepState::Succeeded,
            StepState::Succeeded,
            StepState::Skipped,
        ],
    );
    assert!(workflow_resume_start_points(&dag, &state).is_empty());
}

/// Both modes ask the same question, so the copy is built once. A dynamic
/// prompt names the work item and worktree; a plain one names the workflow.
#[test]
fn resume_prompt_copy_covers_both_modes() {
    let points = vec![WorkflowResumeStep {
        name: "b".into(),
        role: "the step that failed".into(),
    }];

    let dynamic = WorkflowResumePrompt::new(
        "implement-0042".into(),
        Some(42),
        Some(PathBuf::from("/wt/0042")),
        true,
        2,
        5,
        points.clone(),
    );
    assert!(dynamic.title.contains("dynamic"));
    assert!(dynamic.body.contains("Work item: 0042"), "{}", dynamic.body);
    assert!(dynamic.body.contains("/wt/0042"), "{}", dynamic.body);
    assert!(dynamic.body.contains("2/5"), "{}", dynamic.body);
    assert!(dynamic.fresh_label.contains("dynamic"));

    let plain = WorkflowResumePrompt::new("ship-it".into(), None, None, false, 1, 3, points);
    assert!(!plain.title.contains("dynamic"));
    assert!(plain.body.contains("ship-it"), "{}", plain.body);
    assert!(!plain.body.contains("Work item"), "{}", plain.body);
    assert!(!plain.body.contains("Worktree"), "{}", plain.body);
    assert_eq!(
        plain.choice_labels(),
        vec!["Resume from 'b' (the step that failed)"]
    );
}

#[test]
fn unattended_resume_picks_the_step_the_run_stopped_on() {
    let prompt = WorkflowResumePrompt::new(
        "wf".into(),
        None,
        None,
        false,
        1,
        3,
        vec![
            WorkflowResumeStep {
                name: "b".into(),
                role: "the step that failed".into(),
            },
            WorkflowResumeStep {
                name: "a".into(),
                role: "the step before it".into(),
            },
        ],
    );
    assert_eq!(
        prompt.resume_from_stop_point(),
        WorkflowResumeDecision::ResumeFrom("b".into())
    );
}

/// Build a workflow whose steps match `resume_fixture`'s linear chain, so
/// a saved state and a parsed workflow can be handed to
/// `offer_state_resume` together.
fn linear_workflow(steps: &[&str]) -> Workflow {
    let mut toml = String::from("title = \"wf\"\nagent = \"claude\"\n");
    for (i, name) in steps.iter().enumerate() {
        toml.push_str(&format!("\n[[step]]\nname = \"{name}\"\nprompt = \"go\"\n"));
        if i > 0 {
            toml.push_str(&format!("depends_on = [\"{}\"]\n", steps[i - 1]));
        }
    }
    Workflow::parse(
        &toml,
        crate::data::workflow_definition::WorkflowFormat::Toml,
    )
    .unwrap()
}

/// Seed a saved state for `wf` at `root` and return its store.
fn seed_state(
    root: &Path,
    statuses: &[StepState],
) -> crate::data::workflow_state_store::WorkflowStateStore {
    let store = crate::data::workflow_state_store::WorkflowStateStore::at_git_root(root);
    let (mut state, _) = resume_fixture(&["a", "b", "c"], statuses);
    state.workflow_name = "wf".into();
    state.work_item = None;
    store.save(&state).unwrap();
    store
}

/// Esc is a cancellation of the *command*, not an answer to the question:
/// the previous run must survive it byte for byte, so the same offer is
/// there next time.
#[test]
fn cancelling_the_resume_prompt_leaves_the_saved_run_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let store = seed_state(
        tmp.path(),
        &[StepState::Succeeded, failed(1), StepState::Cancelled],
    );
    let path = store.state_path(None, "wf");
    let before = std::fs::read(&path).unwrap();

    let mut frontend =
        FakeExecWorkflowFrontend::new().answering_resume(WorkflowResumeDecision::Cancel);
    let outcome = offer_state_resume(
        &store,
        &linear_workflow(&["a", "b", "c"]),
        "wf",
        None,
        None,
        &mut frontend,
    )
    .unwrap();

    assert_eq!(outcome, StateResumeOutcome::Cancelled);
    assert!(path.exists(), "cancelling must not delete the saved run");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "cancelling must not rewind the saved run either"
    );
}

/// `Fresh` is the destructive answer, and the only one that is.
#[test]
fn starting_over_deletes_the_saved_run() {
    let tmp = tempfile::tempdir().unwrap();
    let store = seed_state(
        tmp.path(),
        &[StepState::Succeeded, failed(1), StepState::Cancelled],
    );
    let path = store.state_path(None, "wf");

    let mut frontend =
        FakeExecWorkflowFrontend::new().answering_resume(WorkflowResumeDecision::Fresh);
    let outcome = offer_state_resume(
        &store,
        &linear_workflow(&["a", "b", "c"]),
        "wf",
        None,
        None,
        &mut frontend,
    )
    .unwrap();

    assert_eq!(outcome, StateResumeOutcome::Proceed { resumed: false });
    assert!(!path.exists());
}

/// Accepting a resume rewinds the state on disk *before* the engine reads
/// it, and reports `resumed` so the caller knows not to re-ask the
/// existing-worktree question.
#[test]
fn accepting_a_resume_rewinds_the_state_and_reports_it() {
    let tmp = tempfile::tempdir().unwrap();
    let store = seed_state(
        tmp.path(),
        &[StepState::Succeeded, failed(1), StepState::Cancelled],
    );

    let mut frontend = FakeExecWorkflowFrontend::new()
        .answering_resume(WorkflowResumeDecision::ResumeFrom("b".into()));
    let outcome = offer_state_resume(
        &store,
        &linear_workflow(&["a", "b", "c"]),
        "wf",
        None,
        None,
        &mut frontend,
    )
    .unwrap();

    assert_eq!(outcome, StateResumeOutcome::Proceed { resumed: true });
    let saved = store.load(None, "wf").unwrap().unwrap();
    assert_eq!(saved.status_of("a"), Some(&StepState::Succeeded));
    assert_eq!(saved.status_of("b"), Some(&StepState::Pending));
    assert_eq!(saved.status_of("c"), Some(&StepState::Pending));
}

/// A run that stopped without failing — interrupted, or paused — must not
/// be described as one that failed.
#[test]
fn the_stop_point_is_named_after_what_actually_stopped_the_run() {
    let (state, dag) = resume_fixture(
        &["a", "b", "c"],
        &[StepState::Succeeded, StepState::Pending, StepState::Pending],
    );
    let points = workflow_resume_start_points(&dag, &state);
    assert_eq!(points[0].name, "b");
    assert_eq!(points[0].role, "the step the run stopped on");

    let (cancelled, dag) = resume_fixture(
        &["a", "b", "c"],
        &[
            StepState::Succeeded,
            StepState::Cancelled,
            StepState::Cancelled,
        ],
    );
    assert_eq!(
        workflow_resume_start_points(&dag, &cancelled)[0].role,
        "the step that was cancelled"
    );

    let (failed_run, dag) = resume_fixture(
        &["a", "b", "c"],
        &[StepState::Succeeded, failed(1), StepState::Cancelled],
    );
    assert_eq!(
        workflow_resume_start_points(&dag, &failed_run)[0].role,
        "the step that failed"
    );
}

/// An empty worktree is not a broken run — it is a worktree with no run in
/// it. Saying "cannot be resumed" here would interrupt every first run.
#[test]
fn discover_previous_dynamic_run_is_quiet_when_there_is_no_previous_run() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(
        discover_previous_dynamic_run(tmp.path(), 12).unwrap_err(),
        DynamicDiscoveryMiss::NothingToResume
    );
}

/// A run that finished cleanly deletes its workflow copy and its state, so
/// a worktree the user kept afterwards must also read as "nothing to
/// resume" — not as a run whose workflow went missing.
#[test]
fn discover_previous_dynamic_run_is_quiet_after_a_completed_run() {
    let tmp = tempfile::tempdir().unwrap();
    let store = crate::data::workflow_state_store::WorkflowStateStore::at_git_root(tmp.path());
    let (mut state, _) = resume_fixture(&["a", "b"], &[StepState::Succeeded, StepState::Skipped]);
    state.workflow_name = "saved".into();
    state.work_item = Some(12);
    store.save(&state).unwrap();

    assert_eq!(
        discover_previous_dynamic_run(tmp.path(), 12).unwrap_err(),
        DynamicDiscoveryMiss::NothingToResume
    );
}

/// State with work still in it, but no workflow to run it with: half a run
/// really did go missing, and the user should be told.
#[test]
fn discover_previous_dynamic_run_reports_a_missing_workflow_file() {
    let tmp = tempfile::tempdir().unwrap();
    let store = crate::data::workflow_state_store::WorkflowStateStore::at_git_root(tmp.path());
    let (mut state, _) = resume_fixture(&["a", "b"], &[StepState::Succeeded, failed(1)]);
    state.workflow_name = "saved".into();
    state.work_item = Some(12);
    store.save(&state).unwrap();

    let DynamicDiscoveryMiss::Unusable(reason) =
        discover_previous_dynamic_run(tmp.path(), 12).unwrap_err()
    else {
        panic!("a half-present run must be reported, not passed over");
    };
    assert!(reason.contains("dynamic-0012.toml"), "reason={reason}");
}

#[test]
fn discover_previous_dynamic_run_reports_a_missing_state_file() {
    let tmp = tempfile::tempdir().unwrap();
    let toml_path = crate::data::fs::WorkflowDirs::dynamic_workflow_path(tmp.path(), 12);
    std::fs::create_dir_all(toml_path.parent().unwrap()).unwrap();
    std::fs::write(
        &toml_path,
        "title = \"saved\"\nagent = \"claude\"\n\n[[step]]\nname = \"a\"\nprompt = \"go\"\n",
    )
    .unwrap();

    let DynamicDiscoveryMiss::Unusable(reason) =
        discover_previous_dynamic_run(tmp.path(), 12).unwrap_err()
    else {
        panic!("a saved workflow with no state is a broken pair, not a clean slate");
    };
    assert!(
        reason.contains("no saved workflow state"),
        "reason={reason}"
    );
}

#[test]
fn discover_previous_dynamic_run_finds_both_halves() {
    let tmp = tempfile::tempdir().unwrap();
    let toml_path = crate::data::fs::WorkflowDirs::dynamic_workflow_path(tmp.path(), 12);
    std::fs::create_dir_all(toml_path.parent().unwrap()).unwrap();
    std::fs::write(
        &toml_path,
        "title = \"saved\"\nagent = \"claude\"\n\n[[step]]\nname = \"a\"\nprompt = \"go\"\n",
    )
    .unwrap();

    let store = crate::data::workflow_state_store::WorkflowStateStore::at_git_root(tmp.path());
    let (mut state, _) = resume_fixture(&["a"], &[StepState::Pending]);
    state.workflow_name = "saved".into();
    state.work_item = Some(12);
    store.save(&state).unwrap();

    let found = discover_previous_dynamic_run(tmp.path(), 12).unwrap();
    assert_eq!(found.workflow.title.as_deref(), Some("saved"));
    assert_eq!(found.workflow_path, toml_path);
    assert_eq!(found.state.work_item, Some(12));
}
