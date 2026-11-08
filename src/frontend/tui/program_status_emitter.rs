//! OSC 7501 emission from the TUI to its own terminal (WI 0141 hygiene §2).
//!
//! After each tick the event loop builds an `EmitInput` from its tabs, asks
//! `command::program_status_projection` for the records, and this emitter
//! writes them straight to stdout, outside the ratatui frame buffer:
//! coalesced (at most four writes per second, only on change), gated by the
//! startup feature probe (`frontend::osc7501::ProbeDetector`), and cleared
//! with `state=clear` on normal exit and in the panic hook so the terminal
//! never keeps a stale "working". Off under `tui.programStatus.emit=false`,
//! `TERM=dumb`, or when stdout is not a TTY.

use std::io::Write;
use std::time::{Duration, Instant};

use crate::command::program_status_projection::ProgramStatusProjection;
use crate::data::program_status::ProgramStatusReport;
use crate::frontend::osc7501::{
    encode_report, encode_reports, ProbeDetector, ProbeOutcome, DA1_PROBE, QUERY,
};

/// Coalescing: never more than this many writes per second.
pub const MAX_WRITES_PER_SECOND: u32 = 4;
/// The shortest gap between two writes.
pub const MIN_WRITE_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub struct ProgramStatusEmitter {
    /// `tui.programStatus.emit` and the TTY/TERM checks, resolved at start.
    enabled: bool,
    probe: ProbeDetector,
    /// The last bytes written, to skip unchanged ticks.
    last_written: Vec<u8>,
    last_write_at: Option<Instant>,
}

impl ProgramStatusEmitter {
    pub(super) fn disable(&mut self) {
        self.enabled = false;
    }
    /// `enabled` is `tui.programStatus.emit && stdout is a TTY && TERM !=
    /// "dumb"`, decided by the caller (Layer 2 resolves the config).
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            probe: ProbeDetector::new(),
            last_written: Vec::new(),
            last_write_at: None,
        }
    }

    /// Send the feature probe (`QUERY` then `DA1_PROBE`). No-op when
    /// disabled.
    pub fn start_probe(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        if self.enabled {
            let mut bytes = QUERY.to_vec();
            bytes.extend_from_slice(DA1_PROBE);
            out.write_all(&bytes)?;
            out.flush()?;
        }
        Ok(())
    }

    /// End the startup probe window, releasing an ordinary Escape key.
    pub fn expire_probe(&mut self) -> Vec<u8> {
        self.probe.expire()
    }

    /// Pass terminal input through the probe detector; the returned bytes
    /// are what the key handler sees.
    pub fn observe_input(&mut self, input: &[u8]) -> Vec<u8> {
        if !self.enabled {
            return input.to_vec();
        }
        self.probe.feed(input)
    }

    pub(super) fn observe_stream_input(&mut self, input: &[u8]) -> Vec<u8> {
        if self.enabled {
            self.probe.feed_stream(input)
        } else {
            input.to_vec()
        }
    }

    pub(super) fn expire_stream_probe(&mut self) -> Vec<u8> {
        self.probe.expire_stream()
    }

    pub(super) fn release_key_prefix(&mut self) -> Vec<u8> {
        self.probe.release_key_prefix()
    }
    pub(super) fn is_control_prefix(&self) -> bool {
        self.probe.is_control_prefix()
    }

    /// Whether emission is on: enabled and the probe said `Supported`.
    pub fn active(&self) -> bool {
        self.enabled && self.probe.outcome() == ProbeOutcome::Supported
    }

    /// Write the records if they changed and the rate limit allows.
    pub fn tick(
        &mut self,
        reports: &[ProgramStatusReport],
        now: Instant,
        out: &mut impl Write,
    ) -> std::io::Result<()> {
        if !self.active()
            || self
                .last_write_at
                .is_some_and(|last| now.saturating_duration_since(last) < MIN_WRITE_INTERVAL)
        {
            return Ok(());
        }
        let bytes = encode_reports(reports);
        if bytes == self.last_written {
            return Ok(());
        }
        // Clear removed children, then replace the set in one output batch.
        let mut update = encode_report(&ProgramStatusProjection.clear());
        update.extend_from_slice(&bytes);
        out.write_all(&update)?;
        out.flush()?;
        self.last_written = bytes;
        self.last_write_at = Some(now);
        Ok(())
    }

    /// `state=clear` (no `id`), for exit and the panic hook. Always written
    /// when `active()`, regardless of the rate limit.
    pub fn clear(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        clear_terminal_status(self.active(), out)?;
        self.last_written.clear();
        self.last_write_at = None;
        Ok(())
    }
}

/// Shared by normal teardown and the panic hook. Requires no emitter lock,
/// so a panic during emission cannot deadlock while clearing the terminal.
pub(super) fn clear_terminal_status(active: bool, out: &mut impl Write) -> std::io::Result<()> {
    if active {
        out.write_all(&encode_report(&ProgramStatusProjection.clear()))?;
        out.flush()?;
    }
    Ok(())
}

