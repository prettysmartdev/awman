//! `engine::claws` — `ClawsEngine`. Multi-phase state machine for `claws init`,
//! `claws ready`, and `claws chat`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::data::session::Session;
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;
use crate::engine::overlay::OverlayEngine;
use crate::engine::step_status::StepStatus;

pub mod frontend;
pub mod phase;
pub mod summary;

pub use frontend::ClawsFrontend;
pub use phase::{ClawsFailure, ClawsPhase};
pub use summary::ClawsSummary;

#[derive(Debug, Clone)]
pub enum ClawsMode {
    Init,
    Ready,
    Chat,
}

#[derive(Debug, Clone)]
pub struct ClawsEngineOptions {
    pub mode: ClawsMode,
    pub nanoclaw_url: Option<String>,
    pub refresh: bool,
    pub no_cache: bool,
    /// Resolved on-disk path for the local nanoclaw clone.
    pub clone_dir: PathBuf,
}

pub struct ClawsEngine {
    session: Arc<Session>,
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
    container_runtime: Arc<ContainerRuntime>,
    options: ClawsEngineOptions,
    phase: ClawsPhase,
    summary: ClawsSummary,
}

impl ClawsEngine {
    pub fn new(
        session: Arc<Session>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
        container_runtime: Arc<ContainerRuntime>,
        options: ClawsEngineOptions,
    ) -> Self {
        Self {
            session,
            git_engine,
            overlay_engine,
            container_runtime,
            options,
            phase: ClawsPhase::Preflight,
            summary: ClawsSummary::default(),
        }
    }

    pub fn phase(&self) -> &ClawsPhase {
        &self.phase
    }

    pub fn summary(&self) -> ClawsSummary {
        self.summary.clone()
    }

    pub async fn step(
        &mut self,
        frontend: &mut dyn ClawsFrontend,
    ) -> Result<ClawsPhase, EngineError> {
        frontend.report_phase(&self.phase);
        let next = match (&self.phase, &self.options.mode) {
            (ClawsPhase::Preflight, ClawsMode::Init) => {
                if self.options.clone_dir.exists() {
                    ClawsPhase::AwaitingCloneDecision
                } else {
                    ClawsPhase::CloningRepo
                }
            }
            (ClawsPhase::Preflight, ClawsMode::Ready) => {
                self.summary.clone = StepStatus::Skipped;
                self.summary.permissions_check = StepStatus::Skipped;
                self.summary.image_build = StepStatus::Skipped;
                self.summary.audit = StepStatus::Skipped;
                self.summary.configure = StepStatus::Skipped;
                ClawsPhase::LaunchingController
            }
            (ClawsPhase::Preflight, ClawsMode::Chat) => {
                self.summary.clone = StepStatus::Skipped;
                self.summary.permissions_check = StepStatus::Skipped;
                self.summary.image_build = StepStatus::Skipped;
                self.summary.audit = StepStatus::Skipped;
                self.summary.configure = StepStatus::Skipped;
                self.summary.controller = StepStatus::Skipped;
                ClawsPhase::Complete
            }
            (ClawsPhase::AwaitingCloneDecision, _) => {
                if frontend.ask_replace_existing_clone(&self.options.clone_dir)? {
                    ClawsPhase::CloningRepo
                } else {
                    self.summary.clone = StepStatus::Skipped;
                    ClawsPhase::CheckingPermissions
                }
            }
            (ClawsPhase::CloningRepo, _) => {
                self.summary.clone = StepStatus::Done;
                ClawsPhase::CheckingPermissions
            }
            (ClawsPhase::CheckingPermissions, _) => {
                self.summary.permissions_check = StepStatus::Done;
                ClawsPhase::BuildingImage
            }
            (ClawsPhase::BuildingImage, _) => {
                let _ = frontend.container_frontend();
                self.summary.image_build = StepStatus::Done;
                ClawsPhase::AwaitingAuditDecision
            }
            (ClawsPhase::AwaitingAuditDecision, _) => {
                if frontend.ask_run_audit()? {
                    ClawsPhase::RunningAudit
                } else {
                    self.summary.audit = StepStatus::Skipped;
                    ClawsPhase::Configuring
                }
            }
            (ClawsPhase::RunningAudit, _) => {
                let _ = frontend.container_frontend();
                self.summary.audit = StepStatus::Done;
                ClawsPhase::Configuring
            }
            (ClawsPhase::Configuring, _) => {
                self.summary.configure = StepStatus::Done;
                ClawsPhase::LaunchingController
            }
            (ClawsPhase::LaunchingController, _) => {
                let _ = frontend.container_frontend();
                self.summary.controller = StepStatus::Done;
                ClawsPhase::Complete
            }
            (ClawsPhase::Complete | ClawsPhase::Failed(_), _) => self.phase.clone(),
        };
        self.phase = next.clone();
        if matches!(self.phase, ClawsPhase::Complete | ClawsPhase::Failed(_)) {
            frontend.report_summary(&self.summary);
        }
        Ok(next)
    }

