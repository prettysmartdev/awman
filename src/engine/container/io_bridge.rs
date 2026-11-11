//! Shared I/O bridge infrastructure used by both Docker and Apple backends.
//!
//! Covers:
//! - PTY-bridged path: reader thread, writer task, resize task
//! - Piped path: stdout/stderr reader threads, stdin writer task
//! - Activity tracking (shared timestamp for stuck detection)
//! - Stuck detector task (broadcast `StuckEvent`)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::engine::agent_runtime::execution::StuckEvent;
use crate::engine::agent_runtime::frontend::AgentIo;
use crate::engine::agent_runtime::output_tail::OutputTail;
use crate::engine::agent_runtime::program_status::{
    ProgramStatusBridgeConfig, ProgramStatusStream, ProgramStatusTracker, ReportedGate,
    SharedProgramStatusTracker,
};
use crate::engine::error::EngineError;

/// A PTY bridge's writer that never types into the session when dropped.
///
/// portable-pty's `take_writer` handle writes "\n" + VEOF when dropped, and a
/// bridge's writer drops when its session ends — while the client on the
/// slave may still be alive (a `docker attach` being detached, an unwaited
/// `docker run -it` that must stay reattachable). The client would forward
/// those bytes to the agent as Enter + Ctrl-D. The drop writes only when VEOF
/// is set, and termios is shared by both ends, so clear VEOF on the slave
/// first. The session is over, so its EOF character no longer matters.
///
/// This is a `Drop` guard rather than a call at the end of the writer task
/// because the task is not always allowed to finish: a CLI detach returns
/// while the task still waits for input, and runtime shutdown then drops it.
pub(crate) struct QuietPtyWriter {
    writer: Box<dyn std::io::Write + Send>,
    slave_path: Option<std::path::PathBuf>,
}

impl QuietPtyWriter {
    pub(crate) fn new(
        writer: Box<dyn std::io::Write + Send>,
        master: &dyn portable_pty::MasterPty,
    ) -> Self {
        Self {
            writer,
            slave_path: pty_slave_path(master),
        }
    }
}

impl std::io::Write for QuietPtyWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

impl Drop for QuietPtyWriter {
    // `self.writer` itself drops after this returns, once VEOF is cleared.
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(path) = &self.slave_path {
            use nix::sys::termios::{tcgetattr, tcsetattr, SetArg, SpecialCharacterIndices};
            use std::os::unix::fs::OpenOptionsExt;
            if let Ok(slave) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOCTTY)
                .open(path)
            {
                if let Ok(mut termios) = tcgetattr(&slave) {
                    termios.control_chars[SpecialCharacterIndices::VEOF as usize] = 0;
                    let _ = tcsetattr(&slave, SetArg::TCSANOW, &termios);
                }
            }
        }
    }
}

/// The slave device path for [`QuietPtyWriter`]; `None` off Unix.
fn pty_slave_path(master: &dyn portable_pty::MasterPty) -> Option<std::path::PathBuf> {
    #[cfg(unix)]
    return master.tty_name();
    #[cfg(not(unix))]
    {
        let _ = master;
        None
    }
}

/// Shared last-activity timestamp. Updated by reader threads on every byte
/// chunk from stdout or stderr. Read by the stuck detector task.
pub(crate) type SharedActivity = Arc<Mutex<Option<Instant>>>;

/// Best-effort cancel callback the detector invokes when the startup grace
/// window expires. Implemented by the backend (a closure that calls
/// `docker stop <name>` / `container stop <name>`). Optional because tests
/// and the inert backend don't need it.
pub(crate) type CancelFn = Arc<dyn Fn() + Send + Sync>;

/// Per-bridge configuration. Bundled into one struct so the bridge functions
/// don't grow an unwieldy parameter list as more knobs land.
pub(crate) struct BridgeConfig {
    pub terminal: Option<crate::engine::agent_runtime::execution::TerminalLaunch>,
    pub program_status: ProgramStatusBridgeConfig,
    pub grace_timeout: Duration,
    pub stuck_timeout: Duration,
    /// Window after container launch during which the detector ignores
    /// activity / first_byte updates. Some runtimes (notably Apple's
    /// `container` binary) print their own startup chatter on stdout before
    /// the real workload starts, which would otherwise prematurely satisfy
    /// the grace-phase "first byte" check. Default `Duration::ZERO` keeps
    /// the existing behaviour for backends that don't need it.
    pub container_start_delay: Duration,
    /// Invoked once when the startup grace window expires. The reader
    /// threads have not seen a byte from the container; the detector calls
    /// this to force the container to exit so callers' `wait()` futures
    /// resolve with a failure status.
    pub cancel_on_grace_expired: Option<CancelFn>,
    /// Rolling buffer of the container's recent combined stdout/stderr. The
    /// reader threads append every byte chunk here (in addition to forwarding
    /// it to the frontend) so a failure log can be written if the container
    /// exits unexpectedly.
    pub output_tail: Arc<OutputTail>,
    /// Optional live tap on the output stream: every chunk the reader threads
    /// forward is also broadcast here. Used by the attach rendezvous socket
    /// (`attach_socket.rs`) on runtimes without a native attach, so connected
    /// attach clients see exactly what the launching frontend sees. `None`
    /// keeps the existing behaviour.
    pub output_broadcast: Option<Arc<tokio::sync::broadcast::Sender<Vec<u8>>>>,
}

