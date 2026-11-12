//! Typed reads of every environment variable awman honours.
//!
//! Reads are funnelled through `Env` so that no scattered `std::env::var(…)`
//! calls leak elsewhere in the data layer.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

use serde_json::Value;

use crate::data::config::fields::{self, KeyError, RESERVED_ENV};
use crate::data::config::overrides::ConfigOverrides;

/// `AWMAN_HOME` — the one root for awman's config and data (see
/// `data::fs::data_paths`). Replaces `AWMAN_CONFIG_HOME`, `AWMAN_API_ROOT`,
/// `AWMAN_SQUAD_ROOT` and `AWMAN_ATTACH_DIR`.
pub use crate::data::fs::data_paths::AWMAN_HOME;

/// `AWMAN_OVERLAYS` — the env form of the `overlays` config key. It is not a
/// hand-named variable: the key table derives exactly this name for
/// `overlays`, and the constant only exists for the call sites that name it.
pub const AWMAN_OVERLAYS: &str = "AWMAN_OVERLAYS";

/// `AWMAN_REMOTE_SESSION` — sticky session id for remote operations, the env
/// form of `--session`. A session id is not a setting, so this is not config.
pub const AWMAN_REMOTE_SESSION: &str = "AWMAN_REMOTE_SESSION";

/// The prefix every awman variable shares. A captured `AWMAN_*` variable that
/// is not a [`PROCESS_VARS`] entry and not in the [`AWMAN_TEST_PREFIX`]
/// namespace is a config variable: its name is derived from the key table
/// (`agents.stuckTimeout` → `AWMAN_AGENTS_STUCK_TIMEOUT`).
pub const AWMAN_PREFIX: &str = "AWMAN_";

/// The test-only namespace (`AWMAN_TEST_ISOLATION`, …). Never config.
pub const AWMAN_TEST_PREFIX: &str = "AWMAN_TEST_";

/// `GITHUB_TOKEN` — optional token used by the GitHub issue provider.
pub const GITHUB_TOKEN: &str = "GITHUB_TOKEN";

/// Trusted host Git transport overrides, preserved by remote-session push-back.
pub const AWMAN_LEASE_CREDENTIAL_FILE: &str = "AWMAN_LEASE_CREDENTIAL_FILE";

pub const GIT_SSH_COMMAND: &str = "GIT_SSH_COMMAND";
pub const GIT_SSH: &str = "GIT_SSH";

/// `XDG_CONFIG_HOME` — XDG base directory for user-specific configuration.
pub const XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";

/// `XDG_DATA_HOME` — XDG base directory for user-specific data files.
pub const XDG_DATA_HOME: &str = "XDG_DATA_HOME";

/// `SHELL` — the user's login shell. Read only to tailor the shell snippet
/// printed alongside a freshly minted squad key; never to execute anything.
pub const SHELL: &str = "SHELL";

/// `PATH` — the executable search path. Host spawns resolve it from the
/// invoking instance's snapshot; daemon bootstrap also forwards it explicitly.
pub const PATH: &str = "PATH";

/// `HOME` — the user's home directory. Declared for the same reason as
/// [`PATH`]: a launchd agent or `systemd --user` unit does not inherit it.
pub const HOME: &str = "HOME";

/// `AWMAN_TEST_ISOLATION` — set to a truthy value (`1`, `true`, `yes`, `on`)
/// to keep awman off every per-user OS resource it would otherwise share with
/// a real installation: the keychain becomes a process-local, in-memory one,
/// and daemons are started as plain child processes, never through launchd or
/// `systemd --user`. For test runs, where those resources are the developer's
/// own: the squad daemon's stored env is one fixed keychain item, and its
/// launchd label / systemd unit is one fixed name, so a test daemon would
/// otherwise replace or stop the developer's real one. See
/// [`test_isolation_active`].
pub const AWMAN_TEST_ISOLATION: &str = "AWMAN_TEST_ISOLATION";

