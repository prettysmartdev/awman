//! `TabRelay`: the cut line of the frontend protocol (WI 0142 Part B §2). One
//! per hosted tab, it holds what a presenting TUI `Tab` holds for the builtin
//! engine — the consumer ends of the tab's feeds (`command_result_rx`,
//! `dialog_request_rx`, `dialog_response_tx`, the `ContainerSlotEvent` queue,
//! the shared `TabCommandFeeds`) — and turns them into typed
//! [`InstanceEvent`]s instead of drawing them: command results, dialog
//! requests, container slots (each slot's I/O registered as a hub terminal
//! through `TerminalHub::bridge_agent_channels`), and `TabCommandFeeds` diffs
//! as [`InstanceDelta`]s. The command thread itself is unchanged.
//!
//! The relay assigns no dialog ids and arbitrates no answers: it reports a
//! raised request and forwards the one answer the host's arbitration let
//! through (`EngineInstanceHost`, first answer wins).

use std::sync::atomic::Ordering;
use std::sync::Arc;

use chrono::Utc;

use crate::command::commands::agents::{AgentsTuiContext, TuiTabSnapshot};
use crate::command::dispatch::CommandOutcome;
use crate::command::error::CommandError;
use crate::data::annotations::Annotations;
use crate::data::daemon_state::FeatureName;
use crate::data::dialog::{DialogRequest, DialogResponse};
use crate::data::frontend_protocol::{
    AnsweredBy, CommandResultWire, ErrorWire, OutputStream, TabKindWire, WorkflowRequestWire,
};
use crate::data::ids::{DialogId, TabId, TerminalId};
use crate::data::instance_snapshot::{
    AgentStatsWire, AgentsDashboardWire, ContainerWindowState, ExecutionPhase, InstanceDelta,
    LabelColorWire, SessionView, SlotId, SlotSnapshot, StatusLine, StepViewStatus, TabLabelWire,
    TabSnapshot, WorkflowViewState, YoloState,
};
use crate::data::message::MessageLevel;
use crate::data::program_status::EffectiveStatus;
use crate::data::session::{Session, SessionId};
use crate::data::terminal::{DetachReason, TerminalOwner, TerminalOwnerInfo};
use crate::data::workflow_state::CountdownKind;
use crate::engine::agent_runtime::execution::{AgentStats, StuckEvent};
use crate::engine::terminal_hub::bridge::BridgedTerminal;
use crate::engine::terminal_hub::{TerminalHub, TerminalRegistration};
use crate::engine::workflow::EngineRequest;

use super::feeds::{
    AgentsDashboardData, ContainerSlotEvent, ContainerSlotIo, SharedYoloCancelFlag,
    TabCommandFeeds, TabFeeds,
};
use super::lifecycle::QuitReason;
use super::SubmittedCommand;

/// Everything that happens in a hosted instance, as the host fans it out to
/// frontends (as `EngineFrame`s) and host presenters. Contract §4.7.
// Events are drained and moved once, like the `EngineFrame`s they become.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum InstanceEvent {
    CommandBytes {
        tab_id: TabId,
        stream: OutputStream,
        bytes: Vec<u8>,
    },
    Delta(InstanceDelta),
    CommandOutput {
        tab_id: TabId,
        stream: OutputStream,
        level: MessageLevel,
        text: String,
    },
    CommandFinished {
        tab_id: TabId,
        result: CommandResultWire,
    },
    /// A command raised a dialog. The relay leaves `dialog_id` to the host.
    DialogRaised {
        tab_id: TabId,
        request: DialogRequest,
    },
    DialogClosed {
        dialog_id: DialogId,
        answered_by: AnsweredBy,
    },
    DismissReportDialog {
        tab_id: TabId,
    },
    ProgramStatus {
        tab_id: TabId,
        slot_id: SlotId,
        status: Option<EffectiveStatus>,
    },
    TabOpened {
        tab_id: TabId,
    },
    TabClosed {
        tab_id: TabId,
    },
    ContainerWindow {
        tab_id: TabId,
        slot_id: SlotId,
        state: ContainerWindowState,
    },
    TerminalOpened {
        tab_id: TabId,
        terminal_id: TerminalId,
    },
    TerminalClosed {
        tab_id: TabId,
        terminal_id: TerminalId,
        exit_code: Option<i32>,
    },
    TerminalViewerDetached {
        terminal_id: TerminalId,
        reason: DetachReason,
        annotations: Annotations,
    },
    Notice {
        level: MessageLevel,
        text: String,
    },
    Ended(QuitReason),
}

/// The wire form of a command's result, including the outcome body, exit code,
/// and error classification used by the CLI and API.
pub fn command_result_wire(result: &Result<CommandOutcome, CommandError>) -> CommandResultWire {
    match result {
        Ok(outcome) => CommandResultWire {
            exit_code: outcome.exit_code(),
            outcome: serde_json::to_value(outcome).ok(),
            error: None,
        },
        Err(error) => CommandResultWire {
            exit_code: i32::from(error.exit_code()),
            outcome: None,
            error: Some(ErrorWire {
                code: error.code().to_string(),
                message: error.to_string(),
            }),
        },
    }
}

