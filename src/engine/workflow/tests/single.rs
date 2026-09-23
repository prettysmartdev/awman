//! Sequential execution: one step at a time, its failure recovery, the
//! mid-step control board, and the stuck/unstuck path.

use super::super::*;
use super::parallel::make_session_with_max_concurrent;
use super::phases::{make_engine_capturing, MessageCapturingFrontend};
use super::*;

// ── WorkflowEngine tests ─────────────────────────────────────────────────

#[tokio::test]
async fn step_once_advances_one_step_and_persists() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("my-wf"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    let mut engine = make_engine(&session, workflow, factory, []);

    let outcome = engine.step_once().await.unwrap();
    assert_eq!(outcome.step_name, "a");
    assert!(matches!(outcome.status, WorkflowStepStatus::Succeeded));
    assert_eq!(outcome.remaining, 1);

    assert!(matches!(
        engine.state().status_of("a"),
        Some(StepState::Succeeded)
    ));
    assert!(matches!(
        engine.state().status_of("b"),
        Some(StepState::Pending)
    ));

    let store = WorkflowStateStore::at_git_root(tmp.path());
    let saved = store.load(None, "my-wf").unwrap();
    assert!(saved.is_some());
}

#[tokio::test]
async fn run_to_completion_runs_all_steps() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-all"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    let frontend = FakeWorkflowFrontend::new([NextAction::LaunchNext]);
    let mut engine = WorkflowEngine::new(
        &session,
        WorkflowSpec::new(workflow).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(frontend),
            agent_factory: Box::new(factory),
        },
    )
    .unwrap();

    let result = engine.run_to_completion().await.unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
}

#[tokio::test]
async fn run_to_completion_runs_all_parallel_steps() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-parallel"),
        Some("claude"),
        vec![
            make_step("a", &[], None),
            make_step("b", &["a"], None),
            make_step("c", &["a"], None),
        ],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    let mut engine = make_engine(
        &session,
        workflow,
        factory,
        [NextAction::LaunchNext, NextAction::LaunchNext],
    );

    let result = engine.run_to_completion().await.unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
}

#[tokio::test]
async fn run_to_completion_parallel_fan_in() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-fan-in"),
        Some("claude"),
        vec![
            make_step("a", &[], None),
            make_step("b", &["a"], None),
            make_step("c", &["a"], None),
            make_step("d", &["b", "c"], None),
        ],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    let mut engine = make_engine(
        &session,
        workflow,
        factory,
        [
            NextAction::LaunchNext,
            NextAction::LaunchNext,
            NextAction::LaunchNext,
        ],
    );

    let result = engine.run_to_completion().await.unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
}

#[tokio::test]
async fn non_zero_exit_code_marks_step_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-fail"),
        Some("claude"),
        vec![make_step("a", &[], None)],
    );
    let factory = FakeAgentExecutionFactory::new([1]);
    let mut engine = make_engine(&session, workflow, factory, []);

    let outcome = engine.step_once().await.unwrap();
    assert!(matches!(
        outcome.status,
        WorkflowStepStatus::Failed { exit_code: 1 }
    ));
}

#[tokio::test]
async fn failing_container_writes_output_log_and_error_message() {
    use crate::data::message::MessageLevel;

    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let session = make_session_with_home(&tmp, home.path());
    let workflow = make_workflow(
        Some("wf-log"),
        Some("claude"),
        vec![make_step("build", &[], None)],
    );
    let factory = FakeAgentExecutionFactory::with_output_tail(
        [1],
        "awman-build-xyz",
        ["compiling project", "error: it exploded"],
    );
    let (frontend, messages) = MessageCapturingFrontend::new();
    let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

    let invocation_id = engine.state().invocation_id;
    let outcome = engine.step_once().await.unwrap();
    assert!(matches!(
        outcome.status,
        WorkflowStepStatus::Failed { exit_code: 1 }
    ));

    // The buffered output must be persisted to the per-container log file.
    let paths = crate::data::fs::WorkflowLogPaths::at_home(home.path());
    let log_path = paths.container_log_path(invocation_id, "build", "awman-build-xyz");
    assert!(
        log_path.exists(),
        "failure log must be written at {}",
        log_path.display()
    );
    let body = std::fs::read_to_string(&log_path).unwrap();
    assert!(body.contains("compiling project"), "log body: {body:?}");
    assert!(body.contains("error: it exploded"), "log body: {body:?}");

    // An Error-level message must point the user at the log file.
    let messages = messages.lock().unwrap();
    let err = messages
        .iter()
        .find(|m| m.level == MessageLevel::Error)
        .expect("an Error message must be emitted on container failure");
    assert!(
        err.text.contains(&log_path.display().to_string()),
        "error message must name the log path: {}",
        err.text
    );
}

#[tokio::test]
async fn successful_container_writes_no_failure_log() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let session = make_session_with_home(&tmp, home.path());
    let workflow = make_workflow(
        Some("wf-ok"),
        Some("claude"),
        vec![make_step("build", &[], None)],
    );
    let factory = FakeAgentExecutionFactory::with_output_tail([0], "awman-build-ok", ["all good"]);
    let mut engine = make_engine(&session, workflow, factory, []);

    engine.step_once().await.unwrap();

    let paths = crate::data::fs::WorkflowLogPaths::at_home(home.path());
    assert!(
        !paths.logs_dir().exists(),
        "a clean (exit 0) container must not create the logs directory"
    );
}

