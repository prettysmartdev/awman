//! `ApiDispatchFrontend` — the single Layer 3 struct that implements
//! every per-command frontend trait for API HTTP command dispatch.
//!
//! When a `POST /v1/worker/commands` request arrives, the route handler constructs
//! a `ApiDispatchFrontend` pre-loaded with the parsed args/flags from
//! the HTTP request body, then hands it to `Dispatch::run_command`. All
//! output (UserMessages, container stdout/stderr) is written to the
//! command's `output.log` file on disk. SSE clients tailing the log see
//! new lines in real time.
//!
//! Every interactive Q&A method is a one-line delegation to
//! [`HeadlessDefaults::api`](crate::command::headless::HeadlessDefaults::api),
//! the Layer 2 profile that owns the API's headless answers. They are *not*
//! the same as the CLI's or the squad daemon's — the differences are
//! deliberate per-host policy, tabulated and explained in
//! `src/command/headless.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::command::commands::api_server::event_bus::EventBusSender;
use crate::data::agent_output_format::AgentOutputFormat;
use crate::data::execution_event::{
    CommandStatusKind, EventPayload, EventSource, EventSourcePhase, PhaseStatusKind, StepStatusKind,
};
use crate::engine::agent_output::{FinalMessageSink, OutputStream, Translated};

use async_trait::async_trait;

use crate::command::commands::agent_auth::{AgentAuthDecision, AgentAuthFrontend};
use crate::command::commands::agent_setup::{
    AgentSetupDecision, AgentSetupFrontend, HasAgentFrontend,
};
use crate::command::commands::agents::AgentsCommandFrontend;
use crate::command::commands::chat::ChatCommandFrontend;
use crate::command::commands::config::{ConfigCommandFrontend, ConfigEditRequest, ConfigFieldRow};
use crate::command::commands::exec_prompt::ExecPromptCommandFrontend;
use crate::command::commands::exec_workflow::{
    ExecWorkflowCommandFrontend, WorkflowResumeDecision, WorkflowResumePrompt, WorkflowSummary,
};
use crate::command::commands::mount_scope::{MountScopeDecision, MountScopeFrontend};
use crate::command::commands::new::NewCommandFrontend;
use crate::command::commands::remote::RemoteCommandFrontend;
use crate::command::commands::specs::SpecsCommandFrontend;
use crate::command::commands::worktree_lifecycle::{
    DirtyMergeTargetDecision, ExistingWorktreeDecision, MergeFailure, PostWorkflowWorktreeAction,
    PreWorktreeDecision, WorktreeLifecycleFrontend, WorktreeMergeMode,
};
use crate::command::dispatch::catalogue::{CommandCatalogue, FrontendKind};
use crate::command::dispatch::projections::raw_args::ParsedArgs;
use crate::command::dispatch::CommandFrontend;
use crate::command::error::CommandError;
use crate::command::headless::HeadlessDefaults;
use crate::data::config::work_items::WorkItemsConfig;
use crate::data::fs::api_command_log::CommandStateWriter;
use crate::data::fs::api_db::{FailedStep, FailedStepPhase};
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::ready_phase::ReadyPhase;
use crate::data::ready_summary::ReadySummary;
use crate::data::session::AgentName;
use crate::data::step_status::StepStatus;
use crate::data::workflow_definition::WorkflowStep;
use crate::data::workflow_state::{PhaseKind, WorkflowState};
use crate::engine::acp::{AcpFrontend, PermissionDecision, PermissionRequest, SessionUpdate};
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentProgress, AgentStatus};
use crate::engine::error::EngineError;
use crate::engine::init::frontend::InitFrontend;
use crate::engine::init::phase::InitPhase;
use crate::engine::init::summary::InitSummary;
use crate::engine::ready::frontend::ReadyFrontend;
use crate::engine::workflow::actions::{
    AvailableActions, CountdownKind, NextAction, ResumeMismatch, StepOutput, WorkflowOutcome,
    WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::frontend::{ContainerSource, WorkflowFrontend};

/// The API dispatch frontend. Emits typed events to an `EventBusSender`
/// for distribution to logfile writers and SSE clients.
pub struct ApiDispatchFrontend {
    /// Initial prerequisite progress follows its structured report on the wire.
    prerequisite_messages: Option<Vec<UserMessage>>,

    /// The run's resolved interaction (WI 0141), from Layer 2. `None` for a
    /// frontend built without one (tests, admission probes): piped.
    interaction: Option<crate::command::interaction::InteractionDecision>,
    /// The sender of the last single-agent PTY sink handed out
    /// (`container_frontend_for_pty`): `program_status` events of that agent
    /// carry its container attribution.
    pty_sink_sender: Option<EventBusSender>,
    /// Forwards a single-agent launch.s `StuckEvent::Reported` to the bus
    /// (`set_stuck_sender`); aborted when replaced or dropped.
    status_forwarder: Option<AbortOnDrop>,
    frontend_kind: FrontendKind,
    /// The outcome of Layer 2 parsing (`CommandCatalogue::parse_raw_args`),
    /// captured at construction. Every `CommandFrontend` accessor reports a
    /// failure so Dispatch aborts the command rather than running with
    /// silently-dropped flags.
    parsed: Arc<Result<ParsedArgs, CommandError>>,
    event_bus: EventBusSender,
    state_writer: Option<CommandStateWriter>,
    line_buffer_stdout: String,
    line_buffer_stderr: String,
    /// Main step names in definition order (`WorkflowState.steps`), from the
    /// engine's progress reports and state snapshots. A step's position here
    /// is its `step_index` in every event (WI 0137 §5).
    step_order: Vec<String>,
    /// The attribution of the container whose call is being forwarded right
    /// now, lent by its per-container proxy (`set_container_source`).
    container_source: Option<ContainerSource>,
    /// The last container name each main step ran in, for the attribution
    /// of the step's own workflow events.
    step_containers: HashMap<String, String>,
    /// Set after the first main-phase `WorkflowPhaseTransition` event.
    /// Prevents duplicate main-phase start events for the same workflow run.
    phase_emitted: std::sync::Mutex<bool>,
    /// Latched once `Done` has been emitted, so both `emit_done` and `Drop`
    /// stay idempotent.
    done_emitted: std::sync::atomic::AtomicBool,
    /// The queue worker owns completion when post-command stages are enabled.
    emit_done_on_drop: bool,
    step_attempts: HashMap<String, u32>,
    phase_attempts: HashMap<(String, usize), u32>,
    workflow_state: Option<WorkflowState>,
    /// The structured output format of the agent about to run, announced by
    /// the engine through `set_agent_output_format` and consumed by the next
    /// `take_io` (WI 0137 §4). `Text` when nothing was announced.
    output_format: AgentOutputFormat,
    /// Throttle: last time a yolo countdown status message was emitted.
    last_sink_message_time: Option<std::time::Instant>,
    /// What the countdown currently being ticked will do when it expires
    /// (WI-0115 §3). An API consumer reading "auto-advancing" through a
    /// failure retry would draw the wrong conclusion about its run.
    /// The command path this frontend was built for, e.g. "init", "ready",
    /// "exec prompt". Used as the `phase` tag on agent-image setup events so
    /// the one `AgentImageFrontend` impl keeps the per-command phase string
    /// the separate `InitFrontend`/`ReadyFrontend` impls emitted (F-33).
    command_path: String,
    countdown_kind: CountdownKind,
    /// Every answer this frontend gives to a question it cannot ask a human.
    /// See `src/command/headless.rs` for the table and why the API's row
    /// differs from squad's and the CLI's.
    headless: HeadlessDefaults,
}

#[async_trait::async_trait]
impl crate::command::commands::squad::commands::SquadCommandFrontend for ApiDispatchFrontend {}

impl crate::command::commands::squad::attach::SquadAttachFrontend for ApiDispatchFrontend {
    fn ask_pick_candidate(
        &mut self,
        _candidates: &[crate::command::commands::squad::attach::SquadContainer],
    ) -> Result<Option<usize>, CommandError> {
        Err(CommandError::NotAvailableForFrontend {
            command: "squad attach".into(),
            frontend: "api".into(),
        })
    }

    fn on_slot_attached(
        &mut self,
        _step: &str,
        _instance: Box<dyn crate::engine::agent_runtime::AgentInstance>,
    ) -> Result<(), CommandError> {
        Err(CommandError::NotAvailableForFrontend {
            command: "squad attach".into(),
            frontend: "api".into(),
        })
    }

    fn on_slot_exited(&mut self, _step: &str) {}
}

impl ApiDispatchFrontend {
    /// Construct a new frontend from the HTTP request's subcommand + args.
    ///
    /// `event_bus` is the sender handle for emitting execution events.
    /// `subcommand` is the command path (e.g. "exec prompt" → ["exec", "prompt"]).
    /// `args` is the raw args vector from the HTTP request body.
    ///
    /// Parsing is delegated wholesale to Layer 2
    /// ([`CommandCatalogue::parse_raw_args_with_profile`]): the API frontend
    /// hands the raw HTTP strings straight to the catalogue and keeps no
    /// parsing, type-coercion, or flag-default policy of its own (work item
    /// 0097, Findings A + D). The `Api` frontend profile is what forces
    /// `non-interactive=true` and defaults `yolo=true`.
    pub fn new(subcommand: &str, args: &[String], event_bus: EventBusSender) -> Self {
        Self::for_frontend(subcommand, args, event_bus, FrontendKind::WORKER_LOCAL)
    }
    /// Project an already catalogue-validated foreground invocation.
    pub fn from_input(
        input: crate::command::dispatch::ParsedCommandBoxInput,
        events: EventBusSender,
    ) -> Self {
        let mut frontend = Self::new(&input.path.join(" "), &[], events);
        frontend.parsed = Arc::new(Ok(input.args));
        frontend
    }

    pub fn for_frontend(
        subcommand: &str,
        args: &[String],
        event_bus: EventBusSender,
        frontend_kind: FrontendKind,
    ) -> Self {
        let path: Vec<&str> = subcommand.split_whitespace().collect();
        let parsed =
            CommandCatalogue::get().parse_raw_args_with_profile(&path, args, frontend_kind);

        Self {
            prerequisite_messages: None,
            interaction: None,
            pty_sink_sender: None,
            status_forwarder: None,
            frontend_kind,
            parsed: Arc::new(parsed),
            event_bus,
            state_writer: None,
            line_buffer_stdout: String::new(),
            line_buffer_stderr: String::new(),
            step_order: Vec::new(),
            container_source: None,
            step_containers: HashMap::new(),
            phase_emitted: std::sync::Mutex::new(false),
            done_emitted: std::sync::atomic::AtomicBool::new(false),
            emit_done_on_drop: true,
            step_attempts: HashMap::new(),
            phase_attempts: HashMap::new(),
            workflow_state: None,
            output_format: AgentOutputFormat::Text,
            last_sink_message_time: None,
            countdown_kind: CountdownKind::StuckStep,
            // The *catalogue's* spelling of the path, not the request's. This
            // string is the `phase` label on every status event the merged
            // `AgentImageFrontend` impl emits (F-33), and `subcommand` is a
            // raw HTTP field: `{"subcommand": " ready"}` passes validation and
            // would emit `phase: " ready"`. `canonical_path` also resolves
            // aliases, so an aliased request labels its events the same as the
            // canonical one.
            command_path: CommandCatalogue::get().canonical_path(&path).join(" "),
            headless: HeadlessDefaults::api(),
        }
    }

    /// The interaction Layer 2 resolved for this run (WI 0141). It decides
    /// what `headless` reads as and whether `take_io` asks for a PTY
    /// (`AgentIo::initial_size = decision.pty_size`); the frontend decides
    /// nothing about it itself.
    pub fn with_interaction(
        mut self,
        decision: crate::command::interaction::InteractionDecision,
    ) -> Self {
        self.interaction = Some(decision);
        self
    }

    fn is_interactive(&self) -> bool {
        self.interaction.is_some_and(|d| !d.is_headless())
    }

    /// Mirror workflow snapshots into the command directory for API polling.
    pub fn with_state_writer(mut self, writer: CommandStateWriter) -> Self {
        self.state_writer = Some(writer);
        self
    }

    /// Let the queue worker emit `Done` after post-command stages finish.
    pub fn deferring_done(mut self) -> Self {
        self.emit_done_on_drop = false;
        self
    }

    /// The step's index in `WorkflowState.steps` (WI 0137 §5), stable across
    /// attempts. The engine reports the definition before any step launches,
    /// so an unknown step has no index until an authoritative snapshot arrives.
    fn step_index_for(&self, name: &str) -> Option<usize> {
        self.step_order.iter().position(|n| n == name)
    }

    /// The authoritative definition order from the engine's latest report.
    fn set_step_order(&mut self, names: impl Iterator<Item = String>) {
        self.step_order = names.collect();
    }

    /// A sender attributing a main step's workflow events to that step.
    fn step_sender(&mut self, name: &str) -> EventBusSender {
        let index = self.step_index_for(name);
        self.event_bus.with_source(EventSource {
            phase: EventSourcePhase::Main,
            step_name: Some(name.to_string()),
            step_index: index,
            container: self.step_containers.get(name).cloned(),
            step_attempt: self.step_attempts.get(name).copied(),
        })
    }

    /// A sender attributing events to a phase with no step name.
    fn phase_sender(&self, phase: EventSourcePhase) -> EventBusSender {
        self.event_bus.with_source(EventSource::phase(phase))
    }

    /// The sender for a container call: attributed to the container whose
    /// proxy lent its source, otherwise the unattributed root sender.
    fn container_sender(&self) -> EventBusSender {
        match &self.container_source {
            Some(cell) => self.event_bus.with_shared_source(cell.clone()),
            None => self.event_bus.clone(),
        }
    }

    /// Flush any remaining partial lines in the stdout/stderr buffers.
    pub fn flush_line_buffers(&mut self) {
        if !self.line_buffer_stdout.is_empty() {
            let line = std::mem::take(&mut self.line_buffer_stdout);
            self.event_bus.emit(EventPayload::StdoutLine(line));
        }
        if !self.line_buffer_stderr.is_empty() {
            let line = std::mem::take(&mut self.line_buffer_stderr);
            self.event_bus.emit(EventPayload::StderrLine(line));
        }
    }

    /// Flush partial line buffers and emit `Done`. Calling this multiple
    /// times — or in addition to `Drop` — is safe; the second emission is
    /// elided via `done_emitted`.
    pub fn emit_done(&mut self) {
        self.flush_line_buffers();
        if !self
            .done_emitted
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            self.event_bus.emit(EventPayload::Done);
        }
    }

    /// Get a clone of the event bus sender (for creating child sinks).
    pub fn event_bus_sender(&self) -> EventBusSender {
        self.event_bus.clone()
    }
}

