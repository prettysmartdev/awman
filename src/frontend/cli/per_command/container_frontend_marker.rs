//! `ContainerFrontend` impl for the CLI.
//!
//! All container I/O flows through `ContainerIo` channels. The CLI
//! constructs either interactive (PTY with raw mode) or non-interactive
//! (piped, no PTY) channels depending on `self.non_interactive`.

use async_trait::async_trait;
use std::io::Write;

use crate::engine::container::frontend::{
    ContainerFrontend, ContainerIo, ContainerProgress, ContainerStatus,
};
use crate::engine::message::{UserMessage, UserMessageSink};

use crate::frontend::cli::command_frontend::{CliFrontend, RawModeGuard};

#[async_trait]
impl ContainerFrontend for CliFrontend {
    fn report_status(&mut self, _status: ContainerStatus) {}
    fn report_progress(&mut self, _progress: ContainerProgress) {}

    fn take_container_io(&mut self) -> ContainerIo {
        if self.non_interactive {
            self.take_non_interactive_io()
        } else {
            self.take_interactive_io()
        }
    }
}

impl CliFrontend {
    /// Non-interactive: piped stdout/stderr; the frontend keeps no stdin
    /// sender. The engine owns the single `stdin_tx` returned in
    /// `ContainerIo`, writes the seeded prompt through it, then drops it so
    /// the writer task sees EOF (see `spawn_piped_docker` /
    /// `spawn_piped_apple`).
    fn take_non_interactive_io(&mut self) -> ContainerIo {
        let (stdout_tx, mut stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stderr_tx, mut stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

        tokio::spawn(async move {
            while let Some(bytes) = stdout_rx.recv().await {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&bytes);
                let _ = out.flush();
            }
        });

        tokio::spawn(async move {
            while let Some(bytes) = stderr_rx.recv().await {
                let mut err = std::io::stderr().lock();
                let _ = err.write_all(&bytes);
                let _ = err.flush();
            }
        });

        ContainerIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }

    /// Interactive: raw mode, PTY-bridged stdout/stderr, raw stdin reader,
    /// SIGWINCH-driven resize channel.
    fn take_interactive_io(&mut self) -> ContainerIo {
        let (stdout_tx, mut stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stderr_tx, mut stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();

        // Enable raw mode and store the RAII guard.
        match RawModeGuard::enable() {
            Ok(guard) => self.raw_mode_guard = Some(guard),
            Err(e) => {
                eprintln!("awman: failed to enable raw mode: {e}");
                return self.take_non_interactive_io();
            }
        }

        let initial_size = crossterm::terminal::size().ok();

        // Drain stdout to host stdout (unbuffered for raw mode).
        tokio::spawn(async move {
            while let Some(bytes) = stdout_rx.recv().await {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&bytes);
                let _ = out.flush();
            }
        });

        // Drain stderr to host stderr (unbuffered for raw mode).
        tokio::spawn(async move {
            while let Some(bytes) = stderr_rx.recv().await {
                let mut err = std::io::stderr().lock();
                let _ = err.write_all(&bytes);
                let _ = err.flush();
            }
        });

        // Raw-mode stdin reader: forwards bytes from the terminal into
        // stdin_tx. The thread is woken every 200ms by `poll(2)` so it can
        // check the shutdown flag — without this, a blocking `read()` would
        // leave the thread alive after the container exits, racing the next
        // step's reader thread for `/dev/stdin`. The shutdown flag is set
        // by `report_step_status` on a terminal status.
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.stdin_reader_shutdown = Some(shutdown.clone());
        let stdin_writer = stdin_tx.clone();
        #[cfg(unix)]
        {
            std::thread::spawn(move || spawn_unix_stdin_reader(stdin_writer, shutdown));
        }
        #[cfg(not(unix))]
        {
            // No poll(2) on non-Unix; fall back to blocking read. The reader
            // thread will leak until a final keystroke arrives — Windows
            // interactive support is best-effort.
            let _ = &shutdown; // keep ref alive for parity
            std::thread::spawn(move || {
                use std::io::Read as _;
                let mut stdin = std::io::stdin().lock();
                let mut buf = [0u8; 1024];
                loop {
                    match stdin.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if stdin_writer.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        // SIGWINCH listener: propagates terminal size changes to the container PTY.
        #[cfg(unix)]
        {
            let resize_sender = resize_tx.clone();
            tokio::spawn(async move {
                let mut sig =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                        .expect("failed to register SIGWINCH handler");
                while sig.recv().await.is_some() {
                    if let Ok((cols, rows)) = crossterm::terminal::size() {
                        if resize_sender.send((cols, rows)).is_err() {
                            break;
                        }
                    }
                }
            });
        }

        ContainerIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: Some(resize_rx),
            initial_size,
        }
    }
}

/// Unix interactive stdin reader. Polls `/dev/stdin` with a 200 ms timeout so
/// the thread wakes up periodically to check the shutdown flag, then drains
/// any ready bytes through `stdin_writer`. Exits when:
///   - `shutdown` is set (by `report_step_status` on a terminal status), or
///   - stdin returns EOF (`read` returns 0), or
///   - `stdin_writer.send` fails (channel closed).
#[cfg(unix)]
fn spawn_unix_stdin_reader(
    stdin_writer: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::io::Read as _;
    use std::os::fd::AsFd;
    use std::sync::atomic::Ordering;

    let stdin = std::io::stdin();
    let mut stdin_lock = stdin.lock();
    let mut buf = [0u8; 1024];
    let timeout = PollTimeout::try_from(200u16).unwrap_or(PollTimeout::NONE);

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        // Re-acquire BorrowedFd each iteration; the stdin handle is shared.
        let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, timeout) {
            Ok(0) => continue, // timeout — check shutdown
            Ok(_) => match stdin_lock.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    if stdin_writer.send(buf[..n].to_vec()).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            },
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => return,
        }
    }
}

// ─── Standalone proxy used by InitFrontend / ReadyFrontend ─────────────────

/// Stand-alone `ContainerFrontend` returned by engines that need a
/// `Box<dyn ContainerFrontend>` for a single container's lifetime
/// (`InitFrontend::container_frontend`, etc.). Streams to host stdio.
pub(crate) struct CliContainerProxy;

impl UserMessageSink for CliContainerProxy {
    fn write_message(&mut self, msg: UserMessage) {
        use crate::engine::message::MessageLevel;
        let prefix = match msg.level {
            MessageLevel::Info | MessageLevel::Success => "awman:",
            MessageLevel::Warning => "awman warning:",
            MessageLevel::Error => "awman error:",
        };
        let _ = writeln!(std::io::stderr(), "{prefix} {}", msg.text);
    }
    fn replay_queued(&mut self) {}
}

#[async_trait]
impl ContainerFrontend for CliContainerProxy {
    fn report_status(&mut self, _status: ContainerStatus) {}
    fn report_progress(&mut self, _progress: ContainerProgress) {}

    fn take_container_io(&mut self) -> ContainerIo {
        let (stdout_tx, mut stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stderr_tx, mut stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

        tokio::spawn(async move {
            while let Some(bytes) = stdout_rx.recv().await {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&bytes);
                let _ = out.flush();
            }
        });

        tokio::spawn(async move {
            while let Some(bytes) = stderr_rx.recv().await {
                let mut err = std::io::stderr().lock();
                let _ = err.write_all(&bytes);
                let _ = err.flush();
            }
        });

        // Engine owns the single `stdin_tx`; piped paths drop it after
        // seeding so the child sees stdin EOF.
        ContainerIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }
}
