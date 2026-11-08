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
pub const GIT_SSH_COMMAND: &str = "GIT_SSH_COMMAND";
pub const GIT_SSH: &str = "GIT_SSH";

/// `XDG_CONFIG_HOME` — XDG base directory for user-specific configuration.
pub const XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";

/// `XDG_DATA_HOME` — XDG base directory for user-specific data files.
pub const XDG_DATA_HOME: &str = "XDG_DATA_HOME";

/// `SHELL` — the user's login shell. Read only to tailor the shell snippet
/// printed alongside a freshly minted squad key; never to execute anything.
pub const SHELL: &str = "SHELL";

/// `PATH` — the executable search path. awman never reads it to resolve a
/// binary itself; it is declared because an OS process manager starting a
/// daemon must be handed it explicitly (see [`ForwardedEnv`]).
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
            .filter(|name| self.get(name).is_some())
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
    /// Capture every awman-relevant env var from the current process: the
    /// fixed list below, plus every variable whose name starts with
    /// [`AWMAN_PREFIX`], so derived config names reach the snapshot without a
    /// key-table lookup at capture time.
    pub fn from_process() -> EnvSnapshot {
        let keys = [
            AWMAN_HOME,
            AWMAN_REMOTE_SESSION,
            GITHUB_TOKEN,
            XDG_CONFIG_HOME,
            XDG_DATA_HOME,
            SHELL,
            AWMAN_TEST_ISOLATION,
            AWMAN_TEST_DOCKER,
            AWMAN_TEST_APPLE_CONTAINER,
            AWMAN_TEST_SBX,
            AWMAN_TEST_BUILTIN,
            AWMAN_TEST_YOLO_COUNTDOWN_MS,
            DOCKER_HOST,
            DOCKER_CERT_PATH,
            DOCKER_TLS_VERIFY,
        ];
        let mut values = HashMap::new();
        // The home directory is resolved here, once, so nothing above Layer 0
        // consults the ambient environment (or the passwd database) for it.
        if let Some(home) = dirs::home_dir() {
            values.insert(HOME.to_string(), home.to_string_lossy().into_owned());
        }
        // Config variables come from the real process environment only (never
        // the daemon overlay): a daemon client must not be able to change
        // this process's config by pushing a payload named like one.
        for (name, value) in awman_vars_os() {
            if is_config_var_name(&name) {
                values.insert(name, value);
            }
        }
        for k in keys {
            let read = std::env::var(k).ok();
            if let Some(v) = read {
                values.insert(k.to_string(), v);
            }
        }
        // These SDK overrides are presence-sensitive, and may be non-UTF-8.
        // Store a marker because their contents are never used by awman.
        for &name in AMBIENT_MSB_OVERRIDES {
            if host_var_os(name).is_some() {
                values.insert(name.to_string(), String::new());
            }
        }
        EnvSnapshot { values }
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

/// Exclusive ownership of the process-wide [`AWMAN_HOME`] for the
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
    /// Take the lock and point [`AWMAN_HOME`] at `home`.
    pub(crate) fn set(home: impl AsRef<std::path::Path>) -> Self {
        // Recover a poisoned lock rather than propagating the poison: a test
        // that panicked while holding it has already reported its own
        // failure, and cascading a `PoisonError` into every other
        // config-home test buries the real cause behind a wall of noise.
        let _lock = CONFIG_HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os(AWMAN_HOME);
        std::env::set_var(AWMAN_HOME, home.as_ref());
        Self { _lock, previous }
    }
}

