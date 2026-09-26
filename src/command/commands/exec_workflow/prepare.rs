//! Everything that has to be true before a workflow can run: the persisted
//! state resume offer, the overlays and base image a phase needs, the
//! workflow mutations an isolated worktree implies, and `PreparedRun`, which
//! both the plain and the dynamic path hand to `execute_prepared`.
//!
//! Split out of `commands/exec_workflow.rs` by WI 0114 F-51. A child module
//! of `exec_workflow`, so it reaches that module's private items unchanged.

use super::phase_container::InteractivePhaseContainer;
use super::*;
use crate::engine::workflow::PhaseStepRef;

/// Emit the deprecation warning for `agent`, if its vendor has deprecated it.
///
/// The wording is the one per-agent table's
/// (`AgentMatrix::deprecation_note`, reached through
/// `LaunchPolicy::deprecation_warning`). This used to be a third hand-written
/// copy of the gemini text, alongside `chat`'s and `exec prompt`'s (F-32).
pub(crate) fn workflow_resume_start_points(
    dag: &crate::data::workflow_dag::WorkflowDag,
    state: &crate::data::workflow_state::WorkflowState,
) -> Vec<WorkflowResumeStep> {
    let Some(idx) = state.resume_stop_point(dag) else {
        return Vec::new();
    };
    let order = dag.topological_order();
    // `resume_stop_point` finds a `Failed`/`Cancelled` step when there is one,
    // and otherwise falls back to the first step that never succeeded — which
    // is what an interrupted or paused run leaves behind. Naming that second
    // case "the step that failed" would describe a failure that never happened.
    let stopped_role = match state.status_of(&order[idx]) {
        Some(crate::data::workflow_state::StepState::Failed { .. }) => "the step that failed",
        Some(crate::data::workflow_state::StepState::Cancelled) => "the step that was cancelled",
        _ => "the step the run stopped on",
    };
    let mut points = vec![WorkflowResumeStep {
        name: order[idx].clone(),
        role: stopped_role.to_string(),
    }];
    if idx > 0 {
        points.push(WorkflowResumeStep {
            name: order[idx - 1].clone(),
            role: "the step before it".to_string(),
        });
    }
    if let Some(next) = order.get(idx + 1) {
        points.push(WorkflowResumeStep {
            name: next.clone(),
            role: "the step after it".to_string(),
        });
    }
    points
}

/// Count of steps a saved state records as done.
pub(crate) fn completed_step_count(state: &crate::data::workflow_state::WorkflowState) -> usize {
    use crate::data::workflow_state::StepState;
    state
        .step_states
        .values()
        .filter(|s| matches!(s, StepState::Succeeded | StepState::Skipped))
        .count()
}

/// What the shared saved-state resume question decided (WI-0115 §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StateResumeOutcome {
    /// Carry on with the run. `resumed` is true when saved state was rewound,
    /// which also answers the existing-worktree question: the caller must not
    /// ask it again.
    Proceed { resumed: bool },
    /// The user cancelled the command at the prompt. Nothing was written and
    /// nothing was deleted.
    Cancelled,
}

