//! `engine::overlay` — `OverlayEngine`.
//!
//! Consolidates overlay construction and management. Layer 0 *resolves* host
//! paths; this layer *builds* the resolved overlay specs that
//! `ContainerOption::Overlay` accepts. Replaces `oldsrc/overlays/` and the
//! agent-settings-passthrough bits of `oldsrc/passthrough.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::data::fs::auth_paths::AuthPathResolver;
use crate::data::fs::overlay_paths::OverlayPathResolver;
use crate::data::session::{AgentName, Session};
use crate::engine::container::options::{OverlayPermission, OverlaySpec};
use crate::engine::error::EngineError;

/// Top-level entries in `~/.claude/` that the legacy code excludes when
/// preparing a sanitized overlay copy. Single source of truth.
pub const CLAUDE_DENYLIST: &[&str] = &[
    "projects",
    "sessions",
    "session-env",
    "debug",
    "file-history",
    "history.jsonl",
    "telemetry",
    "downloads",
    "ide",
    "shell-snapshots",
    "paste-cache",
];

/// Description of "overlays I want for this command, with these flags".
#[derive(Debug, Default, Clone)]
pub struct OverlayRequest {
    /// Inline directory specs (host:container[:perm]).
    pub directories: Vec<DirectorySpec>,
    /// When true, mount the global amux skills directory into the agent's
    /// native skills/commands path inside the container.
    pub include_skills: bool,
    /// Whether to include agent-settings overlays for `agent`. When `Some`
    /// the engine prepares per-agent host configs (e.g. `~/.claude.json`).
    pub agent: Option<AgentName>,
    /// When `true`, write `skipDangerousModePermissionPrompt: true` into the
    /// prepared Claude `settings.json` (Yolo mode).
    pub yolo: bool,
    /// Override container `$HOME` (defaults to `/root`).
    pub container_home: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectorySpec {
    pub host: String,
    pub container: String,
    pub permission: OverlayPermission,
}

/// Resolved directory overlay (after canonicalization + tilde expansion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryOverlay {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub permission: OverlayPermission,
}

#[derive(Debug)]
pub struct OverlayEngine {
    auth_resolver: AuthPathResolver,
    /// Sanitized temp directories that back agent-settings overlays. Held
    /// here so the directories live as long as this engine instance and are
    /// removed on `Drop` (RAII via `tempfile::TempDir`). This prevents the
    /// sanitized `~/.claude.json` and copied `~/.claude/` contents from
    /// leaking to `/tmp` after process exit.
    sanitized: std::sync::Mutex<Vec<tempfile::TempDir>>,
}

