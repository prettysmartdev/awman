//! Supervised processes under a pseudo-terminal (Unix PTY, Windows ConPTY,
//! through `portable-pty`). Same environment, allowlist, deadline and
//! kill-on-drop rules as [`crate::OwnedCommand`].
//!
//! On Unix the child is a session leader (portable-pty calls `setsid`), so
//! its process group is killed as a whole. On Windows only the child itself
//! is killed (ConPTY does not expose a suspended start for job assignment);
//! recorded as a known gap in the WI 0144 support handoff.

use crate::process::{Capture, Exit};
use crate::sandbox::Shared;
use crate::wait;
use crate::EnvMap;
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Builder for a process under a pseudo-terminal
/// ([`crate::TestSandbox::pty_command`]).
#[must_use = "a command does nothing until spawned"]
pub struct PtyCommand {
    shared: Arc<Shared>,
    program: OsString,
    args: Vec<OsString>,
    env: EnvMap,
    cwd: PathBuf,
    label: Option<String>,
    deadline: Duration,
    size: (u16, u16),
}

impl PtyCommand {
    pub(crate) fn new(shared: Arc<Shared>, program: &OsStr, env: EnvMap, cwd: PathBuf) -> Self {
        let deadline = shared.tier.budget();
        let mut env = env;
        env.set("TERM", "xterm-256color");
        PtyCommand {
            shared,
            program: program.to_os_string(),
            args: Vec::new(),
            env,
            cwd,
            label: None,
            deadline,
            size: (24, 80),
        }
    }

    /// Add one argument.
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    /// Add arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_os_string()));
        self
    }

    /// Set a variable on top of the sandbox environment.
    pub fn env(mut self, name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env.set(name, value);
        self
    }

    /// Working directory (default: the sandbox `work` directory).
    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.cwd = dir.as_ref().to_path_buf();
        self
    }

    /// Name used in diagnostics.
    pub fn label(mut self, label: &str) -> Self {
        self.label = Some(label.to_string());
        self
    }

    /// Kill the child after `limit` (default: the tier budget).
    pub fn deadline(mut self, limit: Duration) -> Self {
        self.deadline = limit;
        self
    }

    /// Terminal size in rows and columns (default 24×80).
    pub fn size(mut self, rows: u16, cols: u16) -> Self {
        self.size = (rows, cols);
        self
    }

    /// Start the child.
    pub fn spawn(self) -> PtyChild {
        self.shared.check_may_spawn();
        let label = self
            .label
            .clone()
            .unwrap_or_else(|| self.program.to_string_lossy().into_owned());
        let program = self
            .env
            .resolve(&self.program)
            .unwrap_or_else(|| panic!("cannot start `{label}`: not on the sandbox PATH"));
        self.shared.check_program(&program, &self.args);
        let pty = portable_pty::native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: self.size.0,
                cols: self.size.1,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap_or_else(|e| panic!("BLOCKED[subprocess]: cannot open a pty: {e}"));
        let mut cmd = CommandBuilder::new(&program);
        cmd.args(&self.args);
        cmd.env_clear();
        for (k, v) in self.env.iter() {
            cmd.env(k, v);
        }
        cmd.cwd(&self.cwd);
        crate::sys::before_spawn();
        let child = pair
            .slave
            .spawn_command(cmd)
            .unwrap_or_else(|e| panic!("cannot start `{label}` under a pty: {e}"));
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().expect("pty reader");
        let writer = pair.master.take_writer().expect("pty writer");
        let index = self.shared.next_process_index();
        let proc_dir = self.shared.root.join("proc");
        let _ = std::fs::create_dir_all(&proc_dir);
        let output = Capture::start(reader, proc_dir.join(format!("{index}.pty")), 1 << 20);
        let pid = child.process_id().unwrap_or(0);
        let core = Arc::new(PtyCore {
            label,
            pid,
            child: Mutex::new(child),
            exit: Mutex::new(None),
            killed_at_deadline: AtomicBool::new(false),
            deadline: Mutex::new(None),
        });
        core.start_deadline(self.deadline);
        let cleanup = core.clone();
        self.shared.probes.lock().unwrap().push((
            format!("pty:{}", core.label),
            Box::new(move || {
                cleanup.kill_and_reap();
                Vec::new()
            }),
        ));
        PtyChild {
            core,
            output,
            writer,
            master: pair.master,
        }
    }
}

struct PtyCore {
    label: String,
    pid: u32,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    exit: Mutex<Option<Exit>>,
    killed_at_deadline: AtomicBool,
    deadline: Mutex<Option<(mpsc::Sender<()>, JoinHandle<()>)>>,
}

