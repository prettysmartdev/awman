//! The OS-keychain backend for squad's daemon-env store (WI 0116 §5).
//!
//! Layer 0 owns the [`DaemonEnvStore`] seam, the [`NoStore`] fallback and the
//! pure helpers ([`crate::data::fs::daemon_env`]). The concrete keychain
//! implementation lives here because it needs the shell-out shims in
//! [`crate::engine::auth::keychain`], which are Layer 1 — Layer 0 may not
//! import them.
//!
//! One item, service `awman-squad`, account `daemon-env`, value a JSON map.
//! Reads go through the existing `go-keyring-base64:` envelope handling, so a
//! payload written here is unwrapped by the same code that already unwraps
//! zalando/go-keyring's items.
//!
//! Nothing in this module is authoritative and nothing in it is fatal: see the
//! three rules in [`crate::data::fs::daemon_env`].

use std::time::Duration;

use crate::data::config::env::DaemonEnvMap;
use crate::data::config::repo::SquadConfig;
use crate::data::error::DataError;
use crate::data::fs::daemon_env::{
    call_with_cap, encode_go_keyring_payload, DaemonEnvStore, EnvPersistenceSetting,
    FallbackReason, NoStore, DAEMON_ENV_ACCOUNT, DAEMON_ENV_SERVICE, KEYCHAIN_CALL_CAP,
};
use crate::engine::auth::keychain;

/// The default backend: one generic-password item in the OS keychain.
#[derive(Debug, Clone)]
pub struct KeychainStore {
    /// Hard cap on every call. Injectable so tests can drive a slow backend
    /// without a real keychain.
    cap: Duration,
}

impl KeychainStore {
    pub fn new() -> Self {
        Self {
            cap: KEYCHAIN_CALL_CAP,
        }
    }

    /// Same store with a different cap. Test seam for rule 1.
    pub fn with_cap(cap: Duration) -> Self {
        Self { cap }
    }

    /// Whether this platform has a keychain backend at all. Windows has none,
    /// which is an expected steady state rather than a fault (§5b).
    pub fn platform_supported() -> bool {
        cfg!(target_os = "macos") || cfg!(target_os = "linux")
    }

    /// Whether the item exists, without decoding it.
    ///
    /// `awman clean` needs presence, not readability: an item whose payload no
    /// longer parses is exactly the kind of leftover a user wants removed.
    pub fn item_present(&self) -> Result<bool, DataError> {
        keychain::keychain_lookup(DAEMON_ENV_SERVICE, DAEMON_ENV_ACCOUNT, self.cap)
            .map(|item| item.is_some())
            .map_err(|e| data_err("lookup", &e))
    }
}

impl Default for KeychainStore {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonEnvStore for KeychainStore {
    fn backend_name(&self) -> &'static str {
        "keychain"
    }

    fn store(&self, vars: &DaemonEnvMap) -> Result<(), DataError> {
        let json = serde_json::to_vec(vars).map_err(|e| {
            // `e` describes the shape, never the content, but the map is
            // stringly-typed so this cannot fail in practice.
            DataError::Other(format!("keychain store: cannot serialize payload: {e}"))
        })?;
        let envelope = encode_go_keyring_payload(&json);
        keychain::keychain_store(DAEMON_ENV_SERVICE, DAEMON_ENV_ACCOUNT, &envelope, self.cap)
            .map_err(|e| data_err("store", &e))
    }

    fn load(&self) -> Result<Option<DaemonEnvMap>, DataError> {
        let Some(raw) = keychain::keychain_lookup(DAEMON_ENV_SERVICE, DAEMON_ENV_ACCOUNT, self.cap)
            .map_err(|e| data_err("load", &e))?
        else {
            return Ok(None);
        };
        let decoded = keychain::decode_go_keyring_payload(&raw).ok_or_else(|| {
            DataError::Other("keychain load: stored item is not a decodable envelope".to_string())
        })?;
        let map: DaemonEnvMap = serde_json::from_slice(&decoded).map_err(|_| {
            // Deliberately not `{e}`: serde's message quotes the offending
            // input, which is the payload.
            DataError::Other("keychain load: stored item is not a valid env map".to_string())
        })?;
        Ok(Some(map))
    }

    fn clear(&self) -> Result<(), DataError> {
        keychain::keychain_clear(DAEMON_ENV_SERVICE, DAEMON_ENV_ACCOUNT, self.cap)
            .map_err(|e| data_err("clear", &e))
    }
}

