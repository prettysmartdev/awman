//! Per-repository configuration: `<git_root>/.amux/config.json`.
//!
//! Schema parity with the legacy `RepoConfig` (`oldsrc/config/mod.rs`) is
//! preserved for forward and backward compatibility — users upgrading from a
//! prior release must continue to read their existing files.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;

/// Subdirectory under the git root in which amux stores per-repo state.
pub const REPO_CONFIG_SUBDIR: &str = ".amux";

/// Filename of the per-repo config inside `REPO_CONFIG_SUBDIR`.
pub const REPO_CONFIG_FILENAME: &str = "config.json";

/// Legacy subdirectory used before the move to `.amux/`.
const LEGACY_REPO_CONFIG_SUBDIR: &str = "aspec";

/// Legacy filename used before the move to `config.json`.
const LEGACY_REPO_CONFIG_FILENAME: &str = ".amux.json";

/// Remote-mode configuration nested inside `GlobalConfig`.
///
/// Lives in `repo.rs` per the work-item layout even though it is consumed
/// by `GlobalConfig`; the entire family of config structs is grouped together.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteConfig {
    #[serde(rename = "defaultAddr", skip_serializing_if = "Option::is_none")]
    pub default_addr: Option<String>,
    #[serde(rename = "savedDirs", skip_serializing_if = "Option::is_none")]
    pub saved_dirs: Option<Vec<String>>,
    #[serde(rename = "defaultAPIKey", skip_serializing_if = "Option::is_none")]
    pub default_api_key: Option<String>,
}

/// Headless server configuration nested inside `GlobalConfig`.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeadlessConfig {
    #[serde(rename = "workDirs", skip_serializing_if = "Option::is_none")]
    pub work_dirs: Option<Vec<String>>,
    #[serde(
        rename = "alwaysNonInteractive",
        skip_serializing_if = "Option::is_none"
    )]
    pub always_non_interactive: Option<bool>,
}

/// Overlay configuration for mounting host resources into agent containers.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OverlaysConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directories: Option<Vec<DirectoryOverlayConfig>>,
    /// When true, mount the global amux skills dir into the agent container.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<bool>,
}

/// A single directory overlay entry as stored in JSON config.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectoryOverlayConfig {
    /// Host path (absolute or `~`-expanded).
    pub host: String,
    /// Container path (absolute).
    pub container: String,
    /// Mount permission: `"ro"` or `"rw"`. Defaults to `"ro"` when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
}

/// Work-items configuration nested within `RepoConfig`.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkItemsConfig {
    /// Path to the work items directory (relative to repo root, or absolute).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Path to the work item template file (relative to repo root, or absolute).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

/// Per-repository configuration stored at `<git_root>/.amux/config.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_agent_auth_accepted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_scrollback_lines: Option<usize>,
    #[serde(
        rename = "yoloDisallowedTools",
        skip_serializing_if = "Option::is_none"
    )]
    pub yolo_disallowed_tools: Option<Vec<String>>,
    #[serde(rename = "envPassthrough", skip_serializing_if = "Option::is_none")]
    pub env_passthrough: Option<Vec<String>>,
    #[serde(rename = "workItems", skip_serializing_if = "Option::is_none")]
    pub work_items: Option<WorkItemsConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlays: Option<OverlaysConfig>,
    #[serde(rename = "agentStuckTimeout", skip_serializing_if = "Option::is_none")]
    pub agent_stuck_timeout_secs: Option<u64>,
}

impl RepoConfig {
    /// Path to the per-repo config under a git root.
    pub fn path(git_root: &Path) -> PathBuf {
        git_root.join(REPO_CONFIG_SUBDIR).join(REPO_CONFIG_FILENAME)
    }

    /// Path to the pre-`.amux/` legacy config under a git root.
    pub fn legacy_path(git_root: &Path) -> PathBuf {
        git_root
            .join(LEGACY_REPO_CONFIG_SUBDIR)
            .join(LEGACY_REPO_CONFIG_FILENAME)
    }

