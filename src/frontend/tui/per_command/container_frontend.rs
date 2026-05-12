//! `ContainerFrontend` impls for the TUI — both on `TuiCommandFrontend`
//! (direct container I/O) and on a standalone `TuiContainerProxy` (used by
//! `container_frontend()` return values in Init/Ready).
//!
//! For TUI mode, the engine's container backend takes ownership of the byte
//! channels via `take_container_io` and bridges them directly to the
//! container's PTY master — so `write_stdout`/`read_stdin`/`resize_pty` are
//! no-ops on `TuiCommandFrontend`.

use async_trait::async_trait;

use crate::engine::container::frontend::{
    ContainerFrontend, ContainerIo, ContainerProgress, ContainerStatus,
};
use crate::engine::error::EngineError;
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::user_message::{SharedStatusLog, StatusLogEntry};

// ─── ContainerFrontend for TuiCommandFrontend ────────────────────────────

#[async_trait]
impl ContainerFrontend for TuiCommandFrontend {
    fn write_stdout(&mut self, _bytes: &[u8]) -> Result<(), EngineError> {
        // No-op: the engine bridges the PTY directly via the channels taken
        // through `take_container_io`. This method is unused for TUI mode.
        Ok(())
    }

    fn write_stderr(&mut self, _bytes: &[u8]) -> Result<(), EngineError> {
        Ok(())
    }

    async fn read_stdin(&mut self, _buf: &mut [u8]) -> Result<usize, EngineError> {
        // The engine reads stdin directly off the channel taken in
        // `take_container_io`. Return EOF here so any backend that calls this
        // legacy path stops cleanly.
        Ok(0)
    }

    fn report_status(&mut self, status: ContainerStatus) {
        if let ContainerStatus::Running { ref container_name } = status {
            if let Ok(mut name) = self.container_name_shared.lock() {
                *name = Some(container_name.clone());
            }
        }
        self.messages.info(format!("Container: {status:?}"));
    }

    fn report_progress(&mut self, progress: ContainerProgress) {
        self.messages
            .info(format!("{}: {}", progress.stage, progress.message));
    }

    fn resize_pty(&mut self, _cols: u16, _rows: u16) {
        // No-op: handled via the resize channel taken through take_container_io.
    }

    fn take_container_io(&mut self) -> Option<ContainerIo> {
        self.container_io.take()
    }
}

// ─── TuiContainerProxy ──────────────────────────────────────────────────

/// Standalone proxy returned by `container_frontend()` in Init/Ready/Chat
/// trait impls.
///
/// Two modes:
/// - **Without `ContainerIo`** (`new`): routes stdout/stderr line-by-line into
///   the shared status log. Used by non-PTY text commands like `ready`/`init`.
/// - **With `ContainerIo`** (`with_io`): hands the byte channels to the
///   engine's container backend so it can bridge a real PTY directly. Used by
///   PTY commands like `chat` so their output renders inside the TUI's
///   container overlay.
pub struct TuiContainerProxy {
    log: SharedStatusLog,
    container_io: Option<crate::engine::container::frontend::ContainerIo>,
    container_name_shared: Option<crate::frontend::tui::tabs::SharedContainerName>,
}

impl TuiContainerProxy {
    /// Construct a status-log-only proxy (no PTY bridging).
    pub fn new(log: SharedStatusLog) -> Self {
        Self {
            log,
            container_io: None,
            container_name_shared: None,
        }
    }

    /// Construct a proxy that also carries the byte-stream I/O channels for
    /// engine-side PTY bridging, plus the shared container name slot so the
    /// TUI stats poller can discover the container.
    pub fn with_io(
        log: SharedStatusLog,
        io: crate::engine::container::frontend::ContainerIo,
        container_name_shared: crate::frontend::tui::tabs::SharedContainerName,
    ) -> Self {
        Self {
            log,
            container_io: Some(io),
            container_name_shared: Some(container_name_shared),
        }
    }
}

impl UserMessageSink for TuiContainerProxy {
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
impl ContainerFrontend for TuiContainerProxy {
    fn write_stdout(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        let text = String::from_utf8_lossy(bytes);
        for line in text.lines() {
            if !line.trim().is_empty() {
                if let Ok(mut log) = self.log.lock() {
                    log.push(StatusLogEntry {
                        level: MessageLevel::Info,
                        text: line.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    fn write_stderr(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        let text = String::from_utf8_lossy(bytes);
        for line in text.lines() {
            if !line.trim().is_empty() {
                if let Ok(mut log) = self.log.lock() {
                    log.push(StatusLogEntry {
                        level: MessageLevel::Warning,
                        text: line.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    async fn read_stdin(&mut self, _buf: &mut [u8]) -> Result<usize, EngineError> {
        Ok(0) // Text commands don't need stdin
    }

    fn report_status(&mut self, status: ContainerStatus) {
        if let ContainerStatus::Running { ref container_name } = status {
            if let Some(ref shared) = self.container_name_shared {
                if let Ok(mut name) = shared.lock() {
                    *name = Some(container_name.clone());
                }
            }
        }
    }

    fn report_progress(&mut self, _progress: ContainerProgress) {}

    fn resize_pty(&mut self, _cols: u16, _rows: u16) {}

    fn take_container_io(&mut self) -> Option<ContainerIo> {
        self.container_io.take()
    }
}
