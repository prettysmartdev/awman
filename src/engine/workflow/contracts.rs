//! Workflow execution contracts: persistence, recovery, dependency barriers and
//! cancellation through the production scheduler. No agent runs on the host.
use super::*;
use crate::data::{
    config::env::EnvSnapshot,
    message::{UserMessage, UserMessageSink},
    session::SessionOpenOptions,
    workflow_state::WorkflowPhase,
};
use crate::engine::agent_runtime::execution::ExecutionBackend;
use awman_test_support::{TestCase, TestSandbox};
use std::collections::HashMap;
use std::sync::{mpsc, Mutex};

struct Running {
    exit: mpsc::Receiver<i32>,
    cancel: mpsc::Sender<i32>,
    cancelled: Arc<Mutex<HashSet<String>>>,
    name: String,
}
impl ExecutionBackend for Running {
    fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        let code = self
            .exit
            .recv_timeout(Duration::from_secs(5))
            .expect("scenario releases or cancels each execution");
        Ok(AgentExitInfo {
            exit_code: code,
            signal: None,
            started_at: chrono::Utc::now(),
            ended_at: chrono::Utc::now(),
        })
    }
    fn cancel(&self) -> Result<(), EngineError> {
        self.cancelled.lock().unwrap().insert(self.name.clone());
        let _ = self.cancel.send(KILLED_EXIT_CODE);
        Ok(())
    }
    fn cancel_handle(&self) -> CancelHandle {
        let cancel = self.cancel.clone();
        let cancelled = self.cancelled.clone();
        let name = self.name.clone();
        CancelHandle::new(move || {
            cancelled.lock().unwrap().insert(name.clone());
            let _ = cancel.send(KILLED_EXIT_CODE);
            Ok(())
        })
    }
}
#[derive(Default)]
struct Trace {
    launched: Vec<String>,
    contexts: Vec<WorkflowRuntimeContext>,
    stopped: Vec<String>,
    exits: VecDeque<i32>,
    held: Vec<mpsc::Sender<i32>>,
    reports: HashMap<String, Arc<tokio::sync::broadcast::Sender<StuckEvent>>>,
    cancelled: Arc<Mutex<HashSet<String>>>,
}
struct Runtime {
    trace: Arc<Mutex<Trace>>,
    hold: bool,
    started: tokio::sync::mpsc::UnboundedSender<String>,
}
impl AgentExecutionFactory for Runtime {
    fn execution_for_step(
        &self,
        step: &WorkflowStep,
        _: &Session,
        runtime: &WorkflowRuntimeContext,
    ) -> Result<AgentExecution, EngineError> {
        let (tx, rx) = mpsc::channel();
        let mut t = self.trace.lock().unwrap();
        t.launched.push(step.name.clone());
        t.contexts.push(runtime.clone());
        if self.hold {
            t.held.push(tx.clone());
        } else {
            tx.send(t.exits.pop_front().unwrap_or(0)).unwrap();
        }
        let (stuck, _) = tokio::sync::broadcast::channel(8);
        let stuck = Arc::new(stuck);
        t.reports.insert(step.name.clone(), stuck.clone());
        let _ = self.started.send(step.name.clone());
        Ok(AgentExecution::new(
            AgentHandle {
                id: step.name.clone(),
                name: step.name.clone(),
                image_tag: "contract/runtime".into(),
                started_at: chrono::Utc::now(),
            },
            Box::new(Running {
                exit: rx,
                cancel: tx,
                cancelled: t.cancelled.clone(),
                name: step.name.clone(),
            }),
            stuck,
            None,
        ))
    }
    fn inject_prompt(&self, _: &AgentExecution, _: &str) -> Result<Option<()>, EngineError> {
        Ok(None)
    }
    fn stop_agent(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        self.trace.lock().unwrap().stopped.push(handle.id.clone());
        Ok(())
    }
}
struct Frontend {
    retries: usize,
}
impl UserMessageSink for Frontend {
    fn write_message(&mut self, _: UserMessage) {}
    fn replay_queued(&mut self) {}
}
impl WorkflowFrontend for Frontend {
    fn show_workflow_control_board(
        &mut self,
        _: &WorkflowState,
        actions: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        if self.retries > 0 {
            if actions.step_failure.is_some() && actions.can_restart_current_step {
                self.retries -= 1;
                return Ok(NextAction::RestartCurrentStep);
            }
            if let Some(step) = &actions.retry_failed_step {
                self.retries -= 1;
                return Ok(NextAction::RetryFailedStep {
                    step_name: step.clone(),
                });
            }
        }
        Ok(if actions.can_launch_next {
            NextAction::LaunchNext
        } else if actions.can_finish_workflow {
            NextAction::FinishWorkflow
        } else {
            NextAction::Abort
        })
    }
    fn supports_interactive_recovery(&self) -> bool {
        true
    }
    fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn yolo_countdown_tick(
        &mut self,
        _: &str,
        _: Duration,
        _: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(YoloTickOutcome::Cancel)
    }
    fn report_step_status(&mut self, _: &WorkflowStep, _: WorkflowStepStatus) {}
    fn report_workflow_completed(&mut self, _: &WorkflowOutcome) {}
}
fn environment(s: &TestSandbox) -> EnvSnapshot {
    EnvSnapshot::from_full_env(
        s.env()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect(),
    )
}
fn definition(parallel: bool) -> Workflow {
    Workflow {
        title: Some("execution-contract".into()),
        agent: Some("claude".into()),
        model: None,
        setup: vec![],
        teardown: vec![],
        teardown_on_failure: false,
        overlays: None,
        steps: [
            ("build", vec![]),
            ("lint", if parallel { vec![] } else { vec!["build"] }),
            ("publish", vec!["build", "lint"]),
        ]
        .into_iter()
        .map(|(name, deps)| WorkflowStep {
            name: name.into(),
            depends_on: deps.into_iter().map(str::to_string).collect(),
            prompt_template: "contract task".into(),
            agent: None,
            model: None,
            overlays: None,
            abort_on_failure: false,
            read_only_workspace: false,
        })
        .collect(),
    }
}
async fn engine(
    s: &TestSandbox,
    trace: Arc<Mutex<Trace>>,
    hold: bool,
    retries: usize,
    parallel: bool,
    resume: bool,
) -> (WorkflowEngine, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let session = Session::open_at_directory_root(
        s.work(),
        SessionOpenOptions {
            env: Some(environment(s)),
            ..Default::default()
        },
    )
    .unwrap();
    let (started, rx) = tokio::sync::mpsc::unbounded_channel();
    let spec = WorkflowSpec::new(definition(parallel)).with_work_item_context(None);
    let deps = WorkflowEngineDeps {
        frontend: Box::new(Frontend { retries }),
        agent_factory: Box::new(Runtime {
            trace,
            hold,
            started,
        }),
    };
    let engine = if resume {
        WorkflowEngine::resume(&session, spec, deps).await
    } else {
        WorkflowEngine::new(&session, spec, deps)
    }
    .unwrap();
    (engine, rx)
}
#[tokio::test]
async fn persisted_execution_resumes_without_repeating_completed_steps() {
    let s = TestSandbox::for_test(TestCase::component("engine::workflow::contracts::persisted_execution_resumes_without_repeating_completed_steps"));
    let trace = Arc::new(Mutex::new(Trace::default()));
    let (mut first, _) = engine(&s, trace.clone(), false, 0, false, false).await;
    let step = first.step_once().await.unwrap();
    assert_eq!(step.step_name, "build");
    assert_eq!(step.status, WorkflowStepStatus::Succeeded);
    drop(first);
    let (mut resumed, _) = engine(&s, trace.clone(), false, 0, false, true).await;
    assert_eq!(
        resumed.run_to_completion().await.unwrap(),
        WorkflowOutcome::Completed
    );
    assert_eq!(trace.lock().unwrap().launched, ["build", "lint", "publish"]);
    let saved = WorkflowStateStore::at_git_root(s.work())
        .load(None, "execution-contract")
        .unwrap()
        .unwrap();
    for step in ["build", "lint", "publish"] {
        assert_eq!(saved.status_of(step), Some(&StepState::Succeeded));
    }
    s.finish();
}
#[tokio::test]
async fn resume_rewind_preserves_completed_parallel_branches() {
    let s = TestSandbox::for_test(TestCase::component(
        "engine::workflow::contracts::resume_rewind_preserves_completed_parallel_branches",
    ));
    let session = Session::open_at_directory_root(
        s.work(),
        SessionOpenOptions {
            env: Some(environment(&s)),
            ..Default::default()
        },
    )
    .unwrap();
    let mut workflow = definition(false);
    workflow.steps = [
        ("plan-a", vec![]),
        ("implement-a", vec!["plan-a"]),
        ("review-a", vec!["implement-a"]),
        ("plan-b", vec![]),
        ("implement-b", vec!["plan-b"]),
        ("join", vec!["review-a", "implement-b"]),
    ]
    .into_iter()
    .map(|(name, deps)| WorkflowStep {
        name: name.into(),
        depends_on: deps.into_iter().map(str::to_string).collect(),
        ..workflow.steps[0].clone()
    })
    .collect();
    let dag = WorkflowDag::build(&workflow.steps).unwrap();
    let store = WorkflowStateStore::at_git_root(s.work());
    let mut saved = WorkflowState::new(
        "execution-contract".into(),
        &workflow.steps,
        compute_workflow_hash(&workflow),
        None,
    );
    // Both branches reached review; one was interrupted while the other's
    // implementation was deliberately skipped. Its lower stages occur AFTER
    // review-a in the DFS listing used by both resume prompts.
    for name in ["plan-a", "implement-a", "plan-b"] {
        saved.set_status(name, StepState::Succeeded);
    }
    saved.set_status("implement-b", StepState::Skipped);
    saved.set_status("review-a", StepState::Running { container_id: None });
    saved.set_status("join", StepState::Cancelled);
    store.save(&saved).unwrap();

    // The command paths (plain and dynamic) rewind and persist before loading
    // the engine. A direct engine resume alone missed this regression.
    let mut saved = store.load(None, "execution-contract").unwrap().unwrap();
    saved.rewind_to(&dag, "review-a");
    store.save(&saved).unwrap();
    let trace = Arc::new(Mutex::new(Trace::default()));
    let deps = || {
        let (started, _) = tokio::sync::mpsc::unbounded_channel();
        WorkflowEngineDeps {
            frontend: Box::new(Frontend { retries: 0 }),
            agent_factory: Box::new(Runtime {
                trace: trace.clone(),
                hold: false,
                started,
            }),
        }
    };
    let mut resumed = WorkflowEngine::resume(&session, WorkflowSpec::new(workflow.clone()), deps())
        .await
        .unwrap();
    assert_eq!(
        resumed.state().status_of("plan-b"),
        Some(&StepState::Succeeded)
    );
    assert_eq!(
        resumed.state().status_of("implement-b"),
        Some(&StepState::Skipped)
    );
    assert_eq!(resumed.step_once().await.unwrap().step_name, "review-a");
    drop(resumed);

    // Interrupt and resume again at the join: progress must survive repeated
    // save/load/rewind cycles, and the scheduler must launch only remaining work.
    let mut saved = store.load(None, "execution-contract").unwrap().unwrap();
    saved.rewind_to(&dag, "join");
    store.save(&saved).unwrap();
    let mut resumed = WorkflowEngine::resume(&session, WorkflowSpec::new(workflow), deps())
        .await
        .unwrap();
    assert_eq!(
        resumed.run_to_completion().await.unwrap(),
        WorkflowOutcome::Completed
    );
    assert_eq!(trace.lock().unwrap().launched, ["review-a", "join"]);
    assert_eq!(resumed.state().completed_steps.len(), 6);
    s.finish();
}

