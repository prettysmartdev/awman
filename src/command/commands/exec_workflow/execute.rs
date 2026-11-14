//! `impl Command for ExecWorkflowCommand` — argument handling, preflight,
//! worktree setup, and the handover to `execute_prepared`.
//!
//! Split out of `commands/exec_workflow.rs` by WI 0114 F-51. A child module
//! of `exec_workflow`, so it reaches that module's private items unchanged.

use super::*;

// ─── Command impl ─────────────────────────────────────────────────────────────

#[async_trait]
impl Command for ExecWorkflowCommand {
    type Frontend = Box<dyn ExecWorkflowCommandFrontend>;
    type Outcome = ExecWorkflowOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        if self.output_dir.is_some() && !self.engines.runtime.capabilities().arbitrary_host_mounts {
            frontend.write_message(UserMessage { level: MessageLevel::Warning, text: "Persistent command outputs are unavailable on this runtime; /awman/output is not mounted.".into() });
        }
        // Early flag validation (Layer 2) — runs before any IO for both the
        // dynamic and non-dynamic paths. Surfaces an error message and aborts.
        if let Err(e) =
            validate_dynamic_flags(&self.flags, self.session.inline_work_item().is_some())
        {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("exec workflow: {e}"),
            });
            return Err(e);
        }

        // Resolve against the invoking checkout before prerequisites, worktree
        // preparation, setup, or any agent launch. Fail closed on every error.
        let work_item = crate::command::work_item::WorkItemResolver::new(
            self.session.clone(),
            Arc::clone(&self.engines.git_engine),
        )
        .resolve(self.flags.work_item.as_deref(), &mut *frontend)
        .await?;

        // Dynamic mode: a leader agent designs the workflow, then it executes.
        if self.flags.dynamic {
            return self.run_dynamic(frontend, work_item).await;
        }
        let mut frontend = SharedWorkflowFrontend::new(frontend);

        // Non-dynamic: the positional path is required.
        let workflow_arg = match &self.flags.workflow {
            Some(p) => p.clone(),
            None => {
                let err =
                    CommandError::missing_required_argument(&["exec", "workflow"], "workflow");
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: "exec workflow: missing required argument 'workflow'".into(),
                });
                return Err(err);
            }
        };

        // Resolve the workflow path relative to the session's working
        // directory so that relative paths work regardless of where the
        // awman process was originally launched.
        let workflow_path = if workflow_arg.is_absolute() {
            workflow_arg.clone()
        } else {
            self.session.working_dir().join(&workflow_arg)
        };

        // 1. Load the workflow file.
        if !workflow_path.exists() {
            if let Some(agent) = self.flags.agent.as_deref() {
                frontend.with(|frontend| emit_deprecation_warning(agent, frontend));
            }
            let err = CommandError::WorkflowFileNotFound {
                path: workflow_path.clone(),
            };
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!(
                    "exec workflow: workflow file not found: {}",
                    workflow_path.display()
                ),
            });
            return Err(err);
        }
        let mut workflow = match Workflow::load(&workflow_path) {
            Ok(w) => w,
            Err(e) => {
                let err = CommandError::Other(format!("loading workflow: {e}"));
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec workflow: failed to load workflow: {e}"),
                });
                return Err(err);
            }
        };

        // Custom agent definitions are reviewed once per distinct agent
        // before anything resolves them (the preflight below included).
        if let Err(e) = frontend.with(|frontend| {
            ensure_workflow_agents_approved(&workflow, &self.session, &self.flags, frontend)
        }) {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("exec workflow: {e}"),
            });
            return Err(e);
        }

        // Workflow prerequisites (WI 0149): what the teardown steps need is
        // checked, asked about and granted now, before the worktree, the
        // image or any container exists. `--check-prerequisites` ends here.
        let prerequisite_needs =
            crate::data::workflow_prerequisites::WorkflowPrerequisites::from_workflow(&workflow);
        let prerequisites = match self
            .start_prerequisites(
                &prerequisite_needs,
                crate::engine::workflow::workflow_name_for(&workflow),
                &frontend,
            )
            .await?
        {
            PrerequisiteStart::Run(prepared) => prepared,
            PrerequisiteStart::CheckOnly(outcome) => return Ok(outcome),
        };

        // Track whether the gemini deprecation warning has already been emitted
        // so we never fire it twice (early CLI check + post-load TOML scan).
        let mut deprecation_warned = false;
        if let Some(agent) = self.flags.agent.as_deref() {
            deprecation_warned =
                frontend.with(|frontend| emit_deprecation_warning(agent, frontend));
        }

        // Emit deprecation warnings for legacy config fields.
        frontend.with(|frontend| LaunchPolicy::for_session(&self.session).warn_config(frontend));

        if self.flags.yolo && self.flags.worktree {
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "--yolo implies --worktree. Running in isolated worktree.".into(),
            });
        }

        // After load: scan the workflow's per-step and workflow-level agents,
        // plus the session default (used when neither step nor workflow set an
        // agent). Per-step resolution mirrors WorkflowEngine::resolve_agent so
        // the warning fires for the same agent the engine will actually launch.
        if !deprecation_warned {
            for agent in workflow_agents(&workflow, &self.session) {
                if frontend.with(|frontend| emit_deprecation_warning(&agent, frontend)) {
                    deprecation_warned = true;
                    break;
                }
            }
        }

        // Warn (don't error) when context(workflow) appears in a setup or
        // teardown step's overlays — workflow step progression state is not
        // available during those phases, so the dynamic prompt fields will
        // be empty.
        frontend.with(|frontend| warn_context_workflow_in_phase(&workflow, frontend));
        let _ = deprecation_warned;

        // ACP compatibility is a workflow-wide pre-flight check.  Do it
        // before mount scope, worktree preparation, image setup, overlays, or
        // the workflow engine so `fallback: error` cannot leave partial work.
        let launch_modes = match frontend.with(|frontend| {
            validate_workflow_acp_preflight(&workflow, &self.session, &self.flags, frontend)
        }) {
            Ok(modes) => modes,
            Err(e) => return Err(CommandError::from(e)),
        };

        // 2. Resolve mount scope — confirm with the user when cwd differs from git root.
        let cwd = self.session.working_dir().to_path_buf();
        let git_root_for_scope = self.session.git_root().to_path_buf();
        let mount_path = match frontend
            .with(|frontend| MountScope::resolve(&cwd, &git_root_for_scope, frontend))
        {
            Ok(p) => p,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec workflow: mount scope resolution failed: {e}"),
                });
                return Err(e);
            }
        };

        // Keep private remote input alive through steps, teardown and cancellation.
        // Local input continues to use the file in the mounted checkout.
        let prepared_input = work_item
            .as_ref()
            .map(|item| crate::command::work_item::prepare_work_item_input(item, &self.session))
            .transpose()?;
        let work_item_context = prepared_input.as_ref().map(|input| input.context.clone());
        let work_item_overlay = prepared_input
            .as_ref()
            .and_then(|input| input.overlay.clone());
        // 4. Worktree prepare (if --worktree is set).
        // When a worktree is used, capture its path so the session below is
        // rooted at the worktree checkout rather than the main repo.
        if self.flags.worktree && self.session.session_type().is_remote() {
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "Skipping worktree creation for remote session — repo is already isolated."
                    .into(),
            });
        }
        let mut worktree_path: Option<PathBuf> = None;
        // Set once the worktree branch below has asked the resume question, so
        // `execute_prepared` does not ask it a second time (WI-0115 §2). A run
        // without a worktree creates nothing before `execute_prepared`, so it
        // is left to ask there.
        let mut state_resume_settled = false;
        let worktree_lifecycle = if self.flags.worktree && !self.session.session_type().is_remote()
        {
            let git_root = self.session.git_root().to_path_buf();
            let worktree_name = WorktreeLifecycle::name_for(work_item.as_ref(), &workflow_path);
            let lifecycle = match WorktreeLifecycle::open(
                Arc::clone(&self.engines.git_engine),
                git_root,
                &worktree_name,
            ) {
                Ok(l) => l,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec workflow: failed to create worktree: {e}"),
                    });
                    return Err(e);
                }
            };
            // 4a. Ask the resume question *before* the worktree is prepared
            //     (WI-0115 §2), the same way the dynamic path asks before its
            //     leader phase. Cancelling is only a true cancellation if
            //     nothing has been created yet, and a resume that recreated
            //     the worktree first would delete the very state it resumes.
            let state_root = self.workflow_state_root_for(lifecycle.worktree_path());
            let store =
                crate::data::workflow_state_store::WorkflowStateStore::at_git_root(state_root);
            let workflow_name = crate::engine::workflow::workflow_name_for(&workflow);
            let offer_resume = frontend.with(|frontend| {
                lifecycle.should_offer_resume(frontend, self.flags.on_existing_worktree)
            })?;
            let resumed = if !offer_resume {
                false
            } else {
                match frontend.with(|frontend| {
                    offer_state_resume(
                        &store,
                        &workflow,
                        &workflow_name,
                        work_item_context.as_ref(),
                        Some(lifecycle.worktree_path()),
                        frontend,
                    )
                })? {
                    StateResumeOutcome::Cancelled => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Info,
                            text: "exec workflow: cancelled; nothing was created or changed."
                                .to_string(),
                        });
                        return Ok(ExecWorkflowOutcome {
                            prerequisites: None,
                            workflow: workflow_name,
                            exit_code: None,
                            worktree_used: false,
                        });
                    }
                    StateResumeOutcome::Proceed { resumed } => resumed,
                }
            };
            state_resume_settled = true;

            let prepared_worktree = frontend
                .prepare_worktree(
                    lifecycle,
                    // An accepted resume is an answer to the existing-worktree
                    // question: the saved run lives in that worktree, and
                    // recreating it would throw away both the commits and the
                    // state we just rewound. An explicit
                    // `--on-existing-worktree` still wins.
                    existing_worktree_choice(self.flags.on_existing_worktree, resumed),
                )
                .await;
            let (lifecycle, wt_path) = match prepared_worktree {
                Ok(prepared) => prepared,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec workflow: worktree prepare failed: {e}"),
                    });
                    return Err(e);
                }
            };
            worktree_path = Some(wt_path);
            Some(lifecycle)
        } else {
            None
        };

        // 4b. Override mount path when a worktree is active so setup/teardown
        // containers bind to the worktree checkout, not the main repo.
        let mount_path = if let Some(ref wt) = worktree_path {
            wt.clone()
        } else {
            mount_path
        };

        // 4c. When running in a worktree, compute an extra overlay that mounts
        // the main repo's `.git` directory into setup/teardown containers.
        // Without this, the worktree's `.git` pointer file references a host
        // path that doesn't exist inside the container, breaking all git ops.
        let worktree_git_mount: Option<crate::engine::container::options::OverlaySpec> =
            if worktree_path.is_some() {
                worktree_git_overlay(&self.engines.git_engine, &git_root_for_scope, &mount_path)?
            } else {
                None
            };

        // 5. Parse CLI overlay specs early so errors surface before PTY is activated.
        let cli_typed = {
            let mut all = Vec::new();
            for s in &self.flags.overlay {
                match parse_overlay_list(s) {
                    Ok(parsed) => all.extend(parsed),
                    Err(reason) => {
                        let e = CommandError::InvalidOverlaySpec {
                            spec: s.clone(),
                            reason,
                        };
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("exec workflow: invalid overlay spec: {e}"),
                        });
                        return Err(e);
                    }
                }
            }
            all
        };

        // 6. Every agent the steps launch needs its image. Ask about each
        //    one that is missing (set up / fall back to the default agent /
        //    abort) now, before any setup step or agent container starts.
        //    Images are tagged by the invoking repo, not the worktree.
        if let Err(e) = ensure_static_workflow_agent_images(
            &mut workflow,
            &self.engines,
            &self.session,
            &self.flags,
            &mut frontend,
        )
        .await
        {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("exec workflow: {e}"),
            });
            return Err(e);
        }

        let prepared = PreparedRun {
            workflow,
            workflow_path,
            work_item_context,
            work_item_overlay,
            cli_typed,
            mount_path,
            worktree_path,
            worktree_lifecycle,
            worktree_git_mount,
            git_root_for_scope,
            cwd,
            original_session: self.session,
            launch_modes,
            skip_state_resume_prompt: state_resume_settled,
            prerequisites,
        };
        execute_prepared_with_outputs(
            &self.flags,
            &self.engines,
            prepared,
            frontend,
            self.squad_identity.as_ref(),
            self.task_workspace.as_deref(),
            self.workflow_state_root.as_deref(),
            self.managed_session.as_ref(),
            &self.cancel,
            self.output_dir.as_deref(),
        )
        .await
    }
}

/// Settle the existing-worktree question for a run: an explicit
/// `--on-existing-worktree` value wins; otherwise an accepted workflow/dynamic
/// resume is passed on as `ResumeAccepted` (the host decides whether that
/// implies `Resume`); otherwise the host asks.
pub(crate) fn existing_worktree_choice(
    flag: Option<crate::command::commands::worktree_lifecycle::ExistingWorktreeDecision>,
    resume_accepted: bool,
) -> crate::command::commands::worktree_lifecycle::ExistingWorktreeChoice {
    use crate::command::commands::worktree_lifecycle::ExistingWorktreeChoice;
    match (flag, resume_accepted) {
        (Some(d), _) => ExistingWorktreeChoice::Explicit(d),
        (None, true) => ExistingWorktreeChoice::ResumeAccepted,
        (None, false) => ExistingWorktreeChoice::Ask,
    }
}
