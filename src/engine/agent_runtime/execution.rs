//! Cross-paradigm execution types — `AgentInstance` trait + `AgentExecution`
//! type, plus the unified handle/exit/stats shapes shared by both runtime
//! tiers (container-class and sandbox-class).

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::engine::agent_runtime::output_tail::OutputTail;
use crate::engine::error::EngineError;

pub use crate::data::session::AgentHandle;

/// Stats returned by the runtime for a single running agent.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentStats {
    pub name: String,
    pub cpu_percent: f64,
    pub memory_mb: f64,
}

/// The process that launched an agent, when the runtime records one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentOwner {
    pub pid: u32,
    /// Whether the OS still reports `pid` alive at inspection time.
    pub alive: bool,
}

/// Everything a runtime can say about one agent. Common fields are typed;
/// runtime-specific facts go in `runtime_extras` as display-ready
/// `key → value` pairs so Layer 2 never matches on the runtime type.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentDetail {
    pub id: String,
    pub name: String,
    pub image: String,
    pub started_at: DateTime<Utc>,
    pub running: bool,
    /// `None` when `capabilities().reports_resource_stats` is false or the
    /// stats call failed.
    pub cpu_percent: Option<f64>,
    pub memory_mb: Option<f64>,
    /// `None` on runtimes that record no launching process.
    pub owner: Option<AgentOwner>,
    /// Ordered, display-ready extras.
    pub runtime_extras: BTreeMap<String, String>,
}

/// Build an [`AgentDetail`] from a handle and a stats result. Shared by the
/// `AgentRuntimeEngine::inspect` and `ContainerBackend::inspect` defaults.
/// Stats are dropped when the runtime does not measure them or the call failed.
pub fn default_detail(
    reports_resource_stats: bool,
    handle: &AgentHandle,
    stats: Result<AgentStats, EngineError>,
) -> AgentDetail {
    let stats = if reports_resource_stats {
        stats.ok()
    } else {
        None
    };
    AgentDetail {
        id: handle.id.clone(),
        name: handle.name.clone(),
        image: handle.image_tag.clone(),
        started_at: handle.started_at,
        running: true,
        cpu_percent: stats.as_ref().map(|s| s.cpu_percent),
        memory_mb: stats.map(|s| s.memory_mb),
        owner: None,
        runtime_extras: BTreeMap::new(),
    }
}

/// Exit information returned when an agent's execution finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentExitInfo {
    pub exit_code: i32,
    pub signal: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
}

/// Identity preview for a configured-but-not-running agent: name + image,
/// no started-at (the agent hasn't started yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHandlePreview {
    pub id: String,
    pub name: String,
    pub image: String,
}

/// Stuck/unstuck transition published by the engine's stuck detector task.
///
/// Lifecycle: the detector first runs in *grace* mode — it watches for the
/// agent's first byte of output. If grace expires before that byte
/// arrives, `StartupGraceExpired` is published once and the detector
/// kills the agent via its cancel callback, exiting. After the first
/// byte arrives, grace is discarded and the detector switches to the
/// regular `Stuck`/`Unstuck` loop driven by `stuck_timeout`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StuckEvent {
    Stuck,
    Unstuck,
    /// The agent never produced output before its grace window
    /// elapsed. The detector has invoked the cancel callback; subscribers
    /// should treat the step / prompt as failed.
    StartupGraceExpired,
}

/// Fully-built but not-yet-running agent handle. Trait so `Box<dyn>` keeps
/// the runtime's concrete type opaque to callers outside Layer 1.
///
/// This is the first half of the two-step build/run pattern: a runtime's
/// `build()` configures an instance without spawning; the caller can run
/// preparatory side-effects (image checks, overlay validation) and then
/// spawn with the appropriate frontend via `run_with_frontend`.
pub trait AgentInstance: Send + Sync {
    /// Identity preview (name + image) before the agent has started.
    fn handle_preview(&self) -> AgentHandlePreview;

    /// Run the agent with the supplied frontend bound to its I/O. Consumes
    /// `self` and produces an `AgentExecution` that the caller awaits.
    /// A runtime MUST release the frontend by the time `wait()` resolves,
    /// its future is dropped, or the execution is dropped.
    fn run_with_frontend(
        self: Box<Self>,
        frontend: Box<dyn AgentFrontend>,
    ) -> Result<AgentExecution, EngineError>;
}

