//! `engine::ready` — `ReadyEngine`. Multi-phase state machine for `awman ready`.

use std::sync::Arc;

use crate::data::session::{AgentName, Session};
use crate::data::setup_step::{ReadyStep, SetupStep};
use crate::data::step_status::StepStatus;
use crate::engine::agent::AgentEngine;
use crate::engine::agent_runtime::{AgentRuntimeEngine, ReadyAgentOptions, ResolvedAgentOptions};
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;
use crate::engine::overlay::OverlayEngine;

pub mod frontend;
pub mod host_agent;

pub use crate::data::ready_phase::{ReadyFailure, ReadyPhase};
// Re-exported at the module root so the many call sites that name the ping
// result or the greeting table keep their import path; the host execution
// itself is reachable only through `HostAgentPinger`.
pub use crate::data::ready_summary::ReadySummary;
pub use frontend::ReadyFrontend;
pub use host_agent::{
    select_random_greeting, HostAgentPinger, HostRefreshOutcome, LocalAgentPingResult, GREETINGS,
};

#[derive(Debug, Clone)]
pub struct ReadyEngineOptions {
    pub agent: AgentName,
    pub refresh: bool,
    pub build: bool,
    pub no_cache: bool,
    pub allow_docker: bool,
    pub non_interactive: bool,
    /// Env-passthrough list for audit container runs.
    pub env_passthrough: Option<Vec<String>>,
}

pub struct ReadyEngine {
    session: Arc<Session>,
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
    runtime: Arc<dyn AgentRuntimeEngine>,
    agent_engine: Arc<AgentEngine>,
    options: ReadyEngineOptions,
    phase: ReadyPhase,
    summary: ReadySummary,
    /// Hash of `Dockerfile.dev` captured just before the audit runs, so we can
    /// detect modifications made by the agent and trigger a rebuild.
    pre_audit_dockerfile_hash: Option<u64>,
    /// The one handle permitted to execute an agent binary on the host
    /// (F-38). `awman ready`'s local-agent check is authorized trigger (a)
    /// in `security.md`.
    host_agent: HostAgentPinger,
}

impl ReadyEngine {
    pub fn new(
        session: Arc<Session>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
        runtime: Arc<dyn AgentRuntimeEngine>,
        agent_engine: Arc<AgentEngine>,
        options: ReadyEngineOptions,
    ) -> Self {
        let runtime_name = runtime.runtime_name().to_string();
        Self {
            session,
            git_engine,
            overlay_engine,
            runtime,
            agent_engine,
            options,
            phase: ReadyPhase::Preflight,
            summary: ReadySummary::new(runtime_name),
            pre_audit_dockerfile_hash: None,
            host_agent: HostAgentPinger::new(),
        }
    }

    pub fn phase(&self) -> &ReadyPhase {
        &self.phase
    }

    pub fn summary(&self) -> ReadySummary {
        self.summary.clone()
    }

