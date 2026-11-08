//! The host-terminal output filter (WI 0141 hygiene §1, Layer 3).
//!
//! Applied wherever agent bytes reach a **host** TTY: `awman attach`
//! (whenever stdout is a TTY, no flag), `agents attach` and `squad attach`
//! (CLI and TUI). The TUI's own renderer parses into `vt100` and never
//! forwards raw OSC, so it does not use this.
//!
//! Stripped by default: OSC 52 (clipboard), OSC 7 (cwd), OSC 9/99/777
//! (notifications, unless the policy allows them), OSC 1337 (iTerm2
//! transfers and `SetUserVar`), OSC 5113 / kitty file transfer, APC `_G`
//! graphics, and every DCS (tmux passthrough included), SOS, PM and APC
//! string. OSC 7501 reports are parsed with `data::program_status` and
//! re-encoded from the sanitised record through `frontend::osc7501`, never
//! forwarded raw; 7501 queries are dropped because awman's bridge answers
//! them. Printable text, CSI (colour, cursor, modes), OSC 0/1/2 titles and
//! OSC 8 hyperlinks pass byte for byte. Both `ESC \` and BEL terminators and
//! the 8-bit C1 forms (`0x9D` OSC, `0x90` DCS, `0x9C` ST, `0x9F` APC, `0x9E`
//! PM, `0x98` SOS) are handled.
//!
//! The OSC rule is an allow-list: only OSC 0, 1, 2 and 8 (plus 9/99/777
//! when notifications are allowed, and re-encoded 7501) ever leave the
//! filter. Any other OSC number — the ones listed above, palette and colour
//! queries (4, 10, 11, …), and non-numeric ones — is dropped.
//!
//! C1 controls: a byte in `0x80..=0x9F` is a UTF-8 continuation byte when it
//! follows a UTF-8 lead byte, and is passed as text. Otherwise it is an
//! 8-bit C1 control. The UTF-8 encoding of a C1 code point (`C2 80..=9F`,
//! which UTF-8 terminals such as xterm act on) is treated the same as the
//! raw byte. Inside a forwarded OSC, any C0 (other than its BEL terminator)
//! or C1 control makes the whole OSC be dropped, so a forwarded title can
//! never carry a sequence a terminal might act on.
//!
//! Safety argument: every string the filter drops loses its introducer, so
//! the host terminal never enters that string state. Where the filter and a
//! terminal might disagree about where a dropped string ends, the bytes
//! after the filter's idea of the end are scanned again from ground state.

use crate::data::program_status::{parse_body, ParsedOsc7501};
use crate::data::terminal::{HostOutputPolicy, ProgramStatusMode};
use crate::frontend::osc7501;

/// Longest partial sequence held across chunks. Past this the sequence is
/// dropped up to its terminator; plain text is never held back.
pub const MAX_PENDING_BYTES: usize = 4096;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
const C1_DCS: u8 = 0x90;
const C1_SOS: u8 = 0x98;
const C1_ST: u8 = 0x9c;
const C1_OSC: u8 = 0x9d;
const C1_PM: u8 = 0x9e;
const C1_APC: u8 = 0x9f;
/// The UTF-8 lead byte of U+0080–U+00BF (and so of every C1 code point).
const UTF8_C2: u8 = 0xc2;

/// OSC numbers forwarded byte for byte: titles and hyperlinks.
const ALWAYS_FORWARDED_OSC: &[u32] = &[0, 1, 2, 8];
/// Desktop-notification OSC numbers, forwarded only when allowed.
const NOTIFICATION_OSC: &[u32] = &[9, 99, 777];
const PROGRAM_STATUS_OSC: u32 = 7501;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterState {
    Ground,
    /// Holding a UTF-8 `C2` lead byte in ground: the next byte decides
    /// between text and an encoded C1 control.
    Lead,
    /// Saw `ESC`.
    Escape,
    /// Inside an OSC whose number is not yet known / being collected.
    Osc,
    /// Inside a string (DCS/SOS/PM/APC) that is being dropped.
    DroppedString,
    /// Inside a dropped string and saw `ESC`; `\` ends it.
    DroppedStringEscape,
    /// Inside an OSC and saw `ESC`; `\` ends it.
    OscEscape,
}

/// A streaming filter: feed chunks, get safe bytes.
#[derive(Debug)]
pub struct HostOutputFilter {
    policy: HostOutputPolicy,
    state: FilterState,
    /// The sequence in progress, introducer included.
    pending: Vec<u8>,
    /// Bytes at the start of `pending` that are the OSC introducer.
    intro_len: usize,
    /// UTF-8 continuation bytes still expected (ground and string bodies).
    utf8_remaining: u8,
    /// The previous string-body byte was a `C2` lead.
    after_c2: bool,
    /// The OSC passed [`MAX_PENDING_BYTES`]: discard through its terminator.
    osc_overflow: bool,
    /// The OSC body held a control character: drop it at its terminator.
    osc_tainted: bool,
}

