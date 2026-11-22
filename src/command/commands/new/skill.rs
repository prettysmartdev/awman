//! `awman new skill` — agent authoring and pulling skill libraries.
//!
//! Split out of `commands/new.rs` by WI 0114 F-51: `run_with_frontend` was
//! one 530-line `match` over three unrelated scaffolds.

use super::*;

/// Run `awman new skill`.
pub(super) async fn run(
    command: &NewCommand,
    f: NewSkillFlags,
    frontend: &mut dyn NewCommandFrontend,
) -> Result<NewOutcome, CommandError> {
    Ok({
        if f.pull_all {
            let skill_dirs = match SkillDirs::from_env(command.session.env(), None) {
                Ok(dirs) => dirs,
                Err(error) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("new skill: failed to resolve skill dirs: {error}"),
                    });
                    return Err(CommandError::from(error));
                }
            };
            let slugs = skill_dirs.list_libraries();
            let results = pull_all_libraries(&command.engines.git_engine, &skill_dirs);
            let mut libraries = Vec::with_capacity(results.len());
            let mut successes = 0usize;
            let mut failures = 0usize;

            for (slug, result) in slugs.into_iter().zip(results) {
                match result {
                    Ok(outcome) => {
                        if let Some(warning) = &outcome.sparse_fallback_warning {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Warning,
                                text: warning.clone(),
                            });
                        }
                        frontend.write_message(pull_success_message(&outcome));
                        successes += 1;
                        libraries.push(pull_library_outcome(outcome));
                    }
                    Err(error) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("Failed to pull '{slug}': {error}"),
                        });
                        failures += 1;
                        let dir = skill_dirs.library_dir(&slug).display().to_string();
                        libraries.push(PullLibraryOutcome {
                            slug,
                            dir,
                            updated: false,
                            skills_found: Vec::new(),
                            error: Some(error.to_string()),
                        });
                    }
                }
            }

            if libraries.is_empty() {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: "no skill libraries pulled yet".to_string(),
                });
            } else {
                frontend.write_message(UserMessage {
                    level: if failures == 0 {
                        MessageLevel::Info
                    } else {
                        MessageLevel::Error
                    },
                    text: format!(
                        "Skill library refresh complete: {successes} succeeded, {failures} failed."
                    ),
                });
            }

            NewOutcome::Skill(NewSkillOutcome {
                global: false,
                path: None,
                pull: true,
                libraries,
            })
        } else if let Some(target) = &f.pull {
            let skill_dirs = match SkillDirs::from_env(command.session.env(), None) {
                Ok(dirs) => dirs,
                Err(error) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("new skill: failed to resolve skill dirs: {error}"),
                    });
                    return Err(CommandError::from(error));
                }
            };
            let outcome = match resolve_pull_target(target)
                .map_err(CommandError::Other)
                .and_then(|target| {
                    pull_library(
                        &command.engines.git_engine,
                        &skill_dirs,
                        target,
                        f.subdir.as_deref(),
                    )
                }) {
                Ok(outcome) => outcome,
                Err(error) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("new skill: failed to pull skill library: {error}"),
                    });
                    return Err(error);
                }
            };
            if let Some(warning) = &outcome.sparse_fallback_warning {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: warning.clone(),
                });
            }
            frontend.write_message(pull_success_message(&outcome));
            let path = outcome.dir.display().to_string();
            NewOutcome::Skill(NewSkillOutcome {
                global: false,
                path: Some(path),
                pull: true,
                libraries: vec![pull_library_outcome(outcome)],
            })
        } else {
            command.engines.require_container_runtime()?;
            let title = f
                .input
                .title(f.non_interactive, || frontend.ask_skill_name())?;
            let summary = f
                .input
                .summary(f.non_interactive, || frontend.ask_skill_summary())?;
            let name = crate::command::work_item::spec_service::slugify(&title);
            if name.len() > 64 {
                return Err(CommandError::Other(
                    "skill name must be at most 64 characters".into(),
                ));
            }
            let dirs = SkillDirs::from_env(
                command.session.env(),
                Some(command.session.git_root().to_path_buf()),
            )?;
            let dir = if f.global {
                dirs.global_dir()
            } else {
                dirs.repo_dir().expect("repository session")
            }
            .join(&name);
            let path = dir.join("SKILL.md");
            let metadata =
                serde_yaml::to_string(&serde_json::json!({"name": name, "description": summary}))
                    .map_err(|e| CommandError::Other(e.to_string()))?;
            let seed = format!("---\n{metadata}---\n\n# {title}\n");
            let root = if f.global {
                crate::data::fs::data_paths::data_home(command.session.env())?
            } else {
                command.session.git_root().to_path_buf()
            };
            write_seed(&root, &path, &seed)?;
            author_artifact(
                command,
                crate::data::fs::edit_target::ArtifactKind::Skill,
                &path,
                f.global,
                NewSession {
                    headless: f.non_interactive,
                    yolo: f.yolo,
                    yolo_authorized: f.yolo_authorized,
                    summary,
                },
                frontend,
            )
            .await?;
            NewOutcome::Skill(NewSkillOutcome {
                global: f.global,
                path: Some(path.display().to_string()),
                pull: false,
                libraries: Vec::new(),
            })
        }
    })
}