/// Bundle returned by `bridge_pty` / `bridge_piped` containing the artifacts
/// the backend needs to store for the execution lifetime.
pub(crate) struct BridgeResult {
    pub program_status: SharedProgramStatusTracker,
    /// Sender for `try_inject_stdin` — the engine retains a clone.
    pub stdin_injector: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Broadcast sender for stuck events — stored in `AgentExecution`.
    pub stuck_tx: Arc<tokio::sync::broadcast::Sender<StuckEvent>>,
    /// The rolling output tail the reader threads feed — stored in
    /// `AgentExecution` so the workflow engine can read it after `wait()`.
    pub output_tail: Arc<OutputTail>,
}

/// Output-liveness state shared between whatever reads a runtime's bytes and
/// its [`StuckDetector`]. Byte-stream-agnostic: a PTY reader thread, a piped
/// child reader, an attach socket reader and the builtin event loop all feed
/// it the same way, by calling [`OutputActivity::touch`] per chunk.
#[derive(Clone, Default)]
pub(crate) struct OutputActivity {
    last: SharedActivity,
    first_byte: Arc<AtomicBool>,
}

impl OutputActivity {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record that a chunk of output arrived now.
    pub(crate) fn touch(&self) {
        if let Ok(mut guard) = self.last.lock() {
            *guard = Some(Instant::now());
        }
        self.first_byte.store(true, Ordering::Release);
    }
}

/// Timing knobs and the grace-expiry callback for one [`StuckDetector`].
pub(crate) struct StuckConfig {
    pub program_status: Option<SharedProgramStatusTracker>,
    pub grace_timeout: Duration,
    pub stuck_timeout: Duration,
    /// See [`BridgeConfig::container_start_delay`].
    pub container_start_delay: Duration,
    /// See [`BridgeConfig::cancel_on_grace_expired`].
    pub cancel_on_grace_expired: Option<CancelFn>,
    /// How often the detector samples the output clock. Every runtime uses
    /// [`STUCK_POLL_INTERVAL`]; tests shorten it to exercise transitions fast.
    pub poll_interval: Duration,
}

