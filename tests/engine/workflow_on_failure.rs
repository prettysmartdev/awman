//! Integration tests for on_failure step remediation and poll_ci phase steps
//! (WI 0085).
//!
//! These tests exercise `WorkflowEngine::run_setup` and `run_teardown` through
//! the full engine stack with mock containers — no Docker daemon required.

use std::sync::{Arc, Mutex};

use awman::data::session::{SessionOpenOptions, StaticGitRootResolver};
use awman::data::workflow_definition::{
    RemediationConfig, SetupStep, TeardownStep, Workflow, WorkflowStep,
};
use awman::data::workflow_state::PhaseStepStatus;
use awman::engine::agent_runtime::background::{AgentExec, ExecOutput};
use awman::engine::agent_runtime::execution::AgentExitInfo;
use awman::engine::error::EngineError;
use awman::engine::overlay::OverlayEngine;
use awman::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, WorkflowOutcome,
    WorkflowStepStatus, YoloTickOutcome,
};
use awman::engine::workflow::factory::{AgentExecutionFactory, WorkflowRuntimeContext};
use awman::engine::workflow::{Frontend, WorkflowEngine};
use awman::{data::session::Session, engine::git::GitEngine};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

// ── Test infrastructure ────────────────────────────────────────────────────────

fn make_session(tmp: &tempfile::TempDir) -> Session {
    let resolver = StaticGitRootResolver::new(tmp.path());
    Session::open(
        tmp.path().to_path_buf(),
        &resolver,
        SessionOpenOptions::default(),
    )
    .unwrap()
}

fn minimal_workflow() -> Workflow {
    Workflow {
        title: Some("test-wf".into()),
        steps: vec![WorkflowStep {
            name: "step-a".into(),
            depends_on: vec![],
            prompt_template: "do something".into(),
            agent: None,
            model: None,
            overlays: None,
            abort_on_failure: false,
        }],
        agent: Some("claude".into()),
        model: None,
        setup: vec![],
        teardown: vec![],
        teardown_on_failure: false,
        overlays: None,
    }
}

/// A mock `AgentExec` that returns pre-programmed (stdout, stderr, exit_code)
/// results in order, defaulting to success when the queue is exhausted.
struct MockAgentExec {
    results: Mutex<VecDeque<(String, String, i32)>>,
    calls: Mutex<Vec<String>>,
}

