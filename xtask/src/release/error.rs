//! One table of stop codes, their class and the default recovery text.
//!
//! Every phase reports at most one primary error:
//!
//! ```text
//! release: BLOCKED [linux-host-unreachable] ssh build@kvm1 timed out after 8 s
//!   recovery: start build@kvm1, then run make release
//! ```
//!
//! Exit codes: 0 done, 1 FAILED (a check ran and found a defect), 2 BLOCKED
//! (prerequisite or infrastructure), 130 interrupted. `Killed` exists only for
//! the fault-injection seam: it stops the driver exactly where a SIGKILL
//! would, without any further state write, and exits 137.

use std::fmt;
use std::process::ExitCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Failed,
    Blocked,
    Interrupted,
    Killed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stop {
    pub kind: Kind,
    pub code: String,
    pub message: String,
    pub remedy: String,
}

/// `(code, kind, default recovery)`. A test asserts every code used in the
/// release module is listed here and every entry has a recovery.
pub const CODES: &[(&str, Kind, &str)] = &[
    ("usage", Kind::Blocked, "see `cargo xtask release --help`"),
    ("tool-missing", Kind::Blocked, "install the named tool, then run make release"),
    ("gh-auth", Kind::Blocked, "run `gh auth login` with a token that can write this repository, then make release"),
    ("wrong-repo", Kind::Blocked, "run from a clone whose origin is the canonical repository"),
    ("dirty-checkout", Kind::Blocked, "commit or stash source changes, then run make release"),
    ("wrong-branch", Kind::Blocked, "check out main or release/<version> with the release prep committed, then run make release"),
    ("version-mismatch", Kind::Blocked, "make VERSION, Cargo.toml and Cargo.lock agree, commit, then run make release"),
    ("notes-missing", Kind::Blocked, "write docs/releases/<version>.md without the placeholder, commit, then run make release"),
    ("version-taken", Kind::Blocked, "choose a new version; an existing tag or release is never reused"),
    ("base-not-contained", Kind::Blocked, "rebase the release branch on origin/main, then run make release"),
    ("publication-policy-denied", Kind::Blocked, "ask a repository admin for the named capability, then run make release"),
    ("publication-policy-unknown", Kind::Blocked, "give this gh login read access to the repository's branch and tag rules (or fix the named ruleset), then run make release"),
    ("workflow-missing", Kind::Blocked, "complete the one-time workflow rollout on the default branch, then run make release"),
    ("workflow-closure", Kind::Blocked, "grant the named scope in the calling workflow, commit, then run make release"),
    ("manifest-stale", Kind::Blocked, "run `cargo xtask gates render` and `cargo xtask gates check`, commit, then run make release"),
    ("pin-pending", Kind::Blocked, "publish the fixture with the Fixtures workflow and `cargo xtask fixtures pin`, then run make release"),
    ("pin-unreachable", Kind::Blocked, "restore access to the pinned input URL, then run make release"),
    ("no-sbx-cli", Kind::Blocked, "install sbx, then run make release"),
    ("sbx-cli-incompatible", Kind::Blocked, "install sbx >= 0.53.0, then run make release"),
    ("sbx-identity-missing", Kind::Blocked, "provision the isolated SBX HOME (0700), identity label and provider key file (0600), then run make release"),
    ("sbx-not-signed-in", Kind::Blocked, "sign the dedicated SBX identity in, then run make release"),
    ("sbx-identity-not-isolated", Kind::Blocked, "provide a HOME-isolated SBX sign-in, then run make release"),
    ("sbx-network", Kind::Blocked, "restore access to docker.io and api.anthropic.com:443, then run make release"),
    ("mac-host", Kind::Blocked, "fix the named Mac prerequisite, then run make release"),
    ("linux-host-missing", Kind::Blocked, "run make release LINUX_HOST=user@host"),
    ("linux-host-unreachable", Kind::Blocked, "start or reconnect the Linux KVM host, then run make release"),
    ("linux-host-prerequisite", Kind::Blocked, "install the named prerequisite on the Linux host, then run make release"),
    ("attempt-locked", Kind::Blocked, "wait for the named owner to finish, or stop it and run make release"),
    ("state-foreign", Kind::Blocked, "resume from the owning clone, or run make release TAKEOVER=<owner> after stopping the other owner"),
    ("state-corrupt", Kind::Blocked, "inspect the named state file; restore it or run make release-abandon"),
    ("supersede-refused", Kind::Blocked, "a sealed or tagged release cannot change; choose a new version"),
    ("workflow-rejected", Kind::Blocked, "fix the workflow definition named in the run, commit, run make release SUPERSEDE=1"),
    ("dispatch-uncertain", Kind::Blocked, "run make release; the pending dispatch is reconciled, never duplicated blindly"),
    ("network", Kind::Blocked, "check connectivity to GitHub, then run make release"),
    ("timeout", Kind::Blocked, "run make release; completed work is kept"),
    ("ci-failed", Kind::Failed, "fix the failing test, commit, then run make release SUPERSEDE=1"),
    ("ci-evidence-invalid", Kind::Failed, "inspect the named CI run; evidence that cannot be verified is never reused"),
    ("ci-unavailable", Kind::Blocked, "run make release once GitHub Actions is reachable"),
    ("build-failed", Kind::Failed, "fix the build, commit, then run make release SUPERSEDE=1"),
    ("build-infrastructure", Kind::Blocked, "run make release; the failed stage is retried on the retained bytes"),
    ("build-evidence-invalid", Kind::Failed, "inspect the named release.yml run; incomplete evidence never counts as passed"),
    ("attestation-invalid", Kind::Failed, "the bytes are not attested by release.yml at the release commit; do not publish them"),
    ("retained-bytes-missing", Kind::Blocked, "run make release SUPERSEDE=1 to rebuild the affected asset in a new unsealed attempt"),
    ("hosted-evidence-missing", Kind::Blocked, "restore the named evidence file from a backup, or run make release SUPERSEDE=1 to revalidate in a new unsealed attempt"),
    ("kit-invalid", Kind::Failed, "inspect the named kit; fix the kit producer and run make release SUPERSEDE=1"),
    ("native-failed", Kind::Failed, "fix the failing native test, commit, then run make release SUPERSEDE=1"),
    ("native-blocked", Kind::Blocked, "fix the named host capability, then run make release"),
    ("native-result-invalid", Kind::Failed, "inspect the named result; incomplete native evidence never counts as passed"),
    ("evidence-expired", Kind::Blocked, "run make release SUPERSEDE=1 to revalidate in a new unsealed attempt"),
    ("draft-foreign", Kind::Blocked, "remove or rename the foreign draft yourself after checking it, then run make release"),
    ("upload-conflict", Kind::Blocked, "inspect the draft; a completed foreign or wrong-digest asset is never replaced"),
    ("seal-mismatch", Kind::Failed, "the sealed release changed on GitHub; do not publish it, choose a new version"),
    ("not-confirmed", Kind::Blocked, "run make release and answer y, or make release YES=1"),
    ("land-conflict", Kind::Blocked, "merge the release commit into main without rewriting it, push, then run make release"),
    ("policy-changed", Kind::Blocked, "run make release SUPERSEDE=1 to revalidate under the changed policy (or a new version once sealed)"),
    ("tag-foreign", Kind::Failed, "the version tag exists with another commit or seal; choose a new version"),
    ("publish-failed", Kind::Blocked, "run make release; the tag is kept and publication is retried"),
    ("verify-mismatch", Kind::Failed, "public bytes differ from the sealed bytes: withdraw the release and publish a new version"),
    ("verify-unavailable", Kind::Blocked, "run make release once the public download works"),
    ("rehearsal-refused", Kind::Blocked, "rehearsal attempts never land, tag or publish; run make release without REHEARSE"),
    ("interrupted", Kind::Interrupted, "run make release to resume"),
    ("killed", Kind::Killed, "run make release to resume"),
];