/// `AWMAN_TEST_DOCKER` — under [`AWMAN_TEST_ISOLATION`], set to a truthy value
/// to let awman run the real `docker` CLI. Off by default: the Docker tests
/// build, run and remove images and containers in the developer's own daemon.
pub const AWMAN_TEST_DOCKER: &str = "AWMAN_TEST_DOCKER";

/// `AWMAN_TEST_APPLE_CONTAINER` — as [`AWMAN_TEST_DOCKER`], for Apple's
/// `container` CLI.
pub const AWMAN_TEST_APPLE_CONTAINER: &str = "AWMAN_TEST_APPLE_CONTAINER";

/// `AWMAN_TEST_SBX` — as [`AWMAN_TEST_DOCKER`], for the Docker Sandboxes
/// `sbx` CLI.
pub const AWMAN_TEST_SBX: &str = "AWMAN_TEST_SBX";

/// `AWMAN_TEST_BUILTIN` — as [`AWMAN_TEST_DOCKER`], for the builtin microVM
/// runtime. Off by default: the builtin tests boot guests and write the
/// runtime's state directory.
pub const AWMAN_TEST_BUILTIN: &str = "AWMAN_TEST_BUILTIN";

/// `AWMAN_TEST_YOLO_COUNTDOWN_MS` — under [`AWMAN_TEST_ISOLATION`], a shorter
/// yolo / failure-retry countdown in milliseconds, so an integration test that
/// drives a real `awman` process through the countdown need not wait out the
/// 60 s default. Ignored outside test isolation; see
/// `engine::workflow::timing::yolo_countdown_duration`.
pub const AWMAN_TEST_YOLO_COUNTDOWN_MS: &str = "AWMAN_TEST_YOLO_COUNTDOWN_MS";

/// `AWMAN_TEST_AGENT_DONE_COUNTDOWN_MS` — under [`AWMAN_TEST_ISOLATION`], a
/// shorter `AgentDone` countdown (WI 0141) in milliseconds; see
/// `engine::workflow::timing::agent_done_countdown_duration`.
pub const AWMAN_TEST_AGENT_DONE_COUNTDOWN_MS: &str = "AWMAN_TEST_AGENT_DONE_COUNTDOWN_MS";

/// Every `AWMAN_*` name awman reads as a **process variable** rather than as
/// config: the storage root, the session id, the squad bearer key and the
/// test switches. None of these is ever resolved against the key table, and
/// no derived config name may equal one (`derived_names_never_collide_with_a_process_variable`).
/// `fields::RESERVED_ENV` is the key table's copy of this list; a test keeps
/// the two equal.
pub const PROCESS_VARS: &[&str] = &[
    AWMAN_HOME,
    AWMAN_REMOTE_SESSION,
    AWMAN_LEASE_CREDENTIAL_FILE,
    AWMAN_TEST_ISOLATION,
    AWMAN_TEST_DOCKER,
    AWMAN_TEST_APPLE_CONTAINER,
    AWMAN_TEST_SBX,
    AWMAN_TEST_BUILTIN,
    AWMAN_TEST_YOLO_COUNTDOWN_MS,
    AWMAN_TEST_AGENT_DONE_COUNTDOWN_MS,
];

/// Whether `name` is an `AWMAN_*` config variable (as opposed to a process
/// variable, a test switch, or something else entirely).
pub fn is_config_var_name(name: &str) -> bool {
    name.starts_with(AWMAN_PREFIX)
        && !name.starts_with(AWMAN_TEST_PREFIX)
        && !PROCESS_VARS.contains(&name)
        && !RESERVED_ENV.contains(&name)
}

/// `MSB_PATH` — Microsandbox's worker-binary override. awman never honours it:
/// the builtin runtime refuses to start while it (or any other name in
/// [`AMBIENT_MSB_OVERRIDES`]) is set, because the SDK lets it outrank the
/// embedded runtime. Read only to detect it; never forwarded.
pub const MSB_PATH: &str = "MSB_PATH";

/// `MSB_LIBKRUNFW_PATH` — Microsandbox's firmware override. See [`MSB_PATH`].
pub const MSB_LIBKRUNFW_PATH: &str = "MSB_LIBKRUNFW_PATH";

