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
    pub dockerfile: StepStatus,
    pub base_image: StepStatus,
    pub agent_image: StepStatus,
    pub local_agent: StepStatus,
    pub audit: StepStatus,
    pub image_rebuild: StepStatus,
    pub legacy_migration: StepStatus,
    /// `true` when `--json` was passed; controls how the CLI renders the outcome.
    #[serde(skip)]
    pub json_requested: bool,
    /// `true` when `--refresh` was passed; carried into legacy JSON output.
    #[serde(skip)]
    pub refresh_requested: bool,
}

impl From<ReadySummary> for ReadyOutcome {
    fn from(s: ReadySummary) -> Self {
        Self {
            runtime: s.runtime_name,
            dockerfile: s.dockerfile,
            base_image: s.base_image,
            agent_image: s.agent_image,
            local_agent: s.local_agent,
            audit: s.audit,
            image_rebuild: s.image_rebuild,
            legacy_migration: s.legacy_migration,
            json_requested: false,
            refresh_requested: false,
        }
    }
}

impl ReadyOutcome {
    /// Render the outcome in the legacy `amux ready --json` schema:
    ///
    /// ```json
    /// { "ready": <bool>,
    ///   "steps": {
    ///     "docker_daemon": {"status": "ok|skipped|failed|pending", "message": "..."},
    ///     "dockerfile": {...}, "aspec_folder": {...}, "work_items_config": {...},
    ///     "local_agent": {...}, "dev_image": {...}, "refresh": {...},
    ///     "image_rebuild": {...}
    ///   }
    /// }
    /// ```
    pub fn to_legacy_json(&self) -> serde_json::Value {
        fn step_to_json(s: &StepStatus) -> serde_json::Value {
            match s {
                StepStatus::Pending => serde_json::json!({"status": "pending", "message": ""}),
                StepStatus::Running => serde_json::json!({"status": "running", "message": ""}),
                StepStatus::Done => serde_json::json!({"status": "ok", "message": ""}),
                StepStatus::Skipped => serde_json::json!({"status": "skipped", "message": ""}),
                StepStatus::Warn(msg) => serde_json::json!({"status": "warn", "message": msg}),
                StepStatus::Failed(msg) => {
                    serde_json::json!({"status": "failed", "message": msg})
                }
            }
        }

        let any_failed = matches!(self.dockerfile, StepStatus::Failed(_))
            || matches!(self.base_image, StepStatus::Failed(_))
            || matches!(self.agent_image, StepStatus::Failed(_))
            || matches!(self.local_agent, StepStatus::Failed(_))
            || matches!(self.image_rebuild, StepStatus::Failed(_));

        // `docker_daemon` isn't tracked as a separate step in the new engine;
        // if we made it this far, the daemon was reachable.
        let docker_daemon = StepStatus::Done;

        // `aspec_folder` and `work_items_config` are owned by `init`, not by
        // `ready`. Report them as Skipped so consumers see a complete schema.
        let aspec_folder = StepStatus::Skipped;
        let work_items_config = StepStatus::Skipped;

        // `refresh` is derived from the flag — Done if user asked for it,
        // Skipped otherwise.
        let refresh = if self.refresh_requested {
            StepStatus::Done
        } else {
            StepStatus::Skipped
        };

        serde_json::json!({
            "ready": !any_failed,
            "runtime": self.runtime,
            "steps": {
                "docker_daemon": step_to_json(&docker_daemon),
                "dockerfile": step_to_json(&self.dockerfile),
                "aspec_folder": step_to_json(&aspec_folder),
                "work_items_config": step_to_json(&work_items_config),
                "local_agent": step_to_json(&self.local_agent),
                "dev_image": step_to_json(&self.agent_image),
                "refresh": step_to_json(&refresh),
                "image_rebuild": step_to_json(&self.image_rebuild),
            }
        })
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
            env_passthrough: None,
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
        let mut outcome: ReadyOutcome = summary.into();
        outcome.json_requested = self.flags.json;
        outcome.refresh_requested = self.flags.refresh;
        Ok(outcome)
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
