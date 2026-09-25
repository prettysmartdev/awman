//! Hermetic doubles for the builtin backend: an in-memory `SandboxDriver`
//! and a channel-backed frontend. No VM, hypervisor, SDK or real HOME.
use super::driver::*;
use super::naming;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::session::AgentHandle;
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
use crate::engine::agent_runtime::AgentStats;
use crate::engine::error::EngineError;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A `Control` message as recorded by the double (`Control` itself is not `Clone`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    Stdin(Vec<u8>),
    Eof,
    Resize(u16, u16),
    Interrupt,
    Kill,
}

struct Recorder(Arc<Mutex<Vec<Sent>>>);
impl ExecControl for Recorder {
    fn send(&self, command: Control) -> Result<(), EngineError> {
        self.0.lock().unwrap().push(match command {
            Control::Stdin(bytes) => Sent::Stdin(bytes),
            Control::Eof => Sent::Eof,
            Control::Resize(cols, rows) => Sent::Resize(cols, rows),
            Control::Interrupt => Sent::Interrupt,
            Control::Kill => Sent::Kill,
        });
        Ok(())
    }
}

pub struct FakeSandbox {
    pub spec: SandboxSpec,
    pub created: chrono::DateTime<chrono::Utc>,
    pub stopped: bool,
    pub execs: Vec<ExecRequest>,
    pub events: Option<tokio::sync::broadcast::Sender<ExecEvent>>,
    pub controls: Arc<Mutex<Vec<Sent>>>,
}

#[derive(Default)]
pub struct FakeState {
    pub sandboxes: Vec<FakeSandbox>,
    pub images: HashMap<String, ImageConfigSummary>,
    pub creates: usize,
    /// `(id, owner token, remove)` for every `finish_owned` that was accepted.
    pub finished: Vec<(String, String, bool)>,
    pub removed_images: Vec<String>,
    pub fail_exec: bool,
    pub hypervisor_error: Option<String>,
}

#[derive(Default)]
pub struct FakeDriver {
    pub state: Mutex<FakeState>,
}

impl FakeDriver {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn with_image(self: Arc<Self>, tag: &str, config: ImageConfigSummary) -> Arc<Self> {
        self.state.lock().unwrap().images.insert(tag.into(), config);
        self
    }

    pub fn names(&self) -> Vec<String> {
        let state = self.state.lock().unwrap();
        state
            .sandboxes
            .iter()
            .map(|s| s.spec.name.clone())
            .collect()
    }

    pub fn creates(&self) -> usize {
        self.state.lock().unwrap().creates
    }

    pub fn spec(&self, id: &str) -> SandboxSpec {
        let state = self.state.lock().unwrap();
        state
            .sandboxes
            .iter()
            .find(|s| s.spec.name == id)
            .expect("sandbox exists")
            .spec
            .clone()
    }

    pub fn execs(&self, id: &str) -> Vec<ExecRequest> {
        let state = self.state.lock().unwrap();
        state
            .sandboxes
            .iter()
            .find(|s| s.spec.name == id)
            .map(|s| s.execs.clone())
            .unwrap_or_default()
    }

    pub fn controls(&self, id: &str) -> Vec<Sent> {
        let state = self.state.lock().unwrap();
        state
            .sandboxes
            .iter()
            .find(|s| s.spec.name == id)
            .map(|s| s.controls.lock().unwrap().clone())
            .unwrap_or_default()
    }

    pub fn is_stopped(&self, id: &str) -> Option<bool> {
        let state = self.state.lock().unwrap();
        state
            .sandboxes
            .iter()
            .find(|s| s.spec.name == id)
            .map(|s| s.stopped)
    }

    pub fn finished(&self) -> Vec<(String, String, bool)> {
        self.state.lock().unwrap().finished.clone()
    }

    /// Deliver a guest event to everything subscribed to `id`'s exec stream.
    pub fn emit(&self, id: &str, event: ExecEvent) {
        let state = self.state.lock().unwrap();
        let sandbox = state
            .sandboxes
            .iter()
            .find(|s| s.spec.name == id)
            .expect("sandbox exists");
        let sender = sandbox.events.as_ref().expect("an exec is running");
        let _ = sender.send(event);
    }

