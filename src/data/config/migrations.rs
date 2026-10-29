//! Typed, scope-tagged config rewrites and reviewable migration plans.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::json_order::{KeyOrder, Ordered};
use super::repo::{RepoConfig, BOTH_DYNAMIC_WORKFLOWS_KEYS};
use crate::data::error::DataError;
use crate::data::fs::config_file::{
    read_config_file_within, write_config_file_within, write_user_config_file,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigMigrationScope {
    Global,
    Repo,
}

/// An idempotent rewrite. Steps inspect raw JSON so unrelated keys survive.
pub struct ConfigMigration {
    pub id: &'static str,
    pub scope: ConfigMigrationScope,
    pub summary: &'static str,
    pub apply: fn(&mut Value) -> Result<Option<ConfigMigrationChange>, DataError>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ConfigMigrationChange {
    pub id: &'static str,
    pub summary: &'static str,
    pub before: Value,
    pub after: Value,
}

pub const MOVE_DYNAMIC_WORKFLOWS: ConfigMigration = ConfigMigration {
    id: "move-dynamic-workflows",
    scope: ConfigMigrationScope::Repo,
    summary: "Move `dynamicWorkflows` to `workflows.dynamic`.",
    apply: move_dynamic_workflows,
};

pub const CONFIG_MIGRATIONS: &[ConfigMigration] = &[MOVE_DYNAMIC_WORKFLOWS];

fn move_dynamic_workflows(
    document: &mut Value,
) -> Result<Option<ConfigMigrationChange>, DataError> {
    let root = document
        .as_object_mut()
        .ok_or_else(|| DataError::Other("config document must be a JSON object".into()))?;
    let Some(legacy) = root
        .get("dynamicWorkflows")
        .filter(|v| !v.is_null())
        .cloned()
    else {
        root.remove("dynamicWorkflows");
        return Ok(None);
    };
    if root
        .get("workflows")
        .and_then(|w| w.get("dynamic"))
        .is_some_and(|v| !v.is_null())
    {
        return Err(DataError::Other(BOTH_DYNAMIC_WORKFLOWS_KEYS.into()));
    }
    let workflows = root.entry("workflows").or_insert_with(|| json!({}));
    if workflows.is_null() {
        *workflows = json!({});
    }
    let workflows = workflows
        .as_object_mut()
        .ok_or_else(|| DataError::Other("workflows must be a JSON object".into()))?;
    workflows.insert("dynamic".into(), legacy.clone());
    root.remove("dynamicWorkflows");
    Ok(Some(ConfigMigrationChange {
        id: MOVE_DYNAMIC_WORKFLOWS.id,
        summary: MOVE_DYNAMIC_WORKFLOWS.summary,
        before: json!({"dynamicWorkflows": legacy}),
        after: json!({"workflows": {"dynamic": legacy}}),
    }))
}

struct MigrationPlan {
    path: PathBuf,
    /// The git root for a repo plan: no component below it may be a symlink,
    /// checked when the plan is read and again just before it is written.
    root: Option<PathBuf>,
    original_text: Option<String>,
    document: Value,
    changes: Vec<ConfigMigrationChange>,
}

impl MigrationPlan {
    fn read(path: &Path, root: Option<&Path>) -> Result<Self, DataError> {
        let original_text = match root {
            Some(root) => read_config_file_within(root, path)?,
            None => match std::fs::read_to_string(path) {
                Ok(content) => Some(content),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(DataError::io(path, e)),
            },
        };
        let document = match original_text.as_deref() {
            Some(content) => {
                serde_json::from_str(content).map_err(|e| DataError::config_parse(path, e))?
            }
            None => json!({}),
        };
        Ok(Self {
            path: path.to_path_buf(),
            root: root.map(Path::to_path_buf),
            original_text,
            document,
            changes: vec![],
        })
    }

    fn run(&mut self, scope: ConfigMigrationScope) -> Result<(), DataError> {
        for step in CONFIG_MIGRATIONS.iter().filter(|step| step.scope == scope) {
            if let Some(change) = (step.apply)(&mut self.document)
                .map_err(|e| DataError::Other(format!("{}: {e}", self.path.display())))?
            {
                self.changes.push(change);
            }
        }
        Ok(())
    }

    fn validate_repo(&self) -> Result<(), DataError> {
        let mut cfg: RepoConfig = serde_json::from_value(self.document.clone())
            .map_err(|e| DataError::config_parse(&self.path, e))?;
        cfg.validate_loaded(&self.path)
    }

    fn preview(&self) -> String {
        let mut preview = self.path.display().to_string();
        for change in &self.changes {
            // JSON values always serialize successfully.
            preview.push_str(&format!(
                "\n\n{}\nBefore:\n{}\nAfter:\n{}",
                change.summary,
                serde_json::to_string_pretty(&change.before).expect("JSON value serializes"),
                serde_json::to_string_pretty(&change.after).expect("JSON value serializes"),
            ));
        }
        preview
    }

    fn apply(self) -> Result<(), DataError> {
        if self.changes.is_empty() {
            return Ok(());
        }
        // Re-read through the same checks: `.awman/` may have been replaced
        // while the user was looking at the prompt.
        let current = match &self.root {
            Some(root) => read_config_file_within(root, &self.path)?,
            None => Some(
                std::fs::read_to_string(&self.path).map_err(|e| DataError::io(&self.path, e))?,
            ),
        };
        if self.original_text != current {
            return Err(DataError::Other(format!(
                "{} changed since the migration was planned; run `awman config migrate` again",
                self.path.display()
            )));
        }
        // Keep the file's own key order; only the moved block changes place.
        let order = current.as_deref().map(KeyOrder::of);
        let content = serde_json::to_string_pretty(&Ordered {
            value: &self.document,
            order: order.as_ref(),
        })
        .map_err(|e| DataError::ConfigSerialize { source: e })?;
        match &self.root {
            Some(root) => write_config_file_within(root, &self.path, &content),
            None => write_user_config_file(&self.path, &content),
        }
    }
}

/// A planned migration of `<git_root>/.awman/config.json`.
pub struct RepoConfigMigration {
    plan: MigrationPlan,
}

impl RepoConfigMigration {
    pub fn plan(git_root: &Path) -> Result<Self, DataError> {
        let mut plan = MigrationPlan::read(&RepoConfig::path(git_root), Some(git_root))?;
        // Validate the original too, so invalid legacy values name the old key.
        plan.validate_repo()?;
        plan.run(ConfigMigrationScope::Repo)?;
        plan.validate_repo()?;
        Ok(Self { plan })
    }

    pub fn path(&self) -> &Path {
        &self.plan.path
    }
    pub fn is_empty(&self) -> bool {
        self.plan.changes.is_empty()
    }
    pub fn changes(&self) -> &[ConfigMigrationChange] {
        &self.plan.changes
    }
    pub fn preview(&self) -> String {
        self.plan.preview()
    }
    pub fn apply(self) -> Result<(), DataError> {
        self.plan.apply()
    }
}

/// A global plan. No global steps are registered for WI 0150.
pub struct GlobalConfigMigration {
    plan: MigrationPlan,
    has_never_read_dynamic_workflows: bool,
}

impl GlobalConfigMigration {
    pub fn plan(path: &Path) -> Result<Self, DataError> {
        let mut plan = MigrationPlan::read(path, None)?;
        let has_never_read_dynamic_workflows = plan
            .document
            .get("dynamicWorkflows")
            .is_some_and(|v| !v.is_null());
        plan.run(ConfigMigrationScope::Global)?;
        Ok(Self {
            plan,
            has_never_read_dynamic_workflows,
        })
    }

    pub fn path(&self) -> &Path {
        &self.plan.path
    }
    pub fn is_empty(&self) -> bool {
        self.plan.changes.is_empty()
    }
    pub fn changes(&self) -> &[ConfigMigrationChange] {
        &self.plan.changes
    }
    pub fn preview(&self) -> String {
        self.plan.preview()
    }
    pub fn has_never_read_dynamic_workflows(&self) -> bool {
        self.has_never_read_dynamic_workflows
    }
    pub fn apply(self) -> Result<(), DataError> {
        self.plan.apply()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_workflows_migration_is_idempotent_and_preserves_unrelated_keys() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join(".awman/config.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, r#"{"agent":"claude","future":{"keep":true},"dynamicWorkflows":{"guidance":["hello"]}}"#).unwrap();
        let plan = RepoConfigMigration::plan(root.path()).unwrap();
        assert_eq!(plan.changes().len(), 1);
        plan.apply().unwrap();
        let once = std::fs::read(&config).unwrap();
        let plan = RepoConfigMigration::plan(root.path()).unwrap();
        assert!(plan.is_empty());
        assert!(plan.changes().is_empty());
        plan.apply().unwrap();
        assert_eq!(std::fs::read(&config).unwrap(), once);
        let value: Value = serde_json::from_slice(&once).unwrap();
        assert_eq!(value["agent"], "claude");
        assert_eq!(value["future"]["keep"], true);
        assert_eq!(value["workflows"]["dynamic"]["guidance"][0], "hello");
        assert!(value.get("dynamicWorkflows").is_none());
    }
}

#[cfg(test)]
mod safety_tests {
    use super::*;

    #[test]
    fn migration_keeps_the_order_of_every_other_key() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join(".awman/config.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            r#"{"zeta":1,"agent":"claude","dynamicWorkflows":{"maxConcurrentSteps":2},"alpha":{"y":1,"b":2}}"#,
        )
        .unwrap();
        RepoConfigMigration::plan(root.path())
            .unwrap()
            .apply()
            .unwrap();
        let after = std::fs::read_to_string(&config).unwrap();
        let positions: Vec<usize> = [
            "\"zeta\"",
            "\"agent\"",
            "\"workflows\"",
            "\"alpha\"",
            "\"y\"",
            "\"b\"",
        ]
        .iter()
        .map(|key| {
            after
                .find(key)
                .unwrap_or_else(|| panic!("{key} in {after}"))
        })
        .collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "key order must survive: {after}"
        );

        // An existing `workflows` block keeps its slot and its own keys.
        std::fs::write(
            &config,
            r#"{"workflows":{"zz":true,"prerequisites":{}},"agent":"claude","dynamicWorkflows":{}}"#,
        )
        .unwrap();
        RepoConfigMigration::plan(root.path())
            .unwrap()
            .apply()
            .unwrap();
        let after = std::fs::read_to_string(&config).unwrap();
        let order: Vec<usize> = [
            "\"workflows\"",
            "\"zz\"",
            "\"prerequisites\"",
            "\"dynamic\"",
            "\"agent\"",
        ]
        .iter()
        .map(|key| after.find(key).unwrap())
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{after}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_awman_directory_is_refused_at_plan_and_at_apply() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let outside = tmp.path().join("outside");
        let legacy = r#"{"dynamicWorkflows":{"guidance":["keep"]}}"#;
        std::fs::create_dir_all(repo.join(".awman")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(repo.join(".awman/config.json"), legacy).unwrap();
        std::fs::write(outside.join("config.json"), legacy).unwrap();

        // Swapped while the prompt is open: the plan was read from the real
        // directory, the write would follow the replacement.
        let plan = RepoConfigMigration::plan(&repo).unwrap();
        std::fs::rename(repo.join(".awman"), repo.join(".awman-original")).unwrap();
        std::os::unix::fs::symlink(&outside, repo.join(".awman")).unwrap();
        let err = plan.apply().unwrap_err().to_string();
        assert!(err.contains("is a symlink"), "{err}");
        assert_eq!(
            std::fs::read_to_string(outside.join("config.json")).unwrap(),
            legacy
        );

        // Already in place: refused before anything is read.
        let err = RepoConfigMigration::plan(&repo).err().unwrap().to_string();
        assert!(err.contains("is a symlink"), "{err}");
    }
}
