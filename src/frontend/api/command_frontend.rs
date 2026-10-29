//! `ApiDispatchFrontend` — the single Layer 3 struct that implements
//! every per-command frontend trait for API HTTP command dispatch.
//!
//! When a `POST /v1/commands` request arrives, the route handler constructs
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
use crate::command::commands::api_server::{
    ApiKeyDisclosure, ApiServerCommandFrontend, ApiServerRuntime,
};
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
use crate::data::config::repo::WorkItemsConfig;
use crate::data::fs::api_command_log::CommandStateWriter;
use crate::data::fs::api_db::{FailedStep, FailedStepPhase};
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
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
    frontend_kind: FrontendKind,
    /// Typed flags/arguments parsed by Layer 2 (`CommandCatalogue::parse_raw_args`).
    /// Empty when parsing failed; the error is surfaced via `parse_error`.
    parsed: ParsedArgs,
    /// A structured parse error captured at construction time, if any. Every
    /// `CommandFrontend` accessor returns it so Dispatch aborts the command
    /// rather than running with silently-dropped flags. `None` on success.
    parse_error: Option<CommandError>,
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
        Self::for_frontend(subcommand, args, event_bus, FrontendKind::Api)
    }

    pub fn for_frontend(
        subcommand: &str,
        args: &[String],
        event_bus: EventBusSender,
        frontend_kind: FrontendKind,
    ) -> Self {
        let path: Vec<&str> = subcommand.split_whitespace().collect();
        let (parsed, parse_error) =
            match CommandCatalogue::get().parse_raw_args_with_profile(&path, args, frontend_kind) {
                Ok(parsed) => (parsed, None),
                Err(e) => (ParsedArgs::default(), Some(e)),
            };

        Self {
            frontend_kind,
            parsed,
            parse_error,
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

impl Drop for ApiDispatchFrontend {
    fn drop(&mut self) {
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
        let phase = match msg.level {
            crate::data::message::MessageLevel::Info => "info",
            crate::data::message::MessageLevel::Warning => "warn",
            crate::data::message::MessageLevel::Error => "error",
            crate::data::message::MessageLevel::Success => "ok",
        };
        self.container_sender().emit(EventPayload::StatusMessage {
            phase: phase.to_string(),
            message: msg.text,
        });
    }

    fn replay_queued(&mut self) {}
}

// ─── CommandFrontend (flag/argument access) ─────────────────────────────────

impl ApiDispatchFrontend {
    /// Re-materialize the construction-time parse error, if any. The first
    /// accessor Dispatch calls returns this so a malformed request aborts the
    /// command instead of running with silently-dropped flags.
    fn check_parse(&self) -> Result<(), CommandError> {
        match &self.parse_error {
            None => Ok(()),
            Some(e) => Err(clone_parse_error(e)),
        }
    }
}

impl CommandFrontend for ApiDispatchFrontend {
    fn kind(&self) -> crate::command::dispatch::catalogue::FrontendKind {
        self.frontend_kind
    }
    fn flag_bool(&self, _command_path: &[&str], flag: &str) -> Result<Option<bool>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.flag_bool(flag))
    }

    fn flag_string(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<String>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.flag_string(flag))
    }

    fn flag_strings(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Vec<String>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.flag_strings(flag))
    }

    fn flag_path(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<PathBuf>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.flag_path(flag))
    }

    fn flag_enum(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<String>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.flag_enum(flag))
    }

    fn flag_u16(&self, _command_path: &[&str], flag: &str) -> Result<Option<u16>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.flag_u16(flag))
    }

    fn flag_usize(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<usize>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.flag_usize(flag))
    }

    fn argument(&self, _command_path: &[&str], name: &str) -> Result<Option<String>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.argument(name))
    }

    fn arguments(&self, _command_path: &[&str], name: &str) -> Result<Vec<String>, CommandError> {
        self.check_parse()?;
        Ok(self.parsed.arguments(name))
    }
}

