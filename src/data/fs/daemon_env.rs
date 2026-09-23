//! Persistence seam for the squad daemon's payload environment (WI 0116 §5).
//!
//! The squad daemon receives the values named by `env(VAR)` overlays over its
//! authenticated loopback socket and holds them in memory
//! ([`set_daemon_overlay`](crate::data::config::env::set_daemon_overlay)). That
//! leaves one gap: the OS restarting the daemon with no client present —
//! launchd `RunAtLoad` at the next login, a systemd restart after a crash. The
//! daemon comes back with an empty overlay and stays that way until some CLI or
//! TUI invocation happens to push, and scheduled tasks in that window run
//! without their tokens.
//!
//! [`DaemonEnvStore`] closes that gap. It is the *persistence* layer, not a
//! transport: a push is still the only way a value reaches a running daemon,
//! and the store only carries values across an OS-initiated restart.
//!
//! This module is Layer 0: it owns the trait, the [`NoStore`] fallback, the
//! configuration and reporting types, and the pure helpers. The `KeychainStore`
//! implementation lives in `engine::squad::env_store` because the OS keychain
//! shell-out shims are Layer 1 (`engine::auth::keychain`) and Layer 0 may not
//! import them.
//!
//! # Three non-negotiable rules
//!
//! An unattended daemon must not be able to hang, and must not lose data to a
//! store it cannot reach. Every implementation and every caller obeys:
//!
//! 1. **Never block.** Every keychain call runs under [`KEYCHAIN_CALL_CAP`]
//!    (see [`call_with_cap`]). A locked login keychain, an absent Secret
//!    Service, or any prompt that does appear is a timeout that logs a warning
//!    and continues with whatever the daemon already has. It is never an error
//!    that fails a start or a run.
//! 2. **Never authoritative.** A pushed value always replaces a stored one. The
//!    store is a cold-start hint, not a source of truth.
//! 3. **Never fatal.** A store or load failure degrades that daemon to
//!    [`NoStore`] for its lifetime. Squad keeps working exactly as it does with
//!    persistence off; the only thing lost is surviving an OS-initiated
//!    restart.

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::data::config::env::DaemonEnvMap;
use crate::data::error::DataError;

/// Keychain service name for the single stored item.
pub const DAEMON_ENV_SERVICE: &str = "awman-squad";

/// Keychain account name for the single stored item.
pub const DAEMON_ENV_ACCOUNT: &str = "daemon-env";

/// Hard cap on every keychain call (rule 1: never block).
pub const KEYCHAIN_CALL_CAP: Duration = Duration::from_secs(5);

/// The envelope zalando/go-keyring writes every macOS secret with, and the one
/// `engine::auth::keychain::decode_go_keyring_payload` already unwraps.
pub const GO_KEYRING_B64_PREFIX: &str = "go-keyring-base64:";

/// Where the daemon's payload environment is persisted across an OS-initiated
/// restart.
///
/// Implementations are best-effort by construction: every method returns a
/// `Result` the caller is expected to degrade on rather than propagate (rule 3).
pub trait DaemonEnvStore: Send + Sync {
    /// `"keychain"` or `"none"` — what [`EnvPersistence`] reports for a healthy
    /// backend of this kind.
    fn backend_name(&self) -> &'static str;

    /// Replace the stored payload wholesale. A name that has left the daemon's
    /// required set is therefore garbage-collected by the next write.
    fn store(&self, vars: &DaemonEnvMap) -> Result<(), DataError>;

    /// Read the stored payload. `Ok(None)` means "the backend works and holds
    /// no item" — the normal cold-start state, and explicitly not an error.
    fn load(&self) -> Result<Option<DaemonEnvMap>, DataError>;

    /// Remove the stored item. Idempotent: clearing what is not there is
    /// `Ok(())`.
    fn clear(&self) -> Result<(), DataError>;
}

/// The fallback backend: nothing is persisted anywhere.
///
/// Selected when the platform cannot provide a keychain, when a probe fails,
/// after a mid-session failure degrades a daemon (rule 3), or when the user
/// opts out with `squad.envPersistence: "none"`. Push-only behaviour is exactly
/// what squad did before WI 0116.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoStore;

impl DaemonEnvStore for NoStore {
    fn backend_name(&self) -> &'static str {
        "none"
    }

    fn store(&self, _vars: &DaemonEnvMap) -> Result<(), DataError> {
        Ok(())
    }

    fn load(&self) -> Result<Option<DaemonEnvMap>, DataError> {
        Ok(None)
    }

    fn clear(&self) -> Result<(), DataError> {
        Ok(())
    }
}

