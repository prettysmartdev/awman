//! Dynamic workflows (WI-0092, WI-0115 §2): discovering a previous run,
//! offering to resume it, driving the leader agent, and its control board.
//!
//! Split out of `commands/exec_workflow.rs` by WI 0114 F-51. A child module
//! of `exec_workflow`, so it reaches that module's private items unchanged.

use super::*;

// ─── Dynamic-workflow resume (WI-0115 §2) ────────────────────────────────────

/// A previous `--dynamic` run recovered from disk: the workflow its leader
/// designed, and the engine state that run left behind.
#[derive(Debug)]
pub(crate) struct PreviousDynamicRun {
    pub(crate) workflow: Workflow,
    /// The saved `dynamic-NNNN.toml` the workflow was parsed from.
    pub(crate) workflow_path: PathBuf,
    pub(crate) state: crate::data::workflow_state::WorkflowState,
}

/// Why [`discover_previous_dynamic_run`] found no resumable run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DynamicDiscoveryMiss {
    /// There is simply no previous run recorded here. Ordinary and silent —
    /// a worktree kept after a clean run looks exactly like this, and telling
    /// the user a completed run "cannot be resumed" would be noise.
    NothingToResume,
    /// A previous run left something behind, but it cannot be reconstructed.
    /// The string says what is missing, phrased for the user.
    Unusable(String),
}

/// Look for a resumable dynamic run inside `worktree_path`.
///
/// A run is resumable only as a pair — the leader's `workflow.toml` and the
/// engine state that references it. Neither half present is
/// [`DynamicDiscoveryMiss::NothingToResume`]; one half present without the
/// other is [`DynamicDiscoveryMiss::Unusable`], which is worth interrupting the
/// user over because it means a run *was* there and something ate half of it.
pub(crate) fn discover_previous_dynamic_run(
    state_git_root: &Path,
    work_item: u32,
) -> Result<PreviousDynamicRun, DynamicDiscoveryMiss> {
    use crate::data::fs::WorkflowDirs;
    use crate::data::workflow_state_store::WorkflowStateStore;
    use DynamicDiscoveryMiss::{NothingToResume, Unusable};

    let workflow_path = WorkflowDirs::dynamic_workflow_path(state_git_root, work_item);
    if !workflow_path.exists() {
        // A run that finished cleanly deletes its saved workflow but keeps its
        // (all-succeeded) state file, so a kept worktree lands here routinely.
        // Only a state file with work still left in it means something is
        // genuinely missing.
        return Err(
            match unresumed_dynamic_state_exists(state_git_root, work_item) {
                true => Unusable(format!(
                    "no saved workflow.toml at {} — the previous run's generated workflow is gone",
                    workflow_path.display()
                )),
                false => NothingToResume,
            },
        );
    }
    let raw = std::fs::read_to_string(&workflow_path)
        .map_err(|e| Unusable(format!("reading {}: {e}", workflow_path.display())))?;
    let workflow = Workflow::parse(&raw, crate::data::workflow_definition::WorkflowFormat::Toml)
        .map_err(|e| Unusable(format!("the saved workflow.toml no longer parses: {e}")))?;

    let workflow_name = crate::engine::workflow::workflow_name_for(&workflow);
    let store = WorkflowStateStore::at_git_root(state_git_root.to_path_buf());
    let state = match store.load(Some(work_item), &workflow_name) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return Err(Unusable(format!(
                "no saved workflow state for '{workflow_name}' at {} — there is no progress to \
                 resume from",
                store.state_path(Some(work_item), &workflow_name).display()
            )))
        }
        Err(e) => return Err(Unusable(format!("reading the saved workflow state: {e}"))),
    };

    Ok(PreviousDynamicRun {
        workflow,
        workflow_path,
        state,
    })
}

/// Does `state_git_root` hold a dynamic workflow state for `work_item` that
/// still has steps left to run?
///
/// Used to tell "the previous run finished and tidied up after itself" apart
/// from "the previous run's generated workflow went missing". The state file is
/// named after the workflow title, which is exactly what the missing
/// `workflow.toml` would have told us — so this scans the workflows directory
/// for the work item's state file rather than guessing the title.
pub(crate) fn unresumed_dynamic_state_exists(state_git_root: &Path, work_item: u32) -> bool {
    let dir = crate::data::fs::WorkflowDirs::repo_dir_for(state_git_root);
    let marker = format!("-{work_item:04}-");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json")
            || !path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(&marker))
        {
            return false;
        }
        match crate::data::workflow_state_store::WorkflowStateStore::read_state_path(&path) {
            // A state whose every step succeeded or was skipped has nothing
            // left to resume, so its workflow.toml is not missed. Note this
            // deliberately is not `is_complete()`: a failed or aborted run is
            // "complete" by that predicate — every step terminal — and it is
            // exactly the run whose missing workflow the user needs told about.
            Ok(Some(state)) => completed_step_count(&state) < state.step_states.len(),
            _ => false,
        }
    })
}

/// What [`ExecWorkflowCommand::offer_dynamic_resume`] decided.
pub(crate) enum DynamicResumeOutcome {
    /// Resume the previous run from the chosen step; no leader runs.
    /// Boxed because the plan is far larger than the other two variants.
    Resume(Box<DynamicResumePlan>),
    /// Design a new workflow with a fresh leader pass.
    Fresh,
    /// The user cancelled the command; nothing was created or deleted.
    Cancelled,
}

/// A resume the user confirmed: which saved workflow to run, and from where.
pub(crate) struct DynamicResumePlan {
    workflow: Workflow,
    workflow_path: PathBuf,
    state: crate::data::workflow_state::WorkflowState,
    /// Validated step graph of the saved workflow.
    dag: crate::data::workflow_dag::WorkflowDag,
    /// The step the user chose to start from.
    start_step: String,
    /// Root the rewritten state must be written back to.
    state_root: PathBuf,
}