impl OverlayEngine {
    pub fn new(_session: &Session) -> Result<Self, EngineError> {
        let auth_resolver = AuthPathResolver::from_process_env().map_err(EngineError::Data)?;
        Ok(Self {
            auth_resolver,
            sanitized: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub fn with_auth_resolver(auth_resolver: AuthPathResolver) -> Self {
        Self {
            auth_resolver,
            sanitized: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Track a sanitized tempdir so its cleanup is deferred until this
    /// engine is dropped.
    fn retain_tempdir(&self, dir: tempfile::TempDir) -> PathBuf {
        let path = dir.path().to_path_buf();
        if let Ok(mut guard) = self.sanitized.lock() {
            guard.push(dir);
        }
        path
    }

    /// Build the resolved overlay set for a request. Deduplicated by
    /// canonicalized host path; most restrictive permission wins.
    pub fn build_overlays(
        &self,
        session: &Session,
        request: &OverlayRequest,
    ) -> Result<Vec<OverlaySpec>, EngineError> {
        let mut by_key: HashMap<String, OverlaySpec> = HashMap::new();

        // 1. User-supplied directory overlays.
        for spec in &request.directories {
            let resolved = self.resolve_user_overlay(spec, session.working_dir())?;
            let key = OverlayPathResolver::conflict_key(&resolved.host_path);
            insert_or_merge(&mut by_key, key, resolved);
        }

        // 2. Agent settings overlays. Forward the yolo flag so Claude's
        //    settings sanitization can inject the bypass-permissions overlay.
        if let Some(agent) = &request.agent {
            for spec in
                self.agent_settings_overlays_with(agent, request.yolo, session.git_root())?
            {
                let key = OverlayPathResolver::conflict_key(&spec.host_path);
                insert_or_merge(&mut by_key, key, spec);
            }
        }

        // 3. Skills overlay (mount ~/.amux/skills/ read-only into agent's native path).
        if request.include_skills {
            if let Some(agent) = &request.agent {
                for spec in
                    self.skill_overlays(agent, &request.container_home, session.git_root())?
                {
                    let key = OverlayPathResolver::conflict_key(&spec.host_path);
                    insert_or_merge(&mut by_key, key, spec);
                }
            }
        }

        let mut out: Vec<OverlaySpec> = by_key.into_values().collect();
        out.sort_by(|a, b| a.host_path.cmp(&b.host_path));
        Ok(out)
    }

    /// Resolve a single user-supplied overlay spec into its canonical form.
    ///
    /// Relative host paths are resolved against `cwd` (the session's working
    /// directory), not the process's current directory.
    pub fn resolve_user_overlay(
        &self,
        spec: &DirectorySpec,
        cwd: &Path,
    ) -> Result<OverlaySpec, EngineError> {
        if !Path::new(&spec.container).is_absolute() {
            return Err(EngineError::Other(format!(
                "overlay container path '{}' must be absolute",
                spec.container
            )));
        }
        let host_abs = OverlayPathResolver::make_absolute_with_cwd(&spec.host, cwd);
        let host_canon = OverlayPathResolver::canonicalize_lossy(&host_abs);
        Ok(OverlaySpec {
            host_path: host_canon,
            container_path: PathBuf::from(&spec.container),
            permission: spec.permission,
        })
    }

    /// Per-agent settings overlays. Returns the host paths that exist; an
    /// empty list when the agent has no configured credentials on disk.
    pub fn agent_settings_overlays(
        &self,
        agent: &AgentName,
        git_root: &Path,
    ) -> Result<Vec<OverlaySpec>, EngineError> {
        self.agent_settings_overlays_with(agent, false, git_root)
    }

    /// Like `agent_settings_overlays` but threading the `yolo` flag so the
    /// Claude agent path can inject the bypass-permissions setting.
    pub fn agent_settings_overlays_with(
        &self,
        agent: &AgentName,
        yolo: bool,
        git_root: &Path,
    ) -> Result<Vec<OverlaySpec>, EngineError> {
        let home = self.auth_resolver.home();
        let paths = self.auth_resolver.resolve(agent.as_str());
        let mut out = Vec::new();
        let container_home = detect_container_home(home, agent.as_str(), git_root)
            .unwrap_or_else(|| "/root".to_string());

        match agent.as_str() {
            "claude" => {
                let has_config = paths
                    .config_file
                    .as_ref()
                    .map(|p| p.exists())
                    .unwrap_or(false);
                if has_config {
                    let cfg = paths.config_file.as_ref().unwrap();
                    let host_path = match sanitize_claude_config(cfg) {
                        Ok((dir, path)) => {
                            let _retained = self.retain_tempdir(dir);
                            path
                        }
                        Err(_) => cfg.clone(),
                    };
                    out.push(OverlaySpec {
                        host_path,
                        container_path: PathBuf::from(format!("{container_home}/.claude.json")),
                        permission: OverlayPermission::ReadWrite,
                    });
                } else {
                    // First-time user: no ~/.claude.json on host. Synthesize a
                    // minimal config with the /workspace trust dialog accepted
                    // so the agent doesn't prompt inside the container.
                    let host_path = match synthesize_minimal_claude_config() {
                        Ok((dir, path)) => {
                            let _retained = self.retain_tempdir(dir);
                            path
                        }
                        Err(_) => {
                            // Can't create temp file — skip this overlay.
                            PathBuf::new()
                        }
                    };
                    if host_path.exists() {
                        out.push(OverlaySpec {
                            host_path,
                            container_path: PathBuf::from(format!("{container_home}/.claude.json")),
                            permission: OverlayPermission::ReadWrite,
                        });
                    }
                }
                let has_settings_dir = paths
                    .settings_dir
                    .as_ref()
                    .map(|p| p.exists())
                    .unwrap_or(false);
                if has_settings_dir {
                    let dir = paths.settings_dir.as_ref().unwrap();
                    let host_path = match sanitize_claude_settings_dir(dir, yolo) {
                        Ok((tmp, path)) => {
                            let _retained = self.retain_tempdir(tmp);
                            path
                        }
                        Err(_) => dir.clone(),
                    };
                    out.push(OverlaySpec {
                        host_path,
                        container_path: PathBuf::from(format!("{container_home}/.claude")),
                        permission: OverlayPermission::ReadWrite,
                    });
                } else {
                    // First-time user: no ~/.claude/ on host. Synthesize a
                    // minimal settings dir with LSP suppression.
                    if let Ok((tmp, path)) = synthesize_minimal_claude_settings_dir(yolo) {
                        let _retained = self.retain_tempdir(tmp);
                        out.push(OverlaySpec {
                            host_path: path,
                            container_path: PathBuf::from(format!("{container_home}/.claude")),
                            permission: OverlayPermission::ReadWrite,
                        });
                    }
                }
            }
            "codex" => {
                if let Some(dir) = paths.settings_dir.as_ref() {
                    if dir.exists() {
                        out.push(OverlaySpec {
                            host_path: dir.clone(),
                            container_path: PathBuf::from(format!("{container_home}/.codex")),
                            permission: OverlayPermission::ReadWrite,
                        });
                    }
                }
            }
            "gemini" => {
                if let Some(dir) = paths.settings_dir.as_ref() {
                    if dir.exists() {
                        out.push(OverlaySpec {
                            host_path: dir.clone(),
                            container_path: PathBuf::from(format!("{container_home}/.gemini")),
                            permission: OverlayPermission::ReadWrite,
                        });
                    }
                }
            }
            "opencode" => {
                if let Some(dir) = paths.settings_dir.as_ref() {
                    if dir.exists() {
                        out.push(OverlaySpec {
                            host_path: dir.clone(),
                            container_path: PathBuf::from(format!(
                                "{container_home}/.config/opencode"
                            )),
                            permission: OverlayPermission::ReadWrite,
                        });
                    }
                }
            }
            "crush" => {
                let dir = home.join(".config").join("crush");
                if dir.exists() {
                    out.push(OverlaySpec {
                        host_path: dir,
                        container_path: PathBuf::from(format!("{container_home}/.config/crush")),
                        permission: OverlayPermission::ReadWrite,
                    });
                }
            }
            "cline" => {
                let dir = home.join(".cline").join("data");
                if dir.exists() {
                    out.push(OverlaySpec {
                        host_path: dir,
                        container_path: PathBuf::from(format!("{container_home}/.cline/data")),
                        permission: OverlayPermission::ReadWrite,
                    });
                }
            }
            // copilot, maki: no host overlays.
            _ => {}
        }

        Ok(out)
    }

    /// Build overlay specs for the global skills directory, mapping it to the
    /// agent's native skills/commands path inside the container (read-only).
    pub fn skill_overlays(
        &self,
        agent: &AgentName,
        container_home_override: &Option<String>,
        git_root: &Path,
    ) -> Result<Vec<OverlaySpec>, EngineError> {
        let skill_dirs = crate::data::fs::skill_dirs::SkillDirs::from_process_env(None)
            .map_err(EngineError::Data)?;
        let host_skills_dir = skill_dirs.global_dir();
        if !host_skills_dir.exists() {
            tracing::debug!(
                path = %host_skills_dir.display(),
                "global skills directory does not exist; skipping skills overlay"
            );
            return Ok(vec![]);
        }

        let home = self.auth_resolver.home();
        let container_home = container_home_override.clone().unwrap_or_else(|| {
            detect_container_home(home, agent.as_str(), git_root)
                .unwrap_or_else(|| "/root".to_string())
        });

        let container_path = match agent.as_str() {
            "claude" => format!("{container_home}/.claude/commands"),
            "codex" => format!("{container_home}/.codex/skills"),
            "opencode" => format!("{container_home}/.config/opencode/commands"),
            "gemini" => format!("{container_home}/.gemini/commands"),
            "copilot" => format!("{container_home}/.copilot/instructions"),
            "crush" => format!("{container_home}/.config/crush/commands"),
            "cline" => format!("{container_home}/.cline/skills"),
            "maki" => {
                tracing::warn!(
                    agent = "maki",
                    "skills overlay is not supported for maki; no known skills directory"
                );
                return Ok(vec![]);
            }
            other => {
                tracing::warn!(agent = other, "skills overlay: unknown agent, skipping");
                return Ok(vec![]);
            }
        };

        Ok(vec![OverlaySpec {
            host_path: OverlayPathResolver::canonicalize_lossy(&host_skills_dir),
            container_path: PathBuf::from(container_path),
            permission: OverlayPermission::ReadOnly,
        }])
    }
}

/// Strip `oauthAccount` from `~/.claude.json`, inject
/// `projects["/workspace"]["hasTrustDialogAccepted"] = true` to suppress the
/// in-container trust dialog, and write the result to a `TempDir` whose
/// lifetime is owned by the caller. The sanitized path is `<tempdir>/claude.json`.
fn sanitize_claude_config(src: &Path) -> Result<(tempfile::TempDir, PathBuf), std::io::Error> {
    let raw = std::fs::read_to_string(src)?;
    let mut value: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}));
    if let serde_json::Value::Object(obj) = &mut value {
        obj.remove("oauthAccount");

        // Mark `/workspace` as a trusted project so Claude does not prompt for
        // trust inside the container. Mirrors legacy
        // `oldsrc/runtime/mod.rs::sanitize_claude_config`.
        let projects = obj
            .entry("projects".to_string())
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if let serde_json::Value::Object(p) = projects {
            let project = p
                .entry("/workspace".to_string())
                .or_insert_with(|| serde_json::Value::Object(Default::default()));
            if let serde_json::Value::Object(pobj) = project {
                pobj.insert(
                    "hasTrustDialogAccepted".into(),
                    serde_json::Value::Bool(true),
                );
            }
        }
    }

    let tmp_dir = tempfile::Builder::new().prefix("amux-claude-").tempdir()?;
    let dest = tmp_dir.path().join("claude.json");
    let body = serde_json::to_string_pretty(&value).unwrap_or(raw);
    std::fs::write(&dest, body)?;
    Ok((tmp_dir, dest))
}

/// Sanitize `~/.claude/`: filter out denylisted entries, optionally inject
/// the yolo-mode settings file, and suppress the LSP recommendation banner.
/// Returns the `TempDir` (cleaned on drop) and its path.
fn sanitize_claude_settings_dir(
    src: &Path,
    yolo: bool,
) -> Result<(tempfile::TempDir, PathBuf), std::io::Error> {
    let tmp = tempfile::Builder::new()
        .prefix("amux-claude-dir-")
        .tempdir()?;
    let tmp_root = tmp.path().to_path_buf();
    // Mirror only the entries that are not on the denylist.
    let denylist: std::collections::HashSet<&str> = CLAUDE_DENYLIST.iter().copied().collect();
    if let Ok(entries) = std::fs::read_dir(src) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if denylist.contains(name_str.as_ref()) {
                continue;
            }
            let dest = tmp_root.join(&name);
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                copy_dir_all(&entry.path(), &dest)?;
            } else {
                std::fs::copy(entry.path(), dest)?;
            }
        }
    }
    // Inject (or update) settings.json to suppress LSP banner and optionally
    // grant yolo bypass-permissions.
    let settings_path = tmp_root.join("settings.json");
    let mut settings: serde_json::Value = if settings_path.exists() {
        std::fs::read_to_string(&settings_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if let serde_json::Value::Object(obj) = &mut settings {
        // Set both LSP suppression keys for compatibility with different
        // Claude Code versions.
        obj.insert(
            "hasShownLspRecommendation".into(),
            serde_json::Value::Bool(true),
        );
        obj.insert(
            "lspRecommendationDismissed".into(),
            serde_json::Value::Bool(true),
        );
        if yolo {
            obj.insert(
                "skipDangerousModePermissionPrompt".into(),
                serde_json::Value::Bool(true),
            );
            obj.insert(
                "permissionMode".into(),
                serde_json::Value::String("bypassPermissions".into()),
            );
        }
    }
    let body = serde_json::to_string_pretty(&settings).unwrap_or_default();
    let _ = std::fs::write(&settings_path, body);
    Ok((tmp, tmp_root))
}

/// Synthesize a minimal `.claude.json` for first-time users: trust dialog
/// accepted for `/workspace`, no oauthAccount.
fn synthesize_minimal_claude_config() -> Result<(tempfile::TempDir, PathBuf), std::io::Error> {
    let value = serde_json::json!({
        "projects": {
            "/workspace": {
                "hasTrustDialogAccepted": true
            }
        }
    });
    let tmp_dir = tempfile::Builder::new()
        .prefix("amux-claude-minimal-")
        .tempdir()?;
    let dest = tmp_dir.path().join("claude.json");
    let body = serde_json::to_string_pretty(&value).unwrap_or_default();
    std::fs::write(&dest, body)?;
    Ok((tmp_dir, dest))
}

/// Synthesize a minimal `~/.claude/` directory for first-time users with
/// LSP suppression and (optionally) yolo bypass.
fn synthesize_minimal_claude_settings_dir(
    yolo: bool,
) -> Result<(tempfile::TempDir, PathBuf), std::io::Error> {
    let tmp = tempfile::Builder::new()
        .prefix("amux-claude-dir-minimal-")
        .tempdir()?;
    let tmp_root = tmp.path().to_path_buf();
    let mut settings = serde_json::json!({});
    if let serde_json::Value::Object(obj) = &mut settings {
        obj.insert(
            "hasShownLspRecommendation".into(),
            serde_json::Value::Bool(true),
        );
        obj.insert(
            "lspRecommendationDismissed".into(),
            serde_json::Value::Bool(true),
        );
        if yolo {
            obj.insert(
                "skipDangerousModePermissionPrompt".into(),
                serde_json::Value::Bool(true),
            );
            obj.insert(
                "permissionMode".into(),
                serde_json::Value::String("bypassPermissions".into()),
            );
        }
    }
    let body = serde_json::to_string_pretty(&settings).unwrap_or_default();
    std::fs::write(tmp_root.join("settings.json"), body)?;
    Ok((tmp, tmp_root))
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    if let Ok(entries) = std::fs::read_dir(src) {
        for entry in entries.flatten() {
            let target = dst.join(entry.file_name());
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                copy_dir_all(&entry.path(), &target)?;
            } else {
                std::fs::copy(entry.path(), target)?;
            }
        }
    }
    Ok(())
}