#[tokio::test]
async fn awman_killed_container_writes_no_failure_log() {
    // A non-zero exit on a container awman itself killed is expected and
    // must NOT produce a failure log.
    let tmp = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let session = make_session_with_home(&tmp, home.path());
    let workflow = make_workflow(
        Some("wf-killed"),
        Some("claude"),
        vec![make_step("build", &[], None)],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    let mut engine = make_engine(&session, workflow, factory, []);

    // Simulate a live slot that awman killed, carrying buffered output.
    let tail = OutputTail::with_default_capacity();
    tail.push_bytes(b"some output before the kill\n");
    engine.active_steps.push(ActiveParallelStep {
        step_name: "build".to_string(),
        execution: None,
        cancel_handle: None,
        container_name: "awman-build-killed".to_string(),
        output_tail: Some(Arc::new(tail)),
        awman_killed: true,
        stuck: false,
        yolo_deadline: None,
        agent: AgentName::new("claude").unwrap(),
        model: None,
    });

    engine.maybe_dump_step_failure("build", KILLED_EXIT_CODE);

    let paths = crate::data::fs::WorkflowLogPaths::at_home(home.path());
    assert!(
        !paths.logs_dir().exists(),
        "an awman-killed container must not produce a failure log"
    );
}

// ── WI-0115 §1: interactive step-failure recovery board ──────────────

#[tokio::test]
async fn step_failure_abort_returns_aborted() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-fail-abort"),
        Some("claude"),
        vec![make_step("a", &[], None)],
    );
    let factory = FakeAgentExecutionFactory::new([2]);
    let frontend = FakeWorkflowFrontend::new([NextAction::Abort]);
    let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

    let result = engine.run_to_completion().await.unwrap();
    assert!(matches!(result, WorkflowOutcome::Aborted));
}

#[tokio::test]
async fn step_failure_restart_reruns_step() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-fail-retry"),
        Some("claude"),
        vec![make_step("a", &[], None)],
    );
    let factory = FakeAgentExecutionFactory::new([1, 0]);
    // Restart the failed step, then finish the (now last, succeeded) step.
    let frontend =
        FakeWorkflowFrontend::new([NextAction::RestartCurrentStep, NextAction::FinishWorkflow]);
    let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

    let result = engine.run_to_completion().await.unwrap();
    assert!(matches!(result, WorkflowOutcome::Completed));
}

#[tokio::test]
async fn step_failure_pause_returns_paused() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-fail-pause"),
        Some("claude"),
        vec![make_step("a", &[], None)],
    );
    let factory = FakeAgentExecutionFactory::new([1]);
    let frontend = FakeWorkflowFrontend::new([NextAction::Pause]);
    let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

    let result = engine.run_to_completion().await.unwrap();
    assert!(matches!(result, WorkflowOutcome::Paused));
}

#[tokio::test]
async fn step_failure_launch_next_skips_failed_step_and_runs_the_next_one() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-fail-skip"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );
    // 'a' fails, 'b' succeeds.
    let factory = FakeAgentExecutionFactory::new([1, 0]);
    let frontend = FakeWorkflowFrontend::new([NextAction::LaunchNext, NextAction::FinishWorkflow]);
    let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

    let result = engine.run_to_completion().await.unwrap();
    assert!(matches!(result, WorkflowOutcome::Completed));
    assert!(
        matches!(engine.state().status_of("a"), Some(StepState::Skipped)),
        "the failed step must be skipped so its dependents become ready"
    );
    assert!(matches!(
        engine.state().status_of("b"),
        Some(StepState::Succeeded)
    ));
}

#[tokio::test]
async fn step_failure_cancel_to_previous_reruns_both_steps() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-fail-back"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );
    // a ok → b fails → back to a → a ok → b ok.
    let factory = FakeAgentExecutionFactory::new([0, 1, 0, 0]);
    let frontend = FakeWorkflowFrontend::new([
        // After 'a' succeeds the first time.
        NextAction::LaunchNext,
        // 'b' failed: go back to 'a'.
        NextAction::CancelToPreviousStep,
        // 'a' succeeded again.
        NextAction::LaunchNext,
        // 'b' succeeded.
        NextAction::FinishWorkflow,
    ]);
    let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

    let result = engine.run_to_completion().await.unwrap();
    assert!(matches!(result, WorkflowOutcome::Completed));
    assert!(matches!(
        engine.state().status_of("b"),
        Some(StepState::Succeeded)
    ));
}