impl HostOutputFilter {
    pub fn new(policy: HostOutputPolicy) -> Self {
        Self {
            policy,
            state: FilterState::Ground,
            pending: Vec::new(),
            intro_len: 0,
            utf8_remaining: 0,
            after_c2: false,
            osc_overflow: false,
            osc_tainted: false,
        }
    }

    pub fn policy(&self) -> HostOutputPolicy {
        self.policy
    }

    /// Filter one chunk, appending the bytes to forward to `out`.
    pub fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        let mut i = 0;
        while i < chunk.len() {
            if self.state == FilterState::Ground && self.utf8_remaining == 0 {
                let run = chunk[i..]
                    .iter()
                    .position(|b| *b == ESC || *b >= 0x80)
                    .unwrap_or(chunk.len() - i);
                if run > 0 {
                    out.extend_from_slice(&chunk[i..i + run]);
                    i += run;
                    continue;
                }
            }
            self.step(chunk[i], out);
            i += 1;
        }
    }

    /// End of stream (detach): a buffered, unterminated sequence is dropped,
    /// never flushed raw.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if self.state == FilterState::Lead {
            // An incomplete character, not a sequence: it is text.
            out.push(UTF8_C2);
        }
        self.state = FilterState::Ground;
        self.pending = Vec::new();
        self.intro_len = 0;
        self.utf8_remaining = 0;
        self.after_c2 = false;
        self.osc_overflow = false;
        self.osc_tainted = false;
    }

    /// Whether a sequence is being held across a chunk boundary.
    pub fn is_mid_sequence(&self) -> bool {
        self.state != FilterState::Ground
    }

    fn step(&mut self, byte: u8, out: &mut Vec<u8>) {
        // Some transitions hand the byte back to be read again in the new
        // state; each does so at most once, so this loop is short.
        loop {
            let reprocess = match self.state {
                FilterState::Ground => {
                    self.ground(byte, out);
                    false
                }
                FilterState::Lead => self.lead(byte, out),
                FilterState::Escape => self.escape(byte, out),
                FilterState::Osc => {
                    self.osc(byte, out);
                    false
                }
                FilterState::OscEscape => self.osc_escape(byte, out),
                FilterState::DroppedString => {
                    self.dropped_string(byte);
                    false
                }
                FilterState::DroppedStringEscape => self.dropped_string_escape(byte),
            };
            if !reprocess {
                return;
            }
        }
    }

    fn ground(&mut self, byte: u8, out: &mut Vec<u8>) {
        if byte == ESC {
            self.utf8_remaining = 0;
            self.pending.clear();
            self.pending.push(ESC);
            self.state = FilterState::Escape;
            return;
        }
        if self.utf8_remaining > 0 && (0x80..=0xbf).contains(&byte) {
            self.utf8_remaining -= 1;
            out.push(byte);
            return;
        }
        self.utf8_remaining = 0;
        match byte {
            UTF8_C2 => {
                self.pending.clear();
                self.pending.push(UTF8_C2);
                self.state = FilterState::Lead;
            }
            0x80..=0x9f => self.c1_control(&[byte], out),
            _ => {
                self.utf8_remaining = continuation_count(byte);
                out.push(byte);
            }
        }
    }

    /// After a ground `C2`: an encoded C1 control, or ordinary text.
    fn lead(&mut self, byte: u8, out: &mut Vec<u8>) -> bool {
        self.pending.clear();
        self.state = FilterState::Ground;
        if (0x80..=0x9f).contains(&byte) {
            self.c1_control(&[UTF8_C2, byte], out);
            false
        } else {
            out.push(UTF8_C2);
            self.utf8_remaining = 1;
            true
        }
    }

    /// A C1 control outside any string, raw (`[0x9D]`) or UTF-8 encoded
    /// (`[0xC2, 0x9D]`).
    fn c1_control(&mut self, form: &[u8], out: &mut Vec<u8>) {
        match form[form.len() - 1] {
            C1_OSC => self.start_osc(form),
            C1_DCS | C1_SOS | C1_PM | C1_APC => self.start_dropped_string(),
            // A stray ST closes nothing; CSI and the rest are harmless.
            C1_ST => {}
            _ => out.extend_from_slice(form),
        }
    }

    fn escape(&mut self, byte: u8, out: &mut Vec<u8>) -> bool {
        match byte {
            b']' => {
                self.start_osc(&[ESC, b']']);
                false
            }
            // DCS, SOS, PM, APC.
            b'P' | b'X' | b'^' | b'_' => {
                self.start_dropped_string();
                false
            }
            // `ESC ESC`: the first is a no-op to the terminal; keep the second.
            ESC => false,
            // A stray ST closes nothing.
            b'\\' => {
                self.pending.clear();
                self.state = FilterState::Ground;
                false
            }
            // CAN and SUB cancel the escape sequence.
            CAN | SUB => {
                self.back_to_ground();
                false
            }
            // Terminals execute C0 controls inside an escape sequence and
            // then continue it (`ESC CR ]` is still OSC), so forward the
            // control and keep holding the `ESC`.
            0x00..=0x1f => {
                out.push(byte);
                false
            }
            // DEL is ignored inside an escape sequence.
            0x7f => false,
            // An ordinary escape (`ESC 7`, `ESC c`, `ESC (`, `ESC [` …):
            // the `ESC` is only ever emitted together with the byte after
            // it, so stripping something later can never join a lone `ESC`
            // to unrelated bytes.
            0x20..=0x7e => {
                out.push(ESC);
                out.push(byte);
                self.pending.clear();
                self.state = FilterState::Ground;
                false
            }
            // A C1 control or UTF-8 byte cancels the escape; read it again.
            _ => {
                self.pending.clear();
                self.state = FilterState::Ground;
                true
            }
        }
    }

    fn start_osc(&mut self, introducer: &[u8]) {
        self.pending.clear();
        self.pending.extend_from_slice(introducer);
        self.intro_len = introducer.len();
        self.utf8_remaining = 0;
        self.after_c2 = false;
        self.osc_overflow = false;
        self.osc_tainted = false;
        self.state = FilterState::Osc;
    }

    fn start_dropped_string(&mut self) {
        self.pending.clear();
        self.utf8_remaining = 0;
        self.after_c2 = false;
        self.state = FilterState::DroppedString;
    }

    fn back_to_ground(&mut self) {
        self.pending.clear();
        if self.pending.capacity() > MAX_PENDING_BYTES {
            self.pending.shrink_to(MAX_PENDING_BYTES);
        }
        self.utf8_remaining = 0;
        self.after_c2 = false;
        self.state = FilterState::Ground;
    }

    /// An `ESC` that interrupted a string starts a new escape sequence.
    fn restart_escape(&mut self) {
        self.back_to_ground();
        self.pending.push(ESC);
        self.state = FilterState::Escape;
    }

    fn push_osc(&mut self, byte: u8) {
        if self.osc_overflow {
            return;
        }
        if self.pending.len() >= MAX_PENDING_BYTES {
            self.osc_overflow = true;
            self.pending.clear();
            return;
        }
        self.pending.push(byte);
    }

    fn osc(&mut self, byte: u8, out: &mut Vec<u8>) {
        match byte {
            BEL => return self.finish_osc(&[BEL], out),
            ESC => {
                self.state = FilterState::OscEscape;
                return;
            }
            CAN | SUB => return self.back_to_ground(),
            _ => {}
        }
        if self.after_c2 {
            self.after_c2 = false;
            if byte == C1_ST {
                // `C2 9C` is ST; the `C2` is already in `pending`.
                self.pending.pop();
                return self.finish_osc(&[UTF8_C2, C1_ST], out);
            }
            if (0x80..=0x9f).contains(&byte) {
                self.osc_tainted = true;
                return self.push_osc(byte);
            }
        }
        if self.utf8_remaining > 0 && (0x80..=0xbf).contains(&byte) {
            self.utf8_remaining -= 1;
            return self.push_osc(byte);
        }
        self.utf8_remaining = 0;
        match byte {
            C1_ST => self.finish_osc(&[C1_ST], out),
            0x80..=0x9f | 0x00..=0x1f => {
                self.osc_tainted = true;
                self.push_osc(byte);
            }
            UTF8_C2 => {
                self.after_c2 = true;
                self.push_osc(byte);
            }
            _ => {
                self.utf8_remaining = continuation_count(byte);
                self.push_osc(byte);
            }
        }
    }

    fn osc_escape(&mut self, byte: u8, out: &mut Vec<u8>) -> bool {
        if byte == b'\\' {
            self.finish_osc(&[ESC, b'\\'], out);
            false
        } else {
            // `ESC` aborted the OSC (as it does in the terminal); drop it.
            self.restart_escape();
            true
        }
    }

    fn finish_osc(&mut self, terminator: &[u8], out: &mut Vec<u8>) {
        if !self.osc_overflow && !self.osc_tainted {
            self.dispatch_osc(terminator, out);
        }
        self.osc_overflow = false;
        self.osc_tainted = false;
        self.back_to_ground();
    }

    /// A complete, in-cap, control-free OSC: forward, re-encode or drop.
    fn dispatch_osc(&self, terminator: &[u8], out: &mut Vec<u8>) {
        let body = &self.pending[self.intro_len..];
        let digits = body.iter().take_while(|b| b.is_ascii_digit()).count();
        let rest = &body[digits..];
        if digits == 0 || digits > 6 || !(rest.is_empty() || rest[0] == b';') {
            return;
        }
        let number: u32 = std::str::from_utf8(&body[..digits])
            .ok()
            .and_then(|d| d.parse().ok())
            .unwrap_or(u32::MAX);
        let forward = ALWAYS_FORWARDED_OSC.contains(&number)
            || (self.policy.allow_notifications && NOTIFICATION_OSC.contains(&number));
        if forward {
            out.extend_from_slice(&self.pending);
            out.extend_from_slice(terminator);
            return;
        }
        if number == PROGRAM_STATUS_OSC
            && self.policy.program_status == ProgramStatusMode::Reencode
            && !rest.is_empty()
        {
            if let ParsedOsc7501::Report(report) = parse_body(&rest[1..]) {
                out.extend_from_slice(&osc7501::encode_report(&report));
            }
        }
    }

    fn dropped_string(&mut self, byte: u8) {
        match byte {
            ESC => {
                self.state = FilterState::DroppedStringEscape;
                return;
            }
            CAN | SUB => return self.back_to_ground(),
            _ => {}
        }
        if self.after_c2 {
            self.after_c2 = false;
            if byte == C1_ST {
                return self.back_to_ground();
            }
            if (0x80..=0x9f).contains(&byte) {
                return;
            }
        }
        if self.utf8_remaining > 0 && (0x80..=0xbf).contains(&byte) {
            self.utf8_remaining -= 1;
            return;
        }
        self.utf8_remaining = 0;
        match byte {
            C1_ST => self.back_to_ground(),
            UTF8_C2 => self.after_c2 = true,
            _ => self.utf8_remaining = continuation_count(byte),
        }
    }

    fn dropped_string_escape(&mut self, byte: u8) -> bool {
        if byte == b'\\' {
            self.back_to_ground();
            false
        } else {
            // e.g. tmux passthrough's doubled `ESC ESC`: the string is over
            // and what follows is scanned like any other output.
            self.restart_escape();
            true
        }
    }
}