/// Everything [`ExecWorkflowCommand::execute_generated_workflow`] needs. Both
/// dynamic paths — leader-designed and resumed — fill one in.
pub(crate) struct DynamicExecution {
    effective_flags: ExecWorkflowCommandFlags,
    workflow: Workflow,
    workflow_path: PathBuf,
    work_item_context: WorkItemContext,
    /// Session re-rooted at the worktree; agent/image validation runs against it.
    worktree_session: Session,
    paths: crate::data::RepoDockerfilePaths,
    git_root_for_scope: PathBuf,
    cwd: PathBuf,
    base_session: Session,
    mount_path: PathBuf,
    worktree_path: PathBuf,
    lifecycle: WorktreeLifecycle,
    worktree_git_mount: Option<crate::engine::container::options::OverlaySpec>,
    skip_state_resume_prompt: bool,
    frontend: Box<dyn ExecWorkflowCommandFrontend>,
}

/// Copy the leader's generated `workflow.toml` to its stable per-work-item path
/// inside the worktree. Best-effort: a failure costs a later resume, never the
/// run in progress.
pub(crate) fn save_dynamic_workflow_copy(
    state_git_root: &Path,
    work_item: u32,
    generated_path: &Path,
) {
    let dest = crate::data::fs::WorkflowDirs::dynamic_workflow_path(state_git_root, work_item);
    let result = dest
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|()| std::fs::copy(generated_path, &dest).map(|_| ()));
    if let Err(e) = result {
        tracing::warn!(
            dest = %dest.display(),
            error = %e,
            "failed to save the dynamic workflow copy; this run will not be resumable"
        );
    }
}

/// Retire both halves of a resumable dynamic run — the saved `workflow.toml`
/// and the engine state that references it (WI-0115 §2b).
///
/// Called when a dynamic run finishes with exit code 0, from both the
/// leader-designed and the resumed path. Best-effort: a worktree the user kept
/// is only tidier for this, and failing to delete it must not fail the run.
/// Both must go together — a state file with no workflow beside it is what
/// produces a spurious "cannot be resumed" notice on the next invocation.
pub(crate) fn clear_dynamic_resume_artifacts(
    state_git_root: &Path,
    work_item: u32,
    workflow_name: &str,
) {
    let _ = std::fs::remove_file(crate::data::fs::WorkflowDirs::dynamic_workflow_path(
        state_git_root,
        work_item,
    ));
    let store = crate::data::workflow_state_store::WorkflowStateStore::at_git_root(
        state_git_root.to_path_buf(),
    );
    if let Err(e) = store.delete(Some(work_item), workflow_name) {
        tracing::warn!(
            work_item,
            workflow = %workflow_name,
            error = %e,
            "failed to delete the finished dynamic run's workflow state"
        );
    }
}

/// Outcome of driving a single leader/repair agent attempt through the stuck →
/// yolo countdown → auto-advance pipeline.
pub(crate) enum LeaderDriveOutcome {
    /// The leader container completed or was advanced; proceed to validation.
    Advanced,
    /// The user aborted the dynamic invocation.
    Aborted,
    /// The user paused at the leader step — stop cleanly (no error) and leave
    /// the worktree in place so re-running resumes with a fresh leader.
    Paused,
    /// The user asked (via the Workflow Control Board) to restart the leader
    /// agent from scratch. The caller relaunches a fresh leader with the
    /// original prompt.
    Restart,
}

/// Outcome of the Workflow Control Board while it is driven from the dynamic
/// leader phase (there is no `WorkflowEngine` yet, so the leader loop maps the
/// returned [`NextAction`] onto these leader-scoped choices).
pub(crate) enum LeaderControlOutcome {
    /// Right arrow — kill the leader and start the generated workflow.
    StartWorkflow,
    /// Up arrow — restart the leader agent from scratch.
    Restart,
    /// Ctrl-C / `[a]` — abort the dynamic invocation with an error.
    Abort,
    /// `[p]` — kill the leader and stop cleanly; resumable by re-running.
    Pause,
    /// Esc, or any action that is not meaningful before a workflow exists —
    /// close the board and keep waiting on the leader.
    Dismiss,
}

