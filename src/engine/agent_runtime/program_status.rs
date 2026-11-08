//! OSC 7501 on a byte stream (WI 0141, Layer 1, runtime-neutral).
//!
//! [`Osc7501Scanner`] finds `ESC ] 7501 ; … (BEL | ESC \)` and RIS (`ESC c`)
//! in agent output, one chunk at a time, keeping state across chunk
//! boundaries. [`ProgramStatusTracker`] folds accepted reports into a
//! `ProgramStatusSet` and owns the `reporting` latch that makes the agent's
//! own report supersede the silence heuristic. [`ReportedGate`] is the
//! debounce the `StuckDetector` applies before publishing
//! `StuckEvent::Reported`.
//!
//! This lives under `agent_runtime` so both tiers can use it: the container
//! bridges (`engine::container::io_bridge`, `attach_socket`,
//! `builtin::exec_bridge`) and the sandbox bridge (`engine::sandbox::dsbx`)
//! import it, while sandbox never imports container (WI 0090).

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::data::program_status::{
    EffectiveStatus, ParsedOsc7501, ProgramStatusSet, MAX_SEQUENCE_BYTES,
};

/// The fixed reply to the feature-detection query, written into the agent's
/// stdin on the PTY path only. No `states=`/`kinds=` fields: the full default
/// sets. It carries no report data (spec security section).
pub const OSC7501_QUERY_REPLY: &[u8] = b"\x1b]7501;?\x1b\\";

/// Transitions (state or kind) are published at most this often per agent.
pub const REPORTED_TRANSITION_DEBOUNCE: Duration = Duration::from_millis(250);
/// Progress- or msg-only changes are published at most this often, for
/// display only.
pub const REPORTED_DISPLAY_INTERVAL: Duration = Duration::from_secs(1);

/// What the scanner found in one chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanEvent {
    /// A complete OSC 7501 sequence, parsed by `data::program_status`.
    Osc(ParsedOsc7501),
    /// RIS (`ESC c`): the agent reset its terminal.
    Reset,
}

/// One `feed` call's output: the bytes to forward and the events found.
///
/// `forward` is `Cow::Borrowed(chunk)` whenever nothing was removed, which is
/// every chunk without a query sequence. Reports are forwarded byte for byte;
/// only query sequences (`7501;?`) are cut out, so an outer real terminal
/// never answers as well.
#[derive(Debug)]
pub struct ScanResult<'a> {
    pub forward: Cow<'a, [u8]>,
    pub events: Vec<ScanEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Constructed by `Osc7501Scanner::feed` once program-status-protocol fills it in.
#[allow(dead_code)]
enum ScanState {
    Ground,
    /// Saw `ESC`; the next byte decides.
    Escape,
    /// Inside `ESC ] …` before we know whether it is 7501.
    OscPrefix,
    /// Inside a 7501 body, collecting until BEL or ST.
    Osc7501Body,
    /// Inside a 7501 body and saw `ESC`; `\` completes ST.
    Osc7501BodyEscape,
    /// Inside some other OSC: pass bytes through, just find the end.
    OtherOsc,
    OtherOscEscape,
}

/// A small state machine over agent output. One per stream (stdout and
/// stderr each get their own in piped mode).
///
/// Buffers at most [`crate::data::program_status::MAX_SEQUENCE_BYTES`] of one
/// sequence; on overflow it drops that sequence and returns to ground. It
/// never holds normal output back waiting for a terminator: bytes are
/// forwarded as they arrive, and only the query form is removed.
#[derive(Debug)]
pub struct Osc7501Scanner {
    state: ScanState,
    /// Bytes of the sequence in progress (introducer included), so a split
    /// non-7501 OSC or a split query can be forwarded or dropped whole.
    pending: Vec<u8>,
    /// Whether the current 7501 candidate has already been forwarded as
    /// ordinary output. Exact queries stay buffered until their terminator.
    sequence_forwarded: bool,
}

