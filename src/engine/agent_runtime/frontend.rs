//! `AgentFrontend` trait — defined by Layer 1, implemented by Layer 3.
//!
//! Shared by both runtime tiers (container-class and sandbox-class): the
//! engine calls back into the frontend for PTY size, stdin, and status
//! reporting regardless of the underlying isolation paradigm.

use std::time::Duration;

use async_trait::async_trait;

use crate::engine::container::timing::{DEFAULT_GRACE_TIMEOUT, DEFAULT_STUCK_TIMEOUT};
use crate::engine::message::UserMessageSink;

/// What stage an agent execution is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    Building,
    Pulling,
    Starting,
    Running { container_name: String },
    Stopping,
    Exited(i32),
    Failed(String),
}

/// A unit of progress reported during a long-running runtime action
/// (image pull, build step, layer extract).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProgress {
    pub stage: String,
    pub message: String,
    pub current: Option<u64>,
    pub total: Option<u64>,
}

/// Byte-stream I/O channels detached from a frontend so the engine can
/// bridge them to the agent process.
///
/// Every frontend must provide an `AgentIo` via `take_io()`.
/// The engine uses these channels exclusively for all agent I/O:
///
/// - **PTY path** (`resize`/`initial_size` are `Some`): the engine opens a
///   PTY via `portable-pty` and bridges it through these channels.
/// - **Piped path** (`resize`/`initial_size` are `None`): the engine spawns
///   the agent with `Stdio::piped()` and bridges through these channels.
///
/// The stdin direction has both ends because the frontend needs a sender
/// (for keystrokes) and the engine retains its own sender clone — used by
/// `AgentExecution::try_inject_stdin` to send a fresh prompt into a
/// still-running agent during workflow `ContinueInCurrentContainer`
/// advances.
pub struct AgentIo {
    /// Engine sends agent stdout bytes here.
    pub stdout: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Engine sends agent stderr bytes here.
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
    /// `Stdio::piped()` for the agent process.
    pub resize: Option<tokio::sync::mpsc::UnboundedReceiver<(u16, u16)>>,
    /// Initial PTY size at spawn time. `Some` for interactive frontends,
    /// `None` for non-interactive. When `None`, the engine uses
    /// `Stdio::piped()`.
    pub initial_size: Option<(u16, u16)>,
}

/// Abstract agent-side I/O. Implementations live in Layer 3 (CLI binds
/// stdio, TUI binds a PTY, API binds an SSE/WebSocket stream).
///
/// The engine exclusively uses the channels from `take_io()` for
/// all agent I/O — stdout, stderr, and stdin.
#[async_trait]
pub trait AgentFrontend: UserMessageSink + Send {
    fn report_status(&mut self, status: AgentStatus);
    fn report_progress(&mut self, progress: AgentProgress);

    /// Detach the byte-stream I/O channels for engine bridging.
    ///
    /// The engine takes ownership of these channels in `run_with_frontend`
    /// and spawns reader/writer tasks. Every frontend must implement this.
    fn take_io(&mut self) -> AgentIo;

    /// Startup-grace timeout: how long the agent has to emit its first
    /// byte of output before it is considered failed-to-start and killed.
    /// The regular stuck timer does not begin until the first byte is seen
    /// and the grace timer has been discarded.
    ///
    /// CLI/TUI keep the default (30s). API overrides to 15 minutes so an
    /// agent that needs to pull a large image or warm up a model isn't
    /// killed before it has a chance to produce output.
    fn grace_timeout(&self) -> Duration {
        DEFAULT_GRACE_TIMEOUT
    }

    /// Stuck timeout: how long the agent can go without producing any
    /// further output (after its first byte) before the engine publishes
    /// `StuckEvent::Stuck`. CLI, TUI, and API all use 30s; only the
    /// grace timeout differs between modes.
    fn stuck_timeout(&self) -> Duration {
        DEFAULT_STUCK_TIMEOUT
    }
}
