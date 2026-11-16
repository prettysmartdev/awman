//! `InitCommand` — thin wrapper over `InitEngine`.
//!
//! Layer 2 owns `init`'s headless policy for API invocations: a daemon
//! frontend (the worker feature's push routes and local sessions) gets its
//! questions answered from [`HeadlessDefaults::api`] here, not by whatever its
//! Layer 3 frontend would answer. See [`InitCommand::headless`].

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::catalogue::FrontendKind;
use crate::command::dispatch::{BuildContext, Engines};
use crate::command::error::CommandError;
use crate::command::headless::HeadlessDefaults;
use crate::data::config::work_items::WorkItemsConfig;
use crate::data::init_files::InitFile;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::prompt::Prompt;
use crate::data::session::AgentName;
use crate::data::setup_step::SetupStep;
use crate::data::step_status::StepStatus;
use crate::engine::agent::registry::AgentRegistry;
use crate::engine::agent::AgentImageFrontend;
use crate::engine::error::EngineError;
use crate::engine::init::frontend::{DockerfileSetupChoice, DockerfileSetupDecision};
use crate::engine::init::{InitEngine, InitEngineOptions, InitFrontend, InitPhase, InitSummary};

#[derive(Debug, Clone)]
pub struct InitCommandFlags {
    pub agent: String,
    pub aspec: bool,
}

/// The `init` command result.
#[derive(Debug, Clone, Serialize)]
pub struct InitOutcome {
    pub agent: String,
    pub aspec_requested: bool,
    pub summary: SerializableInitSummary,
    /// Each project file init created, updated or kept — and the project
    /// Dockerfile if it was skipped, with why — relative to the git root:
    /// the project Dockerfile (`Dockerfile.dev` or the configured
    /// `runtime.dockerfile`), `.awman/config.json` and
    /// `.awman/Dockerfile.<agent>`. A client commits the `created`/`updated`
    /// ones without diffing the tree.
    pub files: Vec<InitFile>,
}

impl InitOutcome {
    /// The project Dockerfile's entry in [`Self::files`]. `summary.dockerfile`
    /// is `Done` when this is `Created` or `Kept` and `Skipped` when it is
    /// `Skipped`.
    pub fn project_dockerfile(&self) -> Option<&InitFile> {
        self.files
            .iter()
            .find(|f| f.kind == crate::data::init_files::InitFileKind::ProjectDockerfile)
    }
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

impl From<&InitSummary> for SerializableInitSummary {
    fn from(s: &InitSummary) -> Self {
        Self {
            aspec_folder: s.aspec_folder.clone(),
            dockerfile: s.dockerfile.clone(),
            config: s.config.clone(),
            audit: s.audit.clone(),
            image_build: s.image_build.clone(),
            work_items_setup: s.work_items_setup.clone(),
        }
    }
}

impl From<InitSummary> for SerializableInitSummary {
    fn from(s: InitSummary) -> Self {
        Self::from(&s)
    }
}

pub trait InitCommandFrontend: InitFrontend + Send {}

impl<T: InitFrontend + Send> InitCommandFrontend for T {}

pub struct InitCommand {
    flags: InitCommandFlags,
    engines: Engines,
    session: crate::data::session::Session,
    /// Set for API invocations: the answers init's questions get, whatever
    /// the frontend would have said. `None` lets the frontend ask (a TTY) or
    /// apply its own headless profile (a piped CLI).
    headless: Option<HeadlessDefaults>,
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
            headless: None,
        }
    }

    /// Answer every init question from `defaults` instead of the frontend.
    pub fn with_headless(mut self, defaults: HeadlessDefaults) -> Self {
        self.headless = Some(defaults);
        self
    }

    /// Construct from the catalogue-resolved input (WI 0113 F-10). `--agent`
    /// carries `FlagDefault::Str("claude")`, so no default is restated here.
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        let command = Self::new(
            InitCommandFlags {
                agent: ctx.flags.require_str("agent")?,
                aspec: ctx.flags.bool("aspec"),
            },
            ctx.engines.clone(),
            ctx.session.clone(),
        );
        Ok(match Self::headless_for(ctx.caller.frontend()) {
            Some(defaults) => command.with_headless(defaults),
            None => command,
        })
    }

    /// The headless profile init answers from for `kind`, if any.
    ///
    /// A daemon frontend serving an API client (the worker feature's push
    /// routes, a local session) has nobody to ask, so init answers from
    /// [`HeadlessDefaults::api`]: keep an existing `aspec/` and project
    /// Dockerfile, create the project Dockerfile from the default template
    /// when it is missing, skip the audit (and so the image build), no
    /// work-items setup. The CLI and TUI — and an engine instance serving
    /// one — keep asking through their frontend, which applies
    /// [`HeadlessDefaults::cli`] when stdin is not a terminal.
    pub fn headless_for(kind: FrontendKind) -> Option<HeadlessDefaults> {
        match kind {
            FrontendKind::Daemon { .. } if !kind.is_engine_local() => Some(HeadlessDefaults::api()),
            _ => None,
        }
    }

    pub fn flags(&self) -> &InitCommandFlags {
        &self.flags
    }

    /// The profile this invocation answers from, if it does not ask.
    pub fn headless(&self) -> Option<&HeadlessDefaults> {
        self.headless.as_ref()
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
        match self.headless.clone() {
            Some(defaults) => {
                let mut answering = HeadlessInitFrontend {
                    inner: frontend.as_mut(),
                    defaults,
                };
                self.run(&mut answering).await
            }
            None => self.run(frontend.as_mut()).await,
        }
    }
}

