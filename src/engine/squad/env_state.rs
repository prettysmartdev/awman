//! The daemon's payload-environment state (WI 0116 §4).
//!
//! The values themselves live in Layer 0's process-wide overlay
//! ([`daemon_overlay_snapshot`] / [`set_daemon_overlay`]). What lives here is
//! everything *about* them: which names the daemon requires, which task asked
//! for each, when each was last provided, how long each has been unmet, and the
//! per-lifetime salt that lets a client check coverage without either side
//! putting a secret on the wire.
//!
//! # Why a digest and not a push
//!
//! Every squad command performs a coverage *check*; almost none perform a
//! *push*. The daemon reports, per required name, `sha256(salt ‖ name ‖ value)`
//! truncated to 16 hex characters. The client digests its own value the same
//! way: equal means send nothing. Steady state is therefore zero pushes and zero
//! secrets on the wire.
//!
//! What the salt does and does not buy, stated exactly, because it is easy to
//! overclaim. It **does** make digests incomparable across daemons and across
//! machines, and — because it is rotated per daemon lifetime — it stops a
//! restarted daemon having stale digests trusted against it. It does **not**
//! make the digest safe to expose to the party that receives it: the coverage
//! response carries the salt alongside the digests, so for that caller the
//! digest is an unstretched, unrated-limited commitment to the value. Against a
//! high-entropy token that is inert; against a low-entropy or structured value
//! (an account id, an environment label, a short password) it is an offline
//! confirmation oracle. The mitigation is the one that actually applies: the
//! response only ever reaches an already-authenticated caller, and a caller who
//! has the bearer key can overwrite the values anyway.
//!
//! # Names, never values
//!
//! Nothing in this module returns, logs, or formats a payload value.
//! [`RequiredEnvEntry`] carries a name, a digest and timestamps; the push
//! response carries names; `DaemonEnvMap`'s `Debug` prints names. That is what
//! makes a compromised bearer key unable to read secrets back out of a daemon —
//! it could only overwrite them, which it could do anyway.
//!
//! [`daemon_overlay_snapshot`]: crate::data::config::env::daemon_overlay_snapshot
//! [`set_daemon_overlay`]: crate::data::config::env::set_daemon_overlay

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::data::config::env::{daemon_overlay_snapshot, update_daemon_overlay, DaemonEnvMap};
use crate::data::error::DataError;
use crate::data::fs::daemon_env::{
    call_with_cap, env_overlay_names, DaemonEnvStore, EnvPersistence, NoStore, KEYCHAIN_CALL_CAP,
};
use crate::data::fs::task_store::Task;

/// How many hex characters of the coverage hash are published.
///
/// Eight bytes is far more than enough to tell "the same value" from "a
/// different value" while leaving nothing useful to an attacker who already
/// cannot guess the 32-byte salt.
const DIGEST_HEX_LEN: usize = 16;

/// The per-daemon-lifetime coverage salt: 32 random bytes.
///
/// Generated at startup, held in memory for the daemon's lifetime, and handed
/// out with every coverage response. There is no `rand` dependency in this
/// tree, so the bytes come from two v4 UUIDs — 122 bits of entropy each, from
/// the same generator the rest of awman trusts for identifiers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Salt([u8; 32]);

impl Salt {
    /// A fresh salt for one daemon lifetime.
    pub fn random() -> Self {
        let mut bytes = [0u8; 32];
        bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self(bytes)
    }

    /// Parse the 64-lowercase-hex form a coverage response carries. `None` for
    /// anything that is not exactly 32 bytes of hex.
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        Some(Self(bytes))
    }

    /// 64 lowercase hex characters.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// A salt is a secret of its own kind: never print it by accident.
impl std::fmt::Debug for Salt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Salt(<redacted>)")
    }
}

/// `sha256(salt ‖ len(name) ‖ name ‖ value)`, first 8 bytes, 16 lowercase hex
/// characters.
///
/// The name is length-prefixed (four big-endian bytes) so the concatenation is
/// unambiguous: without it `("AB", "C")` and `("A", "BC")` hash identically.
/// Nothing exploits that today — every comparison on both sides is per-name
/// with the name fixed, so only the value varies — but this function is the
/// interop contract between client and daemon, and an ambiguous encoding in
/// such a place is a latent bug rather than a saved instruction.
///
/// Both sides call this exact function — the client through
/// [`plan_push`](crate::command::commands::squad::env_sync::plan_push) — so the
/// rule cannot drift between them. A client and daemon that disagreed would
/// merely push a value the daemon already holds, never fail.
pub fn coverage_digest(salt: &Salt, name: &str, value: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update((name.len() as u32).to_be_bytes());
    hasher.update(name.as_bytes());
    hasher.update(value.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..DIGEST_HEX_LEN]
        .to_string()
}

/// Where the value the daemon currently holds for a name came from.
///
/// The distinction is what `awman squad env` reports, and it is the only thing
/// that says whether restarting the daemon will lose the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvSource {
    /// A client pushed it over the socket.
    Pushed,
    /// It came back from the OS keychain at startup (§5).
    Keychain,
}

/// One name in the daemon's `required_env`, as reported to a client.
///
/// Carries a *digest*, never a value. Everything else is metadata a client
/// needs to decide whether to push and a user needs to understand what is
/// missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredEnvEntry {
    pub name: String,
    /// Task names that declare this variable, sorted. Every task's name for one
    /// that comes from the daemon's own config or `AWMAN_OVERLAYS`, since those
    /// apply to every run. Never empty: a name is in `required_env` only
    /// because something declared it.
    pub required_by: Vec<String>,
    /// When the name first entered `required_env` in this daemon's lifetime.
    pub required_since: DateTime<Utc>,
    /// When a push last supplied a value for it.
    pub last_provided_at: Option<DateTime<Utc>>,
    /// Since when the daemon has had no usable value.
    ///
    /// Stamped when a name first becomes *required* while uncovered — never
    /// when a push happens to omit it — so a typo'd `env(GTIHUB_TOKEN)` reads
    /// as long-unmet rather than newly-unmet. `None` when covered.
    pub unmet_since: Option<DateTime<Utc>>,
    /// Where the held value came from. `None` when the daemon holds none.
    pub source: Option<EnvSource>,
    /// `sha256(salt ‖ name ‖ value)` truncated to 16 hex characters. `None`
    /// when the daemon holds no value.
    pub digest: Option<String>,
}

/// The stored half of an entry: what survives across refreshes. `source` and
/// `digest` are derived from the live overlay every time [`DaemonEnvState::entries`]
/// is called, so they can never go stale.
#[derive(Debug, Clone)]
struct StoredEntry {
    required_by: Vec<String>,
    required_since: DateTime<Utc>,
    last_provided_at: Option<DateTime<Utc>>,
    unmet_since: Option<DateTime<Utc>>,
}

