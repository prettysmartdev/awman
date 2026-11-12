//! OSC 7501 as terminal bytes (WI 0141, Layer 3 presentation codec).
//!
//! Encodes `ProgramStatusReport`s into `ESC ] 7501 ; … ESC \` for a host
//! terminal, and recognises the terminal's replies to the TUI's feature
//! probe. Built by the host-output-filter step (the filter re-encodes
//! sanitised reports through it) and reused by the TUI emitter. Pure: no
//! I/O, no policy about *what* to report (that is
//! `command::program_status_projection`).

use base64::Engine as _;

use crate::data::program_status::{
    is_valid_id, strip_invisible, ProgramState, ProgramStatusReport, ReportState, MAX_ID_BYTES,
    MAX_MSG_BYTES, MAX_SEQUENCE_BYTES, MAX_TITLE_BYTES,
};

/// The feature-detection query the TUI sends at startup.
pub const QUERY: &[u8] = b"\x1b]7501;?\x1b\\";
/// Primary device attributes, sent right after [`QUERY`]. A terminal that
/// supports 7501 answers the query before it answers this.
pub const DA1_PROBE: &[u8] = b"\x1b[c";
/// The whole record set cleared: `state=clear` with no `id`.
pub const CLEAR_ALL: &[u8] = b"\x1b]7501;state=clear\x1b\\";

const INTRODUCER: &[u8] = b"\x1b]7501;";
const TERMINATOR: &[u8] = b"\x1b\\";

/// Encode one report. `msg`/`title` are base64-encoded; the protocol caps
/// (4096-byte sequence, 192-byte `title`, 2048-byte `msg`, 128-byte `id`)
/// are enforced by truncation that keeps valid UTF-8.
///
/// Text is re-sanitised here (control characters dropped, invisible and
/// bidi characters stripped) so the output always re-parses. A report whose
/// `id` is not a valid record path encodes to nothing: dropping its id would
/// turn it into a root report.
pub fn encode_report(report: &ProgramStatusReport) -> Vec<u8> {
    if let Some(id) = &report.id {
        if id.len() > MAX_ID_BYTES || !is_valid_id(id) {
            return Vec::new();
        }
    }
    let title = report.title.as_deref().map(sanitize_text);
    let msg = report.msg.as_deref().map(sanitize_text);

    let mut title_cap = MAX_TITLE_BYTES;
    let mut msg_cap = MAX_MSG_BYTES;
    loop {
        let bytes = encode_with_caps(report, title.as_deref(), msg.as_deref(), title_cap, msg_cap);
        if bytes.len() <= MAX_SEQUENCE_BYTES {
            return bytes;
        }
        // Unreachable with today's caps (the largest legal sequence is well
        // under 4096 bytes), kept so the sequence cap holds on its own.
        let excess = bytes.len() - MAX_SEQUENCE_BYTES;
        let shrink = excess.div_ceil(4) * 3 + 3;
        if msg_cap > 0 {
            msg_cap = msg_cap.saturating_sub(shrink);
        } else {
            title_cap = title_cap.saturating_sub(shrink);
        }
    }
}

fn encode_with_caps(
    report: &ProgramStatusReport,
    title: Option<&str>,
    msg: Option<&str>,
    title_cap: usize,
    msg_cap: usize,
) -> Vec<u8> {
    let mut fields: Vec<String> = Vec::new();
    if let Some(id) = &report.id {
        fields.push(format!("id={id}"));
    }
    let state = match report.state {
        ReportState::Clear => None,
        ReportState::State(state) => Some(state),
    };
    fields.push(format!(
        "state={}",
        state.map(ProgramState::as_str).unwrap_or("clear")
    ));
    if state == Some(ProgramState::Blocked) {
        if let Some(kind) = report.kind {
            fields.push(format!("kind={}", kind.as_str()));
        }
    }
    if matches!(state, Some(ProgramState::Working | ProgramState::Blocked)) {
        if let Some(progress) = report.progress.filter(|p| *p <= 100) {
            fields.push(format!("progress={progress}"));
        }
    }
    if let Some(app) = report.app.as_deref().filter(|a| valid_app(a)) {
        fields.push(format!("app={app}"));
    }
    if state.is_some() {
        for (key, text, cap) in [("title", title, title_cap), ("msg", msg, msg_cap)] {
            let Some(text) = text.map(|t| truncate_utf8(t, cap)) else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
            fields.push(format!("{key}={encoded}"));
        }
    }

    let body = fields.join(":");
    let mut out = Vec::with_capacity(INTRODUCER.len() + body.len() + TERMINATOR.len());
    out.extend_from_slice(INTRODUCER);
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(TERMINATOR);
    out
}