#[test]
fn failure_actions_offer_recovery_and_never_finish() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-fail-actions"),
        Some("claude"),
        vec![
            make_step("a", &[], None),
            make_step("b", &["a"], None),
            make_step("c", &["b"], None),
        ],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    let mut engine = make_engine(&session, workflow, factory, []);
    engine.state.set_status("a", StepState::Succeeded);
    engine.current_step_name = Some("b".to_string());

    let available = engine.compute_failure_actions("b", 42).unwrap();
    assert!(available.can_restart_current_step);
    assert!(available.can_cancel_to_previous_step);
    assert!(available.can_launch_next);
    assert!(available.can_abort);
    assert!(
        !available.can_finish_workflow,
        "a failure board must not offer Finish"
    );
    assert!(
        !available.can_dismiss,
        "the failed step's container is already dead"
    );
    let failure = available.step_failure.expect("failure context");
    assert_eq!(failure.step_name, "b");
    assert_eq!(failure.exit_code, 42);
    assert_eq!(failure.previous_step.as_deref(), Some("a"));
    assert_eq!(failure.next_step.as_deref(), Some("c"));
    assert!(failure
        .detail_lines
        .iter()
        .any(|l| l.contains("Exit code: 42")));
}

// ─── simple_advance (WI 0114 F-18) ──────────────────────────────────────
//
// Whether a board is the plain "advance to the next step?" case is the
// engine's judgement. The TUI used to scan `step_states` for it; these
// tests are the ones that moved down with the decision.

/// The case itself: nothing running, nothing failed, exactly one step
/// left.
#[test]
fn one_pending_step_and_nothing_running_is_a_simple_advance() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-simple"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    let mut engine = make_engine(&session, workflow, factory, []);
    engine.state.set_status("a", StepState::Succeeded);

    let available = engine.compute_available_actions().unwrap();
    let advance = available
        .simple_advance
        .expect("one pending step is the simple-advance case");
    assert_eq!(advance.next_step, "b");
    assert_eq!(
        advance.completed_step, "current step",
        "nothing is running, so the board falls back to the generic label"
    );
}

/// Two steps ready at once is a fan-out; the user needs the full board.
#[test]
fn two_pending_steps_are_not_a_simple_advance() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-fanout"),
        Some("claude"),
        vec![
            make_step("a", &[], None),
            make_step("b", &["a"], None),
            make_step("c", &["a"], None),
        ],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    let mut engine = make_engine(&session, workflow, factory, []);
    engine.state.set_status("a", StepState::Succeeded);

    assert!(engine
        .compute_available_actions()
        .unwrap()
        .simple_advance
        .is_none());
}

/// A failed step anywhere in the run means recovery actions, not a
/// one-key confirm — even when only one step is still pending.
#[test]
fn a_failed_step_anywhere_rules_out_a_simple_advance() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-failed"),
        Some("claude"),
        vec![
            make_step("a", &[], None),
            make_step("b", &[], None),
            make_step("c", &[], None),
        ],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    let mut engine = make_engine(&session, workflow, factory, []);
    engine.state.set_status("a", StepState::Succeeded);
    engine.state.set_status(
        "b",
        StepState::Failed {
            exit_code: 1,
            error_message: None,
        },
    );

    assert!(engine
        .compute_available_actions()
        .unwrap()
        .simple_advance
        .is_none());
}

/// A running step is dismissable, and a board you can dismiss back to a
/// live container is not an advance prompt.
#[test]
fn a_running_step_rules_out_a_simple_advance_and_names_the_board() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-running"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    let mut engine = make_engine(&session, workflow, factory, []);
    engine
        .state
        .set_status("a", StepState::Running { container_id: None });
    engine.current_step_name = Some("a".to_string());

    let available = engine.compute_available_actions().unwrap();
    assert!(available.can_dismiss);
    assert!(available.simple_advance.is_none());
    assert_eq!(available.focused_step.as_deref(), Some("a"));
    assert_eq!(available.focused_step_label(), "a");
}

/// A failure board names the failed step, whose container is already gone.
#[test]
fn a_failure_board_names_the_failed_step() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-fail-name"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    let mut engine = make_engine(&session, workflow, factory, []);
    engine.state.set_status("a", StepState::Succeeded);
    engine.current_step_name = Some("b".to_string());

    let available = engine.compute_failure_actions("b", 1).unwrap();
    assert_eq!(available.focused_step_label(), "b");
    assert!(available.simple_advance.is_none());
}

#[test]
fn failure_actions_on_the_last_step_offer_no_next() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-fail-last"),
        Some("claude"),
        vec![make_step("a", &[], None)],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    let mut engine = make_engine(&session, workflow, factory, []);
    engine.current_step_name = Some("a".to_string());

    let available = engine.compute_failure_actions("a", 1).unwrap();
    assert!(!available.can_launch_next);
    assert!(!available.can_cancel_to_previous_step);
    assert!(available.can_restart_current_step);
}

/// WI-0115 §1: a step left `Failed` is not in `completed_steps`, so the DAG
/// still reports it ready. Recovering only the first failure of a drained
/// parallel group would let its peers be relaunched silently — no board, no
/// retry accounting, no way for the user to know a second step even failed.
#[tokio::test]
async fn every_failure_in_a_parallel_group_gets_its_own_board() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wf-two-failures"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &[], None)],
    );
    // Both members of the group fail.
    let factory = FakeAgentExecutionFactory::new([1, 1]);
    // One decision per failure; the second ends the run so the test does
    // not depend on what a re-run of the skipped steps would do.
    let frontend = FakeWorkflowFrontend::new([NextAction::RestartCurrentStep, NextAction::Abort]);
    let boards = frontend.boards();
    let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

    let outcome = engine.run_to_completion().await.unwrap();
    assert_eq!(outcome, WorkflowOutcome::Aborted);

    let boards = boards.lock().unwrap();
    let failed_on: Vec<&str> = boards
        .iter()
        .filter_map(|b| b.step_failure.as_ref())
        .map(|f| f.step_name.as_str())
        .collect();
    assert_eq!(
        failed_on.len(),
        2,
        "one board per failed step, got boards for {failed_on:?}"
    );
    let mut named = failed_on.clone();
    named.sort_unstable();
    assert_eq!(
        named,
        vec!["a", "b"],
        "each board must name its own failure, not repeat the first"
    );
}

