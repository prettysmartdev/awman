//! `engine::ready` — `ReadyEngine`. Multi-phase state machine for `amux ready`.

use std::sync::Arc;

use crate::data::session::{AgentName, Session};
use crate::engine::agent::AgentEngine;
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;
use crate::engine::overlay::OverlayEngine;
use crate::engine::step_status::StepStatus;

pub mod frontend;
pub mod phase;
pub mod summary;

pub use frontend::ReadyFrontend;
pub use phase::{ReadyFailure, ReadyPhase};
pub use summary::ReadySummary;

#[derive(Debug, Clone)]
pub struct ReadyEngineOptions {
    pub agent: AgentName,
    pub refresh: bool,
    pub build: bool,
    pub no_cache: bool,
    pub allow_docker: bool,
}

pub struct ReadyEngine {
    session: Arc<Session>,
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
    container_runtime: Arc<ContainerRuntime>,
    agent_engine: Arc<AgentEngine>,
    options: ReadyEngineOptions,
    phase: ReadyPhase,
    summary: ReadySummary,
}

impl ReadyEngine {
    pub fn new(
        session: Arc<Session>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
        container_runtime: Arc<ContainerRuntime>,
        agent_engine: Arc<AgentEngine>,
        options: ReadyEngineOptions,
    ) -> Self {
        let runtime_name = container_runtime.runtime_name().to_string();
        Self {
            session,
            git_engine,
            overlay_engine,
            container_runtime,
            agent_engine,
            options,
            phase: ReadyPhase::Preflight,
            summary: ReadySummary::new(runtime_name),
        }
    }

    pub fn phase(&self) -> &ReadyPhase {
        &self.phase
    }

    pub fn summary(&self) -> ReadySummary {
        self.summary.clone()
    }

    /// Advance one phase. Drives Q&A and progress through `frontend`.
    pub async fn step(
        &mut self,
        frontend: &mut dyn ReadyFrontend,
    ) -> Result<ReadyPhase, EngineError> {
        frontend.report_phase(&self.phase);
        let next = match &self.phase {
            ReadyPhase::Preflight => ReadyPhase::AwaitingDockerfileDecision,
            ReadyPhase::AwaitingDockerfileDecision => {
                if frontend.ask_create_dockerfile()? {
                    ReadyPhase::CreatingDockerfile
                } else {
                    ReadyPhase::Failed(ReadyFailure {
                        phase: "AwaitingDockerfileDecision".into(),
                        message: "user declined to create Dockerfile.dev".into(),
                    })
                }
            }
            ReadyPhase::CreatingDockerfile => {
                frontend.report_step_status("Create Dockerfile.dev", StepStatus::Done);
                ReadyPhase::AwaitingLegacyMigrationDecision
            }
            ReadyPhase::AwaitingLegacyMigrationDecision => {
                let _ = frontend.ask_migrate_legacy_layout(&self.options.agent)?;
                self.summary.legacy_migration = StepStatus::Skipped;
                ReadyPhase::MigratingLegacyLayout
            }
            ReadyPhase::MigratingLegacyLayout => ReadyPhase::BuildingBaseImage,
            ReadyPhase::BuildingBaseImage => {
                frontend.report_step_status("Build base image", StepStatus::Running);
                let _ = frontend.container_frontend();
                self.summary.base_image = StepStatus::Done;
                frontend.report_step_status("Build base image", StepStatus::Done);
                ReadyPhase::BuildingAgentImage
            }
            ReadyPhase::BuildingAgentImage => {
                frontend.report_step_status("Build agent image", StepStatus::Running);
                let _ = frontend.container_frontend();
                self.summary.agent_image = StepStatus::Done;
                frontend.report_step_status("Build agent image", StepStatus::Done);
                ReadyPhase::CheckingLocalAgent
            }
            ReadyPhase::CheckingLocalAgent => {
                self.summary.local_agent = StepStatus::Done;
                ReadyPhase::RunningAudit
            }
            ReadyPhase::RunningAudit => {
                if frontend.ask_run_audit_on_template()? {
                    let _ = frontend.container_frontend();
                    self.summary.audit = StepStatus::Done;
                } else {
                    self.summary.audit = StepStatus::Skipped;
                }
                ReadyPhase::RebuildingAfterAudit
            }
            ReadyPhase::RebuildingAfterAudit => ReadyPhase::Complete,
            ReadyPhase::Complete | ReadyPhase::Failed(_) => self.phase.clone(),
        };
        self.phase = next.clone();
        if matches!(self.phase, ReadyPhase::Complete | ReadyPhase::Failed(_)) {
            frontend.report_summary(&self.summary);
        }
        Ok(next)
    }

    /// Drive to completion: advance phases in a loop until terminal.
    pub async fn run_to_completion(
        &mut self,
        frontend: &mut dyn ReadyFrontend,
    ) -> Result<ReadySummary, EngineError> {
        loop {
            let next = self.step(frontend).await?;
            if matches!(next, ReadyPhase::Complete | ReadyPhase::Failed(_)) {
                break;
            }
        }
        Ok(self.summary.clone())
    }
}

