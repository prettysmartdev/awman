//! Self-contained PTY / piped I/O bridge for the sandbox runtime tier.
//!
//! Deliberately a sibling of — not a reuse of — the container tier's
//! `io_bridge`. WI 0090's layering rule forbids `src/engine/sandbox/` from
//! importing `src/engine/container/`, so the sandbox tier carries its own
//! bridge. It is intentionally simpler than the container bridge: sandboxes
//! boot in 2–5s and the experimental runtime does not yet drive stuck/grace
//! detection. It publishes OSC 7501 reports on the shared attention channel
//! without introducing a silence heuristic for non-reporting sandboxes.

use crate::engine::agent_runtime::program_status::SharedProgramStatusTracker;
use crate::engine::agent_runtime::program_status::{
    ProgramStatusBridgeConfig, ProgramStatusStream, ProgramStatusTracker,
};
use std::sync::{Arc, Mutex};

use crate::engine::agent_runtime::execution::{spawn_reported_publisher, StuckEvent};
use crate::engine::agent_runtime::frontend::AgentIo;
use crate::engine::error::EngineError;

/// Master PTY end, shared so the resize task and the execution backend can
/// both reach it.
pub(super) type PtyMaster = Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>;

/// Cap on captured output bytes — enough to hold sbx launch diagnostics
/// (kit compose errors, login prompts) without retaining a whole session.
const CAPTURE_CAP: usize = 16 * 1024;

/// Bounded capture of the most recent agent output bytes, shared between the
/// bridge reader threads and the execution backend so a non-zero exit can
/// replay what sbx printed into the message sink.
#[derive(Default)]
pub(super) struct OutputCapture {
    buf: Vec<u8>,
}

impl OutputCapture {
    pub(super) fn len(&self) -> usize {
        self.buf.len()
    }

    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > CAPTURE_CAP {
            let excess = self.buf.len() - CAPTURE_CAP;
            self.buf.drain(..excess);
        }
    }

    /// The last `max` non-empty output lines, terminal control sequences
    /// stripped so PTY output reads as plain message text.
    pub(super) fn tail_lines(&self, max: usize) -> Vec<String> {
        let text = strip_control_sequences(&String::from_utf8_lossy(&self.buf));
        let lines: Vec<String> = text
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect();
        let skip = lines.len().saturating_sub(max);
        lines[skip..].to_vec()
    }
}