impl InitCommand {
    async fn run(self, frontend: &mut dyn InitFrontend) -> Result<InitOutcome, CommandError> {
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "init: initializing awman for this repository".into(),
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
        // `--agent` is free-form: any first-party name or a custom definition
        // discovered for this repo. Anything else is the same `unknown agent`
        // error a launch would give, before init writes anything.
        let root = Some(session.definition_root());
        if !AgentRegistry::list(session.env(), root).contains(agent_name.as_str()) {
            let cmd_err = CommandError::Engine(AgentRegistry::unknown(
                session.env(),
                agent_name.as_str(),
                root,
                false,
            ));
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("init: {cmd_err}"),
            });
            return Err(cmd_err);
        }
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("init: resolved git root at {:?}", session.git_root()),
        });
        let options = InitEngineOptions {
            agent: agent_name,
            run_aspec_setup: self.flags.aspec,
            git_root: session.git_root().to_path_buf(),
            // Prompt copy is Layer 2's and reaches Layer 1 through the options
            // struct, because an engine cannot name a Layer 2 type (F-19).
            dockerfile_setup_prompt: crate::command::prompts::dockerfile_setup(),
        };
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("init: configuring agent '{}'", &self.flags.agent),
        });
        let mut engine = InitEngine::new(
            std::sync::Arc::new(session),
            self.engines.git_engine.clone(),
            self.engines.overlay_engine.clone(),
            self.engines
                .require_container_runtime()
                .map_err(CommandError::from)?
                .clone(),
            self.engines.agent_engine.clone(),
            options,
        );
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "init: running initialization steps (directories, config, image build)".into(),
        });
        let summary = match engine.run_to_completion(frontend).await {
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
            summary: SerializableInitSummary::from(&summary),
            files: summary.files,
        })
    }
}

/// Answers init's questions from a [`HeadlessDefaults`] profile and forwards
/// everything else — messages, phases, image-build step status (the
/// command's event stream) and the summary — to the frontend it wraps.
struct HeadlessInitFrontend<'a> {
    inner: &'a mut dyn InitFrontend,
    defaults: HeadlessDefaults,
}

impl UserMessageSink for HeadlessInitFrontend<'_> {
    fn write_message(&mut self, msg: UserMessage) {
        self.inner.write_message(msg);
    }
    fn replay_queued(&mut self) {
        self.inner.replay_queued();
    }
}

impl AgentImageFrontend for HeadlessInitFrontend<'_> {
    fn report_step_status(&mut self, step: &SetupStep, status: StepStatus) {
        self.inner.report_step_status(step, status);
    }
    fn container_frontend(
        &mut self,
    ) -> Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend> {
        self.inner.container_frontend()
    }
}