    /// Supply command-layer credential health before the terminal summary is
    /// rendered. The ready engine itself never reads credential contents.
    pub fn set_agent_credentials(
        &mut self,
        credentials: Vec<crate::data::ready_summary::AgentCredentialHealth>,
    ) {
        self.summary.agent_credentials = credentials;
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

        // A kit-declarative runtime prepares an agent by emitting and
        // validating a kit, not by building Docker images. The whole
        // container phase machine is skipped in one step. F-40b moved this
        // branch down here from `commands/ready.rs`: it is a paradigm
        // decision, and Layer 1 is where paradigm decisions on
        // `capabilities()` belong.
        if self.runtime.capabilities().kit_declarative
            && !matches!(self.phase, ReadyPhase::Complete)
        {
            let result = self.runtime.ready_agent(
                self.options.agent.as_str(),
                ReadyAgentOptions {
                    no_cache: self.options.no_cache,
                    build: self.options.build,
                },
                frontend,
            );
            // Every container-tier step is genuinely not applicable here, so
            // the summary reports Skipped rather than a misleading Done.
            self.summary.dockerfile = StepStatus::Skipped;
            self.summary.base_image = StepStatus::Skipped;
            self.summary.local_agent = StepStatus::Skipped;
            self.summary.audit = StepStatus::Skipped;
            self.summary.agent_image = match &result {
                Ok(()) => StepStatus::Done,
                Err(e) => StepStatus::Failed(e.to_string()),
            };
            frontend.report_step_status(
                &ReadyStep::ApplyAgentKit.into(),
                self.summary.agent_image.clone(),
            );
            self.phase = ReadyPhase::Complete;
            frontend.report_summary(&self.summary);
            result?;
            return Ok(self.phase.clone());
        }

        let next = match &self.phase {
            ReadyPhase::Preflight => {
                // Issue 23: Check aspec folder and work-items config presence.
                let aspec_dir = git_root.join("aspec");
                if aspec_dir.exists() {
                    self.summary.aspec_folder = StepStatus::Done;
                } else {
                    self.summary.aspec_folder = StepStatus::Warn("aspec/ folder not found".into());
                    frontend.write_message(crate::data::message::UserMessage {
                        level: crate::data::message::MessageLevel::Warning,
                        text: "aspec/ folder not found in git root; run `awman init` to create it."
                            .to_string(),
                    });
                }
                // Repo config: .awman/config.json
                let repo_config = git_root.join(".awman").join("config.json");
                if repo_config.exists() {
                    self.summary.work_items_config = StepStatus::Done;
                } else {
                    self.summary.work_items_config =
                        StepStatus::Warn(".awman/config.json not found".into());
                    frontend.write_message(crate::data::message::UserMessage {
                        level: crate::data::message::MessageLevel::Warning,
                        text: ".awman/config.json not found; run `awman init` to create it."
                            .to_string(),
                    });
                }

                let dockerfile_path = self
                    .session
                    .repo_config()
                    .dockerfile_path_or_default(&git_root);
                if dockerfile_path.exists() {
                    self.summary.dockerfile = StepStatus::Done;
                    frontend.report_step_status(
                        &ReadyStep::CheckDockerfile(dockerfile_path.clone()).into(),
                        StepStatus::Done,
                    );
                    ReadyPhase::BuildingBaseImage
                } else {
                    ReadyPhase::AwaitingDockerfileDecision
                }
            }
            ReadyPhase::AwaitingDockerfileDecision => {
                let configured_path = self
                    .session
                    .repo_config()
                    .dockerfile_path_or_default(&git_root);
                if frontend.ask_create_dockerfile(&configured_path)? {
                    ReadyPhase::CreatingDockerfile
                } else {
                    ReadyPhase::Failed(ReadyFailure {
                        phase: "AwaitingDockerfileDecision".into(),
                        message: format!("user declined to create {}", configured_path.display()),
                    })
                }
            }
            ReadyPhase::CreatingDockerfile => {
                let dockerfile_path = self
                    .session
                    .repo_config()
                    .dockerfile_path_or_default(&git_root);
                std::fs::write(&dockerfile_path, templates::project_dockerfile_dev())
                    .map_err(|e| EngineError::io(dockerfile_path.clone(), e))?;
                self.summary.dockerfile = StepStatus::Done;
                frontend.report_step_status(
                    &ReadyStep::CreateDockerfile(dockerfile_path.clone()).into(),
                    StepStatus::Done,
                );
                ReadyPhase::BuildingBaseImage
            }
            ReadyPhase::BuildingBaseImage => {
                // Issue 22: Docker daemon pre-check — soft failure allows
                // run_to_completion to surface a summary rather than aborting.
                if !self.runtime.is_available() {
                    let runtime = self.runtime.display_name();
                    let msg = format!(
                        "{runtime} is not available. Ensure {runtime} is running and retry."
                    );
                    self.summary.base_image = StepStatus::Failed(msg.clone());
                    frontend.report_step_status(
                        &ReadyStep::BuildBaseImage.into(),
                        StepStatus::Failed(msg),
                    );
                    // Bypass the `self.phase = next` assignment below to short-
                    // circuit straight to the next phase, but DO advance self.phase
                    // first — otherwise `run_to_completion` re-enters this branch
                    // forever (the docker-unavailable case is otherwise infinite).
                    self.phase = ReadyPhase::BuildingAgentImage;
                    return Ok(self.phase.clone());
                }

                let tag = project_image_tag(&git_root);
                // Rebuild when --build was passed or when the base image is
                // missing. Otherwise skip (`awman ready` is idempotent).
                let needs_build =
                    self.options.build || !self.runtime.image_exists(&tag).unwrap_or(false);
                if !needs_build {
                    self.summary.base_image = StepStatus::Done;
                    frontend
                        .report_step_status(&ReadyStep::BuildBaseImage.into(), StepStatus::Done);
                    ReadyPhase::BuildingAgentImage
                } else {
                    frontend
                        .report_step_status(&ReadyStep::BuildBaseImage.into(), StepStatus::Running);
                    let dockerfile_path = self
                        .session
                        .repo_config()
                        .dockerfile_path_or_default(&git_root);
                    let mut sink = |line: &str| {
                        frontend.report_step_status(&SetupStep::output(line), StepStatus::Running);
                    };
                    let result = self.runtime.build_image(
                        &tag,
                        &dockerfile_path,
                        &git_root,
                        self.options.no_cache,
                        &mut sink,
                    );
                    match result {
                        Ok(()) => {
                            self.summary.base_image = StepStatus::Done;
                            frontend.report_step_status(
                                &ReadyStep::BuildBaseImage.into(),
                                StepStatus::Done,
                            );
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            self.summary.base_image = StepStatus::Failed(msg.clone());
                            frontend.report_step_status(
                                &ReadyStep::BuildBaseImage.into(),
                                StepStatus::Failed(msg),
                            );
                        }
                    }
                    ReadyPhase::BuildingAgentImage
                }
            }
            ReadyPhase::BuildingAgentImage => {
                // Issue 22 (extended for WI 0078): if Docker isn't available
                // there's no point attempting to download an agent Dockerfile
                // from the network — the build would fail anyway. Mark as
                // failed and continue, so the setup task exits promptly in
                // sandboxed test environments.
                if !self.runtime.is_available() {
                    let msg = format!("{} is not available.", self.runtime.display_name());
                    self.summary.agent_image = StepStatus::Failed(msg.clone());
                    frontend.report_step_status(
                        &ReadyStep::BuildAgentImage.into(),
                        StepStatus::Failed(msg),
                    );
                    return Ok({
                        self.phase = ReadyPhase::CheckingNonDefaultAgents;
                        self.phase.clone()
                    });
                }
                let paths = RepoDockerfilePaths::new(&git_root);
                let agent_dockerfile = paths.agent_dockerfile(self.options.agent.as_str());
                let tag = agent_image_tag(&git_root, self.options.agent.as_str());
                let needs_build =
                    self.options.build || !self.runtime.image_exists(&tag).unwrap_or(false);
                if !needs_build {
                    self.summary.agent_image = StepStatus::Done;
                    frontend
                        .report_step_status(&ReadyStep::BuildAgentImage.into(), StepStatus::Done);
                    return Ok({
                        self.phase = ReadyPhase::CheckingNonDefaultAgents;
                        self.phase.clone()
                    });
                }
                frontend
                    .report_step_status(&ReadyStep::BuildAgentImage.into(), StepStatus::Running);
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
                            &ReadyStep::DownloadAgentDockerfile.into(),
                            StepStatus::Failed(msg),
                        );
                        // Continue but mark agent image not built.
                        return Ok({
                            self.phase = ReadyPhase::CheckingNonDefaultAgents;
                            self.phase.clone()
                        });
                    }
                }
                let mut sink = |line: &str| {
                    frontend.report_step_status(&SetupStep::output(line), StepStatus::Running);
                };
                let result = self.runtime.build_image(
                    &tag,
                    &agent_dockerfile,
                    &git_root,
                    self.options.no_cache,
                    &mut sink,
                );
                match result {
                    Ok(()) => {
                        self.summary.agent_image = StepStatus::Done;
                        frontend.report_step_status(
                            &ReadyStep::BuildAgentImage.into(),
                            StepStatus::Done,
                        );
                    }
                    Err(e) => {
                        self.summary.agent_image = StepStatus::Failed(e.to_string());
                        frontend.report_step_status(
                            &ReadyStep::BuildAgentImage.into(),
                            StepStatus::Failed(e.to_string()),
                        );
                    }
                }

