//! `ContainerInstance` trait + `ContainerExecution` type.

use std::sync::Arc;
use std::time::SystemTime;

use chrono::{DateTime, Utc};

use crate::data::session::ContainerHandle;
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::container::options::{ContainerName, ImageRef};
use crate::engine::error::EngineError;

/// Identity-only handle to a container ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerId(pub String);

impl ContainerId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stats returned by the runtime for a single container.
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerStats {
    pub name: String,
    pub cpu_percent: f64,
    pub memory_mb: f64,
}

/// Exit information returned when a container's execution finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerExitInfo {
    pub exit_code: i32,
    pub signal: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
}

/// Stuck/unstuck transition published by the container engine's stuck
/// detector task.
///
/// Lifecycle: the detector first runs in *grace* mode — it watches for the
/// container's first byte of output. If grace expires before that byte
/// arrives, `StartupGraceExpired` is published once and the detector
/// kills the container via its cancel callback, exiting. After the first
/// byte arrives, grace is discarded and the detector switches to the
/// regular `Stuck`/`Unstuck` loop driven by `stuck_timeout`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StuckEvent {
    Stuck,
    Unstuck,
    /// The container never produced output before its grace window
    /// elapsed. The detector has invoked the cancel callback; subscribers
    /// should treat the step / prompt as failed.
    StartupGraceExpired,
}

/// Fully-built but not-yet-running container handle. Trait so `Box<dyn>` keeps
/// the backend type opaque to callers outside `src/engine/container/`.
pub trait ContainerInstance: Send + Sync {
    fn id(&self) -> &ContainerId;
    fn name(&self) -> &ContainerName;
    fn image(&self) -> &ImageRef;

    /// Run the container with the supplied frontend bound to its I/O. Consumes
    /// `self` and produces a `ContainerExecution` that the caller awaits.
    fn run_with_frontend(
        self: Box<Self>,
        frontend: Box<dyn ContainerFrontend>,
    ) -> Result<ContainerExecution, EngineError>;
}

/// "Fully prepared, ready-to-run container handle" — the type passed by
/// Layer 2 to `WorkflowEngine` without leaking backend or frontend details.
pub struct ContainerExecution {
    handle: ContainerHandle,
    inner: ExecutionState,
    stuck_tx: Arc<tokio::sync::broadcast::Sender<StuckEvent>>,
}

enum ExecutionState {
    Running(Box<dyn ExecutionBackend>),
    Finished(ContainerExitInfo),
    Detached,
}

/// Standalone cancel handle — extracted before `wait()` moves the backend,
/// so the engine can cancel a container mid-step while the wait future is
/// in flight. Backends produce these via `ExecutionBackend::cancel_handle`.
pub struct CancelHandle(Box<dyn Fn() -> Result<(), EngineError> + Send + Sync>);

impl CancelHandle {
    pub fn new(f: impl Fn() -> Result<(), EngineError> + Send + Sync + 'static) -> Self {
        Self(Box::new(f))
    }
    pub fn cancel(&self) -> Result<(), EngineError> {
        (self.0)()
    }
}

/// Internal trait — the concrete execution wrapper that backends produce.
/// Not pub outside `src/engine/container/`.
pub(crate) trait ExecutionBackend: Send {
    fn wait_blocking(self: Box<Self>) -> Result<ContainerExitInfo, EngineError>;
    fn cancel(&self) -> Result<(), EngineError>;

    /// Return a standalone cancel handle that works even after `wait()` has
    /// moved the backend into a blocking task. Default returns `None` for
    /// backends that don't support mid-step cancellation.
    fn cancel_handle(&self) -> Option<CancelHandle> {
        None
    }

    /// Best-effort: push raw bytes into the running container's stdin.
    ///
    /// Used by `WorkflowEngine` for the `ContinueInCurrentContainer` advance
    /// — the next step's prompt is written into the still-running PTY rather
    /// than spawning a fresh container. Returns `Ok(false)` when the backend
    /// cannot inject (e.g. inherit-stdio with no PTY bridge), in which case
    /// the engine falls back to a fresh container launch.
    fn try_inject_stdin(&self, _bytes: &[u8]) -> Result<bool, EngineError> {
        Ok(false)
    }
}

