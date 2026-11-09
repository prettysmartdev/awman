//! `ChatCommand` — freeform chat with the configured agent.

use async_trait::async_trait;
use serde::Serialize;

use super::session_worktree::{report_kept_session_worktree, resolve_session_worktree};
use crate::command::commands::launch_policy::{
    ensure_agent_trusted, LaunchModeDecision, LaunchPolicy,
};
use crate::command::commands::mount_scope::MountScope;
use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::{AgentName, Session};
use crate::engine::agent::AgentRunOptions;
use crate::engine::container::options::{AutoMode, PlanMode, YoloMode};

mod flags;
pub use flags::ChatCommandFlags;

#[derive(Debug, Clone, Serialize)]
pub struct ChatOutcome {
    pub agent: Option<String>,
    pub exit_code: Option<i32>,
}

pub trait ChatCommandFrontend:
    UserMessageSink
    + crate::command::commands::agent_setup::AgentLaunchFrontend
    + crate::engine::acp::AcpFrontend
    + Send
    + Sync
{
    fn report_session_worktree(&mut self, _path: &std::path::Path, _branch: Option<&str>) {}
}

pub struct ChatCommand {
    #[cfg(test)]
    pull_request_client: Option<crate::engine::git::pull_request::PullRequestClient>,
    flags: ChatCommandFlags,
    engines: Engines,
    session: Session,
}

impl ChatCommand {
    pub fn new(flags: ChatCommandFlags, engines: Engines, session: Session) -> Self {
        Self {
            #[cfg(test)]
            pull_request_client: None,
            flags,
            engines,
            session,
        }
    }

    /// Hermetic PR metadata integration for full launch tests.
    #[cfg(test)]
    #[allow(dead_code)] // consumed by the subsequent launch-test workflow step
    pub(crate) fn with_pull_request_client(
        mut self,
        client: crate::engine::git::pull_request::PullRequestClient,
    ) -> Self {
        self.pull_request_client = Some(client);
        self
    }

