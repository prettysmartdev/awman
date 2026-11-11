//! The process supervisor: the only way tests start processes.
//!
//! Every child:
//! - leads its own process group (Unix) or runs in its own job object,
//!   created suspended and assigned before it runs (Windows);
//! - gets exactly the sandbox environment plus the builder's changes
//!   (the parent environment is cleared first) and `/dev/null` stdin;
//! - inherits only descriptors 0–2 (everything else is marked close-on-exec
//!   in the parent before the spawn);
//! - has its stdout/stderr drained by reader threads into bounded memory and
//!   into `<sandbox>/proc/<n>.out|err` (size-capped, truncation marked);
//! - is killed with its whole tree when its deadline passes (a watchdog
//!   thread, so this works while the test thread is blocked), on drop, on
//!   panic and at [`crate::TestSandbox::finish`].
//!
//! `spawn` returns only after the [`Ready`] condition was observed; on expiry
//! it kills the tree and panics with the label, command line and output.

use crate::sandbox::Shared;
use crate::sys;
use crate::wait;
use crate::EnvMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Bytes of each stream kept in memory.
const RETAIN_BYTES: usize = 1 << 20;
/// Bytes of each stream written to `<sandbox>/proc/<n>.out|err`.
const FILE_CAP_BYTES: u64 = 16 << 20;
/// How long to wait for output pipes to close after the child is reaped.
const PIPE_CLOSE_GRACE: Duration = Duration::from_secs(2);

/// What the child reads on stdin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Stdin {
    /// `/dev/null` (the default).
    #[default]
    Null,
    /// A pipe the test writes to through [`OwnedChild::stdin`].
    Piped,
    /// These bytes, then end of file.
    Bytes(Vec<u8>),
}

/// The condition `spawn` waits for before it returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ready {
    /// Return right after the process started.
    Immediately,
    /// A stdout line starting with this prefix.
    StdoutLine(String),
    /// A stderr line starting with this prefix.
    StderrLine(String),
    /// This file exists.
    FileExists(PathBuf),
    /// A TCP connection to this address succeeds.
    TcpAccepts(SocketAddr),
    /// A Unix-socket connection to this path succeeds (Unix only).
    UnixAccepts(PathBuf),
}

/// How a child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Exited with this code.
    Code(i32),
    /// Killed by this signal (Unix), not by the supervisor's deadline.
    Signal(i32),
    /// Killed by the supervisor because its deadline passed.
    KilledAtDeadline,
}

impl Exit {
    /// `Code(0)`.
    pub fn success(self) -> bool {
        self == Exit::Code(0)
    }
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exit::Code(code) => write!(f, "exit code {code}"),
            Exit::Signal(signal) => write!(f, "signal {signal}"),
            Exit::KilledAtDeadline => write!(f, "killed at deadline"),
        }
    }
}

/// The result of [`OwnedCommand::run`].
#[derive(Debug, Clone)]
pub struct Output {
    /// How the process ended.
    pub exit: Exit,
    /// Captured stdout (bounded; see the module docs).
    pub stdout: Vec<u8>,
    /// Captured stderr (bounded).
    pub stderr: Vec<u8>,
}

impl Output {
    /// stdout as (lossy) UTF-8.
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// stderr as (lossy) UTF-8.
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// Panic with both streams unless the process exited with code 0.
    pub fn assert_success(&self) -> &Output {
        assert!(
            self.exit.success(),
            "process failed: {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.exit,
            self.stdout_text(),
            self.stderr_text()
        );
        self
    }
}

/// Builder for a supervised child. Create it with
/// [`crate::TestSandbox::command`].
#[must_use = "a command does nothing until spawned or run"]
pub struct OwnedCommand {
    shared: Arc<Shared>,
    program: OsString,
    args: Vec<OsString>,
    env: EnvMap,
    cwd: PathBuf,
    stdin: Stdin,
    label: Option<String>,
    deadline: Duration,
    ready: Ready,
    max_output: usize,
}

