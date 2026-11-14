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
            screen: self.parser.screen().state_formatted(),
        }
    }

    pub fn ring(&self) -> &RingBuffer {
        &self.ring
    }
}
