//! Setup and teardown phases, their remediation (`on_failure`) path, and the
//! failure artefacts a remediation agent is handed.
//!
//! Split out of `workflow/mod.rs` by WI 0114 F-51. A child module of
//! `workflow`, so it reaches `WorkflowEngine`'s private fields exactly as the
//! code did before the move; methods it defines are `pub(super)` so the
//! other halves of the engine can still call them.

use super::*;

impl WorkflowEngine {
    /// Declare setup and teardown before either runs, with substituted
    /// descriptions. A resumed phase with the same number of steps keeps its
    /// recorded statuses, including a setup phase that will be skipped.
    /// Persists once so clients immediately see the whole workflow.
    pub fn declare_phase_steps(
        &mut self,
        setup: &[crate::data::workflow_definition::SetupStep],
        teardown: &[crate::data::workflow_definition::TeardownStep],
    ) -> Result<(), EngineError> {
        {
            let steps: Vec<_> = setup
                .iter()
                .map(|s| s.substitute(self.work_item_context.as_ref()))
                .collect();
            self.refresh_phase_states(PhaseKind::Setup, &steps);
        }
        {
            let steps: Vec<_> = teardown
                .iter()
                .map(|s| s.substitute(self.work_item_context.as_ref()))
                .collect();
            self.refresh_phase_states(PhaseKind::Teardown, &steps);
        }
        if !self.state.setup_completed
            && !setup.is_empty()
            && self.state.current_phase == crate::data::workflow_state::WorkflowPhase::Main
            && self
                .state
                .step_states
                .values()
                .all(|s| matches!(s, StepState::Pending))
        {
            self.state.current_phase = crate::data::workflow_state::WorkflowPhase::Setup;
        }
        self.persist()
    }

    /// Refresh descriptions without losing completed phase statuses on resume.
    fn refresh_phase_states<S: PhaseStepSpec>(&mut self, kind: PhaseKind, steps: &[S]) {
        use crate::data::workflow_state::{PhaseStepState, PhaseStepStatus};
        let states = self.state.phase_step_states_mut(kind);
        if states.len() == steps.len() {
            for (state, step) in states.iter_mut().zip(steps) {
                state.description = step.description();
            }
        } else {
            *states = steps
                .iter()
                .map(|s| PhaseStepState {
                    description: s.description(),
                    status: PhaseStepStatus::Pending,
                })
                .collect();
        }
    }

    /// Refresh declared descriptions and reset statuses on phase entry, since
    /// resume re-runs every step. Rebuild only when the step count changed.
    /// `steps` already have work-item substitution applied.
    fn reset_or_rebuild_phase_states<S: PhaseStepSpec>(&mut self, kind: PhaseKind, steps: &[S]) {
        self.refresh_phase_states(kind, steps);
        for state in self.state.phase_step_states_mut(kind) {
            state.status = crate::data::workflow_state::PhaseStepStatus::Pending;
        }
    }

    /// Run one shell phase, asking the caller for a fresh container per step.
    ///
    /// `container_for_step(step)` is invoked once per step (and again for
    /// each `on_failure` retry) and must return a
    /// container with that step's overlays/env applied — and only that step's.
    /// The returned container is dropped when the step finishes, which kills
    /// the container via `BackgroundContainer::drop`. This is what gives each
    /// step its own isolated resource set (WI-0082): two teardown entries
    /// declaring `overlays = ["ssh()"]` and `overlays = ["env(GITHUB_TOKEN)"]`
    /// must NOT each see both.
    ///
    /// A failing step does not stop the phase unless its `abort_on_failure`
    /// flag is set; failures are recorded and the phase continues
    /// (best-effort). When the per-step container factory itself fails (an
    /// overlay won't resolve, the runtime can't start the container) the step
    /// is recorded as `Failed` the same way a non-zero exit code is.
    ///
    /// Callers decide *whether* teardown runs at all — see
    /// [`WorkflowEngine::teardown_applies`].
    ///
    /// Before F-34 this was two functions, `run_setup` and `run_teardown`,
    /// with a further four twinned helpers beneath them. Teardown's richer
    /// behaviour is now the common behaviour, so a failing **setup** step with
    /// an `on_failure` agent now gets the same captured-output failure file
    /// teardown steps have always had.
    pub fn run_phase<S, F>(
        &mut self,
        kind: PhaseKind,
        steps: &[S],
        abort_flags: &[bool],
        on_failure_configs: &[Option<crate::data::workflow_definition::RemediationConfig>],
        container_for_step: F,
    ) -> Result<PhaseOutcome, EngineError>
    where
        S: PhaseStepSpec,
        F: FnMut(&PhaseStepRef) -> Result<Box<dyn AgentExec>, EngineError>,
    {
        // Setup stops between steps on a cancel so the main phase never
        // starts. A teardown entered normally (the main phase already
        // finished) switches to cancelled cleanup on a cancel: its remaining
        // publishing steps are skipped and no remediation is launched.
        let options = PhaseRunOptions {
            resume: false,
            skip: Vec::new(),
            allow_remediation: true,
            observe_cancel: kind == PhaseKind::Setup,
        };
        self.run_phase_with(
            kind,
            steps,
            abort_flags,
            on_failure_configs,
            options,
            container_for_step,
        )
    }