impl OwnedCommand {
    pub(crate) fn new(shared: Arc<Shared>, program: &OsStr, env: EnvMap, cwd: PathBuf) -> Self {
        let deadline = shared.tier.budget();
        OwnedCommand {
            shared,
            program: program.to_os_string(),
            args: Vec::new(),
            env,
            cwd,
            stdin: Stdin::Null,
            label: None,
            deadline,
            ready: Ready::Immediately,
            max_output: RETAIN_BYTES,
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

    /// Several variables, e.g. from a backend's environment.
    pub fn envs(mut self, vars: &EnvMap) -> Self {
        for (k, v) in vars.iter() {
            self.env.set(k, v);
        }
        self
    }

    /// Remove a variable from the child's environment.
    pub fn env_remove(mut self, name: impl AsRef<OsStr>) -> Self {
        self.env.remove(name);
        self
    }

    /// Working directory (default: the sandbox `work` directory).
    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.cwd = dir.as_ref().to_path_buf();
        self
    }

    /// What the child reads on stdin.
    pub fn stdin(mut self, input: Stdin) -> Self {
        self.stdin = input;
        self
    }

    /// Name used in diagnostics and evidence (default: the program name).
    pub fn label(mut self, label: &str) -> Self {
        self.label = Some(label.to_string());
        self
    }

    /// Kill the tree after `limit` (default: the tier budget).
    pub fn deadline(mut self, limit: Duration) -> Self {
        self.deadline = limit;
        self
    }

    /// What `spawn` waits for before returning (default: `Immediately`).
    pub fn ready(mut self, signal: Ready) -> Self {
        self.ready = signal;
        self
    }

    /// Bytes of each output stream kept in memory (default 1 MiB; the
    /// `proc/<n>.out|err` files keep up to 16 MiB). Larger output is
    /// truncated with an explicit marker.
    pub fn max_output(mut self, bytes: usize) -> Self {
        self.max_output = bytes;
        self
    }

    fn command_line(&self) -> String {
        std::iter::once(&self.program)
            .chain(self.args.iter())
            .map(|a| {
                let s = a.to_string_lossy();
                if s.contains(' ') || s.is_empty() {
                    format!("'{s}'")
                } else {
                    s.into_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Start the child and wait for its readiness signal.
    pub fn spawn(self) -> OwnedChild {
        let started = Instant::now();
        let shared = self.shared.clone();
        shared.check_may_spawn();
        let command_line = self.shared.redact(&self.command_line());
        let label = self.label.clone().unwrap_or_else(|| {
            Path::new(&self.program)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| command_line.clone())
        });
        let Some(program) = self.env.resolve(&self.program) else {
            panic!(
                "cannot start `{label}`: `{}` is not on the sandbox PATH ({:?})",
                self.program.to_string_lossy(),
                self.env.path_dirs()
            );
        };
        shared.check_program(&program, &self.args);

        let index = shared.next_process_index();
        let proc_dir = shared.root.join("proc");
        std::fs::create_dir_all(&proc_dir).expect("create <sandbox>/proc");
        let stdout_path = proc_dir.join(format!("{index}.out"));
        let stderr_path = proc_dir.join(format!("{index}.err"));

        let mut cmd = Command::new(&program);
        cmd.args(&self.args)
            .env_clear()
            .envs(self.env.iter())
            .current_dir(&self.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(match self.stdin {
                Stdin::Null => Stdio::null(),
                Stdin::Piped | Stdin::Bytes(_) => Stdio::piped(),
            });
        sys::prepare(&mut cmd);

        let mut child = spawn_retrying_busy(&mut cmd, self.deadline, &label, &command_line);
        let group = match sys::after_spawn(&child) {
            Ok(group) => group,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("cannot supervise `{label}` ({command_line}): {e}");
            }
        };
        let stdout = Capture::start(
            child.stdout.take().expect("piped stdout"),
            stdout_path.clone(),
            self.max_output,
        );
        let stderr = Capture::start(
            child.stderr.take().expect("piped stderr"),
            stderr_path.clone(),
            self.max_output,
        );
        let mut stdin_pipe = child.stdin.take();
        if let Stdin::Bytes(bytes) = &self.stdin {
            let mut pipe = stdin_pipe.take().expect("piped stdin");
            let bytes = bytes.clone();
            std::thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            });
        }
        let core = Arc::new(ChildCore {
            label: label.clone(),
            command_line: command_line.clone(),
            pid: child.id(),
            group,
            child: Mutex::new(Some(child)),
            exit: Mutex::new(None),
            killed_at_deadline: AtomicBool::new(false),
            group_gone: AtomicBool::new(false),
            survivors: AtomicBool::new(false),
            leaked: AtomicBool::new(false),
            stdout,
            stderr,
            stdout_path,
            stderr_path,
            deadline: Mutex::new(None),
        });
        core.start_deadline(self.deadline);
        shared.register_child(core.clone());

        let owned = OwnedChild {
            core,
            stdin: stdin_pipe,
        };
        let remaining = self.deadline.saturating_sub(started.elapsed());
        owned.await_ready(&self.ready, remaining);
        owned
    }

    /// Spawn, wait for exit within the deadline, and return the output.
    /// A deadline expiry is reported as [`Exit::KilledAtDeadline`].
    pub fn run(self) -> Output {
        let deadline = self.deadline;
        let mut child = self.ready(Ready::Immediately).spawn();
        child.stdin.take(); // close a piped stdin: nobody can write to it now
        let exit = wait::poll(deadline + Duration::from_secs(5), || child.try_exit())
            .unwrap_or_else(|| {
                child.kill_and_reap();
                Exit::KilledAtDeadline
            });
        child.core.wait_pipes_closed(PIPE_CLOSE_GRACE);
        Output {
            exit,
            stdout: child.core.stdout.bytes(),
            stderr: child.core.stderr.bytes(),
        }
    }
}

fn spawn_retrying_busy(cmd: &mut Command, limit: Duration, label: &str, line: &str) -> Child {
    let started = Instant::now();
    let mut delay = Duration::from_millis(2);
    loop {
        sys::before_spawn();
        // The supervisor is the one sanctioned spawn site for test code.
        #[allow(clippy::disallowed_methods)]
        match cmd.spawn() {
            Ok(child) => return child,
            // Guards: 39fe0e05, 72537ed5, d5043695 executable-busy launches.
            Err(e) if sys::is_text_file_busy(&e) && started.elapsed() < limit => {
                wait::pause(delay);
                delay = (delay * 2).min(Duration::from_millis(50));
            }
            Err(e) => panic!("cannot start `{label}` ({line}): {e}"),
        }
    }
}

/// A supervised child process. Dropping it kills and reaps the whole tree.
pub struct OwnedChild {
    core: Arc<ChildCore>,
    stdin: Option<ChildStdin>,
}

impl fmt::Debug for OwnedChild {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedChild")
            .field("label", &self.core.label)
            .field("pid", &self.core.pid)
            .finish()
    }
}

impl OwnedChild {
    /// The process ID.
    pub fn pid(&self) -> u32 {
        self.core.pid
    }