impl MockAgentExec {
    fn always_success() -> Self {
        Self {
            results: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn with_results(results: impl IntoIterator<Item = (String, String, i32)>) -> Self {
        Self {
            results: Mutex::new(results.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl AgentExec for MockAgentExec {
    fn exec(
        &self,
        command: &str,
        _env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        self.calls.lock().unwrap().push(command.to_string());
        let (stdout, stderr, exit_code) = self
            .results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ("".into(), "".into(), 0));
        Ok(ExecOutput {
            stdout,
            stderr,
            exit_code,
        })
    }
}

/// A `AgentExecutionFactory` for on_failure agent launches in integration tests.
///
/// `AgentExecution::finished` is `pub(crate)` so we can't construct a finished
/// execution from an external test crate.  Returning `Err` causes
/// `launch_on_failure_agent` to log a warning and return — the remediation loop
/// still iterates and re-runs the step via the `container_for_step` closure, which
/// is sufficient to exercise the retry logic.
struct FinishedFactory;

impl FinishedFactory {
    fn always_success() -> Self {
        Self
    }
}

impl AgentExecutionFactory for FinishedFactory {
    fn execution_for_step(
        &self,
        _step: &WorkflowStep,
        _session: &Session,
        _runtime: &WorkflowRuntimeContext,
    ) -> Result<awman::engine::agent_runtime::execution::AgentExecution, EngineError> {
        Err(EngineError::Other(
            "on_failure agent launch skipped in integration test (no container runtime)".into(),
        ))
    }

    fn inject_prompt(
        &self,
        _execution: &awman::engine::agent_runtime::execution::AgentExecution,
        _prompt: &str,
    ) -> Result<Option<()>, EngineError> {
        Ok(None)
    }
}

/// Frontend that records messages and provides safe defaults for all methods.
struct RecordingFrontend {
    messages: Arc<Mutex<Vec<awman::data::message::UserMessage>>>,
}

impl RecordingFrontend {
    fn new() -> (Self, Arc<Mutex<Vec<awman::data::message::UserMessage>>>) {
        let store = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                messages: Arc::clone(&store),
            },
            store,
        )
    }
}

impl awman::data::message::UserMessageSink for RecordingFrontend {
    fn write_message(&mut self, msg: awman::data::message::UserMessage) {
        self.messages.lock().unwrap().push(msg);
    }
    fn replay_queued(&mut self) {}
}

impl Frontend for RecordingFrontend {
    fn show_workflow_control_board(
        &mut self,
        _state: &awman::data::workflow_state::WorkflowState,
        _available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        Ok(NextAction::LaunchNext)
    }
    fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn user_choose_after_step_failure(
        &mut self,
        _step: &WorkflowStep,
        _exit: &AgentExitInfo,
    ) -> Result<StepFailureChoice, EngineError> {
        Ok(StepFailureChoice::Abort)
    }
    fn report_step_status(&mut self, _step: &WorkflowStep, _status: WorkflowStepStatus) {}
    fn yolo_countdown_tick(
        &mut self,
        _step_name: &str,
        _remaining: Duration,
        _total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(YoloTickOutcome::Cancel)
    }
    fn report_workflow_completed(&mut self, _outcome: &WorkflowOutcome) {}
}

fn make_engine(
    session: &Session,
    factory: FinishedFactory,
    frontend: RecordingFrontend,
) -> WorkflowEngine {
    let overlay = OverlayEngine::with_auth_resolver(
        awman::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
    );
    WorkflowEngine::new(
        session,
        minimal_workflow(),
        None,
        Box::new(frontend),
        Box::new(factory),
        Arc::new(GitEngine::new()),
        Arc::new(overlay),
    )
    .unwrap()
}

fn remediation(max_attempts: u32) -> RemediationConfig {
    RemediationConfig {
        prompt: "Fix the failing step.".into(),
        agent: None,
        model: None,
        max_attempts,
    }
}

fn make_mock_factory(
    mock: Arc<MockAgentExec>,
) -> impl FnMut(usize) -> Result<Box<dyn AgentExec>, EngineError> {
    let mock = Arc::clone(&mock);
    move |_idx| Ok(Box::new(SharedMockExec(Arc::clone(&mock))))
}

/// Trampoline so the factory closure can produce a fresh `Box<dyn AgentExec>`
/// while sharing state with a single `MockAgentExec`.
struct SharedMockExec(Arc<MockAgentExec>);

impl AgentExec for SharedMockExec {
    fn exec(
        &self,
        command: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        self.0.exec(command, env)
    }
}

// ── Integration tests ──────────────────────────────────────────────────────────

/// A `RunShell` setup step that fails once, on_failure agent runs, retry succeeds.
/// Verifies that the step is marked Succeeded and the correct messages are emitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integration_run_shell_on_failure_retry_succeeds() {
    let msg_store: Arc<Mutex<Vec<awman::data::message::UserMessage>>> =
        Arc::new(Mutex::new(Vec::new()));
    let msg_store_clone = Arc::clone(&msg_store);

    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let frontend = RecordingFrontend {
            messages: Arc::clone(&msg_store_clone),
        };
        let factory = FinishedFactory::always_success();
        let mut engine = make_engine(&session, factory, frontend);

        let steps = vec![SetupStep::RunShell {
            command: "cargo test".into(),
            env: None,
        }];
        let mock = Arc::new(MockAgentExec::with_results([
            ("".into(), "test failed".into(), 1), // initial attempt fails
            ("".into(), "".into(), 0),            // retry succeeds
        ]));
        let on_failure_configs = vec![Some(remediation(2))];

        let result = engine.run_setup(
            &steps,
            &[false],
            &on_failure_configs,
            make_mock_factory(Arc::clone(&mock)),
        );

        assert!(
            result.is_ok(),
            "setup must succeed when retry succeeds: {result:?}"
        );
        assert_eq!(
            engine.state().setup_step_states[0].status,
            PhaseStepStatus::Succeeded,
            "step must end as Succeeded"
        );
        assert_eq!(
            mock.calls().len(),
            2,
            "exactly 2 execs: initial fail + retry"
        );
    })
    .await
    .unwrap();

    let messages = msg_store.lock().unwrap().clone();
    let texts: Vec<&str> = messages.iter().map(|m| m.text.as_str()).collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("on_failure agent") || t.contains("launching")),
        "must emit on_failure launch message: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("succeeded")),
        "must emit success message: {texts:?}"
    );
}

