//! The Layer 2 side of workflow execution: the per-container frontend proxy
//! the engine drives, and the factory that turns a workflow step into a
//! prepared agent run.
//!
//! Split out of `commands/exec_workflow.rs` by WI 0114 F-51. A child module
//! of `exec_workflow`, so it reaches that module's private items unchanged.

use super::*;
use crate::data::config::effective::EffectiveConfig;

fn workflow_step_stuck_timeout() -> std::time::Duration {
    // Workflow `--yolo` gives Stuck its own lifecycle meaning in the engine.
    // The advisory inactivity setting must not change that countdown.
    crate::engine::container::timing::DEFAULT_STUCK_TIMEOUT
}

// ─── AgentFrontendProxy ──────────────────────────────────────────────────
//
// Passed to `AgentInstance::run_with_frontend`. Unlike the deleted
// `WorkflowProxy`, this is not a pure forwarder and the blanket
// `AgentFrontend for Arc<Mutex<Box<_>>>` impl cannot replace it: it owns
// per-container state (the I/O pairing below and the ACP PTY suppression in
// `take_io`). Everything that *is* pure forwarding goes through the blanket
// impl on `self.frontend`.

pub(crate) struct AgentFrontendProxy {
    frontend: SharedWorkflowFrontend,
    acp: bool,
    /// Each workflow container must receive the I/O sink paired with its own
    /// `Running { container_name }` callback. Keeping the taken I/O on this
    /// per-container proxy prevents parallel workflow steps from consuming a
    /// different step's per-container log file.
    io: Option<crate::engine::agent_runtime::frontend::AgentIo>,
    /// Whether this container's I/O has been taken from the frontend, by
    /// either path. Runtimes differ in order: Docker and Apple report
    /// `Running` before `take_io`, the builtin runtime takes its I/O first.
    /// A second take would make the TUI build fresh channels and move the
    /// keyboard onto them while the agent still reads the first set.
    io_taken: bool,
    output_format: crate::data::agent_output_format::AgentOutputFormat,
    /// This container's event attribution (WI 0137 §5), lent to the shared
    /// frontend for the duration of every forwarded call. The container
    /// name is filled in here when the runtime reports `Running`.
    source: Option<crate::engine::workflow::frontend::ContainerSource>,
}

impl AgentFrontendProxy {
    pub(crate) fn new(frontend: SharedWorkflowFrontend, acp: bool) -> Self {
        Self {
            frontend,
            acp,
            io: None,
            io_taken: false,
            output_format: Default::default(),
            source: None,
        }
    }

    /// Attribute this container's events to `source`.
    pub(crate) fn with_source(mut self, source: crate::data::execution_event::EventSource) -> Self {
        self.source = Some(std::sync::Arc::new(std::sync::Mutex::new(source)));
        self
    }

    /// Run `operation` on the shared frontend with this container's source
    /// set, under one lock so no other container's call sees it.
    fn with_attributed<T>(
        &self,
        operation: impl FnOnce(&mut dyn ExecWorkflowCommandFrontend) -> T,
    ) -> T {
        let mut frontend = self.frontend.lock().unwrap();
        attributed(&self.source, frontend.as_mut(), operation)
    }
}

fn attributed<T>(
    source: &Option<crate::engine::workflow::frontend::ContainerSource>,
    frontend: &mut dyn ExecWorkflowCommandFrontend,
    operation: impl FnOnce(&mut dyn ExecWorkflowCommandFrontend) -> T,
) -> T {
    if source.is_none() {
        return operation(frontend);
    }
    frontend.set_container_source(source.clone());
    let result = operation(&mut *frontend);
    frontend.set_container_source(None);
    result
}