    /// Teardown after a cancel: `PushBranch`, `CreatePullRequest` and `PollCi`
    /// entries are recorded `PhaseStepStatus::Skipped` and reported through
    /// `WorkflowFrontend::on_phase_step_skipped`; the remaining entries
    /// (`RunShell`, `RunScript`, `CommitChanges`) run in order with no
    /// `on_failure` remediation and without observing the cancel token (the run
    /// is already cancelled; cleanup must finish). `abort_flags` keep their
    /// meaning. Returns the phase outcome with `cancelled = true`.
    pub fn run_teardown_cancelled<F>(
        &mut self,
        steps: &[crate::data::workflow_definition::TeardownStep],
        abort_flags: &[bool],
        container_for_step: F,
    ) -> Result<PhaseOutcome, EngineError>
    where
        F: FnMut(&PhaseStepRef) -> Result<Box<dyn AgentExec>, EngineError>,
    {
        let options = PhaseRunOptions {
            resume: false,
            skip: steps
                .iter()
                .map(|s| !Self::teardown_step_runs_when_cancelled(s))
                .collect(),
            allow_remediation: false,
            observe_cancel: false,
        };
        let mut outcome = self.run_phase_with(
            PhaseKind::Teardown,
            steps,
            abort_flags,
            &[],
            options,
            container_for_step,
        )?;
        outcome.cancelled = true;
        Ok(outcome)
    }

    /// Finish interrupted cancelled teardown without repeating terminal steps.
    pub fn resume_teardown_cancelled<F>(
        &mut self,
        steps: &[crate::data::workflow_definition::TeardownStep],
        abort_flags: &[bool],
        container_for_step: F,
    ) -> Result<PhaseOutcome, EngineError>
    where
        F: FnMut(&PhaseStepRef) -> Result<Box<dyn AgentExec>, EngineError>,
    {
        self.run_phase_with(
            PhaseKind::Teardown,
            steps,
            abort_flags,
            &[],
            PhaseRunOptions {
                resume: true,
                skip: steps
                    .iter()
                    .map(|s| !Self::teardown_step_runs_when_cancelled(s))
                    .collect(),
                allow_remediation: false,
                observe_cancel: false,
            },
            container_for_step,
        )
    }

    /// Whether a teardown step still runs after a cancel. Nothing that
    /// publishes the cancelled work (a push, a pull request, waiting on CI
    /// for it) does; local cleanup does.
    pub fn teardown_step_runs_when_cancelled(
        step: &crate::data::workflow_definition::TeardownStep,
    ) -> bool {
        use crate::data::workflow_definition::TeardownStep;
        !matches!(
            step,
            TeardownStep::PushBranch { .. }
                | TeardownStep::CreatePullRequest { .. }
                | TeardownStep::PollCi { .. }
        )
    }

