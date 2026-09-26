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
//!
//! A tail can also be unbounded ([`OutputTail::unbounded`]) to hold a
//! container's whole transcript. The reader threads write through
//! [`TailWriter`] handles, so a caller that needs every byte can wait for them
//! to finish ([`OutputTail::wait_for_writers`]) after the container exits —
//! the process exiting does not mean its last output has been read yet.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

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
    /// Live [`TailWriter`]s — reader threads still feeding this tail.
    writers: Mutex<usize>,
    /// Signalled whenever a writer finishes.
    writer_done: Condvar,
}

/// A reader thread's handle for feeding an [`OutputTail`]. Dropping it (when
/// the thread's stream reaches EOF) tells the tail that this writer is done.
pub struct TailWriter {
    tail: Arc<OutputTail>,
}

impl TailWriter {
    pub fn push_bytes(&self, bytes: &[u8]) {
        self.tail.push_bytes(bytes);
    }
}

impl Drop for TailWriter {
    fn drop(&mut self) {
        let mut writers = self.tail.writers.lock().unwrap_or_else(|p| p.into_inner());
        *writers = writers.saturating_sub(1);
        self.tail.writer_done.notify_all();
    }
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
            writers: Mutex::new(0),
            writer_done: Condvar::new(),
        }
    }

    /// Create a tail with the default (`DEFAULT_OUTPUT_TAIL_LINES`) capacity.
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_OUTPUT_TAIL_LINES)
    }

    /// Create a tail that keeps every line: a container's full transcript.
    pub fn unbounded() -> Self {
        Self::new(usize::MAX)
    }

    /// Register a writer. Take it before spawning the reader thread, so a
    /// caller that waits for writers can never miss one that has not started.
    pub fn writer(self: &Arc<Self>) -> TailWriter {
        *self.writers.lock().unwrap_or_else(|p| p.into_inner()) += 1;
        TailWriter {
            tail: Arc::clone(self),
        }
    }

    /// Block until every [`TailWriter`] has been dropped — i.e. every byte the
    /// container wrote is in the tail — or `limit` passes. Returns whether
    /// all writers finished.
    pub fn wait_for_writers(&self, limit: Duration) -> bool {
        let writers = self.writers.lock().unwrap_or_else(|p| p.into_inner());
        let (writers, _) = self
            .writer_done
            .wait_timeout_while(writers, limit, |n| *n > 0)
            .unwrap_or_else(|p| p.into_inner());
        *writers == 0
    }

    /// The retained output as plain text: terminal escape sequences removed
    /// and a bare `\r` (a progress line redrawing itself) turned into a line
    /// break, so the redraws stay separate lines.
    pub fn plain_text(&self) -> String {
        let text = self.snapshot_text().replace('\r', "\n");
        strip_ansi_escapes::strip_str(text)
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

    #[test]
    fn an_unbounded_tail_keeps_every_line() {
        let tail = OutputTail::unbounded();
        for i in 0..(DEFAULT_OUTPUT_TAIL_LINES * 3) {
            tail.push_bytes(format!("line {i}\n").as_bytes());
        }
        assert_eq!(tail.snapshot().len(), DEFAULT_OUTPUT_TAIL_LINES * 3);
        assert_eq!(tail.snapshot()[0], "line 0");
    }

    #[test]
    fn plain_text_strips_escapes_and_splits_carriage_return_redraws() {
        let tail = OutputTail::new(10);
        tail.push_bytes(b"\x1b[32mok\x1b[0m\r\nstep 1\rstep 2\r\ndone");
        assert_eq!(tail.plain_text(), "ok\nstep 1\nstep 2\ndone\n");
    }

    #[test]
    fn plain_text_of_nothing_is_empty() {
        assert_eq!(OutputTail::new(10).plain_text(), "");
    }

    #[test]
    fn waiting_for_writers_returns_once_the_last_writer_is_dropped() {
        let tail = Arc::new(OutputTail::unbounded());
        let writer = tail.writer();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            writer.push_bytes(b"late\n");
        });
        assert!(tail.wait_for_writers(Duration::from_secs(5)));
        assert_eq!(tail.snapshot(), vec!["late"]);
        handle.join().unwrap();
    }

    #[test]
    fn waiting_for_writers_gives_up_at_the_limit() {
        let tail = Arc::new(OutputTail::unbounded());
        let _writer = tail.writer();
        assert!(!tail.wait_for_writers(Duration::from_millis(20)));
    }

    #[test]
    fn a_tail_with_no_writers_is_already_flushed() {
        assert!(OutputTail::new(1).wait_for_writers(Duration::ZERO));
    }
}
