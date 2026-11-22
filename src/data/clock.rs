//! Injectable wall and monotonic clocks for data-layer expiry and timestamp
//! decisions. Tests use [`ManualClock`] and never change process-global time.

use chrono::{DateTime, Utc};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub trait Clock: Send + Sync + std::fmt::Debug + 'static {
    fn now_utc(&self) -> DateTime<Utc>;
    /// Elapsed monotonic time since this clock was created.
    fn monotonic(&self) -> Duration;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_utc(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn monotonic(&self) -> Duration {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        START.get_or_init(Instant::now).elapsed()
    }
}

#[derive(Debug, Clone)]
pub struct ManualClock(Arc<Mutex<(DateTime<Utc>, Duration)>>);

impl ManualClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self(Arc::new(Mutex::new((start, Duration::ZERO))))
    }

    /// Move both wall and monotonic time forward by the same duration.
    pub fn advance(&self, by: Duration) {
        let mut state = self.0.lock().expect("manual clock mutex poisoned");
        state.0 += chrono::Duration::from_std(by).expect("duration fits chrono");
        state.1 = state.1.saturating_add(by);
    }

    /// Change wall time without affecting monotonic time.
    pub fn set_utc(&self, now: DateTime<Utc>) {
        self.0.lock().expect("manual clock mutex poisoned").0 = now;
    }
}

impl Clock for ManualClock {
    fn now_utc(&self) -> DateTime<Utc> {
        self.0.lock().expect("manual clock mutex poisoned").0
    }

    fn monotonic(&self) -> Duration {
        self.0.lock().expect("manual clock mutex poisoned").1
    }
}

pub type SharedClock = Arc<dyn Clock>;

pub fn system_clock() -> SharedClock {
    Arc::new(SystemClock)
}

#[cfg(test)]
mod tests {
    use super::{Clock, ManualClock};
    use chrono::{TimeZone, Utc};
    use std::time::Duration;

    #[test]
    fn advancing_moves_both_clocks_and_wall_adjustment_is_independent() {
        let initial = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let clock = ManualClock::new(initial);
        clock.advance(Duration::from_secs(7));
        assert_eq!(clock.now_utc(), initial + chrono::Duration::seconds(7));
        assert_eq!(clock.monotonic(), Duration::from_secs(7));

        let jumped = initial - chrono::Duration::days(1);
        clock.set_utc(jumped);
        assert_eq!(clock.now_utc(), jumped);
        assert_eq!(clock.monotonic(), Duration::from_secs(7));
    }

    #[test]
    fn clones_share_one_clock_state() {
        let clock = ManualClock::new(Utc.timestamp_opt(0, 0).unwrap());
        let clone = clock.clone();
        clone.advance(Duration::from_millis(25));
        assert_eq!(clock.monotonic(), Duration::from_millis(25));
    }
}