/// A keychain error as a `DataError`, carrying the operation and the reason and
/// never any part of the value.
fn data_err(op: &str, err: &keychain::KeychainCallError) -> DataError {
    DataError::Other(format!("keychain {op}: {err}"))
}

/// What [`resolve`] decided, plus what its probe already read.
pub struct ResolvedEnvStore {
    /// The backend this daemon will use for its lifetime.
    pub store: Box<dyn DaemonEnvStore>,
    /// Why persistence is unavailable, when it is. `None` covers both a healthy
    /// keychain and an explicit `envPersistence: "none"` — a choice is never a
    /// fallback and is never warned about.
    pub fallback: Option<FallbackReason>,
    /// The payload the availability probe read back, when it ran and succeeded.
    ///
    /// The probe *is* a `load`: it decides availability by reading the item.
    /// Handing that answer to [`DaemonEnvState::load_from_store`] is what keeps
    /// daemon startup to **one** capped keychain call rather than two. It
    /// matters because both calls happen before the listener is bound, so on a
    /// keychain that answers slowly — a macOS login keychain responding in ~4.5s
    /// with the prompt suppressed — two of them could push the start past the
    /// supervisor's ten-second wait and make `ensure_running` report a daemon
    /// that was in fact seconds from being up.
    ///
    /// `None` means no usable answer (opt-out, timeout, error, or an
    /// unsupported platform); the caller then asks the store itself, which for
    /// every one of those cases is [`NoStore`] and answers instantly.
    ///
    /// [`DaemonEnvState::load_from_store`]: crate::engine::squad::env_state::DaemonEnvState::load_from_store
    pub probed: Option<DaemonEnvMap>,
}

impl ResolvedEnvStore {
    /// The "persist nothing" answer, with a reason when there is one to give.
    fn no_store(fallback: Option<FallbackReason>) -> Self {
        Self {
            store: Box::new(NoStore),
            fallback,
            probed: None,
        }
    }
}

/// Decide this daemon's backend. Runs **once at daemon startup**, before the
/// scheduler, under the same cap.
///
/// Probes rather than assumes: a `load` that returns a value, returns cleanly
/// empty, or reports "no such item" all mean the backend is available. A
/// missing binary, a timeout, a locked collection, or an unsupported platform
/// do not. The probe's answer is carried back in
/// [`ResolvedEnvStore::probed`] rather than discarded, so the caller does not
/// have to read the same item a second time.
pub fn resolve(config: &SquadConfig) -> ResolvedEnvStore {
    let setting = config.env_persistence_or_default();
    if setting == EnvPersistenceSetting::Keychain && !KeychainStore::platform_supported() {
        // No probe to run: there is no binary to probe with.
        return ResolvedEnvStore::no_store(Some(FallbackReason::UnsupportedPlatform));
    }
    resolve_with(setting, Box::new(KeychainStore::new()), KEYCHAIN_CALL_CAP)
}

/// [`resolve`] against an injected backend, so the decision table is testable
/// without a keychain.
pub fn resolve_with(
    setting: EnvPersistenceSetting,
    probe: Box<dyn DaemonEnvStore>,
    cap: Duration,
) -> ResolvedEnvStore {
    if setting == EnvPersistenceSetting::None {
        // Opting out removes what opting in stored, rather than merely stopping
        // future writes. Best-effort and silent: an explicit choice is never
        // second-guessed and never warned about, so there is no FallbackReason.
        let _ = call_with_cap(cap, move || probe.clear());
        return ResolvedEnvStore::no_store(None);
    }

    // The probe is handed to the helper thread and handed back with its answer,
    // so a backend that responded in time is the one we keep using. One that
    // did not is abandoned along with its thread (rule 1).
    match call_with_cap(cap, move || {
        let outcome = probe.load();
        (probe, outcome)
    }) {
        None => ResolvedEnvStore::no_store(Some(FallbackReason::Timeout)),
        // A value, or a clean "no such item" — both mean available. An item
        // that is simply not there is an empty map, not "no answer": the
        // difference is what stops a cold-start daemon re-reading the keychain
        // to be told the same nothing twice.
        Some((probe, Ok(loaded))) => ResolvedEnvStore {
            store: probe,
            fallback: None,
            probed: Some(loaded.unwrap_or_default()),
        },
        Some((_, Err(e))) => ResolvedEnvStore::no_store(Some(classify_probe_error(&e.to_string()))),
    }
}