/// Ask the shared workflow-resume question for whatever state `store` holds for
/// `(work_item, workflow_name)`, and apply the answer (WI-0115 §2).
///
/// One implementation for every caller — the plain path asks it before the
/// worktree is prepared, the dynamic path asks its own richer version before
/// the leader phase, and `execute_prepared` asks it for the runs that reach the
/// engine without either. Keeping the decision *and* its consequences here is
/// what stops "resume" meaning three subtly different things.
///
/// Applying the answer means: `ResumeFrom` rewinds the saved state and writes
/// it back so the engine restarts at the chosen step; `Fresh` deletes the state
/// file; `Cancel` touches nothing at all.
pub(crate) fn offer_state_resume(
    store: &crate::data::workflow_state_store::WorkflowStateStore,
    workflow: &Workflow,
    workflow_name: &str,
    work_item: Option<u32>,
    worktree_path: Option<&Path>,
    frontend: &mut dyn ExecWorkflowCommandFrontend,
) -> Result<StateResumeOutcome, CommandError> {
    let proceed = StateResumeOutcome::Proceed { resumed: false };
    let drop_state = |frontend: &mut dyn ExecWorkflowCommandFrontend| {
        if let Err(e) = store.delete(work_item, workflow_name) {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!("exec workflow: failed to delete workflow state file: {e}"),
            });
        }
    };

    let mut saved = match store.load(work_item, workflow_name) {
        Ok(Some(saved)) => saved,
        Ok(None) => return Ok(proceed),
        Err(e) => {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!(
                    "exec workflow: failed to read workflow state file: {e}; starting fresh",
                ),
            });
            return Ok(proceed);
        }
    };

    let dag = match crate::data::workflow_dag::WorkflowDag::build(&workflow.steps) {
        Ok(dag) => dag,
        Err(e) => {
            // The saved state cannot be mapped onto the workflow as it stands
            // now; starting fresh is the only safe reading.
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!(
                    "exec workflow: saved state cannot be matched to this workflow ({e}); \
                     starting fresh"
                ),
            });
            drop_state(frontend);
            return Ok(proceed);
        }
    };

    let start_points = workflow_resume_start_points(&dag, &saved);
    if start_points.is_empty() {
        // Every step of the saved run succeeded. Resuming it would run
        // nothing, so retire the state rather than offer a choice that has
        // only one sane answer.
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!(
                "The saved run of '{workflow_name}' completed every step; starting fresh."
            ),
        });
        drop_state(frontend);
        return Ok(proceed);
    }

    let prompt = WorkflowResumePrompt::new(
        workflow_name.to_string(),
        work_item,
        worktree_path.map(|p| p.to_path_buf()),
        false,
        completed_step_count(&saved),
        saved.step_states.len(),
        start_points,
    );
    match frontend.ask_workflow_resume(&prompt)? {
        WorkflowResumeDecision::ResumeFrom(start) => {
            // Rewind before the engine loads the file, so a run that ended on
            // a failure or an abort — every step terminal — is runnable again.
            saved.rewind_to(&dag, &start);
            store.save(&saved).map_err(|e| {
                CommandError::Other(format!("rewinding the resumed workflow state: {e}"))
            })?;
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!("Resuming '{workflow_name}' from step '{start}'"),
            });
            Ok(StateResumeOutcome::Proceed { resumed: true })
        }
        WorkflowResumeDecision::Fresh => {
            drop_state(frontend);
            Ok(proceed)
        }
        WorkflowResumeDecision::Cancel => Ok(StateResumeOutcome::Cancelled),
    }
}

// ─── Shared workflow execution + dynamic preflight (WI-0092) ─────────────────

/// All state needed to execute a parsed workflow once the worktree, session,
/// context, and work item have been prepared. Both the non-dynamic path and
/// the dynamic leader path build one of these and hand it to
/// [`execute_prepared`], so workflow execution lives in exactly one place
/// (WI-0092 §10) — neither path recursively re-enters
/// `ExecWorkflowCommand::run_with_frontend`.
pub(crate) struct PreparedRun {
    pub(crate) workflow: Workflow,
    pub(crate) workflow_path: PathBuf,
    pub(crate) work_item_context: Option<WorkItemContext>,
    pub(crate) cli_typed: Vec<TypedOverlay>,
    pub(crate) mount_path: PathBuf,
    pub(crate) worktree_path: Option<PathBuf>,
    pub(crate) worktree_lifecycle: Option<WorktreeLifecycle>,
    pub(crate) worktree_git_mount: Option<crate::engine::container::options::OverlaySpec>,
    pub(crate) git_root_for_scope: PathBuf,
    pub(crate) cwd: PathBuf,
    /// The pre-worktree session. `execute_prepared` re-roots it at the worktree
    /// when `worktree_path` is set.
    pub(crate) original_session: Session,
    /// Kept alive for the duration of the run; its Drop removes the issue temp
    /// file. `None` for non-issue invocations.
    pub(crate) issue_temp_file: Option<IssueTempFile>,
    pub(crate) launch_modes: HashMap<String, crate::data::config::repo::LaunchMode>,
    /// Skip the persisted-state resume prompt below. Set only by the dynamic
    /// resume path, which has already asked the user a strictly better version
    /// of the same question (WI-0115 §2).
    pub(crate) skip_state_resume_prompt: bool,
}

