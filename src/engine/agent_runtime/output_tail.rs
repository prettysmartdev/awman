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
//! A tail can also be disk-backed to hold a container's whole transcript.
//! The reader threads write through
//! [`TailWriter`] handles, so a caller that needs every byte can wait for them
//! to finish ([`OutputTail::wait_for_writers`]) after the container exits —
//! the process exiting does not mean its last output has been read yet.

use std::collections::VecDeque;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Default number of lines retained. "~100 lines" per the feature spec.
pub const DEFAULT_OUTPUT_TAIL_LINES: usize = 100;

/// Upper bound on a single un-terminated line. A container that streams a very
/// long line with no newline (e.g. a progress bar redrawing) must not grow the
/// partial-line buffer without bound; once a line crosses this it is committed
/// as-is and a fresh partial begins.
const MAX_LINE_BYTES: usize = 64 * 1024;
pub const MAX_TRANSCRIPT_SPOOL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TRANSCRIPT_STRING_BYTES: u64 = 1024 * 1024;

struct Spool {
    file: BufWriter<std::fs::File>,
    bytes: u64,
    failure: Option<String>,
    warning_pending: bool,
}
impl Spool {
    fn fail(&mut self, error: impl std::fmt::Display) {
        if self.failure.is_none() {
            self.failure = Some(format!("[awman: transcript incomplete after {} bytes: {error}; the last 100 lines follow]\n", self.bytes));
            self.warning_pending = true;
            tracing::warn!("{}", self.failure.as_deref().unwrap_or_default());
        }
    }
}

/// Independent bounded-memory cursor. Readers and writers serialize file seeks.
struct SpoolReader {
    inner: Arc<Mutex<Inner>>,
    position: u64,
    end: u64,
}
impl Read for SpoolReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = bytes.len().min((self.end - self.position) as usize);
        if count == 0 {
            return Ok(0);
        }
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let spool = inner.spool.as_mut().expect("reader owns a spool");
        spool.file.flush()?;
        let file = spool.file.get_mut();
        file.seek(SeekFrom::Start(self.position))?;
        let result = file.read(&mut bytes[..count]);
        file.seek(SeekFrom::End(0))?;
        let n = result?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "transcript spool ended early",
            ));
        }
        self.position += n as u64;
        Ok(n)
    }
}

/// Bounded ring buffer of recent output lines. Cheap to clone the `Arc` that
/// wraps it; the buffer itself is guarded by a mutex so the bridge's reader
/// threads and the engine's snapshot call can share it.
pub struct OutputTail {
    capacity: usize,
    inner: Arc<Mutex<Inner>>,
    /// Live [`TailWriter`]s — reader threads still feeding this tail.
    writers: Mutex<usize>,
    /// Signalled whenever a writer finishes.
    writer_done: Condvar,
    warning_sink: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>,
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
        if *writers == 0 {
            self.tail.close_capture();
        }
        self.tail.writer_done.notify_all();
    }
}

#[derive(Default)]
struct Inner {
    /// Completed lines, oldest at the front. Never exceeds `capacity`.
    lines: VecDeque<String>,
    /// Bytes seen since the last newline — the in-progress trailing line.
    partial: Vec<u8>,
    spool: Option<Spool>,
}

