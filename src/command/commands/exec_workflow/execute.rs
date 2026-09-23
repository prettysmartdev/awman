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
        // Early flag validation (Layer 2) — runs before any IO for both the
        // dynamic and non-dynamic paths. Surfaces an error message and aborts.
        if let Err(e) = validate_dynamic_flags(&self.flags) {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("exec workflow: {e}"),
            });
            return Err(e);
        }

        // Dynamic mode: a leader agent designs the workflow, then it executes.
        if self.flags.dynamic {
            return self.run_dynamic(frontend).await;
        }

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

        // Track whether the gemini deprecation warning has already been emitted
        // so we never fire it twice (early CLI check + post-load TOML scan).
        let mut deprecation_warned = false;
        if let Some(agent) = self.flags.agent.as_deref() {
            deprecation_warned = emit_deprecation_warning(agent, frontend.as_mut());
        }

        // Emit deprecation warnings for legacy config fields.
        LaunchPolicy::for_session(&self.session).warn_legacy_config(frontend.as_mut());

        if self.flags.yolo && self.flags.worktree {
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "--yolo implies --worktree. Running in isolated worktree.".into(),
            });
        }

        // 1. Load the workflow file.
        if !workflow_path.exists() {
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
        let workflow = match Workflow::load(&workflow_path) {
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

        // After load: scan the workflow's per-step and workflow-level agents,
        // plus the session default (used when neither step nor workflow set an
        // agent). Per-step resolution mirrors WorkflowEngine::resolve_agent so
        // the warning fires for the same agent the engine will actually launch.
        if !deprecation_warned {
            for agent in workflow_agents(&workflow, &self.session) {
                if emit_deprecation_warning(&agent, frontend.as_mut()) {
                    deprecation_warned = true;
                    break;
                }
            }
        }

        // Warn (don't error) when context(workflow) appears in a setup or
        // teardown step's overlays — workflow step progression state is not
        // available during those phases, so the dynamic prompt fields will
        // be empty.
        warn_context_workflow_in_phase(&workflow, frontend.as_mut());
        let _ = deprecation_warned;

        // ACP compatibility is a workflow-wide pre-flight check.  Do it
        // before mount scope, worktree preparation, image setup, overlays, or
        // the workflow engine so `fallback: error` cannot leave partial work.
        let launch_modes = match validate_workflow_acp_preflight(
            &workflow,
            &self.session,
            &self.flags,
            frontend.as_mut(),
        ) {
            Ok(modes) => modes,
            Err(e) => return Err(CommandError::from(e)),
        };

        // 2. Resolve mount scope — confirm with the user when cwd differs from git root.
        let cwd = self.session.working_dir().to_path_buf();
        let git_root_for_scope = self.session.git_root().to_path_buf();
        let mount_path = match MountScope::resolve(&cwd, &git_root_for_scope, frontend.as_mut()) {
            Ok(p) => p,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec workflow: mount scope resolution failed: {e}"),
                });
                return Err(e);
            }
        };

        // 3. Load work item context from --work-item or --issue.
        // `_issue_temp_file` keeps the temp file alive for the duration of
        // this function — its Drop impl deletes the file regardless of how
        // the function exits (success, error, panic).
        let issue_title_slug: Option<String>;
        let _issue_temp_file: Option<IssueTempFile>;
        let issue_overlay: Option<TypedOverlay>;

        let work_item_context = if let Some(ref issue_ref) = self.flags.issue_source.issue {
            // --issue: fetch issue and construct work item context from it.
            let router = crate::engine::issue::router::IssueSourceRouter::new(
                std::sync::Arc::clone(&self.engines.git_engine),
                self.session.env(),
            );
            match router.fetch_issue_with_progress(issue_ref, &git_root_for_scope, &mut *frontend) {
                Ok((issue, source)) => {
                    let work_items_dir = self
                        .session
                        .repo_config()
                        .work_items_dir_or_default(&git_root_for_scope);
                    let build = match issue_source_overlay(
                        source,
                        &issue,
                        &git_root_for_scope,
                        &work_items_dir,
                    ) {
                        Ok(b) => b,
                        Err(e) => {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!(
                                    "exec workflow: failed to write issue temp file: {e}"
                                ),
                            });
                            return Err(CommandError::Other(format!(
                                "writing issue temp file: {e}"
                            )));
                        }
                    };

                    frontend.write_message(UserMessage {
                        level: MessageLevel::Info,
                        text: format!(
                            "exec workflow: fetched issue '{}' ({})",
                            issue.title, issue.source_id
                        ),
                    });

                    issue_overlay = Some(build.overlay);
                    issue_title_slug = Some(build.slug);
                    let number = build.number;
                    let content = build.content;
                    _issue_temp_file = Some(build.temp_file);
                    Some(WorkItemContext { number, content })
                }
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec workflow: failed to fetch issue: {e}"),
                    });
                    return Err(CommandError::Other(e.to_string()));
                }
            }
        } else if let Some(wi_str) = &self.flags.work_item {
            issue_title_slug = None;
            _issue_temp_file = None;
            issue_overlay = None;
            match parse_work_item_number(wi_str) {
                Some(number) => {
                    let path = find_work_item_file(&self.session, &git_root_for_scope, number);
                    match path.and_then(|p| std::fs::read_to_string(&p).ok()) {
                        Some(content) => Some(WorkItemContext { number, content }),
                        None => {
                            frontend.write_message(crate::data::message::UserMessage {
                                level: crate::data::message::MessageLevel::Warning,
                                text: format!(
                                    "work item file for {:04} not found; \
                                     {{{{work_item_*}}}} placeholders will be empty",
                                    number
                                ),
                            });
                            None
                        }
                    }
                }
                None => {
                    frontend.write_message(crate::data::message::UserMessage {
                        level: crate::data::message::MessageLevel::Warning,
                        text: format!(
                            "could not parse work item number from {:?}; \
                             {{{{work_item_*}}}} placeholders will be empty",
                            wi_str
                        ),
                    });
                    None
                }
            }
        } else {
            issue_title_slug = None;
            _issue_temp_file = None;
            issue_overlay = None;
            None
        };
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
            let git_root = match self.engines.git_engine.resolve_root(&cwd) {
                Ok(r) => r,
                Err(e) => {
                    let err = CommandError::from(e);
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec workflow: failed to resolve git root: {err}"),
                    });
                    return Err(err);
                }
            };
            // `--issue` names the worktree after the issue slug, `--work-item`
            // after the number, otherwise the workflow filename. The choice is
            // `WorktreeLifecycle::name_for`'s; this opens it once (F-51).
            let worktree_name = WorktreeLifecycle::name_for(
                issue_title_slug.as_deref(),
                self.flags
                    .work_item
                    .is_some()
                    .then(|| work_item_context.as_ref().map(|ctx| ctx.number))
                    .flatten(),
                &workflow_path,
            );
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
            let resumed = match offer_state_resume(
                &store,
                &workflow,
                &workflow_name,
                work_item_context.as_ref().map(|c| c.number),
                Some(lifecycle.worktree_path()),
                &mut *frontend,
            )? {
                StateResumeOutcome::Cancelled => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Info,
                        text: "exec workflow: cancelled; nothing was created or changed."
                            .to_string(),
                    });
                    return Ok(ExecWorkflowOutcome {
                        workflow: workflow_name,
                        exit_code: None,
                        worktree_used: false,
                    });
                }
                StateResumeOutcome::Proceed { resumed } => resumed,
            };
            state_resume_settled = true;

            let wt_path = match lifecycle
                .prepare_with_existing(
                    &mut *frontend,
                    // An accepted resume is an answer to the existing-worktree
                    // question: the saved run lives in that worktree, and
                    // recreating it would throw away both the commits and the
                    // state we just rewound.
                    resumed.then_some(
                        crate::command::commands::worktree_lifecycle::ExistingWorktreeDecision::Resume,
                    ),
                )
                .await
            {
                Ok(p) => p,
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
                worktree_git_overlay(&mount_path)?
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
            if let Some(overlay) = issue_overlay {
                all.push(overlay);
            }
            all
        };

        let prepared = PreparedRun {
            workflow,
            workflow_path,
            work_item_context,
            cli_typed,
            mount_path,
            worktree_path,
            worktree_lifecycle,
            worktree_git_mount,
            git_root_for_scope,
            cwd,
            original_session: self.session,
            issue_temp_file: _issue_temp_file,
            launch_modes,
            skip_state_resume_prompt: state_resume_settled,
        };
        execute_prepared(
            &self.flags,
            &self.engines,
            prepared,
            frontend,
            self.squad_identity.as_ref(),
            self.task_workspace.as_deref(),
            self.workflow_state_root.as_deref(),
            self.managed_session.as_ref(),
        )
        .await
    }
}
