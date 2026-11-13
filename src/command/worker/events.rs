//! Persistence and ordered event publication share one command-layer boundary.
use crate::command::commands::api_server::event_bus::EventBusSender;
use crate::data::execution_event::EventPayload;
use crate::data::fs::api_command_log::CommandStateWriter;
use crate::data::workflow_state::WorkflowState;

pub fn persist_workflow_snapshot(
    writer: &CommandStateWriter,
    events: &EventBusSender,
    state: &WorkflowState,
) {
    if let Err(error) = crate::data::context::CommandInvocationPins::new(writer.paths().clone())
        .pin(
            writer.session_id(),
            writer.command_id(),
            events.attempt(),
            state.invocation_id,
        )
    {
        tracing::warn!(%error, "failed to pin command workflow invocation");
    }
    let mut value = match serde_json::to_value(state) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "failed to project command workflow state");
            return;
        }
    };
    events.redaction().redact_json(&mut value);
    match writer.write(&value) {
        Ok(()) => events.emit(EventPayload::WorkflowState(
            crate::command::daemon_api::workflow_view::client_view(value),
        )),
        Err(error) => tracing::warn!(%error, "failed to write command workflow state"),
    }
}

/// Foreground frontends keep their usual presentation while publishing the
/// worker's persisted state and result contributions through this recorder.
#[derive(Clone)]
pub struct ForegroundRecorder {
    source: std::sync::Arc<
        std::sync::Mutex<Option<crate::engine::workflow::frontend::ContainerSource>>,
    >,
    pub events: EventBusSender,
    pub writer: CommandStateWriter,
    final_message: bool,
    format: std::sync::Arc<std::sync::Mutex<crate::data::agent_output_format::AgentOutputFormat>>,
    phase_attempts:
        std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<(String, usize), u32>>>,
    phase: std::sync::Arc<std::sync::Mutex<crate::data::workflow_state::WorkflowPhase>>,
}
impl ForegroundRecorder {
    pub fn new(events: EventBusSender, writer: CommandStateWriter, final_message: bool) -> Self {
        Self {
            source: Default::default(),
            final_message,
            format: std::sync::Arc::new(std::sync::Mutex::new(
                crate::data::agent_output_format::AgentOutputFormat::Text,
            )),
            events,
            writer,
            phase_attempts: Default::default(),
            phase: std::sync::Arc::new(std::sync::Mutex::new(
                crate::data::workflow_state::WorkflowPhase::Setup,
            )),
        }
    }
    pub fn state(&self, state: &WorkflowState) {
        if state.current_phase == crate::data::workflow_state::WorkflowPhase::Main {
            self.writer.record_workflow_main_started();
        }
        use crate::data::{
            fs::api_db::{FailedStep, FailedStepPhase},
            workflow_state::{PhaseKind, PhaseStepStatus, StepState},
        };
        *self.phase.lock().unwrap() = state.current_phase;
        for kind in [PhaseKind::Setup, PhaseKind::Teardown] {
            for (index, step) in state.phase_step_states(kind).iter().enumerate() {
                let key = (kind.label().to_string(), index);
                let phase = if kind == PhaseKind::Setup {
                    FailedStepPhase::Setup
                } else {
                    FailedStepPhase::Teardown
                };
                match &step.status {
                    PhaseStepStatus::Remediating { attempt, .. } => {
                        self.phase_attempts.lock().unwrap().insert(key, attempt + 1);
                    }
                    PhaseStepStatus::Failed { error } => {
                        self.writer.record_failed_step(FailedStep {
                            name: step.description.clone(),
                            index,
                            phase,
                            exit_code: None,
                            step_attempt: *self
                                .phase_attempts
                                .lock()
                                .unwrap()
                                .get(&key)
                                .unwrap_or(&1),
                            error: Some(error.clone()),
                        })
                    }
                    PhaseStepStatus::Succeeded => self.writer.record_step_succeeded(phase, index),
                    _ => {}
                }
            }
        }
        for (index, step) in state.steps.iter().enumerate() {
            match state.step_states.get(&step.name) {
                Some(StepState::Failed {
                    exit_code,
                    error_message,
                }) => self.writer.record_failed_step(FailedStep {
                    name: step.name.clone(),
                    index,
                    phase: FailedStepPhase::Main,
                    exit_code: Some(*exit_code),
                    step_attempt: *state.step_attempts.get(&step.name).unwrap_or(&1),
                    error: error_message.clone(),
                }),
                Some(StepState::Succeeded) => self
                    .writer
                    .record_step_succeeded(FailedStepPhase::Main, index),
                _ => {}
            }
        }
        persist_workflow_snapshot(&self.writer, &self.events, state);
    }
    pub fn completed(&self, outcome: &crate::engine::workflow::actions::WorkflowOutcome) {
        use crate::{
            data::{
                execution_event::CommandStatusKind as Status,
                fs::api_db::{failure_codes, CommandFailure},
                workflow_state::WorkflowPhase,
            },
            engine::workflow::actions::WorkflowOutcome as Outcome,
        };
        let (status, failure) = match outcome {
            Outcome::Completed => (Status::Done, None),
            Outcome::Paused => (Status::Paused, None),
            Outcome::Aborted => (Status::Aborted, None),
            Outcome::Cancelled => (
                Status::Cancelled,
                Some(CommandFailure {
                    stage: match *self.phase.lock().unwrap() {
                        WorkflowPhase::Setup => "setup",
                        WorkflowPhase::Teardown => "teardown",
                        _ => "step",
                    }
                    .into(),
                    code: failure_codes::CANCELLED.into(),
                    message: "cancelled".into(),
                    step: None,
                }),
            ),
            Outcome::CompletedTeardownFailed => (
                Status::Error,
                Some(CommandFailure {
                    stage: "teardown".into(),
                    code: failure_codes::TEARDOWN_FAILED.into(),
                    message: "Teardown failed".into(),
                    step: None,
                }),
            ),
            Outcome::Failed { last_step, .. } => (
                Status::Error,
                Some(CommandFailure {
                    stage: "step".into(),
                    code: failure_codes::STEP_FAILED.into(),
                    message: format!("Step '{last_step}' failed"),
                    step: Some(last_step.clone()),
                }),
            ),
        };
        self.writer.record_workflow_outcome(status);
        self.writer.record_workflow_failure(failure);
    }
    pub fn output_format(&self, format: crate::data::agent_output_format::AgentOutputFormat) {
        *self.format.lock().unwrap() = format;
    }
    pub fn container_source(
        &self,
        source: Option<crate::engine::workflow::frontend::ContainerSource>,
    ) {
        *self.source.lock().unwrap() = source;
    }
    fn translated(&self, items: Vec<crate::engine::agent_output::Translated>) {
        use crate::engine::agent_output::{OutputStream, Translated};
        for item in items {
            match item {
                Translated::Line {
                    stream: OutputStream::Stdout,
                    text,
                } => self.events.emit(EventPayload::StdoutLine(text)),
                Translated::Line {
                    stream: OutputStream::Stderr,
                    text,
                } => self.events.emit(EventPayload::StderrLine(text)),
                Translated::Activity { kind, summary } => self
                    .events
                    .emit(EventPayload::StepActivity { kind, summary }),
                Translated::FinalMessage(message) if self.final_message => {
                    self.writer.record_final_message(message)
                }
                Translated::FinalMessage(_) => {}
            }
        }
    }
    /// Tee byte channels without taking over terminal presentation. Each
    /// reader is tracked by the same output barrier as pushed executions.
    pub fn io(
        &self,
        mut io: crate::engine::agent_runtime::frontend::AgentIo,
    ) -> crate::engine::agent_runtime::frontend::AgentIo {
        for (stream, stderr) in [(&mut io.stdout, false), (&mut io.stderr, true)] {
            let destination = stream.clone();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            *stream = tx;
            let events = self.events.clone();
            let mut recorder = self.clone();
            if let Some(source) = self.source.lock().unwrap().clone() {
                recorder.events = self.events.with_shared_source(source);
            }
            let format = if stderr {
                crate::data::agent_output_format::AgentOutputFormat::Text
            } else {
                *self.format.lock().unwrap()
            };
            self.events.spawn_drain(async move {
                let mut translator = crate::engine::agent_output::translator_for_redacted(
                    format,
                    events.redaction(),
                );
                let stream = if stderr {
                    crate::engine::agent_output::OutputStream::Stderr
                } else {
                    crate::engine::agent_output::OutputStream::Stdout
                };
                use crate::engine::agent_output::{
                    line_framer::{
                        FramedLine, LineFramer, MAX_LINE_BYTES, MAX_STRUCTURED_FRAME_BYTES,
                    },
                    Translated,
                };
                let mut framer = LineFramer::new(if format.is_structured() {
                    MAX_STRUCTURED_FRAME_BYTES
                } else {
                    MAX_LINE_BYTES
                });
                let mut process = |line: FramedLine| {
                    let items = if line.truncated && format.is_structured() {
                        translator = crate::engine::agent_output::translator_for_redacted(
                            format,
                            events.redaction(),
                        );
                        vec![Translated::Activity {
                            kind: crate::data::execution_event::StepActivityKind::Message,
                            summary: "Agent frame too large; final message may be unavailable"
                                .into(),
                        }]
                    } else if line.truncated {
                        vec![Translated::Line {
                            stream,
                            text: line.marked_lossy_text(),
                        }]
                    } else {
                        translator.feed_line(stream, &line.lossy_text())
                    };
                    recorder.translated(items);
                };
                let stop = events.output_shutdown();
                loop {
                    let bytes =
                        tokio::select! { _ = stop.cancelled() => None, bytes = rx.recv() => bytes };
                    let Some(bytes) = bytes else {
                        break;
                    };
                    for line in framer.push(&bytes) {
                        process(line);
                    }
                    let _ = destination.send(bytes);
                }
                if let Some(line) = framer.finish() {
                    process(line);
                }
                recorder.translated(translator.finish());
            });
        }
        io
    }
}