impl ExecWorkflowCommand {
    /// Dynamic mode (WI-0092): a leader agent designs a `workflow.toml` for the
    /// requested work item, then awman validates and executes it. Performs all
    /// shared setup (worktree, context, work item) exactly once, then falls
    /// through to [`execute_prepared`] — it never re-enters
    /// `run_with_frontend`.
    pub(crate) async fn run_dynamic(
        self,
        mut frontend: Box<dyn ExecWorkflowCommandFrontend>,
    ) -> Result<ExecWorkflowOutcome, CommandError> {
        LaunchPolicy::for_session(&self.session).warn_legacy_config(frontend.as_mut());

        // ── Effective (implied) flags: --dynamic forces --yolo, --worktree,
        //    and context(workflow) (WI-0092 §4). Computed once so all
        //    downstream code sees the correct values.
        let mut effective_flags = self.flags.clone();
        apply_dynamic_implied_flags(&mut effective_flags);

        // ── Resolve the work item file + content (REQUIRED for dynamic). ────
        let wi_str = self
            .flags
            .work_item
            .as_deref()
            .expect("validated: --dynamic requires --work-item");
        let wi_number = parse_work_item_number(wi_str).ok_or_else(|| {
            CommandError::Other(format!(
                "could not parse a work item number from {wi_str:?}"
            ))
        })?;
        let base_session = self.session.clone();
        let git_root_for_scope = base_session.git_root().to_path_buf();
        let cwd = base_session.working_dir().to_path_buf();
        let wi_file = find_work_item_file(&base_session, &git_root_for_scope, wi_number)
            .ok_or_else(|| {
                CommandError::Other(format!(
                    "work item file for {wi_number:04} not found; dynamic mode cannot design a \
                 workflow without the work item content"
                ))
            })?;
        let wi_content = std::fs::read_to_string(&wi_file).map_err(|e| {
            CommandError::Other(format!(
                "failed to read work item file {}: {e}",
                wi_file.display()
            ))
        })?;
        let work_item_context = WorkItemContext {
            number: wi_number,
            content: wi_content,
        };

        // ── Worktree prepare BEFORE launching the leader (WI-0092 §5). ──────
        if base_session.session_type().is_remote() {
            return Err(CommandError::Other(
                "dynamic workflows are not supported for remote sessions".into(),
            ));
        }
        let git_root = self
            .engines
            .git_engine
            .resolve_root(&cwd)
            .map_err(CommandError::from)?;
        let lifecycle = WorktreeLifecycle::for_work_item(
            Arc::clone(&self.engines.git_engine),
            git_root,
            wi_number,
        )?;

        // ── Resumable previous run? (WI-0115 §2) ────────────────────────────
        //
        // A dynamic run stashes its generated workflow.toml beside the engine's
        // state file inside its own worktree. Both survive a Ctrl-C abort and
        // both die with the worktree, so an existing worktree is the one place
        // worth looking before paying for another leader-design pass.
        let resume_plan =
            match self.offer_dynamic_resume(&lifecycle, wi_number, frontend.as_mut())? {
                DynamicResumeOutcome::Resume(plan) => Some(plan),
                DynamicResumeOutcome::Fresh => None,
                DynamicResumeOutcome::Cancelled => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Info,
                        text: format!(
                            "exec workflow: cancelled; the previous dynamic run for work item \
                         {wi_number:04} is unchanged."
                        ),
                    });
                    return Ok(ExecWorkflowOutcome {
                        workflow: format!("dynamic-{wi_number:04}"),
                        exit_code: None,
                        worktree_used: false,
                    });
                }
            };

        let worktree_path = lifecycle
            .prepare_with_existing(
                &mut *frontend,
                // A confirmed resume is an answer to the existing-worktree
                // question; asking it again would be the same question twice.
                resume_plan.is_some().then_some(
                    crate::command::commands::worktree_lifecycle::ExistingWorktreeDecision::Resume,
                ),
            )
            .await?;
        let mount_path = worktree_path.clone();
        let worktree_git_mount = worktree_git_overlay(&mount_path)?;
        let state_git_root = self.workflow_state_root_for(&worktree_path);

        // Re-root a session at the worktree so the leader operates on the
        // isolated checkout.
        let leader_git_root = self
            .engines
            .git_engine
            .resolve_root(&worktree_path)
            .map_err(CommandError::from)?;
        let leader_session = Session::open_at_git_root(
            worktree_path.clone(),
            leader_git_root,
            crate::data::session::SessionOpenOptions::default(),
        )
        .map_err(|e| CommandError::Other(format!("opening worktree session: {e}")))?;

        let paths = crate::data::RepoDockerfilePaths::new(&git_root_for_scope);

        // ── Resume path: no leader, no context seeding, no image build for a
        //    leader that is never launched. Rewrite the saved state so the
        //    engine restarts at the chosen step and hand it straight to the
        //    shared execution tail (WI-0115 §2).
        if let Some(plan) = resume_plan {
            let DynamicResumePlan {
                workflow,
                workflow_path,
                mut state,
                dag,
                start_step,
                state_root,
            } = *plan;
            state.rewind_to(&dag, &start_step);
            let store =
                crate::data::workflow_state_store::WorkflowStateStore::at_git_root(state_root);
            store.save(&state).map_err(|e| {
                CommandError::Other(format!("rewriting the resumed workflow state: {e}"))
            })?;
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!(
                    "Resuming the previous dynamic workflow for work item {wi_number:04} \
                     from step '{start_step}'"
                ),
            });
            let workflow_name = crate::engine::workflow::workflow_name_for(&workflow);
            let outcome = self
                .execute_generated_workflow(DynamicExecution {
                    effective_flags,
                    workflow,
                    workflow_path,
                    work_item_context,
                    worktree_session: leader_session,
                    paths,
                    git_root_for_scope,
                    cwd,
                    base_session,
                    mount_path,
                    worktree_path,
                    lifecycle,
                    worktree_git_mount,
                    // The resume prompt already asked; execute_prepared must
                    // not ask the same question in different words.
                    skip_state_resume_prompt: true,
                    frontend,
                })
                .await?;
            // A resumed run that finishes clean is as done as a fresh one.
            if outcome.exit_code == Some(0) {
                clear_dynamic_resume_artifacts(&state_git_root, wi_number, &workflow_name);
            }
            return Ok(outcome);
        }

        // ── Resolve the leader agent + model (WI-0092 §7). Deliberately after
        //    the resume branch: a resumed run never launches a leader, and must
        //    not be blocked by leader config that has drifted since.
        let (leader_agent, leader_model) = resolve_leader_model(&self.flags, &self.session)?;

        // The work item path the leader sees is inside the mounted worktree.
        let wi_relative = wi_file
            .strip_prefix(&git_root_for_scope)
            .unwrap_or(&wi_file);
        let leader_work_item_path = std::path::Path::new("/workspace").join(wi_relative);

        // ── Resolve the context(workflow) overlay for the leader. ───────────
        let (leader_context_overlays, leader_system_prompt) =
            LaunchPolicy::for_session(&leader_session)
                .with_git(&self.engines.git_engine)
                .resolve_context_overlays(
                    &[crate::command::commands::ContextOverlaySpec {
                        scope: crate::data::config::overlays::ContextScope::Workflow,
                        permission: crate::data::config::overlays::OverlayPermission::ReadWrite,
                    }],
                    &leader_agent,
                    None,
                    None,
                    frontend.as_mut(),
                )?;
        let context_dir = leader_context_overlays
            .iter()
            .find(|o| matches!(o.scope, crate::engine::overlay::ContextScope::Workflow))
            .map(|o| o.host_path.clone())
            .ok_or_else(|| {
                CommandError::Other("failed to resolve workflow context directory".into())
            })?;

        frontend.report_workflow_context_path(&context_dir);

        // ── Seed the context dir: remove stale workflow.toml, write refs. ───
        let generated_path = context_dir.join("workflow.toml");
        let _ = std::fs::remove_file(&generated_path);
        std::fs::write(
            context_dir.join("example-workflow.toml"),
            crate::data::dynamic_workflow_assets::EXAMPLE_WORKFLOW_TOML,
        )
        .map_err(|e| CommandError::Other(format!("writing example-workflow.toml: {e}")))?;
        std::fs::write(
            context_dir.join("workflow-usage.md"),
            crate::data::dynamic_workflow_assets::WORKFLOW_USAGE_MD,
        )
        .map_err(|e| CommandError::Other(format!("writing workflow-usage.md: {e}")))?;

        // ── Discover available agents. ──────────────────────────────────────
        // (`paths` was resolved above, before the resume branch.)
        let available_agents = paths.discover_agent_dockerfiles();

        // ── Resolve the dynamicWorkflows config (WI-0095): the configured
        //    agent/model listing (validated against discovered Dockerfiles) and
        //    the concurrency advisory both feed the leader prompt. Validated
        //    before ensure_agent_image so a misconfigured agentsToModels fails
        //    before any image build or container work.
        let dynamic_cfg = base_session.repo_config().dynamic_workflows.clone();
        let max_concurrent_steps = dynamic_cfg.as_ref().and_then(|d| d.max_concurrent_steps);
        let configured_agents = dynamic_cfg
            .as_ref()
            .and_then(|d| d.agents_to_models.as_ref())
            .filter(|m| !m.is_empty());
        let agents_section = if let Some(map) = configured_agents {
            let mut warnings: Vec<String> = Vec::new();
            let effective =
                build_effective_agents_to_models(map, &available_agents, &mut warnings)?;
            for w in warnings {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: w,
                });
            }
            format_agents_with_models(&effective)
        } else {
            if dynamic_cfg
                .as_ref()
                .and_then(|d| d.agents_to_models.as_ref())
                .is_some_and(|m| m.is_empty())
            {
                tracing::debug!(
                    "dynamicWorkflows.agentsToModels is an empty map; falling back to \
                     Dockerfile discovery"
                );
            }
            format_available_agents(&available_agents)
        };

        // ── Ensure the leader image is built. ───────────────────────────────
        ensure_agent_image(
            &self.engines,
            &git_root_for_scope,
            &paths,
            leader_agent.as_str(),
            frontend.as_mut(),
        )?;

        let leader_prompt = crate::data::dynamic_workflow_assets::build_leader_prompt(
            &format!("{wi_number:04}"),
            &leader_work_item_path.display().to_string(),
            &agents_section,
            max_concurrent_steps,
            dynamic_cfg.as_ref().and_then(|d| d.guidance.as_deref()),
        );

        // Record the worktree's clean baseline so we can detect a leader that
        // illicitly modifies source files (WI-0092 §7 mutation guard).
        // `uncommitted_files` is `git status --porcelain` through the engine
        // (F-39); comparing the `Vec<String>` is the same comparison the raw
        // string was, without a second shell-out of our own.
        let worktree_baseline = self
            .engines
            .git_engine
            .uncommitted_files(&worktree_path)
            .unwrap_or_default();

        // ── Wrap the frontend so the agent run + yolo ticks can share it. ───
        let shared: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>> =
            Arc::new(Mutex::new(frontend));

        // ── Wire an engine request channel so Ctrl-W opens the Workflow
        //    Control Board during the leader phase (the leader runs without a
        //    `WorkflowEngine`, so nothing else installs a sender). Registering
        //    it here makes the shared `engine_tx_shared` slot the TUI reads
        //    non-empty; the leader select loops below drain the receiver. The
        //    real engine overwrites this sender once the generated workflow
        //    starts.
        let (engine_tx, mut engine_rx) = tokio::sync::mpsc::unbounded_channel::<EngineRequest>();
        shared.lock().unwrap().attach_engine(
            crate::engine::workflow::frontend::EngineHandles::requests(engine_tx),
        );

        // ── Leader + repair loop (WI-0092 §9). ──────────────────────────────
        //
        // The attempt budget, the repair-prompt substitution, and the
        // exhaustion message live in the one shared `WorkflowRepairLoop`; squad's
        // unattended evaluator drives the same object. Only the *driving* of the
        // leader container (stuck → yolo countdown → control board) is specific
        // to this interactive caller.
        let mut repair = WorkflowRepairLoop::new(generated_path.clone(), leader_prompt.clone());
        let validated_workflow = loop {
            let label = repair.label();
            let current_prompt = repair.prompt().to_string();
            let drive = self
                .drive_leader_agent(
                    Arc::clone(&shared),
                    &leader_session,
                    &leader_agent,
                    leader_model.as_deref(),
                    &current_prompt,
                    &git_root_for_scope,
                    leader_context_overlays.clone(),
                    leader_system_prompt.clone(),
                    &label,
                    &mut engine_rx,
                )
                .await?;
            match drive {
                LeaderDriveOutcome::Advanced => {}
                LeaderDriveOutcome::Aborted => {
                    return Err(CommandError::Other(
                        "dynamic workflow aborted during the leader step".into(),
                    ));
                }
                LeaderDriveOutcome::Paused => {
                    // Clean stop at the leader step — no error, worktree left in
                    // place. Re-running the command starts a fresh leader.
                    shared.lock().unwrap().write_message(UserMessage {
                        level: MessageLevel::Info,
                        text: format!(
                            "Dynamic workflow paused at the leader step. Re-run \
                             `awman exec workflow --dynamic --work-item {wi_number}` to resume \
                             with a fresh leader."
                        ),
                    });
                    return Ok(ExecWorkflowOutcome {
                        workflow: format!("dynamic-{wi_number:04}"),
                        exit_code: None,
                        worktree_used: true,
                    });
                }
                LeaderDriveOutcome::Restart => {
                    // Discard any partial workflow.toml and relaunch a fresh
                    // leader with the original prompt (the repair budget resets
                    // — a user restart is not a validation failure).
                    repair.restart();
                    shared.lock().unwrap().write_message(UserMessage {
                        level: MessageLevel::Info,
                        text: "Restarting the dynamic workflow leader agent…".into(),
                    });
                    continue;
                }
            }

            // Mutation guard: the leader may only write under the context dir.
            let after = self
                .engines
                .git_engine
                .uncommitted_files(&worktree_path)
                .unwrap_or_default();
            if after != worktree_baseline {
                return Err(CommandError::Other(format!(
                    "leader agent modified files in the worktree; dynamic pre-flight may only \
                     write under the workflow context directory. Changed worktree status:\n{}",
                    after.join("\n")
                )));
            }

            // Validate: file present → parse → agent validation.
            let result = validate_generated_workflow(&generated_path, &leader_session, &paths);

            match repair.record(result) {
                RepairDecision::Accepted(wf) => break *wf,
                RepairDecision::Exhausted(message) => {
                    return Err(CommandError::Other(message));
                }
                RepairDecision::Retry { attempt, error } => {
                    shared.lock().unwrap().write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: format!(
                            "workflow.toml validation failed (attempt {attempt}/{}): {error}",
                            WorkflowRepairLoop::MAX_REPAIR_ATTEMPTS
                        ),
                    });
                }
            }
        };

        // ── Reclaim the frontend and execute the generated workflow. ────────
        let frontend = Arc::try_unwrap(shared)
            .unwrap_or_else(|_| panic!("no other Arc references remain after leader phase"))
            .into_inner()
            .unwrap();

        // Stash the generated workflow inside the worktree so a failed run can
        // be resumed without a second leader-design pass (WI-0115 §2).
        save_dynamic_workflow_copy(&state_git_root, wi_number, &generated_path);

        // Captured before the workflow moves into the execution tail: the state
        // file is keyed by workflow name, and the cleanup below needs it.
        let dynamic_workflow_name = crate::engine::workflow::workflow_name_for(&validated_workflow);

        let outcome = self
            .execute_generated_workflow(DynamicExecution {
                effective_flags: effective_flags.clone(),
                workflow: validated_workflow,
                workflow_path: generated_path,
                work_item_context,
                worktree_session: leader_session,
                paths,
                git_root_for_scope,
                cwd,
                base_session,
                mount_path,
                worktree_path,
                lifecycle,
                worktree_git_mount,
                skip_state_resume_prompt: false,
                frontend,
            })
            .await?;

        // A clean finish means there is nothing left to resume; drop *both*
        // halves so the next run on this work item starts from a fresh design.
        if outcome.exit_code == Some(0) {
            clear_dynamic_resume_artifacts(&state_git_root, wi_number, &dynamic_workflow_name);
        }
        Ok(outcome)
    }

    /// Look for — and offer to resume — a previous `--dynamic` run on this work
    /// item (WI-0115 §2).
    ///
    /// Called before the worktree is prepared and before the leader launches,
    /// so every answer is still free: `Resume` skips the leader entirely,
    /// `Fresh` clears the previous run and designs a new workflow, and `Cancel`
    /// backs out of the command with the previous run untouched.
    ///
    /// `Fresh` is also the answer when there is no worktree to look inside;
    /// when the worktree is there but the run cannot be reconstructed the user
    /// is told why before the fresh design starts.
    pub(crate) fn offer_dynamic_resume(
        &self,
        lifecycle: &WorktreeLifecycle,
        work_item: u32,
        frontend: &mut dyn ExecWorkflowCommandFrontend,
    ) -> Result<DynamicResumeOutcome, CommandError> {
        let worktree_path = lifecycle.worktree_path();
        if !worktree_path.exists() {
            return Ok(DynamicResumeOutcome::Fresh);
        }
        let state_root = self.workflow_state_root_for(worktree_path);

        let previous = match discover_previous_dynamic_run(&state_root, work_item) {
            Ok(p) => p,
            Err(DynamicDiscoveryMiss::NothingToResume) => return Ok(DynamicResumeOutcome::Fresh),
            Err(DynamicDiscoveryMiss::Unusable(reason)) => {
                frontend.notify_dynamic_workflow_resume_unavailable(work_item, &reason)?;
                return Ok(DynamicResumeOutcome::Fresh);
            }
        };

        let dag = match crate::data::workflow_dag::WorkflowDag::build(&previous.workflow.steps) {
            Ok(dag) => dag,
            Err(e) => {
                frontend.notify_dynamic_workflow_resume_unavailable(
                    work_item,
                    &format!("the saved workflow's step graph is no longer valid: {e}"),
                )?;
                return Ok(DynamicResumeOutcome::Fresh);
            }
        };

        let start_points = workflow_resume_start_points(&dag, &previous.state);
        if start_points.is_empty() {
            frontend.notify_dynamic_workflow_resume_unavailable(
                work_item,
                "the previous dynamic workflow ran every step to completion; there is nothing \
                 to resume",
            )?;
            return Ok(DynamicResumeOutcome::Fresh);
        }

        let prompt = WorkflowResumePrompt::new(
            crate::engine::workflow::workflow_name_for(&previous.workflow),
            Some(work_item),
            Some(worktree_path.to_path_buf()),
            true,
            completed_step_count(&previous.state),
            previous.state.step_states.len(),
            start_points,
        );

        match frontend.ask_workflow_resume(&prompt)? {
            WorkflowResumeDecision::ResumeFrom(start_step) => {
                Ok(DynamicResumeOutcome::Resume(Box::new(DynamicResumePlan {
                    workflow: previous.workflow,
                    workflow_path: previous.workflow_path,
                    state: previous.state,
                    dag,
                    start_step,
                    state_root,
                })))
            }
            WorkflowResumeDecision::Fresh => {
                // Clear both halves so neither this run's state check nor the
                // next invocation trips over the abandoned run.
                let store = crate::data::workflow_state_store::WorkflowStateStore::at_git_root(
                    state_root.clone(),
                );
                let name = crate::engine::workflow::workflow_name_for(&previous.workflow);
                if let Err(e) = store.delete(Some(work_item), &name) {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: format!("exec workflow: failed to delete stale workflow state: {e}"),
                    });
                }
                let _ = std::fs::remove_file(&previous.workflow_path);
                Ok(DynamicResumeOutcome::Fresh)
            }
            // Nothing has been created and nothing is deleted: the same
            // resume is on offer next time the command runs.
            WorkflowResumeDecision::Cancel => Ok(DynamicResumeOutcome::Cancelled),
        }
    }

    /// Where this run's `WorkflowState` (and, in dynamic mode, the saved
    /// `workflow.toml`) live — the same root [`execute_prepared`] hands the
    /// engine, so a resume reads exactly the file the previous run wrote.
    ///
    /// Safe to call before a worktree has been prepared: an unborn worktree
    /// cannot be resolved, and falling back to the path itself is the answer
    /// `resolve_root` gives once it exists — a git worktree checkout is its own
    /// root. Either way there is no state file there yet, so an early caller
    /// correctly finds nothing to resume.
    pub(crate) fn workflow_state_root_for(&self, session_root: &Path) -> PathBuf {
        if let Some(root) = &self.workflow_state_root {
            return root.clone();
        }
        Arc::clone(&self.engines.git_engine)
            .resolve_root(session_root)
            .unwrap_or_else(|_| session_root.to_path_buf())
    }

    /// Shared tail of both dynamic paths: build any missing agent images, run
    /// the whole-workflow ACP pre-flight, then hand the run to
    /// [`execute_prepared`]. The leader path reaches it with a freshly designed
    /// workflow; the resume path with the one recovered from disk.
    pub(crate) async fn execute_generated_workflow(
        &self,
        exec: DynamicExecution,
    ) -> Result<ExecWorkflowOutcome, CommandError> {
        let DynamicExecution {
            effective_flags,
            workflow,
            workflow_path,
            work_item_context,
            worktree_session,
            paths,
            git_root_for_scope,
            cwd,
            base_session,
            mount_path,
            worktree_path,
            lifecycle,
            worktree_git_mount,
            skip_state_resume_prompt,
            mut frontend,
        } = exec;

        // ── Build any missing agent images before execution (WI-0092 §9b). ──
        let resolved_agents =
            resolve_and_validate_workflow_agents(&workflow, &worktree_session, &paths)
                .map_err(CommandError::Other)?;
        for agent in &resolved_agents {
            ensure_agent_image(
                &self.engines,
                &git_root_for_scope,
                &paths,
                agent,
                frontend.as_mut(),
            )?;
        }

        // The dynamically generated workflow is not known until after the
        // leader phase, so this is its first possible whole-workflow ACP
        // pre-flight. It still runs before any generated workflow step.
        let launch_modes = validate_workflow_acp_preflight(
            &workflow,
            &base_session,
            &effective_flags,
            frontend.as_mut(),
        )
        .map_err(CommandError::from)?;

        // Build the CLI overlay list (includes the implied context(workflow)).
        let mut cli_typed = Vec::new();
        for s in &effective_flags.overlay {
            match parse_overlay_list(s) {
                Ok(parsed) => cli_typed.extend(parsed),
                Err(reason) => {
                    return Err(CommandError::InvalidOverlaySpec {
                        spec: s.clone(),
                        reason,
                    });
                }
            }
        }

        let prepared = PreparedRun {
            workflow,
            workflow_path,
            work_item_context: Some(work_item_context),
            cli_typed,
            mount_path,
            worktree_path: Some(worktree_path),
            worktree_lifecycle: Some(lifecycle),
            worktree_git_mount,
            git_root_for_scope,
            cwd,
            original_session: base_session,
            issue_temp_file: None,
            launch_modes,
            skip_state_resume_prompt,
        };
        execute_prepared(
            &effective_flags,
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

    /// Launch a single leader/repair agent container and drive it through the
    /// same stuck → yolo countdown → auto-advance pipeline a workflow step
    /// uses. The container is killed when the countdown advances (WI-0092 §8).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn drive_leader_agent(
        &self,
        shared: Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>,
        session: &Session,
        agent: &crate::data::session::AgentName,
        model: Option<&str>,
        prompt: &str,
        image_git_root: &std::path::Path,
        context_overlays: Vec<crate::engine::overlay::ContextOverlay>,
        system_prompt: Option<String>,
        label: &str,
        engine_rx: &mut tokio::sync::mpsc::UnboundedReceiver<EngineRequest>,
    ) -> Result<LeaderDriveOutcome, CommandError> {
        use crate::engine::agent_runtime::execution::{StuckEvent, KILLED_EXIT_CODE};

        let run_opts = AgentRunOptions {
            yolo: Some(YoloMode::Enabled),
            initial_prompt: Some(prompt.to_string()),
            model: model.map(|m| m.to_string()),
            allow_docker: self.flags.allow_docker,
            non_interactive: self.flags.non_interactive,
            image_tag_override: Some(crate::data::image_tags::agent_image_tag(
                image_git_root,
                agent.as_str(),
            )),
            system_prompt,
            context_overlays,
            ..Default::default()
        };
        let resolved_credentials = self
            .engines
            .auth_engine
            .resolve_agent_auth(session, agent)
            .unwrap_or_default();
        let credentials = if self.engines.runtime.capabilities().kit_declarative
            && matches!(
                resolved_credentials.delivery,
                crate::engine::auth::CredentialDelivery::File(_)
            ) {
            self.engines
                .auth_engine
                .agent_env_credentials(agent)
                .unwrap_or_default()
        } else {
            resolved_credentials
        };
        let resolved = self.engines.agent_engine.resolve_agent_options(
            session,
            agent,
            &run_opts,
            &credentials,
        )?;
        // Stamp the squad identity on the dynamic leader too, when this is an
        // squad-generated workflow, so its container carries the task's
        // discoverable name and labels.
        let resolved = match &self.squad_identity {
            Some(identity) => identity.stamp(resolved)?,
            None => resolved,
        };
        let instance = self.engines.runtime.build(resolved)?;

        shared.lock().unwrap().write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!(
                "Launching dynamic workflow {label} agent ({})…",
                agent.as_str()
            ),
        });
        shared.lock().unwrap().set_pty_active(true);

        let proxy = AgentFrontendProxy {
            frontend: Arc::clone(&shared),
            acp: false,
            io: None,
        };
        let mut execution = match instance.run_with_frontend(Box::new(proxy)) {
            Ok(e) => e,
            Err(e) => {
                let mut g = shared.lock().unwrap();
                g.set_pty_active(false);
                g.replay_queued();
                return Err(CommandError::from(e));
            }
        };

        let cancel = execution.cancel_handle();
        let mut stuck_rx = execution.subscribe_stuck();
        let (wait_tx, mut wait_rx) = tokio::sync::oneshot::channel::<i32>();
        tokio::spawn(async move {
            let code = execution
                .wait()
                .await
                .map(|info| info.exit_code)
                .unwrap_or(-1);
            let _ = wait_tx.send(code);
        });

        // Every `break` below corresponds to the leader container actually
        // being dead (self-exit, engine kill, or grace-expiry kill), so each
        // reports the exit to the frontend before leaving the loop. The
        // container window must NOT close on mere stuck states or while the
        // yolo countdown is still running — those paths `continue` instead.
        let outcome = loop {
            tokio::select! {
                biased;
                code = &mut wait_rx => {
                    shared
                        .lock()
                        .unwrap()
                        .report_container_exited(code.unwrap_or(-1));
                    break LeaderDriveOutcome::Advanced;
                }
                ev = stuck_rx.recv() => {
                    match ev {
                        Ok(StuckEvent::Stuck) => {
                            match run_leader_yolo_countdown(
                                &shared,
                                &mut wait_rx,
                                &mut stuck_rx,
                                engine_rx,
                                label,
                            )
                            .await
                            {
                                LeaderCountdownOutcome::Advance => {
                                    if let Some(c) = &cancel {
                                        let _ = c.cancel();
                                    }
                                    shared
                                        .lock()
                                        .unwrap()
                                        .report_container_exited(KILLED_EXIT_CODE);
                                    break LeaderDriveOutcome::Advanced;
                                }
                                LeaderCountdownOutcome::Completed(code) => {
                                    shared.lock().unwrap().report_container_exited(code);
                                    break LeaderDriveOutcome::Advanced;
                                }
                                LeaderCountdownOutcome::Recovered => continue,
                                LeaderCountdownOutcome::Abort => {
                                    if let Some(c) = &cancel {
                                        let _ = c.cancel();
                                    }
                                    shared
                                        .lock()
                                        .unwrap()
                                        .report_container_exited(KILLED_EXIT_CODE);
                                    break LeaderDriveOutcome::Aborted;
                                }
                                // Ctrl-W during the countdown: cancel the
                                // countdown and open the WCB in its place.
                                LeaderCountdownOutcome::ShowControlBoard => {
                                    let choice = show_leader_control_board(&shared, label);
                                    match apply_leader_control_outcome(choice, &cancel, &shared) {
                                        Some(o) => break o,
                                        None => continue,
                                    }
                                }
                            }
                        }
                        Ok(StuckEvent::Unstuck) => continue,
                        Ok(StuckEvent::StartupGraceExpired) => {
                            // The io bridge already killed the container.
                            shared
                                .lock()
                                .unwrap()
                                .report_container_exited(KILLED_EXIT_CODE);
                            break LeaderDriveOutcome::Advanced;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            let code = (&mut wait_rx).await.unwrap_or(-1);
                            shared.lock().unwrap().report_container_exited(code);
                            break LeaderDriveOutcome::Advanced;
                        }
                    }
                }
                // Ctrl-W while the leader is actively running (not stuck): open
                // the Workflow Control Board on request.
                Some(req) = engine_rx.recv() => {
                    if let EngineRequest::OpenControlBoard { .. } = req {
                        let choice = show_leader_control_board(&shared, label);
                        match apply_leader_control_outcome(choice, &cancel, &shared) {
                            Some(o) => break o,
                            None => continue,
                        }
                    }
                }
            }
        };

        let mut g = shared.lock().unwrap();
        g.set_pty_active(false);
        g.replay_queued();
        Ok(outcome)
    }
}