    pub async fn run_to_completion(
        &mut self,
        frontend: &mut dyn ClawsFrontend,
    ) -> Result<ClawsSummary, EngineError> {
        loop {
            let next = self.step(frontend).await?;
            if matches!(next, ClawsPhase::Complete | ClawsPhase::Failed(_)) {
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
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};
    use crate::engine::container::frontend::{ContainerFrontend, ContainerProgress, ContainerStatus};
    use crate::engine::message::{UserMessage, UserMessageSink};
    use crate::engine::overlay::OverlayEngine;
    use crate::engine::step_status::StepStatus;

    // ── Fake frontend ────────────────────────────────────────────────────────

    struct FakeClawsFrontend {
        replace_existing_clone: bool,
        run_audit: bool,
        container_frontend_call_count: usize,
    }

    impl FakeClawsFrontend {
        fn new(replace_existing_clone: bool, run_audit: bool) -> Self {
            Self {
                replace_existing_clone,
                run_audit,
                container_frontend_call_count: 0,
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

    impl UserMessageSink for FakeClawsFrontend {
        fn write_message(&mut self, _: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl ClawsFrontend for FakeClawsFrontend {
        fn ask_replace_existing_clone(&mut self, _path: &Path) -> Result<bool, EngineError> {
            Ok(self.replace_existing_clone)
        }

        fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
            Ok(self.run_audit)
        }

        fn report_phase(&mut self, _phase: &ClawsPhase) {}

        fn report_step_status(&mut self, _step: &str, _status: StepStatus) {}

        fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
            self.container_frontend_call_count += 1;
            Box::new(FakeContainerFrontend)
        }

        fn report_summary(&mut self, _: &ClawsSummary) {}
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_engine(mode: ClawsMode, clone_dir: std::path::PathBuf) -> ClawsEngine {
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
        ClawsEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            runtime,
            ClawsEngineOptions {
                mode,
                nanoclaw_url: None,
                refresh: false,
                no_cache: false,
                clone_dir,
            },
        )
    }

    // ── Tests ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn init_mode_fresh_clone_runs_all_phases() {
        // clone_dir does not exist → no AwaitingCloneDecision, goes straight to CloningRepo.
        let clone_dir = tempfile::tempdir().unwrap();
        let clone_path = clone_dir.path().join("nanoclaw"); // nonexistent subdir
        let mut engine = make_engine(ClawsMode::Init, clone_path);
        let mut frontend = FakeClawsFrontend::new(true, true);
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::Complete);
        assert!(matches!(summary.clone, StepStatus::Done));
        assert!(matches!(summary.permissions_check, StepStatus::Done));
        assert!(matches!(summary.image_build, StepStatus::Done));
        assert!(matches!(summary.audit, StepStatus::Done));
        assert!(matches!(summary.configure, StepStatus::Done));
        assert!(matches!(summary.controller, StepStatus::Done));
    }

    #[tokio::test]
    async fn awaiting_clone_decision_false_skips_clone() {
        // clone_dir exists → triggers AwaitingCloneDecision.
        let clone_dir = tempfile::tempdir().unwrap();
        let mut engine = make_engine(ClawsMode::Init, clone_dir.path().to_path_buf());
        // Decline the clone replacement.
        let mut frontend = FakeClawsFrontend::new(false, true);
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::Complete);
        assert!(
            matches!(summary.clone, StepStatus::Skipped),
            "clone must be Skipped when user declines"
        );
        // Continues to permissions and beyond.
        assert!(matches!(summary.permissions_check, StepStatus::Done));
    }

    #[tokio::test]
    async fn awaiting_audit_decision_false_skips_audit() {
        let clone_dir = tempfile::tempdir().unwrap();
        let clone_path = clone_dir.path().join("nanoclaw");
        let mut engine = make_engine(ClawsMode::Init, clone_path);
        let mut frontend = FakeClawsFrontend::new(true, false); // decline audit
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::Complete);
        assert!(
            matches!(summary.audit, StepStatus::Skipped),
            "audit must be Skipped when declined"
        );
        assert!(matches!(summary.configure, StepStatus::Done));
    }

    #[tokio::test]
    async fn ready_mode_skips_all_init_phases_and_launches_controller() {
        let clone_dir = tempfile::tempdir().unwrap();
        let mut engine = make_engine(ClawsMode::Ready, clone_dir.path().to_path_buf());
        let mut frontend = FakeClawsFrontend::new(true, true);
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::Complete);
        assert!(matches!(summary.clone, StepStatus::Skipped));
        assert!(matches!(summary.permissions_check, StepStatus::Skipped));
        assert!(matches!(summary.image_build, StepStatus::Skipped));
        assert!(matches!(summary.audit, StepStatus::Skipped));
        assert!(matches!(summary.configure, StepStatus::Skipped));
        assert!(matches!(summary.controller, StepStatus::Done));
    }

    #[tokio::test]
    async fn chat_mode_skips_everything_and_completes_without_container() {
        let clone_dir = tempfile::tempdir().unwrap();
        let mut engine = make_engine(ClawsMode::Chat, clone_dir.path().to_path_buf());
        let mut frontend = FakeClawsFrontend::new(true, true);
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::Complete);
        assert!(matches!(summary.clone, StepStatus::Skipped));
        assert!(matches!(summary.controller, StepStatus::Skipped));
        // No container_frontend calls in Chat mode.
        assert_eq!(
            frontend.container_frontend_call_count, 0,
            "Chat mode must not call container_frontend"
        );
    }

    #[tokio::test]
    async fn each_phase_reachable_via_step_in_init_mode() {
        let clone_dir = tempfile::tempdir().unwrap();
        let clone_path = clone_dir.path().join("nanoclaw"); // doesn't exist → no AwaitingCloneDecision
        let mut engine = make_engine(ClawsMode::Init, clone_path);
        let mut frontend = FakeClawsFrontend::new(true, true);
        assert_eq!(engine.phase(), &ClawsPhase::Preflight);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::CloningRepo);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::CheckingPermissions);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::BuildingImage);
    }
}
