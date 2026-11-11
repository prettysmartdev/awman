//! Staging an agent's own settings directory into a container.
//!
//! Split out of `engine/overlay/mod.rs` by WI 0114 F-51. A child module of
//! `overlay`, so it reaches `OverlayEngine`'s private items unchanged.

use super::*;

impl OverlayEngine {
    /// Per-agent settings overlays. Returns the host paths that exist; an
    /// empty list when the agent has no configured credentials on disk.
    pub fn agent_settings_overlays(
        &self,
        agent: &AgentName,
        git_root: &Path,
    ) -> Result<Vec<OverlaySpec>, EngineError> {
        self.agent_settings_overlays_with(agent, false, git_root, None)
    }

    /// Like `agent_settings_overlays` but threading the `yolo` flag so the
    /// Claude agent path can inject the bypass-permissions setting, and an
    /// optional `container_home_override` so all overlay container paths
    /// agree on the agent's home directory (matches `resolve_user_overlay`
    /// and `skill_overlays`).
    pub fn agent_settings_overlays_with(
        &self,
        agent: &AgentName,
        yolo: bool,
        git_root: &Path,
        container_home_override: Option<&str>,
    ) -> Result<Vec<OverlaySpec>, EngineError> {
        let definition = resolve_definition(&self.snapshot(), agent, git_root);
        self.agent_settings_overlays_with_credentials(
            agent,
            definition.as_deref(),
            yolo,
            git_root,
            container_home_override,
            false,
            crate::data::fs::data_paths::data_home(&self.snapshot())
                .ok()
                .as_deref(),
        )
        .map(|(overlays, _)| overlays)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn agent_settings_overlays_with_credentials(
        &self,
        agent: &AgentName,
        definition: Option<&AgentDefinition>,
        yolo: bool,
        git_root: &Path,
        container_home_override: Option<&str>,
        materialize_credentials: bool,
        data_home: Option<&Path>,
    ) -> Result<(Vec<OverlaySpec>, Vec<StagedCredentialFile>), EngineError> {
        let home = self.auth_resolver.home();
        let paths = self.auth_resolver.resolve(agent.as_str());
        let mut out = Vec::new();
        let mut staged_credentials = Vec::new();
        let container_home = container_home_override
            .map(|s| s.to_string())
            .or_else(|| detect_container_home(home, agent.as_str(), git_root))
            .unwrap_or_else(|| "/root".to_string());

        // Which strategy applies is a definition fact (F-32). Most agents
        // take the plain `Direct` mount; claude and antigravity stage a
        // sanitized or keychain-seeded copy and keep an explicit strategy
        // below. An agent with no resolved definition has no settings overlay.
        let settings_mount = definition
            .map(|d| d.settings_mount.clone())
            .unwrap_or(SettingsMount::None);
        match settings_mount {
            SettingsMount::Claude => {
                let has_config = paths
                    .config_file
                    .as_ref()
                    .map(|p| p.exists())
                    .unwrap_or(false);
                if has_config {
                    let cfg = paths.config_file.as_ref().unwrap();
                    // Fail closed: never mount the host file itself. If it
                    // cannot be sanitized, fall back to the synthesised minimal
                    // config, and skip the overlay if even that fails.
                    let staged =
                        sanitize_claude_config(cfg).or_else(|_| synthesize_minimal_claude_config());
                    if let Ok((dir, host_path)) = staged {
                        let _retained = self.retain_tempdir(dir);
                        out.push(OverlaySpec {
                            host_path,
                            container_path: PathBuf::from(format!("{container_home}/.claude.json")),
                            permission: OverlayPermission::ReadWrite,
                        });
                    }
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
                    let staged = sanitize_claude_settings_dir(dir, yolo).or_else(|error| {
                        tracing::warn!(
                            path = %dir.display(),
                            %error,
                            "could not sanitize Claude settings; using an empty safe overlay"
                        );
                        synthesize_minimal_claude_settings_dir(yolo)
                    });
                    if let Ok((tmp, path)) = staged {
                        self.plant_credential_file(
                            agent,
                            &path,
                            materialize_credentials,
                            &mut staged_credentials,
                        )?;
                        let host_path = self.retain_tempdir(tmp);
                        out.push(OverlaySpec {
                            host_path,
                            container_path: PathBuf::from(format!("{container_home}/.claude")),
                            permission: OverlayPermission::ReadWrite,
                        });
                    }
                } else {
                    // First-time user: no ~/.claude/ on host. Synthesize a
                    // minimal settings dir with LSP suppression.
                    if let Ok((tmp, path)) = synthesize_minimal_claude_settings_dir(yolo) {
                        self.plant_credential_file(
                            agent,
                            &path,
                            materialize_credentials,
                            &mut staged_credentials,
                        )?;
                        let host_path = self.retain_tempdir(tmp);
                        out.push(OverlaySpec {
                            host_path,
                            container_path: PathBuf::from(format!("{container_home}/.claude")),
                            permission: OverlayPermission::ReadWrite,
                        });
                    }
                }
            }
            // The generic strategy: mount `$HOME/<rel>` at
            // `<container_home>/<rel>` when the host directory exists. Before
            // F-32 this was five near-identical arms; cline (and the former
            // built-in crush) reached `$HOME` directly because
            // `AuthPathResolver` has no entry for it, which `$HOME/<rel>`
            // reproduces exactly.
            SettingsMount::Direct(rel) => {
                use crate::data::config::agent_definition::is_deny_listed_host_dir;
                for rel in std::iter::once(rel).chain(
                    definition
                        .into_iter()
                        .flat_map(|d| d.extra_settings_mounts.iter().cloned()),
                ) {
                    if is_deny_listed_host_dir(&rel)
                        || rel.is_empty()
                        || !std::path::Path::new(&rel)
                            .components()
                            .all(|c| matches!(c, std::path::Component::Normal(_)))
                    {
                        let path = definition
                            .and_then(|d| match &d.origin {
                                crate::engine::agent::AgentOrigin::Custom { path, .. } => {
                                    Some(path.clone())
                                }
                                _ => None,
                            })
                            .unwrap_or_else(|| home.join(&rel));
                        return Err(EngineError::AgentDefinitionInvalid {
                            path,
                            problems: vec![format!(
                                "settings.host_dir: '{rel}' is not an allowed host directory"
                            )],
                        });
                    }
                    let dir = home.join(&rel);
                    if dir.is_dir() {
                        let canonical_home =
                            std::fs::canonicalize(home).map_err(|e| EngineError::io(home, e))?;
                        let dir =
                            std::fs::canonicalize(&dir).map_err(|e| EngineError::io(&dir, e))?;
                        let allowed = dir.strip_prefix(&canonical_home).ok().is_some_and(|path| {
                            !path.as_os_str().is_empty()
                                && !is_deny_listed_host_dir(&path.to_string_lossy())
                        }) && !overlaps_protected_root(&dir, home, data_home);
                        if !allowed {
                            return Err(EngineError::AgentDefinitionInvalid {
                            path: home.join(&rel),
                            problems: vec!["settings.host_dir resolves outside HOME or overlaps a protected directory".into()],
                        });
                        }
                        out.push(OverlaySpec {
                            host_path: dir,
                            container_path: PathBuf::from(format!("{container_home}/{rel}")),
                            permission: OverlayPermission::ReadWrite,
                        });
                    }
                }
            }
            SettingsMount::Antigravity => {
                // Antigravity reads its OAuth token from a fixed file inside
                // `~/.gemini/antigravity-cli/` when the in-container keyring
                // (Secret Service / D-Bus) is unreachable — which is always
                // the case in our agent containers. We pull the same token
                // from the host keychain and seed it into the staged dir.
                let secret_files = (self.secret_files_provider)(agent);
                let host_dir = paths.settings_dir.as_ref();
                let dir_exists = host_dir.map(|p| p.exists()).unwrap_or(false);
                if dir_exists || !secret_files.is_empty() {
                    let staged = if dir_exists {
                        stage_settings_dir_with_secrets(
                            host_dir.unwrap(),
                            &secret_files,
                            "awman-antigravity-",
                        )
                    } else {
                        // First-time user: no host `~/.gemini` but a keychain
                        // token is still good enough for agy to authenticate.
                        synthesize_settings_dir_with_secrets(
                            &secret_files,
                            "awman-antigravity-minimal-",
                        )
                    };
                    let host_path = match staged {
                        Ok((tmp, path)) => {
                            let _retained = self.retain_tempdir(tmp);
                            path
                        }
                        Err(_) => host_dir
                            .cloned()
                            .unwrap_or_else(|| PathBuf::from("/nonexistent")),
                    };
                    if host_path.exists() {
                        out.push(OverlaySpec {
                            host_path,
                            container_path: PathBuf::from(format!("{container_home}/.gemini")),
                            permission: OverlayPermission::ReadWrite,
                        });
                    }
                }
            }
            // Agents without a settings mount (including unresolved custom
            // definitions): no host overlays.
            SettingsMount::None => {}
        }

        Ok((out, staged_credentials))
    }

    pub(crate) fn plant_credential_file(
        &self,
        agent: &AgentName,
        staged_root: &Path,
        materialize_credentials: bool,
        staged: &mut Vec<StagedCredentialFile>,
    ) -> Result<(), EngineError> {
        if !materialize_credentials {
            return Ok(());
        }
        let Some(file) = (self.credential_provider)(agent) else {
            return Ok(());
        };
        write_credential_file_atomic(staged_root, &file)
            .map_err(|error| EngineError::io(staged_root, error))?;
        // The descriptor's materialized Claude JSON has the same deliberately
        // refresh-token-free shape as the source parser accepts. Parse only the
        // access token/expiry fields to create the monitor's opaque identity.
        let fingerprint = credential_fingerprint_for_file(&file)?;
        staged.push(StagedCredentialFile {
            agent: agent.clone(),
            path: staged_root.join(&file.relative_path),
            root: staged_root.to_path_buf(),
            fingerprint,
        });
        Ok(())
    }
}

/// Whether canonical `dir` contains, or is inside, a directory a settings
/// mount must never expose: awman's real data home (wherever XDG or
/// `AWMAN_HOME` put it — it holds the trust store and global
/// definitions) and the real target of every deny-listed HOME directory, so
/// a relocated `~/.awman -> ~/state` is protected under its alias too. A
/// root that does not exist yet is compared through its nearest existing
/// ancestor, so mounting a parent cannot let an agent create it. ASCII case
/// is folded, as for the deny list.
///
/// When the data home *is* HOME (an `AWMAN_HOME=$HOME` setup), every
/// settings directory is inside it, so only its awman-owned children
/// (`agents/`, `squad/`) are protected in both directions; HOME itself, which
/// holds the trust store, is never mountable anyway.
fn overlaps_protected_root(dir: &Path, home: &Path, data_home: Option<&Path>) -> bool {
    use crate::data::config::agent_definition::HOST_DIR_DENY_LIST;
    use crate::data::fs::agent_dirs::AGENTS_SUBDIR;
    let fold = |p: &Path| PathBuf::from(p.to_string_lossy().to_ascii_lowercase());
    let canonical = |p: &Path| canonical_through_existing_ancestor(p).map(|p| fold(&p));
    let dir = fold(dir);
    let overlaps = |root: &PathBuf| dir.starts_with(root) || root.starts_with(&dir);
    let mut both_ways: Vec<PathBuf> = HOST_DIR_DENY_LIST
        .iter()
        .filter_map(|denied| canonical(&home.join(denied)))
        .collect();
    if let Some(data_home) = data_home.and_then(canonical) {
        // Never mount the data home or anything above it.
        if data_home.starts_with(&dir) {
            return true;
        }
        if Some(&data_home) == canonical(home).as_ref() {
            both_ways.extend(
                [AGENTS_SUBDIR, "squad"]
                    .iter()
                    .map(|child| data_home.join(child)),
            );
        } else {
            both_ways.push(data_home);
        }
    }
    both_ways.iter().any(overlaps)
}

/// `path` canonicalized through its deepest existing ancestor, with the
/// missing components appended unchanged.
fn canonical_through_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(existing) {
            return Some(rest.iter().rev().fold(canonical, |p, c| p.join(c)));
        }
        rest.push(existing.file_name()?);
        existing = existing.parent()?;
    }
}
