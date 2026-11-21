//! CPU usage of a session VM, derived from the runtime's cumulative vCPU time.
//!
//! The runtime's metrics slot carries a ready-made `cpu_percent`, but it is
//! not a reading awman can rely on. Each sampler window is timed around a VMM
//! snapshot that also scans guest RAM for host residency, and a window whose
//! snapshot took longer than a twentieth of the interval is discarded as
//! skewed. With a few GiB of guest memory (always, on macOS) the scan alone
//! exceeds that, every window is discarded, and `cpu_percent` stays at its
//! zero default for the life of the VM.
//!
//! The cumulative `vcpu_time_ns` in the same slot is published on every
//! sample regardless, stamped with the sample's own wall-clock time. So
//! usage is measured here instead: vCPU time gained between two published
//! samples over the wall time between them, docker's per-core scale (100% is
//! one busy vCPU).
use std::time::Duration;

use chrono::{DateTime, Utc};

/// One cumulative vCPU-time reading from a published metrics sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct CpuReading {
    pub vcpu_time_ns: u64,
    pub sampled_at: DateTime<Utc>,
    pub uptime: Duration,
}

/// The last reading kept for a sandbox and the usage measured when it
/// arrived.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct CpuState {
    pub reading: CpuReading,
    pub percent: f64,
}

/// What a fresh reading says about CPU usage, given the state kept from the
/// previous stats call for the same sandbox.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum CpuStep {
    /// Usage is known; keep this state for the next call.
    Known(CpuState),
    /// There is no earlier sample to measure against (first call, or the VM
    /// restarted and its counters began again): the caller waits for the
    /// runtime's next sample and measures between the two.
    AwaitNewerSample,
}

/// Decide what `reading` gives, given the state `kept` from the last call.
pub(super) fn step(kept: Option<&CpuState>, reading: CpuReading) -> CpuStep {
    let Some(kept) = kept else {
        return CpuStep::AwaitNewerSample;
    };
    // Polled again before the runtime published a new sample: nothing has
    // been measured since, so the last figure still stands.
    if kept.reading.sampled_at == reading.sampled_at {
        return CpuStep::Known(*kept);
    }
    match percent_between(&kept.reading, &reading) {
        Some(percent) => CpuStep::Known(CpuState { reading, percent }),
        None => CpuStep::AwaitNewerSample,
    }
}

/// vCPU usage between two readings of the same run, as a percentage of one
/// vCPU. `None` when the readings cannot be compared: the later one is not
/// later, or the counters went backwards because the VM was restarted.
pub(super) fn percent_between(earlier: &CpuReading, later: &CpuReading) -> Option<f64> {
    if later.uptime < earlier.uptime || later.vcpu_time_ns < earlier.vcpu_time_ns {
        return None;
    }
    let wall_ns = (later.sampled_at - earlier.sampled_at).num_nanoseconds()?;
    if wall_ns <= 0 {
        return None;
    }
    let busy_ns = later.vcpu_time_ns - earlier.vcpu_time_ns;
    Some(busy_ns as f64 / wall_ns as f64 * 100.0)
}
