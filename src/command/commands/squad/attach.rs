//! Layer 2 implementation of `awman squad attach`.
//!
//! Runtime discovery, target selection, phase detection and workflow-step
//! reconciliation live here. Frontends only choose among ambiguous candidates
//! and bind each attached [`AgentInstance`] to their own presentation I/O.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::command::commands::agents::match_reference;
use crate::command::commands::remote_client::{
    RemoteWorkflowPoller, SquadTaskWorkflowSource, WorkflowStateSource,
};
use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::commands::squad::supervisor::SquadGatewayResolver;
use crate::command::commands::Command;
use crate::command::dispatch::catalogue::GatewayNeed;
use crate::command::dispatch::BuildContext;
use crate::command::error::CommandError;
use crate::data::config::env::Env;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::AgentHandle;
use crate::data::workflow_state::{StepState, WorkflowState};
use crate::engine::agent_runtime::{AgentInstance, AgentRuntimeEngine};
use crate::engine::container::naming::SQUAD_NAME_PREFIX;

/// One running squad container, as presented for disambiguation.
#[derive(Debug, Clone)]
pub struct SquadContainer {
    pub handle: AgentHandle,
    /// First 12 characters of the runtime handle ID.
    pub short_id: String,
    /// A workflow step name when known, otherwise the runtime container name.
    pub label: String,
}

/// The non-guessing result of attach target selection.
#[derive(Debug)]
pub enum AttachResolution {
    One(SquadContainer),
    Ambiguous(Vec<SquadContainer>),
}

/// A pure transition produced by [`SquadSlotDriver::reconcile`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotAction {
    Attach {
        step_name: String,
        container_id: String,
        agent: String,
        model: Option<String>,
    },
    Exit {
        step_name: String,
    },
}

/// Diff workflow snapshots into attach/exit transitions.
#[derive(Debug, Default)]
pub struct SquadSlotDriver {
    /// Steps currently handed to the frontend, keyed by step name -> runtime
    /// id/name published by the workflow state.
    slotted: HashMap<String, String>,
}

impl SquadSlotDriver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Diff the authoritative state against the currently slotted steps.
    /// This method performs no runtime calls and no I/O.
    pub fn reconcile(&mut self, state: &WorkflowState) -> Vec<SlotAction> {
        let mut actions = Vec::new();

        let mut slotted_names: Vec<String> = self.slotted.keys().cloned().collect();
        slotted_names.sort();
        for step_name in slotted_names {
            let current_id = self.slotted.get(&step_name);
            let still_same_slot = match state.step_states.get(&step_name) {
                // The scheduler may publish Running just before its runtime
                // id. Keep an existing attachment during that short window.
                Some(StepState::Running { container_id: None }) => true,
                Some(StepState::Running {
                    container_id: Some(container_id),
                }) => current_id == Some(container_id),
                _ => false,
            };
            if !still_same_slot {
                self.slotted.remove(&step_name);
                actions.push(SlotAction::Exit { step_name });
            }
        }

        let mut running: Vec<_> = state.step_states.iter().collect();
        running.sort_by(|(left, _), (right, _)| left.cmp(right));
        for (step_name, step_state) in running {
            let StepState::Running {
                container_id: Some(container_id),
            } = step_state
            else {
                continue;
            };
            if self.slotted.contains_key(step_name) {
                continue;
            }

            let step_info = state.steps.iter().find(|step| step.name == *step_name);
            let agent = step_info
                .and_then(|step| step.agent.clone())
                .unwrap_or_else(|| "agent".to_string());
            let model = step_info.and_then(|step| step.model.clone());
            self.slotted.insert(step_name.clone(), container_id.clone());
            actions.push(SlotAction::Attach {
                step_name: step_name.clone(),
                container_id: container_id.clone(),
                agent,
                model,
            });
        }

        actions
    }

    /// Let a later snapshot retry an attach whose runtime handle was not yet
    /// discoverable or whose frontend binding failed.
    fn retry(&mut self, step_name: &str) {
        self.slotted.remove(step_name);
    }
}

