//! Bounded retry, deadline and cancellation for image acquisition.
//!
//! Acquisition is idempotent: every attempt resolves the *same* source and
//! endpoint, stages into a fresh private directory, and either commits a
//! fully validated archive or leaves nothing. That makes retrying safe, and
//! this module makes it bounded:
//!
//! * at most [`RetryPolicy::max_attempts`] attempts, within one overall
//!   [`RetryPolicy::deadline`], with exponential backoff between them;
//! * only *transient transport* failures are retried ([`is_retryable`]):
//!   connection refused/reset, a disconnect or truncation mid-transfer, a
//!   5xx from the daemon or registry. Authentication, certificate,
//!   platform, digest, format, disk-space and configuration failures are
//!   final on the first occurrence — retrying them cannot succeed and would
//!   only hammer a credential or hide a real mismatch;
//! * a [`CancelToken`] stops an acquisition between attempts and inside a
//!   transfer loop; the staged bytes are discarded with the staging
//!   directory.
//!
//! Nothing here ever changes the source kind, the endpoint, the reference
//! or the credentials between attempts, and nothing runs a credential
//! helper: a retry is the same request again.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::engine::error::EngineError;

/// How many times, how long, and how spaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts, including the first. `1` disables retries.
    pub max_attempts: u32,
    /// Overall wall-clock budget for all attempts, sleeps included. It also
    /// bounds every individual request (no transfer may outlive it).
    pub deadline: Duration,
    /// Sleep before the second attempt; doubles each time up to
    /// `max_backoff`.
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl RetryPolicy {
    /// Three attempts within one hour, 1 s → 2 s → 4 s … capped at 30 s.
    pub const DEFAULT: Self = Self {
        max_attempts: 3,
        deadline: Duration::from_secs(60 * 60),
        initial_backoff: Duration::from_secs(1),
        max_backoff: Duration::from_secs(30),
    };

    /// A single attempt, no waiting.
    pub const NONE: Self = Self {
        max_attempts: 1,
        deadline: Duration::from_secs(60 * 60),
        initial_backoff: Duration::ZERO,
        max_backoff: Duration::ZERO,
    };

    /// The sleep before attempt number `next_attempt` (2-based: the wait
    /// before the second attempt is `backoff(2)`).
    pub fn backoff(&self, next_attempt: u32) -> Duration {
        if next_attempt < 2 {
            return Duration::ZERO;
        }
        let doublings = next_attempt.saturating_sub(2).min(16);
        self.initial_backoff
            .checked_mul(1u32 << doublings)
            .unwrap_or(self.max_backoff)
            .min(self.max_backoff)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A cooperative cancellation flag shared with whoever asked for the
/// acquisition. Cheap to clone; checked between attempts and inside every
/// transfer loop.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// `Err(cancelled)` once cancellation was requested.
    pub fn check(&self) -> Result<(), EngineError> {
        if self.is_cancelled() {
            Err(cancelled())
        } else {
            Ok(())
        }
    }
}

/// The message every cancellation error carries, so callers can recognise it.
pub const CANCELLED_MESSAGE: &str = "image acquisition was cancelled";

/// The error an acquisition returns when it was cancelled.
pub fn cancelled() -> EngineError {
    EngineError::Container(CANCELLED_MESSAGE.into())
}

/// Whether `err` is the cancellation error.
pub fn is_cancelled(err: &EngineError) -> bool {
    matches!(err, EngineError::Container(m) if m == CANCELLED_MESSAGE)
}

/// Whether a failed attempt may be repeated. Only transport-level failures
/// qualify; everything that reflects the request, the credentials, the
/// content or the host is final.
pub fn is_retryable(err: &EngineError) -> bool {
    match err {
        EngineError::Network(message) => !is_final_network_failure(message),
        EngineError::Auth(_)
        | EngineError::Config(_)
        | EngineError::Container(_)
        | EngineError::UnsupportedImageSource { .. }
        | EngineError::ImageSourceBlocked { .. }
        | EngineError::ImageSourceUnconfigured { .. }
        | EngineError::ImageDigestMismatch { .. }
        | EngineError::ImagePlatformMismatch { .. }
        | EngineError::ImageArchiveRejected { .. }
        | EngineError::InsufficientDiskSpace { .. } => false,
        _ => false,
    }
}

/// Network errors that describe a certificate or TLS identity problem are
/// final: the peer will present the same certificate again.
fn is_final_network_failure(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "certificate",
        "tls",
        "handshake",
        "unknownissuer",
        "invalid peer",
        "hostname",
        "client setup failed",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Deadline bookkeeping for one acquisition.
#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    started: Instant,
    budget: Duration,
}

impl Deadline {
    pub fn start(budget: Duration) -> Self {
        Self {
            started: Instant::now(),
            budget,
        }
    }

    /// Time left, `None` when the budget is exhausted.
    pub fn remaining(&self) -> Option<Duration> {
        self.budget
            .checked_sub(self.started.elapsed())
            .filter(|left| !left.is_zero())
    }

    /// A per-request timeout: the smaller of `preferred` and what is left.
    /// `None` (no timeout) becomes "what is left".
    pub fn request_timeout(&self, preferred: Option<Duration>) -> Duration {
        let left = self.remaining().unwrap_or(Duration::from_millis(1));
        match preferred {
            Some(p) => p.min(left),
            None => left,
        }
    }

    pub fn expired(&self) -> bool {
        self.remaining().is_none()
    }

    pub fn check(&self) -> Result<(), EngineError> {
        if self.expired() {
            Err(EngineError::Network(
                "image acquisition exceeded its deadline".into(),
            ))
        } else {
            Ok(())
        }
    }
}

/// Cooperative budget shared by local copying, validation and publication.
/// Filesystem syscalls cannot be preempted; check before and after each read.
#[derive(Debug, Clone, Default)]
pub(crate) struct OperationControl {
    cancel: CancelToken,
    deadline: Option<Deadline>,
}

impl OperationControl {
    pub(crate) fn new(cancel: CancelToken, deadline: Deadline) -> Self {
        Self {
            cancel,
            deadline: Some(deadline),
        }
    }

    pub(crate) fn check(&self) -> Result<(), EngineError> {
        self.cancel.check()?;
        if let Some(deadline) = self.deadline {
            deadline.check()?;
        }
        Ok(())
    }

    pub(crate) fn reader<R: std::io::Read>(&self, inner: R) -> ControlledReader<R> {
        ControlledReader {
            inner,
            control: self.clone(),
        }
    }
}

pub(crate) struct ControlledReader<R> {
    inner: R,
    control: OperationControl,
}

impl<R: std::io::Read> std::io::Read for ControlledReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.control.check().map_err(std::io::Error::other)?;
        // Bound even a caller that supplies a very large buffer.
        let len = buf.len().min(64 * 1024);
        let count = self.inner.read(&mut buf[..len])?;
        self.control.check().map_err(std::io::Error::other)?;
        Ok(count)
    }
}