/// Execute a fully-prepared workflow: persisted-state resume check, engine
/// setup/main/teardown phases, summary reporting, and worktree finalize.
/// Shared by the non-dynamic and dynamic execution paths.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_prepared(
    flags: &ExecWorkflowCommandFlags,
    engines: &Engines,
    prepared: PreparedRun,
    frontend: Box<dyn ExecWorkflowCommandFrontend>,
    squad_identity: Option<&crate::engine::squad::launcher::SquadContainerIdentity>,
    task_workspace: Option<&Path>,
    workflow_state_root: Option<&Path>,
    managed_session: Option<&Arc<tokio::sync::RwLock<Session>>>,
) -> Result<ExecWorkflowOutcome, CommandError> {
    let PreparedRun {
        mut workflow,
        workflow_path,
        work_item_context,
        cli_typed,
        mount_path,
        worktree_path,
        worktree_lifecycle,
        worktree_git_mount,
        git_root_for_scope,
        cwd,
        original_session,
        issue_temp_file: _issue_temp_file,
        launch_modes,
        skip_state_resume_prompt,
    } = prepared;
    let mut frontend = frontend;

    // When the run is inside an isolated worktree (--worktree, or implied by
    // --yolo/--dynamic), any `checkout_create_branch` setup step is redundant:
    // the worktree already put the run on its own branch. Skip-and-warn, not
    // a failure.
    if worktree_path.is_some() {
        skip_checkout_branch_steps_in_worktree(&mut workflow, frontend.as_mut());
    }

    // 5b. Detect a persisted workflow-state file and ask the user where to
    //     pick it up (WI-0115 §2). The check uses the session_root the engine
    //     will pick up below — the worktree path when --worktree is active,
    //     otherwise cwd. A caller that supplied `workflow_state_root` keeps
    //     its state file out of the session root entirely, so the resume check
    //     must look where the engine will actually read and write it.
    //
    //     `skip_state_resume_prompt` is set by the paths that already asked
    //     this question before anything was created on disk — the plain path
    //     ahead of `WorktreeLifecycle::prepare`, and the dynamic resume path
    //     ahead of the leader phase. This is the fallback site for the runs
    //     that reach the engine without either.
    if !skip_state_resume_prompt {
        let session_root_for_state = worktree_path.as_deref().unwrap_or(&cwd).to_path_buf();
        let git_root_for_state = match workflow_state_root {
            Some(root) => root.to_path_buf(),
            None => match Arc::clone(&engines.git_engine).resolve_root(&session_root_for_state) {
                Ok(r) => r,
                Err(_) => session_root_for_state,
            },
        };
        let store =
            crate::data::workflow_state_store::WorkflowStateStore::at_git_root(git_root_for_state);
        let workflow_name = crate::engine::workflow::workflow_name_for(&workflow);
        let decision = offer_state_resume(
            &store,
            &workflow,
            &workflow_name,
            work_item_context.as_ref().map(|c| c.number),
            worktree_path.as_deref(),
            frontend.as_mut(),
        )?;
        if decision == StateResumeOutcome::Cancelled {
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "exec workflow: cancelled; the saved run is unchanged.".to_string(),
            });
            return Ok(ExecWorkflowOutcome {
                workflow: workflow_name,
                exit_code: None,
                worktree_used: worktree_path.is_some(),
            });
        }
    }

    // 6. Set PTY active — queues user messages during the engine run.
    frontend.set_pty_active(true);

    // 7. Wrap the frontend in Arc<Mutex> so both the workflow engine and
    //    CommandLayerFactory can share it for the duration of the engine run.
    let shared: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>> = Arc::new(Mutex::new(frontend));

    let flags_arc = Arc::new(flags.clone());

    // 8. Build the session for the engine.
    // When a worktree is active, re-root the session at the worktree so
    // that `build_options` mounts the worktree checkout, not the main repo.
    let mut session = if let Some(ref wt) = worktree_path {
        let git_root_for_session = match Arc::clone(&engines.git_engine).resolve_root(wt) {
            Ok(r) => r,
            Err(e) => {
                let err = CommandError::from(e);
                shared.lock().unwrap().write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!(
                        "exec workflow: failed to resolve git root for worktree session: {err}"
                    ),
                });
                return Err(err);
            }
        };
        match Session::open_at_git_root(
            wt.clone(),
            git_root_for_session,
            crate::data::session::SessionOpenOptions::default(),
        ) {
            Ok(s) => s,
            Err(e) => {
                let err = CommandError::Other(format!("opening worktree session: {e}"));
                shared.lock().unwrap().write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec workflow: failed to open worktree session: {e}"),
                });
                return Err(err);
            }
        }
    } else {
        original_session
    };
    session.set_flags(workflow_flag_config(flags));

    // 9. Run the engine with three-phase coordination.
    // The engine block is scoped so proxy + factory are dropped before we
    // reclaim the frontend via Arc::try_unwrap.
    let yolo = flags.yolo;
    let setup_steps: Vec<crate::data::workflow_definition::SetupStep> =
        workflow.setup.iter().map(|e| e.step.clone()).collect();
    let teardown_steps: Vec<crate::data::workflow_definition::TeardownStep> =
        workflow.teardown.iter().map(|e| e.step.clone()).collect();
    let setup_entry_overlays: Vec<Option<Vec<String>>> =
        workflow.setup.iter().map(|e| e.overlays.clone()).collect();
    let setup_abort_flags: Vec<bool> = workflow.setup.iter().map(|e| e.abort_on_failure).collect();
    let setup_on_failure_configs: Vec<Option<crate::data::workflow_definition::RemediationConfig>> =
        workflow
            .setup
            .iter()
            .map(|e| e.on_failure.clone())
            .collect();
    let teardown_entry_overlays: Vec<Option<Vec<String>>> = workflow
        .teardown
        .iter()
        .map(|e| e.overlays.clone())
        .collect();
    let teardown_on_failure_configs: Vec<
        Option<crate::data::workflow_definition::RemediationConfig>,
    > = workflow
        .teardown
        .iter()
        .map(|e| e.on_failure.clone())
        .collect();
    let teardown_abort_flags: Vec<bool> = workflow
        .teardown
        .iter()
        .map(|e| e.abort_on_failure)
        .collect();
    let teardown_on_failure = workflow.teardown_on_failure;
    let engine_work_item_context = work_item_context.clone();
    let workflow_overlays_for_factory = workflow.overlays.clone();
    let active_workflow_context_permission = session
        .effective_config()
        .collected_overlays(
            cli_typed.clone(),
            workflow_overlays_for_factory.as_deref(),
            None,
        )
        .ok()
        .and_then(|collected| {
            collected
                .context_overlays
                .into_iter()
                .find(|c| c.scope == crate::engine::overlay::ContextScope::Workflow)
                .map(|c| c.permission)
        });
    let (engine_result, step_counts) = {
        let proxy = Arc::clone(&shared);
        let factory = CommandLayerFactory {
            shared: Arc::clone(&shared),
            engines: engines.clone(),
            flags: Arc::clone(&flags_arc),
            cli_typed_overlays: cli_typed.clone(),
            work_item_context,
            image_git_root: git_root_for_scope.clone(),
            workflow_overlays: workflow_overlays_for_factory,
            squad_identity: squad_identity.cloned(),
            task_workspace: task_workspace.map(Path::to_path_buf),
            launch_modes: Arc::new(launch_modes),
        };
        let mut engine = match WorkflowEngine::resume(
            &session,
            crate::engine::workflow::WorkflowSpec::new(workflow)
                .with_work_item_context(engine_work_item_context)
                .with_state_root(workflow_state_root.map(Path::to_path_buf)),
            crate::engine::workflow::WorkflowEngineDeps {
                frontend: Box::new(proxy),
                agent_factory: Box::new(factory),
            },
        )
        .await
        {
            Ok(eng) => eng,
            Err(e) => {
                let err = CommandError::from(e);
                shared.lock().unwrap().write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec workflow: failed to initialize workflow engine: {err}"),
                });
                return Err(err);
            }
        };
        // Decision Q3: a frontend viewing this session sees the run's
        // progress through `SessionState`, mirrored by the engine after every
        // persist (WI 0114 F-22).
        if let Some(session) = managed_session {
            engine = engine.mirror_into_session(Arc::clone(session));
        }
        engine.set_yolo(yolo);
        engine.set_parallel_group_prompts(crate::command::prompts::parallel_group_prompts());
        engine.set_workflow_context_permission(active_workflow_context_permission);

        // Warn if the workflow will commit but git identity is not configured.
        if teardown_steps.iter().any(|s| {
            matches!(
                s,
                crate::data::workflow_definition::TeardownStep::CommitChanges { .. }
            )
        }) {
            // Probed at the worktree (or session root) so a repo-local
            // identity counts: the pre-F-39 probe ran `git config` with no
            // working directory and only ever saw the global value.
            let identity = engines
                .git_engine
                .identity_configured(worktree_path.as_deref().unwrap_or(&git_root_for_scope))
                .unwrap_or_default();
            if !identity.is_complete() {
                let missing = identity.missing_keys();
                shared.lock().unwrap().write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: format!(
                        "workflow has a commit_changes teardown step but git {} not set; \
                         set them locally (git config {0}) or use a dir() overlay to mount \
                         your global ~/.gitconfig into the agent container",
                        missing.join(" and "),
                    ),
                });
            }
        }

        // When a person is at the frontend (the CLI on a TTY, the TUI), each
        // setup/teardown shell step runs as a foreground, PTY-attached
        // container they can watch and type into, like an agent step.
        // Everything else — the API server, the squad daemon, a
        // `--non-interactive` CLI — keeps the headless background container.
        // A squad-generated workflow is always headless, whichever frontend
        // is driving it.
        let interactive_phase_steps =
            squad_identity.is_none() && shared.lock().unwrap().supports_interactive_recovery();

        // === SETUP PHASE ===
        //
        // Each setup entry runs in its own container built from THAT
        // entry's overlays only (WI-0082): per-step isolation matters
        // because, e.g. an entry asking for `env(GITHUB_TOKEN)` must not
        // leak that token into a sibling entry that only asked for
        // `ssh()`. Container start/stop cost is amortized acceptably by
        // the small number of setup steps in real workflows.
        let mut setup_failed = false;
        if !setup_steps.is_empty() && !engine.state().setup_completed {
            let base_image = resolve_base_image(&session, &git_root_for_scope);
            let resolved = resolve_phase_overlays(
                engines,
                &session,
                &cli_typed,
                &setup_entry_overlays,
                worktree_git_mount.as_ref(),
                &base_image,
            );

            // A bad overlay on ANY entry aborts the whole phase before
            // any container starts — otherwise earlier steps would have
            // already mutated the workspace.
            if let Some(e) = resolved.iter().find_map(|r| r.as_ref().err()) {
                shared.lock().unwrap().write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec workflow: {e}"),
                });
                setup_failed = true;
            }

            if !setup_failed {
                let runtime = Arc::clone(
                    engines
                        .require_container_runtime()
                        .map_err(CommandError::from)?,
                );
                let mount = mount_path.clone();
                let base = base_image.clone();
                let shared_for_factory = Arc::clone(&shared);
                let setup_result = tokio::task::block_in_place(|| {
                    let factory = |step: &PhaseStepRef| -> Result<
                        Box<dyn crate::engine::agent_runtime::background::AgentExec>,
                        EngineError,
                    > {
                        let idx = step.index;
                        let (overlays, env) = resolved
                            .get(idx)
                            .ok_or_else(|| {
                                EngineError::Other(format!(
                                    "internal: missing pre-resolved overlays for setup step {idx}",
                                ))
                            })?
                            .as_ref()
                            .map_err(|e| EngineError::Other(e.to_string()))?;
                        if interactive_phase_steps {
                            return Ok(Box::new(InteractivePhaseContainer::new(
                                Arc::clone(&runtime),
                                Arc::clone(&shared_for_factory),
                                step,
                                &base,
                                mount.clone(),
                                env.clone(),
                                overlays.clone(),
                            )));
                        }
                        let container = runtime.start_background(&base, &mount, env, overlays)?;
                        Ok(Box::new(container))
                    };
                    let r = engine.run_phase(
                        PhaseKind::Setup,
                        &setup_steps,
                        &setup_abort_flags,
                        &setup_on_failure_configs,
                        factory,
                    );
                    match &r {
                        Err(e) => shared_for_factory
                            .lock()
                            .unwrap()
                            .write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!("exec workflow: setup phase failed: {e}"),
                            }),
                        // An `abort_on_failure` setup step no longer comes
                        // back as an `Err` — the phase runner reports it in
                        // the outcome (F-34) — so say the same thing here.
                        Ok(outcome) if outcome.aborted => shared_for_factory
                            .lock()
                            .unwrap()
                            .write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: "exec workflow: setup phase failed (abort_on_failure)"
                                    .to_string(),
                            }),
                        Ok(_) => {}
                    }
                    r
                });
                if setup_result.map(|o| o.aborted).unwrap_or(true) {
                    setup_failed = true;
                }
            }
        }

        // === MAIN PHASE ===
        let result = if setup_failed {
            Err(crate::engine::error::EngineError::Container(
                "setup phase failed; main workflow not started".into(),
            ))
        } else {
            engine.run_to_completion().await
        };

        let workflow_succeeded = matches!(
            result,
            Ok(WorkflowOutcome::Completed) | Ok(WorkflowOutcome::CompletedTeardownFailed)
        );

        // === TEARDOWN PHASE ===
        //
        // Same per-entry container pattern as setup: overlays are
        // pre-resolved via `resolve_phase_overlays` and the factory
        // indexes into the results. Unlike setup, no upfront abort
        // gate — per-entry overlay errors flow through the factory and
        // `run_teardown` handles them as per-step failures (best-effort).
        //
        // If the setup or main phase triggered abort_on_failure,
        // teardown is skipped regardless of teardown_on_failure.
        let mut teardown_outcome = crate::engine::workflow::PhaseOutcome::default();
        if !teardown_steps.is_empty() && !engine.abort_on_failure_triggered() {
            let should_run = teardown_on_failure || workflow_succeeded;
            if should_run {
                let base_image = resolve_base_image(&session, &git_root_for_scope);
                let resolved = resolve_phase_overlays(
                    engines,
                    &session,
                    &cli_typed,
                    &teardown_entry_overlays,
                    worktree_git_mount.as_ref(),
                    &base_image,
                );
                let runtime = Arc::clone(
                    engines
                        .require_container_runtime()
                        .map_err(CommandError::from)?,
                );
                let mount = mount_path.clone();
                teardown_outcome = tokio::task::block_in_place(|| {
                    let factory = |step: &PhaseStepRef| -> Result<
                        Box<dyn crate::engine::agent_runtime::background::AgentExec>,
                        EngineError,
                    > {
                        let idx = step.index;
                        let (overlays, env) = resolved
                            .get(idx)
                            .ok_or_else(|| {
                                EngineError::Other(format!(
                                    "internal: missing pre-resolved overlays for teardown step {idx}",
                                ))
                            })?
                            .as_ref()
                            .map_err(|e| EngineError::Other(e.to_string()))?;
                        if interactive_phase_steps {
                            return Ok(Box::new(InteractivePhaseContainer::new(
                                Arc::clone(&runtime),
                                Arc::clone(&shared),
                                step,
                                &base_image,
                                mount.clone(),
                                env.clone(),
                                overlays.clone(),
                            )));
                        }
                        let container =
                            runtime.start_background(&base_image, &mount, env, overlays)?;
                        Ok(Box::new(container))
                    };
                    engine
                        .run_phase(
                            PhaseKind::Teardown,
                            &teardown_steps,
                            &teardown_abort_flags,
                            &teardown_on_failure_configs,
                            factory,
                        )
                        .unwrap_or_default()
                });
            }
        }

        // If any teardown step failed, promote the result to
        // CompletedTeardownFailed so post-workflow flows know.
        let result =
            if (teardown_outcome.aborted || teardown_outcome.any_failed) && workflow_succeeded {
                shared.lock().unwrap().write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: "Workflow completed but one or more teardown steps failed".into(),
                });
                Ok(WorkflowOutcome::CompletedTeardownFailed)
            } else {
                result
            };

        // If teardown didn't run (no teardown steps, or skipped on failure)
        // the engine's current_phase still reads Main — promote it to Done
        // so persisted state reflects completion.
        if !matches!(
            engine.state().current_phase,
            crate::data::workflow_state::WorkflowPhase::Done
        ) {
            let _ = engine.mark_done();
        }

        let mut completed = 0usize;
        let mut failed = 0usize;
        for state in engine.state().step_states.values() {
            match state {
                crate::data::workflow_state::StepState::Succeeded
                | crate::data::workflow_state::StepState::Skipped => completed += 1,
                crate::data::workflow_state::StepState::Failed { .. } => failed += 1,
                _ => {}
            }
        }
        (result, (completed, failed))
    };

    // 8. Reclaim exclusive ownership of the frontend after proxy + factory drop.
    let mut frontend = Arc::try_unwrap(shared)
        .unwrap_or_else(|_| panic!("no other Arc references remain after engine block"))
        .into_inner()
        .unwrap();

    // 9. PTY inactive — flush queued messages.
    frontend.set_pty_active(false);
    frontend.replay_queued();

    // 10. Determine whether the workflow ended with an error.
    // A Pause reached from the step-failure control board leaves a step in
    // `Failed`, so the step tally counts too — otherwise the worktree prompt
    // would greet a broken run with "completed successfully" (WI-0115 §1).
    // Every recovery action clears the failed status, so a run that recovered
    // and finished is not caught by this.
    let had_error = step_counts.1 > 0
        || matches!(
            engine_result,
            Err(_)
                | Ok(WorkflowOutcome::Failed { .. })
                | Ok(WorkflowOutcome::Aborted)
                | Ok(WorkflowOutcome::CompletedTeardownFailed)
        );

    // 11. Report summary.
    //
    // `exit_code` is the unambiguous overall outcome:
    //   Some(0) — workflow completed successfully
    //   Some(N) — a step failed (Failed → failing step's exit code;
    //             Aborted → 1, since the user/engine bailed after a failure)
    //   None    — workflow paused; no terminal status yet
    //
    // Callers (CLI, TUI, API queue worker) inspect this to determine the
    // final success/failure of the run.
    let exit_code = match &engine_result {
        Ok(WorkflowOutcome::Completed) => Some(0),
        Ok(WorkflowOutcome::CompletedTeardownFailed) => Some(1),
        Ok(WorkflowOutcome::Failed { exit_code, .. }) => Some(*exit_code),
        Ok(WorkflowOutcome::Aborted) => Some(1),
        Ok(WorkflowOutcome::Paused) => None,
        Err(_) => Some(1),
    };
    frontend.report_workflow_summary(&WorkflowSummary {
        steps_completed: step_counts.0,
        steps_failed: step_counts.1.max(if had_error { 1 } else { 0 }),
    });

    // 12. Worktree finalize.
    if let Some(lifecycle) = worktree_lifecycle {
        if let Err(e) = lifecycle.finalize(&mut *frontend, had_error).await {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("exec workflow: worktree finalize failed: {e}"),
            });
            return Err(e);
        }
        frontend.replay_queued();
    }

    // 13. Surface engine errors after lifecycle cleanup.
    if let Err(e) = engine_result {
        let err = CommandError::from(e);
        frontend.write_message(UserMessage {
            level: MessageLevel::Error,
            text: format!("exec workflow: workflow engine error: {err}"),
        });
        return Err(err);
    }

    // `_issue_temp_file`'s Drop impl removes the temp file when this
    // function returns — covers both this success path and every early
    // error return above.

    Ok(ExecWorkflowOutcome {
        workflow: workflow_path.display().to_string(),
        exit_code,
        worktree_used: flags.worktree,
    })
}

