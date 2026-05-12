//! `InitCommand` — thin wrapper over `InitEngine`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::session::AgentName;
use crate::engine::init::{InitEngine, InitEngineOptions, InitFrontend, InitSummary};
use crate::engine::message::{MessageLevel, UserMessage};
use crate::engine::step_status::StepStatus;

#[derive(Debug, Clone)]
pub struct InitCommandFlags {
    pub agent: String,
    pub aspec: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct InitOutcome {
    pub agent: String,
    pub aspec_requested: bool,
    pub summary: SerializableInitSummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct SerializableInitSummary {
    pub aspec_folder: StepStatus,
    pub dockerfile: StepStatus,
    pub config: StepStatus,
    pub audit: StepStatus,
    pub image_build: StepStatus,
    pub work_items_setup: StepStatus,
}

impl From<InitSummary> for SerializableInitSummary {
    fn from(s: InitSummary) -> Self {
        Self {
            aspec_folder: s.aspec_folder,
            dockerfile: s.dockerfile,
            config: s.config,
            audit: s.audit,
            image_build: s.image_build,
            work_items_setup: s.work_items_setup,
        }
    }
}

pub trait InitCommandFrontend: InitFrontend + Send {}

impl<T: InitFrontend + Send> InitCommandFrontend for T {}

pub struct InitCommand {
    flags: InitCommandFlags,
    engines: Engines,
    session: crate::data::session::Session,
}

impl InitCommand {
    pub fn new(
        flags: InitCommandFlags,
        engines: Engines,
        session: crate::data::session::Session,
    ) -> Self {
        Self {
            flags,
            engines,
            session,
        }
    }

    pub fn flags(&self) -> &InitCommandFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for InitCommand {
    type Frontend = Box<dyn InitCommandFrontend>;
    type Outcome = InitOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "init: initializing amux for this repository".into(),
        });
        let agent_name = match AgentName::new(self.flags.agent.clone()) {
            Ok(n) => n,
            Err(e) => {
                let cmd_err = CommandError::from(e);
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("init: invalid agent name: {cmd_err}"),
                });
                return Err(cmd_err);
            }
        };
        let session = self.session;
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("init: resolved git root at {:?}", session.git_root()),
        });
        let options = InitEngineOptions {
            agent: agent_name,
            run_aspec_setup: self.flags.aspec,
            git_root: session.git_root().to_path_buf(),
        };
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("init: configuring agent '{}'", &self.flags.agent),
        });
        let mut engine = InitEngine::new(
            std::sync::Arc::new(session),
            self.engines.git_engine.clone(),
            self.engines.overlay_engine.clone(),
            self.engines.runtime.clone(),
            self.engines.agent_engine.clone(),
            options,
        );
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "init: running initialization steps (directories, config, image build)".into(),
        });
        let summary = match engine.run_to_completion(frontend.as_mut()).await {
            Ok(s) => s,
            Err(e) => {
                let cmd_err = CommandError::from(e);
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("init: engine run_to_completion failed: {cmd_err}"),
                });
                return Err(cmd_err);
            }
        };
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "init: configuration written successfully".into(),
        });
        frontend.replay_queued();
        Ok(InitOutcome {
            agent: self.flags.agent,
            aspec_requested: self.flags.aspec,
            summary: summary.into(),
        })
    }
}