struct Inner {
    salt: Salt,
    /// Shared, not owned exclusively: every keychain call is handed to a
    /// detached helper thread ([`call_with_cap`]), which needs a `'static`
    /// handle. An `Arc` is what makes that sound without the state having to
    /// hold its own mutex across a five-second call.
    store: std::sync::Arc<dyn DaemonEnvStore>,
    /// The backend `store` held before [`DaemonEnvState::degrade`] swapped in
    /// [`NoStore`], kept for one purpose: `clear_store` must still be able to
    /// remove an item this daemon already wrote. A degraded daemon is precisely
    /// the case where a stored item exists and nothing will ever rewrite it, so
    /// answering "nothing persisted" to `squad env --clear` there would leave a
    /// real item in the keychain behind a message saying there is none.
    ///
    /// Never used for a write: `persist` checks `degraded` first, and rule 3 is
    /// that a degraded daemon never retries.
    store_before_degrade: Option<std::sync::Arc<dyn DaemonEnvStore>>,
    persistence: EnvPersistence,
    entries: BTreeMap<String, StoredEntry>,
    /// Where each currently-held value came from. Keyed by name, kept
    /// independently of `entries` so a name that leaves and re-enters
    /// `required_env` does not lose the fact that its value came from the
    /// keychain.
    sources: BTreeMap<String, EnvSource>,
    /// Names already warned about at run start, so a task evaluating every five
    /// minutes produces one `warn!` per name and not one per tick.
    warned: BTreeSet<String>,
    /// Whether the one store-failure `warn!` has been emitted.
    store_warned: bool,
    /// Rule 3: a store failure degrades this daemon for its whole lifetime.
    degraded: bool,
}

/// Everything the daemon knows about its payload environment except the values.
///
/// Shared behind an `Arc` by the local gateway (which answers the coverage and
/// push routes) and the scheduler (which snapshots unmet names onto each run
/// row). One `Mutex` guards the lot: every operation is a handful of map
/// lookups, and the keychain call each push makes is issued *outside* the lock
/// so a slow backend cannot block a status request.
pub struct DaemonEnvState {
    inner: Mutex<Inner>,
}

impl DaemonEnvState {
    pub fn new(store: Box<dyn DaemonEnvStore>, persistence: EnvPersistence, salt: Salt) -> Self {
        Self {
            inner: Mutex::new(Inner {
                salt,
                store: store.into(),
                store_before_degrade: None,
                persistence,
                entries: BTreeMap::new(),
                sources: BTreeMap::new(),
                warned: BTreeSet::new(),
                store_warned: false,
                degraded: false,
            }),
        }
    }

