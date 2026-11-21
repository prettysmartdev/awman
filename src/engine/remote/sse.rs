//! Bounded Server-Sent Events framing.
//!
//! Layer 1. [`SseDecoder`] accumulates raw response bytes and yields one
//! [`SseItem`] per complete frame. Frames are cut on bytes — a frame ends at a
//! blank line, with LF or CRLF line endings — and UTF-8 is decoded once per
//! complete frame, so a multibyte character split across HTTP chunks is never
//! corrupted. The buffer holds at most one incomplete frame plus the chunk
//! being pushed: a frame longer than the limit is a [`FrameTooLarge`] protocol
//! error instead of unbounded growth.
//!
//! Supported fields: `data:` (with or without one leading space, repeated
//! lines joined by `\n`), `id:`, `event:` and `:` comments. `retry:` and
//! unknown fields are ignored, as the SSE specification requires.

/// Largest SSE frame (bytes between blank lines) a client accepts.
pub const MAX_SSE_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// One complete, UTF-8-valid frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseFrame {
    /// The last `id:` value, if any.
    pub id: Option<String>,
    /// The last `event:` value, if any.
    pub event: Option<String>,
    /// The joined `data:` lines; `None` when the frame had no `data:` line
    /// (a comment or keepalive frame), which dispatches nothing.
    pub data: Option<String>,
}

/// What one complete frame decoded to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseItem {
    Frame(SseFrame),
    /// The frame was not valid UTF-8. `id` is its `id:` value when that line
    /// alone could still be read.
    Malformed {
        id: Option<String>,
        reason: String,
    },
}

/// A frame (or the incomplete frame being buffered) exceeded the limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTooLarge {
    pub limit: usize,
    pub observed: usize,
}

impl std::fmt::Display for FrameTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SSE frame of at least {} bytes exceeds the {} byte limit",
            self.observed, self.limit
        )
    }
}

impl std::error::Error for FrameTooLarge {}

/// Incremental byte-level SSE framer. See the module docs.
#[derive(Debug)]
pub struct SseDecoder {
    buf: Vec<u8>,
    /// Start of the line currently being scanned, relative to `buf`.
    line_start: usize,
    limit: usize,
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::with_limit(MAX_SSE_FRAME_BYTES)
    }

    pub fn with_limit(limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            line_start: 0,
            limit,
        }
    }

    /// Append `chunk` and push every frame it completes onto `out`, in order.
    /// After an error the decoder is cleared; the stream must not continue.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<SseItem>) -> Result<(), FrameTooLarge> {
        // Only the new bytes need scanning for line ends.
        let mut scan = self.buf.len();
        self.buf.extend_from_slice(chunk);
        let mut consumed = 0;
        while let Some(rel) = self.buf[scan..].iter().position(|&b| b == b'\n') {
            let newline = scan + rel;
            let mut line_end = newline;
            if line_end > self.line_start && self.buf[line_end - 1] == b'\r' {
                line_end -= 1;
            }
            if line_end == self.line_start {
                // Blank line: the frame is everything before it.
                let frame = &self.buf[consumed..self.line_start];
                if frame.len() > self.limit {
                    return Err(self.fail(frame.len()));
                }
                if !frame.is_empty() {
                    out.push(parse_frame(frame));
                }
                consumed = newline + 1;
            }
            self.line_start = newline + 1;
            scan = newline + 1;
        }
        self.buf.drain(..consumed);
        self.line_start -= consumed;
        let frame_bytes = self.buffered_frame_bytes();
        if frame_bytes > self.limit {
            return Err(self.fail(frame_bytes));
        }
        Ok(())
    }

    /// Whether bytes of an unfinished frame are buffered (EOF mid-frame).
    pub fn has_partial_frame(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Bytes currently buffered.
    pub fn buffered_len(&self) -> usize {
        self.buf.len()
    }

    /// Bytes of the unfinished frame. A trailing `\r` that opens a line could
    /// be the first half of the frame's closing CRLF blank line; it is
    /// delimiter lookahead, not frame content, so a limit-sized frame is
    /// accepted however its terminator is partitioned.
    fn buffered_frame_bytes(&self) -> usize {
        let len = self.buf.len();
        if len > 0 && len == self.line_start + 1 && self.buf[len - 1] == b'\r' {
            len - 1
        } else {
            len
        }
    }

    fn fail(&mut self, observed: usize) -> FrameTooLarge {
        self.buf = Vec::new();
        self.line_start = 0;
        FrameTooLarge {
            limit: self.limit,
            observed,
        }
    }
}