/// Why a daemon fell back to [`NoStore`] despite `envPersistence: "keychain"`.
///
/// An explicit `"none"` never produces one: an opt-out is a choice, not a
/// degradation, and is never warned about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackReason {
    /// No keychain backend exists on this platform (Windows).
    UnsupportedPlatform,
    /// The backend's binary (`security`, `secret-tool`) is not installed.
    BinaryMissing(String),
    /// The probe did not answer within the cap — a locked collection, an
    /// absent D-Bus, or a prompt nobody is there to dismiss.
    Timeout,
    /// The probe answered with an error.
    ProbeFailed(String),
}

impl std::fmt::Display for FallbackReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FallbackReason::UnsupportedPlatform => {
                write!(f, "no keychain backend on this platform")
            }
            FallbackReason::BinaryMissing(bin) => write!(f, "{bin} not found"),
            FallbackReason::Timeout => write!(f, "keychain probe timed out after 5s"),
            FallbackReason::ProbeFailed(msg) => write!(f, "keychain probe failed: {msg}"),
        }
    }
}

impl FallbackReason {
    /// The one `warn!` line a degraded daemon emits, at startup only.
    ///
    /// Warned **once per daemon lifetime, not per write**: a daemon writes on
    /// every env push, so warning per attempt would bury the very log
    /// `awman squad logs` prints and a failed start tells users to read.
    pub fn startup_warning(&self) -> String {
        format!(
            "squad env persistence unavailable ({self}); continuing without it. \
             Task env() values will be supplied by the next awman command and will \
             not survive a daemon restart. Set squad.envPersistence to \"none\" to \
             silence this."
        )
    }
}

/// The persistence state a daemon reports on `GET /v1/status`, so
/// `awman squad status` and the TUI tab can show it without anyone reading a
/// log file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum EnvPersistence {
    /// Storing to the OS keychain.
    Keychain,
    /// Not storing: either the explicit opt-out or a degraded daemon that
    /// started that way by configuration.
    #[default]
    None,
    /// Configured for the keychain, but unavailable — the string is a
    /// [`FallbackReason`] rendered.
    Unavailable(String),
}

impl std::fmt::Display for EnvPersistence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvPersistence::Keychain => write!(f, "keychain"),
            EnvPersistence::None => write!(f, "none"),
            EnvPersistence::Unavailable(reason) => write!(f, "unavailable({reason})"),
        }
    }
}

/// Whether the daemon holds a value for a required environment variable.
///
/// A two-valued fact that rode the wire as a `String` and was matched with
/// `as_str()` in the CLI renderer (WI 0114 F-48). Every required name is
/// treated alike: a name is in `required_env` only because a task's `env()`
/// overlay, the daemon's config, or `AWMAN_OVERLAYS` asked for it, so there is
/// no third, exempt state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvVarState {
    /// The daemon holds a value.
    Set,
    /// It does not.
    Unmet,
}

impl std::fmt::Display for EnvVarState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            EnvVarState::Set => "set",
            EnvVarState::Unmet => "unmet",
        })
    }
}

impl std::str::FromStr for EnvPersistence {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "keychain" => Ok(EnvPersistence::Keychain),
            "none" => Ok(EnvPersistence::None),
            other => other
                .strip_prefix("unavailable(")
                .and_then(|rest| rest.strip_suffix(')'))
                .map(|reason| EnvPersistence::Unavailable(reason.to_string()))
                .ok_or(()),
        }
    }
}

impl Serialize for EnvPersistence {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for EnvPersistence {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse()
            .map_err(|_| serde::de::Error::custom(format!("unknown env persistence state: {raw}")))
    }
}

/// The `squad.envPersistence` setting.
///
/// Defaults to [`Keychain`](EnvPersistenceSetting::Keychain): a squad daemon
/// exists to run unattended, so surviving an OS-initiated restart is the normal
/// case rather than an advanced one. `"none"` restores the pre-WI-0116
/// behaviour — nothing persisted anywhere — and, being an explicit choice, is
/// never probed for, never warned about, and clears any item a previous
/// keychain run stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvPersistenceSetting {
    #[default]
    Keychain,
    None,
}

