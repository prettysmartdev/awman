//! Component-tier tests of awman-test-support itself (suite
//! `component-test-support`): real processes, loopback sockets, local git.
//! Every test declares its tier explicitly through `TestCase`.

mod canary;
mod capability;
mod evidence;
mod example;
mod fake_cli;
mod fakes;
mod fixtures;
mod ports;
mod process;
mod pty;
mod security;

use awman_test_support::{TestCase, TestSandbox};
use std::path::Path;

/// The fake CLI built for this package.
pub fn fake_cli_exe() -> &'static Path {
    Path::new(awman_test_support::cargo_bin!(
        "awman-test-support-fake-cli"
    ))
}

/// A component sandbox with the fake CLI registered.
pub fn sandbox(id: &'static str) -> TestSandbox {
    let mut sandbox = TestSandbox::for_test(TestCase::component(id));
    sandbox.register_exe("fake-cli", fake_cli_exe());
    sandbox
}

/// The text of a caught panic.
pub fn panic_text(err: &Box<dyn std::any::Any + Send>) -> String {
    err.downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default()
}

/// Run `f`, expect a panic, return its text.
pub fn expect_panic(f: impl FnOnce()) -> String {
    let err =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).expect_err("expected a panic");
    panic_text(&err)
}

/// A component sandbox under a throwaway parent, for tests that make the
/// sandbox fail on purpose (a failed sandbox is kept for evidence; the
/// parent removes it).
pub fn leaky_sandbox(id: &'static str) -> (tempfile::TempDir, TestSandbox) {
    let parent = tempfile::tempdir().expect("parent dir");
    let mut sandbox = TestSandbox::for_test_in(TestCase::component(id), Some(parent.path()));
    sandbox.register_exe("fake-cli", fake_cli_exe());
    (parent, sandbox)
}
