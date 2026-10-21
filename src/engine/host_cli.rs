//! The container and sandbox CLIs awman drives — `docker`, Apple's
//! `container`, and `sbx` — resolved through test isolation.
//!
//! Every spawn of one of them passes its program name through [`program`].
//! Outside test isolation that is the name itself. Under it, the real CLI is
//! off unless its opt-in variable is set (`AWMAN_TEST_DOCKER`,
//! `AWMAN_TEST_APPLE_CONTAINER`, `AWMAN_TEST_SBX`): the name resolves to a path
//! that does not exist, so awman sees exactly what it sees when the CLI is not
//! installed. The container tests would otherwise build, run and remove
//! images, containers and sandboxes in the developer's own daemon.
//!
//! A fake a test put on `PATH` inside its own temp directory is not the
//! developer's CLI, so it is always allowed.

use std::path::Path;

use crate::data::config::env::{test_isolation_active, Env, EnvSnapshot};

/// The program to spawn for the CLI named `name`. See the module docs.
///
/// Names other than the three CLIs pass through unchanged.
pub fn program(name: &str) -> &str {
    let (opted_in, disabled): (fn(&EnvSnapshot) -> bool, &'static str) = match name {
        "docker" => (
            EnvSnapshot::test_docker,
            "/nonexistent/awman-test-isolation/docker",
        ),
        "container" => (
            EnvSnapshot::test_apple_container,
            "/nonexistent/awman-test-isolation/container",
        ),
        "sbx" => (
            EnvSnapshot::test_sbx,
            "/nonexistent/awman-test-isolation/sbx",
        ),
        _ => return name,
    };
    if !test_isolation_active() || opted_in(&Env::from_process()) || resolves_to_test_fake(name) {
        name
    } else {
        disabled
    }
}

/// Whether `name` on `PATH` is a file inside the temp directory, i.e. a fake
/// a test wrote for itself.
fn resolves_to_test_fake(name: &str) -> bool {
    let Ok(found) = which::which(name) else {
        return false;
    };
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    canonical(&found).starts_with(canonical(&std::env::temp_dir()))
}

// Runtime CLI calls in this scope poll the cancellation token and reap their process group.
thread_local! {
    static COLLECTION_CANCEL: std::cell::RefCell<Option<tokio_util::sync::CancellationToken>> = const { std::cell::RefCell::new(None) };
}

pub fn with_collection_cancel<T>(
    cancel: tokio_util::sync::CancellationToken,
    f: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            COLLECTION_CANCEL.with(|c| *c.borrow_mut() = None);
        }
    }
    COLLECTION_CANCEL.with(|c| *c.borrow_mut() = Some(cancel));
    let _reset = Reset;
    f()
}

pub fn check_collection_cancel() -> Result<(), crate::engine::error::EngineError> {
    if COLLECTION_CANCEL.with(|c| c.borrow().as_ref().is_some_and(|c| c.is_cancelled())) {
        Err(crate::engine::error::EngineError::Other(
            "collection cancelled".into(),
        ))
    } else {
        Ok(())
    }
}

/// Bounded output capture without pipe-reader threads that descendants can hold open.
/// Cancellation kills and reaps the local CLI process group, never the agent.
pub(crate) fn checked_output(
    command: &mut std::process::Command,
    binary: &str,
) -> Result<std::process::Output, crate::engine::error::EngineError> {
    use crate::engine::error::EngineError;
    use std::io::{Read, Seek};
    check_collection_cancel()?;
    let mut stdout = tempfile::tempfile().map_err(|e| EngineError::Container(e.to_string()))?;
    let mut stderr = tempfile::tempfile().map_err(|e| EngineError::Container(e.to_string()))?;
    command
        .stdin(std::process::Stdio::null())
        .stdout(
            stdout
                .try_clone()
                .map_err(|e| EngineError::Container(e.to_string()))?,
        )
        .stderr(
            stderr
                .try_clone()
                .map_err(|e| EngineError::Container(e.to_string()))?,
        );
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EngineError::ContainerRuntimeUnavailable {
                binary: binary.into(),
            }
        } else {
            EngineError::Container(format!("{binary}: {e}"))
        }
    })?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        let error = check_collection_cancel().err().or_else(|| {
            (std::time::Instant::now() >= deadline)
                .then(|| EngineError::Container(format!("{binary} timed out")))
        });
        if let Some(error) = error {
            #[cfg(unix)]
            {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(child.id() as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(EngineError::Container(e.to_string()));
            }
        }
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout
        .rewind()
        .and_then(|_| stdout.read_to_end(&mut out))
        .map_err(|e| EngineError::Container(e.to_string()))?;
    stderr
        .rewind()
        .and_then(|_| stderr.read_to_end(&mut err))
        .map_err(|e| EngineError::Container(e.to_string()))?;
    if !status.success() {
        return Err(EngineError::Container(format!(
            "{binary}: {}",
            String::from_utf8_lossy(&err).trim()
        )));
    }
    Ok(std::process::Output {
        status,
        stdout: out,
        stderr: err,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn other_programs_pass_through() {
        assert_eq!(program("git"), "git");
        assert_eq!(program("fake-kit"), "fake-kit");
    }

    /// Unit tests are always isolated. Unless the opt-in is set (it is not in
    /// `make test`) or a test's fake is on `PATH`, the real CLI is replaced
    /// with a path that does not exist.
    #[test]
    fn real_container_clis_are_off_under_isolation() {
        for (name, opted_in) in [
            ("docker", Env::from_process().test_docker()),
            ("container", Env::from_process().test_apple_container()),
            ("sbx", Env::from_process().test_sbx()),
        ] {
            let resolved = program(name);
            if opted_in || resolves_to_test_fake(name) {
                assert_eq!(resolved, name);
            } else {
                assert!(
                    resolved.starts_with("/nonexistent/"),
                    "{name} must be off under test isolation, got {resolved}"
                );
                assert!(!Path::new(resolved).exists());
            }
        }
    }
}
