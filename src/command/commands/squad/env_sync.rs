//! The client half of the daemon-env protocol (WI 0116 §4a).
//!
//! Every squad command that opens a keyed gateway performs a coverage *check*;
//! almost none perform a *push*. The daemon reports a salted digest per name it
//! holds, this module digests the same names from this shell's environment, and
//! only a genuine difference puts a value on the wire.
//!
//! That distinction is the whole point. Re-sending every token on every
//! `awman squad list` would be needless secret movement, needless work, and
//! would make socket traffic proportional to how often someone looks at a list
//! rather than to how often anything changes. A push happens on exactly three
//! occasions: the first command after a daemon starts, a value actually
//! changing, and a new `env()` name entering `required_env`.
//!
//! The bootstrapping wrinkle — a client cannot know `required_env` before the
//! daemon answers — is resolved by reading it in the same exchange: the cold
//! call sends nothing, receives the required list, and sends once with the
//! intersection of that list and this shell's own environment. Nothing outside
//! `required_env` is ever transmitted.

use crate::command::commands::squad::gateway::{EnvCoverage, EnvPush, TaskGateway};
use crate::data::config::env::{host_var, DaemonEnvMap};
use crate::engine::squad::env_state::{coverage_digest, Salt};

/// What one sync did, in names only. Reported by `awman squad env`; never
/// rendered anywhere a value could join it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EnvSyncReport {
    /// Names the daemon accepted from this shell.
    pub pushed: Vec<String>,
    /// Names this shell was asked for and does not have.
    pub absent: Vec<String>,
    /// Names whose digest already matched — the steady-state case, where
    /// nothing crossed the socket.
    pub unchanged: Vec<String>,
}

/// Decide what to send, given the daemon's coverage and a lookup for this
/// process's own values. Pure: no I/O, no globals.
///
/// Per required name:
///
/// * this shell has it and the digests match → send nothing;
/// * this shell has it and they differ, or the daemon holds none → `vars`;
/// * this shell does not have it (unset, or set but empty) → `absent`.
///
/// `force` skips the digest comparison and sends every locally-present required
/// name, which is what `awman squad env --push` means.
///
/// A coverage whose salt does not parse is treated as "digest unavailable", so
/// every locally-present name is sent rather than silently skipped: a daemon
/// this client cannot check against is one it should re-arm.
pub fn plan_push(
    coverage: &EnvCoverage,
    lookup: &dyn Fn(&str) -> Option<String>,
    force: bool,
) -> EnvPush {
    let salt = Salt::from_hex(&coverage.salt);
    let mut vars = DaemonEnvMap::new();
    let mut absent: Vec<String> = Vec::new();

    for entry in &coverage.required {
        // Set *and non-empty*: the same rule the daemon applies, and the same
        // one `dedup_credentials_by_declared_env` already applies to a declared
        // `env()`. An empty value is not a provision.
        let Some(value) = lookup(&entry.name).filter(|value| !value.is_empty()) else {
            absent.push(entry.name.clone());
            continue;
        };
        let matches = !force
            && match (&salt, &entry.digest) {
                (Some(salt), Some(digest)) => &coverage_digest(salt, &entry.name, &value) == digest,
                _ => false,
            };
        if matches {
            continue;
        }
        vars.insert(entry.name.clone(), value);
    }

    absent.sort();
    EnvPush { vars, absent }
}