impl ContainerExecution {
    pub(crate) fn new(
        handle: ContainerHandle,
        backend: Box<dyn ExecutionBackend>,
        stuck_tx: Arc<tokio::sync::broadcast::Sender<StuckEvent>>,
    ) -> Self {
        Self {
            handle,
            inner: ExecutionState::Running(backend),
            stuck_tx,
        }
    }

    /// Construct a pre-finished execution (used by the inert backend below
    /// and by tests).
    pub(crate) fn finished(handle: ContainerHandle, info: ContainerExitInfo) -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(4);
        Self {
            handle,
            inner: ExecutionState::Finished(info),
            stuck_tx: Arc::new(tx),
        }
    }

    pub fn handle(&self) -> &ContainerHandle {
        &self.handle
    }

    /// Subscribe to stuck/unstuck transitions for this container's output.
    /// Multiple subscribers are supported (broadcast semantics).
    pub fn subscribe_stuck(&self) -> tokio::sync::broadcast::Receiver<StuckEvent> {
        self.stuck_tx.subscribe()
    }

    /// Return the stuck broadcast sender so external parties (e.g. TUI) can
    /// subscribe independently.
    pub fn stuck_sender(&self) -> Arc<tokio::sync::broadcast::Sender<StuckEvent>> {
        self.stuck_tx.clone()
    }

    /// Block until the container exits. Transitions the execution to `Finished`
    /// state; the execution remains in scope so callers can pass it to
    /// `inject_prompt` afterwards.
    pub async fn wait(&mut self) -> Result<ContainerExitInfo, EngineError> {
        // Temporarily replace with Detached while we run the future so that the
        // execution is in a safe state if the task is dropped mid-await.
        match std::mem::replace(&mut self.inner, ExecutionState::Detached) {
            ExecutionState::Running(backend) => {
                let info = tokio::task::spawn_blocking(move || backend.wait_blocking())
                    .await
                    .map_err(|e| EngineError::Other(format!("execution join error: {e}")))?;
                let info = info?;
                self.inner = ExecutionState::Finished(info.clone());
                Ok(info)
            }
            ExecutionState::Finished(info) => {
                self.inner = ExecutionState::Finished(info.clone());
                Ok(info)
            }
            ExecutionState::Detached => Err(EngineError::Other(
                "cannot wait on a detached execution".into(),
            )),
        }
    }

    /// Best-effort cancel the running container. No-op when already finished
    /// or detached.
    pub fn cancel(&self) -> Result<(), EngineError> {
        match &self.inner {
            ExecutionState::Running(b) => b.cancel(),
            _ => Ok(()),
        }
    }

    /// Extract a standalone cancel handle. Must be called before `wait()`
    /// which moves the backend into a blocking task. Returns `None` when the
    /// execution is not in Running state or the backend doesn't support it.
    pub fn cancel_handle(&self) -> Option<CancelHandle> {
        match &self.inner {
            ExecutionState::Running(b) => b.cancel_handle(),
            _ => None,
        }
    }

    /// Attempt to push raw bytes into the running container's stdin.
    ///
    /// `WorkflowEngine` calls this for `ContinueInCurrentContainer` to inject
    /// the next step's prompt without spawning a new container. Returns
    /// `Ok(false)` when the backend can't inject (no PTY bridge, already
    /// finished/detached) — the engine will then fall back to launching a
    /// fresh container.
    pub fn try_inject_stdin(&self, bytes: &[u8]) -> Result<bool, EngineError> {
        match &self.inner {
            ExecutionState::Running(b) => b.try_inject_stdin(bytes),
            _ => Ok(false),
        }
    }

    /// Hand ownership of the running container back to the caller without
    /// joining. Useful for API background mode.
    pub fn detach(mut self) -> ContainerHandle {
        self.inner = ExecutionState::Detached;
        self.handle
    }
}