    /// A state with no persistence at all, for callers that need a gateway
    /// without a daemon behind it (tests, and the non-daemon `DaemonStatus`
    /// literals).
    pub fn without_store() -> Self {
        Self::new(Box::new(NoStore), EnvPersistence::None, Salt::random())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What `GET /v1/status` reports: `keychain`, `none`, or
    /// `unavailable(<reason>)`.
    pub fn persistence(&self) -> EnvPersistence {
        self.lock().persistence.clone()
    }

    /// This daemon's coverage salt.
    pub fn salt(&self) -> Salt {
        self.lock().salt
    }

    /// Merge whatever the store holds into the overlay, once, at startup.
    ///
    /// Best-effort and capped (rule 1). `Some(map)` on success — an empty map
    /// when the backend simply holds no item — and `None` when the load failed
    /// or timed out, which also degrades this daemon for its lifetime (rule 3).
    ///
    /// Loaded values never overwrite something already in the overlay: a push
    /// is always authoritative over a stored value (rule 2).
    ///
    /// `probed` is the payload the availability probe in
    /// [`env_store::resolve`](crate::engine::squad::env_store::resolve) already
    /// read back. When it is `Some`, **no keychain call is made here at all**:
    /// the probe decides availability *by reading the item*, so asking again
    /// would be a second capped call for an answer already in hand — and both
    /// happen before the daemon binds its listener, where a slow keychain can
    /// cost the supervisor's whole ten-second wait. `None` means the probe had
    /// no usable answer, in which case the backend is `NoStore` and this
    /// returns instantly anyway.
    pub fn load_from_store(&self, probed: Option<DaemonEnvMap>) -> Option<DaemonEnvMap> {
        let loaded = match probed {
            Some(loaded) => {
                // Rule 3 still applies: a daemon already degraded holds nothing
                // it is willing to trust from a store.
                if self.lock().degraded {
                    return None;
                }
                loaded
            }
            None => {
                let store = {
                    let guard = self.lock();
                    if guard.degraded {
                        return None;
                    }
                    std::sync::Arc::clone(&guard.store)
                };
                // The call runs outside the lock: a keychain that takes the full
                // five seconds must not block a concurrent status request.
                match call_with_cap(KEYCHAIN_CALL_CAP, move || store.load()) {
                    Some(Ok(loaded)) => loaded.unwrap_or_default(),
                    Some(Err(error)) => {
                        self.degrade("load", &error.to_string());
                        return None;
                    }
                    None => {
                        self.degrade("load", "timed out");
                        return None;
                    }
                }
            }
        };
        if loaded.is_empty() {
            return Some(loaded);
        }

        // Merge under the overlay's own write lock so a push racing this
        // bootstrap read cannot be clobbered by a whole-map replace, and record
        // which names actually landed so `sources` matches the overlay exactly.
        let mut merged: Vec<String> = Vec::new();
        update_daemon_overlay(|overlay| {
            for (name, value) in loaded.iter() {
                if value.is_empty() || overlay.contains_key(name) {
                    continue;
                }
                overlay.insert(name.clone(), value.clone());
                merged.push(name.clone());
            }
        });
        if !merged.is_empty() {
            let mut guard = self.lock();
            for name in merged {
                guard.sources.insert(name, EnvSource::Keychain);
            }
        }
        Some(loaded)
    }

    /// Replace the required set.
    ///
    /// `required` maps each name to the task names that declare it, and is the
    /// whole of the required set: every name is there because a task's
    /// `env(NAME)` overlay, the daemon's own config, or `AWMAN_OVERLAYS` asked
    /// for it. No name is seeded on the daemon's own behalf, so no name is
    /// exempt from the unmet reporting the rest of §6 is built on.
    ///
    /// A name that has *left* the set is the one unambiguous signal that
    /// nothing needs its value any more, so it is removed from the overlay
    /// here — and only here — and the store is rewritten to garbage-collect it.
    /// An `absent` report never removes anything.
    pub fn set_required(&self, required: BTreeMap<String, Vec<String>>, now: DateTime<Utc>) {
        let mut wanted: BTreeMap<String, Vec<String>> = required;
        for names in wanted.values_mut() {
            names.sort();
            names.dedup();
        }

        let wanted_names: BTreeSet<String> = wanted.keys().cloned().collect();
        let overlay = daemon_overlay_snapshot();
        let mut removed: Vec<String> = Vec::new();
        {
            let mut guard = self.lock();
            guard.entries.retain(|name, _| {
                if wanted.contains_key(name) {
                    true
                } else {
                    removed.push(name.clone());
                    false
                }
            });
            for (name, required_by) in wanted {
                let covered = is_set_and_non_empty(&overlay, &name);
                match guard.entries.get_mut(&name) {
                    Some(entry) => {
                        entry.required_by = required_by;
                        // Invariant 11: a covered name has no unmet clock; an
                        // uncovered one keeps the clock it already started.
                        if covered {
                            entry.unmet_since = None;
                        } else if entry.unmet_since.is_none() {
                            entry.unmet_since = Some(now);
                        }
                    }
                    None => {
                        guard.entries.insert(
                            name,
                            StoredEntry {
                                required_by,
                                required_since: now,
                                last_provided_at: None,
                                unmet_since: (!covered).then_some(now),
                            },
                        );
                    }
                }
            }
            for name in &removed {
                guard.sources.remove(name);
                guard.warned.remove(name);
            }
        }

        // Garbage-collect the overlay against the *required set*, not against
        // the names that just left `entries`. A value loaded from the keychain
        // for a name no task declares any more never enters `entries` at all,
        // so it would never appear in `removed` — it would sit in the overlay
        // and be written back by every subsequent `persist`, forever, with no
        // surface that shows it exists.
        //
        // The retain runs under the overlay's write lock so a push landing
        // concurrently is either kept (its name is required) or dropped
        // deliberately, never lost to a stale whole-map replace.
        let mut dropped: Vec<String> = Vec::new();
        let overlay = update_daemon_overlay(|overlay| {
            overlay.retain(|name, _| {
                if wanted_names.contains(name) {
                    true
                } else {
                    dropped.push(name.clone());
                    false
                }
            });
        });
        if dropped.is_empty() {
            return;
        }
        tracing::debug!(
            names = ?dropped,
            "squad env: dropping values for names that left required_env"
        );
        self.persist(&overlay);
    }

    /// Apply one push as a **per-name merge**, returning `(accepted, ignored)`.
    ///
    /// The three states, and why a whole-map replace would be wrong:
    ///
    /// * **provided** — replaces whatever the daemon holds, so a rotated token
    ///   propagates on the next command with no restart;
    /// * **absent** — leaves any existing value untouched. "I don't have it" is
    ///   not "nobody should have it"; a client run from the wrong terminal must
    ///   not be able to disarm every scheduled task;
    /// * **unmentioned** — outside `required_env`, so it was never asked for and
    ///   is reported in `ignored`.
    ///
    /// An empty value counts as absent, not as a provision, matching the
    /// set-and-non-empty rule `dedup_credentials_by_declared_env` already
    /// applies. It is therefore reported in neither list.
    ///
    /// An all-absent push changes nothing: not the overlay, not the store, not
    /// `last_provided_at`.
    pub fn apply_push(
        &self,
        vars: DaemonEnvMap,
        absent: &[String],
        now: DateTime<Utc>,
    ) -> (Vec<String>, Vec<String>) {
        let mut ignored: Vec<String> = Vec::new();
        // Name/value pairs that were accepted, held until the metadata lock is
        // released so the overlay write below never nests inside it.
        let mut landing: Vec<(String, String)> = Vec::new();

        {
            let mut guard = self.lock();
            let mut names: Vec<(String, String)> = vars
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            names.sort();
            for (name, value) in names {
                if value.is_empty() {
                    // Exactly as absent: no change, and nothing reported —
                    // including for a name outside `required_env`, which is
                    // still not an "I sent you something you did not ask for".
                    continue;
                }
                let Some(entry) = guard.entries.get_mut(&name) else {
                    // Never asked for. Reported so a client can notice its own
                    // drift, but never stored: the daemon must not accumulate
                    // values it has no declared use for.
                    ignored.push(name);
                    continue;
                };
                entry.last_provided_at = Some(now);
                entry.unmet_since = None;
                guard.sources.insert(name.clone(), EnvSource::Pushed);
                landing.push((name, value));
            }
            // `absent` deliberately does nothing at all beyond being read: it
            // is the client saying "I was asked and cannot supply it".
            let _ = absent;
        }

        let accepted: Vec<String> = landing.iter().map(|(name, _)| name.clone()).collect();
        if landing.is_empty() {
            return (accepted, ignored);
        }
        // Insert under the overlay's write lock. A snapshot-mutate-replace here
        // would silently drop a value another push (or the coverage refresh)
        // installed in between, after this one had already answered `accepted`.
        let overlay = update_daemon_overlay(|overlay| {
            for (name, value) in landing {
                overlay.insert(name, value);
            }
        });
        self.persist(&overlay);
        (accepted, ignored)
    }

    /// Every required name with its live coverage, sorted by name.
    ///
    /// `source` and `digest` are computed here from the current overlay, so a
    /// coverage response can never report a digest for a value the daemon no
    /// longer holds.
    pub fn entries(&self) -> Vec<RequiredEnvEntry> {
        let overlay = daemon_overlay_snapshot();
        let guard = self.lock();
        guard
            .entries
            .iter()
            .map(|(name, stored)| {
                let held = overlay.get(name).filter(|value| !value.is_empty());
                RequiredEnvEntry {
                    name: name.clone(),
                    required_by: stored.required_by.clone(),
                    required_since: stored.required_since,
                    last_provided_at: stored.last_provided_at,
                    unmet_since: stored.unmet_since,
                    source: held.and(guard.sources.get(name).copied()),
                    digest: held.map(|value| coverage_digest(&guard.salt, name, value)),
                }
            })
            .collect()
    }

    /// Required names the daemon has no usable value for, sorted. This is the
    /// count `squad status` reports and the list `squad env` shows.
    pub fn unmet_names(&self) -> Vec<String> {
        let overlay = daemon_overlay_snapshot();
        let guard = self.lock();
        guard
            .entries
            .keys()
            .filter(|name| !is_set_and_non_empty(&overlay, name))
            .cloned()
            .collect()
    }

    /// The names *this task's* runs would go without, sorted.
    ///
    /// Its own `env()` names plus every name attributed to it by the daemon's
    /// global config or `AWMAN_OVERLAYS` (which apply to every run), minus
    /// whatever the daemon effectively has. Derived, never stored — exactly as
    /// `Task::last_run_status` is.
    pub fn unmet_for_task(&self, task: &Task) -> Vec<String> {
        let overlay = daemon_overlay_snapshot();
        let guard = self.lock();
        let mut names: BTreeSet<String> = env_overlay_names(&task.overlays).into_iter().collect();
        for (name, stored) in &guard.entries {
            if stored.required_by.iter().any(|owner| owner == &task.name) {
                names.insert(name.clone());
            }
        }
        names
            .into_iter()
            .filter(|name| !is_set_and_non_empty(&overlay, name))
            .collect()
    }

    /// Escalation point 4: one `warn!` the first time a given name goes unmet
    /// in this daemon's lifetime, `debug!` on every repeat.
    ///
    /// A task evaluating every five minutes must not produce a warning every
    /// five minutes — that buries the log `awman squad logs` prints.
    pub fn note_unmet_at_run_start(&self, task_name: &str, unmet: &[String]) {
        if unmet.is_empty() {
            return;
        }
        let mut fresh: Vec<&str> = Vec::new();
        let mut repeat: Vec<&str> = Vec::new();
        {
            let mut guard = self.lock();
            for name in unmet {
                if guard.warned.insert(name.clone()) {
                    fresh.push(name.as_str());
                } else {
                    repeat.push(name.as_str());
                }
            }
        }
        if !fresh.is_empty() {
            tracing::warn!(
                task = %task_name,
                names = ?fresh,
                "squad: starting a run with no value for declared env() names; \
                 its containers will start without them"
            );
        }
        if !repeat.is_empty() {
            tracing::debug!(
                task = %task_name,
                names = ?repeat,
                "squad: run still missing env() values already reported"
            );
        }
    }

    /// Remove the stored item (§5c). `Ok(true)` when a keychain backend was
    /// asked and answered; `Ok(false)` when there is nothing persisted to
    /// remove. The in-memory overlay is deliberately untouched: clearing what is
    /// stored must not disarm a daemon that is running fine.
    pub fn clear_store(&self) -> Result<bool, DataError> {
        let store = {
            let guard = self.lock();
            // A degraded daemon swapped `NoStore` in, but it may well have
            // written an item before it degraded — and nothing will ever
            // rewrite that item, because rule 3 forbids retrying. Answering
            // "nothing persisted" there would print a reassurance over a real
            // secret, so the backend that was in place before the degradation
            // is what gets asked. Clearing is idempotent, so asking costs
            // nothing when there is nothing to remove.
            let candidate = guard.store_before_degrade.as_ref().unwrap_or(&guard.store);
            if candidate.backend_name() != "keychain" {
                return Ok(false);
            }
            std::sync::Arc::clone(candidate)
        };
        match call_with_cap(KEYCHAIN_CALL_CAP, move || store.clear()) {
            Some(Ok(())) => Ok(true),
            Some(Err(error)) => Err(error),
            None => Err(DataError::Other(
                "keychain clear: timed out after 5s".to_string(),
            )),
        }
    }

    /// Whether the daemon effectively has a usable value for `name` — the live
    /// overlay, into which anything the store held was merged at startup.
    pub fn is_covered(&self, name: &str) -> bool {
        is_set_and_non_empty(&daemon_overlay_snapshot(), name)
    }

    /// Write the whole overlay through to the store, best-effort.
    ///
    /// The store is never authoritative and never fatal: a failure degrades
    /// this daemon to [`NoStore`] for the rest of its lifetime (rule 3), with no
    /// retry, and squad keeps working exactly as it does with persistence off.
    fn persist(&self, overlay: &DaemonEnvMap) {
        let store = {
            let guard = self.lock();
            if guard.degraded || guard.store.backend_name() == "none" {
                return;
            }
            std::sync::Arc::clone(&guard.store)
        };
        let vars = overlay.clone();
        match call_with_cap(KEYCHAIN_CALL_CAP, move || store.store(&vars)) {
            Some(Ok(())) => {}
            Some(Err(error)) => self.degrade("store", &error.to_string()),
            None => self.degrade("store", "timed out"),
        }
    }

    /// Rule 3, in one place: warn once, debug after, then stop trying.
    fn degrade(&self, op: &str, reason: &str) {
        let reason = sanitize_store_reason(reason);
        let mut guard = self.lock();
        if guard.degraded {
            return;
        }
        guard.degraded = true;
        // Keep the failed backend for `clear_store` only — see the field docs.
        guard.store_before_degrade = Some(std::sync::Arc::clone(&guard.store));
        guard.store = std::sync::Arc::new(NoStore);
        guard.persistence = EnvPersistence::Unavailable(format!("{op} failed: {reason}"));
        if guard.store_warned {
            tracing::debug!(op, reason, "squad env persistence failed again");
        } else {
            guard.store_warned = true;
            tracing::warn!(
                op,
                reason,
                "squad env persistence failed; continuing without it for the rest of \
                 this daemon's lifetime. Values already held stay in memory and runs \
                 are unaffected; only surviving a daemon restart is lost."
            );
        }
    }
}

/// How much of a backend's own error text may become daemon state.
const STORE_REASON_MAX_LEN: usize = 200;

/// Bound and scrub a store failure before it becomes the `persistence` string.
///
/// That string is not private: it reaches the daemon log, `GET /v1/status`,
/// `GET /v1/daemon/env`, and `awman squad env`'s header verbatim. Its ultimate
/// source is the `stderr` of `security` or `secret-tool` — unbounded text from
/// an external binary whose wording varies across OS versions. The comment on
/// `keychain::failed` reasons that neither tool echoes a value it was handed on
/// stdin, and that is very probably true; it is not a property this codebase
/// controls, and `env_store`'s keychain read already refuses to include serde's
/// message for exactly this reason. So any token carrying the payload envelope
/// is dropped and the whole thing is truncated.
fn sanitize_store_reason(reason: &str) -> String {
    use crate::data::fs::daemon_env::GO_KEYRING_B64_PREFIX;

    let mut scrubbed: String = reason
        .split_whitespace()
        .filter(|token| !token.contains(GO_KEYRING_B64_PREFIX))
        .collect::<Vec<_>>()
        .join(" ");
    if scrubbed.len() > STORE_REASON_MAX_LEN {
        // Truncate on a character boundary: the text is an external binary's
        // and may be any UTF-8 at all.
        let end = (0..=STORE_REASON_MAX_LEN)
            .rev()
            .find(|i| scrubbed.is_char_boundary(*i))
            .unwrap_or(0);
        scrubbed.truncate(end);
        scrubbed.push('…');
    }
    scrubbed
}

/// The one answer to "is this variable usable": set, and non-empty.
///
/// The same rule `dedup_credentials_by_declared_env` applies when deciding
/// whether a declared `env()` covers a credential. Two different answers in one
/// codebase would be a bug waiting to happen.
fn is_set_and_non_empty(overlay: &DaemonEnvMap, name: &str) -> bool {
    overlay.get(name).is_some_and(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::set_daemon_overlay;

    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::data::fs::task_store::{MountScope, TaskStatus};

    /// The overlay is one process-wide static, shared by every `#[cfg(test)]`
    /// module in the crate within the same `cargo test` binary — not just the
    /// tests in this file — so all of them serialise on the one lock in
    /// `data::config::env` rather than each other's private copy.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::data::config::env::DAEMON_OVERLAY_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_daemon_overlay(DaemonEnvMap::new());
        guard
    }

    fn state() -> DaemonEnvState {
        DaemonEnvState::new(Box::new(NoStore), EnvPersistence::None, Salt::random())
    }

    /// Entries are sorted by name, so every assertion looks its subject up
    /// rather than indexing.
    fn entry_for(state: &DaemonEnvState, name: &str) -> RequiredEnvEntry {
        state
            .entries()
            .into_iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("{name} must be in required_env"))
    }