/// `MSB_AGENTD_PATH` — Microsandbox's guest-agent override. See [`MSB_PATH`].
pub const MSB_AGENTD_PATH: &str = "MSB_AGENTD_PATH";

/// `MSB_HOME` — Microsandbox's home-directory override. See [`MSB_PATH`].
pub const MSB_HOME: &str = "MSB_HOME";

/// `MSB_BACKEND` — Microsandbox's backend override. See [`MSB_PATH`].
pub const MSB_BACKEND: &str = "MSB_BACKEND";

/// `MSB_CONFIG_PATH` — Microsandbox's configuration-file override. See [`MSB_PATH`].
pub const MSB_CONFIG_PATH: &str = "MSB_CONFIG_PATH";

/// `MSB_PROFILE` — Microsandbox's profile override. See [`MSB_PATH`].
pub const MSB_PROFILE: &str = "MSB_PROFILE";

/// `MSB_CACHE_DIR` — Microsandbox's cache-directory override. See [`MSB_PATH`].
pub const MSB_CACHE_DIR: &str = "MSB_CACHE_DIR";

/// `MSB_SANDBOXES_DIR` — Microsandbox's sandbox-directory override. See [`MSB_PATH`].
pub const MSB_SANDBOXES_DIR: &str = "MSB_SANDBOXES_DIR";

/// `MSB_VOLUMES_DIR` — Microsandbox's volume-directory override. See [`MSB_PATH`].
pub const MSB_VOLUMES_DIR: &str = "MSB_VOLUMES_DIR";

/// `MSB_SNAPSHOTS_DIR` — Microsandbox's snapshot-directory override. See [`MSB_PATH`].
pub const MSB_SNAPSHOTS_DIR: &str = "MSB_SNAPSHOTS_DIR";

/// `MSB_LOGS_DIR` — Microsandbox's log-directory override. See [`MSB_PATH`].
pub const MSB_LOGS_DIR: &str = "MSB_LOGS_DIR";

/// `MSB_SECRETS_DIR` — Microsandbox's secrets-directory override. See [`MSB_PATH`].
pub const MSB_SECRETS_DIR: &str = "MSB_SECRETS_DIR";

/// Every ambient Microsandbox override the builtin runtime refuses to run
/// under. Never add these to [`ForwardedEnv::NAMES`].
pub const AMBIENT_MSB_OVERRIDES: &[&str] = &[
    MSB_PATH,
    MSB_LIBKRUNFW_PATH,
    MSB_AGENTD_PATH,
    MSB_HOME,
    MSB_BACKEND,
    MSB_CONFIG_PATH,
    MSB_PROFILE,
    MSB_CACHE_DIR,
    MSB_SANDBOXES_DIR,
    MSB_VOLUMES_DIR,
    MSB_SNAPSHOTS_DIR,
    MSB_LOGS_DIR,
    MSB_SECRETS_DIR,
];

/// `DOCKER_HOST` — the Docker Engine endpoint (`unix://`, `tcp://`,
/// `npipe://`). Read only when a `docker-local` image source names no host.
pub const DOCKER_HOST: &str = "DOCKER_HOST";

/// `DOCKER_CONTEXT` — Docker CLI context, which takes precedence over its host.
pub const DOCKER_CONTEXT: &str = "DOCKER_CONTEXT";

/// `DOCKER_CERT_PATH` — directory holding `ca.pem`, `cert.pem`, `key.pem` for
/// a TLS Docker Engine endpoint.
pub const DOCKER_CERT_PATH: &str = "DOCKER_CERT_PATH";

