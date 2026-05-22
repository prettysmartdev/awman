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
    Running { container_name: String },
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

/// Byte-stream I/O channels detached from a frontend so the engine can bridge
/// them to a real PTY in the container backend.
///
/// When a frontend opts into PTY bridging (TUI, API), the engine takes
/// ownership of these channels in `run_with_frontend` and spawns reader/writer
/// tasks against the PTY master. When a frontend does not opt in (the bare
/// CLI), `take_container_io` returns `None` and the backend falls back to its
/// inherit-stdio path.
///
/// The stdin direction has both ends because the TUI also needs a sender (for
/// keystrokes) and the engine retains its own sender clone — used by
/// `ContainerExecution::try_inject_stdin` to send a fresh prompt into a still-
/// running container during workflow `ContinueInCurrentContainer` advances.
pub struct ContainerIo {
    /// Engine sends container stdout/stderr bytes here. The frontend drains it
    /// (e.g. into a vt100 parser).
    pub stdout: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Sender side of the stdin channel — engine retains a clone for
    /// `try_inject_stdin`; frontend also keeps its own clone for keystrokes.
    pub stdin_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Receiver side of the stdin channel — consumed by the engine's PTY
    /// writer task. Both the frontend (keystrokes) and the engine
    /// (`try_inject_stdin`) push into the matching sender.
    pub stdin_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    /// Engine reads PTY resize requests from here whenever the host terminal
    /// resizes. The frontend pushes (cols, rows).
    pub resize: tokio::sync::mpsc::UnboundedReceiver<(u16, u16)>,
    /// Initial PTY size at spawn time.
    pub initial_size: (u16, u16),
}

/// Abstract container-side I/O. Implementations live in Layer 3 (CLI binds
/// stdio, TUI binds a PTY, API binds an SSE/WebSocket stream).
///
/// `read_stdin` is async so that async frontends (TUI, API) do not need
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

    /// Detach the byte-stream I/O channels for engine PTY bridging.
    ///
    /// If `Some`, the backend should bridge the container's PTY directly via
    /// these channels (instead of inheriting host stdio). The default
    /// implementation returns `None` — appropriate for CLI/API frontends
    /// that have no PTY to bridge.
    ///
    /// Once channels have been taken, `write_stdout`/`read_stdin`/`resize_pty`
    /// are unused — the engine drives the PTY directly via the channels.
    fn take_container_io(&mut self) -> Option<ContainerIo> {
        None
    }
}
