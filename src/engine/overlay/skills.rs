//! Mounting the global skills library into a container.
//!
//! Split out of `engine/overlay/mod.rs` by WI 0114 F-51. A child module of
//! `overlay`, so it reaches `OverlayEngine`'s private items unchanged.

use super::*;

impl OverlayEngine {
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

        // Where this agent reads skills from, per the one per-agent table.
        let container_path = match matrix_for(agent.as_str()) {
            Ok(matrix) => match matrix.skills_mount {
                Some(rel) => format!("{container_home}/{rel}"),
                None => {
                    tracing::warn!(
                        agent = agent.as_str(),
                        "skills overlay is not supported for this agent; no known skills directory"
                    );
                    return Ok(vec![]);
                }
            },
            Err(_) => {
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