/// `DOCKER_TLS_VERIFY` — any non-empty value enables TLS verification of the
/// Docker Engine endpoint (the Docker CLI's own rule).
pub const DOCKER_TLS_VERIFY: &str = "DOCKER_TLS_VERIFY";

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
/// added to it.** `AWMAN_SQUAD_KEY` is deliberately absent, and so is every
/// config variable of a `sensitive` key (`AWMAN_REMOTE_TARGETS_<NAME>_API_KEY`):
/// this list is serialized into a plist on disk and passed as
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
    /// The fixed allowlist, in the order a job's environment is rendered.
    /// Config variables (`AWMAN_*` derived from the key table) follow it; see
    /// [`ForwardedEnv::from_process`].
    pub const NAMES: &'static [&'static str] = &[
        PATH,
        HOME,
        RUST_LOG,
        AWMAN_HOME,
        XDG_CONFIG_HOME,
        XDG_DATA_HOME,
    ];

    /// The subset of [`ForwardedEnv::NAMES`] actually set in this process, in
    /// list order, then every non-sensitive `AWMAN_*` config variable set in
    /// this process, sorted by name — so a daemon started by launchd or
    /// systemd resolves the same config the shell that started it would.
    pub fn from_process() -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Self::NAMES
            .iter()
            .filter_map(|name| std::env::var(name).ok().map(|v| ((*name).to_string(), v)))
            .collect();
        let mut config: Vec<(String, String)> = awman_vars_os()
            .into_iter()
            .filter(|(name, _)| Self::forwards_config_var(name))
            .collect();
        config.sort();
        out.extend(config);
        out
    }

    /// Whether a config variable may ride the bootstrap class: it must be a
    /// known, non-sensitive key. An unknown name forwards nothing (it would
    /// be ignored anyway), and a secret never reaches a plist or an argv.
    fn forwards_config_var(name: &str) -> bool {
        is_config_var_name(name)
            && fields::resolve_env(name, &|_| Vec::new()).is_some_and(|r| !r.key.sensitive)
    }
}

/// Frozen, memory-only environment, working directory and arguments.
///
/// [`Env::from_process`] captures the current process once. This type has no
/// serialization implementation. Tests construct snapshots via `Self::default()` or
/// `EnvSnapshot::with_overrides(…)`.
#[derive(Default, Clone, PartialEq, Eq)]
pub struct EnvSnapshot {
    values: HashMap<String, String>,
    // Native entries preserve non-UTF-8 values when the builtin frontend captures
    // its own process. Neither this type nor these fields implement serde.
    native: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    cwd: PathBuf,
    argv: Vec<std::ffi::OsString>,
    /// Process captures may read the daemon's in-memory task overlay for
    /// explicitly requested host payloads. Frontend-supplied snapshots may not.
    process_overlay: bool,
}

impl std::fmt::Debug for EnvSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EnvSnapshot { env: <redacted>, cwd: <redacted>, argv: <redacted> }")
    }
}

impl EnvSnapshot {
    /// Construct an empty snapshot.
    pub fn empty() -> Self {
        Self::default()
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
        let mut native: Vec<(std::ffi::OsString, std::ffi::OsString)> =
            values.iter().map(|(k, v)| (k.into(), v.into())).collect();
        native.sort_by(|a, b| a.0.cmp(&b.0));
        Self {
            values,
            native,
            ..Self::default()
        }
    }

    /// Instance input, held only in memory. There is deliberately no serde impl.
    pub fn from_full_env(env: std::collections::BTreeMap<String, String>) -> Self {
        Self::with_overrides(env)
    }

    pub fn from_instance_env(env: Vec<(String, String)>, cwd: PathBuf, argv: Vec<String>) -> Self {
        Self::with_overrides(env).with_invocation(cwd, argv)
    }

    pub fn with_invocation(mut self, cwd: PathBuf, argv: Vec<String>) -> Self {
        self.cwd = cwd;
        self.argv = argv.into_iter().map(Into::into).collect();
        self
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }
    pub fn argv(&self) -> &[std::ffi::OsString] {
        &self.argv
    }