#[tokio::test]
async fn remediation_inherits_workflow_progress_and_invocation() {
    use crate::engine::context_prompt::{ContextPromptBuilder, WorkflowStepState};
    let s = TestSandbox::for_test(TestCase::component(
        "engine::workflow::contracts::remediation_inherits_workflow_progress_and_invocation",
    ));
    let trace = Arc::new(Mutex::new(Trace::default()));
    let (mut workflow, _) = engine(&s, trace.clone(), false, 0, false, false).await;
    let invocation = workflow.state().invocation_id;
    workflow.work_item_context = Some(
        crate::data::workflow_prompt_template::WorkItemContext::from_legacy(
            7,
            "# Shared context".into(),
        ),
    );
    let session = workflow.session.clone();
    for (phase, label) in [
        (WorkflowPhase::Setup, "Setup"),
        (WorkflowPhase::Teardown, "Teardown"),
        (WorkflowPhase::Done, "Merge"),
    ] {
        workflow.state.current_phase = phase;
        if phase != WorkflowPhase::Setup {
            workflow.state.set_status("build", StepState::Succeeded);
        }
        workflow
            .run_remediation_agent(
                &session,
                RemediationAgentSpec {
                    label: "on_failure: check".into(),
                    prompt: "Fix the failure".into(),
                    agent: AgentName::new("claude").unwrap(),
                    model: None,
                    extra_overlays: None,
                },
            )
            .await
            .unwrap();
        let trace = trace.lock().unwrap();
        let runtime = trace.contexts.last().unwrap();
        assert_eq!(runtime.workflow_invocation_id, invocation);
        let info = runtime
            .workflow_step_info
            .as_ref()
            .expect("remediation workflow metadata");
        assert_eq!(info.workflow_title, "execution-contract");
        assert_eq!(
            info.current_step_name,
            format!("{label} remediation: on_failure: check")
        );
        assert_eq!(info.current_step_index, None);
        assert_eq!(info.work_item_number, Some(7));
        assert_eq!(
            info.steps[0].1,
            if phase == WorkflowPhase::Setup {
                WorkflowStepState::Pending
            } else {
                WorkflowStepState::Completed
            }
        );
        let prompt = ContextPromptBuilder::new()
            .with_workflow(info)
            .build()
            .unwrap();
        assert!(prompt.contains("read any files left by previous steps"));
        assert!(prompt.contains("Write your outputs, notes"));
        assert!(!prompt.contains("one-shot"));
        assert!(!prompt.contains("step 1 of"));
    }
    s.finish();
}

