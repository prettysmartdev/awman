//! Workflow prerequisites for `exec workflow` (WI 0149): the start-of-run
//! flow, `--check-prerequisites`, and turning the run's grants into each
//! setup/teardown container's overlays, env and step command options.
//!
//! A child module of `exec_workflow`. The grants built here are held by the
//! command (in `PreparedRun`) for the life of the run and are never put on a
//! `Session`; only the secret-free report is mirrored there.

use super::*;
pub(crate) use crate::command::commands::workflow_prerequisites::PreparedPrerequisites;
use crate::command::commands::workflow_prerequisites::{
    WorkflowPrerequisiteFlow, WorkflowPrerequisiteFlowOptions,
};
use crate::data::workflow_definition::{SetupStep, TeardownStep};
use crate::data::workflow_prerequisites::{
    PrerequisiteOrigin, PrerequisiteStepKind, WorkflowPrerequisiteReport, WorkflowPrerequisites,
};
use crate::engine::overlay::prerequisite_grants::{GrantOverlays, GrantTarget};
use crate::engine::workflow::step_commands::StepCommandOptions;

/// What the start-of-run prerequisite step decided.
pub(crate) enum PrerequisiteStart {
    /// Carry on; the grants travel with the run.
    Run(Box<PreparedPrerequisites>),
    /// `--check-prerequisites`: nothing else runs.
    CheckOnly(ExecWorkflowOutcome),
}

impl ExecWorkflowCommand {
    /// The flow for this command, rooted at the session's git root.
    pub(super) fn prerequisite_flow(&self, origin: PrerequisiteOrigin) -> WorkflowPrerequisiteFlow {
        let phase_image = resolve_phase_image(
            &self.session,
            self.session.git_root(),
            self.engines.runtime.capabilities().image_acquisition,
        );
        let mut options = WorkflowPrerequisiteFlowOptions::for_session(
            &self.session,
            self.frontend_kind,
            &self.flags.prerequisites,
            self.flags.squad_task_prerequisites.as_deref(),
            origin,
            Some(phase_image),
        );
        options.worktree_planned = self.flags.worktree;
        WorkflowPrerequisiteFlow::new(&self.engines, &self.session, options)
    }

    /// Probe, show, ask and decide, before the worktree, the image or any
    /// container exists, so every question is asked while the person is
    /// still at the keyboard. `--check-prerequisites` stops here.
    pub(super) async fn start_prerequisites(
        &self,
        needs: &WorkflowPrerequisites,
        workflow_name: String,
        frontend: &SharedWorkflowFrontend,
    ) -> Result<PrerequisiteStart, CommandError> {
        let flow = self.prerequisite_flow(needs.origin);
        if self.flags.prerequisites.check_only {
            let report = flow.preflight(needs, false);
            frontend.with(|f| f.show_workflow_prerequisites(&report))?;
            self.record_prerequisite_report(&report).await;
            return Ok(PrerequisiteStart::CheckOnly(ExecWorkflowOutcome {
                prerequisites: Some(report.clone()),
                workflow: workflow_name,
                exit_code: Some(WorkflowPrerequisiteFlow::check_only_exit_code(&report)),
                worktree_used: false,
            }));
        }
        let prepared = frontend.with(|f| flow.prepare(needs, f));
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(e) => {
                if let CommandError::WorkflowPrerequisitesUnresolved { report } = &e {
                    self.record_prerequisite_report(report).await;
                }
                frontend.with(|f| {
                    f.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec workflow: {e}"),
                    })
                });
                return Err(e);
            }
        };
        self.record_prerequisite_report(&prepared.report).await;
        Ok(PrerequisiteStart::Run(Box::new(prepared)))
    }

    /// Mirror the (secret-free) report into the shared session for viewers.
    pub(super) async fn record_prerequisite_report(&self, report: &WorkflowPrerequisiteReport) {
        if let Some(session) = &self.managed_session {
            session
                .write()
                .await
                .state_mut()
                .record_workflow_prerequisites(report.clone());
        }
    }
}

/// Per-step command options, keyed by phase and step index.
pub(crate) type StepCommandOptionsByStep = Vec<((PhaseKind, usize), StepCommandOptions)>;

