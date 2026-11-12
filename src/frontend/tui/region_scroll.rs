//! Scroll-region scrollback emulation for the container PTY stream.
//!
//! Inline-viewport TUIs (notably the codex CLI) push chat history into the
//! terminal's scrollback by setting a DECSTBM scroll region anchored at the
//! top of the screen (`CSI 1;Nr`, ending above their viewport) and scrolling
//! it with newlines. Real terminal emulators (iTerm2, kitty, VTE, WezTerm)
//! move the rows that fall off the top of such a region into scrollback; the
//! vt100 crate discards them whenever any region is active, which would
//! leave awman's container scrollback empty for these agents.
//!
//! [`RegionScrollEmulator`] closes that gap without forking the crate: it
//! strips top-anchored partial DECSTBM sequences before they reach the
//! parser and, for each scroll the region would have performed, feeds the
//! parser an equivalent *full-screen* scroll (which vt100 does record in
//! scrollback) followed by a repaint of the rows below the region from the
//! grid state vt100 already holds. The resulting grid is identical to native
//! region handling, and the discarded rows land in scrollback.
//!
//! Non-top-anchored regions (e.g. vim-style pinned-header layouts) are
//! passed through untouched and keep vt100's native behavior.
//!
//! Known limitations (accepted, in line with the per-chunk processing the
//! TUI already does elsewhere): autowrap-triggered scrolls at the region
//! bottom are not intercepted (codex pre-wraps its lines so this does not
//! occur in practice), and DECOM origin mode is not accounted for in the
//! synthetic cursor moves (codex does not use it).

/// Upper bound for the carry buffer used to stitch escape sequences that
/// split across PTY read chunks. Sequences longer than this are flushed
/// verbatim (the parser handles them; only scrollback emulation is skipped).
const MAX_CARRY: usize = 64;

#[derive(Default)]
pub struct RegionScrollEmulator {
    /// 0-based bottom row of the active top-anchored partial scroll region,
    /// if one has been stripped from the stream.
    region_bottom: Option<u16>,
    /// Tail bytes of an escape sequence that ran past the end of the last
    /// chunk, replayed in front of the next one.
    carry: Vec<u8>,
}

/// Outcome of scanning one escape sequence (or control byte) at the head of
/// the remaining input.
enum Token {
    /// `len` bytes that need no special handling — leave them in the
    /// pass-through segment.
    Passthrough { len: usize },
    /// A complete CSI sequence: `len` total bytes, `params` is the byte
    /// range between `ESC [` and the final byte, which is `final_byte`.
    /// Only "plain" sequences (digits and `;` parameters, no private
    /// markers or intermediates) are reported as `Csi`.
    Csi {
        len: usize,
        params_start: usize,
        params_end: usize,
        final_byte: u8,
    },
    /// `ESC D` / `ESC E` / `ESC M` (IND / NEL / RI).
    EscSingle { len: usize, which: u8 },
    /// The sequence runs past the end of the chunk.
    Incomplete,
}