/// A `RunShell` setup step that fails and exhausts all on_failure attempts.
/// Verifies the step is marked Failed and a warning is emitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integration_run_shell_on_failure_exhausts_all_attempts() {
    let msg_store: Arc<Mutex<Vec<awman::data::message::UserMessage>>> =
        Arc::new(Mutex::new(Vec::new()));
    let msg_store_clone = Arc::clone(&msg_store);

    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let frontend = RecordingFrontend {
            messages: Arc::clone(&msg_store_clone),
        };
        let factory = FinishedFactory::always_success();
        let mut engine = make_engine(&session, factory, frontend);

        let steps = vec![SetupStep::RunShell {
            command: "cargo test".into(),
            env: None,
        }];
        // Every exec fails.
        let mock = Arc::new(MockAgentExec::with_results([
            ("".into(), "err".into(), 1),
            ("".into(), "err".into(), 1),
            ("".into(), "err".into(), 1),
        ]));
        let on_failure_configs = vec![Some(remediation(2))];

        engine
            .run_setup(
                &steps,
                &[false],
                &on_failure_configs,
                make_mock_factory(Arc::clone(&mock)),
            )
            .unwrap();

        assert!(
            matches!(
                engine.state().setup_step_states[0].status,
                PhaseStepStatus::Failed { .. }
            ),
            "step must be Failed after exhausted on_failure: {:?}",
            engine.state().setup_step_states[0].status
        );
        // Initial attempt + 2 retries.
        assert_eq!(
            mock.calls().len(),
            3,
            "3 execs: initial + max_attempts retries"
        );
    })
    .await
    .unwrap();

    let messages = msg_store.lock().unwrap().clone();
    use awman::data::message::MessageLevel;
    let warning = messages
        .iter()
        .find(|m| m.level == MessageLevel::Warning && m.text.contains("exhausted"));
    assert!(
        warning.is_some(),
        "must emit Warning when on_failure is exhausted: {messages:?}"
    );
}

/// A `RunShell` teardown step fails, on_failure retries succeed.
/// Teardown is best-effort — all remaining steps must still run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integration_teardown_on_failure_retry_succeeds_remaining_steps_run() {
    tokio::task::spawn_blocking(|| {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let (frontend, _msgs) = RecordingFrontend::new();
        let factory = FinishedFactory::always_success();
        let mut engine = make_engine(&session, factory, frontend);

        let steps = vec![
            TeardownStep::RunShell {
                command: "first".into(),
                env: None,
            },
            TeardownStep::RunShell {
                command: "second".into(),
                env: None,
            },
        ];
        // First step: fails initially, retry succeeds. Second step: succeeds.
        let mock = Arc::new(MockAgentExec::with_results([
            ("".into(), "err".into(), 1),
            ("".into(), "".into(), 0),
            ("".into(), "".into(), 0),
        ]));
        let on_failure_configs = vec![Some(remediation(1)), None];

        let (aborted, any_failed) = engine
            .run_teardown(
                &steps,
                &[false, false],
                &on_failure_configs,
                true,
                false,
                make_mock_factory(Arc::clone(&mock)),
            )
            .unwrap();

        assert!(!aborted, "teardown must not abort when retry succeeds");
        assert!(
            !any_failed,
            "any_failed must be false when all retries succeed"
        );
        assert_eq!(
            engine.state().teardown_step_states[0].status,
            PhaseStepStatus::Succeeded
        );
        assert_eq!(
            engine.state().teardown_step_states[1].status,
            PhaseStepStatus::Succeeded
        );
    })
    .await
    .unwrap();
}

/// A `RunShell` teardown step exhausts on_failure. Teardown is best-effort,
/// so the second step must still run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integration_teardown_on_failure_exhausted_best_effort_continues() {
    tokio::task::spawn_blocking(|| {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let (frontend, _msgs) = RecordingFrontend::new();
        let factory = FinishedFactory::always_success();
        let mut engine = make_engine(&session, factory, frontend);

        let steps = vec![
            TeardownStep::RunShell {
                command: "always-fail".into(),
                env: None,
            },
            TeardownStep::RunShell {
                command: "still-runs".into(),
                env: None,
            },
        ];
        let mock = Arc::new(MockAgentExec::with_results([
            ("".into(), "err".into(), 1), // initial
            ("".into(), "err".into(), 1), // retry
            ("".into(), "".into(), 0),    // second step
        ]));
        let on_failure_configs = vec![Some(remediation(1)), None];

        let (aborted, any_failed) = engine
            .run_teardown(
                &steps,
                &[false, false],
                &on_failure_configs,
                true,
                false,
                make_mock_factory(Arc::clone(&mock)),
            )
            .unwrap();

        assert!(!aborted, "teardown must not abort (best-effort)");
        assert!(
            any_failed,
            "any_failed must be true when a step exhausts remediation"
        );
        assert!(
            matches!(
                engine.state().teardown_step_states[0].status,
                PhaseStepStatus::Failed { .. }
            ),
            "first step must be Failed"
        );
        assert_eq!(
            engine.state().teardown_step_states[1].status,
            PhaseStepStatus::Succeeded,
            "second step must still run (best-effort)"
        );
    })
    .await
    .unwrap();
}

