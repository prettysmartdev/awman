//! Background poller for the squad tab's task list and selected-task
//! detail.
//!
//! One task per squad tab. It holds no policy: every value it writes into the
//! shared snapshot came verbatim from a `TaskGateway` call. The task ticks
//! on a fixed interval and is a no-op while the tab is unfocused; it exits only
//! when its `CancellationToken` fires (on tab close).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::command::commands::squad::gateway::{TaskGateway, DEFAULT_RUN_HISTORY_LIMIT};
use crate::frontend::tui::tabs::squad_state::{SharedSelected, SharedSquadSnapshot, SquadTabState};

/// How often the squad tab polls the gateway while focused.
pub const SQUAD_POLL_INTERVAL: Duration = Duration::from_millis(2000);

/// Polls `list` (always) and `get` + `runs` (whenever a row is selected) into
/// the tab's shared snapshot.
pub struct SquadTaskPoller {
    gateway: Arc<dyn TaskGateway>,
    snapshot: SharedSquadSnapshot,
    focused: Arc<AtomicBool>,
    selected: SharedSelected,
    run_workflow_request:
        Arc<std::sync::Mutex<Option<(String, crate::data::fs::task_store::RunId)>>>,
    poll_wake: Arc<tokio::sync::Notify>,
}

impl SquadTaskPoller {
    pub fn new(gateway: Arc<dyn TaskGateway>, state: &SquadTabState) -> Self {
        Self {
            gateway,
            snapshot: state.snapshot.clone(),
            focused: state.focused.clone(),
            selected: state.poll_selected.clone(),
            run_workflow_request: state.run_workflow_request.clone(),
            poll_wake: state.poll_wake.clone(),
        }
    }

    /// Ticks every [`SQUAD_POLL_INTERVAL`]; each tick is a no-op while `focused`
    /// is false. Exits only when `cancel` fires.
    pub fn start(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(SQUAD_POLL_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        if self.focused.load(Ordering::Relaxed) {
                            self.poll_once().await;
                        }
                    }
                    _ = self.poll_wake.notified() => {
                        if self.focused.load(Ordering::Relaxed) { self.poll_once().await; }
                    }
                }
            }
        })
    }

    async fn poll_once(&self) {
        self.refresh_tasks().await;
        // Served even when the list, detail or history refresh failed, so an
        // open run detail shows "Workflow unavailable: …" instead of loading
        // forever.
        self.serve_run_workflow().await;
    }

    async fn refresh_tasks(&self) {
        // 1. Always refresh the list. On error, set `error` and leave the
        //    last-known tasks untouched so the tab never blanks to what
        //    would read as "no tasks".
        match self.gateway.list().await {
            Ok(tasks) => {
                if let Ok(mut snap) = self.snapshot.lock() {
                    snap.tasks = tasks;
                    snap.error = None;
                    snap.loaded = true;
                }
            }
            Err(error) => {
                if let Ok(mut snap) = self.snapshot.lock() {
                    snap.error = Some(error.to_string());
                    snap.loaded = true;
                }
                return;
            }
        }

        // 2. If a row is selected, refresh its detail + run history. A
        //    not-found error for the selected name sets `error` and clears the
        //    stale runs.
        let selected = self.selected.lock().ok().and_then(|guard| guard.clone());
        let Some(name) = selected else {
            return;
        };
        if let Err(error) = self.gateway.get(&name).await {
            if let Ok(mut snap) = self.snapshot.lock() {
                snap.error = Some(error.to_string());
                snap.runs.clear();
            }
            return;
        }
        match self.gateway.runs(&name, DEFAULT_RUN_HISTORY_LIMIT).await {
            Ok(runs) => {
                if let Ok(mut snap) = self.snapshot.lock() {
                    snap.runs = runs;
                }
            }
            Err(error) => {
                if let Ok(mut snap) = self.snapshot.lock() {
                    snap.error = Some(error.to_string());
                    snap.runs.clear();
                }
            }
        }
    }

    /// Fetch the workflow of the run whose detail is open, when it is not
    /// cached yet, the cached attempt failed (a transient error is retried),
    /// or the run is still live.
    async fn serve_run_workflow(&self) {
        let Some(name) = self.selected.lock().ok().and_then(|guard| guard.clone()) else {
            return;
        };
        let requested = self
            .run_workflow_request
            .lock()
            .ok()
            .and_then(|request| request.clone());
        if let Some((task, run_id)) = requested.filter(|(task, _)| task == &name) {
            let refresh = self.snapshot.lock().ok().is_none_or(|snap| {
                snap.run_workflow
                    .as_ref()
                    .is_none_or(|(existing, result)| existing != &run_id || result.is_err())
                    || snap
                        .runs
                        .iter()
                        .find(|run| run.id == run_id)
                        .is_some_and(|run| {
                            run.status == crate::data::fs::task_store::RunStatus::Running
                        })
            });
            if refresh {
                let result = self
                    .gateway
                    .run_workflow(&task, &run_id)
                    .await
                    .map_err(|error| error.to_string());
                if let Ok(mut snap) = self.snapshot.lock() {
                    snap.run_workflow = Some((run_id, result));
                }
            }
        }
    }
}