#[tokio::test]
async fn preallocated_invocation_is_persisted_and_resume_keeps_it() {
    let s = TestSandbox::for_test(TestCase::component(
        "engine::workflow::contracts::preallocated_invocation_is_persisted_and_resume_keeps_it",
    ));
    let session = Session::open_at_directory_root(
        s.work(),
        SessionOpenOptions {
            env: Some(environment(&s)),
            ..Default::default()
        },
    )
    .unwrap();
    let trace = Arc::new(Mutex::new(Trace::default()));
    let deps = || {
        let (started, _) = tokio::sync::mpsc::unbounded_channel();
        WorkflowEngineDeps {
            frontend: Box::new(Frontend { retries: 0 }),
            agent_factory: Box::new(Runtime {
                trace: trace.clone(),
                hold: false,
                started,
            }),
        }
    };
    let invocation = uuid::Uuid::new_v4();
    let mut first = WorkflowEngine::resume(
        &session,
        WorkflowSpec::new(definition(false)).with_invocation_id(Some(invocation)),
        deps(),
    )
    .await
    .unwrap();
    assert_eq!(first.state().invocation_id, invocation);
    first.step_once().await.unwrap();
    drop(first);
    let mut resumed = WorkflowEngine::resume(
        &session,
        WorkflowSpec::new(definition(false)).with_invocation_id(Some(uuid::Uuid::new_v4())),
        deps(),
    )
    .await
    .unwrap();
    assert_eq!(resumed.state().invocation_id, invocation);
    resumed.step_once().await.unwrap();
    for runtime in &trace.lock().unwrap().contexts {
        assert_eq!(runtime.workflow_invocation_id, invocation);
    }
    s.finish();
}

