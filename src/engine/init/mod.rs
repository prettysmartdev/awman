//! `engine::init` — `InitEngine`. Multi-phase state machine for `awman init`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::data::config::file::{repo_config_path, ConfigFile, FileKind};
use crate::data::fs::repo_tree::{RepoRelPath, RepoTree};
use crate::data::init_files::{
    create_project_dockerfile, inspect_project_dockerfile, ExistingDockerfile, FileSnapshot,
    InitFile, InitFileKind, InitFileStatus, ProjectDockerfileTarget,
};
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

#[cfg(test)]
mod contracts;
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
    /// `.awman/config.json` and `.awman/Dockerfile.<agent>` as they were when
    /// init started (`None` = absent), captured in `Preflight`. Compared with
    /// the files on completion to report each as created, updated or kept —
    /// whichever phase wrote it, and however many times.
    snapshots: Vec<FileSnapshot>,
    /// The git root every project file is written through (no linked
    /// ancestors), opened in `Preflight`.
    tree: Option<RepoTree>,
    /// Where `--aspec` gets the template archive.
    aspec_downloader: crate::engine::aspec::AspecDownloader,
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
            snapshots: Vec::new(),
            tree: None,
            aspec_downloader: crate::engine::aspec::AspecDownloader::canonical(),
        }
    }

    /// Serve `--aspec` from `downloader` instead of the canonical template.
    pub fn with_aspec_downloader(
        mut self,
        downloader: crate::engine::aspec::AspecDownloader,
    ) -> Self {
        self.aspec_downloader = downloader;
        self
    }

    fn tree(&self) -> RepoTree {
        self.tree
            .clone()
            .expect("init preflight opened the repository tree")
    }

    /// Report `.awman/config.json` and the agent Dockerfile by comparing them
    /// with their `Preflight` snapshots. A file absent at both ends (an agent
    /// Dockerfile whose download failed) is not reported. Then mark which
    /// reported files a client still has to commit.
    fn record_snapshotted_files(&mut self) -> Result<(), EngineError> {
        if let Some(tree) = self.tree.clone() {
            for snapshot in std::mem::take(&mut self.snapshots) {
                if let Some(file) = snapshot
                    .finish(&tree)
                    .map_err(|e| EngineError::Other(e.to_string()))?
                {
                    self.summary.record_file(file);
                }
            }
        }
        self.mark_uncommitted();
        Ok(())
    }

    /// Set [`InitFile::uncommitted`] from `git status`: a file is uncommitted
    /// when it is new or differs from HEAD — whoever wrote it. Session setup
    /// provisions a missing project and agent Dockerfile before init can run,
    /// so init then reports them `kept` yet a client must still commit them.
    /// Outside git (or if git fails) each file keeps the default: uncommitted
    /// exactly when init wrote it.
    fn mark_uncommitted(&mut self) {
        let candidates: Vec<String> = self
            .summary
            .files
            .iter()
            .filter(|f| {
                !matches!(f.status, InitFileStatus::Skipped { .. })
                    && !std::path::Path::new(&f.path).is_absolute()
            })
            .map(|f| f.path.clone())
            .collect();
        if candidates.is_empty() {
            return;
        }
        match self
            .git_engine
            .uncommitted_paths(&self.options.git_root, &candidates)
        {
            Ok(uncommitted) => {
                for file in &mut self.summary.files {
                    if candidates.contains(&file.path) {
                        file.uncommitted = uncommitted.contains(&file.path);
                    }
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "init: git status unavailable; reporting written files as uncommitted")
            }
        }
    }

    /// Report the project Dockerfile at `path`.
    fn record_project_dockerfile(&mut self, path: &std::path::Path, status: InitFileStatus) {
        self.summary.record_file(InitFile::at(
            &self.options.git_root,
            path,
            InitFileKind::ProjectDockerfile,
            status,
        ));
    }

    /// Report the project Dockerfile at `rel` below the git root.
    fn record_project_dockerfile_rel(&mut self, rel: &RepoRelPath, status: InitFileStatus) {
        self.summary.record_file(InitFile::new(
            rel.display(),
            InitFileKind::ProjectDockerfile,
            status,
        ));
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
                let _ = self.overlay_engine;
                // Every project file below is written through this tree: no
                // linked ancestor (a symlinked `.awman/`, say) is followed.
                let tree =
                    RepoTree::open(&git_root).map_err(|e| EngineError::Other(e.to_string()))?;
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
                let agent_dockerfile = RepoDockerfilePaths::new(&git_root)
                    .agent_dockerfile(self.options.agent.as_str());
                let mut snapshots = Vec::new();
                for (path, kind) in [
                    (repo_config_path(&git_root), InitFileKind::RepoConfig),
                    (agent_dockerfile, InitFileKind::AgentDockerfile),
                ] {
                    let rel = tree.relative(&path).ok_or_else(|| {
                        EngineError::Other(format!("{} is not below the git root", path.display()))
                    })?;
                    snapshots.push(
                        FileSnapshot::take(&tree, rel, kind)
                            .map_err(|e| EngineError::Other(e.to_string()))?,
                    );
                }
                self.snapshots = snapshots;
                // `.awman/` must be a real directory: the config and agent
                // Dockerfile are written into it.
                tree.create_dir_all(
                    &RepoRelPath::new(std::path::Path::new(".awman")).expect("plain name"),
                )
                .map_err(|e| EngineError::Other(e.to_string()))?;
                self.tree = Some(tree);
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
                // Always try to download the aspec template — this phase is only
                // reached when --aspec is passed or the user confirmed replacement.
                let tree = self.tree();
                let mut downloaded = false;
                let mut written = Vec::new();
                let result = match self.aspec_downloader.download().await {
                    Ok(bytes) => {
                        crate::engine::aspec::AspecDownloader::extract(&bytes, &tree, &mut written)
                    }
                    Err(e) => Err(e),
                };
                // Every template file is reported — also those written
                // before a failure part-way through.
                for file in written {
                    self.summary.record_file(file);
                }
                match result {
                    Ok(()) => downloaded = true,
                    Err(e) => frontend.write_message(crate::data::message::UserMessage {
                        level: crate::data::message::MessageLevel::Warning,
                        text: format!("aspec download failed: {e}; using empty aspec directory"),
                    }),
                }
                if !downloaded {
                    // Fall back to creating an empty aspec dir so subsequent
                    // engines can write into it.
                    tree.create_dir_all(
                        &RepoRelPath::new(std::path::Path::new("aspec")).expect("plain name"),
                    )
                    .map_err(|e| EngineError::Other(e.to_string()))?;
                }
                self.summary.aspec_folder = StepStatus::Done;
                InitPhase::SettingUpDockerfile
            }
            InitPhase::SettingUpDockerfile => {
                let dockerfile_path = dockerfile_path_or_default(&self.repo_config, &git_root);
                match inspect_project_dockerfile(&dockerfile_path) {
                    ExistingDockerfile::Usable => {
                        self.summary.dockerfile = StepStatus::Done;
                        self.record_project_dockerfile(&dockerfile_path, InitFileStatus::Kept);
                        InitPhase::SettingUpAgentDockerfile
                    }
                    ExistingDockerfile::Missing => InitPhase::AwaitingDockerfileDecision,
                    // A dangling link, directory or special file: not a
                    // Dockerfile a build can use, and not something to
                    // replace. Say so instead of reporting it kept.
                    ExistingDockerfile::Unusable(reason) => {
                        frontend.write_message(crate::data::message::UserMessage {
                            level: crate::data::message::MessageLevel::Warning,
                            text: reason.clone(),
                        });
                        self.record_project_dockerfile(
                            &dockerfile_path,
                            InitFileStatus::Skipped { reason },
                        );
                        self.summary.dockerfile = StepStatus::Skipped;
                        InitPhase::SettingUpAgentDockerfile
                    }
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
                        // The path `SettingUpDockerfile` looked at and every
                        // build reads — the configured `runtime.dockerfile`,
                        // else `Dockerfile.dev` — never a fixed name the
                        // build would then ignore.
                        let dockerfile_path =
                            dockerfile_path_or_default(&self.repo_config, &git_root);
                        let tree = self.tree();
                        match ProjectDockerfileTarget::classify(&tree, &dockerfile_path) {
                            ProjectDockerfileTarget::InRepo(rel) => {
                                let status = create_project_dockerfile(
                                    &tree,
                                    &rel,
                                    templates::project_dockerfile_dev().as_bytes(),
                                )
                                .map_err(|e| EngineError::Other(e.to_string()))?;
                                if let InitFileStatus::Skipped { reason } = &status {
                                    frontend.write_message(crate::data::message::UserMessage {
                                        level: crate::data::message::MessageLevel::Warning,
                                        text: reason.clone(),
                                    });
                                    self.summary.dockerfile = StepStatus::Skipped;
                                } else {
                                    self.summary.dockerfile = StepStatus::Done;
                                }
                                self.record_project_dockerfile_rel(&rel, status);
                            }
                            ProjectDockerfileTarget::OutsideRepo(path) => {
                                let reason = ProjectDockerfileTarget::outside_reason(&path);
                                frontend.write_message(crate::data::message::UserMessage {
                                    level: crate::data::message::MessageLevel::Warning,
                                    text: reason.clone(),
                                });
                                self.record_project_dockerfile(
                                    &path,
                                    InitFileStatus::Skipped { reason },
                                );
                                self.summary.dockerfile = StepStatus::Skipped;
                            }
                        }
                        InitPhase::SettingUpAgentDockerfile
                    }
                    DockerfileSetupDecision::UseExisting(path) => {
                        let resolved = {
                            let p = std::path::Path::new(&path);
                            if p.is_absolute() {
                                p.to_path_buf()
                            } else {
                                git_root.join(p)
                            }
                        };
                        let status = match inspect_project_dockerfile(&resolved) {
                            ExistingDockerfile::Usable => InitFileStatus::Kept,
                            ExistingDockerfile::Missing => InitFileStatus::Skipped {
                                reason: format!("{path} does not exist"),
                            },
                            ExistingDockerfile::Unusable(reason) => {
                                InitFileStatus::Skipped { reason }
                            }
                        };
                        self.record_project_dockerfile(&resolved, status);
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
                        let dockerfile_path =
                            dockerfile_path_or_default(&self.repo_config, &git_root);
                        self.record_project_dockerfile(
                            &dockerfile_path,
                            InitFileStatus::Skipped {
                                reason: "Dockerfile setup was declined".to_string(),
                            },
                        );
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
            self.record_snapshotted_files()?;
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
