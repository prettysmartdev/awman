//! The execution-private native lifecycle channel (WI 0070).
//!
//! awman-authored guest helpers (run only inside the container/guest) append
//! versioned JSONL records to a fixed file in an owned directory mounted at
//! `/awman/automation`. The host reads that file descriptor-relative with no
//! symlink following and decodes each record here, bound to a 256-bit
//! execution nonce, the execution ID and a strictly increasing sequence.
//! Records never supply host paths, processes, executables or cleanup
//! authority; this binding prevents cross-run attribution, it is not an
//! attestation against malicious guest code.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::data::automation_observation::{
    BlockedReason, NativeOutcome, Observation, ObservationBinding, ObservationKind,
    ObservationSource,
};
use crate::data::ids::ExecutionId;
use crate::data::settings_profile::NativeSessionId;
use crate::engine::agent_output::line_framer::LineFramer;

/// The fixed record file inside the channel directory.
pub const CHANNEL_FILE: &str = "events.jsonl";
/// The guest environment variables the helpers read.
pub const NONCE_ENV: &str = "AWMAN_AUTOMATION_NONCE";
pub const EXECUTION_ENV: &str = "AWMAN_AUTOMATION_EXECUTION";
pub const CHANNEL_DIR_ENV: &str = "AWMAN_AUTOMATION_DIR";

/// One lifecycle record line's bound.
pub const MAX_RECORD_BYTES: usize = 64 * 1024;
/// Sustained records per second and burst.
pub const RECORD_RATE: u32 = 256;
pub const RECORD_BURST: u32 = 512;
/// Unread bytes the host will hold for one channel before latching failure.
pub const MAX_PENDING_BYTES: usize = 1024 * 1024;
/// The whole spool's bound.
pub const MAX_SPOOL_BYTES: u64 = 16 * 1024 * 1024;

/// A random 256-bit per-execution nonce, hex encoded.
pub fn new_nonce() -> String {
    let a = uuid::Uuid::new_v4().simple().to_string();
    let b = uuid::Uuid::new_v4().simple().to_string();
    format!("{a}{b}")
}

#[derive(Debug, Deserialize)]
struct RawRecord {
    schema: u32,
    execution_id: String,
    nonce: String,
    sequence: u64,
    #[serde(default)]
    native_session_id: Option<String>,
    #[serde(default)]
    native_turn_id: Option<String>,
    root: bool,
    kind: String,
    #[serde(default)]
    payload: serde_json::Value,
}

/// Decodes one channel generation's records in order.
#[derive(Debug)]
pub struct ChannelDecoder {
    execution_id: ExecutionId,
    nonce: String,
    generation: u64,
    framer: LineFramer,
    next_sequence: u64,
    recent: VecDeque<(u64, [u8; 32])>,
    root_session: Option<NativeSessionId>,
    failed: bool,
    tokens: f64,
    last_refill: Option<Instant>,
    host_sequence: u64,
    /// Records that could not affect the root (wrong nonce/execution,
    /// subagents, unrelated sessions). Counted toward bounds.
    pub ignored: u64,
}

impl ChannelDecoder {
    pub fn new(execution_id: ExecutionId, nonce: String, generation: u64) -> Self {
        Self {
            execution_id,
            nonce,
            generation,
            framer: LineFramer::new(MAX_RECORD_BYTES),
            next_sequence: 1,
            recent: VecDeque::new(),
            root_session: None,
            failed: false,
            tokens: RECORD_BURST as f64,
            last_refill: None,
            host_sequence: 0,
            ignored: 0,
        }
    }

    pub fn failed(&self) -> bool {
        self.failed
    }

    fn emit(&mut self, kind: ObservationKind) -> Observation {
        self.host_sequence = crate::data::automation_observation::next_receive_sequence();
        Observation {
            binding: ObservationBinding {
                execution_id: self.execution_id.clone(),
                source: ObservationSource::Native,
                generation: self.generation,
                sequence: self.host_sequence,
            },
            kind,
        }
    }

    fn lose(&mut self, reason: &str) -> Vec<Observation> {
        if self.failed {
            return Vec::new();
        }
        self.failed = true;
        vec![self.emit(ObservationKind::SourceLost {
            reason: reason.into(),
        })]
    }