/// Carry a launch's structured stdout protocol through a runtime-neutral
/// instance. Announce it before the backend can report Running or take I/O.
pub fn with_output_format(
    instance: Box<dyn AgentInstance>,
    format: crate::data::agent_output_format::AgentOutputFormat,
) -> Box<dyn AgentInstance> {
    if !format.is_structured() {
        return instance;
    }
    Box::new(OutputFormatInstance { instance, format })
}

struct OutputFormatInstance {
    instance: Box<dyn AgentInstance>,
    format: crate::data::agent_output_format::AgentOutputFormat,
}

impl AgentInstance for OutputFormatInstance {
    fn handle_preview(&self) -> AgentHandlePreview {
        self.instance.handle_preview()
    }

    fn run_with_frontend(
        self: Box<Self>,
        mut frontend: Box<dyn AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        frontend.set_agent_output_format(self.format);
        self.instance.run_with_frontend(frontend)
    }
}

/// "Fully prepared, ready-to-run agent handle" — the type passed by
/// Layer 2 to `WorkflowEngine` without leaking backend or frontend details.
pub struct AgentExecution {
    handle: AgentHandle,
    inner: ExecutionState,
    stuck_tx: Arc<tokio::sync::broadcast::Sender<StuckEvent>>,
    /// Rolling buffer of the container's recent combined stdout/stderr, fed by
    /// the I/O bridge. `None` for paradigms without a byte-stream bridge (e.g.
    /// sandbox-class runtimes) and for the inert/test constructor. The workflow
    /// engine reads this after `wait()` to persist a failure log when a
    /// container exits unexpectedly.
    output_tail: Option<Arc<OutputTail>>,
}

enum ExecutionState {
    Running(Box<dyn ExecutionBackend>),
    Finished(AgentExitInfo),
    Detached,
}

/// Exit code recorded when the engine kills a container without waiting
/// for its real exit status (128 + SIGKILL, the code `docker kill` yields).
pub const KILLED_EXIT_CODE: i32 = 137;

/// Standalone cancel handle — extracted before `wait()` moves the backend,
/// so the engine can cancel an agent mid-step while the wait future is
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
/// Not pub outside the engine crate.
pub(crate) trait ExecutionBackend: Send {
    fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError>;
    fn cancel(&self) -> Result<(), EngineError>;

    fn frontend_for_wait(&mut self) -> Option<FrontendWaitGuard> {
        None
    }

    /// Return a standalone cancel handle that works after `wait()` moves the
    /// backend into a blocking task. Every backend must provide one so a
    /// dropped wait future can stop the child and release its frontend.
    fn cancel_handle(&self) -> CancelHandle;

    /// Whether dropping the owning `AgentExecution` should cancel the child.
    /// Reattachable runtimes override this because process shutdown must not
    /// turn ownership release into container removal.
    fn cancel_on_execution_drop(&self) -> bool {
        true
    }

    /// Best-effort: push raw bytes into the running agent's stdin.
    ///
    /// Used by `WorkflowEngine` for the `ContinueInCurrentContainer` advance
    /// — the next step's prompt is written into the still-running PTY rather
    /// than spawning a fresh agent. Returns `Ok(false)` when the backend
    /// cannot inject (e.g. inherit-stdio with no PTY bridge), in which case
    /// the engine falls back to a fresh launch.
    fn try_inject_stdin(&self, _bytes: &[u8]) -> Result<bool, EngineError> {
        Ok(false)
    }
}

pub(crate) struct FrontendWaitGuard {
    frontend: Arc<std::sync::Mutex<Box<dyn AgentFrontend>>>,
    cleanup: Option<Box<dyn FnOnce() + Send>>,
}

impl FrontendWaitGuard {
    pub(crate) fn new(frontend: Box<dyn AgentFrontend>) -> Self {
        Self {
            frontend: Arc::new(std::sync::Mutex::new(frontend)),
            cleanup: None,
        }
    }

    pub(crate) fn from_shared(
        frontend: Arc<std::sync::Mutex<Box<dyn AgentFrontend>>>,
        cleanup: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            frontend,
            cleanup: Some(Box::new(cleanup)),
        }
    }

    pub(crate) fn downgrade(&self) -> std::sync::Weak<std::sync::Mutex<Box<dyn AgentFrontend>>> {
        Arc::downgrade(&self.frontend)
    }
}