#[cfg(test)]
mod contracts {
    //! The poller's run-workflow request handling: a failed refresh still
    //! answers the open detail, an error is retried, and a finished run's
    //! graph is fetched once.
    use super::*;
    use crate::command::commands::squad::gateway::{CreateTask, DaemonStatus, UpdateTask};
    use crate::command::CommandError;
    use crate::data::fs::task_store::{Run, RunId, Task, TaskStatus};
    use crate::data::fs::RunWorkflow;
    use std::sync::atomic::AtomicUsize;

    struct Gateway {
        task: Task,
        run: Run,
        list_fails: AtomicBool,
        workflow_fails: AtomicBool,
        workflow_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl TaskGateway for Gateway {
        async fn list(&self) -> Result<Vec<Task>, CommandError> {
            if self.list_fails.load(Ordering::SeqCst) {
                return Err(CommandError::Other("daemon not reachable".into()));
            }
            Ok(vec![self.task.clone()])
        }
        async fn get(&self, _: &str) -> Result<Task, CommandError> {
            Ok(self.task.clone())
        }
        async fn runs(&self, _: &str, _: usize) -> Result<Vec<Run>, CommandError> {
            Ok(vec![self.run.clone()])
        }
        async fn run_workflow(&self, _: &str, _: &RunId) -> Result<RunWorkflow, CommandError> {
            self.workflow_calls.fetch_add(1, Ordering::SeqCst);
            if self.workflow_fails.load(Ordering::SeqCst) {
                return Err(CommandError::Other("gateway timed out".into()));
            }
            Ok(RunWorkflow::NoWorkflow)
        }
        async fn create(&self, _: CreateTask) -> Result<Task, CommandError> {
            unreachable!()
        }
        async fn update(&self, _: &str, _: UpdateTask) -> Result<Task, CommandError> {
            unreachable!()
        }
        async fn set_status(&self, _: &str, _: TaskStatus) -> Result<(), CommandError> {
            unreachable!()
        }
        async fn trigger(&self, _: &str) -> Result<(), CommandError> {
            unreachable!()
        }
        async fn cancel(&self, _: &str) -> Result<(), CommandError> {
            unreachable!()
        }
        async fn delete(&self, _: &str) -> Result<(), CommandError> {
            unreachable!()
        }
        async fn status(&self) -> Result<DaemonStatus, CommandError> {
            unreachable!()
        }
    }

    fn fixture() -> (Arc<Gateway>, SquadTabState, SquadTaskPoller) {
        let task: Task = serde_json::from_value(serde_json::json!({
            "id": crate::data::ids::TaskId::new(), "name": "alpha", "description": "d",
            "repo_scope": "/nonexistent-poll-fixture", "mount_scope": "directory", "overlays": [],
            "interval_secs": 60, "status": "active",
            "created_at": chrono::Utc::now(), "updated_at": chrono::Utc::now()
        }))
        .unwrap();
        let run: Run = serde_json::from_value(serde_json::json!({
            "id": RunId::new(), "task_id": task.id, "status": "failed",
            "started_at": chrono::Utc::now(), "finished_at": chrono::Utc::now()
        }))
        .unwrap();
        let gateway = Arc::new(Gateway {
            task,
            run,
            list_fails: AtomicBool::new(false),
            workflow_fails: AtomicBool::new(false),
            workflow_calls: AtomicUsize::new(0),
        });
        let state = SquadTabState::new();
        *state.poll_selected.lock().unwrap() = Some("alpha".into());
        *state.run_workflow_request.lock().unwrap() =
            Some(("alpha".into(), gateway.run.id.clone()));
        let poller = SquadTaskPoller::new(gateway.clone(), &state);
        (gateway, state, poller)
    }

    fn cached(state: &SquadTabState) -> Option<Result<RunWorkflow, String>> {
        state
            .snapshot
            .lock()
            .unwrap()
            .run_workflow
            .clone()
            .map(|(_, result)| result)
    }

    #[tokio::test]
    async fn a_failed_list_refresh_still_answers_the_open_run_detail() {
        let (gateway, state, poller) = fixture();
        gateway.list_fails.store(true, Ordering::SeqCst);
        gateway.workflow_fails.store(true, Ordering::SeqCst);
        poller.poll_once().await;
        assert!(state.snapshot.lock().unwrap().error.is_some());
        assert_eq!(cached(&state), Some(Err("gateway timed out".into())));
    }

    #[tokio::test]
    async fn a_failed_fetch_is_retried_and_a_finished_runs_graph_is_fetched_once() {
        let (gateway, state, poller) = fixture();
        gateway.workflow_fails.store(true, Ordering::SeqCst);
        poller.poll_once().await;
        assert!(matches!(cached(&state), Some(Err(_))));

        gateway.workflow_fails.store(false, Ordering::SeqCst);
        poller.poll_once().await;
        assert_eq!(cached(&state), Some(Ok(RunWorkflow::NoWorkflow)));
        assert_eq!(gateway.workflow_calls.load(Ordering::SeqCst), 2);

        // Finished and cached: no further fetches.
        poller.poll_once().await;
        assert_eq!(gateway.workflow_calls.load(Ordering::SeqCst), 2);
    }
}
