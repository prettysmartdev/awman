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

#[cfg(test)]
impl ConfigFile {
    /// A config file from JSON, for tests. Panics on a document serde
    /// rejects (the key-table walker is not run).
    pub(crate) fn test_json(value: serde_json::Value) -> Self {
        serde_json::from_value(value).expect("valid test config")
    }

    /// A global file selecting `runtime.kind`.
    pub(crate) fn with_runtime(kind: &str) -> Self {
        Self::test_json(serde_json::json!({"runtime": {"kind": kind}}))
    }
}

#[cfg(test)]
impl ConfigFile {
    /// Save as `<git_root>/.awman/config.json`, for tests.
    pub(crate) fn save_repo(&self, git_root: &Path) -> Result<(), DataError> {
        self.save(&repo_config_path(git_root), FileKind::Repo)
    }

    /// Save as the global file `env` resolves, for tests.
    pub(crate) fn save_global(&self, env: &EnvSnapshot) -> Result<(), DataError> {
        self.save(&global_config_path(env)?, FileKind::Global)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::fields::{ValueKind, CONFIG_KEYS};

    #[test]
    fn work_server_config_rejects_reserved_reported_labels() {
        let config = serde_json::json!({"workServers":{"team":{"url":"https://example.com","labels":{"awman/os":"fake"}}}});
        let error = ConfigFile::from_value(config, FileKind::Global)
            .unwrap_err()
            .to_string();
        assert!(error.contains("reserved awman/ namespace"), "{error}");
    }

    /// A sample value for a row, used to build one document carrying every
    /// leaf.
    fn sample(key: &fields::ConfigKey) -> serde_json::Value {
        use serde_json::json;
        match key.path {
            "daemon.network.features" => return json!(["worker"]),
            "daemon.network.listen" => return json!("127.0.0.1:9876"),
            "runtime.builtin.stateDir" => return json!("/state"),
            "runtime.builtin.imageSource" | "runtime.builtin.images.*" => {
                return json!({"type": "registry", "registry": "localhost:5000"});
            }
            "runtime.builtin.registries.*" => return json!({"insecure": false}),
            "runtime.builtin.network.mode" => return json!("allowlist"),
            "runtime.builtin.network.allow" => return json!(["example.com"]),
            "runtime.builtin.network.hostPorts" => return json!([8080]),
            "runtime.builtin.network.nameservers" => return json!(["1.1.1.1"]),
            "workflows.dynamic.defaultLeader" => return json!("claude::opus"),
            "workflows.prerequisites.gitIdentity.email" => return json!("a@b.c"),
            "workflows.prerequisites.githubToken.envVars" => return json!(["GH_TOKEN"]),
            "workflows.prerequisites.githubToken.acquire.argv" => {
                return json!(["gh", "auth", "token"]);
            }
            "agents.flags.*" | "workflows.dynamic.agentsToModels.*" => return json!(["x"]),
            "workServers.*.labels" => return json!({"zone": "eu"}),
            _ => {}
        }
        match key.kind {
            ValueKind::Bool => json!(true),
            ValueKind::Int { min, .. } => json!(min.unwrap_or(0).max(1)),
            ValueKind::String => json!("value"),
            ValueKind::Enum(values) => json!(values[values.len() - 1]),
            ValueKind::Duration => json!("5s"),
            ValueKind::Size => json!("1GiB"),
            ValueKind::StringList => json!(["a"]),
            ValueKind::IntList => json!([1]),
            ValueKind::StringMap => json!({"k": "v"}),
            ValueKind::Object => json!({}),
        }
    }

    fn insert(doc: &mut serde_json::Value, path: &str, value: serde_json::Value) {
        let mut current = doc;
        let parts: Vec<&str> = path.split('.').collect();
        for (i, part) in parts.iter().enumerate() {
            let part = if *part == "*" { "x" } else { part };
            if i + 1 == parts.len() {
                current[part] = value.clone();
            } else {
                if current.get(part).is_none() {
                    current[part] = serde_json::json!({});
                }
                current = current.get_mut(part).unwrap();
            }
        }
    }

    #[test]
    fn key_table_matches_config_file_leaves() {
        // Every row, populated at once, passes the table walker and serde,
        // and survives a round trip through the typed struct unchanged.
        let mut doc = serde_json::json!({});
        for key in CONFIG_KEYS {
            insert(&mut doc, key.path, sample(key));
        }
        fields::validate_document(&doc, FileKind::Global).expect("table accepts every leaf");
        let file: ConfigFile = serde_json::from_value(doc.clone())
            .unwrap_or_else(|e| panic!("struct rejects a table leaf: {e}"));
        assert_eq!(file.to_value(), doc, "a table leaf has no struct field");
        // And the struct has no field the table lacks: an unknown struct
        // field would serialize to a path the walker rejects.
        fields::validate_document(&file.to_value(), FileKind::Global).unwrap();
    }

    #[test]
    fn parse_names_the_file_and_rejects_old_keys() {
        let path = Path::new("/repo/.awman/config.json");
        let err = ConfigFile::parse(r#"{"agentStuckTimeout": 30}"#, FileKind::Repo, path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("/repo/.awman/config.json"), "{err}");
        assert!(err.contains("agents.stuckTimeout"), "{err}");
    }

    #[test]
    fn semantic_errors_name_the_key() {
        let path = Path::new("/g/config.json");
        for (text, needle) in [
            (
                r#"{"agents":{"flags":{"codex":[" "]}}}"#,
                "agents.flags.codex",
            ),
            (
                r#"{"workflows":{"dynamic":{"defaultLeader":"nope"}}}"#,
                "defaultLeader",
            ),
            (
                r#"{"workflows":{"dynamic":{"guidance":[" "]}}}"#,
                "guidance",
            ),
            (
                r#"{"runtime":{"builtin":{"memory":"64MiB"}}}"#,
                "runtime.builtin.memory",
            ),
            (r#"{"runtime":{"builtin":{"stateDir":"rel"}}}"#, "stateDir"),
            (
                r#"{"workServers":{"a":{"enabled":true}}}"#,
                "workServers.a.url",
            ),
            (
                r#"{"remote":{"targets":{"p":{"apiKey":"k"}}}}"#,
                "remote.targets.p.url",
            ),
            (
                r#"{"workflows":{"prerequisites":{"githubToken":{"acquire":{"argv":["x"],"timeout":"301s"}}}}}"#,
                "acquire.timeout",
            ),
        ] {
            let err = ConfigFile::parse(text, FileKind::Global, path)
                .unwrap_err()
                .to_string();
            assert!(err.contains(needle), "{text}: {err}");
        }
    }

    #[test]
    fn load_missing_is_default_and_save_load_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        assert_eq!(
            ConfigFile::load(&path, FileKind::Global).unwrap(),
            ConfigFile::default()
        );
        let file = ConfigFile::parse(
            r#"{"agents":{"default":"codex","stuckTimeout":"45s"},
                "runtime":{"kind":"builtin","builtin":{"memory":"8GiB"}},
                "overlays":["ssh()"],
                "remote":{"default":"prod","targets":{"prod":{"url":"https://h:1","apiKey":"k"}}}}"#,
            FileKind::Global,
            &path,
        )
        .unwrap();
        file.save(&path, FileKind::Global).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"stuckTimeout\": \"45s\""), "{text}");
        assert_eq!(ConfigFile::load(&path, FileKind::Global).unwrap(), file);
    }

    #[test]
    fn save_refuses_a_file_that_would_not_load() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let path = repo_config_path(root);
        let file: ConfigFile =
            serde_json::from_value(serde_json::json!({"runtime": {"kind": "docker"}})).unwrap();
        assert!(file.save(&path, FileKind::Repo).is_err());
        assert!(!path.exists());
    }
}