/// Exclusive ownership of the config-home lock plus a snapshot of other
/// process variables that resolve configuration (`XDG_*`, forwarded
/// `AWMAN_*` names), restored on drop. Tests that mutate those variables, or
/// that compare against paths derived from the process environment, take
/// this so a concurrent [`ConfigHomeGuard`] test cannot interleave.
#[cfg(test)]
pub(crate) struct ProcessEnvGuard {
    /// Held for the guard's lifetime to keep the lock; never read.
    _lock: std::sync::MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

#[cfg(test)]
impl ProcessEnvGuard {
    /// Take the config-home lock and snapshot `names` for restoring on drop.
    pub(crate) fn acquire(names: &[&'static str]) -> Self {
        let _lock = CONFIG_HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let saved = names
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect();
        Self { _lock, saved }
    }
}

#[cfg(test)]
impl Drop for ProcessEnvGuard {
    fn drop(&mut self) {
        for (name, value) in self.saved.drain(..) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

#[cfg(test)]
impl Drop for ConfigHomeGuard {
    fn drop(&mut self) {
        // Restoring here rather than at the end of the test body is what
        // keeps a panicking test from leaking its temporary config home —
        // usually an already-deleted directory — onto the test that runs next.
        match self.previous.take() {
            Some(value) => std::env::set_var(AWMAN_HOME, value),
            None => std::env::remove_var(AWMAN_HOME),
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

    #[test]
    fn home_dir_comes_from_the_snapshot_only() {
        assert!(EnvSnapshot::empty().home_dir().is_none());
        assert!(EnvSnapshot::with_overrides([(HOME, "")])
            .home_dir()
            .is_none());
        assert_eq!(
            EnvSnapshot::with_overrides([(HOME, "/home/a")]).home_dir(),
            Some(PathBuf::from("/home/a"))
        );
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
        let _env = ProcessEnvGuard::acquire(&[XDG_CONFIG_HOME, XDG_DATA_HOME]);
        let cfg_val = "/tmp/awman-test-xdg-cfg-0086";
        let data_val = "/tmp/awman-test-xdg-data-0086";
        std::env::set_var(XDG_CONFIG_HOME, cfg_val);
        std::env::set_var(XDG_DATA_HOME, data_val);
        let snap = Env::from_process();
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

    #[cfg(unix)]
    #[test]
    fn host_var_os_preserves_non_utf8_and_overlay_precedence() {
        use std::os::unix::ffi::OsStringExt;
        let _lock = DAEMON_OVERLAY_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = daemon_overlay_snapshot();
        let name = "AWMAN_TEST_NON_UTF8_OVERRIDE_0121";
        let value = std::ffi::OsString::from_vec(vec![0xff]);
        std::env::set_var(name, &value);
        assert_eq!(host_var_os(name), Some(value));
        update_daemon_overlay(|vars| {
            vars.insert(name.into(), "overlay".into());
        });
        assert_eq!(host_var_os(name), Some("overlay".into()));
        std::env::remove_var(name);
        set_daemon_overlay(previous);
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

    // ── Builtin runtime env (WI 0119) ─────────────────────────────────────

    #[test]
    fn ambient_msb_overrides_are_detected_and_never_forwarded() {
        let env = EnvSnapshot::with_overrides([(MSB_HOME, ""), (MSB_PATH, "/x/msb")]);
        assert_eq!(env.ambient_msb_overrides(), vec![MSB_PATH, MSB_HOME]);
        assert!(EnvSnapshot::empty().ambient_msb_overrides().is_empty());
        for name in AMBIENT_MSB_OVERRIDES {
            assert!(
                !ForwardedEnv::NAMES.contains(name),
                "{name} must not be forwarded"
            );
        }
    }

    #[test]
    fn documented_ambient_msb_override_list_matches_the_constant() {
        let documentation = include_str!("../../../aspec/uxui/cli.md");
        let section = documentation
            .split_once("The builtin runtime rejects ambient ")
            .expect("CLI documentation contains the builtin ambient override paragraph")
            .1
            .split_once("overrides instead of loading")
            .expect("CLI paragraph ends after the override list")
            .0;
        let documented: Vec<&str> = section
            .split('`')
            .filter(|word| word.starts_with("MSB_"))
            .collect();
        assert_eq!(documented, AMBIENT_MSB_OVERRIDES);
    }

    #[test]
    fn all_ambient_msb_names_are_captured_from_process() {
        let _lock = DAEMON_OVERLAY_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous_overlay = daemon_overlay_snapshot();
        set_daemon_overlay(DaemonEnvMap::new());
        let previous: Vec<_> = AMBIENT_MSB_OVERRIDES
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect();
        for &name in AMBIENT_MSB_OVERRIDES {
            std::env::set_var(name, "set");
        }
        assert_eq!(
            Env::from_process().ambient_msb_overrides(),
            AMBIENT_MSB_OVERRIDES
        );
        for (name, value) in previous {
            if let Some(value) = value {
                std::env::set_var(name, value);
            } else {
                std::env::remove_var(name);
            }
        }
        set_daemon_overlay(previous_overlay);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_ambient_msb_path_is_captured_from_process() {
        use std::os::unix::ffi::OsStringExt;
        let _lock = DAEMON_OVERLAY_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous_overlay = daemon_overlay_snapshot();
        set_daemon_overlay(DaemonEnvMap::new());
        let previous = std::env::var_os(MSB_PATH);
        std::env::set_var(MSB_PATH, std::ffi::OsString::from_vec(vec![0xff]));
        assert_eq!(Env::from_process().ambient_msb_overrides(), vec![MSB_PATH]);
        if let Some(value) = previous {
            std::env::set_var(MSB_PATH, value);
        } else {
            std::env::remove_var(MSB_PATH);
        }
        set_daemon_overlay(previous_overlay);
    }

    #[test]
    fn builtin_and_docker_accessors() {
        let env = EnvSnapshot::with_overrides([
            (AWMAN_TEST_BUILTIN, "yes"),
            (DOCKER_HOST, " tcp://h:2376 "),
            (DOCKER_CERT_PATH, "/certs"),
            (DOCKER_TLS_VERIFY, "1"),
        ]);
        assert!(env.test_builtin());
        assert_eq!(env.docker_host().as_deref(), Some("tcp://h:2376"));
        assert_eq!(env.docker_cert_path(), Some(PathBuf::from("/certs")));
        assert!(env.docker_tls_verify());

        let empty = EnvSnapshot::with_overrides([(DOCKER_HOST, ""), (DOCKER_TLS_VERIFY, "")]);
        assert!(!empty.test_builtin());
        assert_eq!(empty.docker_host(), None);
        assert!(!empty.docker_tls_verify());
    }

    // ── ForwardedEnv (WI 0114 F-29) ──────────────────────────────────────

    /// A bearer key must never be serialized into the plist: it is a file on
    /// disk, and the daemon authenticates against the on-disk key *hash*
    /// instead. `PATH` must be forwarded — without it launchd's minimal one
    /// leaves the daemon unable to find `docker`.
    #[test]
    fn no_secret_is_ever_forwarded_to_a_daemon_job() {
        assert!(!ForwardedEnv::NAMES.contains(&"AWMAN_SQUAD_KEY"));
        assert!(ForwardedEnv::NAMES.contains(&"PATH"));
        assert!(ForwardedEnv::NAMES.contains(&AWMAN_HOME));
        // Config variables ride along (WI 0116 §2: overlays included), but a
        // sensitive key never does, and an unknown name forwards nothing.
        assert!(ForwardedEnv::forwards_config_var(AWMAN_OVERLAYS));
        assert!(ForwardedEnv::forwards_config_var(
            "AWMAN_AGENTS_LAUNCH_MODE"
        ));
        assert!(!ForwardedEnv::forwards_config_var(
            "AWMAN_REMOTE_TARGETS_PROD_API_KEY"
        ));
        assert!(!ForwardedEnv::forwards_config_var("AWMAN_NOT_A_KEY"));
        assert!(!ForwardedEnv::forwards_config_var("AWMAN_SQUAD_KEY"));
    }

    #[test]
    fn test_isolation_parses_truthy_values_only() {
        for (value, want) in [
            ("1", true),
            (" TRUE ", true),
            ("yes", true),
            ("on", true),
            ("0", false),
            ("false", false),
            ("memory", false),
            ("", false),
        ] {
            let env = EnvSnapshot::with_overrides([(AWMAN_TEST_ISOLATION, value)]);
            assert_eq!(env.test_isolation(), want, "AWMAN_TEST_ISOLATION={value:?}");
        }
        assert!(!EnvSnapshot::empty().test_isolation());
    }

    /// The countdown override is a test-isolation knob only: without
    /// `AWMAN_TEST_ISOLATION` it is ignored, whatever it holds.
    #[test]
    fn test_yolo_countdown_is_honoured_only_under_test_isolation() {
        use std::time::Duration;
        let isolated = |value: &str| {
            EnvSnapshot::with_overrides([
                (AWMAN_TEST_ISOLATION, "1"),
                (AWMAN_TEST_YOLO_COUNTDOWN_MS, value),
            ])
            .test_yolo_countdown()
        };
        assert_eq!(isolated("250"), Some(Duration::from_millis(250)));
        assert_eq!(isolated(" 0 "), Some(Duration::ZERO));
        assert_eq!(isolated("soon"), None);
        assert_eq!(isolated("-5"), None);
        let normal = EnvSnapshot::with_overrides([(AWMAN_TEST_YOLO_COUNTDOWN_MS, "250")]);
        assert_eq!(normal.test_yolo_countdown(), None);
        assert_eq!(EnvSnapshot::empty().test_yolo_countdown(), None);
    }

    /// Unit tests are isolated whatever the environment says: they cannot be
    /// run in a way that reaches the developer's keychain or service manager.
    #[test]
    fn unit_tests_are_always_isolated() {
        assert!(test_isolation_active());
    }

    /// The fixed names come first, in `ForwardedEnv::NAMES`'s own order and only
    /// when set; known, non-sensitive config variables follow, sorted by name.
    #[test]
    fn forwarded_env_lists_set_names_in_order_then_config_vars() {
        const LAUNCH: &str = "AWMAN_AGENTS_LAUNCH_MODE";
        const API_KEY: &str = "AWMAN_REMOTE_TARGETS_FWD_API_KEY";
        let _env = ProcessEnvGuard::acquire(&[AWMAN_HOME, AWMAN_OVERLAYS, LAUNCH, API_KEY]);
        std::env::set_var(AWMAN_HOME, "/tmp/awman-forwarded-0139");
        std::env::remove_var(AWMAN_OVERLAYS);
        std::env::set_var(LAUNCH, "acp");
        std::env::set_var(API_KEY, "secret");

        let result = ForwardedEnv::from_process();
        let names: Vec<&str> = result.iter().map(|(n, _)| n.as_str()).collect();
        assert!(!names.contains(&AWMAN_OVERLAYS), "unset: {names:?}");
        assert!(!names.contains(&API_KEY), "sensitive: {names:?}");
        assert!(result.iter().any(|(n, v)| n == LAUNCH && v == "acp"));
        let fixed: Vec<&str> = names
            .iter()
            .copied()
            .filter(|n| ForwardedEnv::NAMES.contains(n))
            .collect();
        let expected: Vec<&str> = ForwardedEnv::NAMES
            .iter()
            .copied()
            .filter(|n| names.contains(n))
            .collect();
        assert_eq!(fixed, expected);
        let first_config = names.iter().position(|n| *n == LAUNCH).unwrap();
        assert!(names[..first_config]
            .iter()
            .all(|n| ForwardedEnv::NAMES.contains(n)));
    }
}

/// The override channel: derived `AWMAN_*` names and `--set`.
#[cfg(test)]
mod override_tests {
    use super::*;
    use crate::data::config::fields::{ConfigKey, CONFIG_KEYS};
    use serde_json::json;

    /// The hand-named variables earlier releases read. They are unknown now.
    const REMOVED: &[&str] = &[
        "AWMAN_MAX_CONCURRENT_AGENTS",
        "AWMAN_LAUNCH_MODE",
        "AWMAN_REMOTE_ADDR",
        "AWMAN_API_KEY",
        "AWMAN_BUILTIN_STATE_DIR",
        "AWMAN_API_VERBOSE_SETUP",
        "AWMAN_CONFIG_HOME",
        "AWMAN_API_ROOT",
        "AWMAN_SQUAD_ROOT",
        "AWMAN_ATTACH_DIR",
    ];

    fn no_names(_: &str) -> Vec<String> {
        Vec::new()
    }

    fn set(args: &[&str]) -> Result<ConfigOverrides, KeyError> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        ConfigOverrides::parse_set_args(&args)
    }

    fn one(args: &[&str]) -> (String, Value) {
        let overrides = set(args).unwrap();
        assert_eq!(overrides.0.len(), 1, "{overrides:?}");
        overrides.0.into_iter().next().unwrap()
    }

    /// Every derived name (map rows with a sample name too) is unique and is
    /// neither a process variable nor in the test namespace, and the key
    /// table's reserved list is exactly this module's process-variable list.
    #[test]
    fn derived_names_never_collide_with_a_process_variable() {
        let mut reserved: Vec<&str> = RESERVED_ENV.to_vec();
        let mut process: Vec<&str> = PROCESS_VARS.to_vec();
        reserved.sort_unstable();
        process.sort_unstable();
        assert_eq!(
            reserved, process,
            "fields::RESERVED_ENV must equal env::PROCESS_VARS"
        );

        let mut seen = std::collections::HashSet::new();
        for key in CONFIG_KEYS.iter().filter(|k| k.has_env()) {
            let samples = if key.is_map_member() {
                vec![key.env_name(&["prod"]), key.env_name(&["in-fact"])]
            } else {
                vec![key.env_pattern()]
            };
            for name in samples {
                assert!(seen.insert(name.clone()), "{name} is derived twice");
                assert!(
                    !PROCESS_VARS.contains(&name.as_str()),
                    "{name} is a process variable"
                );
                assert!(
                    !name.starts_with(AWMAN_TEST_PREFIX),
                    "{name} is in the test namespace"
                );
                assert!(is_config_var_name(&name), "{name} must read as config");
                let resolved = fields::resolve_env(&name, &no_names)
                    .unwrap_or_else(|| panic!("{name} must resolve"));
                assert_eq!(
                    resolved.key.path, key.path,
                    "{name} resolves to the wrong row"
                );
            }
        }
        for name in PROCESS_VARS {
            assert!(!is_config_var_name(name), "{name} is a process variable");
            assert!(
                fields::resolve_env(name, &no_names).is_none(),
                "{name} resolved"
            );
        }
    }

    /// Names are derived mechanically: dots and camel humps become `_`, `-` in
    /// a map name becomes `_`. `AWMAN_OVERLAYS` keeps its name because the
    /// derivation gives exactly that.
    #[test]
    fn derived_names_follow_the_path() {
        let name = |path: &str| fields::resolve(path).unwrap().key.env_pattern();
        assert_eq!(name("overlays"), AWMAN_OVERLAYS);
        assert_eq!(
            name("workflows.maxConcurrentSteps"),
            "AWMAN_WORKFLOWS_MAX_CONCURRENT_STEPS"
        );
        assert_eq!(name("agents.launchMode"), "AWMAN_AGENTS_LAUNCH_MODE");
        assert_eq!(
            name("runtime.builtin.stateDir"),
            "AWMAN_RUNTIME_BUILTIN_STATE_DIR"
        );
        let key: &ConfigKey = fields::resolve("remote.targets.in-fact.apiKey")
            .unwrap()
            .key;
        assert_eq!(
            key.env_name(&["in-fact"]),
            "AWMAN_REMOTE_TARGETS_IN_FACT_API_KEY"
        );
    }

    #[test]
    fn removed_hand_named_variables_are_not_config() {
        for name in REMOVED {
            assert!(
                fields::resolve_env(name, &no_names).is_none(),
                "{name} must not resolve to a config key"
            );
        }
        let env = EnvSnapshot::with_overrides(REMOVED.iter().map(|n| (*n, "7")));
        let overrides = ConfigOverrides::from_env(&env, no_names).unwrap();
        assert!(overrides.is_empty(), "removed names applied: {overrides:?}");
    }

    #[test]
    fn config_vars_skip_process_and_test_variables() {
        let env = EnvSnapshot::with_overrides([
            (AWMAN_HOME, "/h"),
            (AWMAN_REMOTE_SESSION, "s"),
            (AWMAN_TEST_ISOLATION, "1"),
            ("AWMAN_TEST_SOMETHING_ADHOC", "1"),
            ("AWMAN_TUI_SCROLLBACK_LINES", "5"),
            ("AWMAN_AGENTS_LAUNCH_MODE", "acp"),
            ("HOME", "/home/a"),
        ]);
        assert_eq!(
            env.config_vars(),
            vec![
                ("AWMAN_AGENTS_LAUNCH_MODE".to_string(), "acp".to_string()),
                ("AWMAN_TUI_SCROLLBACK_LINES".to_string(), "5".to_string()),
            ]
        );
        assert_eq!(env.awman_home(), Some(PathBuf::from("/h")));
        assert_eq!(
            EnvSnapshot::with_overrides([(AWMAN_HOME, " ")]).awman_home(),
            None
        );
    }

    #[test]
    fn env_values_are_parsed_by_the_key_type_and_bind_known_map_names() {
        let env = EnvSnapshot::with_overrides([
            ("AWMAN_WORKFLOWS_MAX_CONCURRENT_STEPS", "3"),
            ("AWMAN_OVERLAYS", "ssh(), env(TOKEN)"),
            ("AWMAN_WORK_SERVERS_IN_FACT_URL", "https://h:1"),
            ("AWMAN_AGENTS_FLAGS_CODEX", "--a,--b"),
            ("AWMAN_TUI_BACKEND", ""),
        ]);
        let known = |section: &str| match section {
            "workServers" => vec!["in-fact".to_string()],
            _ => Vec::new(),
        };
        let overrides = ConfigOverrides::from_env(&env, known).unwrap();
        let mut pairs = overrides.0.clone();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            pairs,
            vec![
                ("agents.flags.codex".to_string(), json!(["--a", "--b"])),
                ("overlays".to_string(), json!(["ssh()", "env(TOKEN)"])),
                ("workServers.in-fact.url".to_string(), json!("https://h:1")),
                ("workflows.maxConcurrentSteps".to_string(), json!(3)),
            ],
            "an empty value is unset; a map name binds to the file's spelling"
        );
    }

    #[test]
    fn a_bad_env_value_is_an_error_naming_the_variable() {
        let env = EnvSnapshot::with_overrides([("AWMAN_AGENTS_LAUNCH_MODE", "banana")]);
        let err = ConfigOverrides::from_env(&env, no_names)
            .unwrap_err()
            .to_string();
        assert!(err.contains("AWMAN_AGENTS_LAUNCH_MODE"), "{err}");
        assert!(err.contains("agents.launchMode"), "{err}");
    }

    // ── --set parsing, one case per value type ────────────────────────────

    #[test]
    fn set_parses_every_value_type() {
        assert_eq!(one(&["agents.auth.refresh.enabled=false"]).1, json!(false));
        assert_eq!(one(&["tui.scrollbackLines=500"]).1, json!(500));
        assert_eq!(
            one(&["runtime.baseImage=ghcr.io/x/y:1"]).1,
            json!("ghcr.io/x/y:1")
        );
        assert_eq!(one(&["agents.launchMode=acp"]).1, json!("acp"));
        assert_eq!(one(&["agents.stuckTimeout=45s"]).1, json!("45s"));
        assert_eq!(one(&["runtime.builtin.memory=8GiB"]).1, json!("8GiB"));
        assert_eq!(
            one(&["agents.yolo.disallowedTools=Bash, WebFetch"]).1,
            json!(["Bash", "WebFetch"])
        );
        assert_eq!(
            one(&["runtime.builtin.network.hostPorts=8080,9090"]).1,
            json!([8080, 9090])
        );
        assert_eq!(
            one(&["workServers.prod.labels=os=linux,gpu=a100"]),
            (
                "workServers.prod.labels".to_string(),
                json!({"os": "linux", "gpu": "a100"})
            )
        );
        // A value may itself contain `=`: only the first one splits.
        assert_eq!(
            one(&["workItems.template=a=b"]).1,
            json!("a=b"),
            "only the first `=` separates key from value"
        );
        // `null` resets the key to its default.
        assert_eq!(one(&["agents.stuckTimeout=null"]).1, Value::Null);
    }

    #[test]
    fn set_rejects_bad_values_for_every_value_type() {
        for arg in [
            "agents.auth.refresh.enabled=maybe",
            "tui.scrollbackLines=lots",
            "tui.scrollbackLines=-1",
            "agents.launchMode=banana",
            "agents.stuckTimeout=soon",
            "runtime.builtin.memory=lots",
            "runtime.builtin.network.hostPorts=80,http",
            "workServers.prod.labels=novalue",
        ] {
            let err = set(&[arg]).unwrap_err();
            assert!(matches!(err, KeyError::Value { .. }), "{arg}: {err}");
        }
        let err = set(&["runtime.builtin.imageSource=x"]).unwrap_err();
        assert!(matches!(err, KeyError::ObjectNotSettable { .. }), "{err}");
    }

    #[test]
    fn set_rejects_unknown_keys_with_the_key_table_error() {
        let err = set(&["maxConcurrentAgents=2"]).unwrap_err();
        assert!(matches!(err, KeyError::Unknown { .. }), "{err}");
        let text = err.to_string();
        assert!(
            text.contains("unknown config key `maxConcurrentAgents`"),
            "{text}"
        );
        assert!(text.contains("workflows.maxConcurrentSteps"), "{text}");

        let err = set(&["agents.nope=1"]).unwrap_err().to_string();
        assert!(
            err.contains("valid keys here:") && err.contains("launchMode"),
            "{err}"
        );

        let err = set(&["agents.launchMode"]).unwrap_err().to_string();
        assert!(err.contains("expected --set <key>=<value>"), "{err}");

        let err = set(&["workServers.Bad.url=x"]).unwrap_err();
        assert!(matches!(err, KeyError::Name { .. }), "{err}");
    }

    #[test]
    fn set_keeps_command_line_order_so_the_last_value_wins() {
        let overrides = set(&["tui.scrollbackLines=1", "tui.scrollbackLines=2"]).unwrap();
        assert_eq!(overrides.0.last().unwrap().1, json!(2));
    }

    // ── Precedence: default → files → env → --set ─────────────────────────

    fn effective(
        home: &std::path::Path,
        env: &[(&str, &str)],
        set_args: &[&str],
    ) -> crate::data::config::effective::EffectiveConfig {
        use crate::data::config::effective::{ConfigLayers, EffectiveConfig};
        let mut vars: Vec<(String, String)> =
            vec![(AWMAN_HOME.to_string(), home.to_string_lossy().into_owned())];
        vars.extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let env = EnvSnapshot::with_overrides(vars);
        let layers = ConfigLayers::load(&env, None, set(set_args).unwrap()).unwrap();
        EffectiveConfig::resolve(layers, Default::default(), env).unwrap()
    }

    #[test]
    fn env_beats_files_and_set_beats_env() {
        use crate::data::config::effective::Layer;
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.json"),
            r#"{"agents":{"stuckTimeout":"10s"},"workflows":{"maxConcurrentSteps":4}}"#,
        )
        .unwrap();
        let path = "agents.stuckTimeout";
        let secs = |ec: &crate::data::config::effective::EffectiveConfig| {
            ec.agent_stuck_timeout().as_secs()
        };

        let ec = effective(home.path(), &[], &[]);
        assert_eq!((secs(&ec), ec.source_of(path)), (10, Layer::Global));

        let env = [("AWMAN_AGENTS_STUCK_TIMEOUT", "20s")];
        let ec = effective(home.path(), &env, &[]);
        assert_eq!((secs(&ec), ec.source_of(path)), (20, Layer::Env));

        let ec = effective(home.path(), &env, &["agents.stuckTimeout=30s"]);
        assert_eq!((secs(&ec), ec.source_of(path)), (30, Layer::Set));

        // `--set key=null` resets to the built-in default even over env.
        let ec = effective(home.path(), &env, &["agents.stuckTimeout=null"]);
        assert_eq!(secs(&ec), 30, "the table default is 30s");

        // A removed hand-named variable changes nothing.
        let ec = effective(home.path(), &[("AWMAN_MAX_CONCURRENT_AGENTS", "1")], &[]);
        assert_eq!(ec.max_concurrent_steps(), Some(4));
        let ec = effective(
            home.path(),
            &[("AWMAN_WORKFLOWS_MAX_CONCURRENT_STEPS", "2")],
            &["workflows.maxConcurrentSteps=1"],
        );
        assert_eq!(ec.max_concurrent_steps(), Some(1));
    }
}

#[cfg(test)]
mod published_key_tests {
    use super::*;

    /// Snapshots take process configuration only: nothing *but* the published key is resolved through
    /// the overlay, so a daemon client cannot redirect this process's storage
    /// root, or change its config, by pushing an `env(VAR)` payload named
    /// like one of awman's own variables.
    #[test]
    fn the_overlay_cannot_redirect_a_storage_root_or_set_config() {
        const CONFIG_VAR: &str = "AWMAN_AGENTS_LAUNCH_MODE";
        let _lock = DAEMON_OVERLAY_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = daemon_overlay_snapshot();
        let real_root = std::env::var(AWMAN_HOME).ok();
        let real_config = std::env::var(CONFIG_VAR).ok();

        update_daemon_overlay(|vars| {
            vars.insert(AWMAN_HOME.to_string(), "/attacker/root".to_string());
            vars.insert(CONFIG_VAR.to_string(), "acp".to_string());
        });

        let snapshot = Env::from_process();
        assert_eq!(snapshot.get(AWMAN_HOME), real_root.as_deref());
        assert_eq!(snapshot.get(CONFIG_VAR), real_config.as_deref());

        set_daemon_overlay(previous);
    }
}
