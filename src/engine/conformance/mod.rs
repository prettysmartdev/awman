//! The shared runtime conformance suite (WI 0144 §3.6–3.7, design 6.2).
//!
//! Layer 1 has several exchangeable implementations of the same contract: a
//! container runtime reached through the Docker CLI (fake or real), a host
//! process runner, git over a transport (local file, smart HTTP, SSH), an OCI
//! registry (fake or real). Each suite here drives awman's own engine code
//! against one implementation and checks the behaviour awman relies on.
//!
//! The suites never decide how the world is arranged. A *world* (one trait
//! per suite) does that: it puts an image in the store, builds a remote,
//! answers questions about the state awman left behind. The `component`
//! tier implements the worlds with verified fakes (`tests/component/engine/
//! fake_world.rs`); the `boundary` tier implements them with the real
//! Docker Engine, registry and git transports. Because the assertions live
//! here, a fake that passes in `component` and a real implementation that
//! passes in `boundary` agree on everything the suite checks.
//!
//! Entry points (each returns a [`Report`]):
//! - [`container::container_runtime`] with a [`container::ContainerWorld`];
//! - [`process::host_command_runner`] with a [`process::ProcessWorld`];
//! - [`git::git_transport`] with a [`git::GitRemoteWorld`];
//! - [`registry::oci_registry`] with a [`registry::RegistryWorld`].
//!
//! The suites contain no test framework, no sleeping and no environment
//! reads: every process they start goes through the engine with the
//! world's `EnvSnapshot`, and every wait goes through [`Waiter`], which the
//! tier implements with its event-based `wait_for`.

pub mod container;
pub mod git;
pub mod process;
pub mod registry;

use std::time::Duration;

/// The result of running one suite against one implementation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The implementation under test, e.g. `docker CLI (fake)`.
    pub subject: String,
    /// Checks that passed, by name.
    pub passed: Vec<String>,
    /// Checks that failed: name and reason.
    pub failed: Vec<(String, String)>,
    /// Checks the world declared not applicable: name and reason.
    pub not_applicable: Vec<(String, String)>,
}

impl Report {
    /// An empty report for `subject`.
    pub fn new(subject: impl Into<String>) -> Report {
        Report {
            subject: subject.into(),
            ..Report::default()
        }
    }

    /// Record the outcome of the check `name`.
    pub fn check(&mut self, name: &str, result: Result<(), String>) {
        match result {
            Ok(()) => self.passed.push(name.to_string()),
            Err(reason) => self.failed.push((name.to_string(), reason)),
        }
    }

    /// Record that `name` does not apply to this implementation.
    pub fn skip(&mut self, name: &str, reason: &str) {
        self.not_applicable
            .push((name.to_string(), reason.to_string()));
    }

    /// Whether every check passed and at least one ran.
    pub fn is_success(&self) -> bool {
        self.failed.is_empty() && !self.passed.is_empty()
    }

    /// Every check name the report knows about, in suite order.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.passed.clone();
        names.extend(self.failed.iter().map(|(n, _)| n.clone()));
        names.extend(self.not_applicable.iter().map(|(n, _)| n.clone()));
        names.sort();
        names
    }

    /// Panic with every failed check when the implementation does not conform.
    pub fn assert_conforms(&self) {
        assert!(
            self.is_success(),
            "{} does not conform:\n{}\n(passed: {:?}; not applicable: {:?})",
            self.subject,
            self.failed
                .iter()
                .map(|(name, reason)| format!("  {name}: {reason}"))
                .collect::<Vec<_>>()
                .join("\n"),
            self.passed,
            self.not_applicable,
        );
    }
}

/// How a suite waits for something to become true. The tier implements it
/// with its event-based waiting helper (polling with diagnostics on expiry);
/// suites never sleep themselves.
pub trait Waiter {
    /// Return `true` as soon as `condition` holds, `false` when `limit`
    /// passes first.
    fn wait_until(&self, what: &str, limit: Duration, condition: &mut dyn FnMut() -> bool) -> bool;
}

/// `Ok` when `condition` holds, otherwise the reason.
pub(crate) fn ensure(condition: bool, reason: impl FnOnce() -> String) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(reason())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_without_checks_is_not_a_success() {
        assert!(!Report::new("nothing").is_success());
    }

    #[test]
    fn one_failure_makes_the_report_fail_and_is_listed() {
        let mut report = Report::new("subject");
        report.check("ok", Ok(()));
        report.check("bad", Err("because".into()));
        report.skip("other", "not here");
        assert!(!report.is_success());
        assert_eq!(
            report.failed,
            vec![("bad".to_string(), "because".to_string())]
        );
        assert_eq!(report.names(), vec!["bad", "ok", "other"]);
        let panic = std::panic::catch_unwind(|| report.assert_conforms()).unwrap_err();
        let text = panic.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(text.contains("bad: because"), "{text}");
    }

    #[test]
    fn skipped_checks_do_not_fail_a_report_with_passes() {
        let mut report = Report::new("subject");
        report.check("ok", Ok(()));
        report.skip("other", "not here");
        assert!(report.is_success());
        report.assert_conforms();
    }
}