#[tokio::test]
async fn failure_can_be_retried_before_dependents_run() {
    let s = TestSandbox::for_test(TestCase::component(
        "engine::workflow::contracts::failure_can_be_retried_before_dependents_run",
    ));
    let trace = Arc::new(Mutex::new(Trace {
        exits: VecDeque::from([17, 0, 0, 0]),
        ..Default::default()
    }));
    let (mut workflow, _) = engine(&s, trace.clone(), false, 1, false, false).await;
    assert_eq!(
        workflow.run_to_completion().await.unwrap(),
        WorkflowOutcome::Completed
    );
    assert_eq!(
        trace.lock().unwrap().launched,
        ["build", "build", "lint", "publish"]
    );
    assert_eq!(
        workflow.state().status_of("build"),
        Some(&StepState::Succeeded)
    );
    s.finish();
}
#[tokio::test]
async fn cancellation_stops_the_running_agent_and_never_launches_dependents() {
    let s = TestSandbox::for_test(TestCase::component("engine::workflow::contracts::cancellation_stops_the_running_agent_and_never_launches_dependents"));
    let trace = Arc::new(Mutex::new(Trace::default()));
    let (mut workflow, mut started) = engine(&s, trace.clone(), true, 0, false, false).await;
    let token = CancellationToken::new();
    workflow.set_cancellation(token.clone());
    let task = tokio::spawn(async move { workflow.run_to_completion().await.unwrap() });
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), started.recv())
            .await
            .unwrap()
            .unwrap(),
        "build"
    );
    token.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap(),
        WorkflowOutcome::Cancelled
    );
    assert_eq!(trace.lock().unwrap().launched, ["build"]);
    assert_eq!(trace.lock().unwrap().stopped, ["build"]);
    s.finish();
}
#[tokio::test]
async fn dependency_join_waits_for_every_parallel_predecessor() {
    let s = TestSandbox::for_test(TestCase::component(
        "engine::workflow::contracts::dependency_join_waits_for_every_parallel_predecessor",
    ));
    let trace = Arc::new(Mutex::new(Trace::default()));
    let (mut workflow, mut started) = engine(&s, trace.clone(), true, 0, true, false).await;
    let task = tokio::spawn(async move { workflow.run_to_completion().await.unwrap() });
    let mut first = Vec::new();
    for _ in 0..2 {
        first.push(
            tokio::time::timeout(Duration::from_secs(5), started.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    first.sort();
    assert_eq!(first, ["build", "lint"]);
    {
        let t = trace.lock().unwrap();
        assert_eq!(t.launched.len(), 2);
        for exit in &t.held {
            exit.send(0).unwrap();
        }
    }
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), started.recv())
            .await
            .unwrap()
            .unwrap(),
        "publish"
    );
    trace.lock().unwrap().held.last().unwrap().send(0).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap(),
        WorkflowOutcome::Completed
    );
    s.finish();
}