/// Build a leader-scoped Workflow Control Board, present it, and map the user's
/// choice onto a [`LeaderControlOutcome`]. Because the leader phase has no
/// `WorkflowEngine`/`WorkflowState`, a synthetic single-step state is
/// constructed so the shared `show_workflow_control_board` renderer has a
/// running step to name.
pub(crate) fn show_leader_control_board(
    shared: &Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>,
    label: &str,
) -> LeaderControlOutcome {
    use crate::data::workflow_state::{StepState, WorkflowState};

    let mut state = WorkflowState::new(label.to_string(), &[], String::new(), None);
    state.set_status(label, StepState::Running { container_id: None });

    // `can_dismiss` keeps the full diamond board (not the lightweight step
    // confirm) and enables the Esc/Dismiss + Pause footer. Only the actions
    // meaningful before a workflow exists are offered.
    let available = AvailableActions {
        can_launch_next: true,
        launch_next_label: Some("Start dynamic workflow".to_string()),
        can_restart_current_step: true,
        can_abort: true,
        can_dismiss: true,
        ..Default::default()
    };

    let action = shared
        .lock()
        .unwrap()
        .show_workflow_control_board(&state, &available);

    match action {
        Ok(NextAction::LaunchNext) => LeaderControlOutcome::StartWorkflow,
        Ok(NextAction::RestartCurrentStep) => LeaderControlOutcome::Restart,
        Ok(NextAction::Abort) => LeaderControlOutcome::Abort,
        Ok(NextAction::Pause) => LeaderControlOutcome::Pause,
        // Dismiss, or any action not valid before a workflow exists
        // (Continue/CancelToPrevious/Finish), just closes the board.
        Ok(_) | Err(_) => LeaderControlOutcome::Dismiss,
    }
}

