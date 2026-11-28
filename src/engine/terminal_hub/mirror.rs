//! The per-terminal screen mirror: a `vt100::Parser` (the same crate the
//! TUI's `container_view.rs` renders from) plus a bounded ring of the raw
//! bytes, so a snapshot can be rendered as ANSI that redraws the screen.

use crate::data::terminal::TerminalSnapshot;

/// vt100-ctt 0.17 cannot wrap in a single row, or draw a wide character in
/// a single column. Keep its minimum safe dimensions at the hub boundary.
pub(super) fn normalize_size(cols: u16, rows: u16) -> (u16, u16) {
    (
        cols.clamp(2, super::MAX_TERMINAL_COLS),
        rows.clamp(2, super::MAX_TERMINAL_ROWS),
    )
}

/// How far into evicted history [`RingBuffer::replay`] looks for a safe
/// start (a line boundary, else an escape introducer).
pub const REPLAY_TRIM_WINDOW: usize = 4 * 1024;

/// Bounded FIFO of raw output bytes; the newest `capacity` bytes are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RingBuffer {
    buf: Vec<u8>,
    capacity: usize,
    /// Some byte was ever dropped (or refused, at zero capacity).
    evicted: bool,
}

/// The retained bytes a late viewer may replay (WI 0105).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingReplay<'a> {
    pub bytes: &'a [u8],
    /// Older output was evicted (or withheld) and is not part of `bytes`.
    pub truncated: bool,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: Vec::new(),
            capacity,
            evicted: false,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.capacity == 0 {
            self.evicted |= !bytes.is_empty();
            return;
        }
        if bytes.len() >= self.capacity {
            self.evicted |= !self.buf.is_empty() || bytes.len() > self.capacity;
            self.buf.clear();
            self.buf
                .extend_from_slice(&bytes[bytes.len() - self.capacity..]);
        } else {
            let discard = self
                .buf
                .len()
                .saturating_add(bytes.len())
                .saturating_sub(self.capacity);
            self.evicted |= discard > 0;
            self.buf.drain(..discard);
            self.buf.extend_from_slice(bytes);
        }
    }

    /// Whether any output was dropped to stay within `capacity`.
    pub fn evicted(&self) -> bool {
        self.evicted
    }

    /// The bytes to replay. Without eviction, everything retained. After
    /// eviction the cut point may fall inside a line, an escape sequence or
    /// a UTF-8 character, so the replay starts after the first `\n` within
    /// the first [`REPLAY_TRIM_WINDOW`] bytes, else at the first `ESC` in
    /// that window, else after any leading UTF-8 continuation bytes.
    pub fn replay(&self) -> RingReplay<'_> {
        if !self.evicted {
            return RingReplay {
                bytes: &self.buf,
                truncated: false,
            };
        }
        let window = &self.buf[..self.buf.len().min(REPLAY_TRIM_WINDOW)];
        let start = if let Some(newline) = window.iter().position(|b| *b == b'\n') {
            newline + 1
        } else if let Some(escape) = window.iter().position(|b| *b == 0x1b) {
            escape
        } else {
            self.buf
                .iter()
                .position(|b| b & 0xc0 != 0x80)
                .unwrap_or(self.buf.len())
        };
        RingReplay {
            bytes: &self.buf[start..],
            truncated: true,
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }
}

/// What the agent's screen looks like right now.
pub struct ScreenMirror {
    has_output: bool,
    parser: vt100::Parser,
    ring: RingBuffer,
}

impl std::fmt::Debug for ScreenMirror {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (rows, cols) = self.parser.screen().size();
        f.debug_struct("ScreenMirror")
            .field("cols", &cols)
            .field("rows", &rows)
            .field("ring_len", &self.ring.len())
            .finish()
    }
}

impl ScreenMirror {
    pub fn new(cols: u16, rows: u16, scrollback_lines: usize, ring_bytes: usize) -> Self {
        let (cols, rows) = normalize_size(cols, rows);
        Self {
            parser: vt100::Parser::new(rows, cols, scrollback_lines),
            has_output: false,
            ring: RingBuffer::new(ring_bytes),
        }
    }