impl Default for Osc7501Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Osc7501Scanner {
    pub fn new() -> Self {
        Self {
            state: ScanState::Ground,
            pending: Vec::new(),
            sequence_forwarded: false,
        }
    }

    /// Scan one chunk. Plain text and CSI/SGR-only chunks in ground state
    /// are allocation-free. A split candidate may require owned output.
    pub fn feed<'a>(&mut self, chunk: &'a [u8]) -> ScanResult<'a> {
        if self.state == ScanState::Ground
            && chunk.last() != Some(&0x1b)
            && !chunk
                .windows(2)
                .any(|bytes| bytes[0] == 0x1b && matches!(bytes[1], b']' | b'c' | 0x1b))
        {
            return ScanResult {
                forward: Cow::Borrowed(chunk),
                events: Vec::new(),
            };
        }

        const PREFIX: &[u8] = b"\x1b]7501;";
        let mut output = Vec::with_capacity(chunk.len());
        let mut events = Vec::new();
        for &byte in chunk {
            if matches!(byte, 0x18 | 0x1a) && self.state != ScanState::Ground {
                output.extend(self.finish());
                output.push(byte);
                continue;
            }
            // ESC aborts an OSC unless it is the ST terminator. Reprocess
            // the new escape so a query after an interrupted OSC is removed.
            if self.state == ScanState::Osc7501BodyEscape && byte != b'\\' {
                self.pending.pop();
                if !self.sequence_forwarded {
                    output.extend_from_slice(&self.pending);
                }
                self.pending.clear();
                self.pending.push(0x1b);
                self.sequence_forwarded = false;
                self.state = ScanState::Escape;
            }
            if matches!(
                self.state,
                ScanState::Osc7501Body | ScanState::Osc7501BodyEscape
            ) && self.pending.len() == MAX_SEQUENCE_BYTES
            {
                if !self.sequence_forwarded {
                    output.extend_from_slice(&self.pending);
                }
                self.pending.clear();
                self.sequence_forwarded = false;
                self.state = ScanState::Ground;
                if byte == 0x1b {
                    self.pending.push(byte);
                    self.state = ScanState::Escape;
                } else {
                    output.push(byte);
                }
                continue;
            }
            match self.state {
                ScanState::Ground => {
                    if byte == 0x1b {
                        self.sequence_forwarded = false;
                        self.pending.clear();
                        self.pending.push(byte);
                        self.state = ScanState::Escape;
                    } else {
                        output.push(byte);
                    }
                }
                ScanState::Escape => {
                    if byte == 0x1b {
                        output.extend_from_slice(&self.pending);
                        self.pending.clear();
                        self.pending.push(byte);
                        continue;
                    }
                    self.pending.push(byte);
                    match byte {
                        b'c' => {
                            output.extend_from_slice(&self.pending);
                            self.pending.clear();
                            self.state = ScanState::Ground;
                            events.push(ScanEvent::Reset);
                        }
                        b']' => self.state = ScanState::OscPrefix,
                        _ => {
                            output.extend_from_slice(&self.pending);
                            self.pending.clear();
                            self.state = ScanState::Ground;
                        }
                    }
                }
                ScanState::OscPrefix => {
                    if byte == 0x1b {
                        output.extend_from_slice(&self.pending);
                        self.pending.clear();
                        self.pending.push(byte);
                        self.state = ScanState::Escape;
                        continue;
                    }
                    self.pending.push(byte);
                    let matches_prefix = self.pending.len() <= PREFIX.len()
                        && self
                            .pending
                            .iter()
                            .zip(PREFIX)
                            .all(|(got, want)| got == want);
                    if matches_prefix {
                        if self.pending.len() == PREFIX.len() {
                            self.sequence_forwarded = false;
                            self.state = ScanState::Osc7501Body;
                        }
                    } else {
                        output.extend_from_slice(&self.pending);
                        self.pending.clear();
                        self.state = if byte == 0x07 {
                            ScanState::Ground
                        } else if byte == 0x1b {
                            ScanState::OtherOscEscape
                        } else {
                            ScanState::OtherOsc
                        };
                    }
                }
                ScanState::OtherOsc => {
                    if byte == 0x1b {
                        self.pending.clear();
                        self.pending.push(byte);
                        self.state = ScanState::Escape;
                    } else if byte == 0x07 {
                        output.push(byte);
                        self.state = ScanState::Ground;
                    } else {
                        output.push(byte);
                    }
                }
                ScanState::OtherOscEscape => {
                    output.push(byte);
                    self.state = if byte == b'\\' {
                        ScanState::Ground
                    } else if byte == 0x1b {
                        ScanState::OtherOscEscape
                    } else {
                        ScanState::OtherOsc
                    };
                }
                ScanState::Osc7501Body => {
                    self.pending.push(byte);
                    if byte == 0x07 {
                        finish_osc(
                            &mut self.pending,
                            false,
                            self.sequence_forwarded,
                            &mut output,
                            &mut events,
                        );
                        self.state = ScanState::Ground;
                    } else if byte == 0x1b {
                        self.state = ScanState::Osc7501BodyEscape;
                    } else if self.sequence_forwarded {
                        output.push(byte);
                    }
                }
                ScanState::Osc7501BodyEscape => {
                    self.pending.push(byte);
                    if byte == b'\\' {
                        finish_osc(
                            &mut self.pending,
                            true,
                            self.sequence_forwarded,
                            &mut output,
                            &mut events,
                        );
                        self.state = ScanState::Ground;
                    } else if byte == 0x07 {
                        finish_osc(
                            &mut self.pending,
                            false,
                            self.sequence_forwarded,
                            &mut output,
                            &mut events,
                        );
                        self.state = ScanState::Ground;
                    } else {
                        if self.sequence_forwarded {
                            output.push(byte);
                        }
                        self.state = if byte == 0x1b {
                            ScanState::Osc7501BodyEscape
                        } else {
                            ScanState::Osc7501Body
                        };
                    }
                }
            }

            if matches!(
                self.state,
                ScanState::Osc7501Body | ScanState::Osc7501BodyEscape
            ) && !self.sequence_forwarded
                && !is_query_prefix(&self.pending)
            {
                output.extend_from_slice(&self.pending);
                self.sequence_forwarded = true;
            }
        }
        ScanResult {
            forward: if output == chunk {
                Cow::Borrowed(chunk)
            } else {
                Cow::Owned(output)
            },
            events,
        }
    }

    /// Forward an incomplete candidate at EOF; completed queries alone are
    /// removed. Calling this also resets the scanner for a subsequent stream.
    pub fn finish(&mut self) -> Vec<u8> {
        let bytes = if !self.sequence_forwarded {
            std::mem::take(&mut self.pending)
        } else if self.state == ScanState::Osc7501BodyEscape {
            vec![0x1b]
        } else {
            Vec::new()
        };
        self.pending.clear();
        self.sequence_forwarded = false;
        self.state = ScanState::Ground;
        bytes
    }

    /// Whether a sequence is in progress across a chunk boundary.
    pub fn is_mid_sequence(&self) -> bool {
        self.state != ScanState::Ground
    }
}