/// Reconstruct a parse-stage [`CommandError`] so accessors can return it more
/// than once from `&self` (the type is not `Clone` because it wraps engine/data
/// errors, but the parse-stage variants carry only owned string data).
fn clone_parse_error(e: &CommandError) -> CommandError {
    match e {
        CommandError::UnknownCommand { path, suggestions } => CommandError::UnknownCommand {
            path: path.clone(),
            suggestions: suggestions.clone(),
        },
        CommandError::UnknownFlag { command, flag } => CommandError::UnknownFlag {
            command: command.clone(),
            flag: flag.clone(),
        },
        CommandError::InvalidFlagValue {
            command,
            flag,
            reason,
        } => CommandError::InvalidFlagValue {
            command: command.clone(),
            flag: flag.clone(),
            reason: reason.clone(),
        },
        CommandError::MissingRequiredFlag { command, flag } => CommandError::MissingRequiredFlag {
            command: command.clone(),
            flag: flag.clone(),
        },
        CommandError::MissingRequiredArgument { command, argument } => {
            CommandError::MissingRequiredArgument {
                command: command.clone(),
                argument: argument.clone(),
            }
        }
        CommandError::UnexpectedArgument { command, argument } => {
            CommandError::UnexpectedArgument {
                command: command.clone(),
                argument: argument.clone(),
            }
        }
        // parse_raw_args only produces the variants above; anything else is
        // rendered to a stable string so the command still aborts cleanly.
        other => CommandError::Other(other.to_string()),
    }
}

// ─── AgentFrontend ──────────────────────────────────────────────────────

#[async_trait]
impl AgentFrontend for ApiDispatchFrontend {
    fn report_status(&mut self, status: AgentStatus) {
        if matches!(status, AgentStatus::Running { .. }) {
            if let Some(writer) = &self.state_writer {
                writer.record_container_started();
            }
        }
        let message = match &status {
            AgentStatus::Building => "Building container image...".to_string(),
            AgentStatus::Pulling => "Pulling container image...".to_string(),
            AgentStatus::Starting => "Starting container...".to_string(),
            AgentStatus::Running { container_name } => {
                format!("Container running: {container_name}")
            }
            AgentStatus::Stopping => "Stopping container...".to_string(),
            AgentStatus::Exited(code) => format!("Container exited with code {code}"),
            AgentStatus::Failed(reason) => format!("Container failed: {reason}"),
        };
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
        self.container_sender().emit(EventPayload::StatusMessage {
            phase: "container".to_string(),
            message,
        });
    }

    fn report_progress(&mut self, progress: AgentProgress) {
        self.container_sender().emit(EventPayload::StatusMessage {
            phase: progress.stage,
            message: progress.message,
        });
    }

    fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
        // API never has interactive stdin: the engine owns the only sender
        // and drops it after seeding the prompt (see `spawn_piped_docker`)
        // so the container sees EOF promptly. The drains keep the lent
        // source, so every line of this container stays attributed to it.
        spawn_output_drains(
            &self.container_sender(),
            std::mem::take(&mut self.output_format),
            self.final_message_sink(),
        )
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
        let mut translator = crate::engine::agent_output::translator_for(format);
        let emit = |items: Vec<Translated>| {
            for item in items {
                emit_translated(&event_bus_stdout, final_message.as_ref(), item);
            }
        };
        let mut buf = Vec::new();
        loop {
            let bytes = tokio::select! {
                _ = shutdown.cancelled(), if !stdout_rx.is_closed() => { stdout_rx.close(); continue; }
                bytes = stdout_rx.recv() => bytes,
            };
            let Some(bytes) = bytes else { break; };
            buf.extend_from_slice(&bytes);
            while let Some(pos) = buf.iter().position(|byte| *byte == b'\n') {
                let line = String::from_utf8_lossy(&buf[..pos]).into_owned();
                buf.drain(..=pos);
                emit(translator.feed_line(OutputStream::Stdout, &line));
            }
        }
        if !buf.is_empty() {
            emit(translator.feed_line(OutputStream::Stdout, &String::from_utf8_lossy(&buf)));
        }
        emit(translator.finish());
    });

    // Drain stderr → event bus (line-buffered). Structured formats print
    // on stdout only, so stderr never goes through the translator.
    event_bus_stderr.clone().spawn_drain(async move {
        let shutdown = event_bus_stderr.output_shutdown();
        let mut buf = Vec::new();
        loop {
            let bytes = tokio::select! {
                _ = shutdown.cancelled(), if !stderr_rx.is_closed() => { stderr_rx.close(); continue; }
                bytes = stderr_rx.recv() => bytes,
            };
            let Some(bytes) = bytes else { break; };
            buf.extend_from_slice(&bytes);
            while let Some(pos) = buf.iter().position(|byte| *byte == b'\n') {
                let line = String::from_utf8_lossy(&buf[..pos]).into_owned();
                buf.drain(..=pos);
                event_bus_stderr.emit(EventPayload::StderrLine(line));
            }
        }
        if !buf.is_empty() {
            event_bus_stderr.emit(EventPayload::StderrLine(String::from_utf8_lossy(&buf).into_owned()));
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
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(ApiContainerSink::new(
            &self.event_bus,
            self.final_message_sink(),
            self.state_writer.clone(),
        ))
    }
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
        }
    }
}