/// Where a slot came from: the sequential backbone (keyed by launch id) or a
/// parallel-group step (keyed by step name).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SlotKey {
    Sequential(uuid::Uuid),
    Step(String),
}

struct RelaySlot {
    key: SlotKey,
    snapshot: SlotSnapshot,
    terminal: Option<BridgedTerminal>,
    yolo_cancel: Option<SharedYoloCancelFlag>,
}

impl RelaySlot {
    fn finish(&mut self, exit_code: i32) -> Option<TerminalId> {
        let terminal = self.terminal.take()?;
        terminal.flush_pending_output();
        terminal.handle.exit(exit_code);
        Some(terminal.handle.id().clone())
    }
}

/// The projected workflow presentation: view, invocation, context and worktree.
type WorkflowProjection = (
    Option<crate::data::instance_snapshot::WorkflowViewState>,
    Option<uuid::Uuid>,
    Option<std::path::PathBuf>,
    Option<std::path::PathBuf>,
);

/// One workflow step whose status changed between two projections.
#[derive(Debug, PartialEq, Eq)]
struct StepChange<'a> {
    step: &'a str,
    from: Option<StepViewStatus>,
    to: StepViewStatus,
    agent: Option<&'a str>,
}

/// The step status changes worth a daemon log line: every change into a
/// non-pending status. Re-projecting an unchanged view yields nothing, so the
/// log stays at one line per real transition.
fn workflow_step_changes<'a>(
    previous: Option<&WorkflowViewState>,
    next: &'a WorkflowViewState,
) -> Vec<StepChange<'a>> {
    next.steps
        .iter()
        .filter_map(|step| {
            let from = previous
                .and_then(|p| p.steps.iter().find(|s| s.name == step.name))
                .map(|s| s.status);
            (from != Some(step.status) && step.status != StepViewStatus::Pending).then(|| {
                StepChange {
                    step: &step.name,
                    from,
                    to: step.status,
                    agent: step.agent.as_deref(),
                }
            })
        })
        .collect()
}

fn step_status_label(status: StepViewStatus) -> &'static str {
    match status {
        StepViewStatus::Pending => "pending",
        StepViewStatus::Running => "running",
        StepViewStatus::Fixing => "fixing",
        StepViewStatus::Done => "done",
        StepViewStatus::Error => "error",
        StepViewStatus::Cancelled => "cancelled",
        StepViewStatus::Skipped => "skipped",
    }
}

/// Daemon log lines for a tab's workflow: run start and step transitions.
fn log_workflow_progress(
    tab_id: &TabId,
    previous: Option<&WorkflowProjection>,
    next: &WorkflowProjection,
) {
    let (prev_view, prev_run) = previous
        .map(|p| (p.0.as_ref(), p.1))
        .unwrap_or((None, None));
    if let Some(run) = next.1.filter(|run| Some(*run) != prev_run) {
        tracing::info!(
            tab = %tab_id,
            invocation_id = %run,
            steps = next.0.as_ref().map_or(0, |v| v.steps.len()),
            "workflow run started"
        );
    }
    let Some(view) = next.0.as_ref() else {
        return;
    };
    // A new run starts its step statuses from scratch.
    let prev_view = if next.1 == prev_run { prev_view } else { None };
    for change in workflow_step_changes(prev_view, view) {
        let from = change.from.map_or("new", step_status_label);
        let to = step_status_label(change.to);
        let agent = change.agent.unwrap_or("");
        match change.to {
            StepViewStatus::Error => tracing::warn!(
                tab = %tab_id, step = change.step, from, to, "workflow step failed"
            ),
            _ => tracing::info!(
                tab = %tab_id, step = change.step, from, to, agent, "workflow step"
            ),
        }
    }
}

/// The last projected state, for diffing `TabCommandFeeds` into deltas.
#[derive(Default)]
struct Projection {
    status_lines: usize,
    workflow: Option<WorkflowProjection>,
    yolo: Option<YoloState>,
    phase: Option<ExecutionPhase>,
    agents_dashboard: Option<AgentsDashboardData>,
    /// The sequential agent's stuck state, as last relayed.
    stuck: bool,
}

/// The `agents status` table as it crosses the protocol.
fn dashboard_wire(data: &AgentsDashboardData) -> Option<AgentsDashboardWire> {
    Some(AgentsDashboardWire {
        overview: serde_json::to_value(&data.overview).ok()?,
        watching: data.watching,
    })
}

/// One hosted tab's relay.
pub struct TabRelay {
    tab_id: TabId,
    session_id: SessionId,
    kind: TabKindWire,
    session: SessionView,
    feeds: TabCommandFeeds,
    command: Option<TabFeeds>,
    hub: Option<Arc<TerminalHub>>,
    sandboxed: bool,
    pty_size: (u16, u16),
    slots: Vec<RelaySlot>,
    focused: Option<SlotId>,
    engine_requests: Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>,
    stuck: Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
    /// The sequential launch whose agent reports arrive on `stuck`.
    stuck_launch: Option<uuid::Uuid>,
    projection: Projection,
    status_log: Vec<StatusLine>,
    phase: ExecutionPhase,
    current_workflow: Option<crate::data::workflow_state::WorkflowSummary>,
    current_container: Option<crate::data::session::AgentHandle>,
    yolo_mode: bool,
    annotations: Annotations,
}

