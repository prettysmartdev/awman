//! Claude Code `--print --output-format stream-json --verbose` NDJSON.
use std::collections::HashMap;

use serde_json::Value;

use super::{
    activity, final_message, passthrough, AgentOutputTranslator, OutputStream, StepActivityKind,
    Translated,
};

#[derive(Default)]
pub struct Translator {
    observation: super::NativeObservation,
    tools: HashMap<String, String>,
    candidate: Option<String>,
    finalized: bool,
}

impl Translator {
    fn translate(&mut self, event: &Value) -> Option<Vec<Translated>> {
        let kind = event.get("type").and_then(Value::as_str);
        let identity = match kind {
            Some("system") if event.get("subtype").and_then(Value::as_str) == Some("init") => {
                event.get("session_id")
            }
            _ => None,
        };
        if let Some(id) = identity
            .and_then(Value::as_str)
            .and_then(|s| crate::data::settings_profile::NativeSessionId::parse(s).ok())
        {
            self.observation.identity = Some(id);
        }
        if kind == Some("result") {
            // A terminal result needs its schema: an error flag or error
            // subtype, or (for success) the answer text. Later records cannot
            // reset an outcome.
            let errored = event.get("is_error").and_then(Value::as_bool) == Some(true)
                || event
                    .get("subtype")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.starts_with("error"));
            match (errored, event.get("result").and_then(Value::as_str)) {
                (true, _) => self.observation.terminal(false, None),
                (false, Some(answer)) => self.observation.terminal(true, Some(answer.to_owned())),
                _ => self
                    .observation
                    .protocol("terminal result record without a valid schema"),
            }
        }

        match event.get("type")?.as_str()? {
            "system" if event.get("subtype").and_then(Value::as_str) == Some("init") => {
                Some(vec![])
            }
            "assistant" | "user" => {
                let blocks = event.get("message")?.get("content")?.as_array()?;
                let mut items = vec![];
                let mut text = vec![];
                for block in blocks {
                    match block["type"].as_str()? {
                        "text" => {
                            let value = block["text"].as_str()?;
                            text.push(value);
                            items.push(activity(StepActivityKind::Message, value));
                        }
                        "thinking" | "redacted_thinking" => {
                            items.push(activity(StepActivityKind::Thinking, "thinking"));
                        }
                        "tool_use" => {
                            let name = block["name"].as_str()?;
                            if let Some(id) = block.get("id").and_then(Value::as_str) {
                                self.tools.insert(id.to_owned(), name.to_owned());
                            }
                            // Name and a selected hint only; never serialize the input object.
                            let hint = ["file_path", "path", "command", "pattern", "query"]
                                .iter()
                                .find_map(|key| block.get("input")?.get(key)?.as_str());
                            let summary = hint
                                .map_or_else(|| name.to_owned(), |hint| format!("{name}: {hint}"));
                            items.push(activity(StepActivityKind::ToolUse, &summary));
                        }
                        "tool_result" => {
                            let id = block["tool_use_id"].as_str()?;
                            let name = self.tools.remove(id).unwrap_or_else(|| id.to_owned());
                            // Report size/status, never echo a tool's potentially huge payload.
                            let bytes = block.get("content").map_or(0, content_bytes);
                            let status =
                                if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                                    "error"
                                } else {
                                    "completed"
                                };
                            items.push(activity(
                                StepActivityKind::ToolResult,
                                &format!("{name}: {status} ({bytes} bytes)"),
                            ));
                        }
                        _ => items.push(activity(
                            StepActivityKind::Message,
                            "unrecognised content block",
                        )),
                    }
                }
                if event["type"] == "assistant" && !text.is_empty() && !self.finalized {
                    self.candidate = Some(text.join("\n"));
                }
                Some(items)
            }
            "result" => {
                if self.finalized {
                    return Some(vec![]);
                }
                self.finalized = true;
                self.candidate = None;
                if event.get("is_error").and_then(Value::as_bool) == Some(true) {
                    return Some(vec![activity(StepActivityKind::Message, "agent failed")]);
                }
                let message = event.get("result").and_then(Value::as_str)?;
                Some(final_message(message.to_owned()))
            }
            _ => None,
        }
    }
}

fn content_bytes(value: &Value) -> usize {
    match value {
        Value::String(text) => text.len(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| block.get("text").map_or(0, content_bytes))
            .sum(),
        _ => 0,
    }
}

impl AgentOutputTranslator for Translator {
    fn native_observation(&self) -> super::NativeObservation {
        self.observation.clone()
    }
    fn feed_line(&mut self, stream: OutputStream, line: &str) -> Vec<Translated> {
        if stream == OutputStream::Stderr {
            return passthrough(stream, line);
        }
        match serde_json::from_str::<Value>(line) {
            Ok(event) => self.translate(&event).unwrap_or_else(|| {
                vec![activity(
                    StepActivityKind::Message,
                    "unrecognised agent frame",
                )]
            }),
            Err(_) => passthrough(stream, line),
        }
    }

    fn finish(&mut self) -> Vec<Translated> {
        if self.finalized {
            return vec![];
        }
        self.finalized = true;
        self.candidate.take().map(final_message).unwrap_or_default()
    }
}