/// The accepted reports of one agent execution plus the `reporting` latch.
///
/// Shared (`Arc<Mutex<_>>`) between the reader threads that feed it and the
/// `StuckDetector` that reads it. Dropped and recreated per execution and per
/// new step/container, so support is re-learned (a new container may run a
/// different agent).
#[derive(Debug, Default)]
pub struct ProgramStatusTracker {
    generation: u64,
    ended: bool,
    set: ProgramStatusSet,
    /// Set by the first accepted report (any state, including `clear`) and
    /// kept for the rest of the execution. Malformed reports never set it.
    reporting: bool,
    /// When the last accepted report arrived (for the TUI's "silent Nm").
    last_report_at: Option<Instant>,
    /// What [`Self::exit_status`] answers once the execution ended: the
    /// effective status at that moment (outer `None`: never reported).
    ended_with: Option<Option<EffectiveStatus>>,
}

pub type SharedProgramStatusTracker = Arc<Mutex<ProgramStatusTracker>>;

impl ProgramStatusTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn shared() -> SharedProgramStatusTracker {
        Arc::new(Mutex::new(Self::new()))
    }

    /// Fold one scanner event in. Returns `true` when a report was accepted
    /// (which also sets the latch). `Query` and `Ignored` change nothing.
    pub fn apply(&mut self, event: &ScanEvent, now: Instant) -> bool {
        if self.ended {
            return false;
        }
        match event {
            ScanEvent::Osc(ParsedOsc7501::Report(report)) => {
                if report
                    .id
                    .as_deref()
                    .is_some_and(|id| !crate::data::program_status::is_valid_id(id))
                {
                    return false;
                }
                self.set.apply(report);
                self.reporting = true;
                self.last_report_at = Some(now);
                true
            }
            ScanEvent::Reset => {
                self.set.reset();
                if self.reporting {
                    self.last_report_at = Some(now);
                }
                false
            }
            ScanEvent::Osc(_) => false,
        }
    }

    /// End an execution or start a new step, relearning support next time.
    pub fn reset(&mut self) {
        let generation = self.generation.wrapping_add(1);
        *self = Self::new();
        self.generation = generation;
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Release status at execution end and reject any late reader chunks.
    pub(crate) fn end_execution(&mut self) {
        let last = self.exit_status();
        self.reset();
        self.ended = true;
        self.ended_with = last;
    }

    /// The agent's last effective status for this execution, kept across
    /// [`Self::end_execution`] so the consumer finalising the exited step
    /// still sees a report the detector never published. Outer `None` when
    /// the agent never reported; `Some(None)` when it cleared its records.
    pub fn exit_status(&self) -> Option<Option<EffectiveStatus>> {
        if self.ended {
            self.ended_with.clone()
        } else {
            self.reporting.then(|| self.effective())
        }
    }

    pub(crate) fn is_ended(&self) -> bool {
        self.ended
    }

    /// Whether the agent has ever sent an accepted report this execution.
    pub fn reporting(&self) -> bool {
        self.reporting
    }

    /// The current aggregate, `None` when the agent holds no records (never
    /// reported, or cleared everything).
    pub fn effective(&self) -> Option<EffectiveStatus> {
        self.set.effective()
    }

    pub fn last_report_at(&self) -> Option<Instant> {
        self.last_report_at
    }

    pub fn set(&self) -> &ProgramStatusSet {
        &self.set
    }
}

