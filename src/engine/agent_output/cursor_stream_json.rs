//! Cursor Agent `--print --output-format stream-json` NDJSON.
//!
//! One JSON object per line (https://cursor.com/docs/cli/reference/output-format):
//! `system`/`init`, `user` (echo of the prompt), `assistant` text messages,
//! `tool_call` `started`/`completed`, and a terminal `result`. The CLI may end
//! a failed run without any `result` frame (verified: an unauthenticated run
//! exits 1 with nothing on stdout), so the translator never infers success
//! from the stream; the exit status always comes from the process.
//!
//! Tool payloads (`args`, `result`) are never echoed: a summary names the tool
//! and one short hint. Unknown fields are ignored; an unknown frame becomes a
//! fixed activity, never a raw protocol line.
use serde_json::Value;

use super::{
    activity, final_message, passthrough, AgentOutputTranslator, OutputStream, StepActivityKind,
    Translated,
};

#[derive(Default)]
pub struct Translator {
    /// Text of the latest assistant message, flushed at `finish()` when no
    /// `result` frame arrived.
    candidate: Option<String>,
    /// A terminal `result` frame (success or error) was seen.
    finalized: bool,
}

/// `readToolCall` → `read`; `{"function":{"name":…}}` → that name.
fn tool_name(tool_call: &Value) -> String {
    if let Some(name) = tool_call
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
    {
        return name.to_owned();
    }
    tool_call
        .as_object()
        .and_then(|map| map.keys().find(|k| k.as_str() != "function"))
        .map(|key| key.strip_suffix("ToolCall").unwrap_or(key).to_owned())
        .unwrap_or_else(|| "tool".to_owned())
}

/// The inner `{args, result}` object of a tool call, whichever shape it has.
fn tool_body(tool_call: &Value) -> Option<&Value> {
    let map = tool_call.as_object()?;
    map.iter()
        .find(|(k, _)| k.as_str() != "function")
        .map(|(_, v)| v)
}

fn tool_hint(tool_call: &Value) -> Option<String> {
    let args = tool_body(tool_call).and_then(|b| b.get("args"));
    if let Some(args) = args {
        for key in ["path", "command", "pattern", "query"] {
            if let Some(text) = args.get(key).and_then(Value::as_str) {
                return Some(text.to_owned());
            }
        }
    }
    // `function.arguments` is an opaque JSON string; never echo it.
    None
}

impl Translator {
    fn translate(&mut self, event: &Value) -> Option<Vec<Translated>> {
        let event_type = event.get("type")?.as_str()?;
        match event_type {
            "system" | "user" => Some(vec![]),
            "assistant" => {
                let content = event.get("message")?.get("content")?.as_array()?;
                let texts: Vec<&str> = content
                    .iter()
                    .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect();
                if texts.is_empty() {
                    return Some(vec![]);
                }
                if !self.finalized {
                    self.candidate = Some(texts.concat());
                }
                Some(
                    texts
                        .iter()
                        .map(|text| activity(StepActivityKind::Message, text))
                        .collect(),
                )
            }
            "tool_call" => {
                let subtype = event.get("subtype")?.as_str()?;
                let tool_call = event.get("tool_call")?;
                let name = tool_name(tool_call);
                match subtype {
                    "started" => {
                        let summary = match tool_hint(tool_call) {
                            Some(hint) => format!("{name}: {hint}"),
                            None => name,
                        };
                        Some(vec![activity(StepActivityKind::ToolUse, &summary)])
                    }
                    "completed" => {
                        let result = tool_body(tool_call).and_then(|b| b.get("result"));
                        let status = match result {
                            Some(r) if r.get("success").is_some() => "completed",
                            Some(_) => "error",
                            None => "completed",
                        };
                        Some(vec![activity(
                            StepActivityKind::ToolResult,
                            &format!("{name}: {status}"),
                        )])
                    }
                    _ => None,
                }
            }
            "result" => {
                if self.finalized {
                    return Some(vec![]);
                }
                self.finalized = true;
                let candidate = self.candidate.take();
                let is_error = event
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || event.get("subtype").and_then(Value::as_str) != Some("success");
                if is_error {
                    return Some(vec![activity(StepActivityKind::Message, "agent failed")]);
                }
                let message = event
                    .get("result")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(candidate);
                Some(message.map(final_message).unwrap_or_default())
            }
            _ => None,
        }
    }
}

