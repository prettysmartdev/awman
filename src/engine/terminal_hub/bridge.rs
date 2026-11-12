//! Registers an agent's I/O channel set (the consumer ends of an
//! `agent_runtime::frontend::AgentIo`) as a hub terminal, so engine sessions
//! are served through the one `awman.terminal.v1` hub like every other daemon
//! feature (WI 0142 Part A §1 "Engine terminal transport").
//!
//! Layer 1 mechanics only: output is teed into the mirror and fan-out, hub input
//! grants drive `stdin`, hub resizes drive the PTY. The Layer 2 relay decides
//! which slot becomes a terminal and with what owner.
//!
//! Contract: `/awman/context/workflow/0142-design.md` §4.6
//! (`TerminalHub::bridge_agent_channels`). Filled by the
//! `engine-feature-layer1` step.

use std::sync::{Arc, Mutex};

use super::{input_channel, TerminalHandle, TerminalHub, TerminalInput, TerminalRegistration};

/// A registered agent terminal and the task that feeds it.
pub struct BridgedTerminal {
    pub handle: TerminalHandle,
    /// Ends when `stdout_rx` closes (the agent's output ended); the terminal
    /// is then marked exited with code 0 unless `handle.exit` already ran.
    pub task: tokio::task::JoinHandle<()>,
    stdout: Arc<Mutex<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>>,
}

impl BridgedTerminal {
    /// Deliver bytes already queued by the agent before publishing its exit.
    /// The background bridge polls and writes under this same lock, so a
    /// fetched chunk cannot overtake the exit while the relay flushes.
    pub fn flush_pending_output(&self) {
        let mut stdout = self
            .stdout
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while let Ok(bytes) = stdout.try_recv() {
            self.handle.push_output(&bytes);
        }
    }
}

impl TerminalHub {
    /// Register a terminal whose bytes come from `stdout_rx` and whose input
    /// and resizes go to `stdin_tx` / `resize_tx` (the consumer ends of an
    /// `AgentIo`). Must be called inside a Tokio runtime.
    pub fn bridge_agent_channels(
        self: &Arc<Self>,
        registration: TerminalRegistration,
        stdout_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        stdin_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
        resize_tx: tokio::sync::mpsc::UnboundedSender<(u16, u16)>,
    ) -> BridgedTerminal {
        let (input_tx, mut input_rx) = input_channel();
        let resize: Arc<dyn Fn(u16, u16) + Send + Sync> = Arc::new(move |cols, rows| {
            let _ = resize_tx.send((cols, rows));
        });
        let handle = self.register(
            registration,
            TerminalInput {
                stdin: input_tx,
                resize: Some(resize),
            },
        );
        let output = handle.clone();
        let stdout = Arc::new(Mutex::new(stdout_rx));
        let task_stdout = Arc::clone(&stdout);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    open = futures_util::future::poll_fn(|cx| {
                        let mut receiver = task_stdout
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        match receiver.poll_recv(cx) {
                            std::task::Poll::Ready(Some(bytes)) => {
                                output.push_output(&bytes);
                                std::task::Poll::Ready(true)
                            }
                            std::task::Poll::Ready(None) => std::task::Poll::Ready(false),
                            std::task::Poll::Pending => std::task::Poll::Pending,
                        }
                    }) => if !open { break },
                    // Drained here, by the writer itself, so a stalled agent
                    // pushes back on viewers through the bounded queue.
                    input = input_rx.recv() => match input {
                        Some(bytes) => {
                            if stdin_tx.send(bytes).is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                }
            }
            output.exit(0);
        });
        BridgedTerminal {
            handle,
            task,
            stdout,
        }
    }
}

// ─── Host PTYs ─────────────────────────────────────────────────────────────
//
// A host-side interactive subprocess (`DialogRequest::TerminalRelease`, e.g.
// `gh auth login` for a prerequisite) has no terminal of its own in the
// daemon. The engine opens a PTY pair here, registers the master as a hub
// terminal (so the frontend relays its bytes exactly as for an agent), and
// scopes the slave to the command thread with [`HostPty::scope`]; the host
// command spawner (`HostSecretCommand::run`) uses the scoped PTY in place of
// the process's own TTY. WI 0142 Part A, edge cases.