/// Presentation and input required by `squad attach`.
#[async_trait]
pub trait SquadAttachFrontend: UserMessageSink + Send {
    /// Start the frontend's attach-session lifecycle and return the token that
    /// detaching/closing should cancel.
    fn begin_attach(&mut self, _task: &str) -> Result<CancellationToken, CommandError> {
        Ok(CancellationToken::new())
    }

    /// Pick one candidate in an evaluation phase. `None` means cancel.
    fn ask_pick_candidate(
        &mut self,
        candidates: &[SquadContainer],
    ) -> Result<Option<usize>, CommandError>;

    /// Optional metadata used only to label a frontend slot.
    fn on_slot_metadata(&mut self, _step: &str, _agent: &str, _model: Option<&str>) {}

    /// Bind an already-built attach instance to frontend I/O.
    fn on_slot_attached(
        &mut self,
        step: &str,
        instance: Box<dyn AgentInstance>,
    ) -> Result<(), CommandError>;

    /// Remove a presentation slot after its workflow step leaves Running.
    fn on_slot_exited(&mut self, step: &str);

    /// Publish the workflow overview. Non-visual frontends may ignore it.
    fn on_workflow_state(&mut self, _state: &WorkflowState) {}

    /// Frontend-owned reachability indicator, when one exists.
    fn reachable_flag(&self) -> Option<Arc<AtomicBool>> {
        None
    }

    /// Called once after the poll loop ends.
    fn on_attach_finished(&mut self) {}

