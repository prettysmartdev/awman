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
mod tests;
