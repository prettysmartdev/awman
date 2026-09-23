//! Typed reads of every environment variable awman honours.
//!
//! Reads are funnelled through `Env` so that no scattered `std::env::var(…)`
//! calls leak elsewhere in the data layer.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

/// `AWMAN_CONFIG_HOME` — overrides the global config home directory.
pub const AWMAN_CONFIG_HOME: &str = "AWMAN_CONFIG_HOME";

/// `AWMAN_API_ROOT` — overrides the API storage root directory.
pub const AWMAN_API_ROOT: &str = "AWMAN_API_ROOT";

/// `AWMAN_SQUAD_ROOT` — overrides the squad storage root directory.
pub const AWMAN_SQUAD_ROOT: &str = "AWMAN_SQUAD_ROOT";

/// `AWMAN_OVERLAYS` — comma-separated list of overlay specs.
pub const AWMAN_OVERLAYS: &str = "AWMAN_OVERLAYS";

/// `AWMAN_REMOTE_ADDR` — overrides remote server address.
pub const AWMAN_REMOTE_ADDR: &str = "AWMAN_REMOTE_ADDR";

/// `AWMAN_REMOTE_SESSION` — sticky session id for remote operations.
pub const AWMAN_REMOTE_SESSION: &str = "AWMAN_REMOTE_SESSION";

/// `AWMAN_API_KEY` — API key for the remote API server.
pub const AWMAN_API_KEY: &str = "AWMAN_API_KEY";

/// `AWMAN_SQUAD_KEY` — bearer key the CLI and TUI authenticate to the squad
/// daemon with. Minted on the daemon's first start and printed once as a
/// shell-export snippet; see `awman squad start`.
pub const AWMAN_SQUAD_KEY: &str = "AWMAN_SQUAD_KEY";

/// `GITHUB_TOKEN` — optional token used by the GitHub issue provider.
pub const GITHUB_TOKEN: &str = "GITHUB_TOKEN";

/// `AWMAN_MAX_CONCURRENT_AGENTS` — overrides the max-concurrent-agents cap
/// for workflow execution.
pub const AWMAN_MAX_CONCURRENT_AGENTS: &str = "AWMAN_MAX_CONCURRENT_AGENTS";

/// `AWMAN_LAUNCH_MODE` — overrides the repository launch mode.
pub const AWMAN_LAUNCH_MODE: &str = "AWMAN_LAUNCH_MODE";

/// `XDG_CONFIG_HOME` — XDG base directory for user-specific configuration.
pub const XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";

/// `XDG_DATA_HOME` — XDG base directory for user-specific data files.
pub const XDG_DATA_HOME: &str = "XDG_DATA_HOME";

/// `SHELL` — the user's login shell. Read only to tailor the shell snippet
/// printed alongside a freshly minted squad key; never to execute anything.
pub const SHELL: &str = "SHELL";

/// `AWMAN_ATTACH_DIR` — overrides the directory attach sockets live in
/// (`~/.awman/attach/` by default). Used by tests and relocated setups; both
/// the serving and the attaching process must derive it the same way, which
/// is what makes a socket discoverable across processes of the same user.
pub const AWMAN_ATTACH_DIR: &str = "AWMAN_ATTACH_DIR";

/// `AWMAN_API_VERBOSE_SETUP` — set to a falsy value (`0`, `false`, `no`,
/// `off`) to demote the API server's per-session setup chatter from `info` to
/// `debug`, so operators can silence it while keeping it reachable through
/// `RUST_LOG`. Verbose is the default.
pub const AWMAN_API_VERBOSE_SETUP: &str = "AWMAN_API_VERBOSE_SETUP";

/// `PATH` — the executable search path. awman never reads it to resolve a
/// binary itself; it is declared because an OS process manager starting a
/// daemon must be handed it explicitly (see [`ForwardedEnv`]).
pub const PATH: &str = "PATH";

/// `HOME` — the user's home directory. Declared for the same reason as
/// [`PATH`]: a launchd agent or `systemd --user` unit does not inherit it.
pub const HOME: &str = "HOME";

/// `RUST_LOG` — the `tracing` filter directive. Forwarded to a daemon job so
/// an operator can start a daemon with the verbosity they asked for.
pub const RUST_LOG: &str = "RUST_LOG";