impl PtyCore {
    fn start_deadline(self: &Arc<Self>, limit: Duration) {
        let (tx, rx) = mpsc::channel::<()>();
        let weak = Arc::downgrade(self);
        let handle = std::thread::spawn(move || {
            if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(limit) {
                if let Some(core) = weak.upgrade() {
                    core.killed_at_deadline.store(true, Ordering::SeqCst);
                    core.kill_tree();
                }
            }
        });
        *self.deadline.lock().unwrap() = Some((tx, handle));
    }

    fn kill_tree(&self) {
        if self.exit.lock().unwrap().is_some() {
            return;
        }
        #[cfg(unix)]
        {
            if self.pid > 0 {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(self.pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
        let _ = self.child.lock().unwrap().kill();
    }

    fn convert(&self, status: portable_pty::ExitStatus) -> Exit {
        if self.killed_at_deadline.load(Ordering::SeqCst) {
            Exit::KilledAtDeadline
        } else if status.signal().is_some() {
            Exit::Signal(9)
        } else {
            Exit::Code(status.exit_code() as i32)
        }
    }

    fn try_exit(&self) -> Option<Exit> {
        if let Some(exit) = *self.exit.lock().unwrap() {
            return Some(exit);
        }
        let status = self.child.lock().unwrap().try_wait().ok()??;
        let exit = self.convert(status);
        *self.exit.lock().unwrap() = Some(exit);
        self.cancel_deadline();
        Some(exit)
    }

    fn kill_and_reap(&self) {
        self.kill_tree();
        if self.exit.lock().unwrap().is_none() {
            let status = self.child.lock().unwrap().wait();
            let exit = status.map(|s| self.convert(s)).unwrap_or(Exit::Code(-1));
            self.exit.lock().unwrap().get_or_insert(exit);
        }
        self.cancel_deadline();
    }

    fn cancel_deadline(&self) {
        if let Some((tx, handle)) = self.deadline.lock().unwrap().take() {
            drop(tx);
            if handle.thread().id() != std::thread::current().id() {
                let _ = handle.join();
            }
        }
    }
}

/// A process running under a pseudo-terminal. Dropping it kills it.
pub struct PtyChild {
    core: Arc<PtyCore>,
    output: Arc<Capture>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
}

impl PtyChild {
    /// The process ID.
    pub fn pid(&self) -> u32 {
        self.core.pid
    }

    /// Type into the terminal.
    pub fn write_all(&mut self, bytes: &[u8]) {
        self.writer
            .write_all(bytes)
            .and_then(|_| self.writer.flush())
            .unwrap_or_else(|e| panic!("writing to `{}`: {e}", self.core.label));
    }

    /// Everything the terminal printed so far (raw, including escapes).
    pub fn output_so_far(&self) -> String {
        self.output.text()
    }

    /// Wait until the output contains `needle`; return the output so far.
    pub fn wait_for_output(&self, needle: &str, limit: Duration) -> String {
        if let Err(why) = self
            .output
            .wait_until(limit, |d| String::from_utf8_lossy(d).contains(needle))
        {
            panic!(
                "`{}`: terminal output {why} before it contained {needle:?}\n--- output ---\n{}",
                self.core.label,
                self.output.text()
            );
        }
        self.output.text()
    }

    /// Resize the terminal.
    pub fn resize(&self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap_or_else(|e| panic!("resizing `{}`: {e}", self.core.label));
    }

    /// Wait up to `limit` for exit; panics (after killing) on expiry.
    /// On Unix this also drains the terminal to EOF, so output read after
    /// wait includes the child's final bytes. A descendant keeping the
    /// terminal open is an error rather than a truncated successful capture.
    pub fn wait(&mut self, limit: Duration) -> Exit {
        #[cfg(unix)]
        let started = std::time::Instant::now();
        match wait::poll(limit, || self.core.try_exit()) {
            Some(exit) => {
                #[cfg(unix)]
                assert!(
                    self.output.wait_closed(limit.saturating_sub(started.elapsed())),
                    "`{}` exited but its terminal did not close within {limit:?}; a descendant may still hold it open\n--- output ---\n{}",
                    self.core.label, self.output.text()
                );
                exit
            }
            None => {
                self.core.kill_and_reap();
                panic!(
                    "`{}` did not exit within {limit:?}\n--- output ---\n{}",
                    self.core.label,
                    self.output.text()
                );
            }
        }
    }

    /// The exit status if it has exited.
    pub fn try_exit(&mut self) -> Option<Exit> {
        self.core.try_exit()
    }

    /// Kill (the whole session on Unix) and reap.
    pub fn kill_and_reap(&mut self) {
        self.core.kill_and_reap();
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        self.core.kill_and_reap();
    }
}