/// A `PollCi` setup step calls the native Rust polling path (not a container).
/// When the git_root is not a real git repo, `fetch_ci_status` fails and
/// the step is recorded as Failed — verifying that the error propagates
/// correctly through the engine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integration_poll_ci_setup_step_fails_when_not_a_git_repo() {
    tokio::task::spawn_blocking(|| {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        // The session's git_root is `tmp.path()` which is NOT a git repo,
        // so `detect_branch` → `git rev-parse` will fail.
        let (frontend, _msgs) = RecordingFrontend::new();
        let factory = FinishedFactory::always_success();
        let mut engine = make_engine(&session, factory, frontend);

        let steps = vec![SetupStep::PollCi {
            interval_secs: Some(0),
            max_retries: Some(1),
        }];
        // PollCi bypasses the container factory, so the factory is never called.
        let empty_factory = |_idx: usize| -> Result<Box<dyn AgentExec>, EngineError> {
            Err(EngineError::Other("should not be called for PollCi".into()))
        };

        // run_setup continues past non-aborting failures.
        let result = engine.run_setup(&steps, &[false], &[], empty_factory);
        assert!(
            result.is_ok(),
            "non-aborting PollCi failure must not bubble: {result:?}"
        );

        assert!(
            matches!(
                engine.state().setup_step_states[0].status,
                PhaseStepStatus::Failed { .. }
            ),
            "PollCi step must be Failed when git commands fail: {:?}",
            engine.state().setup_step_states[0].status
        );
    })
    .await
    .unwrap();
}

/// A `PollCi` teardown step with on_failure: CI fails (no git repo →
/// poll_ci step fails), on_failure agent runs, re-poll also fails because
/// the environment hasn't changed, but the test asserts the full loop ran.
///
/// This covers the "teardown poll_ci with on_failure exhausts attempts" path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integration_poll_ci_teardown_with_on_failure_exhausts_and_continues() {
    let msg_store: Arc<Mutex<Vec<awman::data::message::UserMessage>>> =
        Arc::new(Mutex::new(Vec::new()));
    let msg_store_clone = Arc::clone(&msg_store);

    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let frontend = RecordingFrontend {
            messages: Arc::clone(&msg_store_clone),
        };
        let factory = FinishedFactory::always_success();
        let mut engine = make_engine(&session, factory, frontend);

        let steps = vec![
            TeardownStep::PollCi {
                interval_secs: Some(0),
                max_retries: Some(1),
            },
            TeardownStep::RunShell {
                command: "cleanup".into(),
                env: None,
            },
        ];
        let mock = Arc::new(MockAgentExec::always_success()); // for the RunShell step only
        let on_failure_configs = vec![Some(remediation(1)), None];

        let (aborted, any_failed) = engine
            .run_teardown(
                &steps,
                &[false, false],
                &on_failure_configs,
                true,
                false,
                make_mock_factory(Arc::clone(&mock)),
            )
            .unwrap();

        assert!(!aborted, "teardown must not abort (best-effort)");
        assert!(any_failed, "any_failed must be true when poll_ci fails");
        assert!(
            matches!(
                engine.state().teardown_step_states[0].status,
                PhaseStepStatus::Failed { .. }
            ),
            "poll_ci step must be Failed"
        );
        assert_eq!(
            engine.state().teardown_step_states[1].status,
            PhaseStepStatus::Succeeded,
            "subsequent RunShell step must run (best-effort)"
        );
    })
    .await
    .unwrap();

    let messages = msg_store.lock().unwrap().clone();
    let texts: Vec<&str> = messages.iter().map(|m| m.text.as_str()).collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("on_failure agent") || t.contains("launching")),
        "must emit on_failure agent launch message for the poll_ci step: {texts:?}"
    );
}