impl ApiDispatchFrontend {
    fn flush_prerequisite_messages(&mut self) {
        for message in self.prerequisite_messages.take().unwrap_or_default() {
            emit_user_message(&self.container_sender(), message);
        }
    }
}

impl Drop for ApiDispatchFrontend {
    fn drop(&mut self) {
        self.flush_prerequisite_messages();
        // The engine writes container output in arbitrary byte chunks. Anything
        // not terminated by `\n` lives in the line buffers — flush it as a
        // final event so SSE clients and `events.log` see the trailing line.
        if !self.line_buffer_stdout.is_empty() {
            let line = std::mem::take(&mut self.line_buffer_stdout);
            self.event_bus.emit(EventPayload::StdoutLine(line));
        }
        if !self.line_buffer_stderr.is_empty() {
            let line = std::mem::take(&mut self.line_buffer_stderr);
            self.event_bus.emit(EventPayload::StderrLine(line));
        }
        if self.emit_done_on_drop
            && !self
                .done_emitted
                .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            self.event_bus.emit(EventPayload::Done);
        }
    }
}

// ─── UserMessageSink ────────────────────────────────────────────────────────

/// F-45: takes the default `command_started`, so the `$ git …` echo line
/// is byte-identical to the one `run_git_logged` composed before.
impl crate::engine::git::GitFrontend for ApiDispatchFrontend {}

impl UserMessageSink for ApiDispatchFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        if let Some(messages) = &mut self.prerequisite_messages {
            messages.push(msg);
        } else {
            emit_user_message(&self.container_sender(), msg);
        }
    }

    fn replay_queued(&mut self) {}
}

// ─── CommandFrontend (flag/argument access) ─────────────────────────────────

impl ApiDispatchFrontend {
    /// The parsed arguments, or the construction-time parse error with its
    /// structured classification. The first accessor Dispatch calls returns
    /// the error so a malformed request aborts the command.
    fn parsed(&self) -> Result<&ParsedArgs, CommandError> {
        self.parsed.as_ref().as_ref().map_err(|e| {
            e.clone_parse_stage()
                .unwrap_or_else(|| CommandError::Other(e.to_string()))
        })
    }
}