/// The final board must leave every live agent available until Finish, even
/// after Esc. Unattended frontends must never be asked for final review.
struct FinalReviewFrontend {
    trace: Arc<Mutex<Trace>>,
    attended: bool,
    boards: Arc<std::sync::atomic::AtomicUsize>,
    requests: Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>,
}
impl UserMessageSink for FinalReviewFrontend {
    fn write_message(&mut self, _: UserMessage) {}
    fn replay_queued(&mut self) {}
}
impl WorkflowFrontend for FinalReviewFrontend {
    fn supports_interactive_recovery(&self) -> bool {
        self.attended
    }
    fn attach_engine(&mut self, handles: EngineHandles) {
        if let Some(requests) = handles.requests {
            self.requests = Some(requests);
        }
    }
    fn show_workflow_control_board(
        &mut self,
        state: &WorkflowState,
        actions: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        use std::sync::atomic::Ordering::SeqCst;
        assert!(
            self.attended,
            "unattended runs must not wait for final review"
        );
        assert!(actions.can_finish_workflow);
        assert!(!actions.can_launch_next);
        assert!(actions.simple_advance.is_none());
        assert_eq!(state.current_phase, WorkflowPhase::Main);
        assert_eq!(state.teardown_step_states.len(), 2);
        assert!(state.teardown_step_states.iter().all(|step| matches!(
            step.status,
            crate::data::workflow_state::PhaseStepStatus::Pending
        )));
        assert!(!state.teardown_completed);
        let trace = self.trace.lock().unwrap();
        assert_eq!(
            trace.cancelled.lock().unwrap().len(),
            0,
            "agents killed before Finish"
        );
        assert!(trace.stopped.is_empty());
        let count = self.boards.fetch_add(1, SeqCst);
        if count == 0 {
            self.requests
                .as_ref()
                .unwrap()
                .send(EngineRequest::OpenControlBoard {
                    step_name: trace.launched.last().unwrap().clone(),
                })
                .unwrap();
            Ok(NextAction::Dismiss)
        } else {
            Ok(NextAction::FinishWorkflow)
        }
    }
    fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn yolo_countdown_tick(
        &mut self,
        _: &str,
        _: Duration,
        _: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(YoloTickOutcome::AdvanceNow)
    }
    fn parallel_step_yolo_countdown_tick(
        &mut self,
        _: &str,
        _: Duration,
        _: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(YoloTickOutcome::AdvanceNow)
    }
    fn report_step_status(&mut self, _: &WorkflowStep, _: WorkflowStepStatus) {}
    fn report_workflow_completed(&mut self, _: &WorkflowOutcome) {}
}

