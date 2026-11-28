//! Shared task-terminal startup decisions through the production gateway interface.
use crate::support;
use async_trait::async_trait;
use awman::command::commands::squad::{
    attach::{
        resolve_task_terminal, AttachStartupPolicy, TaskAttachEnd, TaskTerminalResolution,
        WAITING_FOR_TERMINAL,
    },
    gateway::{CreateTask, DaemonStatus, TaskGateway, UpdateTask},
};
use awman::command::error::CommandError;
use awman::data::{
    execution_event::{EventSource, EventSourcePhase},
    fs::{MountScope, Run, RunStatus, Task, TaskStatus},
    ids::{RunId, TaskId, TerminalId},
    message::UserMessage,
    terminal::{TerminalOwner, TerminalRecord, TerminalState},
};
use awman_test_support::{DeadlineBudget, TestSandbox};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct State {
    runs: Vec<Run>,
    records: Vec<TerminalRecord>,
    daemon_failure: bool,
    unknown_task: bool,
    replace_on_records: bool,
    request_started: Option<tokio::sync::oneshot::Sender<()>>,
    release: Option<tokio::sync::oneshot::Receiver<()>>,
}
struct Gateway {
    task: Task,
    state: Arc<Mutex<State>>,
}
#[async_trait]
impl TaskGateway for Gateway {
    async fn get(&self, _: &str) -> Result<Task, CommandError> {
        let state = self.state.lock().unwrap();
        if state.unknown_task {
            return Err(CommandError::Other("unknown task".into()));
        }
        if state.daemon_failure {
            return Err(CommandError::DaemonNotRunning);
        }
        Ok(self.task.clone())
    }
    async fn runs(&self, _: &str, _: usize) -> Result<Vec<Run>, CommandError> {
        let state = self.state.lock().unwrap();
        if state.daemon_failure {
            return Err(CommandError::DaemonNotRunning);
        }
        Ok(state.runs.clone())
    }
    async fn run_terminals(&self, run: &RunId) -> Result<Vec<TerminalRecord>, CommandError> {
        let release = {
            let mut state = self.state.lock().unwrap();
            assert_eq!(
                run, &state.runs[0].id,
                "request must be scoped to the pinned run"
            );
            if state.replace_on_records {
                state.runs[0].id = RunId::new();
            }
            if let Some(started) = state.request_started.take() {
                started.send(()).unwrap();
            }
            state.release.take()
        };
        if let Some(release) = release {
            release.await.unwrap();
        }
        Ok(self.state.lock().unwrap().records.clone())
    }
    async fn create(&self, _: CreateTask) -> Result<Task, CommandError> {
        unreachable!()
    }
    async fn update(&self, _: &str, _: UpdateTask) -> Result<Task, CommandError> {
        unreachable!()
    }
    async fn list(&self) -> Result<Vec<Task>, CommandError> {
        unreachable!()
    }
    async fn set_status(&self, _: &str, _: TaskStatus) -> Result<(), CommandError> {
        unreachable!()
    }
    async fn trigger(&self, _: &str) -> Result<(), CommandError> {
        unreachable!()
    }
    async fn cancel(&self, _: &str) -> Result<(), CommandError> {
        panic!("attach never cancels a run")
    }
    async fn delete(&self, _: &str) -> Result<(), CommandError> {
        unreachable!()
    }
    async fn status(&self) -> Result<DaemonStatus, CommandError> {
        unreachable!()
    }
}
struct Fixture {
    sandbox: TestSandbox,
    gateway: Arc<Gateway>,
    state: Arc<Mutex<State>>,
    cancel: CancellationToken,
    budget: DeadlineBudget,
    policy: AttachStartupPolicy,
}
impl Fixture {
    fn new(test: &'static str) -> Self {
        let sandbox = support::sandbox(test);
        let now = chrono::Utc::now();
        let task = Task {
            id: TaskId::new(),
            name: "task".into(),
            description: "fixture".into(),
            repo_scope: sandbox.work(),
            mount_scope: MountScope::Directory,
            overlays: vec![],
            interval_secs: 60,
            status: TaskStatus::Active,
            agent: None,
            model: None,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            trigger_requested_at: None,
            last_run_status: None,
            last_run_phase: None,
            unmet_env: vec![],
            prerequisites: None,
        };
        let state = Arc::new(Mutex::new(State {
            runs: vec![Run {
                id: RunId::new(),
                task_id: task.id.clone(),
                status: RunStatus::Running,
                workflow_path: None,
                workflow_state_path: None,
                session_id: None,
                started_at: now,
                finished_at: None,
                error: None,
                reason: None,
                unmet_env: vec![],
                phase: None,
                image_warning: None,
            }],
            ..State::default()
        }));
        Self {
            sandbox,
            gateway: Arc::new(Gateway {
                task,
                state: state.clone(),
            }),
            state,
            cancel: CancellationToken::new(),
            budget: DeadlineBudget::new(Duration::from_secs(5)),
            policy: AttachStartupPolicy {
                window: Duration::from_secs(2),
                check_interval: Duration::from_millis(5),
            },
        }
    }
    fn run_id(&self) -> RunId {
        self.state.lock().unwrap().runs[0].id.clone()
    }
    fn publish(&self, container: &str, owner: RunId) -> TerminalId {
        let mut source = EventSource::phase(EventSourcePhase::Leader);
        source.container = Some(container.into());
        let id = TerminalId::new();
        self.state.lock().unwrap().records.push(TerminalRecord {
            credential_updates: None,
            automation: None,
            id: id.clone(),
            owner_id: TerminalOwner::Run(owner),
            feature: awman::data::daemon_state::FeatureName::Squad,
            source: Some(source),
            agent: "fixture".into(),
            model: None,
            attempt: 1,
            cols: 80,
            rows: 24,
            state: TerminalState::Open,
            exit_code: None,
            created_at: chrono::Utc::now(),
            allow_remote_input: false,
        });
        id
    }
    async fn resolve(
        &self,
        container: Option<&str>,
        progress: &mut (dyn FnMut(UserMessage) + Send),
    ) -> Result<TaskTerminalResolution, CommandError> {
        tokio::time::timeout(
            self.budget.remaining(),
            resolve_task_terminal(
                self.gateway.clone(),
                "task",
                None,
                container,
                self.policy,
                &self.cancel,
                progress,
            ),
        )
        .await
        .expect("startup deadline")
    }
}