impl CommandFrontend for ApiDispatchFrontend {
    fn input_available(&self) -> bool {
        self.is_interactive()
    }
    fn kind(&self) -> crate::command::dispatch::catalogue::FrontendKind {
        self.frontend_kind
    }
    fn flag_bool(&self, _command_path: &[&str], flag: &str) -> Result<Option<bool>, CommandError> {
        self.parsed()?;
        // The resolved decision is authoritative for both spellings, so a
        // run never reads a mode admission did not choose.
        if let Some(decision) = self.interaction {
            match flag {
                "headless" => return Ok(Some(decision.is_headless())),
                "interactive" => return Ok(Some(!decision.is_headless())),
                _ => {}
            }
        }
        Ok(self.parsed()?.flag_bool(flag))
    }

    fn flag_string(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<String>, CommandError> {
        Ok(self.parsed()?.flag_string(flag))
    }

    fn flag_strings(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Vec<String>, CommandError> {
        Ok(self.parsed()?.flag_strings(flag))
    }

    fn flag_path(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<PathBuf>, CommandError> {
        Ok(self.parsed()?.flag_path(flag))
    }

    fn flag_enum(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<String>, CommandError> {
        Ok(self.parsed()?.flag_enum(flag))
    }

    fn flag_u16(&self, _command_path: &[&str], flag: &str) -> Result<Option<u16>, CommandError> {
        Ok(self.parsed()?.flag_u16(flag))
    }

    fn flag_usize(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<usize>, CommandError> {
        Ok(self.parsed()?.flag_usize(flag))
    }

    fn argument(&self, _command_path: &[&str], name: &str) -> Result<Option<String>, CommandError> {
        Ok(self.parsed()?.argument(name))
    }

    fn arguments(&self, _command_path: &[&str], name: &str) -> Result<Vec<String>, CommandError> {
        Ok(self.parsed()?.arguments(name))
    }
}

// ─── AgentFrontend ──────────────────────────────────────────────────────

#[async_trait]
impl AgentFrontend for ApiDispatchFrontend {
    fn report_status(&mut self, status: AgentStatus) {
        // The proxy has already written the container name into its source;
        // remember it so the step's own workflow events carry it too.
        if let (AgentStatus::Running { container_name }, Some(cell)) =
            (&status, &self.container_source)
        {
            let step_name = cell
                .lock()
                .expect("event source lock poisoned")
                .step_name
                .clone();
            if let Some(step_name) = step_name {
                self.step_containers
                    .insert(step_name, container_name.clone());
            }
        }
        emit_container_status(
            &self.container_sender(),
            self.state_writer.as_ref(),
            &status,
        );
    }

    fn report_progress(&mut self, progress: AgentProgress) {
        emit_progress(&self.container_sender(), progress);
    }

    fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
        // Headless: no interactive stdin. The engine owns the only sender and
        // drops it after seeding the prompt (see `spawn_piped_docker`) so the
        // container sees EOF promptly. The drains keep the lent source, so
        // every line of this container stays attributed to it.
        let io = spawn_output_drains(
            &self.container_sender(),
            std::mem::take(&mut self.output_format),
            self.final_message_sink(),
        );
        // Interactive (WI 0141): the size Layer 2 read from
        // `daemon.terminal.{cols,rows}` at spawn is what makes the runtime
        // take `spawn_pty_bridged`. Nobody here resizes the PTY; viewers do
        // that through the terminal hub. ACP launches never reach this
        // `take_io` (they bind their own piped transport), so they stay
        // piped whatever the decision says.
        with_pty_size(io, self.interaction.and_then(|d| d.pty_size))
    }

    fn set_agent_output_format(&mut self, format: AgentOutputFormat) {
        self.output_format = format;
    }

    /// API runs unattended workloads — an agent may need minutes to pull
    /// an image or warm up a model before producing output. A 15-minute
    /// startup grace gives the container reasonable runway before the
    /// detector kills it as failed-to-start.
    fn grace_timeout(&self) -> Duration {
        Duration::from_secs(15 * 60)
    }
}

// Structured frames contain whole messages and tool results. Keep a larger
// explicit bound than plain text, while bounding malformed protocol output.
const MAX_STRUCTURED_FRAME_BYTES: usize =
    crate::engine::agent_output::line_framer::MAX_STRUCTURED_FRAME_BYTES;

/// `EffectiveStatus` → the `program_status` event payload: spec spellings
/// (`snake_case` state and kind), absent fields omitted. The record `id` is
/// not part of the event; step attribution rides the envelope `source`.
pub(crate) fn program_status_event(
    status: &crate::data::program_status::EffectiveStatus,
) -> EventPayload {
    EventPayload::ProgramStatus {
        state: status.state,
        kind: status.kind,
        progress: status.progress,
        msg: status.msg.clone(),
        app: status.app.clone(),
    }
}

/// Ask for a PTY of `size` (`Some`) or keep the piped I/O (`None`). The
/// resize receiver has no sender here: an unattended PTY keeps its spawn
/// size until a viewer holding input resizes it through the hub.
fn with_pty_size(
    mut io: crate::engine::agent_runtime::frontend::AgentIo,
    size: Option<(u16, u16)>,
) -> crate::engine::agent_runtime::frontend::AgentIo {
    if let Some(size) = size {
        io.initial_size = Some(size);
        io.resize = Some(tokio::sync::mpsc::unbounded_channel().1);
    }
    io
}

/// Wire a container's stdout/stderr byte channels to the event bus
/// (WI 0137 §4): both streams are line-buffered, stdout runs through the
/// translator for `format`, and every [`Translated`] item becomes an event or
/// — for the final message — a call to `final_message`. Shared by the
/// top-level frontend and every per-container sink, so the API has one
/// drain implementation.
///
/// Attribution (WI 0137 §5) rides on `event_bus`: a sender made with
/// `EventBusSender::with_source` stamps every event emitted here.
fn spawn_output_drains(
    event_bus: &EventBusSender,
    format: AgentOutputFormat,
    final_message: Option<FinalMessageSink>,
) -> crate::engine::agent_runtime::frontend::AgentIo {
    let event_bus_stdout = event_bus.clone();
    let event_bus_stderr = event_bus.clone();

    let (stdout_tx, mut stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (stderr_tx, mut stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    // Drain stdout → translator → event bus (line-buffered).
    event_bus_stdout.clone().spawn_drain(async move {
        let shutdown = event_bus_stdout.output_shutdown();
        let mut translator = crate::engine::agent_output::translator_for_redacted(format, event_bus_stdout.redaction());
        let emit = |items: Vec<Translated>| {
            for item in items {
                emit_translated(&event_bus_stdout, final_message.as_ref(), item);
            }
        };
        let mut framer = crate::engine::agent_output::line_framer::LineFramer::new(
            if format.is_structured() {
                MAX_STRUCTURED_FRAME_BYTES
            } else {
                crate::engine::agent_output::line_framer::MAX_LINE_BYTES
            },
        );
        {
            let mut process_line = |line: crate::engine::agent_output::line_framer::FramedLine| {
                let items = if line.truncated && format.is_structured() {
                    // A partial protocol frame is neither text nor a final
                    // answer. Forget any earlier candidate before EOF fallback.
                    translator = crate::engine::agent_output::translator_for_redacted(format, event_bus_stdout.redaction());
                    vec![Translated::Activity {
                        kind: crate::data::execution_event::StepActivityKind::Message,
                        summary: "Agent frame too large; final message may be unavailable".into(),
                    }]
                } else if line.truncated {
                    vec![Translated::Line {
                        stream: OutputStream::Stdout,
                        text: line.marked_lossy_text(),
                    }]
                } else {
                    translator.feed_line(OutputStream::Stdout, &line.lossy_text())
                };
                emit(items);
            };
            loop {
                let bytes = tokio::select! {
                    _ = shutdown.cancelled(), if !stdout_rx.is_closed() => { stdout_rx.close(); continue; }
                    bytes = stdout_rx.recv() => bytes,
                };
                let Some(bytes) = bytes else { break; };
                for line in framer.push(&bytes) {
                    process_line(line);
                }
            }
            if let Some(line) = framer.finish() {
                process_line(line);
            }
        }
        emit(translator.finish());
    });

    // Drain stderr → event bus (line-buffered). Structured formats print
    // on stdout only, so stderr never goes through the translator.
    event_bus_stderr.clone().spawn_drain(async move {
        let shutdown = event_bus_stderr.output_shutdown();
        let mut framer = crate::engine::agent_output::line_framer::LineFramer::default();
        loop {
            let bytes = tokio::select! {
                _ = shutdown.cancelled(), if !stderr_rx.is_closed() => { stderr_rx.close(); continue; }
                bytes = stderr_rx.recv() => bytes,
            };
            let Some(bytes) = bytes else { break; };
            for line in framer.push(&bytes) {
                event_bus_stderr.emit(EventPayload::StderrLine(line.marked_lossy_text()));
            }
        }
        if let Some(line) = framer.finish() {
            event_bus_stderr.emit(EventPayload::StderrLine(line.marked_lossy_text()));
        }
    });

    crate::engine::agent_runtime::frontend::AgentIo {
        stdout: stdout_tx,
        stderr: stderr_tx,
        stdin_tx,
        stdin_rx,
        resize: None,
        initial_size: None,
    }
}

/// One translated item onto the bus (or into the final-message sink).
fn emit_translated(
    event_bus: &EventBusSender,
    final_message: Option<&FinalMessageSink>,
    item: Translated,
) {
    match item {
        Translated::Line {
            stream: OutputStream::Stdout,
            text,
        } => event_bus.emit(EventPayload::StdoutLine(text)),
        Translated::Line {
            stream: OutputStream::Stderr,
            text,
        } => event_bus.emit(EventPayload::StderrLine(text)),
        Translated::Activity { kind, summary } => {
            event_bus.emit(EventPayload::StepActivity { kind, summary })
        }
        Translated::FinalMessage(message) => {
            if let Some(sink) = final_message {
                sink(message);
            }
        }
    }
}

use crate::engine::agent_output::container_status_message;

impl ApiDispatchFrontend {
    /// Where a translated final message goes: the shared state writer, so
    /// the queue worker finds it in `ResultContributions::final_message`.
    /// `None` without a state writer (unit tests, nested frontends).
    fn final_message_sink(&self) -> Option<FinalMessageSink> {
        if self.command_path != "exec prompt" {
            return None;
        }
        let writer = self.state_writer.clone()?;
        Some(std::sync::Arc::new(move |message: String| {
            writer.record_final_message(message)
        }))
    }
}

// ─── HasAgentFrontend ───────────────────────────────────────────────────

impl HasAgentFrontend for ApiDispatchFrontend {
    /// Image setup, probes and other helper containers: always piped.
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(ApiContainerSink::new(
            &self.event_bus,
            self.final_message_sink(),
            self.state_writer.clone(),
        ))
    }

    /// The single-agent launch (`exec prompt`, `chat`): it carries the
    /// interaction Layer 2 resolved, so an interactive run gets its PTY
    /// (`AgentIo::initial_size = decision.pty_size`). ACP launches bind
    /// their own piped transport and never call this.
    fn container_frontend_for_pty(&mut self) -> Box<dyn AgentFrontend> {
        let mut sink = ApiContainerSink::new(
            &self.event_bus,
            self.final_message_sink(),
            self.state_writer.clone(),
        );
        sink.pty_size = self.interaction.and_then(|d| d.pty_size);
        self.pty_sink_sender = Some(sink.event_bus.clone());
        Box::new(sink)
    }
}

/// Aborts its task when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Forward one agent's `StuckEvent::Reported` as `program_status` events,
/// skipping an unchanged repeat, until its detector channel closes. The
/// `Arc<Sender>` itself is not retained, so the task never keeps the channel
/// open past the execution.
fn spawn_status_forwarder(
    sender: &tokio::sync::broadcast::Sender<crate::engine::agent_runtime::StuckEvent>,
    event_bus: EventBusSender,
) -> Option<AbortOnDrop> {
    use crate::engine::agent_runtime::StuckEvent;
    use tokio::sync::broadcast::error::RecvError;
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    let mut events = sender.subscribe();
    Some(AbortOnDrop(runtime.spawn(async move {
        let mut last: Option<crate::data::program_status::EffectiveStatus> = None;
        loop {
            match events.recv().await {
                Ok(StuckEvent::Reported(status)) => {
                    if status == last {
                        continue;
                    }
                    if let Some(status) = &status {
                        event_bus.emit(program_status_event(status));
                    }
                    last = status;
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
    })))
}

// ─── AgentImageFrontend ─────────────────────────────────────────────────

/// One `AgentImageFrontend` for the API frontend: `ReadyFrontend` and
/// `InitFrontend` both extend it (F-33).
///
/// The separate `InitFrontend`/`ReadyFrontend` impls each tagged their
/// status events with their own command name (`phase: "init"` /
/// `phase: "ready"`, message prefix `Init step` / `Ready step`). The merged
/// impl reads that label from `command_path`, so the event stream is
/// unchanged for both commands.
impl crate::engine::agent::AgentImageFrontend for ApiDispatchFrontend {
    fn report_step_status(
        &mut self,
        step: &crate::data::setup_step::SetupStep,
        status: StepStatus,
    ) {
        let phase = self.command_path.clone();
        let label = title_case(&phase);
        self.event_bus.emit(EventPayload::StatusMessage {
            phase,
            message: format!("{label} step '{step}': {status:?}"),
        });
    }

    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(ApiContainerSink::new(
            &self.event_bus,
            self.final_message_sink(),
            self.state_writer.clone(),
        ))
    }
}

/// The event-source phase of a setup or teardown phase step.
fn phase_source(kind: PhaseKind) -> EventSourcePhase {
    match kind {
        PhaseKind::Setup => EventSourcePhase::Setup,
        PhaseKind::Teardown => EventSourcePhase::Teardown,
    }
}

/// Upper-case the first character; used only to label setup-progress events
/// with the command name the way the pre-merge impls spelled it.
fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Standalone container frontend that emits events to the EventBus.
///
/// One is created per container (`exec prompt` and the other single-agent
/// commands). Every event it emits is attributed (WI 0137 §5) to the main
/// phase, `step_attempt` 1, and the container name once `Running` reports
/// it. Workflow steps do not use it: their per-container proxy lends its
/// own source to the shared frontend.
struct ApiContainerSink {
    state_writer: Option<CommandStateWriter>,
    /// Stamps `source` as it is when each event is emitted.
    event_bus: EventBusSender,
    /// This container's attribution: the main phase, attempt 1, and the
    /// container name once it runs.
    source: ContainerSource,
    /// Set by the engine through `set_agent_output_format` before `take_io`;
    /// `Text` until then, and taken (reset) by `take_io`.
    output_format: AgentOutputFormat,
    /// Where a translated final message goes (`exec prompt`).
    final_message: Option<FinalMessageSink>,
    /// The interactive PTY size for the single-agent launch; `None` = piped.
    pty_size: Option<(u16, u16)>,
}

impl ApiContainerSink {
    fn new(
        event_bus: &EventBusSender,
        final_message: Option<FinalMessageSink>,
        state_writer: Option<CommandStateWriter>,
    ) -> Self {
        let source: ContainerSource = std::sync::Arc::new(std::sync::Mutex::new(
            EventSource::phase(EventSourcePhase::Main).with_step_attempt(1),
        ));
        Self {
            state_writer,
            event_bus: event_bus.with_shared_source(source.clone()),
            source,
            output_format: AgentOutputFormat::Text,
            final_message,
            pty_size: None,
        }
    }
}

impl UserMessageSink for ApiContainerSink {
    fn write_message(&mut self, msg: UserMessage) {
        emit_user_message(&self.event_bus, msg);
    }
    fn replay_queued(&mut self) {}
}

/// The status-line projection both API agent frontends share (the dispatch
/// frontend for workflow steps and `ApiContainerSink` for single-agent
/// commands): a `Running` status marks the command's container as started,
/// and every status becomes one `container` status message. Attribution
/// (which source the sender stamps) stays with each caller.
fn emit_container_status(
    sender: &EventBusSender,
    state_writer: Option<&CommandStateWriter>,
    status: &AgentStatus,
) {
    if matches!(status, AgentStatus::Running { .. }) {
        if let Some(writer) = state_writer {
            writer.record_container_started();
        }
    }
    sender.emit(EventPayload::StatusMessage {
        phase: "container".to_string(),
        message: container_status_message(status),
    });
}

/// One agent progress report as a status message in its own stage.
fn emit_progress(sender: &EventBusSender, progress: AgentProgress) {
    sender.emit(EventPayload::StatusMessage {
        phase: progress.stage,
        message: progress.message,
    });
}

/// One user message as a status message whose phase names its level.
fn emit_user_message(sender: &EventBusSender, msg: UserMessage) {
    let phase = match msg.level {
        crate::data::message::MessageLevel::Info => "info",
        crate::data::message::MessageLevel::Warning => "warn",
        crate::data::message::MessageLevel::Error => "error",
        crate::data::message::MessageLevel::Success => "ok",
    };
    sender.emit(EventPayload::StatusMessage {
        phase: phase.to_string(),
        message: msg.text,
    });
}

#[async_trait]
impl AgentFrontend for ApiContainerSink {
    fn terminal_source(&self) -> Option<EventSource> {
        self.source.lock().ok().map(|s| s.clone())
    }
    fn report_status(&mut self, status: AgentStatus) {
        // Attribute this and every later event to the running container
        // before the status line is emitted.
        if let AgentStatus::Running { container_name } = &status {
            self.source
                .lock()
                .expect("event source lock poisoned")
                .container = Some(container_name.clone());
        }
        emit_container_status(&self.event_bus, self.state_writer.as_ref(), &status);
    }
    fn report_progress(&mut self, progress: AgentProgress) {
        emit_progress(&self.event_bus, progress);
    }

    fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
        // Engine owns the single stdin_tx and drops it after seeding so EOF
        // arrives at the container's stdin pipe (see `spawn_piped_docker`).
        // An interactive single-agent launch asks for its PTY here.
        let io = spawn_output_drains(
            &self.event_bus,
            std::mem::take(&mut self.output_format),
            self.final_message.clone(),
        );
        with_pty_size(io, self.pty_size)
    }

    fn set_agent_output_format(&mut self, format: AgentOutputFormat) {
        self.output_format = format;
    }

    fn grace_timeout(&self) -> Duration {
        Duration::from_secs(15 * 60)
    }
}

// ─── MountScopeFrontend ─────────────────────────────────────────────────────

impl MountScopeFrontend for ApiDispatchFrontend {
    fn ask_mount_scope(
        &mut self,
        _git_root: &Path,
        _cwd: &Path,
    ) -> Result<MountScopeDecision, CommandError> {
        Ok(self.headless.mount_scope())
    }
}

// ─── AgentSetupFrontend ─────────────────────────────────────────────────────

impl AgentSetupFrontend for ApiDispatchFrontend {
    fn ask_agent_setup(
        &mut self,
        _requested: &AgentName,
        _default: &AgentName,
        default_available: bool,
        _image_only: bool,
    ) -> Result<AgentSetupDecision, CommandError> {
        Ok(self.headless.agent_setup(default_available))
    }

    fn record_fallback(&mut self, _requested: &AgentName, _fallback: &AgentName) {}
}

// ─── AgentAuthFrontend ──────────────────────────────────────────────────────

impl AgentAuthFrontend for ApiDispatchFrontend {
    fn ask_agent_auth_consent(
        &mut self,
        _agent: &AgentName,
        _env_var_names: &[&str],
    ) -> Result<AgentAuthDecision, CommandError> {
        Ok(self.headless.agent_auth_consent())
    }
}

// ─── WorkflowFrontend ───────────────────────────────────────────────────────

impl WorkflowFrontend for ApiDispatchFrontend {
    fn report_workflow_state(&mut self, state: &WorkflowState) {
        self.workflow_state = Some(state.clone());
        self.set_step_order(state.steps.iter().map(|step| step.name.clone()));
        // The engine's launch counts are authoritative and survive a restart.
        for (name, attempt) in &state.step_attempts {
            self.step_attempts.insert(name.clone(), *attempt);
        }
        for kind in [PhaseKind::Setup, PhaseKind::Teardown] {
            for (index, step) in state.phase_step_states(kind).iter().enumerate() {
                let key = (kind.label().to_string(), index);
                if let crate::data::workflow_state::PhaseStepStatus::Remediating {
                    attempt, ..
                } = step.status
                {
                    self.phase_attempts.insert(key.clone(), attempt + 1);
                }
                if let crate::data::workflow_state::PhaseStepStatus::Failed { ref error } =
                    step.status
                {
                    if let Some(writer) = &self.state_writer {
                        writer.record_failed_step(FailedStep {
                            name: step.description.clone(),
                            index,
                            phase: if kind == PhaseKind::Setup {
                                FailedStepPhase::Setup
                            } else {
                                FailedStepPhase::Teardown
                            },
                            exit_code: None,
                            step_attempt: *self.phase_attempts.get(&key).unwrap_or(&1),
                            error: Some(error.clone()),
                        });
                    }
                }
                if matches!(
                    step.status,
                    crate::data::workflow_state::PhaseStepStatus::Succeeded
                ) {
                    if let Some(writer) = &self.state_writer {
                        writer.record_step_succeeded(
                            if kind == PhaseKind::Setup {
                                FailedStepPhase::Setup
                            } else {
                                FailedStepPhase::Teardown
                            },
                            index,
                        );
                    }
                }
            }
        }
        if let Some(writer) = &self.state_writer {
            for (index, step) in state.steps.iter().enumerate() {
                if matches!(
                    state.step_states.get(&step.name),
                    Some(crate::data::workflow_state::StepState::Succeeded)
                ) {
                    writer.record_step_succeeded(FailedStepPhase::Main, index);
                }
                if let Some(crate::data::workflow_state::StepState::Failed {
                    exit_code,
                    error_message,
                }) = state.step_states.get(&step.name)
                {
                    writer.record_failed_step(FailedStep {
                        name: step.name.clone(),
                        index,
                        phase: FailedStepPhase::Main,
                        exit_code: Some(*exit_code),
                        step_attempt: *self.step_attempts.get(&step.name).unwrap_or(&1),
                        error: error_message.clone(),
                    });
                }
            }
            crate::command::worker::events::persist_workflow_snapshot(
                writer,
                &self.event_bus,
                state,
            );
        }
    }

    fn show_workflow_control_board(
        &mut self,
        _state: &crate::data::workflow_state::WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        let _awaiting_input = self.event_bus.awaiting_input();
        Ok(self.headless.workflow_next_action(available))
    }

    fn yolo_countdown_tick(
        &mut self,
        step_name: &str,
        remaining: Duration,
        _total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        use crate::engine::workflow::timing::YOLO_SINK_THROTTLE_INTERVAL;

        let should_emit = self
            .last_sink_message_time
            .map(|t| t.elapsed() >= YOLO_SINK_THROTTLE_INTERVAL)
            .unwrap_or(true);
        if should_emit {
            let what = match self.countdown_kind {
                CountdownKind::StuckStep => "auto-advancing",
                CountdownKind::FailureRetry => "retrying after failure",
                CountdownKind::AgentDone => "advancing after the agent reported done",
                CountdownKind::AgentBlocked => "failing the step: the agent reported blocked",
                CountdownKind::AgentError => "failing the step: the agent reported an error",
            };
            // `step_name` is a countdown label, which for an `on_failure` agent
            // is not a step: attribute it only when it names one.
            let sender = if self.step_order.iter().any(|n| n == step_name) {
                self.step_sender(step_name)
            } else {
                self.event_bus.clone()
            };
            sender.emit(EventPayload::StatusMessage {
                phase: "yolo_countdown".to_string(),
                message: format!("Step '{}': {what} in {}s", step_name, remaining.as_secs()),
            });
            self.last_sink_message_time = Some(std::time::Instant::now());
        }
        Ok(self.headless.yolo_tick())
    }

    fn yolo_countdown_started(&mut self, _step_name: &str, kind: CountdownKind) {
        self.countdown_kind = kind;
    }

    /// WI 0141: the agent's effective OSC 7501 status, as a `program_status`
    /// event attributed to its step (an `on_failure` agent or the dynamic
    /// leader reports under a label that is not a step: root sender). The
    /// engine already deduplicated and rate-limited it. A clear (`None`) has
    /// no `program_status` spelling, so it emits nothing; the step's next
    /// status event or its end supersedes the last reported state.
    fn report_agent_status(
        &mut self,
        step_name: &str,
        status: Option<&crate::data::program_status::EffectiveStatus>,
    ) {
        let Some(payload) = status.map(program_status_event) else {
            return;
        };
        let sender = if self.step_order.iter().any(|n| n == step_name) {
            self.step_sender(step_name)
        } else {
            self.event_bus.clone()
        };
        sender.emit(payload);
    }

    fn yolo_countdown_finished(&mut self, _step_name: &str) {
        self.last_sink_message_time = None;
        self.countdown_kind = CountdownKind::default();
    }

    fn report_step_launching(&mut self, step_name: &str, step_attempt: u32) {
        self.step_attempts
            .insert(step_name.to_string(), step_attempt);
        // A new launch runs in a new container; its name arrives with `Running`.
        self.step_containers.remove(step_name);
    }

    fn set_container_source(&mut self, source: Option<ContainerSource>) {
        self.container_source = source;
    }

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        let Some(index) = self.step_index_for(&step.name) else {
            tracing::warn!("Skipping step transition without a definition index");
            return;
        };
        if matches!(status, WorkflowStepStatus::Succeeded) {
            if let Some(writer) = &self.state_writer {
                writer.record_step_succeeded(FailedStepPhase::Main, index);
            }
        }
        if let WorkflowStepStatus::Failed { exit_code } = &status {
            if let Some(writer) = &self.state_writer {
                writer.record_failed_step(FailedStep {
                    name: step.name.clone(),
                    index,
                    phase: FailedStepPhase::Main,
                    exit_code: Some(*exit_code),
                    step_attempt: *self.step_attempts.get(&step.name).unwrap_or(&1),
                    error: None,
                });
            }
        }
        // The `from` status is what the step must have been in for this
        // transition to be reachable; the engine reports only the new one.
        let (from, to) = match &status {
            WorkflowStepStatus::Pending => return,
            WorkflowStepStatus::Running => (StepStatusKind::Pending, StepStatusKind::Running),
            WorkflowStepStatus::Succeeded => (StepStatusKind::Running, StepStatusKind::Succeeded),
            WorkflowStepStatus::Failed { .. } => (StepStatusKind::Running, StepStatusKind::Failed),
            WorkflowStepStatus::Cancelled => (StepStatusKind::Pending, StepStatusKind::Cancelled),
            WorkflowStepStatus::Skipped => (StepStatusKind::Pending, StepStatusKind::Skipped),
        };
        let idx = index;
        self.step_sender(&step.name)
            .emit(EventPayload::WorkflowStepTransition {
                step_name: step.name.clone(),
                step_index: idx,
                from_status: from,
                to_status: to,
            });
    }

    fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}

    fn report_parallel_step_launched(&mut self, step_name: &str, agent: &str, model: Option<&str>) {
        let Some(idx) = self.step_index_for(step_name) else {
            return;
        };
        self.step_sender(step_name)
            .emit(EventPayload::WorkflowParallelStepLaunched {
                step_name: step_name.to_string(),
                step_index: idx,
                agent: agent.to_string(),
                model: model.map(|m| m.to_string()),
            });
    }

    fn report_parallel_step_exited(&mut self, step_name: &str, exit_code: i32) {
        let Some(idx) = self.step_index_for(step_name) else {
            return;
        };
        self.step_sender(step_name)
            .emit(EventPayload::WorkflowParallelStepExited {
                step_name: step_name.to_string(),
                step_index: idx,
                exit_code,
            });
    }

    fn report_parallel_step_container(&mut self, step_name: &str, container_name: &str) {
        self.step_containers
            .insert(step_name.to_string(), container_name.to_string());
    }

    fn report_parallel_group_finished(&mut self) {
        self.event_bus
            .emit(EventPayload::WorkflowParallelGroupFinished);
    }

    fn report_workflow_progress(
        &mut self,
        steps: &[crate::engine::workflow::actions::WorkflowStepProgressInfo],
    ) {
        if let Some(writer) = &self.state_writer {
            writer.record_workflow_main_started();
        }
        // `WorkflowState.steps` is the reference order; the definition walk
        // stands in for it only until the first state snapshot arrives.
        if self.workflow_state.is_none() {
            self.set_step_order(steps.iter().map(|step| step.name.clone()));
        }
        // Emit one WorkflowPhaseTransition event the first time the engine
        // reports progress (the workflow has entered the main phase) and one
        // more on completion (handled in report_workflow_completed).
        let mut phase_emitted = self
            .phase_emitted
            .lock()
            .expect("phase_emitted lock poisoned");
        if !*phase_emitted {
            *phase_emitted = true;
            drop(phase_emitted);
            let total = steps.len();
            self.phase_sender(EventSourcePhase::Main)
                .emit(EventPayload::WorkflowPhaseTransition {
                    phase: "main".to_string(),
                    step_desc: format!(
                        "Running workflow ({total} step{})",
                        if total == 1 { "" } else { "s" }
                    ),
                    status: PhaseStatusKind::Running,
                });
        }
    }

    fn confirm_resume(&mut self, _mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        Ok(self.headless.confirm_resume())
    }

    // `supports_interactive_recovery` keeps its `false` default: an API run has
    // no user to ask, so a failed step takes the engine's countdown-and-retry
    // path (WI-0115 §3).
    // One set of four for both phases (F-34). `PhaseKind::label()` is the
    // `phase` string, so the event stream is unchanged: `"setup"` and
    // `"teardown"` exactly as before.

    fn on_phase_step_started(&mut self, kind: PhaseKind, description: &str) {
        self.phase_sender(phase_source(kind))
            .emit(EventPayload::StatusMessage {
                phase: kind.label().to_string(),
                message: format!("started: {description}"),
            });
        self.phase_sender(phase_source(kind))
            .emit(EventPayload::WorkflowPhaseTransition {
                phase: kind.label().to_string(),
                step_desc: description.to_string(),
                status: PhaseStatusKind::Running,
            });
    }

    fn on_phase_step_output(&mut self, kind: PhaseKind, line: &str) {
        self.phase_sender(phase_source(kind))
            .emit(EventPayload::StatusMessage {
                phase: kind.label().to_string(),
                message: line.to_string(),
            });
    }

    fn on_phase_step_completed(&mut self, kind: PhaseKind, description: &str) {
        self.phase_sender(phase_source(kind))
            .emit(EventPayload::StatusMessage {
                phase: kind.label().to_string(),
                message: format!("completed: {description}"),
            });
        self.phase_sender(phase_source(kind))
            .emit(EventPayload::WorkflowPhaseTransition {
                phase: kind.label().to_string(),
                step_desc: description.to_string(),
                status: PhaseStatusKind::Succeeded,
            });
    }

    fn on_phase_step_skipped(&mut self, kind: PhaseKind, description: &str) {
        self.phase_sender(phase_source(kind))
            .emit(EventPayload::StatusMessage {
                phase: kind.label().to_string(),
                message: format!("skipped (cancelled): {description}"),
            });
        self.phase_sender(phase_source(kind))
            .emit(EventPayload::WorkflowPhaseTransition {
                phase: kind.label().to_string(),
                step_desc: description.to_string(),
                status: PhaseStatusKind::Cancelled,
            });
    }

    fn on_phase_step_failed(
        &mut self,
        kind: PhaseKind,
        description: &str,
        exit_code: i32,
        stderr: &str,
    ) {
        if let Some(writer) = &self.state_writer {
            let index = self
                .workflow_state
                .as_ref()
                .and_then(|state| {
                    state.phase_step_states(kind).iter().rposition(|step| {
                        step.description == description
                            && matches!(
                                step.status,
                                crate::data::workflow_state::PhaseStepStatus::Failed { .. }
                            )
                    })
                })
                .unwrap_or(0);
            writer.record_failed_step(FailedStep {
                name: description.into(),
                index,
                phase: if kind == PhaseKind::Setup {
                    FailedStepPhase::Setup
                } else {
                    FailedStepPhase::Teardown
                },
                exit_code: Some(exit_code),
                step_attempt: *self
                    .phase_attempts
                    .get(&(kind.label().into(), index))
                    .unwrap_or(&1),
                error: Some(stderr.to_string()),
            });
            if kind == PhaseKind::Teardown
                && writer.result_contributions().workflow_outcome == Some(CommandStatusKind::Done)
            {
                writer.record_workflow_outcome(CommandStatusKind::Error);
                writer.record_workflow_failure(Some(crate::data::fs::api_db::CommandFailure {
                    stage: "teardown".into(),
                    code: crate::data::fs::api_db::failure_codes::TEARDOWN_FAILED.into(),
                    message: "Teardown failed".into(),
                    step: Some(description.into()),
                }));
                self.phase_sender(EventSourcePhase::Main).emit(
                    EventPayload::WorkflowPhaseTransition {
                        phase: "main".into(),
                        step_desc: "Workflow completed but teardown failed".into(),
                        status: PhaseStatusKind::TeardownFailed,
                    },
                );
            }
        }
        self.phase_sender(phase_source(kind))
            .emit(EventPayload::StatusMessage {
                phase: kind.label().to_string(),
                message: format!("failed: {description} (exit {exit_code}): {stderr}"),
            });
        self.phase_sender(phase_source(kind))
            .emit(EventPayload::WorkflowPhaseTransition {
                phase: kind.label().to_string(),
                step_desc: description.to_string(),
                status: PhaseStatusKind::Failed,
            });
    }

    fn report_pull_request_created(&mut self, url: &str) {
        if let Some(writer) = &self.state_writer {
            writer.record_pull_request(url);
        }
    }

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        let (status, phase_status, step_desc) = match outcome {
            WorkflowOutcome::Completed => (
                CommandStatusKind::Done,
                PhaseStatusKind::Succeeded,
                "Workflow completed".into(),
            ),
            WorkflowOutcome::Paused => (
                CommandStatusKind::Paused,
                PhaseStatusKind::Paused,
                "Workflow paused".into(),
            ),
            WorkflowOutcome::Aborted => (
                CommandStatusKind::Aborted,
                PhaseStatusKind::Failed,
                "Workflow aborted".into(),
            ),
            WorkflowOutcome::Cancelled => (
                CommandStatusKind::Cancelled,
                PhaseStatusKind::Cancelled,
                "Workflow cancelled".into(),
            ),
            WorkflowOutcome::CompletedTeardownFailed => (
                CommandStatusKind::Error,
                PhaseStatusKind::TeardownFailed,
                "Workflow completed but teardown failed".into(),
            ),
            WorkflowOutcome::Failed { last_step, .. } => (
                CommandStatusKind::Error,
                PhaseStatusKind::Failed,
                format!("Step '{last_step}' failed"),
            ),
        };
        if let Some(writer) = &self.state_writer {
            writer.record_workflow_outcome(status);
            writer.record_workflow_failure(match outcome {
                WorkflowOutcome::Failed { last_step, .. } => {
                    Some(crate::data::fs::api_db::CommandFailure {
                        stage: "step".into(),
                        code: crate::data::fs::api_db::failure_codes::STEP_FAILED.into(),
                        message: format!("Step '{last_step}' failed"),
                        step: Some(last_step.clone()),
                    })
                }
                WorkflowOutcome::CompletedTeardownFailed => {
                    Some(crate::data::fs::api_db::CommandFailure {
                        stage: "teardown".into(),
                        code: crate::data::fs::api_db::failure_codes::TEARDOWN_FAILED.into(),
                        message: "Teardown failed".into(),
                        step: None,
                    })
                }
                WorkflowOutcome::Cancelled => Some(crate::data::fs::api_db::CommandFailure {
                    stage: match self.workflow_state.as_ref().map(|s| s.current_phase) {
                        Some(crate::data::workflow_state::WorkflowPhase::Setup) => "setup",
                        Some(crate::data::workflow_state::WorkflowPhase::Teardown) => "teardown",
                        _ => "step",
                    }
                    .into(),
                    code: crate::data::fs::api_db::failure_codes::CANCELLED.into(),
                    message: "cancelled".into(),
                    step: None,
                }),
                _ => None,
            });
        }
        self.phase_sender(EventSourcePhase::Main)
            .emit(EventPayload::WorkflowPhaseTransition {
                phase: "main".into(),
                step_desc,
                status: phase_status,
            });
    }
}

// ─── WorktreeLifecycleFrontend ──────────────────────────────────────────────

impl WorktreeLifecycleFrontend for ApiDispatchFrontend {
    fn ask_pre_worktree_uncommitted_files(
        &mut self,
        _files: &[String],
        suggested_message: &str,
    ) -> Result<PreWorktreeDecision, CommandError> {
        Ok(self
            .headless
            .pre_worktree_uncommitted_files(suggested_message))
    }

    fn ask_existing_worktree(
        &mut self,
        _path: &Path,
        _branch: &str,
    ) -> Result<ExistingWorktreeDecision, CommandError> {
        Ok(self.headless.existing_worktree())
    }

    fn existing_worktree_after_resume(
        &mut self,
        _path: &Path,
        _branch: &str,
    ) -> Result<ExistingWorktreeDecision, CommandError> {
        Ok(self.headless.existing_worktree())
    }

    fn report_worktree_created(&mut self, path: &Path, branch: &str) {
        if let Some(writer) = &self.state_writer {
            writer.record_worktree(path);
        }
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "worktree".to_string(),
            message: format!("Worktree created: {} (branch: {branch})", path.display()),
        });
    }

    fn report_worktree_resumed(&mut self, path: &Path, _branch: &str) {
        if let Some(writer) = &self.state_writer {
            writer.record_worktree(path);
        }
    }

    fn ask_post_workflow_action(
        &mut self,
        prompt: &crate::command::commands::worktree_lifecycle::PostWorkflowWorktreePrompt,
    ) -> Result<PostWorkflowWorktreeAction, CommandError> {
        Ok(self.headless.post_workflow_action(prompt))
    }

    fn ask_worktree_commit_before_merge(
        &mut self,
        _branch: &str,
        _files: &[String],
        suggested_message: &str,
    ) -> Result<Option<String>, CommandError> {
        Ok(self
            .headless
            .worktree_commit_before_merge(suggested_message))
    }

    fn ask_merge_mode(&mut self, _branch: &str) -> Result<WorktreeMergeMode, CommandError> {
        Ok(self.headless.merge_mode())
    }

    fn ask_dirty_merge_target(
        &mut self,
        _target_branch: &str,
        _files: &[String],
        _suggested_message: &str,
    ) -> Result<DirtyMergeTargetDecision, CommandError> {
        Ok(self.headless.dirty_merge_target())
    }

    fn ask_launch_merge_fix_agent(
        &mut self,
        _failure: &MergeFailure,
    ) -> Result<bool, CommandError> {
        Ok(self.headless.launch_merge_fix_agent())
    }

    fn confirm_worktree_cleanup(
        &mut self,
        _branch: &str,
        _path: &Path,
    ) -> Result<bool, CommandError> {
        Ok(self.headless.confirm_worktree_cleanup())
    }

    fn report_merge_conflict(&mut self, branch: &str, worktree_path: &Path, _git_root: &Path) {
        self.phase_sender(EventSourcePhase::Merge)
            .emit(EventPayload::StatusMessage {
                phase: "worktree".to_string(),
                message: format!(
                    "Merge conflict on branch '{branch}' at {}",
                    worktree_path.display()
                ),
            });
    }

    fn report_worktree_discarded(&mut self, branch: &str) {
        if let Some(writer) = &self.state_writer {
            writer.clear_worktree();
        }
        self.phase_sender(EventSourcePhase::Merge)
            .emit(EventPayload::StatusMessage {
                phase: "worktree".to_string(),
                message: format!("Worktree discarded: {branch}"),
            });
    }

    fn report_worktree_kept(&mut self, path: &Path, branch: &str) {
        if let Some(writer) = &self.state_writer {
            writer.record_worktree(path);
        }
        self.phase_sender(EventSourcePhase::Merge)
            .emit(EventPayload::StatusMessage {
                phase: "worktree".to_string(),
                message: format!("Worktree kept: {} (branch: {branch})", path.display()),
            });
    }
}

// ─── InitFrontend ───────────────────────────────────────────────────────────

impl InitFrontend for ApiDispatchFrontend {
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError> {
        Ok(self.headless.replace_aspec())
    }
    fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
        Ok(self.headless.run_audit())
    }
    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError> {
        Ok(self.headless.work_items_setup())
    }
    fn ask_dockerfile_setup(
        &mut self,
        _prompt: &crate::data::prompt::Prompt<crate::engine::init::frontend::DockerfileSetupChoice>,
        _git_root: &std::path::Path,
        _dockerfile_path: &str,
    ) -> Result<crate::engine::init::frontend::DockerfileSetupDecision, EngineError> {
        Ok(self.headless.dockerfile_setup())
    }
    fn report_phase(&mut self, phase: &InitPhase) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "init".to_string(),
            message: format!("Init phase: {phase:?}"),
        });
    }
    fn report_summary(&mut self, _summary: &InitSummary) {}
}

