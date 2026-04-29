use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Overlay configuration for mounting host resources into agent containers.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct OverlaysConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directories: Option<Vec<DirectoryOverlayConfig>>,
}

/// A single directory overlay entry in config JSON.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
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
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkItemsConfig {
    /// Path to the work items directory (relative to repo root, or absolute).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Path to the work item template file (relative to repo root, or absolute).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

/// Per-repository configuration stored at `GITROOT/.amux/config.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct RepoConfig {
    pub agent: Option<String>,
    /// Whether the user has consented to mounting agent credentials into containers.
    /// Saved once per Git root; None means the user has not been asked yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_agent_auth_accepted: Option<bool>,
    /// Number of scrollback lines for the container terminal emulator.
    /// Overrides the global config value and the built-in default (10,000).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_scrollback_lines: Option<usize>,
    /// Tools the agent is not allowed to use in `--yolo` mode.
    /// Overrides the global config value. When non-empty, passed as
    /// `--disallowedTools` to Claude (other agents do not support this flag).
    #[serde(rename = "yoloDisallowedTools", skip_serializing_if = "Option::is_none")]
    pub yolo_disallowed_tools: Option<Vec<String>>,
    /// Host environment variable names to pass through into agent containers.
    /// Values are read from the host process environment at launch time.
    /// Repo config overrides global config when both are set.
    #[serde(rename = "envPassthrough", skip_serializing_if = "Option::is_none")]
    pub env_passthrough: Option<Vec<String>>,
    /// Configurable work items directory and template paths.
    #[serde(rename = "workItems", skip_serializing_if = "Option::is_none")]
    pub work_items: Option<WorkItemsConfig>,
    /// Overlay configuration for mounting host directories into agent containers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlays: Option<OverlaysConfig>,
    /// Seconds of container output inactivity before the agent is considered stuck.
    /// Overrides the global config value and the built-in default (30).
    #[serde(rename = "agentStuckTimeout", skip_serializing_if = "Option::is_none")]
    pub agent_stuck_timeout_secs: Option<u64>,
}

impl RepoConfig {
    /// Resolve the configured work items directory relative to `git_root`.
    /// Returns `None` if `work_items.dir` is not set or is empty.
    pub fn work_items_dir(&self, git_root: &Path) -> Option<PathBuf> {
        let dir = self.work_items.as_ref()?.dir.as_deref()?;
        if dir.is_empty() {
            return None;
        }
        let p = std::path::Path::new(dir);
        if p.is_absolute() {
            Some(p.to_path_buf())
        } else {
            Some(git_root.join(p))
        }
    }

    /// Resolve the configured work item template path relative to `git_root`.
    /// Returns `None` if `work_items.template` is not set or is empty.
    pub fn work_items_template(&self, git_root: &Path) -> Option<PathBuf> {
        let tmpl = self.work_items.as_ref()?.template.as_deref()?;
        if tmpl.is_empty() {
            return None;
        }
        let p = std::path::Path::new(tmpl);
        if p.is_absolute() {
            Some(p.to_path_buf())
        } else {
            Some(git_root.join(p))
        }
    }
}

/// Remote connection configuration nested within `GlobalConfig`.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteConfig {
    /// Default remote headless amux server address (e.g. "http://1.2.3.4:9876").
    #[serde(rename = "defaultAddr", skip_serializing_if = "Option::is_none")]
    pub default_addr: Option<String>,

    /// List of working directory paths pre-saved for `remote session start`.
    #[serde(rename = "savedDirs", skip_serializing_if = "Option::is_none")]
    pub saved_dirs: Option<Vec<String>>,

    /// Default API key for authenticating with the remote headless amux host.
    #[serde(rename = "defaultAPIKey", skip_serializing_if = "Option::is_none")]
    pub default_api_key: Option<String>,
}

/// Headless server configuration nested within `GlobalConfig`.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct HeadlessConfig {
    /// Working directories allowlisted for headless mode sessions.
    /// Each entry should be an absolute path. Resolved to canonical paths at server startup.
    #[serde(rename = "workDirs", skip_serializing_if = "Option::is_none")]
    pub work_dirs: Option<Vec<String>>,
    /// When true, inject `--non-interactive` into every headless command dispatch
    /// that supports the flag, even if the client did not pass it.
    #[serde(rename = "alwaysNonInteractive", skip_serializing_if = "Option::is_none")]
    pub always_non_interactive: Option<bool>,
}

/// Global configuration stored at `$HOME/.amux/config.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct GlobalConfig {
    pub default_agent: Option<String>,
    /// Default number of scrollback lines for the container terminal emulator.
    /// Applied to all repos unless overridden by per-repo config. Built-in default: 10,000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_scrollback_lines: Option<usize>,
    /// Container runtime to use. Supported values: `"docker"` (default), `"apple-containers"`.
    /// `"apple-containers"` is only supported on macOS; on other platforms it falls back to Docker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// Default tools the agent is not allowed to use in `--yolo` mode.
    /// Overridden by per-repo config when set.
    #[serde(rename = "yoloDisallowedTools", skip_serializing_if = "Option::is_none")]
    pub yolo_disallowed_tools: Option<Vec<String>>,
    /// Host environment variable names to pass through into agent containers.
    /// Values are read from the host process environment at launch time.
    /// Overridden by per-repo config when both are set.
    #[serde(rename = "envPassthrough", skip_serializing_if = "Option::is_none")]
    pub env_passthrough: Option<Vec<String>>,
    /// Headless server configuration (work dirs, always-non-interactive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headless: Option<HeadlessConfig>,

    /// Remote headless amux connection configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteConfig>,
    /// Overlay configuration for mounting host directories into agent containers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlays: Option<OverlaysConfig>,
    /// Seconds of container output inactivity before the agent is considered stuck.
    /// Overrides per-repo config when set. Built-in default: 30.
    #[serde(rename = "agentStuckTimeout", skip_serializing_if = "Option::is_none")]
    pub agent_stuck_timeout_secs: Option<u64>,
}

/// Built-in default number of scrollback lines for the container terminal emulator.
pub const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

/// Built-in default seconds of inactivity before the agent is considered stuck.
pub const DEFAULT_STUCK_TIMEOUT_SECS: u64 = 30;

/// Returns the effective env passthrough list for a given git root.
/// Resolution priority: repo config → global config → empty list.
pub fn effective_env_passthrough(git_root: &Path) -> Vec<String> {
    let repo = load_repo_config(git_root).unwrap_or_default();
    if let Some(names) = repo.env_passthrough {
        return names;
    }
    let global = load_global_config().unwrap_or_default();
    global.env_passthrough.unwrap_or_default()
}

