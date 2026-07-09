//! WI-0098 Finding A — end-to-end proof that the name-only `-e KEY` credential
//! transport keeps secret values out of `docker run`'s argument vector while
//! still delivering them, byte-for-byte, into the container.
//!
//! `docker.rs` builds credentials as the argv pair `-e KEY` (name only) and
//! sets `KEY=VALUE` on the docker-client child process via `Command::env`; the
//! docker CLI then resolves the name-only `-e KEY` from its own environment.
//! The *argv construction* is unit-tested in `src/engine/container/docker.rs`
//! (values never appear in the built argv, including `=`/newline cases). This
//! file closes the loop the unit tests cannot: it launches a real container and
//! asserts (a) the exact value arrives inside, and (b) on Linux the docker
//! client's `/proc/<pid>/cmdline` never contains the secret during launch.
//!
//! Every test is gated on `helpers::docker_available()` and has `docker` in its
//! name, so `make test-fast` skips it and `make test-full` runs it. It mirrors
//! the exact invocation form `docker.rs` produces rather than calling the
//! `pub(super)` `build_run_argv` (which the integration crate cannot reach).

use std::process::{Command, Stdio};

use crate::helpers::docker_available;

/// The smallest image with a POSIX shell that we can rely on pulling in CI.
const IMAGE: &str = "busybox:latest";

fn try_pull(image: &str) -> bool {
    Command::new("docker")
        .args(["pull", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run `docker run --rm -e KEY <image> sh -c 'printf %s "$KEY"'` exactly the way
/// `docker.rs` does — the value goes on the child's environment, never argv —
/// and return the raw bytes the container observed for `$KEY`.
fn value_seen_inside_container(key: &str, value: &str) -> Vec<u8> {
    let output = Command::new("docker")
        .args([
            "run",
            "--rm",
            // Name-only `-e KEY`: the exact form `build_run_argv` emits.
            "-e",
            key,
            IMAGE,
            "sh",
            "-c",
            // `printf %s` emits the value with no trailing newline, so the
            // container's view of the bytes is exact — even for `=`/newline.
            &format!("printf %s \"${key}\""),
        ])
        // The secret only ever lives on the client child's environment.
        .env(key, value)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run docker");
    assert!(
        output.status.success(),
        "docker run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn docker_credential_plain_value_arrives_intact_and_not_in_argv() {
    if !docker_available() || !try_pull(IMAGE) {
        eprintln!("skipping: docker/{IMAGE} unavailable");
        return;
    }
    let value = "sk-plain-credential-value-12345";
    let seen = value_seen_inside_container("ANTHROPIC_API_KEY", value);
    assert_eq!(
        String::from_utf8_lossy(&seen),
        value,
        "the container must observe the exact credential value"
    );
}

#[test]
fn docker_credential_value_with_equals_arrives_intact() {
    if !docker_available() || !try_pull(IMAGE) {
        eprintln!("skipping: docker/{IMAGE} unavailable");
        return;
    }
    // A value full of `=` would be ambiguous in the `-e KEY=VALUE` argv form;
    // the name-only transport carries it verbatim.
    let value = "aaa=bbb==ccc=";
    let seen = value_seen_inside_container("TOKEN", value);
    assert_eq!(
        String::from_utf8_lossy(&seen),
        value,
        "a value containing `=` must arrive byte-for-byte"
    );
}

#[test]
fn docker_credential_value_with_newline_arrives_intact() {
    if !docker_available() || !try_pull(IMAGE) {
        eprintln!("skipping: docker/{IMAGE} unavailable");
        return;
    }
    // Newlines can never survive an argv element; they must here.
    let value = "line1\nline2\nline3";
    let seen = value_seen_inside_container("MULTILINE_SECRET", value);
    assert_eq!(
        String::from_utf8_lossy(&seen),
        value,
        "a multi-line value must arrive byte-for-byte"
    );
}

/// The security guarantee itself: while the docker client runs, its own
/// `/proc/<pid>/cmdline` (what `ps` and other local users would scrape) must
/// contain the credential name but never its value. Linux-only; on other hosts
/// the `/proc` interface does not exist and this assertion is skipped.
#[cfg(target_os = "linux")]
#[test]
fn docker_credential_value_absent_from_proc_cmdline_during_launch() {
    if !docker_available() || !try_pull(IMAGE) {
        eprintln!("skipping: docker/{IMAGE} unavailable");
        return;
    }
    let key = "ANTHROPIC_API_KEY";
    let value = "sk-proc-scrape-target-9f8e7d6c5b4a";

    // A container that lingers long enough for us to inspect the launching
    // client's cmdline before it exits.
    let mut child = Command::new("docker")
        .args(["run", "--rm", "-e", key, IMAGE, "sleep", "3"])
        .env(key, value)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn docker");

    let pid = child.id();
    let cmdline_path = format!("/proc/{pid}/cmdline");

    // `/proc/<pid>/cmdline` is NUL-separated argv. Read it a few times while the
    // client is alive; the value must never appear in any read.
    let mut checked = false;
    for _ in 0..30 {
        match std::fs::read(&cmdline_path) {
            Ok(bytes) => {
                checked = true;
                let joined = String::from_utf8_lossy(&bytes);
                assert!(
                    !joined.contains(value),
                    "credential VALUE leaked into {cmdline_path}: {joined:?}"
                );
                // The name-only `-e KEY` form should be visible — confirms we
                // inspected the right (credential-carrying) invocation.
                assert!(
                    joined.contains(key),
                    "expected the name-only `-e {key}` in cmdline: {joined:?}"
                );
            }
            // Client already exited between spawn and read — nothing to assert.
            Err(_) => break,
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let _ = child.wait();
    assert!(
        checked,
        "never managed to read {cmdline_path} while the client was alive; \
         the /proc assertion did not run"
    );
}