/// UTF-8 continuation bytes that follow `lead` (0 for anything not a lead
/// byte). `C2` is handled by the callers.
fn continuation_count(lead: u8) -> u8 {
    match lead {
        0xc2..=0xdf => 1,
        0xe0..=0xef => 2,
        0xf0..=0xf4 => 3,
        _ => 0,
    }
}

/// Filter a whole buffer at once (tests, one-shot writers).
pub fn filter_all(policy: HostOutputPolicy, bytes: &[u8]) -> Vec<u8> {
    let mut filter = HostOutputFilter::new(policy);
    let mut out = Vec::with_capacity(bytes.len());
    filter.feed(bytes, &mut out);
    filter.finish(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::program_status::{ProgramState, ProgramStatusReport, ReportState};

    fn strip_policy() -> HostOutputPolicy {
        HostOutputPolicy::default()
    }

    fn notify_policy() -> HostOutputPolicy {
        HostOutputPolicy {
            allow_notifications: true,
            ..HostOutputPolicy::default()
        }
    }

    fn status_strip_policy() -> HostOutputPolicy {
        HostOutputPolicy {
            program_status: ProgramStatusMode::Strip,
            ..HostOutputPolicy::default()
        }
    }

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    /// OSC introducers: 7-bit, raw C1, UTF-8-encoded C1.
    const OSC_INTROS: &[&[u8]] = &[b"\x1b]", b"\x9d", b"\xc2\x9d"];
    /// OSC/string terminators: BEL (OSC only), `ESC \`, raw ST, encoded ST.
    const ST_TERMS: &[&[u8]] = &[b"\x1b\\", b"\x9c", b"\xc2\x9c"];

    /// Every stripped OSC family, with a body.
    const STRIPPED_OSC: &[&[u8]] = &[
        b"52;c;ZXZpbA==",
        b"52;c;?",
        b"7;file://evil/etc",
        b"9;you have mail",
        b"99;i=1:d=0;hello",
        b"777;notify;title;body",
        b"1337;File=name=eA==;inline=1:AAAA",
        b"1337;SetUserVar=foo=YmFy",
        b"5113;ac=send;id=1",
        b"052;c;ZXZpbA==",
        b"4;1;rgb:ff/00/00",
        b"11;?",
        b"Pffffff",
        b"7501;?",
    ];

    /// String introducers: DCS, SOS, PM, APC in 7-bit, raw C1 and encoded C1.
    const STRING_INTROS: &[&[u8]] = &[
        b"\x1bP",
        b"\x90",
        b"\xc2\x90",
        b"\x1bX",
        b"\x98",
        b"\xc2\x98",
        b"\x1b^",
        b"\x9e",
        b"\xc2\x9e",
        b"\x1b_",
        b"\x9f",
        b"\xc2\x9f",
    ];

    /// Every stripped sequence, as a complete byte string.
    fn stripped_cases(policy_allows_notifications: bool) -> Vec<Vec<u8>> {
        let mut cases = Vec::new();
        for intro in OSC_INTROS {
            for body in STRIPPED_OSC {
                let n = std::str::from_utf8(body).unwrap();
                if policy_allows_notifications
                    && (n.starts_with("9;") || n.starts_with("99;") || n.starts_with("777;"))
                {
                    continue;
                }
                cases.push(cat(&[intro, body, b"\x07"]));
                for term in ST_TERMS {
                    cases.push(cat(&[intro, body, term]));
                }
            }
        }
        for intro in STRING_INTROS {
            for body in [
                &b"tmux;\x1b\x1b]52;c;eA==\x07"[..],
                b"q",
                b"Gf=100,a=T;AAAA",
                b"1$r",
            ] {
                for term in ST_TERMS {
                    cases.push(cat(&[intro, body, term]));
                }
            }
        }
        cases
    }

    /// Feed `input` split at `cut`, return the output.
    fn split_feed(policy: HostOutputPolicy, input: &[u8], cut: usize) -> Vec<u8> {
        let mut f = HostOutputFilter::new(policy);
        let mut out = Vec::new();
        f.feed(&input[..cut], &mut out);
        f.feed(&input[cut..], &mut out);
        f.finish(&mut out);
        out
    }

    fn byte_by_byte(policy: HostOutputPolicy, input: &[u8]) -> Vec<u8> {
        let mut f = HostOutputFilter::new(policy);
        let mut out = Vec::new();
        for b in input {
            f.feed(std::slice::from_ref(b), &mut out);
        }
        f.finish(&mut out);
        out
    }

    #[test]
    fn every_stripped_family_is_removed_with_text_around_it_kept() {
        for (policy, allow) in [(strip_policy(), false), (notify_policy(), true)] {
            for seq in stripped_cases(allow) {
                let input = cat(&[b"before ", &seq, b" after"]);
                assert_eq!(
                    filter_all(policy, &input),
                    b"before  after",
                    "{:?} with {policy:?}",
                    String::from_utf8_lossy(&seq)
                );
            }
        }
    }

    #[test]
    fn stripped_sequences_split_at_every_offset() {
        for seq in stripped_cases(false) {
            let input = cat(&[b"a", &seq, b"z"]);
            for cut in 0..=input.len() {
                assert_eq!(
                    split_feed(strip_policy(), &input, cut),
                    b"az",
                    "{:?} cut {cut}",
                    String::from_utf8_lossy(&input)
                );
            }
            assert_eq!(byte_by_byte(strip_policy(), &input), b"az");
        }
    }

    const PRESERVED: &[&[u8]] = &[
        b"\x1b[31mred\x1b[0m",
        b"\x1b[38;2;10;20;30mrgb\x1b[m",
        b"\x1b[2J\x1b[H\x1b[10;5H",
        b"\x1b[?1049h\x1b[?25l\x1b[?2004h",
        b"\x1b7\x1b8\x1bc\x1b(B\x1b=\x1b>",
        b"\x1b]0;my title\x07",
        b"\x1b]1;icon\x1b\\",
        b"\x1b]2;window title\x1b\\",
        b"\x1b]2;caf\xc3\xa9 \xe2\x9c\x9d \xc3\x90\x07",
        b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\",
        b"\x1b]8;id=x;file:///tmp/a\x07t\x1b]8;;\x07",
        b"\x9d0;c1 title\x9c",
        b"\xc2\x9d2;encoded title\xc2\x9c",
        b"\x9b31mc1 csi",
        "plain text — é ✝ Ð Ý £ 😀 \t\r\n".as_bytes(),
    ];

    #[test]
    fn csi_sgr_titles_and_hyperlinks_pass_byte_for_byte() {
        for seq in PRESERVED {
            for policy in [strip_policy(), notify_policy(), status_strip_policy()] {
                assert_eq!(
                    filter_all(policy, seq),
                    *seq,
                    "{:?}",
                    String::from_utf8_lossy(seq)
                );
            }
            for cut in 0..=seq.len() {
                assert_eq!(split_feed(strip_policy(), seq, cut), *seq, "cut {cut}");
            }
        }
        let all: Vec<u8> = PRESERVED.concat();
        assert_eq!(byte_by_byte(strip_policy(), &all), all);
    }

    #[test]
    fn printable_text_is_never_held_back() {
        let mut f = HostOutputFilter::new(strip_policy());
        let mut out = Vec::new();
        f.feed(b"hello \xe2\x9c", &mut out);
        assert_eq!(out, b"hello \xe2\x9c");
        f.feed(b"\x9d world\x1b]52;c;", &mut out);
        assert_eq!(out, "hello ✝ world".as_bytes());
        assert!(f.is_mid_sequence());
        f.feed(b"eA==", &mut out);
        f.feed(b"\x07!", &mut out);
        assert_eq!(out, "hello ✝ world!".as_bytes());
        assert!(!f.is_mid_sequence());
    }

    #[test]
    fn oversized_sequences_are_dropped_through_their_terminator() {
        for term in [&b"\x07"[..], b"\x1b\\", b"\x9c"] {
            let payload = vec![b'A'; MAX_PENDING_BYTES * 3];
            let input = cat(&[b"x", b"\x1b]52;c;", &payload, term, b"y"]);
            assert_eq!(filter_all(strip_policy(), &input), b"xy");
            // Even an allowed OSC is dropped once it is over the cap.
            let input = cat(&[b"x", b"\x1b]0;", &payload, term, b"y"]);
            assert_eq!(filter_all(strip_policy(), &input), b"xy");
            // Fed in small chunks, the filter never holds more than the cap.
            let mut f = HostOutputFilter::new(strip_policy());
            let mut out = Vec::new();
            for chunk in input.chunks(97) {
                f.feed(chunk, &mut out);
                assert!(f.pending.len() <= MAX_PENDING_BYTES);
            }
            f.finish(&mut out);
            assert_eq!(out, b"xy");
        }
        // Just under the cap, an allowed title still passes.
        let title = vec![b't'; MAX_PENDING_BYTES - 4];
        let input = cat(&[b"\x1b]0;", &title, b"\x07"]);
        assert_eq!(filter_all(strip_policy(), &input), input);
    }

    #[test]
    fn unterminated_sequences_are_dropped_at_finish() {
        for start in [
            &b"\x1b]52;c;ZXZpbA=="[..],
            b"\x1b]0;title",
            b"\x1bPtmux;",
            b"\x9d7;file://x",
            b"\x1b",
            b"\x1b]",
            b"\x1b]7501;state=working",
        ] {
            let mut f = HostOutputFilter::new(strip_policy());
            let mut out = Vec::new();
            f.feed(b"ok", &mut out);
            f.feed(start, &mut out);
            f.finish(&mut out);
            assert_eq!(out, b"ok", "{start:?}");
            // The filter is reusable after finish.
            f.feed(b"\x1b[1mnext", &mut out);
            assert_eq!(out, b"ok\x1b[1mnext");
        }
        // A lone C2 lead at the end is text, not a sequence.
        assert_eq!(filter_all(strip_policy(), b"a\xc2"), b"a\xc2");
    }

    #[test]
    fn interrupted_sequences_drop_the_interrupted_string() {
        // ESC inside an OSC aborts it; the following CSI survives.
        assert_eq!(
            filter_all(strip_policy(), b"\x1b]0;title\x1b[31mred"),
            b"\x1b[31mred"
        );
        // CAN and SUB cancel.
        assert_eq!(filter_all(strip_policy(), b"\x1b]52;c;x\x18ok"), b"ok");
        assert_eq!(filter_all(strip_policy(), b"\x1bPq\x1aok"), b"ok");
        // tmux passthrough: the inner OSC 52 is scanned and stripped too.
        assert_eq!(
            filter_all(
                strip_policy(),
                b"\x1bPtmux;\x1b\x1b]52;c;eA==\x07\x1b\\after"
            ),
            b"after"
        );
        // A forwarded title can never carry a control character.
        assert_eq!(
            filter_all(strip_policy(), b"\x1b]0;a\x90tmux;b\x07ok"),
            b"ok"
        );
        assert_eq!(
            filter_all(strip_policy(), b"\x1b]2;a\xc2\x9fb\x07ok"),
            b"ok"
        );
        assert_eq!(filter_all(strip_policy(), b"\x1b]2;a\rb\x07ok"), b"ok");
        // C0 inside an escape is executed by terminals and the escape goes
        // on: `ESC CR ]52;` is still OSC 52.
        assert_eq!(
            filter_all(strip_policy(), b"\x1b\r]52;c;eA==\x07ok"),
            b"\rok"
        );
        assert_eq!(filter_all(strip_policy(), b"\x1b\n\x7fPq\x1b\\ok"), b"\nok");
        // A cancelled ESC is never emitted on its own, so stripping what
        // follows cannot join it to later bytes (`ESC` + `X` = SOS).
        assert_eq!(filter_all(strip_policy(), b"\x1b\x9d\x07Xok"), b"Xok");
        assert_eq!(filter_all(strip_policy(), b"\x1b\x18Xok"), b"Xok");
        assert_eq!(filter_all(strip_policy(), b"\x1b\x1b]52;x\x07Pok"), b"Pok");
        // Number must be followed by `;` or the terminator.
        assert_eq!(filter_all(strip_policy(), b"\x1b]0x;t\x07ok"), b"ok");
        assert_eq!(filter_all(strip_policy(), b"\x1b]0\x07ok"), b"\x1b]0\x07ok");
    }

    #[test]
    fn notifications_follow_the_policy() {
        for seq in [
            &b"\x1b]9;hello\x07"[..],
            b"\x1b]99;i=1;hi\x1b\\",
            b"\x1b]777;notify;t;b\x07",
        ] {
            assert_eq!(filter_all(strip_policy(), seq), b"", "{seq:?}");
            assert_eq!(filter_all(notify_policy(), seq), seq, "{seq:?}");
        }
        // Allowing notifications allows nothing else.
        assert_eq!(filter_all(notify_policy(), b"\x1b]52;c;eA==\x07"), b"");
        assert_eq!(
            filter_all(notify_policy(), b"\x1b]1337;SetUserVar=a=Yg==\x07"),
            b""
        );
        assert!(!HostOutputPolicy::default().allow_notifications);
    }

    fn status_report(state: ProgramState, msg: &str) -> ProgramStatusReport {
        ProgramStatusReport {
            id: Some("a/b".into()),
            state: ReportState::State(state),
            kind: None,
            progress: Some(30),
            app: Some("codex".into()),
            title: None,
            msg: Some(msg.into()),
        }
    }

    #[test]
    fn program_status_reports_are_reencoded_never_forwarded_raw() {
        // Raw form: odd spacing, unknown key, a repeated key, BEL terminator.
        let raw = b"\x1b]7501; id=a/b : junk=1 : state=idle : state=working : progress=30 : app=codex : msg=aGVsbG8=\x07";
        let expected = osc7501::encode_report(&status_report(ProgramState::Working, "hello"));
        let input = cat(&[b"x", raw, b"y"]);
        assert_eq!(
            filter_all(strip_policy(), &input),
            cat(&[b"x", &expected, b"y"])
        );
        for cut in 0..=input.len() {
            assert_eq!(
                split_feed(strip_policy(), &input, cut),
                cat(&[b"x", &expected, b"y"]),
                "cut {cut}"
            );
        }
        // C1 introducer and terminator, leading zeros in the number.
        let c1 = b"\x9d07501;state=working:id=a/b:progress=30:app=codex:msg=aGVsbG8=\x9c";
        assert_eq!(filter_all(strip_policy(), c1), expected);
        // Strip mode removes it.
        assert_eq!(filter_all(status_strip_policy(), &input), b"xy");
    }

    #[test]
    fn program_status_queries_and_invalid_reports_are_removed() {
        for policy in [strip_policy(), status_strip_policy()] {
            for seq in [
                &b"\x1b]7501;?\x1b\\"[..],
                b"\x1b]7501;?\x07",
                b"\x1b]7501;state=bogus\x07",
                b"\x1b]7501;msg=aGk=\x07",
                b"\x1b]7501;id=a,b:state=done\x07",
                b"\x1b]7501;id=a//b:state=done\x07",
                // msg decodes to an ESC sequence: invalid, removed.
                b"\x1b]7501;state=error:msg=G101MjtjO2VB\x07",
                b"\x1b]7501\x07",
                b"\x1b]7501;\xff\x07",
            ] {
                assert_eq!(filter_all(policy, seq), b"", "{seq:?}");
            }
        }
    }

    /// The TUI renderer parses agent bytes into `vt100` and only ever writes
    /// the parsed screen; OSC sequences in agent output never come back out.
    #[test]
    fn vt100_renderer_never_reemits_raw_osc() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"hi\x1b]52;c;ZXZpbA==\x07\x1b]7;file://x\x07\x1b]9;n\x07\x1bPtmux;x\x1b\\\x1b]1337;SetUserVar=a=Yg==\x07");
        let screen = parser.screen();
        for rendered in [screen.contents_formatted(), screen.contents().into_bytes()] {
            assert!(!rendered.windows(2).any(|w| w == b"\x1b]"), "{rendered:?}");
            assert!(!rendered.windows(2).any(|w| w == b"\x1bP"), "{rendered:?}");
        }
        assert!(screen.contents().starts_with("hi"));
    }

    // ---- property test (seeded xorshift; the workspace has no proptest) ----

    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len())]
        }
    }

    /// Fragments that make the filter's edge cases likely.
    const NOISE: &[&[u8]] = &[
        b"\x1b",
        b"]",
        b"P",
        b"X",
        b"^",
        b"_",
        b"\\",
        b"\x07",
        b"\x9c",
        b"\x9d",
        b"\x90",
        b"\x9f",
        b"\x98",
        b"\x9e",
        b"\xc2",
        b"\xc3",
        b"\xe2",
        b"52",
        b"7",
        b"9",
        b"99",
        b"777",
        b"1337",
        b"5113",
        b"7501",
        b"0",
        b"8",
        b";",
        b"c;",
        b"?",
        b"\x18",
        b"tmux;",
        b"\x1b[",
        b"31m",
        b"text",
        b" ",
        b"\x80",
        b"\xbf",
        b"\xff",
        b"state=working",
        b"\r\n",
        b"\r",
        b"\x7f",
        b"(",
    ];

    /// Text the filter must keep exactly: printable ASCII and valid UTF-8.
    const TEXT: &[&str] = &[
        "a", "hello", " ", "é", "✝", "Ð", "Ý", "£", "😀", "\t", "\n", "~[];",
    ];

    /// Scan `out` the way the filter defines its terms and report any
    /// forbidden introducer.
    fn forbidden_in(out: &[u8], allow_notifications: bool) -> Option<String> {
        let mut allowed: Vec<u32> = vec![0, 1, 2, 8, 7501];
        if allow_notifications {
            allowed.extend_from_slice(&[9, 99, 777]);
        }
        let mut remaining = 0u8;
        let mut i = 0;
        while i < out.len() {
            let b = out[i];
            if remaining > 0 && (0x80..=0xbf).contains(&b) {
                remaining -= 1;
                i += 1;
                continue;
            }
            remaining = 0;
            // (introducer length, kind) at i
            let intro = match b {
                ESC => match out.get(i + 1) {
                    Some(b']') => Some((2, true)),
                    Some(b'P' | b'X' | b'^' | b'_') => Some((2, false)),
                    _ => None,
                },
                C1_OSC => Some((1, true)),
                C1_DCS | C1_SOS | C1_PM | C1_APC => Some((1, false)),
                UTF8_C2 => match out.get(i + 1) {
                    Some(&C1_OSC) => Some((2, true)),
                    Some(&(C1_DCS | C1_SOS | C1_PM | C1_APC)) => Some((2, false)),
                    _ => None,
                },
                _ => None,
            };
            match intro {
                Some((_, false)) => return Some(format!("string introducer at {i}")),
                Some((len, true)) => {
                    let body = &out[i + len..];
                    let digits: String = body
                        .iter()
                        .take_while(|c| c.is_ascii_digit())
                        .map(|c| *c as char)
                        .collect();
                    let n: Option<u32> = digits.parse().ok();
                    if !n.is_some_and(|n| allowed.contains(&n)) {
                        return Some(format!("OSC {digits:?} at {i}"));
                    }
                    i += len;
                    continue;
                }
                None => {}
            }
            remaining = if b == UTF8_C2 {
                1
            } else {
                continuation_count(b)
            };
            i += 1;
        }
        None
    }

    #[test]
    fn property_arbitrary_input_never_yields_forbidden_sequences() {
        let mut rng = XorShift(0x0141_5eed_dead_beef);
        for case in 0..10_000 {
            let mut input = Vec::new();
            for _ in 0..rng.below(40) {
                if rng.below(4) == 0 {
                    input.push(rng.next() as u8);
                } else {
                    input.extend_from_slice(rng.pick::<&[u8]>(NOISE));
                }
            }
            let policy = if case % 2 == 0 {
                strip_policy()
            } else {
                notify_policy()
            };
            let whole = filter_all(policy, &input);
            if let Some(why) = forbidden_in(&whole, policy.allow_notifications) {
                panic!("case {case}: {why}: in {input:?} out {whole:?}");
            }
            // Chunking never changes the result.
            let cut = rng.below(input.len() + 1);
            assert_eq!(
                split_feed(policy, &input, cut),
                whole,
                "case {case} cut {cut}"
            );
        }
    }

    #[test]
    fn property_printable_text_survives_between_sequences() {
        let mut rng = XorShift(0x7501_0000_1234_5678);
        let stripped = stripped_cases(false);
        for case in 0..10_000 {
            let mut input = Vec::new();
            let mut expected = Vec::new();
            for _ in 0..rng.below(12) {
                for _ in 0..rng.below(4) {
                    let t = rng.pick(TEXT).as_bytes();
                    input.extend_from_slice(t);
                    expected.extend_from_slice(t);
                }
                match rng.below(3) {
                    0 => input.extend_from_slice(rng.pick::<Vec<u8>>(&stripped)),
                    1 => {
                        let keep: &[u8] = rng.pick::<&[u8]>(PRESERVED);
                        input.extend_from_slice(keep);
                        expected.extend_from_slice(keep);
                    }
                    _ => {}
                }
            }
            let mut f = HostOutputFilter::new(strip_policy());
            let mut out = Vec::new();
            let mut rest = &input[..];
            while !rest.is_empty() {
                let n = 1 + rng.below(rest.len().min(17));
                f.feed(&rest[..n], &mut out);
                rest = &rest[n..];
            }
            f.finish(&mut out);
            assert_eq!(
                out,
                expected,
                "case {case}: {:?}",
                String::from_utf8_lossy(&input)
            );
        }
    }
}
