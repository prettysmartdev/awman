//! Codex `exec --json` NDJSON. The last agent message is the final answer.
use serde_json::Value;

use super::{
    activity, final_message, passthrough, AgentOutputTranslator, OutputStream, StepActivityKind,
    Translated,
};

#[derive(Default)]
pub struct Translator {
    candidate: Option<String>,
    finalized: bool,
}

impl Translator {
    fn complete(&mut self) -> Vec<Translated> {
        if self.finalized {
            return vec![];
        }
        self.finalized = true;
        self.candidate.take().map(final_message).unwrap_or_default()
    }

    fn translate(&mut self, event: &Value) -> Option<Vec<Translated>> {
        let event_type = event.get("type")?.as_str()?;
        match event_type {
            "thread.started" if event.get("thread_id").and_then(Value::as_str).is_some() => {
                Some(vec![])
            }
            "turn.started" => Some(vec![]),
            "turn.completed" => Some(self.complete()),
            "turn.failed" => {
                self.candidate = None;
                self.finalized = true;
                Some(vec![activity(StepActivityKind::Message, "agent failed")])
            }
            "item.started" | "item.updated" | "item.completed" => {
                let item = event.get("item")?;
                let item_type = item.get("type")?.as_str()?;
                let completed = event_type == "item.completed";
                if event_type == "item.updated" {
                    return Some(vec![]);
                }
                let translated = match item_type {
                    "agent_message" if completed => {
                        let message = item.get("text")?.as_str()?;
                        if !self.finalized {
                            self.candidate = Some(message.to_owned());
                        }
                        activity(StepActivityKind::Message, message)
                    }
                    "reasoning" => activity(StepActivityKind::Thinking, "thinking"),
                    "agent_message" => return Some(vec![]),
                    "command_execution" => {
                        let command = item.get("command")?.as_str()?;
                        if completed {
                            let status = item.get("exit_code").and_then(Value::as_i64).map_or_else(
                                || "completed".to_owned(),
                                |code| format!("exit {code}"),
                            );
                            activity(StepActivityKind::ToolResult, &format!("command: {status}"))
                        } else if event_type == "item.started" {
                            activity(StepActivityKind::ToolUse, &format!("command: {command}"))
                        } else {
                            return Some(vec![]);
                        } // Streaming payload updates are not activities.
                    }
                    "file_change" => {
                        let changes = item.get("changes")?.as_array()?;
                        let path = changes
                            .first()
                            .and_then(|change| change.get("path"))
                            .and_then(Value::as_str)
                            .unwrap_or("file");
                        let kind = if completed {
                            StepActivityKind::ToolResult
                        } else {
                            StepActivityKind::ToolUse
                        };
                        activity(
                            kind,
                            &format!("file change: {path} ({} files)", changes.len()),
                        )
                    }
                    "mcp_tool_call" => {
                        let tool = item.get("tool")?.as_str()?;
                        let server = item.get("server")?.as_str()?;
                        let kind = if completed {
                            StepActivityKind::ToolResult
                        } else {
                            StepActivityKind::ToolUse
                        };
                        let status = if completed { " completed" } else { "" };
                        activity(kind, &format!("{server}/{tool}{status}"))
                    }
                    "web_search" => {
                        let query = item.get("query")?.as_str()?;
                        let kind = if completed {
                            StepActivityKind::ToolResult
                        } else {
                            StepActivityKind::ToolUse
                        };
                        activity(kind, &format!("web search: {query}"))
                    }
                    _ => return None,
                };
                Some(vec![translated])
            }
            _ => None, // Errors and future frame types stay visible as output lines.
        }
    }
}

impl AgentOutputTranslator for Translator {
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
        self.complete()
    }
}