impl Inner {
    fn tail_text(&self) -> String {
        // A hundred pathological long lines can themselves exceed the String
        // API bound. Select suffixes before allocating the diagnostic view.
        let partial = (!self.partial.is_empty()).then(|| decode_line(&self.partial));
        let lines = self
            .lines
            .iter()
            .map(String::as_str)
            .chain(partial.as_deref());
        let mut remaining = MAX_TRANSCRIPT_STRING_BYTES as usize - 1024;
        let mut selected = Vec::new();
        let mut truncated = false;
        for line in lines.rev() {
            if line.len() < remaining {
                selected.push(line);
                remaining -= line.len() + 1;
            } else {
                truncated = true;
                if remaining > 1 {
                    let mut start = line.len().saturating_sub(remaining - 1);
                    while !line.is_char_boundary(start) {
                        start += 1;
                    }
                    selected.push(&line[start..]);
                }
                break;
            }
        }
        let mut text = if truncated {
            "[awman: diagnostic tail exceeds 1 MiB; newest bytes follow]\n".to_owned()
        } else {
            String::new()
        };
        for line in selected.into_iter().rev() {
            text.push_str(line);
            text.push('\n');
        }
        text
    }
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
            inner: Arc::new(Mutex::new(Inner::default())),
            writers: Mutex::new(0),
            writer_done: Condvar::new(),
            warning_sink: Mutex::new(None),
        }
    }

    /// Create a tail with the default (`DEFAULT_OUTPUT_TAIL_LINES`) capacity.
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_OUTPUT_TAIL_LINES)
    }

    /// Establish private transcript storage before starting the child.
    pub fn spooled(data_home: &Path) -> io::Result<Self> {
        let dir = data_home.join("spool").join("transcripts");
        crate::data::fs::private_files::create_private_dir(&dir).map_err(io::Error::other)?;
        let file = tempfile::tempfile_in(dir)?;
        let tail = Self::with_default_capacity();
        tail.lock().spool = Some(Spool {
            file: BufWriter::with_capacity(64 * 1024, file),
            bytes: 0,
            failure: None,
            warning_pending: false,
        });
        Ok(tail)
    }

    /// Read the full ordered post-redaction transcript with bounded memory.
    /// Storage faults return an explicit incomplete notice and the retained tail.
    pub fn transcript_reader(&self) -> io::Result<Box<dyn Read + Send>> {
        let mut inner = self.lock();
        if let Some(spool) = &mut inner.spool {
            if let Err(error) = spool.file.flush() {
                spool.fail(error);
            }
            if let Some(failure) = spool.failure.clone() {
                return Ok(Box::new(io::Cursor::new(format!(
                    "{failure}{}",
                    inner.tail_text()
                ))));
            }
            return Ok(Box::new(SpoolReader {
                inner: self.inner.clone(),
                position: 0,
                end: spool.bytes,
            }));
        }
        Ok(Box::new(io::Cursor::new(inner.tail_text())))
    }

    /// A frontend can deliver this warning once in addition to the log notice.
    pub fn take_warning(&self) -> Option<String> {
        let mut inner = self.lock();
        let spool = inner.spool.as_mut()?;
        if std::mem::take(&mut spool.warning_pending) {
            spool.failure.clone()
        } else {
            None
        }
    }

    pub fn set_warning_sink(&self, sink: tokio::sync::mpsc::UnboundedSender<Vec<u8>>) {
        *self.warning_sink.lock().unwrap_or_else(|p| p.into_inner()) = Some(sink);
        self.emit_warning();
    }

    fn emit_warning(&self) {
        if let Some(warning) = self.take_warning() {
            if let Some(sink) = self
                .warning_sink
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
            {
                let _ = sink.send(warning.into_bytes());
            }
        }
    }

    /// Detect buffered write errors before reporting the child as finished.
    pub fn finish_capture(&self) {
        let mut inner = self.lock();
        if let Some(spool) = &mut inner.spool {
            if spool.failure.is_none() {
                if let Err(error) = spool.file.flush() {
                    spool.fail(error);
                }
            }
        }
        drop(inner);
        self.emit_warning();
    }

    /// Release the warning channel along with the ordinary output senders so
    /// a frontend can finish draining even while a failure reporter owns us.
    pub fn close_capture(&self) {
        self.finish_capture();
        self.warning_sink
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
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
        let text = self
            .snapshot_text()
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        strip_ansi_escapes::strip_str(text)
    }

    /// Append a raw byte chunk from the container, splitting on `\n`. A chunk
    /// that ends mid-line is retained and completed by a later chunk.
    pub fn push_bytes(&self, bytes: &[u8]) {
        let mut inner = self.lock();
        if let Some(spool) = &mut inner.spool {
            if spool.failure.is_none() {
                if spool.bytes.saturating_add(bytes.len() as u64) > MAX_TRANSCRIPT_SPOOL_BYTES {
                    spool.fail("256 MiB spool limit exceeded");
                } else if let Err(error) = spool.file.write_all(bytes) {
                    spool.fail(error);
                } else {
                    spool.bytes += bytes.len() as u64;
                }
            }
        }
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
        drop(inner);
        self.emit_warning();
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
        let inner = self.lock();
        if let Some(spool) = &inner.spool {
            if let Some(failure) = &spool.failure {
                return format!("{failure}{}", inner.tail_text());
            }
            if spool.bytes <= MAX_TRANSCRIPT_STRING_BYTES {
                let end = spool.bytes;
                drop(inner);
                let mut text = String::new();
                let mut reader = SpoolReader {
                    inner: self.inner.clone(),
                    position: 0,
                    end,
                };
                return match reader.read_to_string(&mut text) {
                    Ok(_) => text,
                    Err(error) => format!(
                        "[awman: transcript read failed: {error}]\n{}",
                        self.lock().tail_text()
                    ),
                };
            }
            return format!(
                "[awman: full transcript exceeds 1 MiB; the last 100 lines follow]\n{}",
                inner.tail_text()
            );
        }
        inner.tail_text()
    }

    /// Lock the inner buffer, recovering from a poisoned mutex — a reader
    /// thread panicking must not permanently disable output capture.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod spool_contracts {
    use super::*;
    use awman_test_support::TestSandbox;

    #[test]
    fn full_transcript_streams_beyond_string_limit_with_bounded_tail_and_no_named_file() {
        let s = TestSandbox::new();
        let tail = OutputTail::spooled(&s.awman_home()).unwrap();
        let chunk = vec![b'x'; 4095]
            .into_iter()
            .chain([b'\n'])
            .collect::<Vec<_>>();
        for _ in 0..512 {
            tail.push_bytes(&chunk);
        }
        tail.finish_capture();
        assert_eq!(tail.snapshot().len(), DEFAULT_OUTPUT_TAIL_LINES);
        assert!(tail
            .snapshot_text()
            .starts_with("[awman: full transcript exceeds 1 MiB"));
        let mut first = tail.transcript_reader().unwrap();
        let mut second = tail.transcript_reader().unwrap();
        let mut buf = [0; 4096];
        for _ in 0..512 {
            first.read_exact(&mut buf).unwrap();
            assert_eq!(buf.as_slice(), chunk);
            second.read_exact(&mut buf).unwrap();
            assert_eq!(buf.as_slice(), chunk);
        }
        assert_eq!(first.read(&mut buf).unwrap(), 0);
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_dir(s.awman_home().join("spool/transcripts"))
                .unwrap()
                .count(),
            0
        );
        drop(tail); // Readers own the descriptor until their own drop.
        assert_eq!(second.read(&mut buf).unwrap(), 0);
        drop(first);
        drop(second);
        assert_eq!(
            std::fs::read_dir(s.awman_home().join("spool/transcripts"))
                .unwrap()
                .count(),
            0
        );
        let long_line = OutputTail::spooled(&s.awman_home()).unwrap();
        let chunk = vec![b'x'; MAX_LINE_BYTES];
        for _ in 0..32 {
            long_line.push_bytes(&chunk);
        }
        assert!(long_line.snapshot_text().len() < MAX_TRANSCRIPT_STRING_BYTES as usize);
        assert_eq!(
            io::copy(&mut long_line.transcript_reader().unwrap(), &mut io::sink()).unwrap(),
            2 * 1024 * 1024
        );
    }

    #[test]
    fn write_failure_and_cap_report_incomplete_output_and_warn_once() {
        let s = TestSandbox::new();
        for cap in [false, true] {
            let tail = OutputTail::spooled(&s.awman_home()).unwrap();
            let (sink, mut warnings) = tokio::sync::mpsc::unbounded_channel();
            tail.set_warning_sink(sink);
            tail.push_bytes(b"first\n");
            tail.finish_capture();
            {
                let mut inner = tail.lock();
                let spool = inner.spool.as_mut().unwrap();
                if cap {
                    spool.bytes = MAX_TRANSCRIPT_SPOOL_BYTES;
                } else {
                    let path = s.root().join("read-only-spool");
                    std::fs::write(&path, b"").unwrap();
                    spool.file = BufWriter::with_capacity(1, std::fs::File::open(path).unwrap());
                }
            }
            tail.push_bytes(b"last\n");
            tail.finish_capture();
            let warning = String::from_utf8(warnings.try_recv().unwrap()).unwrap();
            assert!(warning.contains("transcript incomplete after"));
            assert!(warnings.try_recv().is_err());
            let mut text = String::new();
            tail.transcript_reader()
                .unwrap()
                .read_to_string(&mut text)
                .unwrap();
            assert!(text.starts_with("[awman: transcript incomplete after"));
            assert!(text.ends_with("first\nlast\n"));
        }
    }

    #[test]
    fn spool_refuses_nonprivate_location_before_capture() {
        let s = TestSandbox::new();
        std::fs::create_dir_all(s.awman_home().join("spool")).unwrap();
        std::fs::write(s.awman_home().join("spool/transcripts"), b"preexisting").unwrap();
        assert!(OutputTail::spooled(&s.awman_home()).is_err());
    }
}