// Suppress unused warnings on engines we'll wire up in 0068.
#[allow(dead_code)]
fn _suppress(_: &Session, _: &Arc<GitEngine>, _: &Arc<OverlayEngine>, _: &Arc<ContainerRuntime>, _: &Arc<AgentEngine>) {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};
    use crate::engine::container::frontend::{ContainerFrontend, ContainerProgress, ContainerStatus};
    use crate::engine::error::EngineError;
    use crate::engine::message::{UserMessage, UserMessageSink};
    use crate::engine::overlay::OverlayEngine;
    use crate::engine::step_status::StepStatus;

    // ── Fake frontend ────────────────────────────────────────────────────────

    struct FakeReadyFrontend {
        create_dockerfile: bool,
        run_audit: bool,
        migrate_legacy: bool,
        phases: Vec<ReadyPhase>,
        statuses: Vec<(String, StepStatus)>,
    }

    impl FakeReadyFrontend {
        fn all_yes() -> Self {
            Self {
                create_dockerfile: true,
                run_audit: true,
                migrate_legacy: true,
                phases: Vec::new(),
                statuses: Vec::new(),
            }
        }
    }

    struct FakeContainerFrontend;

    impl UserMessageSink for FakeContainerFrontend {
        fn write_message(&mut self, _msg: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    #[async_trait::async_trait]
    impl ContainerFrontend for FakeContainerFrontend {
        fn write_stdout(&mut self, _bytes: &[u8]) -> Result<(), EngineError> { Ok(()) }
        fn write_stderr(&mut self, _bytes: &[u8]) -> Result<(), EngineError> { Ok(()) }
        async fn read_stdin(&mut self, _buf: &mut [u8]) -> Result<usize, EngineError> { Ok(0) }
        fn report_status(&mut self, _status: ContainerStatus) {}
        fn report_progress(&mut self, _progress: ContainerProgress) {}
        fn resize_pty(&mut self, _cols: u16, _rows: u16) {}
    }

    impl UserMessageSink for FakeReadyFrontend {
        fn write_message(&mut self, _msg: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl ReadyFrontend for FakeReadyFrontend {
        fn ask_create_dockerfile(&mut self) -> Result<bool, EngineError> {
            Ok(self.create_dockerfile)
        }

        fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
            Ok(self.run_audit)
        }

        fn ask_migrate_legacy_layout(
            &mut self,
            _agent: &AgentName,
        ) -> Result<bool, EngineError> {
            Ok(self.migrate_legacy)
        }

        fn report_phase(&mut self, phase: &ReadyPhase) {
            self.phases.push(phase.clone());
        }

        fn report_step_status(&mut self, step: &str, status: StepStatus) {
            self.statuses.push((step.to_string(), status));
        }

        fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
            Box::new(FakeContainerFrontend)
        }

        fn report_summary(&mut self, _summary: &ReadySummary) {}
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_engine_and_frontend(
        create_dockerfile: bool,
        run_audit: bool,
    ) -> (ReadyEngine, FakeReadyFrontend) {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = StaticGitRootResolver::new(tmp.path());
        let session = Arc::new(
            crate::data::session::Session::open(
                tmp.path().to_path_buf(),
                &resolver,
                SessionOpenOptions::default(),
            )
            .unwrap(),
        );
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(tmp.path()),
        ));
        let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let options = ReadyEngineOptions {
            agent: AgentName::new("claude").unwrap(),
            refresh: false,
            build: true,
            no_cache: false,
            allow_docker: false,
        };
        let engine = ReadyEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            runtime,
            agent_engine,
            options,
        );
        let frontend = FakeReadyFrontend {
            create_dockerfile,
            run_audit,
            migrate_legacy: true,
            phases: Vec::new(),
            statuses: Vec::new(),
        };
        (engine, frontend)
    }

    // ── Tests ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_to_completion_happy_path_all_done() {
        let (mut engine, mut frontend) = make_engine_and_frontend(true, true);
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ReadyPhase::Complete);
        assert!(matches!(summary.base_image, StepStatus::Done));
        assert!(matches!(summary.agent_image, StepStatus::Done));
        assert!(matches!(summary.local_agent, StepStatus::Done));
        assert!(matches!(summary.audit, StepStatus::Done));
    }

    #[tokio::test]
    async fn awaiting_dockerfile_decision_false_leads_to_failed_phase() {
        let (mut engine, mut frontend) = make_engine_and_frontend(false, true);
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert!(
            matches!(engine.phase(), ReadyPhase::Failed(_)),
            "expected Failed phase, got {:?}",
            engine.phase()
        );
        // Summary fields should still be Pending (nothing ran after abort).
        assert!(matches!(summary.base_image, StepStatus::Pending));
    }

    #[tokio::test]
    async fn awaiting_legacy_migration_false_sets_summary_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = StaticGitRootResolver::new(tmp.path());
        let session = Arc::new(
            crate::data::session::Session::open(
                tmp.path().to_path_buf(),
                &resolver,
                SessionOpenOptions::default(),
            )
            .unwrap(),
        );
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(tmp.path()),
        ));
        let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let options = ReadyEngineOptions {
            agent: AgentName::new("claude").unwrap(),
            refresh: false,
            build: true,
            no_cache: false,
            allow_docker: false,
        };
        let mut engine = ReadyEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            runtime,
            agent_engine,
            options,
        );
        let mut frontend = FakeReadyFrontend {
            create_dockerfile: true,
            run_audit: true,
            migrate_legacy: false, // decline migration
            phases: Vec::new(),
            statuses: Vec::new(),
        };
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        // Engine continues (doesn't abort) even when migration declined.
        assert_eq!(engine.phase(), &ReadyPhase::Complete);
        assert!(
            matches!(summary.legacy_migration, StepStatus::Skipped),
            "legacy_migration must be Skipped when declined"
        );
    }

    #[tokio::test]
    async fn each_phase_reachable_via_step_calls() {
        let (mut engine, mut frontend) = make_engine_and_frontend(true, false);
        // Step through from Preflight to Awaiting* phases individually.
        assert_eq!(engine.phase(), &ReadyPhase::Preflight);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ReadyPhase::AwaitingDockerfileDecision);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ReadyPhase::CreatingDockerfile);
    }
}