    /// A corrupt record in the execution-private channel. Only awman's
    /// helpers write this file, so an unparseable or oversized line is a
    /// damaged required record, not transport loss: it is a terminal
    /// protocol failure and can never be masked by an OSC fallback.
    fn corrupt(&mut self, reason: &str) -> Vec<Observation> {
        if self.failed {
            return Vec::new();
        }
        self.failed = true;
        vec![self.emit(ObservationKind::ProtocolFailure {
            reason: reason.into(),
        })]
    }

    /// Feed raw channel bytes (already redacted by the caller) received at `now`.
    pub fn feed(&mut self, bytes: &[u8], now: Instant) -> Vec<Observation> {
        let mut out = Vec::new();
        for line in self.framer.push(bytes) {
            if self.failed {
                break;
            }
            if line.truncated {
                out.extend(self.corrupt("lifecycle record exceeded its size bound"));
                break;
            }
            if !self.take_token(now) {
                out.extend(self.lose("lifecycle record rate exceeded"));
                break;
            }
            out.extend(self.record(&line.bytes));
        }
        out
    }

    /// The channel ended (EOF on a closed writer, reader cancellation).
    pub fn finish(&mut self) -> Vec<Observation> {
        if self.framer.finish().is_some() {
            return self.lose("truncated final lifecycle record");
        }
        self.lose("lifecycle channel ended")
    }

