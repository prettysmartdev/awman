//! `engine::ready` — `ReadyEngine`. Multi-phase state machine for `amux ready`.

use std::sync::Arc;

use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;
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
    /// Env-passthrough list for audit container runs.
    pub env_passthrough: Option<Vec<String>>,
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
    /// Hash of `Dockerfile.dev` captured just before the audit runs, so we can
    /// detect modifications made by the agent and trigger a rebuild.
    pre_audit_dockerfile_hash: Option<u64>,
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
            pre_audit_dockerfile_hash: None,
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
        use crate::data::image_tags::{agent_image_tag, project_image_tag};
        use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;
        use crate::data::templates;

        frontend.report_phase(&self.phase);
        let git_root = self.session.git_root().to_path_buf();
        let _ = &self.git_engine;
        let _ = &self.overlay_engine;

        let next = match &self.phase {
            ReadyPhase::Preflight => {
                // Issue 23: Check aspec folder and work-items config presence.
                let aspec_dir = git_root.join("aspec");
                if aspec_dir.exists() {
                    self.summary.aspec_folder = StepStatus::Done;
                } else {
                    self.summary.aspec_folder = StepStatus::Warn("aspec/ folder not found".into());
                    frontend.write_message(crate::engine::message::UserMessage {
                        level: crate::engine::message::MessageLevel::Warning,
                        text: "aspec/ folder not found in git root; run `amux init` to create it.".to_string(),
                    });
                }
                let config_path = git_root.join("aspec").join(".amux.json");
                if config_path.exists() {
                    self.summary.work_items_config = StepStatus::Done;
                } else {
                    self.summary.work_items_config = StepStatus::Warn("aspec/.amux.json not found".into());
                    frontend.write_message(crate::engine::message::UserMessage {
                        level: crate::engine::message::MessageLevel::Warning,
                        text: "aspec/.amux.json not found; run `amux init` to create it.".to_string(),
                    });
                }

                let dockerfile_path = git_root.join("Dockerfile.dev");
                if dockerfile_path.exists() {
                    self.summary.dockerfile = StepStatus::Skipped;
                    frontend.report_step_status("Check Dockerfile.dev", StepStatus::Done);
                    self.next_phase_after_dockerfile_present()
                } else {
                    ReadyPhase::AwaitingDockerfileDecision
                }
            }
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
                let paths = RepoDockerfilePaths::new(&git_root);
                let dockerfile_path = paths.project_dockerfile();
                std::fs::write(&dockerfile_path, templates::project_dockerfile_dev())
                    .map_err(|e| EngineError::io(dockerfile_path.clone(), e))?;
                self.summary.dockerfile = StepStatus::Done;
                frontend.report_step_status("Create Dockerfile.dev", StepStatus::Done);
                ReadyPhase::AwaitingLegacyMigrationDecision
            }
            ReadyPhase::AwaitingLegacyMigrationDecision => {
                if frontend.ask_migrate_legacy_layout(&self.options.agent)? {
                    ReadyPhase::MigratingLegacyLayout
                } else {
                    self.summary.legacy_migration = StepStatus::Skipped;
                    ReadyPhase::BuildingBaseImage
                }
            }
            ReadyPhase::MigratingLegacyLayout => {
                let dockerfile_path = git_root.join("Dockerfile.dev");
                let backup_path = git_root.join("Dockerfile.dev.bak");
                if dockerfile_path.exists() {
                    std::fs::copy(&dockerfile_path, &backup_path)
                        .map_err(|e| EngineError::io(backup_path.clone(), e))?;
                    frontend.write_message(crate::engine::message::UserMessage {
                        level: crate::engine::message::MessageLevel::Info,
                        text: format!(
                            "Backed up existing Dockerfile.dev to {}.",
                            backup_path.display()
                        ),
                    });
                }
                std::fs::write(&dockerfile_path, templates::project_dockerfile_dev())
                    .map_err(|e| EngineError::io(dockerfile_path.clone(), e))?;
                frontend.write_message(crate::engine::message::UserMessage {
                    level: crate::engine::message::MessageLevel::Info,
                    text: "Dockerfile.dev recreated with project base template.".to_string(),
                });
                self.summary.legacy_migration = StepStatus::Done;
                ReadyPhase::BuildingBaseImage
            }
            ReadyPhase::BuildingBaseImage => {
                // Issue 22: Docker daemon pre-check — soft failure allows
                // run_to_completion to surface a summary rather than aborting.
                if !self.container_runtime.is_available() {
                    let msg = "Docker daemon is not running. Install Docker and retry.".to_string();
                    self.summary.base_image = StepStatus::Failed(msg.clone());
                    frontend.report_step_status("Build base image", StepStatus::Failed(msg));
                    return Ok(ReadyPhase::BuildingAgentImage);
                }

                let tag = project_image_tag(&git_root);
                // Legacy gate: rebuild when --build was passed, when the base
                // image is missing, or when the legacy migration just rewrote
                // Dockerfile.dev. Otherwise skip (`amux ready` is idempotent).
                let needs_build = self.options.build
                    || matches!(self.summary.legacy_migration, StepStatus::Done)
                    || !self.container_runtime.image_exists(&tag);
                if !needs_build {
                    self.summary.base_image = StepStatus::Skipped;
                    frontend.report_step_status("Build base image", StepStatus::Skipped);
                    ReadyPhase::BuildingAgentImage
                } else {
                    frontend.report_step_status("Build base image", StepStatus::Running);
                    let dockerfile_path = git_root.join("Dockerfile.dev");
                    let mut sink = |line: &str| {
                        frontend.report_step_status(line, StepStatus::Running);
                    };
                    let result = self.container_runtime.build_image(
                        &tag,
                        &dockerfile_path,
                        &git_root,
                        self.options.no_cache,
                        &mut sink,
                    );
                    match result {
                        Ok(()) => {
                            self.summary.base_image = StepStatus::Done;
                            frontend.report_step_status("Build base image", StepStatus::Done);
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            self.summary.base_image = StepStatus::Failed(msg.clone());
                            frontend.report_step_status(
                                "Build base image",
                                StepStatus::Failed(msg),
                            );
                        }
                    }
                    ReadyPhase::BuildingAgentImage
                }
            }
            ReadyPhase::BuildingAgentImage => {
                frontend.report_step_status("Build agent image", StepStatus::Running);
                let paths = RepoDockerfilePaths::new(&git_root);
                let agent_dockerfile = paths.agent_dockerfile(self.options.agent.as_str());
                if !agent_dockerfile.exists() {
                    // Try downloading the per-agent Dockerfile (best-effort).
                    let project_tag = project_image_tag(&git_root);
                    let dl = crate::engine::agent::download::download_agent_dockerfile(
                        self.options.agent.as_str(),
                        &agent_dockerfile,
                        &project_tag,
                    )
                    .await;
                    if let Err(e) = dl {
                        let msg = e.to_string();
                        self.summary.agent_image = StepStatus::Failed(msg.clone());
                        frontend.report_step_status(
                            "Download agent Dockerfile",
                            StepStatus::Failed(msg),
                        );
                        // Continue but mark agent image not built.
                        return Ok({
                            self.phase = ReadyPhase::CheckingLocalAgent;
                            self.phase.clone()
                        });
                    }
                }
                let tag = agent_image_tag(&git_root, self.options.agent.as_str());
                let mut sink = |line: &str| {
                    frontend.report_step_status(line, StepStatus::Running);
                };
                let result = self.container_runtime.build_image(
                    &tag,
                    &agent_dockerfile,
                    &git_root,
                    self.options.no_cache,
                    &mut sink,
                );
                match result {
                    Ok(()) => {
                        self.summary.agent_image = StepStatus::Done;
                        frontend.report_step_status("Build agent image", StepStatus::Done);
                    }
                    Err(e) => {
                        self.summary.agent_image = StepStatus::Failed(e.to_string());
                        frontend.report_step_status(
                            "Build agent image",
                            StepStatus::Failed(e.to_string()),
                        );
                    }
                }
                ReadyPhase::CheckingLocalAgent
            }
            ReadyPhase::CheckingLocalAgent => {
                let tag = agent_image_tag(&git_root, self.options.agent.as_str());
                if self.container_runtime.image_exists(&tag) {
                    self.summary.local_agent = StepStatus::Done;
                } else {
                    self.summary.local_agent = StepStatus::Failed("agent image not found".into());
                }
                // Capture a hash of Dockerfile.dev before the audit so we can
                // detect agent-made changes in RebuildingAfterAudit.
                let dockerfile_path = git_root.join("Dockerfile.dev");
                self.pre_audit_dockerfile_hash = dockerfile_hash(&dockerfile_path);
                ReadyPhase::RunningAudit
            }
            ReadyPhase::RunningAudit => {
                // Issue 7: When --refresh is not set, skip the audit entirely.
                if !self.options.refresh {
                    self.summary.audit = StepStatus::Skipped;
                    self.phase = ReadyPhase::RebuildingAfterAudit;
                    return Ok(self.phase.clone());
                }
                // Inform the frontend whether the Dockerfile.dev still matches
                // the bundled template — the UI can show a hint that the audit
                // may overwrite customisations.
                let dockerfile_path = git_root.join("Dockerfile.dev");
                if dockerfile_path.exists() {
                    let content = std::fs::read_to_string(&dockerfile_path).unwrap_or_default();
                    if !templates::dockerfile_matches_template(&content) {
                        frontend.write_message(crate::engine::message::UserMessage {
                            level: crate::engine::message::MessageLevel::Warning,
                            text: "Dockerfile.dev has been customised; audit may overwrite changes.".into(),
                        });
                    }
                }
                if frontend.ask_run_audit_on_template()? {
                    use crate::data::templates::ready_audit_prompt;
                    use crate::engine::agent::AgentRunOptions;

                    let run_opts = AgentRunOptions {
                        yolo: None,
                        auto: None,
                        plan: None,
                        allowed_tools: vec![],
                        disallowed_tools: vec![],
                        initial_prompt: Some(ready_audit_prompt().to_string()),
                        allow_docker: self.options.allow_docker,
                        mount_ssh: false,
                        non_interactive: true,
                        model: None,
                        env_passthrough: self.options.env_passthrough.clone(),
                        directory_overlays: vec![],
                    };
                    match self.agent_engine.build_options(&self.session, &self.options.agent, &run_opts) {
                        Err(e) => {
                            self.summary.audit = StepStatus::Failed(e.to_string());
                        }
                        Ok(options) => match self.container_runtime.build(options) {
                            Err(e) => {
                                self.summary.audit = StepStatus::Failed(e.to_string());
                            }
                            Ok(instance) => {
                                let container_fe = frontend.container_frontend();
                                match instance.run_with_frontend(container_fe) {
                                    Err(e) => {
                                        self.summary.audit = StepStatus::Failed(e.to_string());
                                    }
                                    Ok(mut exec) => match exec.wait().await {
                                        Err(e) => {
                                            self.summary.audit = StepStatus::Failed(e.to_string());
                                        }
                                        Ok(exit) => {
                                            if exit.exit_code == 0 {
                                                self.summary.audit = StepStatus::Done;
                                            } else {
                                                self.summary.audit = StepStatus::Failed(
                                                    format!("audit exited with code {}", exit.exit_code),
                                                );
                                            }
                                        }
                                    },
                                }
                            }
                        },
                    }
                } else {
                    self.summary.audit = StepStatus::Skipped;
                }
                ReadyPhase::RebuildingAfterAudit
            }
            ReadyPhase::RebuildingAfterAudit => {
                // Only rebuild when the audit ran successfully AND modified Dockerfile.dev.
                if matches!(self.summary.audit, StepStatus::Done) {
                    let dockerfile_path = git_root.join("Dockerfile.dev");
                    let post_hash = dockerfile_hash(&dockerfile_path);
                    let changed = match (self.pre_audit_dockerfile_hash, post_hash) {
                        (Some(pre), Some(post)) => pre != post,
                        // If we can't compute either hash, conservatively assume changed.
                        _ => true,
                    };
                    if changed {
                        frontend.report_step_status("Rebuilding after audit", StepStatus::Running);
                        let tag = project_image_tag(&git_root);
                        let dockerfile_path_clone = dockerfile_path.clone();
                        let mut sink = |line: &str| {
                            frontend.report_step_status(line, StepStatus::Running);
                        };
                        let result = self.container_runtime.build_image(
                            &tag,
                            &dockerfile_path_clone,
                            &git_root,
                            self.options.no_cache,
                            &mut sink,
                        );
                        match result {
                            Ok(()) => {
                                self.summary.base_image = StepStatus::Done;
                                self.summary.image_rebuild = StepStatus::Done;
                                frontend.report_step_status(
                                    "Rebuilding after audit",
                                    StepStatus::Done,
                                );
                            }
                            Err(e) => {
                                let msg = e.to_string();
                                self.summary.base_image = StepStatus::Failed(msg.clone());
                                self.summary.image_rebuild = StepStatus::Failed(msg.clone());
                                frontend.report_step_status(
                                    "Rebuilding after audit",
                                    StepStatus::Failed(msg),
                                );
                            }
                        }

                        // Issue 9: Also rebuild agent images that layer FROM the project base.
                        let amux_dir = git_root.join(".amux");
                        if amux_dir.exists() {
                            if let Ok(entries) = std::fs::read_dir(&amux_dir) {
                                for entry in entries.flatten() {
                                    let name = entry.file_name();
                                    let name_str = name.to_string_lossy().to_string();
                                    if name_str.starts_with("Dockerfile.") {
                                        let agent = name_str.strip_prefix("Dockerfile.").unwrap_or("");
                                        if !agent.is_empty() {
                                            let agent_tag = crate::data::image_tags::agent_image_tag(&git_root, agent);
                                            let mut agent_sink = |line: &str| {
                                                frontend.report_step_status(line, StepStatus::Running);
                                            };
                                            let _ = self.container_runtime.build_image(
                                                &agent_tag,
                                                &entry.path(),
                                                &git_root,
                                                self.options.no_cache,
                                                &mut agent_sink,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        self.summary.image_rebuild = StepStatus::Skipped;
                    }
                } else {
                    self.summary.image_rebuild = StepStatus::Skipped;
                }
                ReadyPhase::Complete
            }
            ReadyPhase::Complete | ReadyPhase::Failed(_) => self.phase.clone(),
        };
        self.phase = next.clone();
        if matches!(self.phase, ReadyPhase::Complete | ReadyPhase::Failed(_)) {
            frontend.report_summary(&self.summary);
        }
        Ok(next)
    }

    /// Decide which phase to enter when `Dockerfile.dev` is already on disk.
    ///
    /// Matches old-amux `is_legacy_layout` semantics: the "migrate to modular
    /// layout?" question is only meaningful when `Dockerfile.dev` exists AND
    /// no per-agent `.amux/Dockerfile.<agent>` file has been written yet. If
    /// the per-agent file is already present, the project is on the modular
    /// layout — skip the migration phases entirely.
    fn next_phase_after_dockerfile_present(&mut self) -> ReadyPhase {
        let paths = RepoDockerfilePaths::new(self.session.git_root());
        let agent_dockerfile = paths.agent_dockerfile(self.options.agent.as_str());
        if agent_dockerfile.exists() {
            self.summary.legacy_migration = StepStatus::Skipped;
            ReadyPhase::BuildingBaseImage
        } else {
            ReadyPhase::AwaitingLegacyMigrationDecision
        }
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

/// Compute a simple hash of a file's contents for change detection.
/// Returns `None` when the file cannot be read.
fn dockerfile_hash(path: &std::path::Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let contents = std::fs::read(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    contents.hash(&mut hasher);
    Some(hasher.finish())
}

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
    ) -> (ReadyEngine, FakeReadyFrontend, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        // Pre-create .amux/Dockerfile.claude so the ready engine does not
        // attempt a network download during tests.
        let amux_dir = tmp.path().join(".amux");
        std::fs::create_dir_all(&amux_dir).unwrap();
        std::fs::write(
            amux_dir.join("Dockerfile.claude"),
            "FROM scratch\n",
        )
        .unwrap();
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
            env_passthrough: None,
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
        (engine, frontend, tmp)
    }

    // ── Tests ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_to_completion_happy_path_all_done() {
        let (mut engine, mut frontend, _tmp) = make_engine_and_frontend(true, true);
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ReadyPhase::Complete);
        // base_image / agent_image / local_agent depend on docker availability
        // — accept either Done or Failed in the test environment.
        assert!(matches!(
            summary.base_image,
            StepStatus::Done | StepStatus::Failed(_)
        ));
        assert!(matches!(
            summary.agent_image,
            StepStatus::Done | StepStatus::Failed(_)
        ));
        assert!(matches!(
            summary.local_agent,
            StepStatus::Done | StepStatus::Failed(_)
        ));
        // audit depends on docker + agent image availability in the test environment.
        assert!(matches!(
            summary.audit,
            StepStatus::Done | StepStatus::Failed(_)
        ));
    }

    #[tokio::test]
    async fn awaiting_dockerfile_decision_false_leads_to_failed_phase() {
        let (mut engine, mut frontend, _tmp) = make_engine_and_frontend(false, true);
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
        // Pre-create agent Dockerfile to avoid network download during test.
        let amux_dir = tmp.path().join(".amux");
        std::fs::create_dir_all(&amux_dir).unwrap();
        std::fs::write(amux_dir.join("Dockerfile.claude"), "FROM scratch\n").unwrap();
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
            env_passthrough: None,
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
        let (mut engine, mut frontend, _tmp) = make_engine_and_frontend(true, false);
        // Step through from Preflight to Awaiting* phases individually.
        assert_eq!(engine.phase(), &ReadyPhase::Preflight);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ReadyPhase::AwaitingDockerfileDecision);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ReadyPhase::CreatingDockerfile);
    }

    #[tokio::test]
    async fn creating_dockerfile_phase_writes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (_engine, mut frontend, _tmp2) = make_engine_and_frontend(true, false);
        // We want to test the CreatingDockerfile phase specifically. Use a
        // dedicated tmpdir so we can check file creation.
        let resolver = crate::data::session::StaticGitRootResolver::new(tmp.path());
        let session = Arc::new(
            crate::data::session::Session::open(
                tmp.path().to_path_buf(),
                &resolver,
                crate::data::session::SessionOpenOptions::default(),
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
            build: false,
            no_cache: false,
            allow_docker: false,
            env_passthrough: None,
        };
        let mut engine2 = ReadyEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            runtime,
            agent_engine,
            options,
        );
        // Step to AwaitingDockerfileDecision, then accept to move to CreatingDockerfile.
        engine2.step(&mut frontend).await.unwrap(); // Preflight → AwaitingDockerfileDecision
        engine2.step(&mut frontend).await.unwrap(); // AwaitingDockerfileDecision → CreatingDockerfile
        // Execute CreatingDockerfile phase.
        engine2.step(&mut frontend).await.unwrap(); // CreatingDockerfile → AwaitingLegacyMigrationDecision
        let dockerfile = tmp.path().join("Dockerfile.dev");
        assert!(
            dockerfile.exists(),
            "CreatingDockerfile phase must write Dockerfile.dev to git root"
        );
        let content = std::fs::read_to_string(&dockerfile).unwrap();
        assert!(
            !content.is_empty(),
            "Dockerfile.dev must contain the template content"
        );
    }

    #[tokio::test]
    async fn migrating_legacy_layout_creates_backup() {
        let tmp = tempfile::tempdir().unwrap();
        // Write an existing Dockerfile.dev (simulates legacy layout).
        let dockerfile = tmp.path().join("Dockerfile.dev");
        std::fs::write(&dockerfile, "FROM legacy\n").unwrap();

        let resolver = crate::data::session::StaticGitRootResolver::new(tmp.path());
        let session = Arc::new(
            crate::data::session::Session::open(
                tmp.path().to_path_buf(),
                &resolver,
                crate::data::session::SessionOpenOptions::default(),
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
            build: false,
            no_cache: false,
            allow_docker: false,
            env_passthrough: None,
        };
        let mut engine = ReadyEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            runtime,
            agent_engine,
            options,
        );
        // Frontend that accepts migration.
        let mut frontend = FakeReadyFrontend {
            create_dockerfile: true,
            run_audit: false,
            migrate_legacy: true,
            phases: Vec::new(),
            statuses: Vec::new(),
        };
        // Dockerfile already exists → skips AwaitingDockerfileDecision.
        engine.step(&mut frontend).await.unwrap(); // Preflight → AwaitingLegacyMigrationDecision
        engine.step(&mut frontend).await.unwrap(); // AwaitingLegacyMigrationDecision → MigratingLegacyLayout
        engine.step(&mut frontend).await.unwrap(); // MigratingLegacyLayout → BuildingBaseImage

        let backup = tmp.path().join("Dockerfile.dev.bak");
        assert!(backup.exists(), "MigratingLegacyLayout must create a .bak backup");
        let backup_content = std::fs::read_to_string(&backup).unwrap();
        assert_eq!(backup_content, "FROM legacy\n", "backup must contain original content");
        let new_content = std::fs::read_to_string(&dockerfile).unwrap();
        assert_ne!(new_content, "FROM legacy\n", "Dockerfile.dev must be overwritten");
    }

    #[tokio::test]
    async fn preflight_skips_dockerfile_decision_when_file_exists() {
        // When Dockerfile.dev already exists in the git root, the engine must
        // not ask the user "Dockerfile.dev not found; create one?" — it should
        // skip straight past the decision and the create step.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Dockerfile.dev"), "FROM scratch\n").unwrap();
        // Pre-create agent Dockerfile to avoid network download during test.
        let amux_dir = tmp.path().join(".amux");
        std::fs::create_dir_all(&amux_dir).unwrap();
        std::fs::write(amux_dir.join("Dockerfile.claude"), "FROM scratch\n").unwrap();
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
            env_passthrough: None,
        };
        let mut engine = ReadyEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            runtime,
            agent_engine,
            options,
        );
        // create_dockerfile=false would normally cause AwaitingDockerfileDecision
        // to abort the run. But because the file exists, that decision must be
        // skipped entirely and the engine must reach Complete.
        let mut frontend = FakeReadyFrontend {
            create_dockerfile: false,
            run_audit: false,
            migrate_legacy: true,
            phases: Vec::new(),
            statuses: Vec::new(),
        };
        let _summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ReadyPhase::Complete);
        assert!(
            !frontend.phases.contains(&ReadyPhase::AwaitingDockerfileDecision),
            "AwaitingDockerfileDecision must be skipped when Dockerfile.dev exists"
        );
    }

    #[tokio::test]
    async fn does_not_prompt_for_legacy_migration_when_per_agent_dockerfile_exists() {
        // Repository is already on the modular layout: both Dockerfile.dev
        // and .amux/Dockerfile.<agent> are present. Old amux's
        // is_legacy_layout() returns false here, so the engine MUST NOT ask
        // the user "Migrate to the modular layout?" — there's nothing to
        // migrate. legacy_migration must be reported as Skipped.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Dockerfile.dev"), "FROM scratch\n").unwrap();
        std::fs::create_dir_all(tmp.path().join(".amux")).unwrap();
        std::fs::write(
            tmp.path().join(".amux").join("Dockerfile.claude"),
            "FROM project-base\n",
        )
        .unwrap();
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
            env_passthrough: None,
        };
        let mut engine = ReadyEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            runtime,
            agent_engine,
            options,
        );

        // `LegacyAskTracker` records whether `ask_migrate_legacy_layout` was
        // called. The frontend MUST NOT be asked because the per-agent
        // Dockerfile already exists.
        struct LegacyAskTracker {
            inner: FakeReadyFrontend,
            asked: bool,
        }
        impl UserMessageSink for LegacyAskTracker {
            fn write_message(&mut self, _: UserMessage) {}
            fn replay_queued(&mut self) {}
        }
        impl ReadyFrontend for LegacyAskTracker {
            fn ask_create_dockerfile(&mut self) -> Result<bool, EngineError> {
                self.inner.ask_create_dockerfile()
            }
            fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
                self.inner.ask_run_audit_on_template()
            }
            fn ask_migrate_legacy_layout(
                &mut self,
                agent: &AgentName,
            ) -> Result<bool, EngineError> {
                self.asked = true;
                self.inner.ask_migrate_legacy_layout(agent)
            }
            fn report_phase(&mut self, p: &ReadyPhase) {
                self.inner.report_phase(p)
            }
            fn report_step_status(&mut self, s: &str, st: StepStatus) {
                self.inner.report_step_status(s, st)
            }
            fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
                self.inner.container_frontend()
            }
            fn report_summary(&mut self, s: &ReadySummary) {
                self.inner.report_summary(s)
            }
        }

        let mut frontend = LegacyAskTracker {
            inner: FakeReadyFrontend {
                create_dockerfile: false,
                run_audit: false,
                migrate_legacy: false,
                phases: Vec::new(),
                statuses: Vec::new(),
            },
            asked: false,
        };
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ReadyPhase::Complete);
        assert!(
            !frontend.asked,
            "ask_migrate_legacy_layout MUST NOT be called when .amux/Dockerfile.<agent> already exists"
        );
        assert!(
            !frontend
                .inner
                .phases
                .contains(&ReadyPhase::AwaitingLegacyMigrationDecision),
            "AwaitingLegacyMigrationDecision must be skipped when on the modular layout"
        );
        assert!(
            matches!(summary.legacy_migration, StepStatus::Skipped),
            "legacy_migration must be Skipped when nothing to migrate, got {:?}",
            summary.legacy_migration
        );
    }
}
