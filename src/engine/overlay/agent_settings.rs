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
        let definition = resolve_definition(agent, git_root);
        self.agent_settings_overlays_with_credentials(
            agent,
            definition.as_deref(),
            yolo,
            git_root,
            container_home_override,
            false,
            crate::data::config::global::GlobalConfig::data_home_with(
                &crate::data::config::env::Env::from_process(),
            )
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
                    let dir = std::fs::canonicalize(&dir).map_err(|e| EngineError::io(&dir, e))?;
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
            // copilot, and any agent without a resolved definition: no host
            // overlays.
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

#[cfg(test)]
mod image_home_tests {
    //! Settings destinations follow the image's HOME (what an importing runtime
    //! reports), never `/root` or the host's own home.
    use super::*;
    use crate::data::fs::auth_paths::AuthPathResolver;
    use crate::engine::agent::first_party::{first_party, FIRST_PARTY_AGENTS};

    fn host_with_settings() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("host-home");
        for agent in FIRST_PARTY_AGENTS {
            if let SettingsMount::Direct(relative) = first_party(agent).unwrap().settings_mount {
                let dir = home.join(relative);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("settings.json"), b"{}").unwrap();
            }
            let paths = AuthPathResolver::at_home(&home).resolve(agent);
            if let Some(dir) = paths.settings_dir {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("settings.json"), b"{}").unwrap();
            }
            if let Some(file) = paths.config_file {
                std::fs::write(file, b"{}").unwrap();
            }
        }
        (temp, home)
    }

    fn overlays(engine: &OverlayEngine, agent: &str, home: Option<&str>) -> Vec<OverlaySpec> {
        engine
            .agent_settings_overlays_with(
                &AgentName::new(agent).unwrap(),
                false,
                Path::new("/nonexistent-repo"),
                home,
            )
            .unwrap()
    }

    #[test]
    fn every_settings_destination_is_rooted_at_the_supplied_image_home() {
        let (_temp, home) = host_with_settings();
        let engine = OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(&home));
        for image_home in ["/home/probe", "/opt/agent home/u-1", "/var/lib/agent"] {
            for agent in FIRST_PARTY_AGENTS {
                for overlay in overlays(&engine, agent, Some(image_home)) {
                    let destination = overlay.container_path.to_string_lossy().into_owned();
                    assert!(
                        destination.starts_with(&format!("{image_home}/")),
                        "{agent}: {destination} is not under {image_home}"
                    );
                    assert!(!destination.starts_with("/root"), "{agent}: {destination}");
                    assert_eq!(overlay.permission, OverlayPermission::ReadWrite);
                }
            }
        }
    }

    #[test]
    fn each_family_maps_to_its_documented_destination_and_source() {
        let (_temp, home) = host_with_settings();
        let engine = OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(&home));
        for agent in FIRST_PARTY_AGENTS {
            let found = overlays(&engine, agent, Some("/home/probe"));
            let destinations: Vec<String> = found
                .iter()
                .map(|o| o.container_path.to_string_lossy().into_owned())
                .collect();
            match first_party(agent).unwrap().settings_mount {
                SettingsMount::None => assert!(found.is_empty(), "{agent}: {destinations:?}"),
                SettingsMount::Direct(relative) => {
                    assert_eq!(destinations, [format!("/home/probe/{relative}")], "{agent}");
                    // The host directory itself is bound: live, not a copy.
                    assert_eq!(
                        found[0].host_path,
                        home.join(relative).canonicalize().unwrap(),
                        "{agent}"
                    );
                }
                SettingsMount::Claude => {
                    let mut sorted = destinations.clone();
                    sorted.sort();
                    assert_eq!(sorted, ["/home/probe/.claude", "/home/probe/.claude.json"]);
                    // Sanitized staging: never the raw host settings directory.
                    let raw = AuthPathResolver::at_home(&home).resolve(agent);
                    for overlay in &found {
                        assert_ne!(Some(&overlay.host_path), raw.settings_dir.as_ref());
                        assert_ne!(Some(&overlay.host_path), raw.config_file.as_ref());
                    }
                }
                SettingsMount::Antigravity => {
                    assert_eq!(destinations, ["/home/probe/.gemini"]);
                    let raw = AuthPathResolver::at_home(&home).resolve(agent);
                    assert_ne!(Some(&found[0].host_path), raw.settings_dir.as_ref());
                }
            }
        }
    }

    #[test]
    fn without_host_settings_only_claude_synthesizes_a_safe_overlay() {
        let empty = tempfile::tempdir().unwrap();
        let engine = OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(empty.path()));
        for agent in FIRST_PARTY_AGENTS {
            let found = overlays(&engine, agent, Some("/home/probe"));
            match first_party(agent).unwrap().settings_mount {
                SettingsMount::Claude => {
                    assert_eq!(found.len(), 2, "minimal .claude and .claude.json");
                    assert!(found.iter().all(|o| o.host_path.exists()));
                }
                _ => assert!(
                    found.is_empty(),
                    "{agent}: nothing to mount without host settings"
                ),
            }
        }
    }
}