    pub fn var_names(&self) -> Vec<&str> {
        let mut names: Vec<_> = self.values.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    pub fn to_hello_env(&self) -> crate::data::frontend_protocol::HelloEnv {
        crate::data::frontend_protocol::HelloEnv::from_pairs(
            self.values.iter().map(|(k, v)| (k.clone(), v.clone())),
        )
    }
    /// Environment for lease Git, including config probes. Never inherit Git
    /// configuration, tracing, repository selection, askpass or SSH commands.
    /// SSH may use the owner's agent and normal ~/.ssh/config in batch mode.
    pub fn for_lease_git(&self) -> Self {
        const ALLOWED: &[&str] = &[
            "PATH",
            "HOME",
            "USER",
            "LOGNAME",
            "TMPDIR",
            "TMP",
            "TEMP",
            "SystemRoot",
            "PATHEXT",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            "SSH_AUTH_SOCK",
        ];
        // Windows environment names are case-insensitive. In particular,
        // SYSTEMROOT must survive scrubbing for Winsock's provider loading.
        let allowed = |key: &str| {
            ALLOWED.iter().any(|name| {
                if cfg!(windows) {
                    name.eq_ignore_ascii_case(key)
                } else {
                    *name == key
                }
            })
        };
        let mut clean = self.clone();
        clean
            .native
            .retain(|(key, _)| key.to_str().is_some_and(&allowed));
        clean.values.retain(|key, _| allowed(key));
        for (key, value) in [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ] {
            clean.values.insert(key.into(), value.into());
            clean.native.push((key.into(), value.into()));
        }
        clean.process_overlay = false;
        clean
    }
    pub fn lease_credential_file(&self) -> Option<PathBuf> {
        self.get_os(AWMAN_LEASE_CREDENTIAL_FILE).map(PathBuf::from)
    }
    pub fn spawn_env(&self) -> impl Iterator<Item = (&std::ffi::OsStr, &std::ffi::OsStr)> {
        self.native
            .iter()
            .map(|(k, v)| (k.as_os_str(), v.as_os_str()))
    }
    pub fn path_dirs(&self) -> Vec<PathBuf> {
        let path = self.get_os(PATH);
        if let Some(path) = path {
            return std::env::split_paths(path).collect();
        }
        // execvp's default search when PATH is absent. An explicitly empty
        // PATH still means cwd; neither case may consult the daemon's PATH.
        #[cfg(unix)]
        {
            std::env::split_paths(if cfg!(target_os = "macos") {
                "/usr/bin:/bin:/usr/sbin:/sbin"
            } else {
                "/bin:/usr/bin"
            })
            .collect()
        }
        #[cfg(not(unix))]
        {
            Vec::new()
        }
    }
    pub fn temp_dir(&self) -> PathBuf {
        for key in if cfg!(windows) {
            &["TMP", "TEMP", "USERPROFILE"][..]
        } else {
            &["TMPDIR"][..]
        } {
            if let Some(path) = self.get_os(key).filter(|v| !v.is_empty()) {
                let path = PathBuf::from(path);
                return if path.is_relative() && !self.cwd.as_os_str().is_empty() {
                    self.cwd.join(path)
                } else {
                    path
                };
            }
        }
        PathBuf::from(if cfg!(windows) {
            r"C:\Windows\Temp"
        } else {
            "/tmp"
        })
    }

    /// CLI routing identity. Context overrides host as it does in Docker's CLI.
    pub fn runtime_endpoint(&self) -> (Option<String>, Option<String>) {
        (
            self.get(DOCKER_CONTEXT)
                .filter(|v| !v.is_empty())
                .map(str::to_owned),
            self.docker_host(),
        )
    }

    /// Return the raw value of a single var, if set.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values
            .get(key)
            .or_else(|| {
                cfg!(windows)
                    .then(|| {
                        self.values
                            .iter()
                            .find(|(name, _)| name.eq_ignore_ascii_case(key))
                            .map(|(_, value)| value)
                    })
                    .flatten()
            })
            .map(|s| s.as_str())
    }

    /// Resolve a container passthrough value. A daemon process snapshot sees
    /// a task's live overlay; an instance snapshot stays confined to the
    /// launching frontend's environment.
    pub fn host_payload(&self, key: &str) -> Option<String> {
        if self.process_overlay {
            let guard = daemon_overlay().read().unwrap_or_else(|e| e.into_inner());
            if let Some(value) = guard.get(key) {
                return Some(value.clone());
            }
        }
        self.get(key).map(str::to_owned)
    }

