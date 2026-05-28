//! Per-repository configuration: `<git_root>/.awman/config.json`.
//!
//! Schema parity with the legacy `RepoConfig` (`oldsrc/config/mod.rs`) is
//! preserved for forward and backward compatibility — users upgrading from a
//! prior release must continue to read their existing files.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;

/// Subdirectory under the git root in which awman stores per-repo state.
pub const REPO_CONFIG_SUBDIR: &str = ".awman";

/// Filename of the per-repo config inside `REPO_CONFIG_SUBDIR`.
pub const REPO_CONFIG_FILENAME: &str = "config.json";

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

/// API server configuration nested inside `GlobalConfig`.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiConfig {
    #[serde(rename = "workDirs", skip_serializing_if = "Option::is_none")]
    pub work_dirs: Option<Vec<String>>,
    #[serde(
        rename = "alwaysNonInteractive",
        skip_serializing_if = "Option::is_none"
    )]
    pub always_non_interactive: Option<bool>,
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

/// Per-repository configuration stored at `<git_root>/.awman/config.json`.
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
    #[serde(rename = "envPassthrough", default, skip_serializing)]
    pub legacy_env_passthrough: Option<Vec<String>>,
    #[serde(rename = "workItems", skip_serializing_if = "Option::is_none")]
    pub work_items: Option<WorkItemsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlays: Option<Vec<String>>,
    #[serde(rename = "agentStuckTimeout", skip_serializing_if = "Option::is_none")]
    pub agent_stuck_timeout_secs: Option<u64>,
    #[serde(rename = "baseImage", skip_serializing_if = "Option::is_none")]
    pub base_image: Option<String>,
}

impl RepoConfig {
    /// Path to the per-repo config under a git root.
    pub fn path(git_root: &Path) -> PathBuf {
        git_root.join(REPO_CONFIG_SUBDIR).join(REPO_CONFIG_FILENAME)
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
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(awman_dir.join(REPO_CONFIG_FILENAME), b"{not valid json").unwrap();

        let err = RepoConfig::load(tmp.path()).unwrap_err();
        assert!(
            matches!(err, DataError::ConfigParse { .. }),
            "expected ConfigParse, got {err:?}"
        );
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
    fn path_is_inside_awman_subdir() {
        let tmp = make_git_root();
        let p = RepoConfig::path(tmp.path());
        assert_eq!(
            p,
            tmp.path()
                .join(REPO_CONFIG_SUBDIR)
                .join(REPO_CONFIG_FILENAME)
        );
    }

    // ─── Overlay field deserialization ───────────────────────────────────────

    #[test]
    fn overlays_new_flat_format_deserializes_correctly() {
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join(REPO_CONFIG_FILENAME),
            r#"{"overlays": ["skill(*)", "env(X)"]}"#,
        )
        .unwrap();

        let cfg = RepoConfig::load(tmp.path()).unwrap();
        assert_eq!(
            cfg.overlays,
            Some(vec!["skill(*)".to_string(), "env(X)".to_string()]),
            "new flat overlays array must deserialize correctly"
        );
        assert!(cfg.legacy_env_passthrough.is_none());
    }

    #[test]
    fn overlays_old_object_format_fails_to_deserialize() {
        // Old format: {"overlays": {"directories": [...], "skills": true}}
        // Must not silently migrate — must surface a config parse error.
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join(REPO_CONFIG_FILENAME),
            r#"{"overlays": {"directories": [], "skills": true}}"#,
        )
        .unwrap();

        let result = RepoConfig::load(tmp.path());
        assert!(
            result.is_err(),
            "old object-format overlays must fail to deserialize (no auto-migration); got Ok"
        );
        assert!(
            matches!(result.unwrap_err(), DataError::ConfigParse { .. }),
            "error must be ConfigParse"
        );
    }

    #[test]
    fn legacy_env_passthrough_deserializes_with_overlays_absent() {
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join(REPO_CONFIG_FILENAME),
            r#"{"envPassthrough": ["MY_VAR", "OTHER_VAR"]}"#,
        )
        .unwrap();

        let cfg = RepoConfig::load(tmp.path()).unwrap();
        assert_eq!(
            cfg.legacy_env_passthrough,
            Some(vec!["MY_VAR".to_string(), "OTHER_VAR".to_string()]),
            "legacy envPassthrough must deserialize into legacy_env_passthrough"
        );
        assert!(
            cfg.overlays.is_none(),
            "overlays must be absent when only envPassthrough is present"
        );
    }

    #[test]
    fn overlays_round_trip_with_dir_and_env_expressions() {
        let tmp = make_git_root();
        let original = RepoConfig {
            overlays: Some(vec![
                "dir(~/data:/workspace:ro)".to_string(),
                "env(TOKEN)".to_string(),
            ]),
            ..Default::default()
        };
        original.save(tmp.path()).unwrap();
        let reloaded = RepoConfig::load(tmp.path()).unwrap();
        assert_eq!(
            original.overlays, reloaded.overlays,
            "overlays with dir() and env() expressions must round-trip correctly"
        );
    }

    #[test]
    fn legacy_env_passthrough_is_not_serialized_in_save() {
        // legacy_env_passthrough has skip_serializing — it must not appear in the
        // JSON written by save(), so it doesn't persist across a load/save cycle.
        let tmp = make_git_root();
        let awman_dir = tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join(REPO_CONFIG_FILENAME),
            r#"{"envPassthrough": ["VAR"]}"#,
        )
        .unwrap();

        let cfg = RepoConfig::load(tmp.path()).unwrap();
        cfg.save(tmp.path()).unwrap();

        let written = std::fs::read_to_string(RepoConfig::path(tmp.path())).unwrap();
        assert!(
            !written.contains("envPassthrough"),
            "legacy envPassthrough must not be re-serialized by save(); got: {written}"
        );
    }
}