    /// The label used in diagnostics.
    pub fn label(&self) -> &str {
        &self.core.label
    }

    /// Wait up to `limit` for the child to exit; panics with diagnostics on
    /// expiry (the tree is killed first).
    pub fn wait(&mut self, limit: Duration) -> Exit {
        self.stdin.take();
        match wait::poll(limit, || self.core.try_exit()) {
            Some(exit) => {
                self.core.wait_pipes_closed(PIPE_CLOSE_GRACE);
                exit
            }
            None => {
                self.core.kill_and_reap();
                self.core.wait_pipes_closed(PIPE_CLOSE_GRACE);
                panic!(
                    "`{}` did not exit within {limit:?}\n{}",
                    self.core.label,
                    self.core.diagnostics()
                );
            }
        }
    }

    /// The exit status if the child has exited (reaps it).
    pub fn try_exit(&mut self) -> Option<Exit> {
        self.core.try_exit()
    }

    /// SIGINT to the group (Unix) / CTRL_BREAK (Windows).
    pub fn interrupt(&mut self) {
        if !self.core.group_gone.load(Ordering::SeqCst) {
            self.core.group.interrupt();
        }
    }

    /// SIGTERM to the group (Unix) / terminate the job (Windows).
    pub fn terminate(&mut self) {
        if !self.core.group_gone.load(Ordering::SeqCst) {
            self.core.group.terminate();
        }
    }

    /// Kill the whole tree, then reap. Idempotent.
    pub fn kill_and_reap(&mut self) {
        self.stdin.take();
        self.core.kill_and_reap();
    }

    /// The child's stdin. Panics unless it was spawned with [`Stdin::Piped`].
    pub fn stdin(&mut self) -> &mut dyn Write {
        self.stdin
            .as_mut()
            .expect("stdin is not piped: use .stdin(Stdin::Piped)")
    }

    /// Close the child's stdin (end of file).
    pub fn close_stdin(&mut self) {
        self.stdin.take();
    }

