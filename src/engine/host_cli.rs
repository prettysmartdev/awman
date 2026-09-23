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