/// Encode a record set, at most 64 reports, in order.
pub fn encode_reports(reports: &[ProgramStatusReport]) -> Vec<u8> {
    reports
        .iter()
        .take(crate::data::program_status::MAX_RECORDS)
        .flat_map(encode_report)
        .collect()
}

/// The longest prefix of `text` that is at most `max` bytes and ends on a
/// character boundary.
pub fn truncate_utf8(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Drop every C0/DEL/C1 control character and strip invisible/bidi
/// characters, so the text passes the parser's checks after decoding.
fn sanitize_text(text: &str) -> String {
    let visible: String = text
        .chars()
        .filter(|c| !matches!(*c as u32, 0x00..=0x1F | 0x7F..=0x9F))
        .collect();
    strip_invisible(&visible)
}

/// Same grammar as the parser's `app`: 1–32 bytes of `[A-Za-z0-9_.+-]`.
fn valid_app(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
}

/// What the probe has learnt so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProbeOutcome {
    /// Neither reply seen yet.
    #[default]
    Pending,
    /// The 7501 reply arrived before the DA1 reply: emit.
    Supported,
    /// DA1 arrived first, or the 7501 reply came after it: never emit.
    Unsupported,
}

/// Longest partial reply held across reads; past it the bytes are released
/// to the key handler unchanged.
const MAX_PROBE_PENDING: usize = 256;

/// Recognises the probe replies in the terminal's input and removes them so
/// they never reach the key handler. Fed from the TUI event loop's reader.
///
/// Recognised replies:
/// - the 7501 reply: an OSC 7501 whose body starts with `?`
///   (`ESC ] 7501 ; ? … ST|BEL`), removed whenever it arrives;
/// - the DA1 reply: `ESC [ ? <digits and ;> c`, removed once: the first one
///   after the probe decides the outcome.
///
/// While the probe is pending, a possible prefix of a reply (a lone `ESC`
/// included) is held across reads. The event loop calls [`Self::expire`]
/// when its probe timeout fires, which settles the outcome as unsupported
/// and returns any held bytes (for example a real Escape key press).
#[derive(Debug, Default)]
pub struct ProbeDetector {
    outcome: ProbeOutcome,
    saw_7501_reply: bool,
    saw_da1_reply: bool,
    /// [`Self::expire`] ran: the probe window is over, so short prefixes
    /// (a lone `ESC`, `ESC [`) are key presses, not replies.
    expired: bool,
    stream_mode: bool,
    pending: Vec<u8>,
}

/// How a byte run at the start of the buffer relates to a reply.
enum ReplyMatch {
    /// A complete 7501 reply of this length.
    Osc7501(usize),
    /// A complete DA1 reply of this length.
    Da1(usize),
    /// Could still become a reply.
    Prefix,
    /// Not a reply.
    No,
}

