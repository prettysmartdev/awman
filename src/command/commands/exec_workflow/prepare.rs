//! Everything that has to be true before a workflow can run: the persisted
//! state resume offer, the overlays and base image a phase needs, the
//! workflow mutations an isolated worktree implies, and `PreparedRun`, which
//! both the plain and the dynamic path hand to `execute_prepared`.
//!
//! Split out of `commands/exec_workflow.rs` by WI 0114 F-51. A child module
//! of `exec_workflow`, so it reaches that module's private items unchanged.

use super::*;
use crate::command::commands::worktree_lifecycle::{FinalizeOutcome, MergeFailure};
use crate::data::config::effective::EffectiveConfig;
use crate::data::merge_fix_prompt::{
    render_merge_fix_prompt, MergeFixPromptVars, MERGE_FIX_PROMPT_MD,
};
use crate::data::session::AgentName;
use crate::engine::workflow::{PhaseStepRef, RemediationAgentSpec};

/// Emit the deprecation warning for `agent`, if its vendor has deprecated it.
///
/// The wording is the one per-agent table's
/// (`AgentDefinition::deprecation_note`, reached through
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
    /// The run's workflow prerequisites (WI 0149): needs, report, grants and
    /// the redaction set. Grants live here for the run and nowhere else.
    pub(crate) prerequisites:
        Box<crate::command::commands::workflow_prerequisites::PreparedPrerequisites>,
}

