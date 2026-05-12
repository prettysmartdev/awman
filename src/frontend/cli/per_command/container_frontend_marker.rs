//! `ContainerFrontend` impl for the CLI.
//!
//! The CLI binds container stdout/stderr to the host
//! stdout/stderr and reads stdin via `tokio::task::spawn_blocking`. The
//! [`set_pty_active`] gate on the message queue ensures `UserMessage`s
//! are queued while the container owns the terminal.

use async_trait::async_trait;
use std::io::Write;

use crate::engine::container::frontend::{ContainerFrontend, ContainerProgress, ContainerStatus};
use crate::engine::error::EngineError;
use crate::engine::message::{UserMessage, UserMessageSink};

use crate::frontend::cli::command_frontend::CliFrontend;

#[async_trait]
impl ContainerFrontend for CliFrontend {
    fn write_stdout(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        let mut out = std::io::stdout().lock();
        out.write_all(bytes)
            .map_err(|e| EngineError::Other(format!("write stdout: {e}")))?;
        out.flush()
            .map_err(|e| EngineError::Other(format!("flush stdout: {e}")))?;
        Ok(())
    }

    fn write_stderr(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        let mut err = std::io::stderr().lock();
        err.write_all(bytes)
            .map_err(|e| EngineError::Other(format!("write stderr: {e}")))?;
        err.flush()
            .map_err(|e| EngineError::Other(format!("flush stderr: {e}")))?;
        Ok(())
    }

    async fn read_stdin(&mut self, buf: &mut [u8]) -> Result<usize, EngineError> {
        let len = buf.len();
        let read = tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut local = vec![0u8; len];
            let n = std::io::stdin()
                .lock()
                .read(&mut local)
                .map_err(|e| EngineError::Other(format!("read stdin: {e}")))?;
            local.truncate(n);
            Ok::<Vec<u8>, EngineError>(local)
        })
        .await
        .map_err(|e| EngineError::Other(format!("stdin task panicked: {e}")))??;
        let n = read.len().min(buf.len());
        buf[..n].copy_from_slice(&read[..n]);
        Ok(n)
    }

    fn report_status(&mut self, _status: ContainerStatus) {}
    fn report_progress(&mut self, _progress: ContainerProgress) {}
    fn resize_pty(&mut self, _cols: u16, _rows: u16) {}
}

// ─── Standalone proxy used by InitFrontend / ReadyFrontend ─────────────────

/// Stand-alone `ContainerFrontend` returned by engines that need a
/// `Box<dyn ContainerFrontend>` for a single container's lifetime
/// (`InitFrontend::container_frontend`, etc.). Streams to host stdio.
pub(crate) struct CliContainerProxy;

impl UserMessageSink for CliContainerProxy {
    fn write_message(&mut self, msg: UserMessage) {
        // This proxy is used by Init/Ready container phases which don't
        // have a PTY gate — write immediately to stderr.
        use crate::engine::message::MessageLevel;
        let prefix = match msg.level {
            MessageLevel::Info | MessageLevel::Success => "amux:",
            MessageLevel::Warning => "amux warning:",
            MessageLevel::Error => "amux error:",
        };
        let _ = writeln!(std::io::stderr(), "{prefix} {}", msg.text);
    }
    fn replay_queued(&mut self) {}
}

#[async_trait]
impl ContainerFrontend for CliContainerProxy {
    fn write_stdout(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        let mut out = std::io::stdout().lock();
        out.write_all(bytes)
            .map_err(|e| EngineError::Other(format!("write stdout: {e}")))?;
        out.flush()
            .map_err(|e| EngineError::Other(format!("flush stdout: {e}")))?;
        Ok(())
    }

    fn write_stderr(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        let mut err = std::io::stderr().lock();
        err.write_all(bytes)
            .map_err(|e| EngineError::Other(format!("write stderr: {e}")))?;
        err.flush()
            .map_err(|e| EngineError::Other(format!("flush stderr: {e}")))?;
        Ok(())
    }

    async fn read_stdin(&mut self, _buf: &mut [u8]) -> Result<usize, EngineError> {
        Ok(0)
    }

    fn report_status(&mut self, _status: ContainerStatus) {}
    fn report_progress(&mut self, _progress: ContainerProgress) {}
    fn resize_pty(&mut self, _cols: u16, _rows: u16) {}
}