#[async_trait]
impl AgentFrontend for AgentFrontendProxy {
    fn translated_output(&mut self, item: crate::engine::agent_output::Translated) -> bool {
        self.with_attributed(|f| f.translated_output(item))
    }
    fn terminal_source(&self) -> Option<crate::data::execution_event::EventSource> {
        self.source
            .as_ref()
            .and_then(|source| source.lock().ok().map(|s| s.clone()))
    }
    fn report_status(&mut self, status: crate::engine::agent_runtime::frontend::AgentStatus) {
        let is_running = matches!(
            &status,
            crate::engine::agent_runtime::frontend::AgentStatus::Running { .. }
        );
        if let (
            Some(source),
            crate::engine::agent_runtime::frontend::AgentStatus::Running { container_name },
        ) = (&self.source, &status)
        {
            source.lock().unwrap().container = Some(container_name.clone());
        }
        // One lock for both calls: a parallel step must not slip in between
        // the `Running` report and the matching `take_io`.
        let mut frontend = self.frontend.lock().unwrap();
        let take_io = is_running && !self.io_taken;
        let io = attributed(&self.source, frontend.as_mut(), |frontend| {
            frontend.report_status(status);
            take_io.then(|| {
                frontend.set_agent_output_format(self.output_format);
                frontend.take_io()
            })
        });
        if take_io {
            self.io = io;
            self.io_taken = true;
        }
    }

    fn report_progress(&mut self, progress: crate::engine::agent_runtime::frontend::AgentProgress) {
        self.with_attributed(|frontend| frontend.report_progress(progress));
    }

    fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
        let mut io = self.io.take().unwrap_or_else(|| {
            self.with_attributed(|frontend| {
                frontend.set_agent_output_format(self.output_format);
                frontend.take_io()
            })
        });
        self.io_taken = true;
        if self.acp {
            // ACP framing is line-delimited JSON, never a PTY stream.
            io.initial_size = None;
            io.resize = None;
        }
        io
    }

    fn set_agent_output_format(
        &mut self,
        format: crate::data::agent_output_format::AgentOutputFormat,
    ) {
        // Lend the protocol atomically with take_io, just like attribution.
        self.output_format = format;
    }

    fn grace_timeout(&self) -> std::time::Duration {
        self.frontend.with(|frontend| frontend.grace_timeout())
    }
    fn startup_timing(&self) -> crate::data::automation::StartupTiming {
        self.frontend.with(|frontend| frontend.startup_timing())
    }

    fn stuck_timeout(&self) -> std::time::Duration {
        workflow_step_stuck_timeout()
    }
}

impl UserMessageSink for AgentFrontendProxy {
    fn write_message(&mut self, msg: UserMessage) {
        self.with_attributed(|frontend| frontend.write_message(msg));
    }

    fn replay_queued(&mut self) {
        self.frontend.replay_queued();
    }
}

// ─── CommandLayerFactory ─────────────────────────────────────────────────────
//
// Implements `AgentExecutionFactory` for the workflow engine. Builds a
// container instance from per-step parameters + command flags, then binds a
// `AgentFrontendProxy` to it via `run_with_frontend`.
//
// Workflow prerequisite credentials (WI 0149) are deliberately not a field
// and never passed in: this factory launches main agent steps, parallel
// groups and `on_failure` remediation agents (`launch_on_failure_agent`),
// none of which may receive the user's SSH key or token. Those reach only
// setup/teardown containers, through `apply_grant_overlays` in `prepare.rs`.
// The one grant every container shares is the run's fixed commit identity
// (`grant_env`, from `agent_container_env`), the same value setup/teardown
// containers get from `prerequisite_grant_overlays`.

pub(crate) struct CommandLayerFactory {
    pub(crate) settings_profile: Option<crate::data::settings_profile::SettingsProfileName>,
    pub(crate) profiles: std::sync::Mutex<
        HashMap<String, crate::command::commands::launch_policy::settings_profile::ProfileSession>,
    >,

