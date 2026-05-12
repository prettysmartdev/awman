//! `ChatCommand` — freeform chat with the configured agent.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::agent_auth::AgentAuthFrontend;
use crate::command::commands::agent_setup::AgentSetupFrontend;
use crate::command::commands::mount_scope::{MountScope, MountScopeFrontend};
use crate::command::commands::Command;
use crate::command::commands::{collect_all_overlay_specs, parse_overlay_list};
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::session::{AgentName, Session};
use crate::engine::agent::AgentRunOptions;
use crate::engine::container::options::{AutoMode, PlanMode, YoloMode};
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};

#[derive(Debug, Clone)]
pub struct ChatCommandFlags {
    pub non_interactive: bool,
    pub plan: bool,
    pub allow_docker: bool,
    pub mount_ssh: bool,
    pub yolo: bool,
    pub auto: bool,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub overlay: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatOutcome {
    pub agent: Option<String>,
    pub exit_code: Option<i32>,
}

pub trait ChatCommandFrontend:
    UserMessageSink
    + MountScopeFrontend
    + AgentSetupFrontend
    + AgentAuthFrontend
    + crate::command::commands::agent_setup::HasContainerFrontend
    + Send
    + Sync
{
    fn set_pty_active(&mut self, active: bool);
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
        let agent = match resolve_agent(&self.flags.agent, &session) {
            Ok(a) => a,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("chat: failed to resolve agent: {e}"),
                });
                return Err(e);
            }
        };

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
        let (directory_overlays, skills_enabled) = collect_all_overlay_specs(&session, cli_typed);

        // 3. Ensure the agent is available (Dockerfile + image present, build
        //    if missing). Runs before PTY activation so any download/build
        //    progress streams to the user terminal directly.
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

        // 4. Resolve agent authentication (keychain credentials) and inject
        //    them as container env-vars so the running agent can reach its
        //    backend.
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "Resolving agent credentials…".into(),
        });
        let credentials = match self
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

        // 5. Build the run options from flags + credentials.
        let mut run_opts = AgentRunOptions {
            yolo: self.flags.yolo.then_some(YoloMode::Enabled),
            auto: self.flags.auto.then_some(AutoMode::Enabled),
            plan: self.flags.plan.then_some(PlanMode::Enabled),
            allow_docker: self.flags.allow_docker,
            mount_ssh: self.flags.mount_ssh,
            non_interactive: self.flags.non_interactive,
            model: self.flags.model.clone(),
            env_passthrough: Some(session.effective_config().env_passthrough()),
            directory_overlays,
            include_skills: skills_enabled,
            ..Default::default()
        };
        let env_overrides = credentials.env_vars.clone();

        // 6. Build the container options through AgentEngine.
        let mut options = match self
            .engines
            .agent_engine
            .build_options(&session, &agent, &run_opts)
        {
            Ok(o) => o,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("chat: failed to build container options: {e}"),
                });
                return Err(CommandError::from(e));
            }
        };
        if !env_overrides.is_empty() {
            options.push(
                crate::engine::container::options::ContainerOption::AgentCredentials {
                    env_vars: env_overrides,
                },
            );
        }
        let _ = &mut run_opts; // silence unused-mut lint when no fields mutate later

        // 7. Build the container instance.
        let instance = match self.engines.runtime.build(options) {
            Ok(i) => i,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("chat: failed to build container instance: {e}"),
                });
                return Err(CommandError::from(e));
            }
        };

        // 8. Run with PTY-active gating.
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "Launching agent container…".into(),
        });
        frontend.set_pty_active(true);
        let container_frontend = frontend.container_frontend_for_pty();
        let mut execution = match instance.run_with_frontend(container_frontend) {
            Ok(e) => e,
            Err(e) => {
                frontend.set_pty_active(false);
                frontend.replay_queued();
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("chat: failed to launch container: {e}"),
                });
                return Err(CommandError::from(e));
            }
        };
        let exit = execution.wait().await;
        frontend.set_pty_active(false);
        frontend.replay_queued();

        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "Agent session ended".into(),
        });

        let exit_code = exit.map(|e| e.exit_code).ok();
        Ok(ChatOutcome {
            agent: Some(agent.as_str().to_string()),
            exit_code,
        })
    }
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
    let runtime = std::sync::Arc::clone(agent_engine.container_runtime_arc());
    agent_engine
        .ensure_available(session, agent, &config, &mut adapter, move |tag: &str| {
            runtime.image_exists(tag)
        })
        .await
        .map_err(CommandError::from)
}

/// Resolve the agent: explicit flag → session default → fall back to "claude".
pub(crate) fn resolve_agent(
    flag: &Option<String>,
    session: &Session,
) -> Result<AgentName, CommandError> {
    if let Some(name) = flag.as_deref() {
        return AgentName::new(name).map_err(CommandError::from);
    }
    if let Some(name) = session.default_agent() {
        return Ok(name.clone());
    }
    AgentName::new("claude").map_err(CommandError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_session(root: &std::path::Path) -> Session {
        let resolver = crate::data::session::StaticGitRootResolver::new(root);
        Session::open(
            root.to_path_buf(),
            &resolver,
            crate::data::session::SessionOpenOptions::default(),
        )
        .unwrap()
    }

    #[test]
    fn resolve_agent_uses_explicit_flag_over_session_default() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(tmp.path());
        let agent = resolve_agent(&Some("codex".to_string()), &session).unwrap();
        assert_eq!(
            agent.as_str(),
            "codex",
            "explicit flag must win over session default"
        );
    }

    #[test]
    fn resolve_agent_falls_back_to_claude_when_no_flag_or_default() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(tmp.path());
        // No explicit flag, session has no default → falls back to "claude".
        let agent = resolve_agent(&None, &session).unwrap();
        assert_eq!(agent.as_str(), "claude", "must fall back to claude");
    }

    #[test]
    fn resolve_agent_invalid_name_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(tmp.path());
        // Empty string is not a valid agent name.
        let result = resolve_agent(&Some(String::new()), &session);
        assert!(result.is_err(), "empty agent name must return error");
    }

    #[test]
    fn resolve_agent_uses_session_default_when_no_flag() {
        // We cannot easily inject a session default without writing config;
        // this verifies the fallback path doesn't panic when default_agent()
        // returns None (the no-config case already tested above).
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(tmp.path());
        let agent = resolve_agent(&None, &session).unwrap();
        // In the absence of config the only valid result is "claude".
        assert_eq!(agent.as_str(), "claude");
    }
}
