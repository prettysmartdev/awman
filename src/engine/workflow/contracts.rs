//! Workflow execution contracts: persistence, recovery, dependency barriers and
//! cancellation through the production scheduler. No agent runs on the host.
use super::*;
use crate::data::{
    config::env::EnvSnapshot,
    message::{UserMessage, UserMessageSink},
    session::SessionOpenOptions,
};
use crate::engine::agent_runtime::execution::ExecutionBackend;
use awman_test_support::{TestCase, TestSandbox};
use std::sync::{mpsc, Mutex};

struct Running {
    exit: mpsc::Receiver<i32>,
    cancel: mpsc::Sender<i32>,
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
        let _ = self.cancel.send(KILLED_EXIT_CODE);
        Ok(())
    }
    fn cancel_handle(&self) -> CancelHandle {
        let cancel = self.cancel.clone();
        CancelHandle::new(move || {
            let _ = cancel.send(KILLED_EXIT_CODE);
            Ok(())
        })
    }
}
#[derive(Default)]
struct Trace {
    launched: Vec<String>,
    stopped: Vec<String>,
    exits: VecDeque<i32>,
    held: Vec<mpsc::Sender<i32>>,
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
        _: &WorkflowRuntimeContext,
    ) -> Result<AgentExecution, EngineError> {
        let (tx, rx) = mpsc::channel();
        let mut t = self.trace.lock().unwrap();
        t.launched.push(step.name.clone());
        if self.hold {
            t.held.push(tx.clone());
        } else {
            tx.send(t.exits.pop_front().unwrap_or(0)).unwrap();
        }
        let _ = self.started.send(step.name.clone());
        let (stuck, _) = tokio::sync::broadcast::channel(8);
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
            }),
            Arc::new(stuck),
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
