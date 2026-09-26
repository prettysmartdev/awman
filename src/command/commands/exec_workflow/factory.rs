//! The Layer 2 side of workflow execution: the per-container frontend proxy
//! the engine drives, and the factory that turns a workflow step into a
//! prepared agent run.
//!
//! Split out of `commands/exec_workflow.rs` by WI 0114 F-51. A child module
//! of `exec_workflow`, so it reaches that module's private items unchanged.

use super::*;

// ─── AgentFrontendProxy ──────────────────────────────────────────────────
//
// Passed to `AgentInstance::run_with_frontend`. Unlike the deleted
// `WorkflowProxy`, this is not a pure forwarder and the blanket
// `AgentFrontend for Arc<Mutex<Box<_>>>` impl cannot replace it: it owns
// per-container state (the I/O pairing below and the ACP PTY suppression in
// `take_io`). Everything that *is* pure forwarding goes through the blanket
// impl on `self.frontend`.

pub(crate) struct AgentFrontendProxy {
    pub(crate) frontend: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>,
    pub(crate) acp: bool,
    /// Each workflow container must receive the I/O sink paired with its own
    /// `Running { container_name }` callback. Keeping the taken I/O on this
    /// per-container proxy prevents parallel workflow steps from consuming a
    /// different step's per-container log file.
    pub(crate) io: Option<crate::engine::agent_runtime::frontend::AgentIo>,
}

#[async_trait]
impl AgentFrontend for AgentFrontendProxy {
    fn report_status(&mut self, status: crate::engine::agent_runtime::frontend::AgentStatus) {
        let is_running = matches!(
            &status,
            crate::engine::agent_runtime::frontend::AgentStatus::Running { .. }
        );
        // One lock for both calls: a parallel step must not slip in between
        // the `Running` report and the matching `take_io`.
        let mut frontend = self.frontend.lock().unwrap();
        frontend.report_status(status);
        if is_running {
            self.io = Some(frontend.take_io());
        }
    }

    fn report_progress(&mut self, progress: crate::engine::agent_runtime::frontend::AgentProgress) {
        self.frontend.report_progress(progress);
    }

    fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
        let mut io = self.io.take().unwrap_or_else(|| self.frontend.take_io());
        if self.acp {
            // ACP framing is line-delimited JSON, never a PTY stream.
            io.initial_size = None;
            io.resize = None;
        }
        io
    }

    fn grace_timeout(&self) -> std::time::Duration {
        self.frontend.grace_timeout()
    }

    fn stuck_timeout(&self) -> std::time::Duration {
        self.frontend.stuck_timeout()
    }
}

impl UserMessageSink for AgentFrontendProxy {
    fn write_message(&mut self, msg: UserMessage) {
        self.frontend.write_message(msg);
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

pub(crate) struct CommandLayerFactory {
    pub(crate) shared: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>,
    pub(crate) engines: Engines,
    pub(crate) flags: Arc<ExecWorkflowCommandFlags>,
    pub(crate) cli_typed_overlays: Vec<TypedOverlay>,
    pub(crate) work_item_context: Option<WorkItemContext>,
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
    pub(crate) launch_modes: Arc<HashMap<String, crate::data::config::repo::LaunchMode>>,
}

/// A workflow launch must never wait indefinitely for a host credential
/// refresh. The monitor applies this timeout internally; this helper also
/// keeps the synchronous `AgentExecutionFactory` boundary free of runtime
/// assumptions by driving the wait on a short-lived current-thread runtime.
pub(crate) const CREDENTIAL_REFRESH_WAIT: Duration = Duration::from_secs(30);

pub(crate) fn refresh_credential_blocking(
    monitor: Arc<crate::engine::credential_refresh::CredentialRefreshMonitor>,
    agent: crate::data::session::AgentName,
) -> RefreshOutcome {
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

impl AgentExecutionFactory for CommandLayerFactory {
    fn execution_for_step(
        &self,
        step: &WorkflowStep,
        session: &Session,
        runtime: &WorkflowRuntimeContext,
    ) -> Result<crate::engine::agent_runtime::execution::AgentExecution, EngineError> {
        // Substitute work item template tokens in the step prompt.
        let substitution =
            substitute_prompt(&step.prompt_template, self.work_item_context.as_ref());

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
        let correct_tag = crate::data::image_tags::agent_image_tag(
            &self.image_git_root,
            runtime.step_agent.as_str(),
        );
        let run_opts = AgentRunOptions {
            yolo: self.flags.yolo.then_some(YoloMode::Enabled),
            auto: self.flags.auto.then_some(AutoMode::Enabled),
            plan: self.flags.plan.then_some(PlanMode::Enabled),
            // Squad agents are always PTY-backed so a later attach reaches
            // the real agent UI. ACP is inherently a piped JSON-RPC transport
            // and therefore cannot satisfy that contract; ordinary `exec
            // workflow` continues to honour its per-step ACP configuration.
            launch_mode: if self.squad_identity.is_some() {
                crate::data::config::repo::LaunchMode::Stdio
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
            model: runtime.step_model.clone(),
            env_passthrough: if collected.env_passthrough.is_empty() {
                None
            } else {
                Some(collected.env_passthrough)
            },
            directory_overlays: collected.directories,
            include_all_skills: collected.include_all_skills,
            named_skills: collected.named_skills,
            image_tag_override: Some(correct_tag),
            system_prompt,
            context_overlays,
        };
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
                    let outcome = refresh_credential_blocking(monitor, runtime.step_agent.clone());
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

        let resolved = self.engines.agent_engine.resolve_agent_options(
            session,
            &runtime.step_agent,
            &run_opts,
            &credentials,
        )?;
        // For a squad-generated workflow, stamp the task's squad name +
        // labels so this step's container is discoverable by prefix, exactly as
        // the evaluation leader is. A non-squad run leaves `resolved` untouched.
        let resolved = match &self.squad_identity {
            Some(identity) => identity.stamp(resolved)?,
            None => resolved,
        };
        let instance = self.engines.runtime.build(resolved)?;
        let proxy = AgentFrontendProxy {
            frontend: Arc::clone(&self.shared),
            acp: run_opts.launch_mode == crate::data::config::repo::LaunchMode::Acp,
            io: None,
        };
        instance.run_with_frontend(Box::new(proxy))
    }

    fn inject_prompt(
        &self,
        execution: &crate::engine::agent_runtime::execution::AgentExecution,
        prompt: &str,
    ) -> Result<Option<()>, EngineError> {
        // Mirror old amux's `launch_next_workflow_step_in_current_container`:
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
        let outcome = refresh_credential_blocking(monitor, agent.clone());
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
}