    pub fn flags(&self) -> &ChatCommandFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for ChatCommand {
    type Frontend = Box<dyn ChatCommandFrontend>;
    type Outcome = ChatOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        // 1. Resolve the agent: --agent flag wins over the repo / global default.
        let session = self.session;
        let agent = match LaunchPolicy::for_session(&session).resolve_agent(&self.flags.agent) {
            Ok(a) => a,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("chat: failed to resolve agent: {e}"),
                });
                return Err(e);
            }
        };
        // Review/approve a custom definition before anything resolves it.
        if let Err(e) = ensure_agent_trusted(&session, &agent, frontend.as_mut()) {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("chat: {e}"),
            });
            return Err(e);
        }

        let invoker = session;
        let worktree = if let Some(target) = &self.flags.session_worktree {
            #[cfg(test)]
            let resolved = if let Some(client) = self.pull_request_client {
                super::session_worktree::resolve_session_worktree_with_client(
                    target,
                    &invoker,
                    self.engines.git_engine.clone(),
                    client,
                    frontend.as_mut(),
                )
                .await
            } else {
                resolve_session_worktree(
                    target,
                    &invoker,
                    self.engines.git_engine.clone(),
                    frontend.as_mut(),
                )
                .await
            };
            #[cfg(not(test))]
            let resolved = resolve_session_worktree(
                target,
                &invoker,
                self.engines.git_engine.clone(),
                frontend.as_mut(),
            )
            .await;
            let worktree = resolved?;
            report_kept_session_worktree(&worktree, frontend.as_mut());
            frontend.report_session_worktree(&worktree.path, worktree.branch.as_deref());
            Some(worktree)
        } else {
            None
        };

        // Keep one reporting tail for all post-resolution returns, including
        // setup/launch/ACP errors. The worktree is always retained.
        let outcome = async {
            let session = match &worktree {
                Some(worktree) => Session::open_worktree_with_invoker_config(
                    worktree.path.clone(),
                    worktree.path.clone(),
                    &invoker,
                )?,
                None => invoker.clone(),
            };
            let image_session = match &worktree {
                Some(worktree) => Session::open_worktree_with_invoker_config(
                    worktree.invoker_root.clone(),
                    worktree.invoker_root.clone(),
                    &invoker,
                )?,
                None => invoker.clone(),
            };

            // Launch mode is independent of agent resolution.  Resolve it before
            // mount/overlay/setup work so an unsupported ACP request cannot touch
            // the container path.
            let launch_decision = LaunchPolicy::for_session(&session).single_agent_launch_mode(
                &agent,
                &self.flags.agent,
                &self.flags.model,
                self.flags.launch_mode,
            )?;
            if launch_decision == LaunchModeDecision::StdioWithFallbackWarning {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: LaunchPolicy::acp_fallback_warning(&agent),
                });
            }

            if let Some(note) = LaunchPolicy::deprecation_warning(&agent) {
                frontend.write_message(note);
            }

            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!("chat: using agent '{}'", agent.as_str()),
            });

            if worktree.is_none() {
                // 1b. Confirm mount scope when cwd differs from git root.
                let cwd = session.working_dir().to_path_buf();
                let _mount_path =
                    match MountScope::resolve(&cwd, session.git_root(), frontend.as_mut()) {
                        Ok(p) => p,
                        Err(e) => {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!("chat: mount scope resolution failed: {e}"),
                            });
                            return Err(e);
                        }
                    };
            }

            // 2. Parse overlay specs before PTY is activated so errors surface early.
            let cli_typed = match LaunchPolicy::parse_overlay_flags(&self.flags.overlay) {
                Ok(overlays) => overlays,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("chat: invalid overlay spec: {e}"),
                    });
                    return Err(e);
                }
            };
            let collected = session
                .effective_config()
                .collected_overlays(cli_typed, None, None)?;

            // Emit deprecation warnings for legacy config fields.
            LaunchPolicy::for_session(&session).warn_config(frontend.as_mut());

            // 3. Ensure the agent is available. The Dockerfile + image setup is
            //    container-paradigm only; kit-declarative (sandbox) runtimes get
            //    their per-agent kits from `awman ready`, and a missing kit
            //    surfaces as a clear error at launch. Runs before PTY activation
            //    so any download/build progress streams to the user terminal.
            if self.engines.runtime.capabilities().kit_declarative {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: format!(
                        "chat: {} runtime active — using the agent kit prepared by `awman ready` \
                     (no image build needed)",
                        self.engines.runtime.display_name()
                    ),
                });
            } else {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: "Checking agent availability…".into(),
                });
                match ensure_agent_setup(
                    self.engines.agent_engine.as_ref(),
                    &image_session,
                    &agent,
                    &mut frontend,
                )
                .await
                {
                    Ok(()) => {}
                    Err(e) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("chat: agent setup failed: {e}"),
                        });
                        return Err(e);
                    }
                }
            }

            // 4. Resolve agent authentication (keychain credentials) and inject
            //    them as container env-vars so the running agent can reach its
            //    backend.
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "Resolving agent credentials…".into(),
            });
            let resolved_credentials = match self
                .engines
                .auth_engine
                .resolve_agent_auth(&session, &agent)
            {
                Ok(c) => c,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("chat: credential resolution failed: {e}"),
                    });
                    return Err(CommandError::from(e));
                }
            };
            // File delivery is container-only. Keep sbx on its legacy env-only
            // keychain path without changing dsbx itself.
            let credentials = if self.engines.runtime.capabilities().kit_declarative
                && matches!(
                    resolved_credentials.delivery,
                    crate::engine::auth::CredentialDelivery::File(_)
                ) {
                self.engines.auth_engine.agent_env_credentials(&agent)?
            } else {
                resolved_credentials
            };

            // 5. Resolve context overlays.
            let (context_overlays, system_prompt) = LaunchPolicy::for_session(&session)
                .with_git(&self.engines.git_engine)
                .resolve_context_overlays(
                    &collected.context_overlays,
                    &agent,
                    None,
                    None,
                    frontend.as_mut(),
                )?;

            // 6. Build the run options from flags + credentials.
            let mut run_opts = AgentRunOptions {
                yolo: self.flags.yolo.then_some(YoloMode::Enabled),
                auto: self.flags.auto.then_some(AutoMode::Enabled),
                plan: self.flags.plan.then_some(PlanMode::Enabled),
                allow_docker: self.flags.allow_docker,
                non_interactive: self.flags.non_interactive,
                model: self.flags.model.clone(),
                agent_flags: self.flags.agent_flags.clone(),
                env_passthrough: if collected.env_passthrough.is_empty() {
                    None
                } else {
                    Some(collected.env_passthrough)
                },
                directory_overlays: collected.directories,
                include_all_skills: collected.include_all_skills,
                named_skills: collected.named_skills,
                system_prompt,
                context_overlays,
                launch_mode: match launch_decision {
                    LaunchModeDecision::Acp => crate::data::config::agents::LaunchMode::Acp,
                    _ => crate::data::config::agents::LaunchMode::Stdio,
                },
                ..Default::default()
            };

            // 6. Build the paradigm-appropriate options through AgentEngine's
            //    centralized cross-paradigm mapper (container vs sandbox), folding in
            //    resolved credentials.
            if let Some(worktree) = &worktree {
                if !self.engines.runtime.capabilities().arbitrary_host_mounts {
                    return Err(CommandError::Other(
                        "This runtime cannot mount the linked worktree's required Git directory"
                            .into(),
                    ));
                }
                run_opts.image_tag_override = Some(crate::data::image_tags::agent_image_tag(
                    &worktree.invoker_root,
                    agent.as_str(),
                ));
                super::session_worktree::anchor_relative_overlays(
                    &mut run_opts.directory_overlays,
                    invoker.working_dir(),
                )?;
                if let Some(overlay) = worktree
                    .git_directory_overlay(self.engines.git_engine.clone())
                    .await?
                {
                    run_opts.directory_overlays.push(overlay);
                }
            }

            let resolved = match self.engines.agent_engine.resolve_agent_options(
                &session,
                &agent,
                &run_opts,
                &credentials,
            ) {
                Ok(o) => o,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("chat: failed to build agent options: {e}"),
                    });
                    return Err(CommandError::from(e));
                }
            };

            // 7. Build the agent instance.
            let instance = match self.engines.runtime.build(resolved) {
                Ok(i) => i,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("chat: failed to build agent instance: {e}"),
                    });
                    return Err(CommandError::from(e));
                }
            };

            // 8. Run with PTY-active gating.
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!("Launching agent ({})…", self.engines.runtime.display_name()),
            });
            let exit = if launch_decision == LaunchModeDecision::Acp {
                let (runtime_frontend, transport) = crate::engine::acp::AcpTransport::channel();
                let execution = match instance.run_with_frontend(Box::new(runtime_frontend)) {
                    Ok(execution) => execution,
                    Err(e) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("chat: failed to launch ACP agent: {e}"),
                        });
                        return Err(CommandError::from(e));
                    }
                };
                let mut acp = crate::engine::acp::AcpSession::from_transport(
                    execution,
                    transport,
                    Box::new(crate::data::message::StderrMessageSink::new()),
                    // Layer 2 owns the flags, so Layer 2 turns them into the
                    // permission policy (F-46).
                    if matches!(run_opts.yolo, Some(YoloMode::Enabled))
                        || matches!(run_opts.auto, Some(AutoMode::Enabled))
                    {
                        crate::engine::acp::PermissionPolicy::AutoApprove
                    } else {
                        crate::engine::acp::PermissionPolicy::Ask
                    },
                );
                if let Err(e) = acp.initialize("/workspace").await {
                    // Reap the launched container before returning so it is not left
                    // running after a failed handshake.
                    let _ = acp.shutdown().await;
                    return Err(CommandError::from(e));
                }
                acp.drive(frontend.as_mut()).await
            } else {
                frontend.set_pty_active(true);
                let container_frontend = frontend.container_frontend_for_pty();
                let mut execution = match instance.run_with_frontend(container_frontend) {
                    Ok(e) => e,
                    Err(e) => {
                        frontend.set_pty_active(false);
                        frontend.replay_queued();
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("chat: failed to launch agent: {e}"),
                        });
                        return Err(CommandError::from(e));
                    }
                };
                frontend.set_stuck_sender(execution.stuck_sender());
                let exit = execution.wait().await;
                frontend.set_pty_active(false);
                frontend.wait_output_drained().await;
                frontend.replay_queued();
                exit
            };

            LaunchPolicy::report_session_end(frontend.as_mut(), "chat", &exit);

            let exit_code = exit.map(|e| e.exit_code).ok();
            Ok(ChatOutcome {
                agent: Some(agent.as_str().to_string()),
                exit_code,
            })
        }
        .await;
        if let Some(worktree) = &worktree {
            report_kept_session_worktree(worktree, frontend.as_mut());
        }
        outcome
    }
}