#[cfg(test)]
mod custom_tests {
    use super::*;
    #[test]
    fn direct_custom_settings_recheck_deny_list_before_mounting() {
        let temp = tempfile::tempdir().unwrap();
        let engine = OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(temp.path()));
        let mut def = crate::engine::agent::custom::crush_fixture();
        for relative in [
            ".ssh",
            ".aws/credentials",
            ".awman",
            "../outside",
            "/etc",
            ".config",
            ".local",
            "Library",
            ".SSH",
            "library/keychains",
        ] {
            def.settings_mount = SettingsMount::Direct(relative.into());
            let result = engine.agent_settings_overlays_with_credentials(
                &AgentName::new("crush").unwrap(),
                Some(&def),
                false,
                temp.path(),
                Some("/home/awman"),
                false,
                None,
            );
            assert!(
                matches!(result, Err(EngineError::AgentDefinitionInvalid { .. })),
                "{relative}: {result:?}"
            );
        }
        def.settings_mount = SettingsMount::Direct(".pi".into());
        std::fs::create_dir_all(temp.path().join(".pi")).unwrap();
        let (overlays, _) = engine
            .agent_settings_overlays_with_credentials(
                &AgentName::new("crush").unwrap(),
                Some(&def),
                false,
                temp.path(),
                Some("/home/awman"),
                false,
                None,
            )
            .unwrap();
        assert_eq!(
            overlays[0].host_path,
            temp.path().join(".pi").canonicalize().unwrap()
        );
        assert_eq!(overlays[0].container_path, PathBuf::from("/home/awman/.pi"));
        assert_eq!(overlays[0].permission, OverlayPermission::ReadWrite);
    }

    #[cfg(unix)]
    #[test]
    fn direct_mounts_recheck_canonical_targets_within_home() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let outside = temp.path().join("outside");
        for path in [
            home.join(".ssh"),
            home.join(".AWS"),
            outside.clone(),
            home.join("safe"),
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        let engine = OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(&home));
        let mut def = crate::engine::agent::custom::crush_fixture();
        def.settings_mount = SettingsMount::Direct(".pi".into());
        let link = home.join(".pi");
        for target in [home.join(".ssh"), home.join(".AWS"), home.clone(), outside] {
            symlink(target, &link).unwrap();
            let result = engine.agent_settings_overlays_with_credentials(
                &AgentName::new("pi").unwrap(),
                Some(&def),
                false,
                temp.path(),
                Some("/home/awman"),
                false,
                None,
            );
            assert!(
                matches!(result, Err(EngineError::AgentDefinitionInvalid { .. })),
                "{result:?}"
            );
            std::fs::remove_file(&link).unwrap();
        }
        symlink(home.join("safe"), &link).unwrap();
        let (overlays, _) = engine
            .agent_settings_overlays_with_credentials(
                &AgentName::new("pi").unwrap(),
                Some(&def),
                false,
                temp.path(),
                Some("/home/awman"),
                false,
                None,
            )
            .unwrap();
        assert_eq!(
            overlays[0].host_path,
            std::fs::canonicalize(home.join("safe")).unwrap()
        );
    }
}

