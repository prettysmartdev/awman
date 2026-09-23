//! `awman new workflow` — the step interview and the TOML/YAML writer.
//!
//! Split out of `commands/new.rs` by WI 0114 F-51: `run_with_frontend` was
//! one 530-line `match` over three unrelated scaffolds.

use super::*;

/// Run `awman new workflow`.
pub(super) async fn run(
    command: &NewCommand,
    f: NewWorkflowFlags,
    frontend: &mut dyn NewCommandFrontend,
) -> Result<NewOutcome, CommandError> {
    Ok({
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "new workflow: starting workflow creation".into(),
        });
        let name = frontend
            .ask_workflow_name()
            .unwrap_or_else(|_| "workflow".into());
        let extension = match f.format.as_str() {
            "yaml" => "yaml",
            "yml" => "yml",
            _ => "toml",
        };
        let session = if !f.global || f.interview {
            Some(command.session.clone())
        } else {
            None
        };

        // Resolve destination directory. Non-global workflows go
        // under <git_root>/aspec/workflows/ (the user-facing
        // definitions directory), matching old-amux behaviour.
        let dir = if f.global {
            let git_root = session.as_ref().map(|s| s.git_root().to_path_buf());
            let workflow_dirs = match WorkflowDirs::from_process_env(git_root) {
                Ok(d) => d,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("new workflow: failed to resolve workflow dirs: {e}"),
                    });
                    return Err(CommandError::from(e));
                }
            };
            workflow_dirs.global_dir()
        } else {
            let git_root = session
                .as_ref()
                .expect("session required for non-global workflow")
                .git_root();
            git_root.join(REPO_WORKFLOW_DEFINITIONS_DIR)
        };
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{name}.{extension}"));

        if f.interview {
            // Interview mode: write a skeleton, then launch an agent
            // to fill it in.
            let skeleton = match extension {
                "yaml" | "yml" => format!("title: \"{name}\"\nsteps: []\n"),
                _ => format!("title = \"{name}\"\n"),
            };
            let _ = std::fs::write(&path, skeleton);

            let session = session.as_ref().unwrap();
            let agent = match LaunchPolicy::for_session(session).resolve_agent(&None) {
                Ok(a) => a,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("new workflow: failed to resolve agent: {e}"),
                    });
                    return Err(e);
                }
            };
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!(
                    "new workflow: launching interview agent '{}'",
                    agent.as_str()
                ),
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
                        text: format!("new workflow: failed to resolve agent auth: {e}"),
                    });
                    return Err(CommandError::from(e));
                }
            };
            let summary = frontend.ask_workflow_summary().unwrap_or_default();
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&name)
                .to_string();
            let path_str = path.display().to_string();
            let prompt = render_workflow_interview_prompt(&filename, &path_str, &summary);
            let run_opts = AgentRunOptions {
                initial_prompt: Some(prompt),
                non_interactive: f.non_interactive,
                env_passthrough: None,
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
                        text: format!("new workflow: failed to build agent options: {e}"),
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
                            text: format!("new workflow: failed to build container: {e}"),
                        });
                        return Err(CommandError::from(e));
                    }
                };
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "Launching agent container…".into(),
            });
            frontend.set_pty_active(true);
            let cf = frontend.container_frontend_for_pty();
            let mut execution = match instance.run_with_frontend(cf) {
                Ok(e) => e,
                Err(e) => {
                    frontend.set_pty_active(false);
                    frontend.replay_queued();
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("new workflow: failed to run container: {e}"),
                    });
                    return Err(CommandError::from(e));
                }
            };
            let _ = execution.wait().await;
            frontend.set_pty_active(false);
            frontend.replay_queued();
        } else {
            // Non-interview: collect title and steps from the user,
            // then serialize the complete workflow to disk.
            let title = frontend
                .ask_workflow_title()
                .unwrap_or_else(|_| name.clone());
            let title = if title.is_empty() {
                name.clone()
            } else {
                title
            };

            let mut steps: Vec<WorkflowStepInput> = Vec::new();
            loop {
                let step_name = frontend.ask_workflow_step_name()?;
                if step_name.is_empty() {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: "Step name cannot be empty.".into(),
                    });
                    continue;
                }
                let agent = frontend.ask_workflow_step_agent()?;
                let model = frontend.ask_workflow_step_model()?;
                let prompt = frontend.ask_workflow_step_prompt()?;
                steps.push(WorkflowStepInput {
                    name: step_name,
                    agent,
                    model,
                    prompt,
                });
                match frontend.ask_add_another_step() {
                    Ok(true) => continue,
                    _ => break,
                }
            }

            if steps.is_empty() {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: "At least one step is required.".into(),
                });
                return Err(CommandError::Aborted);
            }

            let body = match extension {
                "yaml" | "yml" => serialize_workflow_yaml(&title, &steps),
                _ => serialize_workflow_toml(&title, &steps),
            };
            let _ = std::fs::write(&path, body);
        }

        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("Created workflow: {}", path.display()),
        });

        NewOutcome::Workflow(NewWorkflowOutcome {
            interview: f.interview,
            global: f.global,
            format: f.format,
            path: Some(path.display().to_string()),
        })
    })
}