    /// The one phase runner behind [`Self::run_phase`] and
    /// [`Self::run_teardown_cancelled`].
    fn run_phase_with<S, F>(
        &mut self,
        kind: PhaseKind,
        steps: &[S],
        abort_flags: &[bool],
        on_failure_configs: &[Option<crate::data::workflow_definition::RemediationConfig>],
        options: PhaseRunOptions,
        mut container_for_step: F,
    ) -> Result<PhaseOutcome, EngineError>
    where
        S: PhaseStepSpec,
        F: FnMut(&PhaseStepRef) -> Result<Box<dyn AgentExec>, EngineError>,
    {
        use crate::data::workflow_state::PhaseStepStatus;

        debug_assert_eq!(
            kind,
            S::kind(),
            "run_phase called with a step type that belongs to the other phase"
        );

        let wi_ctx = self.work_item_context.as_ref();
        let steps: Vec<S> = steps.iter().map(|s| s.substitute(wi_ctx)).collect();

        self.state.current_phase = kind.workflow_phase();
        if options.resume {
            self.refresh_phase_states(kind, &steps);
        } else {
            self.reset_or_rebuild_phase_states(kind, &steps);
        }
        self.persist()?;

        let mut outcome = PhaseOutcome::default();
        for (idx, step) in steps.iter().enumerate() {
            if options.resume
                && matches!(
                    self.state.phase_step_states(kind)[idx].status,
                    PhaseStepStatus::Succeeded
                        | PhaseStepStatus::Skipped
                        | PhaseStepStatus::Failed { .. }
                )
            {
                continue;
            }
            // A shell step already executing finishes (its exec is a blocking
            // call); the cancel is honoured before the next one starts.
            if options.observe_cancel && self.cancel_requested() {
                outcome.cancelled = true;
                break;
            }

            let desc = self.state.phase_step_states(kind)[idx].description.clone();
            let abort = abort_flags.get(idx).copied().unwrap_or(false);

            // A cancel during teardown switches the rest of it to cancelled
            // cleanup: nothing that publishes the cancelled work starts, local
            // cleanup still runs (and launches no remediation, below).
            let cancelled_teardown = kind == PhaseKind::Teardown && self.cancel_requested();
            if cancelled_teardown {
                outcome.cancelled = true;
            }

            if options.skip.get(idx).copied().unwrap_or(false)
                || (cancelled_teardown && step.publishes())
            {
                self.state.phase_step_states_mut(kind)[idx].status = PhaseStepStatus::Skipped;
                self.persist()?;
                self.frontend.on_phase_step_skipped(kind, &desc);
                continue;
            }

            self.state.phase_step_states_mut(kind)[idx].status = PhaseStepStatus::Running;
            self.persist()?;

            self.frontend.on_phase_step_started(kind, &desc);

            let mut step_outcome =
                self.run_single_phase_step(kind, step, idx, &mut container_for_step);

            // No `on_failure` agent is ever launched once a cancel arrived.
            if step_outcome.failed && options.allow_remediation && !self.cancel_requested() {
                let rem = on_failure_configs
                    .get(idx)
                    .and_then(|c| c.as_ref())
                    .cloned();
                if let Some(rem_config) = rem {
                    step_outcome = self.run_phase_remediation(
                        kind,
                        &rem_config,
                        step,
                        idx,
                        step_outcome,
                        &mut container_for_step,
                    );
                }
            }

            // A setup step (or its remediation) that failed after a cancel
            // arrived is part of the cancelled run, not a failure: the abort
            // policy must not fire, since that would suppress teardown.
            if step_outcome.failed && options.observe_cancel && self.cancel_requested() {
                outcome.cancelled = true;
                break;
            }

            if !step_outcome.failed {
                self.state.phase_step_states_mut(kind)[idx].status = PhaseStepStatus::Succeeded;
                self.persist()?;
                self.frontend.on_phase_step_completed(kind, &desc);
            } else {
                let error = self.phase_step_failed_error(kind, idx);
                self.frontend.on_phase_step_failed(
                    kind,
                    &desc,
                    step_outcome.reported_exit_code(),
                    &error,
                );
                outcome.any_failed = true;
                if abort {
                    outcome.aborted = true;
                    // A setup abort stops the whole run: the main phase must
                    // not start and teardown must not run. Teardown is already
                    // the last phase, so an abort there needs no such flag.
                    if kind == PhaseKind::Setup {
                        self.abort_on_failure_triggered = true;
                    }
                    break;
                }
            }
        }

        // A cancel that arrived during the last teardown step still makes the
        // run cancelled: the command has not finished yet.
        if kind == PhaseKind::Teardown && self.cancel_requested() {
            outcome.cancelled = true;
        }

        // An aborted or cancelled setup did not finish: leaving
        // `setup_completed` false is what makes a resumed run re-run it, and
        // leaving `current_phase` at `Setup` keeps the run out of the main
        // phase. Teardown is terminal, so an abort there still records
        // completion and moves to `Done`.
        if !(outcome.aborted || outcome.cancelled) || kind == PhaseKind::Teardown {
            self.state.set_phase_completed(kind);
            self.state.current_phase = kind.next_workflow_phase();
        }
        self.persist()?;
        Ok(outcome)
    }