/// The prompt/init accessors return a detached agent frontend. Preserve worker
/// recording on that path as well as on workflow's shared frontend proxy.
pub fn recording_frontend(
    inner: Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend>,
    recorder: Option<ForegroundRecorder>,
) -> Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend> {
    match recorder {
        Some(recorder) => Box::new(RecordedFrontend { inner, recorder }),
        None => inner,
    }
}
struct RecordedFrontend {
    inner: Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend>,
    recorder: ForegroundRecorder,
}
impl crate::data::message::UserMessageSink for RecordedFrontend {
    fn write_message(&mut self, message: crate::data::message::UserMessage) {
        self.inner.write_message(message);
    }
    fn replay_queued(&mut self) {
        self.inner.replay_queued();
    }
}
impl crate::engine::agent_runtime::frontend::AgentFrontend for RecordedFrontend {
    fn terminal_launch(&self) -> Option<crate::engine::agent_runtime::execution::TerminalLaunch> {
        self.inner.terminal_launch()
    }
    fn terminal_source(&self) -> Option<crate::data::execution_event::EventSource> {
        self.inner.terminal_source()
    }
    fn program_status_config(
        &self,
    ) -> crate::engine::agent_runtime::program_status::ProgramStatusBridgeConfig {
        self.inner.program_status_config()
    }
    fn report_status(&mut self, status: crate::engine::agent_runtime::frontend::AgentStatus) {
        if matches!(
            status,
            crate::engine::agent_runtime::frontend::AgentStatus::Running { .. }
        ) {
            self.recorder.writer.record_container_started();
        }
        self.inner.report_status(status);
    }
    fn report_progress(&mut self, progress: crate::engine::agent_runtime::frontend::AgentProgress) {
        self.inner.report_progress(progress);
    }
    fn set_agent_output_format(
        &mut self,
        format: crate::data::agent_output_format::AgentOutputFormat,
    ) {
        self.recorder.output_format(format);
        self.inner.set_agent_output_format(format);
    }
    fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
        self.recorder.io(self.inner.take_io())
    }
    fn grace_timeout(&self) -> std::time::Duration {
        self.inner.grace_timeout()
    }
    fn stuck_timeout(&self) -> std::time::Duration {
        self.inner.stuck_timeout()
    }
}