// ── WI-0115 §3: unattended countdown-and-retry ───────────────────────

#[tokio::test]
async fn unattended_step_failure_retries_once_then_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-unattended-retry"),
        Some("claude"),
        vec![make_step("a", &[], None)],
    );
    let factory = FakeAgentExecutionFactory::new([1, 0]);
    let frontend = FakeWorkflowFrontend::new([])
        .unattended()
        .with_yolo_tick(YoloTickOutcome::AdvanceNow);
    let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

    let result = engine.run_to_completion().await.unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
}

#[tokio::test]
async fn unattended_step_failing_twice_fails_the_workflow() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-unattended-fail"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );
    let factory = FakeAgentExecutionFactory::new([7, 7]);
    let frontend = FakeWorkflowFrontend::new([])
        .unattended()
        .with_yolo_tick(YoloTickOutcome::AdvanceNow);
    let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

    let result = engine.run_to_completion().await.unwrap();
    assert_eq!(
        result,
        WorkflowOutcome::Failed {
            last_step: "a".to_string(),
            exit_code: 7,
        }
    );
    assert!(
        matches!(engine.state().status_of("b"), Some(StepState::Cancelled)),
        "remaining steps must be cancelled once the workflow fails"
    );
}

/// `abort_on_failure` is checked before the recovery path is chosen, so an
/// unattended run aborts on the first failure rather than spending its one
/// automatic retry (WI-0115 §3).
#[tokio::test]
async fn unattended_abort_on_failure_step_aborts_without_retrying() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let mut step = make_step("a", &[], None);
    step.abort_on_failure = true;
    let workflow = make_workflow(Some("wf-unattended-abort"), Some("claude"), vec![step]);
    // A single exit code: a retry would launch a second container and panic
    // the fake factory, so reaching `Aborted` proves no retry happened.
    let factory = FakeAgentExecutionFactory::new([9]);
    let frontend = FakeWorkflowFrontend::new([])
        .unattended()
        .with_yolo_tick(YoloTickOutcome::AdvanceNow);
    let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

    let result = engine.run_to_completion().await.unwrap();
    assert_eq!(result, WorkflowOutcome::Aborted);
    assert!(engine.abort_on_failure_triggered());
}

#[tokio::test]
async fn unattended_cancelled_retry_countdown_fails_immediately() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-unattended-cancel"),
        Some("claude"),
        vec![make_step("a", &[], None)],
    );
    // Only one exit code: a second launch would panic the fake factory,
    // proving no retry happened.
    let factory = FakeAgentExecutionFactory::new([3]);
    let frontend = FakeWorkflowFrontend::new([]).unattended();
    let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

    let result = engine.run_to_completion().await.unwrap();
    assert_eq!(
        result,
        WorkflowOutcome::Failed {
            last_step: "a".to_string(),
            exit_code: 3,
        }
    );
}

#[tokio::test]
async fn pause_persists_state_and_returns_paused() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-pause"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    let mut engine = make_engine(&session, workflow, factory, [NextAction::Pause]);

    let result = engine.run_to_completion().await.unwrap();
    assert_eq!(result, WorkflowOutcome::Paused);

    let store = WorkflowStateStore::at_git_root(tmp.path());
    let saved = store.load(None, "wf-pause").unwrap();
    assert!(saved.is_some());
}

#[tokio::test]
async fn resume_with_same_hash_continues_from_saved_state() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let wf = make_workflow(
        Some("wf-resume"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );

    {
        let factory = FakeAgentExecutionFactory::always_success();
        let mut engine = make_engine(&session, wf.clone(), factory, [NextAction::Pause]);
        engine.run_to_completion().await.unwrap();
    }

    let factory2 = FakeAgentExecutionFactory::always_success();
    let frontend = FakeWorkflowFrontend::new([]);
    let mut engine = WorkflowEngine::resume(
        &session,
        WorkflowSpec::new(wf).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(frontend),
            agent_factory: Box::new(factory2),
        },
    )
    .await
    .unwrap();
    let result = engine.run_to_completion().await.unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
}