impl RegionScrollEmulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget all stream state. Call whenever the vt100 parser is reset or
    /// replaced.
    pub fn reset(&mut self) {
        self.region_bottom = None;
        self.carry.clear();
    }

    /// Process a chunk of PTY output into `parser`, emulating scrollback
    /// for top-anchored scroll regions.
    pub fn process(&mut self, parser: &mut vt100::Parser, input: &[u8]) {
        if self.carry.is_empty() {
            self.process_inner(parser, input);
        } else {
            let mut merged = std::mem::take(&mut self.carry);
            merged.extend_from_slice(input);
            self.process_inner(parser, &merged);
        }
    }

    fn process_inner(&mut self, parser: &mut vt100::Parser, input: &[u8]) {
        let mut seg_start = 0;
        let mut i = 0;

        while i < input.len() {
            let b = input[i];

            // Plain newline: only interesting while a region is active.
            if b == 0x0a {
                if let Some(b0) = self.active_bottom(parser) {
                    flush(parser, &input[seg_start..i]);
                    self.handle_lf(parser, b0);
                    i += 1;
                    seg_start = i;
                } else {
                    i += 1;
                }
                continue;
            }

            if b != 0x1b {
                i += 1;
                continue;
            }

            match scan_escape(&input[i..]) {
                Token::Passthrough { len } => {
                    i += len;
                }
                Token::Incomplete => {
                    // Hold the tail until the rest of the sequence arrives.
                    flush(parser, &input[seg_start..i]);
                    let tail = &input[i..];
                    if tail.len() <= MAX_CARRY {
                        self.carry.extend_from_slice(tail);
                    } else {
                        // Pathological; give up on emulation for this
                        // sequence and let the parser have it verbatim.
                        flush(parser, tail);
                    }
                    return;
                }
                Token::EscSingle { len, which } => {
                    if let Some(b0) = self.active_bottom(parser) {
                        flush(parser, &input[seg_start..i]);
                        match which {
                            b'D' => self.handle_lf(parser, b0),
                            b'E' => {
                                self.handle_lf(parser, b0);
                                parser.process(b"\r");
                            }
                            b'M' => {
                                if parser.screen().cursor_position().0 == 0 {
                                    synth_scroll_down(parser, b0, 1);
                                } else {
                                    parser.process(b"\x1bM");
                                }
                            }
                            _ => unreachable!(),
                        }
                        i += len;
                        seg_start = i;
                    } else {
                        i += len;
                    }
                }
                Token::Csi {
                    len,
                    params_start,
                    params_end,
                    final_byte,
                } => {
                    let params = &input[i + params_start..i + params_end];
                    match final_byte {
                        b'r' => {
                            flush(parser, &input[seg_start..i]);
                            self.handle_decstbm(parser, params, &input[i..i + len]);
                            i += len;
                            seg_start = i;
                        }
                        b'S' | b'T' if self.active_bottom(parser).is_some() => {
                            flush(parser, &input[seg_start..i]);
                            let b0 = self.active_bottom(parser).unwrap();
                            let n = parse_param(params, 0).unwrap_or(1).max(1);
                            if final_byte == b'S' {
                                synth_scroll_up(parser, b0, n);
                            } else {
                                synth_scroll_down(parser, b0, n);
                            }
                            i += len;
                            seg_start = i;
                        }
                        b'L' | b'M' if self.active_bottom(parser).is_some() => {
                            flush(parser, &input[seg_start..i]);
                            let b0 = self.active_bottom(parser).unwrap();
                            let row = parser.screen().cursor_position().0;
                            if row <= b0 {
                                // Apply the insert/delete, then repair the
                                // rows below the region that vt100 (with no
                                // region set) wrongly shifted.
                                let repaint = capture_below(parser, b0);
                                parser.process(&input[i..i + len]);
                                apply_repaint(parser, repaint);
                            }
                            // Below the region IL/DL would be a no-op under
                            // native region handling — strip it.
                            i += len;
                            seg_start = i;
                        }
                        _ => {
                            i += len;
                        }
                    }
                }
            }
        }

        flush(parser, &input[seg_start..]);
    }

    /// The active region bottom, revalidated against the current parser
    /// size (a resize can shrink the screen past the remembered bottom, at
    /// which point the region is effectively the full screen and native
    /// handling is correct).
    fn active_bottom(&mut self, parser: &vt100::Parser) -> Option<u16> {
        let b0 = self.region_bottom?;
        let (rows, _) = parser.screen().size();
        if b0 + 1 >= rows {
            self.region_bottom = None;
            return None;
        }
        Some(b0)
    }

    /// LF / IND while a region is active: scroll the region when the cursor
    /// sits on its bottom row; suppress the screen-bottom scroll vt100 would
    /// perform when the cursor is parked below the region.
    fn handle_lf(&mut self, parser: &mut vt100::Parser, b0: u16) {
        let (rows, _) = parser.screen().size();
        let row = parser.screen().cursor_position().0;
        if row == b0 {
            synth_scroll_up(parser, b0, 1);
        } else if row + 1 < rows {
            parser.process(b"\n");
        }
        // row == rows-1 below the region: native DECSTBM clamps without
        // scrolling, so the newline is dropped entirely.
    }

    /// DECSTBM: strip-and-emulate top-anchored partial regions, pass
    /// everything else through to vt100 untouched.
    fn handle_decstbm(&mut self, parser: &mut vt100::Parser, params: &[u8], raw: &[u8]) {
        let (rows, _) = parser.screen().size();
        let top = parse_param(params, 0).unwrap_or(1).max(1);
        let bottom = parse_param(params, 1).unwrap_or(rows).min(rows);

        if top == 1 && bottom >= 2 && bottom < rows {
            self.region_bottom = Some(bottom - 1);
            // DECSTBM homes the cursor; replicate that since the sequence
            // itself never reaches the parser.
            parser.process(b"\x1b[H");
        } else {
            self.region_bottom = None;
            parser.process(raw);
        }
    }
}

fn flush(parser: &mut vt100::Parser, bytes: &[u8]) {
    if !bytes.is_empty() {
        parser.process(bytes);
    }
}

/// A snapshot of everything a synthetic scroll disturbs below the region:
/// the formatted rows, the pen attributes, and the cursor position.
struct Repaint {
    below: Vec<Vec<u8>>,
    attrs: Vec<u8>,
    cursor: (u16, u16),
    b0: u16,
}