    /// stdout captured so far (lossy UTF-8).
    pub fn stdout_so_far(&self) -> String {
        self.core.stdout.text()
    }

    /// stderr captured so far (lossy UTF-8).
    pub fn stderr_so_far(&self) -> String {
        self.core.stderr.text()
    }

    /// Wait until stdout contains `needle`; return stdout so far. Panics with
    /// diagnostics on expiry or if stdout closes first.
    pub fn wait_for_stdout(&self, needle: &str, limit: Duration) -> String {
        self.wait_stream(&self.core.stdout, "stdout", needle, limit)
    }

    /// Wait until stderr contains `needle`; return stderr so far.
    pub fn wait_for_stderr(&self, needle: &str, limit: Duration) -> String {
        self.wait_stream(&self.core.stderr, "stderr", needle, limit)
    }

    fn wait_stream(&self, capture: &Capture, name: &str, needle: &str, limit: Duration) -> String {
        let found =
            capture.wait_until(limit, |data| String::from_utf8_lossy(data).contains(needle));
        if let Err(why) = found {
            panic!(
                "`{}`: {name} {why} before it contained {needle:?}\n{}",
                self.core.label,
                self.core.diagnostics()
            );
        }
        capture.text()
    }

    fn await_ready(&self, ready: &Ready, limit: Duration) {
        let core = &self.core;
        let outcome: Result<(), String> = match ready {
            Ready::Immediately => Ok(()),
            Ready::StdoutLine(prefix) => core
                .stdout
                .wait_until(limit, |d| has_line_starting(d, prefix))
                .map_err(|why| format!("stdout {why} without a line starting with {prefix:?}")),
            Ready::StderrLine(prefix) => core
                .stderr
                .wait_until(limit, |d| has_line_starting(d, prefix))
                .map_err(|why| format!("stderr {why} without a line starting with {prefix:?}")),
            Ready::FileExists(path) => self.poll_ready(limit, || path.exists(), path.display()),
            Ready::TcpAccepts(addr) => self.poll_ready(
                limit,
                || TcpStream::connect_timeout(addr, Duration::from_millis(200)).is_ok(),
                addr,
            ),
            Ready::UnixAccepts(path) => {
                self.poll_ready(limit, || unix_accepts(path), path.display())
            }
        };
        if let Err(why) = outcome {
            core.kill_and_reap();
            core.wait_pipes_closed(PIPE_CLOSE_GRACE);
            panic!(
                "`{}` was not ready: {why}\n{}",
                core.label,
                core.diagnostics()
            );
        }
    }

