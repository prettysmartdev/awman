//! Observing the argument vectors of docker client processes.
//!
//! The container conformance suite asserts that host values reach the
//! container by name and never appear in a CLI argv. A fake CLI records its
//! argv itself; the real docker CLI cannot, so the real world watches the
//! process table with a supervised shell loop (`ps` pacing itself) and keeps
//! every distinct `docker …` command line it sees. A line the watcher
//! misses cannot cause a false failure; a value that appears in a line it
//! sees fails the check.

use awman_test_support::{OwnedChild, TestSandbox};
use std::cell::RefCell;

/// `ps` prints every process's full argument list; `awk` keeps the distinct
/// docker client lines. `ps -eww -o args=` works with procps and BSD `ps`.
const WATCH_SCRIPT: &str = "while :; do ps -eww -o args= || exit 1; done \
    | awk '/(^|\\/)docker /&&!seen[$0]++{print;fflush()}'";

pub struct ArgvWatcher {
    child: RefCell<Option<OwnedChild>>,
}

impl ArgvWatcher {
    /// Start watching on behalf of `sandbox`.
    pub fn start(sandbox: &TestSandbox) -> ArgvWatcher {
        let probe = sandbox
            .command("sh")
            .args(["-c", "command -v ps"])
            .label("ps probe")
            .run();
        if !probe.exit.success() {
            panic!(
                "BLOCKED[docker]: the real-engine conformance check needs `ps` to observe docker \
                 command lines (install procps)"
            );
        }
        let child = sandbox
            .command("sh")
            .args(["-c", WATCH_SCRIPT])
            .label("docker argv watcher")
            .max_output(8 << 20)
            .spawn();
        ArgvWatcher {
            child: RefCell::new(Some(child)),
        }
    }

    /// Every distinct docker command line seen so far, split on whitespace.
    pub fn seen(&self) -> Vec<Vec<String>> {
        match self.child.borrow().as_ref() {
            Some(child) => child
                .stdout_so_far()
                .lines()
                .map(|line| line.split_whitespace().map(str::to_string).collect())
                .collect(),
            None => Vec::new(),
        }
    }

    /// Stop the loop and reap its process group.
    pub fn stop(&self) {
        if let Some(mut child) = self.child.borrow_mut().take() {
            child.kill_and_reap();
        }
    }
}

impl Drop for ArgvWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}