/// The grant overlays for one phase, one entry per step, appended after the
/// user's and the step's own overlays. Also returns the per-step command
/// options (SSH key / HTTPS token push auth) the engine renders steps with.
///
/// Only `OverlayEngine::prerequisite_grant_overlays` turns grants into mounts
/// or env; step kinds that never receive grants (`run_shell`, `run_script`,
/// `checkout_create_branch`, `poll_ci`) get nothing from it. Secret values go
/// into the launch env map, which reaches the container by name only.
pub(crate) fn apply_grant_overlays(
    engines: &Engines,
    prepared: &PreparedPrerequisites,
    phase: PhaseKind,
    kinds: &[(PrerequisiteStepKind, Option<String>)],
    resolved: &mut [PhaseOverlayResult],
    image_tag: &str,
    sink: &mut dyn UserMessageSink,
) -> (Vec<(usize, GrantOverlays)>, StepCommandOptionsByStep) {
    let container_home = engines
        .container_runtime
        .as_ref()
        .and_then(|rt| rt.image_home_dir(image_tag));
    let mut keep = Vec::new();
    let mut options = Vec::new();
    for (index, (kind, remote)) in kinds.iter().enumerate() {
        let Some(Ok((overlays, env))) = resolved.get_mut(index) else {
            continue;
        };
        let remote_url = remote
            .as_deref()
            .or(Some("origin"))
            .and_then(|r| prepared.remote(r));
        let target = GrantTarget {
            git_source: prepared.flow.workflow_git_source(),
            repo: Some(
                prepared
                    .flow
                    .options()
                    .worktree
                    .as_deref()
                    .unwrap_or(&prepared.flow.options().git_root),
            ),
            step_kind: *kind,
            phase,
            container_home: container_home.as_deref(),
            remote: remote_url.as_ref(),
        };
        let grant = match engines
            .overlay_engine
            .prerequisite_grant_overlays(&target, &prepared.grants)
        {
            Ok(grant) => grant,
            Err(e) => {
                sink.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: format!(
                        "{} step {index} ({}): workflow prerequisite grants not applied: {e}",
                        phase.label(),
                        kind.label()
                    ),
                });
                continue;
            }
        };
        if grant.is_empty() {
            continue;
        }
        overlays.extend(grant.overlays.iter().cloned());
        for (name, value) in &grant.env_literals {
            env.insert(name.clone(), value.clone());
        }
        if !grant.display.is_empty() {
            sink.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!(
                    "{} step {index} ({}): {}",
                    phase.label(),
                    kind.label(),
                    grant.display.join(", ")
                ),
            });
        }
        options.push((
            (phase, index),
            StepCommandOptions {
                push_auth: grant.push_auth.clone(),
                remote_url: remote_url.as_ref().map(|r| {
                    prepared
                        .grants
                        .ssh_transports
                        .get(&r.redacted)
                        .map_or_else(|| r.redacted.clone(), |u| u.expose().to_string())
                }),
            },
        ));
        keep.push((index, grant));
    }
    (keep, options)
}

/// Step kind and remote (name or URL) of every teardown step.
pub(crate) fn teardown_grant_kinds(
    steps: &[TeardownStep],
) -> Vec<(PrerequisiteStepKind, Option<String>)> {
    steps
        .iter()
        .map(|step| {
            let remote = match step {
                TeardownStep::PushBranch { remote, .. } => remote.clone(),
                _ => None,
            };
            (PrerequisiteStepKind::of_teardown(step), remote)
        })
        .collect()
}

/// Step kind and remote (name or URL) of every setup step.
pub(crate) fn setup_grant_kinds(
    steps: &[SetupStep],
) -> Vec<(PrerequisiteStepKind, Option<String>)> {
    steps
        .iter()
        .map(|step| {
            let remote = match step {
                SetupStep::PullBranch { remote, .. } => remote.clone(),
                SetupStep::CloneRepo { url, .. } => Some(url.clone()),
                _ => None,
            };
            (PrerequisiteStepKind::of_setup(step), remote)
        })
        .collect()
}
