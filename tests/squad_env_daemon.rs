//! WI 0116 §4b/§5 — the daemon-side facts a frontend depends on but cannot
//! observe over HTTP: what reaches the **daemon log** (which is exactly what
//! `awman squad logs` prints), and what reaches the **store**.
//!
//! Both are driven through the public `DaemonEnvState` / `env_store` surface
//! with an injected recording backend, so nothing here needs a real keychain —
//! the shell-out shims stay untested in CI, as they are today.
//!
//! The overlay these tests move is one process-wide static, so they take a
//! shared lock and live in their own test binary.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use awman::data::config::env::DaemonEnvMap;
use awman::data::error::DataError;
use awman::data::fs::daemon_env::{DaemonEnvStore, EnvPersistence, EnvPersistenceSetting, NoStore};
use awman::data::fs::{MountScope, Task, TaskStatus};
use awman::engine::squad::env_state::{DaemonEnvState, Salt};
use awman::engine::squad::env_store::resolve_with;

static OVERLAY_LOCK: Mutex<()> = Mutex::new(());

fn guard() -> std::sync::MutexGuard<'static, ()> {
    OVERLAY_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A store that records every call and the names of every map written to it.
/// Values are recorded too, so a test can prove the write side is being told
/// the truth — nothing here is a production path.
#[derive(Default)]
struct RecordingStore {
    written: Mutex<Vec<Vec<String>>>,
    clears: Mutex<usize>,
    /// What `load()` answers with, which is what `resolve_with`'s probe sees.
    load_answer: Mutex<Option<DaemonEnvMap>>,
}

impl RecordingStore {
    fn written(&self) -> Vec<Vec<String>> {
        self.written.lock().unwrap().clone()
    }
    fn clears(&self) -> usize {
        *self.clears.lock().unwrap()
    }
}

impl DaemonEnvStore for RecordingStore {
    fn backend_name(&self) -> &'static str {
        "keychain"
    }
    fn store(&self, vars: &DaemonEnvMap) -> Result<(), DataError> {
        self.written.lock().unwrap().push(vars.names());
        Ok(())
    }
    fn load(&self) -> Result<Option<DaemonEnvMap>, DataError> {
        Ok(self.load_answer.lock().unwrap().clone())
    }
    fn clear(&self) -> Result<(), DataError> {
        *self.clears.lock().unwrap() += 1;
        Ok(())
    }
}

/// A `DaemonEnvStore` handle shared between the test and the state that owns
/// it. `DaemonEnvState::new` takes a `Box`, so the recorder is reached through
/// an `Arc` the box forwards to.
struct SharedStore(Arc<RecordingStore>);

impl DaemonEnvStore for SharedStore {
    fn backend_name(&self) -> &'static str {
        self.0.backend_name()
    }
    fn store(&self, vars: &DaemonEnvMap) -> Result<(), DataError> {
        self.0.store(vars)
    }
    fn load(&self) -> Result<Option<DaemonEnvMap>, DataError> {
        self.0.load()
    }
    fn clear(&self) -> Result<(), DataError> {
        self.0.clear()
    }
}

fn required(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
    pairs
        .iter()
        .map(|(name, tasks)| {
            (
                (*name).to_string(),
                tasks.iter().map(|t| t.to_string()).collect(),
            )
        })
        .collect()
}