/// Check coverage and push only what actually differs.
///
/// Never returns an error: a daemon that cannot be reached, or that answers
/// something unparseable, must not fail the command the user actually ran. The
/// failure is logged at `debug!` and the report comes back empty — squad then
/// behaves exactly as it did before WI 0116.
pub async fn sync_env(gateway: &dyn TaskGateway, force: bool) -> EnvSyncReport {
    let coverage = match gateway.env_coverage().await {
        Ok(coverage) => coverage,
        Err(error) => {
            tracing::debug!(error = %error, "squad env: coverage check unavailable");
            return EnvSyncReport::default();
        }
    };
    let push = plan_push(&coverage, &host_var, force);
    let mut report = EnvSyncReport {
        pushed: Vec::new(),
        absent: push.absent.clone(),
        unchanged: coverage
            .required
            .iter()
            .map(|entry| entry.name.clone())
            .filter(|name| !push.vars.contains(name) && !push.absent.contains(name))
            .collect(),
    };
    // Steady state: nothing to send, so nothing is sent. The server treats an
    // empty `vars` as a no-op anyway; skipping the request keeps the common
    // path to a single round trip.
    if push.vars.is_empty() {
        return report;
    }
    match gateway.push_env(push).await {
        Ok(response) => report.pushed = response.accepted,
        Err(error) => tracing::debug!(error = %error, "squad env: push failed"),
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::Utc;

    use crate::engine::squad::env_state::RequiredEnvEntry;

    fn entry(name: &str, digest: Option<&str>) -> RequiredEnvEntry {
        RequiredEnvEntry {
            name: name.to_string(),
            required_by: vec!["nightly".into()],
            required_since: Utc::now(),
            last_provided_at: None,
            unmet_since: None,
            source: None,
            digest: digest.map(str::to_string),
        }
    }

    fn coverage(salt: &Salt, required: Vec<RequiredEnvEntry>) -> EnvCoverage {
        EnvCoverage {
            salt: salt.to_hex(),
            persistence: "keychain".into(),
            required,
        }
    }

    /// Invariant 15, and the reason §4a exists: when the daemon already holds
    /// what this shell holds, nothing crosses the socket.
    #[test]
    fn a_matching_digest_sends_nothing() {
        let salt = Salt::random();
        let digest = coverage_digest(&salt, "TOKEN", "v1");
        let coverage = coverage(&salt, vec![entry("TOKEN", Some(&digest))]);

        let push = plan_push(&coverage, &|_| Some("v1".to_string()), false);

        assert!(push.vars.is_empty(), "steady state is zero pushes");
        assert!(push.absent.is_empty());
    }

    /// A rotated token is the case a push exists for.
    #[test]
    fn a_different_value_is_sent() {
        let salt = Salt::random();
        let digest = coverage_digest(&salt, "TOKEN", "old");
        let coverage = coverage(&salt, vec![entry("TOKEN", Some(&digest))]);

        let push = plan_push(&coverage, &|_| Some("new".to_string()), false);

        assert_eq!(push.vars.get("TOKEN"), Some("new"));
        assert!(push.absent.is_empty());
    }

    /// The cold call: the daemon holds nothing, so everything this shell has
    /// goes, and everything it lacks is reported absent rather than omitted.
    #[test]
    fn a_daemon_holding_nothing_gets_what_this_shell_has() {
        let salt = Salt::random();
        let coverage = coverage(
            &salt,
            vec![entry("TOKEN", None), entry("AWS_PROFILE", None)],
        );

        let push = plan_push(
            &coverage,
            &|name| (name == "TOKEN").then(|| "v1".to_string()),
            false,
        );

        assert_eq!(push.vars.names(), vec!["TOKEN".to_string()]);
        assert_eq!(push.absent, vec!["AWS_PROFILE".to_string()]);
    }

    /// Invariant 2 on the client side: `plan_push` never emits an empty value,
    /// so an exported-but-empty variable reads as absent on both sides.
    #[test]
    fn an_empty_local_value_is_reported_absent_and_never_sent() {
        let salt = Salt::random();
        let coverage = coverage(&salt, vec![entry("TOKEN", None)]);

        let push = plan_push(&coverage, &|_| Some(String::new()), false);

        assert!(push.vars.is_empty());
        assert_eq!(push.absent, vec!["TOKEN".to_string()]);
    }

    /// `--push` is the explicit "send it anyway" escape hatch, so it must not
    /// consult the digest at all.
    #[test]
    fn force_sends_a_matching_value_anyway() {
        let salt = Salt::random();
        let digest = coverage_digest(&salt, "TOKEN", "v1");
        let coverage = coverage(&salt, vec![entry("TOKEN", Some(&digest))]);

        let push = plan_push(&coverage, &|_| Some("v1".to_string()), true);

        assert_eq!(push.vars.get("TOKEN"), Some("v1"));
    }

    /// Names outside `required_env` are never transmitted: the client only
    /// answers what it was asked.
    #[test]
    fn nothing_outside_required_env_is_transmitted() {
        let salt = Salt::random();
        let coverage = coverage(&salt, vec![entry("TOKEN", None)]);

        let push = plan_push(&coverage, &|_| Some("v".to_string()), false);

        assert_eq!(push.vars.names(), vec!["TOKEN".to_string()]);
    }

    /// A daemon whose salt cannot be parsed cannot be checked against, so the
    /// client re-arms it rather than assuming coverage.
    #[test]
    fn an_unparseable_salt_falls_back_to_sending() {
        let coverage = EnvCoverage {
            salt: "not-hex".into(),
            persistence: "none".into(),
            required: vec![entry("TOKEN", Some("0123456789abcdef"))],
        };

        let push = plan_push(&coverage, &|_| Some("v1".to_string()), false);

        assert_eq!(push.vars.get("TOKEN"), Some("v1"));
    }
}
