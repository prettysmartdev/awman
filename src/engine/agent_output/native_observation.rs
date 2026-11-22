//! Reuse the native translator at the execution I/O boundary. A profiled
//! headless run parses each stdout frame once, then presents translated rows
//! to API frontends or ordinary text lines to CLI/TUI frontends.
use super::*;
use crate::data::settings_profile::NativeSessionId;
use crate::engine::agent_runtime::frontend::*;
use std::sync::{Arc, Mutex};
#[derive(Clone, Default)]
pub struct NativeStream {
    inner: Arc<Mutex<State>>,
    pub mismatch: tokio_util::sync::CancellationToken,
}
#[derive(Default)]
struct State {
    requested: Option<NativeSessionId>,
    identity: Option<NativeSessionId>,
    complete: bool,
    failed: bool,
    ended: bool,
    final_message: Option<String>,
    protocol_error: Option<String>,
}
impl NativeStream {
    pub fn new(requested: Option<NativeSessionId>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(State {
                requested,
                ..Default::default()
            })),
            ..Default::default()
        }
    }
    fn update(&self, o: NativeObservation) {
        let mut state = self.inner.lock().unwrap();
        if let Some(id) = o.identity {
            if state
                .requested
                .as_ref()
                .is_some_and(|expected| expected != &id)
                || state
                    .identity
                    .as_ref()
                    .is_some_and(|previous| previous != &id)
            {
                self.mismatch.cancel();
            }
            state.identity = Some(id);
        }
        state.complete = o.completed;
        state.failed = o.failed;
        state.final_message = o.final_message;
        state.protocol_error = o.protocol_error;
    }
    fn authorized(&self) -> bool {
        let s = self.inner.lock().unwrap();
        s.requested.is_none() || s.requested == s.identity
    }
    pub fn identity(&self) -> Option<NativeSessionId> {
        self.inner.lock().unwrap().identity.clone()
    }
    /// The provider reported a failed terminal outcome.
    pub fn failed(&self) -> bool {
        self.inner.lock().unwrap().failed
    }
    /// The stream ended (the translator finished).
    pub fn ended(&self) -> bool {
        self.inner.lock().unwrap().ended
    }
    pub fn complete(&self) -> bool {
        let s = self.inner.lock().unwrap();
        s.ended
            && s.complete
            && !s.failed
            && s.protocol_error.is_none()
            && !self.mismatch.is_cancelled()
    }
    /// A malformed or conflicting terminal record (latched by the translator).
    pub fn protocol_error(&self) -> Option<String> {
        self.inner.lock().unwrap().protocol_error.clone()
    }
    /// The answer carried by the confirmed terminal success record.
    pub fn final_message(&self) -> Option<String> {
        let s = self.inner.lock().unwrap();
        s.complete.then(|| s.final_message.clone()).flatten()
    }
    pub fn exact_resume_confirmed(&self) -> bool {
        let s = self.inner.lock().unwrap();
        !self.mismatch.is_cancelled()
            && (s.requested.is_none() || (s.ended && s.requested == s.identity))
    }
}
pub fn with_native_observation(
    frontend: Box<dyn AgentFrontend>,
    format: AgentOutputFormat,
    stream: NativeStream,
) -> Box<dyn AgentFrontend> {
    Box::new(ObservedFrontend {
        frontend: Arc::new(Mutex::new(frontend)),
        format,
        stream,
    })
}
struct ObservedFrontend {
    frontend: Arc<Mutex<Box<dyn AgentFrontend>>>,
    format: AgentOutputFormat,
    stream: NativeStream,
}
impl crate::data::message::UserMessageSink for ObservedFrontend {
    fn write_message(&mut self, m: crate::data::message::UserMessage) {
        self.frontend.lock().unwrap().write_message(m)
    }
    fn replay_queued(&mut self) {
        self.frontend.lock().unwrap().replay_queued()
    }
}
#[async_trait::async_trait]
impl AgentFrontend for ObservedFrontend {
    fn output_redaction(&self) -> crate::engine::auth::redaction::RedactionSet {
        self.frontend.lock().unwrap().output_redaction()
    }
    fn terminal_launch(&self) -> Option<crate::engine::agent_runtime::execution::TerminalLaunch> {
        self.frontend.lock().unwrap().terminal_launch()
    }
    fn terminal_source(&self) -> Option<crate::data::execution_event::EventSource> {
        self.frontend.lock().unwrap().terminal_source()
    }
    fn program_status_config(
        &self,
    ) -> crate::engine::agent_runtime::program_status::ProgramStatusBridgeConfig {
        self.frontend.lock().unwrap().program_status_config()
    }
    fn grace_timeout(&self) -> std::time::Duration {
        self.frontend.lock().unwrap().grace_timeout()
    }
    fn startup_timing(&self) -> crate::data::automation::StartupTiming {
        self.frontend.lock().unwrap().startup_timing()
    }
    fn stuck_timeout(&self) -> std::time::Duration {
        self.frontend.lock().unwrap().stuck_timeout()
    }
    fn set_agent_output_format(&mut self, format: AgentOutputFormat) {
        self.format = format;
        self.frontend
            .lock()
            .unwrap()
            .set_agent_output_format(AgentOutputFormat::Text)
    }
    fn report_status(&mut self, s: AgentStatus) {
        self.frontend.lock().unwrap().report_status(s)
    }
    fn report_progress(&mut self, p: AgentProgress) {
        self.frontend.lock().unwrap().report_progress(p)
    }
    fn take_io(&mut self) -> AgentIo {
        self.frontend
            .lock()
            .unwrap()
            .set_agent_output_format(AgentOutputFormat::Text);
        let mut io = self.frontend.lock().unwrap().take_io();
        let downstream = io.stdout;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        io.stdout = tx;
        let frontend = self.frontend.clone();
        let format = self.format;
        let stream = self.stream.clone();
        let redaction = self.output_redaction();
        tokio::spawn(async move {
            let mut translator = translator_for_redacted(format, redaction);
            let mut framer = line_framer::LineFramer::new(line_framer::MAX_STRUCTURED_FRAME_BYTES);
            let emit = |items: Vec<Translated>| {
                if stream.mismatch.is_cancelled() || !stream.authorized() {
                    return;
                }
                for item in items {
                    if !frontend.lock().unwrap().translated_output(item.clone()) {
                        if let Translated::Line { text, .. } = item {
                            let _ = downstream.send(format!("{text}\n").into_bytes());
                        }
                    }
                }
            };
            while let Some(bytes) = rx.recv().await {
                for line in framer.push(&bytes) {
                    if line.truncated {
                        stream.mismatch.cancel();
                        continue;
                    }
                    let items = translator.feed_line(OutputStream::Stdout, &line.lossy_text());
                    stream.update(translator.native_observation());
                    emit(items);
                }
            }
            if let Some(line) = framer.finish() {
                if !line.truncated {
                    let items = translator.feed_line(OutputStream::Stdout, &line.lossy_text());
                    stream.update(translator.native_observation());
                    emit(items);
                }
            }
            emit(translator.finish());
            stream.inner.lock().unwrap().ended = true;
        });
        io
    }
}