/// The bootstrap environment an OS process manager must be handed explicitly
/// when it starts an awman daemon.
///
/// A launchd agent inherits *nothing* from the shell that started awman: it
/// runs with launchd's own minimal `PATH` (no `/usr/local/bin`, so no
/// `docker`) and none of awman's path overrides. A daemon started without
/// those overrides resolves a different storage root than the process waiting
/// for it, then publishes its endpoint somewhere that process never looks.
/// A `systemd --user` unit has the same problem: its environment is systemd's
/// own minimal template, not the invoking shell's.
///
/// This is the **bootstrap class** — values the daemon needs before it can
/// open its storage root or start listening. **[`ForwardedEnv::NAMES`] is a
/// non-secret allowlist, and nothing that could hold a secret may ever be
/// added to it.** [`AWMAN_API_KEY`] / [`AWMAN_SQUAD_KEY`] are deliberately
/// absent: this list is serialized into a plist on disk and passed as
/// `--setenv` arguments on a visible `systemd-run` invocation, and a bearer
/// key belongs in none of those places. The daemon authenticates against the
/// key *hash* it reads from the storage root, so it needs no key of its own.
/// Secrets a task needs at run time (the **payload class** — `env(VAR)`
/// overlay values) never travel this way; they are pushed over the
/// authenticated loopback socket after the daemon is already listening and
/// held only in memory (and, optionally, the OS keychain).
///
/// That is also why [`ForwardedEnv::from_process`] reads the real process
/// environment rather than [`host_var`]: `host_var` consults the squad
/// daemon's payload overlay first, and a payload value must never be written
/// to a plist or an argv, whatever it happens to be named.
///
/// Declared here rather than beside the spawner (WI 0114 F-29/F-37): every
/// environment variable awman reads is named in this module, and reading the
/// process environment is Layer 0's job.
pub struct ForwardedEnv;

impl ForwardedEnv {
    /// The allowlist, in the order a job's environment is rendered.
    pub const NAMES: &'static [&'static str] = &[
        PATH,
        HOME,
        RUST_LOG,
        AWMAN_CONFIG_HOME,
        AWMAN_API_ROOT,
        AWMAN_SQUAD_ROOT,
        XDG_CONFIG_HOME,
        XDG_DATA_HOME,
        AWMAN_OVERLAYS,
        AWMAN_MAX_CONCURRENT_AGENTS,
        AWMAN_LAUNCH_MODE,
    ];

    /// The subset of [`ForwardedEnv::NAMES`] actually set in this process, in
    /// list order.
    pub fn from_process() -> Vec<(String, String)> {
        Self::NAMES
            .iter()
            .filter_map(|name| std::env::var(name).ok().map(|v| ((*name).to_string(), v)))
            .collect()
    }
}

/// Frozen snapshot of every env var awman reads.
///
/// `EnvSnapshot::from_process()` captures the current process's environment
/// once. Tests construct snapshots directly via `EnvSnapshot::default()` or
/// `EnvSnapshot::with_overrides(…)`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EnvSnapshot {
    values: HashMap<String, String>,
}

impl EnvSnapshot {
    /// Construct an empty snapshot.
    pub fn empty() -> Self {
        Self {
            values: HashMap::new(),
        }
    }

    /// Build a snapshot from a list of `(key, value)` pairs. Useful in tests.
    pub fn with_overrides<I, K, V>(entries: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut values = HashMap::new();
        for (k, v) in entries {
            values.insert(k.into(), v.into());
        }
        Self { values }
    }

