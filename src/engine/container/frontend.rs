//! `ContainerFrontend` trait — defined by Layer 1, implemented by Layer 3.

use async_trait::async_trait;

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

/// Byte-stream I/O channels detached from a frontend so the engine can
/// bridge them to the container process.
///
/// Every frontend must provide a `ContainerIo` via `take_container_io()`.
/// The engine uses these channels exclusively for all container I/O:
///
/// - **PTY path** (`resize`/`initial_size` are `Some`): the engine opens a
///   PTY via `portable-pty` and bridges it through these channels.
/// - **Piped path** (`resize`/`initial_size` are `None`): the engine spawns
///   the container with `Stdio::piped()` and bridges through these channels.
///
/// The stdin direction has both ends because the frontend needs a sender
/// (for keystrokes) and the engine retains its own sender clone — used by
/// `ContainerExecution::try_inject_stdin` to send a fresh prompt into a
/// still-running container during workflow `ContinueInCurrentContainer`
/// advances.
pub struct ContainerIo {
    /// Engine sends container stdout bytes here.
    pub stdout: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Engine sends container stderr bytes here.
    pub stderr: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Sender side of the stdin channel — engine retains a clone for
    /// `try_inject_stdin`; frontend also keeps its own clone for keystrokes.
    pub stdin_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Receiver side of the stdin channel — consumed by the engine's writer
    /// task. Both the frontend (keystrokes) and the engine
    /// (`try_inject_stdin`) push into the matching sender.
    pub stdin_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    /// PTY resize requests from the frontend. `Some` for interactive
    /// frontends (TUI, CLI with TTY), `None` for non-interactive (CLI
    /// `--non-interactive`, API). When `None`, the engine uses
    /// `Stdio::piped()` for the container process.
    pub resize: Option<tokio::sync::mpsc::UnboundedReceiver<(u16, u16)>>,
    /// Initial PTY size at spawn time. `Some` for interactive frontends,
    /// `None` for non-interactive. When `None`, the engine uses
    /// `Stdio::piped()`.
    pub initial_size: Option<(u16, u16)>,
}

/// Abstract container-side I/O. Implementations live in Layer 3 (CLI binds
/// stdio, TUI binds a PTY, API binds an SSE/WebSocket stream).
///
/// The engine exclusively uses the channels from `take_container_io()` for
/// all container I/O — stdout, stderr, and stdin.
#[async_trait]
pub trait ContainerFrontend: UserMessageSink + Send {
    fn report_status(&mut self, status: ContainerStatus);
    fn report_progress(&mut self, progress: ContainerProgress);

    /// Detach the byte-stream I/O channels for engine bridging.
    ///
    /// The engine takes ownership of these channels in `run_with_frontend`
    /// and spawns reader/writer tasks. Every frontend must implement this.
    fn take_container_io(&mut self) -> ContainerIo;
}
