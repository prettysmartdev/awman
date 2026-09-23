//! Layer 2 — remote workflow polling.
//!
//! The HTTP client itself is Layer 1 (`engine::remote`, WI 0114 F-28). What
//! stays here is what depends on Layer 2: `SquadTaskWorkflowSource` is backed
//! by the `TaskGateway` trait, and `RemoteWorkflowPoller` is the presentation
//! cadence a command drives. The engine's types are re-exported so existing
//! `commands::remote_client::…` paths keep working.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::error::CommandError;
use crate::data::workflow_state::WorkflowState;

pub use crate::engine::remote::client::{
    ExecArg, ExecJobResponse, JobStatus, RemoteClient, RemoteResponse, SessionSetupStatusResponse,
    StartSessionRequest, StartSessionResponse,
};
pub use crate::engine::remote::events::ExecutionEventSink;
#[cfg(test)]
pub use crate::engine::remote::events::RemoteEventSink;

/// How often a remote workflow snapshot is refreshed.
pub const REMOTE_WORKFLOW_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Source of workflow snapshots for [`RemoteWorkflowPoller`].
#[async_trait]
pub trait WorkflowStateSource: Send + Sync {
    /// Fetch the current workflow snapshot. `Ok(None)` means that no workflow
    /// state exists right now, not that the source is unreachable.
    async fn fetch_workflow_state(&self) -> Result<Option<WorkflowState>, CommandError>;

    /// Whether the remote job reached a terminal status. Sources without a
    /// separate job-status route leave the default in place.
    async fn is_terminal(&self) -> bool {
        false
    }
}

/// Workflow source backed by the API server's per-command routes.
pub struct RemoteApiWorkflowSource {
    client: Arc<RemoteClient>,
    command_id: String,
}

impl RemoteApiWorkflowSource {
    pub fn new(client: Arc<RemoteClient>, command_id: impl Into<String>) -> Self {
        Self {
            client,
            command_id: command_id.into(),
        }
    }
}

#[async_trait]
impl WorkflowStateSource for RemoteApiWorkflowSource {
    async fn fetch_workflow_state(&self) -> Result<Option<WorkflowState>, CommandError> {
        let Some(value) = self.client.get_workflow_state(&self.command_id).await? else {
            return Ok(None);
        };
        serde_json::from_value(value).map(Some).map_err(|error| {
            CommandError::RemoteTransport(format!("invalid remote workflow state: {error}"))
        })
    }

    async fn is_terminal(&self) -> bool {
        self.client
            .job_status(&self.command_id)
            .await
            .is_ok_and(JobStatus::is_terminal)
    }
}

/// Workflow source backed by the squad task gateway.
pub struct SquadTaskWorkflowSource {
    gateway: Arc<dyn TaskGateway>,
    task: String,
}

impl SquadTaskWorkflowSource {
    pub fn new(gateway: Arc<dyn TaskGateway>, task: impl Into<String>) -> Self {
        Self {
            gateway,
            task: task.into(),
        }
    }
}

#[async_trait]
impl WorkflowStateSource for SquadTaskWorkflowSource {
    async fn fetch_workflow_state(&self) -> Result<Option<WorkflowState>, CommandError> {
        self.gateway.workflow_state(&self.task).await
    }
}

/// Polls a remote workflow and publishes each successful snapshot to a
/// caller-owned presentation callback.
pub struct RemoteWorkflowPoller {
    source: Arc<dyn WorkflowStateSource>,
    reachable: Arc<AtomicBool>,
    on_state: Box<dyn FnMut(&WorkflowState) + Send>,
    initial_state_seen: bool,
}

impl RemoteWorkflowPoller {
    pub fn new(
        source: Arc<dyn WorkflowStateSource>,
        on_state: Box<dyn FnMut(&WorkflowState) + Send>,
    ) -> Self {
        Self {
            source,
            reachable: Arc::new(AtomicBool::new(true)),
            on_state,
            initial_state_seen: false,
        }
    }

    /// Publish source reachability into a caller-owned indicator.
    pub fn with_reachable(mut self, flag: Arc<AtomicBool>) -> Self {
        self.reachable = flag;
        self
    }

    /// Seed disappearance detection when the caller fetched and published an
    /// initial snapshot before starting this poller.
    pub fn with_initial_state_seen(mut self, seen: bool) -> Self {
        self.initial_state_seen = seen;
        self
    }