/// Execute a fully-prepared workflow: persisted-state resume check, engine
/// setup/main/teardown phases, summary reporting, and worktree finalize.
/// Shared by the non-dynamic and dynamic execution paths.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_prepared(
    flags: &ExecWorkflowCommandFlags,
    engines: &Engines,
    prepared: PreparedRun,
    frontend: SharedWorkflowFrontend,
    squad_identity: Option<&crate::engine::squad::launcher::SquadContainerIdentity>,
    task_workspace: Option<&Path>,
    workflow_state_root: Option<&Path>,
    managed_session: Option<&Arc<tokio::sync::RwLock<Session>>>,
    cancel: &tokio_util::sync::CancellationToken,
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
        prerequisites,
    } = prepared;
    let mut prerequisites = prerequisites;
    // The pre-teardown re-check probes the checkout the steps really mount.
    prerequisites.flow.set_worktree(worktree_path.clone());

    // Resolved from the invoking session: the worktree session's config is
    // whatever was committed on the branch, not what the user has now.
    let finalize_policy = finalize_policy(flags, &original_session);

    // When the run is inside an isolated worktree (--worktree, or implied by
    // --yolo/--dynamic), any `checkout_create_branch` setup step is redundant:
    // the worktree already put the run on its own branch. Skip-and-warn, not
    // a failure.
    if worktree_path.is_some() {
        frontend.with(|frontend| skip_checkout_branch_steps_in_worktree(&mut workflow, frontend));
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
            None => session_root_for_state,
        };
        let store =
            crate::data::workflow_state_store::WorkflowStateStore::at_git_root(git_root_for_state);
        let workflow_name = crate::engine::workflow::workflow_name_for(&workflow);
        let decision = frontend.with(|frontend| {
            offer_state_resume(
                &store,
                &workflow,
                &workflow_name,
                work_item_context.as_ref().map(|c| c.number),
                worktree_path.as_deref(),
                frontend,
            )
        })?;
        if decision == StateResumeOutcome::Cancelled {
            frontend.with(|frontend| {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: "exec workflow: cancelled; the saved run is unchanged.".to_string(),
                })
            });
            return Ok(ExecWorkflowOutcome {
                prerequisites: None,
                workflow: workflow_name,
                exit_code: None,
                worktree_used: worktree_path.is_some(),
            });
        }
    }

    // 6. Set PTY active — queues user messages during the engine run.
    frontend.set_pty_active(true);
    let shared = frontend.clone();

    let flags_arc = Arc::new(flags.clone());

    // `agentFlags` come from the invoking session, captured before it is
    // swapped for the worktree session below. `--agent-flag` targets the
    // workflow's default agent: the workflow `agent` field, else `--agent`,
    // else config — the agent a step runs when it names none.
    let workflow_agent_flags = super::factory::workflow_agent_flags(
        &original_session,
        workflow.agent.clone().or_else(|| flags.agent.clone()),
        flags.agent_flags.clone(),
    );

    // A merge-fix agent (after a failed merge-back) mounts the parent
    // checkout, not the worktree, so keep the invoking session to re-root it
    // there; it is moved into the worktree session below.
    let merge_fix_invoker = worktree_lifecycle
        .as_ref()
        .map(|_| original_session.clone());
    let merge_fix_work_item = work_item_context.clone();

    // 8. Build the session for the engine.
    // When a worktree is active, re-root the session at the worktree so
    // that `build_options` mounts the worktree checkout, not the main repo.
    let mut session = if let Some(ref wt) = worktree_path {
        let git_root_for_session = wt.clone();
        // The invoker's builtin block, default agent, env and global config
        // carry over: the worktree checkout lacks uncommitted edits.
        match Session::open_worktree_for(wt.clone(), git_root_for_session, &original_session) {
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
    // The engine outlives this block: a merge-fix agent after the worktree
    // finalize runs through its agent launch path.
    let (engine_result, step_counts, mut engine) = {
        let proxy = shared.clone();
        let factory = CommandLayerFactory {
            shared: shared.clone(),
            engines: engines.clone(),
            flags: Arc::clone(&flags_arc),
            cli_typed_overlays: cli_typed.clone(),
            work_item_context,
            image_git_root: git_root_for_scope.clone(),
            workflow_overlays: workflow_overlays_for_factory,
            squad_identity: squad_identity.cloned(),
            task_workspace: task_workspace.map(Path::to_path_buf),
            launch_modes: Arc::new(launch_modes),
            agent_flags: workflow_agent_flags,
        };
        let cleanup_workflow = workflow.clone();
        let cleanup_work_item = engine_work_item_context
            .as_ref()
            .map(|wi| (wi.number, wi.content.clone()));
        let mut engine = match WorkflowEngine::resume(
            &session,
            crate::engine::workflow::WorkflowSpec::new(workflow)
                .with_work_item_context(engine_work_item_context)
                .with_state_root(workflow_state_root.map(Path::to_path_buf))
                // poll_ci runs on the host with the granted token, never the
                // ambient GITHUB_TOKEN.
                .with_ci_token(
                    prerequisites
                        .remote("origin")
                        .and_then(|remote| prerequisites.grants.token_for_remote(&remote)),
                ),
            crate::engine::workflow::WorkflowEngineDeps {
                frontend: proxy.into_workflow_frontend(),
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
        engine.set_ci_destination(prerequisites.remote("origin").map(|r| r.redacted));
        engine.set_ci_host_binaries(prerequisites.flow.options().custom_agent_binaries.clone());
        engine.set_cancellation(cancel.clone());
        engine.set_parallel_group_prompts(crate::command::prompts::parallel_group_prompts());
        engine.set_workflow_context_permission(active_workflow_context_permission);
        if let Err(e) = engine.declare_phase_steps(&setup_steps, &teardown_steps) {
            shared.lock().unwrap().write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("exec workflow: failed to declare phase steps: {e}"),
            });
            return Err(CommandError::from(e));
        }

        // The commit_changes identity is a workflow prerequisite (WI 0149): it
        // was checked against what the container sees, and granted, before
        // the run started. The run's one redaction set covers every granted
        // token in step output and failure files.
        engine.set_redaction(prerequisites.redaction.clone());
        let mut step_command_options: Vec<(
            (PhaseKind, usize),
            crate::engine::workflow::step_commands::StepCommandOptions,
        )> = Vec::new();

        // When a person is at the frontend (the CLI on a TTY, the TUI), each
        // setup/teardown shell step runs as a foreground, PTY-attached
        // container they can watch and type into, like an agent step.
        // Everything else — the API server, the squad daemon, a
        // `--non-interactive` CLI — keeps the headless background container.
        // A squad-generated workflow is always headless, whichever frontend
        // is driving it.
        let interactive_phase_steps = prerequisites.redaction.is_empty()
            && squad_identity.is_none()
            && shared.lock().unwrap().supports_interactive_recovery();

        shared.lock().unwrap().record_cancel_cleanup(
            &crate::data::workflow_cleanup::WorkflowCleanup {
                workflow: cleanup_workflow,
                work_item: cleanup_work_item,
                workdir: session.working_dir().to_path_buf(),
                mount_path: mount_path.clone(),
                state_root: workflow_state_root.map(Path::to_path_buf),
                image: resolve_phase_image(
                    &session,
                    &git_root_for_scope,
                    engines.runtime.capabilities().image_acquisition,
                ),
                cli_overlays: cli_typed.clone(),
                worktree_git_mount: worktree_git_mount
                    .as_ref()
                    .map(|m| (m.host_path.clone(), m.container_path.clone(), m.permission)),
            },
        )?;

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
            let base_image = resolve_phase_image(
                &session,
                &git_root_for_scope,
                engines.runtime.capabilities().image_acquisition,
            );
            let mut resolved = resolve_phase_overlays(
                engines,
                &session,
                &cli_typed,
                &setup_entry_overlays,
                worktree_git_mount.as_ref(),
                &base_image,
            );
            // Grant overlays go after the user's and the step's own, and only
            // to the step kinds that receive grants (pull_branch, clone_repo).
            // `_setup_grants` keeps their scratch files alive for the phase.
            let (_setup_grants, setup_options) = apply_grant_overlays(
                engines,
                &prerequisites,
                PhaseKind::Setup,
                &setup_grant_kinds(&setup_steps),
                &mut resolved,
                &base_image,
                &mut **shared.lock().unwrap(),
            );
            step_command_options.extend(setup_options);
            engine.set_phase_step_command_options(step_command_options.clone());

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
                let shared_for_factory = shared.clone();
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
                        super::phase_container::PhaseContainerFactory {
                            name_prefix: engines.runtime.container_name_prefix(),
                            runtime: &runtime,
                            frontend: &shared_for_factory,
                            agent_engine: &engines.agent_engine,
                            session: &session,
                            image: &base,
                            workdir: &mount,
                            interactive: interactive_phase_steps,
                            secret_env: _setup_grants
                                .iter()
                                .find(|(i, _)| *i == idx)
                                .map_or(&[], |(_, g)| g.secret_env.as_slice()),
                            transport: _setup_grants
                                .iter()
                                .find(|(i, _)| *i == idx)
                                .and_then(|(_, g)| g.transport.as_ref()),
                        }
                        .build(step, env, overlays)
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
        //
        // A cancel during setup stops the phase before its next step; the
        // main phase then never starts and the run ends cancelled. The cancel
        // takes precedence over a setup failure it caused or raced.
        let result = if engine.cancel_requested() {
            engine.cancel_run()
        } else if setup_failed {
            Err(crate::engine::error::EngineError::Container(
                "setup phase failed; main workflow not started".into(),
            ))
        } else {
            engine.run_to_completion().await
        };
        // A runtime wait error may race the token too. Reconcile before
        // deciding which teardown policy to apply, rather than after cleanup.
        let result =
            if engine.cancel_requested() && !matches!(result, Ok(WorkflowOutcome::Cancelled)) {
                engine.cancel_run()
            } else {
                result
            };
        let cancelled = matches!(result, Ok(WorkflowOutcome::Cancelled));

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
        //
        // A cancelled run always tears down, in cancelled mode: push, pull
        // request and CI-poll steps are skipped, cleanup steps still run
        // (without `on_failure` remediation).
        let mut teardown_outcome = crate::engine::workflow::PhaseOutcome::default();
        if !teardown_steps.is_empty() && !engine.abort_on_failure_triggered() {
            let should_run = cancelled || teardown_on_failure || workflow_succeeded;
            if should_run {
                let base_image = resolve_phase_image(
                    &session,
                    &git_root_for_scope,
                    engines.runtime.capabilities().image_acquisition,
                );
                // Re-check the prerequisites without prompting (WI 0149 §3):
                // a granted env var may have gone, and a `refresh:
                // "beforeTeardown"` token command runs again. Gaps become one
                // warning here rather than N step failures. A cancelled
                // teardown skips push, PR and poll_ci, so it skips this too.
                if !cancelled {
                    let crate::command::commands::workflow_prerequisites::PreparedPrerequisites {
                        flow,
                        needs,
                        grants,
                        redaction,
                        remotes,
                        ..
                    } = &mut *prerequisites;
                    let report = flow.recheck_before_teardown(
                        needs,
                        grants,
                        redaction,
                        &mut **shared.lock().unwrap(),
                    );
                    if let Some(session) = managed_session {
                        session
                            .write()
                            .await
                            .state_mut()
                            .record_workflow_prerequisites(report);
                    }
                    engine.set_ci_token(
                        remotes
                            .get("origin")
                            .and_then(|remote| grants.token_for_remote(remote)),
                    );
                }
                if cancelled {
                    prerequisites.grants = Default::default();
                    engine.set_ci_token(None);
                }
                let mut resolved = resolve_phase_overlays(
                    engines,
                    &session,
                    &cli_typed,
                    &teardown_entry_overlays,
                    worktree_git_mount.as_ref(),
                    &base_image,
                );
                // `_teardown_grants` keeps the grant scratch files (the
                // writable known_hosts copy) alive until the phase ends.
                let (_teardown_grants, teardown_options) = apply_grant_overlays(
                    engines,
                    &prerequisites,
                    PhaseKind::Teardown,
                    &teardown_grant_kinds(&teardown_steps),
                    &mut resolved,
                    &base_image,
                    &mut **shared.lock().unwrap(),
                );
                step_command_options.extend(teardown_options);
                engine.set_phase_step_command_options(step_command_options.clone());
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
                        super::phase_container::PhaseContainerFactory {
                            name_prefix: engines.runtime.container_name_prefix(),
                            runtime: &runtime,
                            frontend: &shared,
                            agent_engine: &engines.agent_engine,
                            session: &session,
                            image: &base_image,
                            workdir: &mount,
                            interactive: interactive_phase_steps,
                            secret_env: _teardown_grants
                                .iter()
                                .find(|(i, _)| *i == idx)
                                .map_or(&[], |(_, g)| g.secret_env.as_slice()),
                            transport: _teardown_grants
                                .iter()
                                .find(|(i, _)| *i == idx)
                                .and_then(|(_, g)| g.transport.as_ref()),
                        }
                        .build(step, env, overlays)
                    };
                    if cancelled {
                        engine
                            .run_teardown_cancelled(&teardown_steps, &teardown_abort_flags, factory)
                            .unwrap_or_default()
                    } else {
                        engine
                            .run_phase(
                                PhaseKind::Teardown,
                                &teardown_steps,
                                &teardown_abort_flags,
                                &teardown_on_failure_configs,
                                factory,
                            )
                            .unwrap_or_default()
                    }
                });
            }
        }

        // A cancel that arrived during a normal teardown (its publishing steps
        // were then skipped) cancels the run: the command had not finished.
        // So does one that arrived after the main phase with no teardown to
        // run; only a command that finished first keeps its result.
        let cancelled_late = !cancelled
            && (teardown_outcome.cancelled || engine.cancel_requested())
            && !engine.abort_on_failure_triggered();
        // If any teardown step failed, promote the result to
        // CompletedTeardownFailed so post-workflow flows know.
        let result = if cancelled_late {
            engine.cancel_run()
        } else if (teardown_outcome.aborted || teardown_outcome.any_failed) && workflow_succeeded {
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
        (result, (completed, failed), engine)
    };

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
                | Ok(WorkflowOutcome::Cancelled)
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
        // SIGINT convention, as `CommandError::Aborted` maps to.
        Ok(WorkflowOutcome::Cancelled) => Some(crate::data::command_status::CANCELLED_EXIT_CODE),
        Ok(WorkflowOutcome::Paused) => None,
        Err(_) => Some(1),
    };
    frontend.with(|frontend| {
        frontend.report_workflow_summary(&WorkflowSummary {
            steps_completed: step_counts.0,
            steps_failed: step_counts.1.max(if had_error { 1 } else { 0 }),
        })
    });

    // 12. Worktree finalize.
    if let Some(lifecycle) = worktree_lifecycle {
        let result = match frontend
            .finalize_worktree(&lifecycle, had_error, finalize_policy)
            .await
        {
            Ok(FinalizeOutcome::Done) => Ok(()),
            Ok(FinalizeOutcome::MergeFixRequested(failure)) => {
                // The frontend lock is released here: the agent reports
                // through the same frontend (see `finalize_worktree`).
                let agent_ran = run_merge_fix_agent(
                    &mut engine,
                    flags,
                    merge_fix_invoker.as_ref(),
                    &failure,
                    merge_fix_work_item.as_ref(),
                    &frontend,
                )
                .await;
                frontend
                    .complete_merge_after_fix(&lifecycle, &failure, agent_ran)
                    .await
            }
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            frontend.with(|frontend| {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec workflow: worktree finalize failed: {e}"),
                })
            });
            return Err(e);
        }
        frontend.replay_queued();
    }

    // 13. Surface engine errors after lifecycle cleanup.
    if let Err(e) = engine_result {
        let err = CommandError::from(e);
        frontend.with(|frontend| {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("exec workflow: workflow engine error: {err}"),
            })
        });
        return Err(err);
    }

    // `_issue_temp_file`'s Drop impl removes the temp file when this
    // function returns — covers both this success path and every early
    // error return above.

    Ok(ExecWorkflowOutcome {
        prerequisites: None,
        workflow: workflow_path.display().to_string(),
        exit_code,
        worktree_used: worktree_path.is_some(),
    })
}

