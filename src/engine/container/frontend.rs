//! `ContainerFrontend` trait — defined by Layer 1, implemented by Layer 3.

use async_trait::async_trait;

use crate::engine::error::EngineError;
use crate::engine::message::UserMessageSink;

/// What stage a container execution is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerStatus {
    Building,
    Pulling,
    Starting,
    Running,
    Stopping,
    Exited(i32),
    Failed(String),
}

/// A unit of progress reported during a long-running container action
/// (image pull, build step, layer extract).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerProgress {
    pub stage: String,
    pub message: String,
    pub current: Option<u64>,
    pub total: Option<u64>,
}

/// Abstract container-side I/O. Implementations live in Layer 3 (CLI binds
/// stdio, TUI binds a PTY, headless binds an SSE/WebSocket stream).
///
/// `read_stdin` is async so that async frontends (TUI, headless) do not need
/// to block a thread. CLI frontends use `tokio::task::spawn_blocking` at their
/// implementation site.
#[async_trait]
pub trait ContainerFrontend: UserMessageSink + Send {
    fn write_stdout(&mut self, bytes: &[u8]) -> Result<(), EngineError>;
    fn write_stderr(&mut self, bytes: &[u8]) -> Result<(), EngineError>;
    /// Read a chunk of stdin from the user. `Ok(0)` means EOF. Async so that
    /// implementations may suspend without blocking a thread.
    async fn read_stdin(&mut self, buf: &mut [u8]) -> Result<usize, EngineError>;
    fn report_status(&mut self, status: ContainerStatus);
    fn report_progress(&mut self, progress: ContainerProgress);
    fn resize_pty(&mut self, cols: u16, rows: u16);
}