impl ProbeDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume probe replies from `input`, returning the bytes that remain
    /// for the key handler.
    pub fn feed(&mut self, input: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(input);
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        while i < buf.len() {
            if buf[i] != 0x1b {
                let next = buf[i..]
                    .iter()
                    .position(|b| *b == 0x1b)
                    .map_or(buf.len(), |p| i + p);
                out.extend_from_slice(&buf[i..next]);
                i = next;
                continue;
            }
            match self.match_reply(&buf[i..]) {
                ReplyMatch::Osc7501(len) => {
                    self.saw_7501_reply = true;
                    if self.outcome == ProbeOutcome::Pending {
                        self.outcome = ProbeOutcome::Supported;
                    }
                    i += len;
                }
                ReplyMatch::Da1(len) => {
                    self.saw_da1_reply = true;
                    if self.outcome == ProbeOutcome::Pending {
                        self.outcome = if self.saw_7501_reply {
                            ProbeOutcome::Supported
                        } else {
                            ProbeOutcome::Unsupported
                        };
                    }
                    i += len;
                }
                ReplyMatch::Prefix if buf.len() - i <= MAX_PROBE_PENDING => {
                    self.pending = buf[i..].to_vec();
                    return out;
                }
                ReplyMatch::Prefix | ReplyMatch::No => {
                    out.push(buf[i]);
                    i += 1;
                }
            }
        }
        out
    }

    /// The probe timed out: settle a pending outcome as
    /// [`ProbeOutcome::Unsupported`] and release any held bytes.
    pub fn expire(&mut self) -> Vec<u8> {
        self.expired = true;
        if self.outcome == ProbeOutcome::Pending {
            self.outcome = ProbeOutcome::Unsupported;
        }
        std::mem::take(&mut self.pending)
    }

    pub fn outcome(&self) -> ProbeOutcome {
        self.outcome
    }

    /// Raw readers must hold a short escape prefix even after DA/timeout:
    /// a late reply can be split at its very first byte. Release just an
    /// ambiguous key prefix after the reader's ordinary Escape-key delay.
    pub fn release_key_prefix(&mut self) -> Vec<u8> {
        if self.pending == b"\x1b" || self.pending == b"\x1b[" || self.pending == b"\x1b]" {
            std::mem::take(&mut self.pending)
        } else {
            Vec::new()
        }
    }

    pub fn is_control_prefix(&self) -> bool {
        self.pending == b"\x1b[" || self.pending == b"\x1b]"
    }

    /// Feed a raw stream, retaining short prefixes until the input reader
    /// resolves the Escape-key ambiguity with [`Self::release_key_prefix`].
    pub fn feed_stream(&mut self, input: &[u8]) -> Vec<u8> {
        self.stream_mode = true;
        let out = self.feed(input);
        self.stream_mode = false;
        out
    }

    /// Expire detection while continuing to consume a fragmented late reply.
    pub fn expire_stream(&mut self) -> Vec<u8> {
        let pending = std::mem::take(&mut self.pending);
        self.expire();
        self.pending = pending;
        self.release_key_prefix()
    }

    /// Match `buf` (starting at an `ESC`) against the two replies.
    fn match_reply(&self, buf: &[u8]) -> ReplyMatch {
        const OSC_PREFIX: &[u8] = b"\x1b]7501;?";
        let pending = self.outcome == ProbeOutcome::Pending || self.stream_mode;

        // The 7501 reply.
        let n = buf.len().min(OSC_PREFIX.len());
        if buf[..n] == OSC_PREFIX[..n] {
            if buf.len() < OSC_PREFIX.len() {
                // A lone ESC after the probe settled is a key press.
                return if pending || buf.len() >= 3 {
                    ReplyMatch::Prefix
                } else {
                    self.match_da1(buf)
                };
            }
            let mut j = OSC_PREFIX.len();
            while j < buf.len() {
                match buf[j] {
                    0x07 => return ReplyMatch::Osc7501(j + 1),
                    0x1b => {
                        return match buf.get(j + 1) {
                            Some(b'\\') => ReplyMatch::Osc7501(j + 2),
                            Some(_) => ReplyMatch::No,
                            None => ReplyMatch::Prefix,
                        };
                    }
                    0x20..=0x7e => j += 1,
                    _ => return ReplyMatch::No,
                }
            }
            return ReplyMatch::Prefix;
        }
        self.match_da1(buf)
    }

    fn match_da1(&self, buf: &[u8]) -> ReplyMatch {
        const DA_PREFIX: &[u8] = b"\x1b[?";
        if self.stream_mode && buf.len() <= 2 && DA_PREFIX.starts_with(buf) {
            return ReplyMatch::Prefix;
        }
        if self.saw_da1_reply {
            return ReplyMatch::No;
        }
        let n = buf.len().min(DA_PREFIX.len());
        if buf[..n] != DA_PREFIX[..n] {
            return ReplyMatch::No;
        }
        if buf.len() < DA_PREFIX.len() {
            return if self.expired && !self.stream_mode {
                ReplyMatch::No
            } else {
                ReplyMatch::Prefix
            };
        }
        for (j, b) in buf.iter().enumerate().skip(DA_PREFIX.len()) {
            match b {
                b'0'..=b'9' | b';' => {}
                b'c' => return ReplyMatch::Da1(j + 1),
                _ => return ReplyMatch::No,
            }
        }
        ReplyMatch::Prefix
    }
}
