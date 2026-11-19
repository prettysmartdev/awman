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
pub(crate) fn overlaps_protected_root(dir: &Path, home: &Path, data_home: Option<&Path>) -> bool {
    use crate::data::config::agent_definition::HOST_DIR_DENY_LIST;
    use crate::data::fs::agent_dirs::AGENTS_SUBDIR;
    let fold = |p: &Path| PathBuf::from(p.to_string_lossy().to_ascii_lowercase());
    let canonical = |p: &Path| canonical_through_existing_ancestor(p).map(|p| fold(&p));
    let Some(dir) = canonical(dir) else {
        return true;
    };
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

/// Merge a native-mode settings `patch` (shallow, keys replace) into
/// `relative_file` inside `private_dir`, an execution-private directory awman
/// created and seeded for this launch (WI 0070, opencode-v2 `default_agent`).
/// Never receives a host settings path.
///
/// Target: the named file when it exists; else a sibling `.jsonc` of the
/// same stem, rewritten as plain JSON (valid JSONC; it is a private copy);
/// else a new file holding only the patch. A non-object root is refused.
/// Written `0600` through a private sibling and a rename. Returns a notice
/// for each key whose different value was replaced.
pub(crate) fn apply_native_settings_patch(
    private_dir: &Path,
    relative_file: &Path,
    patch: &serde_json::Value,
) -> Result<Vec<String>, EngineError> {
    use crate::data::fs::private_files::write_private_json_atomic;
    if !relative_file
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return Err(EngineError::Config(format!(
            "invalid native settings path: {}",
            relative_file.display()
        )));
    }
    let Some(patch) = patch.as_object() else {
        return Err(EngineError::Config(
            "a native settings patch must be a JSON object".into(),
        ));
    };
    let primary = private_dir.join(relative_file);
    let jsonc = primary.with_extension("jsonc");
    let target = [&primary, &jsonc]
        .into_iter()
        .find(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file()))
        .cloned();
    let mut root = match &target {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| EngineError::io(path, e))?;
            crate::data::jsonc::parse_tolerant(&text).map_err(|e| {
                EngineError::Config(format!(
                    "{} in the private settings copy cannot be parsed ({e}); --plan cannot \
                     select the native mode",
                    relative_file.display()
                ))
            })?
        }
        None => serde_json::Value::Object(Default::default()),
    };
    let Some(object) = root.as_object_mut() else {
        return Err(EngineError::Config(format!(
            "{} in the private settings copy is not a JSON object; --plan cannot select the \
             native mode",
            relative_file.display()
        )));
    };
    let mut notices = Vec::new();
    for (key, value) in patch {
        if let Some(previous) = object.insert(key.clone(), value.clone()) {
            if &previous != value {
                notices.push(format!(
                    "overrode {key}={previous} for this session; host configuration unchanged"
                ));
            }
        }
    }
    let path = target.unwrap_or(primary);
    if let Some(parent) = path.parent().filter(|p| !p.exists()) {
        crate::data::fs::private_files::create_private_dir(parent)?;
    }
    write_private_json_atomic(&path, &root, || Ok(()))?;
    Ok(notices)
}

impl OverlayEngine {
    pub(crate) fn profile_settings_overlays(
        &self,
        agent: &AgentName,
        def: &AgentDefinition,
        profile: &crate::engine::agent::profile_state::ProfileRunState,
        home: &str,
        materialize: bool,
    ) -> Result<(Vec<OverlaySpec>, Vec<StagedCredentialFile>), EngineError> {
        let spec = def
            .profile_state
            .as_ref()
            .ok_or_else(|| EngineError::Config("profile state layout absent".into()))?;
        let mut staged = Vec::new();
        match def.settings_mount {
            SettingsMount::Claude => self.plant_credential_file(
                agent,
                &profile.run_dir.join(".claude"),
                materialize,
                &mut staged,
            )?,
            SettingsMount::Antigravity => {
                let root = profile.run_dir.join(".gemini");
                for file in (self.secret_files_provider)(agent) {
                    super::credential_file::write_secret_file(&root, &file)
                        .map_err(|e| EngineError::io(&root, e))?;
                }
            }
            _ => {}
        }
        let overlays = spec
            .roots
            .iter()
            .map(|r| OverlaySpec {
                host_path: profile.run_dir.join(&r.home_relative),
                container_path: Path::new(home).join(&r.home_relative),
                permission: OverlayPermission::ReadWrite,
            })
            .collect();
        Ok((overlays, staged))
    }
}
