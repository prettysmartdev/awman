//! `ExecPromptCommand` — one-shot prompt injection.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::agent_auth::AgentAuthFrontend;
use crate::command::commands::agent_setup::AgentSetupFrontend;
use crate::command::commands::chat::{open_session_for_cwd, resolve_agent};
use crate::command::commands::mount_scope::MountScopeFrontend;
use crate::command::commands::{collect_all_overlay_specs, parse_overlay_spec};
use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::session::{AgentName, Session};
use crate::engine::agent::AgentRunOptions;
use crate::engine::container::options::{AutoMode, PlanMode, YoloMode};
use crate::engine::message::UserMessageSink;

#[derive(Debug, Clone)]
pub struct ExecPromptCommandFlags {
    pub prompt: String,
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
pub struct ExecPromptOutcome {
    pub agent: Option<String>,
    pub exit_code: Option<i32>,
}

pub trait ExecPromptCommandFrontend:
    UserMessageSink
    + MountScopeFrontend
    + AgentSetupFrontend
    + AgentAuthFrontend
    + crate::command::commands::agent_setup::HasContainerFrontend
    + Send
    + Sync
{
    /// Inform the frontend that the host stdio is now owned by a running
    /// container. Frontends that would otherwise interleave UserMessages with
    /// container output (e.g. the CLI) queue messages until the container
    /// releases stdio. Default impl: no-op (suitable for non-blocking sinks
    /// like the TUI).
    fn set_pty_active(&mut self, _active: bool) {}
}

async fn ensure_exec_prompt_agent_setup(
    agent_engine: &crate::engine::agent::AgentEngine,
    session: &Session,
    agent: &AgentName,
    frontend: &mut Box<dyn ExecPromptCommandFrontend>,
) -> Result<(), CommandError> {
    use crate::data::config::effective::EffectiveConfig;
    let config = EffectiveConfig::default();
    let mut adapter =
        crate::command::commands::agent_setup::AgentFrontendAdapter::new(frontend.as_mut());
    let runtime = std::sync::Arc::clone(agent_engine.container_runtime_arc());
    agent_engine
        .ensure_available(
            session,
            agent,
            &config,
            &mut adapter,
            move |tag: &str| runtime.image_exists(tag),
        )
        .await
        .map_err(CommandError::from)
}

pub struct ExecPromptCommand {
    flags: ExecPromptCommandFlags,
    engines: Engines,
}

impl ExecPromptCommand {
    pub fn new(flags: ExecPromptCommandFlags, engines: Engines) -> Self {
        Self { flags, engines }
    }

    pub fn flags(&self) -> &ExecPromptCommandFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for ExecPromptCommand {
    type Frontend = Box<dyn ExecPromptCommandFrontend>;
    type Outcome = ExecPromptOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let session = open_session_for_cwd(&self.engines)?;
        let agent = resolve_agent(&self.flags.agent, &session)?;

        let cli_overlays = self
            .flags
            .overlay
            .iter()
            .map(|s| {
                parse_overlay_spec(s).map_err(|reason| CommandError::InvalidOverlaySpec {
                    spec: s.clone(),
                    reason,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let directory_overlays = collect_all_overlay_specs(&session, cli_overlays);

        // Ensure the agent is available (downloads + builds when missing).
        ensure_exec_prompt_agent_setup(
            self.engines.agent_engine.as_ref(),
            &session,
            &agent,
            &mut frontend,
        )
        .await?;

        // Resolve agent credentials so the running container can reach its
        // backend.
        let credentials = self
            .engines
            .auth_engine
            .resolve_agent_auth(&session, &agent)
            .map_err(CommandError::from)?;

        let run_opts = AgentRunOptions {
            yolo: self.flags.yolo.then_some(YoloMode::Enabled),
            auto: self.flags.auto.then_some(AutoMode::Enabled),
            plan: self.flags.plan.then_some(PlanMode::Enabled),
            allow_docker: self.flags.allow_docker,
            mount_ssh: self.flags.mount_ssh,
            // Force non-interactive: this is a one-shot prompt injection.
            non_interactive: true,
            model: self.flags.model.clone(),
            initial_prompt: Some(self.flags.prompt.clone()),
            env_passthrough: Some(session.effective_config().env_passthrough()),
            directory_overlays,
            ..Default::default()
        };

        let mut options = self
            .engines
            .agent_engine
            .build_options(&session, &agent, &run_opts)?;
        if !credentials.env_vars.is_empty() {
            options.push(crate::engine::container::options::ContainerOption::AgentCredentials {
                env_vars: credentials.env_vars,
            });
        }

        let instance = self.engines.runtime.build(options)?;
        frontend.set_pty_active(true);
        let container_frontend = frontend.container_frontend();
        let mut execution = match instance.run_with_frontend(container_frontend) {
            Ok(e) => e,
            Err(e) => {
                frontend.set_pty_active(false);
                frontend.replay_queued();
                return Err(CommandError::from(e));
            }
        };
        let exit = execution.wait().await;
        frontend.set_pty_active(false);
        frontend.replay_queued();

        let exit_code = exit.map(|e| e.exit_code).ok();
        Ok(ExecPromptOutcome {
            agent: Some(agent.as_str().to_string()),
            exit_code,
        })
    }
}