    /// Return the raw value of a single var, if set.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(|s| s.as_str())
    }

    /// `AWMAN_CONFIG_HOME` as a `PathBuf` if set.
    pub fn config_home(&self) -> Option<PathBuf> {
        self.get(AWMAN_CONFIG_HOME).map(PathBuf::from)
    }

    /// `AWMAN_API_ROOT` as a `PathBuf` if set.
    pub fn api_root(&self) -> Option<PathBuf> {
        self.get(AWMAN_API_ROOT).map(PathBuf::from)
    }

    /// `AWMAN_SQUAD_ROOT` as a `PathBuf` if set.
    pub fn squad_root(&self) -> Option<PathBuf> {
        self.get(AWMAN_SQUAD_ROOT).map(PathBuf::from)
    }

    /// `AWMAN_OVERLAYS` raw string if set.
    pub fn overlays(&self) -> Option<&str> {
        self.get(AWMAN_OVERLAYS)
    }

    /// `AWMAN_REMOTE_ADDR` if set.
    pub fn remote_addr(&self) -> Option<&str> {
        self.get(AWMAN_REMOTE_ADDR)
    }

    /// `AWMAN_REMOTE_SESSION` if set.
    pub fn remote_session(&self) -> Option<&str> {
        self.get(AWMAN_REMOTE_SESSION)
    }

    /// `AWMAN_API_KEY` if set.
    pub fn api_key(&self) -> Option<&str> {
        self.get(AWMAN_API_KEY)
    }

    /// `AWMAN_SQUAD_KEY` if set and non-empty.
    pub fn squad_key(&self) -> Option<&str> {
        self.get(AWMAN_SQUAD_KEY).filter(|v| !v.is_empty())
    }

    /// `GITHUB_TOKEN` if set and non-empty.
    pub fn github_token(&self) -> Option<&str> {
        self.get(GITHUB_TOKEN).filter(|v| !v.is_empty())
    }

    /// `AWMAN_MAX_CONCURRENT_AGENTS` parsed as a `usize`, if set and valid.
    pub fn max_concurrent_agents(&self) -> Option<usize> {
        self.get(AWMAN_MAX_CONCURRENT_AGENTS)?.parse().ok()
    }

    /// `SHELL` if set and non-empty.
    pub fn shell(&self) -> Option<&str> {
        self.get(SHELL).filter(|v| !v.is_empty())
    }

    /// `AWMAN_LAUNCH_MODE` parsed as a launch mode, if set to a recognized
    /// value. Invalid environment values are ignored, matching the tolerant
    /// behavior of other typed environment accessors.
    pub fn launch_mode(&self) -> Option<crate::data::config::repo::LaunchMode> {
        match self.get(AWMAN_LAUNCH_MODE)? {
            "stdio" => Some(crate::data::config::repo::LaunchMode::Stdio),
            "acp" => Some(crate::data::config::repo::LaunchMode::Acp),
            _ => None,
        }
    }

    /// `XDG_CONFIG_HOME` as a `PathBuf` if set and non-empty.
    pub fn xdg_config_home(&self) -> Option<PathBuf> {
        self.get(XDG_CONFIG_HOME)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }

    /// `XDG_DATA_HOME` as a `PathBuf` if set and non-empty.
    pub fn xdg_data_home(&self) -> Option<PathBuf> {
        self.get(XDG_DATA_HOME)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }

    /// `AWMAN_ATTACH_DIR` as a `PathBuf` if set and non-empty.
    pub fn attach_dir(&self) -> Option<PathBuf> {
        self.get(AWMAN_ATTACH_DIR)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }

    /// Whether the API server logs per-session setup lines at `info` rather
    /// than `debug`. `true` unless `AWMAN_API_VERBOSE_SETUP` is set to one of
    /// `0`, `false`, `no`, `off` (case- and whitespace-insensitive).
    pub fn api_verbose_setup(&self) -> bool {
        match self.get(AWMAN_API_VERBOSE_SETUP) {
            Some(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            ),
            None => true,
        }
    }
}

/// Namespace for capturing process-environment snapshots.
pub struct Env;

impl Env {
    /// The bearer keys a snapshot resolves through [`host_var`] rather than
    /// straight from the process environment.
    ///
    /// These two are *published* into a running process, not merely inherited
    /// by it. The process that mints a squad key is the TUI (or CLI) that
    /// started the daemon, and it cannot be told to `export` the key
    /// afterwards, so `SquadSupervisor::provision_key` writes the key into the
    /// Layer 0 daemon overlay instead. Every later reader builds a *new*
    /// supervisor from a *fresh* snapshot — `Dispatch::admit`, `squad attach`,
    /// the TUI's `refresh_squad_key` — so a snapshot that read only
    /// `std::env::var` would not see it, `provision_key` would return `None`,
    /// and the daemon would answer 401 to the very process that minted its key
    /// (WI 0114 F-47).
    ///
    /// Only these two. Everything else in the snapshot is a property of *this*
    /// process — its storage roots, its launch mode, its shell — which nothing
    /// publishes at run time and which therefore keeps reading the real
    /// environment.
    const PUBLISHED_KEYS: &'static [&'static str] = &[AWMAN_API_KEY, AWMAN_SQUAD_KEY];

    /// Capture every awman-relevant env var from the current process.
    ///
    /// Reads are limited to the known constants above so that the snapshot
    /// is deterministic and minimal.
    pub fn from_process() -> EnvSnapshot {
        let keys = [
            AWMAN_CONFIG_HOME,
            AWMAN_API_ROOT,
            AWMAN_SQUAD_ROOT,
            AWMAN_OVERLAYS,
            AWMAN_REMOTE_ADDR,
            AWMAN_REMOTE_SESSION,
            AWMAN_API_KEY,
            AWMAN_SQUAD_KEY,
            GITHUB_TOKEN,
            AWMAN_MAX_CONCURRENT_AGENTS,
            AWMAN_LAUNCH_MODE,
            XDG_CONFIG_HOME,
            XDG_DATA_HOME,
            SHELL,
            AWMAN_ATTACH_DIR,
            AWMAN_API_VERBOSE_SETUP,
        ];
        let mut values = HashMap::new();
        for k in keys {
            let read = if Self::PUBLISHED_KEYS.contains(&k) {
                host_var(k)
            } else {
                std::env::var(k).ok()
            };
            if let Some(v) = read {
                values.insert(k.to_string(), v);
            }
        }
        EnvSnapshot { values }
    }
}