    /// Process outcome after a frontend-owned local attach client exits.
    fn exit_code(&self) -> i32 {
        0
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SquadAttachOutcome {
    pub exit_code: i32,
}

pub struct SquadAttachCommand {
    task: String,
    requested_container: Option<String>,
    runtime: Arc<dyn AgentRuntimeEngine>,
    gateway: Option<Arc<dyn TaskGateway>>,
}

impl SquadAttachCommand {
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        Ok(Self {
            task: ctx.args.require("name")?,
            requested_container: ctx.flags.string("container"),
            runtime: ctx.engines.runtime.clone(),
            gateway: ctx.gateway.clone(),
        })
    }

    pub async fn run_with_frontend(
        self,
        frontend: Box<dyn SquadAttachFrontend>,
    ) -> Result<SquadAttachOutcome, CommandError> {
        let gateway = match self.gateway {
            Some(gateway) => gateway,
            None => SquadGatewayResolver::from_env(&Env::from_process())?
                .gateway_for(GatewayNeed::Running)
                .await?
                .ok_or_else(|| CommandError::Other("squad daemon not reachable".into()))?,
        };
        let source: Arc<dyn WorkflowStateSource> = Arc::new(SquadTaskWorkflowSource::new(
            gateway.clone(),
            self.task.clone(),
        ));
        let initial_workflow = source.fetch_workflow_state().await;

        let mut candidates = list_task_containers(self.runtime.as_ref(), &self.task)?;
        if let Ok(Some(state)) = &initial_workflow {
            label_with_step_names(&mut candidates, state);
        }

        // An explicit selector is always validated against runtime discovery,
        // including during a workflow phase. It must never escape the task's
        // authoritative candidate set.
        let explicit_target = if let Some(requested) = self.requested_container.as_deref() {
            match resolve_attach_target(candidates.clone(), &self.task, Some(requested))? {
                AttachResolution::One(candidate) => Some(candidate),
                AttachResolution::Ambiguous(_) => unreachable!("an explicit target is exact"),
            }
        } else {
            None
        };
        let explicit_attach = explicit_target.is_some();

        let workflow_phase = matches!(&initial_workflow, Ok(Some(_)));
        if candidates.is_empty() && !workflow_phase {
            return Err(no_run_in_progress(&self.task));
        }

        let frontend = Arc::new(Mutex::new(frontend));
        let cancel = frontend
            .lock()
            .map_err(|_| CommandError::Other("squad attach frontend is unavailable".into()))?
            .begin_attach(&self.task)?;

        let driver = Arc::new(Mutex::new(SquadSlotDriver::new()));
        let evaluation_attached = Arc::new(AtomicBool::new(false));

        if let Some(target) = explicit_target {
            let workflow_metadata = initial_workflow
                .as_ref()
                .ok()
                .and_then(|state| state.as_ref())
                .and_then(|state| workflow_slot_for_handle(state, &target.handle));
            let (step, agent, model) = match workflow_metadata {
                Some(metadata) => metadata,
                None => {
                    let (agent, model) = task_agent_metadata(&gateway, &self.task).await;
                    ("evaluation".to_string(), agent, model)
                }
            };
            let instance = self.runtime.attach(&target.handle)?;
            let mut locked = frontend
                .lock()
                .map_err(|_| CommandError::Other("squad attach frontend is unavailable".into()))?;
            locked.on_slot_metadata(&step, &agent, model.as_deref());
            locked.on_slot_attached(&step, instance)?;
        } else if let Ok(Some(state)) = &initial_workflow {
            apply_workflow_state(
                state,
                self.runtime.as_ref(),
                &driver,
                &frontend,
                &evaluation_attached,
            );
        } else {
            let target = choose_target(&frontend, candidates, &self.task)?;
            let (agent, model) = task_agent_metadata(&gateway, &self.task).await;
            let instance = self.runtime.attach(&target.handle)?;
            let mut locked = frontend
                .lock()
                .map_err(|_| CommandError::Other("squad attach frontend is unavailable".into()))?;
            locked.on_slot_metadata("evaluation", &agent, model.as_deref());
            locked.on_slot_attached("evaluation", instance)?;
            evaluation_attached.store(true, Ordering::Relaxed);
        }

        let callback_runtime = self.runtime.clone();
        let callback_driver = driver.clone();
        let callback_frontend = frontend.clone();
        let callback_evaluation = evaluation_attached.clone();
        let on_state: Box<dyn FnMut(&WorkflowState) + Send> = Box::new(move |state| {
            if explicit_attach {
                if let Ok(mut frontend) = callback_frontend.lock() {
                    frontend.on_workflow_state(state);
                }
            } else {
                apply_workflow_state(
                    state,
                    callback_runtime.as_ref(),
                    &callback_driver,
                    &callback_frontend,
                    &callback_evaluation,
                );
            }
        });

        let reachable = frontend
            .lock()
            .ok()
            .and_then(|locked| locked.reachable_flag())
            .unwrap_or_else(|| Arc::new(AtomicBool::new(initial_workflow.is_ok())));
        reachable.store(initial_workflow.is_ok(), Ordering::Relaxed);
        let poller = RemoteWorkflowPoller::new(source, on_state)
            .with_reachable(reachable)
            .with_initial_state_seen(matches!(initial_workflow, Ok(Some(_))));
        let _ = poller.start(cancel).await;

        let mut locked = frontend
            .lock()
            .map_err(|_| CommandError::Other("squad attach frontend is unavailable".into()))?;
        locked.on_attach_finished();
        Ok(SquadAttachOutcome {
            exit_code: locked.exit_code(),
        })
    }
}

async fn task_agent_metadata(
    gateway: &Arc<dyn TaskGateway>,
    task: &str,
) -> (String, Option<String>) {
    gateway
        .get(task)
        .await
        .map(|task| {
            (
                task.agent.unwrap_or_else(|| "agent".to_string()),
                task.model,
            )
        })
        .unwrap_or_else(|_| ("agent".to_string(), None))
}

fn workflow_slot_for_handle(
    state: &WorkflowState,
    handle: &AgentHandle,
) -> Option<(String, String, Option<String>)> {
    let (step_name, _) = state.step_states.iter().find(|(_, step_state)| {
        matches!(
            step_state,
            StepState::Running {
                container_id: Some(container_id)
            } if container_id_matches_handle(handle, container_id)
        )
    })?;
    let info = state.steps.iter().find(|step| step.name == *step_name);
    Some((
        step_name.clone(),
        info.and_then(|step| step.agent.clone())
            .unwrap_or_else(|| "agent".to_string()),
        info.and_then(|step| step.model.clone()),
    ))
}

fn choose_target(
    frontend: &Arc<Mutex<Box<dyn SquadAttachFrontend>>>,
    candidates: Vec<SquadContainer>,
    task: &str,
) -> Result<SquadContainer, CommandError> {
    match resolve_attach_target(candidates, task, None)? {
        AttachResolution::One(candidate) => Ok(candidate),
        AttachResolution::Ambiguous(candidates) => {
            let choice = frontend
                .lock()
                .map_err(|_| CommandError::Other("squad attach frontend is unavailable".into()))?
                .ask_pick_candidate(&candidates)?;
            choice
                .and_then(|index| candidates.get(index).cloned())
                .ok_or(CommandError::Aborted)
        }
    }
}

fn apply_workflow_state(
    state: &WorkflowState,
    runtime: &dyn AgentRuntimeEngine,
    driver: &Arc<Mutex<SquadSlotDriver>>,
    frontend: &Arc<Mutex<Box<dyn SquadAttachFrontend>>>,
    evaluation_attached: &Arc<AtomicBool>,
) {
    let Ok(mut frontend) = frontend.lock() else {
        return;
    };
    frontend.on_workflow_state(state);
    if evaluation_attached.swap(false, Ordering::Relaxed) {
        frontend.on_slot_exited("evaluation");
    }
    let Ok(mut driver) = driver.lock() else {
        return;
    };
    for action in driver.reconcile(state) {
        match action {
            SlotAction::Exit { step_name } => frontend.on_slot_exited(&step_name),
            SlotAction::Attach {
                step_name,
                container_id,
                agent,
                model,
            } => {
                let Some(handle) = handle_for_container_id(runtime, &container_id) else {
                    driver.retry(&step_name);
                    continue;
                };
                match runtime.attach(&handle) {
                    Ok(instance) => {
                        frontend.on_slot_metadata(&step_name, &agent, model.as_deref());
                        if let Err(error) = frontend.on_slot_attached(&step_name, instance) {
                            driver.retry(&step_name);
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!(
                                    "failed to attach workflow step {step_name:?}: {error}"
                                ),
                            });
                        }
                    }
                    Err(error) => {
                        driver.retry(&step_name);
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("failed to attach workflow step {step_name:?}: {error}"),
                        });
                    }
                }
            }
        }
    }
}

