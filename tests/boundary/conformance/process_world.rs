//! The host process conformance world backed by the real `sh`: awman's
//! process runner driving real child processes (exit codes, both streams,
//! cancellation, deadlines, a descendant holding the pipes open).

use super::waiter::PollWaiter;
use awman::data::config::env::EnvSnapshot;
use awman::engine::conformance::process::{ProcessCase, ProcessWorld};
use awman::engine::conformance::Waiter;
use awman_test_support::TestSandbox;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

pub struct ShellProcessWorld<'a> {
    sandbox: &'a TestSandbox,
    env: EnvSnapshot,
    dir: PathBuf,
}

impl<'a> ShellProcessWorld<'a> {
    pub fn new(sandbox: &'a TestSandbox, env: EnvSnapshot) -> ShellProcessWorld<'a> {
        let dir = sandbox.root().join("process-world");
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("BLOCKED[tempdir]: {}: {e}", dir.display()));
        ShellProcessWorld { sandbox, env, dir }
    }

    fn pidfile(&self, case: ProcessCase) -> PathBuf {
        self.dir.join(format!("{case:?}.pid"))
    }

    fn pid(&self, case: ProcessCase) -> Option<String> {
        let text = std::fs::read_to_string(self.pidfile(case)).ok()?;
        let pid = text.trim();
        (!pid.is_empty()).then(|| pid.to_string())
    }

    fn signal(&self, pid: &str, signal: &str) -> bool {
        self.sandbox
            .command("sh")
            .args([
                "-c",
                &format!("kill {signal} \"$1\" 2>/dev/null"),
                "sh",
                pid,
            ])
            .label("kill")
            .run()
            .exit
            .success()
    }
}

impl Waiter for ShellProcessWorld<'_> {
    fn wait_until(&self, what: &str, limit: Duration, condition: &mut dyn FnMut() -> bool) -> bool {
        PollWaiter.wait_until(what, limit, condition)
    }
}

impl ProcessWorld for ShellProcessWorld<'_> {
    fn subject(&self) -> String {
        "host processes (/bin/sh)".to_string()
    }

    fn command(&self, case: ProcessCase) -> Command {
        let pidfile = self.pidfile(case);
        let _ = std::fs::remove_file(&pidfile);
        let script = match case {
            ProcessCase::Streams => "echo out-1; echo err-1 >&2; echo out-2; exit 7",
            ProcessCase::Hang => "echo $$ > \"$1\"; exec sleep 3600",
            ProcessCase::LeavesDescendant => {
                "echo parent-done; sleep 30 & echo $! > \"$1\"; exit 0"
            }
        };
        let mut command = awman::engine::host_cli::command(&self.env, "sh");
        command.args(["-c", script, "sh"]).arg(pidfile);
        command
    }

    fn missing_program(&self) -> Command {
        awman::engine::host_cli::command(&self.env, "awman-boundary-no-such-program")
    }

    fn is_running(&self, case: ProcessCase) -> bool {
        match self.pid(case) {
            Some(pid) => self.signal(&pid, "-0"),
            None => false,
        }
    }

    fn cleanup(&self) {
        for case in [ProcessCase::Hang, ProcessCase::LeavesDescendant] {
            if let Some(pid) = self.pid(case) {
                self.signal(&pid, "-9");
            }
        }
    }
}