#[cfg(test)]
mod strict_outcome_tests {
    use super::*;

    fn observe(format: AgentOutputFormat, lines: &[&str]) -> NativeObservation {
        let mut t = translator_for(format);
        for line in lines {
            t.feed_line(OutputStream::Stdout, line);
        }
        t.finish();
        t.native_observation()
    }

    /// WI 0070 automation finding 14: a known failure is latched, a later
    /// duplicate or malformed terminal record never turns it into success,
    /// and a success needs a valid terminal schema carrying the answer.
    #[test]
    fn terminal_outcomes_latch_and_need_a_valid_schema() {
        for format in [
            AgentOutputFormat::ClaudeStreamJson,
            AgentOutputFormat::CursorStreamJson,
        ] {
            let init = r#"{"type":"system","subtype":"init","session_id":"s-1"}"#;
            let o = observe(
                format,
                &[
                    init,
                    r#"{"type":"result","is_error":true}"#,
                    r#"{"type":"result"}"#,
                ],
            );
            assert!(o.failed && !o.completed, "{format:?}: {o:?}");
            let o = observe(format, &[init, r#"{"type":"result"}"#]);
            assert!(
                !o.completed && o.protocol_error.is_some(),
                "{format:?}: {o:?}"
            );
            let o = observe(
                format,
                &[
                    init,
                    r#"{"type":"result","is_error":false,"result":"done"}"#,
                ],
            );
            assert!(o.completed && !o.failed, "{format:?}: {o:?}");
            assert_eq!(o.final_message.as_deref(), Some("done"));
            // Conflicting terminal records are a protocol error.
            let o = observe(
                format,
                &[
                    init,
                    r#"{"type":"result","is_error":false,"result":"done"}"#,
                    r#"{"type":"result","is_error":true}"#,
                ],
            );
            assert!(
                !o.completed && o.protocol_error.is_some(),
                "{format:?}: {o:?}"
            );
        }
        let o = observe(
            AgentOutputFormat::CodexJson,
            &[
                r#"{"type":"thread.started","thread_id":"t-1"}"#,
                r#"{"type":"turn.failed"}"#,
                r#"{"type":"turn.completed"}"#,
            ],
        );
        assert!(o.failed && !o.completed, "{o:?}");
    }

    #[test]
    fn the_stream_exposes_the_confirmed_answer_only_after_a_valid_end() {
        let stream = NativeStream::new(None);
        let mut o = NativeObservation::default();
        o.terminal(true, Some("answer".into()));
        stream.update(o);
        assert_eq!(stream.final_message(), Some("answer".into()));
        assert!(!stream.complete(), "not complete before the stream ended");
        stream.inner.lock().unwrap().ended = true;
        assert!(stream.complete());
        let mut o = NativeObservation::default();
        o.protocol("bad");
        stream.update(o);
        assert!(!stream.complete());
        assert!(stream.protocol_error().is_some());
    }
}