impl Drop for FrontendWaitGuard {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

impl AgentExecution {
    pub(crate) fn new(
        handle: AgentHandle,
        backend: Box<dyn ExecutionBackend>,
        stuck_tx: Arc<tokio::sync::broadcast::Sender<StuckEvent>>,
        output_tail: Option<Arc<OutputTail>>,
    ) -> Self {
        Self {
            handle,
            inner: ExecutionState::Running(backend),
            stuck_tx,
            output_tail,
        }
    }

    /// Construct a pre-finished execution. Test-only.
    #[cfg(test)]
    pub(crate) fn finished(handle: AgentHandle, info: AgentExitInfo) -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(4);
        Self {
            handle,
            inner: ExecutionState::Finished(info),
            stuck_tx: Arc::new(tx),
            output_tail: None,
        }
    }

    /// Shared handle to this execution's rolling output tail, if the runtime
    /// captures one. Cloneable and readable after `wait()` resolves.
    pub fn output_tail(&self) -> Option<Arc<OutputTail>> {
        self.output_tail.clone()
    }

    /// Construct a pre-finished execution carrying an output tail. Test-only:
    /// lets engine tests exercise the container failure-log path without a live
    /// runtime.
    #[cfg(test)]
    pub(crate) fn finished_with_tail(
        handle: AgentHandle,
        info: AgentExitInfo,
        output_tail: Option<Arc<OutputTail>>,
    ) -> Self {
        let mut execution = Self::finished(handle, info);
        execution.output_tail = output_tail;
        execution
    }

    pub fn handle(&self) -> &AgentHandle {
        &self.handle
    }

    /// Subscribe to stuck/unstuck transitions for this agent's output.
    /// Multiple subscribers are supported (broadcast semantics).
    pub fn subscribe_stuck(&self) -> tokio::sync::broadcast::Receiver<StuckEvent> {
        self.stuck_tx.subscribe()
    }

    /// Return the stuck broadcast sender so external parties (e.g. TUI) can
    /// subscribe independently.
    pub fn stuck_sender(&self) -> Arc<tokio::sync::broadcast::Sender<StuckEvent>> {
        self.stuck_tx.clone()
    }

