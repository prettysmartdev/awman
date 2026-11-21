//! Mounting the global skills library into a container.
//!
//! Split out of `engine/overlay/mod.rs` by WI 0114 F-51. A child module of
//! `overlay`, so it reaches `OverlayEngine`'s private items unchanged.

use super::*;

/// Whether a reviewed lease skill resolves using the normal mount search order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillStatus {
    Installed { default_library: bool },
    Missing,
}

impl OverlayEngine {
    /// Resolve installed skills without opening a session or materializing mounts.
    /// Keep the search order aligned with skill_overlays_for_with_env.
    pub fn lease_skill_status(
        env: &crate::data::config::env::EnvSnapshot,
        name: &str,
    ) -> SkillStatus {
        let resolve = || -> Option<bool> {
            let dirs = crate::data::fs::skill_dirs::SkillDirs::from_env(env, None).ok()?;
            let (library, skill) = match name.split_once('/') {
                Some((library, skill)) => {
                    validate_skill_reference_segment(library, name).ok()?;
                    validate_skill_reference_segment(skill, name).ok()?;
                    (library, Some(skill))
                }
                None => {
                    validate_skill_reference_segment(name, name).ok()?;
                    let plain = dirs.global_dir().join(name);
                    if plain.exists() {
                        return (plain.is_dir()
                            && plain.join("SKILL.md").is_file()
                            && skill_path_confined(&dirs.global_dir(), &plain))
                        .then_some(false);
                    }
                    (name, None)
                }
            };
            let library_dir = dirs.library_dir(library);
            let meta = read_library_meta(&library_dir).ok()?;
            let subdir = validate_library_subdir(&meta.subdir).ok()?;
            let mount = library_dir.join(subdir);
            let path = skill.map_or_else(|| mount.clone(), |skill| mount.join(skill));
            if !path.is_dir()
                || !skill_path_confined(&dirs.global_dir(), &path)
                || skill.is_some() && !path.join("SKILL.md").is_file()
            {
                return None;
            }
            // The builtin library's identity, as used by skill library pull.
            Some(library == "awman" && meta.owner == "prettysmartdev" && meta.repo == "awman")
        };
        match resolve() {
            Some(default_library) => SkillStatus::Installed { default_library },
            None => SkillStatus::Missing,
        }
    }

    /// Candidates for registration; admission still reviews each named request.
    pub fn lease_skill_names(
        env: &crate::data::config::env::EnvSnapshot,
        allowed: &[String],
    ) -> Vec<String> {
        let mut names = allowed.to_vec();
        // A library allow entry also authorizes each installed child. Resolve
        // candidates exactly as admission does, including plain-skill shadows.
        let mut libraries: Vec<_> = allowed
            .iter()
            .filter(|name| !name.contains('/'))
            .cloned()
            .collect();
        libraries.push("awman".into());
        if let Ok(dirs) = crate::data::fs::skill_dirs::SkillDirs::from_env(env, None) {
            for name in libraries {
                let library = dirs.library_dir(&name);
                if let Ok(meta) = read_library_meta(&library) {
                    if let Ok(subdir) = validate_library_subdir(&meta.subdir) {
                        names.push(name.clone());
                        if let Ok(entries) = std::fs::read_dir(library.join(subdir)) {
                            for entry in entries.flatten() {
                                if let Some(child) = entry.file_name().to_str() {
                                    names.push(format!("{name}/{child}"));
                                }
                            }
                        }
                    }
                }
            }
        }
        names.retain(|name| {
            // Registration labels must never advertise a name admission would
            // reject. Invalid label keys are separately omitted by HostFacts.
            let valid = crate::data::config::work_servers::validate_allow_skill_entry(name).is_ok();
            valid
                && match Self::lease_skill_status(env, name) {
                    SkillStatus::Missing => false,
                    SkillStatus::Installed { default_library } => {
                        default_library
                            || allowed.iter().any(|entry| {
                                entry == name
                                    || name
                                        .strip_prefix(entry)
                                        .is_some_and(|tail| tail.starts_with('/'))
                            })
                    }
                }
        });
        names.sort();
        names.dedup();
        names
    }

    /// Build overlay specs for the global skills directory, mapping it to the
    /// agent's native skills/commands path inside the container (read-only).
    pub fn skill_overlays(
        &self,
        agent: &AgentName,
        include_all: bool,
        names: &[String],
        container_home_override: &Option<String>,
        git_root: &Path,
    ) -> Result<Vec<OverlaySpec>, EngineError> {
        // Early return when no skills requested.
        if !include_all && names.is_empty() {
            return Ok(vec![]);
        }
        let definition = resolve_definition(&self.snapshot(), agent, git_root);
        self.skill_overlays_for(
            definition.as_deref(),
            agent,
            include_all,
            names,
            container_home_override,
            git_root,
        )
    }