#[tokio::test]
async fn startup_waits_for_current_run_and_ignores_other_runs() {
    let f =
        Fixture::new("squad::attach_startup::startup_waits_for_current_run_and_ignores_other_runs");
    f.publish("old", RunId::new());
    let mut messages = vec![];
    let result = f
        .resolve(None, &mut |m| {
            messages.push(m.text);
            f.publish("current", f.run_id());
        })
        .await
        .unwrap();
    let TaskTerminalResolution::Ready(record) = result else {
        panic!("current terminal not resolved")
    };
    assert_eq!(record.owner_id, TerminalOwner::Run(f.run_id()));
    assert_eq!(record.source.unwrap().container.as_deref(), Some("current"));
    assert_eq!(messages, [WAITING_FOR_TERMINAL]);
    f.sandbox.finish();
}

#[tokio::test]
async fn explicit_selector_waits_for_absence_and_readiness() {
    let f =
        Fixture::new("squad::attach_startup::explicit_selector_waits_for_absence_and_readiness");
    let result = f
        .resolve(Some("abc"), &mut |_| {
            f.publish("abcdef", f.run_id());
        })
        .await
        .unwrap();
    assert!(matches!(result, TaskTerminalResolution::Ready(_)));
    f.state.lock().unwrap().records[0].state = TerminalState::Exited;
    let result = f
        .resolve(Some("abcdef"), &mut |_| {
            f.state.lock().unwrap().records[0].state = TerminalState::Open;
        })
        .await
        .unwrap();
    assert!(matches!(result, TaskTerminalResolution::Ready(_)));
    f.sandbox.finish();
}

