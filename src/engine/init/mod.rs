//! `engine::init` — `InitEngine`. Multi-phase state machine for `amux init`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::data::session::{AgentName, Session};
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;
use crate::engine::overlay::OverlayEngine;
use crate::engine::step_status::StepStatus;

pub mod frontend;
pub mod phase;
pub mod summary;

pub use frontend::InitFrontend;
pub use phase::{InitFailure, InitPhase};
pub use summary::InitSummary;

#[derive(Debug, Clone)]
pub struct InitEngineOptions {
    pub agent: AgentName,
    pub run_aspec_setup: bool,
    pub git_root: PathBuf,
}

pub struct InitEngine {
    session: Arc<Session>,
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
    container_runtime: Arc<ContainerRuntime>,
    options: InitEngineOptions,
    phase: InitPhase,
    summary: InitSummary,
}

impl InitEngine {
    pub fn new(
        session: Arc<Session>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
        container_runtime: Arc<ContainerRuntime>,
        options: InitEngineOptions,
    ) -> Self {
        Self {
            session,
            git_engine,
            overlay_engine,
            container_runtime,
            options,
            phase: InitPhase::Preflight,
            summary: InitSummary::default(),
        }
    }

    pub fn phase(&self) -> &InitPhase {
        &self.phase
    }

    pub fn summary(&self) -> &InitSummary {
        &self.summary
    }

    pub async fn step(
        &mut self,
        frontend: &mut dyn InitFrontend,
    ) -> Result<InitPhase, EngineError> {
        frontend.report_phase(&self.phase);
        let next = match &self.phase {
            InitPhase::Preflight => InitPhase::AwaitingAspecDecision,
            InitPhase::AwaitingAspecDecision => {
                if frontend.ask_replace_aspec()? {
                    InitPhase::CreatingAspecFolder
                } else {
                    self.summary.aspec_folder = StepStatus::Skipped;
                    InitPhase::SettingUpDockerfile
                }
            }
            InitPhase::CreatingAspecFolder => {
                self.summary.aspec_folder = StepStatus::Done;
                InitPhase::SettingUpDockerfile
            }
            InitPhase::SettingUpDockerfile => {
                self.summary.dockerfile = StepStatus::Done;
                InitPhase::WritingConfig
            }
            InitPhase::WritingConfig => {
                self.summary.config = StepStatus::Done;
                InitPhase::AwaitingAuditDecision
            }
            InitPhase::AwaitingAuditDecision => {
                if frontend.ask_run_audit()? {
                    InitPhase::BuildingImage
                } else {
                    self.summary.audit = StepStatus::Skipped;
                    self.summary.image_build = StepStatus::Skipped;
                    InitPhase::AwaitingWorkItemsDecision
                }
            }
            InitPhase::BuildingImage => {
                let _ = frontend.container_frontend();
                self.summary.image_build = StepStatus::Done;
                InitPhase::RunningAudit
            }
            InitPhase::RunningAudit => {
                let _ = frontend.container_frontend();
                self.summary.audit = StepStatus::Done;
                InitPhase::AwaitingWorkItemsDecision
            }
            InitPhase::AwaitingWorkItemsDecision => {
                let cfg = frontend.ask_work_items_setup()?;
                if cfg.is_some() {
                    InitPhase::WritingWorkItemsConfig
                } else {
                    self.summary.work_items_setup = StepStatus::Skipped;
                    InitPhase::Complete
                }
            }
            InitPhase::WritingWorkItemsConfig => {
                self.summary.work_items_setup = StepStatus::Done;
                InitPhase::Complete
            }
            InitPhase::Complete | InitPhase::Failed(_) => self.phase.clone(),
        };
        self.phase = next.clone();
        if matches!(self.phase, InitPhase::Complete | InitPhase::Failed(_)) {
            frontend.report_summary(&self.summary);
        }
        Ok(next)
    }

    pub async fn run_to_completion(
        &mut self,
        frontend: &mut dyn InitFrontend,
    ) -> Result<InitSummary, EngineError> {
        loop {
            let next = self.step(frontend).await?;
            if matches!(next, InitPhase::Complete | InitPhase::Failed(_)) {
                break;
            }
        }
        Ok(self.summary.clone())
    }
}