/// Apply a leader-phase WCB choice: for every terminal choice, kill the leader
/// container (reporting the exit) and return the drive outcome to break the
/// leader loop with; `None` means Dismiss — keep the leader running.
pub(crate) fn apply_leader_control_outcome(
    outcome: LeaderControlOutcome,
    cancel: &Option<crate::engine::agent_runtime::execution::CancelHandle>,
    shared: &Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>,
) -> Option<LeaderDriveOutcome> {
    use crate::engine::agent_runtime::execution::KILLED_EXIT_CODE;

    let drive = match outcome {
        LeaderControlOutcome::StartWorkflow => LeaderDriveOutcome::Advanced,
        LeaderControlOutcome::Restart => LeaderDriveOutcome::Restart,
        LeaderControlOutcome::Abort => LeaderDriveOutcome::Aborted,
        LeaderControlOutcome::Pause => LeaderDriveOutcome::Paused,
        LeaderControlOutcome::Dismiss => return None,
    };
    if let Some(c) = cancel {
        let _ = c.cancel();
    }
    shared
        .lock()
        .unwrap()
        .report_container_exited(KILLED_EXIT_CODE);
    Some(drive)
}

/// Result of the leader yolo countdown.
pub(crate) enum LeaderCountdownOutcome {
    /// Countdown expired or user advanced — kill the container and proceed.
    Advance,
    /// The leader container exited on its own (with this exit code) during
    /// the countdown.
    Completed(i32),
    /// The leader resumed output (`Unstuck`) — cancel the countdown.
    Recovered,
    /// The user aborted.
    Abort,
    /// The user pressed Ctrl-W — cancel the countdown and open the WCB.
    ShowControlBoard,
}

