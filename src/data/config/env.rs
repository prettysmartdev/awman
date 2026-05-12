//! Typed reads of every environment variable amux honours.
//!
//! Reads are funnelled through `Env` so that no scattered `std::env::var(…)`
//! calls leak elsewhere in the data layer.

use std::collections::HashMap;
use std::path::PathBuf;

/// `AMUX_CONFIG_HOME` — overrides the global config home directory.
pub const AMUX_CONFIG_HOME: &str = "AMUX_CONFIG_HOME";

/// `AMUX_HEADLESS_ROOT` — overrides the headless storage root directory.
pub const AMUX_HEADLESS_ROOT: &str = "AMUX_HEADLESS_ROOT";

/// `AMUX_OVERLAYS` — comma-separated list of overlay specs.
pub const AMUX_OVERLAYS: &str = "AMUX_OVERLAYS";

/// `AMUX_REMOTE_ADDR` — overrides remote server address.
pub const AMUX_REMOTE_ADDR: &str = "AMUX_REMOTE_ADDR";

/// `AMUX_REMOTE_SESSION` — sticky session id for remote operations.
pub const AMUX_REMOTE_SESSION: &str = "AMUX_REMOTE_SESSION";

/// `AMUX_API_KEY` — API key for the remote headless server.
pub const AMUX_API_KEY: &str = "AMUX_API_KEY";

/// Frozen snapshot of every env var amux reads.
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

    /// `AMUX_CONFIG_HOME` as a `PathBuf` if set.
    pub fn config_home(&self) -> Option<PathBuf> {
        self.get(AMUX_CONFIG_HOME).map(PathBuf::from)
    }

    /// `AMUX_HEADLESS_ROOT` as a `PathBuf` if set.
    pub fn headless_root(&self) -> Option<PathBuf> {
        self.get(AMUX_HEADLESS_ROOT).map(PathBuf::from)
    }

    /// `AMUX_OVERLAYS` raw string if set.
    pub fn overlays(&self) -> Option<&str> {
        self.get(AMUX_OVERLAYS)
    }

    /// `AMUX_REMOTE_ADDR` if set.
    pub fn remote_addr(&self) -> Option<&str> {
        self.get(AMUX_REMOTE_ADDR)
    }

    /// `AMUX_REMOTE_SESSION` if set.
    pub fn remote_session(&self) -> Option<&str> {
        self.get(AMUX_REMOTE_SESSION)
    }

    /// `AMUX_API_KEY` if set.
    pub fn api_key(&self) -> Option<&str> {
        self.get(AMUX_API_KEY)
    }
}

/// Namespace for capturing process-environment snapshots.
pub struct Env;

impl Env {
    /// Capture every amux-relevant env var from the current process.
    ///
    /// Reads are limited to the known constants above so that the snapshot
    /// is deterministic and minimal.
    pub fn from_process() -> EnvSnapshot {
        let keys = [
            AMUX_CONFIG_HOME,
            AMUX_HEADLESS_ROOT,
            AMUX_OVERLAYS,
            AMUX_REMOTE_ADDR,
            AMUX_REMOTE_SESSION,
            AMUX_API_KEY,
        ];
        let mut values = HashMap::new();
        for k in keys {
            if let Ok(v) = std::env::var(k) {
                values.insert(k.to_string(), v);
            }
        }
        EnvSnapshot { values }
    }
}
