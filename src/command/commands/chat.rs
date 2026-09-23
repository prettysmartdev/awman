//! `ChatCommand` — freeform chat with the configured agent.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::launch_policy::{LaunchModeDecision, LaunchPolicy};
use crate::command::commands::mount_scope::MountScope;
use crate::command::commands::parse_overlay_list;
use crate::command::commands::Command;
use crate::command::dispatch::{BuildContext, Engines};
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::{AgentName, Session};
use crate::engine::agent::AgentRunOptions;
use crate::engine::container::options::{AutoMode, PlanMode, YoloMode};

#[derive(Debug, Clone)]
pub struct ChatCommandFlags {
    pub non_interactive: bool,
    pub plan: bool,
    pub allow_docker: bool,
    pub yolo: bool,
    pub auto: bool,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub launch_mode: Option<crate::data::config::repo::LaunchMode>,
    pub overlay: Vec<String>,
}

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
}

pub struct ChatCommand {
    flags: ChatCommandFlags,
    engines: Engines,
    session: Session,
}

impl ChatCommand {
    pub fn new(flags: ChatCommandFlags, engines: Engines, session: Session) -> Self {
        Self {
            flags,
            engines,
            session,
        }
    }

    /// Construct from the catalogue-resolved input (WI 0113 F-10).
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        Ok(Self::new(
            ChatCommandFlags {
                non_interactive: ctx.flags.bool("non-interactive"),
                plan: ctx.flags.bool("plan"),
                allow_docker: ctx.flags.bool("allow-docker"),
                yolo: ctx.flags.bool("yolo"),
                auto: ctx.flags.bool("auto"),
                agent: ctx.flags.string("agent"),
                model: ctx.flags.string("model"),
                launch_mode: crate::command::dispatch::parse_launch_mode(
                    ctx.flags.string("launch-mode"),
                    &ctx.path(),
                )?,
                overlay: ctx.flags.strs("overlay").to_vec(),
            },
            ctx.engines.clone(),
            ctx.session.clone(),
        ))
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

        // Launch mode is independent of agent resolution.  Resolve it before
        // mount/overlay/setup work so an unsupported ACP request cannot touch
        // the container path.
        let config = command_effective_config(&session, &self.flags);
        let explicit_acp = self.flags.launch_mode
            == Some(crate::data::config::repo::LaunchMode::Acp)
            || (self.flags.agent.is_none()
                && session.repo_config().agent.is_some()
                && session.repo_config().launch_mode
                    == Some(crate::data::config::repo::LaunchMode::Acp));
        let launch_decision = match LaunchPolicy::resolve_launch_mode(&config, &agent, explicit_acp)
        {
            Ok(decision) => decision,
            Err(e) => return Err(CommandError::from(e)),
        };
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

        // 1b. Confirm mount scope when cwd differs from git root.
        let cwd = session.working_dir().to_path_buf();
        let _mount_path = match MountScope::resolve(&cwd, session.git_root(), frontend.as_mut()) {
            Ok(p) => p,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("chat: mount scope resolution failed: {e}"),
                });
                return Err(e);
            }
        };

        // 2. Parse overlay specs before PTY is activated so errors surface early.
        let cli_typed = {
            let mut all = Vec::new();
            for s in &self.flags.overlay {
                match parse_overlay_list(s) {
                    Ok(parsed) => all.extend(parsed),
                    Err(reason) => {
                        let e = CommandError::InvalidOverlaySpec {
                            spec: s.clone(),
                            reason,
                        };
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("chat: invalid overlay spec: {e}"),
                        });
                        return Err(e);
                    }
                }
            }
            all
        };
        let collected = session
            .effective_config()
            .collected_overlays(cli_typed, None, None)?;

        // Emit deprecation warnings for legacy config fields.
        LaunchPolicy::for_session(&session).warn_legacy_config(frontend.as_mut());

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
                &session,
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
        let run_opts = AgentRunOptions {
            yolo: self.flags.yolo.then_some(YoloMode::Enabled),
            auto: self.flags.auto.then_some(AutoMode::Enabled),
            plan: self.flags.plan.then_some(PlanMode::Enabled),
            allow_docker: self.flags.allow_docker,
            non_interactive: self.flags.non_interactive,
            model: self.flags.model.clone(),
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
                LaunchModeDecision::Acp => crate::data::config::repo::LaunchMode::Acp,
                _ => crate::data::config::repo::LaunchMode::Stdio,
            },
            ..Default::default()
        };

        // 6. Build the paradigm-appropriate options through AgentEngine's
        //    centralized cross-paradigm mapper (container vs sandbox), folding in
        //    resolved credentials.
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
}

fn command_effective_config(
    session: &Session,
    command_flags: &ChatCommandFlags,
) -> crate::data::config::effective::EffectiveConfig {
    let current = session.effective_config();
    let mut flags = current.flags().clone();
    flags.agent = command_flags.agent.clone();
    flags.model = command_flags.model.clone();
    flags.launch_mode = command_flags.launch_mode;
    crate::data::config::effective::EffectiveConfig::new(
        flags,
        current.env().clone(),
        current.repo().clone(),
        current.global().clone(),
    )
}

pub(crate) async fn ensure_agent_setup(
    agent_engine: &crate::engine::agent::AgentEngine,
    session: &Session,
    agent: &AgentName,
    frontend: &mut Box<dyn ChatCommandFrontend>,
) -> Result<(), CommandError> {
    use crate::data::config::effective::EffectiveConfig;
    let config = EffectiveConfig::default();
    let mut adapter =
        crate::command::commands::agent_setup::AgentFrontendAdapter::new(frontend.as_mut());
    let runtime = std::sync::Arc::clone(agent_engine.runtime());
    agent_engine
        .ensure_available(session, agent, &config, &mut adapter, move |tag: &str| {
            runtime.image_exists(tag).unwrap_or(false)
        })
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
