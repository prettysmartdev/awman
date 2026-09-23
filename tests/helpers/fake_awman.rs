//! A live process that awman's pidfile check accepts as a running daemon.
//!
//! `check_already_running` only honours a pidfile whose PID is alive *and*
//! whose command name contains "awman", so a test that stands up an "already
//! running" daemon needs a real process with such a name. This one is the
//! test's own binary, run with libtest arguments that execute nothing but
//! [`fake_awman_holder_body`], which sleeps. How it gets its awman name
//! depends on where `pid_is_awman` reads the name from:
//!
//! * **macOS** reads `argv[0]` (`ps -o comm`), so the test binary runs from its
//!   own path with `argv[0]` set to the name. No file is created: macOS kills a
//!   freshly copied or linked executable at launch often enough (SIGKILL,
//!   before it is ready) that any new file here is a flaky fixture.
//! * **Linux** (and anything else) reads the executable's file name
//!   (`/proc/<pid>/comm`), so the test binary is hard-linked under the name,
//!   beside itself so the link stays on one filesystem.
//!
//! It used to be a copy of `/bin/sleep`. On macOS that copy showed up under its
//! new name and then died, leaving a zombie that `kill(pid, 0)` still reports
//! alive but `ps` names `<defunct>`: a CLI checking the pidfile a moment later
//! judged the "daemon" stale, cleared its pidfile and started a real one.
//!
//! Included with `#[path = "helpers/fake_awman.rs"] mod fake_awman;`, which is
//! the module path [`HOLDER_TEST`] names.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Duration;

use awman::engine::daemon::DaemonSupervisor;

/// The libtest name of the holder's body, as the including crate sees it.
const HOLDER_TEST: &str = "fake_awman::fake_awman_holder_body";

/// Not a test: the body a [`FakeAwmanProcess`] runs. Ignored, so it runs only
/// when the holder asks for it by name.
#[test]
#[ignore = "runs only as the body of a FakeAwmanProcess"]
fn fake_awman_holder_body() {
    std::thread::sleep(Duration::from_secs(120));
}

pub struct FakeAwmanProcess {
    /// The hard link's directory, where one was needed; removed on drop.
    dir: Option<PathBuf>,
    child: Child,
}

impl FakeAwmanProcess {
    /// Start one, named `awman-fake-<label>`, and wait until it presents that
    /// name.
    pub fn spawn(label: &str) -> Self {
        let test_binary = std::env::current_exe().expect("test binary path");
        let (mut command, dir) = holder_command(&test_binary, &format!("awman-fake-{label}"));
        let mut child = command
            .args(["--exact", HOLDER_TEST, "--ignored", "--test-threads=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("fake awman-named process must start");
        let remove_dir = |dir: &Option<PathBuf>| {
            if let Some(dir) = dir {
                let _ = std::fs::remove_dir_all(dir);
            }
        };

        // `spawn` returns once the child *exists*, not once it has finished
        // `execve`. Until then its command name is the spawning test thread's,
        // which `pid_is_awman` rejects, and a check inside that window would
        // clear the pidfile as stale. Wait for the identity this fixture exists
        // to present; no test may observe it before it holds.
        let pid = child.id();
        for _ in 0..500 {
            if let Ok(Some(status)) = child.try_wait() {
                remove_dir(&dir);
                panic!("fake awman-named process (PID {pid}) exited before it was ready: {status}");
            }
            if DaemonSupervisor::pid_is_awman(pid) {
                return Self { dir, child };
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
        remove_dir(&dir);
        panic!("fake awman-named process (PID {pid}) never presented an awman command name");
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// How the process ended, if it already has — for a failure message.
    /// Reaps it, so only call this once the test has already failed.
    #[allow(dead_code)]
    pub fn exit_status(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }
}

impl Drop for FakeAwmanProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(dir) = &self.dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// macOS: the test binary itself, with `argv[0]` set to `name`.
#[cfg(target_os = "macos")]
fn holder_command(test_binary: &Path, name: &str) -> (Command, Option<PathBuf>) {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new(test_binary);
    command.arg0(name);
    (command, None)
}

/// Elsewhere: the test binary hard-linked as `name`, in a directory of its own
/// beside the test binary.
#[cfg(not(target_os = "macos"))]
fn holder_command(test_binary: &Path, name: &str) -> (Command, Option<PathBuf>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    // Beside the test binary rather than under TMPDIR: a hard link needs the
    // same filesystem, and a copy of a test binary is tens to hundreds of
    // megabytes.
    let dir = test_binary
        .parent()
        .expect("test binary directory")
        .join("awman-fake-daemons")
        .join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
    std::fs::create_dir_all(&dir).expect("fake daemon directory");
    let executable = dir.join(name);
    if std::fs::hard_link(test_binary, &executable).is_err() {
        std::fs::copy(test_binary, &executable).expect("copy test binary for fake daemon");
    }
    (Command::new(executable), Some(dir))
}