/// Whether canonical `dir` contains, or is inside, a directory a settings
/// mount must never expose: awman's real data home (wherever XDG or
/// `AWMAN_CONFIG_HOME` put it — it holds the trust store and global
/// definitions) and the real target of every deny-listed HOME directory, so
/// a relocated `~/.awman -> ~/state` is protected under its alias too. A
/// root that does not exist yet is compared through its nearest existing
/// ancestor, so mounting a parent cannot let an agent create it. ASCII case
/// is folded, as for the deny list.
///
/// When the data home *is* HOME (an `AWMAN_CONFIG_HOME=$HOME` setup), every
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

#[cfg(all(test, unix))]
mod protected_root_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn pi_settings(rel: &str) -> AgentDefinition {
        let mut def = crate::engine::agent::custom::crush_fixture();
        def.settings_mount = SettingsMount::Direct(rel.into());
        def
    }

    fn mount(
        home: &Path,
        rel: &str,
        data_home: Option<&Path>,
    ) -> Result<Vec<OverlaySpec>, EngineError> {
        let engine = OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(home));
        engine
            .agent_settings_overlays_with_credentials(
                &AgentName::new("pi").unwrap(),
                Some(&pi_settings(rel)),
                false,
                home,
                Some("/home/awman"),
                false,
                data_home,
            )
            .map(|(overlays, _)| overlays)
    }

    #[test]
    fn xdg_or_explicit_data_home_and_its_ancestors_cannot_be_mounted() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let data_home = home.join(".local/share/awman");
        std::fs::create_dir_all(&data_home).unwrap();
        std::fs::write(data_home.join("trusted-agents.json"), "{}").unwrap();
        for rel in [".local/share/awman", ".local/share/awman/agents"] {
            std::fs::create_dir_all(home.join(rel)).unwrap();
            let err = mount(&home, rel, Some(&data_home)).unwrap_err();
            assert!(
                matches!(err, EngineError::AgentDefinitionInvalid { .. }),
                "{rel}"
            );
        }
        // An explicit config home elsewhere under HOME, mounted through a parent.
        let explicit = home.join("state/awman-config");
        std::fs::create_dir_all(&explicit).unwrap();
        assert!(mount(&home, "state", Some(&explicit)).is_err());
        // A data home that does not exist yet is still protected via its parent.
        let pending = home.join("later/awman");
        std::fs::create_dir_all(home.join("later")).unwrap();
        assert!(mount(&home, "later", Some(&pending)).is_err());
        // An unrelated directory is still allowed.
        std::fs::create_dir_all(home.join(".pi")).unwrap();
        assert_eq!(mount(&home, ".pi", Some(&data_home)).unwrap().len(), 1);
    }

    #[test]
    fn a_data_home_that_is_home_protects_its_awman_children_only() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        for rel in [".pi", "agents", "squad"] {
            std::fs::create_dir_all(home.join(rel)).unwrap();
        }
        assert_eq!(mount(&home, ".pi", Some(&home)).unwrap().len(), 1);
        for rel in ["agents", "squad"] {
            assert!(mount(&home, rel, Some(&home)).is_err(), "{rel}");
        }
    }

    #[test]
    fn relocated_protected_directories_are_protected_at_their_real_target() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        for (alias, target) in [(".awman", "state"), (".ssh", "keys")] {
            std::fs::create_dir_all(home.join(target)).unwrap();
            symlink(home.join(target), home.join(alias)).unwrap();
            let err = mount(&home, target, Some(&home.join(".awman"))).unwrap_err();
            assert!(
                matches!(err, EngineError::AgentDefinitionInvalid { .. }),
                "{alias} -> {target}"
            );
        }
    }
}