/// The merge-fix agent's rendered prompt for `failure`.
///
/// The template is `worktree.mergeFix.promptTemplate` (a path relative to
/// the git root) when configured, else the built-in
/// [`MERGE_FIX_PROMPT_MD`]. A configured file that cannot be read is an
/// error: silently falling back would run an agent with instructions the
/// user did not choose.
pub(crate) fn merge_fix_prompt(
    config: &EffectiveConfig,
    failure: &MergeFailure,
    work_item: Option<&WorkItemContext>,
) -> Result<crate::data::workflow_prompt_template::Substitution, CommandError> {
    let template = match config.worktree_merge_fix_prompt_template() {
        Some(relative) => {
            let path = failure.git_root.join(&relative);
            crate::data::fs::repo_file::read_template(
                &failure.git_root,
                std::path::Path::new(&relative),
            )
            .map_err(|e| {
                CommandError::Other(format!(
                    "reading worktree.mergeFix.promptTemplate {}: {e}",
                    path.display()
                ))
            })?
        }
        None => MERGE_FIX_PROMPT_MD.to_string(),
    };
    Ok(render_merge_fix_prompt(
        &template,
        &MergeFixPromptVars {
            merge_command: &failure.command,
            merge_exit_code: failure.exit_code,
            merge_output: &failure.output,
            branch: &failure.branch,
            target_branch: &failure.target_branch,
            unmerged_paths: &failure.unmerged_paths,
        },
        work_item,
    ))
}