pub(crate) fn workflow_flag_config(
    flags: &ExecWorkflowCommandFlags,
) -> crate::data::config::FlagConfig {
    crate::data::config::FlagConfig {
        agent: flags.agent.clone(),
        model: flags.model.clone(),
        launch_mode: flags.launch_mode,
        yolo: Some(flags.yolo),
        auto: Some(flags.auto),
        non_interactive: Some(flags.non_interactive),
        overlays_raw: (!flags.overlay.is_empty()).then_some(flags.overlay.clone()),
        work_item: flags
            .work_item
            .as_deref()
            .and_then(|raw| raw.parse::<u32>().ok()),
        max_concurrent_agents: flags.max_concurrent,
        ..Default::default()
    }
}

/// Resolve launch mode for every main workflow step before the engine is
/// constructed.  Returning a complete map makes the result immutable command
/// policy: a later step cannot discover an unsupported ACP agent after an
/// earlier step has already launched.
pub(crate) fn emit_deprecation_warning(agent: &str, sink: &mut dyn UserMessageSink) -> bool {
    let Ok(agent) = crate::data::session::AgentName::new(agent) else {
        return false;
    };
    match crate::command::commands::launch_policy::LaunchPolicy::deprecation_warning(&agent) {
        Some(note) => {
            sink.write_message(note);
            true
        }
        None => false,
    }
}