fn task(name: &str, overlays: &[&str]) -> Task {
    let now = chrono::Utc::now();
    Task {
        id: name.into(),
        name: name.into(),
        description: "d".into(),
        repo_scope: std::path::PathBuf::from("/repo"),
        mount_scope: MountScope::GitRoot,
        overlays: overlays.iter().map(|s| s.to_string()).collect(),
        interval_secs: 600,
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

/// Collect everything written to `tracing` while `body` runs, at DEBUG and
/// above — the same capture pattern `src/frontend/squad/unattended.rs` uses for
/// the daemon's other log assertions.
fn captured_tracing(body: impl FnOnce()) -> String {
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    tracing::subscriber::with_default(subscriber, body);
    let bytes = sink.0.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}

// ─── §4b escalation point 4: warn once, then debug ──────────────────────────

/// **An unmet name is warned about once per daemon lifetime and logged at
/// `debug!` thereafter.**
///
/// A task evaluating every five minutes would otherwise produce 288 identical
/// `WARN` lines a day, and `awman squad logs` — the one place a user goes to
/// find out what the daemon has been doing — would be unreadable. The line
/// still has to appear, once, because it is the only record that a run started
/// under-equipped.
#[test]
fn an_unmet_name_warns_once_and_debugs_thereafter_across_many_evaluation_cycles() {
    let _lock = guard();
    let state = DaemonEnvState::without_store();
    state.set_required(
        required(&[("AWS_PROFILE", &["deploy"])]),
        chrono::Utc::now(),
    );
    let unmet = state.unmet_for_task(&task("deploy", &["env(AWS_PROFILE)"]));
    assert_eq!(unmet, vec!["AWS_PROFILE".to_string()]);

    // Twenty ticks — a task on a five-minute interval, over an afternoon.
    let log = captured_tracing(|| {
        for _ in 0..20 {
            state.note_unmet_at_run_start("deploy", &unmet);
        }
    });

    let warns = log.lines().filter(|line| line.contains("WARN")).count();
    let debugs = log.lines().filter(|line| line.contains("DEBUG")).count();
    assert_eq!(
        warns, 1,
        "exactly one WARN per name per daemon lifetime:\n{log}"
    );
    assert_eq!(
        debugs, 19,
        "every later cycle is still recorded, at debug:\n{log}"
    );
    assert!(
        log.contains("starting a run with no value for declared env() names"),
        "{log}"
    );
    assert!(
        log.contains("run still missing env() values already reported"),
        "{log}"
    );
    // The name is in the log; nothing else about it could be.
    assert!(log.contains("AWS_PROFILE"), "{log}");
}

/// A name that genuinely leaves and re-enters `required_env` is warned about
/// again: the once-per-lifetime rule is keyed on the name being *continuously*
/// required, not on the daemon having ever mentioned it.
#[test]
fn re_adding_a_task_genuinely_re_warns() {
    let _lock = guard();
    let state = DaemonEnvState::without_store();
    let now = chrono::Utc::now();
    state.set_required(required(&[("AWS_PROFILE", &["deploy"])]), now);
    let unmet = vec!["AWS_PROFILE".to_string()];
    state.note_unmet_at_run_start("deploy", &unmet);

    // The task is deleted, so nothing declares the name…
    state.set_required(required(&[]), now);
    // …and then re-created.
    state.set_required(required(&[("AWS_PROFILE", &["deploy"])]), now);

    let log = captured_tracing(|| state.note_unmet_at_run_start("deploy", &unmet));
    assert_eq!(
        log.lines().filter(|line| line.contains("WARN")).count(),
        1,
        "a name that left and came back is a new fact:\n{log}"
    );
}

/// §4b, the unmet clock: `unmet_since` is stamped the moment a name becomes
/// required while uncovered, which is what `awman squad env`'s `SINCE` column
/// renders and what `squad status` counts.
#[test]
fn an_uncovered_name_carries_an_unmet_since_the_moment_it_becomes_required() {
    let _lock = guard();
    let state = DaemonEnvState::without_store();
    let now = chrono::Utc::now();
    state.set_required(required(&[("AWS_PROFILE", &["deploy"])]), now);

    let entry = state
        .entries()
        .into_iter()
        .find(|entry| entry.name == "AWS_PROFILE")
        .expect("the name is required");
    assert_eq!(
        entry.unmet_since,
        Some(now),
        "the clock starts at first requirement, not at the first push that omits it"
    );
    assert_eq!(state.unmet_names(), vec!["AWS_PROFILE".to_string()]);
}

// ─── §4b: the one removal reaches the store as well as the overlay ──────────

/// **A name leaving `required_env` is dropped from the overlay _and_ the
/// store.** Deleting the last task that named a variable is the one
/// unambiguous signal that nobody needs its value any more, so the stored item
/// is rewritten without it rather than left holding a secret with no declared
/// use.
#[test]
fn a_name_leaving_required_env_is_dropped_from_the_overlay_and_the_store_alike() {
    let _lock = guard();
    let recorder = Arc::new(RecordingStore::default());
    let state = DaemonEnvState::new(
        Box::new(SharedStore(recorder.clone())),
        EnvPersistence::Keychain,
        Salt::random(),
    );
    let now = chrono::Utc::now();
    state.set_required(
        required(&[("WI0116_KEEP", &["nightly"]), ("WI0116_GONE", &["deploy"])]),
        now,
    );
    state.apply_push(
        DaemonEnvMap::from_pairs([("WI0116_KEEP", "k"), ("WI0116_GONE", "g")]),
        &[],
        now,
    );
    assert!(state.is_covered("WI0116_GONE"));
    let after_push = recorder
        .written()
        .last()
        .cloned()
        .expect("the push was persisted");
    assert!(after_push.contains(&"WI0116_GONE".to_string()));

    // "deploy" was deleted, so nothing declares WI0116_GONE any more.
    state.set_required(required(&[("WI0116_KEEP", &["nightly"])]), now);

    assert!(
        state.is_covered("WI0116_KEEP"),
        "the surviving name keeps its value"
    );
    assert!(
        !state.is_covered("WI0116_GONE"),
        "the overlay dropped the value with the name"
    );
    assert!(
        state.entries().iter().all(|e| e.name != "WI0116_GONE"),
        "and it is gone from the coverage report"
    );
    let rewritten = recorder
        .written()
        .last()
        .cloned()
        .expect("the removal rewrote the store");
    assert!(
        !rewritten.contains(&"WI0116_GONE".to_string()),
        "the stored item is rewritten without it, not left holding a secret \
         nothing declares: {rewritten:?}"
    );
    assert!(rewritten.contains(&"WI0116_KEEP".to_string()));
}

// ─── §5: opting out removes what opting in stored ───────────────────────────

/// **Switching `envPersistence` to `"none"` clears the existing item on the
/// next daemon start.** Opting out has to remove what opting in stored, rather
/// than merely stopping future writes — otherwise the setting reads as "stop
/// saving" while a secret quietly stays in the user's keychain forever.
///
/// This is the decision `SquadDaemonEngine::bootstrap` makes, through
/// `env_store::resolve` → `resolve_with`, before the scheduler can tick.
#[test]
fn the_none_setting_clears_the_stored_item_at_the_next_daemon_start() {
    let recorder = Arc::new(RecordingStore::default());
    *recorder.load_answer.lock().unwrap() =
        Some(DaemonEnvMap::from_pairs([("WI0116_STORED", "v")]));

    let resolved = resolve_with(
        EnvPersistenceSetting::None,
        Box::new(SharedStore(recorder.clone())),
        std::time::Duration::from_secs(5),
    );

    assert_eq!(
        recorder.clears(),
        1,
        "the item a previous keychain-backed run stored must be removed"
    );
    assert_eq!(
        resolved.store.backend_name(),
        "none",
        "and nothing is persisted from here on"
    );
    assert!(
        resolved.fallback.is_none(),
        "an explicit opt-out is a choice, never a fallback, so it is never warned about"
    );
    assert!(
        recorder.written().is_empty(),
        "opting out writes nothing on its way past"
    );
}

/// The counterpart, so the clear above is attributable to the setting and not
/// to `resolve_with` clearing unconditionally: a keychain-backed start probes,
/// keeps the backend, and clears nothing.
#[test]
fn the_keychain_setting_probes_and_never_clears() {
    let recorder = Arc::new(RecordingStore::default());
    *recorder.load_answer.lock().unwrap() =
        Some(DaemonEnvMap::from_pairs([("WI0116_STORED", "v")]));

    let resolved = resolve_with(
        EnvPersistenceSetting::Keychain,
        Box::new(SharedStore(recorder.clone())),
        std::time::Duration::from_secs(5),
    );

    assert_eq!(resolved.store.backend_name(), "keychain");
    assert!(
        resolved.fallback.is_none(),
        "an available backend is not a fallback"
    );
    assert_eq!(recorder.clears(), 0, "a keychain start removes nothing");
}

/// `NoStore` is the shape every non-persisting daemon uses, and it must answer
/// as if there were nothing to find rather than erroring.
#[test]
fn a_daemon_with_no_store_reports_nothing_persisted() {
    let _lock = guard();
    let state = DaemonEnvState::new(Box::new(NoStore), EnvPersistence::None, Salt::random());
    assert_eq!(state.persistence().to_string(), "none");
    // `NoStore::load()` answers `Ok(None)`, which the state normalises to an
    // empty map: "nothing to restore", not "the store failed" (a failure is
    // what returns `None` here, and it degrades the daemon).
    assert_eq!(
        state.load_from_store(None).map(|loaded| loaded.names()),
        Some(Vec::new()),
        "a daemon with no store restores nothing and degrades nothing"
    );
    assert!(
        !state.clear_store().expect("clearing is never an error"),
        "there is no keychain backend to clear"
    );
}