    /// Feed one output chunk to the parser and the ring.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.has_output |= !bytes.is_empty();
        self.parser.process(bytes);
        self.ring.push(bytes);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = normalize_size(cols, rows);
        self.parser.screen_mut().set_size(rows, cols);
    }

    pub fn size(&self) -> (u16, u16) {
        let (rows, cols) = self.parser.screen().size();
        (cols, rows)
    }

    /// The screen as ANSI that redraws it from a cleared terminal
    /// (`vt100::Screen::state_formatted`), with the size.
    pub fn snapshot(&self) -> TerminalSnapshot {
        let (cols, rows) = self.size();
        TerminalSnapshot {
            has_output: self.has_output,
            cols,
            rows,
            screen: self.snapshot_screen(),
        }
    }

    /// `state_formatted` plus the scroll margins, which it does not carry: a
    /// viewer repainting from a cleared screen (and an `ESC[r` reset) would
    /// otherwise scroll the whole screen where the program set a region.
    /// DECSTBM homes the cursor, so the cursor is placed again after it.
    fn snapshot_screen(&self) -> Vec<u8> {
        let screen = self.parser.screen();
        let mut bytes = screen.state_formatted();
        if let Some((top, bottom)) = self.scroll_region() {
            let (row, col) = screen.cursor_position();
            bytes.extend_from_slice(
                format!("\x1b[{top};{bottom}r\x1b[{};{}H", row + 1, col + 1).as_bytes(),
            );
        }
        bytes
    }

    /// The active grid's scroll margins (1-based, inclusive), or `None` for
    /// the full screen. vt100 keeps them private, so they are measured on a
    /// copy of the screen: line feeds from the top stop at the bottom
    /// margin, reverse indexes from the bottom stop at the top margin.
    pub fn scroll_region(&self) -> Option<(u16, u16)> {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let mut probe = vt100::Parser::new(rows, cols, 0);
        *probe.screen_mut() = screen.clone();
        let mut down = b"\x1b[?6l\x1b[1;1H".to_vec();
        down.extend(std::iter::repeat_n(b'\n', usize::from(rows)));
        probe.process(&down);
        let bottom = probe.screen().cursor_position().0;
        let mut up = format!("\x1b[{rows};1H").into_bytes();
        for _ in 0..rows {
            up.extend_from_slice(b"\x1bM");
        }
        probe.process(&up);
        let top = probe.screen().cursor_position().0;
        (top != 0 || bottom != rows.saturating_sub(1)).then_some((top + 1, bottom + 1))
    }

    pub fn ring(&self) -> &RingBuffer {
        &self.ring
    }

    /// Whether the program is on the alternate screen right now.
    pub fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    /// The history a late viewer replays before the snapshot. A full-screen
    /// program whose alternate-screen switch may have been evicted gets no
    /// history (one empty, truncated replay): its frames without the switch
    /// would land in the viewer's normal screen as noise.
    pub fn history(&self) -> RingReplay<'_> {
        let replay = self.ring.replay();
        if replay.truncated && self.alternate_screen() {
            return RingReplay {
                bytes: &[],
                truncated: true,
            };
        }
        replay
    }
}

/// Re-render a snapshot captured at `from` (cols, rows) for a viewer of size
/// `to`. The result is a *screen replacement*: it clears the viewer's screen
/// and places each kept row with an absolute cursor move, so it can never
/// scroll and never push the source screen into the viewer's scrollback.
///
/// Policy: rows are cropped to the viewer's width; when the source is taller,
/// the window of `to.rows` rows that ends at the cursor's row (or the top of
/// the screen when the cursor is higher) is kept. Input modes and the scroll
/// region are not re-applied (a region from a different geometry would be
/// wrong for this viewer); the producer's next output sets them again.
pub fn fit_snapshot(screen: &[u8], from: (u16, u16), to: (u16, u16)) -> Vec<u8> {
    let (from_cols, from_rows) = normalize_size(from.0, from.1);
    let (to_cols, to_rows) = normalize_size(to.0, to.1);
    if (from_cols, from_rows) == (to_cols, to_rows) {
        return screen.to_vec();
    }
    let mut parser = vt100::Parser::new(from_rows, from_cols, 0);
    parser.process(screen);
    let source = parser.screen();
    let (cursor_row, cursor_col) = source.cursor_position();
    let first = if from_rows > to_rows {
        (cursor_row + 1)
            .saturating_sub(to_rows)
            .min(from_rows - to_rows)
    } else {
        0
    };
    let mut out = b"\x1b[0m\x1b[r\x1b[H\x1b[2J".to_vec();
    for (index, row) in source
        .rows_formatted(0, to_cols)
        .skip(usize::from(first))
        .take(usize::from(to_rows))
        .enumerate()
    {
        out.extend_from_slice(format!("\x1b[{};1H", index + 1).as_bytes());
        out.extend_from_slice(&row);
        out.extend_from_slice(b"\x1b[0m");
    }
    let row = cursor_row.saturating_sub(first).min(to_rows - 1);
    let col = cursor_col.min(to_cols - 1);
    out.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
    if source.hide_cursor() {
        out.extend_from_slice(b"\x1b[?25l");
    }
    out
}

