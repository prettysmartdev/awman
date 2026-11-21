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
use crate::data::config::squad::SquadConfig;
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