// ─── ReadyFrontend ──────────────────────────────────────────────────────────

impl ReadyFrontend for ApiDispatchFrontend {
    fn ask_create_dockerfile(
        &mut self,
        _dockerfile_path: &std::path::Path,
    ) -> Result<bool, EngineError> {
        Ok(self.headless.create_dockerfile())
    }
    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
        Ok(self.headless.run_audit_on_template())
    }
    fn report_phase(&mut self, phase: &ReadyPhase) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "ready".to_string(),
            // The shared `ReadyPhase` Display text, as the TUI reports it.
            message: format!("ready: {phase}"),
        });
    }

    /// The rows, outcome line and hint are the summary's own (F-23;
    /// readiness now accounts for per-image build/import results too). This
    /// reports them through the same `StatusMessage` event path every other
    /// `ready` progress line already uses, rather than discarding the
    /// summary as before.
    fn report_summary(&mut self, summary: &ReadySummary) {
        for (label, status) in summary.rows() {
            self.event_bus.emit(EventPayload::StatusMessage {
                phase: "ready".to_string(),
                message: format!("{label}: {}", status.label()),
            });
        }
        if let Some(hint) = summary.hint() {
            self.event_bus.emit(EventPayload::StatusMessage {
                phase: "ready".to_string(),
                message: format!("Tip: {hint}"),
            });
        }
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "ready".to_string(),
            message: summary.outcome_line().to_string(),
        });
    }
}