    fn poll_ready(
        &self,
        limit: Duration,
        mut probe: impl FnMut() -> bool,
        what: impl fmt::Display,
    ) -> Result<(), String> {
        let mut exited = None;
        let ok = wait::poll(limit, || {
            if probe() {
                return Some(true);
            }
            if let Some(exit) = self.core.peek_exit() {
                exited = Some(exit);
                return Some(false);
            }
            None
        });
        match (ok, exited) {
            (Some(true), _) => Ok(()),
            (_, Some(exit)) => Err(format!("it ended ({exit}) before {what} was ready")),
            _ => Err(format!("timed out after {limit:?} waiting for {what}")),
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.stdin.take();
        self.core.kill_and_reap();
    }
}

#[cfg(unix)]
fn unix_accepts(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

#[cfg(not(unix))]
fn unix_accepts(_path: &Path) -> bool {
    false
}

fn has_line_starting(data: &[u8], prefix: &str) -> bool {
    data.split(|b| *b == b'\n')
        .any(|line| line.starts_with(prefix.as_bytes()))
}

/// Shared state of one child, also held by the sandbox for the leak check.
pub(crate) struct ChildCore {
    pub label: String,
    pub command_line: String,
    pub pid: u32,
    group: sys::Group,
    child: Mutex<Option<Child>>,
    exit: Mutex<Option<Exit>>,
    killed_at_deadline: AtomicBool,
    /// The group was observed empty; it must never be signalled again.
    group_gone: AtomicBool,
    /// The leader exited while other members of its group were still running.
    survivors: AtomicBool,
    /// Survivors were still running when the supervisor killed the group.
    leaked: AtomicBool,
    stdout: Arc<Capture>,
    stderr: Arc<Capture>,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    deadline: Mutex<Option<(mpsc::Sender<()>, JoinHandle<()>)>>,
}

impl ChildCore {
    fn start_deadline(self: &Arc<Self>, limit: Duration) {
        let (tx, rx) = mpsc::channel::<()>();
        let weak: Weak<ChildCore> = Arc::downgrade(self);
        let handle = std::thread::Builder::new()
            .name(format!("deadline:{}", self.label))
            .spawn(move || {
                if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(limit) {
                    if let Some(core) = weak.upgrade() {
                        core.killed_at_deadline.store(true, Ordering::SeqCst);
                        if !core.group_gone.load(Ordering::SeqCst) {
                            core.group.kill();
                        }
                    }
                }
            })
            .expect("start deadline thread");
        *self.deadline.lock().unwrap() = Some((tx, handle));
    }

    fn cancel_deadline(&self) {
        let taken = self.deadline.lock().unwrap().take();
        if let Some((tx, handle)) = taken {
            drop(tx);
            if handle.thread().id() != std::thread::current().id() {
                let _ = handle.join();
            }
        }
    }

    pub fn group_id(&self) -> i64 {
        self.group.id()
    }

    /// The recorded exit without reaping.
    fn peek_exit(&self) -> Option<Exit> {
        if let Some(exit) = *self.exit.lock().unwrap() {
            return Some(exit);
        }
        self.try_exit()
    }

    pub fn exit(&self) -> Option<Exit> {
        *self.exit.lock().unwrap()
    }

    fn status_to_exit(&self, status: std::process::ExitStatus) -> Exit {
        if self.killed_at_deadline.load(Ordering::SeqCst) {
            return Exit::KilledAtDeadline;
        }
        if let Some(code) = status.code() {
            return Exit::Code(code);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                return Exit::Signal(signal);
            }
        }
        Exit::Code(-1)
    }

    /// Reap the leader if it has exited.
    pub fn try_exit(&self) -> Option<Exit> {
        let mut guard = self.child.lock().unwrap();
        let Some(child) = guard.as_mut() else {
            return *self.exit.lock().unwrap();
        };
        let status = match child.try_wait() {
            Ok(Some(status)) => status,
            Ok(None) => return None,
            Err(e) => panic!("waiting for `{}` failed: {e}", self.label),
        };
        let exit = self.status_to_exit(status);
        *guard = None;
        *self.exit.lock().unwrap() = Some(exit);
        drop(guard);
        if self.group.has_members() {
            self.survivors.store(true, Ordering::SeqCst);
        } else {
            self.group_gone.store(true, Ordering::SeqCst);
            self.cancel_deadline();
        }
        Some(exit)
    }

    /// Kill the whole tree and reap the leader. Idempotent.
    pub fn kill_and_reap(&self) {
        if !self.group_gone.load(Ordering::SeqCst) {
            if self.survivors.load(Ordering::SeqCst) && self.group.has_members() {
                self.leaked.store(true, Ordering::SeqCst);
            }
            self.group.kill();
        }
        let taken = self.child.lock().unwrap().take();
        if let Some(mut child) = taken {
            let status = child.wait();
            let mut exit = self.exit.lock().unwrap();
            if exit.is_none() {
                *exit = Some(match status {
                    Ok(status) => self.status_to_exit(status),
                    Err(_) => Exit::Code(-1),
                });
            }
        }
        if !self.group_gone.load(Ordering::SeqCst) {
            // Killed members are zombies until their new parent reaps them.
            let empty = wait::poll(Duration::from_secs(2), || {
                (!self.group.has_members()).then_some(())
            });
            if empty.is_some() {
                self.group_gone.store(true, Ordering::SeqCst);
            }
        }
        self.cancel_deadline();
    }

    /// Leak check for this child (run by the sandbox at the end of the test).
    pub fn leak_check(&self) -> Vec<crate::Leak> {
        let mut leaks = Vec::new();
        let still_running = self.survivors.load(Ordering::SeqCst)
            && !self.group_gone.load(Ordering::SeqCst)
            && self.group.has_members();
        if still_running || self.leaked.load(Ordering::SeqCst) {
            leaks.push(crate::Leak::new(
                "process",
                format!(
                    "`{}` (pid {}) exited but left processes running in its group",
                    self.label, self.pid
                ),
            ));
        }
        self.kill_and_reap();
        if !self.wait_pipes_closed(PIPE_CLOSE_GRACE) {
            leaks.push(crate::Leak::new(
                "process",
                format!(
                    "the output of `{}` is still held open by a process outside its group",
                    self.label
                ),
            ));
        }
        leaks
    }

    fn wait_pipes_closed(&self, limit: Duration) -> bool {
        self.stdout.wait_closed(limit) && self.stderr.wait_closed(limit)
    }

    pub fn diagnostics(&self) -> String {
        format!(
            "command: {}\npid: {}  status: {}\n--- stdout ({}) ---\n{}\n--- stderr ({}) ---\n{}",
            self.command_line,
            self.pid,
            self.exit()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "running".to_string()),
            self.stdout_path.display(),
            tail(&self.stdout.text(), 4000),
            self.stderr_path.display(),
            tail(&self.stderr.text(), 4000),
        )
    }
}

fn tail(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// One drained output stream.
pub(crate) struct Capture {
    state: Mutex<CaptureState>,
    changed: Condvar,
}

struct CaptureState {
    data: Vec<u8>,
    truncated: bool,
    closed: bool,
}

impl Capture {
    pub(crate) fn start(
        mut reader: impl Read + Send + 'static,
        path: PathBuf,
        retain: usize,
    ) -> Arc<Capture> {
        let capture = Arc::new(Capture {
            state: Mutex::new(CaptureState {
                data: Vec::new(),
                truncated: false,
                closed: false,
            }),
            changed: Condvar::new(),
        });
        let writer = capture.clone();
        std::thread::spawn(move || {
            let mut file = File::create(&path).ok();
            let mut written: u64 = 0;
            let mut buf = [0u8; 8192];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                if let Some(f) = file.as_mut() {
                    let room = FILE_CAP_BYTES.saturating_sub(written) as usize;
                    let take = n.min(room);
                    let _ = f.write_all(&buf[..take]);
                    written += take as u64;
                    if take < n {
                        let _ = f.write_all(b"\n[awman-test-support: output truncated]\n");
                        file = None;
                    }
                }
                let mut state = writer.state.lock().unwrap();
                let room = retain.saturating_sub(state.data.len());
                if room < n {
                    state.truncated = true;
                }
                state.data.extend_from_slice(&buf[..n.min(room)]);
                writer.changed.notify_all();
            }
            let mut state = writer.state.lock().unwrap();
            state.closed = true;
            writer.changed.notify_all();
        });
        capture
    }

    pub(crate) fn bytes(&self) -> Vec<u8> {
        let state = self.state.lock().unwrap();
        let mut data = state.data.clone();
        if state.truncated {
            data.extend_from_slice(b"\n[awman-test-support: output truncated]\n");
        }
        data
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes()).into_owned()
    }

