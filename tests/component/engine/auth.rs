//! `engine::auth` — the bounded host-tool call behind the keychain backends
//! (`security`, `secret-tool`), against real child processes. Children are
//! built by the production spawner (`engine::host_cli::command`) from the
//! sandbox snapshot and reaped by `run_capped` itself.

use crate::support;
use awman::engine::auth::keychain::{run_capped, KeychainCallError};
use awman::engine::host_cli;
use std::time::Duration;

/// Remediation of review-security F4 / review-adversarial F8: the cap covers
/// the stdin write. A child that never drains stdin must time out, not block.
#[cfg_attr(not(unix), ignore = "not-applicable: needs a POSIX shell")]
#[test]
fn run_capped_times_out_on_a_child_that_never_reads_its_stdin() {
    let sandbox = support::sandbox(
        "engine::auth::run_capped_times_out_on_a_child_that_never_reads_its_stdin",
    );
    let env = support::env_snapshot(&sandbox);
    // Far larger than any pipe buffer, so the write blocks on a non-reader.
    let payload = vec![b'x'; 512 * 1024];
    let mut cmd = host_cli::command(&env, "sh");
    // `exec` so the process the cap kills is the one holding the pipe.
    cmd.args(["-c", "exec sleep 30"]);
    let outcome = run_capped(&mut cmd, Some(&payload), Duration::from_millis(300));
    assert!(
        matches!(outcome, Err(KeychainCallError::Timeout)),
        "a child that never drains stdin must time out: {outcome:?}"
    );
    sandbox.finish();
}

/// The cap covers the child's output too: output larger than a pipe buffer
/// is drained while the child runs, so the call succeeds well within its cap.
#[cfg_attr(not(unix), ignore = "not-applicable: needs a POSIX shell")]
#[test]
fn run_capped_reads_back_a_payload_larger_than_a_pipe_buffer() {
    let sandbox =
        support::sandbox("engine::auth::run_capped_reads_back_a_payload_larger_than_a_pipe_buffer");
    let env = support::env_snapshot(&sandbox);
    let want = 256 * 1024;
    let mut cmd = host_cli::command(&env, "sh");
    cmd.args(["-c", &format!("head -c {want} /dev/zero")]);
    let out = run_capped(&mut cmd, None, Duration::from_secs(15))
        .expect("a child that writes more than a pipe buffer must not time out");
    assert!(out.status.success());
    assert_eq!(out.stdout.len(), want, "every byte must be read back");
    sandbox.finish();
}

/// A draining child receives a payload larger than a pipe buffer intact.
#[cfg_attr(not(unix), ignore = "not-applicable: needs a POSIX shell")]
#[test]
fn run_capped_feeds_a_draining_child_a_payload_larger_than_a_pipe_buffer() {
    let sandbox = support::sandbox(
        "engine::auth::run_capped_feeds_a_draining_child_a_payload_larger_than_a_pipe_buffer",
    );
    let env = support::env_snapshot(&sandbox);
    let payload = vec![b'x'; 256 * 1024];
    let mut cmd = host_cli::command(&env, "sh");
    cmd.args(["-c", "wc -c"]);
    let out = run_capped(&mut cmd, Some(&payload), Duration::from_secs(15))
        .expect("a draining child must succeed");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        payload.len().to_string()
    );
    sandbox.finish();
}

/// A missing backend binary is reported as missing, not as a failure.
#[test]
fn run_capped_reports_a_missing_backend_binary() {
    let sandbox = support::sandbox("engine::auth::run_capped_reports_a_missing_backend_binary");
    let env = support::env_snapshot(&sandbox);
    let mut cmd = host_cli::command(&env, "awman-no-such-keychain-tool");
    let outcome = run_capped(&mut cmd, None, Duration::from_secs(5));
    assert!(
        matches!(outcome, Err(KeychainCallError::BinaryMissing(_))),
        "{outcome:?}"
    );
    sandbox.finish();
}