    fn take_token(&mut self, now: Instant) -> bool {
        if let Some(last) = self.last_refill {
            let elapsed = now.saturating_duration_since(last).as_secs_f64();
            self.tokens = (self.tokens + elapsed * RECORD_RATE as f64).min(RECORD_BURST as f64);
        }
        self.last_refill = Some(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    fn record(&mut self, bytes: &[u8]) -> Vec<Observation> {
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Vec::new();
        }
        let Ok(text) = std::str::from_utf8(bytes) else {
            return self.corrupt("lifecycle record is not UTF-8");
        };
        let Ok(raw) = serde_json::from_str::<RawRecord>(text) else {
            return self.corrupt("malformed lifecycle record");
        };
        if raw.nonce != self.nonce || raw.execution_id != self.execution_id.as_str() {
            self.ignored += 1;
            return Vec::new();
        }
        if raw.schema != 1 {
            return vec![self.emit(ObservationKind::ProtocolFailure {
                reason: format!("unsupported lifecycle schema {}", raw.schema),
            })];
        }
        let digest = sha256(bytes);
        if raw.sequence < self.next_sequence {
            // An identical duplicate is harmless; a conflicting one is not.
            return match self.recent.iter().find(|(s, _)| *s == raw.sequence) {
                Some((_, d)) if *d == digest => Vec::new(),
                _ => self.lose("conflicting duplicate lifecycle record"),
            };
        }
        if raw.sequence != self.next_sequence {
            return self.lose("lifecycle record sequence gap");
        }
        self.next_sequence += 1;
        self.recent.push_back((raw.sequence, digest));
        if self.recent.len() > 64 {
            self.recent.pop_front();
        }
        if !raw.root {
            self.ignored += 1;
            return Vec::new();
        }
        // Redaction runs before parsing. An identity it touched is no
        // longer the provider's identity: refuse it rather than adopt a
        // masked value (or keep secret material).
        let mark = crate::engine::auth::redaction::RedactionSet::MARK;
        if raw
            .native_session_id
            .as_deref()
            .is_some_and(|s| s.contains(mark))
            || raw
                .native_turn_id
                .as_deref()
                .is_some_and(|s| s.contains(mark))
        {
            return vec![self.emit(ObservationKind::ProtocolFailure {
                reason: "a native identity was damaged by secret redaction".into(),
            })];
        }
        let session = match raw.native_session_id.as_deref().map(NativeSessionId::parse) {
            Some(Ok(id)) => Some(id),
            Some(Err(_)) => {
                return vec![self.emit(ObservationKind::ProtocolFailure {
                    reason: "invalid native session identity".into(),
                })]
            }
            None => None,
        };
        if let (Some(root), Some(id)) = (&self.root_session, &session) {
            if root != id {
                self.ignored += 1;
                return Vec::new();
            }
        }
        if let Some(turn) = &raw.native_turn_id {
            if NativeSessionId::parse(turn).is_err() {
                return vec![self.emit(ObservationKind::ProtocolFailure {
                    reason: "invalid native turn identity".into(),
                })];
            }
        }
        let mut out = Vec::new();
        if self.root_session.is_none() {
            if let Some(id) = session {
                self.root_session = Some(id.clone());
                out.push(self.emit(ObservationKind::Identity {
                    native_session_id: id,
                }));
            }
        }
        match decode_kind(&raw) {
            Ok(Some(kind)) => out.push(self.emit(kind)),
            // Unknown additive kinds keep their order but authorize nothing.
            Ok(None) => {}
            Err(reason) => out.push(self.emit(ObservationKind::ProtocolFailure { reason })),
        }
        out
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

fn str_field<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(serde_json::Value::as_str)
}

fn decode_kind(raw: &RawRecord) -> Result<Option<ObservationKind>, String> {
    let p = &raw.payload;
    let blocked = |value: Option<&str>| match value {
        Some("auth") => Ok(BlockedReason::Auth),
        Some("permission") => Ok(BlockedReason::Permission),
        Some("input") | Some("question") | None => Ok(BlockedReason::Input),
        Some(other) => Err(format!("unknown blocked reason `{other}`")),
    };
    let turn_id = |what: &str| {
        raw.native_turn_id
            .clone()
            .ok_or_else(|| format!("{what} record without a native turn identity"))
    };
    Ok(Some(match raw.kind.as_str() {
        // Identity metadata only: the record's native_session_id carries
        // it. A session start is neither readiness nor work in progress.
        "identity" | "session_start" => {
            if raw.native_session_id.is_none() {
                return Err("identity record without native session".into());
            }
            return Ok(None);
        }
        "facility" => ObservationKind::Facility {
            full_lifecycle: p.get("full_lifecycle").and_then(serde_json::Value::as_bool),
            reason: str_field(p, "reason").map(str::to_owned),
        },
        "ready" => ObservationKind::Ready,
        // Acceptance must be correlated by the adapter's contract: the
        // native turn identity and the delivered prompt's digest.
        "prompt_accepted" => ObservationKind::PromptAccepted {
            native_turn_id: Some(turn_id("prompt_accepted")?),
            prompt_digest: Some(
                str_field(p, "prompt_sha256")
                    .ok_or("prompt_accepted record without a prompt digest")?
                    .to_owned(),
            ),
        },
        // Generic stop/session-end hooks are activity: provisional until a
        // correlated settlement.
        "working" | "stop" | "session_end" => ObservationKind::Working,
        "blocked" => ObservationKind::Blocked(blocked(str_field(p, "reason"))?),
        "continuation" => ObservationKind::Continuation,
        "settled" => ObservationKind::Settled {
            native_turn_id: Some(turn_id("settled")?),
            outcome: match str_field(p, "outcome") {
                // The answer field is the turn's output boundary; an
                // explicitly empty answer is still a confirmed answer.
                Some("success") => NativeOutcome::Succeeded {
                    final_message: Some(
                        str_field(p, "final_message")
                            .ok_or("settled success without its final answer boundary")?
                            .to_owned(),
                    ),
                },
                Some("failure") => NativeOutcome::Failed {
                    message: str_field(p, "message")
                        .unwrap_or("provider reported a failed turn")
                        .to_owned(),
                },
                _ => return Err("settled record without a valid outcome".into()),
            },
        },
        "resynchronized" => ObservationKind::Resynchronized {
            active: p
                .get("active")
                .and_then(serde_json::Value::as_bool)
                .ok_or("resynchronization without active state")?,
            blocked: match str_field(p, "blocked") {
                None => None,
                value => Some(blocked(value)?),
            },
        },
        _ => return Ok(None),
    }))
}

/// Host side of one channel: polls the owned record file and feeds the
/// decoder until cancelled, then reads what remains once, flushes the
/// redactor and ends the generation ([`ChannelDecoder::finish`]), so the
/// final records of a turn are not lost and a truncated tail is reported.
/// A record file that is not a regular file (symlink, FIFO, device) is a
/// source failure, never followed.
pub async fn read_channel(
    dir: std::path::PathBuf,
    decoder: std::sync::Arc<std::sync::Mutex<ChannelDecoder>>,
    sink: std::sync::Arc<dyn Fn(Vec<Observation>) + Send + Sync>,
    redaction: crate::engine::auth::redaction::RedactionSet,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut offset: u64 = 0;
    let mut redactor = crate::engine::auth::redaction::RedactionStream::new(redaction);
    let step = |offset: &mut u64,
                redactor: &mut crate::engine::auth::redaction::RedactionStream| {
        let observations = match read_new_bytes(&dir, *offset) {
            Ok(bytes) if !bytes.is_empty() => {
                *offset += bytes.len() as u64;
                let bytes = redactor.feed(&bytes);
                let mut d = decoder.lock().unwrap_or_else(|p| p.into_inner());
                if *offset > MAX_SPOOL_BYTES {
                    d.lose("lifecycle spool exceeded its bound")
                } else {
                    d.feed(&bytes, Instant::now())
                }
            }
            Ok(_) => Vec::new(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(_) => decoder
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .lose("lifecycle record file is unreadable or not a regular file"),
        };
        if !observations.is_empty() {
            sink(observations);
        }
    };
    loop {
        step(&mut offset, &mut redactor);
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    step(&mut offset, &mut redactor);
    let tail = redactor.finish();
    let observations = {
        let mut d = decoder.lock().unwrap_or_else(|p| p.into_inner());
        let mut out = if tail.is_empty() {
            Vec::new()
        } else {
            d.feed(&tail, Instant::now())
        };
        out.extend(d.finish());
        out
    };
    if !observations.is_empty() {
        sink(observations);
    }
}

/// One execution's owned channel directory and its binding. The host
/// directory is private (0700) and lives exactly as long as the mount guard
/// (through confirmed guest exit); it is mounted read-write at
/// [`crate::data::automation::AUTOMATION_GUEST_DIR`] and never names the
/// profile root, a host socket or a provider listener.
#[derive(Debug, Clone)]
pub struct ChannelMount {
    guard: crate::engine::container::options::MountGuard,
    execution_id: ExecutionId,
    nonce: String,
}

impl ChannelMount {
    /// Create a fresh channel directory under `temp_root` with a new nonce.
    pub fn create(temp_root: &std::path::Path, execution_id: ExecutionId) -> std::io::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("awman-automation-")
            .tempdir_in(temp_root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            guard: crate::engine::container::options::MountGuard::new_dir(dir),
            execution_id,
            nonce: new_nonce(),
        })
    }

    pub fn host_dir(&self) -> &std::path::Path {
        self.guard.host_path()
    }

    pub fn execution_id(&self) -> &ExecutionId {
        &self.execution_id
    }

    /// A decoder for this channel's first generation.
    pub fn decoder(&self) -> ChannelDecoder {
        ChannelDecoder::new(self.execution_id.clone(), self.nonce.clone(), 1)
    }

    /// The container options that expose the channel to guest helpers:
    /// the owned directory plus the binding the helpers stamp on records.
    /// The nonce binds records to this run; it is not a credential.
    pub fn container_options(&self) -> Vec<crate::engine::container::options::ContainerOption> {
        use crate::engine::container::options::{ContainerOption, EnvLiteral};
        vec![
            ContainerOption::TemporaryDirectoryMount {
                container_path: std::path::PathBuf::from(
                    crate::data::automation::AUTOMATION_GUEST_DIR,
                ),
                env_var: Some(CHANNEL_DIR_ENV.into()),
                guard: self.guard.clone(),
            },
            ContainerOption::EnvLiteral(EnvLiteral {
                key: EXECUTION_ENV.into(),
                value: self.execution_id.as_str().to_owned(),
            }),
            ContainerOption::EnvLiteral(EnvLiteral {
                key: NONCE_ENV.into(),
                value: self.nonce.clone(),
            }),
        ]
    }
}

/// Read the bytes after `offset` from the fixed record file, refusing
/// symlinks and special files. At most [`MAX_PENDING_BYTES`] per read.
fn read_new_bytes(dir: &std::path::Path, offset: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let path = dir.join(CHANNEL_FILE);
    let meta = std::fs::symlink_metadata(&path)?;
    if !meta.file_type().is_file() {
        return Err(std::io::Error::other(
            "lifecycle record file is not a regular file",
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::other(
            "lifecycle record file is not a regular file",
        ));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(MAX_PENDING_BYTES as u64)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(d: &ChannelDecoder, seq: u64, kind: &str, extra: serde_json::Value) -> Vec<u8> {
        let mut value = serde_json::json!({
            "schema": 1,
            "execution_id": d.execution_id.as_str(),
            "nonce": d.nonce,
            "sequence": seq,
            "native_session_id": "sess-1",
            "native_turn_id": null,
            "root": true,
            "kind": kind,
            "payload": {},
        });
        if let serde_json::Value::Object(extra) = extra {
            for (k, v) in extra {
                value[k] = v;
            }
        }
        let mut line = serde_json::to_vec(&value).unwrap();
        line.push(b'\n');
        line
    }

    fn kinds(obs: &[Observation]) -> Vec<ObservationKind> {
        obs.iter().map(|o| o.kind.clone()).collect()
    }

    #[test]
    fn records_bind_identity_and_decode_in_order() {
        let mut d = ChannelDecoder::new(ExecutionId::new(), new_nonce(), 1);
        let now = Instant::now();
        let mut bytes = record(&d, 1, "session_start", serde_json::json!({}));
        bytes.extend(record(&d, 2, "ready", serde_json::json!({})));
        bytes.extend(record(
            &d,
            3,
            "settled",
            serde_json::json!({"native_turn_id":"t1","payload":{"outcome":"success","final_message":""}}),
        ));
        let out = d.feed(&bytes, now);
        assert_eq!(
            kinds(&out),
            vec![
                ObservationKind::Identity {
                    native_session_id: NativeSessionId::parse("sess-1").unwrap()
                },
                // The session start is identity metadata only.
                ObservationKind::Ready,
                ObservationKind::Settled {
                    native_turn_id: Some("t1".into()),
                    outcome: NativeOutcome::Succeeded {
                        final_message: Some(String::new())
                    }
                }
            ]
        );
    }

    #[test]
    fn wrong_nonce_subagents_and_unrelated_sessions_cannot_affect_root() {
        let mut d = ChannelDecoder::new(ExecutionId::new(), new_nonce(), 1);
        let now = Instant::now();
        let mut bytes = record(&d, 1, "ready", serde_json::json!({"nonce": "f".repeat(64)}));
        bytes.extend(record(&d, 1, "ready", serde_json::json!({"root": false})));
        bytes.extend(record(&d, 2, "session_start", serde_json::json!({})));
        bytes.extend(record(
            &d,
            3,
            "ready",
            serde_json::json!({"native_session_id": "other"}),
        ));
        let out = d.feed(&bytes, now);
        assert!(!kinds(&out).contains(&ObservationKind::Ready), "{out:?}");
        assert_eq!(d.ignored, 3);
    }

    #[test]
    fn gaps_conflicts_and_eof_truncation_lose_the_source_but_duplicates_do_not() {
        let now = Instant::now();
        let mut d = ChannelDecoder::new(ExecutionId::new(), new_nonce(), 1);
        let one = record(&d, 1, "working", serde_json::json!({}));
        d.feed(&one, now);
        assert!(d.feed(&one, now).is_empty(), "identical duplicate ignored");
        let conflicting = record(&d, 1, "ready", serde_json::json!({}));
        assert!(matches!(
            kinds(&d.feed(&conflicting, now))[..],
            [ObservationKind::SourceLost { .. }]
        ));

        let mut d = ChannelDecoder::new(ExecutionId::new(), new_nonce(), 1);
        let gap = record(&d, 2, "ready", serde_json::json!({}));
        assert!(matches!(
            kinds(&d.feed(&gap, now))[..],
            [ObservationKind::SourceLost { .. }]
        ));
        assert!(d.failed());

        let mut d = ChannelDecoder::new(ExecutionId::new(), new_nonce(), 1);
        let mut huge = vec![b'x'; MAX_RECORD_BYTES + 10];
        huge.push(b'\n');
        // An oversized record is a corrupt required record (finding 8).
        assert!(matches!(
            kinds(&d.feed(&huge, now))[..],
            [ObservationKind::ProtocolFailure { .. }]
        ));

        let mut d = ChannelDecoder::new(ExecutionId::new(), new_nonce(), 1);
        d.feed(b"{\"schema\":1", now);
        assert!(matches!(
            kinds(&d.finish())[..],
            [ObservationKind::SourceLost { .. }]
        ));
    }

    #[test]
    fn invalid_required_semantics_are_protocol_failures_and_unknown_kinds_are_ignored() {
        let now = Instant::now();
        let mut d = ChannelDecoder::new(ExecutionId::new(), new_nonce(), 1);
        let mut bytes = record(&d, 1, "settled", serde_json::json!({"payload":{}}));
        bytes.extend(record(&d, 2, "future_kind", serde_json::json!({})));
        let out = kinds(&d.feed(&bytes, now));
        assert!(matches!(out[1], ObservationKind::ProtocolFailure { .. }));
        assert_eq!(out.len(), 2, "{out:?}");
    }

    #[test]
    fn rate_limit_latches_failure() {
        let now = Instant::now();
        let mut d = ChannelDecoder::new(ExecutionId::new(), new_nonce(), 1);
        let mut bytes = Vec::new();
        for seq in 1..=(RECORD_BURST as u64 + 1) {
            bytes.extend(record(&d, seq, "working", serde_json::json!({})));
        }
        let out = kinds(&d.feed(&bytes, now));
        assert!(matches!(
            out.last(),
            Some(ObservationKind::SourceLost { .. })
        ));
    }

    #[test]
    fn nonce_is_256_bits_of_hex() {
        let n = new_nonce();
        assert_eq!(n.len(), 64);
        assert!(n.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(n, new_nonce());
    }

    #[test]
    fn corrupt_or_incomplete_required_records_are_protocol_failures_not_source_loss() {
        let now = Instant::now();
        for bad in [&b"{not json\n"[..], &b"\xff\xfe\n"[..]] {
            let mut d = ChannelDecoder::new(ExecutionId::new(), new_nonce(), 1);
            let out = kinds(&d.feed(bad, now));
            assert!(
                matches!(out[..], [ObservationKind::ProtocolFailure { .. }]),
                "{out:?}"
            );
            assert!(d.failed());
        }
        // Required correlation fields and the answer boundary are mandatory.
        for (kind, extra) in [
            (
                "prompt_accepted",
                serde_json::json!({"payload": {"prompt_sha256": "d"}}),
            ),
            (
                "prompt_accepted",
                serde_json::json!({"native_turn_id": "t1"}),
            ),
            (
                "settled",
                serde_json::json!({"payload": {"outcome": "success", "final_message": "x"}}),
            ),
            (
                "settled",
                serde_json::json!({"native_turn_id": "t1", "payload": {"outcome": "success"}}),
            ),
        ] {
            let mut d = ChannelDecoder::new(ExecutionId::new(), new_nonce(), 1);
            let line = record(&d, 1, kind, extra);
            let out = kinds(&d.feed(&line, now));
            assert!(
                matches!(out.last(), Some(ObservationKind::ProtocolFailure { .. })),
                "{kind}: {out:?}"
            );
        }
        // An identity touched by redaction is refused, never adopted.
        let mut d = ChannelDecoder::new(ExecutionId::new(), new_nonce(), 1);
        let line = record(
            &d,
            1,
            "session_start",
            serde_json::json!({"native_session_id": "sess-***"}),
        );
        let out = kinds(&d.feed(&line, now));
        assert!(
            matches!(out[..], [ObservationKind::ProtocolFailure { .. }]),
            "{out:?}"
        );
    }
}
