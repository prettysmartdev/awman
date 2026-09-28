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
        self.agent_settings_overlays_with_credentials(
            agent,
            yolo,
            git_root,
            container_home_override,
            false,
        )
        .map(|(overlays, _)| overlays)
    }

    pub(crate) fn agent_settings_overlays_with_credentials(
        &self,
        agent: &AgentName,
        yolo: bool,
        git_root: &Path,
        container_home_override: Option<&str>,
        materialize_credentials: bool,
    ) -> Result<(Vec<OverlaySpec>, Vec<StagedCredentialFile>), EngineError> {
        let home = self.auth_resolver.home();
        let paths = self.auth_resolver.resolve(agent.as_str());
        let mut out = Vec::new();
        let mut staged_credentials = Vec::new();
        let container_home = container_home_override
            .map(|s| s.to_string())
            .or_else(|| detect_container_home(home, agent.as_str(), git_root))
            .unwrap_or_else(|| "/root".to_string());

        // Which strategy applies is a table fact (F-32). Five agents take the
        // plain `Direct` mount; claude and antigravity stage a sanitized or
        // keychain-seeded copy and keep an explicit strategy below.
        let settings_mount = matrix_for(agent.as_str())
            .map(|m| m.settings_mount)
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
            // F-32 this was five near-identical arms; two of them (crush,
            // cline) reached `$HOME` directly because `AuthPathResolver` has
            // no entry for them, which `$HOME/<rel>` reproduces exactly.
            SettingsMount::Direct(rel) => {
                let dir = home.join(rel);
                if dir.exists() {
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
            // copilot, maki, and any agent the matrix does not know: no host
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
    use crate::engine::agent::agent_matrix::{matrix_for, SUPPORTED_AGENTS};

    fn host_with_settings() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("host-home");
        for agent in SUPPORTED_AGENTS {
            if let SettingsMount::Direct(relative) = matrix_for(agent).unwrap().settings_mount {
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
            for agent in SUPPORTED_AGENTS {
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
        for agent in SUPPORTED_AGENTS {
            let found = overlays(&engine, agent, Some("/home/probe"));
            let destinations: Vec<String> = found
                .iter()
                .map(|o| o.container_path.to_string_lossy().into_owned())
                .collect();
            match matrix_for(agent).unwrap().settings_mount {
                SettingsMount::None => assert!(found.is_empty(), "{agent}: {destinations:?}"),
                SettingsMount::Direct(relative) => {
                    assert_eq!(destinations, [format!("/home/probe/{relative}")], "{agent}");
                    // The host directory itself is bound: live, not a copy.
                    assert_eq!(found[0].host_path, home.join(relative), "{agent}");
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
        for agent in SUPPORTED_AGENTS {
            let found = overlays(&engine, agent, Some("/home/probe"));
            match matrix_for(agent).unwrap().settings_mount {
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