// ── Daemon environment overlay (WI 0116 §2) ─────────────────────────────────
//
// Layer 0's single read path for *host-supplied* values. The squad daemon does
// not inherit the environment of the shell that created a task, so `env(VAR)`
// overlays resolved inside the daemon would otherwise silently see nothing.
// The daemon is handed those values over its authenticated loopback socket and
// installs them here; every production read of a host-supplied value then goes
// through [`host_var`], which prefers the overlay and falls back to the real
// process environment.
//
// The overlay deliberately replaces one process-global (the environment) with a
// controlled one rather than adding a second source of truth:
//
//   * it needs no `std::env::set_var`, which is a genuine data race when called
//     from a request-handler thread while other threads read the environment;
//   * it makes these reads testable, which `std::env::var` is not (see the
//     serialising `AWMAN_ENV_LOCK` mutex in `engine::overlay`).
//
// `std::env::var` remains correct — and is deliberately kept — for awman's own
// invariants (storage-root resolution, launch mode, and every `EnvSnapshot`
// key but the two in [`Env::PUBLISHED_KEYS`]): those are properties of *this*
// process, not values a daemon's client can supply.

/// The daemon's payload environment. Empty in every process that never calls
/// [`set_daemon_overlay`], which is every process except the squad daemon.
static DAEMON_OVERLAY: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();

fn daemon_overlay() -> &'static RwLock<HashMap<String, String>> {
    DAEMON_OVERLAY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Resolve a host environment value: the daemon overlay first, then the real
/// process environment.
///
/// In a process that never called [`set_daemon_overlay`] the overlay is empty,
/// so this is byte-for-byte `std::env::var(name).ok()` — including returning
/// `Some("")` for a variable that is set to the empty string. Callers that need
/// "set and non-empty" filter for it themselves, exactly as they do today.
pub fn host_var(name: &str) -> Option<String> {
    // A poisoned lock still holds a perfectly readable map: a panic in a writer
    // cannot corrupt a `HashMap<String, String>` that is only ever assigned
    // wholesale. Degrading to the process environment here would be a silent
    // loss of the daemon's payload, which is the exact failure this exists to
    // prevent, so the guard is recovered instead.
    let guard = daemon_overlay().read().unwrap_or_else(|e| e.into_inner());
    if let Some(value) = guard.get(name) {
        return Some(value.clone());
    }
    drop(guard);
    std::env::var(name).ok()
}

/// Install (replacing) the daemon's payload environment.
///
/// The squad daemon is the only writer in production. Values live in memory
/// only; nothing here is ever written to disk, to a plist, or to an argv.
pub fn set_daemon_overlay(vars: DaemonEnvMap) {
    let mut guard = daemon_overlay().write().unwrap_or_else(|e| e.into_inner());
    *guard = vars.into_inner();
}

/// Mutate the overlay **in place**, under its write lock, and return the map as
/// it stands afterwards.
///
/// This is the only correct way to do a read-modify-write of the overlay.
/// `daemon_overlay_snapshot()` + mutate a local copy + [`set_daemon_overlay`]
/// is a lost update: the whole-map replace at the end clobbers anything a
/// concurrent writer installed in between. That is not theoretical — the
/// daemon's coverage refresh runs on every `list` and `status` (so every ten
/// seconds from the TUI indicator poller) while `POST /v1/daemon/env` handlers
/// run concurrently on the axum runtime, and losing a just-accepted push is
/// precisely the silent "container starts without its token" failure the
/// overlay exists to prevent.
///
/// The closure must not touch the daemon's state mutex: the lock ordering that
/// keeps this deadlock-free is that the overlay lock is never held while the
/// state mutex is.
pub fn update_daemon_overlay(f: impl FnOnce(&mut HashMap<String, String>)) -> DaemonEnvMap {
    let mut guard = daemon_overlay().write().unwrap_or_else(|e| e.into_inner());
    f(&mut guard);
    DaemonEnvMap(guard.clone())
}