/// The [`StuckDetector`]'s sampling period in production.
pub(crate) const STUCK_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Spawn the stuck detector task. Returns the broadcast sender (caller
/// stores it in `AgentExecution`).
///
/// The task holds only a `Weak` to the sender so it can detect when its
/// owning `AgentExecution` is dropped: the next `upgrade()` returns
/// `None` and the task exits. Without this the task would leak for the
/// lifetime of the process. Transient `SendError` (no subscribers right
/// now) is ignored — broadcast semantics allow a subscriber to appear
/// later as long as the sender Arc is still alive.
///
/// Three phases:
///   0. *Start delay* (optional, when `container_start_delay` is non-zero)
///      — the detector sleeps without emitting events and ignores any
///      first-byte / activity updates from the reader threads. After the
///      delay it resets `first_byte` and `activity` to clear out any
///      bytes the container runtime itself produced (e.g. Apple's
///      `container` binary prints "creating container…" / "starting
///      container…" before the real workload runs). This prevents the
///      runtime's startup chatter from satisfying the grace check.
///   1. *Grace* — until the first byte of (real) output arrives, the
///      detector watches its grace clock. If it crosses `grace_timeout`,
///      the detector publishes `StartupGraceExpired`, invokes the cancel
///      callback (so the container dies and `wait()` resolves), and exits.
///   2. *Stuck* — once `first_byte` flips to `true`, the detector
///      switches to the regular Stuck/Unstuck loop driven by the last
///      activity timestamp and `stuck_timeout`. Grace is discarded.
pub(crate) struct StuckDetector;

impl StuckDetector {
    pub(crate) fn spawn(
        config: StuckConfig,
        output: &OutputActivity,
    ) -> Arc<tokio::sync::broadcast::Sender<StuckEvent>> {
        let StuckConfig {
            program_status,
            grace_timeout,
            stuck_timeout,
            container_start_delay,
            cancel_on_grace_expired,
            poll_interval,
        } = config;
        let activity = Arc::clone(&output.last);
        let first_byte = Arc::clone(&output.first_byte);
        let (stuck_tx, _) = tokio::sync::broadcast::channel(16);
        let stuck_tx = Arc::new(stuck_tx);
        let weak = Arc::downgrade(&stuck_tx);
        tokio::spawn(async move {
            // Phase 0: optional start-delay window. The reader threads keep
            // updating activity / first_byte during this sleep; we discard
            // those updates at the end of the window so only post-delay bytes
            // count.
            if !container_start_delay.is_zero() {
                tokio::time::sleep(container_start_delay).await;
                if weak.upgrade().is_none() {
                    return;
                }
                first_byte.store(false, Ordering::Release);
                if let Ok(mut guard) = activity.lock() {
                    *guard = None;
                }
            }

            let mut is_stuck = false;
            let mut reported_gate = ReportedGate::new();
            let mut generation = 0;
            let grace_start = Instant::now();
            loop {
                tokio::time::sleep(poll_interval).await;
                let tx = match weak.upgrade() {
                    Some(tx) => tx,
                    None => break,
                };

                let (reporting, effective, last_report, ended, tracker_generation) = program_status
                    .as_ref()
                    .and_then(|tracker| tracker.lock().ok())
                    .map(|tracker| {
                        (
                            tracker.reporting(),
                            tracker.effective(),
                            tracker.last_report_at(),
                            tracker.is_ended(),
                            tracker.generation(),
                        )
                    })
                    .unwrap_or((false, None, None, false, 0));
                if ended {
                    break;
                }
                if generation != tracker_generation {
                    generation = tracker_generation;
                    reported_gate = ReportedGate::new();
                }
                if reporting {
                    if is_stuck && effective.is_some() {
                        is_stuck = false;
                        let _ = tx.send(StuckEvent::Unstuck);
                    }
                    if let Some(status) = reported_gate.next(Instant::now(), effective.as_ref()) {
                        let _ = tx.send(StuckEvent::Reported(status));
                    }
                } else {
                    reported_gate = ReportedGate::new();
                }

                if !first_byte.load(Ordering::Acquire) {
                    // Grace phase — measure from end of the start-delay window.
                    if grace_start.elapsed() >= grace_timeout {
                        let _ = tx.send(StuckEvent::StartupGraceExpired);
                        if let Some(cb) = &cancel_on_grace_expired {
                            cb();
                        }
                        break;
                    }
                    continue;
                }

                // Stuck phase — measure from the most recent byte.
                if reporting && effective.is_some() {
                    continue;
                }
                let elapsed = {
                    let guard = match activity.lock() {
                        Ok(g) => g,
                        Err(_) => break,
                    };
                    let last = if reporting {
                        (*guard).max(last_report)
                    } else {
                        *guard
                    };
                    match last {
                        Some(t) => t.elapsed(),
                        None => grace_start.elapsed(),
                    }
                };
                let now_stuck = elapsed >= stuck_timeout;
                if now_stuck && !is_stuck {
                    is_stuck = true;
                    let _ = tx.send(StuckEvent::Stuck);
                } else if !now_stuck && is_stuck {
                    is_stuck = false;
                    let _ = tx.send(StuckEvent::Unstuck);
                }
            }
        });
        stuck_tx
    }
}

/// Bridge a PTY master to the `AgentIo` channels.
///
/// Spawns:
/// - Reader thread: PTY master → `io.stdout` (activity tracked)
/// - Writer task: `io.stdin_rx` → PTY master
/// - Resize task: `io.resize` → `master.resize()`
///
/// Returns the master wrapped in `Arc<Mutex>` (for resize + cleanup) plus
/// the `BridgeResult`.
type PtyMaster = Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>;

pub(crate) fn bridge_pty(
    io: AgentIo,
    pair: portable_pty::PtyPair,
    config: BridgeConfig,
) -> Result<(PtyMaster, BridgeResult), EngineError> {
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| EngineError::Container(format!("clone pty reader: {e}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| EngineError::Container(format!("take pty writer: {e}")))?;
    let mut writer = QuietPtyWriter::new(writer, pair.master.as_ref());
    let master_arc = Arc::new(Mutex::new(pair.master));
    let (terminal, mut remote_input) = match &config.terminal {
        Some(launch) => {
            let (cols, rows) = io.initial_size.unwrap_or((120, 40));
            let (handle, input) = launch.register_pty(cols, rows, &master_arc);
            (Some(handle), Some(input))
        }
        None => (None, None),
    };
    let reader_terminal = terminal.clone();
    let drain = config.terminal.as_ref().map(|launch| launch.drain.writer());

    let output = OutputActivity::new();
    let program_status = ProgramStatusTracker::shared();

    // Reader thread: PTY → stdout channel (activity tracked).
    //
    // If the frontend's stdout sink dies (drain task panics or exits early),
    // we keep draining the PTY but discard bytes — the container must not be
    // backpressured by a dead sink. Activity tracking continues so stuck
    // detection reflects what the container is actually emitting. The output
    // tail is fed unconditionally (even after the sink dies) so a failure log
    // always reflects what the container really printed.
    let stdout_tx = io.stdout;
    let act = output.clone();
    // A writer handle, so a caller can wait for this thread to finish.
    let tail = config.output_tail.writer();
    let broadcast = config.output_broadcast.clone();
    let mut status_stream = ProgramStatusStream::new(
        program_status.clone(),
        ProgramStatusBridgeConfig {
            answer_queries: true,
            ..config.program_status
        },
    );
    let query_stdin = io.stdin_tx.clone();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        let mut sink_open = true;
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    act.touch();
                    let bytes = status_stream.feed(&buf[..n], |reply| {
                        let _ = query_stdin.send(reply.to_vec());
                    });
                    tail.push_bytes(&bytes);
                    if let Some(terminal) = &reader_terminal {
                        terminal.push_output(&bytes);
                    }
                    if let Some(tx) = &broadcast {
                        // Copy only when someone is attached; a lagged client
                        // drops chunks. (A new client's subscription always
                        // precedes the repaint its initial resize triggers,
                        // so this gate can't starve it.)
                        if tx.receiver_count() > 0 {
                            let _ = tx.send(bytes.to_vec());
                        }
                    }
                    if sink_open && stdout_tx.send(bytes.to_vec()).is_err() {
                        sink_open = false;
                    }
                }
            }
        }
        let bytes = status_stream.finish();
        tail.push_bytes(&bytes);
        if let Some(terminal) = &reader_terminal {
            terminal.push_output(&bytes);
        }
        if !bytes.is_empty() {
            let _ = stdout_tx.send(bytes);
        }
        drop(drain);
    });

    // Writer task: stdin channel → PTY.
    let stdin_tx = io.stdin_tx;
    let mut stdin_rx = io.stdin_rx;
    tokio::spawn(async move {
        use std::io::Write;
        loop {
            let bytes = tokio::select! {
                bytes = stdin_rx.recv() => match bytes { Some(bytes) => bytes, None => break },
                bytes = crate::engine::agent_runtime::execution::next_terminal_input(&mut remote_input) => bytes,
            };
            if writer.write_all(&bytes).is_err() {
                break;
            }
            if writer.flush().is_err() {
                break;
            }
        }
    });

    // Resize task.
    if let Some(mut resize_rx) = io.resize {
        let master_for_resize = Arc::clone(&master_arc);
        tokio::spawn(async move {
            use portable_pty::PtySize;
            while let Some((cols, rows)) = resize_rx.recv().await {
                if let Some(terminal) = &terminal {
                    // Local presentation changes only the fallback while a
                    // remote viewer owns the PTY size.
                    if let Some(launch) = &config.terminal {
                        let _ = launch.hub.set_default_size(terminal.id(), cols, rows);
                    }
                    continue;
                }
                if let Ok(master) = master_for_resize.lock() {
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
        });
    }

    let stuck_tx = StuckDetector::spawn(
        StuckConfig {
            grace_timeout: config.grace_timeout,
            stuck_timeout: config.stuck_timeout,
            container_start_delay: config.container_start_delay,
            cancel_on_grace_expired: config.cancel_on_grace_expired,
            poll_interval: STUCK_POLL_INTERVAL,
            program_status: Some(program_status.clone()),
        },
        &output,
    );

    Ok((
        master_arc,
        BridgeResult {
            stdin_injector: stdin_tx,
            stuck_tx,
            output_tail: config.output_tail,
            program_status,
        },
    ))
}

/// Bridge a piped child process to the `AgentIo` channels.
///
/// Spawns:
/// - Reader thread for child stdout → `io.stdout` (activity tracked)
/// - Reader thread for child stderr → `io.stderr` (activity tracked)
/// - Writer task: `io.stdin_rx` → child stdin
///
/// The child's stdout/stderr/stdin pipes are taken from the `Child`.
pub(crate) fn bridge_piped(
    io: AgentIo,
    child: &mut std::process::Child,
    config: BridgeConfig,
) -> BridgeResult {
    let output = OutputActivity::new();
    let program_status = ProgramStatusTracker::shared();

    // stdout reader thread.
    //
    // If the frontend's sink dies, keep draining the pipe but discard bytes —
    // a piped child will block on stdout if we stop reading. Activity
    // tracking continues unconditionally so the stuck detector reflects what
    // the container actually produces.
    if let Some(child_stdout) = child.stdout.take() {
        let stdout_tx = io.stdout;
        let act = output.clone();
        // A writer handle, so a caller can wait for this thread to finish.
        let tail = config.output_tail.writer();
        let broadcast = config.output_broadcast.clone();
        let mut status_stream = ProgramStatusStream::new(
            program_status.clone(),
            ProgramStatusBridgeConfig {
                answer_queries: false,
                ..config.program_status
            },
        );
        std::thread::spawn(move || {
            use std::io::Read;
            let mut reader = child_stdout;
            let mut buf = [0u8; 4096];
            let mut sink_open = true;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        act.touch();
                        let bytes = status_stream.feed(&buf[..n], |_| {});
                        tail.push_bytes(&bytes);
                        if let Some(tx) = &broadcast {
                            if tx.receiver_count() > 0 {
                                let _ = tx.send(bytes.to_vec());
                            }
                        }
                        if sink_open && stdout_tx.send(bytes.to_vec()).is_err() {
                            sink_open = false;
                        }
                    }
                }
            }
            let bytes = status_stream.finish();
            tail.push_bytes(&bytes);
            if let Some(tx) = &broadcast {
                let _ = tx.send(bytes.clone());
            }
            if !bytes.is_empty() {
                let _ = stdout_tx.send(bytes);
            }
        });
    }

    // stderr reader thread — same drain-after-sink-dies semantics as stdout.
    // Feeds the same tail as stdout so the buffer holds combined output.
    if let Some(child_stderr) = child.stderr.take() {
        let stderr_tx = io.stderr;
        let act = output.clone();
        // A writer handle, so a caller can wait for this thread to finish.
        let tail = config.output_tail.writer();
        let broadcast = config.output_broadcast.clone();
        let mut status_stream = ProgramStatusStream::new(
            program_status.clone(),
            ProgramStatusBridgeConfig {
                answer_queries: false,
                ..config.program_status
            },
        );
        std::thread::spawn(move || {
            use std::io::Read;
            let mut reader = child_stderr;
            let mut buf = [0u8; 4096];
            let mut sink_open = true;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        act.touch();
                        let bytes = status_stream.feed(&buf[..n], |_| {});
                        tail.push_bytes(&bytes);
                        if let Some(tx) = &broadcast {
                            if tx.receiver_count() > 0 {
                                let _ = tx.send(bytes.to_vec());
                            }
                        }
                        if sink_open && stderr_tx.send(bytes.to_vec()).is_err() {
                            sink_open = false;
                        }
                    }
                }
            }
            let bytes = status_stream.finish();
            tail.push_bytes(&bytes);
            if let Some(tx) = &broadcast {
                let _ = tx.send(bytes.clone());
            }
            if !bytes.is_empty() {
                let _ = stderr_tx.send(bytes);
            }
        });
    }

    // stdin writer task
    let stdin_tx = io.stdin_tx;
    if let Some(child_stdin) = child.stdin.take() {
        let mut stdin_rx = io.stdin_rx;
        tokio::spawn(async move {
            use std::io::Write;
            let mut writer = child_stdin;
            while let Some(bytes) = stdin_rx.recv().await {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
                if writer.flush().is_err() {
                    break;
                }
            }
        });
    }

    let stuck_tx = StuckDetector::spawn(
        StuckConfig {
            grace_timeout: config.grace_timeout,
            stuck_timeout: config.stuck_timeout,
            container_start_delay: config.container_start_delay,
            cancel_on_grace_expired: config.cancel_on_grace_expired,
            poll_interval: STUCK_POLL_INTERVAL,
            program_status: Some(program_status.clone()),
        },
        &output,
    );

    BridgeResult {
        stdin_injector: stdin_tx,
        stuck_tx,
        output_tail: config.output_tail,
        program_status,
    }
}