/// Detect the container home directory by inspecting `Dockerfile.<agent>`.
///
/// Looks for a `USER <name>` directive (where `<name>` is not "root" or "0")
/// in `Dockerfile.<agent>` files under `<git_root>/.amux/` and `<home>/.amux/`.
/// Returns `Some("/home/<name>")` when found, `None` otherwise.
fn detect_container_home(home: &Path, agent: &str, git_root: &Path) -> Option<String> {
    let dockerfile_name = format!("Dockerfile.{agent}");
    let search_dirs: Vec<PathBuf> = [git_root.join(".amux"), home.join(".amux")]
        .into_iter()
        .collect();

    for dir in &search_dirs {
        let path = dir.join(&dockerfile_name);
        if !path.exists() {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            for line in content.lines() {
                let trimmed = line.trim();
                // Look for "USER <name>" (case-insensitive directive).
                let upper = trimmed.to_uppercase();
                if let Some(rest) = upper.strip_prefix("USER ") {
                    let name = rest.split_whitespace().next().unwrap_or("").trim();
                    if !name.is_empty() && name != "ROOT" && name != "0" {
                        // Use original case from the line.
                        let orig_rest = &trimmed[5..]; // skip "USER "
                        let orig_name = orig_rest.split_whitespace().next().unwrap_or("root");
                        return Some(format!("/home/{orig_name}"));
                    }
                }
            }
        }
    }
    None
}

