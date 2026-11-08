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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::program_status::{parse_body, BlockedKind, ParsedOsc7501};

    fn report(state: ProgramState) -> ProgramStatusReport {
        ProgramStatusReport {
            id: None,
            state: ReportState::State(state),
            kind: None,
            progress: None,
            app: None,
            title: None,
            msg: None,
        }
    }

    fn reparse(bytes: &[u8]) -> ProgramStatusReport {
        assert!(bytes.starts_with(INTRODUCER), "{bytes:?}");
        assert!(bytes.ends_with(TERMINATOR), "{bytes:?}");
        let body = &bytes[INTRODUCER.len()..bytes.len() - TERMINATOR.len()];
        match parse_body(body) {
            ParsedOsc7501::Report(r) => r,
            other => panic!("did not re-parse: {other:?} from {bytes:?}"),
        }
    }

    #[test]
    fn encodes_every_field_and_base64s_text() {
        let r = ProgramStatusReport {
            id: Some("tab1/0".into()),
            state: ReportState::State(ProgramState::Blocked),
            kind: Some(BlockedKind::Permission),
            progress: Some(40),
            app: Some("claude".into()),
            title: Some("build".into()),
            msg: Some("Allow rm?".into()),
        };
        let bytes = encode_report(&r);
        assert_eq!(
            bytes,
            b"\x1b]7501;id=tab1/0:state=blocked:kind=permission:progress=40:app=claude:title=YnVpbGQ=:msg=QWxsb3cgcm0/\x1b\\"
        );
        assert_eq!(reparse(&bytes), r);
    }

    #[test]
    fn clear_matches_constant_and_drops_text() {
        let mut r = report(ProgramState::Idle);
        r.state = ReportState::Clear;
        r.msg = Some("x".into());
        assert_eq!(encode_report(&r), CLEAR_ALL);
    }

    #[test]
    fn kind_and_progress_only_where_the_protocol_keeps_them() {
        let mut r = report(ProgramState::Done);
        r.kind = Some(BlockedKind::Auth);
        r.progress = Some(10);
        assert_eq!(encode_report(&r), b"\x1b]7501;state=done\x1b\\");
        let mut r = report(ProgramState::Working);
        r.progress = Some(101);
        assert_eq!(encode_report(&r), b"\x1b]7501;state=working\x1b\\");
    }

    #[test]
    fn invalid_id_encodes_to_nothing_invalid_app_is_omitted() {
        let mut r = report(ProgramState::Working);
        r.id = Some("bad id".into());
        assert!(encode_report(&r).is_empty());
        r.id = Some("a".repeat(MAX_ID_BYTES + 1));
        assert!(encode_report(&r).is_empty());
        r.id = None;
        r.app = Some("evil;app".into());
        assert_eq!(encode_report(&r), b"\x1b]7501;state=working\x1b\\");
    }

    #[test]
    fn caps_truncate_on_char_boundaries() {
        let mut r = report(ProgramState::Working);
        // 3-byte chars: neither cap is a multiple of 3, so a naive cut splits one.
        r.title = Some(format!("a{}", "€".repeat(200)));
        r.msg = Some("€".repeat(1000));
        let bytes = encode_report(&r);
        assert!(bytes.len() <= MAX_SEQUENCE_BYTES);
        let back = reparse(&bytes);
        let title = back.title.unwrap();
        let msg = back.msg.unwrap();
        assert_eq!(title.len(), 190);
        assert!(title.len() <= MAX_TITLE_BYTES);
        assert_eq!(msg.len(), 2046);
        assert!(msg.len() <= MAX_MSG_BYTES);
        assert!(title[1..].chars().all(|c| c == '€'));
    }

    #[test]
    fn largest_report_fits_the_sequence_cap() {
        let r = ProgramStatusReport {
            id: Some(format!(
                "{}/{}/{}/b",
                "a".repeat(32),
                "a".repeat(32),
                "a".repeat(32)
            )),
            state: ReportState::State(ProgramState::Blocked),
            kind: Some(BlockedKind::Question),
            progress: Some(100),
            app: Some("x".repeat(32)),
            title: Some("t".repeat(5000)),
            msg: Some("m".repeat(5000)),
        };
        let bytes = encode_report(&r);
        assert!(bytes.len() <= MAX_SEQUENCE_BYTES);
        let back = reparse(&bytes);
        assert_eq!(back.msg.unwrap().len(), MAX_MSG_BYTES);
        assert_eq!(back.title.unwrap().len(), MAX_TITLE_BYTES);
    }

    #[test]
    fn control_and_invisible_characters_are_removed_before_encoding() {
        let mut r = report(ProgramState::Error);
        r.msg = Some("a\x1b]52;c;x\x07b\u{202E}c\u{9d}d".into());
        let back = reparse(&encode_report(&r));
        assert_eq!(back.msg.as_deref(), Some("a]52;c;xbcd"));
    }

    #[test]
    fn encode_reports_caps_record_count() {
        let reports = vec![report(ProgramState::Idle); 100];
        let one = encode_report(&reports[0]).len();
        assert_eq!(encode_reports(&reports).len(), one * 64);
    }

    #[test]
    fn truncate_utf8_respects_boundaries() {
        assert_eq!(truncate_utf8("héllo", 2), "h");
        assert_eq!(truncate_utf8("héllo", 3), "hé");
        assert_eq!(truncate_utf8("abc", 10), "abc");
        assert_eq!(truncate_utf8("😀", 3), "");
    }

    // ---- probe ----

    const REPLY: &[u8] = b"\x1b]7501;?\x1b\\";
    const DA1: &[u8] = b"\x1b[?62;22c";

    #[test]
    fn reply_then_da_is_supported_and_both_removed() {
        let mut p = ProbeDetector::new();
        let mut input = b"a".to_vec();
        input.extend_from_slice(REPLY);
        input.extend_from_slice(b"b");
        input.extend_from_slice(DA1);
        input.extend_from_slice(b"c");
        assert_eq!(p.feed(&input), b"abc");
        assert_eq!(p.outcome(), ProbeOutcome::Supported);
    }

    #[test]
    fn da_only_is_unsupported() {
        let mut p = ProbeDetector::new();
        assert_eq!(p.feed(DA1), b"");
        assert_eq!(p.outcome(), ProbeOutcome::Unsupported);
    }

    #[test]
    fn late_reply_stays_unsupported_and_is_removed() {
        let mut p = ProbeDetector::new();
        let mut input = DA1.to_vec();
        input.extend_from_slice(b"\x1b]7501;?\x07");
        input.push(b'k');
        assert_eq!(p.feed(&input), b"k");
        assert_eq!(p.outcome(), ProbeOutcome::Unsupported);
        // Once settled, later DA replies belong to someone else.
        assert_eq!(p.feed(DA1), DA1);
    }

    #[test]
    fn replies_split_at_every_offset_never_leak() {
        let mut stream = REPLY.to_vec();
        stream.extend_from_slice(DA1);
        stream.extend_from_slice(b"xy");
        for cut in 0..=stream.len() {
            let mut p = ProbeDetector::new();
            let mut out = p.feed(&stream[..cut]);
            out.extend(p.feed(&stream[cut..]));
            assert_eq!(out, b"xy", "cut {cut}");
            assert_eq!(p.outcome(), ProbeOutcome::Supported, "cut {cut}");
        }
    }

    #[test]
    fn key_presses_pass_and_expire_releases_a_held_escape() {
        let mut p = ProbeDetector::new();
        assert_eq!(p.feed(b"\x1b[A"), b"\x1b[A"); // arrow key, not DA
        assert_eq!(p.feed(b"\x1b"), b"");
        assert_eq!(p.expire(), b"\x1b");
        assert_eq!(p.outcome(), ProbeOutcome::Unsupported);
        // Settled: a lone Escape key goes straight through.
        assert_eq!(p.feed(b"\x1b"), b"\x1b");
        assert_eq!(p.feed(b"\x1b]7501;?;x=1\x1b\\q"), b"q");
    }

    #[test]
    fn reply_with_fields_is_recognised() {
        let mut p = ProbeDetector::new();
        assert_eq!(p.feed(b"\x1b]7501;?;states=idle,working\x07"), b"");
        assert_eq!(p.outcome(), ProbeOutcome::Supported);
    }
}

#[cfg(test)]
mod review_regressions {
    use super::*;
    #[test]
    fn alt_bracket_can_be_released_but_known_reply_prefixes_are_retained() {
        let mut probe = ProbeDetector::new();
        assert!(probe.feed_stream(b"\x1b]").is_empty());
        assert!(probe.is_control_prefix());
        assert_eq!(probe.release_key_prefix(), b"\x1b]");
        assert!(probe.feed_stream(b"\x1b]7501;").is_empty());
        assert!(probe.release_key_prefix().is_empty());
        assert!(probe.feed_stream(b"?\x1b\\").is_empty());
    }
}