#[tokio::test]
async fn invalid_and_ambiguous_selectors_fail_without_waiting() {
    let f =
        Fixture::new("squad::attach_startup::invalid_and_ambiguous_selectors_fail_without_waiting");
    f.publish("abc1", f.run_id());
    f.publish("abc2", f.run_id());
    for selector in ["missing", "abc"] {
        let error = f
            .resolve(Some(selector), &mut |_| {
                panic!("invalid selector must not wait")
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("legal containers: abc1, abc2"));
    }
    f.sandbox.finish();
}

#[tokio::test]
async fn cancellation_does_not_emit_progress_after_detach() {
    let f = Fixture::new("squad::attach_startup::cancellation_does_not_emit_progress_after_detach");
    f.cancel.cancel();
    let result = f
        .resolve(None, &mut |_| panic!("cancelled viewer must stay silent"))
        .await
        .unwrap();
    assert!(matches!(
        result,
        TaskTerminalResolution::End(TaskAttachEnd::Cancelled)
    ));
    f.sandbox.finish();
}

#[tokio::test]
async fn cancellation_interrupts_an_outstanding_daemon_request() {
    let f = Fixture::new(
        "squad::attach_startup::cancellation_interrupts_an_outstanding_daemon_request",
    );
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (_release_tx, release_rx) = tokio::sync::oneshot::channel();
    {
        let mut state = f.state.lock().unwrap();
        state.request_started = Some(started_tx);
        state.release = Some(release_rx);
    }
    let mut progress = |_| panic!("detached while reading");
    let (result, ()) = tokio::join!(f.resolve(None, &mut progress), async {
        started_rx.await.unwrap();
        f.cancel.cancel();
    });
    assert!(matches!(
        result.unwrap(),
        TaskTerminalResolution::End(TaskAttachEnd::Cancelled)
    ));
    f.sandbox.finish();
}

#[tokio::test]
async fn completed_and_replaced_runs_end_waiting() {
    let f = Fixture::new("squad::attach_startup::completed_and_replaced_runs_end_waiting");
    let result = f
        .resolve(None, &mut |_| {
            f.state.lock().unwrap().runs[0].status = RunStatus::WorkflowExecuted
        })
        .await
        .unwrap();
    assert!(matches!(
        result,
        TaskTerminalResolution::End(TaskAttachEnd::RunCompleted)
    ));
    f.state.lock().unwrap().runs[0].status = RunStatus::Running;
    f.publish("stale", f.run_id());
    f.state.lock().unwrap().replace_on_records = true;
    let result = f
        .resolve(None, &mut |_| panic!("stale snapshot must be refused"))
        .await
        .unwrap();
    assert!(matches!(
        result,
        TaskTerminalResolution::End(TaskAttachEnd::RunCompleted)
    ));
    f.sandbox.finish();
}

#[tokio::test]
async fn idle_unknown_and_daemon_errors_are_immediate() {
    let f = Fixture::new("squad::attach_startup::idle_unknown_and_daemon_errors_are_immediate");
    f.state.lock().unwrap().runs.clear();
    assert!(f
        .resolve(None, &mut |_| panic!("idle"))
        .await
        .unwrap_err()
        .to_string()
        .contains("no run currently in progress"));
    f.state.lock().unwrap().unknown_task = true;
    assert!(f
        .resolve(None, &mut |_| panic!("unknown"))
        .await
        .unwrap_err()
        .to_string()
        .contains("unknown task"));
    {
        let mut state = f.state.lock().unwrap();
        state.unknown_task = false;
        state.daemon_failure = true;
    }
    assert!(matches!(
        f.resolve(None, &mut |_| panic!("daemon failure")).await,
        Err(CommandError::DaemonNotRunning)
    ));
    f.sandbox.finish();
}

#[tokio::test]
async fn startup_timeout_is_bounded_and_typed() {
    let mut f = Fixture::new("squad::attach_startup::startup_timeout_is_bounded_and_typed");
    f.policy.window = Duration::from_millis(50);
    let mut messages = vec![];
    let result = f
        .resolve(None, &mut |m| messages.push(m.text))
        .await
        .unwrap();
    assert!(matches!(
        result,
        TaskTerminalResolution::End(TaskAttachEnd::StartupTimedOut)
    ));
    assert_eq!(messages, [WAITING_FOR_TERMINAL]);
    assert!(TaskAttachEnd::StartupTimedOut
        .message(AttachStartupPolicy::default())
        .contains("10 seconds"));
    f.sandbox.finish();
}