/// The wire view of a session.
pub fn session_view(session: &Session) -> SessionView {
    SessionView {
        session_id: session.id(),
        session_type: session.session_type().clone(),
        cwd: session.working_dir().to_path_buf(),
        git_root: session.git_root().to_path_buf(),
        default_agent: session.default_agent().map(|agent| agent.to_string()),
        labels: Default::default(),
    }
}

impl TabRelay {
    pub fn new(
        tab_id: TabId,
        session: &Session,
        kind: TabKindWire,
        feeds: TabCommandFeeds,
        hub: Option<Arc<TerminalHub>>,
        pty_size: (u16, u16),
    ) -> Self {
        Self {
            tab_id,
            session_id: session.id(),
            kind,
            session: session_view(session),
            phase: ExecutionPhase::of_session_state(session.state()),
            current_workflow: session.state().current_workflow.clone(),
            current_container: session.state().current_container.clone(),
            feeds,
            command: None,
            hub,
            sandboxed: false,
            pty_size,
            slots: Vec::new(),
            focused: None,
            engine_requests: None,
            stuck: None,
            stuck_launch: None,
            projection: Projection {
                workflow: Some((None, None, None, None)),
                phase: Some(ExecutionPhase::of_session_state(session.state())),
                ..Default::default()
            },
            status_log: Vec::new(),
            yolo_mode: false,
            annotations: Annotations::new(),
        }
    }