/// Decides when a changed aggregate becomes a `StuckEvent::Reported`.
///
/// Transitions (state or kind changed) are deduplicated and debounced to one
/// per [`REPORTED_TRANSITION_DEBOUNCE`]; progress/msg-only changes pass at
/// most once per [`REPORTED_DISPLAY_INTERVAL`]. Lives here so the one
/// `StuckDetector` applies it and no consumer re-implements it.
#[derive(Debug, Default)]
pub struct ReportedGate {
    initialized: bool,
    last_published: Option<EffectiveStatus>,
    last_transition_at: Option<Instant>,
    last_display_at: Option<Instant>,
}

impl ReportedGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Outer `Some` means publish; inner `None` means records were cleared.
    pub fn next(
        &mut self,
        now: Instant,
        effective: Option<&EffectiveStatus>,
    ) -> Option<Option<EffectiveStatus>> {
        if (!self.initialized && effective.is_none())
            || (self.initialized && self.last_published.as_ref() == effective)
        {
            return None;
        }
        let transition = match (self.last_published.as_ref(), effective) {
            (Some(old), Some(new)) => !old.same_transition(new),
            _ => true,
        };
        let (last, interval) = if transition {
            (self.last_transition_at, REPORTED_TRANSITION_DEBOUNCE)
        } else {
            (self.last_display_at, REPORTED_DISPLAY_INTERVAL)
        };
        if last.is_some_and(|last| now.duration_since(last) < interval)
            || self
                .last_display_at
                .is_some_and(|last| now.duration_since(last) < REPORTED_TRANSITION_DEBOUNCE)
        {
            return None;
        }
        self.last_published = effective.cloned();
        self.initialized = true;
        if transition {
            self.last_transition_at = Some(now);
        }
        self.last_display_at = Some(now);
        Some(self.last_published.clone())
    }
}