/// A clone of the current overlay. Empty in every non-daemon process.
pub fn daemon_overlay_snapshot() -> DaemonEnvMap {
    let guard = daemon_overlay().read().unwrap_or_else(|e| e.into_inner());
    DaemonEnvMap(guard.clone())
}

/// Serialises every test in the crate that reads or writes the process-wide
/// [`DAEMON_OVERLAY`] — `cargo test` runs every `#[cfg(test)] mod tests` in
/// this crate inside the *same* binary, so a static like `DAEMON_OVERLAY` is
/// genuinely shared across module boundaries and across threads, not just
/// within one file. Any test elsewhere in the crate that calls
/// [`set_daemon_overlay`] must take this lock first.
#[cfg(test)]
pub(crate) static DAEMON_OVERLAY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
static CONFIG_HOME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Exclusive ownership of the process-wide [`AWMAN_CONFIG_HOME`] for the
/// guard's lifetime, restoring the previous value on drop.
///
/// The same "one binary, one process" point as [`DAEMON_OVERLAY_TEST_LOCK`]
/// applies, and it is easy to miss: five modules used to guard this variable
/// with a mutex of their own, which serialises each module against itself and
/// nothing else — the same as no lock at all. A `clean` or `overlay` test
/// could repoint the config home out from under `dispatch`'s daemon
/// bootstrap, so `Engines::for_daemon` read a config it was never given and
/// built the wrong runtime tier, and out from under the workflow phase tests,
/// so `ContextDirResolver::from_process_env` resolved a directory they never
/// created. Every test that mutates the variable goes through this type.
#[cfg(test)]
pub(crate) struct ConfigHomeGuard {
    /// Held for the guard's lifetime to keep the lock; never read.
    _lock: std::sync::MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl ConfigHomeGuard {
    /// Take the lock and point [`AWMAN_CONFIG_HOME`] at `home`.
    pub(crate) fn set(home: impl AsRef<std::path::Path>) -> Self {
        // Recover a poisoned lock rather than propagating the poison: a test
        // that panicked while holding it has already reported its own
        // failure, and cascading a `PoisonError` into every other
        // config-home test buries the real cause behind a wall of noise.
        let _lock = CONFIG_HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os(AWMAN_CONFIG_HOME);
        std::env::set_var(AWMAN_CONFIG_HOME, home.as_ref());
        Self { _lock, previous }
    }
}

#[cfg(test)]
impl Drop for ConfigHomeGuard {
    fn drop(&mut self) {
        // Restoring here rather than at the end of the test body is what
        // keeps a panicking test from leaking its temporary config home —
        // usually an already-deleted directory — onto the test that runs next.
        match self.previous.take() {
            Some(value) => std::env::set_var(AWMAN_CONFIG_HOME, value),
            None => std::env::remove_var(AWMAN_CONFIG_HOME),
        }
    }
}

/// A set of host environment values bound for (or held by) the squad daemon.
///
/// The type exists for one reason beyond naming: its [`Debug`](std::fmt::Debug)
/// implementation prints **names only, never values**, so a `DaemonEnvMap` that
/// wanders into a tracing span, an error string, or a `dbg!` cannot leak a
/// secret. `Serialize` is used in exactly two places — the keychain payload and
/// the `POST /v1/daemon/env` request body — and never in a log or an error.
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct DaemonEnvMap(HashMap<String, String>);

impl DaemonEnvMap {
    /// An empty map.
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    /// Build a map from `(name, value)` pairs verbatim, with no filtering.
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self(
            pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }

    /// Capture `names` through `lookup`, keeping only those whose value is set
    /// **and non-empty**.
    ///
    /// The set-and-non-empty rule is not incidental: it is the same rule
    /// `dedup_credentials_by_declared_env` already applies when deciding
    /// whether a declared `env()` covers a credential. Two different answers to
    /// "is this variable usable" in one codebase would be a bug waiting to
    /// happen, so an empty value never counts as a provision.
    pub fn capture<'a>(
        names: impl IntoIterator<Item = &'a str>,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        let mut map = HashMap::new();
        for name in names {
            if let Some(value) = lookup(name) {
                if !value.is_empty() {
                    map.insert(name.to_string(), value);
                }
            }
        }
        Self(map)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(|s| s.as_str())
    }

    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) -> Option<String> {
        self.0.insert(name.into(), value.into())
    }

    pub fn remove(&mut self, name: &str) -> Option<String> {
        self.0.remove(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    /// Every name held, sorted, so callers render a stable order.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.0.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }

    /// Drop every name for which `keep` returns `false`.
    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.0.retain(|name, _| keep(name.as_str()));
    }

    pub fn into_inner(self) -> HashMap<String, String> {
        self.0
    }
}