/// Wrap a payload in the `go-keyring-base64:` envelope.
///
/// Not cosmetic. The payload is a JSON map, so it always contains `"` and may
/// contain spaces and backslashes, and `security -i` parses its stdin as
/// command lines. Base64's alphabet passes that parser untouched, which removes
/// the escaping question by construction rather than by careful quoting — and
/// the read side already unwraps this exact envelope via
/// `decode_go_keyring_payload`, which zalando/go-keyring writes on macOS
/// anyway.
pub fn encode_go_keyring_payload(raw: &[u8]) -> String {
    use base64::Engine;
    format!(
        "{GO_KEYRING_B64_PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    )
}

/// Build the single line handed to `security -i` on stdin.
///
/// `security -i` reads *commands* from stdin, so the secret never enters any
/// process's argv — `ps` sees only `security -i`. This matters: the alternative,
/// `security add-generic-password … -w <value>`, would put the value in a
/// world-readable `/proc/<pid>/cmdline`, and piping the value to it instead was
/// measured to fail silently (it opens `/dev/tty` when there is one, and under
/// launchd demands the value twice, hits EOF, reports `passwords don't match`
/// and still exits 0 with nothing stored — see `tools/probe-keychain-stdin.sh`).
///
/// `envelope` must already be [`encode_go_keyring_payload`]'d, so it carries no
/// quote, space or backslash for the interactive parser to trip over.
///
/// `service` and `account` must be the two module constants. They stay
/// parameters so the round-trip test can name them explicitly, but a runtime
/// string here would reintroduce exactly the interactive-parser injection the
/// envelope exists to prevent — and `tools/architecture-lint.sh` could not
/// catch it, because the offending string would still be inside this file.
///
/// This is the **only** place `add-generic-password` may appear in `src/`.
pub fn security_add_generic_password_script(
    service: &str,
    account: &str,
    envelope: &str,
) -> String {
    debug_assert!(
        service == DAEMON_ENV_SERVICE && account == DAEMON_ENV_ACCOUNT,
        "security -i takes an interactive command line: only the fixed \
         service/account constants may be interpolated into it"
    );
    format!("add-generic-password -U -s {service} -a {account} -w {envelope}\n")
}

/// Run `f` on a helper thread and give up on it after `cap` (rule 1: never
/// block).
///
/// Returns `None` when `f` did not finish in time. The thread is deliberately
/// **detached** rather than joined: the whole point is that a hung keychain
/// call — a locked collection, a Secret Service that never answers — cannot
/// hold up a daemon start, a push, or a run. Callers that spawn a child process
/// pair this with a kill inside `f` so nothing is left running either.
pub fn call_with_cap<T: Send + 'static>(
    cap: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // The receiver may already be gone (we timed out); dropping the value
        // is the correct outcome and is not an error worth reporting.
        let _ = tx.send(f());
    });
    rx.recv_timeout(cap).ok()
}

/// Every `env(NAME)` named by a list of raw overlay specs, de-duplicated and in
/// first-seen order. Specs of any other shape are ignored.
///
/// Mirrors `parse_overlay_list`'s tolerance: the argument is any single
/// non-empty, comma-free string, trimmed.
pub fn env_overlay_names(specs: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for spec in specs {
        let trimmed = spec.trim();
        let Some(inner) = trimmed
            .strip_prefix("env(")
            .and_then(|rest| rest.strip_suffix(')'))
        else {
            continue;
        };
        let name = inner.trim();
        if name.is_empty() || name.contains(',') {
            continue;
        }
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fallback backend does nothing and says so — `store`/`clear` are
    /// `Ok(())`, `load` is cleanly `Ok(None)`, never an error.
    #[test]
    fn no_store_round_trips_as_none() {
        let store = NoStore;
        let vars = DaemonEnvMap::from_pairs([("TOKEN", "v1")]);
        assert!(store.store(&vars).is_ok());
        assert_eq!(store.load().unwrap(), None);
        assert!(store.clear().is_ok());
        assert_eq!(store.backend_name(), "none");
    }

    /// The rule 1 seam itself: a backend that answers in time is returned
    /// promptly.
    #[test]
    fn call_with_cap_returns_the_value_when_the_backend_answers_in_time() {
        let result = call_with_cap(Duration::from_secs(1), || 42);
        assert_eq!(result, Some(42));
    }

    /// A backend that never answers must not be able to hold up its caller:
    /// `call_with_cap` gives up at the cap and returns `None`, well short of
    /// how long the injected slow backend actually takes — no real keychain
    /// involved, just a closure that outlives the cap.
    #[test]
    fn call_with_cap_returns_none_without_blocking_past_the_cap() {
        let cap = Duration::from_millis(50);
        let started = std::time::Instant::now();
        let result = call_with_cap(cap, || {
            std::thread::sleep(Duration::from_secs(2));
            "never observed"
        });
        let elapsed = started.elapsed();
        assert_eq!(
            result, None,
            "a load exceeding the timeout must return None"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "the caller must not block past the cap even though the backend keeps \
             running in its detached thread: {elapsed:?}"
        );
    }
}