    /// Whether the teardown phase runs at all: after a successful workflow, or
    /// after a failed one only when the workflow asked for
    /// `teardown_on_failure`.
    pub fn teardown_applies(workflow_succeeded: bool, teardown_on_failure: bool) -> bool {
        workflow_succeeded || teardown_on_failure
    }

    /// Execute a shell command in a container for a phase step.
    /// Returns a [`PhaseStepOutcome`] carrying the failure flag and, on
    /// failure, the full captured stdout/stderr, which feeds the remediation
    /// agent's failure file.
    pub(super) fn run_shell_phase_step(
        &mut self,
        container: &dyn AgentExec,
        command: &str,
        env: Option<&std::collections::HashMap<String, String>>,
        kind: PhaseKind,
        idx: usize,
    ) -> PhaseStepOutcome {
        let result = match container.exec_streaming(command, env, &mut |line| {
            self.frontend
                .on_phase_step_output(kind, &self.redaction.redact(line));
        }) {
            Ok(mut r) => {
                r.stdout = self.redaction.redact(&r.stdout);
                r.stderr = self.redaction.redact(&r.stderr);
                r
            }
            Err(e) => {
                let error = self.redaction.redact(&e.to_string());
                self.set_phase_step_failed(kind, idx, &error);
                // The runtime never produced an ExecOutput, so stdout is empty
                // and stderr carries the launch error for the failure file.
                return PhaseStepOutcome::failed(String::new(), error);
            }
        };

        if result.exit_code != 0 {
            // The exit code travels in the outcome to `on_phase_step_failed`;
            // wording a failure (including one with nothing on stderr, as an
            // interactive run's never has) is the frontend's job (F-45).
            self.set_phase_step_failed(kind, idx, &result.stderr);
            return PhaseStepOutcome::exited(result.exit_code, result.stdout, result.stderr);
        }

        PhaseStepOutcome::succeeded()
    }

    /// Record a phase step as failed in the persisted state. Does NOT notify
    /// the frontend — terminal-failure notification (`on_phase_step_failed`)
    /// is fired by the outer phase loop only after any `on_failure`
    /// remediation is exhausted, so frontends don't see a misleading failure
    /// event when remediation succeeds.
    pub(super) fn set_phase_step_failed(&mut self, kind: PhaseKind, idx: usize, error: &str) {
        use crate::data::workflow_state::PhaseStepStatus;

        self.state.phase_step_states_mut(kind)[idx].status = PhaseStepStatus::Failed {
            error: self.redaction.redact(error),
        };
        let _ = self.persist();
    }

    /// Read the last recorded error string for a failed phase step, or
    /// `"unknown error"` if the state isn't `Failed`.
    pub(super) fn phase_step_failed_error(&self, kind: PhaseKind, idx: usize) -> String {
        use crate::data::workflow_state::PhaseStepStatus;
        match self
            .state
            .phase_step_states(kind)
            .get(idx)
            .map(|s| &s.status)
        {
            Some(PhaseStepStatus::Failed { error }) => error.clone(),
            _ => "unknown error".to_string(),
        }
    }

    /// Execute a PollCi step natively. Returns `true` if the step failed.
    pub(super) fn run_poll_ci_phase_step(
        &mut self,
        interval_secs: u32,
        max_retries: u32,
        kind: PhaseKind,
        idx: usize,
    ) -> bool {
        let git_root = self.session.git_root().to_path_buf();
        let poller = poll_ci::CiPoller::new(
            std::sync::Arc::new(crate::engine::git::GitEngine::new()),
            self.ci_token.clone(),
        )
        .with_destination(self.ci_destination.clone())
        .with_host_policy(self.ci_refused_binaries.clone(), self.redaction.clone());
        // The poller reports facts; the frontend draws them. The default
        // `report_ci_poll` writes the same narration this closure used to
        // compose (F-45).
        let result = poller.poll(&git_root, interval_secs, max_retries, |mut event| {
            if let crate::data::ci_poll_event::CiPollEvent::Failed { detail } = &mut event {
                *detail = self.redaction.redact(detail);
            }
            self.frontend.report_ci_poll(&event);
        });

        if let Err(e) = result {
            let error = self.redaction.redact(&e.to_string());
            self.set_phase_step_failed(kind, idx, &error);
            return true;
        }

        false
    }