/// Turn a probe `DataError` into the reason the user is told about.
fn classify_probe_error(message: &str) -> FallbackReason {
    if let Some(idx) = message.find(" not found") {
        // "keychain load: secret-tool not found" → "secret-tool".
        let head = &message[..idx];
        let bin = head.rsplit(' ').next().unwrap_or(head);
        if !bin.is_empty() {
            return FallbackReason::BinaryMissing(bin.to_string());
        }
    }
    if message.contains("timed out") {
        return FallbackReason::Timeout;
    }
    FallbackReason::ProbeFailed(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a scripted probe's `load()` should do. A plain enum (rather than a
    /// closure) so the probe stays `Send + Sync + 'static` without needing
    /// `DataError`/`DaemonEnvMap` to be `Clone` (they are not).
    enum ScriptedOutcome {
        /// `Ok(None)` — the backend answered and simply holds no item yet
        /// ("no such item"), which is available, not a fallback.
        NoItemYet,
        /// `Ok(Some(...))` — the backend answered with a value.
        HasValue(&'static str, &'static str),
        /// `Err(...)` shaped exactly like a missing-binary probe error, so
        /// `classify_probe_error` names the binary.
        BinaryMissing(&'static str),
        /// Sleeps past whatever cap the test gives `resolve_with`, then
        /// answers cleanly — standing in for a locked collection or an absent
        /// D-Bus that a real keychain would present as a hang, not an error.
        NeverAnswersInTime(Duration),
    }

    struct ScriptedProbe(ScriptedOutcome);

    impl DaemonEnvStore for ScriptedProbe {
        fn backend_name(&self) -> &'static str {
            "keychain"
        }
        fn store(&self, _vars: &DaemonEnvMap) -> Result<(), DataError> {
            Ok(())
        }
        fn load(&self) -> Result<Option<DaemonEnvMap>, DataError> {
            match &self.0 {
                ScriptedOutcome::NoItemYet => Ok(None),
                ScriptedOutcome::HasValue(k, v) => Ok(Some(DaemonEnvMap::from_pairs([(*k, *v)]))),
                ScriptedOutcome::BinaryMissing(bin) => {
                    Err(DataError::Other(format!("keychain load: {bin} not found")))
                }
                ScriptedOutcome::NeverAnswersInTime(delay) => {
                    std::thread::sleep(*delay);
                    Ok(None)
                }
            }
        }
        fn clear(&self) -> Result<(), DataError> {
            Ok(())
        }
    }

    /// An available backend is kept as the live store whether it already
    /// holds a value or cleanly reports "no such item" — both mean the
    /// backend works, so neither is a fallback.
    #[test]
    fn resolve_with_an_available_backend_is_kept_with_no_fallback_reason() {
        let resolved = resolve_with(
            EnvPersistenceSetting::Keychain,
            Box::new(ScriptedProbe(ScriptedOutcome::NoItemYet)),
            Duration::from_secs(1),
        );
        assert!(
            resolved.fallback.is_none(),
            "no such item yet is available, not a fallback"
        );
        assert_eq!(resolved.store.backend_name(), "keychain");

        let resolved = resolve_with(
            EnvPersistenceSetting::Keychain,
            Box::new(ScriptedProbe(ScriptedOutcome::HasValue("A", "v"))),
            Duration::from_secs(1),
        );
        assert!(resolved.fallback.is_none());
        assert_eq!(resolved.store.backend_name(), "keychain");
    }

    /// `envPersistence: "none"` never probes for a reason to report: an
    /// explicit opt-out is a choice, not a degradation.
    #[test]
    fn resolve_with_none_setting_never_reports_a_fallback_reason() {
        let resolved = resolve_with(
            EnvPersistenceSetting::None,
            Box::new(ScriptedProbe(ScriptedOutcome::HasValue("A", "v"))),
            Duration::from_secs(1),
        );
        assert!(resolved.fallback.is_none());
        assert_eq!(resolved.store.backend_name(), "none");
    }

    /// An absent backend (binary missing) falls back to `NoStore` **with** a
    /// `FallbackReason` naming it.
    #[test]
    fn resolve_with_a_missing_binary_falls_back_with_the_reason_named() {
        let resolved = resolve_with(
            EnvPersistenceSetting::Keychain,
            Box::new(ScriptedProbe(ScriptedOutcome::BinaryMissing("secret-tool"))),
            Duration::from_secs(1),
        );
        assert_eq!(resolved.store.backend_name(), "none");
        assert_eq!(
            resolved.fallback,
            Some(FallbackReason::BinaryMissing("secret-tool".to_string()))
        );
    }

    /// A `load` that exceeds the cap returns `None` rather than blocking, and
    /// `resolve_with` turns that into `NoStore` + `Timeout` — driven entirely
    /// by the injected slow backend above, no real keychain involved.
    #[test]
    fn resolve_with_a_slow_probe_times_out_without_blocking_past_the_cap() {
        let cap = Duration::from_millis(50);
        let started = std::time::Instant::now();
        let resolved = resolve_with(
            EnvPersistenceSetting::Keychain,
            Box::new(ScriptedProbe(ScriptedOutcome::NeverAnswersInTime(
                Duration::from_secs(2),
            ))),
            cap,
        );
        let elapsed = started.elapsed();
        assert_eq!(resolved.store.backend_name(), "none");
        assert_eq!(resolved.fallback, Some(FallbackReason::Timeout));
        assert!(
            elapsed < Duration::from_millis(500),
            "resolve_with must not block past the cap even though the backend keeps \
             running: {elapsed:?}"
        );
    }

    /// A config that omits `envPersistence` must resolve through the
    /// `Keychain` setting — asserted via the accessor `resolve` itself uses,
    /// not by trusting `#[derive(Default)]` to keep meaning that. Driven
    /// through `resolve_with` (not the real `resolve`) so this stays hermetic:
    /// `resolve` would otherwise shell out to a real keychain on this
    /// platform.
    #[test]
    fn a_config_omitting_env_persistence_resolves_through_the_keychain_setting() {
        let config = SquadConfig::default();
        assert_eq!(
            config.env_persistence, None,
            "envPersistence must be omitted by default"
        );
        let resolved = resolve_with(
            config.env_persistence_or_default(),
            Box::new(ScriptedProbe(ScriptedOutcome::NoItemYet)),
            Duration::from_secs(1),
        );
        assert!(resolved.fallback.is_none());
        assert_eq!(resolved.store.backend_name(), "keychain");
    }

    /// Remediation of review-adversarial F6.
    ///
    /// The probe decides availability *by reading the item*, so its answer is
    /// carried back rather than discarded. Both the probe and the load happen
    /// before the daemon binds its listener; two capped calls on a keychain
    /// that answers slowly could exceed the supervisor's ten-second wait, and
    /// `ensure_running` would then report a daemon that was seconds from up.
    #[test]
    fn an_available_probe_carries_back_what_it_read_so_the_load_costs_no_second_call() {
        let resolved = resolve_with(
            EnvPersistenceSetting::Keychain,
            Box::new(ScriptedProbe(ScriptedOutcome::HasValue("A", "v"))),
            Duration::from_secs(1),
        );
        assert_eq!(
            resolved.probed.as_ref().map(|map| map.names()),
            Some(vec!["A".to_string()]),
            "what the probe read must reach the caller, not be thrown away"
        );
        assert_eq!(
            resolved
                .probed
                .and_then(|map| map.get("A").map(String::from)),
            Some("v".to_string())
        );

        // "No item yet" is an *answer* — an empty map — not "no answer". The
        // difference is what stops a cold-start daemon re-reading the keychain
        // to be told the same nothing a second time.
        let resolved = resolve_with(
            EnvPersistenceSetting::Keychain,
            Box::new(ScriptedProbe(ScriptedOutcome::NoItemYet)),
            Duration::from_secs(1),
        );
        let probed = resolved
            .probed
            .expect("a clean 'no such item' is an answer");
        assert!(probed.is_empty());
    }

    /// The three ways a probe yields nothing usable. Each leaves `probed` at
    /// `None`, and each also leaves the backend at `NoStore`, so the caller's
    /// fallback read costs nothing.
    #[test]
    fn a_probe_with_no_usable_answer_carries_nothing_back() {
        for (label, setting, outcome) in [
            (
                "opt-out",
                EnvPersistenceSetting::None,
                ScriptedOutcome::HasValue("A", "v"),
            ),
            (
                "error",
                EnvPersistenceSetting::Keychain,
                ScriptedOutcome::BinaryMissing("secret-tool"),
            ),
            (
                "timeout",
                EnvPersistenceSetting::Keychain,
                ScriptedOutcome::NeverAnswersInTime(Duration::from_millis(300)),
            ),
        ] {
            let resolved = resolve_with(
                setting,
                Box::new(ScriptedProbe(outcome)),
                Duration::from_millis(50),
            );
            assert!(
                resolved.probed.is_none(),
                "{label}: nothing was read, so nothing may be carried back"
            );
            assert_eq!(
                resolved.store.backend_name(),
                "none",
                "{label}: and the fallback read the caller then makes is free"
            );
        }
    }
}
