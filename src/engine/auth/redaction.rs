//! The one redaction implementation (WI 0149 §6).
//!
//! A per-run [`RedactionSet`] collects every secret value that enters a grant
//! (env token, pasted token, `gh auth token`, `acquire` output). Layer 1 runs
//! all text that leaves a container step through [`RedactionSet::redact`]
//! before any frontend, log or failure file sees it. Clones share one set, so
//! a token registered late (a pre-teardown refresh) reaches every holder.

use std::sync::{Arc, RwLock};

use super::credential::SecretString;

#[derive(Clone, Default)]
pub struct RedactionSet {
    inner: Arc<RwLock<Vec<SecretString>>>,
    retired: Arc<std::sync::atomic::AtomicBool>,
}

impl RedactionSet {
    pub const MARK: &'static str = "***";
    /// Shorter secrets are not registered: replacing them would mangle
    /// ordinary output without protecting anything meaningful.
    pub const MIN_LEN: usize = 1;

    pub fn new() -> Self {
        Self::default()
    }

    /// Register `secret`. Duplicates and values shorter than [`Self::MIN_LEN`]
    /// are ignored. Kept sorted longest-first so a secret that contains
    /// another is replaced whole.
    pub fn add(&self, secret: SecretString) {
        if secret.expose().chars().count() < Self::MIN_LEN {
            return;
        }
        // Output events are line-oriented. Also protect complete lines of a
        // multiline value; arbitrary fragments across separate events are not
        // buffered, since doing so would delay publication indefinitely.
        use base64::Engine as _;
        let text = secret.expose();
        let mut forms: Vec<String> = text
            .lines()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let json = serde_json::to_string(text).expect("string serialization");
        forms.push(json[1..json.len() - 1].into());
        forms.push(text.chars().filter(|c| !c.is_control()).collect());
        let encoded: String = text
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        forms.push(encoded);
        forms.push(base64::engine::general_purpose::STANDARD.encode(text));
        forms.push(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text));
        let mut secrets = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if secrets.iter().any(|s| s.expose() == secret.expose()) {
            return;
        }
        secrets.push(secret);
        for line in forms
            .into_iter()
            .filter(|s| !s.is_empty())
            .map(SecretString::new)
        {
            if !secrets.iter().any(|s| s.expose() == line.expose()) {
                secrets.push(line);
            }
        }
        secrets.sort_by_key(|s| std::cmp::Reverse(s.expose().len()));
    }

    /// Replace every exact occurrence of a registered secret with [`Self::MARK`].
    pub fn redact(&self, text: &str) -> String {
        if self.retired.load(std::sync::atomic::Ordering::SeqCst) {
            return Self::MARK.into();
        }
        let secrets = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut ranges = Vec::new();
        for secret in secrets.iter() {
            let needle = secret.expose();
            let mut at = 0;
            while let Some(offset) = text[at..].find(needle) {
                let start = at + offset;
                ranges.push((start, start + needle.len()));
                at = start
                    + text[start..]
                        .chars()
                        .next()
                        .expect("nonempty match")
                        .len_utf8();
            }
            // Text framers may have cut a credential at their byte bound.
            let end = text
                .strip_suffix('…')
                .or_else(|| text.strip_suffix(" [line truncated]"));
            if let Some(end) = end {
                for (index, _) in needle.char_indices().skip(1) {
                    if index >= 4 && end.ends_with(&needle[..index]) {
                        ranges.push((end.len() - index, text.len()));
                    }
                }
            }
        }
        String::from_utf8(replace_ranges(text.as_bytes(), ranges)).expect("UTF-8 match boundaries")
    }

    /// Byte-level variant for raw process output that may not be UTF-8.
    pub fn redact_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        if self.retired.load(std::sync::atomic::Ordering::SeqCst) {
            return Self::MARK.as_bytes().to_vec();
        }
        let secrets = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut ranges = Vec::new();
        for secret in secrets.iter() {
            let needle = secret.expose().as_bytes();
            ranges.extend(
                bytes
                    .windows(needle.len())
                    .enumerate()
                    .filter(|(_, part)| *part == needle)
                    .map(|(at, _)| (at, at + needle.len())),
            );
        }
        replace_ranges(bytes, ranges)
    }

    pub fn is_empty(&self) -> bool {
        !self.retired.load(std::sync::atomic::Ordering::SeqCst)
            && self
                .inner
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
    }

    /// Retire values once publication has permanently closed for the command.
    pub fn clear(&self) {
        self.retired
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Walk text and data keys, including future fields and agent-authored maps.
    pub fn redact_json(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => *text = self.redact(text),
            serde_json::Value::Array(values) => {
                for value in values {
                    self.redact_json(value);
                }
            }
            serde_json::Value::Object(values) => {
                let original = std::mem::take(values);
                for (key, mut value) in original {
                    self.redact_json(&mut value);
                    values.insert(self.redact(&key), value);
                }
            }
            _ => {}
        }
    }

    /// No event variant can opt out of redaction. Protect the outer enum tag
    /// only; if a structural string elsewhere changes, withhold the event.
    pub fn redact_event(
        &self,
        payload: crate::data::execution_event::EventPayload,
    ) -> crate::data::execution_event::EventPayload {
        use crate::data::execution_event::EventPayload;
        if self.is_empty() {
            return payload;
        }
        // Terminal status and exit code are protocol facts, not agent text.
        // Keep them usable even for a secret such as "done" or "0".
        if let EventPayload::CommandStatus {
            status,
            exit_code,
            error,
            mut result,
        } = payload
        {
            if let Some(result) = &mut result {
                self.redact_result(result);
            }
            return EventPayload::CommandStatus {
                status,
                exit_code,
                error: error.map(|e| self.redact(&e)),
                result,
            };
        }
        if matches!(payload, EventPayload::Done) {
            return payload;
        }
        let converted = serde_json::to_value(&payload).ok().and_then(|mut value| {
            let tag = value.get("type").cloned();
            // Preserve envelope schema; data-keyed maps are still walked.
            if let Some(data) = value.get_mut("data") {
                self.redact_json(data);
            }
            if let Some(tag) = tag {
                value["type"] = tag;
            }
            serde_json::from_value(value).ok()
        });
        converted.unwrap_or_else(|| EventPayload::StatusMessage {
            phase: "redaction".into(),
            message: "*** event withheld: redaction".into(),
        })
    }

    /// Attribution can include an agent-authored step name too. If redacting
    /// structural metadata makes it invalid, omit the attribution safely.
    pub fn redact_source(
        &self,
        source: crate::data::execution_event::EventSource,
    ) -> Option<crate::data::execution_event::EventSource> {
        if self.is_empty() {
            return Some(source);
        }
        let mut value = serde_json::to_value(source).ok()?;
        self.redact_json(&mut value);
        serde_json::from_value(value).ok()
    }

    /// The same walk before result sidecars and database rows are written.
    pub fn redact_result(&self, result: &mut crate::data::fs::api_db::CommandResult) {
        if self.is_empty() {
            return;
        }
        if let Ok(original) = serde_json::to_value(&*result) {
            let mut value = original.clone();
            self.redact_fields(&mut value);
            // Preserve only these established protocol fields, by full path.
            // A future arbitrary map named `code` still receives redaction.
            for path in ["/outcome", "/stage", "/failure/stage", "/failure/code"] {
                if let (Some(original), Some(redacted)) =
                    (original.pointer(path), value.pointer_mut(path))
                {
                    *redacted = original.clone();
                }
            }
            if let (Some(original), Some(redacted)) = (
                original.get("failed_steps").and_then(|v| v.as_array()),
                value.get_mut("failed_steps").and_then(|v| v.as_array_mut()),
            ) {
                for (original, redacted) in original.iter().zip(redacted) {
                    redacted["phase"] = original["phase"].clone();
                }
            }
            for field in ["data", "setup_error"] {
                if let Some(original) = original.get(field).cloned() {
                    let mut original = original;
                    self.redact_json(&mut original);
                    value[field] = original;
                }
            }
            if let Ok(redacted) = serde_json::from_value(value) {
                *result = redacted;
                return;
            }
        }
        // Never retain an unredacted result on a conversion failure.
        *result = crate::data::fs::api_db::CommandResult {
            exit_code: result.exit_code,
            outcome: result.outcome,
            error: Some("*** result withheld: redaction".into()),
            ..Default::default()
        };
    }
    fn redact_fields(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => *text = self.redact(text),
            serde_json::Value::Array(array) => {
                for value in array {
                    self.redact_fields(value);
                }
            }
            serde_json::Value::Object(map) => {
                for value in map.values_mut() {
                    self.redact_fields(value);
                }
            }
            _ => {}
        }
    }
}