    /// Block (on the condition variable, no polling) until `pred` holds.
    pub(crate) fn wait_until(
        &self,
        limit: Duration,
        pred: impl Fn(&[u8]) -> bool,
    ) -> Result<(), &'static str> {
        let started = Instant::now();
        let mut state = self.state.lock().unwrap();
        loop {
            if pred(&state.data) {
                return Ok(());
            }
            if state.closed {
                return Err("closed");
            }
            let elapsed = started.elapsed();
            if elapsed >= limit {
                return Err("timed out");
            }
            state = self.changed.wait_timeout(state, limit - elapsed).unwrap().0;
        }
    }

    pub(crate) fn wait_closed(&self, limit: Duration) -> bool {
        let started = Instant::now();
        let mut state = self.state.lock().unwrap();
        while !state.closed {
            let elapsed = started.elapsed();
            if elapsed >= limit {
                return false;
            }
            state = self.changed.wait_timeout(state, limit - elapsed).unwrap().0;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_prefix_matching() {
        assert!(has_line_starting(b"a\nready on 1\n", "ready"));
        assert!(has_line_starting(b"ready", "ready"));
        assert!(!has_line_starting(b"not ready\n", "ready"));
    }

    #[test]
    fn exit_display_and_success() {
        assert!(Exit::Code(0).success());
        assert!(!Exit::Signal(9).success());
        assert_eq!(Exit::KilledAtDeadline.to_string(), "killed at deadline");
    }

    #[test]
    fn tail_respects_char_boundaries() {
        assert_eq!(tail("abc", 10), "abc");
        assert_eq!(tail("ééé", 3), "é");
    }
}