/// Names only — never a value. See the type docs.
impl std::fmt::Debug for DaemonEnvMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonEnvMap")
            .field("names", &self.names())
            .finish()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    // ── AWMAN_ATTACH_DIR (F-37) ──────────────────────────────────────────

    #[test]
    fn attach_dir_returns_none_when_absent() {
        assert!(EnvSnapshot::empty().attach_dir().is_none());
    }

    #[test]
    fn attach_dir_returns_none_when_empty_string() {
        let snap = EnvSnapshot::with_overrides([(AWMAN_ATTACH_DIR, "")]);
        assert!(
            snap.attach_dir().is_none(),
            "an empty value must fall back to the default directory, not resolve \
             sockets against a relative path"
        );
    }

    #[test]
    fn attach_dir_returns_the_path_when_set() {
        let snap = EnvSnapshot::with_overrides([(AWMAN_ATTACH_DIR, "/tmp/sockets")]);
        assert_eq!(snap.attach_dir(), Some(PathBuf::from("/tmp/sockets")));
    }

    // ── AWMAN_API_VERBOSE_SETUP (F-37) ───────────────────────────────────
    //
    // The falsy set and the default are the contract this moved out of
    // `api_server/session_setup.rs`; they must not drift.

    #[test]
    fn api_verbose_setup_defaults_to_true_when_absent() {
        assert!(EnvSnapshot::empty().api_verbose_setup());
    }

    #[test]
    fn api_verbose_setup_is_false_for_every_falsy_spelling() {
        for raw in [
            "0",
            "false",
            "no",
            "off",
            "FALSE",
            "No",
            "OFF",
            " off ",
            "\tfalse\n",
        ] {
            let snap = EnvSnapshot::with_overrides([(AWMAN_API_VERBOSE_SETUP, raw)]);
            assert!(
                !snap.api_verbose_setup(),
                "{raw:?} must disable verbose setup logging"
            );
        }
    }

    #[test]
    fn api_verbose_setup_is_true_for_anything_else() {
        for raw in ["1", "true", "yes", "on", "", "anything"] {
            let snap = EnvSnapshot::with_overrides([(AWMAN_API_VERBOSE_SETUP, raw)]);
            assert!(
                snap.api_verbose_setup(),
                "{raw:?} must leave verbose setup logging on"
            );
        }
    }

    #[test]
    fn xdg_config_home_returns_none_when_absent() {
        let snap = EnvSnapshot::empty();
        assert!(snap.xdg_config_home().is_none());
    }

    #[test]
    fn xdg_config_home_returns_none_when_empty_string() {
        let snap = EnvSnapshot::with_overrides([(XDG_CONFIG_HOME, "")]);
        assert!(snap.xdg_config_home().is_none());
    }

    #[test]
    fn xdg_data_home_returns_none_when_absent() {
        let snap = EnvSnapshot::empty();
        assert!(snap.xdg_data_home().is_none());
    }

    #[test]
    fn xdg_data_home_returns_none_when_empty_string() {
        let snap = EnvSnapshot::with_overrides([(XDG_DATA_HOME, "")]);
        assert!(snap.xdg_data_home().is_none());
    }

    #[test]
    fn github_token_returns_only_non_empty_values() {
        assert_eq!(
            EnvSnapshot::with_overrides([(GITHUB_TOKEN, "token")]).github_token(),
            Some("token")
        );
        assert!(EnvSnapshot::with_overrides([(GITHUB_TOKEN, "")])
            .github_token()
            .is_none());
    }

    #[test]
    fn env_from_process_captures_xdg_config_and_data_home() {
        let cfg_val = "/tmp/awman-test-xdg-cfg-0086";
        let data_val = "/tmp/awman-test-xdg-data-0086";
        std::env::set_var(XDG_CONFIG_HOME, cfg_val);
        std::env::set_var(XDG_DATA_HOME, data_val);
        let snap = Env::from_process();
        std::env::remove_var(XDG_CONFIG_HOME);
        std::env::remove_var(XDG_DATA_HOME);
        assert_eq!(
            snap.xdg_config_home(),
            Some(PathBuf::from(cfg_val)),
            "from_process() must capture XDG_CONFIG_HOME"
        );
        assert_eq!(
            snap.xdg_data_home(),
            Some(PathBuf::from(data_val)),
            "from_process() must capture XDG_DATA_HOME"
        );
    }

    /// Invariant 12: in a process whose overlay is empty (every process that
    /// never calls `set_daemon_overlay`, and this process once the guard
    /// below clears it), `host_var` must be byte-for-byte
    /// `std::env::var(name).ok()` — including `Some("")` for a variable set
    /// to the empty string.
    #[test]
    fn host_var_matches_std_env_var_when_the_overlay_is_empty() {
        let _lock = DAEMON_OVERLAY_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_daemon_overlay(DaemonEnvMap::new());
        let name = "AWMAN_TEST_HOST_VAR_PARITY_0116";
        std::env::remove_var(name);

        assert_eq!(host_var(name), None);
        assert_eq!(host_var(name), std::env::var(name).ok());

        std::env::set_var(name, "process-value");
        assert_eq!(host_var(name), Some("process-value".to_string()));
        assert_eq!(host_var(name), std::env::var(name).ok());

        // `Some("")` is a real, distinct answer from `None` — parity must
        // hold there too, not just for a non-empty value.
        std::env::set_var(name, "");
        assert_eq!(host_var(name), Some(String::new()));
        assert_eq!(host_var(name), std::env::var(name).ok());

        std::env::remove_var(name);
        set_daemon_overlay(DaemonEnvMap::new());
    }

    /// The overlay is checked first and wins over whatever the process
    /// environment holds for the same name.
    #[test]
    fn host_var_prefers_the_overlay_over_the_process_environment() {
        let _lock = DAEMON_OVERLAY_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_daemon_overlay(DaemonEnvMap::new());
        let name = "AWMAN_TEST_HOST_VAR_OVERLAY_WINS_0116";
        std::env::set_var(name, "process-value");

        let mut overlay = DaemonEnvMap::new();
        overlay.insert(name, "overlay-value");
        set_daemon_overlay(overlay);

        assert_eq!(host_var(name), Some("overlay-value".to_string()));
        assert_ne!(
            host_var(name),
            std::env::var(name).ok(),
            "the overlay must win, so parity with std::env::var breaks here on purpose"
        );

        std::env::remove_var(name);
        set_daemon_overlay(DaemonEnvMap::new());
    }

    /// `DaemonEnvMap`'s hand-written `Debug` must print names only, under
    /// both `{:?}` and `{:#?}`, for values chosen specifically because they
    /// would be easy to spot if they leaked.
    #[test]
    fn daemon_env_map_debug_prints_names_never_values() {
        let map = DaemonEnvMap::from_pairs([
            ("A_NAME", "s3cret \"quoted\" value"),
            ("B_NAME", "back\\slash and space"),
        ]);

        let compact = format!("{map:?}");
        assert_eq!(compact, r#"DaemonEnvMap { names: ["A_NAME", "B_NAME"] }"#);

        let pretty = format!("{map:#?}");
        for forbidden in ["s3cret", "quoted", "back\\slash", "and space"] {
            assert!(
                !compact.contains(forbidden),
                "{compact:?} leaked {forbidden:?}"
            );
            assert!(
                !pretty.contains(forbidden),
                "{pretty:?} leaked {forbidden:?}"
            );
        }
    }

    // ── ForwardedEnv (WI 0114 F-29) ──────────────────────────────────────

    /// A bearer key must never be serialized into the plist: it is a file on
    /// disk, and the daemon authenticates against the on-disk key *hash*
    /// instead. `PATH` must be forwarded — without it launchd's minimal one
    /// leaves the daemon unable to find `docker`.
    #[test]
    fn no_bearer_key_is_ever_forwarded_to_a_daemon_job() {
        assert!(!ForwardedEnv::NAMES.contains(&AWMAN_SQUAD_KEY));
        assert!(!ForwardedEnv::NAMES.contains(&AWMAN_API_KEY));
        assert!(ForwardedEnv::NAMES.contains(&"PATH"));
        assert!(ForwardedEnv::NAMES.contains(&AWMAN_SQUAD_ROOT));
        // WI 0116 §2: the payload's own overlay spec now rides the bootstrap
        // class too, so a daemon started by systemd/launchd resolves the
        // same overlays the shell that ran `squad start` would have.
        assert!(ForwardedEnv::NAMES.contains(&AWMAN_OVERLAYS));
    }

    /// `ForwardedEnv::from_process()` must emit only the names actually set in this
    /// process, and in `ForwardedEnv::NAMES`'s own order — not insertion order of
    /// whichever happen to be set, and not alphabetical.
    #[test]
    fn forwarded_env_returns_only_set_names_in_list_order() {
        let prev_overlays = std::env::var(AWMAN_OVERLAYS).ok();
        let prev_agents = std::env::var(AWMAN_MAX_CONCURRENT_AGENTS).ok();
        let prev_launch = std::env::var(AWMAN_LAUNCH_MODE).ok();

        std::env::remove_var(AWMAN_OVERLAYS);
        std::env::set_var(AWMAN_MAX_CONCURRENT_AGENTS, "7");
        std::env::remove_var(AWMAN_LAUNCH_MODE);

        let result = ForwardedEnv::from_process();

        assert!(
            !result.iter().any(|(n, _)| n == AWMAN_OVERLAYS),
            "an unset name must not appear at all: {result:?}"
        );
        assert!(
            !result.iter().any(|(n, _)| n == AWMAN_LAUNCH_MODE),
            "an unset name must not appear at all: {result:?}"
        );
        assert!(
            result
                .iter()
                .any(|(n, v)| n == AWMAN_MAX_CONCURRENT_AGENTS && v == "7"),
            "a set name must appear with its value: {result:?}"
        );

        let names: Vec<&str> = result.iter().map(|(n, _)| n.as_str()).collect();
        let expected_order: Vec<&str> = ForwardedEnv::NAMES
            .iter()
            .copied()
            .filter(|candidate| names.contains(candidate))
            .collect();
        assert_eq!(
            names, expected_order,
            "ForwardedEnv::from_process() must preserve ForwardedEnv::NAMES's own order"
        );

        match prev_overlays {
            Some(v) => std::env::set_var(AWMAN_OVERLAYS, v),
            None => std::env::remove_var(AWMAN_OVERLAYS),
        }
        match prev_agents {
            Some(v) => std::env::set_var(AWMAN_MAX_CONCURRENT_AGENTS, v),
            None => std::env::remove_var(AWMAN_MAX_CONCURRENT_AGENTS),
        }
        match prev_launch {
            Some(v) => std::env::set_var(AWMAN_LAUNCH_MODE, v),
            None => std::env::remove_var(AWMAN_LAUNCH_MODE),
        }
    }
}

