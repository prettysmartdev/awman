//! `AgentFrontend` impls for the TUI — both on `InstanceCommandFrontend`
//! (direct container I/O) and on a standalone `InstanceContainerProxy` (used by
//! `container_frontend()` return values in Init/Ready).
//!
//! For TUI mode, the engine's container backend takes ownership of the byte
//! channels via `take_io` and bridges them directly to the
//! container's PTY master or piped stdio.

use async_trait::async_trait;

use crate::command::instance::feeds::{SharedStatusLog, StatusLogEntry};
use crate::command::instance::frontend::InstanceCommandFrontend;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};

// ─── AgentFrontend for InstanceCommandFrontend ────────────────────────────

#[async_trait]
impl AgentFrontend for InstanceCommandFrontend {
    fn set_agent_output_format(
        &mut self,
        format: crate::data::agent_output_format::AgentOutputFormat,
    ) {
        if let Some(recorder) = &self.worker_events {
            recorder.output_format(format);
        }
    }
    fn report_status(&mut self, status: AgentStatus) {
        if matches!(status, AgentStatus::Running { .. }) {
            if let Some(recorder) = &self.worker_events {
                recorder.writer.record_container_started();
            }
        }
        if self.is_cli() {
            return;
        }
        if let AgentStatus::Running { ref container_name } = status {
            if !self.parallel_group_active {
                self.container.running(container_name.clone());
            }
        }
        self.messages.info(format!("Container: {status:?}"));
    }

    fn report_progress(&mut self, progress: AgentProgress) {
        if self.is_cli() {
            return;
        }
        self.messages
            .info(format!("{}: {}", progress.stage, progress.message));
    }

    fn take_io(&mut self) -> AgentIo {
        if self.is_headless_cli() {
            let io = self.command_bytes.pipe(self.wake.clone());
            return match &self.worker_events {
                Some(recorder) => recorder.io(io),
                None => io,
            };
        }
        // The slot is normally refilled by `report_step_interactive_launch`
        // before each container launch. Launch paths that skip that hook
        // (e.g. a remediation agent) must not crash the whole command task —
        // build fresh channels so the container still bridges into the TUI.
        if self.container_io.is_none() {
            self.recreate_container_io();
        }
        let io = self
            .container_io
            .take()
            .expect("recreate_container_io always fills the slot");
        match &self.worker_events {
            Some(recorder) => recorder.io(io),
            None => io,
        }
    }
}

// ─── InstanceContainerProxy ──────────────────────────────────────────────────

/// Standalone proxy returned by `container_frontend()` in Init/Ready/Chat
/// trait impls.
///
/// Two modes:
/// - **Without `AgentIo`** (`new`): creates a headless AgentIo
///   that routes stdout/stderr into the shared status log.
/// - **With `AgentIo`** (`with_io`): hands the byte channels to the
///   engine's container backend so it can bridge a real PTY directly. Used by
///   PTY commands like `chat` so their output renders inside the TUI's
///   container overlay.
pub struct InstanceContainerProxy {
    log: SharedStatusLog,
    container_io: Option<crate::engine::agent_runtime::frontend::AgentIo>,
    container: Option<crate::command::instance::feeds::ContainerLifecycle>,
}

impl InstanceContainerProxy {
    /// Construct a status-log-only proxy (no PTY bridging).
    /// Creates headless AgentIo channels that route to the status log.
    pub fn new(log: SharedStatusLog) -> Self {
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stderr_tx, stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        // Headless: engine owns the single stdin_tx and drops it
        // after seeding so the child sees EOF (see `spawn_piped_docker`).

        // Spawn drain tasks that route stdout/stderr into the status log.
        let log_for_stdout = log.clone();
        let mut stdout_rx = stdout_rx;
        tokio::spawn(async move {
            while let Some(bytes) = stdout_rx.recv().await {
                let text = String::from_utf8_lossy(&bytes);
                for line in text.lines() {
                    if !line.trim().is_empty() {
                        if let Ok(mut log) = log_for_stdout.lock() {
                            log.push(StatusLogEntry {
                                level: MessageLevel::Info,
                                text: line.to_string(),
                            });
                        }
                    }
                }
            }
        });

        let log_for_stderr = log.clone();
        let mut stderr_rx = stderr_rx;
        tokio::spawn(async move {
            while let Some(bytes) = stderr_rx.recv().await {
                let text = String::from_utf8_lossy(&bytes);
                for line in text.lines() {
                    if !line.trim().is_empty() {
                        if let Ok(mut log) = log_for_stderr.lock() {
                            log.push(StatusLogEntry {
                                level: MessageLevel::Warning,
                                text: line.to_string(),
                            });
                        }
                    }
                }
            }
        });

        let io = AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        };

        Self {
            log,
            container_io: Some(io),
            container: None,
        }
    }

    /// Construct a proxy that also carries the byte-stream I/O channels for
    /// engine-side PTY bridging, plus the shared container name slot so the
    /// TUI stats poller can discover the container.
    pub fn with_io(
        log: SharedStatusLog,
        io: crate::engine::agent_runtime::frontend::AgentIo,
        container: Option<crate::command::instance::feeds::ContainerLifecycle>,
    ) -> Self {
        Self {
            log,
            container_io: Some(io),
            container,
        }
    }
}

impl UserMessageSink for InstanceContainerProxy {
    fn write_message(&mut self, msg: UserMessage) {
        if let Ok(mut log) = self.log.lock() {
            log.push(StatusLogEntry {
                level: msg.level,
                text: msg.text,
            });
        }
    }

    fn replay_queued(&mut self) {}
}

#[async_trait]
impl AgentFrontend for InstanceContainerProxy {
    fn report_status(&mut self, status: AgentStatus) {
        if let AgentStatus::Running { ref container_name } = status {
            if let Some(ref shared) = self.container {
                shared.running(container_name.clone());
            }
        }
    }

    fn report_progress(&mut self, _progress: AgentProgress) {}

    fn take_io(&mut self) -> AgentIo {
        self.container_io
            .take()
            .expect("InstanceContainerProxy::take_io called but no AgentIo available")
    }
}
