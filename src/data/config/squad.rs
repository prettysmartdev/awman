//! The squad scheduler's view of the config tree.
//!
//! A squad evaluation reads two things: the daemon-wide squad settings
//! (`daemon.features.squad.*`, global file only) and the dynamic-workflow
//! settings (`workflows.dynamic.*`), layered global → the task's own
//! `config.json` (a task-scope [`ConfigFile`]). [`SquadConfig`] gathers both
//! so the scheduler and the task gateway read them in one place.

use std::collections::HashMap;

use crate::data::config::file::{ConfigFile, FileKind};
use crate::data::config::workflows::{DynamicConfig, WorkflowsConfig};
use crate::data::error::DataError;
use crate::data::fs::daemon_env::EnvPersistenceSetting;
use crate::data::fs::SquadPaths;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SquadConfig {
    /// `workflows.dynamic.agentsToModels`.
    pub agents_to_models: Option<HashMap<String, Vec<String>>>,
    /// `daemon.features.squad.maxConcurrentEvaluations` (daemon-wide).
    pub max_concurrent_evaluations: Option<usize>,
    /// `workflows.dynamic.defaultLeader`.
    pub default_leader: Option<String>,
    /// `workflows.dynamic.guidance`.
    pub guidance: Option<Vec<String>>,
    /// `daemon.features.squad.envPersistence` (daemon-wide).
    pub env_persistence: Option<EnvPersistenceSetting>,
}

impl SquadConfig {
    /// The squad view of the global config file.
    pub fn from_global(global: &ConfigFile) -> Self {
        let squad = global
            .daemon
            .as_ref()
            .and_then(|d| d.features.as_ref())
            .and_then(|f| f.squad.as_ref());
        Self {
            max_concurrent_evaluations: squad.and_then(|s| s.max_concurrent_evaluations),
            env_persistence: squad.and_then(|s| s.env_persistence),
            ..Self::from_dynamic(dynamic_of(global))
        }
    }

    /// The squad view of a task's own file (dynamic settings only).
    pub fn from_task_file(task: &ConfigFile) -> Self {
        Self::from_dynamic(dynamic_of(task))
    }

    fn from_dynamic(dynamic: Option<&DynamicConfig>) -> Self {
        Self {
            agents_to_models: dynamic
                .and_then(|d| d.agents_to_models.clone())
                .map(|m| m.into_iter().collect()),
            default_leader: dynamic.and_then(|d| d.default_leader.clone()),
            guidance: dynamic.and_then(|d| d.guidance.clone()),
            ..Self::default()
        }
    }

