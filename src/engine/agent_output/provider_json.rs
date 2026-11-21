//! Provider structured formats with separate definition-selected contracts.
//! Unknown records are ignored; only explicit native terminal records finish
//! a turn. Additive provider fields never become text-screen heuristics.
use super::*;
use serde_json::Value;
pub struct Translator {
    candidate: String,
    format: AgentOutputFormat,
    observation: NativeObservation,
}
impl Translator {
    pub fn new(format: AgentOutputFormat) -> Self {
        Self {
            candidate: String::new(),
            format,
            observation: Default::default(),
        }
    }
}
impl AgentOutputTranslator for Translator {
    fn native_observation(&self) -> NativeObservation {
        self.observation.clone()
    }
    fn feed_line(&mut self, stream: OutputStream, line: &str) -> Vec<Translated> {
        if stream == OutputStream::Stderr {
            return passthrough(stream, line);
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return passthrough(stream, line);
        };
        let kind = v.get("type").and_then(Value::as_str).unwrap_or_default();
        let (id, done) = match self.format {
            AgentOutputFormat::OpenCodeJson | AgentOutputFormat::OpenCodeV2Json => (
                v.get("sessionID")
                    .or_else(|| v.pointer("/properties/sessionID")),
                kind == "step_finish"
                    && v.pointer("/part/reason").and_then(Value::as_str) == Some("stop"),
            ),
            AgentOutputFormat::CopilotJson => (
                v.pointer("/data/sessionId").or_else(|| v.get("sessionId")),
                kind == "session.idle",
            ),
            AgentOutputFormat::ClineJson => (
                v.get("taskId")
                    .or_else(|| v.get("id").filter(|_| kind == "task_started")),
                kind == "task_completed",
            ),
            AgentOutputFormat::AntigravityStreamJson => (
                v.get("conversation_id").filter(|_| kind == "init"),
                kind == "result",
            ),
            _ => (None, false),
        };
        if let Some(id) = id
            .and_then(Value::as_str)
            .and_then(|s| crate::data::settings_profile::NativeSessionId::parse(s).ok())
        {
            self.observation.identity = Some(id)
        }
        if kind == "error" || v.get("is_error").and_then(Value::as_bool) == Some(true) {
            self.observation.terminal(false, None);
            return vec![activity(StepActivityKind::Message, "agent failed")];
        }
        if done {
            let text = v
                .get("result")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| std::mem::take(&mut self.candidate));
            self.observation.terminal(true, Some(text.clone()));
            return final_message(text);
        }
        if let Some(text) = v
            .get("text")
            .or_else(|| v.pointer("/part/text"))
            .or_else(|| v.pointer("/data/content"))
            .and_then(Value::as_str)
        {
            if self.candidate.len().saturating_add(text.len()) > 4 * 1024 * 1024 {
                self.candidate.clear();
                self.observation.protocol("answer exceeded its bound");
                return Vec::new();
            }
            self.candidate.push_str(text);
            return vec![activity(StepActivityKind::Message, text)];
        }
        Vec::new()
    }
    fn finish(&mut self) -> Vec<Translated> {
        if !self.observation.completed && !self.observation.failed && !self.candidate.is_empty() {
            return final_message(std::mem::take(&mut self.candidate));
        }
        Vec::new()
    }
}