                // ENG-1: When --build is set, also build all other agent images.
                if self.options.build {
                    let all_agents = paths.discover_agent_dockerfiles();
                    let default_agent = self.options.agent.as_str();
                    for (agent_name, agent_path) in &all_agents {
                        if agent_name == default_agent {
                            continue;
                        }
                        let other_tag = agent_image_tag(&git_root, agent_name);
                        frontend.report_step_status(
                            &ReadyStep::BuildAgentImageFor(agent_name.to_string()).into(),
                            StepStatus::Running,
                        );
                        let mut agent_sink = |line: &str| {
                            frontend
                                .report_step_status(&SetupStep::output(line), StepStatus::Running);
                        };
                        let agent_result = self.runtime.build_image(
                            &other_tag,
                            agent_path,
                            &git_root,
                            self.options.no_cache,
                            &mut agent_sink,
                        );
                        match agent_result {
                            Ok(()) => {
                                frontend.report_step_status(
                                    &ReadyStep::BuildAgentImageFor(agent_name.to_string()).into(),
                                    StepStatus::Done,
                                );
                            }
                            Err(e) => {
                                frontend.report_step_status(
                                    &ReadyStep::BuildAgentImageFor(agent_name.to_string()).into(),
                                    StepStatus::Failed(e.to_string()),
                                );
                            }
                        }
                    }
                }