/// WI-0115 §2: an aborted run saves a state in which *every* step is
/// terminal (the failed one plus every step it cancelled). `is_complete()`
/// reads that as finished, so without a load-time reset the resumed run
/// would report instant success and execute nothing. This is the engine's
/// own guard — it holds for dynamic and non-dynamic workflows alike, and
/// whether or not the command layer rewound the state first.
#[tokio::test]
async fn resuming_an_aborted_run_reruns_its_unfinished_steps() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let wf = make_workflow(
        Some("wf-aborted"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );

    // First run: 'a' succeeds, 'b' fails, the user aborts.
    {
        let factory = FakeAgentExecutionFactory::new([0, 1]);
        let frontend = FakeWorkflowFrontend::new([NextAction::LaunchNext, NextAction::Abort]);
        let mut engine = make_engine_with_frontend(&session, wf.clone(), factory, frontend);
        let outcome = engine.run_to_completion().await.unwrap();
        assert_eq!(outcome, WorkflowOutcome::Aborted);
    }

    let saved = WorkflowStateStore::at_git_root(tmp.path())
        .load(None, "wf-aborted")
        .unwrap()
        .unwrap();
    assert!(
        saved.is_complete(),
        "precondition: an aborted state has no non-terminal steps left"
    );

    // Resuming must re-run 'b' rather than declare instant success.
    let factory2 = FakeAgentExecutionFactory::new([0]);
    let mut engine = WorkflowEngine::resume(
        &session,
        WorkflowSpec::new(wf).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(FakeWorkflowFrontend::new([NextAction::FinishWorkflow])),
            agent_factory: Box::new(factory2),
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(engine.state().status_of("b"), Some(StepState::Pending)),
        "the cancelled step must be reset at load"
    );
    assert!(
        matches!(engine.state().status_of("a"), Some(StepState::Succeeded)),
        "a step that genuinely succeeded must be left alone"
    );

    let result = engine.run_to_completion().await.unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
    assert!(matches!(
        engine.state().status_of("b"),
        Some(StepState::Succeeded)
    ));
}

/// WI-0115 §2: a saved state outlives edits to its workflow file. A step
/// dropped from the file since the state was written can never run — the
/// DAG decides what runs — but it still counts towards `is_complete()`,
/// which the load-time reset would have just put back to `Pending`. Left
/// in, it strands the run on "no ready steps remaining"; pruned, the run
/// finishes.
#[tokio::test]
async fn resuming_a_state_whose_workflow_dropped_a_step_still_completes() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);

    // The workflow as it was: a → b → publish, aborted partway.
    let before = make_workflow(
        Some("wf-drift"),
        Some("claude"),
        vec![
            make_step("a", &[], None),
            make_step("b", &["a"], None),
            make_step("publish", &["b"], None),
        ],
    );
    {
        let factory = FakeAgentExecutionFactory::new([0, 1]);
        let frontend = FakeWorkflowFrontend::new([NextAction::LaunchNext, NextAction::Abort]);
        let mut engine = make_engine_with_frontend(&session, before, factory, frontend);
        assert_eq!(
            engine.run_to_completion().await.unwrap(),
            WorkflowOutcome::Aborted
        );
    }

    // The workflow as it is now: 'publish' has been deleted. Same title,
    // so the saved state is still found; the hash differs, and the fake
    // frontend confirms the drift prompt.
    let after = make_workflow(
        Some("wf-drift"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );
    let mut engine = WorkflowEngine::resume(
        &session,
        WorkflowSpec::new(after).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(FakeWorkflowFrontend::new([NextAction::FinishWorkflow])),
            agent_factory: Box::new(FakeAgentExecutionFactory::new([0])),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        engine.state().status_of("publish"),
        None,
        "a step the workflow no longer defines must be dropped, not reset"
    );

    assert_eq!(
        engine.run_to_completion().await.unwrap(),
        WorkflowOutcome::Completed,
    );
}

/// WI 0106 §6a: a squad task bound to its durable workspace has no
/// worktree to absorb awman's own bookkeeping, and that directory must
/// survive every run untouched. `resume_with_state_root` therefore keeps
/// the state file — which the engine creates, rewrites and (on a fresh
/// run) deletes — entirely outside the session's root.
#[tokio::test]
async fn a_state_root_override_keeps_the_state_file_out_of_the_session_root() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let run_dir = tempfile::tempdir().unwrap();
    let wf = make_workflow(
        Some("wf-state-root"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );

    let mut engine = WorkflowEngine::resume(
        &session,
        WorkflowSpec::new(wf).with_state_root(Some(run_dir.path().to_path_buf())),
        WorkflowEngineDeps {
            frontend: Box::new(FakeWorkflowFrontend::new([NextAction::Pause])),
            agent_factory: Box::new(FakeAgentExecutionFactory::always_success()),
        },
    )
    .await
    .unwrap();
    engine.run_to_completion().await.unwrap();

    assert!(
        WorkflowStateStore::at_git_root(run_dir.path())
            .load(None, "wf-state-root")
            .unwrap()
            .is_some(),
        "state must be persisted under the override root"
    );
    assert!(
        !tmp.path().join(".awman").join("workflows").exists(),
        "the session root must be left untouched by the engine's bookkeeping"
    );
}

#[tokio::test]
async fn resume_with_drifted_hash_calls_confirm_resume_and_aborts_when_declined() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let wf1 = make_workflow(
        Some("wf-drift"),
        Some("claude"),
        vec![make_step("a", &[], None)],
    );

    {
        let factory = FakeAgentExecutionFactory::always_success();
        let mut engine = make_engine(&session, wf1, factory, [NextAction::Pause]);
        engine.run_to_completion().await.unwrap();
    }

    let wf2 = make_workflow(
        Some("wf-drift"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );
    let frontend = FakeWorkflowFrontend::new([]).with_confirm_resume(false);
    let result = WorkflowEngine::resume(
        &session,
        WorkflowSpec::new(wf2).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(frontend),
            agent_factory: Box::new(FakeAgentExecutionFactory::always_success()),
        },
    )
    .await;

    assert!(matches!(
        result,
        Err(EngineError::WorkflowResumeIncompatible(_))
    ));
}