    fn required(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(name, owners)| {
                (
                    (*name).to_string(),
                    owners.iter().map(|o| (*o).to_string()).collect(),
                )
            })
            .collect()
    }

    fn task(name: &str, overlays: &[&str]) -> Task {
        let now = Utc::now();
        Task {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            description: String::new(),
            repo_scope: PathBuf::from("/repo"),
            mount_scope: MountScope::GitRoot,
            overlays: overlays.iter().map(|o| (*o).to_string()).collect(),
            interval_secs: 60,
            status: TaskStatus::Active,
            agent: None,
            model: None,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            trigger_requested_at: None,
            last_run_status: None,
            unmet_env: Vec::new(),
        }
    }

    /// The digest rule is the contract between two processes, so it is pinned
    /// to a vector rather than to "whatever the implementation does".
    #[test]
    fn the_digest_is_sixteen_hex_chars_over_salt_name_and_value() {
        let salt = Salt::from_hex(&"ab".repeat(32)).unwrap();
        let digest = coverage_digest(&salt, "GITHUB_TOKEN", "s3cret");
        assert_eq!(digest.len(), 16);
        assert!(digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        // Deterministic across processes: the same inputs, the same string.
        assert_eq!(digest, coverage_digest(&salt, "GITHUB_TOKEN", "s3cret"));
        // The name is inside the hash, so two names sharing a value differ.
        assert_ne!(digest, coverage_digest(&salt, "OTHER", "s3cret"));
        // And so does the same name under a different salt.
        let other = Salt::from_hex(&"cd".repeat(32)).unwrap();
        assert_ne!(digest, coverage_digest(&other, "GITHUB_TOKEN", "s3cret"));
        // The name is length-prefixed, so a boundary shift between name and
        // value cannot collide (review-security F13a).
        assert_ne!(
            coverage_digest(&salt, "AB", "C"),
            coverage_digest(&salt, "A", "BC"),
            "an unambiguous encoding is what stops the split point from being free"
        );
    }

    #[test]
    fn a_salt_round_trips_through_its_hex_form() {
        let salt = Salt::random();
        assert_eq!(salt.to_hex().len(), 64);
        assert_eq!(
            Salt::from_hex(&salt.to_hex()).unwrap().as_bytes(),
            salt.as_bytes()
        );
        assert!(Salt::from_hex("short").is_none());
        assert!(Salt::from_hex(&"zz".repeat(32)).is_none());
        // A salt must never print itself.
        assert_eq!(format!("{salt:?}"), "Salt(<redacted>)");
    }

    /// Invariant 1, and the edge case the work item calls the single most
    /// damaging way to get §4 wrong: a client with none of the variables must
    /// not be able to wipe what a better-equipped client pushed.
    #[test]
    fn an_all_absent_push_is_a_no_op_on_the_overlay() {
        let _lock = guard();
        let state = state();
        let now = Utc::now();
        state.set_required(required(&[("TOKEN", &["nightly"])]), now);
        state.apply_push(DaemonEnvMap::from_pairs([("TOKEN", "v1")]), &[], now);
        assert!(state.is_covered("TOKEN"));
        let provided = entry_for(&state, "TOKEN").last_provided_at;

        let (accepted, ignored) = state.apply_push(
            DaemonEnvMap::new(),
            &["TOKEN".to_string(), "OTHER".to_string()],
            now + chrono::Duration::minutes(5),
        );

        assert!(accepted.is_empty() && ignored.is_empty());
        assert!(state.is_covered("TOKEN"), "absent must never delete");
        assert_eq!(
            entry_for(&state, "TOKEN").last_provided_at,
            provided,
            "an absent report must not touch last_provided_at"
        );
        assert!(state.unmet_names().is_empty());
    }

    /// Invariant 2: an empty value is the same answer as "I don't have it".
    #[test]
    fn an_empty_value_counts_as_absent_and_never_replaces_a_held_one() {
        let _lock = guard();
        let state = state();
        let now = Utc::now();
        state.set_required(required(&[("TOKEN", &["nightly"])]), now);
        state.apply_push(DaemonEnvMap::from_pairs([("TOKEN", "v1")]), &[], now);

        let (accepted, ignored) =
            state.apply_push(DaemonEnvMap::from_pairs([("TOKEN", "")]), &[], now);

        assert!(accepted.is_empty(), "an empty value is not a provision");
        assert!(ignored.is_empty(), "and is reported exactly as absent is");
        assert!(state.is_covered("TOKEN"));
    }

    /// A rotated token propagates on the next command with no restart, and a
    /// name the daemon never asked for is refused rather than accumulated.
    #[test]
    fn a_provision_replaces_and_an_unrequired_name_is_ignored() {
        let _lock = guard();
        let state = state();
        let now = Utc::now();
        state.set_required(required(&[("TOKEN", &["nightly"])]), now);
        state.apply_push(DaemonEnvMap::from_pairs([("TOKEN", "v1")]), &[], now);
        let first = entry_for(&state, "TOKEN").digest.unwrap();

        let (accepted, ignored) = state.apply_push(
            DaemonEnvMap::from_pairs([("TOKEN", "v2"), ("STRAY", "x")]),
            &[],
            now,
        );

        assert_eq!(accepted, vec!["TOKEN".to_string()]);
        assert_eq!(ignored, vec!["STRAY".to_string()]);
        assert_ne!(entry_for(&state, "TOKEN").digest.unwrap(), first);
        assert!(
            !state.is_covered("STRAY"),
            "the daemon must not hold a value it has no declared use for"
        );
    }

    /// Invariant 3: the *only* removal is a name leaving `required_env`, which
    /// is also what garbage-collects the stored item's contents.
    #[test]
    fn a_value_is_dropped_only_when_its_name_leaves_the_required_set() {
        let _lock = guard();
        let state = state();
        let now = Utc::now();
        state.set_required(
            required(&[("TOKEN", &["nightly"]), ("AWS", &["deploy"])]),
            now,
        );
        state.apply_push(
            DaemonEnvMap::from_pairs([("TOKEN", "v1"), ("AWS", "p")]),
            &[],
            now,
        );

        // "deploy" was deleted, so nothing declares AWS any more.
        state.set_required(required(&[("TOKEN", &["nightly"])]), now);

        assert!(state.is_covered("TOKEN"));
        assert!(!state.is_covered("AWS"), "the last task naming it is gone");
        assert!(state.entries().iter().all(|e| e.name != "AWS"));
    }

    /// Invariant 11: the unmet clock starts when a name first becomes required
    /// uncovered, so a typo'd name reads as long-unmet rather than newly-unmet.
    #[test]
    fn unmet_since_is_stamped_at_first_requirement_not_at_first_push() {
        let _lock = guard();
        let state = state();
        let first = Utc::now();
        state.set_required(required(&[("GTIHUB_TOKEN", &["nightly"])]), first);
        assert_eq!(entry_for(&state, "GTIHUB_TOKEN").unmet_since, Some(first));

        // Three later refreshes and a push that cannot supply it change nothing.
        let later = first + chrono::Duration::days(3);
        state.set_required(required(&[("GTIHUB_TOKEN", &["nightly"])]), later);
        state.apply_push(DaemonEnvMap::new(), &["GTIHUB_TOKEN".to_string()], later);
        assert_eq!(
            entry_for(&state, "GTIHUB_TOKEN").unmet_since,
            Some(first),
            "the clock must not restart on every refresh"
        );

        state.apply_push(
            DaemonEnvMap::from_pairs([("GTIHUB_TOKEN", "v")]),
            &[],
            later,
        );
        assert_eq!(entry_for(&state, "GTIHUB_TOKEN").unmet_since, None);
        assert_eq!(
            entry_for(&state, "GTIHUB_TOKEN").last_provided_at,
            Some(later)
        );
    }

    /// Nothing is seeded on the daemon's own behalf: with no task declaring an
    /// `env()` name, `required_env` is empty. `GITHUB_TOKEN` used to be added
    /// here unconditionally and exempted from unmet reporting, which silenced
    /// the one name WI 0116's own examples are written around.
    #[test]
    fn no_name_joins_the_required_set_unless_something_declares_it() {
        let _lock = guard();
        let state = state();
        state.set_required(BTreeMap::new(), Utc::now());

        assert!(
            state.entries().is_empty(),
            "an empty task set requires no env names at all"
        );
        assert!(state.unmet_names().is_empty());
    }

    /// The bug this replaced the host-side exemption for: a task declaring
    /// `env(GITHUB_TOKEN)` is reported unmet exactly like any other name, at
    /// every surface that reads one.
    #[test]
    fn a_task_declared_github_token_is_unmet_like_any_other_name() {
        let _lock = guard();
        let state = state();
        let now = Utc::now();
        state.set_required(required(&[("GITHUB_TOKEN", &["nightly"])]), now);

        let nightly = task("nightly", &["env(GITHUB_TOKEN)"]);
        assert_eq!(
            state.unmet_for_task(&nightly),
            vec!["GITHUB_TOKEN".to_string()],
            "the task card, detail modal and run row all read this"
        );
        assert_eq!(
            state.unmet_names(),
            vec!["GITHUB_TOKEN".to_string()],
            "`squad status` and `awman squad env` read this"
        );
        assert_eq!(
            entry_for(&state, "GITHUB_TOKEN").unmet_since,
            Some(now),
            "and it carries the same unmet clock as any other name"
        );

        // Supplying it clears every one of those, with no special case either.
        state.apply_push(DaemonEnvMap::from_pairs([("GITHUB_TOKEN", "v")]), &[], now);
        assert!(state.unmet_for_task(&nightly).is_empty());
        assert!(state.unmet_names().is_empty());
    }

    /// Invariant 4 and §6c's data path: the marker a task carries is derived
    /// from live coverage, and a global-source name counts for every task.
    #[test]
    fn unmet_for_task_covers_its_own_names_and_the_global_ones() {
        let _lock = guard();
        let state = state();
        let now = Utc::now();
        state.set_required(
            required(&[
                ("TOKEN", &["nightly"]),
                ("AWS", &["deploy"]),
                // A global overlay name: attributed to every task.
                ("SHARED", &["nightly", "deploy"]),
            ]),
            now,
        );

        let nightly = task("nightly", &["env(TOKEN)"]);
        assert_eq!(
            state.unmet_for_task(&nightly),
            vec!["SHARED".to_string(), "TOKEN".to_string()]
        );

        state.apply_push(
            DaemonEnvMap::from_pairs([("TOKEN", "v"), ("SHARED", "s")]),
            &[],
            now,
        );
        assert!(state.unmet_for_task(&nightly).is_empty());
        assert_eq!(
            state.unmet_for_task(&task("deploy", &["env(AWS)"])),
            vec!["AWS".to_string()]
        );
    }

    /// Escalation point 4: one `warn!` per name per lifetime. The counter here
    /// stands in for the log — what is asserted is that a name is only ever
    /// classified as "fresh" once.
    #[test]
    fn a_name_is_only_warned_about_once_per_daemon_lifetime() {
        let _lock = guard();
        let state = Arc::new(state());
        state.set_required(required(&[("TOKEN", &["nightly"])]), Utc::now());

        let unmet = state.unmet_for_task(&task("nightly", &["env(TOKEN)"]));
        assert_eq!(unmet, vec!["TOKEN".to_string()]);

        // Two ticks five minutes apart must not produce two warnings; the
        // second call finds the name already in `warned`.
        state.note_unmet_at_run_start("nightly", &unmet);
        let warned_after_first = state.lock().warned.len();
        state.note_unmet_at_run_start("nightly", &unmet);
        assert_eq!(state.lock().warned.len(), warned_after_first);
        assert_eq!(warned_after_first, 1);
    }

    /// Rule 3: a store failure degrades the daemon for its lifetime, is never
    /// fatal, and is reported through `persistence` rather than a log alone.
    #[test]
    fn a_store_failure_degrades_this_daemon_and_is_never_retried() {
        let _lock = guard();

        struct FailingStore;
        impl DaemonEnvStore for FailingStore {
            fn backend_name(&self) -> &'static str {
                "keychain"
            }
            fn store(&self, _vars: &DaemonEnvMap) -> Result<(), DataError> {
                Err(DataError::Other("keychain store: locked".into()))
            }
            fn load(&self) -> Result<Option<DaemonEnvMap>, DataError> {
                Ok(None)
            }
            fn clear(&self) -> Result<(), DataError> {
                Ok(())
            }
        }

        // The degrade path is exercised through `degrade` directly: `persist`
        // rebuilds a real `KeychainStore` for the detached call, which no unit
        // test may spawn.
        let state = DaemonEnvState::new(
            Box::new(FailingStore),
            EnvPersistence::Keychain,
            Salt::random(),
        );
        assert_eq!(state.persistence(), EnvPersistence::Keychain);

        state.degrade("store", "locked");
        assert_eq!(
            state.persistence(),
            EnvPersistence::Unavailable("store failed: locked".to_string())
        );
        assert!(state.lock().degraded);
        assert_eq!(state.lock().store.backend_name(), "none");

        // A second failure neither re-warns nor re-arms the backend.
        state.degrade("load", "gone");
        assert_eq!(
            state.persistence(),
            EnvPersistence::Unavailable("store failed: locked".to_string()),
            "the first reason is the one reported"
        );

        // And runs keep working: the values already held are untouched.
        let now = Utc::now();
        state.set_required(required(&[("TOKEN", &["nightly"])]), now);
        let (accepted, _) = state.apply_push(DaemonEnvMap::from_pairs([("TOKEN", "v")]), &[], now);
        assert_eq!(accepted, vec!["TOKEN".to_string()]);
        assert!(state.is_covered("TOKEN"));
    }

    /// End-to-end version of the test above: a backend that was probed
    /// available (`EnvPersistence::Keychain`) and then genuinely fails when
    /// `persist` calls it mid-session degrades to `NoStore` for the rest of
    /// the daemon's lifetime and is never retried — exercised through the
    /// real `apply_push` → `persist` → `degrade` path rather than calling
    /// `degrade` directly, with a call counter proving the backend is never
    /// touched again after the first failure.
    #[test]
    fn a_probed_available_store_that_fails_mid_session_is_never_retried() {
        let _lock = guard();

        struct CountingFailingStore(Arc<std::sync::atomic::AtomicUsize>);
        impl DaemonEnvStore for CountingFailingStore {
            fn backend_name(&self) -> &'static str {
                "keychain"
            }
            fn store(&self, _vars: &DaemonEnvMap) -> Result<(), DataError> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(DataError::Other("keychain store: locked".to_string()))
            }
            fn load(&self) -> Result<Option<DaemonEnvMap>, DataError> {
                Ok(None)
            }
            fn clear(&self) -> Result<(), DataError> {
                Ok(())
            }
        }

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let state = DaemonEnvState::new(
            Box::new(CountingFailingStore(calls.clone())),
            EnvPersistence::Keychain,
            Salt::random(),
        );
        let now = Utc::now();
        state.set_required(required(&[("TOKEN", &["nightly"])]), now);

        // First push: persist() calls the real store, which fails and
        // degrades the daemon.
        let (accepted, _) = state.apply_push(DaemonEnvMap::from_pairs([("TOKEN", "v1")]), &[], now);
        assert_eq!(accepted, vec!["TOKEN".to_string()]);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            state.persistence(),
            EnvPersistence::Unavailable("store failed: keychain store: locked".to_string())
        );

        // Second push: the value is still accepted and held in memory (rule
        // 3: never fatal), but persist() must short-circuit on `degraded`
        // rather than calling the failed backend again.
        let (accepted, _) = state.apply_push(DaemonEnvMap::from_pairs([("TOKEN", "v2")]), &[], now);
        assert_eq!(accepted, vec!["TOKEN".to_string()]);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a degraded daemon must never retry the store it already gave up on"
        );
        assert!(
            state.is_covered("TOKEN"),
            "runs proceed even though persistence is gone"
        );
    }

    /// Coverage is judged over the overlay *into which the store's values
    /// were merged at startup* — a name the store alone supplied must not
    /// show up as unmet just because no client has pushed it yet this
    /// lifetime.
    #[test]
    fn load_from_store_merges_into_the_overlay_so_a_store_only_name_is_not_unmet() {
        let _lock = guard();

        struct PreloadedStore;
        impl DaemonEnvStore for PreloadedStore {
            fn backend_name(&self) -> &'static str {
                "keychain"
            }
            fn store(&self, _vars: &DaemonEnvMap) -> Result<(), DataError> {
                Ok(())
            }
            fn load(&self) -> Result<Option<DaemonEnvMap>, DataError> {
                Ok(Some(DaemonEnvMap::from_pairs([("TOKEN", "from-keychain")])))
            }
            fn clear(&self) -> Result<(), DataError> {
                Ok(())
            }
        }

        let state = DaemonEnvState::new(
            Box::new(PreloadedStore),
            EnvPersistence::Keychain,
            Salt::random(),
        );
        let loaded = state.load_from_store(None);
        assert_eq!(
            loaded.as_ref().and_then(|m| m.get("TOKEN")),
            Some("from-keychain")
        );

        let now = Utc::now();
        state.set_required(required(&[("TOKEN", &["nightly"])]), now);

        assert!(
            state.is_covered("TOKEN"),
            "a value the store alone supplied must count as coverage"
        );
        assert!(
            state.unmet_names().is_empty(),
            "a store-only name must not report as unmet: {:?}",
            state.unmet_names()
        );
        let entry = entry_for(&state, "TOKEN");
        assert_eq!(entry.unmet_since, None);
        assert_eq!(entry.source, Some(EnvSource::Keychain));
        assert!(
            entry.digest.is_some(),
            "coverage over overlay ∪ keychain-loaded values must still digest it"
        );
    }

    /// Remediation of the lost-update race (review-security F2 /
    /// review-adversarial F2).
    ///
    /// `apply_push` used to snapshot the overlay, mutate a local copy, and
    /// replace the whole map. Two concurrent pushes then lost one another: both
    /// answered `accepted`, and only the last writer's value survived. The
    /// daemon's metadata said both names were provided, so nothing ever
    /// re-pushed the lost one and its containers started without it.
    #[test]
    fn two_concurrent_pushes_for_different_names_both_survive() {
        let _lock = guard();
        let state = Arc::new(state());
        let now = Utc::now();
        state.set_required(
            required(&[("TOKEN_A", &["nightly"]), ("TOKEN_B", &["deploy"])]),
            now,
        );

        // Enough rounds that a snapshot-mutate-replace loses at least one.
        for round in 0..64 {
            crate::data::config::env::set_daemon_overlay(DaemonEnvMap::new());
            let a = Arc::clone(&state);
            let b = Arc::clone(&state);
            let one = std::thread::spawn(move || {
                a.apply_push(DaemonEnvMap::from_pairs([("TOKEN_A", "a-value")]), &[], now)
            });
            let two = std::thread::spawn(move || {
                b.apply_push(DaemonEnvMap::from_pairs([("TOKEN_B", "b-value")]), &[], now)
            });
            let (accepted_a, _) = one.join().expect("push A panicked");
            let (accepted_b, _) = two.join().expect("push B panicked");
            assert_eq!(accepted_a, vec!["TOKEN_A".to_string()]);
            assert_eq!(accepted_b, vec!["TOKEN_B".to_string()]);
            assert!(
                state.is_covered("TOKEN_A") && state.is_covered("TOKEN_B"),
                "round {round}: a push answered `accepted` and then lost its value; \
                 unmet: {:?}",
                state.unmet_names()
            );
        }
    }

    /// The same race between a push and the coverage refresh, which runs on
    /// every `list` and `status` — i.e. every ten seconds from the TUI
    /// indicator poller. `set_required` must not roll a just-accepted push back.
    #[test]
    fn a_push_racing_the_coverage_refresh_is_never_rolled_back() {
        let _lock = guard();
        let state = Arc::new(state());
        let now = Utc::now();
        let wanted = required(&[("TOKEN", &["nightly"]), ("OTHER", &["deploy"])]);

        for round in 0..64 {
            crate::data::config::env::set_daemon_overlay(DaemonEnvMap::new());
            state.set_required(wanted.clone(), now);
            let pusher = Arc::clone(&state);
            let refresher = Arc::clone(&state);
            let wanted_for_thread = wanted.clone();
            let one = std::thread::spawn(move || {
                pusher.apply_push(DaemonEnvMap::from_pairs([("TOKEN", "v1")]), &[], now)
            });
            let two = std::thread::spawn(move || {
                refresher.set_required(wanted_for_thread, now);
            });
            let (accepted, _) = one.join().expect("push panicked");
            two.join().expect("refresh panicked");
            assert_eq!(accepted, vec!["TOKEN".to_string()]);
            assert!(
                state.is_covered("TOKEN"),
                "round {round}: the coverage refresh erased an accepted push"
            );
        }
    }

    /// Remediation of review-adversarial F3.
    ///
    /// A name loaded from the keychain that no task declares any more never
    /// enters `entries`, so it never appeared in `set_required`'s `removed`
    /// list. It sat in the overlay and was written back by every subsequent
    /// `persist`, forever, with no surface that showed it existed. Garbage
    /// collection is decided against the *required set*, not against the names
    /// that just left `entries`.
    #[test]
    fn a_keychain_loaded_name_no_task_declares_is_collected_and_rewritten_out() {
        let _lock = guard();

        struct RecordingStore(Arc<Mutex<Vec<DaemonEnvMap>>>);
        impl DaemonEnvStore for RecordingStore {
            fn backend_name(&self) -> &'static str {
                "keychain"
            }
            fn store(&self, vars: &DaemonEnvMap) -> Result<(), DataError> {
                self.0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(vars.clone());
                Ok(())
            }
            fn load(&self) -> Result<Option<DaemonEnvMap>, DataError> {
                Ok(Some(DaemonEnvMap::from_pairs([
                    ("KEEP", "kept"),
                    ("STALE", "left-over"),
                ])))
            }
            fn clear(&self) -> Result<(), DataError> {
                Ok(())
            }
        }

        let written: Arc<Mutex<Vec<DaemonEnvMap>>> = Arc::new(Mutex::new(Vec::new()));
        let state = DaemonEnvState::new(
            Box::new(RecordingStore(Arc::clone(&written))),
            EnvPersistence::Keychain,
            Salt::random(),
        );

        // Boot order: load first, then the first `refresh_required_env`.
        state.load_from_store(None);
        assert!(state.is_covered("STALE"), "the load merges everything held");

        state.set_required(required(&[("KEEP", &["nightly"])]), Utc::now());

        assert!(state.is_covered("KEEP"));
        assert!(
            !state.is_covered("STALE"),
            "a stored name nothing declares any more must be dropped from the overlay"
        );
        let written = written.lock().unwrap_or_else(|e| e.into_inner());
        let last = written.last().expect("the GC must rewrite the store");
        assert_eq!(
            last.names(),
            vec!["KEEP".to_string()],
            "the rewritten item must no longer carry the stale name"
        );
    }

    /// An empty value is absent, in both lists: for a name outside
    /// `required_env` it is not an "I sent you something you did not ask for"
    /// either, which is what `EnvPushResponse.ignored` documents.
    #[test]
    fn an_empty_value_for_an_unrequired_name_is_reported_in_neither_list() {
        let _lock = guard();
        let state = state();
        let now = Utc::now();
        state.set_required(required(&[("TOKEN", &["nightly"])]), now);

        let (accepted, ignored) =
            state.apply_push(DaemonEnvMap::from_pairs([("NOT_ASKED_FOR", "")]), &[], now);
        assert!(accepted.is_empty());
        assert!(
            ignored.is_empty(),
            "an empty value is absent, so it is reported in neither list: {ignored:?}"
        );
    }

    /// Remediation of review-security F5.
    ///
    /// The `persistence` string is public — it reaches the daemon log,
    /// `/v1/status`, `/v1/daemon/env` and `awman squad env`'s header verbatim —
    /// and its source is an external binary's unbounded `stderr`.
    #[test]
    fn a_store_failure_reason_is_bounded_and_drops_anything_carrying_the_payload_envelope() {
        use crate::data::fs::daemon_env::GO_KEYRING_B64_PREFIX;

        let echoed = format!("{GO_KEYRING_B64_PREFIX}c2VjcmV0LXZhbHVl");
        let scrubbed = sanitize_store_reason(&format!("security: bad input {echoed} near byte 4"));
        assert!(
            !scrubbed.contains(GO_KEYRING_B64_PREFIX) && !scrubbed.contains("c2VjcmV0LXZhbHVl"),
            "an echoed envelope must not become daemon state: {scrubbed}"
        );
        assert!(
            scrubbed.contains("security: bad input") && scrubbed.contains("near byte 4"),
            "the diagnosable part of the message is kept: {scrubbed}"
        );

        let long = sanitize_store_reason(&"x".repeat(10_000));
        assert!(
            long.chars().count() <= STORE_REASON_MAX_LEN + 1,
            "an unbounded stderr must not become an unbounded response field: {} chars",
            long.chars().count()
        );

        // Multi-byte input must not panic on the truncation boundary.
        let wide = sanitize_store_reason(&"é".repeat(500));
        assert!(wide.len() <= STORE_REASON_MAX_LEN + 3);
    }

    /// Remediation of review-security F9 (the reachable half).
    ///
    /// A daemon that degraded mid-session may already have written an item, and
    /// rule 3 says nothing will ever rewrite it. `squad env --clear` must still
    /// be able to remove it rather than printing "no stored env item to remove"
    /// over a real secret.
    #[test]
    fn a_degraded_daemon_can_still_clear_the_item_it_wrote_before_degrading() {
        let _lock = guard();

        struct FailsToWriteButClears {
            clears: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl DaemonEnvStore for FailsToWriteButClears {
            fn backend_name(&self) -> &'static str {
                "keychain"
            }
            fn store(&self, _vars: &DaemonEnvMap) -> Result<(), DataError> {
                Err(DataError::Other("keychain store: locked".to_string()))
            }
            fn load(&self) -> Result<Option<DaemonEnvMap>, DataError> {
                Ok(None)
            }
            fn clear(&self) -> Result<(), DataError> {
                self.clears
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        }

        let clears = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let state = DaemonEnvState::new(
            Box::new(FailsToWriteButClears {
                clears: Arc::clone(&clears),
            }),
            EnvPersistence::Keychain,
            Salt::random(),
        );
        let now = Utc::now();
        state.set_required(required(&[("TOKEN", &["nightly"])]), now);
        // The write fails, so the daemon degrades to `NoStore` for its lifetime.
        state.apply_push(DaemonEnvMap::from_pairs([("TOKEN", "v1")]), &[], now);
        assert!(
            matches!(state.persistence(), EnvPersistence::Unavailable(_)),
            "the failed write must have degraded this daemon"
        );

        assert!(
            state.clear_store().expect("a clear must not fail"),
            "the item a degraded daemon may have written is still removable"
        );
        assert_eq!(clears.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            state.is_covered("TOKEN"),
            "clearing what is *stored* must never disarm a running daemon"
        );
    }

    /// The counterpart: a daemon that never had a keychain backend has nothing
    /// to ask, and says so without touching the OS.
    #[test]
    fn a_daemon_with_no_backend_reports_nothing_to_clear() {
        let _lock = guard();
        let state = state();
        assert!(!state.clear_store().expect("a clear must not fail"));
    }

    /// Remediation of review-adversarial F6, at the consuming end.
    ///
    /// When the probe's answer is handed in, `load_from_store` must merge it
    /// and make **no** store call: the second capped keychain read is exactly
    /// what could push daemon start past the supervisor's ten-second wait.
    #[test]
    fn a_probed_payload_is_merged_without_asking_the_store_again() {
        let _lock = guard();

        struct CountingStore(Arc<std::sync::atomic::AtomicUsize>);
        impl DaemonEnvStore for CountingStore {
            fn backend_name(&self) -> &'static str {
                "keychain"
            }
            fn store(&self, _vars: &DaemonEnvMap) -> Result<(), DataError> {
                Ok(())
            }
            fn load(&self) -> Result<Option<DaemonEnvMap>, DataError> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(None)
            }
            fn clear(&self) -> Result<(), DataError> {
                Ok(())
            }
        }

        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let state = DaemonEnvState::new(
            Box::new(CountingStore(Arc::clone(&loads))),
            EnvPersistence::Keychain,
            Salt::random(),
        );

        let loaded = state.load_from_store(Some(DaemonEnvMap::from_pairs([(
            "TOKEN",
            "from-the-probe",
        )])));

        assert_eq!(
            loaded.as_ref().and_then(|map| map.get("TOKEN")),
            Some("from-the-probe")
        );
        assert_eq!(
            loads.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the probe already read the item; reading it again is the second \
             capped call this fix exists to remove"
        );

        // And it is a real merge, not just a passthrough: coverage and the
        // `keychain` source both follow, exactly as they do without a hint.
        state.set_required(required(&[("TOKEN", &["nightly"])]), Utc::now());
        assert!(state.is_covered("TOKEN"));
        assert_eq!(entry_for(&state, "TOKEN").source, Some(EnvSource::Keychain));
    }

    /// Without a hint the store is still asked — the path every degraded,
    /// opted-out or timed-out daemon takes.
    #[test]
    fn no_probed_payload_still_reads_the_store() {
        let _lock = guard();

        struct CountingStore(Arc<std::sync::atomic::AtomicUsize>);
        impl DaemonEnvStore for CountingStore {
            fn backend_name(&self) -> &'static str {
                "keychain"
            }
            fn store(&self, _vars: &DaemonEnvMap) -> Result<(), DataError> {
                Ok(())
            }
            fn load(&self) -> Result<Option<DaemonEnvMap>, DataError> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Some(DaemonEnvMap::from_pairs([(
                    "TOKEN",
                    "from-the-store",
                )])))
            }
            fn clear(&self) -> Result<(), DataError> {
                Ok(())
            }
        }

        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let state = DaemonEnvState::new(
            Box::new(CountingStore(Arc::clone(&loads))),
            EnvPersistence::Keychain,
            Salt::random(),
        );

        let loaded = state.load_from_store(None);
        assert_eq!(
            loaded.as_ref().and_then(|map| map.get("TOKEN")),
            Some("from-the-store")
        );
        assert_eq!(loads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