#[async_trait]
impl Command for SquadAttachCommand {
    type Frontend = Box<dyn SquadAttachFrontend>;
    type Outcome = SquadAttachOutcome;

    async fn run_with_frontend(
        self,
        frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        SquadAttachCommand::run_with_frontend(self, frontend).await
    }
}

/// The running-name prefix for one squad task.
pub fn squad_name_prefix(task: &str) -> String {
    format!("{SQUAD_NAME_PREFIX}{task}-")
}

/// Discover running containers for one task through the cross-runtime trait.
pub fn list_task_containers(
    runtime: &dyn AgentRuntimeEngine,
    task: &str,
) -> Result<Vec<SquadContainer>, CommandError> {
    Ok(runtime
        .list_running_with_name_prefix(&squad_name_prefix(task))?
        .into_iter()
        .map(|handle| SquadContainer {
            short_id: handle.id.chars().take(12).collect(),
            label: handle.name.clone(),
            handle,
        })
        .collect())
}

/// Best-effort label enrichment. This never changes the candidate set/order.
pub fn label_with_step_names(candidates: &mut [SquadContainer], state: &WorkflowState) {
    for (step_name, step_state) in &state.step_states {
        let StepState::Running {
            container_id: Some(container_id),
        } = step_state
        else {
            continue;
        };
        for candidate in candidates.iter_mut() {
            if candidate.handle.id == container_id.as_str()
                || candidate.handle.name == container_id.as_str()
            {
                candidate.label = step_name.clone();
            }
        }
    }
}