    pub fn tab_id(&self) -> &TabId {
        &self.tab_id
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn annotations(&self) -> &Annotations {
        &self.annotations
    }

    pub fn annotations_mut(&mut self) -> &mut Annotations {
        &mut self.annotations
    }

    /// Whether `terminal_id` is one of this tab's slot terminals.
    pub fn owns_terminal(&self, terminal_id: &TerminalId) -> bool {
        self.slots.iter().any(|slot| {
            slot.terminal
                .as_ref()
                .is_some_and(|t| t.handle.id() == terminal_id)
        })
    }

    /// Fresh feeds for the next command, exactly as the TUI resets a tab
    /// before a submission: the previous command's feeds are cancelled and
    /// its slots end.
    pub fn reset_for_submit(&mut self) -> (TabCommandFeeds, Vec<InstanceEvent>) {
        let mut events = self.end_slots();
        self.feeds.reset_for_new_command();
        self.command = None;
        self.engine_requests = None;
        self.stuck = None;
        self.stuck_launch = None;
        // A new command starts unstuck, as a fresh local slot does.
        if self.projection.stuck {
            self.projection.stuck = false;
            events.push(self.delta(InstanceDelta::Stuck {
                tab_id: self.tab_id.clone(),
                stuck: false,
            }));
        }
        if !self.status_log.is_empty() {
            self.status_log.clear();
            events.push(self.delta(InstanceDelta::StatusLogReset {
                tab_id: self.tab_id.clone(),
            }));
        }
        self.projection.status_lines = 0;
        (self.feeds.clone(), events)
    }

    /// Bind a submitted command (or the tab's startup command): its channel
    /// ends, and its first container slot as a hub terminal.
    pub fn bind(&mut self, submitted: SubmittedCommand) -> Vec<InstanceEvent> {
        self.command = Some(submitted.feeds);
        self.sandboxed = submitted.sandboxed;
        self.yolo_mode = submitted.display.unattended;
        let mut events = vec![self.delta(InstanceDelta::YoloMode {
            tab_id: self.tab_id.clone(),
            on: self.yolo_mode,
        })];
        let launch_id = self.feeds.container.launch_id();
        events.extend(self.open_slot(
            SlotKey::Sequential(launch_id),
            String::new(),
            submitted.display.agent_display_name.clone(),
            None,
            Some(submitted.slot_io),
            false,
        ));
        events
    }

    fn delta(&self, delta: InstanceDelta) -> InstanceEvent {
        InstanceEvent::Delta(delta)
    }

    fn open_slot(
        &mut self,
        key: SlotKey,
        step_name: String,
        agent: String,
        model: Option<String>,
        io: Option<ContainerSlotIo>,
        dormant: bool,
    ) -> Vec<InstanceEvent> {
        let slot_id = match &key {
            SlotKey::Sequential(launch_id) => SlotId(*launch_id),
            SlotKey::Step(_) => SlotId(uuid::Uuid::new_v4()),
        };
        let mut events = Vec::new();
        let terminal = match (io, &self.hub) {
            (Some(io), Some(hub)) => Some(hub.bridge_agent_channels(
                TerminalRegistration {
                    owner: TerminalOwnerInfo {
                        owner: TerminalOwner::Tab(self.tab_id.clone()),
                        feature: FeatureName::Engine,
                        source: None,
                        attempt: 1,
                        allow_remote_input: false,
                    },
                    agent: agent.clone(),
                    model: model.clone(),
                    cols: self.pty_size.0,
                    rows: self.pty_size.1,
                },
                io.stdout_rx,
                io.stdin_tx,
                io.resize_tx,
            )),
            _ => None,
        };
        let terminal_id = terminal.as_ref().map(|t| t.handle.id().clone());
        let snapshot = SlotSnapshot {
            slot_id,
            step_name,
            agent_display_name: agent,
            model,
            container_name: String::new(),
            terminal_id: terminal_id.clone(),
            acp: None,
            started_at: Utc::now(),
            stats: None,
            program_status: None,
            stuck: false,
            countdown_kind: CountdownKind::default(),
            yolo: None,
            sandboxed: self.sandboxed,
            phase_step_title: None,
            dormant,
        };
        events.push(self.delta(InstanceDelta::SlotLaunched {
            tab_id: self.tab_id.clone(),
            slot: snapshot.clone(),
        }));
        if let Some(terminal_id) = terminal_id {
            events.push(InstanceEvent::TerminalOpened {
                tab_id: self.tab_id.clone(),
                terminal_id,
            });
        }
        self.focused = Some(slot_id);
        self.slots.push(RelaySlot {
            key,
            snapshot,
            terminal,
            yolo_cancel: None,
        });
        events
    }

    fn slot_mut(&mut self, key: &SlotKey) -> Option<&mut RelaySlot> {
        self.slots.iter_mut().find(|slot| &slot.key == key)
    }

    fn close_slot(&mut self, key: &SlotKey, exit_code: Option<i32>) -> Vec<InstanceEvent> {
        let Some(idx) = self.slots.iter().position(|slot| &slot.key == key) else {
            return Vec::new();
        };
        let mut slot = self.slots.remove(idx);
        let mut events = Vec::new();
        let final_screen = if exit_code.is_some() {
            slot.terminal.as_ref().and_then(|terminal| {
                terminal.flush_pending_output();
                self.hub
                    .as_ref()?
                    .snapshot(terminal.handle.id())
                    .ok()
                    .filter(|snapshot| snapshot.has_output)
                    .map(|snapshot| snapshot.screen)
            })
        } else {
            None
        };
        if let Some(terminal_id) = slot.finish(exit_code.unwrap_or(0)) {
            events.push(InstanceEvent::TerminalClosed {
                tab_id: self.tab_id.clone(),
                terminal_id,
                exit_code,
            });
        }
        events.push(self.delta(InstanceDelta::SlotExited {
            tab_id: self.tab_id.clone(),
            slot_id: slot.snapshot.slot_id,
            exit_code,
            final_screen,
        }));
        if self.focused == Some(slot.snapshot.slot_id) {
            self.focused = self.slots.last().map(|s| s.snapshot.slot_id);
        }
        events
    }

    fn end_slots(&mut self) -> Vec<InstanceEvent> {
        let keys: Vec<SlotKey> = self.slots.iter().map(|slot| slot.key.clone()).collect();
        keys.iter()
            .flat_map(|key| self.close_slot(key, None))
            .collect()
    }

    /// Drain every channel once and diff the shared feeds. `session` is the
    /// tab's session when no command holds its write lock.
    pub fn pump(&mut self, session: Option<&Session>) -> Vec<InstanceEvent> {
        let mut events = Vec::new();
        for event in self.feeds.command_bytes.drain() {
            events.push(match event {
                super::output::OutputEvent::Bytes(stream, bytes) => InstanceEvent::CommandBytes {
                    tab_id: self.tab_id.clone(),
                    stream,
                    bytes,
                },
                super::output::OutputEvent::Message(level, text) => InstanceEvent::CommandOutput {
                    tab_id: self.tab_id.clone(),
                    stream: OutputStream::Stderr,
                    level,
                    text,
                },
            });
        }
        self.pump_slot_events(&mut events);
        self.pump_stuck(&mut events);
        self.pump_feeds(&mut events);
        self.pump_channels(&mut events);
        if let Some(session) = session {
            self.pump_session(session, &mut events);
        }
        // Completion is a barrier: clients must receive the last status and
        // pipe bytes before they can exit or clear their command feeds.
        events.sort_by_key(|e| matches!(e, InstanceEvent::CommandFinished { .. }));
        events
    }

    fn pump_channels(&mut self, events: &mut Vec<InstanceEvent>) {
        let Some(command) = &self.command else {
            return;
        };
        while let Ok(request) = command.dialog_request_rx.try_recv() {
            events.push(InstanceEvent::DialogRaised {
                tab_id: self.tab_id.clone(),
                request,
            });
        }
        if self.feeds.signals.take_report_dialog_dismiss() {
            events.push(InstanceEvent::DismissReportDialog {
                tab_id: self.tab_id.clone(),
            });
        }
        if let Ok(result) = command.command_result_rx.try_recv() {
            events.push(InstanceEvent::CommandFinished {
                tab_id: self.tab_id.clone(),
                result: command_result_wire(&result),
            });
        }
    }

    fn pump_slot_events(&mut self, events: &mut Vec<InstanceEvent>) {
        let queued: Vec<ContainerSlotEvent> = match self.feeds.container.events.lock() {
            Ok(mut queue) => queue.drain(..).collect(),
            Err(_) => Vec::new(),
        };
        let tab_id = self.tab_id.clone();
        for event in queued {
            match event {
                ContainerSlotEvent::SequentialLaunched {
                    launch_id,
                    title,
                    io,
                } => {
                    // A new sequential launch replaces the backbone slot. It
                    // keeps the agent's name, as the builtin presenter does;
                    // the title names the step it runs.
                    let previous = self
                        .slots
                        .iter()
                        .find(|slot| matches!(slot.key, SlotKey::Sequential(_)));
                    let agent = previous
                        .map(|slot| slot.snapshot.agent_display_name.clone())
                        .filter(|name| !name.is_empty())
                        .or_else(|| title.clone())
                        .unwrap_or_default();
                    let in_group = previous.is_some_and(|slot| slot.snapshot.dormant);
                    let backbone: Vec<SlotKey> = self
                        .slots
                        .iter()
                        .filter(|slot| matches!(slot.key, SlotKey::Sequential(_)))
                        .map(|slot| slot.key.clone())
                        .collect();
                    for key in backbone {
                        events.extend(self.close_slot(&key, None));
                    }
                    if !in_group {
                        // The presenter clears the tab's stuck flag here.
                        self.projection.stuck = false;
                    }
                    events.extend(self.open_slot(
                        SlotKey::Sequential(launch_id),
                        title.unwrap_or_default(),
                        agent,
                        None,
                        Some(io),
                        in_group,
                    ));
                }
                ContainerSlotEvent::SequentialRunning {
                    launch_id,
                    container_name,
                } => {
                    if let Some(slot) = self.slot_mut(&SlotKey::Sequential(launch_id)) {
                        slot.snapshot.container_name = container_name.clone();
                        let slot_id = slot.snapshot.slot_id;
                        events.push(self.delta(InstanceDelta::SlotContainerName {
                            tab_id: tab_id.clone(),
                            slot_id,
                            container_name,
                        }));
                    }
                }
                ContainerSlotEvent::SequentialExited {
                    launch_id,
                    exit_code,
                } => {
                    events
                        .extend(self.close_slot(&SlotKey::Sequential(launch_id), Some(exit_code)));
                }
                ContainerSlotEvent::EngineAttached {
                    launch_id,
                    requests,
                    stuck,
                } => {
                    // The workflow request sender arrives before the step's
                    // stuck receiver. A later stuck-only handover must not
                    // erase the sender used for Ctrl-W in daemon mode.
                    if let Some(requests) = requests {
                        self.engine_requests = Some(requests);
                    }
                    if let Some(stuck) = stuck {
                        self.stuck_launch = Some(launch_id);
                        self.stuck = Some(stuck);
                    }
                }
                ContainerSlotEvent::AttachedExited { step_name, .. } => {
                    events.extend(self.close_slot(&SlotKey::Step(step_name), None));
                }
                ContainerSlotEvent::GroupStarted => {
                    for slot in &mut self.slots {
                        if matches!(slot.key, SlotKey::Sequential(_)) {
                            slot.snapshot.dormant = true;
                        }
                    }
                    events.push(self.delta(InstanceDelta::GroupStarted {
                        tab_id: tab_id.clone(),
                    }));
                }
                ContainerSlotEvent::Launched {
                    step_name,
                    agent,
                    model,
                    io,
                } => {
                    let key = SlotKey::Step(step_name.clone());
                    events.extend(self.close_slot(&key, None));
                    events.extend(self.open_slot(key, step_name, agent, model, io, false));
                }
                ContainerSlotEvent::ContainerName {
                    step_name,
                    container_name,
                } => {
                    if let Some(slot) = self.slot_mut(&SlotKey::Step(step_name)) {
                        slot.snapshot.container_name = container_name.clone();
                        let slot_id = slot.snapshot.slot_id;
                        events.push(self.delta(InstanceDelta::SlotContainerName {
                            tab_id: tab_id.clone(),
                            slot_id,
                            container_name,
                        }));
                    }
                }
                ContainerSlotEvent::Exited { step_name } => {
                    events.extend(self.close_slot(&SlotKey::Step(step_name), None));
                }
                ContainerSlotEvent::Stuck { step_name } => {
                    events.extend(self.set_slot_stuck(&step_name, true));
                }
                ContainerSlotEvent::Unstuck { step_name } => {
                    events.extend(self.set_slot_stuck(&step_name, false));
                }
                ContainerSlotEvent::Reported { step_name, status } => {
                    if let Some(slot) = self.slot_mut(&SlotKey::Step(step_name)) {
                        slot.snapshot.program_status = status.clone();
                        if status.is_some() {
                            slot.snapshot.stuck = false;
                        }
                        events.push(InstanceEvent::ProgramStatus {
                            tab_id: tab_id.clone(),
                            slot_id: slot.snapshot.slot_id,
                            status,
                        });
                    }
                }
                ContainerSlotEvent::CountdownKind { step_name, kind } => {
                    if let Some(slot) = self.slot_mut(&SlotKey::Step(step_name)) {
                        slot.snapshot.countdown_kind = kind;
                        let slot_id = slot.snapshot.slot_id;
                        events.push(self.delta(InstanceDelta::SlotCountdownKind {
                            tab_id: tab_id.clone(),
                            slot_id,
                            kind,
                        }));
                    }
                }
                ContainerSlotEvent::YoloStarted {
                    step_name,
                    cancel_flag,
                } => {
                    if let Some(slot) = self.slot_mut(&SlotKey::Step(step_name)) {
                        slot.yolo_cancel = Some(cancel_flag);
                    }
                }
                ContainerSlotEvent::YoloTick {
                    step_name,
                    remaining_secs,
                } => {
                    if let Some(slot) = self.slot_mut(&SlotKey::Step(step_name.clone())) {
                        let state = YoloState {
                            step_name,
                            remaining_secs,
                            kind: slot.snapshot.countdown_kind,
                        };
                        slot.snapshot.yolo = Some(state.clone());
                        let slot_id = slot.snapshot.slot_id;
                        events.push(self.delta(InstanceDelta::SlotYolo {
                            tab_id: tab_id.clone(),
                            slot_id,
                            state: Some(state),
                        }));
                    }
                }
                ContainerSlotEvent::YoloFinished { step_name } => {
                    if let Some(slot) = self.slot_mut(&SlotKey::Step(step_name)) {
                        slot.snapshot.yolo = None;
                        slot.yolo_cancel = None;
                        let slot_id = slot.snapshot.slot_id;
                        events.push(self.delta(InstanceDelta::SlotYolo {
                            tab_id: tab_id.clone(),
                            slot_id,
                            state: None,
                        }));
                    }
                }
                ContainerSlotEvent::GroupFinished => {
                    let steps: Vec<SlotKey> = self
                        .slots
                        .iter()
                        .filter(|slot| matches!(slot.key, SlotKey::Step(_)))
                        .map(|slot| slot.key.clone())
                        .collect();
                    for key in steps {
                        events.extend(self.close_slot(&key, None));
                    }
                    for slot in &mut self.slots {
                        slot.snapshot.dormant = false;
                    }
                    events.push(self.delta(InstanceDelta::GroupFinished {
                        tab_id: tab_id.clone(),
                    }));
                }
            }
        }
        self.feeds.signals.set_status(
            EffectiveStatus::most_urgent(
                self.slots
                    .iter()
                    .filter(|slot| !slot.snapshot.dormant)
                    .filter_map(|slot| slot.snapshot.program_status.as_ref()),
            )
            .cloned(),
        );
    }

    fn set_slot_stuck(&mut self, step_name: &str, stuck: bool) -> Option<InstanceEvent> {
        let tab_id = self.tab_id.clone();
        let slot = self.slot_mut(&SlotKey::Step(step_name.to_string()))?;
        slot.snapshot.stuck = stuck;
        let slot_id = slot.snapshot.slot_id;
        Some(InstanceEvent::Delta(InstanceDelta::SlotStuck {
            tab_id,
            slot_id,
            stuck,
        }))
    }

    fn pump_stuck(&mut self, events: &mut Vec<InstanceEvent>) {
        let Some(stuck) = self.stuck.as_mut() else {
            return;
        };
        let mut latest = None;
        let mut reported = None;
        loop {
            match stuck.try_recv() {
                Ok(StuckEvent::Stuck) => latest = Some(true),
                Ok(StuckEvent::Unstuck | StuckEvent::StartupGraceExpired) => latest = Some(false),
                Ok(StuckEvent::Reported(status)) => reported = Some(status),
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
        if let Some(stuck) = latest {
            self.projection.stuck = stuck;
            events.push(self.delta(InstanceDelta::Stuck {
                tab_id: self.tab_id.clone(),
                stuck,
            }));
        }
        if let Some(status) = reported {
            events.extend(self.report_program_status(status));
        }
    }

    /// The slots to sample this round, as `(slot id, container name)`: every
    /// live slot whose container name is known. Unlike the local TUI there
    /// is no first-running-container fallback — a daemon hosts many tabs'
    /// containers, so guessing would show another tab's numbers.
    pub fn stats_targets(&self) -> Vec<(String, String)> {
        self.slots
            .iter()
            .filter(|slot| !slot.snapshot.dormant && !slot.snapshot.container_name.is_empty())
            .map(|slot| {
                (
                    slot.snapshot.slot_id.0.to_string(),
                    slot.snapshot.container_name.clone(),
                )
            })
            .collect()
    }

    /// Record a sample for the slot it was taken for, if that slot still
    /// runs the sampled container.
    pub fn apply_stats(&mut self, slot_id: &str, stats: AgentStats) -> Option<InstanceEvent> {
        let tab_id = self.tab_id.clone();
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.snapshot.slot_id.0.to_string() == slot_id)?;
        if slot.snapshot.container_name != stats.name {
            return None;
        }
        let wire = AgentStatsWire {
            name: stats.name,
            cpu_percent: stats.cpu_percent,
            memory_mb: stats.memory_mb,
        };
        slot.snapshot.stats = Some(wire.clone());
        Some(InstanceEvent::Delta(InstanceDelta::SlotStats {
            tab_id,
            slot_id: slot.snapshot.slot_id,
            stats: wire,
        }))
    }

    /// Forward the agent's latest self-reported status (OSC 7501) for the
    /// sequential slot that owns the report channel. The engine-side signals
    /// get it too: dialog deferral reads them, as the local TUI's do.
    fn report_program_status(&mut self, status: Option<EffectiveStatus>) -> Option<InstanceEvent> {
        self.feeds.signals.set_status(status.clone());
        let launch_id = self.stuck_launch?;
        // A reporting agent is never stuck; the presenter clears its flag on
        // the report, so the relayed state does too.
        if status.is_some() {
            self.projection.stuck = false;
        }
        let slot = self.slot_mut(&SlotKey::Sequential(launch_id))?;
        slot.snapshot.program_status = status.clone();
        if status.is_some() {
            slot.snapshot.stuck = false;
        }
        Some(InstanceEvent::ProgramStatus {
            tab_id: self.tab_id.clone(),
            slot_id: SlotId(launch_id),
            status,
        })
    }

    /// This tab's row of the `agents` TUI context (tab number, container,
    /// stuck, status, command), from the engine's own view of the tab — the
    /// same values a builtin TUI derives from its tab state.
    pub fn agents_tab_snapshot(&self, tab_number: u32) -> TuiTabSnapshot {
        let live = || self.slots.iter().filter(|slot| !slot.snapshot.dormant);
        let container_name = self
            .focused
            .and_then(|id| self.slots.iter().find(|slot| slot.snapshot.slot_id == id))
            .map(|slot| slot.snapshot.container_name.clone())
            .filter(|name| !name.is_empty());
        TuiTabSnapshot {
            tab_number,
            container_name,
            is_stuck: self.projection.stuck || live().any(|slot| slot.snapshot.stuck),
            program_status: EffectiveStatus::most_urgent(
                live().filter_map(|slot| slot.snapshot.program_status.as_ref()),
            )
            .cloned(),
            command_label: self.phase.command_name().to_string(),
        }
    }

    /// Hand the instance's `agents` context to this tab's commands.
    pub fn set_agents_context(&self, context: &AgentsTuiContext) {
        if let Ok(mut shared) = self.feeds.tui_context_shared.lock() {
            *shared = context.clone();
        }
    }

    fn pump_feeds(&mut self, events: &mut Vec<InstanceEvent>) {
        let lines: Vec<StatusLine> = self
            .feeds
            .status_log
            .lock()
            .map(|log| log.clone())
            .unwrap_or_default();
        if lines.len() < self.projection.status_lines {
            self.projection.status_lines = 0;
            self.status_log.clear();
            events.push(self.delta(InstanceDelta::StatusLogReset {
                tab_id: self.tab_id.clone(),
            }));
        }
        for line in &lines[self.projection.status_lines..] {
            if !self.feeds.command_bytes.enabled() {
                events.push(InstanceEvent::CommandOutput {
                    tab_id: self.tab_id.clone(),
                    stream: OutputStream::Stdout,
                    level: line.level,
                    text: line.text.clone(),
                });
            }
            events.push(self.delta(InstanceDelta::StatusLine {
                tab_id: self.tab_id.clone(),
                line: line.clone(),
            }));
            self.status_log.push(line.clone());
        }
        self.projection.status_lines = lines.len();

        let workflow = self.feeds.workflow.snapshot();
        let projected = (
            workflow.view,
            workflow.invocation_id,
            workflow.context_path,
            workflow.worktree_path,
        );
        if self.projection.workflow.as_ref() != Some(&projected) {
            log_workflow_progress(&self.tab_id, self.projection.workflow.as_ref(), &projected);
            events.push(self.delta(InstanceDelta::Workflow {
                tab_id: self.tab_id.clone(),
                view: projected.0.clone(),
                invocation_id: projected.1,
                context_path: projected.2.clone(),
                worktree_path: projected.3.clone(),
            }));
            self.projection.workflow = Some(projected);
        }

        let yolo = self.feeds.yolo_state.lock().ok().and_then(|y| y.clone());
        if yolo != self.projection.yolo {
            events.push(self.delta(InstanceDelta::Yolo {
                tab_id: self.tab_id.clone(),
                state: yolo.clone(),
            }));
            self.projection.yolo = yolo;
        }

        let dashboard = self
            .feeds
            .agents_dashboard
            .lock()
            .ok()
            .and_then(|d| d.clone());
        if dashboard != self.projection.agents_dashboard {
            events.push(self.delta(InstanceDelta::AgentsDashboard {
                tab_id: self.tab_id.clone(),
                data: dashboard.as_ref().and_then(dashboard_wire),
            }));
            self.projection.agents_dashboard = dashboard;
        }
    }

    fn pump_session(&mut self, session: &Session, events: &mut Vec<InstanceEvent>) {
        let phase = ExecutionPhase::of_session_state(session.state());
        if self.projection.phase.as_ref() != Some(&phase)
            || self.current_workflow != session.state().current_workflow
            || self.current_container != session.state().current_container
        {
            self.phase = phase.clone();
            self.current_workflow = session.state().current_workflow.clone();
            self.current_container = session.state().current_container.clone();
            events.push(self.delta(InstanceDelta::Phase {
                tab_id: self.tab_id.clone(),
                phase: phase.clone(),
                current_workflow: self.current_workflow.clone(),
                current_container: self.current_container.clone(),
            }));
            events.push(self.delta(InstanceDelta::TabLabel {
                tab_id: self.tab_id.clone(),
                label: self.label(),
            }));
            self.projection.phase = Some(phase);
        }
    }

    fn label(&self) -> TabLabelWire {
        let text = self
            .session
            .cwd
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.session.cwd.display().to_string());
        let color = match &self.phase {
            ExecutionPhase::Idle => LabelColorWire::Default,
            ExecutionPhase::Running { .. } if self.yolo_mode => LabelColorWire::Yolo,
            ExecutionPhase::Running { .. } => LabelColorWire::Running,
            ExecutionPhase::Done { .. } => LabelColorWire::Done,
            ExecutionPhase::Error { .. } => LabelColorWire::Error,
        };
        TabLabelWire { text, color }
    }

    // ── Inputs ───────────────────────────────────────────────────────────

    /// Forward the one answer arbitration let through to the command thread.
    pub fn answer(&self, response: DialogResponse) -> Result<(), CommandError> {
        // A user answer wins over a queued auto-dismissal. It must not
        // dismiss the next board (for example, Ctrl-W's expanded board).
        self.feeds.signals.take_report_dialog_dismiss();
        let command = self
            .command
            .as_ref()
            .ok_or_else(|| CommandError::Other("no command is waiting for an answer".into()))?;
        command
            .dialog_response_tx
            .send(response)
            .map_err(|_| CommandError::Other("the command is no longer waiting".into()))
    }

    /// Ctrl-C: cancel the running command (the second Ctrl-C only repeats it).
    pub fn cancel(&self, _repeat: bool) {
        self.feeds.command_cancel.cancel();
    }

    /// Esc on a yolo countdown (a step's own, or the sequential one).
    pub fn cancel_countdown(&self, step_name: Option<&str>) {
        match step_name.and_then(|step| {
            self.slots
                .iter()
                .find(|slot| slot.key == SlotKey::Step(step.to_string()))
        }) {
            Some(slot) => {
                if let Some(flag) = &slot.yolo_cancel {
                    flag.store(true, Ordering::SeqCst);
                }
            }
            None => self.feeds.yolo_cancel_flag.store(true, Ordering::SeqCst),
        }
    }

    /// Ctrl-W: open the workflow control board.
    pub fn request_workflow(&self, request: WorkflowRequestWire) {
        if let Some(requests) = &self.engine_requests {
            let WorkflowRequestWire::OpenControlBoard { step_name } = request;
            let _ = requests.send(EngineRequest::OpenControlBoard { step_name });
        }
    }

    /// A person typed or scrolled in this tab.
    pub fn note_activity(&self, step_name: Option<&str>) {
        self.feeds.signals.touch_user_for_step(step_name);
    }

    /// The agent-window size later slots open at.
    pub fn set_pty_size(&mut self, size: (u16, u16)) {
        self.pty_size = size;
    }

    /// End the tab: cancel its command and end its terminals.
    pub fn close(&mut self) -> Vec<InstanceEvent> {
        self.feeds.command_cancel.cancel();
        self.command = None;
        self.end_slots()
    }

    /// The tab as a snapshot.
    pub fn snapshot(&self) -> TabSnapshot {
        let workflow = self.feeds.workflow.snapshot();
        TabSnapshot {
            tab_id: self.tab_id.clone(),
            session: self.session.clone(),
            kind: self.kind.clone(),
            label: self.label(),
            execution_phase: self.phase.clone(),
            current_workflow: self.current_workflow.clone(),
            current_container: self.current_container.clone(),
            workflow: workflow.view,
            workflow_invocation_id: workflow.invocation_id,
            context_path: workflow.context_path,
            worktree_path: workflow.worktree_path,
            slots: self
                .slots
                .iter()
                .map(|slot| slot.snapshot.clone())
                .collect(),
            focused_slot: self.focused,
            yolo: self.projection.yolo.clone(),
            yolo_mode: self.yolo_mode,
            stuck: self.projection.stuck,
            git_summary: None,
            status_log: self.status_log.clone(),
            agents_dashboard: self
                .projection
                .agents_dashboard
                .as_ref()
                .and_then(dashboard_wire),
            last_container_summary: None,
            annotations: self.annotations.clone(),
        }
    }
}

impl Drop for TabRelay {
    fn drop(&mut self) {
        self.feeds.command_cancel.cancel();
        for slot in &mut self.slots {
            slot.finish(0);
        }
    }
}