impl UserMessageSink for ApiContainerSink {
    fn write_message(&mut self, msg: UserMessage) {
        let phase = match msg.level {
            crate::data::message::MessageLevel::Info => "info",
            crate::data::message::MessageLevel::Warning => "warn",
            crate::data::message::MessageLevel::Error => "error",
            crate::data::message::MessageLevel::Success => "ok",
        };
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: phase.to_string(),
            message: msg.text,
        });
    }
    fn replay_queued(&mut self) {}
}

#[async_trait]
impl AgentFrontend for ApiContainerSink {
    fn report_status(&mut self, status: AgentStatus) {
        if let AgentStatus::Running { container_name } = &status {
            if let Some(writer) = &self.state_writer {
                writer.record_container_started();
            }
            self.source
                .lock()
                .expect("event source lock poisoned")
                .container = Some(container_name.clone());
        }
        let message = match &status {
            AgentStatus::Building => "Building container image...".to_string(),
            AgentStatus::Pulling => "Pulling container image...".to_string(),
            AgentStatus::Starting => "Starting container...".to_string(),
            AgentStatus::Running { container_name } => {
                format!("Container running: {container_name}")
            }
            AgentStatus::Stopping => "Stopping container...".to_string(),
            AgentStatus::Exited(code) => format!("Container exited with code {code}"),
            AgentStatus::Failed(reason) => format!("Container failed: {reason}"),
        };
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "container".to_string(),
            message,
        });
    }
    fn report_progress(&mut self, progress: AgentProgress) {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: progress.stage,
            message: progress.message,
        });
    }

    fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
        // Engine owns the single stdin_tx and drops it after seeding so EOF
        // arrives at the container's stdin pipe (see `spawn_piped_docker`).
        spawn_output_drains(
            &self.event_bus,
            std::mem::take(&mut self.output_format),
            self.final_message.clone(),
        )
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
            if let Err(error) = writer.write(state) {
                tracing::warn!(%error, "failed to write command workflow state");
            }
        }
    }

    fn show_workflow_control_board(
        &mut self,
        _state: &crate::data::workflow_state::WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
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
            tracing::warn!(
                step = step.name,
                "Skipping step transition without a definition index"
            );
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
impl ApiServerCommandFrontend for ApiDispatchFrontend {
    async fn serve_until_shutdown(
        &mut self,
        _runtime: ApiServerRuntime,
    ) -> Result<(), CommandError> {
        Err(CommandError::Other(
            "Cannot start a nested API server from within API dispatch".into(),
        ))
    }

    /// The key as text, on the message stream the caller is already reading.
    /// The API used to receive the CLI's banner and serialise the `═` runs
    /// into JSON, which no HTTP client wanted.
    fn show_api_key(&mut self, disclosure: &ApiKeyDisclosure) {
        self.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!(
                "awman API key (store this — it will not be shown again): {}",
                disclosure.key()
            ),
        });
    }
}

impl ChatCommandFrontend for ApiDispatchFrontend {}