    pub fn get_os(&self, key: &str) -> Option<&std::ffi::OsStr> {
        self.native
            .iter()
            .find(|(name, _)| {
                name == key || (cfg!(windows) && name.to_string_lossy().eq_ignore_ascii_case(key))
            })
            .map(|(_, value)| value.as_os_str())
    }

    /// `AWMAN_HOME` as a `PathBuf`, if set and non-empty.
    pub fn awman_home(&self) -> Option<PathBuf> {
        self.get(AWMAN_HOME)
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from)
    }

    /// Every captured `AWMAN_*` **config** variable (see
    /// [`is_config_var_name`]), sorted by name. Nothing here has been checked
    /// against the key table yet; [`ConfigOverrides::from_env`] does that.
    pub fn config_vars(&self) -> Vec<(String, String)> {
        let mut vars: Vec<(String, String)> = self
            .values
            .iter()
            .filter(|(name, _)| is_config_var_name(name))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        vars.sort();
        vars
    }

    /// `AWMAN_REMOTE_SESSION` if set.
    pub fn remote_session(&self) -> Option<&str> {
        self.get(AWMAN_REMOTE_SESSION)
    }

    /// `GITHUB_TOKEN` if set and non-empty.
    pub fn github_token(&self) -> Option<&str> {
        self.get(GITHUB_TOKEN).filter(|v| !v.is_empty())
    }

    /// `SHELL` if set and non-empty.
    pub fn shell(&self) -> Option<&str> {
        self.get(SHELL).filter(|v| !v.is_empty())
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

    /// `HOME` as a `PathBuf` if captured and non-empty. A process snapshot
    /// resolves it at capture time; an empty snapshot has none.
    pub fn home_dir(&self) -> Option<PathBuf> {
        self.get(HOME).filter(|v| !v.is_empty()).map(PathBuf::from)
    }

    /// Whether `AWMAN_TEST_ISOLATION` is set to `1`, `true`, `yes` or `on`
    /// (case- and whitespace-insensitive). Any other value, or none, is off.
    pub fn test_isolation(&self) -> bool {
        self.truthy(AWMAN_TEST_ISOLATION)
    }

    /// Whether a truthy `AWMAN_TEST_DOCKER` opts back into the real `docker`.
    pub fn test_docker(&self) -> bool {
        self.truthy(AWMAN_TEST_DOCKER)
    }

    /// Whether a truthy `AWMAN_TEST_APPLE_CONTAINER` opts back into the real
    /// `container`.
    pub fn test_apple_container(&self) -> bool {
        self.truthy(AWMAN_TEST_APPLE_CONTAINER)
    }

    /// Whether a truthy `AWMAN_TEST_SBX` opts back into the real `sbx`.
    pub fn test_sbx(&self) -> bool {
        self.truthy(AWMAN_TEST_SBX)
    }

    /// Whether a truthy `AWMAN_TEST_BUILTIN` opts back into the real builtin
    /// runtime under test isolation.
    pub fn test_builtin(&self) -> bool {
        self.truthy(AWMAN_TEST_BUILTIN)
    }

    /// The yolo countdown `AWMAN_TEST_YOLO_COUNTDOWN_MS` asks for, when it
    /// holds a whole number of milliseconds and `AWMAN_TEST_ISOLATION` is on.
    /// `None` in every normal run, whatever the variable holds.
    pub fn test_yolo_countdown(&self) -> Option<std::time::Duration> {
        if !self.test_isolation() {
            return None;
        }
        self.get(AWMAN_TEST_YOLO_COUNTDOWN_MS)?
            .trim()
            .parse()
            .ok()
            .map(std::time::Duration::from_millis)
    }

    /// The `AgentDone` countdown `AWMAN_TEST_AGENT_DONE_COUNTDOWN_MS` asks for,
    /// under the same rules as [`Self::test_yolo_countdown`].
    pub fn test_agent_done_countdown(&self) -> Option<std::time::Duration> {
        if !self.test_isolation() {
            return None;
        }
        self.get(AWMAN_TEST_AGENT_DONE_COUNTDOWN_MS)?
            .trim()
            .parse()
            .ok()
            .map(std::time::Duration::from_millis)
    }

    /// The [`AMBIENT_MSB_OVERRIDES`] present in this snapshot, in list order.
    /// Set counts, even to an empty value: the SDK does not distinguish.
    pub fn ambient_msb_overrides(&self) -> Vec<&'static str> {
        AMBIENT_MSB_OVERRIDES
            .iter()
            .copied()
            .filter(|name| {
                self.get(name).is_some() || self.native.iter().any(|(key, _)| key == *name)
            })
            .collect()
    }

    /// `DOCKER_HOST`, when set and non-empty.
    pub fn docker_host(&self) -> Option<String> {
        self.get(DOCKER_HOST)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    }

    /// `DOCKER_CERT_PATH`, when set and non-empty.
    pub fn docker_cert_path(&self) -> Option<PathBuf> {
        self.get(DOCKER_CERT_PATH)
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from)
    }

    /// Whether `DOCKER_TLS_VERIFY` is set to any non-empty value.
    pub fn docker_tls_verify(&self) -> bool {
        self.get(DOCKER_TLS_VERIFY)
            .is_some_and(|v| !v.trim().is_empty())
    }

    /// `key` is set to `1`, `true`, `yes` or `on` (case- and
    /// whitespace-insensitive).
    fn truthy(&self, key: &str) -> bool {
        self.get(key).is_some_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
    }
}

