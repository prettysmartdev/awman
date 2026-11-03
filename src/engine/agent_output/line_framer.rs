//! Bounded byte-oriented framing for newline-delimited agent output.
//!
//! Bytes are held until a complete line is available, so UTF-8 characters may
//! span arbitrary input chunks. Lines longer than the configured limit retain
//! their bounded prefix and are marked truncated; remaining bytes are skipped
//! through the next newline. UTF-8 decoding is explicitly lossy at the point
//! a consumer asks for text.

/// Maximum line prefix retained by production output pumps (64 KiB).
pub const MAX_LINE_BYTES: usize = 64 * 1024;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: Vec<FramedLine>) -> Vec<(String, bool)> {
        lines
            .into_iter()
            .map(|line| (line.lossy_text(), line.truncated))
            .collect()
    }

    #[test]
    fn every_multibyte_utf8_split_point_is_preserved() {
        let input = "a¢€🦀z\n".as_bytes();
        for split in 1..input.len() {
            let mut framer = LineFramer::default();
            let mut lines = framer.push(&input[..split]);
            lines.extend(framer.push(&input[split..]));
            assert_eq!(text(lines), vec![("a¢€🦀z".into(), false)], "split {split}");
        }
    }

    #[test]
    fn frames_many_and_empty_lines_in_one_chunk() {
        let mut framer = LineFramer::default();
        assert_eq!(
            text(framer.push(b"one\n\nthree\nfour\n")),
            vec![
                ("one".into(), false),
                ("".into(), false),
                ("three".into(), false),
                ("four".into(), false),
            ]
        );
        assert!(framer.finish().is_none());
    }

    #[test]
    fn invalid_utf8_is_replaced_explicitly() {
        let mut framer = LineFramer::default();
        let lines = framer.push(b"valid\xff\xfe\n");
        assert_eq!(lines[0].lossy_text(), "valid��");
        assert!(!lines[0].truncated);
    }

    #[test]
    fn eof_flushes_unterminated_line_but_not_empty_tail() {
        let mut framer = LineFramer::default();
        assert!(framer.push(b"last").is_empty());
        assert_eq!(framer.finish().unwrap().lossy_text(), "last");
        assert!(framer.finish().is_none());

        let mut after_newline = LineFramer::default();
        assert_eq!(
            text(after_newline.push(b"done\n")),
            vec![("done".into(), false)]
        );
        assert!(after_newline.finish().is_none());
    }

    #[test]
    fn overlong_lines_keep_a_bounded_marked_prefix_and_skip_the_rest() {
        let mut exact = LineFramer::new(4);
        let exact_line = exact.push(b"abcd\n").pop().unwrap();
        assert_eq!(exact_line.bytes, b"abcd");
        assert!(!exact_line.truncated);

        let mut framer = LineFramer::new(4);
        let mut lines = framer.push(b"abcdef");
        assert!(lines.is_empty());
        lines.extend(framer.push(b"ghi\nnext\n"));
        assert_eq!(lines[0].bytes, b"abcd");
        assert!(lines[0].truncated);
        assert_eq!(lines[0].marked_lossy_text(), "abcd [line truncated]");
        assert_eq!(text(lines[1..].to_vec()), vec![("next".into(), false)]);

        let mut eof = LineFramer::new(2);
        eof.push(b"long");
        let final_line = eof.finish().unwrap();
        assert_eq!(final_line.bytes, b"lo");
        assert!(final_line.truncated);
    }
}