/// Remove ANSI escape sequences (CSI/OSC) and non-printing control bytes,
/// keeping newlines and tabs.
fn strip_control_sequences(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameters/intermediates until a final byte in @..~.
                Some('[') => {
                    for n in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: terminated by BEL or ESC \.
                Some(']') => {
                    while let Some(n) = chars.next() {
                        if n == '\u{07}' {
                            break;
                        }
                        if n == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // Two-character escape — drop both.
                _ => {}
            },
            '\r' => {}
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

/// Artifacts the launch path stores for the execution lifetime.
pub(super) struct SandboxBridge {
    pub program_status: SharedProgramStatusTracker,
    /// Sender feeding the stdin writer task; the engine keeps a clone for
    /// `try_inject_stdin`.
    pub stdin_injector: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Broadcast sender for stuck events, stored in `AgentExecution`.
    pub stuck_tx: Arc<tokio::sync::broadcast::Sender<StuckEvent>>,
    /// Tail of everything the agent wrote, for failure reporting on exit.
    pub output: Arc<Mutex<OutputCapture>>,
}

/// Bridge a PTY master to the frontend's `AgentIo` channels.
///
/// Spawns a reader thread (PTY → `io.stdout`), a writer task
/// (`io.stdin_rx` → PTY), and a resize task (`io.resize` → `master.resize`).
pub(super) fn bridge_pty(
    io: AgentIo,
    pair: portable_pty::PtyPair,
    config: ProgramStatusBridgeConfig,
    launch: Option<crate::engine::agent_runtime::execution::TerminalLaunch>,
) -> Result<(PtyMaster, SandboxBridge), EngineError> {
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| EngineError::Sandbox(format!("clone sbx pty reader: {e}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| EngineError::Sandbox(format!("take sbx pty writer: {e}")))?;
    let mut writer =
        crate::engine::container::io_bridge::QuietPtyWriter::new(writer, pair.master.as_ref());
    let master_arc: PtyMaster = Arc::new(Mutex::new(pair.master));
    let (terminal, mut remote_input) = match &launch {
        Some(launch) => {
            let (cols, rows) = io.initial_size.unwrap_or((120, 40));
            let (handle, input) = launch.register_pty(cols, rows, &master_arc);
            (Some(handle), Some(input))
        }
        None => (None, None),
    };
    let reader_terminal = terminal.clone();
    let drain = launch.as_ref().map(|launch| launch.drain.writer());

    let output: Arc<Mutex<OutputCapture>> = Arc::default();
    let program_status = ProgramStatusTracker::shared();

    // Reader thread: PTY → stdout channel. Keep draining even if the sink dies
    // so the VM is never backpressured by a dead frontend.
    let stdout_tx = io.stdout;
    let capture = Arc::clone(&output);
    let mut status_stream = ProgramStatusStream::new(
        program_status.clone(),
        ProgramStatusBridgeConfig {
            answer_queries: true,
            ..config
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
                    let bytes = status_stream.feed(&buf[..n], |reply| {
                        let _ = query_stdin.send(reply.to_vec());
                    });
                    if let Some(terminal) = &reader_terminal {
                        terminal.push_output(&bytes);
                    }
                    if let Ok(mut cap) = capture.lock() {
                        cap.push(&bytes);
                    }
                    if sink_open && stdout_tx.send(bytes.to_vec()).is_err() {
                        sink_open = false;
                    }
                }
            }
        }
        let bytes = status_stream.finish();
        if let Some(terminal) = &reader_terminal {
            terminal.push_output(&bytes);
        }
        if let Ok(mut cap) = capture.lock() {
            cap.push(&bytes);
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
            if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
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
                if let (Some(terminal), Some(launch)) = (&terminal, &launch) {
                    let _ = launch.hub.set_default_size(terminal.id(), cols, rows);
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

    Ok((
        master_arc,
        SandboxBridge {
            stdin_injector: stdin_tx,
            stuck_tx: spawn_reported_publisher(program_status.clone()),
            program_status,
            output,
        },
    ))
}

/// Bridge a piped child process to the frontend's `AgentIo` channels.
pub(super) fn bridge_piped(
    io: AgentIo,
    child: &mut std::process::Child,
    config: ProgramStatusBridgeConfig,
) -> SandboxBridge {
    let output: Arc<Mutex<OutputCapture>> = Arc::default();
    let program_status = ProgramStatusTracker::shared();

    if let Some(child_stdout) = child.stdout.take() {
        let stdout_tx = io.stdout;
        let capture = Arc::clone(&output);
        let mut status_stream = ProgramStatusStream::new(
            program_status.clone(),
            ProgramStatusBridgeConfig {
                answer_queries: false,
                ..config
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
                        let bytes = status_stream.feed(&buf[..n], |_| {});
                        if let Ok(mut cap) = capture.lock() {
                            cap.push(&bytes);
                        }
                        if sink_open && stdout_tx.send(bytes.to_vec()).is_err() {
                            sink_open = false;
                        }
                    }
                }
            }
            let bytes = status_stream.finish();
            if let Ok(mut cap) = capture.lock() {
                cap.push(&bytes);
            }
            if !bytes.is_empty() {
                let _ = stdout_tx.send(bytes);
            }
        });
    }

    if let Some(child_stderr) = child.stderr.take() {
        let stderr_tx = io.stderr;
        let capture = Arc::clone(&output);
        let mut status_stream = ProgramStatusStream::new(
            program_status.clone(),
            ProgramStatusBridgeConfig {
                answer_queries: false,
                ..config
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
                        let bytes = status_stream.feed(&buf[..n], |_| {});
                        if let Ok(mut cap) = capture.lock() {
                            cap.push(&bytes);
                        }
                        if sink_open && stderr_tx.send(bytes.to_vec()).is_err() {
                            sink_open = false;
                        }
                    }
                }
            }
            let bytes = status_stream.finish();
            if let Ok(mut cap) = capture.lock() {
                cap.push(&bytes);
            }
            if !bytes.is_empty() {
                let _ = stderr_tx.send(bytes);
            }
        });
    }

    let stdin_tx = io.stdin_tx;
    if let Some(child_stdin) = child.stdin.take() {
        let mut stdin_rx = io.stdin_rx;
        tokio::spawn(async move {
            use std::io::Write;
            let mut writer = child_stdin;
            while let Some(bytes) = stdin_rx.recv().await {
                if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });
    }

    SandboxBridge {
        stdin_injector: stdin_tx,
        stuck_tx: spawn_reported_publisher(program_status.clone()),
        program_status,
        output,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sandbox_reports_without_emitting_silence_events() {
        use crate::data::program_status::{parse_body, ProgramState};
        use crate::engine::agent_runtime::program_status::ScanEvent;
        let tracker = ProgramStatusTracker::shared();
        let tx = spawn_reported_publisher(tracker.clone());
        let mut rx = tx.subscribe();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(120), rx.recv())
                .await
                .is_err()
        );
        tracker.lock().unwrap().apply(
            &ScanEvent::Osc(parse_body(b"state=working")),
            std::time::Instant::now(),
        );
        assert!(
            matches!(tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv()).await.unwrap().unwrap(), StuckEvent::Reported(Some(s)) if s.state == ProgramState::Working)
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv())
                .await
                .is_err()
        );
    }

    #[test]
    fn capture_is_bounded_and_keeps_the_newest_bytes() {
        let mut cap = OutputCapture::default();
        cap.push(&vec![b'a'; CAPTURE_CAP]);
        cap.push(b"\nthe end\n");
        assert!(cap.len() <= CAPTURE_CAP, "capture must stay bounded");
        let tail = cap.tail_lines(1);
        assert_eq!(tail, vec!["the end"], "newest bytes must survive the cap");
    }

    #[test]
    fn tail_lines_returns_last_n_non_empty_lines() {
        let mut cap = OutputCapture::default();
        cap.push(b"one\n\ntwo\n   \nthree\n");
        assert_eq!(cap.tail_lines(2), vec!["two", "three"]);
        assert_eq!(cap.tail_lines(10), vec!["one", "two", "three"]);
    }

    #[test]
    fn tail_lines_strips_ansi_and_control_sequences() {
        let mut cap = OutputCapture::default();
        cap.push(b"\x1b[31mERROR:\x1b[0m boom\r\n");
        cap.push(b"\x1b]0;window title\x07plain after osc\r\n");
        assert_eq!(cap.tail_lines(10), vec!["ERROR: boom", "plain after osc"]);
    }

    #[test]
    fn strip_control_sequences_keeps_tabs_and_newlines() {
        assert_eq!(strip_control_sequences("a\tb\nc\x07d"), "a\tb\ncd");
    }

    #[test]
    fn tail_lines_empty_capture_is_empty() {
        assert!(OutputCapture::default().tail_lines(5).is_empty());
    }
}
