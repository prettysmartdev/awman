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