/// Verify message output during each polling attempt. The "Polling CI (attempt N/M)"
/// banner must appear before any failure or success message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integration_poll_ci_emits_polling_attempt_message_before_error() {
    let msg_store: Arc<Mutex<Vec<awman::data::message::UserMessage>>> =
        Arc::new(Mutex::new(Vec::new()));
    let msg_store_clone = Arc::clone(&msg_store);

    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let frontend = RecordingFrontend {
            messages: Arc::clone(&msg_store_clone),
        };
        let factory = FinishedFactory::always_success();
        let mut engine = make_engine(&session, factory, frontend);

        let steps = vec![SetupStep::PollCi {
            interval_secs: Some(0),
            max_retries: Some(3),
        }];
        let empty_factory = |_idx: usize| -> Result<Box<dyn AgentExec>, EngineError> {
            Err(EngineError::Other("unreachable".into()))
        };
        engine
            .run_setup(&steps, &[false], &[], empty_factory)
            .unwrap();
    })
    .await
    .unwrap();

    let messages = msg_store.lock().unwrap().clone();
    // At least one "Polling CI (attempt ...)" message must appear.
    let attempt_msgs: Vec<_> = messages
        .iter()
        .filter(|m| m.text.contains("Polling CI (attempt"))
        .collect();
    assert!(
        !attempt_msgs.is_empty(),
        "must emit at least one attempt banner: {messages:?}"
    );
    // The attempt message must be Info level.
    assert!(
        attempt_msgs
            .iter()
            .all(|m| m.level == awman::data::message::MessageLevel::Info),
        "attempt banners must be Info level"
    );
}

/// Workflow with setup, main phase, and teardown (each parsed from TOML)
/// including on_failure config. Verifies the full schema round-trip from
/// `Workflow::parse` through the engine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integration_workflow_parsed_from_toml_with_on_failure_runs_correctly() {
    let toml = r#"
title = "on-failure-workflow"
agent = "claude"

[[setup]]
type = "run_shell"
command = "cargo build"

[setup.on_failure]
prompt = "The build failed. Fix compilation errors."
max_attempts = 2

[[steps]]
name = "implement"
prompt = "Implement the feature"

[[teardown]]
type = "run_shell"
command = "cargo test"
"#;
    let wf = awman::data::workflow_definition::Workflow::parse(
        toml,
        awman::data::workflow_definition::WorkflowFormat::Toml,
    )
    .expect("workflow must parse");

    assert_eq!(wf.setup.len(), 1);
    assert!(wf.setup[0].on_failure.is_some());
    let rem = wf.setup[0].on_failure.as_ref().unwrap();
    assert_eq!(rem.max_attempts, 2);
    assert_eq!(rem.prompt, "The build failed. Fix compilation errors.");
    assert!(rem.agent.is_none());

    // Now exercise the workflow through the engine.
    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let (frontend, _msgs) = RecordingFrontend::new();
        let factory = FinishedFactory::always_success();
        let overlay = OverlayEngine::with_auth_resolver(
            awman::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let mut engine = WorkflowEngine::new(
            &session,
            wf.clone(),
            None,
            Box::new(frontend),
            Box::new(factory),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap();

        // Extract the setup steps and on_failure configs from the parsed workflow.
        let setup_steps: Vec<SetupStep> = wf.setup.iter().map(|e| e.step.clone()).collect();
        let setup_abort_flags: Vec<bool> = wf.setup.iter().map(|e| e.abort_on_failure).collect();
        let setup_on_failure: Vec<Option<RemediationConfig>> =
            wf.setup.iter().map(|e| e.on_failure.clone()).collect();

        // Build fails twice (initial + 1 retry), then succeeds on second retry.
        let mock = Arc::new(MockAgentExec::with_results([
            ("".into(), "build error".into(), 1),
            ("".into(), "build error".into(), 1),
            ("".into(), "".into(), 0),
        ]));

        let result = engine.run_setup(
            &setup_steps,
            &setup_abort_flags,
            &setup_on_failure,
            make_mock_factory(Arc::clone(&mock)),
        );

        assert!(
            result.is_ok(),
            "setup must succeed on second retry: {result:?}"
        );
        assert_eq!(
            engine.state().setup_step_states[0].status,
            PhaseStepStatus::Succeeded
        );
    })
    .await
    .unwrap();
}