async fn final_review_scenario(
    s: &TestSandbox,
    yolo: bool,
    parallel: bool,
    attended: bool,
    non_interactive: bool,
    report: Option<StuckEvent>,
) {
    use crate::data::workflow_definition::TeardownStep;
    use crate::engine::agent_runtime::background::ExecOutput;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    struct TeardownExec;
    impl AgentExec for TeardownExec {
        fn exec(
            &self,
            _: &str,
            _: Option<&HashMap<String, String>>,
        ) -> Result<ExecOutput, EngineError> {
            Ok(ExecOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            })
        }
    }

    let trace = Arc::new(Mutex::new(Trace::default()));
    let boards = Arc::new(AtomicUsize::new(0));
    let session = Session::open_at_directory_root(
        s.work(),
        SessionOpenOptions {
            env: Some(environment(s)),
            ..Default::default()
        },
    )
    .unwrap();
    let mut workflow = definition(parallel);
    workflow.steps.truncate(if parallel { 3 } else { 1 });
    for step in &mut workflow.steps {
        step.depends_on.clear();
    }
    let count = workflow.steps.len();
    let (started, mut launches) = tokio::sync::mpsc::unbounded_channel();
    let mut engine = WorkflowEngine::new(
        &session,
        WorkflowSpec::new(workflow).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(FinalReviewFrontend {
                trace: trace.clone(),
                attended,
                boards: boards.clone(),
                requests: None,
            }),
            agent_factory: Box::new(Runtime {
                trace: trace.clone(),
                hold: report.is_some(),
                started,
            }),
        },
    )
    .unwrap();
    engine.set_yolo(yolo);
    engine.set_non_interactive(non_interactive);
    // Final interactive groups must retain all peers even with a cap of one.
    engine.max_concurrent = if attended && !non_interactive {
        Some(1)
    } else {
        None
    };
    let teardown = ["verify", "publish"].map(|command| TeardownStep::RunShell {
        command: command.into(),
        env: None,
    });
    // Publishing the full plan must not grant permission to execute teardown.
    engine.declare_phase_steps(&[], &teardown).unwrap();
    let review = attended && !non_interactive;
    let held_agents = report.is_some();
    let task_boards = boards.clone();
    let task_trace = trace.clone();
    let task = tokio::spawn(async move {
        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
        assert_eq!(task_boards.load(SeqCst), if review { 2 } else { 0 });
        assert_eq!(engine.state().current_phase, WorkflowPhase::Main);
        let mut teardown_launches = 0;
        let outcome = engine
            .run_phase(PhaseKind::Teardown, &teardown, &[], &[], |_| {
                teardown_launches += 1;
                if held_agents {
                    let trace = task_trace.lock().unwrap();
                    assert_eq!(trace.cancelled.lock().unwrap().len(), count);
                }
                Ok(Box::new(TeardownExec))
            })
            .unwrap();
        assert!(!outcome.any_failed);
        assert_eq!(teardown_launches, 2);
        assert!(engine.state().teardown_completed);
        result
    });
    for _ in 0..count {
        let name = tokio::time::timeout(Duration::from_secs(5), launches.recv())
            .await
            .unwrap()
            .unwrap();
        if let Some(event) = &report {
            let sender = trace.lock().unwrap().reports[&name].clone();
            sender.send(event.clone()).unwrap();
        }
    }
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap(),
        WorkflowOutcome::Completed
    );
    assert_eq!(boards.load(SeqCst), if review { 2 } else { 0 });
    let trace = trace.lock().unwrap();
    assert_eq!(trace.launched.len(), count);
    assert_eq!(
        trace.cancelled.lock().unwrap().len(),
        if report.is_some() { count } else { 0 }
    );
}

fn completion_events() -> Vec<Option<StuckEvent>> {
    use crate::data::program_status::{EffectiveStatus, ProgramState};
    let mut events: Vec<_> = [ProgramState::Done, ProgramState::Idle]
        .into_iter()
        .map(|state| {
            Some(StuckEvent::Reported(Some(EffectiveStatus {
                state,
                kind: None,
                progress: None,
                msg: None,
                app: None,
                id: None,
            })))
        })
        .collect();
    events.extend([Some(StuckEvent::Stuck), None]);
    events
}

