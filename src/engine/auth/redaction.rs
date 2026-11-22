//! The one redaction implementation (WI 0149 §6).
//!
//! A per-run [`RedactionSet`] collects every secret value that enters a grant
//! (env token, pasted token, `gh auth token`, `acquire` output). Layer 1 runs
//! all text that leaves a container step through [`RedactionSet::redact`]
//! before any frontend, log or failure file sees it. Clones share one set, so
//! a token registered late (a pre-teardown refresh) reaches every holder.

use std::sync::{Arc, Mutex, RwLock, Weak};

use super::credential::SecretString;

/// A live stream's held-back bytes, as registered with its set.
type StreamBuffer = Weak<Mutex<Vec<u8>>>;

#[derive(Clone, Default)]
pub struct RedactionSet {
    inner: Arc<RwLock<Vec<SecretString>>>,
    retired: Arc<std::sync::atomic::AtomicBool>,
    /// Held-back plaintext of every live [`RedactionStream`] on this set, so
    /// retirement scrubs it at once rather than on the stream's next use.
    streams: Arc<Mutex<Vec<StreamBuffer>>>,
}

impl RedactionSet {
    pub const MAX_BYTES: usize = 16 * 1024 * 1024;
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
        if self.retired.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        if secrets.iter().any(|s| s.expose() == secret.expose()) {
            return;
        }
        let mut incoming = vec![secret];
        for value in forms
            .into_iter()
            .filter(|s| !s.is_empty())
            .map(SecretString::new)
        {
            if !secrets
                .iter()
                .chain(incoming.iter())
                .any(|s| s.expose() == value.expose())
            {
                incoming.push(value);
            }
        }
        let retained = secrets.iter().map(|s| s.expose().len()).sum::<usize>();
        let additional = incoming.iter().map(|s| s.expose().len()).sum::<usize>();
        if retained.saturating_add(additional) > Self::MAX_BYTES {
            self.retired
                .store(true, std::sync::atomic::Ordering::SeqCst);
            secrets.clear();
            drop(secrets);
            self.scrub_streams();
            return;
        }
        secrets.extend(incoming);
        secrets.sort_by_key(|s| std::cmp::Reverse(s.expose().len()));
    }

    /// Transactional enrollment for an accepted update. A refused bundle leaves
    /// existing output and its budget intact, including encoded forms.
    pub fn try_add_many(
        &self,
        values: impl IntoIterator<Item = SecretString>,
    ) -> Result<(), &'static str> {
        let incoming = Self::new();
        for value in values {
            incoming.add(value);
        }
        if incoming.is_retired() || self.is_retired() {
            return Err("output_incomplete");
        }
        let incoming = incoming.inner.read().unwrap_or_else(|p| p.into_inner());
        let mut retained = self.inner.write().unwrap_or_else(|p| p.into_inner());
        if self.is_retired() {
            return Err("output_incomplete");
        }
        let additions: Vec<_> = incoming
            .iter()
            .filter(|value| !retained.iter().any(|v| v.expose() == value.expose()))
            .cloned()
            .collect();
        let size = retained
            .iter()
            .chain(additions.iter())
            .map(|s| s.expose().len())
            .sum::<usize>();
        if size > Self::MAX_BYTES {
            return Err("output_incomplete");
        }
        retained.extend(additions);
        retained.sort_by_key(|s| std::cmp::Reverse(s.expose().len()));
        Ok(())
    }

    /// Replace every exact occurrence of a registered secret with [`Self::MARK`].
    pub fn redact(&self, text: &str) -> String {
        if self.retired.load(std::sync::atomic::Ordering::SeqCst) {
            return Self::MARK.into();
        }
        let secrets = self.inner.read().unwrap_or_else(|e| e.into_inner());
        if self.is_retired() {
            return Self::MARK.into();
        }
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

    pub fn is_retired(&self) -> bool {
        self.retired.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn is_empty(&self) -> bool {
        !self.retired.load(std::sync::atomic::Ordering::SeqCst)
            && self
                .inner
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
    }

    /// Register every secret `other` holds now (its later registrations are
    /// not followed). Absorbing a set into itself is a no-op.
    pub fn absorb(&self, other: &RedactionSet) {
        if other.is_retired() {
            self.clear();
            return;
        }
        if Arc::ptr_eq(&self.inner, &other.inner) {
            return;
        }
        let values: Vec<SecretString> = other
            .inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|s| SecretString::new(s.expose().to_owned()))
            .collect();
        for value in values {
            self.add(value);
        }
    }

    /// Retire values once publication has permanently closed for the command.
    pub fn clear(&self) {
        self.retired
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.scrub_streams();
    }

    /// Zero and drop the plaintext every live stream holds back. Called only
    /// after `retired` is set and without the `inner` lock held (a stream
    /// takes its buffer lock before `inner`).
    fn scrub_streams(&self) {
        let streams = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        for pending in streams.iter().filter_map(Weak::upgrade) {
            scrub_buffer(&mut pending.lock().unwrap_or_else(|e| e.into_inner()));
        }
    }

    fn register_stream(&self, pending: &Arc<Mutex<Vec<u8>>>) {
        let mut streams = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        streams.retain(|weak| weak.strong_count() > 0);
        streams.push(Arc::downgrade(pending));
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

/// Union overlapping and adjacent ranges, sorted by start.
fn merge_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut().filter(|last| start <= last.1) {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
}

fn replace_ranges(haystack: &[u8], ranges: Vec<(usize, usize)>) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let merged = merge_ranges(ranges);
    let mut at = 0;
    for (start, end) in merged {
        out.extend_from_slice(&haystack[at..start]);
        out.extend_from_slice(RedactionSet::MARK.as_bytes());
        at = end;
    }
    out.extend_from_slice(&haystack[at..]);
    out
}

/// Incremental redaction whose concatenated output equals
/// [`RedactionSet::redact_bytes`] over the whole input for a fixed secret set.
///
/// Matches are unioned exactly as in the whole-buffer form (overlapping and
/// adjacent matches become one mark), including unions that span feeds. Only
/// a suffix that is a proper prefix of some secret is held back, so pending
/// plaintext never exceeds the longest secret minus one byte. A union that
/// still touches the held region is announced with its mark at once and then
/// only remembered (`open`, `masked`) so later bytes can extend it.
pub struct RedactionStream {
    set: RedactionSet,
    /// Held-back plaintext. Shared with the set's registry so retirement
    /// scrubs it immediately, even while this stream sits idle.
    pending: Arc<Mutex<Vec<u8>>>,
    /// Leading bytes of `pending` already covered by an emitted mark.
    masked: usize,
    /// The last emitted mark may still be extended by the next bytes.
    open: bool,
}
impl RedactionStream {
    pub fn new(set: RedactionSet) -> Self {
        let pending = Arc::new(Mutex::new(Vec::new()));
        set.register_stream(&pending);
        Self {
            set,
            pending,
            masked: 0,
            open: false,
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.drain(bytes, false)
    }
    pub fn finish(&mut self) -> Vec<u8> {
        self.drain(&[], true)
    }
    /// Plaintext bytes currently held back (bounded by the longest secret).
    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
    fn scrub(&mut self) {
        scrub_buffer(&mut self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        self.masked = 0;
        self.open = false;
    }
    fn drain(&mut self, bytes: &[u8], finished: bool) -> Vec<u8> {
        let shared = Arc::clone(&self.pending);
        let mut pending = shared.lock().unwrap_or_else(|e| e.into_inner());
        if self.set.is_retired() {
            scrub_buffer(&mut pending);
            self.masked = 0;
            self.open = false;
            return RedactionSet::MARK.as_bytes().to_vec();
        }
        pending.extend_from_slice(bytes);
        let secrets = self.set.inner.read().unwrap_or_else(|e| e.into_inner());
        if self.set.is_retired() {
            drop(secrets);
            scrub_buffer(&mut pending);
            self.masked = 0;
            self.open = false;
            return RedactionSet::MARK.as_bytes().to_vec();
        }
        let len = pending.len();
        let mut ranges = Vec::new();
        if self.masked > 0 {
            ranges.push((0, self.masked));
        }
        for secret in secrets.iter() {
            let needle = secret.expose().as_bytes();
            if needle.is_empty() || needle.len() > len {
                continue;
            }
            ranges.extend(
                pending
                    .windows(needle.len())
                    .enumerate()
                    .filter(|(_, part)| *part == needle)
                    .map(|(at, _)| (at, at + needle.len())),
            );
        }
        let merged = merge_ranges(ranges);
        // Earliest suffix that could still grow into a registered secret.
        let mut hold = len;
        if !finished {
            for secret in secrets.iter() {
                let needle = secret.expose().as_bytes();
                let longest = needle.len().saturating_sub(1).min(len);
                if let Some(k) = (1..=longest)
                    .rev()
                    .find(|k| pending.ends_with(&needle[..*k]))
                {
                    hold = hold.min(len - k);
                }
            }
        }
        drop(secrets);
        if hold == 0 && !finished {
            // Nothing can be released yet; keep the open-mark state as is.
            return Vec::new();
        }
        let mut output = Vec::with_capacity(hold);
        let mut at = 0;
        let mut open_after = false;
        let mut masked_after = 0;
        for &(start, end) in merged.iter().take_while(|(start, _)| *start < hold) {
            output.extend_from_slice(&pending[at..start]);
            if !(start == 0 && self.open) {
                output.extend_from_slice(RedactionSet::MARK.as_bytes());
            }
            at = end;
            if end >= hold {
                // The union reaches the held region: it may still grow.
                open_after = true;
                masked_after = end - hold;
                at = hold;
                break;
            }
        }
        output.extend_from_slice(&pending[at..hold]);
        if finished {
            scrub_buffer(&mut pending);
            self.masked = 0;
            self.open = false;
            return output;
        }
        pending[..hold].fill(0);
        pending.drain(..hold);
        zero_spare(&mut pending);
        self.masked = masked_after;
        self.open = open_after;
        output
    }
}
/// Zero `buffer`'s bytes and spare capacity (which may hold copies left by
/// an earlier shift), then empty it.
fn scrub_buffer(buffer: &mut Vec<u8>) {
    buffer.fill(0);
    buffer.clear();
    zero_spare(buffer);
}

fn zero_spare(buffer: &mut Vec<u8>) {
    buffer
        .spare_capacity_mut()
        .fill(std::mem::MaybeUninit::new(0));
}
impl Drop for RedactionStream {
    fn drop(&mut self) {
        self.scrub();
    }
}

/// A bounded streaming redactor for transcript publication. Decoding is left to
/// consumers; secret matches may cross arbitrary read boundaries.
pub struct RedactedReader<R> {
    input: R,
    stream: RedactionStream,
    ready: std::io::Cursor<Vec<u8>>,
    ended: bool,
}
impl<R: std::io::Read> RedactedReader<R> {
    pub fn new(input: R, set: RedactionSet) -> Self {
        Self {
            input,
            stream: RedactionStream::new(set),
            ready: std::io::Cursor::new(Vec::new()),
            ended: false,
        }
    }
}
impl<R: std::io::Read> std::io::Read for RedactedReader<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            let n = self.ready.read(output)?;
            if n != 0 || self.ended {
                return Ok(n);
            }
            let mut chunk = [0u8; 64 * 1024];
            let n = self.input.read(&mut chunk)?;
            let bytes = if n == 0 {
                self.ended = true;
                self.stream.finish()
            } else {
                self.stream.feed(&chunk[..n])
            };
            self.ready = std::io::Cursor::new(bytes);
        }
    }
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

#[cfg(test)]
mod credential_retention_tests {
    use super::*;
    #[test]
    fn accumulated_generations_retire_without_evicting_old_secrets() {
        let set = RedactionSet::new();
        set.add(SecretString::new("x".repeat(4 * 1024 * 1024)));
        assert!(!set.is_retired());
        set.add(SecretString::new("y".repeat(4 * 1024 * 1024)));
        assert!(set.is_retired());
        assert_eq!(set.redact("any later output"), "***");
        set.add(SecretString::new("test-secret-later"));
        assert!(set.is_retired());
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    use proptest::prelude::*;

    fn set_of(secrets: &[&str]) -> RedactionSet {
        let set = RedactionSet::new();
        for s in secrets {
            set.add(SecretString::new(*s));
        }
        set
    }

    /// Feed `input` split at `cuts` and return the concatenated output; also
    /// checks the pending bound after every feed.
    fn stream(set: &RedactionSet, input: &[u8], cuts: &[usize]) -> Vec<u8> {
        let longest = set
            .inner
            .read()
            .unwrap()
            .iter()
            .map(|s| s.expose().len())
            .max()
            .unwrap_or(0);
        let mut stream = RedactionStream::new(set.clone());
        let mut out = Vec::new();
        let mut at = 0;
        let mut points: Vec<usize> = cuts
            .iter()
            .map(|c| c.min(&input.len()).to_owned())
            .collect();
        points.sort_unstable();
        points.push(input.len());
        for point in points {
            out.extend(stream.feed(&input[at..point]));
            assert!(stream.pending_len() < longest.max(1), "unbounded pending");
            at = point;
        }
        out.extend(stream.finish());
        assert_eq!(stream.pending_len(), 0);
        out
    }

    fn whole(set: &RedactionSet, input: &[u8]) -> Vec<u8> {
        set.redact_bytes(input)
    }

    fn every_partition_equals(secrets: &[&str], input: &str) {
        let set = set_of(secrets);
        let expected = whole(&set, input.as_bytes());
        let n = input.len();
        // Byte-at-a-time, whole, and every single and double cut.
        let all: Vec<usize> = (1..n).collect();
        assert_eq!(stream(&set, input.as_bytes(), &all), expected, "{input:?}");
        assert_eq!(stream(&set, input.as_bytes(), &[]), expected, "{input:?}");
        for a in 0..=n {
            assert_eq!(
                stream(&set, input.as_bytes(), &[a]),
                expected,
                "{input:?} {a}"
            );
            for b in a..=n {
                assert_eq!(
                    stream(&set, input.as_bytes(), &[a, b]),
                    expected,
                    "{input:?} {a} {b}"
                );
            }
        }
    }

    #[test]
    fn audit_example_unions_overlapping_secrets() {
        let set = set_of(&["abcde", "defgh"]);
        assert_eq!(stream(&set, b"abcdefgh", &[]), b"***");
        assert_eq!(stream(&set, b"abcdefgh", &[4]), b"***");
        assert_eq!(stream(&set, b"xabcdefghy", &[1, 5, 6]), b"x***y");
    }

    #[test]
    fn table_of_overlaps_nesting_duplicates_and_adjacency() {
        let table: &[(&[&str], &str)] = &[
            (&["abcde", "defgh"], "abcdefgh abcde defgh"),
            (&["abcde", "defgh", "ghijk"], "..abcdefghijk.."),
            (&["abcdef", "cde"], "abcdef cde abcde"),
            (&["abc", "abc", "abcd"], "abcabcdabc"),
            (&["aaa"], "aaaaaaaa"),
            (&["a"], "aa b aaa"),
            (&["ab", "bc"], "abcabcbc"),
            (&["token-1", "token-2"], "token-1token-2 token-token-1"),
            (&["aXb", "bYc", "cZd"], "aXbYcZd aXb bYc"),
        ];
        for (secrets, input) in table {
            every_partition_equals(secrets, input);
        }
    }

    #[test]
    fn multibyte_utf8_secrets_split_inside_characters() {
        every_partition_equals(&["héllo🔑", "🔑wörld"], "a héllo🔑wörld é 🔑wörld");
        let set = set_of(&["héllo🔑"]);
        let out = stream(&set, "xhéllo🔑y".as_bytes(), &(1..12).collect::<Vec<_>>());
        assert_eq!(String::from_utf8(out).unwrap(), "x***y");
    }

    #[test]
    fn non_utf8_input_matches_whole_buffer_masking() {
        let set = set_of(&["secret"]);
        let input = b"\xff\xfesecret\x80se\xc3secret";
        let all: Vec<usize> = (1..input.len()).collect();
        assert_eq!(stream(&set, input, &all), whole(&set, input));
    }

    #[test]
    fn overlap_chains_stay_bounded_and_equal() {
        // A long chain of overlapping secrets must not accumulate plaintext.
        let secrets: Vec<String> = (0..40)
            .map(|i| {
                (i..i + 6)
                    .map(|n| char::from(b'a' + (n % 26) as u8))
                    .collect()
            })
            .collect();
        let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
        let set = set_of(&refs);
        let input: String = (0..45).map(|n| char::from(b'a' + (n % 26) as u8)).collect();
        let all: Vec<usize> = (1..input.len()).collect();
        let expected = whole(&set, input.as_bytes());
        assert_eq!(stream(&set, input.as_bytes(), &all), expected);
        assert_eq!(stream(&set, input.as_bytes(), &[7, 20]), expected);
    }

    #[test]
    fn partial_prefix_is_held_then_released_when_it_does_not_match() {
        let set = set_of(&["abcdef"]);
        let mut s = RedactionStream::new(set);
        assert_eq!(s.feed(b"xxabc"), b"xx");
        assert_eq!(s.pending_len(), 3);
        assert_eq!(s.feed(b"zz"), b"abczz");
        assert_eq!(s.feed(b"abcd"), b"");
        assert_eq!(s.finish(), b"abcd");
        assert_eq!(s.pending_len(), 0);
    }

    #[test]
    fn late_registration_applies_to_later_feeds() {
        let set = set_of(&["first-secret"]);
        let mut s = RedactionStream::new(set.clone());
        assert_eq!(s.feed(b"a first-secret b "), b"a *** b ");
        set.add(SecretString::new("later-secret"));
        let mut out = s.feed(b"later-sec");
        out.extend(s.feed(b"ret!"));
        out.extend(s.finish());
        assert_eq!(out, b"***!");
    }

    /// An idle stream (no further feed, finish or drop) holds no plaintext
    /// once any holder retires the shared set, directly or by absorbing a
    /// retired set; streams on other sets keep theirs.
    #[test]
    fn retirement_scrubs_idle_streams_on_every_clone_immediately() {
        let set = set_of(&["held-plaintext-cred"]);
        let other = set_of(&["held-plaintext-cred"]);
        let mut first = RedactionStream::new(set.clone());
        let mut second = RedactionStream::new(set.clone());
        let mut unrelated = RedactionStream::new(other.clone());
        assert!(first.feed(b"x held-plaintext-cre").starts_with(b"x "));
        assert!(second.feed(b"held-plain").is_empty());
        assert!(unrelated.feed(b"held-plain").is_empty());
        assert_eq!(first.pending_len(), 18);
        let retired = RedactionSet::new();
        retired.clear();
        set.clone().absorb(&retired);
        assert_eq!(first.pending_len(), 0);
        assert_eq!(second.pending_len(), 0);
        assert_eq!(unrelated.pending_len(), 10);
        assert_eq!(first.finish(), b"***");
        drop(second);
        // Dropped streams leave no registration behind to scrub.
        let _third = RedactionStream::new(set.clone());
        assert_eq!(set.streams.lock().unwrap().len(), 2);
    }

    #[test]
    fn retirement_clears_pending_and_fails_closed() {
        let set = set_of(&["abcdef"]);
        let mut s = RedactionStream::new(set.clone());
        assert_eq!(s.feed(b"ok abc"), b"ok ");
        assert_eq!(s.pending_len(), 3);
        set.clear();
        // Scrubbed by the retirement itself, before the stream is used again.
        assert_eq!(s.pending_len(), 0);
        assert_eq!(s.feed(b"def and more"), b"***");
        assert_eq!(s.pending_len(), 0);
        assert_eq!(s.feed(b"anything"), b"***");
        assert_eq!(s.finish(), b"***");
        assert_eq!(s.pending_len(), 0);
    }

    #[test]
    fn finish_flushes_held_text_and_a_closing_union_once() {
        let set = set_of(&["abcde", "defgh"]);
        let mut s = RedactionStream::new(set);
        assert_eq!(s.feed(b"abcdefg"), b"***");
        // Whole-buffer masking of "abcdefg" is "***fg": `defgh` never completes.
        assert_eq!(s.finish(), b"fg");
        // A trailing partial prefix is plain text once the stream ends.
        let set = set_of(&["abcdef"]);
        let mut s = RedactionStream::new(set);
        assert_eq!(s.feed(b"zabc"), b"z");
        assert_eq!(s.finish(), b"abc");
    }

    fn arb_secret() -> impl Strategy<Value = String> {
        // A tiny alphabet makes overlaps, nesting and duplicates common.
        proptest::collection::vec(
            prop_oneof![Just('a'), Just('b'), Just('c'), Just('é')],
            1..6,
        )
        .prop_map(|c| c.into_iter().collect())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]
        #[test]
        fn streaming_equals_whole_buffer_for_arbitrary_partitions(
            secrets in proptest::collection::vec(arb_secret(), 1..5),
            input in proptest::collection::vec(
                prop_oneof![Just(b'a'), Just(b'b'), Just(b'c'), Just(0xC3u8), Just(0xA9u8), Just(b'x')],
                0..60,
            ),
            cuts in proptest::collection::vec(0usize..64, 0..12),
        ) {
            let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
            let set = set_of(&refs);
            prop_assert_eq!(stream(&set, &input, &cuts), whole(&set, &input));
        }
    }
}