                ReadyPhase::CheckingNonDefaultAgents
            }
            ReadyPhase::CheckingNonDefaultAgents => {
                let paths = RepoDockerfilePaths::new(&git_root);
                let all_agents = paths.discover_agent_dockerfiles();
                let default_agent = self.options.agent.as_str();

                let mut missing_agents: Vec<(String, String)> = Vec::new();
                let mut all_ok = true;
                let mut count = 0usize;
                for (agent_name, _agent_path) in &all_agents {
                    if agent_name == default_agent {
                        continue;
                    }
                    count += 1;
                    let other_tag = agent_image_tag(&git_root, agent_name);
                    if !self.runtime.image_exists(&other_tag).unwrap_or(false) {
                        all_ok = false;
                        missing_agents.push((agent_name.clone(), other_tag));
                    }
                }

                if count > 0 {
                    if all_ok {
                        // All non-default agents have valid images → single consolidated row.
                        frontend
                            .report_step_status(&ReadyStep::OtherAgents.into(), StepStatus::Done);
                        self.summary
                            .non_default_agent_images
                            .push(("Other agents".to_string(), StepStatus::Done));
                    } else {
                        // One consolidated "Missing images" row listing the
                        // affected agents — easier to scan than one row per
                        // agent in CLI/TUI/API output.
                        let names_csv = missing_agents
                            .iter()
                            .map(|(n, _)| n.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        let status = StepStatus::Warn(names_csv.clone());
                        frontend
                            .report_step_status(&ReadyStep::MissingImages.into(), status.clone());
                        self.summary
                            .non_default_agent_images
                            .push(("Missing images".to_string(), status));
                        frontend.write_message(crate::data::message::UserMessage {
                            level: crate::data::message::MessageLevel::Warning,
                            text: format!("Missing agent images: {names_csv}"),
                        });
                    }
                }

                ReadyPhase::CheckingLocalAgent
            }
            // SECURITY EXCEPTION: this is the ONE sanctioned host-side agent
            // execution in awman (see `aspec/architecture/security.md`). The
            // agent binary runs on the host with a hardcoded greeting — never
            // user input, repo content, or a working directory — to verify it
            // is installed and authenticated, and to force it to refresh its
            // auth token before credentials are mounted into containerized
            // agents. Do not add other host-side agent invocations.
            ReadyPhase::CheckingLocalAgent => {
                frontend
                    .report_step_status(&ReadyStep::CheckLocalAgent.into(), StepStatus::Running);
                let agent_name = self.options.agent.as_str();
                let ping = self.host_agent.ping(&self.options.agent).await;
                // The engine reports the result; the frontend draws the
                // transcript. The default impl writes the same two `>`/`<`
                // lines this arm used to compose (F-45).
                frontend.report_ping(&ping);
                match ping {
                    LocalAgentPingResult::Ok { .. } => {
                        self.summary.local_agent = StepStatus::Done;
                        frontend.report_step_status(
                            &ReadyStep::CheckLocalAgent.into(),
                            StepStatus::Done,
                        );
                    }
                    LocalAgentPingResult::Error => {
                        self.summary.local_agent =
                            StepStatus::Failed(format!("{agent_name}: error (check auth)"));
                        frontend.report_step_status(
                            &ReadyStep::CheckLocalAgent.into(),
                            StepStatus::Failed(format!("{agent_name}: error (check auth)")),
                        );
                    }
                    LocalAgentPingResult::NotInstalled => {
                        self.summary.local_agent =
                            StepStatus::Failed(format!("{agent_name}: not installed"));
                        frontend.report_step_status(
                            &ReadyStep::CheckLocalAgent.into(),
                            StepStatus::Failed(format!("{agent_name}: not installed")),
                        );
                    }
                    LocalAgentPingResult::CouldNotRun => {
                        self.summary.local_agent =
                            StepStatus::Failed(format!("{agent_name}: could not run"));
                        frontend.report_step_status(
                            &ReadyStep::CheckLocalAgent.into(),
                            StepStatus::Failed(format!("{agent_name}: could not run")),
                        );
                    }
                }
                let dockerfile_path = self
                    .session
                    .repo_config()
                    .dockerfile_path_or_default(&git_root);
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
                let dockerfile_path = self
                    .session
                    .repo_config()
                    .dockerfile_path_or_default(&git_root);
                if dockerfile_path.exists() {
                    let content = std::fs::read_to_string(&dockerfile_path).unwrap_or_default();
                    if !templates::dockerfile_matches_template(&content) {
                        frontend.write_message(crate::data::message::UserMessage {
                            level: crate::data::message::MessageLevel::Warning,
                            text:
                                "Dockerfile.dev has been customised; audit may overwrite changes."
                                    .into(),
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
                        non_interactive: self.options.non_interactive,
                        model: None,
                        env_passthrough: self.options.env_passthrough.clone(),
                        directory_overlays: vec![],
                        include_all_skills: false,
                        named_skills: vec![],
                        image_tag_override: None,
                        ..Default::default()
                    };
                    match self.agent_engine.build_options(
                        &self.session,
                        &self.options.agent,
                        &run_opts,
                    ) {
                        Err(e) => {
                            self.summary.audit = StepStatus::Failed(e.to_string());
                        }
                        Ok(options) => match ResolvedAgentOptions::container(options)
                            .and_then(|o| self.runtime.build(o))
                        {
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
                                                self.summary.audit = StepStatus::Failed(format!(
                                                    "audit exited with code {}",
                                                    exit.exit_code
                                                ));
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
                if matches!(self.summary.audit, StepStatus::Done) {
                    let dockerfile_path = self
                        .session
                        .repo_config()
                        .dockerfile_path_or_default(&git_root);
                    let post_hash = dockerfile_hash(&dockerfile_path);
                    let changed = match (self.pre_audit_dockerfile_hash, post_hash) {
                        (Some(pre), Some(post)) => pre != post,
                        // If we can't compute either hash, conservatively assume changed.
                        _ => true,
                    };
                    if changed {
                        frontend.report_step_status(
                            &ReadyStep::RebuildingAfterAudit.into(),
                            StepStatus::Running,
                        );
                        let tag = project_image_tag(&git_root);
                        let dockerfile_path_clone = dockerfile_path.clone();
                        let mut sink = |line: &str| {
                            frontend
                                .report_step_status(&SetupStep::output(line), StepStatus::Running);
                        };
                        let result = self.runtime.build_image(
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
                                    &ReadyStep::RebuildingAfterAudit.into(),
                                    StepStatus::Done,
                                );
                            }
                            Err(e) => {
                                let msg = e.to_string();
                                self.summary.base_image = StepStatus::Failed(msg.clone());
                                self.summary.image_rebuild = StepStatus::Failed(msg.clone());
                                frontend.report_step_status(
                                    &ReadyStep::RebuildingAfterAudit.into(),
                                    StepStatus::Failed(msg),
                                );
                            }
                        }

                        // Issue 9: Also rebuild agent images that layer FROM the project base.
                        let awman_dir = git_root.join(".awman");
                        if awman_dir.exists() {
                            if let Ok(entries) = std::fs::read_dir(&awman_dir) {
                                for entry in entries.flatten() {
                                    let name = entry.file_name();
                                    let name_str = name.to_string_lossy().to_string();
                                    if name_str.starts_with("Dockerfile.") {
                                        let agent =
                                            name_str.strip_prefix("Dockerfile.").unwrap_or("");
                                        if !agent.is_empty() {
                                            let agent_tag =
                                                crate::data::image_tags::agent_image_tag(
                                                    &git_root, agent,
                                                );
                                            let mut agent_sink = |line: &str| {
                                                frontend.report_step_status(
                                                    &SetupStep::output(line),
                                                    StepStatus::Running,
                                                );
                                            };
                                            let _ = self.runtime.build_image(
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
    use crate::data::message::{UserMessage, UserMessageSink};
    use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};
    use crate::data::step_status::StepStatus;
    use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentProgress, AgentStatus};
    use crate::engine::error::EngineError;
    use crate::engine::overlay::OverlayEngine;

    // ── Fake frontend ────────────────────────────────────────────────────────

    struct FakeReadyFrontend {
        create_dockerfile: bool,
        run_audit: bool,
        phases: Vec<ReadyPhase>,
        statuses: Vec<(String, StepStatus)>,
        /// Path passed to the most recent `ask_create_dockerfile` call.
        last_dockerfile_prompt_path: Option<std::path::PathBuf>,
    }

    struct FakeRuntimeFrontend;

    impl UserMessageSink for FakeRuntimeFrontend {
        fn write_message(&mut self, _msg: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    #[async_trait::async_trait]
    impl AgentFrontend for FakeRuntimeFrontend {
        fn report_status(&mut self, _status: AgentStatus) {}
        fn report_progress(&mut self, _progress: AgentProgress) {}
        fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
            let (stdout_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (stderr_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
            crate::engine::agent_runtime::frontend::AgentIo {
                stdout: stdout_tx,
                stderr: stderr_tx,
                stdin_tx,
                stdin_rx,
                resize: None,
                initial_size: None,
            }
        }
    }

    impl UserMessageSink for FakeReadyFrontend {
        fn write_message(&mut self, _msg: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl ReadyFrontend for FakeReadyFrontend {
        fn ask_create_dockerfile(
            &mut self,
            dockerfile_path: &std::path::Path,
        ) -> Result<bool, EngineError> {
            self.last_dockerfile_prompt_path = Some(dockerfile_path.to_path_buf());
            Ok(self.create_dockerfile)
        }

        fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
            Ok(self.run_audit)
        }

        fn report_phase(&mut self, phase: &ReadyPhase) {
            self.phases.push(phase.clone());
        }

        fn report_summary(&mut self, _summary: &ReadySummary) {}
    }

    impl crate::engine::agent::AgentImageFrontend for FakeReadyFrontend {
        fn report_step_status(&mut self, step: &SetupStep, status: StepStatus) {
            self.statuses.push((step.to_string(), status));
        }

        fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
            Box::new(FakeRuntimeFrontend)
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_engine_and_frontend(
        create_dockerfile: bool,
        run_audit: bool,
    ) -> (ReadyEngine, FakeReadyFrontend, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        // Pre-create .awman/Dockerfile.claude so the ready engine does not
        // attempt a network download during tests.
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM scratch\n").unwrap();
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
        let runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> =
            Arc::new(crate::engine::container::ContainerRuntime::docker());
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
            non_interactive: false,
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
            phases: Vec::new(),
            statuses: Vec::new(),
            last_dockerfile_prompt_path: None,
        };
        (engine, frontend, tmp)
    }

    // ── Tests ────────────────────────────────────────────────────────────────
    //
    // The host-ping tests moved to `host_agent.rs` with the code they test.

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
        let runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> =
            Arc::new(crate::engine::container::ContainerRuntime::docker());
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
            non_interactive: false,
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
        engine2.step(&mut frontend).await.unwrap(); // CreatingDockerfile → BuildingBaseImage
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

    // ─── WI-0086: configured dockerfile path in ready ─────────────────────────

    /// Build a ReadyEngine rooted at `git_root` with the agent Dockerfile
    /// pre-created so no network downloads happen during tests. Mirrors
    /// `make_engine_and_frontend` but lets callers stage their own
    /// `.awman/config.json` before the Session is opened (so RepoConfig is
    /// read from disk with the test's contents).
    fn make_engine_at(git_root: &std::path::Path) -> ReadyEngine {
        let awman_dir = git_root.join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM scratch\n").unwrap();
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
        let runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> =
            Arc::new(crate::engine::container::ContainerRuntime::docker());
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
            non_interactive: false,
            env_passthrough: None,
        };
        ReadyEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            runtime,
            agent_engine,
            options,
        )
    }

    #[tokio::test]
    async fn ready_with_configured_dockerfile_reports_configured_path_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // Configure a non-default dockerfile path, but do not create the file.
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join("config.json"),
            r#"{"dockerfile":"infra/Dockerfile.base"}"#,
        )
        .unwrap();

        let mut engine = make_engine_at(tmp.path());
        let mut frontend = FakeReadyFrontend {
            create_dockerfile: false, // decline so the engine fails fast
            run_audit: false,
            phases: Vec::new(),
            statuses: Vec::new(),
            last_dockerfile_prompt_path: None,
        };

        engine.step(&mut frontend).await.unwrap(); // Preflight
        engine.step(&mut frontend).await.unwrap(); // AwaitingDockerfileDecision

        let prompted = frontend
            .last_dockerfile_prompt_path
            .as_ref()
            .expect("ask_create_dockerfile must be called when the configured path is missing");
        assert_eq!(
            prompted,
            &tmp.path().join("infra/Dockerfile.base"),
            "ready must surface the configured dockerfile path, not the default Dockerfile.dev"
        );
    }

    #[tokio::test]
    async fn ready_with_configured_dockerfile_skips_decision_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join("config.json"),
            r#"{"dockerfile":"infra/Dockerfile.base"}"#,
        )
        .unwrap();
        // Create the configured Dockerfile so Preflight short-circuits past
        // AwaitingDockerfileDecision.
        let infra_dir = tmp.path().join("infra");
        std::fs::create_dir_all(&infra_dir).unwrap();
        std::fs::write(infra_dir.join("Dockerfile.base"), "FROM scratch\n").unwrap();

        let mut engine = make_engine_at(tmp.path());
        let mut frontend = FakeReadyFrontend {
            create_dockerfile: false, // sentinel — if the prompt fires the engine would fail
            run_audit: false,
            phases: Vec::new(),
            statuses: Vec::new(),
            last_dockerfile_prompt_path: None,
        };

        engine.step(&mut frontend).await.unwrap(); // Preflight → BuildingBaseImage

        assert!(
            frontend.last_dockerfile_prompt_path.is_none(),
            "ready must not prompt for Dockerfile creation when the configured path exists; \
             got: {:?}",
            frontend.last_dockerfile_prompt_path
        );
        assert!(
            matches!(engine.summary().dockerfile, StepStatus::Done),
            "dockerfile step must be Done when the configured path exists; got: {:?}",
            engine.summary().dockerfile
        );
        assert_eq!(
            engine.phase(),
            &ReadyPhase::BuildingBaseImage,
            "engine must advance directly to BuildingBaseImage after Preflight"
        );
    }

    // ── Kit-declarative (sandbox-tier) ready flow (F-40b) ────────────────
    //
    // `ReadyEngine` is runtime-agnostic: given a kit-declarative runtime it
    // skips the whole container phase machine in one step. Before F-40b this
    // branch lived in Layer 2 (`commands/ready.rs`) and keyed on
    // `engines.sandbox_runtime.is_some()`.

    /// A kit-declarative runtime that records the `ready_agent` call and
    /// refuses every image-store operation, the way the sandbox tier does.
    struct FakeKitRuntime {
        caps: crate::engine::agent_runtime::Capabilities,
        ready_calls:
            std::sync::Mutex<Vec<(String, crate::engine::agent_runtime::ReadyAgentOptions)>>,
    }

    impl FakeKitRuntime {
        fn new() -> Self {
            let mut caps = crate::engine::container::ContainerRuntime::docker()
                .capabilities()
                .clone();
            caps.kit_declarative = true;
            caps.has_image_store = false;
            Self {
                caps,
                ready_calls: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl crate::engine::agent_runtime::AgentRuntimeEngine for FakeKitRuntime {
        fn runtime_name(&self) -> &'static str {
            "fake-kit"
        }
        fn display_name(&self) -> &'static str {
            "Fake Kit Runtime"
        }
        fn capabilities(&self) -> &crate::engine::agent_runtime::Capabilities {
            &self.caps
        }
        fn is_available(&self) -> bool {
            true
        }
        fn build(
            &self,
            _: crate::engine::agent_runtime::ResolvedAgentOptions,
        ) -> Result<Box<dyn crate::engine::agent_runtime::AgentInstance>, EngineError> {
            unimplemented!("FakeKitRuntime never launches an agent")
        }
        fn list_running(
            &self,
            _: &crate::data::session::Session,
        ) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
            Ok(vec![])
        }
        fn list_running_all(&self) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
            Ok(vec![])
        }
        fn stats(
            &self,
            _: &crate::data::session::AgentHandle,
        ) -> Result<crate::engine::agent_runtime::AgentStats, EngineError> {
            Ok(crate::engine::agent_runtime::AgentStats {
                name: String::new(),
                cpu_percent: 0.0,
                memory_mb: 0.0,
            })
        }
        fn stop(&self, _: &crate::data::session::AgentHandle) -> Result<(), EngineError> {
            Ok(())
        }
        fn exec_args(&self, _: &str, _: &str, _: &[&str], _: &[(&str, &str)]) -> Vec<String> {
            vec![]
        }
        fn attach(
            &self,
            _: &crate::data::session::AgentHandle,
        ) -> Result<Box<dyn crate::engine::agent_runtime::AgentInstance>, EngineError> {
            unimplemented!("FakeKitRuntime never attaches")
        }
        fn list_running_with_name_prefix(
            &self,
            _: &str,
        ) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
            Ok(vec![])
        }
        fn cli_binary(&self) -> &'static str {
            "fake-kit"
        }
        fn ready_agent(
            &self,
            agent: &str,
            opts: crate::engine::agent_runtime::ReadyAgentOptions,
            _sink: &mut dyn crate::data::message::UserMessageSink,
        ) -> Result<(), EngineError> {
            self.ready_calls
                .lock()
                .unwrap()
                .push((agent.to_string(), opts));
            Ok(())
        }
        fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
            Err(EngineError::UnsupportedOnRuntime {
                runtime: "fake-kit",
                operation: "local image probe",
            })
        }
        fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
            Err(EngineError::UnsupportedOnRuntime {
                runtime: "fake-kit",
                operation: "image HOME lookup",
            })
        }
        fn build_image(
            &self,
            _: &str,
            _: &std::path::Path,
            _: &std::path::Path,
            _: bool,
            _: &mut dyn FnMut(&str),
        ) -> Result<(), EngineError> {
            Err(EngineError::UnsupportedOnRuntime {
                runtime: "fake-kit",
                operation: "image build",
            })
        }
    }

    #[tokio::test]
    async fn ready_on_a_kit_declarative_runtime_applies_the_kit_and_skips_image_phases() {
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
        let kit = Arc::new(FakeKitRuntime::new());
        let runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> = kit.clone();
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let mut engine = ReadyEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            runtime,
            agent_engine,
            ReadyEngineOptions {
                agent: AgentName::new("claude").unwrap(),
                refresh: false,
                build: true,
                no_cache: true,
                allow_docker: false,
                non_interactive: true,
                env_passthrough: None,
            },
        );
        let mut frontend = FakeReadyFrontend {
            create_dockerfile: false,
            run_audit: false,
            phases: Vec::new(),
            statuses: Vec::new(),
            last_dockerfile_prompt_path: None,
        };

        let phase = engine.step(&mut frontend).await.unwrap();

        assert_eq!(
            phase,
            ReadyPhase::Complete,
            "one step, straight to Complete"
        );
        let calls = kit.ready_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "the kit is applied exactly once");
        assert_eq!(calls[0].0, "claude");
        assert!(calls[0].1.no_cache, "no_cache must reach the runtime");

        let summary = engine.summary();
        assert_eq!(summary.agent_image, StepStatus::Done);
        // Every container-tier step is not applicable, and says so rather
        // than reporting a Done it never did.
        assert_eq!(summary.dockerfile, StepStatus::Skipped);
        assert_eq!(summary.base_image, StepStatus::Skipped);
        assert_eq!(summary.local_agent, StepStatus::Skipped);
        assert_eq!(summary.audit, StepStatus::Skipped);
    }
}
