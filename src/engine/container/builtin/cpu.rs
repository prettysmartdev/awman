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

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(vcpu_ms: u64, at_ms: i64) -> CpuReading {
        CpuReading {
            vcpu_time_ns: vcpu_ms * 1_000_000,
            sampled_at: DateTime::from_timestamp_millis(1_760_000_000_000 + at_ms).unwrap(),
            uptime: Duration::from_millis(at_ms as u64),
        }
    }

    #[test]
    fn usage_is_vcpu_time_gained_over_wall_time_on_a_per_vcpu_scale() {
        // Half a vCPU busy for two seconds.
        let half = percent_between(&reading(1_000, 1_000), &reading(2_000, 3_000)).unwrap();
        assert!((half - 50.0).abs() < 1e-9, "{half}");
        // Two vCPUs flat out read 200%, as docker reports two busy cores.
        let two = percent_between(&reading(0, 1_000), &reading(4_000, 3_000)).unwrap();
        assert!((two - 200.0).abs() < 1e-9, "{two}");
        // An idle guest reads zero, not "unknown".
        assert_eq!(
            percent_between(&reading(500, 1_000), &reading(500, 2_000)),
            Some(0.0)
        );
    }

    #[test]
    fn readings_that_cannot_be_compared_give_no_usage() {
        // Same sample, or out of order.
        assert_eq!(
            percent_between(&reading(10, 1_000), &reading(10, 1_000)),
            None
        );
        assert_eq!(
            percent_between(&reading(10, 2_000), &reading(20, 1_000)),
            None
        );
        // A restarted VM: uptime and the vCPU counter start over.
        let before = reading(9_000, 60_000);
        let mut after = reading(100, 1_000);
        after.sampled_at = before.sampled_at + chrono::Duration::seconds(3);
        assert_eq!(percent_between(&before, &after), None);
    }

    #[test]
    fn a_first_reading_waits_for_a_newer_sample() {
        assert_eq!(step(None, reading(10, 1_000)), CpuStep::AwaitNewerSample);
    }

    #[test]
    fn a_newer_reading_is_measured_against_the_kept_one() {
        let kept = CpuState {
            reading: reading(1_000, 1_000),
            percent: 5.0,
        };
        let next = reading(1_250, 2_000);
        match step(Some(&kept), next) {
            CpuStep::Known(state) => {
                assert_eq!(state.reading, next, "the new reading is kept for next time");
                assert!((state.percent - 25.0).abs() < 1e-9, "{}", state.percent);
            }
            other => panic!("expected a measurement, got {other:?}"),
        }
    }

    #[test]
    fn polling_again_before_a_new_sample_keeps_the_last_figure() {
        let kept = CpuState {
            reading: reading(1_000, 1_000),
            percent: 37.5,
        };
        assert_eq!(step(Some(&kept), kept.reading), CpuStep::Known(kept));
    }

    #[test]
    fn a_restart_since_the_kept_reading_starts_measuring_afresh() {
        let kept = CpuState {
            reading: reading(9_000, 60_000),
            percent: 80.0,
        };
        let mut restarted = reading(100, 1_000);
        restarted.sampled_at = kept.reading.sampled_at + chrono::Duration::seconds(3);
        assert_eq!(step(Some(&kept), restarted), CpuStep::AwaitNewerSample);
    }
}