/// One stream's scanner feeding the execution's shared tracker. The callback
/// receives only the fixed query reply, and is never invoked for piped I/O.
pub struct ProgramStatusStream {
    scanner: Osc7501Scanner,
    tracker: SharedProgramStatusTracker,
    config: ProgramStatusBridgeConfig,
}

impl ProgramStatusStream {
    pub fn finish(&mut self) -> Vec<u8> {
        self.scanner.finish()
    }
    pub fn new(tracker: SharedProgramStatusTracker, config: ProgramStatusBridgeConfig) -> Self {
        Self {
            scanner: Osc7501Scanner::new(),
            tracker,
            config,
        }
    }

    pub fn feed<'a>(&mut self, chunk: &'a [u8], mut reply: impl FnMut(&[u8])) -> Cow<'a, [u8]> {
        let result = self.scanner.feed(chunk);
        if self.config.enabled {
            for event in &result.events {
                if matches!(event, ScanEvent::Osc(ParsedOsc7501::Query))
                    && self.config.answer_queries
                {
                    reply(OSC7501_QUERY_REPLY);
                }
                if let Ok(mut tracker) = self.tracker.lock() {
                    tracker.apply(event, Instant::now());
                }
            }
        }
        result.forward
    }
}

fn is_query_prefix(pending: &[u8]) -> bool {
    let body = &pending[b"\x1b]7501;".len()..];
    body.is_empty() || body == b"?" || body == b"?\x1b"
}

fn finish_osc(
    pending: &mut Vec<u8>,
    st: bool,
    already_forwarded: bool,
    output: &mut Vec<u8>,
    events: &mut Vec<ScanEvent>,
) {
    let terminator_len = if st { 2 } else { 1 };
    let body_start = b"\x1b]7501;".len();
    let body_end = pending.len().saturating_sub(terminator_len);
    let parsed = crate::data::program_status::parse_body(&pending[body_start..body_end]);
    if parsed != ParsedOsc7501::Query {
        if already_forwarded {
            output.extend_from_slice(&pending[pending.len() - terminator_len..]);
        } else {
            output.extend_from_slice(pending);
        }
    }
    events.push(ScanEvent::Osc(parsed));
    pending.clear();
}

/// How a bridge treats OSC 7501 for one execution. Resolved by Layer 2 from
/// `agents.programStatus.enabled` and the launch path (PTY vs piped) and
/// carried on the resolved runtime options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgramStatusBridgeConfig {
    /// `agents.programStatus.enabled`. When `false` the scanner still removes
    /// queries, but the bridge neither replies nor publishes.
    pub enabled: bool,
    /// PTY path only: answer `Query` with [`OSC7501_QUERY_REPLY`] through the
    /// bridge's retained stdin sender. Piped mode never answers.
    pub answer_queries: bool,
}