fn insert_or_merge(map: &mut HashMap<String, OverlaySpec>, key: String, spec: OverlaySpec) {
    use std::collections::hash_map::Entry;
    match map.entry(key) {
        Entry::Occupied(mut e) => {
            // Most restrictive permission wins.
            let existing = e.get_mut();
            if matches!(spec.permission, OverlayPermission::ReadOnly)
                && matches!(existing.permission, OverlayPermission::ReadWrite)
            {
                existing.permission = OverlayPermission::ReadOnly;
            }
            // Keep the existing container path; first writer wins for clarity.
        }
        Entry::Vacant(e) => {
            e.insert(spec);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::session::AgentName;

    /// Serialises tests that write to `AMUX_CONFIG_HOME` (a process-global env var).
    static AMUX_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Set `AMUX_CONFIG_HOME` to `home`, run `f`, then restore the previous value.
    fn with_amux_config_home<F, R>(home: &Path, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _g = AMUX_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("AMUX_CONFIG_HOME").ok();
        std::env::set_var("AMUX_CONFIG_HOME", home.to_str().unwrap());
        let result = f();
        match prev {
            Some(v) => std::env::set_var("AMUX_CONFIG_HOME", v),
            None => std::env::remove_var("AMUX_CONFIG_HOME"),
        }
        result
    }

    fn make_engine(home: &Path) -> OverlayEngine {
        OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(home))
    }

    // ─── skill_overlays ───────────────────────────────────────────────────────

    /// Create a temp dir, make `<dir>/skills/` exist, and return both.
    fn make_home_with_skills() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        let skills_canon = std::fs::canonicalize(&skills).unwrap_or(skills);
        (tmp, skills_canon)
    }

    #[test]
    fn skill_overlays_returns_single_ro_spec_for_claude() {
        let (tmp, skills_canon) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();

        let specs = with_amux_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, &None, Path::new("/"))
                .unwrap()
        });

        assert_eq!(specs.len(), 1, "expected 1 OverlaySpec; got {specs:?}");
        assert_eq!(
            specs[0].host_path, skills_canon,
            "host path must be global skills dir"
        );
        assert_eq!(
            specs[0].permission,
            OverlayPermission::ReadOnly,
            "must be :ro"
        );
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .contains("/.claude/commands"),
            "claude container path must contain /.claude/commands; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_overlays_returns_single_ro_spec_for_codex() {
        let (tmp, skills_canon) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("codex").unwrap();

        let specs = with_amux_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, &None, Path::new("/"))
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].host_path, skills_canon);
        assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .contains("/.codex/skills"),
            "codex container path must contain /.codex/skills; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_overlays_returns_single_ro_spec_for_gemini() {
        let (tmp, skills_canon) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("gemini").unwrap();

        let specs = with_amux_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, &None, Path::new("/"))
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].host_path, skills_canon);
        assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .contains("/.gemini/commands"),
            "gemini container path must contain /.gemini/commands; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_overlays_returns_single_ro_spec_for_opencode() {
        let (tmp, skills_canon) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("opencode").unwrap();

        let specs = with_amux_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, &None, Path::new("/"))
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].host_path, skills_canon);
        assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .contains("/.config/opencode/commands"),
            "opencode container path must contain /.config/opencode/commands; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_overlays_returns_single_ro_spec_for_copilot() {
        let (tmp, skills_canon) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("copilot").unwrap();

        let specs = with_amux_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, &None, Path::new("/"))
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].host_path, skills_canon);
        assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .contains("/.copilot/instructions"),
            "copilot container path must contain /.copilot/instructions; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_overlays_returns_single_ro_spec_for_crush() {
        let (tmp, skills_canon) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("crush").unwrap();

        let specs = with_amux_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, &None, Path::new("/"))
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].host_path, skills_canon);
        assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .contains("/.config/crush/commands"),
            "crush container path must contain /.config/crush/commands; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_overlays_returns_single_ro_spec_for_cline() {
        let (tmp, skills_canon) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("cline").unwrap();

        let specs = with_amux_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, &None, Path::new("/"))
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].host_path, skills_canon);
        assert_eq!(specs[0].permission, OverlayPermission::ReadOnly);
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .contains("/.cline/skills"),
            "cline container path must contain /.cline/skills; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_overlays_returns_empty_when_skills_dir_does_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        // Deliberately do NOT create <tmp>/skills/.
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();

        let specs = with_amux_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, &None, Path::new("/"))
                .unwrap()
        });

        assert!(
            specs.is_empty(),
            "must return empty vec when skills dir is absent; got {specs:?}"
        );
    }

    #[test]
    fn skill_overlays_returns_empty_for_maki_no_error() {
        let (tmp, _) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("maki").unwrap();

        let specs = with_amux_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, &None, Path::new("/"))
                .unwrap()
        });

        assert!(
            specs.is_empty(),
            "maki must produce no skills mount; got {specs:?}"
        );
    }

    #[test]
    fn skill_overlays_uses_container_home_override_when_set() {
        let (tmp, _) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let override_home = Some("/home/appuser".to_string());

        let specs = with_amux_config_home(tmp.path(), || {
            engine
                .skill_overlays(&agent, &override_home, Path::new("/"))
                .unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert!(
            specs[0]
                .container_path
                .to_string_lossy()
                .starts_with("/home/appuser/"),
            "container path must use the override home '/home/appuser'; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn skill_overlays_defaults_to_root_when_no_dockerfile_present() {
        let (tmp, _) = make_home_with_skills();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();

        let specs = with_amux_config_home(tmp.path(), || {
            engine.skill_overlays(&agent, &None, tmp.path()).unwrap()
        });

        assert_eq!(specs.len(), 1);
        assert!(
            specs[0].container_path.to_string_lossy().starts_with("/root/"),
            "container path must default to /root/ when detect_container_home returns None; got {:?}",
            specs[0].container_path
        );
    }

    #[test]
    fn resolve_user_overlay_rejects_relative_container_path() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = make_engine(tmp.path());
        let spec = DirectorySpec {
            host: "/h".into(),
            container: "rel/path".into(),
            permission: OverlayPermission::ReadOnly,
        };
        let err = engine
            .resolve_user_overlay(&spec, Path::new("/"))
            .unwrap_err();
        assert!(matches!(err, EngineError::Other(_)));
    }

    #[test]
    fn agent_settings_synthesized_when_no_files_present() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let out = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
        assert!(
            out.iter().any(|o| o
                .container_path
                .to_string_lossy()
                .ends_with("/.claude.json")),
            "expected synthesized .claude.json overlay for first-time user, got {out:?}"
        );
    }

    #[test]
    fn agent_settings_overlays_claude_config_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        // Create ~/.claude.json so the overlay resolver picks it up.
        let config_file = tmp.path().join(".claude.json");
        std::fs::write(&config_file, r#"{"model":"claude-sonnet-4-6"}"#).unwrap();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
        // The overlay engine sanitizes the .claude.json file (strips
        // oauthAccount) and writes it to a temp path; we expect at least one
        // overlay mounting a file as `/root/.claude.json`.
        assert!(
            overlays.iter().any(|o| o
                .container_path
                .to_string_lossy()
                .ends_with("/.claude.json")),
            "expected overlay targeting /root/.claude.json, got {overlays:?}"
        );
    }

    #[test]
    fn build_overlays_deduplicates_overlapping_host_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let host_dir = tmp.path().join("shared");
        std::fs::create_dir_all(&host_dir).unwrap();
        let engine = make_engine(tmp.path());
        // Fake a session — overlay engine doesn't use it in this path.
        let session_tmp = tempfile::tempdir().unwrap();
        let session = {
            use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};
            let resolver = StaticGitRootResolver::new(session_tmp.path());
            crate::data::session::Session::open(
                session_tmp.path().to_path_buf(),
                &resolver,
                SessionOpenOptions::default(),
            )
            .unwrap()
        };
        let request = OverlayRequest {
            directories: vec![
                DirectorySpec {
                    host: host_dir.to_str().unwrap().to_string(),
                    container: "/app/data".into(),
                    permission: OverlayPermission::ReadWrite,
                },
                DirectorySpec {
                    host: host_dir.to_str().unwrap().to_string(),
                    container: "/app/data".into(),
                    permission: OverlayPermission::ReadOnly,
                },
            ],
            include_skills: false,
            agent: None,
            yolo: false,
            container_home: None,
        };
        let overlays = engine.build_overlays(&session, &request).unwrap();
        // The two entries sharing the same canonicalized host path must collapse.
        let matches: Vec<_> = overlays
            .iter()
            .filter(|o| o.host_path == host_dir.canonicalize().unwrap_or(host_dir.clone()))
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "duplicate host path must be deduplicated, got {overlays:?}"
        );
    }

    #[test]
    fn resolve_user_overlay_rejects_missing_container_path() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = make_engine(tmp.path());
        let spec = DirectorySpec {
            host: tmp.path().to_str().unwrap().to_string(),
            container: "relative/path".into(),
            permission: OverlayPermission::ReadOnly,
        };
        assert!(engine.resolve_user_overlay(&spec, Path::new("/")).is_err());
    }

    #[test]
    fn sanitize_claude_config_strips_oauth_account() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join(".claude.json");
        std::fs::write(
            &config_file,
            r#"{"model":"claude-sonnet-4-6","oauthAccount":{"token":"secret"}}"#,
        )
        .unwrap();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
        // One overlay for the config file.
        let config_overlay = overlays
            .iter()
            .find(|o| {
                o.container_path
                    .to_string_lossy()
                    .ends_with("/.claude.json")
            })
            .expect("must have .claude.json overlay");
        // The sanitized file must not contain oauthAccount.
        let sanitized = std::fs::read_to_string(&config_overlay.host_path).unwrap();
        assert!(
            !sanitized.contains("oauthAccount"),
            "oauthAccount must be stripped from sanitized config: {sanitized}"
        );
        assert!(
            sanitized.contains("claude-sonnet-4-6"),
            "model field must be preserved: {sanitized}"
        );
    }

    #[test]
    fn sanitize_claude_config_injects_workspace_trust_dialog_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join(".claude.json");
        std::fs::write(&config_file, r#"{"model":"claude-sonnet-4-6"}"#).unwrap();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
        let config_overlay = overlays
            .iter()
            .find(|o| {
                o.container_path
                    .to_string_lossy()
                    .ends_with("/.claude.json")
            })
            .expect("must have .claude.json overlay");
        let sanitized = std::fs::read_to_string(&config_overlay.host_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&sanitized).unwrap();
        assert_eq!(
            parsed["projects"]["/workspace"]["hasTrustDialogAccepted"],
            serde_json::Value::Bool(true),
            "trust dialog must be accepted for /workspace: {sanitized}"
        );
    }

    #[test]
    fn sanitize_claude_settings_dir_filters_denylist_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        // Create a denylisted entry.
        std::fs::create_dir_all(claude_dir.join("projects")).unwrap();
        // Create an allowed entry.
        std::fs::write(claude_dir.join("allowed.json"), r#"{"foo":"bar"}"#).unwrap();

        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
        let dir_overlay = overlays
            .iter()
            .find(|o| o.container_path.to_string_lossy().ends_with("/.claude"))
            .expect("must have .claude dir overlay");

        let sanitized_root = &dir_overlay.host_path;
        assert!(
            !sanitized_root.join("projects").exists(),
            "denylisted 'projects' dir must be excluded from sanitized overlay"
        );
        assert!(
            sanitized_root.join("allowed.json").exists(),
            "allowed file must be present in sanitized overlay"
        );
    }

    #[test]
    fn sanitize_claude_settings_dir_suppresses_lsp_banner() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();

        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
        let dir_overlay = overlays
            .iter()
            .find(|o| o.container_path.to_string_lossy().ends_with("/.claude"))
            .expect("must have .claude dir overlay");

        let settings_path = dir_overlay.host_path.join("settings.json");
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(
            settings["lspRecommendationDismissed"],
            serde_json::Value::Bool(true),
            "lspRecommendationDismissed must be true in sanitized settings"
        );
    }

    #[test]
    fn sanitize_claude_settings_dir_injects_yolo_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();

        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let overlays = engine
            .agent_settings_overlays_with(&agent, true, tmp.path())
            .unwrap();
        let dir_overlay = overlays
            .iter()
            .find(|o| o.container_path.to_string_lossy().ends_with("/.claude"))
            .expect("must have .claude dir overlay");

        let settings_path = dir_overlay.host_path.join("settings.json");
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(
            settings["permissionMode"],
            serde_json::Value::String("bypassPermissions".into()),
            "permissionMode must be bypassPermissions when yolo=true"
        );
    }

    #[test]
    fn detect_container_home_finds_user_directive() {
        let tmp = tempfile::tempdir().unwrap();
        let amux_dir = tmp.path().join(".amux");
        std::fs::create_dir_all(&amux_dir).unwrap();
        std::fs::write(
            amux_dir.join("Dockerfile.claude"),
            "FROM ubuntu:22.04\nRUN apt-get update\nUSER appuser\nWORKDIR /home/appuser\n",
        )
        .unwrap();

        let result = detect_container_home(tmp.path(), "claude", tmp.path());

        assert_eq!(
            result,
            Some("/home/appuser".to_string()),
            "detect_container_home must return /home/appuser for USER appuser"
        );
    }

    #[test]
    fn detect_container_home_returns_none_when_no_dockerfile() {
        let tmp = tempfile::tempdir().unwrap();
        let result = detect_container_home(tmp.path(), "claude", tmp.path());
        assert!(
            result.is_none(),
            "detect_container_home must return None when no Dockerfile found"
        );
    }

    #[test]
    fn detect_container_home_returns_none_for_root_user() {
        let tmp = tempfile::tempdir().unwrap();
        let amux_dir = tmp.path().join(".amux");
        std::fs::create_dir_all(&amux_dir).unwrap();
        std::fs::write(
            amux_dir.join("Dockerfile.claude"),
            "FROM ubuntu:22.04\nUSER root\n",
        )
        .unwrap();

        let result = detect_container_home(tmp.path(), "claude", tmp.path());

        assert!(
            result.is_none(),
            "detect_container_home must return None when USER is root"
        );
    }

    #[test]
    fn detect_container_home_returns_none_for_user_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let amux_dir = tmp.path().join(".amux");
        std::fs::create_dir_all(&amux_dir).unwrap();
        std::fs::write(
            amux_dir.join("Dockerfile.claude"),
            "FROM ubuntu:22.04\nUSER 0\n",
        )
        .unwrap();

        let result = detect_container_home(tmp.path(), "claude", tmp.path());

        assert!(
            result.is_none(),
            "detect_container_home must return None when USER is 0"
        );
    }

    #[test]
    fn sanitize_claude_settings_dir_no_yolo_when_false() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();

        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let overlays = engine
            .agent_settings_overlays_with(&agent, false, tmp.path())
            .unwrap();
        let dir_overlay = overlays
            .iter()
            .find(|o| o.container_path.to_string_lossy().ends_with("/.claude"))
            .expect("must have .claude dir overlay");

        let settings_path = dir_overlay.host_path.join("settings.json");
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(
            settings.get("permissionMode").is_none(),
            "permissionMode must NOT be set when yolo=false"
        );
    }
}