#[cfg(unix)]
mod host_pty {
    use std::cell::RefCell;
    use std::io::{Read, Write};
    use std::os::fd::OwnedFd;
    use std::sync::Arc;

    use super::super::{input_channel, TerminalHandle, TerminalHub, TerminalInput};
    use super::TerminalRegistration;

    thread_local! {
        static CURRENT: RefCell<Option<Arc<HostPty>>> = const { RefCell::new(None) };
    }

    /// A PTY pair in the daemon. The slave is what a host subprocess gets as
    /// its stdin/stdout/stderr and controlling terminal.
    pub struct HostPty {
        master: OwnedFd,
        slave: OwnedFd,
    }

    impl std::fmt::Debug for HostPty {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("HostPty").finish_non_exhaustive()
        }
    }

    fn winsize(cols: u16, rows: u16) -> nix::pty::Winsize {
        nix::pty::Winsize {
            ws_row: rows.max(2),
            ws_col: cols.max(2),
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    }

    impl HostPty {
        /// Open a PTY pair of `cols`×`rows`.
        pub fn open(cols: u16, rows: u16) -> std::io::Result<Arc<Self>> {
            let pair = nix::pty::openpty(Some(&winsize(cols, rows)), None)
                .map_err(std::io::Error::from)?;
            Ok(Arc::new(Self {
                master: pair.master,
                slave: pair.slave,
            }))
        }

        /// The slave as three stdio handles for a child process.
        pub fn slave_stdio(
            &self,
        ) -> std::io::Result<(
            std::process::Stdio,
            std::process::Stdio,
            std::process::Stdio,
        )> {
            Ok((
                self.slave.try_clone()?.into(),
                self.slave.try_clone()?.into(),
                self.slave.try_clone()?.into(),
            ))
        }

        /// Run `f` with this PTY as the calling thread's host terminal.
        pub fn scope<T>(self: &Arc<Self>, f: impl FnOnce() -> T) -> T {
            struct Reset(Option<Arc<HostPty>>);
            impl Drop for Reset {
                fn drop(&mut self) {
                    let previous = self.0.take();
                    CURRENT.with(|current| *current.borrow_mut() = previous);
                }
            }
            let previous = CURRENT.with(|current| current.borrow_mut().replace(Arc::clone(self)));
            let _reset = Reset(previous);
            f()
        }

        /// The host terminal scoped to this thread, if any.
        pub fn current() -> Option<Arc<HostPty>> {
            CURRENT.with(|current| current.borrow().clone())
        }

        /// Register the master side as a hub terminal: its output is teed to
        /// the hub and hub input is written to it. The
        /// reader thread ends when every slave handle has closed.
        pub fn bridge(
            self: &Arc<Self>,
            hub: &Arc<TerminalHub>,
            registration: TerminalRegistration,
        ) -> std::io::Result<TerminalHandle> {
            let mut reader = std::fs::File::from(self.master.try_clone()?);
            let mut writer = std::fs::File::from(self.master.try_clone()?);
            let (input_tx, mut input_rx) = input_channel();
            let handle = hub.register(
                registration,
                TerminalInput {
                    stdin: input_tx,
                    // No safe PTY resize is available (the crate forbids
                    // `unsafe`); a host process keeps its opening size.
                    resize: None,
                },
            );
            let output = handle.clone();
            std::thread::Builder::new()
                .name("awman-host-pty-out".into())
                .spawn(move || {
                    let mut buf = [0u8; 8192];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => output.push_output(&buf[..n]),
                        }
                    }
                })?;
            std::thread::Builder::new()
                .name("awman-host-pty-in".into())
                .spawn(move || {
                    while let Some(bytes) = input_rx.blocking_recv() {
                        if writer.write_all(&bytes).is_err() {
                            break;
                        }
                    }
                })?;
            Ok(handle)
        }
    }
}

#[cfg(unix)]
pub use host_pty::HostPty;