/// The agent and model the merge-fix agent runs: `--agent`/`--model` when
/// given, else the workflow's last step's, else the session's configured
/// agent.
pub(crate) fn merge_fix_agent(
    flags: &ExecWorkflowCommandFlags,
    last_step_agent: Option<AgentName>,
    last_step_model: Option<String>,
    session: &Session,
) -> Result<(AgentName, Option<String>), CommandError> {
    let agent = match &flags.agent {
        Some(name) => AgentName::new(name.as_str()).map_err(CommandError::from)?,
        None => match last_step_agent.or_else(|| session.default_agent().cloned()) {
            Some(agent) => agent,
            None => {
                return Err(CommandError::Other(
                    "no agent to run the merge fix with; pass --agent".to_string(),
                ))
            }
        },
    };
    let model = flags.model.clone().or(last_step_model);
    Ok((agent, model))
}

/// Run the merge-fix agent for a failed worktree merge-back and wait for it.
/// Returns whether the agent ran; a failure to start it is reported and the
/// caller falls back to keeping the worktree.
///
/// The agent runs in a container through the same launch path as an
/// `on_failure` remediation agent (`WorkflowEngine::run_remediation_agent`),
/// never on the host. Its session is re-rooted at the parent checkout's git
/// root, where the merge is in progress — the normal mount scope.
async fn run_merge_fix_agent(
    engine: &mut WorkflowEngine,
    flags: &ExecWorkflowCommandFlags,
    invoker: Option<&Session>,
    failure: &MergeFailure,
    work_item: Option<&WorkItemContext>,
    frontend: &SharedWorkflowFrontend,
) -> bool {
    let warn = |text: String| {
        frontend.with(|frontend| {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text,
            })
        })
    };
    let Some(invoker) = invoker else {
        warn("merge-fix: no invoking session to mount the checkout with".to_string());
        return false;
    };
    let mut session = match Session::open_worktree_for(
        failure.git_root.clone(),
        failure.git_root.clone(),
        invoker,
    ) {
        Ok(session) => session,
        Err(e) => {
            warn(format!(
                "merge-fix: opening a session at {}: {e}",
                failure.git_root.display()
            ));
            return false;
        }
    };
    session.set_flags(workflow_flag_config(flags));

    let prompt = match merge_fix_prompt(&session.effective_config(), failure, work_item) {
        Ok(prompt) => prompt,
        Err(e) => {
            warn(format!("merge-fix: {e}"));
            return false;
        }
    };
    for warning in prompt.warnings {
        warn(format!("merge-fix: {warning}"));
    }
    let (agent, model) = match merge_fix_agent(
        flags,
        engine.last_step_agent(),
        engine.last_step_model(),
        &session,
    ) {
        Ok(resolved) => resolved,
        Err(e) => {
            warn(format!("merge-fix: {e}"));
            return false;
        }
    };

    frontend.with(|frontend| {
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!(
                "launching {} to fix the merge of '{}' in {}",
                agent.as_str(),
                failure.branch,
                failure.git_root.display()
            ),
        })
    });
    let spec = RemediationAgentSpec {
        label: format!("merge-fix: {}", failure.branch),
        prompt: prompt.rendered,
        agent,
        model,
        extra_overlays: None,
    };
    // Same PTY bracketing as the engine run: messages queue while the
    // agent owns the terminal and replay once it exits.
    frontend.set_pty_active(true);
    let result = engine.run_remediation_agent(&session, spec).await;
    frontend.set_pty_active(false);
    frontend.replay_queued();
    match result {
        Ok(exit_code) => {
            tracing::info!(exit_code, "merge-fix agent exited");
            true
        }
        Err(e @ crate::engine::workflow::RemediationAgentError::Launch(_)) => {
            warn(format!("merge-fix: {e}"));
            false
        }
        // The agent started; whatever it changed is verified as usual.
        Err(e @ crate::engine::workflow::RemediationAgentError::Execution(_)) => {
            warn(format!("merge-fix: {e}"));
            true
        }
    }
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