// ─── Per-command frontend markers ───────────────────────────────────────────

impl RemoteCommandFrontend for ApiDispatchFrontend {}

// Attach is excluded from API dispatch by the catalogue. Keep the adapter
// total for DispatchFrontend, with an explicit refusal if called directly.
#[async_trait]
impl crate::command::commands::attach::AttachCommandFrontend for ApiDispatchFrontend {
    fn begin_attach(
        &mut self,
        _terminal_id: &crate::data::ids::TerminalId,
    ) -> tokio_util::sync::CancellationToken {
        tokio_util::sync::CancellationToken::new()
    }

    fn hangup(&self) -> tokio_util::sync::CancellationToken {
        tokio_util::sync::CancellationToken::new()
    }

    fn bind_terminal(
        &mut self,
        _stream: crate::engine::remote::terminal_client::TerminalStream,
        _read_only: bool,
        _policy: crate::data::terminal::HostOutputPolicy,
    ) -> Result<tokio::task::JoinHandle<()>, CommandError> {
        Err(CommandError::NotAvailableForFrontend {
            command: "attach".into(),
            frontend: "API".into(),
        })
    }

    fn end_attach(&mut self) {}
}

impl AgentsCommandFrontend for ApiDispatchFrontend {
    fn render_overview(
        &mut self,
        overview: &crate::command::commands::agents::AgentsOverview,
        watching: bool,
    ) {
        if !watching {
            self.emit_agents_json(overview);
        }
    }
    fn show_list(&mut self, outcome: &crate::command::commands::agents::AgentsListOutcome) {
        self.emit_agents_json(&outcome.agents);
    }
    fn show_detail(&mut self, outcome: &crate::command::commands::agents::AgentsDetailOutcome) {
        self.emit_agents_json(&outcome.detail);
    }
    fn show_kill_result(&mut self, outcome: &crate::command::commands::agents::AgentsKillOutcome) {
        self.emit_agents_json(outcome);
    }
}

