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
    ContainerWindowState, ExecutionPhase, InstanceDelta, LabelColorWire, SessionView, SlotId,
    SlotSnapshot, StatusLine, TabLabelWire, TabSnapshot, YoloState,
};
use crate::data::message::MessageLevel;
use crate::data::program_status::EffectiveStatus;
use crate::data::session::{Session, SessionId};
use crate::data::terminal::{DetachReason, TerminalOwner, TerminalOwnerInfo};
use crate::data::workflow_state::CountdownKind;
use crate::engine::agent_runtime::execution::StuckEvent;
use crate::engine::terminal_hub::bridge::BridgedTerminal;
use crate::engine::terminal_hub::{TerminalHub, TerminalRegistration};
use crate::engine::workflow::EngineRequest;

use super::feeds::{
    ContainerSlotEvent, ContainerSlotIo, SharedYoloCancelFlag, TabCommandFeeds, TabFeeds,
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

/// The last projected state, for diffing `TabCommandFeeds` into deltas.
#[derive(Default)]
struct Projection {
    status_lines: usize,
    workflow: Option<WorkflowProjection>,
    yolo: Option<YoloState>,
    phase: Option<ExecutionPhase>,
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
            SlotId(launch_id),
            String::new(),
            submitted.display.agent_display_name.clone(),
            None,
            Some(submitted.slot_io),
        ));
        events
    }

    fn delta(&self, delta: InstanceDelta) -> InstanceEvent {
        InstanceEvent::Delta(delta)
    }

    fn open_slot(
        &mut self,
        key: SlotKey,
        slot_id: SlotId,
        step_name: String,
        agent: String,
        model: Option<String>,
        io: Option<ContainerSlotIo>,
    ) -> Vec<InstanceEvent> {
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
            dormant: false,
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
                    // A new sequential launch replaces the backbone slot.
                    let backbone: Vec<SlotKey> = self
                        .slots
                        .iter()
                        .filter(|slot| matches!(slot.key, SlotKey::Sequential(_)))
                        .map(|slot| slot.key.clone())
                        .collect();
                    for key in backbone {
                        events.extend(self.close_slot(&key, None));
                    }
                    let agent = title.clone().unwrap_or_default();
                    events.extend(self.open_slot(
                        SlotKey::Sequential(launch_id),
                        SlotId(launch_id),
                        title.unwrap_or_default(),
                        agent,
                        None,
                        Some(io),
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
                    launch_id: _,
                    requests,
                    stuck,
                } => {
                    self.engine_requests = requests;
                    self.stuck = stuck;
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
                    events.extend(self.open_slot(
                        key,
                        SlotId(uuid::Uuid::new_v4()),
                        step_name,
                        agent,
                        model,
                        io,
                    ));
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
        loop {
            match stuck.try_recv() {
                Ok(StuckEvent::Stuck) => latest = Some(true),
                Ok(StuckEvent::Unstuck) => latest = Some(false),
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
        if let Some(stuck) = latest {
            events.push(self.delta(InstanceDelta::Stuck {
                tab_id: self.tab_id.clone(),
                stuck,
            }));
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
    }

    fn pump_session(&mut self, session: &Session, events: &mut Vec<InstanceEvent>) {
        let phase = ExecutionPhase::of_session_state(session.state());
        if self.projection.phase.as_ref() != Some(&phase) {
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
    pub fn note_activity(&self) {
        self.feeds.signals.touch_user();
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
            stuck: false,
            git_summary: None,
            status_log: self.status_log.clone(),
            agents_dashboard: None,
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
