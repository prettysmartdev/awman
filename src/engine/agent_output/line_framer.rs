//! Bounded byte-oriented framing for newline-delimited agent output.
//!
//! Bytes are held until a complete line is available, so UTF-8 characters may
//! span arbitrary input chunks. Lines longer than the configured limit retain
//! their bounded prefix and are marked truncated; remaining bytes are skipped
//! through the next newline. UTF-8 decoding is explicitly lossy at the point
//! a consumer asks for text.

/// Maximum line prefix retained by production output pumps (64 KiB).
pub const MAX_LINE_BYTES: usize = 64 * 1024;
/// Structured agent frames can contain tool bodies larger than a text line.
pub const MAX_STRUCTURED_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// One line without its newline delimiter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramedLine {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

impl FramedLine {
    /// Decode invalid UTF-8 with the standard replacement character.
    pub fn lossy_text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// Text suitable for display, with an explicit marker for a bounded line.
    pub fn marked_lossy_text(&self) -> String {
        let mut text = self.lossy_text();
        if self.truncated {
            text.push_str(" [line truncated]");
        }
        text
    }
}

/// Frames newline-delimited bytes while retaining at most `max_line_bytes`.
#[derive(Debug)]
pub struct LineFramer {
    max_line_bytes: usize,
    current: Vec<u8>,
    truncated: bool,
}

impl Default for LineFramer {
    fn default() -> Self {
        Self::new(MAX_LINE_BYTES)
    }
}

impl LineFramer {
    pub fn new(max_line_bytes: usize) -> Self {
        Self {
            max_line_bytes,
            current: Vec::with_capacity(max_line_bytes.min(256)),
            truncated: false,
        }
    }

    /// Append a chunk and return every complete line in order.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<FramedLine> {
        let mut lines = Vec::new();
        for &byte in chunk {
            if byte == b'\n' {
                lines.push(self.take_line());
            } else if !self.truncated {
                if self.current.len() < self.max_line_bytes {
                    self.current.push(byte);
                } else {
                    self.truncated = true;
                }
            }
        }
        lines
    }

    /// Flush an unterminated final line at EOF. An empty stream, or a stream
    /// ending immediately after a newline, does not produce an extra line.
    pub fn finish(&mut self) -> Option<FramedLine> {
        if self.current.is_empty() && !self.truncated {
            return None;
        }
        Some(self.take_line())
    }

    fn take_line(&mut self) -> FramedLine {
        FramedLine {
            bytes: std::mem::replace(
                &mut self.current,
                Vec::with_capacity(self.max_line_bytes.min(256)),
            ),
            truncated: std::mem::replace(&mut self.truncated, false),
        }
    }
}