#[cfg(test)]
mod replay_tests {
    use super::*;

    #[test]
    fn unevicted_ring_replays_everything() {
        let mut ring = RingBuffer::new(64);
        ring.push(b"one\r\n");
        ring.push(b"\x1b[1mtwo\x1b[0m");
        assert!(!ring.evicted());
        assert_eq!(
            ring.replay(),
            RingReplay {
                bytes: b"one\r\n\x1b[1mtwo\x1b[0m",
                truncated: false
            }
        );
        // Exactly full is not an eviction.
        let mut full = RingBuffer::new(4);
        full.push(b"abcd");
        assert!(!full.evicted());
        assert_eq!(full.replay().bytes, b"abcd");
    }

    #[test]
    fn evicted_ring_starts_after_the_first_newline() {
        let mut ring = RingBuffer::new(16);
        ring.push(b"0123456789\r\n");
        ring.push(b"line two\r\n");
        assert!(ring.evicted());
        assert_eq!(ring.len(), 16);
        let replay = ring.replay();
        assert!(replay.truncated);
        assert_eq!(replay.bytes, b"line two\r\n");
        assert!(replay.bytes.len() <= ring.capacity());
    }

    #[test]
    fn evicted_ring_without_newline_starts_at_an_escape() {
        let mut ring = RingBuffer::new(13);
        ring.push(b"\x1b[31mabcdef\x1b[0mXY");
        // Kept: "mabcdef\x1b[0mXY": the cut is inside the first CSI.
        assert!(ring.evicted());
        let replay = ring.replay();
        assert!(replay.truncated);
        assert_eq!(replay.bytes, b"\x1b[0mXY");
    }

    #[test]
    fn evicted_ring_skips_leading_utf8_continuation_bytes() {
        let mut ring = RingBuffer::new(5);
        // "é" is C3 A9; keep the trailing A9 only.
        ring.push("aé🙂".as_bytes());
        assert!(ring.evicted());
        let replay = ring.replay();
        assert!(replay.truncated);
        assert_eq!(replay.bytes, "🙂".as_bytes());
        assert!(std::str::from_utf8(replay.bytes).is_ok());
    }

    #[test]
    fn newline_beyond_the_trim_window_is_not_searched() {
        let mut ring = RingBuffer::new(REPLAY_TRIM_WINDOW + 16);
        ring.push(b"xx");
        let mut long = vec![b'a'; REPLAY_TRIM_WINDOW + 8];
        long.extend_from_slice(b"\nz");
        ring.push(&long);
        ring.push(b"0123456789");
        let replay = ring.replay();
        assert!(replay.truncated);
        // No newline or ESC in the window and no continuation bytes: the
        // replay starts at the cut and stays within capacity.
        assert_eq!(replay.bytes.len(), ring.len());
        assert!(replay.bytes.len() <= ring.capacity());
    }

    #[test]
    fn zero_capacity_ring_replays_nothing_truncated() {
        let mut ring = RingBuffer::new(0);
        assert!(!ring.replay().truncated);
        ring.push(b"lost");
        assert_eq!(
            ring.replay(),
            RingReplay {
                bytes: b"",
                truncated: true
            }
        );
    }

    #[test]
    fn alternate_screen_with_evicted_history_is_withheld() {
        let mut mirror = ScreenMirror::new(20, 4, 0, 32);
        mirror.feed(b"main\r\n\x1b[?1049h");
        assert!(mirror.alternate_screen());
        // Not evicted: the enter sequence is replayed with the history.
        assert_eq!(mirror.history().bytes, b"main\r\n\x1b[?1049h");
        mirror.feed(&[b'#'; 40]);
        assert_eq!(
            mirror.history(),
            RingReplay {
                bytes: b"",
                truncated: true
            }
        );
        // Leaving the alternate screen makes the retained tail replayable.
        mirror.feed(b"\x1b[?1049l\r\ndone\r\n");
        assert!(!mirror.alternate_screen());
        let history = mirror.history();
        assert!(history.truncated);
        assert_eq!(history.bytes, b"done\r\n");
    }
}