    /// Wait until an exec exists for `id` (the launcher runs on another task).
    pub async fn wait_for_exec(&self, id: &str) {
        for _ in 0..400 {
            {
                let state = self.state.lock().unwrap();
                if state
                    .sandboxes
                    .iter()
                    .any(|s| s.spec.name == id && s.events.is_some())
                {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no exec started for {id}");
    }

    fn summary(sandbox: &FakeSandbox) -> SandboxSummary {
        SandboxSummary {
            handle: AgentHandle {
                id: sandbox.spec.name.clone(),
                name: sandbox
                    .spec
                    .labels
                    .get(naming::LABEL_NAME)
                    .cloned()
                    .unwrap_or_default(),
                image_tag: sandbox.spec.image.clone(),
                started_at: sandbox.created,
            },
            labels: sandbox.spec.labels.clone(),
            stopped: sandbox.stopped,
        }
    }
}

impl SandboxDriver for FakeDriver {
    fn hypervisor_available(&self) -> Result<(), EngineError> {
        match &self.state.lock().unwrap().hypervisor_error {
            Some(reason) => Err(EngineError::BuiltinRuntimeUnavailable {
                reason: reason.clone(),
            }),
            None => Ok(()),
        }
    }

    fn create(&self, spec: SandboxSpec) -> Result<(), EngineError> {
        let mut state = self.state.lock().unwrap();
        if state.sandboxes.iter().any(|s| s.spec.name == spec.name) {
            return Err(EngineError::Other("sandbox already exists".into()));
        }
        state.creates += 1;
        state.sandboxes.push(FakeSandbox {
            spec,
            created: chrono::Utc::now(),
            stopped: false,
            execs: Vec::new(),
            events: None,
            controls: Arc::new(Mutex::new(Vec::new())),
        });
        Ok(())
    }

    fn get(&self, id: &str) -> Result<SandboxSummary, EngineError> {
        let state = self.state.lock().unwrap();
        state
            .sandboxes
            .iter()
            .find(|s| s.spec.name == id)
            .map(Self::summary)
            .ok_or_else(|| EngineError::Other("no such sandbox".into()))
    }

    fn list(&self) -> Result<Vec<SandboxSummary>, EngineError> {
        let state = self.state.lock().unwrap();
        Ok(state.sandboxes.iter().map(Self::summary).collect())
    }

    fn exec(&self, id: &str, request: ExecRequest) -> Result<ExecSession, EngineError> {
        let mut state = self.state.lock().unwrap();
        if state.fail_exec {
            return Err(EngineError::Other("exec refused".into()));
        }
        let sandbox = state
            .sandboxes
            .iter_mut()
            .find(|s| s.spec.name == id && !s.stopped)
            .ok_or_else(|| EngineError::Other("no such running sandbox".into()))?;
        sandbox.execs.push(request);
        let (broadcast, events) = tokio::sync::broadcast::channel(256);
        sandbox.events = Some(broadcast.clone());
        Ok(ExecSession {
            events,
            broadcast,
            control: Arc::new(Recorder(sandbox.controls.clone())),
        })
    }

    fn finish_owned(
        &self,
        id: &str,
        owner: &str,
        _grace: Duration,
        remove: bool,
    ) -> Result<(), EngineError> {
        let mut state = self.state.lock().unwrap();
        let position = state
            .sandboxes
            .iter()
            .position(|s| s.spec.name == id)
            .ok_or_else(|| EngineError::Other("no such sandbox".into()))?;
        naming::check_owner(&state.sandboxes[position].spec.labels, owner)?;
        state.sandboxes[position].stopped = true;
        state.finished.push((id.into(), owner.into(), remove));
        if remove {
            state.sandboxes.remove(position);
        }
        Ok(())
    }

    fn remove_stopped(&self, id: &str) -> Result<(), EngineError> {
        let mut state = self.state.lock().unwrap();
        let position = state
            .sandboxes
            .iter()
            .position(|s| s.spec.name == id)
            .ok_or_else(|| EngineError::Other("no such sandbox".into()))?;
        if !state.sandboxes[position].stopped {
            return Err(EngineError::Other("sandbox is still running".into()));
        }
        state.sandboxes.remove(position);
        Ok(())
    }

    fn stats(&self, id: &str) -> Result<AgentStats, EngineError> {
        self.get(id).map(|s| AgentStats {
            name: s.handle.name,
            cpu_percent: 0.0,
            memory_mb: 0.0,
        })
    }

    fn image_config(&self, tag: &str) -> Result<Option<ImageConfigSummary>, EngineError> {
        Ok(self.state.lock().unwrap().images.get(tag).cloned())
    }

    fn import_archive(&self, _path: PathBuf, tag: String) -> Result<(), EngineError> {
        self.state.lock().unwrap().images.entry(tag).or_default();
        Ok(())
    }

    fn remove_image(&self, tag: &str) -> Result<(), EngineError> {
        let mut state = self.state.lock().unwrap();
        state.images.remove(tag);
        state.removed_images.push(tag.into());
        Ok(())
    }
}

/// Everything a test needs to drive one frontend-bound execution.
pub struct Peer {
    pub stdout: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    pub stderr: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    pub stdin: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    pub resize: Option<tokio::sync::mpsc::UnboundedSender<(u16, u16)>>,
    pub statuses: Arc<Mutex<Vec<AgentStatus>>>,
}

pub struct TestFrontend {
    io: Option<AgentIo>,
    statuses: Arc<Mutex<Vec<AgentStatus>>>,
}

/// A frontend whose I/O is `pty: Some(size)` (PTY path) or piped (`None`).
pub fn frontend(pty: Option<(u16, u16)>) -> (Box<TestFrontend>, Peer) {
    let (stdout_tx, stdout) = tokio::sync::mpsc::unbounded_channel();
    let (stderr_tx, stderr) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel();
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let io = AgentIo {
        stdout: stdout_tx,
        stderr: stderr_tx,
        stdin_tx: stdin_tx.clone(),
        stdin_rx,
        resize: pty.map(|_| resize_rx),
        initial_size: pty,
    };
    (
        Box::new(TestFrontend {
            io: Some(io),
            statuses: statuses.clone(),
        }),
        Peer {
            stdout,
            stderr,
            stdin: stdin_tx,
            resize: pty.map(|_| resize_tx),
            statuses,
        },
    )
}

impl UserMessageSink for TestFrontend {
    fn write_message(&mut self, _message: UserMessage) {}
    fn replay_queued(&mut self) {}
}

#[async_trait::async_trait]
impl AgentFrontend for TestFrontend {
    fn report_status(&mut self, status: AgentStatus) {
        self.statuses.lock().unwrap().push(status);
    }
    fn report_progress(&mut self, _progress: AgentProgress) {}
    fn take_io(&mut self) -> AgentIo {
        self.io.take().expect("I/O is taken once")
    }
    fn grace_timeout(&self) -> Duration {
        Duration::from_secs(3600)
    }
    fn stuck_timeout(&self) -> Duration {
        Duration::from_secs(3600)
    }
}

/// An image config resembling the spike fixture: named non-root USER, HOME,
/// WORKDIR, entrypoint script and account database.
pub fn fixture_image() -> ImageConfigSummary {
    ImageConfigSummary {
        home: Some("/home/probe".into()),
        user: Some("probe".into()),
        workdir: Some("/image-work".into()),
        argv: vec!["/compat/entrypoint".into(), "/compat/check-image".into()],
        accounts: ImageAccounts {
            passwd: Some(
                "root:x:0:0:root:/root:/bin/sh\nprobe:x:1234:1234:probe:/home/probe:/bin/sh\n"
                    .into(),
            ),
            group: Some("root:x:0:\nprobe:x:1234:\n".into()),
        },
    }
}

/// A private state directory under `/tmp` (short enough for the socket budget).
pub fn state_dir() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let state = temp.path().canonicalize().unwrap().join("s");
    (temp, state)
}

/// A recording `ExecControl` plus the log it writes to.
pub fn recorder() -> (Arc<dyn ExecControl>, Arc<Mutex<Vec<Sent>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    (Arc::new(Recorder(log.clone())), log)
}

/// Poll `condition` until it holds (3 s budget); panics with `what` otherwise.
pub async fn eventually(what: &str, condition: impl Fn() -> bool) {
    for _ in 0..600 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for: {what}");
}

impl FakeDriver {
    /// Live subscribers on `id`'s exec stream (the launcher plus attach clients).
    pub fn receivers(&self, id: &str) -> usize {
        let state = self.state.lock().unwrap();
        state
            .sandboxes
            .iter()
            .find(|s| s.spec.name == id)
            .and_then(|s| s.events.as_ref().map(|e| e.receiver_count()))
            .unwrap_or(0)
    }
}