impl ApiDispatchFrontend {
    /// One stdout event contains the same JSON payload as the CLI.
    fn emit_agents_json(&self, value: &impl serde::Serialize) {
        if let Some(writer) = &self.state_writer {
            writer.record_data(
                serde_json::to_value(value).expect("agent outcomes are JSON serializable"),
            );
        }
        // Retain JSON stdout for one release for existing log consumers.
        self.event_bus.emit(EventPayload::StdoutLine(
            serde_json::to_string(value).expect("agent outcomes are JSON serializable"),
        ));
    }
}

impl ConfigCommandFrontend for ApiDispatchFrontend {
    fn present_config_table(
        &mut self,
        _rows: &[ConfigFieldRow],
        _rejected: Option<&crate::command::commands::config::ConfigEditRejection>,
    ) -> Result<Option<ConfigEditRequest>, CommandError> {
        Ok(None)
    }
}

// `awman clean` is blocked for the API frontend at the catalogue layer
// (`api_allowed: false`); this impl exists only to satisfy the `DispatchFrontend`
// supertrait bound and never runs. It never confirms a deletion.
impl crate::command::commands::clean::CleanCommandFrontend for ApiDispatchFrontend {
    fn confirm_deletion(
        &mut self,
        _summary: &crate::command::commands::clean::CleanSummary,
    ) -> Result<bool, CommandError> {
        Ok(self.headless.confirm_deletion())
    }
}