    /// The task-file document carrying this view's dynamic settings.
    pub fn to_task_file(&self) -> ConfigFile {
        let dynamic = DynamicConfig {
            default_leader: self.default_leader.clone(),
            agents_to_models: self
                .agents_to_models
                .clone()
                .map(|m| m.into_iter().collect()),
            guidance: self.guidance.clone(),
        };
        ConfigFile {
            workflows: Some(WorkflowsConfig {
                dynamic: Some(dynamic),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Whether no dynamic setting is present.
    pub fn has_dynamic_settings(&self) -> bool {
        self.default_leader.is_some()
            || self
                .agents_to_models
                .as_ref()
                .is_some_and(|m| !m.is_empty())
            || self.guidance.as_ref().is_some_and(|g| !g.is_empty())
    }

    /// The same checks a task file is held to.
    pub fn validate(&self) -> Result<(), DataError> {
        self.to_task_file().validate_semantics()?;
        crate::data::config::fields::validate_document(
            &self.to_task_file().to_value(),
            FileKind::Task,
        )
        .map_err(|e| DataError::Other(e.to_string()))
    }

    pub fn max_concurrent_evaluations_or_default(&self) -> usize {
        self.max_concurrent_evaluations.unwrap_or(2)
    }

    pub fn env_persistence_or_default(&self) -> EnvPersistenceSetting {
        self.env_persistence.unwrap_or_default()
    }

    /// Layer this (a task's view) over `base` (the global view): the task's
    /// leader and model map replace the global ones; guidance adds, global
    /// first, de-duplicated by trimmed text. Daemon-wide settings are always
    /// the base's.
    pub fn layered_over(&self, base: &SquadConfig) -> SquadConfig {
        let guidance = match (&base.guidance, &self.guidance) {
            (None, None) => None,
            (b, t) => {
                let mut seen = std::collections::HashSet::new();
                let merged: Vec<String> = b
                    .iter()
                    .flatten()
                    .chain(t.iter().flatten())
                    .filter(|g| seen.insert(g.trim().to_string()))
                    .cloned()
                    .collect();
                Some(merged)
            }
        };
        SquadConfig {
            agents_to_models: self
                .agents_to_models
                .clone()
                .or_else(|| base.agents_to_models.clone()),
            max_concurrent_evaluations: base.max_concurrent_evaluations,
            default_leader: self
                .default_leader
                .clone()
                .or_else(|| base.default_leader.clone()),
            guidance,
            env_persistence: base.env_persistence,
        }
    }
}

fn dynamic_of(file: &ConfigFile) -> Option<&DynamicConfig> {
    file.workflows.as_ref().and_then(|w| w.dynamic.as_ref())
}

/// A task's own config file, validated as a task-scope file. `None` when the
/// file is absent.
pub fn load_task_file(paths: &SquadPaths, name: &str) -> Result<Option<ConfigFile>, DataError> {
    let path = paths.task_config_file(name)?;
    match std::fs::read_to_string(&path) {
        Ok(text) => ConfigFile::parse(&text, FileKind::Task, &path).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(DataError::io(&path, e)),
    }
}

/// A task's squad view layered over the global one. A malformed task file is
/// an error scoped to that task, never a silent fall back.
pub fn task_squad_config(
    paths: &SquadPaths,
    name: &str,
    global: &SquadConfig,
) -> Result<SquadConfig, DataError> {
    Ok(match task_own_squad_config(paths, name)? {
        Some(task) => task.layered_over(global),
        None => global.clone(),
    })
}

/// The task's own squad view, not layered: `None` when the file is absent
/// or carries no dynamic settings.
pub fn task_own_squad_config(
    paths: &SquadPaths,
    name: &str,
) -> Result<Option<SquadConfig>, DataError> {
    Ok(load_task_file(paths, name)?
        .map(|file| SquadConfig::from_task_file(&file))
        .filter(SquadConfig::has_dynamic_settings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_evaluation_settings_come_from_dynamic_workflows() {
        let global: ConfigFile = serde_json::from_value(serde_json::json!({
            "workflows": { "dynamic": {
                "agentsToModels": { "codex": ["model-a"] },
                "defaultLeader": "claude::leader",
                "guidance": ["run checks"]
            }}
        }))
        .unwrap();
        let resolved = SquadConfig::from_global(&global);
        assert_eq!(resolved.agents_to_models.unwrap()["codex"], ["model-a"]);
        assert_eq!(resolved.default_leader.as_deref(), Some("claude::leader"));
        assert_eq!(resolved.guidance.unwrap(), ["run checks"]);

        let legacy = serde_json::json!({"squad": {"guidance": ["old location"]}});
        let err =
            crate::data::config::fields::validate_document(&legacy, FileKind::Global).unwrap_err();
        assert!(err.to_string().contains("workflows.dynamic.guidance"));
    }

    #[test]
    fn task_layers_over_global_with_additive_guidance() {
        let global = SquadConfig {
            default_leader: Some("claude::a".into()),
            guidance: Some(vec!["one".into(), "two".into()]),
            max_concurrent_evaluations: Some(3),
            ..Default::default()
        };
        let task = SquadConfig {
            guidance: Some(vec![" two ".into(), "three".into()]),
            agents_to_models: Some(HashMap::from([("codex".into(), vec!["m".into()])])),
            max_concurrent_evaluations: Some(9),
            ..Default::default()
        };
        let merged = task.layered_over(&global);
        assert_eq!(merged.default_leader.as_deref(), Some("claude::a"));
        assert_eq!(merged.guidance.unwrap(), ["one", "two", "three"]);
        assert_eq!(merged.max_concurrent_evaluations, Some(3));
        assert!(merged.agents_to_models.is_some());
    }

    #[test]
    fn task_files_are_task_scope_and_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SquadPaths::from_root(tmp.path());
        std::fs::create_dir_all(paths.tasks_dir().join("t")).unwrap();
        let path = paths.task_config_file("t").unwrap();
        let view = SquadConfig {
            agents_to_models: Some(HashMap::from([("codex".into(), vec!["m".into()])])),
            ..Default::default()
        };
        view.validate().unwrap();
        std::fs::write(&path, serde_json::to_string(&view.to_task_file()).unwrap()).unwrap();
        assert_eq!(task_own_squad_config(&paths, "t").unwrap(), Some(view));
        // The pre-0139 `squad` block fails with the moved-to hint.
        std::fs::write(&path, r#"{"squad":{"agentsToModels":{"codex":["m"]}}}"#).unwrap();
        let err = task_own_squad_config(&paths, "t").unwrap_err().to_string();
        assert!(err.contains("workflows.dynamic.agentsToModels"), "{err}");
        // Daemon keys are rejected in task files.
        std::fs::write(&path, r#"{"daemon":{"maxConcurrentJobs":2}}"#).unwrap();
        assert!(task_own_squad_config(&paths, "t").is_err());
    }
}