/// Injectable sleeper so retry tests do not wait for real backoff.
pub type Sleeper<'a> = dyn Fn(Duration) + Send + Sync + 'a;

/// Run `attempt` until it succeeds, fails finally, runs out of attempts or
/// time, or is cancelled. `attempt` receives the 1-based attempt number.
/// The last error is returned, annotated with the attempt count when more
/// than one was made.
pub fn run_bounded<T>(
    policy: RetryPolicy,
    deadline: Deadline,
    cancel: &CancelToken,
    sleep: &Sleeper<'_>,
    mut attempt: impl FnMut(u32) -> Result<T, EngineError>,
) -> Result<T, EngineError> {
    let max = policy.max_attempts.max(1);
    let mut number = 1;
    loop {
        cancel.check()?;
        if deadline.expired() {
            return Err(EngineError::Network(format!(
                "image acquisition exceeded its {}s deadline before attempt {number}",
                policy.deadline.as_secs()
            )));
        }
        match attempt(number) {
            Ok(value) => {
                cancel.check()?;
                deadline.check()?;
                return Ok(value);
            }
            Err(err) if is_cancelled(&err) => return Err(err),
            Err(err) => {
                // Archive and HTTP readers may wrap an interrupted operation
                // in their own error type; cancellation still wins.
                cancel.check()?;
                deadline.check()?;
                let more = number < max;
                let wait = policy.backoff(number + 1);
                let fits = deadline.remaining().is_some_and(|left| left > wait);
                if !(more && is_retryable(&err) && fits) {
                    return Err(annotate(err, number, max));
                }
                let mut remaining = wait;
                while !remaining.is_zero() {
                    cancel.check()?;
                    deadline.check()?;
                    let slice = remaining.min(Duration::from_millis(20));
                    sleep(slice);
                    remaining = remaining.saturating_sub(slice);
                }
                number += 1;
            }
        }
    }
}

fn annotate(err: EngineError, attempts: u32, max: u32) -> EngineError {
    if attempts <= 1 {
        return err;
    }
    match err {
        EngineError::Network(message) => EngineError::Network(format!(
            "{message} (after {attempts} of {max} attempts against the same source)"
        )),
        other => other,
    }
}