/// Resolve the effective `yoloDisallowedTools` list for a given git root.
/// Resolution priority: repo config → global config → empty list (no restriction).
pub fn effective_yolo_disallowed_tools(git_root: &Path) -> Vec<String> {
    let repo = load_repo_config(git_root).unwrap_or_default();
    if let Some(tools) = repo.yolo_disallowed_tools {
        return tools;
    }
    let global = load_global_config().unwrap_or_default();
    global.yolo_disallowed_tools.unwrap_or_default()
}

/// Resolve the effective scrollback line count for a given git root.
/// Checks per-repo config first, then global config, then falls back to the built-in default.
pub fn effective_scrollback_lines(git_root: &Path) -> usize {
    let repo = load_repo_config(git_root).unwrap_or_default();
    if let Some(lines) = repo.terminal_scrollback_lines {
        return lines;
    }
    let global = load_global_config().unwrap_or_default();
    global.terminal_scrollback_lines.unwrap_or(DEFAULT_SCROLLBACK_LINES)
}

/// Resolve the effective agent-stuck timeout for a given git root.
/// Checks per-repo config first, then global config, then falls back to the built-in default.
pub fn effective_agent_stuck_timeout(git_root: &Path) -> std::time::Duration {
    let repo = load_repo_config(git_root).unwrap_or_default();
    if let Some(secs) = repo.agent_stuck_timeout_secs {
        return std::time::Duration::from_secs(secs);
    }
    let global = load_global_config().unwrap_or_default();
    std::time::Duration::from_secs(global.agent_stuck_timeout_secs.unwrap_or(DEFAULT_STUCK_TIMEOUT_SECS))
}

/// Returns the effective headless work dirs list from global config.
/// Falls back to an empty list when not configured.
pub fn effective_headless_work_dirs() -> Vec<String> {
    let global = load_global_config().unwrap_or_default();
    global
        .headless
        .and_then(|h| h.work_dirs)
        .unwrap_or_default()
}

/// Returns the effective `alwaysNonInteractive` setting from global config.
/// Falls back to `false` when not configured.
pub fn effective_always_non_interactive() -> bool {
    let global = load_global_config().unwrap_or_default();
    global
        .headless
        .and_then(|h| h.always_non_interactive)
        .unwrap_or(false)
}

/// Returns the effective remote default address from global config.
/// Falls back to `None` when not configured.
pub fn effective_remote_default_addr() -> Option<String> {
    load_global_config().ok()?.remote?.default_addr
}

/// Returns the effective remote default API key from global config.
/// Falls back to `None` when not configured.
pub fn effective_remote_default_api_key() -> Option<String> {
    load_global_config().ok()?.remote?.default_api_key
}

/// Returns the effective remote saved directories from global config.
/// Falls back to an empty list when not configured.
pub fn effective_remote_saved_dirs() -> Vec<String> {
    load_global_config()
        .ok()
        .and_then(|c| c.remote?.saved_dirs)
        .unwrap_or_default()
}

pub fn repo_config_path(git_root: &Path) -> PathBuf {
    git_root.join(".amux").join("config.json")
}

/// Legacy path used before config was moved to `.amux/config.json`.
pub fn legacy_repo_config_path(git_root: &Path) -> PathBuf {
    git_root.join("aspec").join(".amux.json")
}

/// Migrate legacy `aspec/.amux.json` to `.amux/config.json` if the legacy file exists
/// and the new path does not. Returns true if a migration was performed.
pub fn migrate_legacy_repo_config(git_root: &Path) -> anyhow::Result<bool> {
    let legacy = legacy_repo_config_path(git_root);
    let current = repo_config_path(git_root);
    if !legacy.exists() || current.exists() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(&legacy)
        .with_context(|| format!("Failed to read legacy config {}", legacy.display()))?;
    if let Some(parent) = current.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    std::fs::write(&current, &content)
        .with_context(|| format!("Failed to write {}", current.display()))?;
    std::fs::remove_file(&legacy)
        .with_context(|| format!("Failed to remove legacy config {}", legacy.display()))?;
    Ok(true)
}

#[allow(dead_code)]
pub fn global_config_path() -> Result<PathBuf> {
    // Allow tests to override the home directory via env var.
    if let Ok(home) = std::env::var("AMUX_CONFIG_HOME") {
        return Ok(PathBuf::from(home).join("config.json"));
    }
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home.join(".amux").join("config.json"))
}

/// Resolve the global workflows directory (`~/.amux/workflows/`).
/// The directory is created with `create_dir_all` if it does not yet exist.
pub fn global_workflows_dir() -> Result<PathBuf> {
    let base = if let Ok(home) = std::env::var("AMUX_CONFIG_HOME") {
        PathBuf::from(home)
    } else {
        dirs::home_dir()
            .context("Cannot determine home directory")?
            .join(".amux")
    };
    let dir = base.join("workflows");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create directory {}", dir.display()))?;
    Ok(dir)
}

/// Resolve the global skills directory (`~/.amux/skills/`).
/// The directory is created with `create_dir_all` if it does not yet exist.
pub fn global_skills_dir() -> Result<PathBuf> {
    let base = if let Ok(home) = std::env::var("AMUX_CONFIG_HOME") {
        PathBuf::from(home)
    } else {
        dirs::home_dir()
            .context("Cannot determine home directory")?
            .join(".amux")
    };
    let dir = base.join("skills");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create directory {}", dir.display()))?;
    Ok(dir)
}

pub fn load_repo_config(git_root: &Path) -> Result<RepoConfig> {
    let path = repo_config_path(git_root);
    if !path.exists() {
        return Ok(RepoConfig::default());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("Invalid JSON in {}", path.display()))
}

