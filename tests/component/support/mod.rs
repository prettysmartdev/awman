//! Glue for the `component` tier that needs awman types or
//! `CARGO_BIN_EXE_*` (design 8.3). Owner: build-test-support-crate, except
//! `backend.rs` (slice-daemon-squad-worker).
//!
//! Domain-specific helpers live in `tests/component/<domain>/support.rs`.

// Used by the domain modules as the slices fill them in.
#![allow(dead_code)]

pub mod backend;

use awman::data::config::env::EnvSnapshot;
use awman_test_support::{OwnedCommand, TestCase, TestSandbox};
use std::collections::BTreeMap;
use std::path::Path;

/// The debug `awman` built with this test binary.
pub fn awman_exe() -> &'static Path {
    Path::new(awman_test_support::cargo_bin!("awman"))
}

/// The fake CLI (`awman-fake-cli`), installed under other names with
/// `sandbox.install_fake(fake_cli_exe(), "docker")`.
pub fn fake_cli_exe() -> &'static Path {
    Path::new(awman_test_support::cargo_bin!("awman-fake-cli"))
}

/// A component-tier sandbox for the test `id` (its module path, e.g.
/// `"engine::git::clone_over_ssh"`), with `awman` and the fake CLI registered.
pub fn sandbox(id: &'static str) -> TestSandbox {
    // Match the binary's immutable native provider setup for in-process tests.
    #[cfg(windows)]
    awman_host_shims::install_metadata_publisher();
    let mut sandbox = TestSandbox::for_test(TestCase::component(id));
    sandbox.register_exe("awman", awman_exe());
    sandbox.register_exe("fake-cli", fake_cli_exe());
    sandbox
}

/// `awman` as a supervised child of `sandbox`.
pub fn awman(sandbox: &TestSandbox) -> OwnedCommand {
    sandbox.command(awman_exe()).label("awman")
}

/// The sandbox environment as the library's `EnvSnapshot` (cwd = the
/// sandbox work directory), for in-process engines and commands.
pub fn env_snapshot(sandbox: &TestSandbox) -> EnvSnapshot {
    let env: BTreeMap<String, String> = sandbox
        .env()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    EnvSnapshot::from_full_env(env).with_invocation(sandbox.work(), vec!["awman".to_string()])
}