fn capture_below(parser: &vt100::Parser, b0: u16) -> Repaint {
    let screen = parser.screen();
    let (_, cols) = screen.size();
    Repaint {
        below: screen
            .rows_formatted(0, cols)
            .skip(usize::from(b0) + 1)
            .collect(),
        attrs: screen.attributes_formatted(),
        cursor: screen.cursor_position(),
        b0,
    }
}

fn apply_repaint(parser: &mut vt100::Parser, repaint: Repaint) {
    let mut buf = Vec::new();
    for (idx, row) in repaint.below.iter().enumerate() {
        let row_1 = u32::from(repaint.b0) + 2 + idx as u32;
        // Position, reset pen (row bytes assume a default pen), clear the
        // stale shifted content, then replay the saved row.
        buf.extend_from_slice(format!("\x1b[{row_1};1H\x1b[m\x1b[2K").as_bytes());
        buf.extend_from_slice(row);
    }
    buf.extend_from_slice(&repaint.attrs);
    buf.extend_from_slice(
        format!("\x1b[{};{}H", repaint.cursor.0 + 1, repaint.cursor.1 + 1).as_bytes(),
    );
    parser.process(&buf);
}

/// Scroll the (stripped) region `0..=b0` up by `n`: full-screen scroll so
/// vt100 records the evicted top rows in scrollback, then restore the rows
/// below the region and blank the vacated region-bottom rows.
fn synth_scroll_up(parser: &mut vt100::Parser, b0: u16, n: u16) {
    let (rows, _) = parser.screen().size();
    let n = n.min(b0 + 1);
    let repaint = capture_below(parser, b0);

    let mut buf = Vec::new();
    buf.extend_from_slice(format!("\x1b[{rows};1H").as_bytes());
    buf.extend(std::iter::repeat_n(b'\n', usize::from(n)));
    // The region's vacated bottom rows must end up blank; after the
    // full-screen scroll they hold former viewport content instead.
    for row_0 in (b0 + 1 - n)..=b0 {
        buf.extend_from_slice(format!("\x1b[{};1H\x1b[m\x1b[2K", row_0 + 1).as_bytes());
    }
    parser.process(&buf);
    apply_repaint(parser, repaint);
}

/// Scroll the (stripped) region `0..=b0` down by `n` (RI at the top row or
/// `CSI T`): full-screen scroll down, then restore the rows below the
/// region. Scroll-down never touches scrollback, matching native behavior.
fn synth_scroll_down(parser: &mut vt100::Parser, b0: u16, n: u16) {
    let n = n.min(b0 + 1);
    let repaint = capture_below(parser, b0);

    let mut buf = Vec::new();
    buf.extend_from_slice(b"\x1b[1;1H");
    for _ in 0..n {
        buf.extend_from_slice(b"\x1bM");
    }
    parser.process(&buf);
    apply_repaint(parser, repaint);
}

/// Parse the `idx`-th semicolon-separated numeric parameter.
fn parse_param(params: &[u8], idx: usize) -> Option<u16> {
    let part = params.split(|&b| b == b';').nth(idx)?;
    if part.is_empty() {
        return None;
    }
    let mut value: u32 = 0;
    for &b in part {
        value = value.saturating_mul(10).saturating_add(u32::from(b - b'0'));
    }
    Some(value.min(u32::from(u16::MAX)) as u16)
}

/// Classify the escape sequence at the head of `input` (which starts with
/// `ESC`). Never consumes more than the one sequence.
fn scan_escape(input: &[u8]) -> Token {
    debug_assert_eq!(input[0], 0x1b);
    let Some(&second) = input.get(1) else {
        return Token::Incomplete;
    };

    match second {
        b'D' | b'E' | b'M' => Token::EscSingle {
            len: 2,
            which: second,
        },
        b'[' => {
            let mut j = 2;
            // Parameter bytes (0x30–0x3F), then intermediates (0x20–0x2F),
            // then a final byte (0x40–0x7E).
            let params_start = j;
            while j < input.len() && (0x30..=0x3f).contains(&input[j]) {
                j += 1;
            }
            let params_end = j;
            let mut has_intermediate = false;
            while j < input.len() && (0x20..=0x2f).contains(&input[j]) {
                has_intermediate = true;
                j += 1;
            }
            let Some(&fin) = input.get(j) else {
                return Token::Incomplete;
            };
            if !(0x40..=0x7e).contains(&fin) {
                // Malformed; step past the ESC and let vte sort it out.
                return Token::Passthrough { len: 1 };
            }
            let plain_params = input[params_start..params_end]
                .iter()
                .all(|&b| b.is_ascii_digit() || b == b';');
            if has_intermediate || !plain_params {
                Token::Passthrough { len: j + 1 }
            } else {
                Token::Csi {
                    len: j + 1,
                    params_start,
                    params_end,
                    final_byte: fin,
                }
            }
        }
        _ => Token::Passthrough { len: 1 },
    }
}