/// Remove every `checkout_create_branch` setup step from `workflow`, emitting
/// a Warning for each one removed. Called only when the run executes inside an
/// isolated worktree — the worktree already put the run on its own branch, so
/// creating/checking out another branch there is redundant (and would move the
/// worktree off the branch the post-workflow merge dialog operates on).
pub(crate) fn skip_checkout_branch_steps_in_worktree(
    workflow: &mut Workflow,
    sink: &mut dyn UserMessageSink,
) {
    use crate::data::workflow_definition::SetupStep;
    workflow.setup.retain(|entry| match &entry.step {
        SetupStep::CheckoutCreateBranch { branch, .. } => {
            sink.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!(
                    "skipping checkout_create_branch setup step (branch '{branch}'): \
                     the workflow is already running on an isolated worktree branch"
                ),
            });
            false
        }
        _ => true,
    });
}

/// Emit a Warning for each setup/teardown entry that names `context(workflow)`
/// in its overlay list. Workflow step progression state is not available
/// during those phases, so the dynamic prompt fields will be empty.
pub(crate) fn warn_context_workflow_in_phase(workflow: &Workflow, sink: &mut dyn UserMessageSink) {
    fn mentions_context_workflow(overlay: &str) -> bool {
        let t = overlay.trim();
        t.starts_with("context(workflow") && t[..t.len().min(20)].contains("workflow")
    }
    for (i, entry) in workflow.setup.iter().enumerate() {
        if let Some(overlays) = &entry.overlays {
            for o in overlays {
                if mentions_context_workflow(o) {
                    sink.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: format!(
                            "setup step {i}: '{o}': context(workflow) in setup steps has \
                             no workflow step progress to surface yet; the dynamic prompt \
                             will reflect a setup phase only."
                        ),
                    });
                }
            }
        }
    }
    for (i, entry) in workflow.teardown.iter().enumerate() {
        if let Some(overlays) = &entry.overlays {
            for o in overlays {
                if mentions_context_workflow(o) {
                    sink.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: format!(
                            "teardown step {i}: '{o}': context(workflow) in teardown steps \
                             runs after the main workflow has finished; the dynamic prompt \
                             may not reflect live step progression."
                        ),
                    });
                }
            }
        }
    }
}