impl AgentOutputTranslator for Translator {
    fn feed_line(&mut self, stream: OutputStream, line: &str) -> Vec<Translated> {
        if stream == OutputStream::Stderr {
            return passthrough(stream, line);
        }
        match serde_json::from_str::<Value>(line) {
            Ok(event) if event.is_object() => self.translate(&event).unwrap_or_else(|| {
                vec![activity(
                    StepActivityKind::Message,
                    "unrecognised agent frame",
                )]
            }),
            Ok(_) => vec![activity(
                StepActivityKind::Message,
                "unrecognised agent frame",
            )],
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

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("testdata/cursor_stream_json.ndjson");
    const FINAL: &str = "Implemented structured output.\nValidation: cargo check passed.";

    fn run(lines: &[(OutputStream, &str)]) -> Vec<Translated> {
        let mut t = Translator::default();
        let mut out = Vec::new();
        for (stream, line) in lines {
            out.extend(t.feed_line(*stream, line));
        }
        out.extend(t.finish());
        out
    }

    fn stdout(text: &str) -> Vec<(OutputStream, &str)> {
        text.lines().map(|l| (OutputStream::Stdout, l)).collect()
    }

    fn finals(items: &[Translated]) -> Vec<&str> {
        items
            .iter()
            .filter_map(|i| match i {
                Translated::FinalMessage(m) => Some(m.as_str()),
                _ => None,
            })
            .collect()
    }

    fn activities(items: &[Translated]) -> Vec<(StepActivityKind, &str)> {
        items
            .iter()
            .filter_map(|i| match i {
                Translated::Activity { kind, summary } => Some((*kind, summary.as_str())),
                _ => None,
            })
            .collect()
    }

    fn lines(items: &[Translated]) -> Vec<&str> {
        items
            .iter()
            .filter_map(|i| match i {
                Translated::Line { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn documented_stream_yields_activities_and_one_final_message() {
        let out = run(&stdout(FIXTURE));
        assert_eq!(finals(&out), vec![FINAL]);
        assert_eq!(
            activities(&out),
            vec![
                (StepActivityKind::Message, "Reading the project."),
                (StepActivityKind::ToolUse, "read: src/main.rs"),
                (StepActivityKind::ToolResult, "read: completed"),
                (StepActivityKind::ToolUse, "shell: cargo check"),
                (StepActivityKind::ToolResult, "shell: error"),
                (StepActivityKind::ToolUse, "edit"),
                (StepActivityKind::ToolResult, "edit: completed"),
                (StepActivityKind::Message, "Implemented structured output."),
            ]
        );
        // The malformed line survives; final text appears as two stdout lines.
        assert_eq!(
            lines(&out),
            vec![
                "malformed: not json",
                "Implemented structured output.",
                "Validation: cargo check passed."
            ]
        );
    }

    #[test]
    fn no_raw_protocol_json_ever_reaches_an_output_line() {
        let mut input = stdout(FIXTURE);
        input.push((OutputStream::Stdout, r#"{"type":"future_frame","x":1}"#));
        input.push((OutputStream::Stdout, r#"[1,2]"#));
        let out = run(&input);
        for line in lines(&out) {
            assert!(!line.trim_start().starts_with('{'), "raw frame: {line}");
            assert!(!line.trim_start().starts_with('['), "raw frame: {line}");
        }
        assert!(activities(&out).contains(&(StepActivityKind::Message, "unrecognised agent frame")));
    }

    #[test]
    fn tool_payloads_are_never_echoed() {
        let started = r#"{"type":"tool_call","subtype":"started","call_id":"1","tool_call":{"writeToolCall":{"args":{"path":"a.txt","fileText":"SECRET-BODY"}}}}"#;
        let done = r#"{"type":"tool_call","subtype":"completed","call_id":"1","tool_call":{"writeToolCall":{"args":{"path":"a.txt","fileText":"SECRET-BODY"},"result":{"success":{"linesCreated":1,"content":"SECRET-BODY"}}}}}"#;
        let function = r#"{"type":"tool_call","subtype":"started","call_id":"2","tool_call":{"function":{"name":"mcp_lookup","arguments":"{\"token\":\"SECRET-BODY\"}"}}}"#;
        let out = run(&[
            (OutputStream::Stdout, started),
            (OutputStream::Stdout, done),
            (OutputStream::Stdout, function),
        ]);
        assert_eq!(
            activities(&out),
            vec![
                (StepActivityKind::ToolUse, "write: a.txt"),
                (StepActivityKind::ToolResult, "write: completed"),
                (StepActivityKind::ToolUse, "mcp_lookup"),
            ]
        );
        assert!(!format!("{out:?}").contains("SECRET-BODY"));
    }

    #[test]
    fn failure_without_result_frame_has_stderr_and_no_final_message() {
        // Verified shape: unauthenticated run, nothing on stdout, one stderr line.
        let out = run(&[(
            OutputStream::Stderr,
            "Authentication required. Run cursor-agent login or set CURSOR_API_KEY.",
        )]);
        assert!(finals(&out).is_empty());
        assert_eq!(
            out,
            vec![Translated::Line {
                stream: OutputStream::Stderr,
                text: "Authentication required. Run cursor-agent login or set CURSOR_API_KEY."
                    .into()
            }]
        );
    }

    #[test]
    fn truncated_stream_flushes_the_last_assistant_text_once() {
        let input = stdout(
            r#"{"type":"system","subtype":"init","session_id":"s","model":"m"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"partial "},{"type":"text","text":"answer"}]}}"#,
        );
        let out = run(&input);
        assert_eq!(finals(&out), vec!["partial answer"]);
        let mut t = Translator::default();
        for (s, l) in &input {
            t.feed_line(*s, l);
        }
        assert_eq!(t.finish().len(), 2);
        assert!(t.finish().is_empty(), "flushed at most once");
    }

    #[test]
    fn error_result_reports_failure_and_no_final_message() {
        let out = run(&stdout(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"trying"}]}}
{"type":"result","subtype":"error","is_error":true,"result":"boom"}"#,
        ));
        assert!(finals(&out).is_empty());
        assert!(activities(&out).contains(&(StepActivityKind::Message, "agent failed")));
    }

    #[test]
    fn duplicate_result_frames_finalize_once() {
        let r = r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#;
        let out = run(&[(OutputStream::Stdout, r), (OutputStream::Stdout, r)]);
        assert_eq!(finals(&out), vec!["done"]);
    }

    #[test]
    fn empty_stream_produces_nothing() {
        assert!(run(&[]).is_empty());
    }

    #[test]
    fn registered_in_translator_for() {
        let mut t = super::super::translator_for(super::super::AgentOutputFormat::CursorStreamJson);
        let out = t.feed_line(
            OutputStream::Stdout,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#,
        );
        assert_eq!(finals(&out), vec!["ok"]);
    }
}