/// Resolve a candidate without guessing.
pub fn resolve_attach_target(
    candidates: Vec<SquadContainer>,
    task: &str,
    requested: Option<&str>,
) -> Result<AttachResolution, CommandError> {
    if let Some(requested) = requested {
        // The same matching rules as `awman agents`, over this task's
        // containers only. An ambiguous prefix is reported like a miss.
        let handles: Vec<AgentHandle> = candidates.iter().map(|c| c.handle.clone()).collect();
        let matched = match_reference(&handles, requested)
            .ok()
            .flatten()
            .and_then(|handle| candidates.iter().find(|c| c.handle.id == handle.id));
        return match matched {
            Some(candidate) => Ok(AttachResolution::One(candidate.clone())),
            None => Err(not_in_task(task, requested, &candidates)),
        };
    }

    if candidates.is_empty() {
        return Err(no_run_in_progress(task));
    }

    match candidates.len() {
        1 => Ok(AttachResolution::One(
            candidates.into_iter().next().expect("one candidate"),
        )),
        _ => Ok(AttachResolution::Ambiguous(candidates)),
    }
}

/// The explicit idle-task error shape shared by both frontends.
pub fn no_run_in_progress(task: &str) -> CommandError {
    CommandError::Other(format!("no run currently in progress for task {task:?}"))
}