impl Default for ProgramStatusBridgeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            answer_queries: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::program_status::ReportState;

    fn scan_all(parts: &[&[u8]]) -> (Vec<u8>, Vec<ScanEvent>) {
        let mut scanner = Osc7501Scanner::new();
        let mut forward = Vec::new();
        let mut events = Vec::new();
        for part in parts {
            let result = scanner.feed(part);
            forward.extend_from_slice(&result.forward);
            events.extend(result.events);
        }
        (forward, events)
    }

    #[test]
    fn recognizes_bel_and_st_terminators_and_forwards_reports_unchanged() {
        for sequence in [
            b"\x1b]7501;state=working\x07".as_slice(),
            b"\x1b]7501;state=done\x1b\\".as_slice(),
        ] {
            let mut scanner = Osc7501Scanner::new();
            let result = scanner.feed(sequence);
            assert_eq!(&*result.forward, sequence);
            assert!(matches!(
                result.events.as_slice(),
                [ScanEvent::Osc(ParsedOsc7501::Report(_))]
            ));
        }
    }

    #[test]
    fn every_split_offset_preserves_and_parses_complete_sequence() {
        let sequence = b"pre\x1b]7501;state=blocked:kind=question:msg=aGk\x1b\\post";
        for split in 0..=sequence.len() {
            let (forward, events) = scan_all(&[&sequence[..split], &sequence[split..]]);
            assert_eq!(forward, sequence, "split at {split}");
            assert_eq!(events.len(), 1, "split at {split}");
            assert!(
                matches!(&events[0], ScanEvent::Osc(ParsedOsc7501::Report(r)) if r.state == ReportState::State(crate::data::program_status::ProgramState::Blocked))
            );
        }
    }

    #[test]
    fn query_is_removed_but_surrounding_bytes_and_reports_are_preserved() {
        let input = b"before\x1b]7501;?\x1b\\middle\x1b]7501;state=done\x07after";
        let mut scanner = Osc7501Scanner::new();
        let result = scanner.feed(input);
        assert_eq!(
            &*result.forward,
            b"beforemiddle\x1b]7501;state=done\x07after"
        );
        assert_eq!(
            result.events,
            vec![
                ScanEvent::Osc(ParsedOsc7501::Query),
                ScanEvent::Osc(ParsedOsc7501::Report(
                    crate::data::program_status::ProgramStatusReport {
                        id: None,
                        state: ReportState::State(crate::data::program_status::ProgramState::Done),
                        kind: None,
                        progress: None,
                        app: None,
                        title: None,
                        msg: None,
                    }
                )),
            ]
        );
    }

    #[test]
    fn query_is_removed_at_every_split_offset() {
        let query = b"left\x1b]7501;?\x1b\\right";
        for split in 0..=query.len() {
            let (forward, events) = scan_all(&[&query[..split], &query[split..]]);
            assert_eq!(forward, b"leftright", "split at {split}");
            assert_eq!(
                events,
                vec![ScanEvent::Osc(ParsedOsc7501::Query)],
                "split at {split}"
            );
        }
    }

    #[test]
    fn report_bytes_are_forwarded_before_the_terminator_arrives() {
        let mut scanner = Osc7501Scanner::new();
        let prefix = b"\x1b]7501;state=working";
        let first = scanner.feed(prefix);
        assert_eq!(&*first.forward, prefix);
        assert!(first.events.is_empty());
        let second = scanner.feed(b"\x07");
        assert_eq!(&*second.forward, b"\x07");
        assert_eq!(second.events.len(), 1);
    }

    #[test]
    fn handles_lone_trailing_escape_and_ris() {
        let mut scanner = Osc7501Scanner::new();
        let first = scanner.feed(b"hello\x1b");
        assert_eq!(&*first.forward, b"hello");
        assert!(scanner.is_mid_sequence());
        let second = scanner.feed(b"cworld");
        assert_eq!(&*second.forward, b"\x1bcworld");
        assert_eq!(second.events, vec![ScanEvent::Reset]);
        assert!(!scanner.is_mid_sequence());
    }

    #[test]
    fn drops_overflowed_candidate_from_parser_then_resynchronizes() {
        let huge = [
            b"\x1b]7501;".as_slice(),
            &vec![b'x'; MAX_SEQUENCE_BYTES + 1],
            b"\x07",
        ]
        .concat();
        let valid = b"\x1b]7501;?\x07";
        let mut scanner = Osc7501Scanner::new();
        let mut events = Vec::new();
        let mut forward = Vec::new();
        for chunk in [&huge[..], &valid[..]] {
            let result = scanner.feed(chunk);
            events.extend(result.events);
            forward.extend_from_slice(&result.forward);
        }
        assert_eq!(forward, huge);
        assert_eq!(events, vec![ScanEvent::Osc(ParsedOsc7501::Query)]);
        assert!(!scanner.is_mid_sequence());
    }

    #[test]
    fn other_osc_sequences_are_untouched_and_plain_chunks_are_borrowed() {
        for input in [
            b"\x1b]0;title\x07".as_slice(),
            b"\x1b]52;c;abc\x1b\\".as_slice(),
            b"\x1b]9;4;1;0\x07".as_slice(),
        ] {
            let mut scanner = Osc7501Scanner::new();
            let result = scanner.feed(input);
            assert_eq!(&*result.forward, input);
            assert!(result.events.is_empty());
        }
        let mut scanner = Osc7501Scanner::new();
        let chunk = b"ordinary output";
        let result = scanner.feed(chunk);
        assert!(matches!(result.forward, Cow::Borrowed(_)));
        assert!(result.events.is_empty());
    }

    /// Review finding: the status at exit survives `end_execution`, so the
    /// consumer finalising the step still sees a report the detector never
    /// published; a new step (`reset`) forgets it.
    #[test]
    fn exit_status_survives_end_of_execution() {
        let mut tracker = ProgramStatusTracker::new();
        assert_eq!(tracker.exit_status(), None, "never reported");
        let now = Instant::now();
        assert!(tracker.apply(
            &ScanEvent::Osc(crate::data::program_status::parse_body(b"state=error")),
            now
        ));
        tracker.end_execution();
        assert!(!tracker.reporting());
        let status = tracker
            .exit_status()
            .expect("reported")
            .expect("not cleared");
        assert_eq!(
            status.state,
            crate::data::program_status::ProgramState::Error
        );
        tracker.reset();
        assert_eq!(tracker.exit_status(), None);

        let mut cleared = ProgramStatusTracker::new();
        cleared.apply(
            &ScanEvent::Osc(crate::data::program_status::parse_body(b"state=clear")),
            now,
        );
        cleared.end_execution();
        assert_eq!(cleared.exit_status(), Some(None));
    }
}

