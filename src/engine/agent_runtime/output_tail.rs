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
