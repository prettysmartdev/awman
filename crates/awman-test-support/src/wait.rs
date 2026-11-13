//! Event-based waiting with a deadline and diagnostics.
//!
//! `wait_for` is the fallback when no event exists: prefer [`crate::Ready`],
//! channels and [`crate::OwnedChild::wait_for_stdout`]. This module contains
//! the only `thread::sleep` test code may reach (WI 3.5 "Time").

use std::future::Future;
use std::time::{Duration, Instant};

const FIRST_DELAY: Duration = Duration::from_millis(5);
const MAX_DELAY: Duration = Duration::from_millis(100);

/// Poll `probe` (5 ms backing off to 100 ms) until it returns `Some`.
/// On expiry, panic with `what` and the elapsed time.
///
/// ```
/// use awman_test_support::wait_for;
/// use std::time::Duration;
/// let mut n = 0;
/// let value = wait_for("the counter to reach 3", Duration::from_secs(5), || {
///     n += 1;
///     (n == 3).then_some(n)
/// });
/// assert_eq!(value, 3);
/// ```
pub fn wait_for<T>(what: &str, limit: Duration, probe: impl FnMut() -> Option<T>) -> T {
    wait_for_with(what, limit, probe, String::new)
}

/// Like [`wait_for`], and on expiry also prints `diagnostics()` (for example
/// a child's output so far or a directory listing).
pub fn wait_for_with<T>(
    what: &str,
    limit: Duration,
    probe: impl FnMut() -> Option<T>,
    diagnostics: impl Fn() -> String,
) -> T {
    let started = Instant::now();
    match poll(limit, probe) {
        Some(value) => value,
        None => {
            let details = diagnostics();
            panic!(
                "timed out after {:.1?} (limit {:?}) waiting for {what}{}{}",
                started.elapsed(),
                limit,
                if details.is_empty() {
                    ""
                } else {
                    "\n--- diagnostics ---\n"
                },
                details
            );
        }
    }
}

/// Async variant for tokio tests: polls `probe()` with the same backoff using
/// `tokio::time::sleep` (real time; never use it under paused time).
pub async fn wait_for_async<T, F, Fut>(what: &str, limit: Duration, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let started = Instant::now();
    let mut delay = FIRST_DELAY;
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        let elapsed = started.elapsed();
        if elapsed >= limit {
            panic!("timed out after {elapsed:.1?} (limit {limit:?}) waiting for {what}");
        }
        // The one sanctioned async real-time delay: polling backoff.
        #[allow(clippy::disallowed_methods)]
        tokio::time::sleep(delay.min(limit - elapsed)).await;
        delay = (delay * 2).min(MAX_DELAY);
    }
}

/// Poll without panicking: `None` when `limit` passed. Used by the supervisor.
pub(crate) fn poll<T>(limit: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let started = Instant::now();
    let mut delay = FIRST_DELAY;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        let elapsed = started.elapsed();
        if elapsed >= limit {
            return None;
        }
        pause(delay.min(limit - elapsed));
        delay = (delay * 2).min(MAX_DELAY);
    }
}

/// The one sanctioned blocking real-time delay.
#[allow(clippy::disallowed_methods)]
pub(crate) fn pause(delay: Duration) {
    std::thread::sleep(delay);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_the_first_some() {
        let mut calls = 0;
        let v = wait_for("x", Duration::from_secs(1), || {
            calls += 1;
            (calls >= 2).then_some("done")
        });
        assert_eq!(v, "done");
        assert_eq!(calls, 2);
    }

    #[test]
    fn expiry_panics_with_what_and_diagnostics() {
        let err = std::panic::catch_unwind(|| {
            wait_for_with(
                "the moon",
                Duration::from_millis(20),
                || None::<()>,
                || "lunar state: absent".to_string(),
            )
        })
        .unwrap_err();
        let message = crate::panic_text(&err);
        assert!(message.contains("waiting for the moon"), "{message}");
        assert!(message.contains("lunar state: absent"), "{message}");
    }

    #[test]
    fn poll_gives_up_without_panicking() {
        assert_eq!(poll(Duration::from_millis(10), || None::<u8>), None);
    }

    #[test]
    fn async_variant_waits_and_expires() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let mut n = 0;
        let v = rt.block_on(wait_for_async("n", Duration::from_secs(1), || {
            n += 1;
            let ready = n >= 3;
            async move { ready.then_some(7) }
        }));
        assert_eq!(v, 7);
        let rt2 = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let err = std::panic::catch_unwind(move || {
            rt2.block_on(wait_for_async(
                "never",
                Duration::from_millis(10),
                || async { None::<()> },
            ))
        })
        .unwrap_err();
        assert!(crate::panic_text(&err).contains("never"));
    }
}