    pub(crate) shared: SharedWorkflowFrontend,
    pub(crate) engines: Engines,
    pub(crate) flags: Arc<ExecWorkflowCommandFlags>,
    pub(crate) cli_typed_overlays: Vec<TypedOverlay>,
    pub(crate) work_item_context: Option<WorkItemContext>,
    /// Resolver-authored private input; never promote a user directory here.
    pub(crate) work_item_overlay: Option<TypedOverlay>,
    /// The original repository git root (not the worktree). Used for image tag
    /// derivation so worktree-based runs use the correct project image.
    pub(crate) image_git_root: PathBuf,
    /// Workflow-level overlays applied to every step.
    pub(crate) workflow_overlays: Option<Vec<String>>,
    /// squad identity to stamp on every step container, when this is an
    /// squad-generated workflow. `None` for an ordinary `exec workflow`.
    pub(crate) squad_identity: Option<crate::engine::squad::launcher::SquadContainerIdentity>,
    /// The squad task's durable workspace, overriding the `context(workflow)`
    /// host directory for every step. `None` for an ordinary `exec workflow`.
    pub(crate) task_workspace: Option<PathBuf>,
    /// Fixed by workflow pre-flight before any step can launch.
    pub(crate) launch_modes: Arc<HashMap<String, crate::data::config::agents::LaunchMode>>,
    /// Resolves each step's `agentFlags`; see [`workflow_agent_flags`].
    pub(crate) agent_flags: EffectiveConfig,
    /// Mounted read-write at `/awman/output` into every step container
    /// (WI 0137 §3). `None` outside the API.
    pub(crate) output_dir: Option<PathBuf>,
    /// The command-level `--read-only-workspace` (WI 0152). A step is
    /// read-only when this, its own `read_only_workspace` or a read-only
    /// lease engine says so.
    pub(crate) workspace_access: crate::data::workspace::WorkspaceAccess,
    /// `GIT_AUTHOR_*`/`GIT_COMMITTER_*` for the run's fixed identity, set on
    /// every agent container this factory launches. Empty without one.
    pub(crate) grant_env: Vec<(String, String)>,
}

/// The `agentFlags` resolver for every agent a workflow run launches.
///
/// Built from the *invoking* session, never the re-rooted worktree session:
/// that one re-reads `.awman/config.json` from the worktree checkout (which
/// lacks uncommitted or post-branch edits) and opens with an empty env
/// snapshot (so a relocated global config is missed). `--agent-flag` targets
/// `target_agent` — the workflow's default agent — falling back to the
/// invoking session's configured agent when `None`.
pub(crate) fn workflow_agent_flags(
    invoking: &Session,
    target_agent: Option<String>,
    cli: Option<Vec<String>>,
) -> EffectiveConfig {
    invoking
        .effective_config()
        .with_flags(crate::data::config::FlagConfig {
            agent: target_agent,
            agent_flags: cli,
            ..Default::default()
        })
}

/// A workflow launch must never wait indefinitely for a host credential
/// refresh. The monitor applies this timeout internally; this helper also
/// keeps the synchronous `AgentExecutionFactory` boundary free of runtime
/// assumptions by driving the wait on a short-lived current-thread runtime.
pub(crate) const CREDENTIAL_REFRESH_WAIT: Duration = Duration::from_secs(30);

pub(crate) fn refresh_credential_blocking(
    monitor: Arc<crate::engine::credential_refresh::CredentialRefreshMonitor>,
    agent: crate::data::session::AgentName,
    checkout_roots: Vec<PathBuf>,
) -> RefreshOutcome {
    monitor.add_checkout_roots(checkout_roots);
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| ())?;
                Ok::<_, ()>(runtime.block_on(monitor.refresh_now(&agent, CREDENTIAL_REFRESH_WAIT)))
            })
            .join()
            .ok()
            .and_then(Result::ok)
            .unwrap_or_else(|| RefreshOutcome::Stale {
                remediation: "credential refresh worker stopped unexpectedly".to_string(),
            })
    })
}

pub(crate) fn refresh_warning(outcome: &RefreshOutcome) -> Option<String> {
    match outcome {
        RefreshOutcome::Stale { remediation } => {
            Some(format!("credential refresh did not advance; {remediation}"))
        }
        RefreshOutcome::Unavailable { reason } => {
            Some(format!("credential refresh unavailable: {reason}"))
        }
        RefreshOutcome::NotNeeded { .. } | RefreshOutcome::Refreshed { .. } => None,
    }
}