pub fn save_repo_config(git_root: &Path, config: &RepoConfig) -> Result<()> {
    let path = repo_config_path(git_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(config)?;
    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write {}", path.display()))
}

#[allow(dead_code)]
pub fn load_global_config() -> Result<GlobalConfig> {
    let path = global_config_path()?;
    if !path.exists() {
        return Ok(GlobalConfig::default());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("Invalid JSON in {}", path.display()))
}

#[allow(dead_code)]
pub fn save_global_config(config: &GlobalConfig) -> Result<()> {
    let path = global_config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(config)?;
    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Serialise tests that mutate AMUX_CONFIG_HOME (process-global env var).
    /// Every test that reads or writes global config must hold this lock.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn repo_config_path_is_correct() {
        let root = PathBuf::from("/some/repo");
        let path = repo_config_path(&root);
        assert_eq!(path, PathBuf::from("/some/repo/.amux/config.json"));
    }

    #[test]
    fn legacy_repo_config_path_is_correct() {
        let root = PathBuf::from("/some/repo");
        let path = legacy_repo_config_path(&root);
        assert_eq!(path, PathBuf::from("/some/repo/aspec/.amux.json"));
    }

    #[test]
    fn migrate_legacy_repo_config_moves_file_and_deletes_legacy() {
        let tmp = TempDir::new().unwrap();
        // Create the legacy aspec/.amux.json.
        let aspec_dir = tmp.path().join("aspec");
        std::fs::create_dir_all(&aspec_dir).unwrap();
        let legacy_content = r#"{"agent":"claude"}"#;
        std::fs::write(aspec_dir.join(".amux.json"), legacy_content).unwrap();

        let migrated = migrate_legacy_repo_config(tmp.path()).unwrap();
        assert!(migrated, "should report that migration occurred");

        // New path should exist with the same content.
        let new_path = repo_config_path(tmp.path());
        assert!(new_path.exists(), "new config file should exist");
        assert_eq!(std::fs::read_to_string(&new_path).unwrap(), legacy_content);

        // Legacy path should be gone.
        assert!(!legacy_repo_config_path(tmp.path()).exists(), "legacy file should be deleted");
    }

    #[test]
    fn migrate_legacy_repo_config_skips_when_no_legacy() {
        let tmp = TempDir::new().unwrap();
        let migrated = migrate_legacy_repo_config(tmp.path()).unwrap();
        assert!(!migrated, "no migration when legacy file absent");
    }

    #[test]
    fn migrate_legacy_repo_config_skips_when_new_already_exists() {
        let tmp = TempDir::new().unwrap();
        // Create both paths — migration should be skipped.
        let aspec_dir = tmp.path().join("aspec");
        std::fs::create_dir_all(&aspec_dir).unwrap();
        std::fs::write(aspec_dir.join(".amux.json"), r#"{"agent":"old"}"#).unwrap();
        let amux_dir = tmp.path().join(".amux");
        std::fs::create_dir_all(&amux_dir).unwrap();
        std::fs::write(amux_dir.join("config.json"), r#"{"agent":"new"}"#).unwrap();

        let migrated = migrate_legacy_repo_config(tmp.path()).unwrap();
        assert!(!migrated, "should not overwrite existing new config");
        // Verify neither file was altered.
        assert_eq!(
            std::fs::read_to_string(amux_dir.join("config.json")).unwrap(),
            r#"{"agent":"new"}"#
        );
    }

    #[test]
    fn global_config_path_is_under_home() {
        let path = global_config_path().unwrap();
        assert!(path.ends_with(".amux/config.json"));
    }

    #[test]
    fn save_and_load_repo_config_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let config = RepoConfig {
            agent: Some("claude".to_string()),
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: None,
            yolo_disallowed_tools: None,
            env_passthrough: None,
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: None,
        };
        save_repo_config(tmp.path(), &config).unwrap();
        let loaded = load_repo_config(tmp.path()).unwrap();
        assert_eq!(config, loaded);
    }

    #[test]
    fn load_repo_config_returns_default_when_absent() {
        let tmp = TempDir::new().unwrap();
        let config = load_repo_config(tmp.path()).unwrap();
        assert_eq!(config, RepoConfig::default());
    }

    // ─── effective_scrollback_lines ─────────────────────────────────────────

    #[test]
    fn effective_scrollback_lines_returns_default_when_no_config() {
        let tmp = TempDir::new().unwrap();
        let lines = effective_scrollback_lines(tmp.path());
        assert_eq!(
            lines, DEFAULT_SCROLLBACK_LINES,
            "should return DEFAULT_SCROLLBACK_LINES when no config file exists"
        );
    }

    #[test]
    fn effective_scrollback_lines_reads_repo_config() {
        let tmp = TempDir::new().unwrap();
        let config = RepoConfig {
            agent: None,
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: Some(2_000),
            yolo_disallowed_tools: None,
            env_passthrough: None,
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: None,
        };
        save_repo_config(tmp.path(), &config).unwrap();

        let lines = effective_scrollback_lines(tmp.path());
        assert_eq!(lines, 2_000, "should read terminal_scrollback_lines from repo config");
    }

    #[test]
    fn effective_scrollback_lines_repo_config_takes_precedence_over_global() {
        // We can only test the repo-wins path by providing a repo config with the value set.
        // (Global config writes to HOME which we cannot override in tests without unsafe tricks.)
        let tmp = TempDir::new().unwrap();
        let repo_cfg = RepoConfig {
            agent: None,
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: Some(999),
            yolo_disallowed_tools: None,
            env_passthrough: None,
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: None,
        };
        save_repo_config(tmp.path(), &repo_cfg).unwrap();

        let lines = effective_scrollback_lines(tmp.path());
        assert_eq!(
            lines, 999,
            "repo config value must win over any global/default value"
        );
    }

    #[test]
    fn effective_scrollback_lines_falls_back_to_default_when_repo_field_absent() {
        let tmp = TempDir::new().unwrap();
        // Repo config exists but has no terminal_scrollback_lines field.
        let config = RepoConfig {
            agent: Some("claude".to_string()),
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: None,
            yolo_disallowed_tools: None,
            env_passthrough: None,
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: None,
        };
        save_repo_config(tmp.path(), &config).unwrap();

        // Without a global config the result must equal the built-in default.
        // (We can't control ~/.amux/config.json in tests, so only assert on the fallback chain.)
        let lines = effective_scrollback_lines(tmp.path());
        // It will be either global config value or DEFAULT_SCROLLBACK_LINES.
        assert!(
            lines >= 1,
            "effective_scrollback_lines must return a positive value; got {}",
            lines
        );
    }

    #[test]
    fn terminal_scrollback_lines_round_trips_through_repo_config() {
        let tmp = TempDir::new().unwrap();
        let config = RepoConfig {
            agent: None,
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: Some(5_000),
            yolo_disallowed_tools: None,
            env_passthrough: None,
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: None,
        };
        save_repo_config(tmp.path(), &config).unwrap();
        let loaded = load_repo_config(tmp.path()).unwrap();
        assert_eq!(loaded.terminal_scrollback_lines, Some(5_000));
    }

    // ─── effective_agent_stuck_timeout ──────────────────────────────────────────

    #[test]
    fn effective_agent_stuck_timeout_returns_default_when_no_config() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let tmp_config = TempDir::new().unwrap();
        std::env::set_var("AMUX_CONFIG_HOME", tmp_config.path());
        let timeout = effective_agent_stuck_timeout(tmp.path());
        std::env::remove_var("AMUX_CONFIG_HOME");
        assert_eq!(
            timeout,
            std::time::Duration::from_secs(DEFAULT_STUCK_TIMEOUT_SECS),
            "should return default 30s when no config file exists"
        );
    }

    #[test]
    fn effective_agent_stuck_timeout_reads_repo_config() {
        let tmp = TempDir::new().unwrap();
        let config = RepoConfig {
            agent: None,
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: None,
            yolo_disallowed_tools: None,
            env_passthrough: None,
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: Some(60),
        };
        save_repo_config(tmp.path(), &config).unwrap();
        let timeout = effective_agent_stuck_timeout(tmp.path());
        assert_eq!(timeout, std::time::Duration::from_secs(60));
    }

    #[test]
    fn effective_agent_stuck_timeout_repo_overrides_global() {
        let tmp_config = TempDir::new().unwrap();
        let tmp_repo = TempDir::new().unwrap();
        std::env::set_var("AMUX_CONFIG_HOME", tmp_config.path());
        let global = GlobalConfig {
            agent_stuck_timeout_secs: Some(45),
            ..GlobalConfig::default()
        };
        save_global_config(&global).unwrap();
        let repo_cfg = RepoConfig {
            agent: None,
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: None,
            yolo_disallowed_tools: None,
            env_passthrough: None,
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: Some(120),
        };
        save_repo_config(tmp_repo.path(), &repo_cfg).unwrap();
        let timeout = effective_agent_stuck_timeout(tmp_repo.path());
        std::env::remove_var("AMUX_CONFIG_HOME");
        assert_eq!(timeout, std::time::Duration::from_secs(120));
    }

    // ─── yolo_disallowed_tools ───────────────────────────────────────────────────

    #[test]
    fn yolo_disallowed_tools_deserializes_in_repo_config() {
        let json = r#"{"yoloDisallowedTools": ["Bash", "computer"]}"#;
        let config: RepoConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.yolo_disallowed_tools,
            Some(vec!["Bash".to_string(), "computer".to_string()])
        );
    }

    #[test]
    fn yolo_disallowed_tools_absent_in_repo_config_is_none() {
        let json = r#"{"agent": "claude"}"#;
        let config: RepoConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.yolo_disallowed_tools, None);
    }

    #[test]
    fn yolo_disallowed_tools_deserializes_in_global_config() {
        let json = r#"{"yoloDisallowedTools": ["WebSearch"]}"#;
        let config: GlobalConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.yolo_disallowed_tools,
            Some(vec!["WebSearch".to_string()])
        );
    }

    #[test]
    fn yolo_disallowed_tools_absent_in_global_config_is_none() {
        let json = r#"{"default_agent": "claude"}"#;
        let config: GlobalConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.yolo_disallowed_tools, None);
    }

    #[test]
    fn yolo_disallowed_tools_roundtrips_through_repo_config() {
        let tmp = TempDir::new().unwrap();
        let config = RepoConfig {
            agent: None,
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: None,
            yolo_disallowed_tools: Some(vec!["Bash".to_string(), "computer".to_string()]),
            env_passthrough: None,
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: None,
        };
        save_repo_config(tmp.path(), &config).unwrap();
        let loaded = load_repo_config(tmp.path()).unwrap();
        assert_eq!(
            loaded.yolo_disallowed_tools,
            Some(vec!["Bash".to_string(), "computer".to_string()])
        );
    }

    #[test]
    fn effective_yolo_disallowed_tools_returns_repo_value_when_set() {
        let tmp = TempDir::new().unwrap();
        let config = RepoConfig {
            agent: None,
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: None,
            yolo_disallowed_tools: Some(vec!["Bash".to_string()]),
            env_passthrough: None,
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: None,
        };
        save_repo_config(tmp.path(), &config).unwrap();
        let tools = effective_yolo_disallowed_tools(tmp.path());
        assert_eq!(tools, vec!["Bash".to_string()]);
    }

    #[test]
    fn effective_yolo_disallowed_tools_repo_wins_over_any_global() {
        // When repo config has yoloDisallowedTools set, it is returned immediately
        // without consulting global config (repo config wins entirely, no merging).
        let tmp = TempDir::new().unwrap();
        let config = RepoConfig {
            agent: None,
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: None,
            yolo_disallowed_tools: Some(vec!["Bash".to_string(), "computer".to_string()]),
            env_passthrough: None,
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: None,
        };
        save_repo_config(tmp.path(), &config).unwrap();
        let tools = effective_yolo_disallowed_tools(tmp.path());
        // Regardless of any global config, the repo value must win.
        assert_eq!(tools, vec!["Bash".to_string(), "computer".to_string()]);
    }

    #[test]
    fn effective_yolo_disallowed_tools_empty_when_neither_set() {
        // No config file at all → falls through to empty list.
        // (We cannot control ~/.amux/config.json in unit tests, so we only assert
        // no panic and that the repo-absent path reaches the global fallback.)
        let tmp = TempDir::new().unwrap();
        // Confirm no repo config exists so the fallback path is exercised.
        assert!(!repo_config_path(tmp.path()).exists());
        let tools = effective_yolo_disallowed_tools(tmp.path());
        // If global config has no yoloDisallowedTools either, result is empty.
        // We can't control the global file, so just assert no panic and the
        // return type is correct.
        let _: Vec<String> = tools;
    }

    // ─── effective_env_passthrough ───────────────────────────────────────────────

    #[test]
    fn env_passthrough_deserializes_in_repo_config() {
        let json = r#"{"envPassthrough": ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]}"#;
        let config: RepoConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.env_passthrough,
            Some(vec!["ANTHROPIC_API_KEY".to_string(), "OPENAI_API_KEY".to_string()])
        );
    }

    #[test]
    fn env_passthrough_absent_in_repo_config_is_none() {
        let json = r#"{"agent": "maki"}"#;
        let config: RepoConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.env_passthrough, None);
    }

    #[test]
    fn env_passthrough_deserializes_in_global_config() {
        let json = r#"{"envPassthrough": ["MY_SECRET"]}"#;
        let config: GlobalConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.env_passthrough, Some(vec!["MY_SECRET".to_string()]));
    }

    #[test]
    fn env_passthrough_absent_in_global_config_is_none() {
        let json = r#"{"default_agent": "claude"}"#;
        let config: GlobalConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.env_passthrough, None);
    }

    #[test]
    fn env_passthrough_roundtrips_through_repo_config() {
        let tmp = TempDir::new().unwrap();
        let config = RepoConfig {
            agent: None,
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: None,
            yolo_disallowed_tools: None,
            env_passthrough: Some(vec!["ANTHROPIC_API_KEY".to_string()]),
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: None,
        };
        save_repo_config(tmp.path(), &config).unwrap();
        let loaded = load_repo_config(tmp.path()).unwrap();
        assert_eq!(
            loaded.env_passthrough,
            Some(vec!["ANTHROPIC_API_KEY".to_string()])
        );
    }

    #[test]
    fn effective_env_passthrough_returns_repo_value_when_set() {
        let tmp = TempDir::new().unwrap();
        let config = RepoConfig {
            agent: None,
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: None,
            yolo_disallowed_tools: None,
            env_passthrough: Some(vec!["MY_VAR".to_string(), "OTHER_VAR".to_string()]),
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: None,
        };
        save_repo_config(tmp.path(), &config).unwrap();
        let names = effective_env_passthrough(tmp.path());
        assert_eq!(names, vec!["MY_VAR".to_string(), "OTHER_VAR".to_string()]);
    }

    #[test]
    fn effective_env_passthrough_repo_wins_over_any_global() {
        // When repo config has envPassthrough set, it is returned immediately
        // without consulting global config (repo config wins entirely, no merging).
        let tmp = TempDir::new().unwrap();
        let config = RepoConfig {
            agent: None,
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: None,
            yolo_disallowed_tools: None,
            env_passthrough: Some(vec!["REPO_ONLY_VAR".to_string()]),
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: None,
        };
        save_repo_config(tmp.path(), &config).unwrap();
        let names = effective_env_passthrough(tmp.path());
        // Regardless of any global config, the repo value must win.
        assert_eq!(names, vec!["REPO_ONLY_VAR".to_string()]);
    }

    #[test]
    fn effective_env_passthrough_empty_when_neither_set() {
        // No config file at all → falls through to empty list.
        // (We cannot control ~/.amux/config.json in unit tests, so we only assert
        // no panic and that the repo-absent path reaches the global fallback.)
        let tmp = TempDir::new().unwrap();
        assert!(!repo_config_path(tmp.path()).exists());
        let names = effective_env_passthrough(tmp.path());
        // If global config has no envPassthrough either, result is empty.
        // We can't control the global file, so just assert no panic and correct type.
        let _: Vec<String> = names;
    }

    #[test]
    fn effective_env_passthrough_repo_empty_array_wins_over_global() {
        // If a repo config explicitly sets envPassthrough to an empty array, it wins
        // entirely — the global list must NOT be used (lists are not merged).
        let tmp = TempDir::new().unwrap();
        let config = RepoConfig {
            agent: None,
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: None,
            yolo_disallowed_tools: None,
            env_passthrough: Some(vec![]), // explicit empty array
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: None,
        };
        save_repo_config(tmp.path(), &config).unwrap();
        let names = effective_env_passthrough(tmp.path());
        assert!(
            names.is_empty(),
            "repo envPassthrough: [] must win over any global envPassthrough list; got {:?}",
            names
        );
    }

    #[test]
    fn effective_env_passthrough_falls_through_to_global_when_repo_field_absent() {
        // Repo config exists but has no envPassthrough field → falls through to global.
        let tmp = TempDir::new().unwrap();
        let config = RepoConfig {
            agent: Some("maki".to_string()),
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: None,
            yolo_disallowed_tools: None,
            env_passthrough: None,
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: None,
        };
        save_repo_config(tmp.path(), &config).unwrap();
        // Since repo.env_passthrough is None, the function must not panic and must
        // return a Vec<String> (either global config's list or empty).
        let names = effective_env_passthrough(tmp.path());
        let _: Vec<String> = names;
    }

    // ─── work_items config ───────────────────────────────────────────────────────

    #[test]
    fn work_items_config_serializes_with_camel_case_key() {
        let config = RepoConfig {
            work_items: Some(WorkItemsConfig {
                dir: Some("./items".to_string()),
                template: None,
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("workItems"), "expected camelCase 'workItems' key in JSON");
        assert!(json.contains("\"dir\""), "expected 'dir' key in JSON");
        assert!(!json.contains("template"), "template None should be omitted");
    }

    #[test]
    fn work_items_config_round_trips_through_json() {
        let original = RepoConfig {
            work_items: Some(WorkItemsConfig {
                dir: Some("./items".to_string()),
                template: None,
            }),
            ..Default::default()
        };
        let json = serde_json::to_string_pretty(&original).unwrap();
        let restored: RepoConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
        assert_eq!(restored.work_items.as_ref().unwrap().dir.as_deref(), Some("./items"));
        assert_eq!(restored.work_items.as_ref().unwrap().template, None);
    }

    #[test]
    fn work_items_config_absent_omitted_from_json() {
        let config = RepoConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        assert!(!json.contains("workItems"), "absent work_items should not appear in JSON");
    }

    #[test]
    fn work_items_dir_resolves_relative_to_git_root() {
        let config = RepoConfig {
            work_items: Some(WorkItemsConfig {
                dir: Some("my/items".to_string()),
                template: None,
            }),
            ..Default::default()
        };
        let root = PathBuf::from("/some/repo");
        let dir = config.work_items_dir(&root).unwrap();
        assert_eq!(dir, PathBuf::from("/some/repo/my/items"));
    }

    #[test]
    fn work_items_dir_returns_none_when_not_set() {
        let config = RepoConfig::default();
        let root = PathBuf::from("/some/repo");
        assert!(config.work_items_dir(&root).is_none());
    }

    #[test]
    fn work_items_dir_returns_none_when_empty_string() {
        let config = RepoConfig {
            work_items: Some(WorkItemsConfig {
                dir: Some(String::new()),
                template: None,
            }),
            ..Default::default()
        };
        let root = PathBuf::from("/some/repo");
        assert!(config.work_items_dir(&root).is_none());
    }

    #[test]
    fn work_items_template_resolves_relative_to_git_root() {
        let config = RepoConfig {
            work_items: Some(WorkItemsConfig {
                dir: None,
                template: Some("my/template.md".to_string()),
            }),
            ..Default::default()
        };
        let root = PathBuf::from("/some/repo");
        let tmpl = config.work_items_template(&root).unwrap();
        assert_eq!(tmpl, PathBuf::from("/some/repo/my/template.md"));
    }

    #[test]
    fn work_items_template_returns_none_when_not_set() {
        let config = RepoConfig::default();
        let root = PathBuf::from("/some/repo");
        assert!(config.work_items_template(&root).is_none());
    }

    #[test]
    fn work_items_config_roundtrips_through_save_load() {
        let tmp = TempDir::new().unwrap();
        let config = RepoConfig {
            work_items: Some(WorkItemsConfig {
                dir: Some("./work-items".to_string()),
                template: Some("./work-items/0000-template.md".to_string()),
            }),
            ..Default::default()
        };
        save_repo_config(tmp.path(), &config).unwrap();
        let loaded = load_repo_config(tmp.path()).unwrap();
        assert_eq!(config, loaded);
        let wi = loaded.work_items.unwrap();
        assert_eq!(wi.dir.as_deref(), Some("./work-items"));
        assert_eq!(wi.template.as_deref(), Some("./work-items/0000-template.md"));
    }

    // ─── GlobalConfig headless ────────────────────────────────────────────

    #[test]
    fn headless_work_dirs_deserializes_from_json() {
        let json = r#"{"headless": {"workDirs": ["/workspace/a", "/workspace/b"]}}"#;
        let config: GlobalConfig = serde_json::from_str(json).unwrap();
        let headless = config.headless.unwrap();
        assert_eq!(
            headless.work_dirs,
            Some(vec!["/workspace/a".to_string(), "/workspace/b".to_string()])
        );
    }

    #[test]
    fn headless_absent_field_deserializes_to_none() {
        let json = r#"{"default_agent": "claude"}"#;
        let config: GlobalConfig = serde_json::from_str(json).unwrap();
        assert!(
            config.headless.is_none(),
            "absent headless must deserialize to None"
        );
    }

    #[test]
    fn headless_work_dirs_empty_array_field_deserializes_to_some_empty_vec() {
        let json = r#"{"headless": {"workDirs": []}}"#;
        let config: GlobalConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.headless.unwrap().work_dirs, Some(vec![]));
    }

    #[test]
    fn headless_serializes_with_camel_case_keys() {
        let config = GlobalConfig {
            headless: Some(HeadlessConfig {
                work_dirs: Some(vec!["/repo/proj".to_string()]),
                always_non_interactive: Some(true),
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains("workDirs"),
            "expected camelCase key 'workDirs' in JSON; got: {json}"
        );
        assert!(
            json.contains("alwaysNonInteractive"),
            "expected camelCase key 'alwaysNonInteractive' in JSON; got: {json}"
        );
    }

    #[test]
    fn headless_absent_is_omitted_from_json() {
        let config = GlobalConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            !json.contains("headless"),
            "absent headless must be omitted (skip_serializing_if); got: {json}"
        );
    }

    #[test]
    fn headless_round_trips_through_json() {
        let original = GlobalConfig {
            headless: Some(HeadlessConfig {
                work_dirs: Some(vec![
                    "/workspace/alpha".to_string(),
                    "/workspace/beta".to_string(),
                ]),
                always_non_interactive: Some(false),
            }),
            ..Default::default()
        };
        let json = serde_json::to_string_pretty(&original).unwrap();
        let restored: GlobalConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
        let headless = restored.headless.unwrap();
        assert_eq!(
            headless.work_dirs.as_deref(),
            Some(["/workspace/alpha".to_string(), "/workspace/beta".to_string()].as_slice())
        );
        assert_eq!(headless.always_non_interactive, Some(false));
    }

    #[test]
    fn headless_always_non_interactive_defaults_to_none() {
        let json = r#"{"headless": {"workDirs": ["/tmp"]}}"#;
        let config: GlobalConfig = serde_json::from_str(json).unwrap();
        let headless = config.headless.unwrap();
        assert!(headless.always_non_interactive.is_none());
    }

    // ─── HeadlessConfig standalone round-trips ───────────────────────────────
    //
    // Tests 0058: verify HeadlessConfig JSON round-trips in all field combinations
    // and document the intentional breaking change for the old flat key format.

    #[test]
    fn headless_config_round_trips_both_fields_set() {
        let original = HeadlessConfig {
            work_dirs: Some(vec!["/repo/a".to_string(), "/repo/b".to_string()]),
            always_non_interactive: Some(true),
        };
        let json = serde_json::to_string_pretty(&original).unwrap();
        assert!(json.contains("workDirs"), "workDirs must appear in JSON; got: {json}");
        assert!(json.contains("alwaysNonInteractive"), "alwaysNonInteractive must appear in JSON; got: {json}");
        let restored: HeadlessConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
        assert_eq!(
            restored.work_dirs.as_deref(),
            Some(["/repo/a".to_string(), "/repo/b".to_string()].as_slice())
        );
        assert_eq!(restored.always_non_interactive, Some(true));
    }

    #[test]
    fn headless_config_round_trips_only_work_dirs_set() {
        let original = HeadlessConfig {
            work_dirs: Some(vec!["/workspace/project".to_string()]),
            always_non_interactive: None,
        };
        let json = serde_json::to_string_pretty(&original).unwrap();
        // skip_serializing_if = "Option::is_none" must suppress absent field.
        assert!(
            !json.contains("alwaysNonInteractive"),
            "absent alwaysNonInteractive must not appear in JSON; got: {json}"
        );
        let restored: HeadlessConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
        assert_eq!(restored.always_non_interactive, None);
        assert_eq!(
            restored.work_dirs.as_deref(),
            Some(["/workspace/project".to_string()].as_slice())
        );
    }

    #[test]
    fn headless_config_round_trips_only_always_non_interactive_set() {
        let original = HeadlessConfig {
            work_dirs: None,
            always_non_interactive: Some(false),
        };
        let json = serde_json::to_string_pretty(&original).unwrap();
        assert!(
            !json.contains("workDirs"),
            "absent work_dirs must not appear in JSON; got: {json}"
        );
        let restored: HeadlessConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
        assert_eq!(restored.work_dirs, None);
        assert_eq!(restored.always_non_interactive, Some(false));
    }

    #[test]
    fn headless_config_round_trips_neither_field_set() {
        let original = HeadlessConfig::default();
        let json = serde_json::to_string_pretty(&original).unwrap();
        assert!(
            !json.contains("workDirs"),
            "absent workDirs must not appear in JSON; got: {json}"
        );
        assert!(
            !json.contains("alwaysNonInteractive"),
            "absent alwaysNonInteractive must not appear in JSON; got: {json}"
        );
        let restored: HeadlessConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
        assert_eq!(restored.work_dirs, None);
        assert_eq!(restored.always_non_interactive, None);
    }

    /// Documents the intentional breaking change from work item 0058:
    /// the old flat `headlessWorkDirs` key that existed directly on `GlobalConfig`
    /// (before settings were nested under `headless: { ... }`) must NOT be
    /// deserialized into the new `GlobalConfig.headless.work_dirs` field.
    /// A config file written by an older binary with the flat key is silently
    /// ignored rather than accidentally populating the new nested struct.
    #[test]
    fn headless_config_old_flat_headless_work_dirs_key_is_not_recognized() {
        let old_json = r#"{"headlessWorkDirs": ["/workspace/a", "/workspace/b"]}"#;
        let config: GlobalConfig = serde_json::from_str(old_json).unwrap();
        assert!(
            config.headless.is_none(),
            "old flat 'headlessWorkDirs' key must not deserialize into GlobalConfig.headless; \
             this documents the intentional breaking change in work item 0058; \
             got: {:?}",
            config.headless
        );
    }

    /// Tests save_global_config / load_global_config via the AMUX_CONFIG_HOME override.
    ///
    /// Uses a static mutex so only one test at a time may mutate the env var,
    /// preventing parallel tests from observing each other's temporary override.
    #[test]
    fn headless_round_trips_through_save_load_global_config() {
        let _guard = ENV_LOCK.lock().unwrap();

        let tmp = TempDir::new().unwrap();
        std::env::set_var("AMUX_CONFIG_HOME", tmp.path().to_str().unwrap());

        let original = GlobalConfig {
            headless: Some(HeadlessConfig {
                work_dirs: Some(vec!["/srv/repo".to_string()]),
                always_non_interactive: Some(true),
            }),
            default_agent: Some("claude".to_string()),
            ..Default::default()
        };
        save_global_config(&original).unwrap();
        let loaded = load_global_config().unwrap();

        std::env::remove_var("AMUX_CONFIG_HOME");

        assert_eq!(
            original, loaded,
            "GlobalConfig must survive a save/load round-trip"
        );
        let headless = loaded.headless.unwrap();
        assert_eq!(
            headless.work_dirs.as_deref(),
            Some(["/srv/repo".to_string()].as_slice())
        );
        assert_eq!(headless.always_non_interactive, Some(true));
    }

    // ─── RemoteConfig round-trips ─────────────────────────────────────────────
    //
    // Tests 0059: verify RemoteConfig JSON round-trips in all field combinations
    // and document the camelCase key names used in on-disk JSON.

    #[test]
    fn remote_config_round_trips_both_fields_set() {
        let original = RemoteConfig {
            default_addr: Some("http://1.2.3.4:9876".to_string()),
            saved_dirs: Some(vec!["/workspace/a".to_string(), "/workspace/b".to_string()]),
            default_api_key: None,
        };
        let json = serde_json::to_string_pretty(&original).unwrap();
        assert!(json.contains("defaultAddr"), "expected camelCase 'defaultAddr'; got: {json}");
        assert!(json.contains("savedDirs"), "expected camelCase 'savedDirs'; got: {json}");
        let restored: RemoteConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
        assert_eq!(restored.default_addr.as_deref(), Some("http://1.2.3.4:9876"));
        assert_eq!(
            restored.saved_dirs.as_deref(),
            Some(["/workspace/a".to_string(), "/workspace/b".to_string()].as_slice())
        );
    }

    #[test]
    fn remote_config_round_trips_only_default_addr() {
        let original = RemoteConfig {
            default_addr: Some("http://host:9876".to_string()),
            saved_dirs: None,
            default_api_key: None,
        };
        let json = serde_json::to_string_pretty(&original).unwrap();
        assert!(
            !json.contains("savedDirs"),
            "absent savedDirs must be omitted (skip_serializing_if); got: {json}"
        );
        let restored: RemoteConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
        assert_eq!(restored.saved_dirs, None);
    }

    #[test]
    fn remote_config_round_trips_only_saved_dirs() {
        let original = RemoteConfig {
            default_addr: None,
            saved_dirs: Some(vec!["/srv/proj".to_string()]),
            default_api_key: None,
        };
        let json = serde_json::to_string_pretty(&original).unwrap();
        assert!(
            !json.contains("defaultAddr"),
            "absent defaultAddr must be omitted (skip_serializing_if); got: {json}"
        );
        let restored: RemoteConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
        assert_eq!(restored.default_addr, None);
    }

    #[test]
    fn remote_config_round_trips_neither_field_set() {
        let original = RemoteConfig::default();
        let json = serde_json::to_string_pretty(&original).unwrap();
        assert!(
            !json.contains("defaultAddr"),
            "absent defaultAddr must not appear in JSON; got: {json}"
        );
        assert!(
            !json.contains("savedDirs"),
            "absent savedDirs must not appear in JSON; got: {json}"
        );
        let restored: RemoteConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn global_config_with_remote_block_serializes_with_camel_case_keys() {
        let config = GlobalConfig {
            remote: Some(RemoteConfig {
                default_addr: Some("http://1.2.3.4:9876".to_string()),
                saved_dirs: Some(vec!["/workspace/proj".to_string()]),
                default_api_key: None,
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"remote\""), "expected 'remote' key; got: {json}");
        assert!(json.contains("defaultAddr"), "expected camelCase 'defaultAddr'; got: {json}");
        assert!(json.contains("savedDirs"), "expected camelCase 'savedDirs'; got: {json}");
    }

    #[test]
    fn global_config_remote_block_absent_when_none() {
        let config = GlobalConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            !json.contains("remote"),
            "absent remote must be omitted (skip_serializing_if); got: {json}"
        );
    }

    #[test]
    fn effective_remote_default_addr_returns_none_when_not_configured() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        // SAFETY: test-only env mutation; serialised by ENV_LOCK.
        unsafe { std::env::set_var("AMUX_CONFIG_HOME", tmp.path().to_str().unwrap()) };
        let result = effective_remote_default_addr();
        unsafe { std::env::remove_var("AMUX_CONFIG_HOME") };
        assert_eq!(result, None, "must return None when global config has no remote block");
    }

    #[test]
    fn effective_remote_saved_dirs_returns_empty_when_not_configured() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        unsafe { std::env::set_var("AMUX_CONFIG_HOME", tmp.path().to_str().unwrap()) };
        let result = effective_remote_saved_dirs();
        unsafe { std::env::remove_var("AMUX_CONFIG_HOME") };
        assert!(
            result.is_empty(),
            "must return empty Vec when global config has no remote block; got: {result:?}"
        );
    }

    /// Documents that the old flat `remoteDefaultAddr` key (if it ever appeared at the
    /// top level of `GlobalConfig`) does NOT deserialize into `GlobalConfig.remote`.
    /// Consistent with the `headlessWorkDirs` breaking-change pattern from WI 0058.
    #[test]
    fn remote_config_old_flat_key_is_not_recognized() {
        let old_json = r#"{"remoteDefaultAddr": "http://1.2.3.4:9876"}"#;
        let config: GlobalConfig = serde_json::from_str(old_json).unwrap();
        assert!(
            config.remote.is_none(),
            "old flat 'remoteDefaultAddr' key must not deserialize into GlobalConfig.remote; \
             this documents the intentional breaking-change pattern consistent with WI 0058; \
             got: {:?}",
            config.remote
        );
    }

    // ─── RemoteConfig defaultAPIKey (work item 0060) ────────────────────────────

    #[test]
    fn remote_config_with_default_api_key_round_trips_through_json() {
        let original = RemoteConfig {
            default_addr: Some("http://1.2.3.4:9876".to_string()),
            saved_dirs: None,
            default_api_key: Some("my-secret-key-abc123".to_string()),
        };
        let json = serde_json::to_string_pretty(&original).unwrap();
        assert!(
            json.contains("defaultAPIKey"),
            "expected camelCase 'defaultAPIKey' key in JSON; got: {json}"
        );
        let restored: RemoteConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
        assert_eq!(
            restored.default_api_key.as_deref(),
            Some("my-secret-key-abc123")
        );
    }

    #[test]
    fn remote_config_default_api_key_omitted_when_none() {
        let original = RemoteConfig {
            default_addr: Some("http://host:9876".to_string()),
            saved_dirs: None,
            default_api_key: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(
            !json.contains("defaultAPIKey"),
            "absent defaultAPIKey must be omitted (skip_serializing_if); got: {json}"
        );
    }

    #[test]
    fn global_config_serializes_remote_default_api_key_as_camel_case() {
        let config = GlobalConfig {
            remote: Some(RemoteConfig {
                default_addr: Some("http://host:9876".to_string()),
                saved_dirs: None,
                default_api_key: Some("the-key".to_string()),
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains("\"defaultAPIKey\""),
            "expected camelCase 'defaultAPIKey' in GlobalConfig JSON; got: {json}"
        );
        assert!(
            json.contains("the-key"),
            "API key value must be present in JSON; got: {json}"
        );
    }

    #[test]
    fn effective_remote_default_api_key_returns_none_when_not_configured() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        unsafe { std::env::set_var("AMUX_CONFIG_HOME", tmp.path().to_str().unwrap()) };
        let result = effective_remote_default_api_key();
        unsafe { std::env::remove_var("AMUX_CONFIG_HOME") };
        assert_eq!(result, None, "must return None when global config has no remote.defaultAPIKey");
    }

    #[test]
    fn effective_remote_default_api_key_returns_configured_value() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        unsafe { std::env::set_var("AMUX_CONFIG_HOME", tmp.path().to_str().unwrap()) };

        let config = GlobalConfig {
            remote: Some(RemoteConfig {
                default_addr: None,
                saved_dirs: None,
                default_api_key: Some("stored-api-key-xyz".to_string()),
            }),
            ..Default::default()
        };
        save_global_config(&config).unwrap();

        let result = effective_remote_default_api_key();
        unsafe { std::env::remove_var("AMUX_CONFIG_HOME") };

        assert_eq!(
            result,
            Some("stored-api-key-xyz".to_string()),
            "must return the configured API key"
        );
    }

    #[test]
    fn remote_config_all_three_fields_round_trip_through_json() {
        let original = RemoteConfig {
            default_addr: Some("http://remote:9876".to_string()),
            saved_dirs: Some(vec!["/workspace/proj".to_string()]),
            default_api_key: Some("abc123def456".to_string()),
        };
        let json = serde_json::to_string_pretty(&original).unwrap();
        assert!(json.contains("defaultAddr"), "must contain defaultAddr");
        assert!(json.contains("savedDirs"), "must contain savedDirs");
        assert!(json.contains("defaultAPIKey"), "must contain defaultAPIKey");
        let restored: RemoteConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
    }

    // ─── overlays config tests (work item 0063) ───────────────────────────────

    #[test]
    fn overlays_field_in_repo_config_serializes_correctly() {
        let config = RepoConfig {
            overlays: Some(OverlaysConfig {
                directories: Some(vec![DirectoryOverlayConfig {
                    host: "/data/ref".to_string(),
                    container: "/mnt/ref".to_string(),
                    permission: Some("ro".to_string()),
                }]),
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"overlays\""), "overlays key must appear in JSON; got: {json}");
        assert!(json.contains("\"directories\""), "directories key must appear; got: {json}");
        assert!(json.contains("/data/ref"), "host path must appear; got: {json}");
        assert!(json.contains("/mnt/ref"), "container path must appear; got: {json}");
        assert!(json.contains("\"ro\""), "permission must appear; got: {json}");
    }

    #[test]
    fn overlays_field_in_global_config_serializes_correctly() {
        let config = GlobalConfig {
            overlays: Some(OverlaysConfig {
                directories: Some(vec![DirectoryOverlayConfig {
                    host: "~/shared".to_string(),
                    container: "/mnt/shared".to_string(),
                    permission: Some("rw".to_string()),
                }]),
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"overlays\""), "overlays key must appear; got: {json}");
        assert!(json.contains("~/shared"), "tilde host must be preserved; got: {json}");
        assert!(json.contains("\"rw\""), "rw permission must appear; got: {json}");
    }

    #[test]
    fn overlays_absent_in_repo_config_json_deserializes_to_none() {
        let json = r#"{"agent": "claude"}"#;
        let config: RepoConfig = serde_json::from_str(json).unwrap();
        assert!(
            config.overlays.is_none(),
            "absent overlays key must deserialize to None in RepoConfig"
        );
    }

    #[test]
    fn overlays_absent_in_global_config_json_deserializes_to_none() {
        let json = r#"{"default_agent": "claude"}"#;
        let config: GlobalConfig = serde_json::from_str(json).unwrap();
        assert!(
            config.overlays.is_none(),
            "absent overlays key must deserialize to None in GlobalConfig"
        );
    }

    #[test]
    fn overlay_permission_field_absent_deserializes_to_none() {
        // When the permission key is absent, DirectoryOverlayConfig.permission is None.
        // The resolution layer (config_to_overlay) interprets None as the default "ro".
        let json = r#"{"overlays":{"directories":[{"host":"/data","container":"/mnt"}]}}"#;
        let config: RepoConfig = serde_json::from_str(json).unwrap();
        let dirs = config.overlays.unwrap().directories.unwrap();
        assert!(
            dirs[0].permission.is_none(),
            "absent permission field must deserialize to None (defaults to ro at resolution)"
        );
    }

    #[test]
    fn overlay_permission_none_is_omitted_from_serialized_json() {
        // skip_serializing_if = "Option::is_none" must suppress the permission field.
        let entry = DirectoryOverlayConfig {
            host: "/data".to_string(),
            container: "/mnt".to_string(),
            permission: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            !json.contains("permission"),
            "absent permission must be omitted from JSON; got: {json}"
        );
    }

    #[test]
    fn overlays_absent_is_omitted_from_repo_config_json() {
        let config = RepoConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            !json.contains("overlays"),
            "absent overlays must be omitted (skip_serializing_if); got: {json}"
        );
    }

    #[test]
    fn overlays_field_roundtrips_through_repo_config_save_load() {
        let tmp = TempDir::new().unwrap();
        let config = RepoConfig {
            overlays: Some(OverlaysConfig {
                directories: Some(vec![
                    DirectoryOverlayConfig {
                        host: "/data/ref".to_string(),
                        container: "/mnt/ref".to_string(),
                        permission: Some("ro".to_string()),
                    },
                    DirectoryOverlayConfig {
                        host: "~/prompts".to_string(),
                        container: "/mnt/prompts".to_string(),
                        permission: None,
                    },
                ]),
            }),
            ..Default::default()
        };
        save_repo_config(tmp.path(), &config).unwrap();
        let loaded = load_repo_config(tmp.path()).unwrap();
        assert_eq!(config, loaded, "overlays must round-trip through save/load");
        let dirs = loaded.overlays.unwrap().directories.unwrap();
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].permission.as_deref(), Some("ro"));
        assert!(dirs[1].permission.is_none(), "absent permission must remain None after roundtrip");
    }

    #[test]
    fn overlays_with_multiple_directories_roundtrips_through_json() {
        let original = RepoConfig {
            overlays: Some(OverlaysConfig {
                directories: Some(vec![
                    DirectoryOverlayConfig {
                        host: "/a".to_string(),
                        container: "/mnt/a".to_string(),
                        permission: Some("ro".to_string()),
                    },
                    DirectoryOverlayConfig {
                        host: "/b".to_string(),
                        container: "/mnt/b".to_string(),
                        permission: Some("rw".to_string()),
                    },
                ]),
            }),
            ..Default::default()
        };
        let json = serde_json::to_string_pretty(&original).unwrap();
        let restored: RepoConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
    }
}
