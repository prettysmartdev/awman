//! Shared container-agent launch and wait services for creation and editing.

use tokio_util::sync::CancellationToken;

use super::LaunchPolicy;
use crate::command::commands::agent_setup::AgentLaunchFrontend;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage};
use crate::data::session::{AgentName, Session};
use crate::engine::agent::AgentRunOptions;
use crate::engine::agent_runtime::{AgentExitInfo, AgentInstance};
use crate::engine::auth::AgentCredentials;
use crate::engine::container::options::ContainerOption;
use crate::engine::error::EngineError;

pub struct AgentSessionRequest<'a> {
    pub label: &'a str,
    pub agent: &'a AgentName,
    /// The caller supplies the prompt, overlays and mode flags.
    pub run_opts: AgentRunOptions,
    pub credentials: Option<AgentCredentials>,
}

#[derive(Debug)]
pub enum AgentSessionError {
    /// Nothing ran; callers can discard unused drafts.
    Launch(CommandError),
    /// The agent ran; callers must still validate and save edits.
    Wait(EngineError),
}

pub fn build_instance(
    engines: &Engines,
    session: &Session,
    request: &AgentSessionRequest<'_>,
    frontend: &mut dyn AgentLaunchFrontend,
) -> Result<Box<dyn AgentInstance>, CommandError> {
    let AgentSessionRequest {
        label,
        agent,
        run_opts,
        credentials,
    } = request;
    let built = match credentials {
        Some(credentials) => engines.agent_engine.build_options_with_credentials(
            session,
            agent,
            run_opts,
            credentials,
        ),
        None => engines.agent_engine.build_options(session, agent, run_opts),
    };
    let mut options = match built {
        Ok(o) => o,
        Err(e) => {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("{label}: failed to build agent options: {e}"),
            });
            return Err(CommandError::from(e));
        }
    };
    if let Some(credentials) = credentials {
        if !credentials.env_vars.is_empty()
            && matches!(
                credentials.delivery,
                crate::engine::auth::CredentialDelivery::Env
            )
        {
            options.push(ContainerOption::AgentCredentials {
                env_vars: credentials.env_vars.clone(),
            });
        }
    }
    crate::engine::agent_runtime::ResolvedAgentOptions::container(options)
        .and_then(|o| engines.runtime.build(o))
        .map_err(|e| {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("{label}: failed to build container instance: {e}"),
            });
            CommandError::from(e)
        })
}

pub fn resolve_agent(
    session: &Session,
    flag: &Option<String>,
    label: &str,
    frontend: &mut dyn AgentLaunchFrontend,
) -> Result<AgentName, CommandError> {
    LaunchPolicy::for_session(session)
        .resolve_agent(flag)
        .inspect_err(|e| {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("{label}: failed to resolve agent: {e}"),
            });
        })
}

pub fn resolve_credentials(
    engines: &Engines,
    session: &Session,
    agent: &AgentName,
    label: &str,
    frontend: &mut dyn AgentLaunchFrontend,
) -> Result<AgentCredentials, CommandError> {
    engines
        .auth_engine
        .resolve_agent_auth(session, agent)
        .map_err(|e| {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("{label}: failed to resolve agent auth: {e}"),
            });
            CommandError::from(e)
        })
}

/// Launch a container, own the PTY until its single wait completes, and request
/// cancellation at most once. A wait error is distinct from a launch failure.
pub async fn run_agent_session(
    engines: &Engines,
    session: &Session,
    request: AgentSessionRequest<'_>,
    cancel: &CancellationToken,
    frontend: &mut dyn AgentLaunchFrontend,
) -> Result<AgentExitInfo, AgentSessionError> {
    engines
        .require_container_runtime()
        .map_err(CommandError::from)
        .map_err(AgentSessionError::Launch)?;
    let instance =
        build_instance(engines, session, &request, frontend).map_err(AgentSessionError::Launch)?;
    frontend.write_message(UserMessage {
        level: MessageLevel::Info,
        text: "Launching agent container…".into(),
    });
    frontend.set_pty_active(true);
    let cf = frontend.container_frontend_for_pty();
    let mut execution = match instance.run_with_frontend(cf) {
        Ok(execution) => execution,
        Err(error) => {
            frontend.set_pty_active(false);
            frontend.replay_queued();
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("{}: failed to run container: {error}", request.label),
            });
            return Err(AgentSessionError::Launch(CommandError::from(error)));
        }
    };
    let mut cancel_handle = execution.cancel_handle();
    let wait = execution.wait();
    tokio::pin!(wait);
    let mut cancelled = false;
    let result = loop {
        tokio::select! {
            result = &mut wait => break result,
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                if let Some(handle) = cancel_handle.take() {
                    let result = tokio::task::spawn_blocking(move || handle.cancel()).await;
                    if !matches!(result, Ok(Ok(()))) {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Warning,
                            text: match result {
                                Ok(Err(error)) => format!("Agent cancellation failed: {error}"),
                                _ => "Agent cancellation task failed".into(),
                            },
                        });
                    }
                }
            }
        }
    };
    frontend.set_pty_active(false);
    frontend.replay_queued();
    result.map_err(AgentSessionError::Wait)
}