/// Helper: build a `ContainerHandle` from the assembled facts.
pub(crate) fn handle_now(
    id: &ContainerId,
    name: &ContainerName,
    image: &ImageRef,
) -> ContainerHandle {
    ContainerHandle {
        id: id.0.clone(),
        image_tag: image.0.clone(),
        name: name.0.clone(),
        started_at: chrono::DateTime::<Utc>::from(SystemTime::now()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::container::options::{ContainerName, ImageRef};

    fn make_handle() -> ContainerHandle {
        let id = ContainerId::new("test-container-id");
        let name = ContainerName::new("test-name");
        let image = ImageRef::new("test-image:latest");
        handle_now(&id, &name, &image)
    }

    fn make_exit_info(exit_code: i32) -> ContainerExitInfo {
        let now = Utc::now();
        ContainerExitInfo {
            exit_code,
            signal: None,
            started_at: now,
            ended_at: now,
        }
    }

    #[tokio::test]
    async fn wait_on_finished_returns_exit_info() {
        let handle = make_handle();
        let info = make_exit_info(42);
        let mut execution = ContainerExecution::finished(handle, info);
        let result = execution.wait().await.expect("wait should succeed");
        assert_eq!(result.exit_code, 42);
    }

    #[tokio::test]
    async fn wait_is_idempotent_on_finished_execution() {
        let handle = make_handle();
        let info = make_exit_info(7);
        let mut execution = ContainerExecution::finished(handle, info);
        let r1 = execution.wait().await.unwrap();
        let r2 = execution.wait().await.unwrap();
        assert_eq!(r1.exit_code, 7);
        assert_eq!(r2.exit_code, 7);
    }

    #[tokio::test]
    async fn cancel_on_finished_is_noop() {
        let handle = make_handle();
        let info = make_exit_info(0);
        let execution = ContainerExecution::finished(handle, info);
        assert!(execution.cancel().is_ok());
    }

    #[tokio::test]
    async fn detach_returns_handle() {
        let handle = make_handle();
        let original_id = handle.id.clone();
        let info = make_exit_info(0);
        let execution = ContainerExecution::finished(handle, info);
        let returned_handle = execution.detach();
        assert_eq!(returned_handle.id, original_id);
    }

    #[tokio::test]
    async fn subscribe_stuck_receives_events() {
        let handle = make_handle();
        let info = make_exit_info(0);
        let execution = ContainerExecution::finished(handle, info);
        let mut rx = execution.subscribe_stuck();
        let _ = execution.stuck_tx.send(StuckEvent::Stuck);
        let event = rx.recv().await.unwrap();
        assert_eq!(event, StuckEvent::Stuck);
    }

    /// Two independent receivers from the same `ContainerExecution` both
    /// receive every Stuck/Unstuck event (broadcast semantics).
    #[tokio::test]
    async fn subscribe_stuck_two_receivers_both_get_same_events() {
        let handle = make_handle();
        let info = make_exit_info(0);
        let execution = ContainerExecution::finished(handle, info);

        let mut rx1 = execution.subscribe_stuck();
        let mut rx2 = execution.subscribe_stuck();

        // Publish two events via the stored sender.
        let _ = execution.stuck_tx.send(StuckEvent::Stuck);
        let _ = execution.stuck_tx.send(StuckEvent::Unstuck);

        // Both receivers must see both events in order.
        let (a1, a2) = (rx1.recv().await.unwrap(), rx1.recv().await.unwrap());
        let (b1, b2) = (rx2.recv().await.unwrap(), rx2.recv().await.unwrap());

        assert_eq!(a1, StuckEvent::Stuck, "rx1 first event must be Stuck");
        assert_eq!(a2, StuckEvent::Unstuck, "rx1 second event must be Unstuck");
        assert_eq!(b1, StuckEvent::Stuck, "rx2 first event must be Stuck");
        assert_eq!(b2, StuckEvent::Unstuck, "rx2 second event must be Unstuck");
    }

    /// `stuck_sender()` returns the same underlying channel so its subscribers
    /// also receive events sent through the stored `stuck_tx`.
    #[tokio::test]
    async fn stuck_sender_shares_channel_with_subscribe_stuck() {
        let handle = make_handle();
        let info = make_exit_info(0);
        let execution = ContainerExecution::finished(handle, info);

        let sender = execution.stuck_sender();
        let mut rx_a = execution.subscribe_stuck();
        let mut rx_b = sender.subscribe();

        let _ = sender.send(StuckEvent::Stuck);

        assert_eq!(rx_a.recv().await.unwrap(), StuckEvent::Stuck);
        assert_eq!(rx_b.recv().await.unwrap(), StuckEvent::Stuck);
    }
}