#[async_trait]
impl crate::command::commands::daemon::lifecycle::DaemonCommandFrontend for ApiDispatchFrontend {
    /// The daemon is not a command an API request can become.
    async fn host_daemon(
        &mut self,
        _env: crate::data::config::env::EnvSnapshot,
        _set: crate::data::config::overrides::ConfigOverrides,
    ) -> Result<(), CommandError> {
        Err(CommandError::NotAvailableForFrontend {
            command: "daemon start".into(),
            frontend: "api".into(),
        })
    }
}

impl ChatCommandFrontend for ApiDispatchFrontend {}

/// One `AgentLaunchFrontend` for the API frontend. There is no host terminal
/// behind an HTTP request, so there is nothing for `set_pty_active` to gate.
impl crate::command::commands::agent_setup::AgentLaunchFrontend for ApiDispatchFrontend {
    fn set_pty_active(&mut self, _active: bool) {}

    /// WI 0141: a single-agent command's OSC 7501 reports become
    /// `program_status` events, attributed to the launched container (the
    /// last `container_frontend_for_pty` sink). Workflow steps report
    /// through `WorkflowFrontend::report_agent_status` instead; setup
    /// frontends (`agent_setup`) never call this, so their exemption stays.
    fn set_stuck_sender(
        &mut self,
        sender: std::sync::Arc<
            tokio::sync::broadcast::Sender<crate::engine::agent_runtime::StuckEvent>,
        >,
    ) {
        let bus = self
            .pty_sink_sender
            .clone()
            .unwrap_or_else(|| self.event_bus.clone());
        // Replacing the forwarder aborts the previous agent's.
        self.status_forwarder = spawn_status_forwarder(&sender, bus);
    }
}