pub(super) fn emission_enabled(config: bool, is_tty: bool, term: Option<&str>) -> bool {
    config && is_tty && term != Some("dumb")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::program_status_projection::{AwmanState, EmitInput, EmitSlot, EmitTab};
    use crate::frontend::osc7501::CLEAR_ALL;

    #[derive(Default)]
    struct RecordingWriter {
        writes: Vec<Vec<u8>>,
        flushes: usize,
    }
    impl Write for RecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes.push(bytes.to_vec());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }
    use std::io;

    fn reports(state: AwmanState) -> Vec<ProgramStatusReport> {
        ProgramStatusProjection.reports(&EmitInput {
            tabs: vec![EmitTab {
                index: 1,
                label: "build".into(),
                slots: vec![EmitSlot {
                    slot: 0,
                    status: None,
                    awman: Some(state),
                }],
            }],
        })
    }

    fn supported() -> ProgramStatusEmitter {
        let mut emitter = ProgramStatusEmitter::new(true);
        assert!(emitter
            .observe_input(b"\x1b]7501;?\x1b\\\x1b[?62c")
            .is_empty());
        assert!(emitter.active());
        emitter
    }

    #[test]
    fn gating_and_startup_probe_order() {
        for (config, tty, term, expected) in [
            (true, true, Some("xterm-256color"), true),
            (true, true, None, true),
            (false, true, None, false),
            (true, false, None, false),
            (true, true, Some("dumb"), false),
        ] {
            assert_eq!(emission_enabled(config, tty, term), expected);
            let mut emitter = ProgramStatusEmitter::new(expected);
            let mut out = RecordingWriter::default();
            emitter.start_probe(&mut out).unwrap();
            let bytes: Vec<_> = out.writes.into_iter().flatten().collect();
            assert_eq!(
                bytes,
                if expected {
                    [QUERY, DA1_PROBE].concat()
                } else {
                    vec![]
                }
            );
        }
    }

    #[test]
    fn disabled_pending_da_only_and_late_reply_never_write_or_clear() {
        for (enabled, replies) in [
            (false, &b"\x1b]7501;?\x1b\\\x1b[?62c"[..]),
            (true, &b""[..]),
            (true, &b"\x1b[?62c"[..]),
            (true, &b"\x1b[?62c\x1b]7501;?\x1b\\"[..]),
        ] {
            let mut emitter = ProgramStatusEmitter::new(enabled);
            emitter.observe_input(replies);
            let mut out = RecordingWriter::default();
            emitter
                .tick(
                    &reports(AwmanState::CommandRunning),
                    Instant::now(),
                    &mut out,
                )
                .unwrap();
            emitter.clear(&mut out).unwrap();
            assert!(out.writes.is_empty());
        }
    }

    #[test]
    fn coalesces_to_latest_change_and_skips_unchanged_ticks() {
        let mut emitter = supported();
        let mut out = RecordingWriter::default();
        let start = Instant::now();
        let working = reports(AwmanState::CommandRunning);
        let done = reports(AwmanState::CommandDone);
        let failed = reports(AwmanState::CommandFailed);
        emitter.tick(&working, start, &mut out).unwrap();
        for ms in 1..250 {
            emitter
                .tick(&done, start + Duration::from_millis(ms), &mut out)
                .unwrap();
        }
        assert_eq!(out.writes.len(), 1);
        emitter
            .tick(&failed, start + MIN_WRITE_INTERVAL, &mut out)
            .unwrap();
        assert_eq!(out.writes.len(), 2);
        assert!(out.writes[1].ends_with(&encode_reports(&failed)));
        for ms in 500..1000 {
            emitter
                .tick(&failed, start + Duration::from_millis(ms), &mut out)
                .unwrap();
        }
        assert_eq!(out.writes.len(), 2);
        assert_eq!(out.flushes, 2);
    }

    #[test]
    fn at_most_four_updates_per_second() {
        let mut emitter = supported();
        let mut out = RecordingWriter::default();
        let start = Instant::now();
        for ms in 0..1000 {
            emitter
                .tick(
                    &reports(if ms % 251 == 0 {
                        AwmanState::CommandDone
                    } else {
                        AwmanState::CommandRunning
                    }),
                    start + Duration::from_millis(ms),
                    &mut out,
                )
                .unwrap();
        }
        assert!(out.writes.len() <= MAX_WRITES_PER_SECOND as usize);
    }

    #[test]
    fn clears_removed_children_in_each_changed_batch_and_on_exit() {
        let mut emitter = supported();
        let mut out = RecordingWriter::default();
        let now = Instant::now();
        emitter
            .tick(&reports(AwmanState::CommandRunning), now, &mut out)
            .unwrap();
        let empty = ProgramStatusProjection.reports(&EmitInput::default());
        emitter
            .tick(&empty, now + MIN_WRITE_INTERVAL, &mut out)
            .unwrap();
        assert_eq!(
            out.writes[1],
            [CLEAR_ALL, encode_reports(&empty).as_slice()].concat()
        );
        emitter.clear(&mut out).unwrap(); // bypass the rate limit on exit
        assert_eq!(out.writes[2], CLEAR_ALL);
    }

    #[test]
    fn a_failed_write_is_not_remembered_as_successful() {
        let mut emitter = supported();
        let now = Instant::now();
        let reports = reports(AwmanState::CommandRunning);
        let mut full = &mut [][..];
        assert!(emitter.tick(&reports, now, &mut full).is_err());
        let mut out = RecordingWriter::default();
        emitter.tick(&reports, now, &mut out).unwrap();
        assert_eq!(out.writes.len(), 1);
    }
}
