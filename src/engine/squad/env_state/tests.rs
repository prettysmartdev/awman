//! Tests for `engine::squad::env_state` (WI 0114 F-51: moved out of the
//! module file, unchanged).

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

    let (accepted, ignored) = state.apply_push(DaemonEnvMap::from_pairs([("TOKEN", "")]), &[], now);

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
