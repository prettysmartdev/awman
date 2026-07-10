//! `OutputTail` — a bounded, line-oriented ring buffer of the most recent
//! combined stdout/stderr a running container has emitted.
//!
//! The container I/O bridge feeds every byte chunk into this buffer while a
//! workflow step runs. If the step's container later exits with an unexpected
//! non-zero code, the workflow engine snapshots the tail and writes it to a
//! failure log so the user can see what the container printed just before it
//! died — even after the TUI has scrolled that output away.
//!
//! Combined stdout+stderr: the bridge's reader threads funnel both streams into
//! the same tail, matching what the user saw interleaved on screen.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Default number of lines retained. "~100 lines" per the feature spec.
pub const DEFAULT_OUTPUT_TAIL_LINES: usize = 100;

/// Upper bound on a single un-terminated line. A container that streams a very
/// long line with no newline (e.g. a progress bar redrawing) must not grow the
/// partial-line buffer without bound; once a line crosses this it is committed
/// as-is and a fresh partial begins.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// Bounded ring buffer of recent output lines. Cheap to clone the `Arc` that
/// wraps it; the buffer itself is guarded by a mutex so the bridge's reader
/// threads and the engine's snapshot call can share it.
pub struct OutputTail {
    capacity: usize,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Completed lines, oldest at the front. Never exceeds `capacity`.
    lines: VecDeque<String>,
    /// Bytes seen since the last newline — the in-progress trailing line.
    partial: Vec<u8>,
}

impl Inner {
    /// Commit one completed line, evicting the oldest if at capacity.
    fn commit(&mut self, line: String, capacity: usize) {
        if self.lines.len() >= capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }
}

/// Decode a raw line to a `String`, dropping a trailing `\r` so `\r\n`
/// terminators from PTY output don't leave carriage returns in the log.
fn decode_line(bytes: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(bytes).into_owned();
    if s.ends_with('\r') {
        s.pop();
    }
    s
}

impl OutputTail {
    /// Create a tail retaining up to `capacity` lines (clamped to at least 1).
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Create a tail with the default (`DEFAULT_OUTPUT_TAIL_LINES`) capacity.
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_OUTPUT_TAIL_LINES)
    }

    /// Append a raw byte chunk from the container, splitting on `\n`. A chunk
    /// that ends mid-line is retained and completed by a later chunk.
    pub fn push_bytes(&self, bytes: &[u8]) {
        let mut inner = self.lock();
        for &b in bytes {
            if b == b'\n' {
                let raw = std::mem::take(&mut inner.partial);
                let line = decode_line(&raw);
                inner.commit(line, self.capacity);
            } else {
                inner.partial.push(b);
                if inner.partial.len() >= MAX_LINE_BYTES {
                    let raw = std::mem::take(&mut inner.partial);
                    let line = decode_line(&raw);
                    inner.commit(line, self.capacity);
                }
            }
        }
    }

    /// Snapshot the retained lines, oldest first. Any un-terminated trailing
    /// output is included as a final line so nothing the container printed is
    /// lost just because it didn't end in a newline.
    pub fn snapshot(&self) -> Vec<String> {
        let inner = self.lock();
        let mut out: Vec<String> = inner.lines.iter().cloned().collect();
        if !inner.partial.is_empty() {
            out.push(decode_line(&inner.partial));
        }
        out
    }

    /// Snapshot the retained output as a single newline-joined string with a
    /// trailing newline (empty string when nothing was captured).
    pub fn snapshot_text(&self) -> String {
        let lines = self.snapshot();
        if lines.is_empty() {
            String::new()
        } else {
            let mut text = lines.join("\n");
            text.push('\n');
            text
        }
    }

    /// Lock the inner buffer, recovering from a poisoned mutex — a reader
    /// thread panicking must not permanently disable output capture.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_lines_on_newline() {
        let tail = OutputTail::new(10);
        tail.push_bytes(b"alpha\nbeta\n");
        assert_eq!(tail.snapshot(), vec!["alpha", "beta"]);
    }

    #[test]
    fn retains_only_last_capacity_lines() {
        let tail = OutputTail::new(2);
        tail.push_bytes(b"one\ntwo\nthree\nfour\n");
        assert_eq!(tail.snapshot(), vec!["three", "four"]);
    }

    #[test]
    fn combines_chunked_partial_lines() {
        let tail = OutputTail::new(10);
        tail.push_bytes(b"hel");
        tail.push_bytes(b"lo\nwor");
        tail.push_bytes(b"ld\n");
        assert_eq!(tail.snapshot(), vec!["hello", "world"]);
    }

    #[test]
    fn includes_unterminated_trailing_line() {
        let tail = OutputTail::new(10);
        tail.push_bytes(b"done\nno newline here");
        assert_eq!(tail.snapshot(), vec!["done", "no newline here"]);
    }

    #[test]
    fn strips_carriage_returns_from_crlf() {
        let tail = OutputTail::new(10);
        tail.push_bytes(b"windows\r\nline\r\n");
        assert_eq!(tail.snapshot(), vec!["windows", "line"]);
    }

    #[test]
    fn snapshot_text_joins_with_newlines_and_trailing_newline() {
        let tail = OutputTail::new(10);
        tail.push_bytes(b"a\nb\n");
        assert_eq!(tail.snapshot_text(), "a\nb\n");
    }

    #[test]
    fn snapshot_text_empty_when_no_output() {
        let tail = OutputTail::new(10);
        assert_eq!(tail.snapshot_text(), "");
    }

    #[test]
    fn very_long_line_without_newline_is_bounded() {
        let tail = OutputTail::new(3);
        // Twice the max line length with no newline: must not retain it all as
        // one unbounded partial, and must respect the line capacity.
        let blob = vec![b'x'; MAX_LINE_BYTES * 2 + 10];
        tail.push_bytes(&blob);
        let snap = tail.snapshot();
        assert!(
            snap.len() <= 3,
            "must respect line capacity, got {}",
            snap.len()
        );
    }

    #[test]
    fn interleaved_stdout_stderr_share_one_tail() {
        // The bridge feeds both streams into the same tail; order reflects
        // arrival order at the reader threads.
        let tail = OutputTail::new(10);
        tail.push_bytes(b"out1\n");
        tail.push_bytes(b"err1\n");
        tail.push_bytes(b"out2\n");
        assert_eq!(tail.snapshot(), vec!["out1", "err1", "out2"]);
    }
}