// ── The override channel: `AWMAN_*` config variables and `--set` ──────────

impl ConfigOverrides {
    /// Parse `--set key=value` arguments (the `key=value` text of each, in
    /// command-line order). Each key is resolved against the key table and its
    /// value parsed by the key's type, exactly like `awman config set`: lists
    /// are comma-separated, maps are `k=v,k2=v2`, `null` resets the key to its
    /// default. When a key is given twice the last one wins (the pairs are
    /// applied in order).
    pub fn parse_set_args(args: &[String]) -> Result<Self, KeyError> {
        let mut pairs = Vec::with_capacity(args.len());
        for arg in args {
            let (path, raw) = arg.split_once('=').ok_or_else(|| KeyError::Value {
                path: arg.trim().to_string(),
                reason: "expected --set <key>=<value>".into(),
            })?;
            let path = path.trim();
            let resolved = fields::resolve(path)?;
            let value = fields::parse_input(resolved.key, raw)?;
            pairs.push((resolved.path, value));
        }
        Ok(Self::new(pairs))
    }

    /// The env layer: every `AWMAN_*` config variable in `env`, resolved
    /// against the key table and parsed by the key's type.
    ///
    /// `known_names` answers the map names present in the merged files under a
    /// map section (`workServers`, `remote.targets`, `agents.flags`), so a
    /// variable such as `AWMAN_WORK_SERVERS_IN_FACT_URL` binds to an existing
    /// `in-fact` entry; an unmatched name creates a new, lower-cased entry.
    ///
    /// A name that matches no key is not config: it is ignored with a
    /// warning. That covers the hand-named variables of earlier releases,
    /// which are simply unknown now. An empty value
    /// counts as unset. A known name with a bad value is an error naming the
    /// variable.
    pub fn from_env(
        env: &EnvSnapshot,
        known_names: impl Fn(&str) -> Vec<String>,
    ) -> Result<Self, KeyError> {
        let mut pairs: Vec<(String, Value)> = Vec::new();
        for (var, raw) in env.config_vars() {
            if raw.trim().is_empty() {
                continue;
            }
            let Some(resolved) = fields::resolve_env(&var, &known_names) else {
                tracing::warn!("ignoring {var}: it names no config key (see `awman config show`)");
                continue;
            };
            let value = fields::parse_input(resolved.key, &raw).map_err(|e| match e {
                KeyError::Value { reason, .. } => KeyError::Value {
                    path: format!("{} (from {var})", resolved.path),
                    reason,
                },
                other => other,
            })?;
            pairs.push((resolved.path, value));
        }
        Ok(Self::new(pairs))
    }
}