/// An explicit `--container` is never allowed to escape the discovered set.
pub fn not_in_task(task: &str, requested: &str, set: &[SquadContainer]) -> CommandError {
    let legal = if set.is_empty() {
        "(none)".to_string()
    } else {
        set.iter()
            .map(|candidate| candidate.short_id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    CommandError::Other(format!(
        "container {requested:?} is not a running container for task {task:?}; legal containers: {legal}"
    ))
}

/// One line per candidate for prompts and errors.
pub fn format_candidates(candidates: &[SquadContainer]) -> Vec<String> {
    candidates
        .iter()
        .map(|candidate| format!("{}  {}", candidate.short_id, candidate.label))
        .collect()
}

/// Resolve a daemon-reported id against currently running runtime handles.
pub fn handle_for_container_id(
    runtime: &dyn AgentRuntimeEngine,
    container_id: &str,
) -> Option<AgentHandle> {
    if container_id.is_empty() {
        return None;
    }
    runtime
        .list_running_all()
        .ok()?
        .into_iter()
        .find(|handle| container_id_matches_handle(handle, container_id))
}

fn container_id_matches_handle(handle: &AgentHandle, container_id: &str) -> bool {
    handle.id.starts_with(container_id)
        || container_id.starts_with(&handle.id)
        || handle.name == container_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::workflow_definition::WorkflowStep;

    fn candidate(id: &str, name: &str) -> SquadContainer {
        SquadContainer {
            short_id: id.chars().take(12).collect(),
            label: name.to_string(),
            handle: AgentHandle {
                id: id.to_string(),
                image_tag: "img".to_string(),
                name: name.to_string(),
                started_at: chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap(),
            },
        }
    }

    fn workflow_state(steps: &[(&str, StepState, Option<&str>, Option<&str>)]) -> WorkflowState {
        let definition: Vec<WorkflowStep> = steps
            .iter()
            .map(|(name, _, agent, model)| WorkflowStep {
                name: (*name).to_string(),
                depends_on: Vec::new(),
                prompt_template: String::new(),
                agent: agent.map(str::to_string),
                model: model.map(str::to_string),
                overlays: None,
                abort_on_failure: false,
            })
            .collect();
        let mut state = WorkflowState::new("attach-test".into(), &definition, "hash".into(), None);
        for (name, status, _, _) in steps {
            state.set_status(name, status.clone());
        }
        state
    }

    #[test]
    fn slot_driver_reconciles_running_transitions_without_duplicates() {
        let mut driver = SquadSlotDriver::new();
        let no_id = workflow_state(&[(
            "build",
            StepState::Running { container_id: None },
            Some("claude"),
            Some("sonnet"),
        )]);
        assert!(driver.reconcile(&no_id).is_empty());

        let running = workflow_state(&[(
            "build",
            StepState::Running {
                container_id: Some("abc123".into()),
            },
            Some("claude"),
            Some("sonnet"),
        )]);
        assert_eq!(
            driver.reconcile(&running),
            vec![SlotAction::Attach {
                step_name: "build".into(),
                container_id: "abc123".into(),
                agent: "claude".into(),
                model: Some("sonnet".into()),
            }]
        );
        assert!(driver.reconcile(&running).is_empty());

        let restarted = workflow_state(&[(
            "build",
            StepState::Running {
                container_id: Some("def456".into()),
            },
            Some("claude"),
            Some("sonnet"),
        )]);
        assert_eq!(
            driver.reconcile(&restarted),
            vec![
                SlotAction::Exit {
                    step_name: "build".into()
                },
                SlotAction::Attach {
                    step_name: "build".into(),
                    container_id: "def456".into(),
                    agent: "claude".into(),
                    model: Some("sonnet".into()),
                }
            ]
        );

        let finished = workflow_state(&[("build", StepState::Succeeded, None, None)]);
        assert_eq!(
            driver.reconcile(&finished),
            vec![SlotAction::Exit {
                step_name: "build".into()
            }]
        );
    }

    #[test]
    fn step_labels_resolve_from_a_published_container_name() {
        let mut candidates = vec![candidate("aaaa1111bbbb", "awman-squad-t-00000001")];
        let state = workflow_state(&[(
            "build",
            StepState::Running {
                container_id: Some("awman-squad-t-00000001".to_string()),
            },
            None,
            None,
        )]);
        label_with_step_names(&mut candidates, &state);
        assert_eq!(candidates[0].label, "build");
    }

    #[test]
    fn explicit_container_matches_by_exact_id_name_or_unique_prefix() {
        let candidates = vec![
            candidate("aaaa1111bbbb2222", "awman-squad-t-one"),
            candidate("aaaa9999cccc3333", "awman-squad-t-two"),
        ];
        let picked =
            |requested: &str| match resolve_attach_target(candidates.clone(), "t", Some(requested))
            {
                Ok(AttachResolution::One(c)) => c.handle.name,
                other => panic!("{requested}: expected One, got {other:?}"),
            };
        assert_eq!(picked("aaaa9999cccc3333"), "awman-squad-t-two");
        assert_eq!(picked("awman-squad-t-one"), "awman-squad-t-one");
        assert_eq!(picked("aaaa1"), "awman-squad-t-one");
        assert_eq!(picked("aaaa9"), "awman-squad-t-two");
    }

    #[test]
    fn explicit_container_exact_match_beats_a_prefix_and_ambiguity_is_a_miss() {
        let candidates = vec![candidate("abc", "awman-x"), candidate("abcdef", "awman-y")];
        match resolve_attach_target(candidates.clone(), "t", Some("abc")).unwrap() {
            AttachResolution::One(c) => assert_eq!(c.handle.name, "awman-x"),
            other => panic!("expected One, got {other:?}"),
        }
        // "ab" prefixes both: reported like any container outside the task.
        let err = resolve_attach_target(candidates, "t", Some("ab")).unwrap_err();
        assert!(
            err.to_string().contains("is not a running container"),
            "{err}"
        );
    }
    #[test]
    fn target_resolution_preserves_idle_ambiguous_and_not_in_task_errors() {
        assert!(resolve_attach_target(Vec::new(), "issue-triage", None)
            .unwrap_err()
            .to_string()
            .contains("no run currently in progress"));
        assert_eq!(
            resolve_attach_target(Vec::new(), "issue-triage", Some("cccc3333"))
                .unwrap_err()
                .to_string(),
            "container \"cccc3333\" is not a running container for task \"issue-triage\"; legal containers: (none)"
        );
        let candidates = vec![candidate("aaaa1111", "a"), candidate("bbbb2222", "b")];
        assert!(matches!(
            resolve_attach_target(candidates.clone(), "task", None).unwrap(),
            AttachResolution::Ambiguous(_)
        ));
        assert_eq!(
            resolve_attach_target(candidates, "issue-triage", Some("cccc3333"))
                .unwrap_err()
                .to_string(),
            "container \"cccc3333\" is not a running container for task \"issue-triage\"; legal containers: aaaa1111, bbbb2222"
        );

        let candidates = vec![candidate("aaaa1111bbbb2222", "a")];
        assert_eq!(
            resolve_attach_target(
                candidates,
                "issue-triage",
                Some("aaaa1111bbbb9999")
            )
            .unwrap_err()
            .to_string(),
            "container \"aaaa1111bbbb9999\" is not a running container for task \"issue-triage\"; legal containers: aaaa1111bbbb"
        );
    }
}
