//! Soak: boundary cases that need the public network, credentials, or time.
//! They run in the nightly gate only; none is part of the PR gate.
//!
//! Modules map to manifest suites: `public_registry` → `soak-public-registry`,
//! `private_registry` → `soak-private-registry`, `latency` → `soak-latency`,
//! `fuzz` → `soak-fuzz`.

mod fuzz;
mod latency;
mod private_registry;
mod public_registry;

use awman::data::config::env::EnvSnapshot;
use awman_test_support::TestSandbox;

/// The sandbox environment plus `extra` variables, as the library's
/// `EnvSnapshot` (never the process environment).
fn snapshot_with(sandbox: &TestSandbox, extra: &[(&str, String)]) -> EnvSnapshot {
    let mut env: std::collections::BTreeMap<String, String> = sandbox
        .env()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    for (name, value) in extra {
        env.insert((*name).to_string(), value.clone());
    }
    EnvSnapshot::from_full_env(env).with_invocation(sandbox.work(), vec!["awman".to_string()])
}