/// Every agent the workflow will actually launch, under the same precedence
/// the workflow engine uses (`step.agent` > `workflow.agent` > session
/// default), in first-seen order.
///
/// Was `workflow_resolves_to_gemini`, which answered one hard-coded question
/// about one agent name (F-32). The caller now asks the per-agent table
/// whether each one carries a deprecation note.
pub(crate) fn workflow_agents(workflow: &Workflow, session: &Session) -> Vec<String> {
    let workflow_default = workflow.agent.as_deref();
    let session_default = session.default_agent().map(|a| a.as_str().to_string());
    let mut seen: Vec<String> = Vec::new();
    for step in &workflow.steps {
        let resolved = step
            .agent
            .as_deref()
            .or(workflow_default)
            .or(session_default.as_deref());
        if let Some(agent) = resolved {
            if !seen.iter().any(|a| a == agent) {
                seen.push(agent.to_string());
            }
        }
    }
    seen
}

/// Resolve the base image tag for setup/teardown containers.
/// Checks effective config, falls back to the project image tag convention.
pub(crate) fn resolve_base_image(session: &Session, git_root: &std::path::Path) -> String {
    if let Some(configured) = session.effective_config().base_image() {
        return configured;
    }
    crate::data::image_tags::project_image_tag(git_root)
}