/// One `AgentLaunchFrontend` for the API frontend. There is no host terminal
/// behind an HTTP request, so there is nothing for `set_pty_active` to gate.
impl crate::command::commands::agent_setup::AgentLaunchFrontend for ApiDispatchFrontend {
    fn set_pty_active(&mut self, _active: bool) {}
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
        work_item: u32,
        reason: &str,
    ) -> Result<(), CommandError> {
        self.event_bus.emit(EventPayload::StatusMessage {
            phase: "workflow".to_string(),
            message: format!(
                "cannot resume the previous dynamic workflow for work item {work_item:04}: \
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
    fn show_workflow_prerequisites(
        &mut self,
        report: &crate::data::workflow_prerequisites::WorkflowPrerequisiteReport,
        _next: crate::command::commands::workflow_prerequisites::PrerequisiteReportNext,
    ) -> Result<(), CommandError> {
        self.event_bus.emit(EventPayload::WorkflowPrerequisites {
            phase: report.phase,
            report: report.clone(),
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[path = "wi0132_e2e.rs"]
    mod wi0132_e2e;
    #[path = "wi0132_engine.rs"]
    mod wi0132_engine;
    #[path = "wi0137_agent_output.rs"]
    mod wi0137_agent_output;

    fn make_frontend(subcommand: &str, args: &[&str]) -> ApiDispatchFrontend {
        let bus = crate::command::commands::api_server::event_bus::EventBus::new(16);
        let sender = bus.sender();
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        ApiDispatchFrontend::new(subcommand, &args, sender)
    }

    // ─── WI 0136: output drains finish before Done ─────────────────────────
    //
    // The queue worker waits for the bus's drain barrier and only then emits
    // `Done`. Writing right up to exit, the last stdout line (even without a
    // trailing newline) must precede `Done` for every consumer.

    /// One simulated run: output is produced right up to "exit" by a
    /// concurrent producer, then the worker's barrier and `Done`.
    async fn run_once(run: usize, lines: usize, unterminated_tail: bool) {
        use crate::command::commands::api_server::event_bus::EventBus;
        use crate::data::execution_event::ExecutionEvent;

        let bus = EventBus::new(4096);
        let mut live = bus.subscribe();
        let mut log = bus.subscribe_log();
        let mut fe = ApiDispatchFrontend::new("exec prompt", &[], bus.sender()).deferring_done();
        let io = AgentFrontend::take_io(&mut fe);

        let stdout = io.stdout.clone();
        let stderr = io.stderr.clone();
        drop(io);
        let last = format!("last-{run}");
        let producer = tokio::spawn({
            let last = last.clone();
            async move {
                for i in 0..lines {
                    stdout.send(format!("line {i}\n").into_bytes()).unwrap();
                    if i % 7 == 0 {
                        stderr.send(format!("err {i}\n").into_bytes()).unwrap();
                        tokio::task::yield_now().await;
                    }
                }
                let tail = if unterminated_tail {
                    last
                } else {
                    format!("{last}\n")
                };
                // Split the tail across chunks so a partial line is buffered.
                let (head, rest) = tail.split_at(2);
                stdout.send(head.as_bytes().to_vec()).unwrap();
                stdout.send(rest.as_bytes().to_vec()).unwrap();
                // Senders drop here: the container's pipes closed on exit.
            }
        });
        producer.await.unwrap();
        fe.report_workflow_completed(&WorkflowOutcome::Completed);
        drop(fe);

        assert!(
            bus.wait_output_drained(Duration::from_secs(5)).await,
            "run {run}: drains must finish once the pipes close"
        );
        // Simulate the queue worker's single final status, after the drain.
        bus.sender().finish(EventPayload::CommandStatus {
            status: CommandStatusKind::Done,
            exit_code: Some(0),
            error: None,
            result: None,
        });
        bus.sender().emit(EventPayload::Done);

        let mut from_live: Vec<ExecutionEvent> = Vec::new();
        while let Ok(event) = live.try_recv() {
            from_live.push(event);
        }
        let mut from_log: Vec<ExecutionEvent> = Vec::new();
        while let Ok(event) = log.try_recv() {
            from_log.push(event);
        }
        for (name, events) in [("stream", from_live), ("events.log", from_log)] {
            let done = events
                .iter()
                .position(|e| matches!(e.payload, EventPayload::Done))
                .unwrap_or_else(|| panic!("run {run}: {name} has no Done"));
            assert_eq!(done, events.len() - 1, "run {run}: {name} Done is last");
            let last_stdout = events
                .iter()
                .rposition(|e| matches!(e.payload, EventPayload::StdoutLine(_)))
                .unwrap();
            assert!(
                matches!(&events[last_stdout].payload, EventPayload::StdoutLine(s) if *s == last),
                "run {run}: {name} last stdout line was {:?}",
                events[last_stdout].payload
            );
            assert!(last_stdout < done, "run {run}: {name} stdout before Done");
            let status = events
                .iter()
                .position(|e| matches!(e.payload, EventPayload::CommandStatus { .. }))
                .unwrap();
            let last_output = events
                .iter()
                .rposition(|e| {
                    matches!(
                        e.payload,
                        EventPayload::StdoutLine(_) | EventPayload::StderrLine(_)
                    )
                })
                .unwrap();
            assert!(
                last_output < status && status < done,
                "run {run}: {name} completion order"
            );
            let sequences: Vec<u64> = events.iter().map(|e| e.sequence).collect();
            let expected: Vec<u64> = (0..events.len() as u64).collect();
            assert_eq!(sequences, expected, "run {run}: {name} dense sequences");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stress_last_stdout_line_always_precedes_done_in_stream_and_log() {
        // Many short runs: the race window is the gap between the producer's
        // last write and the worker emitting Done.
        let mut runs = Vec::new();
        for run in 0..200 {
            runs.push(tokio::spawn(run_once(
                run,
                5 + run % 40,
                run.is_multiple_of(2),
            )));
        }
        for run in runs {
            run.await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn drain_barrier_times_out_when_a_pipe_never_closes() {
        use crate::command::commands::api_server::event_bus::EventBus;

        let bus = EventBus::new(16);
        let mut fe = ApiDispatchFrontend::new("exec prompt", &[], bus.sender());
        let io = AgentFrontend::take_io(&mut fe);
        // `io` keeps the senders alive, as a backend that leaks a pipe would.
        assert!(!bus.wait_output_drained(Duration::from_millis(50)).await);
        drop(io);
    }

    // ─── ReadyFrontend::{report_phase, report_summary} ─────────────────────
    //
    // Before WI 0125, `report_summary` was a no-op here — the API dropped the
    // whole `ReadySummary`. It now emits one `StatusMessage` per row, the
    // hint (if any), and the outcome line, through the same event path every
    // other ready/init progress line already uses.

    fn status_messages(
        rx: &mut tokio::sync::broadcast::Receiver<crate::data::execution_event::ExecutionEvent>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let crate::data::execution_event::EventPayload::StatusMessage { message, .. } =
                event.payload
            {
                out.push(message);
            }
        }
        out
    }

    #[test]
    fn report_phase_emits_a_status_message_with_the_shared_ready_phase_display() {
        let bus = crate::command::commands::api_server::event_bus::EventBus::new(16);
        let mut rx = bus.subscribe();
        let mut fe = ApiDispatchFrontend::new("ready", &[], bus.sender());

        ReadyFrontend::report_phase(&mut fe, &ReadyPhase::ImportingAgentImage);

        let messages = status_messages(&mut rx);
        assert!(
            messages.contains(&"ready: Importing agent image...".to_string()),
            "{messages:?}"
        );
    }

    /// `report_summary` was a no-op `fn report_summary(&mut self, _summary: &ReadySummary) {}`
    /// before WI 0125: this is the regression it fixed. Every row, the hint,
    /// and the outcome line must now reach the event stream.
    #[test]
    fn report_summary_emits_rows_hint_and_outcome_line() {
        let bus = crate::command::commands::api_server::event_bus::EventBus::new(16);
        let mut rx = bus.subscribe();
        let mut fe = ApiDispatchFrontend::new("ready", &[], bus.sender());

        let mut summary = ReadySummary::new("builtin");
        summary.dockerfile = StepStatus::Done;
        summary.base_image = StepStatus::Skipped;
        summary.agent_image = StepStatus::Failed("daemon unreachable".into());

        ReadyFrontend::report_summary(&mut fe, &summary);

        let messages = status_messages(&mut rx);
        assert!(
            !summary.is_ready(),
            "sanity: this summary must not be ready"
        );
        assert!(
            messages
                .iter()
                .any(|m| m == &format!("Dockerfile: {}", StepStatus::Done.label())),
            "every row must be reported: {messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.contains("daemon unreachable")),
            "the failed row's message must reach the event stream: {messages:?}"
        );
        assert!(
            messages.contains(
                &"Tip: complete the required image steps, then run \"awman ready\" again."
                    .to_string()
            ),
            "{messages:?}"
        );
        assert!(
            messages.contains(&"awman is not ready.".to_string()),
            "the outcome line must be the summary's own, last: {messages:?}"
        );
    }

    // ─── flag_bool ────────────────────────────────────────────────────────────

    #[test]
    fn flag_bool_bare_flag_is_true() {
        let f = make_frontend("chat", &["--yolo"]);
        assert_eq!(f.flag_bool(&["chat"], "yolo").unwrap(), Some(true));
    }

    #[test]
    fn flag_bool_presence_sets_true() {
        // Bools are `SetTrue` (clap parity): presence implies true; no value
        // token is consumed. `--background` is a real flag on `api start`.
        let f = make_frontend("api start", &["--background"]);
        assert_eq!(
            f.flag_bool(&["api", "start"], "background").unwrap(),
            Some(true)
        );
    }

    #[test]
    fn unknown_flag_surfaces_structured_error_from_accessor() {
        // Catalogue-driven parsing rejects an undeclared flag rather than
        // silently dropping it; the error surfaces on the first accessor call.
        let f = make_frontend("exec prompt", &["--not-a-real-flag"]);
        let err = f.flag_bool(&["exec", "prompt"], "yolo").unwrap_err();
        assert!(matches!(err, CommandError::UnknownFlag { .. }));
    }

    // ─── flag_string ──────────────────────────────────────────────────────────

    #[test]
    fn flag_string_parses_value_after_flag() {
        let f = make_frontend("exec prompt", &["--agent", "claude"]);
        assert_eq!(
            f.flag_string(&["exec", "prompt"], "agent")
                .unwrap()
                .as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn flag_string_parses_equals_syntax() {
        let f = make_frontend("exec prompt", &["--agent=claude"]);
        assert_eq!(
            f.flag_string(&["exec", "prompt"], "agent")
                .unwrap()
                .as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn flag_string_absent_returns_none() {
        let f = make_frontend("exec prompt", &[]);
        assert_eq!(f.flag_string(&["exec", "prompt"], "agent").unwrap(), None);
    }

    // ─── flag_u16 ─────────────────────────────────────────────────────────────

    #[test]
    fn flag_u16_parses_port_value() {
        let f = make_frontend("api start", &["--port", "9876"]);
        assert_eq!(f.flag_u16(&["api", "start"], "port").unwrap(), Some(9876));
    }

    // ─── argument (positional) ────────────────────────────────────────────────

    #[test]
    fn argument_exec_prompt_maps_positional_to_prompt() {
        let f = make_frontend("exec prompt", &["hello", "world"]);
        assert_eq!(
            f.argument(&["exec", "prompt"], "prompt")
                .unwrap()
                .as_deref(),
            Some("hello world")
        );
    }

    // ─── non-interactive and yolo flags are always set ────────────────────────

    #[test]
    fn non_interactive_flag_always_set() {
        let f = make_frontend("chat", &[]);
        assert_eq!(
            f.flag_bool(&["chat"], "non-interactive").unwrap(),
            Some(true),
            "non-interactive must always be set in API mode"
        );
    }

    #[test]
    fn yolo_flag_always_set() {
        let f = make_frontend("chat", &[]);
        assert_eq!(
            f.flag_bool(&["chat"], "yolo").unwrap(),
            Some(true),
            "yolo must always be set in API mode"
        );
    }

    // ─── flag_strings (multi-value) ───────────────────────────────────────────

    #[test]
    fn flag_strings_collects_multiple_values() {
        let f = make_frontend("api start", &["--workdirs", "/a", "--workdirs", "/b"]);
        let dirs = f.flag_strings(&["api", "start"], "workdirs").unwrap();
        assert!(dirs.contains(&"/a".to_string()));
        assert!(dirs.contains(&"/b".to_string()));
    }

    // ─── Drop emits Done and flushes partial lines ─────────────────────────────

    #[test]
    fn deferred_drop_flushes_output_and_leaves_done_to_worker() {
        let bus = crate::command::commands::api_server::event_bus::EventBus::new(16);
        let mut rx = bus.subscribe();
        let mut frontend =
            ApiDispatchFrontend::new("exec prompt", &[], bus.sender()).deferring_done();
        frontend.line_buffer_stdout = "trailing output".into();
        drop(frontend);
        assert!(matches!(rx.try_recv().unwrap().payload,
            EventPayload::StdoutLine(ref line) if line == "trailing output"));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        bus.sender().emit(EventPayload::Done);
        assert!(matches!(rx.try_recv().unwrap().payload, EventPayload::Done));
    }

    #[tokio::test]
    async fn drop_emits_done_sentinel_when_emit_done_was_not_called() {
        use crate::engine::agent_runtime::frontend::AgentFrontend;
        let bus = crate::command::commands::api_server::event_bus::EventBus::new(16);
        let mut rx = bus.subscribe();
        let mut fe = ApiDispatchFrontend::new("exec prompt", &[], bus.sender());
        let io = fe.take_io();
        io.stdout.send(b"a line\n".to_vec()).unwrap();
        drop(io);
        // Give the drain task a moment to process.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(fe);

        let line = rx.recv().await.unwrap();
        assert!(matches!(line.payload, EventPayload::StdoutLine(ref s) if s == "a line"));
        let done = rx.recv().await.unwrap();
        assert!(
            matches!(done.payload, EventPayload::Done),
            "Drop must emit Done; got {:?}",
            done.payload
        );
    }

    #[tokio::test]
    async fn drop_flushes_partial_stdout_line_before_done() {
        use crate::engine::agent_runtime::frontend::AgentFrontend;
        let bus = crate::command::commands::api_server::event_bus::EventBus::new(16);
        let mut rx = bus.subscribe();
        let mut fe = ApiDispatchFrontend::new("exec prompt", &[], bus.sender());
        let io = fe.take_io();
        // No trailing newline — the line lives in the drain task buffer until
        // the sender is dropped and the drain flushes.
        io.stdout.send(b"trailing partial".to_vec()).unwrap();
        drop(io);
        // Give the drain task a moment to process and flush.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(fe);

        let line = rx.recv().await.unwrap();
        assert!(
            matches!(line.payload, EventPayload::StdoutLine(ref s) if s == "trailing partial"),
            "partial stdout line must be flushed by drain task; got {:?}",
            line.payload
        );
        let done = rx.recv().await.unwrap();
        assert!(matches!(done.payload, EventPayload::Done));
    }

    #[tokio::test]
    async fn drop_flushes_partial_stderr_line_before_done() {
        use crate::engine::agent_runtime::frontend::AgentFrontend;
        let bus = crate::command::commands::api_server::event_bus::EventBus::new(16);
        let mut rx = bus.subscribe();
        let mut fe = ApiDispatchFrontend::new("exec prompt", &[], bus.sender());
        let io = fe.take_io();
        io.stderr.send(b"err partial".to_vec()).unwrap();
        drop(io);
        // Give the drain task a moment to process and flush.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(fe);

        let line = rx.recv().await.unwrap();
        assert!(
            matches!(line.payload, EventPayload::StderrLine(ref s) if s == "err partial"),
            "partial stderr line must be flushed by drain task; got {:?}",
            line.payload
        );
        let done = rx.recv().await.unwrap();
        assert!(matches!(done.payload, EventPayload::Done));
    }

    #[tokio::test]
    async fn explicit_emit_done_then_drop_does_not_double_emit() {
        let bus = crate::command::commands::api_server::event_bus::EventBus::new(16);
        let mut rx = bus.subscribe();
        let mut fe = ApiDispatchFrontend::new("exec prompt", &[], bus.sender());
        fe.emit_done();
        drop(fe);

        let done = rx.recv().await.unwrap();
        assert!(matches!(done.payload, EventPayload::Done));
        // Second recv must time out — no second Done.
        let again = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(
            again.is_err(),
            "Drop must NOT emit a second Done after explicit emit_done; got {:?}",
            again
        );
    }

    // ── yolo countdown message throttle ──────────────────────────────────────

    fn count_countdown_events(
        rx: &mut tokio::sync::broadcast::Receiver<crate::data::execution_event::ExecutionEvent>,
    ) -> usize {
        let mut count = 0;
        while let Ok(evt) = rx.try_recv() {
            if matches!(
                &evt.payload,
                EventPayload::StatusMessage { phase, .. } if phase == "yolo_countdown"
            ) {
                count += 1;
            }
        }
        count
    }

    /// Collect the `yolo_countdown` status messages emitted so far.
    fn countdown_messages(
        rx: &mut tokio::sync::broadcast::Receiver<crate::data::execution_event::ExecutionEvent>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            if let EventPayload::StatusMessage { phase, message } = &evt.payload {
                if phase == "yolo_countdown" {
                    out.push(message.clone());
                }
            }
        }
        out
    }

    /// WI-0115 §3: a failure retry and a stuck-step advance share one reporting
    /// channel, but they mean opposite things. An API consumer told a failing
    /// run is "auto-advancing" would conclude the workflow is making progress.
    #[tokio::test]
    async fn a_failure_retry_countdown_is_not_reported_as_auto_advancing() {
        use crate::engine::workflow::frontend::WorkflowFrontend as _;
        let bus = crate::command::commands::api_server::event_bus::EventBus::new(64);
        let mut rx = bus.subscribe();
        let mut fe = ApiDispatchFrontend::new("exec workflow", &[], bus.sender());

        fe.yolo_countdown_started("build", CountdownKind::FailureRetry);
        fe.yolo_countdown_tick(
            "build",
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let messages = countdown_messages(&mut rx);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("retrying after failure"),
            "a retry must say so: {}",
            messages[0]
        );

        // And the kind resets, so the next stuck-step countdown reads normally.
        fe.yolo_countdown_finished("build");
        fe.yolo_countdown_tick(
            "test",
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let messages = countdown_messages(&mut rx);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("auto-advancing"),
            "a stuck-step countdown must keep its own wording: {}",
            messages[0]
        );
    }

    /// Ten rapid ticks must produce exactly one `yolo_countdown` status message
    /// because all subsequent calls fall within the 10-second throttle window.
    #[tokio::test]
    async fn yolo_countdown_throttles_api_messages_within_window() {
        use crate::engine::workflow::frontend::WorkflowFrontend as _;
        let bus = crate::command::commands::api_server::event_bus::EventBus::new(64);
        let mut rx = bus.subscribe();
        let mut fe = ApiDispatchFrontend::new("exec workflow", &[], bus.sender());

        for i in 0..10u64 {
            fe.yolo_countdown_tick(
                "step",
                std::time::Duration::from_secs(60u64.saturating_sub(i)),
                std::time::Duration::from_secs(60),
            )
            .unwrap();
        }

        // Allow any async tasks to flush.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let count = count_countdown_events(&mut rx);
        assert_eq!(
            count, 1,
            "only the first tick should emit within the 10-second throttle window"
        );
    }

    /// After `yolo_countdown_finished` resets the timer, the very next tick
    /// must emit a fresh message.
    #[tokio::test]
    async fn yolo_countdown_tick_emits_again_after_finished_resets_timer() {
        use crate::engine::workflow::frontend::WorkflowFrontend as _;
        let bus = crate::command::commands::api_server::event_bus::EventBus::new(64);
        let mut rx = bus.subscribe();
        let mut fe = ApiDispatchFrontend::new("exec workflow", &[], bus.sender());

        // First tick: emits.
        fe.yolo_countdown_tick(
            "step",
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
        )
        .unwrap();
        // Rapid second tick: throttled.
        fe.yolo_countdown_tick(
            "step",
            std::time::Duration::from_secs(59),
            std::time::Duration::from_secs(60),
        )
        .unwrap();

        // Reset.
        fe.yolo_countdown_finished("step");

        // Next tick after reset: must emit again.
        fe.yolo_countdown_tick(
            "step",
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
        )
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let count = count_countdown_events(&mut rx);
        assert_eq!(
            count, 2,
            "expect 2 events: initial tick + tick after countdown_finished reset"
        );
    }

    /// Simulating an elapsed throttle window by rewinding `last_sink_message_time`
    /// makes the next tick emit a new message.
    #[tokio::test]
    async fn yolo_countdown_tick_emits_after_throttle_window_elapses() {
        use crate::engine::workflow::frontend::WorkflowFrontend as _;
        let bus = crate::command::commands::api_server::event_bus::EventBus::new(64);
        let mut rx = bus.subscribe();
        let mut fe = ApiDispatchFrontend::new("exec workflow", &[], bus.sender());

        // First tick emits.
        fe.yolo_countdown_tick(
            "step",
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
        )
        .unwrap();

        // Rewind the throttle timestamp to simulate 11 seconds having passed.
        fe.last_sink_message_time =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(11));

        // Next tick must emit (window elapsed).
        fe.yolo_countdown_tick(
            "step",
            std::time::Duration::from_secs(58),
            std::time::Duration::from_secs(60),
        )
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let count = count_countdown_events(&mut rx);
        assert_eq!(
            count, 2,
            "expected 2 events: initial tick + tick after simulated 11-second gap"
        );
    }
    #[test]
    fn worktree_preparation_and_resume_record_structured_result_path() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::data::fs::api_paths::ApiPaths::at_root(tmp.path());
        let writer = CommandStateWriter::new(paths, "s", "c");
        let bus = crate::command::commands::api_server::event_bus::EventBus::new(16);
        let mut frontend = ApiDispatchFrontend::new("exec workflow", &[], bus.sender())
            .with_state_writer(writer.clone());
        for resumed in [false, true] {
            let wt = tmp.path().join(if resumed { "resumed" } else { "created" });
            std::fs::create_dir(&wt).unwrap();
            if resumed {
                frontend.report_worktree_resumed(&wt, "branch");
            } else {
                frontend.report_worktree_created(&wt, "branch");
            }
            assert_eq!(writer.kept_worktree(), Some(wt));
        }
        // A later checkout may reuse the same directory before completion;
        // explicit cleanup must clear this command's ownership of the path.
        frontend.report_worktree_discarded("branch");
        assert!(writer.kept_worktree().is_none());
    }
}

#[cfg(test)]
mod config_migrate_tests {
    use super::*;

    /// `config migrate` is not API-allowed, but if its frontend trait is ever
    /// reached the API must refuse to confirm rather than guess: the answer
    /// is the same as the CLI's `--non-interactive` without `--yes`.
    #[test]
    fn api_frontend_never_confirms_a_config_migration() {
        let bus = crate::command::commands::api_server::event_bus::EventBus::new(16);
        let mut fe = ApiDispatchFrontend::new("config migrate", &[], bus.sender());
        let prompt = crate::command::prompts::config_migrate_confirm("PREVIEW");
        let err = fe
            .confirm_config_migration(&prompt)
            .expect_err("the API cannot answer a confirmation");
        assert!(
            matches!(
                err,
                CommandError::InteractiveInputUnavailable { ref prompt } if prompt == "yes"
            ),
            "{err:?}"
        );
    }
}
