//! `engine::init` — `InitEngine`. Multi-phase state machine for `awman init`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::data::config::file::{repo_config_path, ConfigFile, FileKind};
use crate::data::prompt::Prompt;
use crate::data::session::{AgentName, Session};
use crate::data::setup_step::{InitStep, SetupStep};
use crate::data::step_status::StepStatus;
use crate::engine::agent::AgentEngine;
use crate::engine::agent_runtime::{AgentRuntimeEngine, ResolvedAgentOptions};
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;
use crate::engine::init::frontend::DockerfileSetupChoice;
use crate::engine::overlay::OverlayEngine;

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
    /// The Dockerfile-setup question, built by Layer 2
    /// (`command::prompts::dockerfile_setup`).
    ///
    /// An engine cannot name a Layer 2 type, so prompt copy reaches Layer 1
    /// the way `src/data/prompt.rs` prescribes: as a plain Layer 0
    /// `Prompt<D>` handed down through the options struct. It carries the
    /// answer a dismissal means, which both frontends used to choose for
    /// themselves (WI 0114 F-19).
    pub dockerfile_setup_prompt: Prompt<DockerfileSetupChoice>,
}

pub struct InitEngine {
    session: Arc<Session>,
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
    runtime: Arc<dyn AgentRuntimeEngine>,
    agent_engine: Arc<AgentEngine>,
    options: InitEngineOptions,
    phase: InitPhase,
    summary: InitSummary,
    pending_dockerfile_path: Option<String>,
    /// Whether `.awman/config.json` existed when init started. Captured in
    /// `Preflight` so the `UseExisting` decision can decide whether to merge
    /// into the existing file (via `SavingDockerfileConfig`) or fold the
    /// dockerfile field into the fresh write performed by `WritingConfig`.
    config_existed_at_start: bool,
    /// The repo config this run is building, held across phases.
    ///
    /// `init` is the one engine that legitimately re-reads the repo config
    /// mid-run — it is the thing writing it. It used to do that with five
    /// separate `RepoConfig::load(&git_root).unwrap_or_default()` calls
    /// (WI 0114 F-31), which meant five chances to disagree and five silent
    /// `unwrap_or_default`s swallowing a malformed file. It now loads once in
    /// `Preflight`, mutates this copy, and saves it — the saved file and the
    /// engine's view can no longer diverge.
    repo_config: ConfigFile,
    definition: Option<crate::engine::agent::ResolvedAgent>,
}

impl InitEngine {
    pub fn new(
        session: Arc<Session>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
        runtime: Arc<dyn AgentRuntimeEngine>,
        agent_engine: Arc<AgentEngine>,
        options: InitEngineOptions,
    ) -> Self {
        Self {
            session,
            git_engine,
            overlay_engine,
            runtime,
            agent_engine,
            options,
            phase: InitPhase::Preflight,
            summary: InitSummary::default(),
            pending_dockerfile_path: None,
            config_existed_at_start: false,
            repo_config: ConfigFile::default(),
            definition: None,
        }
    }

    pub fn phase(&self) -> &InitPhase {
        &self.phase
    }

    pub fn summary(&self) -> &InitSummary {
        &self.summary
    }

    /// Save the run's repo config.
    fn save_existing_repo_config(
        &mut self,
        git_root: &std::path::Path,
        _frontend: &mut dyn InitFrontend,
    ) -> Result<(), EngineError> {
        self.repo_config
            .save(&repo_config_path(git_root), FileKind::Repo)?;
        Ok(())
    }