impl CommandLayerFactory {
    fn execution_with_prompt(
        &self,
        step: &WorkflowStep,
        session: &Session,
        runtime: &WorkflowRuntimeContext,
        substitution: crate::data::workflow_prompt_template::Substitution,
    ) -> Result<crate::engine::agent_runtime::execution::AgentExecution, EngineError> {
        // Read-only workspace (WI 0152): the step's own field, the command
        // flag and a read-only lease engine combine; read-only always wins.
        let workspace_access = crate::command::commands::workspace_access::effective_access(
            step.workspace_access(self.workspace_access),
            &self.engines.agent_engine,
        );
        let launch_engine = crate::command::commands::workspace_access::agent_engine_for(
            workspace_access,
            &self.engines.agent_engine,
        );
        // Compute per-step overlays by merging config/env/CLI with step-level overlays.
        let collected = session
            .effective_config()
            .collected_overlays(
                self.cli_typed_overlays.clone(),
                self.workflow_overlays.as_deref(),
                step.overlays.as_deref(),
            )
            .map_err(|e| EngineError::Other(format!("overlay collection failed: {e}")))?;

        // Resolve context overlays.
        let (mut context_overlays, system_prompt) = {
            let mut guard = self.shared.lock().unwrap();
            LaunchPolicy::for_session(session)
                .with_git(&self.engines.git_engine)
                .resolve_context_overlays(
                    &collected.context_overlays,
                    &runtime.step_agent,
                    Some(runtime.workflow_invocation_id),
                    runtime.workflow_step_info.as_ref(),
                    guard.as_mut(),
                )
                .map_err(|e| {
                    EngineError::Other(format!("context overlay resolution failed: {e}"))
                })?
        };

        // For a squad run, the workflow-scope context directory *is* the task's
        // durable workspace: the same directory the evaluation leader saw, at
        // the same container path, so a task's persistent files are reachable
        // from its workflow too. Retarget the host side of the workflow-scope
        // overlay rather than adding a second mount at the same container path,
        // which is exactly the collision the overlay engine refuses.
        if let Some(workspace) = &self.task_workspace {
            match context_overlays
                .iter_mut()
                .find(|o| o.scope == crate::engine::overlay::ContextScope::Workflow)
            {
                Some(existing) => existing.host_path = workspace.clone(),
                None => context_overlays.push(crate::engine::overlay::ContextOverlay {
                    scope: crate::engine::overlay::ContextScope::Workflow,
                    host_path: workspace.clone(),
                    container_path: std::path::PathBuf::from(
                        crate::command::commands::squad::evaluation::TASK_DIR_CONTAINER_PATH,
                    ),
                    permission: crate::engine::container::options::OverlayPermission::ReadWrite,
                }),
            }
        }

        if let Some(overlay) = context_overlays
            .iter()
            .find(|overlay| overlay.scope == crate::engine::overlay::ContextScope::Workflow)
        {
            self.shared
                .lock()
                .unwrap()
                .report_workflow_context_path(&overlay.host_path);
        }

        // Use the original repo root for image tag derivation so worktree-
        // based runs resolve the correct image for both the Image option AND
        // for image_home_dir inspection (which determines overlay mount paths).
        // Images pinned on the session (a worker lease, or a squad directory
        // task's prepared run, WI 0105) are what the engine launches, so
        // inspect that tag rather than one derived from the root.
        let correct_tag = session
            .lease_images()
            .and_then(|images| images.agents.get(runtime.step_agent.as_str()))
            .cloned()
            .unwrap_or_else(|| {
                crate::data::image_tags::agent_image_tag(
                    &self.image_git_root,
                    runtime.step_agent.as_str(),
                )
            });
        // Every step shares the command's one outputs directory (WI 0137 §3).
        let directory_overlays = collected.directories;
        let mut command_mounts = Vec::new();
        if let Some(TypedOverlay::Directory(directory)) = &self.work_item_overlay {
            command_mounts.push(directory.clone());
        }
        let outputs_dir = self
            .output_dir
            .as_ref()
            .filter(|_| self.engines.runtime.capabilities().arbitrary_host_mounts);
        let system_prompt = match outputs_dir {
            Some(dir) => {
                command_mounts.push(crate::data::fs::command_outputs::output_dir_overlay(dir));
                Some(crate::engine::context_prompt::with_outputs_section(
                    system_prompt,
                    crate::engine::context_prompt::OutputsPromptRole::Step(&step.name),
                ))
            }
            None => system_prompt,
        };
        // The read-only section goes last, after the outputs section.
        let system_prompt = crate::command::commands::workspace_access::system_prompt_for(
            workspace_access,
            system_prompt,
            outputs_dir.is_some(),
            &context_overlays,
        );
        let mut run_opts = AgentRunOptions {
            native_session: None,
            profile_state: None,
            automation_channel: None,
            command_secrets: self.engines.command_secrets(),
            // --plan conflicts with an explicit --yolo/--auto in the catalogue and
            // suppresses an inherited one (WI 0070).
            yolo: (self.flags.yolo && !self.flags.plan).then_some(YoloMode::Enabled),
            yolo_authorized: self.flags.yolo_authorized,
            auto: (self.flags.auto && !self.flags.plan).then_some(AutoMode::Enabled),
            plan: self.flags.plan.then_some(PlanMode::Enabled),
            // Squad agents are always PTY-backed so a later attach reaches
            // the real agent UI. ACP is inherently a piped JSON-RPC transport
            // and therefore cannot satisfy that contract; ordinary `exec
            // workflow` continues to honour its per-step ACP configuration.
            launch_mode: if self.squad_identity.is_some() {
                crate::data::config::agents::LaunchMode::Stdio
            } else {
                self.launch_modes
                    .get(&step.name)
                    .copied()
                    .unwrap_or_default()
            },
            allowed_tools: vec![],
            disallowed_tools: vec![],
            initial_prompt: Some(substitution.rendered),
            allow_docker: self.flags.allow_docker,
            non_interactive: self.flags.non_interactive,
            structured_output: self.output_dir.is_some(),
            model: runtime.step_model.clone(),
            // Always `Some`: resolved here from the invoking session so the
            // engine never falls back to the worktree session's config.
            agent_flags: Some(self.agent_flags.agent_flags(runtime.step_agent.as_str())),
            env_passthrough: if collected.env_passthrough.is_empty() {
                None
            } else {
                Some(collected.env_passthrough)
            },
            directory_overlays,
            command_mounts,
            include_all_skills: collected.include_all_skills,
            named_skills: collected.named_skills,
            image_tag_override: Some(correct_tag),
            system_prompt,
            context_overlays,
            grant_env: self.grant_env.clone(),
        };
        let request = crate::command::commands::launch_policy::settings_profile::ProfileRequest {
            name: step
                .settings_profile
                .clone()
                .or_else(|| self.settings_profile.clone()),
            agent_session: None,
        };
        let def = launch_engine.resolve_definition(session, &runtime.step_agent)?;
        let mut profile = crate::command::commands::launch_policy::settings_profile::enter_sync(
            session,
            &runtime.step_agent,
            &request,
            &run_opts,
            &def,
            self.engines.runtime.clone(),
        )
        .map_err(|e| EngineError::Other(e.to_string()))?;
        run_opts.profile_state = profile.as_ref().map(|p| p.run_state());
        // Resolve keychain credentials so the agent can reach its backend.
        // Mirrors the same step in `chat` and `exec_prompt`. The centralized
        // builder folds them into the paradigm-appropriate option (container
        // env vars, or — under sbx — `sbx secret set` registration).
        let resolved_credentials = self
            .engines
            .auth_engine
            .resolve_agent_auth(session, &runtime.step_agent)
            .unwrap_or_default();
        // A file-delivered credential is deliberately refreshed before the
        // container options are built when it is on the edge of expiry. The
        // refresh is bounded and advisory: a stale host credential must not
        // make an otherwise runnable workflow step fail to launch.
        if !self.engines.runtime.capabilities().kit_declarative
            && matches!(
                resolved_credentials.delivery,
                crate::engine::auth::CredentialDelivery::File(_)
            )
        {
            let settings = session.effective_config().auth_refresh();
            let near_expiry = self
                .engines
                .auth_engine
                .list_agent_credentials(&runtime.step_agent)
                .ok()
                .and_then(|status| status.expires_at)
                .map(
                    |expires_at| match expires_at.duration_since(std::time::SystemTime::now()) {
                        Ok(remaining) => remaining < settings.threshold,
                        Err(_) => true,
                    },
                )
                .unwrap_or(false);
            if near_expiry {
                if let Some(monitor) = self.engines.credential_monitor.clone() {
                    let outcome = refresh_credential_blocking(
                        monitor,
                        runtime.step_agent.clone(),
                        vec![
                            session.working_dir().to_path_buf(),
                            session.git_root().to_path_buf(),
                            session.definition_root().to_path_buf(),
                        ],
                    );
                    if let Some(warning) = refresh_warning(&outcome) {
                        self.shared.lock().unwrap().write_message(UserMessage {
                            level: MessageLevel::Warning,
                            text: format!("workflow step '{}': {warning}", step.name),
                        });
                    }
                }
            }
        }
        let credentials = if self.engines.runtime.capabilities().kit_declarative
            && matches!(
                resolved_credentials.delivery,
                crate::engine::auth::CredentialDelivery::File(_)
            ) {
            self.engines
                .auth_engine
                .agent_env_credentials(&runtime.step_agent)
                .unwrap_or_default()
        } else {
            resolved_credentials
        };

        let resolved = launch_engine.resolve_agent_options(
            session,
            &runtime.step_agent,
            &run_opts,
            &credentials,
        )?;
        {
            let mut frontend = self.shared.lock().unwrap();
            for note in crate::command::commands::launch_policy::LaunchPolicy::launch_notices(
                &runtime.step_agent,
                &run_opts,
            ) {
                frontend.write_message(UserMessage {
                    level: note.level,
                    text: format!("workflow step '{}': {}", step.name, note.text),
                });
            }
        }
        // For a squad-generated workflow, stamp the task's squad name +
        // labels so this step's container is discoverable by prefix, exactly as
        // the evaluation leader is. A non-squad run leaves `resolved` untouched.
        let resolved = match &self.squad_identity {
            Some(identity) => identity.stamp(resolved)?,
            None => resolved,
        };
        // The step's native start mode and mount access, resolved by the same
        // pure rule as chat and exec prompt (WI 0070). The workflow preflight
        // already proved every step can deliver a requested --plan.
        let plan_kind = if self.flags.plan {
            crate::engine::agent::AgentRegistry::resolve_for_session(session, &runtime.step_agent)?
                .native_plan
                .as_ref()
                .map(|spec| spec.kind)
        } else {
            None
        };
        let policy = crate::command::commands::launch_policy::resolve_agent_policy_pure(
            &crate::command::commands::launch_policy::PureRequest {
                agent: runtime.step_agent.as_str(),
                plan: self.flags.plan,
                yolo: Default::default(),
                auto: Default::default(),
                requested_access: workspace_access,
                engine_read_only: self.engines.agent_engine.read_only_workspace(),
                runtime_supports_read_only: self
                    .engines
                    .runtime
                    .capabilities()
                    .read_only_workspace(),
                runtime_name: self.engines.runtime.display_name(),
                allow_docker: self.flags.allow_docker,
                plan_kind,
                launch: if run_opts.launch_mode == crate::data::config::agents::LaunchMode::Acp {
                    crate::command::commands::launch_policy::LaunchModeDecision::Acp
                } else {
                    crate::command::commands::launch_policy::LaunchModeDecision::Stdio
                },
            },
        )
        .map_err(|e| EngineError::Other(e.to_string()))?
        .policy;
        {
            let mut frontend = self.shared.lock().unwrap();
            if let Some(message) =
                LaunchPolicy::native_mode_message(&runtime.step_agent, &policy, Some(&step.name))
            {
                frontend.write_message(message);
            }
            frontend.report_agent_policy(policy, Some(&step.name));
        }
        let instance = self.engines.runtime.build(resolved)?;
        if workspace_access.is_read_only() {
            let mut frontend = self.shared.lock().unwrap();
            frontend.write_message(
                crate::command::commands::workspace_access::read_only_message(Some(&step.name)),
            );
            frontend.report_workspace_access(workspace_access, Some(&step.name));
        }
        let proxy = AgentFrontendProxy::new(
            self.shared.clone(),
            run_opts.launch_mode == crate::data::config::agents::LaunchMode::Acp,
        )
        .with_source(runtime.source.clone());
        if let Some(profile) = profile.as_mut() {
            let redaction = instance.output_redaction();
            redaction.absorb(&proxy.output_redaction());
            profile
                .bind_redaction(redaction)
                .map_err(|e| EngineError::Other(e.to_string()))?;
            profile
                .bind_resource(&instance.handle_preview().name)
                .map_err(|e| EngineError::Other(e.to_string()))?;
        }
        let execution = instance.run_with_frontend(Box::new(proxy))?;
        if let Some(profile) = profile {
            self.profiles
                .lock()
                .unwrap()
                .insert(execution.handle().id.clone(), profile);
        }
        Ok(execution)
    }
}

