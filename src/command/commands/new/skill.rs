//! `awman new skill` — the skill interview, and pulling skill libraries.
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
            let skill_dirs = match SkillDirs::from_process_env(None) {
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
                interview: false,
                global: false,
                path: None,
                pull: true,
                libraries,
            })
        } else if let Some(target) = &f.pull {
            let skill_dirs = match SkillDirs::from_process_env(None) {
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
            frontend.write_message(pull_success_message(&outcome));
            let path = outcome.dir.display().to_string();
            NewOutcome::Skill(NewSkillOutcome {
                interview: false,
                global: false,
                path: Some(path),
                pull: true,
                libraries: vec![pull_library_outcome(outcome)],
            })
        } else {
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "new skill: starting skill creation".into(),
            });
            let name = frontend.ask_skill_name().unwrap_or_else(|_| "skill".into());
            let session = if !f.global || f.interview {
                Some(command.session.clone())
            } else {
                None
            };
            let git_root = session.as_ref().map(|s| s.git_root().to_path_buf());
            let skill_dirs = match SkillDirs::from_process_env(git_root) {
                Ok(d) => d,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("new skill: failed to resolve skill dirs: {e}"),
                    });
                    return Err(CommandError::from(e));
                }
            };
            let dir = if f.global {
                skill_dirs.global_dir().join(&name)
            } else {
                skill_dirs.repo_dir().unwrap().join(&name)
            };
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("SKILL.md");

            if f.interview {
                let skeleton = format!("# Skill: {name}\n\n## Description\n\n## Body\n");
                let _ = std::fs::write(&path, skeleton);
                let session = session.as_ref().unwrap();
                let agent = match LaunchPolicy::for_session(session).resolve_agent(&None) {
                    Ok(a) => a,
                    Err(e) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("new skill: failed to resolve agent: {e}"),
                        });
                        return Err(e);
                    }
                };
                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: format!("new skill: launching interview agent '{}'", agent.as_str()),
                });
                let credentials = match command
                    .engines
                    .auth_engine
                    .resolve_agent_auth(session, &agent)
                {
                    Ok(c) => c,
                    Err(e) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("new skill: failed to resolve agent auth: {e}"),
                        });
                        return Err(CommandError::from(e));
                    }
                };
                let summary = frontend.ask_skill_summary().unwrap_or_default();
                // The agent container always mounts the repo at
                // `/workspace`, which a `--global` skill never lives
                // under. Mount the new skill's own directory at a
                // fixed container path instead, and point the prompt
                // at that path rather than at the host one.
                let container_file = skill_interview_container_file(&path);
                let prompt = render_skill_interview_prompt(&container_file, &summary);
                let run_opts = AgentRunOptions {
                    initial_prompt: Some(prompt),
                    non_interactive: f.non_interactive,
                    env_passthrough: None,
                    directory_overlays: vec![skill_interview_overlay(&dir)],
                    ..Default::default()
                };
                // Sandbox-class runtimes: agent spawn lands in WI 0090.
                command
                    .engines
                    .require_container_runtime()
                    .map_err(CommandError::from)?;
                let mut options = match command.engines.agent_engine.build_options_with_credentials(
                    session,
                    &agent,
                    &run_opts,
                    &credentials,
                ) {
                    Ok(o) => o,
                    Err(e) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("new skill: failed to build agent options: {e}"),
                        });
                        return Err(CommandError::from(e));
                    }
                };
                if !credentials.env_vars.is_empty()
                    && matches!(
                        credentials.delivery,
                        crate::engine::auth::CredentialDelivery::Env
                    )
                {
                    options.push(ContainerOption::AgentCredentials {
                        env_vars: credentials.env_vars,
                    });
                }
                let instance =
                    match crate::engine::agent_runtime::ResolvedAgentOptions::container(options)
                        .and_then(|o| command.engines.runtime.build(o))
                    {
                        Ok(i) => i,
                        Err(e) => {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!("new skill: failed to build container: {e}"),
                            });
                            return Err(CommandError::from(e));
                        }
                    };
                frontend.set_pty_active(true);
                let cf = frontend.container_frontend_for_pty();
                let mut execution = match instance.run_with_frontend(cf) {
                    Ok(e) => e,
                    Err(e) => {
                        frontend.set_pty_active(false);
                        frontend.replay_queued();
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("new skill: failed to run container: {e}"),
                        });
                        return Err(CommandError::from(e));
                    }
                };
                let _ = execution.wait().await;
                frontend.set_pty_active(false);
                frontend.replay_queued();
            } else {
                let body = frontend.ask_skill_body().unwrap_or_default();
                let content = if body.is_empty() {
                    format!("# Skill: {name}\n\n## Description\n\n## Body\n")
                } else {
                    format!("# Skill: {name}\n\n{body}\n")
                };
                let _ = std::fs::write(&path, content);
            }

            NewOutcome::Skill(NewSkillOutcome {
                interview: f.interview,
                global: f.global,
                path: Some(path.display().to_string()),
                pull: false,
                libraries: Vec::new(),
            })
        }
    })
}