    /// [`Self::skill_overlays`] for an already-resolved definition. `None`
    /// means `agent` did not resolve: the overlay is skipped with a warning.
    pub(crate) fn skill_overlays_for(
        &self,
        definition: Option<&AgentDefinition>,
        agent: &AgentName,
        include_all: bool,
        names: &[String],
        container_home_override: &Option<String>,
        git_root: &Path,
    ) -> Result<Vec<OverlaySpec>, EngineError> {
        self.skill_overlays_for_with_env(
            &self.snapshot(),
            definition,
            agent,
            include_all,
            names,
            container_home_override,
            git_root,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn skill_overlays_for_with_env(
        &self,
        env: &crate::data::config::env::EnvSnapshot,
        definition: Option<&AgentDefinition>,
        agent: &AgentName,
        include_all: bool,
        names: &[String],
        container_home_override: &Option<String>,
        git_root: &Path,
    ) -> Result<Vec<OverlaySpec>, EngineError> {
        // Early return when no skills requested.
        if !include_all && names.is_empty() {
            return Ok(vec![]);
        }
        let skill_dirs = crate::data::fs::skill_dirs::SkillDirs::from_env(env, None)
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

        // Where this agent reads skills from, per its definition.
        let container_path = match definition {
            Some(def) => match &def.skills_mount {
                Some(rel) => format!("{container_home}/{rel}"),
                None => {
                    tracing::warn!(
                        agent = agent.as_str(),
                        "skills overlay is not supported for this agent; no known skills directory"
                    );
                    return Ok(vec![]);
                }
            },
            None => {
                tracing::warn!(
                    agent = agent.as_str(),
                    "skills overlay: unknown agent, skipping"
                );
                return Ok(vec![]);
            }
        };

        if include_all {
            Ok(vec![OverlaySpec {
                host_path: OverlayPathResolver::canonicalize_lossy(&host_skills_dir),
                container_path: PathBuf::from(container_path),
                permission: OverlayPermission::ReadOnly,
            }])
        } else {
            let mut specs = Vec::new();
            for name in names {
                match name.split_once('/') {
                    // ── Single skill inside a pulled library: `library/skill` ──
                    //
                    // Mount only `<library>/<subdir>/<skill>` at
                    // `{container_path}/<library>/<skill>`, preserving the
                    // library namespace so `skill(lib)` and `skill(lib/skill)`
                    // never collide on container path when both are requested.
                    Some((library, skill)) => {
                        validate_skill_reference_segment(library, name)?;
                        validate_skill_reference_segment(skill, name)?;
                        let library_dir = skill_dirs.library_dir(library);
                        if !library_dir.exists() {
                            return Err(EngineError::Other(format!(
                                "skill library '{library}' not found in {} (for named skill '{name}')",
                                skill_dirs.library_root().display()
                            )));
                        }
                        let meta = read_library_meta(&library_dir).map_err(EngineError::Data)?;
                        let subdir = validate_library_subdir(&meta.subdir)?;
                        let skill_path = library_dir.join(&subdir).join(skill);
                        // A skill is a directory holding a `SKILL.md`. Merely
                        // existing is not enough: mounting an arbitrary
                        // directory inside a clone would expose non-skill
                        // content (including `.git/`) to the agent.
                        if !skill_path.is_dir() || !skill_path.join("SKILL.md").is_file() {
                            return Err(EngineError::Other(format!(
                                "skill '{skill}' not found in library '{library}' (looked for a SKILL.md in {})",
                                skill_path.display()
                            )));
                        }
                        specs.push(OverlaySpec {
                            host_path: OverlayPathResolver::canonicalize_lossy(&skill_path),
                            container_path: PathBuf::from(format!(
                                "{container_path}/{library}/{skill}"
                            )),
                            permission: OverlayPermission::ReadOnly,
                        });
                    }
                    // ── No slash: a plain skill, or a whole pulled library ──
                    None => {
                        validate_skill_reference_segment(name, name)?;
                        // 1. Plain skill wins — a user's own local skill is
                        //    never shadowed by a same-named pulled library.
                        let plain_dir = host_skills_dir.join(name);
                        if plain_dir.exists() {
                            specs.push(OverlaySpec {
                                host_path: OverlayPathResolver::canonicalize_lossy(&plain_dir),
                                container_path: PathBuf::from(format!("{container_path}/{name}")),
                                permission: OverlayPermission::ReadOnly,
                            });
                            continue;
                        }
                        // 2. Whole library — mount `<library>/<subdir>` at
                        //    `{container_path}/<name>`, giving the same mount
                        //    shape as any other named skill (a directory of
                        //    `<skill>/SKILL.md` entries).
                        let library_dir = skill_dirs.library_dir(name);
                        if library_dir.exists() {
                            let meta =
                                read_library_meta(&library_dir).map_err(EngineError::Data)?;
                            let subdir = validate_library_subdir(&meta.subdir)?;
                            let mount = library_dir.join(&subdir);
                            specs.push(OverlaySpec {
                                host_path: OverlayPathResolver::canonicalize_lossy(&mount),
                                container_path: PathBuf::from(format!("{container_path}/{name}")),
                                permission: OverlayPermission::ReadOnly,
                            });
                            continue;
                        }
                        // 3. Nothing resolved — name both search locations.
                        return Err(EngineError::Other(format!(
                            "named skill '{name}' not found in {} or {}",
                            host_skills_dir.display(),
                            skill_dirs.library_root().display()
                        )));
                    }
                }
            }
            Ok(specs)
        }
    }
}

fn skill_path_confined(root: &Path, path: &Path) -> bool {
    match (root.canonicalize(), path.canonicalize()) {
        (Ok(root), Ok(path)) => path.starts_with(root),
        _ => false,
    }
}

#[cfg(test)]
mod lease_skill_tests {
    use super::*;
    use crate::data::fs::skill_library::{write_library_meta, SkillLibraryMeta};

    #[test]
    fn installed_skill_review_matches_mount_resolution_and_verifies_builtin_identity() {
        let sandbox = awman_test_support::TestSandbox::for_test(awman_test_support::TestCase::component("engine::overlay::skills::lease_skill_tests::installed_skill_review_matches_mount_resolution_and_verifies_builtin_identity"));
        let env = crate::data::config::env::EnvSnapshot::with_overrides([(
            "AWMAN_HOME",
            sandbox.root().join("data").to_string_lossy().into_owned(),
        )]);
        let dirs = crate::data::fs::skill_dirs::SkillDirs::from_env(&env, None).unwrap();
        let library = dirs.library_dir("awman");
        let skill = library.join("skills/planner");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "planner").unwrap();
        let mut meta = SkillLibraryMeta {
            source: "https://github.com/prettysmartdev/awman".into(),
            owner: "prettysmartdev".into(),
            repo: "awman".into(),
            subdir: "skills".into(),
            sparse: true,
        };
        write_library_meta(&library, &meta).unwrap();
        assert_eq!(
            OverlayEngine::lease_skill_status(&env, "awman/planner"),
            SkillStatus::Installed {
                default_library: true
            }
        );
        assert_eq!(
            OverlayEngine::lease_skill_names(&env, &[]),
            ["awman", "awman/planner"]
        );
        assert_eq!(
            OverlayEngine::lease_skill_status(&env, "awman/missing"),
            SkillStatus::Missing
        );
        for name in ["awman/..", "../planner", "awman/a/b"] {
            assert_eq!(
                OverlayEngine::lease_skill_status(&env, name),
                SkillStatus::Missing
            );
        }
        meta.owner = "different-owner".into();
        write_library_meta(&library, &meta).unwrap();
        assert_eq!(
            OverlayEngine::lease_skill_status(&env, "awman/planner"),
            SkillStatus::Installed {
                default_library: false
            }
        );
        assert!(OverlayEngine::lease_skill_names(&env, &[]).is_empty());
        meta.owner = "prettysmartdev".into();
        write_library_meta(&library, &meta).unwrap();
        let plain = dirs.global_dir().join("awman");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("SKILL.md"), "local shadow").unwrap();
        assert_eq!(
            OverlayEngine::lease_skill_status(&env, "awman"),
            SkillStatus::Installed {
                default_library: false
            }
        );
        assert_eq!(
            OverlayEngine::lease_skill_names(&env, &[]),
            ["awman/planner"]
        );
        assert_eq!(
            OverlayEngine::lease_skill_names(&env, &["awman".into()]),
            ["awman", "awman/planner"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cannot_mark_a_skill_outside_the_library_as_installed() {
        let sandbox = awman_test_support::TestSandbox::for_test(awman_test_support::TestCase::component("engine::overlay::skills::lease_skill_tests::a_symlink_cannot_mark_a_skill_outside_the_library_as_installed"));
        let env = crate::data::config::env::EnvSnapshot::with_overrides([(
            "AWMAN_HOME",
            sandbox.root().join("data").to_string_lossy().into_owned(),
        )]);
        let dirs = crate::data::fs::skill_dirs::SkillDirs::from_env(&env, None).unwrap();
        std::fs::create_dir_all(dirs.global_dir()).unwrap();
        std::fs::write(sandbox.work().join("SKILL.md"), "outside").unwrap();
        std::os::unix::fs::symlink(sandbox.work(), dirs.global_dir().join("escape")).unwrap();
        assert_eq!(
            OverlayEngine::lease_skill_status(&env, "escape"),
            SkillStatus::Missing
        );
    }
}
