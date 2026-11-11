//! `ConfigFile` — the one shape of every config file (global, repo, squad
//! task).
//!
//! Every field is optional: absent means "not set in this file". Parsing is
//! table-driven first ([`fields::validate_document`] produces every unknown
//! key, scope, name and value error), then serde (`deny_unknown_fields` on
//! every struct is a second line of defence, never the source of the error
//! text), then semantic checks the table cannot express.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::data::config::agents::AgentsConfig;
use crate::data::config::daemon::DaemonConfig;
use crate::data::config::env::EnvSnapshot;
use crate::data::config::fields::{self, KeyError};
use crate::data::config::remote::RemoteConfig;
use crate::data::config::runtime::RuntimeConfig;
use crate::data::config::tui::TuiConfig;
use crate::data::config::work_items::WorkItemsConfig;
use crate::data::config::work_servers::WorkServerConfig;
use crate::data::config::workflows::WorkflowsConfig;
use crate::data::config::worktree::WorktreeConfig;
use crate::data::error::DataError;
use crate::data::fs::SquadPaths;

pub use crate::data::config::fields::FileKind;

/// Filename of every config file.
pub const CONFIG_FILENAME: &str = "config.json";

/// Subdirectory under the git root holding the repo config.
pub const REPO_CONFIG_SUBDIR: &str = ".awman";

/// The one shape of every config file.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<AgentsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlays: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_items: Option<WorkItemsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflows: Option<WorkflowsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui: Option<TuiConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon: Option<DaemonConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_servers: Option<BTreeMap<String, WorkServerConfig>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteConfig>,
}

/// A key-table error as a `DataError`, naming the file.
pub fn key_error(error: KeyError, path: &Path) -> DataError {
    match error {
        e @ KeyError::Unknown { .. } => DataError::Other(e.in_file(path).to_string()),
        other => DataError::Other(format!("{}: {other}", path.display())),
    }
}

impl ConfigFile {
    /// Table-driven validation (unknown keys, scope, names, values), then
    /// serde, then semantic checks. `path` names the file in errors.
    pub fn parse(text: &str, kind: FileKind, path: &Path) -> Result<Self, DataError> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| DataError::config_parse(path, e))?;
        Self::from_value_at(value, kind, path)
    }

    /// Load a file; a missing file is an empty (default) config.
    pub fn load(path: &Path, kind: FileKind) -> Result<Self, DataError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, kind, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(DataError::io(path, e)),
        }
    }

    /// Validate as [`ConfigFile::parse`] would, then write. Nothing is
    /// written when validation fails. Repo files are written without
    /// following symlinks below the git root.
    pub fn save(&self, path: &Path, kind: FileKind) -> Result<(), DataError> {
        Self::from_value_at(self.to_value(), kind, path)?;
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| DataError::ConfigSerialize { source: e })?;
        match kind {
            FileKind::Repo => {
                // `<git_root>/.awman/config.json`: confine the write to the root.
                let root = path
                    .parent()
                    .and_then(Path::parent)
                    .unwrap_or_else(|| Path::new("."));
                crate::data::fs::config_file::write_config_file_within(root, path, &content)
            }
            FileKind::Global | FileKind::Task => {
                crate::data::fs::config_file::write_user_config_file(path, &content)
            }
        }
    }

    /// Raw JSON view (absent fields omitted) for merging and provenance.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("ConfigFile serializes")
    }

    /// Validate and convert a JSON document of `kind`.
    pub fn from_value(value: serde_json::Value, kind: FileKind) -> Result<Self, DataError> {
        Self::from_value_at(value, kind, Path::new("<config>"))
    }

    fn from_value_at(
        value: serde_json::Value,
        kind: FileKind,
        path: &Path,
    ) -> Result<Self, DataError> {
        fields::validate_document(&value, kind).map_err(|e| key_error(e, path))?;
        let file: Self =
            serde_json::from_value(value).map_err(|e| DataError::config_parse(path, e))?;
        file.validate_semantics()
            .map_err(|e| DataError::Other(format!("{}: {e}", path.display())))?;
        Ok(file)
    }

    /// Checks that need more than one leaf or a domain rule: agent-flag
    /// entries, leader syntax, guidance entries, prerequisite values, builtin
    /// resources and image sources, required URLs of map entries.
    pub fn validate_semantics(&self) -> Result<(), DataError> {
        if let Some(agents) = &self.agents {
            agents.validate()?;
        }
        if let Some(workflows) = &self.workflows {
            if let Some(dynamic) = &workflows.dynamic {
                dynamic.validate("workflows.dynamic")?;
            }
            if let Some(prerequisites) = &workflows.prerequisites {
                prerequisites.validate(crate::data::config::workflows::ConfigScope::Global)?;
            }
        }
        if let Some(builtin) = self.runtime.as_ref().and_then(|r| r.builtin.as_ref()) {
            builtin.validate().map_err(DataError::BuiltinConfig)?;
            if let Some(dir) = &builtin.state_dir {
                if !dir.is_absolute() {
                    return Err(DataError::Other(format!(
                        "runtime.builtin.stateDir must be an absolute path, got {}",
                        dir.display()
                    )));
                }
            }
            if let Some(network) = &builtin.network {
                network
                    .validate()
                    .map_err(|e| DataError::Other(format!("runtime.builtin.network: {e}")))?;
            }
        }
        for (name, server) in self.work_servers.iter().flatten() {
            if server.url.as_deref().is_none_or(|u| u.trim().is_empty()) {
                return Err(DataError::Other(format!(
                    "workServers.{name}.url is required"
                )));
            }
            if let Some(labels) = &server.labels {
                for key in labels.keys() {
                    if let Err(reason) =
                        crate::data::config::work_servers::validate_configured_label(key)
                    {
                        return Err(DataError::Other(format!(
                            "workServers.{name}.labels.{key} {reason}"
                        )));
                    }
                }
            }
        }
        if let Some(remote) = &self.remote {
            for (name, target) in remote.targets.iter().flatten() {
                if target.url.as_deref().is_none_or(|u| u.trim().is_empty()) {
                    return Err(DataError::Other(format!(
                        "remote.targets.{name}.url is required"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// The global config file: `<config home>/config.json`.
pub fn global_config_path(env: &EnvSnapshot) -> Result<PathBuf, DataError> {
    Ok(crate::data::fs::data_paths::config_home(env)?.join(CONFIG_FILENAME))
}

/// The repo config file: `<git_root>/.awman/config.json`.
pub fn repo_config_path(git_root: &Path) -> PathBuf {
    git_root.join(REPO_CONFIG_SUBDIR).join(CONFIG_FILENAME)
}

/// A squad task's config file: `<squad root>/tasks/<name>/config.json`.
pub fn task_config_path(squad: &SquadPaths, task: &str) -> Result<PathBuf, DataError> {
    squad.task_config_file(task)
}
