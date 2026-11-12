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