pub fn lookup(code: &str) -> Option<(Kind, &'static str)> {
    CODES
        .iter()
        .find(|(c, _, _)| *c == code)
        .map(|(_, k, r)| (*k, *r))
}

impl Stop {
    /// A stop with the table's class and default recovery. Unknown codes are
    /// a programming error and are reported as BLOCKED `state-corrupt`.
    pub fn new(code: &str, message: impl Into<String>) -> Stop {
        let message = message.into();
        match lookup(code) {
            Some((kind, remedy)) => Stop {
                kind,
                code: code.into(),
                message,
                remedy: remedy.into(),
            },
            None => Stop {
                kind: Kind::Blocked,
                code: "state-corrupt".into(),
                message: format!("unknown stop code {code}: {message}"),
                remedy: "report this release tool defect".into(),
            },
        }
    }

    pub fn with_remedy(mut self, remedy: impl Into<String>) -> Stop {
        self.remedy = remedy.into();
        self
    }

    pub fn exit_code(&self) -> u8 {
        match self.kind {
            Kind::Failed => 1,
            Kind::Blocked => 2,
            Kind::Interrupted => 130,
            Kind::Killed => 137,
        }
    }

    pub fn label(&self) -> &'static str {
        match self.kind {
            Kind::Failed => "FAILED",
            Kind::Blocked => "BLOCKED",
            Kind::Interrupted => "INTERRUPTED",
            Kind::Killed => "KILLED",
        }
    }

    /// Print the one primary line and its one recovery line to stderr.
    pub fn report(&self) -> ExitCode {
        eprintln!("{self}");
        ExitCode::from(self.exit_code())
    }
}

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let first = self.message.lines().next().unwrap_or("");
        write!(
            f,
            "release: {} [{}] {}\n  recovery: {}",
            self.label(),
            self.code,
            first,
            self.remedy
        )
    }
}