#[cfg(test)]
mod published_key_tests {
    use super::*;

    /// The regression F-47 introduced and this restores: the process that mints
    /// a squad key publishes it into the daemon overlay, and every *later*
    /// reader in that process resolves it through a fresh `EnvSnapshot`. If
    /// `Env::from_process` read only `std::env::var`, the second supervisor in
    /// the minting process would see no key at all.
    #[test]
    fn a_snapshot_taken_after_a_key_is_published_sees_that_key() {
        let _lock = DAEMON_OVERLAY_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = daemon_overlay_snapshot();

        update_daemon_overlay(|vars| {
            vars.insert(AWMAN_SQUAD_KEY.to_string(), "minted-squad-key".to_string());
            vars.insert(AWMAN_API_KEY.to_string(), "minted-api-key".to_string());
        });

        let snapshot = Env::from_process();
        assert_eq!(
            snapshot.squad_key(),
            Some("minted-squad-key"),
            "a snapshot built after the mint must carry the published squad key"
        );
        assert_eq!(
            snapshot.api_key(),
            Some("minted-api-key"),
            "a snapshot built after the mint must carry the published API key"
        );

        set_daemon_overlay(previous);
    }

    /// The counterpart: nothing *but* the two published keys is resolved
    /// through the overlay, so a daemon client cannot redirect this process's
    /// storage root by pushing an `env(VAR)` payload named like one of awman's
    /// own invariants.
    #[test]
    fn the_overlay_cannot_redirect_a_storage_root() {
        let _lock = DAEMON_OVERLAY_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = daemon_overlay_snapshot();
        let real_root = std::env::var(AWMAN_SQUAD_ROOT).ok();

        update_daemon_overlay(|vars| {
            vars.insert(AWMAN_SQUAD_ROOT.to_string(), "/attacker/root".to_string());
        });

        let snapshot = Env::from_process();
        assert_eq!(
            snapshot.get(AWMAN_SQUAD_ROOT),
            real_root.as_deref(),
            "the overlay must not reach awman's own invariants"
        );

        set_daemon_overlay(previous);
    }
}
