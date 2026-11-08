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

/// Bounded FIFO of raw output bytes; the newest `capacity` bytes are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RingBuffer {
    buf: Vec<u8>,
    capacity: usize,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: Vec::new(),
            capacity,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.capacity == 0 {
            return;
        }
        if bytes.len() >= self.capacity {
            self.buf.clear();
            self.buf
                .extend_from_slice(&bytes[bytes.len() - self.capacity..]);
        } else {
            let discard = self
                .buf
                .len()
                .saturating_add(bytes.len())
                .saturating_sub(self.capacity);
            self.buf.drain(..discard);
            self.buf.extend_from_slice(bytes);
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
            ring: RingBuffer::new(ring_bytes),
        }
    }

    /// Feed one output chunk to the parser and the ring.
    pub fn feed(&mut self, bytes: &[u8]) {
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
            cols,
            rows,
            screen: self.parser.screen().state_formatted(),
        }
    }

    pub fn ring(&self) -> &RingBuffer {
        &self.ring
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_only_the_newest_bytes_including_oversized_and_empty_writes() {
        for capacity in 0..33 {
            let mut ring = RingBuffer::new(capacity);
            let mut expected = Vec::new();
            for len in 0..100 {
                let bytes = vec![len as u8; len];
                ring.push(&bytes);
                expected.extend(bytes);
                expected.drain(..expected.len().saturating_sub(capacity));
                assert_eq!(ring.as_slice(), expected);
                assert!(ring.len() <= capacity);
            }
            assert_eq!(ring.capacity(), capacity);
            assert_eq!(ring.is_empty(), capacity == 0);
        }
    }

    #[test]
    fn snapshot_matches_vt100_after_ten_thousand_arbitrary_chunks() {
        let mut mirror = ScreenMirror::new(20, 6, 0, 57);
        let mut reference = vt100::Parser::new(6, 20, 0);
        let mut seed = 0x0141_a116_99c7_305d_u64;
        let mut random = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let escapes: &[&[u8]] = &[
            b"\x1b[2J",
            b"\x1b[H",
            b"\x1b[31;1mred\x1b[0m",
            b"\r\n",
            b"\x1b[?1049h",
            b"\x1b[?1049l",
            b"\x1b[2;4H",
            b"\x1b[?25l",
            b"\x1b[?25h",
            "wide:界🙂 e\u{301}".as_bytes(),
            b"\x1bc",
        ];
        for index in 0..10_000 {
            let bytes = if index % 3 == 0 {
                escapes[random() as usize % escapes.len()].to_vec()
            } else {
                (0..random() as usize % 48)
                    .map(|_| random() as u8)
                    .collect()
            };
            // Split in arbitrary places, including inside UTF-8 and CSI.
            let split = random() as usize % (bytes.len() + 1);
            for chunk in [&bytes[..split], &bytes[split..]] {
                mirror.feed(chunk);
                reference.process(chunk);
            }
            if index % 71 == 0 {
                let cols = 2 + (random() % 30) as u16;
                let rows = 2 + (random() % 8) as u16;
                mirror.resize(cols, rows);
                reference.screen_mut().set_size(rows, cols);
            }
            let snapshot = mirror.snapshot();
            assert_eq!(
                snapshot.screen,
                reference.screen().state_formatted(),
                "iteration {index}"
            );
            assert_eq!((snapshot.rows, snapshot.cols), reference.screen().size());
            assert!(mirror.ring().len() <= 57);
        }
    }

    #[test]
    fn ansi_snapshot_redraws_colours_wide_cells_cursor_and_blank_areas() {
        let mut mirror = ScreenMirror::new(20, 6, 0, 0);
        mirror.feed("\x1b[32;1m界🙂text\x1b[0m\r\nsecond\x1b[1;3H\x1b[?25l".as_bytes());
        let snapshot = mirror.snapshot();
        let mut viewer = vt100::Parser::new(6, 20, 0);
        viewer.process(b"stale text\r\nmore stale text\x1b[35m");
        viewer.process(&snapshot.screen);
        assert_eq!(viewer.screen().state_formatted(), snapshot.screen);
        assert_eq!(
            viewer.screen().cursor_position(),
            mirror.parser.screen().cursor_position()
        );
        assert!(viewer.screen().hide_cursor());
        assert!(mirror.ring().is_empty());
    }

    #[test]
    fn degenerate_dimensions_are_normalized_before_reaching_vt100() {
        let mut mirror = ScreenMirror::new(0, 0, 0, 0);
        assert_eq!(mirror.size(), (2, 2));
        mirror.feed("界界界界".as_bytes());
        mirror.resize(1, 1);
        mirror.feed(b"abcdefghijk");
        assert_eq!(mirror.size(), (2, 2));
    }
}

#[cfg(test)]
mod mode_regression {
    use super::*;
    #[test]
    fn snapshot_restores_input_modes_as_well_as_the_visible_screen() {
        let mut mirror = ScreenMirror::new(80, 24, 0, 0);
        mirror.feed(b"\x1b[?1h\x1b=\x1b[?2004h\x1b[?1000hhello");
        let mut viewer = vt100::Parser::new(24, 80, 0);
        viewer.process(&mirror.snapshot().screen);
        assert_eq!(
            viewer.screen().input_mode_formatted(),
            mirror.parser.screen().input_mode_formatted()
        );
        assert_eq!(viewer.screen().contents(), "hello");
    }
}