/// Collect overlay specs and env vars for a single setup or teardown entry.
///
/// Merges the entry's own overlays with the global / repo / `AWMAN_OVERLAYS`
/// / `--overlay` flag sources, then resolves directories via the overlay
/// engine and captures env vars from the host process environment.
///
/// One call per entry — that's the whole point post-WI-0082: each step's
/// container sees only the entry's own overlays plus the standing sources,
/// not the union of all phase entries' overlays.
pub(crate) fn collect_single_entry_overlays(
    engines: &Engines,
    session: &Session,
    cli_typed: &[TypedOverlay],
    entry_overlays: Option<&[String]>,
    image_tag: Option<&str>,
) -> Result<
    (
        Vec<crate::engine::container::options::OverlaySpec>,
        std::collections::HashMap<String, String>,
    ),
    CommandError,
> {
    let collected =
        session
            .effective_config()
            .collected_overlays(cli_typed.to_vec(), None, entry_overlays)?;

    // Prefer the running image's baked-in $HOME (the actual runtime
    // authority) over what the local Dockerfile.dev says — the two can
    // diverge when the Dockerfile was changed but the image hasn't been
    // rebuilt yet, in which case mounting at the Dockerfile-derived path
    // silently breaks credential passthrough.
    let dockerfile_path = session
        .repo_config()
        .dockerfile_path_or_default(session.git_root());
    // detect_home_from_dockerfile silently returns None when the file is
    // missing — surface that as a warning so a misconfigured `dockerfile`
    // key doesn't cause overlays to fall back to a default container home
    // without any signal to the user.
    if !dockerfile_path.exists() && image_tag.is_none() {
        tracing::warn!(
            "configured Dockerfile {} not found; container home cannot be \
             inferred from it (falling back to overlay engine defaults)",
            dockerfile_path.display()
        );
    }
    let container_home = image_tag
        .and_then(|tag| {
            engines
                .container_runtime
                .as_ref()
                .and_then(|rt| rt.image_home_dir(tag))
        })
        .or_else(|| crate::engine::overlay::detect_home_from_dockerfile(&dockerfile_path));
    let request = crate::engine::overlay::OverlayRequest {
        directories: collected.directories,
        include_all_skills: false,
        named_skills: Vec::new(),
        agent: None,
        yolo: false,
        container_home,
        context_overlays: Vec::new(),
        materialize_credentials: false,
    };
    let overlay_specs = engines
        .overlay_engine
        .build_overlays(session, &request)
        .map_err(|e| {
            CommandError::Other(format!(
                "failed to resolve overlays for setup/teardown container: {e}",
            ))
        })?;

    let mut env = std::collections::HashMap::new();
    for var_name in &collected.env_passthrough {
        if let Some(val) = crate::data::config::env::host_var(var_name) {
            env.insert(var_name.clone(), val);
        }
    }

    Ok((overlay_specs, env))
}