/// Lines of a frame with their terminators (LF or CRLF) removed. The frame
/// slice ends with its last line's terminator, so the final split is empty.
fn frame_lines(frame: &[u8]) -> impl Iterator<Item = &[u8]> {
    frame
        .split(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
}

fn split_field(line: &[u8]) -> (&[u8], &[u8]) {
    match line.iter().position(|&b| b == b':') {
        Some(i) => {
            let value = &line[i + 1..];
            (&line[..i], value.strip_prefix(b" ").unwrap_or(value))
        }
        None => (line, &[]),
    }
}

fn parse_frame(frame: &[u8]) -> SseItem {
    if std::str::from_utf8(frame).is_err() {
        // Recover the cursor if its own line is readable, so a resume does
        // not replay the undecodable frame.
        let id = frame_lines(frame)
            .filter(|line| !line.is_empty() && line[0] != b':')
            .map(split_field)
            .filter(|(field, _)| *field == b"id")
            .filter_map(|(_, value)| std::str::from_utf8(value).ok())
            .last()
            .map(str::to_string);
        return SseItem::Malformed {
            id,
            reason: "frame is not valid UTF-8".into(),
        };
    }
    let mut parsed = SseFrame::default();
    let mut data: Option<String> = None;
    for line in frame_lines(frame) {
        if line.is_empty() || line[0] == b':' {
            continue;
        }
        let (field, value) = split_field(line);
        // Checked above: every slice of a valid frame cut at ASCII is valid.
        let value = std::str::from_utf8(value).expect("frame is UTF-8");
        match field {
            b"data" => match data.as_mut() {
                Some(joined) => {
                    joined.push('\n');
                    joined.push_str(value);
                }
                None => data = Some(value.to_string()),
            },
            b"id" if !value.contains('\0') => parsed.id = Some(value.to_string()),
            b"event" => parsed.event = Some(value.to_string()),
            _ => {}
        }
    }
    parsed.data = data;
    SseItem::Frame(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: Option<&str>, event: Option<&str>, data: Option<&str>) -> SseItem {
        SseItem::Frame(SseFrame {
            id: id.map(str::to_string),
            event: event.map(str::to_string),
            data: data.map(str::to_string),
        })
    }

    fn decode_chunks(chunks: &[&[u8]]) -> (Vec<SseItem>, SseDecoder) {
        let mut decoder = SseDecoder::new();
        let mut out = Vec::new();
        for chunk in chunks {
            decoder.push(chunk, &mut out).unwrap();
        }
        (out, decoder)
    }

    /// Representative stream: multibyte text (2-, 3- and 4-byte UTF-8),
    /// multi-line data, a comment frame, CRLF and LF frames, several frames.
    fn sample() -> (Vec<u8>, Vec<SseItem>) {
        let bytes = concat!(
            ": keepalive\n\n",
            "id: 1:0\nevent: output\ndata: {\"line\":\"héllo — 日本語 🚀\"}\n\n",
            "id: 1:1\r\nevent: output\r\ndata:first\r\ndata: second ✓\r\n\r\n",
            "data: ünïcode only\n: trailing comment\n\n",
        )
        .as_bytes()
        .to_vec();
        let expected = vec![
            frame(
                Some("1:0"),
                Some("output"),
                Some("{\"line\":\"héllo — 日本語 🚀\"}"),
            ),
            frame(Some("1:1"), Some("output"), Some("first\nsecond ✓")),
            frame(None, None, Some("ünïcode only")),
        ];
        (bytes, expected)
    }

    fn without_comment_frames(items: Vec<SseItem>) -> Vec<SseItem> {
        items
            .into_iter()
            .filter(|item| !matches!(item, SseItem::Frame(SseFrame { data: None, .. })))
            .collect()
    }

    #[test]
    fn every_single_split_point_yields_the_same_frames() {
        let (bytes, expected) = sample();
        for split in 0..=bytes.len() {
            let (out, decoder) = decode_chunks(&[&bytes[..split], &bytes[split..]]);
            assert_eq!(
                without_comment_frames(out),
                expected,
                "split at byte {split}"
            );
            assert!(!decoder.has_partial_frame(), "split at byte {split}");
        }
    }

    #[test]
    fn every_pair_of_split_points_yields_the_same_frames() {
        let (bytes, expected) = sample();
        for a in 0..=bytes.len() {
            for b in a..=bytes.len() {
                let (out, _) = decode_chunks(&[&bytes[..a], &bytes[a..b], &bytes[b..]]);
                assert_eq!(without_comment_frames(out), expected, "splits {a},{b}");
            }
        }
    }

    #[test]
    fn one_byte_at_a_time_yields_the_same_frames() {
        let (bytes, expected) = sample();
        let chunks: Vec<&[u8]> = bytes.chunks(1).collect();
        let (out, _) = decode_chunks(&chunks);
        assert_eq!(without_comment_frames(out), expected);
    }

    #[test]
    fn several_frames_in_one_chunk_arrive_in_order() {
        let (bytes, expected) = sample();
        let (out, _) = decode_chunks(&[&bytes]);
        assert_eq!(out.len(), 4, "comment frame plus three data frames");
        assert_eq!(out[0], frame(None, None, None));
        assert_eq!(without_comment_frames(out), expected);
    }

    #[test]
    fn data_without_space_keeps_further_spaces_and_empty_data_lines() {
        let (out, _) = decode_chunks(&[b"data:  two\ndata\ndata:\n\n"]);
        assert_eq!(out, vec![frame(None, None, Some(" two\n\n"))]);
    }

    #[test]
    fn leading_blank_lines_and_unknown_fields_are_ignored() {
        let (out, _) = decode_chunks(&[b"\n\r\nretry: 10\nfoo: bar\ndata: x\n\n"]);
        assert_eq!(out, vec![frame(None, None, Some("x"))]);
    }

    #[test]
    fn invalid_utf8_frame_is_malformed_with_its_id_and_the_stream_continues() {
        let mut bytes = b"id: 1:4\ndata: {\"bad\":\"".to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe]);
        bytes.extend_from_slice(b"\"}\n\ndata: next\n\n");
        let (out, _) = decode_chunks(&[&bytes]);
        assert_eq!(
            out,
            vec![
                SseItem::Malformed {
                    id: Some("1:4".into()),
                    reason: "frame is not valid UTF-8".into()
                },
                frame(None, None, Some("next")),
            ]
        );
    }

    #[test]
    fn truncated_frame_stays_buffered_and_is_reported_as_partial() {
        let (out, decoder) = decode_chunks(&[b"data: whole\n\nid: 1:2\ndata: {\"cut"]);
        assert_eq!(out, vec![frame(None, None, Some("whole"))]);
        assert!(decoder.has_partial_frame());
        let (_, decoder) = decode_chunks(&[b"data: x\n"]);
        assert!(
            decoder.has_partial_frame(),
            "one newline does not end a frame"
        );
    }

    #[test]
    fn an_incomplete_frame_over_the_limit_fails_and_clears_the_buffer() {
        let mut decoder = SseDecoder::with_limit(16);
        let mut out = Vec::new();
        decoder.push(b"data: 0123456", &mut out).unwrap();
        let err = decoder.push(b"789abcdef", &mut out).unwrap_err();
        assert_eq!(err.limit, 16);
        assert!(err.observed > 16);
        assert_eq!(decoder.buffered_len(), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn a_complete_frame_over_the_limit_fails_even_in_one_chunk() {
        let mut decoder = SseDecoder::with_limit(8);
        let mut out = Vec::new();
        let err = decoder
            .push(b"data: 0123456789\n\ndata: ok\n\n", &mut out)
            .unwrap_err();
        assert_eq!(err.limit, 8);
        assert!(out.is_empty());
    }

    #[test]
    fn the_buffer_never_exceeds_one_frame_plus_one_chunk() {
        let mut decoder = SseDecoder::with_limit(64);
        let mut out = Vec::new();
        for _ in 0..10_000 {
            decoder.push(b"data: abc\n\n: c\n\nda", &mut out).unwrap();
            decoder.push(b"ta: d\n\n", &mut out).unwrap();
            assert!(decoder.buffered_len() <= 64);
        }
        assert_eq!(out.len(), 30_000);
    }

    #[test]
    fn a_frame_exactly_at_the_limit_is_accepted() {
        let mut decoder = SseDecoder::with_limit(9);
        let mut out = Vec::new();
        decoder.push(b"data: 12\n\n", &mut out).unwrap();
        assert_eq!(out, vec![frame(None, None, Some("12"))]);
    }

    /// Every two-cut partition of a frame of exactly `limit` bytes (LF and
    /// CRLF endings, including the closing blank line's CR and LF arriving
    /// separately) is accepted; with one byte less of limit every partition
    /// fails.
    #[test]
    fn a_limit_sized_frame_is_accepted_for_every_partition_of_its_terminator() {
        for (frame_bytes, stream) in [
            (&b"data: 12\n"[..], &b"data: 12\n\n"[..]),
            (&b"data: 12\n"[..], &b"data: 12\n\r\n"[..]),
            (&b"data: 12\r\n"[..], &b"data: 12\r\n\r\n"[..]),
        ] {
            let limit = frame_bytes.len();
            for a in 0..=stream.len() {
                for b in a..=stream.len() {
                    let chunks = [&stream[..a], &stream[a..b], &stream[b..]];
                    let mut decoder = SseDecoder::with_limit(limit);
                    let mut out = Vec::new();
                    for chunk in chunks {
                        if let Err(err) = decoder.push(chunk, &mut out) {
                            panic!("{stream:?} cut at {a},{b} with limit {limit}: {err}");
                        }
                    }
                    assert_eq!(
                        out,
                        vec![frame(None, None, Some("12"))],
                        "{stream:?} {a},{b}"
                    );
                    assert!(!decoder.has_partial_frame());

                    let mut decoder = SseDecoder::with_limit(limit - 1);
                    let mut out = Vec::new();
                    let failed = chunks
                        .iter()
                        .any(|chunk| decoder.push(chunk, &mut out).is_err());
                    assert!(failed && out.is_empty(), "{stream:?} {a},{b} over limit");
                }
            }
        }
    }
}
