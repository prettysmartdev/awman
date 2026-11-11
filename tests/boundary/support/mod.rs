//! Glue for the `boundary` tier (design 8.3). Owner: build-test-support-crate.
//!
//! Real resources come from capabilities, never from defaults:
//!
//! ```ignore
//! let mut sandbox = support::sandbox("docker::build_uses_the_private_engine");
//! let docker = sandbox.require::<Docker>(); // BLOCKED unless the run names `docker=<host>`
//! sandbox.grant(&docker);                    // DOCKER_HOST, AWMAN_TEST_DOCKER=1, CLI on PATH
//! let out = support::awman(&sandbox).args(["…"]).run();
//! ```

// Used by the domain modules as the slices fill them in.
#![allow(dead_code)]

use awman::data::config::env::EnvSnapshot;
use awman_test_support::{OwnedCommand, TestCase, TestSandbox};
use std::collections::BTreeMap;
use std::path::Path;

/// The debug `awman` built with this test binary.
pub fn awman_exe() -> &'static Path {
    Path::new(awman_test_support::cargo_bin!("awman"))
}

/// The fake CLI, for the fake side of a conformance pair run here.
pub fn fake_cli_exe() -> &'static Path {
    Path::new(awman_test_support::cargo_bin!("awman-fake-cli"))
}

fn with_exes(mut sandbox: TestSandbox) -> TestSandbox {
    sandbox.register_exe("awman", awman_exe());
    sandbox.register_exe("fake-cli", fake_cli_exe());
    sandbox
}

/// A boundary-tier sandbox for the test `id` (its module path).
pub fn sandbox(id: &'static str) -> TestSandbox {
    with_exes(TestSandbox::for_test(TestCase::boundary(id)))
}

/// A soak-tier sandbox, for tests under `soak::` in this binary.
pub fn soak_sandbox(id: &'static str) -> TestSandbox {
    with_exes(TestSandbox::for_test(TestCase::soak(id)))
}

/// `awman` as a supervised child of `sandbox`.
pub fn awman(sandbox: &TestSandbox) -> OwnedCommand {
    sandbox.command(awman_exe()).label("awman")
}

/// The sandbox environment as the library's `EnvSnapshot`.
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