#[cfg(test)]
mod review_regressions {
    use super::*;
    #[test]
    fn interrupted_sequences_cannot_hide_queries_at_any_split() {
        for bytes in [
            b"\x1b\x1b]7501;?\x07".as_slice(),
            b"\x1b]0;t\x1b]7501;?\x1b\\",
            b"\x1b]7501;state=working\x18\x1b]7501;?\x07",
            b"\x1b]7501;state=working\x1b]7501;?\x07",
        ] {
            for split in 0..=bytes.len() {
                let mut scanner = Osc7501Scanner::new();
                let mut output = Vec::new();
                let mut queries = 0;
                for chunk in [&bytes[..split], &bytes[split..]] {
                    let result = scanner.feed(chunk);
                    output.extend_from_slice(&result.forward);
                    queries += result
                        .events
                        .iter()
                        .filter(|e| **e == ScanEvent::Osc(ParsedOsc7501::Query))
                        .count();
                }
                output.extend(scanner.finish());
                assert_eq!(queries, 1, "{bytes:?} split {split}");
                assert!(!output.windows(7).any(|w| w == b"]7501;?"));
            }
        }
    }
    #[test]
    fn sgr_is_borrowed_and_eof_preserves_partial_sequences() {
        let mut scanner = Osc7501Scanner::new();
        assert!(matches!(
            scanner.feed(b"\x1b[31mred\x1b[0m").forward,
            Cow::Borrowed(_)
        ));
        for bytes in [
            b"abc\x1b".as_slice(),
            b"\x1b]750",
            b"\x1b]7501;?",
            b"\x1b]7501;state=working\x1b",
            b"\x1b]7501;state=working\x07plain\x1b",
            b"\x1b]7501;state=working\x07\x1b]750",
        ] {
            let mut scanner = Osc7501Scanner::new();
            let mut output = scanner.feed(bytes).forward.into_owned();
            output.extend(scanner.finish());
            assert_eq!(output, bytes);
            assert!(!scanner.is_mid_sequence());
        }
    }
    #[test]
    fn fresh_gate_does_not_publish_a_clear() {
        assert_eq!(ReportedGate::new().next(Instant::now(), None), None);
    }
}