/// Every `AWMAN_*` variable of this process whose name and value are UTF-8.
/// `std::env::vars` panics on a non-UTF-8 entry anywhere in the environment;
/// a non-UTF-8 config value cannot be parsed anyway, so it is skipped.
fn awman_vars_os() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(name, _)| name.starts_with(AWMAN_PREFIX))
        .collect()
}

/// Namespace for capturing process-environment snapshots.
pub struct Env;

impl Env {
    /// Capture the full native process environment and invocation. Host children
    /// can clear their environment and rebuild it without losing unknown names
    /// or non-UTF-8 values. Config reads still use the typed accessors above.
    pub fn from_process() -> EnvSnapshot {
        let mut native: Vec<_> = std::env::vars_os().collect();
        native.sort_by(|a, b| a.0.cmp(&b.0));
        let mut values: HashMap<String, String> = native
            .iter()
            .filter_map(|(k, v)| Some((k.to_str()?.to_owned(), v.to_str()?.to_owned())))
            .collect();
        // Typed home lookup historically also used the OS account database.
        // Preserve that fallback without injecting a new variable into children.
        if !values.contains_key(HOME) {
            if let Some(home) = dirs::home_dir() {
                values.insert(HOME.into(), home.to_string_lossy().into_owned());
            }
        }
        for &name in AMBIENT_MSB_OVERRIDES {
            if host_var_os(name).is_some() {
                values.entry(name.into()).or_default();
            }
        }
        EnvSnapshot {
            values,
            native,
            cwd: std::env::current_dir().unwrap_or_default(),
            argv: std::env::args_os().collect(),
            process_overlay: true,
        }
    }

    /// Capture only the source transport settings and explicitly named auth
    /// variables needed for this acquisition. Absence is authoritative too.
    pub fn for_image_acquisition(
        registries: &std::collections::BTreeMap<
            String,
            crate::data::config::image_source::RegistryHostConfig,
        >,
    ) -> EnvSnapshot {
        use crate::data::config::image_source::RegistryAuthSource;
        let mut snapshot = Self::from_process();
        let mut names = vec![
            HOME,
            "DOCKER_CONFIG",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
        ];
        for registry in registries.values() {
            if let Some(RegistryAuthSource::Env {
                username_var,
                password_var,
            }) = &registry.auth
            {
                names.extend([username_var.as_str(), password_var.as_str()]);
            }
        }
        for name in names {
            if let Some(value) = host_var(name) {
                snapshot.native.retain(|(key, _)| key != name);
                snapshot.native.push((name.into(), value.clone().into()));
                snapshot.values.insert(name.into(), value);
            }
        }
        snapshot
    }
}

/// Whether this process must stay off per-user OS resources (see
/// [`AWMAN_TEST_ISOLATION`]).
///
/// Always true under `cfg(test)`, so a unit test cannot reach them however it
/// is run. Integration tests and the `awman` binaries they spawn are not built
/// with `cfg(test)`; they get it from the variable, which `make test` sets and
/// which a daemon started as a child process inherits.
///
/// Every code path that would touch such a resource asks this first. Read
/// fresh on each call rather than cached, so an integration test that sets the
/// variable in-process is honoured from that point on.
pub fn test_isolation_active() -> bool {
    cfg!(test) || Env::from_process().test_isolation()
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
// key but the two in the daemon overlay): those are properties of *this*
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

/// Resolve an override without discarding non-UTF-8 process values.
/// Presence checks for SDK paths must refuse these values as well.
pub fn host_var_os(name: &str) -> Option<std::ffi::OsString> {
    let guard = daemon_overlay().read().unwrap_or_else(|e| e.into_inner());
    if let Some(value) = guard.get(name) {
        return Some(value.into());
    }
    drop(guard);
    std::env::var_os(name)
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