/// The full cause chain of a transport error (`error sending request:
/// client error (Connect): invalid peer certificate: UnknownIssuer`), so
/// the user sees why and [`is_retryable`] can tell a certificate failure
/// from a dropped connection. Request URLs are never part of the chain.
pub fn error_chain(err: &dyn std::error::Error) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = err.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if parts.last().is_none_or(|last| !last.contains(&text)) {
            parts.push(text);
        }
        source = cause.source();
    }
    parts.join(": ")
}

/// Replace every occurrence of a known secret value in `text`, so a
/// transport library's own error text can never carry a password, token or
/// proxy credential into a log, a summary row or a cache record.
pub fn redact(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if secret.len() >= 4 && out.contains(secret) {
            out = out.replace(secret, "[redacted]");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn late_success_and_cancelled_success_are_rejected() {
        let budget = Duration::from_millis(10);
        let err = run_bounded(
            RetryPolicy::NONE,
            Deadline::start(budget),
            &CancelToken::new(),
            &*no_sleep(),
            |_| {
                std::thread::sleep(Duration::from_millis(30));
                Ok(37)
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("deadline"), "{err}");
        let token = CancelToken::new();
        let err = run_bounded(
            RetryPolicy::NONE,
            Deadline::start(Duration::from_secs(1)),
            &token,
            &*no_sleep(),
            |_| {
                token.cancel();
                Ok(37)
            },
        )
        .unwrap_err();
        assert!(is_cancelled(&err));
    }

    #[test]
    fn controlled_reader_rejects_bytes_returned_after_expiry() {
        struct Slow;
        impl std::io::Read for Slow {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_millis(30));
                bytes[0] = 42;
                Ok(1)
            }
        }
        let control = OperationControl::new(
            CancelToken::new(),
            Deadline::start(Duration::from_millis(10)),
        );
        let err = std::io::copy(&mut control.reader(Slow), &mut std::io::sink()).unwrap_err();
        assert!(err.to_string().contains("deadline"), "{err}");
    }

    fn no_sleep() -> Box<Sleeper<'static>> {
        Box::new(|_| {})
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let p = RetryPolicy {
            max_attempts: 10,
            deadline: Duration::from_secs(100),
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(5),
        };
        assert_eq!(p.backoff(1), Duration::ZERO);
        assert_eq!(p.backoff(2), Duration::from_secs(1));
        assert_eq!(p.backoff(3), Duration::from_secs(2));
        assert_eq!(p.backoff(4), Duration::from_secs(4));
        assert_eq!(p.backoff(5), Duration::from_secs(5));
        assert_eq!(p.backoff(40), Duration::from_secs(5));
    }

    #[test]
    fn only_transport_failures_are_retryable() {
        assert!(is_retryable(&EngineError::Network(
            "Docker Engine at unix:///x is not reachable: connection refused".into()
        )));
        assert!(is_retryable(&EngineError::Network(
            "export of 'a' was truncated: 10 of 20 bytes".into()
        )));
        assert!(!is_retryable(&EngineError::Network(
            "registry r: invalid peer certificate: UnknownIssuer".into()
        )));
        assert!(!is_retryable(&EngineError::Network(
            "HTTP client setup failed: bad".into()
        )));
        assert!(!is_retryable(&EngineError::Auth("denied".into())));
        assert!(!is_retryable(&EngineError::Config("bad".into())));
        assert!(!is_retryable(&EngineError::Container("not found".into())));
        assert!(!is_retryable(&EngineError::ImageDigestMismatch {
            reference: "r".into(),
            expected: "a".into(),
            actual: "b".into()
        }));
        assert!(!is_retryable(&EngineError::ImagePlatformMismatch {
            reference: "r".into(),
            wanted: "a".into(),
            found: "b".into()
        }));
        assert!(!is_retryable(&EngineError::ImageArchiveRejected {
            path: "/x".into(),
            reason: "zstd".into()
        }));
        assert!(!is_retryable(&EngineError::InsufficientDiskSpace {
            path: "/x".into(),
            needed: 1,
            available: 0
        }));
        assert!(!is_retryable(&cancelled()));
    }

    #[test]
    fn run_bounded_retries_transport_failures_up_to_the_cap() {
        let calls = Mutex::new(Vec::new());
        let sleeps = Mutex::new(Vec::new());
        let sleep = |d: Duration| sleeps.lock().unwrap().push(d);
        let policy = RetryPolicy {
            max_attempts: 3,
            ..RetryPolicy::DEFAULT
        };
        let err = run_bounded(
            policy,
            Deadline::start(Duration::from_secs(60)),
            &CancelToken::new(),
            &sleep,
            |n| {
                calls.lock().unwrap().push(n);
                Err::<(), _>(EngineError::Network("connection reset".into()))
            },
        )
        .unwrap_err();
        assert_eq!(*calls.lock().unwrap(), vec![1, 2, 3]);
        let sleeps = sleeps.lock().unwrap();
        assert_eq!(sleeps.iter().sum::<Duration>(), Duration::from_secs(3));
        assert!(sleeps
            .iter()
            .all(|slice| *slice <= Duration::from_millis(20)));
        assert!(err.to_string().contains("after 3 of 3 attempts"), "{err}");
    }

    #[test]
    fn run_bounded_stops_at_the_first_final_failure() {
        let calls = Mutex::new(0);
        let err = run_bounded(
            RetryPolicy::DEFAULT,
            Deadline::start(Duration::from_secs(60)),
            &CancelToken::new(),
            &*no_sleep(),
            |_| {
                *calls.lock().unwrap() += 1;
                Err::<(), _>(EngineError::Auth("denied with credentials".into()))
            },
        )
        .unwrap_err();
        assert_eq!(*calls.lock().unwrap(), 1);
        assert!(matches!(err, EngineError::Auth(m) if m == "denied with credentials"));
    }

    #[test]
    fn run_bounded_succeeds_on_a_later_attempt() {
        let calls = Mutex::new(0);
        let value = run_bounded(
            RetryPolicy::DEFAULT,
            Deadline::start(Duration::from_secs(60)),
            &CancelToken::new(),
            &*no_sleep(),
            |n| {
                *calls.lock().unwrap() += 1;
                if n < 2 {
                    Err(EngineError::Network("disconnected".into()))
                } else {
                    Ok(n)
                }
            },
        )
        .unwrap();
        assert_eq!(value, 2);
    }

    #[test]
    fn cancellation_wins_before_and_between_attempts() {
        let token = CancelToken::new();
        token.cancel();
        let err = run_bounded(
            RetryPolicy::DEFAULT,
            Deadline::start(Duration::from_secs(60)),
            &token,
            &*no_sleep(),
            |_| Ok::<(), _>(()),
        )
        .unwrap_err();
        assert!(is_cancelled(&err));

        let token = CancelToken::new();
        let calls = Mutex::new(0);
        let err = run_bounded(
            RetryPolicy::DEFAULT,
            Deadline::start(Duration::from_secs(60)),
            &token,
            &|_| token.cancel(),
            |_| {
                *calls.lock().unwrap() += 1;
                Err::<(), _>(EngineError::Network("reset".into()))
            },
        )
        .unwrap_err();
        assert!(is_cancelled(&err));
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn cancellation_interrupts_a_long_production_backoff() {
        let token = CancelToken::new();
        let signal = token.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            signal.cancel();
        });
        let started = Instant::now();
        let error = run_bounded(
            RetryPolicy {
                initial_backoff: Duration::from_secs(3),
                ..RetryPolicy::DEFAULT
            },
            Deadline::start(Duration::from_secs(10)),
            &token,
            &std::thread::sleep,
            |_| Err::<(), _>(EngineError::Network("disconnected".into())),
        )
        .unwrap_err();
        assert!(is_cancelled(&error));
        assert!(started.elapsed() < Duration::from_millis(500));
        canceller.join().unwrap();
    }

    #[test]
    fn an_exhausted_deadline_stops_retrying() {
        let calls = Mutex::new(0);
        let err = run_bounded(
            RetryPolicy {
                max_attempts: 5,
                initial_backoff: Duration::from_secs(10),
                ..RetryPolicy::DEFAULT
            },
            Deadline::start(Duration::from_secs(5)),
            &CancelToken::new(),
            &*no_sleep(),
            |_| {
                *calls.lock().unwrap() += 1;
                Err::<(), _>(EngineError::Network("reset".into()))
            },
        )
        .unwrap_err();
        assert_eq!(*calls.lock().unwrap(), 1, "a 10s backoff does not fit 5s");
        assert!(matches!(err, EngineError::Network(_)));
        let expired = Deadline::start(Duration::ZERO);
        assert!(expired.expired());
        assert_eq!(expired.request_timeout(None), Duration::from_millis(1));
    }

    #[test]
    fn redaction_replaces_known_secrets_only() {
        let text = "proxy http://user:SENTINEL-pw-9f3@proxy:3128 refused; token SENTINEL-tok";
        let out = redact(text, &["SENTINEL-pw-9f3", "SENTINEL-tok", "ab"]);
        assert_eq!(
            out,
            "proxy http://user:[redacted]@proxy:3128 refused; token [redacted]"
        );
        assert_eq!(redact("nothing", &[]), "nothing");
    }
}