    /// Load the repo config from disk.
    ///
    /// Returns `RepoConfig::default()` when no file is present.
    /// Returns `DataError::ConfigParse` when the file is present but malformed.
    pub fn load(git_root: &Path) -> Result<Self, DataError> {
        let path = Self::path(git_root);
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path).map_err(|e| DataError::io(&path, e))?;
        serde_json::from_str(&content).map_err(|e| DataError::config_parse(&path, e))
    }

    /// Persist this config to disk, creating parent directories if needed.
    pub fn save(&self, git_root: &Path) -> Result<(), DataError> {
        let path = Self::path(git_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
        }
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| DataError::ConfigSerialize { source: e })?;
        std::fs::write(&path, content).map_err(|e| DataError::io(&path, e))
    }

    /// Migrate a legacy `aspec/.amux.json` to `.amux/config.json` if and only
    /// if the legacy file exists and the new path does not. Removes the legacy
    /// file on success. Returns `true` when a migration was performed.
    pub fn migrate_legacy(git_root: &Path) -> Result<bool, DataError> {
        let legacy = Self::legacy_path(git_root);
        let current = Self::path(git_root);
        if !legacy.exists() || current.exists() {
            return Ok(false);
        }
        let content = std::fs::read_to_string(&legacy).map_err(|e| DataError::io(&legacy, e))?;
        if let Some(parent) = current.parent() {
            std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
        }
        std::fs::write(&current, &content).map_err(|e| DataError::io(&current, e))?;
        std::fs::remove_file(&legacy).map_err(|e| DataError::io(&legacy, e))?;
        Ok(true)
    }

    /// Resolve the configured work items directory relative to `git_root`.
    pub fn work_items_dir(&self, git_root: &Path) -> Option<PathBuf> {
        let dir = self.work_items.as_ref()?.dir.as_deref()?;
        if dir.is_empty() {
            return None;
        }
        let p = Path::new(dir);
        if p.is_absolute() {
            Some(p.to_path_buf())
        } else {
            Some(git_root.join(p))
        }
    }

    /// Resolve the configured work item template path relative to `git_root`.
    pub fn work_items_template(&self, git_root: &Path) -> Option<PathBuf> {
        let tmpl = self.work_items.as_ref()?.template.as_deref()?;
        if tmpl.is_empty() {
            return None;
        }
        let p = Path::new(tmpl);
        if p.is_absolute() {
            Some(p.to_path_buf())
        } else {
            Some(git_root.join(p))
        }
    }

    /// Resolve the work items directory, falling back to `<git_root>/aspec/work-items/`.
    pub fn work_items_dir_or_default(&self, git_root: &Path) -> PathBuf {
        self.work_items_dir(git_root)
            .unwrap_or_else(|| git_root.join("aspec").join("work-items"))
    }

    /// Resolve the work item template path, falling back to `<work_items_dir>/0000-template.md`.
    pub fn work_items_template_or_default(&self, git_root: &Path) -> PathBuf {
        self.work_items_template(git_root).unwrap_or_else(|| {
            self.work_items_dir_or_default(git_root)
                .join("0000-template.md")
        })
    }

    /// Replace the `workItems` config block. The chained `save(git_root)` call
    /// persists the change. Pass `None` to clear the block entirely.
    pub fn set_work_items_config(&mut self, cfg: Option<WorkItemsConfig>) {
        self.work_items = cfg;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_git_root() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn load_missing_config_returns_default() {
        let tmp = make_git_root();
        let cfg = RepoConfig::load(tmp.path()).unwrap();
        assert_eq!(cfg, RepoConfig::default());
        assert!(cfg.agent.is_none());
    }

    #[test]
    fn load_save_load_round_trip_is_byte_stable() {
        let tmp = make_git_root();
        let original = RepoConfig {
            agent: Some("claude".to_string()),
            terminal_scrollback_lines: Some(5000),
            yolo_disallowed_tools: Some(vec!["bash".to_string(), "python".to_string()]),
            env_passthrough: Some(vec!["HOME".to_string(), "PATH".to_string()]),
            agent_stuck_timeout_secs: Some(60),
            ..Default::default()
        };
        original.save(tmp.path()).unwrap();
        let reloaded = RepoConfig::load(tmp.path()).unwrap();
        assert_eq!(original, reloaded);
    }

    #[test]
    fn load_malformed_json_returns_config_parse_error() {
        let tmp = make_git_root();
        let amux_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&amux_dir).unwrap();
        std::fs::write(amux_dir.join(REPO_CONFIG_FILENAME), b"{not valid json").unwrap();

        let err = RepoConfig::load(tmp.path()).unwrap_err();
        assert!(
            matches!(err, DataError::ConfigParse { .. }),
            "expected ConfigParse, got {err:?}"
        );
    }

    #[test]
    fn migrate_legacy_moves_file_and_removes_old() {
        let tmp = make_git_root();
        let legacy_dir = tmp.path().join("aspec");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let legacy_content = r#"{"agent":"claude"}"#;
        std::fs::write(legacy_dir.join(".amux.json"), legacy_content).unwrap();

        let migrated = RepoConfig::migrate_legacy(tmp.path()).unwrap();
        assert!(migrated, "expected migration to be performed");

        // Legacy file must be gone.
        assert!(!RepoConfig::legacy_path(tmp.path()).exists());
        // New file must exist and be readable.
        assert!(RepoConfig::path(tmp.path()).exists());
        let loaded = RepoConfig::load(tmp.path()).unwrap();
        assert_eq!(loaded.agent.as_deref(), Some("claude"));
    }

    #[test]
    fn migrate_legacy_no_op_when_both_files_exist() {
        let tmp = make_git_root();
        // Write both legacy and new config.
        let legacy_dir = tmp.path().join("aspec");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(legacy_dir.join(".amux.json"), r#"{"agent":"old"}"#).unwrap();

        let new_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::write(new_dir.join(REPO_CONFIG_FILENAME), r#"{"agent":"new"}"#).unwrap();

        let migrated = RepoConfig::migrate_legacy(tmp.path()).unwrap();
        assert!(
            !migrated,
            "migration should be a no-op when new file already exists"
        );
        // Legacy file should still be there.
        assert!(RepoConfig::legacy_path(tmp.path()).exists());
    }

    #[test]
    fn migrate_legacy_no_op_when_neither_file_exists() {
        let tmp = make_git_root();
        let migrated = RepoConfig::migrate_legacy(tmp.path()).unwrap();
        assert!(!migrated);
    }

    #[test]
    fn work_items_dir_resolves_relative_path() {
        let tmp = make_git_root();
        let cfg = RepoConfig {
            work_items: Some(WorkItemsConfig {
                dir: Some("aspec/work-items".to_string()),
                template: None,
            }),
            ..Default::default()
        };
        let resolved = cfg.work_items_dir(tmp.path()).unwrap();
        assert_eq!(resolved, tmp.path().join("aspec/work-items"));
    }

    #[test]
    fn work_items_dir_resolves_absolute_path() {
        let tmp = make_git_root();
        let cfg = RepoConfig {
            work_items: Some(WorkItemsConfig {
                dir: Some("/abs/path".to_string()),
                template: None,
            }),
            ..Default::default()
        };
        let resolved = cfg.work_items_dir(tmp.path()).unwrap();
        assert_eq!(resolved, PathBuf::from("/abs/path"));
    }

    #[test]
    fn work_items_dir_none_when_not_set() {
        let cfg = RepoConfig::default();
        let tmp = make_git_root();
        assert!(cfg.work_items_dir(tmp.path()).is_none());
    }

    #[test]
    fn path_is_inside_amux_subdir() {
        let tmp = make_git_root();
        let p = RepoConfig::path(tmp.path());
        assert_eq!(
            p,
            tmp.path()
                .join(REPO_CONFIG_SUBDIR)
                .join(REPO_CONFIG_FILENAME)
        );
    }

    #[test]
    fn legacy_path_is_inside_aspec_dir() {
        let tmp = make_git_root();
        let p = RepoConfig::legacy_path(tmp.path());
        assert_eq!(p, tmp.path().join("aspec").join(".amux.json"));
    }

    // ─── OverlaysConfig / skills deserialization ──────────────────────────────

    #[test]
    fn overlays_config_skills_true_deserializes() {
        let json = r#"{"overlays": {"skills": true}}"#;
        let cfg: RepoConfig = serde_json::from_str(json).unwrap();
        let overlays = cfg.overlays.expect("overlays must be present");
        assert_eq!(
            overlays.skills,
            Some(true),
            "skills: true must deserialize to Some(true)"
        );
        assert!(overlays.directories.is_none(), "directories must be None");
    }

    #[test]
    fn overlays_config_skills_false_deserializes() {
        let json = r#"{"overlays": {"skills": false}}"#;
        let cfg: RepoConfig = serde_json::from_str(json).unwrap();
        let overlays = cfg.overlays.expect("overlays must be present");
        assert_eq!(
            overlays.skills,
            Some(false),
            "skills: false must deserialize to Some(false)"
        );
    }

    #[test]
    fn overlays_config_missing_skills_key_deserializes_to_none() {
        let json = r#"{"overlays": {}}"#;
        let cfg: RepoConfig = serde_json::from_str(json).unwrap();
        let overlays = cfg.overlays.expect("overlays must be present");
        assert!(
            overlays.skills.is_none(),
            "missing 'skills' key must deserialize to None; got {:?}",
            overlays.skills
        );
    }

    #[test]
    fn overlays_config_only_directories_deserializes_without_error() {
        let json = r#"{"overlays": {"directories": [{"host": "/h", "container": "/c", "permission": "ro"}]}}"#;
        let cfg: RepoConfig = serde_json::from_str(json).unwrap();
        let overlays = cfg.overlays.expect("overlays must be present");
        assert!(
            overlays.skills.is_none(),
            "skills must be None when not in JSON"
        );
        assert_eq!(
            overlays.directories.as_ref().map(|d| d.len()),
            Some(1),
            "directories must have 1 entry"
        );
    }
}