/// Pre-resolve overlay specs and env vars for every entry in a setup or
/// teardown phase.
///
/// Each entry is resolved independently via [`collect_single_entry_overlays`]
/// (per-step overlay isolation, WI-0082). When `worktree_git_mount` is
/// `Some`, the backing `.git` directory overlay is appended to every
/// successful entry so git operations work inside worktree-mounted
/// containers.
///
/// Returns one `Result` per entry. The caller decides error policy:
/// - **Setup** aborts the entire phase on the first `Err`.
/// - **Teardown** passes errors through to the factory; `run_teardown`
///   handles per-step failures gracefully.
pub(crate) type PhaseOverlayResult = Result<
    (
        Vec<crate::engine::container::options::OverlaySpec>,
        std::collections::HashMap<String, String>,
    ),
    CommandError,
>;

pub(crate) fn resolve_phase_overlays(
    engines: &Engines,
    session: &Session,
    cli_typed: &[TypedOverlay],
    entries: &[Option<Vec<String>>],
    worktree_git_mount: Option<&crate::engine::container::options::OverlaySpec>,
    image_tag: &str,
) -> Vec<PhaseOverlayResult> {
    entries
        .iter()
        .map(|entry| {
            let (mut overlays, env) = collect_single_entry_overlays(
                engines,
                session,
                cli_typed,
                entry.as_deref(),
                Some(image_tag),
            )?;
            if let Some(wt) = worktree_git_mount {
                overlays.push(wt.clone());
            }
            Ok((overlays, env))
        })
        .collect()
}