    /// Execute a single phase step. Returns a [`PhaseStepOutcome`] carrying
    /// the failure flag and, on failure, the captured stdout/stderr (threaded
    /// to the remediation agent's failure file).
    pub(super) fn run_single_phase_step<S, F>(
        &mut self,
        kind: PhaseKind,
        step: &S,
        idx: usize,
        container_for_step: &mut F,
    ) -> PhaseStepOutcome
    where
        S: PhaseStepSpec,
        F: FnMut(&PhaseStepRef) -> Result<Box<dyn AgentExec>, EngineError>,
    {
        if let Some((interval_secs, max_retries)) = step.poll_ci() {
            let failed = self.run_poll_ci_phase_step(interval_secs, max_retries, kind, idx);
            // PollCi produces no command stdout/stderr; surface the recorded
            // error string as the failure content so the file is still useful.
            return if failed {
                PhaseStepOutcome::failed(String::new(), self.phase_step_failed_error(kind, idx))
            } else {
                PhaseStepOutcome::succeeded()
            };
        }

        let default_options = StepCommandOptions::default();
        let options = self
            .step_command_options
            .iter()
            .find(|((phase, index), _)| *phase == kind && *index == idx)
            .map(|(_, options)| options)
            .unwrap_or(&default_options);
        let (command, env) = step.to_shell_with(options);
        let step_ref = PhaseStepRef {
            kind,
            index: idx,
            description: self.state.phase_step_states(kind)[idx].description.clone(),
        };
        match container_for_step(&step_ref) {
            Ok(c) => self.run_shell_phase_step(&*c, &command, env.as_ref(), kind, idx),
            Err(e) => {
                let error = self.redaction.redact(&e.to_string());
                self.set_phase_step_failed(kind, idx, &error);
                PhaseStepOutcome::failed(String::new(), error)
            }
        }
    }