/// Internal errors (I/O, parse) become BLOCKED `state-corrupt` unless a
/// caller classifies them better.
impl From<anyhow::Error> for Stop {
    fn from(e: anyhow::Error) -> Stop {
        if let Some(stop) = e.downcast_ref::<Stop>() {
            return stop.clone();
        }
        Stop::new("state-corrupt", format!("{e:#}"))
    }
}

impl std::error::Error for Stop {}

pub type Step<T = ()> = std::result::Result<T, Stop>;

/// `stop!("code", "fmt", args…)` → `Err(Stop)`.
macro_rules! stop {
    ($code:expr, $($arg:tt)*) => {
        return Err($crate::release::error::Stop::new($code, format!($($arg)*)))
    };
}
pub(crate) use stop;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_a_class_and_a_recovery_and_reports_one_line_each() {
        let mut seen = std::collections::BTreeSet::new();
        for (code, _, remedy) in CODES {
            assert!(seen.insert(*code), "duplicate {code}");
            assert!(!remedy.trim().is_empty(), "{code}");
        }
        let s = Stop::new(
            "linux-host-unreachable",
            "ssh build@kvm1 timed out after 8 s\nmore",
        );
        let text = s.to_string();
        assert_eq!(text.lines().count(), 2);
        assert!(text.starts_with("release: BLOCKED [linux-host-unreachable] ssh build@kvm1"));
        assert_eq!(s.exit_code(), 2);
        assert_eq!(Stop::new("ci-failed", "x").exit_code(), 1);
        assert_eq!(Stop::new("no-such-code", "x").code, "state-corrupt");
    }

    #[test]
    fn every_code_used_in_the_release_module_is_in_the_table() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/release");
        let mut files = vec![];
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    files.push(p);
                }
            }
        }
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            for marker in ["Stop::new(\"", "stop!(\""] {
                for part in text.split(marker).skip(1) {
                    let code = part.split('"').next().unwrap();
                    if code == "no-such-code" || code == "code" {
                        continue;
                    }
                    assert!(lookup(code).is_some(), "{}: {code}", file.display());
                }
            }
        }
    }
}