impl AgentExecutionFactory for CommandLayerFactory {
    fn settings_scope_matches(&self, current: &WorkflowStep, next: &WorkflowStep) -> bool {
        current
            .settings_profile
            .as_ref()
            .or(self.settings_profile.as_ref())
            == next
                .settings_profile
                .as_ref()
                .or(self.settings_profile.as_ref())
    }

    fn execution_finished(
        &self,
        handle: crate::data::session::AgentHandle,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let profile = self.profiles.lock().unwrap().remove(&handle.id);
        Box::pin(async move {
            if let Some(profile) = profile {
                if let Err(error) = profile
                    .leave(
                        self.engines.runtime.clone(),
                        crate::data::fs::agent_profiles::TurnStateAtExit::Unknown,
                    )
                    .await
                {
                    tracing::warn!(%error,"workflow profile remains closing");
                }
            }
        })
    }

    fn execution_for_step(
        &self,
        step: &WorkflowStep,
        session: &Session,
        runtime: &WorkflowRuntimeContext,
    ) -> Result<crate::engine::agent_runtime::execution::AgentExecution, EngineError> {
        self.execution_with_prompt(
            step,
            session,
            runtime,
            substitute_prompt(&step.prompt_template, self.work_item_context.as_ref()),
        )
    }

    fn execution_for_rendered_step(
        &self,
        step: &WorkflowStep,
        session: &Session,
        runtime: &WorkflowRuntimeContext,
    ) -> Result<crate::engine::agent_runtime::execution::AgentExecution, EngineError> {
        self.execution_with_prompt(
            step,
            session,
            runtime,
            crate::data::workflow_prompt_template::Substitution {
                rendered: step.prompt_template.clone(),
                warnings: vec![],
            },
        )
    }

