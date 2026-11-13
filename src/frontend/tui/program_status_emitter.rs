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