impl InitFrontend for HeadlessInitFrontend<'_> {
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError> {
        Ok(self.defaults.replace_aspec())
    }
    fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
        Ok(self.defaults.run_audit())
    }
    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError> {
        Ok(self.defaults.work_items_setup())
    }
    fn ask_dockerfile_setup(
        &mut self,
        _prompt: &Prompt<DockerfileSetupChoice>,
        _git_root: &std::path::Path,
        _dockerfile_path: &str,
    ) -> Result<DockerfileSetupDecision, EngineError> {
        Ok(self.defaults.dockerfile_setup())
    }
    fn report_phase(&mut self, phase: &InitPhase) {
        self.inner.report_phase(phase);
    }
    fn report_summary(&mut self, summary: &InitSummary) {
        self.inner.report_summary(summary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_invocations_answer_from_the_api_profile_and_local_ones_ask() {
        assert!(InitCommand::headless_for(FrontendKind::WORKER_LOCAL).is_some());
        let worker_remote = FrontendKind::Daemon {
            feature: crate::data::daemon_state::FeatureName::Worker,
            listener: crate::data::daemon_state::CommandListener::Network,
        };
        assert!(InitCommand::headless_for(worker_remote).is_some());
        for local in [
            FrontendKind::Cli,
            FrontendKind::Tui,
            FrontendKind::RELAYED_CLI,
            FrontendKind::RELAYED_TUI,
            FrontendKind::ENGINE_LOCAL,
        ] {
            assert!(InitCommand::headless_for(local).is_none(), "{local:?}");
        }
    }

    /// A frontend that must never be asked: every question panics, every
    /// report is recorded.
    #[derive(Default)]
    struct NeverAsk {
        steps: Vec<String>,
        phases: usize,
        messages: usize,
    }
    impl UserMessageSink for NeverAsk {
        fn write_message(&mut self, _: UserMessage) {
            self.messages += 1;
        }
        fn replay_queued(&mut self) {}
    }
    impl AgentImageFrontend for NeverAsk {
        fn report_step_status(&mut self, step: &SetupStep, status: StepStatus) {
            self.steps.push(format!("{step}: {status:?}"));
        }
        fn container_frontend(
            &mut self,
        ) -> Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend> {
            unreachable!()
        }
    }
    impl InitFrontend for NeverAsk {
        fn ask_replace_aspec(&mut self) -> Result<bool, EngineError> {
            panic!("asked")
        }
        fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
            panic!("asked")
        }
        fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError> {
            panic!("asked")
        }
        fn ask_dockerfile_setup(
            &mut self,
            _: &Prompt<DockerfileSetupChoice>,
            _: &std::path::Path,
            _: &str,
        ) -> Result<DockerfileSetupDecision, EngineError> {
            panic!("asked")
        }
        fn report_phase(&mut self, _: &InitPhase) {
            self.phases += 1;
        }
        fn report_summary(&mut self, _: &InitSummary) {}
    }

    #[test]
    fn headless_wrapper_answers_without_asking_and_forwards_reports() {
        let mut inner = NeverAsk::default();
        let mut wrapper = HeadlessInitFrontend {
            inner: &mut inner,
            defaults: HeadlessDefaults::api(),
        };
        let prompt = crate::command::prompts::dockerfile_setup();
        assert_eq!(
            wrapper
                .ask_dockerfile_setup(&prompt, std::path::Path::new("/repo"), "Dockerfile.dev")
                .unwrap(),
            DockerfileSetupDecision::CreateNew
        );
        assert!(!wrapper.ask_replace_aspec().unwrap());
        assert!(!wrapper.ask_run_audit().unwrap());
        assert!(wrapper.ask_work_items_setup().unwrap().is_none());
        // Image-build progress reaches the wrapped frontend's event stream.
        wrapper.report_step_status(
            &crate::data::setup_step::InitStep::BuildBaseImage.into(),
            StepStatus::Running,
        );
        wrapper.report_phase(&InitPhase::Preflight);
        wrapper.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "x".into(),
        });
        assert_eq!(inner.steps, ["Build base image: Running"]);
        assert_eq!((inner.phases, inner.messages), (1, 1));
    }
}