    /// Block until the agent exits. A runtime MUST release its frontend no
    /// later than when this resolves, when this future is dropped, or when
    /// the execution is dropped. Dropping an in-flight wait requests backend
    /// cancellation on a background thread so blocking host commands cannot
    /// stall a Tokio worker. Dropping an unwaited execution requests
    /// cancellation only from backends that opt in; Docker opts out to keep
    /// its reattach-after-owner-exit behavior when no wait is active. Call
    /// [`Self::detach`] before handing off work that may outlive this owner.
    pub async fn wait(&mut self) -> Result<AgentExitInfo, EngineError> {
        // Temporarily replace with Detached while we run the future so that the
        // execution is in a safe state if the task is dropped mid-await.
        match std::mem::replace(&mut self.inner, ExecutionState::Detached) {
            ExecutionState::Running(mut backend) => {
                let _frontend_guard = backend.frontend_for_wait();
                let mut cancellation = WaitCancellation(Some(backend.cancel_handle()));
                let info = tokio::task::spawn_blocking(move || backend.wait_blocking())
                    .await
                    .map_err(|e| EngineError::Other(format!("execution join error: {e}")))?;
                cancellation.0 = None;
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

    /// Best-effort cancel the running agent. No-op when already finished
    /// or detached.
    pub fn cancel(&self) -> Result<(), EngineError> {
        match &self.inner {
            ExecutionState::Running(b) => b.cancel(),
            _ => Ok(()),
        }
    }

    /// Extract a standalone cancel handle. Must be called before `wait()`
    /// which moves the backend into a blocking task. Returns `None` when the
    /// execution is not in Running state.
    pub fn cancel_handle(&self) -> Option<CancelHandle> {
        match &self.inner {
            ExecutionState::Running(b) => Some(b.cancel_handle()),
            _ => None,
        }
    }

    /// Attempt to push raw bytes into the running agent's stdin.
    ///
    /// `WorkflowEngine` calls this for `ContinueInCurrentContainer` to inject
    /// the next step's prompt without spawning a new agent. Returns
    /// `Ok(false)` when the backend can't inject (no PTY bridge, already
    /// finished/detached) — the engine will then fall back to launching a
    /// fresh agent.
    pub fn try_inject_stdin(&self, bytes: &[u8]) -> Result<bool, EngineError> {
        match &self.inner {
            ExecutionState::Running(b) => b.try_inject_stdin(bytes),
            _ => Ok(false),
        }
    }

    /// Hand ownership of the running agent back to the caller without
    /// joining. Useful for API background mode.
    pub fn detach(mut self) -> AgentHandle {
        self.inner = ExecutionState::Detached;
        self.handle.clone()
    }
}

struct WaitCancellation(Option<CancelHandle>);

impl Drop for WaitCancellation {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            // Backend cancellation may synchronously invoke a host CLI. Drop
            // can run on a Tokio worker, so never execute it on this thread.
            std::thread::spawn(move || {
                let _ = cancel.cancel();
            });
        }
    }
}

impl Drop for AgentExecution {
    fn drop(&mut self) {
        if let ExecutionState::Running(backend) = &self.inner {
            if backend.cancel_on_execution_drop() {
                let cancel = backend.cancel_handle();
                std::thread::spawn(move || {
                    let _ = cancel.cancel();
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::container::instance::{handle_now, ContainerId};
    use crate::engine::container::options::{ContainerName, ImageRef};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct ReleaseProbe(Arc<AtomicBool>);

    impl Drop for ReleaseProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct WaitingBackend {
        done: std::sync::mpsc::Receiver<AgentExitInfo>,
        cancel: std::sync::mpsc::Sender<AgentExitInfo>,
        wait_started: std::sync::mpsc::Sender<()>,
        _frontend: ReleaseProbe,
    }

    impl ExecutionBackend for WaitingBackend {
        fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
            let _ = self.wait_started.send(());
            self.done
                .recv()
                .map_err(|error| EngineError::Other(error.to_string()))
        }

        fn cancel(&self) -> Result<(), EngineError> {
            let _ = self.cancel.send(make_exit_info(KILLED_EXIT_CODE));
            Ok(())
        }

        fn cancel_handle(&self) -> CancelHandle {
            let cancel = self.cancel.clone();
            CancelHandle::new(move || {
                let _ = cancel.send(make_exit_info(KILLED_EXIT_CODE));
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn wait_releases_fake_backend_after_normal_and_dropped_wait() {
        for drop_wait in [false, true] {
            let released = Arc::new(AtomicBool::new(false));
            let (done, received) = std::sync::mpsc::channel();
            let (started, wait_started) = std::sync::mpsc::channel();
            let (stuck_tx, _) = tokio::sync::broadcast::channel(4);
            let mut execution = AgentExecution::new(
                make_handle(),
                Box::new(WaitingBackend {
                    done: received,
                    cancel: done.clone(),
                    wait_started: started,
                    _frontend: ReleaseProbe(released.clone()),
                }),
                Arc::new(stuck_tx),
                None,
            );
            if drop_wait {
                let task = tokio::spawn(async move { execution.wait().await });
                tokio::task::spawn_blocking(move || {
                    wait_started
                        .recv_timeout(std::time::Duration::from_secs(2))
                        .expect("wait_blocking must start before cancellation");
                })
                .await
                .unwrap();
                task.abort();
                let _ = task.await;
            } else {
                done.send(make_exit_info(0)).unwrap();
                execution.wait().await.unwrap();
            }
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !released.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("blocking backend retained frontend after wait");
        }
    }

    #[test]
    fn dropped_wait_cancellation_runs_outside_the_dropping_thread() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = std::sync::Mutex::new(release_rx);
        let cancellation = WaitCancellation(Some(CancelHandle::new(move || {
            let _ = entered_tx.send(());
            let _ = release_rx.lock().unwrap().recv();
            Ok(())
        })));
        drop(cancellation);
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("cancellation closure did not start");
        release_tx.send(()).unwrap();
    }

    #[test]
    fn dropping_unwaited_execution_releases_frontend() {
        let released = Arc::new(AtomicBool::new(false));
        let (done, received) = std::sync::mpsc::channel();
        let (started, _wait_started) = std::sync::mpsc::channel();
        let (stuck_tx, _) = tokio::sync::broadcast::channel(4);
        let execution = AgentExecution::new(
            make_handle(),
            Box::new(WaitingBackend {
                done: received,
                cancel: done,
                wait_started: started,
                _frontend: ReleaseProbe(released.clone()),
            }),
            Arc::new(stuck_tx),
            None,
        );
        drop(execution);
        assert!(released.load(Ordering::SeqCst));
    }

    fn make_handle() -> AgentHandle {
        let id = ContainerId::new("test-container-id");
        let name = ContainerName::new("test-name");
        let image = ImageRef::new("test-image:latest");
        handle_now(&id, &name, &image)
    }

    fn make_exit_info(exit_code: i32) -> AgentExitInfo {
        let now = Utc::now();
        AgentExitInfo {
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
        let mut execution = AgentExecution::finished(handle, info);
        let result = execution.wait().await.expect("wait should succeed");
        assert_eq!(result.exit_code, 42);
    }

    #[tokio::test]
    async fn wait_is_idempotent_on_finished_execution() {
        let handle = make_handle();
        let info = make_exit_info(7);
        let mut execution = AgentExecution::finished(handle, info);
        let r1 = execution.wait().await.unwrap();
        let r2 = execution.wait().await.unwrap();
        assert_eq!(r1.exit_code, 7);
        assert_eq!(r2.exit_code, 7);
    }

    #[tokio::test]
    async fn cancel_on_finished_is_noop() {
        let handle = make_handle();
        let info = make_exit_info(0);
        let execution = AgentExecution::finished(handle, info);
        assert!(execution.cancel().is_ok());
    }

    #[tokio::test]
    async fn detach_returns_handle() {
        let handle = make_handle();
        let original_id = handle.id.clone();
        let info = make_exit_info(0);
        let execution = AgentExecution::finished(handle, info);
        let returned_handle = execution.detach();
        assert_eq!(returned_handle.id, original_id);
    }

    #[tokio::test]
    async fn subscribe_stuck_receives_events() {
        let handle = make_handle();
        let info = make_exit_info(0);
        let execution = AgentExecution::finished(handle, info);
        let mut rx = execution.subscribe_stuck();
        let _ = execution.stuck_tx.send(StuckEvent::Stuck);
        let event = rx.recv().await.unwrap();
        assert_eq!(event, StuckEvent::Stuck);
    }

    /// Two independent receivers from the same `AgentExecution` both
    /// receive every Stuck/Unstuck event (broadcast semantics).
    #[tokio::test]
    async fn subscribe_stuck_two_receivers_both_get_same_events() {
        let handle = make_handle();
        let info = make_exit_info(0);
        let execution = AgentExecution::finished(handle, info);

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
        let execution = AgentExecution::finished(handle, info);

        let sender = execution.stuck_sender();
        let mut rx_a = execution.subscribe_stuck();
        let mut rx_b = sender.subscribe();

        let _ = sender.send(StuckEvent::Stuck);

        assert_eq!(rx_a.recv().await.unwrap(), StuckEvent::Stuck);
        assert_eq!(rx_b.recv().await.unwrap(), StuckEvent::Stuck);
    }
}

#[cfg(test)]
mod detail_tests {
    use super::*;

    fn handle() -> AgentHandle {
        AgentHandle {
            id: "abc".into(),
            name: "awman-x".into(),
            image_tag: "img:latest".into(),
            started_at: chrono::DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
        }
    }

    fn stats() -> AgentStats {
        AgentStats {
            name: "awman-x".into(),
            cpu_percent: 12.5,
            memory_mb: 256.0,
        }
    }

    #[test]
    fn default_detail_copies_identity_and_stats() {
        let h = handle();
        let d = default_detail(true, &h, Ok(stats()));
        assert_eq!(
            (d.id.as_str(), d.name.as_str(), d.image.as_str()),
            ("abc", "awman-x", "img:latest")
        );
        assert_eq!(d.started_at, h.started_at);
        assert!(d.running);
        assert_eq!((d.cpu_percent, d.memory_mb), (Some(12.5), Some(256.0)));
        assert!(d.owner.is_none());
        assert!(d.runtime_extras.is_empty());
    }

    #[test]
    fn default_detail_drops_stats_the_runtime_does_not_measure() {
        // SBX-style: the stats call "succeeds" with zeros nobody measured.
        let zeros = AgentStats {
            cpu_percent: 0.0,
            memory_mb: 0.0,
            ..stats()
        };
        let d = default_detail(false, &handle(), Ok(zeros));
        assert_eq!((d.cpu_percent, d.memory_mb), (None, None));
    }

    #[test]
    fn default_detail_turns_a_stats_error_into_none() {
        let d = default_detail(true, &handle(), Err(EngineError::Other("boom".into())));
        assert_eq!((d.cpu_percent, d.memory_mb), (None, None));
        assert!(d.running, "a stats failure does not change liveness");
    }
}