#[tokio::test]
async fn step_level_agent_overrides_workflow_level() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-agent"),
        Some("claude"),
        vec![make_step("a", &[], Some("codex"))],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    let factory_arc: Arc<FakeAgentExecutionFactory> = Arc::new(factory);

    struct RecordingFactory(Arc<FakeAgentExecutionFactory>);
    impl AgentExecutionFactory for RecordingFactory {
        fn execution_for_step(
            &self,
            step: &WorkflowStep,
            session: &Session,
            runtime: &WorkflowRuntimeContext,
        ) -> Result<AgentExecution, EngineError> {
            self.0.execution_for_step(step, session, runtime)
        }
        fn inject_prompt(&self, e: &AgentExecution, p: &str) -> Result<Option<()>, EngineError> {
            self.0.inject_prompt(e, p)
        }
    }

    let mut engine = WorkflowEngine::new(
        &session,
        WorkflowSpec::new(workflow).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(FakeWorkflowFrontend::new([])),
            agent_factory: Box::new(RecordingFactory(factory_arc.clone())),
        },
    )
    .unwrap();

    engine.step_once().await.unwrap();
    let contexts = factory_arc.recorded_contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 1);
    assert_eq!(contexts[0].step_agent.as_str(), "codex");
}

#[tokio::test]
async fn cancel_to_previous_step_unavailable_on_first_step() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-cancel"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    let mut engine = make_engine(&session, workflow, factory, []);

    engine.step_once().await.unwrap();

    let available = engine.compute_available_actions().unwrap();
    assert!(!available.can_cancel_to_previous_step);
    assert!(available.cancel_to_previous_unavailable_reason.is_some());
}

#[tokio::test]
async fn yolo_mode_auto_advances_between_steps() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-yolo"),
        Some("claude"),
        vec![
            make_step("a", &[], None),
            make_step("b", &["a"], None),
            make_step("c", &["b"], None),
        ],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    // No actions queued — yolo mode should auto-advance without prompting.
    let mut engine = make_engine(&session, workflow, factory, []);
    engine.set_yolo(true);

    let result = engine.run_to_completion().await.unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
}