impl AcpFrontend for ApiDispatchFrontend {
    fn render_update(&mut self, update: SessionUpdate) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "acp".to_string(),
            message: crate::engine::acp::protocol::summarize_update(&update),
        });
    }

    fn request_permission(&mut self, request: PermissionRequest) -> PermissionDecision {
        self.headless.acp_permission(&request.options)
    }

    fn next_prompt(&mut self) -> Option<String> {
        None
    }
}

impl ExecPromptCommandFrontend for ApiDispatchFrontend {}

#[async_trait]
impl ExecWorkflowCommandFrontend for ApiDispatchFrontend {
    fn record_cancel_cleanup(
        &mut self,
        cleanup: &crate::data::workflow_cleanup::WorkflowCleanup,
    ) -> Result<(), CommandError> {
        if let Some(writer) = &self.state_writer {
            writer.write_cleanup(cleanup)?;
        }
        Ok(())
    }
    fn report_workflow_summary(&mut self, summary: &WorkflowSummary) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "workflow".to_string(),
            message: format!(
                "Workflow summary: {} completed, {} failed",
                summary.steps_completed, summary.steps_failed
            ),
        });
    }
    fn ask_workflow_resume(
        &mut self,
        prompt: &WorkflowResumePrompt,
    ) -> Result<WorkflowResumeDecision, CommandError> {
        Ok(self.headless.workflow_resume(prompt))
    }

    fn notify_dynamic_workflow_resume_unavailable(
        &mut self,
        work_item: &str,
        reason: &str,
    ) -> Result<(), CommandError> {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "workflow".to_string(),
            message: format!(
                "cannot resume the previous dynamic workflow for work item {work_item}: \
                 {reason}"
            ),
        });
        Ok(())
    }
}

impl SpecsCommandFrontend for ApiDispatchFrontend {}

impl NewCommandFrontend for ApiDispatchFrontend {}

/// No person is behind an HTTP request, so a custom definition that needs
/// approval is refused (`AgentDefinitionUntrusted`), never approved.
impl crate::command::commands::agent_trust::AgentTrustFrontend for ApiDispatchFrontend {}

impl crate::command::commands::workflow_prerequisites::WorkflowPrerequisiteFrontend
    for ApiDispatchFrontend
{
    fn begin_workflow_prerequisites(&mut self) {
        self.prerequisite_messages.get_or_insert_with(Vec::new);
    }

    fn show_workflow_prerequisites(
        &mut self,
        report: &crate::data::workflow_prerequisites::WorkflowPrerequisiteReport,
        _next: crate::command::commands::workflow_prerequisites::PrerequisiteReportNext,
    ) -> Result<(), CommandError> {
        self.event_bus.emit(EventPayload::WorkflowPrerequisites {
            phase: report.phase,
            report: report.clone(),
        });
        self.flush_prerequisite_messages();
        Ok(())
    }

    fn report_workflow_prerequisites_recheck(
        &mut self,
        report: &crate::data::workflow_prerequisites::WorkflowPrerequisiteReport,
    ) {
        self.event_bus.emit(EventPayload::WorkflowPrerequisites {
            phase: report.phase,
            report: report.clone(),
        });
    }
}