impl std::fmt::Debug for RedactionSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.inner.read().unwrap_or_else(|e| e.into_inner()).len();
        write!(f, "RedactionSet(<{n} secrets>)")
    }
}

fn replace_ranges(haystack: &[u8], mut ranges: Vec<(usize, usize)>) -> Vec<u8> {
    ranges.sort_unstable();
    let mut out = Vec::with_capacity(haystack.len());
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut().filter(|last| start <= last.1) {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    let mut at = 0;
    for (start, end) in merged {
        out.extend_from_slice(&haystack[at..start]);
        out.extend_from_slice(RedactionSet::MARK.as_bytes());
        at = end;
    }
    out.extend_from_slice(&haystack[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::execution_event::EventPayload;
    use serde_json::json;

    #[test]
    fn all_event_payloads_and_nested_results_use_the_generic_string_walk() {
        let set = RedactionSet::new();
        let secret = "test-secret-events";
        set.add(SecretString::new(secret));
        let terminal = crate::data::ids::TerminalId::new();
        // Serde fixtures deliberately avoid an exhaustive Rust match: adding a
        // variant elsewhere must compile without editing this redactor.
        let fixtures = [
            json!({"type":"stdout_line","data":format!("{secret} {secret}")}),
            json!({"type":"stderr_line","data":secret}),
            json!({"type":"status_message","data":{"phase":"main","message":secret}}),
            json!({"type":"step_activity","data":{"kind":"message","summary":secret}}),
            json!({"type":"workflow_step_transition","data":{"step_name":secret,"step_index":0,"from_status":"pending","to_status":"running"}}),
            json!({"type":"workflow_phase_transition","data":{"phase":"setup","step_desc":secret,"status":"running"}}),
            json!({"type":"workflow_parallel_step_launched","data":{"step_name":secret,"step_index":0,"agent":"claude","model":null}}),
            json!({"type":"workflow_parallel_step_exited","data":{"step_name":secret,"step_index":0,"exit_code":0}}),
            json!({"type":"workflow_parallel_group_finished"}),
            json!({"type":"command_status","data":{"status":"error","exit_code":1,"error":secret,"result":{"exit_code":1,"final_message":secret,"failure":{"stage":"step","code":"step_failed","message":secret},"data":{"nested":[secret]}}}}),
            json!({"type":"program_status","data":{"state":"working","msg":secret}}),
            json!({"type":"terminal_opened","data":{"terminal_id":terminal,"agent":secret,"cols":80,"rows":24}}),
            json!({"type":"terminal_closed","data":{"terminal_id":terminal,"exit_code":0}}),
            json!({"type":"terminal_control","data":{"terminal_id":terminal,"holder":secret}}),
            json!({"type":"terminal_viewer_detached","data":{"terminal_id":terminal,"reason":"user"}}),
            json!({"type":"workflow_state","data":{"agent_text":secret}}),
            json!({"type":"workflow_prerequisites","data":{"phase":"start","report":{"phase":"start","origin":"static","checks":[],"advisories":[],"notes":[secret]}}}),
            json!({"type":"done"}),
        ];
        for fixture in fixtures {
            let event: EventPayload = serde_json::from_value(fixture.clone())
                .unwrap_or_else(|e| panic!("invalid event fixture: {fixture}: {e}"));
            let empty = RedactionSet::new();
            assert_eq!(
                serde_json::to_value(empty.redact_event(event.clone())).unwrap(),
                serde_json::to_value(&event).unwrap()
            );
            let redacted = serde_json::to_value(set.redact_event(event)).unwrap();
            assert!(!redacted.to_string().contains(secret), "{redacted}");
        }
    }

    /// WI 0145 F8: the typed `push_retry` event passes a non-empty redaction
    /// set unchanged (it carries no secret by construction) and is still
    /// walked, so a secret that did reach a field would be removed.
    #[test]
    fn push_retry_events_pass_a_non_empty_set_unchanged_and_are_walked() {
        let set = RedactionSet::new();
        set.add(SecretString::new("test-secret-retry"));
        let fixture = json!({"type":"push_retry","data":{"attempt":2,"max_attempts":4,"operation":"push","delay_ms":2000,"remote":"git.example/o/r.git"}});
        let event: EventPayload = serde_json::from_value(fixture.clone()).unwrap();
        let redacted = set.redact_event(event.clone());
        assert_eq!(
            serde_json::to_value(&redacted).unwrap(),
            serde_json::to_value(&event).unwrap()
        );
        let leaked = json!({"type":"push_retry","data":{"attempt":2,"max_attempts":4,"operation":"push","delay_ms":2000,"remote":"test-secret-retry"}});
        let event: EventPayload = serde_json::from_value(leaked).unwrap();
        let redacted = serde_json::to_value(set.redact_event(event)).unwrap();
        assert!(!redacted.to_string().contains("test-secret"), "{redacted}");
    }

    #[test]
    fn multiline_values_protect_whole_values_and_complete_output_lines() {
        let set = RedactionSet::new();
        set.add(SecretString::new(
            "test-secret-first\ntest-secret-second\nshort",
        ));
        assert_eq!(set.redact("test-secret-first test-secret-first"), "*** ***");
        assert_eq!(set.redact("test-secret-second"), "***");
        assert_eq!(
            set.redact("test-secret-first\ntest-secret-second\nshort"),
            "***"
        );
        assert_eq!(set.redact("short"), "***");
        set.add(SecretString::new("test-secret"));
        assert_eq!(set.redact("test-secret-first"), "***");
    }

    #[test]
    fn structural_string_redaction_withholds_instead_of_leaking() {
        let set = RedactionSet::new();
        set.add(SecretString::new("message"));
        let event: EventPayload = serde_json::from_value(
            json!({"type":"step_activity","data":{"kind":"message","summary":"safe"}}),
        )
        .unwrap();
        assert!(
            matches!(set.redact_event(event), EventPayload::StatusMessage { message, .. } if message == "*** event withheld: redaction")
        );
        // An enum tag itself never gets changed, even for a one-byte secret.
        set.add(SecretString::new("StdoutLine"));
        assert!(matches!(
            set.redact_event(EventPayload::StdoutLine("safe".into())),
            EventPayload::StdoutLine(_)
        ));
    }

    #[test]
    fn data_keys_overlaps_encodings_truncation_and_retirement_are_redacted() {
        let set = RedactionSet::new();
        set.add(SecretString::new("abcdef12"));
        set.add(SecretString::new("ef12ghij"));
        assert_eq!(set.redact("abcdef12ghij"), "***");
        assert_eq!(set.redact_bytes(b"abcdef12ghij"), b"***");
        let mut data = json!({"deploy-abcdef12": {"ef12ghij": "safe"}});
        set.redact_json(&mut data);
        assert_eq!(data, json!({"deploy-***":{"***":"safe"}}));
        set.add(SecretString::new("test-secret-p/ss\"word"));
        for encoded in [
            "test-secret-p%2Fss%22word",
            "test-secret-p/ss\\\"word",
            "dGVzdC1zZWNyZXQtcC9zcyJ3b3Jk",
        ] {
            assert_eq!(set.redact(encoded), "***");
        }
        assert_eq!(set.redact("prefix test-secret-p/ss…"), "prefix ***");
        set.clear();
        assert_eq!(set.redact("abcdef12ghij"), "***");
        assert!(!set.is_empty());
    }

    #[test]
    fn short_common_secrets_preserve_terminal_status_and_result_protocol_fields() {
        use crate::data::{
            execution_event::CommandStatusKind,
            fs::api_db::{CommandFailure, CommandResult},
        };
        for secret in ["0", "main", "done", "error", "message"] {
            let set = RedactionSet::new();
            set.add(SecretString::new(secret));
            let mut result = CommandResult {
                exit_code: Some(0),
                outcome: Some(CommandStatusKind::Done),
                failure: Some(CommandFailure {
                    stage: "main".into(),
                    code: "error".into(),
                    message: secret.into(),
                    step: None,
                }),
                data: Some(json!({secret:secret})),
                ..Default::default()
            };
            set.redact_result(&mut result);
            assert_eq!(result.exit_code, Some(0));
            assert_eq!(result.outcome, Some(CommandStatusKind::Done));
            assert_eq!(result.failure.as_ref().unwrap().code, "error");
            assert_eq!(result.failure.as_ref().unwrap().message, "***");
            assert_eq!(result.data, Some(json!({"***":"***"})));
            let event = set.redact_event(EventPayload::CommandStatus {
                status: CommandStatusKind::Done,
                exit_code: Some(0),
                error: None,
                result: Some(Box::new(result)),
            });
            assert!(matches!(
                event,
                EventPayload::CommandStatus {
                    status: CommandStatusKind::Done,
                    exit_code: Some(0),
                    ..
                }
            ));
        }
    }
}