#[tokio::test]
async fn final_agents_wait_for_explicit_finish_in_every_interactive_mode() {
    let s = TestSandbox::for_test(TestCase::component("engine::workflow::contracts::final_agents_wait_for_explicit_finish_in_every_interactive_mode"));
    for yolo in [false, true] {
        for parallel in [false, true] {
            for report in completion_events() {
                final_review_scenario(&s, yolo, parallel, true, false, report).await;
            }
        }
    }
    s.finish();
}

#[tokio::test]
async fn unattended_and_explicit_non_interactive_runs_finish_without_review() {
    let s = TestSandbox::for_test(TestCase::component("engine::workflow::contracts::unattended_and_explicit_non_interactive_runs_finish_without_review"));
    for (attended, non_interactive) in [(false, false), (true, true)] {
        for parallel in [false, true] {
            for report in completion_events() {
                final_review_scenario(&s, true, parallel, attended, non_interactive, report).await;
            }
        }
    }
    s.finish();
}

#[tokio::test]
async fn scratch_lease_spans_setup_single_steps_main_and_teardown_lifetime() {
    let s = TestSandbox::for_test(TestCase::component("engine::workflow::contracts::scratch_lease_spans_setup_single_steps_main_and_teardown_lifetime"));
    let resolver =
        crate::data::fs::context_dirs::ContextDirResolver::from_env(&environment(&s)).unwrap();
    let (mut workflow, _) = engine(
        &s,
        Arc::new(Mutex::new(Trace::default())),
        false,
        0,
        false,
        false,
    )
    .await;
    let invocation = workflow.state().invocation_id;
    let dir = resolver.workflow_dir(invocation);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note"), b"live").unwrap();
    assert!(
        resolver.remove_workflow_dir(invocation).is_err(),
        "lease is held before setup or main starts"
    );
    workflow.step_once().await.unwrap();
    assert!(
        resolver.remove_workflow_dir(invocation).is_err(),
        "single-step foreground runs retain scratch"
    );
    workflow.run_to_completion().await.unwrap();
    assert!(
        resolver.remove_workflow_dir(invocation).is_err(),
        "teardown can still use scratch after main completes"
    );
    drop(workflow);
    resolver.remove_workflow_dir(invocation).unwrap();
    assert!(!dir.exists());
}

/// A live container keeps the workspace access it launched with, so a prompt
/// must never be injected across a read-only/writable boundary.
#[tokio::test]
async fn continue_in_current_container_refuses_a_workspace_access_change() {
    let s = TestSandbox::for_test(TestCase::component("engine::workflow::contracts::continue_in_current_container_refuses_a_workspace_access_change"));
    let session = Session::open_at_directory_root(
        s.work(),
        SessionOpenOptions {
            env: Some(environment(&s)),
            ..Default::default()
        },
    )
    .unwrap();
    for (next_read_only, expect_offered) in [(true, false), (false, true)] {
        let mut workflow = definition(false);
        workflow.steps.truncate(2);
        workflow.steps[1].read_only_workspace = next_read_only;
        let trace = Arc::new(Mutex::new(Trace::default()));
        let (started, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut engine = WorkflowEngine::new(
            &session,
            WorkflowSpec::new(workflow).with_work_item_context(None),
            WorkflowEngineDeps {
                frontend: Box::new(Frontend { retries: 0 }),
                agent_factory: Box::new(Runtime {
                    trace: trace.clone(),
                    hold: true,
                    started,
                }),
            },
        )
        .unwrap();
        assert_eq!(engine.launch_step().await.unwrap(), "build");
        engine.state.set_status("build", StepState::Succeeded);
        let actions = engine.compute_available_actions().unwrap();
        assert_eq!(actions.can_continue_in_current_container, expect_offered);
        let err = engine
            .handle_continue_in_current_container("next")
            .unwrap_err()
            .to_string();
        // Same access reaches the (test) backend, which declines injection;
        // a changed access is refused before any injection is attempted.
        assert_eq!(err.contains("workspace access"), !expect_offered, "{err}");
        for tx in trace.lock().unwrap().held.drain(..) {
            let _ = tx.send(0);
        }
    }
    s.finish();
}
