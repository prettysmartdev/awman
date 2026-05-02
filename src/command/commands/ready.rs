//! `ReadyCommand` — thin wrapper over `ReadyEngine`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::session::AgentName;
use crate::engine::ready::{ReadyEngine, ReadyEngineOptions, ReadyFrontend, ReadySummary};
use crate::engine::step_status::StepStatus;

#[derive(Debug, Clone)]
pub struct ReadyCommandFlags {
    pub refresh: bool,
    pub build: bool,
    pub no_cache: bool,
    pub non_interactive: bool,
    pub allow_docker: bool,
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadyOutcome {
    pub runtime: String,
    pub base_image: StepStatus,
    pub agent_image: StepStatus,
    pub local_agent: StepStatus,
    pub audit: StepStatus,
    pub legacy_migration: StepStatus,
}

impl From<ReadySummary> for ReadyOutcome {
    fn from(s: ReadySummary) -> Self {
        Self {
            runtime: s.runtime_name,
            base_image: s.base_image,
            agent_image: s.agent_image,
            local_agent: s.local_agent,
            audit: s.audit,
            legacy_migration: s.legacy_migration,
        }
    }
}

pub trait ReadyCommandFrontend: ReadyFrontend + Send {}
impl<T: ReadyFrontend + Send> ReadyCommandFrontend for T {}

pub struct ReadyCommand {
    flags: ReadyCommandFlags,
    engines: Engines,
}

impl ReadyCommand {
    pub fn new(flags: ReadyCommandFlags, engines: Engines) -> Self {
        Self { flags, engines }
    }

    pub fn flags(&self) -> &ReadyCommandFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for ReadyCommand {
    type Frontend = Box<dyn ReadyCommandFrontend>;
    type Outcome = ReadyOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let agent = AgentName::new("claude").map_err(CommandError::from)?;
        let session = open_session()?;
        let options = ReadyEngineOptions {
            agent,
            refresh: self.flags.refresh,
            build: self.flags.build,
            no_cache: self.flags.no_cache,
            allow_docker: self.flags.allow_docker,
        };
        let mut engine = ReadyEngine::new(
            std::sync::Arc::new(session),
            self.engines.git_engine.clone(),
            self.engines.overlay_engine.clone(),
            self.engines.runtime.clone(),
            self.engines.agent_engine.clone(),
            options,
        );
        let summary = engine
            .run_to_completion(frontend.as_mut())
            .await
            .map_err(CommandError::from)?;
        frontend.replay_queued();
        Ok(summary.into())
    }
}

fn open_session() -> Result<crate::data::session::Session, CommandError> {
    let cwd = std::env::current_dir()
        .map_err(|e| CommandError::Other(format!("cwd unavailable: {e}")))?;
    let resolver = crate::data::session::StaticGitRootResolver::new(cwd.clone());
    crate::data::session::Session::open(
        cwd,
        &resolver,
        crate::data::session::SessionOpenOptions::default(),
    )
    .map_err(CommandError::from)
}
