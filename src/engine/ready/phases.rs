use super::*;

impl ReadyEngine {
    /// Whether the runtime imports prepared images instead of building them.
    fn imports_images(&self) -> bool {
        self.runtime.capabilities().image_acquisition == ImageAcquisition::Import
    }

    /// The phase after the Dockerfile checks: build or import.
    fn image_phase(&self) -> ReadyPhase {
        if self.imports_images() {
            ReadyPhase::ImportingBaseImage
        } else {
            ReadyPhase::BuildingBaseImage
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

        self.cancel.check()?;
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

                let dockerfile_path = self.preparer.project_dockerfile().to_path_buf();
                if dockerfile_path.exists() {
                    self.summary.dockerfile = StepStatus::Done;
                    frontend.report_step_status(
                        &ReadyStep::CheckDockerfile(dockerfile_path.clone()).into(),
                        StepStatus::Done,
                    );
                    self.image_phase()
                } else {
                    ReadyPhase::AwaitingDockerfileDecision
                }
            }
            ReadyPhase::AwaitingDockerfileDecision => {
                let configured_path = self.preparer.project_dockerfile().to_path_buf();
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
                let dockerfile_path = self.preparer.project_dockerfile().to_path_buf();
                std::fs::write(&dockerfile_path, templates::project_dockerfile_dev())
                    .map_err(|e| EngineError::io(dockerfile_path.clone(), e))?;
                self.summary.dockerfile = StepStatus::Done;
                frontend.report_step_status(
                    &ReadyStep::CreateDockerfile(dockerfile_path.clone()).into(),
                    StepStatus::Done,
                );
                self.image_phase()
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
                    let dockerfile_path = self.preparer.project_dockerfile().to_path_buf();
                    let mut sink = |line: &str| {
                        frontend.report_step_status(&SetupStep::output(line), StepStatus::Running);
                    };
                    let result = self.runtime.build_image(
                        &crate::engine::container::BuildRequest::runtime(
                            &tag,
                            &dockerfile_path,
                            &git_root,
                            self.options.no_cache,
                        ),
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
                    &crate::engine::container::BuildRequest::runtime(
                        &tag,
                        &agent_dockerfile,
                        &git_root,
                        self.options.no_cache,
                    ),
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
                            &crate::engine::container::BuildRequest::runtime(
                                &other_tag,
                                agent_path,
                                &git_root,
                                self.options.no_cache,
                            ),
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
                let mut missing_images = Vec::new();
                let imports = self.imports_images();
                let mut all_ok = true;
                let mut count = 0usize;
                for (agent_name, _agent_path) in &all_agents {
                    if agent_name == default_agent {
                        continue;
                    }
                    count += 1;
                    let other_tag = agent_image_tag(&git_root, agent_name);
                    // An image whose build or import failed in this run is
                    // not "done" just because an older one is installed.
                    let failed_now = self
                        .summary
                        .image_results
                        .iter()
                        .any(|r| r.tag == other_tag && r.has_failure());
                    let missing = if failed_now {
                        missing_agents.push((format!("{agent_name} (failed)"), other_tag.clone()));
                        true
                    } else if !self.runtime.image_exists(&other_tag).unwrap_or(false) {
                        missing_agents.push((agent_name.clone(), other_tag.clone()));
                        true
                    } else {
                        false
                    };
                    if missing {
                        all_ok = false;
                        // Per image: an importing runtime may import one image,
                        // build another into its source store, and have no
                        // source for a third.
                        let plan = if imports {
                            self.preparer.missing_image_plan(&other_tag, agent_name)
                        } else {
                            crate::data::ready_image::ReadyImagePlan::Build
                        };
                        missing_images.push(crate::data::ready_image::MissingAgentImage {
                            agent: agent_name.clone(),
                            plan,
                        });
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
                        self.summary.missing_images = missing_images;
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
                let dockerfile_path = self.preparer.project_dockerfile().to_path_buf();
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
                let dockerfile_path = self.preparer.project_dockerfile().to_path_buf();
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
                    let dockerfile_path = self.preparer.project_dockerfile().to_path_buf();
                    let post_hash = dockerfile_hash(&dockerfile_path);
                    let changed = match (self.pre_audit_dockerfile_hash, post_hash) {
                        (Some(pre), Some(post)) => pre != post,
                        // If we can't compute either hash, conservatively assume changed.
                        _ => true,
                    };
                    if changed && self.imports_images() && self.preparer.can_rebuild_default() {
                        frontend.report_step_status(
                            &ReadyStep::RebuildingAfterAudit.into(),
                            StepStatus::Running,
                        );
                        let old_build = self.options.build;
                        self.options.build = true;
                        let old_policy = self.image_policy;
                        self.image_policy = old_policy.audit_rebuild();
                        self.preparer.reset_bases();
                        self.preparer.prevalidate_build_targets(self.image_policy);
                        let prior = self.summary.image_results.len();
                        self.import_default_agent_image(frontend, &git_root).await;
                        self.import_other_agent_images(frontend, &git_root).await;
                        self.options.build = old_build;
                        self.image_policy = old_policy;
                        let failure = self.summary.image_results[prior..]
                            .iter()
                            .find(|r| r.has_failure())
                            .map(|r| format!("audit rebuild of {} failed", r.tag))
                            .or_else(|| match &self.summary.agent_image {
                                StepStatus::Failed(message) => {
                                    Some(format!("audit rebuild failed: {message}"))
                                }
                                _ => None,
                            });
                        self.summary.image_rebuild = match failure {
                            Some(message) => StepStatus::Failed(message),
                            None => StepStatus::Done,
                        };
                        frontend.report_step_status(
                            &ReadyStep::RebuildingAfterAudit.into(),
                            self.summary.image_rebuild.clone(),
                        );
                    } else if changed && self.imports_images() {
                        self.summary.image_rebuild = StepStatus::Warn(
                            "Dockerfile changed: rebuild the image externally, then run \
                             `awman ready --build` to re-import it"
                                .into(),
                        );
                    } else if changed {
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
                            &crate::engine::container::BuildRequest::runtime(
                                &tag,
                                &dockerfile_path_clone,
                                &git_root,
                                self.options.no_cache,
                            ),
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
                                                &crate::engine::container::BuildRequest::runtime(
                                                    &agent_tag,
                                                    &entry.path(),
                                                    &git_root,
                                                    self.options.no_cache,
                                                ),
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
            // Import-acquisition phases (`ImageAcquisition::Import`). The
            // opted-in local builder prepares images in their source stores;
            // execution remains in the importing runtime.
            //
            // The project image (`Dockerfile.dev`) is only a build input for
            // the agent images; an importing runtime runs agent images alone,
            // so there is nothing to import for it.
            ReadyPhase::ImportingBaseImage => {
                self.summary.base_image = StepStatus::Skipped;
                ReadyPhase::ImportingAgentImage
            }
            ReadyPhase::ImportingAgentImage => {
                self.preparer.prevalidate_build_targets(self.image_policy);
                self.import_default_agent_image(frontend, &git_root).await;
                // The import counterpart of ENG-1: `--build` also acquires
                // every other agent's image.
                if self.options.build && self.runtime.availability().is_ok() {
                    self.import_other_agent_images(frontend, &git_root).await;
                }
                ReadyPhase::CheckingNonDefaultAgents
            }
            ReadyPhase::Complete | ReadyPhase::Failed(_) => self.phase.clone(),
        };
        self.phase = next.clone();
        if matches!(self.phase, ReadyPhase::Complete | ReadyPhase::Failed(_)) {
            frontend.report_summary(&self.summary);
        }
        Ok(next)
    }

    /// `ImportingAgentImage`: import the default agent's image and record the
    /// outcome in the summary.
    pub(super) async fn import_default_agent_image(
        &mut self,
        frontend: &mut dyn ReadyFrontend,
        git_root: &std::path::Path,
    ) {
        let agent = self.options.agent.as_str().to_string();
        match self.prepare_agent_image(frontend, git_root, &agent).await {
            Ok(_) => {
                self.summary.agent_image = StepStatus::Done;
                self.summary.image_source = Some(StepStatus::Done);
            }
            Err(msg) => self.record_import_failure(msg),
        }
    }

    pub(super) async fn prepare_agent_image(
        &mut self,
        frontend: &mut dyn ReadyFrontend,
        git_root: &std::path::Path,
        agent: &str,
    ) -> Result<StepStatus, String> {
        let tag = crate::data::image_tags::agent_image_tag(git_root, agent);
        let result = self
            .preparer
            .prepare(
                &crate::engine::image_prep::AgentImageRequest {
                    tag: &tag,
                    agent,
                    policy: self.image_policy,
                },
                frontend,
            )
            .await
            .map_err(|e| e.to_string())?;
        self.summary
            .base_image_results
            .extend(self.preparer.take_base_results());
        let error = [&result.build, &result.import]
            .into_iter()
            .find_map(|status| match status {
                StepStatus::Failed(text) => Some(text.clone()),
                _ => None,
            });
        let status = result.import.clone();
        if !matches!(status, StepStatus::Warn(_)) && result.source_kind.is_some()
            || result.has_failure()
        {
            self.summary.image_results.push(result);
        }
        match error {
            Some(error) => Err(error),
            None => Ok(status),
        }
    }
    /// `ready --build` on an importing runtime: acquire every non-default
    /// agent's image too, one row per agent. The agents are the ones with a
    /// `.awman/Dockerfile.<agent>` — the same set `CheckingNonDefaultAgents`
    /// reports as missing — so the tip it prints leads here.
    async fn import_other_agent_images(
        &mut self,
        frontend: &mut dyn ReadyFrontend,
        git_root: &std::path::Path,
    ) {
        use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;

        let default_agent = self.options.agent.as_str().to_string();
        let others = RepoDockerfilePaths::new(git_root).discover_agent_dockerfiles();
        for (agent, _) in &others {
            if *agent == default_agent {
                continue;
            }
            if self.cancel.check().is_err() {
                return;
            }
            let step: SetupStep = ReadyStep::ImportAgentImageFor(agent.clone()).into();
            frontend.report_step_status(&step, StepStatus::Running);

            let status = match self.prepare_agent_image(frontend, git_root, agent).await {
                Ok(status) => status,
                Err(msg) => StepStatus::Failed(msg),
            };
            frontend.report_step_status(&step, status);
        }
    }

    fn record_import_failure(&mut self, msg: String) {
        self.summary.agent_image = StepStatus::Failed(msg.clone());
        self.summary.image_source = Some(StepStatus::Failed(msg));
    }

    /// Drive to completion: advance phases in a loop until terminal.
    pub async fn run_to_completion(
        &mut self,
        frontend: &mut dyn ReadyFrontend,
    ) -> Result<ReadySummary, EngineError> {
        loop {
            let next = self.step(frontend).await?;
            self.cancel.check()?;
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
