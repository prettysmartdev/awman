//! Typed reads of every environment variable awman honours.
//!
//! Reads are funnelled through `Env` so that no scattered `std::env::var(…)`
//! calls leak elsewhere in the data layer.

use std::collections::HashMap;
use std::path::PathBuf;

/// `AWMAN_CONFIG_HOME` — overrides the global config home directory.
pub const AWMAN_CONFIG_HOME: &str = "AWMAN_CONFIG_HOME";

/// `AWMAN_API_ROOT` — overrides the API storage root directory.
pub const AWMAN_API_ROOT: &str = "AWMAN_API_ROOT";

/// `AWMAN_OVERLAYS` — comma-separated list of overlay specs.
pub const AWMAN_OVERLAYS: &str = "AWMAN_OVERLAYS";

/// `AWMAN_REMOTE_ADDR` — overrides remote server address.
pub const AWMAN_REMOTE_ADDR: &str = "AWMAN_REMOTE_ADDR";

/// `AWMAN_REMOTE_SESSION` — sticky session id for remote operations.
pub const AWMAN_REMOTE_SESSION: &str = "AWMAN_REMOTE_SESSION";

/// `AWMAN_API_KEY` — API key for the remote API server.
pub const AWMAN_API_KEY: &str = "AWMAN_API_KEY";

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
}

/// Namespace for capturing process-environment snapshots.
pub struct Env;

impl Env {
    /// Capture every awman-relevant env var from the current process.
    ///
    /// Reads are limited to the known constants above so that the snapshot
    /// is deterministic and minimal.
    pub fn from_process() -> EnvSnapshot {
        let keys = [
            AWMAN_CONFIG_HOME,
            AWMAN_API_ROOT,
            AWMAN_OVERLAYS,
            AWMAN_REMOTE_ADDR,
            AWMAN_REMOTE_SESSION,
            AWMAN_API_KEY,
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
