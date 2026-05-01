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

#[derive(Debug, Clone)]
pub struct OverlayEngine {
    auth_resolver: AuthPathResolver,
}

impl OverlayEngine {
    pub fn new(_session: &Session) -> Result<Self, EngineError> {
        let auth_resolver = AuthPathResolver::from_process_env().map_err(EngineError::Data)?;
        Ok(Self { auth_resolver })
    }

    pub fn with_auth_resolver(auth_resolver: AuthPathResolver) -> Self {
        Self { auth_resolver }
    }

    /// Build the resolved overlay set for a request. Deduplicated by
    /// canonicalized host path; most restrictive permission wins.
    pub fn build_overlays(
        &self,
        _session: &Session,
        request: &OverlayRequest,
    ) -> Result<Vec<OverlaySpec>, EngineError> {
        let mut by_key: HashMap<String, OverlaySpec> = HashMap::new();

        // 1. User-supplied directory overlays.
        for spec in &request.directories {
            let resolved = self.resolve_user_overlay(spec)?;
            let key = OverlayPathResolver::conflict_key(&resolved.host_path);
            insert_or_merge(&mut by_key, key, resolved);
        }

        // 2. Agent settings overlays.
        if let Some(agent) = &request.agent {
            for spec in self.agent_settings_overlays(agent)? {
                let key = OverlayPathResolver::conflict_key(&spec.host_path);
                insert_or_merge(&mut by_key, key, spec);
            }
        }

        let mut out: Vec<OverlaySpec> = by_key.into_values().collect();
        out.sort_by(|a, b| a.host_path.cmp(&b.host_path));
        Ok(out)
    }

    /// Resolve a single user-supplied overlay spec into its canonical form.
    pub fn resolve_user_overlay(
        &self,
        spec: &DirectorySpec,
    ) -> Result<OverlaySpec, EngineError> {
        if !Path::new(&spec.container).is_absolute() {
            return Err(EngineError::Other(format!(
                "overlay container path '{}' must be absolute",
                spec.container
            )));
        }
        let host_abs = OverlayPathResolver::make_absolute(&spec.host);
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
    ) -> Result<Vec<OverlaySpec>, EngineError> {
        let home = self.auth_resolver.home();
        let paths = self.auth_resolver.resolve(agent.as_str());
        let mut out = Vec::new();
        let container_home = "/root";

        match agent.as_str() {
            "claude" => {
                if let Some(cfg) = paths.config_file.as_ref() {
                    if cfg.exists() {
                        out.push(OverlaySpec {
                            host_path: cfg.clone(),
                            container_path: PathBuf::from(format!("{container_home}/.claude.json")),
                            permission: OverlayPermission::ReadWrite,
                        });
                    }
                }
                if let Some(dir) = paths.settings_dir.as_ref() {
                    if dir.exists() {
                        out.push(OverlaySpec {
                            host_path: dir.clone(),
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
                        container_path: PathBuf::from(format!(
                            "{container_home}/.config/crush"
                        )),
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

    fn make_engine(home: &Path) -> OverlayEngine {
        OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(home))
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
        let err = engine.resolve_user_overlay(&spec).unwrap_err();
        assert!(matches!(err, EngineError::Other(_)));
    }

    #[test]
    fn agent_settings_empty_when_no_files_present() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let out = engine.agent_settings_overlays(&agent).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn agent_settings_overlays_claude_config_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        // Create ~/.claude.json so the overlay resolver picks it up.
        let config_file = tmp.path().join(".claude.json");
        std::fs::write(&config_file, r#"{"model":"claude-sonnet-4-6"}"#).unwrap();
        let engine = make_engine(tmp.path());
        let agent = AgentName::new("claude").unwrap();
        let overlays = engine.agent_settings_overlays(&agent).unwrap();
        assert!(
            overlays.iter().any(|o| o.host_path == config_file),
            "expected overlay for ~/.claude.json, got {overlays:?}"
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
        assert!(engine.resolve_user_overlay(&spec).is_err());
    }
}