    /// Run on_failure remediation for a phase step. Returns the outcome of
    /// the last retry: succeeded if remediation fixed the step, otherwise the
    /// final failure (with its exit code). With no attempt made, `failure`
    /// comes back unchanged.
    ///
    /// `failure` is the outcome that triggered this remediation. Each retry that fails again overwrites the failure file
    /// with its own fresh output, so the agent always sees the most recent
    /// failure.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_phase_remediation<S, F>(
        &mut self,
        kind: PhaseKind,
        config: &crate::data::workflow_definition::RemediationConfig,
        step: &S,
        idx: usize,
        failure: PhaseStepOutcome,
        container_for_step: &mut F,
    ) -> PhaseStepOutcome
    where
        S: PhaseStepSpec,
        F: FnMut(&PhaseStepRef) -> Result<Box<dyn AgentExec>, EngineError>,
    {
        use crate::data::workflow_state::PhaseStepStatus;

        let desc = self.state.phase_step_states(kind)[idx].description.clone();
        let mut last = failure;
        for attempt in 1..=config.max_attempts {
            if self.cancel_requested() {
                break;
            }
            self.msg_info(format!(
                "Step failed — launching on_failure agent (attempt {attempt}/{})...",
                config.max_attempts,
            ));

            self.state.phase_step_states_mut(kind)[idx].status = PhaseStepStatus::Remediating {
                attempt,
                of: config.max_attempts,
            };
            let _ = self.persist();
            self.frontend
                .on_phase_step_fixing(kind, &desc, attempt, config.max_attempts);

            self.launch_on_failure_agent(
                config,
                Some(PhaseFailureContext {
                    kind,
                    step_name: &desc,
                    stdout: &last.stdout,
                    stderr: &last.stderr,
                }),
            );

            // The agent was stopped by a cancel: do not retry the step.
            if self.cancel_requested() {
                self.set_phase_step_failed(kind, idx, "cancelled during on_failure remediation");
                break;
            }

            self.state.phase_step_states_mut(kind)[idx].status = PhaseStepStatus::Running;
            let _ = self.persist();

            let outcome = self.run_single_phase_step(kind, step, idx, container_for_step);
            if !outcome.failed {
                self.msg_info(format!(
                    "on_failure remediation succeeded on attempt {attempt}"
                ));
                return outcome;
            }
            // Retain the freshest failure so the next attempt's file reflects
            // this retry, not the original failure.
            last = outcome;

            if attempt == config.max_attempts {
                self.msg_warning(format!(
                    "on_failure exhausted all {} attempts; step fully failed",
                    config.max_attempts,
                ));
            }
        }

        last
    }

    /// Launch the on_failure agent container and wait for it to complete.
    /// The agent's own exit code is ignored — only the subsequent retry
    /// determines success.
    ///
    /// When `failure` is `Some` (teardown remediation), the failed command's
    /// stdout/stderr are written to a file mounted into the agent's container
    /// and a preamble pointing the agent at that file is prepended to the
    /// remediation prompt (Feature B). Setup remediation passes `None`.
    pub(super) fn launch_on_failure_agent(
        &mut self,
        config: &crate::data::workflow_definition::RemediationConfig,
        failure: Option<PhaseFailureContext<'_>>,
    ) {
        // Owned so it does not hold a borrow on `self.workflow` across the
        // `&mut self` call to `prepare_phase_failure_file` below.
        let agent_name_str = config
            .agent
            .as_deref()
            .or(self.workflow.agent.as_deref())
            .unwrap_or("claude")
            .to_string();
        let model = config
            .model
            .as_deref()
            .or(self.workflow.model.as_deref())
            .map(|s| s.to_string())
            .or_else(|| self.effective_config.model());

        let agent_name = match crate::data::session::AgentName::new(&agent_name_str) {
            Ok(a) => a,
            Err(e) => {
                self.msg_warning(format!("on_failure: invalid agent name: {e}"));
                return;
            }
        };

        // Feature B: capture the failed command's output into a file the agent
        // can read, and (when needed) an extra read-only mount. A write failure
        // degrades gracefully — the agent still launches, just without the hint.
        let artifacts = failure
            .as_ref()
            .and_then(|f| self.prepare_phase_failure_file(f));

        let substitution = crate::data::workflow_prompt_template::substitute_prompt(
            &config.prompt,
            self.work_item_context.as_ref(),
        );
        for warning in substitution.warnings {
            self.msg_warning(warning);
        }
        let prompt = match &artifacts {
            Some(a) => a.prepend_preamble(&substitution.rendered),
            None => substitution.rendered,
        };
        let extra_overlays = artifacts
            .as_ref()
            .and_then(|a| a.extra_overlay.clone())
            .map(|o| vec![o]);

        // Countdown label: say which phase step this agent is fixing.
        let label = match &failure {
            Some(f) => format!("on_failure: {}", f.step_name),
            None => "on_failure".to_string(),
        };
        let spec = RemediationAgentSpec {
            label,
            prompt,
            agent: agent_name,
            model,
            extra_overlays,
        };
        let session = self.session.clone();
        let handle = tokio::runtime::Handle::current();
        match handle.block_on(self.run_remediation_agent(&session, spec)) {
            Ok(exit_code) => {
                tracing::info!(exit_code, "on_failure agent completed (exit code ignored)");
            }
            Err(RemediationAgentError::Launch(e)) => {
                self.msg_warning(format!("on_failure: failed to launch agent: {e}"));
            }
            Err(RemediationAgentError::Execution(e)) => {
                self.msg_warning(format!("on_failure: agent execution error: {e}"));
            }
        }
    }

    /// Launch `spec` as an agent container through the engine's agent
    /// factory, mounted for `session`, and wait for it: the `on_failure`
    /// remediation agent of a phase step, or the merge-fix agent the command
    /// layer runs after a failed worktree merge-back. The agent runs in a
    /// container exactly like a workflow step — never on the host. Under
    /// yolo a stuck agent gets the usual countdown. Returns the agent's exit
    /// code.
    ///
    /// Async on purpose: the command layer awaits it directly. The sync
    /// `launch_on_failure_agent` wrapper `block_on`s it, which is only legal
    /// inside `run_phase`'s `block_in_place`.
    pub async fn run_remediation_agent(
        &mut self,
        session: &Session,
        spec: RemediationAgentSpec,
    ) -> Result<i32, RemediationAgentError> {
        let synthetic_step = WorkflowStep {
            name: "__on_failure__".to_string(),
            depends_on: Vec::new(),
            prompt_template: spec.prompt,
            agent: Some(spec.agent.as_str().to_string()),
            model: spec.model.clone(),
            overlays: spec.extra_overlays,
            abort_on_failure: false,
        };

        let runtime = WorkflowRuntimeContext {
            step_agent: spec.agent.clone(),
            step_model: spec.model.clone(),
            git_root: session.git_root().to_path_buf(),
            session_id: session.id(),
            workflow_invocation_id: self.state.invocation_id,
            workflow_step_info: None,
        };

        // Same pre-launch notification main steps get (mod.rs `launch_step`).
        // Frontends rely on it to prepare per-container state — the TUI
        // recreates its AgentIo channels here; skipping it would make the
        // factory's `take_io` find no channels and fail.
        self.frontend.report_step_interactive_launch(
            &synthetic_step,
            spec.agent.as_str(),
            spec.model.as_deref(),
        );

        let execution = self
            .agent_factory
            .execution_for_rendered_step(&synthetic_step, session, &runtime)
            .map_err(RemediationAgentError::Launch)?;

        self.wait_on_failure_agent(execution, &spec.label)
            .await
            .map_err(RemediationAgentError::Execution)
    }

    /// The last successfully launched workflow step's identity, independent
    /// of UI focus. In a parallel group, launch order is source order; the
    /// last launched member supplies repair defaults. `None` when no step launched in this
    /// engine, e.g. a resumed run with every step already done.
    pub fn last_step_agent(&self) -> Option<AgentName> {
        self.last_launched_identity
            .as_ref()
            .map(|(agent, _)| agent.clone())
    }

    /// The model the most recently launched workflow step resolved to.
    pub fn last_step_model(&self) -> Option<String> {
        self.last_launched_identity
            .as_ref()
            .and_then(|(_, model)| model.clone())
    }

    /// Wait for an `on_failure` agent to finish. Under `--yolo` (which
    /// `--dynamic` implies) a stuck agent gets the same yolo countdown a
    /// workflow step does: Esc or Ctrl-W cancels it (the agent keeps running),
    /// fresh output cancels it, and expiry kills the agent so the failed phase
    /// step is retried. An external cancel stops the agent (its `CancelHandle`
    /// and `stop_agent`) and returns `KILLED_EXIT_CODE`; the caller then sees
    /// `cancel_requested()` and skips the retry.
    async fn wait_on_failure_agent(
        &mut self,
        mut exec: AgentExecution,
        label: &str,
    ) -> Result<i32, EngineError> {
        use crate::engine::agent_runtime::execution::KILLED_EXIT_CODE;

        let cancel_handle = exec.cancel_handle();
        let handle = exec.handle().clone();
        let cancel = self.cancel.clone();
        // Stuck detection only matters under yolo; without it the receiver
        // stays `None` and that branch never fires.
        let mut stuck_rx = if self.yolo {
            // Same tab-colouring hookup a main step's container gets.
            self.frontend
                .attach_engine(EngineHandles::stuck(exec.stuck_sender()));
            Some(exec.subscribe_stuck())
        } else {
            None
        };

        let (wait_tx, mut wait_rx) =
            tokio::sync::oneshot::channel::<Result<AgentExitInfo, EngineError>>();
        tokio::spawn(async move {
            let _ = wait_tx.send(exec.wait().await);
        });

        loop {
            tokio::select! {
                biased;
                result = &mut wait_rx => {
                    return result
                        .map_err(|_| EngineError::Other("on_failure wait task dropped unexpectedly".into()))?
                        .map(|e| e.exit_code);
                }
                _ = cancel.cancelled() => {
                    self.msg_warning(format!("Cancel requested; stopping '{label}'"));
                    if let Some(ch) = &cancel_handle {
                        let _ = ch.cancel();
                    }
                    if let Err(e) = self.agent_factory.stop_agent(&handle) {
                        tracing::warn!(error = %e, "cancel: stopping the on_failure agent failed");
                    }
                    self.frontend.report_container_exited(KILLED_EXIT_CODE);
                    return Ok(KILLED_EXIT_CODE);
                }
                Some(event) = Self::recv_stuck(&mut stuck_rx) => {
                    if !matches!(event, StuckEvent::Stuck) {
                        continue;
                    }
                    self.msg_warning(format!("'{label}' appears stuck (no output)"));
                    match self
                        .drive_yolo_countdown(label, &mut wait_rx, &mut stuck_rx)
                        .await?
                    {
                        MidStepYoloResult::StepCompleted(result) => return result.map(|e| e.exit_code),
                        MidStepYoloResult::Advanced => {
                            self.msg_info(format!("Yolo auto-advancing past '{label}'"));
                            if let Some(ch) = &cancel_handle {
                                let _ = ch.cancel();
                            }
                            self.frontend.report_container_exited(KILLED_EXIT_CODE);
                            return Ok(KILLED_EXIT_CODE);
                        }
                        // There is no control board during setup/teardown, so
                        // Ctrl-W only cancels the countdown, like Esc.
                        MidStepYoloResult::Cancelled
                        | MidStepYoloResult::ShowControlBoard
                        | MidStepYoloResult::Recovered => continue,
                    }
                }
            }
        }
    }

    /// Whether this workflow declares a writable `context(workflow)` overlay.
    ///
    /// When true, `~/.awman/context/workflows/{invocation}/` is already mounted
    /// read-write at `/awman/context/workflow` in every agent container, so the
    /// teardown-failure file can be written there directly. A read-only
    /// (`context(workflow:ro)`) declaration returns `false` so the ephemeral
    /// read-only remediation mount is used instead (Feature B edge case).
    ///
    /// The command layer overrides this after merging config/env/CLI/workflow
    /// overlays, so this reflects the active context overlay set.
    pub(super) fn workflow_context_overlay_writable(&self) -> bool {
        matches!(
            self.workflow_context_permission,
            Some(OverlayPermission::ReadWrite)
        )
    }

    /// Write the teardown-failure output file and resolve its mount, returning
    /// the artifacts needed to point the remediation agent at it. Returns
    /// `None` on any failure (directory or file write) after logging a
    /// warning — remediation then proceeds without the file hint, never
    /// aborting (Feature B edge cases).
    pub(super) fn prepare_phase_failure_file(
        &mut self,
        failure: &PhaseFailureContext<'_>,
    ) -> Option<PhaseFailureArtifacts> {
        use crate::data::fs::context_dirs::{validate_context_path, ContextDirResolver};

        let resolver = match ContextDirResolver::from_process_env() {
            Ok(r) => r,
            Err(e) => {
                self.msg_warning(format!(
                    "on_failure: could not resolve context directory, launching agent without \
                     failure output: {e}"
                ));
                return None;
            }
        };

        // The host directory is deterministic for this invocation whether or
        // not context(workflow) is declared — always the workflow context dir.
        let host_dir = resolver.workflow_dir(self.state.invocation_id);

        // Decide the container-visible location. When context(workflow) is
        // active and writable the directory is already mounted read-write at
        // /awman/context/workflow; otherwise mount the (ephemeral) directory
        // read-only at /awman/remediation.
        let use_writable_workflow_overlay = self.workflow_context_overlay_writable();
        let container_path: &'static str = if use_writable_workflow_overlay {
            PHASE_FAILURE_OVERLAY_CONTAINER_PATH
        } else {
            PHASE_FAILURE_EPHEMERAL_CONTAINER_PATH
        };

        // Create the directory (idempotent when the overlay already exists).
        if let Err(e) = std::fs::create_dir_all(&host_dir) {
            self.msg_warning(format!(
                "on_failure: could not create directory {}, launching agent without failure \
                 output: {e}",
                host_dir.display()
            ));
            return None;
        }

        // Security: the resolved path must stay under ~/.awman/context/.
        if let Err(e) = validate_context_path(resolver.awman_home(), &host_dir) {
            self.msg_warning(format!(
                "on_failure: refusing to write failure output outside the context root: {e}"
            ));
            return None;
        }

        let sanitized = sanitize_step_name_for_filename(failure.step_name);
        let filename = format!("{}-failure-{sanitized}.txt", failure.kind.label());
        let file_path = host_dir.join(&filename);
        let contents = self.redaction.redact(&format_phase_failure_file(
            failure.step_name,
            failure.stdout,
            failure.stderr,
        ));
        if let Err(e) = std::fs::write(&file_path, contents) {
            self.msg_warning(format!(
                "on_failure: could not write failure output to {}, launching agent without it: {e}",
                file_path.display()
            ));
            return None;
        }

        let extra_overlay = if use_writable_workflow_overlay {
            None
        } else if self.workflow_context_permission == Some(OverlayPermission::ReadOnly) {
            Some(format!(
                "{}:{}/{}:ro",
                file_path.display(),
                PHASE_FAILURE_EPHEMERAL_CONTAINER_PATH,
                filename
            ))
        } else {
            Some(format!(
                "{}:{}:ro",
                host_dir.display(),
                PHASE_FAILURE_EPHEMERAL_CONTAINER_PATH
            ))
        };

        Some(PhaseFailureArtifacts {
            kind: failure.kind,
            container_path,
            filename,
            step_name: failure.step_name.to_string(),
            extra_overlay,
        })
    }
}