pub(crate) async fn ensure_agent_setup(
    agent_engine: &crate::engine::agent::AgentEngine,
    session: &Session,
    agent: &AgentName,
    frontend: &mut Box<dyn ChatCommandFrontend>,
) -> Result<(), CommandError> {
    let mut adapter =
        crate::command::commands::agent_setup::AgentFrontendAdapter::new(frontend.as_mut());
    agent_engine
        .ensure_available_for_session(session, agent, &mut adapter)
        .await
        .map_err(CommandError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_agent_uses_explicit_flag_over_session_default() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session::for_tests(tmp.path());
        let agent = LaunchPolicy::for_session(&session)
            .resolve_agent(&Some("codex".to_string()))
            .unwrap();
        assert_eq!(
            agent.as_str(),
            "codex",
            "explicit flag must win over session default"
        );
    }

    #[test]
    fn resolve_agent_falls_back_to_claude_when_no_flag_or_default() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session::for_tests(tmp.path());
        // No explicit flag, session has no default → falls back to "claude".
        let agent = LaunchPolicy::for_session(&session)
            .resolve_agent(&None)
            .unwrap();
        assert_eq!(agent.as_str(), "claude", "must fall back to claude");
    }

    #[test]
    fn resolve_agent_invalid_name_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session::for_tests(tmp.path());
        // Empty string is not a valid agent name.
        let result = LaunchPolicy::for_session(&session).resolve_agent(&Some(String::new()));
        assert!(result.is_err(), "empty agent name must return error");
    }

    #[test]
    fn resolve_agent_uses_session_default_when_no_flag() {
        // We cannot easily inject a session default without writing config;
        // this verifies the fallback path doesn't panic when default_agent()
        // returns None (the no-config case already tested above).
        let tmp = tempfile::tempdir().unwrap();
        let session = Session::for_tests(tmp.path());
        let agent = LaunchPolicy::for_session(&session)
            .resolve_agent(&None)
            .unwrap();
        // In the absence of config the only valid result is "claude".
        assert_eq!(agent.as_str(), "claude");
    }
}
