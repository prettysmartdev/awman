//! Settings fixed for the lifetime of a runtime graph. Comparing these values
//! never opens a runtime, reads configuration files, or allocates session state.

use std::path::{Path, PathBuf};

use super::{AuthRefreshSettings, EffectiveConfig};
use crate::data::config::{runtime::RuntimeKind, ConfigFile};
use crate::data::error::DataError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessBinding {
    pub runtime_kind: RuntimeKind,
    pub builtin_state_dir: Option<PathBuf>,
    pub auth_refresh: AuthRefreshSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSettingDifference {
    pub key: String,
    pub bound: String,
    pub requested: String,
}

impl ProcessBinding {
    pub const KEYS: &'static [&'static str] = &[
        "runtime.kind",
        "runtime.builtin.stateDir",
        "agents.auth.refresh.enabled",
        "agents.auth.refresh.threshold",
        "agents.auth.refresh.tick",
    ];
    pub fn from_config(config: &EffectiveConfig) -> Result<Self, DataError> {
        let runtime_kind = config
            .runtime_selection()
            .map_err(|e| DataError::Other(format!("unknown runtime: {}", e.0)))?;
        Ok(Self {
            runtime_kind,
            builtin_state_dir: if runtime_kind == RuntimeKind::BuiltinExperimental {
                Some(config.builtin_state_dir()?)
            } else {
                None
            },
            auth_refresh: config.auth_refresh(),
        })
    }

    /// Read the immutable process snapshot retained by `Engines`. Its builtin
    /// state directory has already been resolved, including the default.
    pub fn from_snapshot(config: &ConfigFile) -> Self {
        let runtime_kind = config
            .runtime
            .as_ref()
            .and_then(|r| r.kind)
            .unwrap_or_default();
        Self {
            runtime_kind,
            builtin_state_dir: if runtime_kind == RuntimeKind::BuiltinExperimental {
                config
                    .runtime
                    .as_ref()
                    .and_then(|r| r.builtin.as_ref())
                    .and_then(|b| b.state_dir.clone())
            } else {
                None
            },
            auth_refresh: AuthRefreshSettings::from_config(config),
        }
    }

    /// Preserve the global view for existing consumers, with the exact process
    /// values used by runtime/monitor construction. No second resolution occurs.
    pub fn snapshot(&self, config: &EffectiveConfig) -> ConfigFile {
        let mut snapshot = config.layers().global.clone();
        let runtime = snapshot.runtime.get_or_insert_with(Default::default);
        runtime.kind = Some(self.runtime_kind);
        if let Some(dir) = &self.builtin_state_dir {
            runtime
                .builtin
                .get_or_insert_with(Default::default)
                .state_dir = Some(dir.clone());
        }
        snapshot
            .agents
            .get_or_insert_with(Default::default)
            .auth
            .get_or_insert_with(Default::default)
            .refresh = config
            .config()
            .agents
            .as_ref()
            .and_then(|a| a.auth.as_ref())
            .and_then(|a| a.refresh.clone());
        snapshot
    }

    pub fn difference(&self, requested: &Self) -> Option<ProcessSettingDifference> {
        let difference = |key: &str, bound: String, requested: String| {
            Some(ProcessSettingDifference {
                key: key.into(),
                bound,
                requested,
            })
        };
        if self.runtime_kind != requested.runtime_kind {
            return difference(
                "runtime.kind",
                self.runtime_kind.to_string(),
                requested.runtime_kind.to_string(),
            );
        }
        if !same_directory(
            self.builtin_state_dir.as_deref(),
            requested.builtin_state_dir.as_deref(),
        ) {
            return difference(
                "runtime.builtin.stateDir",
                display_dir(&self.builtin_state_dir),
                display_dir(&requested.builtin_state_dir),
            );
        }
        if self.auth_refresh.enabled != requested.auth_refresh.enabled {
            return difference(
                "agents.auth.refresh.enabled",
                self.auth_refresh.enabled.to_string(),
                requested.auth_refresh.enabled.to_string(),
            );
        }
        if self.auth_refresh.threshold != requested.auth_refresh.threshold {
            return difference(
                "agents.auth.refresh.threshold",
                duration(self.auth_refresh.threshold),
                duration(requested.auth_refresh.threshold),
            );
        }
        if self.auth_refresh.tick != requested.auth_refresh.tick {
            return difference(
                "agents.auth.refresh.tick",
                duration(self.auth_refresh.tick),
                duration(requested.auth_refresh.tick),
            );
        }
        None
    }
}

fn duration(value: std::time::Duration) -> String {
    format!("{}ms", value.as_millis())
}

fn display_dir(dir: &Option<PathBuf>) -> String {
    dir.as_ref()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| "null".into())
}

fn same_directory(a: Option<&Path>, b: Option<&Path>) -> bool {
    a == b
        || match (a, b) {
            (Some(a), Some(b)) => match (a.canonicalize(), b.canonicalize()) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            },
            _ => false,
        }
}