/// Drive the 60-second yolo countdown for the leader step, reusing the same
/// `WorkflowFrontend::yolo_countdown_tick` pipeline as a workflow step. The
/// right-arrow / advance action carries the "Start dynamic workflow" label via
/// [`AvailableActions::launch_next_label`].
pub(crate) async fn run_leader_yolo_countdown(
    shared: &Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>,
    wait_rx: &mut tokio::sync::oneshot::Receiver<i32>,
    stuck_rx: &mut tokio::sync::broadcast::Receiver<
        crate::engine::agent_runtime::execution::StuckEvent,
    >,
    engine_rx: &mut tokio::sync::mpsc::UnboundedReceiver<EngineRequest>,
    step_name: &str,
) -> LeaderCountdownOutcome {
    use crate::engine::agent_runtime::execution::StuckEvent;
    use crate::engine::workflow::actions::YoloTickOutcome;
    use crate::engine::workflow::timing::YOLO_COUNTDOWN_DURATION;

    let total = YOLO_COUNTDOWN_DURATION;
    let tick = Duration::from_millis(200);
    let mut remaining = total;
    shared
        .lock()
        .unwrap()
        .yolo_countdown_started(step_name, CountdownKind::StuckStep);

    let outcome = loop {
        let tick_result = shared
            .lock()
            .unwrap()
            .yolo_countdown_tick(step_name, remaining, total);
        match tick_result {
            Ok(YoloTickOutcome::Continue) => {}
            Ok(YoloTickOutcome::AdvanceNow) => break LeaderCountdownOutcome::Advance,
            Ok(YoloTickOutcome::Cancel) => break LeaderCountdownOutcome::Abort,
            Err(_) => break LeaderCountdownOutcome::Advance,
        }
        if remaining.is_zero() {
            break LeaderCountdownOutcome::Advance;
        }

        tokio::select! {
            biased;
            code = &mut *wait_rx => {
                break LeaderCountdownOutcome::Completed(code.unwrap_or(-1));
            }
            ev = stuck_rx.recv() => {
                match ev {
                    Ok(StuckEvent::Unstuck) => break LeaderCountdownOutcome::Recovered,
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                }
            }
            Some(req) = engine_rx.recv() => {
                if let EngineRequest::OpenControlBoard { .. } = req {
                    break LeaderCountdownOutcome::ShowControlBoard;
                }
            }
            _ = tokio::time::sleep(tick) => {
                remaining = remaining.saturating_sub(tick);
            }
        }
    };

    shared.lock().unwrap().yolo_countdown_finished(step_name);
    outcome
}