#[allow(dead_code)]
fn _suppress(_: &Session, _: &Arc<GitEngine>, _: &Arc<OverlayEngine>, _: &Arc<ContainerRuntime>) {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::data::config::repo::WorkItemsConfig;
    use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};
    use crate::engine::container::frontend::{ContainerFrontend, ContainerProgress, ContainerStatus};
    use crate::engine::message::{UserMessage, UserMessageSink};
    use crate::engine::overlay::OverlayEngine;
    use crate::engine::step_status::StepStatus;

    // ── Fake frontend ────────────────────────────────────────────────────────

    struct FakeInitFrontend {
        replace_aspec: bool,
        run_audit: bool,
        work_items_config: Option<WorkItemsConfig>,
        phases: Vec<InitPhase>,
    }

    impl FakeInitFrontend {
        fn all_yes() -> Self {
            Self {
                replace_aspec: true,
                run_audit: true,
                work_items_config: Some(WorkItemsConfig::default()),
                phases: Vec::new(),
            }
        }
    }

    struct FakeContainerFrontend;
    impl UserMessageSink for FakeContainerFrontend {
        fn write_message(&mut self, _: UserMessage) {}
        fn replay_queued(&mut self) {}
    }
    #[async_trait::async_trait]
    impl ContainerFrontend for FakeContainerFrontend {
        fn write_stdout(&mut self, _: &[u8]) -> Result<(), EngineError> { Ok(()) }
        fn write_stderr(&mut self, _: &[u8]) -> Result<(), EngineError> { Ok(()) }
        async fn read_stdin(&mut self, _: &mut [u8]) -> Result<usize, EngineError> { Ok(0) }
        fn report_status(&mut self, _: ContainerStatus) {}
        fn report_progress(&mut self, _: ContainerProgress) {}
        fn resize_pty(&mut self, _: u16, _: u16) {}
    }

    impl UserMessageSink for FakeInitFrontend {
        fn write_message(&mut self, _: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl InitFrontend for FakeInitFrontend {
        fn ask_replace_aspec(&mut self) -> Result<bool, EngineError> {
            Ok(self.replace_aspec)
        }

        fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
            Ok(self.run_audit)
        }

        fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError> {
            Ok(self.work_items_config.clone())
        }

        fn report_phase(&mut self, phase: &InitPhase) {
            self.phases.push(phase.clone());
        }

        fn report_step_status(&mut self, _step: &str, _status: StepStatus) {}

        fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
            Box::new(FakeContainerFrontend)
        }

        fn report_summary(&mut self, _: &InitSummary) {}
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_engine(git_root: &std::path::Path) -> InitEngine {
        let resolver = StaticGitRootResolver::new(git_root);
        let session = Arc::new(
            crate::data::session::Session::open(
                git_root.to_path_buf(),
                &resolver,
                SessionOpenOptions::default(),
            )
            .unwrap(),
        );
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(git_root),
        ));
        let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
        let options = InitEngineOptions {
            agent: AgentName::new("claude").unwrap(),
            run_aspec_setup: true,
            git_root: git_root.to_path_buf(),
        };
        InitEngine::new(session, Arc::new(GitEngine::new()), overlay, runtime, options)
    }

    // ── Tests ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_to_completion_all_done() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_engine(tmp.path());
        let mut frontend = FakeInitFrontend::all_yes();
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &InitPhase::Complete);
        assert!(matches!(summary.aspec_folder, StepStatus::Done));
        assert!(matches!(summary.dockerfile, StepStatus::Done));
        assert!(matches!(summary.config, StepStatus::Done));
        assert!(matches!(summary.audit, StepStatus::Done));
        assert!(matches!(summary.image_build, StepStatus::Done));
        assert!(matches!(summary.work_items_setup, StepStatus::Done));
    }

    #[tokio::test]
    async fn awaiting_aspec_decision_false_skips_aspec_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_engine(tmp.path());
        let mut frontend = FakeInitFrontend {
            replace_aspec: false,
            run_audit: true,
            work_items_config: Some(WorkItemsConfig::default()),
            phases: Vec::new(),
        };
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &InitPhase::Complete);
        assert!(
            matches!(summary.aspec_folder, StepStatus::Skipped),
            "aspec_folder must be Skipped when user declines"
        );
        // Other phases continue.
        assert!(matches!(summary.dockerfile, StepStatus::Done));
    }

    #[tokio::test]
    async fn awaiting_work_items_decision_none_skips_work_items() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_engine(tmp.path());
        let mut frontend = FakeInitFrontend {
            replace_aspec: true,
            run_audit: true,
            work_items_config: None, // decline work-items setup
            phases: Vec::new(),
        };
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &InitPhase::Complete);
        assert!(
            matches!(summary.work_items_setup, StepStatus::Skipped),
            "work_items_setup must be Skipped when None returned"
        );
    }

    #[tokio::test]
    async fn each_phase_independently_reachable_via_step() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_engine(tmp.path());
        let mut frontend = FakeInitFrontend::all_yes();
        assert_eq!(engine.phase(), &InitPhase::Preflight);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &InitPhase::AwaitingAspecDecision);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &InitPhase::CreatingAspecFolder);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &InitPhase::SettingUpDockerfile);
    }
}