    fn inject_prompt(
        &self,
        execution: &crate::engine::agent_runtime::execution::AgentExecution,
        prompt: &str,
    ) -> Result<Option<()>, EngineError> {
        // Reuse the running container for the next step:
        // write the prompt followed by `\r` (Enter) directly into the running
        // container's PTY stdin. The Container Execution back-end returns
        // `Ok(true)` if it accepted the bytes (PTY-bridged backends do),
        // `Ok(false)` if it can't inject (inherit-stdio with no PTY) — in
        // which case we report `Ok(None)` and the engine launches a fresh
        // container.
        let mut payload = prompt.as_bytes().to_vec();
        payload.push(b'\r');
        match execution.try_inject_stdin(&payload)? {
            true => Ok(Some(())),
            false => Ok(None),
        }
    }

    fn recover_auth_failure(
        &self,
        agent: &crate::data::session::AgentName,
        output_tail: &str,
    ) -> Result<bool, EngineError> {
        let Some(spec) = refreshable_spec_for(agent) else {
            return Ok(false);
        };
        if !(spec.is_auth_failure)(output_tail) {
            return Ok(false);
        }
        let Some(monitor) = self.engines.credential_monitor.clone() else {
            return Ok(false);
        };
        let outcome =
            refresh_credential_blocking(monitor, agent.clone(), vec![self.image_git_root.clone()]);
        if let Some(warning) = refresh_warning(&outcome) {
            self.shared.lock().unwrap().write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!("workflow authentication recovery: {warning}"),
            });
        }
        // A matching descriptor signature authorises one relaunch even when
        // the host cannot rotate. The monitor keeps the last-known-good file;
        // the workflow engine owns the exactly-once guard.
        Ok(true)
    }

    fn stop_agent(&self, handle: &crate::data::session::AgentHandle) -> Result<(), EngineError> {
        crate::command::commands::agents::kill::stop_and_remove_agent(
            self.engines.runtime.as_ref(),
            handle,
        )
        .map(|_removed| ())
    }
}

#[cfg(test)]
mod stuck_timeout_tests {
    #[test]
    fn workflow_stuck_countdown_keeps_its_lifecycle_default() {
        assert_eq!(
            super::workflow_step_stuck_timeout(),
            crate::engine::container::timing::DEFAULT_STUCK_TIMEOUT
        );
    }
}