    pub async fn step(
        &mut self,
        frontend: &mut dyn InitFrontend,
    ) -> Result<InitPhase, EngineError> {
        use crate::data::image_tags::project_image_tag;
        use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;
        use crate::data::templates;

        frontend.report_phase(&self.phase);
        let git_root = self.options.git_root.clone();

        let next = match &self.phase {
            InitPhase::Preflight => {
                self.definition = Some(
                    self.agent_engine
                        .resolve_definition(&self.session, &self.options.agent)?,
                );
                let _ = self.git_engine;
                let _ = self.overlay_engine;
                // Snapshot before creating .awman/ so we can tell whether
                // config.json already existed when init began.
                self.config_existed_at_start = repo_config_path(&git_root).exists();
                // One load for the whole run (F-31). A malformed file is
                // reported here, where the user can still see which file and
                // why, rather than being silently defaulted away five times.
                self.repo_config = if self.config_existed_at_start {
                    ConfigFile::load(&repo_config_path(&git_root), FileKind::Repo)?
                } else {
                    ConfigFile::default()
                };
                let awman_dir = git_root.join(".awman");
                if let Err(e) = std::fs::create_dir_all(&awman_dir) {
                    tracing::warn!("failed to create .awman directory: {e}");
                }
                InitPhase::AwaitingAspecDecision
            }
            InitPhase::AwaitingAspecDecision => {
                let aspec_exists = git_root.join("aspec").exists();
                if self.options.run_aspec_setup {
                    // --aspec flag: always set up (download templates).
                    InitPhase::CreatingAspecFolder
                } else if aspec_exists {
                    // aspec/ already exists: ask if the user wants to replace it.
                    if frontend.ask_replace_aspec()? {
                        InitPhase::CreatingAspecFolder
                    } else {
                        self.summary.aspec_folder = StepStatus::Skipped;
                        InitPhase::SettingUpDockerfile
                    }
                } else {
                    // No aspec/ and no --aspec flag: skip entirely.
                    self.summary.aspec_folder = StepStatus::Skipped;
                    InitPhase::SettingUpDockerfile
                }
            }
            InitPhase::CreatingAspecFolder => {
                let aspec_dir = git_root.join("aspec");
                // Always try to download the aspec template — this phase is only
                // reached when --aspec is passed or the user confirmed replacement.
                let mut downloaded = false;
                let downloader = crate::engine::aspec::AspecDownloader::canonical();
                match downloader.download().await {
                    Ok(bytes) => {
                        match crate::engine::aspec::AspecDownloader::extract(&bytes, &aspec_dir) {
                            Ok(()) => downloaded = true,
                            Err(e) => {
                                frontend.write_message(crate::data::message::UserMessage {
                                    level: crate::data::message::MessageLevel::Warning,
                                    text: format!(
                                        "aspec download failed: {e}; using empty aspec directory"
                                    ),
                                });
                            }
                        }
                    }
                    Err(e) => {
                        frontend.write_message(crate::data::message::UserMessage {
                            level: crate::data::message::MessageLevel::Warning,
                            text: format!(
                                "aspec download failed: {e}; using empty aspec directory"
                            ),
                        });
                    }
                }
                if !downloaded {
                    // Fall back to creating an empty aspec dir so subsequent
                    // engines can write into it.
                    if !aspec_dir.exists() {
                        std::fs::create_dir_all(&aspec_dir)
                            .map_err(|e| EngineError::io(aspec_dir.clone(), e))?;
                    }
                }
                self.summary.aspec_folder = StepStatus::Done;
                InitPhase::SettingUpDockerfile
            }
            InitPhase::SettingUpDockerfile => {
                let dockerfile_path = dockerfile_path_or_default(&self.repo_config, &git_root);
                if dockerfile_path.exists() {
                    self.summary.dockerfile = StepStatus::Done;
                    InitPhase::SettingUpAgentDockerfile
                } else {
                    InitPhase::AwaitingDockerfileDecision
                }
            }
            InitPhase::AwaitingDockerfileDecision => {
                use crate::engine::init::frontend::DockerfileSetupDecision;
                let dockerfile_display = self
                    .repo_config
                    .runtime
                    .as_ref()
                    .and_then(|r| r.dockerfile.clone())
                    .unwrap_or_else(|| "Dockerfile.dev".to_string());
                match frontend.ask_dockerfile_setup(
                    &self.options.dockerfile_setup_prompt,
                    &git_root,
                    &dockerfile_display,
                )? {
                    DockerfileSetupDecision::CreateNew => {
                        let paths = RepoDockerfilePaths::new(&git_root);
                        let dockerfile_path = paths.project_dockerfile();
                        std::fs::write(&dockerfile_path, templates::project_dockerfile_dev())
                            .map_err(|e| EngineError::io(dockerfile_path.clone(), e))?;
                        self.summary.dockerfile = StepStatus::Done;
                        InitPhase::SettingUpAgentDockerfile
                    }
                    DockerfileSetupDecision::UseExisting(path) => {
                        self.pending_dockerfile_path = Some(path);
                        // If config already existed at init start, merge into
                        // it via SavingDockerfileConfig. Otherwise, let
                        // WritingConfig fold the dockerfile field into the
                        // single initial write (preserves the `agent` field).
                        if self.config_existed_at_start {
                            InitPhase::SavingDockerfileConfig
                        } else {
                            self.summary.dockerfile = StepStatus::Done;
                            InitPhase::SettingUpAgentDockerfile
                        }
                    }
                    DockerfileSetupDecision::Skip => {
                        self.summary.dockerfile = StepStatus::Skipped;
                        InitPhase::SettingUpAgentDockerfile
                    }
                }
            }
            InitPhase::SavingDockerfileConfig => {
                if let Some(dockerfile) = self.pending_dockerfile_path.take() {
                    self.repo_config
                        .runtime
                        .get_or_insert_with(Default::default)
                        .dockerfile = Some(dockerfile);
                }
                self.save_existing_repo_config(&git_root, frontend)?;
                self.summary.dockerfile = StepStatus::Done;
                InitPhase::SettingUpAgentDockerfile
            }
            // Issue 10: Ensure .awman/Dockerfile.<agent> exists.
            InitPhase::SettingUpAgentDockerfile => {
                let paths = RepoDockerfilePaths::new(&git_root);
                let project_tag = crate::data::image_tags::project_image_tag(&git_root);
                let def = self
                    .definition
                    .as_ref()
                    .expect("init preflight resolved the definition");
                {
                    let dl =
                        crate::engine::agent::provision_agent_dockerfile(def, &paths, &project_tag)
                            .await;
                    if let Err(e) = dl {
                        if def.is_custom() {
                            return Err(EngineError::Other(format!(
                                "agent Dockerfile render failed: {e}"
                            )));
                        }
                        frontend.write_message(crate::data::message::UserMessage {
                            level: crate::data::message::MessageLevel::Warning,
                            text: format!(
                                "agent Dockerfile download failed: {e}; continuing without it"
                            ),
                        });
                    } else if let Ok(sync) = dl {
                        for warning in sync.warnings {
                            frontend.write_message(crate::data::message::UserMessage {
                                level: crate::data::message::MessageLevel::Warning,
                                text: warning,
                            });
                        }
                    }
                }
                InitPhase::WritingConfig
            }
            InitPhase::WritingConfig => {
                let config_path = repo_config_path(&git_root);
                if !config_path.exists() {
                    let cfg = ConfigFile {
                        agents: Some(crate::data::config::AgentsConfig {
                            default: Some(self.options.agent.as_str().to_string()),
                            ..Default::default()
                        }),
                        runtime: self.pending_dockerfile_path.take().map(|dockerfile| {
                            crate::data::config::RuntimeConfig {
                                dockerfile: Some(dockerfile),
                                ..Default::default()
                            }
                        }),
                        ..Default::default()
                    };
                    cfg.save(&config_path, FileKind::Repo)?;
                } else {
                    frontend.write_message(crate::data::message::UserMessage {
                        level: crate::data::message::MessageLevel::Info,
                        text: ".awman/config.json already present — preserving existing config."
                            .to_string(),
                    });
                }
                self.summary.config = StepStatus::Done;
                InitPhase::AwaitingAuditDecision
            }
            InitPhase::AwaitingAuditDecision => {
                if frontend.ask_run_audit()? {
                    InitPhase::BuildingImage
                } else {
                    self.summary.audit = StepStatus::Skipped;
                    self.summary.image_build = StepStatus::Skipped;
                    self.summary.agent_image_build = StepStatus::Skipped;
                    self.summary.image_rebuild = StepStatus::Skipped;
                    InitPhase::AwaitingWorkItemsDecision
                }
            }
            InitPhase::BuildingImage => {
                // Issue 16: Docker daemon pre-check — soft failure allows
                // run_to_completion to surface a summary rather than aborting.
                if !self.runtime.is_available() {
                    let msg = "Docker daemon is not running. Install Docker and retry.".to_string();
                    self.summary.image_build = StepStatus::Failed(msg.clone());
                    self.summary.audit = StepStatus::Skipped;
                    self.summary.agent_image_build = StepStatus::Skipped;
                    self.summary.image_rebuild = StepStatus::Skipped;
                    frontend
                        .report_step_status(&InitStep::BuildImage.into(), StepStatus::Failed(msg));
                    return Ok(InitPhase::AwaitingWorkItemsDecision);
                }

                let dockerfile_path = dockerfile_path_or_default(&self.repo_config, &git_root);
                let tag = project_image_tag(&git_root);
                frontend.report_step_status(&InitStep::BuildBaseImage.into(), StepStatus::Running);
                let mut sink = |line: &str| {
                    frontend.report_step_status(&SetupStep::output(line), StepStatus::Running);
                };
                let result = self.runtime.build_image(
                    &crate::engine::container::BuildRequest::runtime(
                        &tag,
                        &dockerfile_path,
                        &git_root,
                        false,
                    ),
                    &mut sink,
                );
                match result {
                    Ok(()) => {
                        self.summary.image_build = StepStatus::Done;
                        frontend
                            .report_step_status(&InitStep::BuildBaseImage.into(), StepStatus::Done);
                        InitPhase::BuildingAgentImage
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        self.summary.image_build = StepStatus::Failed(msg.clone());
                        frontend.report_step_status(
                            &InitStep::BuildBaseImage.into(),
                            StepStatus::Failed(msg.clone()),
                        );
                        // Skip audit; nothing to audit without a base image.
                        self.summary.audit = StepStatus::Skipped;
                        self.summary.agent_image_build = StepStatus::Skipped;
                        self.summary.image_rebuild = StepStatus::Skipped;
                        InitPhase::AwaitingWorkItemsDecision
                    }
                }
            }
            // Issue 11: Build the agent image after the base image.
            InitPhase::BuildingAgentImage => {
                use crate::data::image_tags::agent_image_tag;

                let paths = RepoDockerfilePaths::new(&git_root);
                let agent_dockerfile = paths.agent_dockerfile(self.options.agent.as_str());
                let agent_tag = agent_image_tag(&git_root, self.options.agent.as_str());

                if agent_dockerfile.exists() {
                    frontend
                        .report_step_status(&InitStep::BuildAgentImage.into(), StepStatus::Running);
                    let mut sink = |line: &str| {
                        frontend.report_step_status(&SetupStep::output(line), StepStatus::Running);
                    };
                    let result = self.runtime.build_image(
                        &crate::engine::container::BuildRequest::runtime(
                            &agent_tag,
                            &agent_dockerfile,
                            &git_root,
                            false,
                        ),
                        &mut sink,
                    );
                    match result {
                        Ok(()) => {
                            self.summary.agent_image_build = StepStatus::Done;
                            frontend.report_step_status(
                                &InitStep::BuildAgentImage.into(),
                                StepStatus::Done,
                            );
                        }
                        Err(e) => {
                            let msg = self
                                .definition
                                .as_ref()
                                .map(|def| crate::engine::agent::custom_build_failure(def, &e))
                                .unwrap_or_else(|| e.to_string());
                            self.summary.agent_image_build = StepStatus::Failed(msg.clone());
                            frontend.report_step_status(
                                &InitStep::BuildAgentImage.into(),
                                StepStatus::Failed(msg),
                            );
                        }
                    }
                } else {
                    self.summary.agent_image_build = StepStatus::Skipped;
                    frontend.write_message(crate::data::message::UserMessage {
                        level: crate::data::message::MessageLevel::Warning,
                        text: "Agent Dockerfile not found; skipping agent image build.".to_string(),
                    });
                }
                InitPhase::RunningAudit
            }
            InitPhase::RunningAudit => {
                use crate::data::templates::init_audit_prompt;
                use crate::engine::agent::AgentRunOptions;

                // Route through `AgentEngine::build_options` so overlays,
                // agent settings, env passthrough, and the standard /workspace
                // working dir all apply — matching `ReadyEngine::RunningAudit`.
                let run_opts = AgentRunOptions {
                    yolo: None,
                    auto: None,
                    plan: None,
                    allowed_tools: vec![],
                    disallowed_tools: vec![],
                    initial_prompt: Some(init_audit_prompt().to_string()),
                    allow_docker: false,
                    non_interactive: false,
                    model: None,
                    env_passthrough: None,
                    directory_overlays: vec![],
                    include_all_skills: false,
                    named_skills: vec![],
                    image_tag_override: None,
                    ..Default::default()
                };
                match self
                    .agent_engine
                    .build_options(&self.session, &self.options.agent, &run_opts)
                {
                    Err(e) => {
                        // Unknown agent or option-build failure — skip audit
                        // gracefully (init flow continues).
                        self.summary.audit = StepStatus::Skipped;
                        frontend.write_message(crate::data::message::UserMessage {
                            level: crate::data::message::MessageLevel::Warning,
                            text: format!("skipping audit: {e}"),
                        });
                    }
                    Ok(options) => {
                        match ResolvedAgentOptions::container(options)
                            .and_then(|o| self.runtime.build(o))
                        {
                            Err(e) => {
                                self.summary.audit = StepStatus::Skipped;
                                frontend.write_message(crate::data::message::UserMessage {
                                    level: crate::data::message::MessageLevel::Warning,
                                    text: format!("skipping audit: {e}"),
                                });
                            }
                            Ok(instance) => {
                                let container_fe = frontend.container_frontend();
                                match instance.run_with_frontend(container_fe) {
                                    Err(e) => {
                                        self.summary.audit = StepStatus::Skipped;
                                        frontend.write_message(crate::data::message::UserMessage {
                                            level: crate::data::message::MessageLevel::Warning,
                                            text: format!("skipping audit: {e}"),
                                        });
                                    }
                                    Ok(mut exec) => match exec.wait().await {
                                        Err(e) => {
                                            self.summary.audit = StepStatus::Failed(e.to_string());
                                        }
                                        Ok(exit) => {
                                            if exit.exit_code == 0 {
                                                self.summary.audit = StepStatus::Done;
                                            } else {
                                                self.summary.audit = StepStatus::Failed(format!(
                                                    "audit exited with code {}",
                                                    exit.exit_code
                                                ));
                                            }
                                        }
                                    },
                                }
                            }
                        }
                    }
                }
                // Issue 12: After the audit, rebuild images if audit succeeded.
                InitPhase::RebuildingAfterAudit
            }
            // Issue 12: Post-audit image rebuild in init.
            InitPhase::RebuildingAfterAudit => {
                if matches!(self.summary.audit, StepStatus::Done) {
                    let dockerfile_path = dockerfile_path_or_default(&self.repo_config, &git_root);
                    let paths = RepoDockerfilePaths::new(&git_root);
                    let tag = project_image_tag(&git_root);
                    frontend.report_step_status(
                        &InitStep::RebuildingAfterAudit.into(),
                        StepStatus::Running,
                    );
                    let mut sink = |line: &str| {
                        frontend.report_step_status(&SetupStep::output(line), StepStatus::Running);
                    };
                    let result = self.runtime.build_image(
                        &crate::engine::container::BuildRequest::runtime(
                            &tag,
                            &dockerfile_path,
                            &git_root,
                            false,
                        ),
                        &mut sink,
                    );
                    match result {
                        Ok(()) => {
                            self.summary.image_rebuild = StepStatus::Done;
                            frontend.report_step_status(
                                &InitStep::RebuildingAfterAudit.into(),
                                StepStatus::Done,
                            );
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            self.summary.image_rebuild = StepStatus::Failed(msg.clone());
                            frontend.report_step_status(
                                &InitStep::RebuildingAfterAudit.into(),
                                StepStatus::Failed(msg),
                            );
                        }
                    }
                    // Also rebuild agent image.
                    if matches!(self.summary.image_rebuild, StepStatus::Done) {
                        use crate::data::image_tags::agent_image_tag;
                        let agent_dockerfile = paths.agent_dockerfile(self.options.agent.as_str());
                        if agent_dockerfile.exists() {
                            let definition = self
                                .definition
                                .as_ref()
                                .expect("init preflight resolved the definition");
                            let sync = crate::engine::agent::provision_agent_dockerfile(
                                definition, &paths, &tag,
                            )
                            .await?;
                            for warning in sync.warnings {
                                frontend.write_message(crate::data::message::UserMessage {
                                    level: crate::data::message::MessageLevel::Warning,
                                    text: warning,
                                });
                            }
                            let agent_tag = agent_image_tag(&git_root, self.options.agent.as_str());
                            let mut agent_sink = |line: &str| {
                                frontend.report_step_status(
                                    &SetupStep::output(line),
                                    StepStatus::Running,
                                );
                            };
                            let result = self.runtime.build_image(
                                &crate::engine::container::BuildRequest::runtime(
                                    &agent_tag,
                                    &agent_dockerfile,
                                    &git_root,
                                    false,
                                ),
                                &mut agent_sink,
                            );
                            if let Err(error) = result {
                                let status = StepStatus::Failed(
                                    crate::engine::agent::custom_build_failure(definition, error),
                                );
                                self.summary.agent_image_build = status.clone();
                                frontend
                                    .report_step_status(&InitStep::BuildAgentImage.into(), status);
                            }
                        }
                    }
                } else {
                    self.summary.image_rebuild = StepStatus::Skipped;
                }
                InitPhase::AwaitingWorkItemsDecision
            }
            InitPhase::AwaitingWorkItemsDecision => {
                let aspec_exists = git_root.join("aspec").exists();
                if aspec_exists {
                    self.summary.work_items_setup = StepStatus::Skipped;
                    InitPhase::Complete
                } else {
                    let cfg = frontend.ask_work_items_setup()?;
                    if let Some(work_items) = cfg {
                        self.repo_config.work_items = Some(work_items);
                        self.save_existing_repo_config(&git_root, frontend)?;
                        InitPhase::WritingWorkItemsConfig
                    } else {
                        self.summary.work_items_setup = StepStatus::Skipped;
                        InitPhase::Complete
                    }
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

/// `runtime.dockerfile` of a repo file resolved against `git_root`, else
/// `<git_root>/Dockerfile.dev`.
fn dockerfile_path_or_default(config: &ConfigFile, git_root: &std::path::Path) -> PathBuf {
    let raw = config
        .runtime
        .as_ref()
        .and_then(|r| r.dockerfile.as_deref())
        .filter(|d| !d.is_empty())
        .unwrap_or("Dockerfile.dev");
    let path = std::path::Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        git_root.join(path)
    }
}