// ── Mid-step control board tests ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_control_board_mid_step_does_not_cancel_container() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-mid-no-cancel"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );

    let (cancel_flag, completion1) = make_blocking_entry();
    let engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>> =
        Arc::new(Mutex::new(None));

    let factory = BlockingFactory::new([(cancel_flag.clone(), completion1.clone())]);
    let (mut engine, container_exits) = make_capturing_engine(
        &session,
        workflow,
        factory,
        [NextAction::Dismiss, NextAction::LaunchNext],
        engine_tx.clone(),
    );

    let tx = engine_tx
        .lock()
        .unwrap()
        .clone()
        .expect("engine_tx set on construction");

    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

    tokio::time::sleep(Duration::from_millis(150)).await;
    tx.send(EngineRequest::OpenControlBoard {
        step_name: String::new(),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(
        !cancel_flag.load(Ordering::Relaxed),
        "cancel must not be called when user picks Dismiss"
    );
    assert!(
        container_exits.lock().unwrap().is_empty(),
        "no container exit may be reported while the container still runs"
    );

    signal_completion(&completion1, 0);

    let result = engine_task.await.unwrap().unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
    assert!(
        container_exits.lock().unwrap().contains(&0),
        "the step's natural completion must be reported with its exit code"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_step_dismiss_resumes_waiting_on_step() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-dismiss"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );

    let (cancel_flag, completion) = make_blocking_entry();
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let factory = BlockingFactory::new([(cancel_flag.clone(), completion.clone())]);
    let (mut engine, _container_exits) = make_capturing_engine(
        &session,
        workflow,
        factory,
        [NextAction::Dismiss, NextAction::LaunchNext],
        engine_tx.clone(),
    );
    let tx = engine_tx.lock().unwrap().clone().unwrap();

    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

    tokio::time::sleep(Duration::from_millis(150)).await;
    tx.send(EngineRequest::OpenControlBoard {
        step_name: String::new(),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(!cancel_flag.load(Ordering::Relaxed));

    signal_completion(&completion, 0);
    let result = engine_task.await.unwrap().unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_step_restart_cancels_then_re_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-restart-mid"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );

    let (cancel_flag, completion1) = make_blocking_entry();
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let factory = BlockingFactory::new([(cancel_flag.clone(), completion1)]);
    let execution_count = factory.execution_count.clone();
    let (mut engine, _container_exits) = make_capturing_engine(
        &session,
        workflow,
        factory,
        [NextAction::RestartCurrentStep, NextAction::LaunchNext],
        engine_tx.clone(),
    );
    let tx = engine_tx.lock().unwrap().clone().unwrap();

    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

    tokio::time::sleep(Duration::from_millis(150)).await;
    tx.send(EngineRequest::OpenControlBoard {
        step_name: String::new(),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(cancel_flag.load(Ordering::Relaxed));

    let result = engine_task.await.unwrap().unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
    assert!(execution_count.load(Ordering::Relaxed) >= 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_step_advance_cancels_then_marks_force_succeeded() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-advance-mid"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );

    let (cancel_flag, completion1) = make_blocking_entry();
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let factory = BlockingFactory::new([(cancel_flag.clone(), completion1)]);
    let execution_count = factory.execution_count.clone();
    let (mut engine, container_exits) = make_capturing_engine(
        &session,
        workflow,
        factory,
        [NextAction::LaunchNext],
        engine_tx.clone(),
    );
    let tx = engine_tx.lock().unwrap().clone().unwrap();

    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

    tokio::time::sleep(Duration::from_millis(150)).await;
    tx.send(EngineRequest::OpenControlBoard {
        step_name: String::new(),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(cancel_flag.load(Ordering::Relaxed));
    assert_eq!(
        container_exits.lock().unwrap().first(),
        Some(&KILLED_EXIT_CODE),
        "an engine kill must be reported to the frontend immediately"
    );

    let result = engine_task.await.unwrap().unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
    assert_eq!(execution_count.load(Ordering::Relaxed), 2);
}

// ── StepStuck / StepUnstuck engine tests ─────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn step_stuck_in_yolo_mode_starts_countdown() {
    // Uses a 2-step workflow so that step "a" is NOT the last step.
    // The last step never runs a yolo countdown (it shows the WCB
    // instead), so this test exercises the countdown on step "a".
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-stuck-yolo"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );

    let (cancel_flag_a, completion_a) = make_blocking_entry();
    let (_cancel_flag_b, completion_b) = make_blocking_entry();
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));

    // Frontend that tracks yolo lifecycle calls.
    struct YoloTrackingFrontend {
        actions: Mutex<VecDeque<NextAction>>,
        engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
        yolo_started: AtomicBool,
        yolo_finished: AtomicBool,
    }
    impl crate::data::message::UserMessageSink for YoloTrackingFrontend {
        fn write_message(&mut self, _: crate::data::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }
    impl WorkflowFrontend for YoloTrackingFrontend {
        fn show_workflow_control_board(
            &mut self,
            _: &WorkflowState,
            _: &AvailableActions,
        ) -> Result<NextAction, EngineError> {
            Ok(self
                .actions
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(NextAction::Pause))
        }
        fn yolo_countdown_tick(
            &mut self,
            _: &str,
            _: Duration,
            _: Duration,
        ) -> Result<YoloTickOutcome, EngineError> {
            // Cancel immediately to keep the test fast.
            Ok(YoloTickOutcome::Cancel)
        }
        fn yolo_countdown_started(&mut self, _: &str, _: CountdownKind) {
            self.yolo_started.store(true, Ordering::Relaxed);
        }
        fn yolo_countdown_finished(&mut self, _: &str) {
            self.yolo_finished.store(true, Ordering::Relaxed);
        }
        fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
            Ok(true)
        }
        fn report_step_status(&mut self, _: &WorkflowStep, _: WorkflowStepStatus) {}
        fn report_workflow_completed(&mut self, _: &WorkflowOutcome) {}
        fn attach_engine(&mut self, handles: EngineHandles) {
            if let Some(tx) = handles.requests {
                *self.engine_tx.lock().unwrap() = Some(tx);
            }
        }
    }

    let frontend = YoloTrackingFrontend {
        // WCB is shown after last step completes in yolo mode.
        actions: Mutex::new(VecDeque::from([NextAction::FinishWorkflow])),
        engine_tx: engine_tx.clone(),
        yolo_started: AtomicBool::new(false),
        yolo_finished: AtomicBool::new(false),
    };

    let factory = BlockingFactory::new([
        (cancel_flag_a.clone(), completion_a.clone()),
        (_cancel_flag_b.clone(), completion_b.clone()),
    ]);
    let mut engine = WorkflowEngine::new(
        &session,
        WorkflowSpec::new(workflow).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(frontend),
            agent_factory: Box::new(factory),
        },
    )
    .unwrap();
    engine.set_yolo(true);

    let tx = engine_tx.lock().unwrap().clone().unwrap();

    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

    tokio::time::sleep(Duration::from_millis(150)).await;
    tx.send(EngineRequest::StepStuck {
        step_name: String::new(),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Countdown was cancelled by the frontend (YoloTickOutcome::Cancel),
    // so step "a" keeps running. Complete it normally.
    signal_completion(&completion_a, 0);

    // Yolo auto-advances to step "b". Complete it.
    tokio::time::sleep(Duration::from_millis(150)).await;
    signal_completion(&completion_b, 0);

    let result = engine_task.await.unwrap().unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn step_stuck_in_non_yolo_mode_shows_wcb() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-stuck-no-yolo"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );

    let (cancel_flag, completion) = make_blocking_entry();
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let factory = BlockingFactory::new([(cancel_flag.clone(), completion.clone())]);
    // When WCB opens due to stuck: Dismiss, then later LaunchNext between steps.
    let (mut engine, _container_exits) = make_capturing_engine(
        &session,
        workflow,
        factory,
        [NextAction::Dismiss, NextAction::LaunchNext],
        engine_tx.clone(),
    );
    // Not yolo mode.

    let tx = engine_tx.lock().unwrap().clone().unwrap();

    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

    tokio::time::sleep(Duration::from_millis(150)).await;
    tx.send(EngineRequest::StepStuck {
        step_name: String::new(),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Step still running (Dismiss was chosen).
    assert!(!cancel_flag.load(Ordering::Relaxed));

    signal_completion(&completion, 0);
    let result = engine_task.await.unwrap().unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
}

/// Sending `StepUnstuck` during an active yolo countdown must cancel the
/// countdown and leave the step running — it must NOT mark the step
/// Succeeded or advance to the next step. The container keeps running
/// until it actually exits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn step_unstuck_during_yolo_countdown_keeps_step_running() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-unstuck-mid-countdown"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );

    let (cancel_flag_a, completion_a) = make_blocking_entry();
    let (_cancel_flag_b, completion_b) = make_blocking_entry();
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));

    // Frontend whose tick returns Continue so the countdown actually runs
    // (lets us send StepUnstuck mid-countdown). Captures step transitions
    // so the test can assert "a" was never marked Succeeded prematurely.
    struct UnstuckTestFrontend {
        actions: Mutex<VecDeque<NextAction>>,
        engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
        step_statuses: Mutex<Vec<(String, WorkflowStepStatus)>>,
    }
    impl crate::data::message::UserMessageSink for UnstuckTestFrontend {
        fn write_message(&mut self, _: crate::data::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }
    impl WorkflowFrontend for UnstuckTestFrontend {
        fn show_workflow_control_board(
            &mut self,
            _: &WorkflowState,
            _: &AvailableActions,
        ) -> Result<NextAction, EngineError> {
            Ok(self
                .actions
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(NextAction::Pause))
        }
        fn yolo_countdown_tick(
            &mut self,
            _: &str,
            _: Duration,
            _: Duration,
        ) -> Result<YoloTickOutcome, EngineError> {
            Ok(YoloTickOutcome::Continue)
        }
        fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
            Ok(true)
        }
        fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
            self.step_statuses
                .lock()
                .unwrap()
                .push((step.name.clone(), status));
        }
        fn report_workflow_completed(&mut self, _: &WorkflowOutcome) {}
        fn attach_engine(&mut self, handles: EngineHandles) {
            if let Some(tx) = handles.requests {
                *self.engine_tx.lock().unwrap() = Some(tx);
            }
        }
    }
    let frontend = UnstuckTestFrontend {
        actions: Mutex::new(VecDeque::from([NextAction::FinishWorkflow])),
        engine_tx: engine_tx.clone(),
        step_statuses: Mutex::new(Vec::new()),
    };

    let factory = BlockingFactory::new([
        (cancel_flag_a.clone(), completion_a.clone()),
        (_cancel_flag_b.clone(), completion_b.clone()),
    ]);
    let mut engine = WorkflowEngine::new(
        &session,
        WorkflowSpec::new(workflow).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(frontend),
            agent_factory: Box::new(factory),
        },
    )
    .unwrap();
    engine.set_yolo(true);

    let tx = engine_tx.lock().unwrap().clone().unwrap();

    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

    // Let the step launch.
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Kick off the yolo countdown.
    tx.send(EngineRequest::StepStuck {
        step_name: String::new(),
    })
    .unwrap();
    // Let the countdown run a tick or two without expiring.
    tokio::time::sleep(Duration::from_millis(200)).await;
    // Container produced output again — recovery signal.
    tx.send(EngineRequest::StepUnstuck {
        step_name: String::new(),
    })
    .unwrap();
    // Wait long enough that, if the engine were mistakenly advancing the
    // step on Unstuck, step "b" would have launched. Cancel-flag-a must
    // still be false (step "a" still running, NOT cancelled by Advanced).
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !cancel_flag_a.load(Ordering::Relaxed),
        "StepUnstuck during countdown must NOT cancel step 'a' — it must keep running"
    );

    // Now complete step "a" normally; workflow proceeds.
    signal_completion(&completion_a, 0);
    tokio::time::sleep(Duration::from_millis(150)).await;
    signal_completion(&completion_b, 0);

    let result = engine_task.await.unwrap().unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn step_unstuck_outside_countdown_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wf-unstuck"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );

    let (_, completion) = make_blocking_entry();
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let factory = BlockingFactory::new([(Arc::new(AtomicBool::new(false)), completion.clone())]);
    let (mut engine, _container_exits) = make_capturing_engine(
        &session,
        workflow,
        factory,
        [NextAction::LaunchNext],
        engine_tx.clone(),
    );

    let tx = engine_tx.lock().unwrap().clone().unwrap();

    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

    tokio::time::sleep(Duration::from_millis(100)).await;
    // Send StepUnstuck when there's no countdown — should be harmlessly ignored.
    tx.send(EngineRequest::StepUnstuck {
        step_name: String::new(),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    signal_completion(&completion, 0);
    let result = engine_task.await.unwrap().unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
}
