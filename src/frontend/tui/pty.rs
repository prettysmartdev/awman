//! Pseudo-terminal management — wraps `portable-pty` for interactive shell
//! sessions.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// Events emitted by a running PTY session.
#[derive(Debug)]
pub enum PtyEvent {
    Data(Vec<u8>),
    Exit(i32),
}

/// A running PTY session with background reader/writer/waiter threads.
pub struct PtySession {
    writer: Box<dyn Write + Send>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    pub rx: mpsc::Receiver<PtyEvent>,
}

impl PtySession {
    /// Spawn a command inside a new PTY of the given size.
    pub fn spawn(
        cmd: &str,
        args: &[&str],
        cwd: &std::path::Path,
        cols: u16,
        rows: u16,
    ) -> Result<Self, String> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("failed to open PTY: {e}"))?;

        let mut command = CommandBuilder::new(cmd);
        for a in args {
            command.arg(*a);
        }
        command.cwd(cwd);

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| format!("failed to spawn: {e}"))?;

        let (tx, rx) = mpsc::channel();
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("failed to take PTY writer: {e}"))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("failed to clone PTY reader: {e}"))?;

        // Reader thread
        let tx_read = tx.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx_read.send(PtyEvent::Data(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Wait thread
        let tx_wait = tx;
        thread::spawn(move || {
            let mut child = child;
            match child.wait() {
                Ok(status) => {
                    let code = status.exit_code().try_into().unwrap_or(1);
                    let _ = tx_wait.send(PtyEvent::Exit(code));
                }
                Err(_) => {
                    let _ = tx_wait.send(PtyEvent::Exit(1));
                }
            }
        });

        Ok(Self {
            writer,
            _master: pair.master,
            rx,
        })
    }

    /// Write bytes to the PTY (user keystrokes).
    pub fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
        self.writer
            .write_all(data)
            .map_err(|e| format!("PTY write failed: {e}"))?;
        self.writer
            .flush()
            .map_err(|e| format!("PTY flush failed: {e}"))
    }

    /// Resize the PTY.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        self._master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("PTY resize failed: {e}"))
    }
}