/// The merge-back policy for this run: flag > repo config > global config.
/// `None` means "ask", which the frontend answers from its headless profile
/// when nobody is attached.
pub(crate) fn finalize_policy(
    flags: &ExecWorkflowCommandFlags,
    session: &Session,
) -> FinalizePolicy {
    let config = session.effective_config();
    FinalizePolicy {
        on_dirty_merge_target: flags
            .on_dirty_merge_target
            .or_else(|| config.worktree_on_dirty_merge_target()),
        on_merge_failure: flags
            .on_merge_failure
            .or_else(|| config.worktree_merge_fix_on_failure()),
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

pub(crate) fn resolve_phase_image(
    session: &Session,
    git_root: &std::path::Path,
    acquisition: crate::engine::agent_runtime::ImageAcquisition,
) -> String {
    if acquisition == crate::engine::agent_runtime::ImageAcquisition::Import
        && session.effective_config().base_image().is_none()
    {
        // The project image is a build input under an importing runtime.
        // Its agent image contains that base and is the cached runnable image.
        let agent = session
            .effective_config()
            .agent()
            .unwrap_or_else(|| "claude".into());
        return crate::data::image_tags::agent_image_tag(git_root, &agent);
    }
    resolve_base_image(session, git_root)
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
        definition: None,
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

/// Recovery never dispatches setup, main, a leader, or remediation. It uses
/// the inputs saved before the original run and the existing phase statuses.
pub(crate) async fn finish_cancelled_cleanup(
    engines: &Engines,
    original_session: &Session,
    cleanup: crate::data::workflow_cleanup::WorkflowCleanup,
    frontend: SharedWorkflowFrontend,
) -> Result<(), CommandError> {
    let session = Session::open_worktree_for(
        cleanup.workdir.clone(),
        cleanup.workdir.clone(),
        original_session,
    )?;
    let steps: Vec<_> = cleanup
        .workflow
        .teardown
        .iter()
        .map(|e| e.step.clone())
        .collect();
    let aborts: Vec<_> = cleanup
        .workflow
        .teardown
        .iter()
        .map(|e| e.abort_on_failure)
        .collect();
    let entries: Vec<_> = cleanup
        .workflow
        .teardown
        .iter()
        .map(|e| e.overlays.clone())
        .collect();
    let mut engine = WorkflowEngine::resume(
        &session,
        crate::engine::workflow::WorkflowSpec::new(cleanup.workflow)
            .with_work_item_context(
                cleanup
                    .work_item
                    .map(|(number, content)| WorkItemContext { number, content }),
            )
            .with_state_root(cleanup.state_root),
        crate::engine::workflow::WorkflowEngineDeps {
            frontend: frontend.clone().into_workflow_frontend(),
            agent_factory: Box::new(CleanupOnlyFactory),
        },
    )
    .await?;
    engine.cancel_run()?;
    if !engine.state().teardown_completed && !steps.is_empty() {
        let mount = cleanup
            .worktree_git_mount
            .map(|(host_path, container_path, permission)| {
                crate::engine::container::options::OverlaySpec {
                    host_path,
                    container_path,
                    permission,
                }
            });
        let resolved = resolve_phase_overlays(
            engines,
            &session,
            &cleanup.cli_overlays,
            &entries,
            mount.as_ref(),
            &cleanup.image,
        );
        let runtime = engines.require_container_runtime()?;
        tokio::task::block_in_place(|| {
            engine.resume_teardown_cancelled(&steps, &aborts, |step| {
                let (overlays, env) = resolved[step.index]
                    .as_ref()
                    .map_err(|e| EngineError::Other(e.to_string()))?;
                super::phase_container::PhaseContainerFactory {
                    name_prefix: engines.runtime.container_name_prefix(),
                    runtime,
                    frontend: &frontend,
                    agent_engine: &engines.agent_engine,
                    session: &session,
                    image: &cleanup.image,
                    workdir: &cleanup.mount_path,
                    interactive: false,
                    secret_env: &[],
                    transport: None,
                }
                .build(step, env, overlays)
            })
        })?;
    }
    engine.mark_done()?;
    Ok(())
}

struct CleanupOnlyFactory;
impl crate::engine::workflow::factory::AgentExecutionFactory for CleanupOnlyFactory {
    fn execution_for_step(
        &self,
        _: &WorkflowStep,
        _: &Session,
        _: &crate::engine::workflow::factory::WorkflowRuntimeContext,
    ) -> Result<crate::engine::agent_runtime::execution::AgentExecution, EngineError> {
        Err(EngineError::Other(
            "cancel recovery cannot launch agents".into(),
        ))
    }
    fn inject_prompt(
        &self,
        _: &crate::engine::agent_runtime::execution::AgentExecution,
        _: &str,
    ) -> Result<Option<()>, EngineError> {
        Ok(None)
    }
}

#[cfg(test)]
mod compat_tests {
    use super::*;

    #[test]
    fn import_setup_uses_runnable_agent_image_while_build_keeps_project_image() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let session = Session::for_tests_isolated(root.path(), home.path());
        let project = crate::data::image_tags::project_image_tag(root.path());
        let agent = crate::data::image_tags::agent_image_tag(root.path(), "claude");
        assert_eq!(
            resolve_phase_image(
                &session,
                root.path(),
                crate::engine::agent_runtime::ImageAcquisition::Build
            ),
            project
        );
        assert_eq!(
            resolve_phase_image(
                &session,
                root.path(),
                crate::engine::agent_runtime::ImageAcquisition::Import
            ),
            agent
        );
    }
}