    pub fn start(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.poll_loop(cancel).await;
        })
    }

    async fn poll_loop(mut self, cancel: CancellationToken) {
        let mut saw_state = self.initial_state_seen;
        loop {
            let should_stop = tokio::select! {
                _ = cancel.cancelled() => break,
                result = self.poll_once(&mut saw_state) => result,
            };

            if should_stop {
                // Preserve the final refresh: terminal API jobs get their
                // last state, while a disappearing squad route leaves the
                // last terminal snapshot frozen in the TUI.
                let _ = tokio::select! {
                    _ = cancel.cancelled() => None,
                    result = self.fetch_and_publish(&mut saw_state) => result,
                };
                break;
            }

            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(REMOTE_WORKFLOW_POLL_INTERVAL) => {}
            }
        }
    }

    /// Returns true when polling should stop after one final refresh.
    async fn poll_once(&mut self, saw_state: &mut bool) -> bool {
        let terminal = self.source.is_terminal().await;
        match self.fetch_and_publish(saw_state).await {
            // A failed state fetch always keeps polling, even if a separate
            // status route happened to report terminal in the same cycle.
            None => false,
            Some(disappeared_after_state) => terminal || disappeared_after_state,
        }
    }

    /// Fetch and publish one snapshot. `Some(true)` means a source that
    /// previously yielded state now reports no state; `None` means the fetch
    /// failed. Errors freeze the view and never request termination.
    async fn fetch_and_publish(&mut self, saw_state: &mut bool) -> Option<bool> {
        match self.source.fetch_workflow_state().await {
            Err(_) => {
                self.reachable.store(false, Ordering::Relaxed);
                None
            }
            Ok(None) => {
                self.reachable.store(true, Ordering::Relaxed);
                Some(*saw_state)
            }
            Ok(Some(state)) => {
                self.reachable.store(true, Ordering::Relaxed);
                *saw_state = true;
                (self.on_state)(&state);
                Some(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeWorkflowSource {
        states: Mutex<VecDeque<Result<Option<WorkflowState>, CommandError>>>,
        terminals: Mutex<VecDeque<bool>>,
    }

    impl FakeWorkflowSource {
        fn new(
            states: Vec<Result<Option<WorkflowState>, CommandError>>,
            terminals: Vec<bool>,
        ) -> Self {
            Self {
                states: Mutex::new(states.into()),
                terminals: Mutex::new(terminals.into()),
            }
        }
    }

    #[async_trait]
    impl WorkflowStateSource for FakeWorkflowSource {
        async fn fetch_workflow_state(&self) -> Result<Option<WorkflowState>, CommandError> {
            self.states.lock().unwrap().pop_front().unwrap_or(Ok(None))
        }

        async fn is_terminal(&self) -> bool {
            self.terminals.lock().unwrap().pop_front().unwrap_or(false)
        }
    }

    fn state(name: &str) -> WorkflowState {
        WorkflowState::new(name.to_string(), &[], "test-hash".to_string(), None)
    }

    async fn cancel_after_states(
        seen: &Arc<Mutex<Vec<String>>>,
        count: usize,
        cancel: &CancellationToken,
    ) {
        for _ in 0..250 {
            if seen.lock().unwrap().len() >= count {
                cancel.cancel();
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("poller did not publish {count} states");
    }

    #[tokio::test]
    async fn poller_finally_refreshes_after_terminal_status() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let callback_seen = seen.clone();
        let source = Arc::new(FakeWorkflowSource::new(
            vec![
                Ok(Some(state("initial"))),
                Ok(Some(state("terminal"))),
                Ok(Some(state("final"))),
            ],
            vec![false, true],
        ));
        let cancel = CancellationToken::new();
        let task = RemoteWorkflowPoller::new(
            source,
            Box::new(move |snapshot| {
                callback_seen
                    .lock()
                    .unwrap()
                    .push(snapshot.workflow_name.clone());
            }),
        )
        .start(cancel.clone());

        cancel_after_states(&seen, 3, &cancel).await;
        task.await.unwrap();
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["initial", "terminal", "final"]
        );
    }

    #[tokio::test]
    async fn poller_keeps_polling_after_transient_source_error() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let callback_seen = seen.clone();
        let source = Arc::new(FakeWorkflowSource::new(
            vec![
                Ok(Some(state("before-error"))),
                Err(CommandError::RemoteTransport("temporary outage".into())),
                Ok(Some(state("after-error"))),
            ],
            vec![false, false, false],
        ));
        let cancel = CancellationToken::new();
        let task = RemoteWorkflowPoller::new(
            source,
            Box::new(move |snapshot| {
                callback_seen
                    .lock()
                    .unwrap()
                    .push(snapshot.workflow_name.clone());
            }),
        )
        .start(cancel.clone());

        cancel_after_states(&seen, 2, &cancel).await;
        task.await.unwrap();
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["before-error", "after-error"]
        );
    }

    #[tokio::test]
    async fn poller_honours_an_initial_snapshot_published_by_its_caller() {
        let source = Arc::new(FakeWorkflowSource::new(
            vec![Ok(None), Ok(None)],
            vec![false],
        ));
        let task = RemoteWorkflowPoller::new(source, Box::new(|_| {}))
            .with_initial_state_seen(true)
            .start(CancellationToken::new());

        tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .expect("a disappeared pre-fetched state must stop polling")
            .expect("poller task must not panic");
    }
}
