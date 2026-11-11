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
//!   5xx from the daemon or registry. HTTP 408 and 429 remain final: this
//!   fixed backoff policy does not implement server Retry-After scheduling.
//!   Authentication, certificate,
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
use std::time::Duration;

use crate::engine::error::{EngineError, ImageRetryClass};

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

/// The error an acquisition returns when it was cancelled.
pub fn cancelled() -> EngineError {
    EngineError::ImageAcquisitionCancelled
}

/// Whether `err` is the cancellation error.
pub fn is_cancelled(err: &EngineError) -> bool {
    matches!(err, EngineError::ImageAcquisitionCancelled)
}

/// Whether a failed attempt may be repeated. Only transport-level failures
/// qualify; everything that reflects the request, the credentials, the
/// content or the host is final.
pub fn is_retryable(err: &EngineError) -> bool {
    matches!(
        err,
        EngineError::ImageSourceFailure {
            retry: ImageRetryClass::Transient,
            ..
        }
    )
}

fn transient_io_kind(kind: std::io::ErrorKind) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        kind,
        ConnectionRefused
            | ConnectionReset
            | ConnectionAborted
            | NotConnected
            | BrokenPipe
            | TimedOut
            | UnexpectedEof
            | NetworkUnreachable
            | HostUnreachable
            | NetworkDown
    )
}

impl ImageRetryClass {
    pub(crate) fn failure(self, diagnostic: String) -> EngineError {
        EngineError::ImageSourceFailure {
            retry: self,
            diagnostic,
        }
    }

    pub(crate) fn status(status: reqwest::StatusCode) -> Self {
        if status.is_server_error() {
            Self::Transient
        } else {
            Self::Permanent
        }
    }

    /// Reqwest retains rustls errors in its cause chain, sometimes inside
    /// `io::Error`. Check those types before its broad connect/request flags:
    /// certificate and TLS protocol refusals are also connect errors.
    pub(crate) fn http(err: &reqwest::Error) -> Self {
        let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(err);
        let mut transport_io = false;
        while let Some(error) = cause {
            if error.is::<rustls::Error>() {
                return Self::Permanent;
            }
            if let Some(io) = error.downcast_ref::<std::io::Error>() {
                transport_io |= transient_io_kind(io.kind());
                if let Some(inner) = io.get_ref() {
                    cause = Some(inner);
                    continue;
                }
            }
            cause = error.source();
        }
        if err.is_builder() || err.is_redirect() {
            Self::Permanent
        } else if transport_io
            || err.is_timeout()
            || err.is_dns()
            // These clients read raw bytes. Reqwest also calls a truncated
            // HTTP body a decode error; JSON/schema decoding happens later
            // at the source boundary and is always permanent.
            || (!err.is_connect() && (err.is_request() || err.is_body() || err.is_decode()))
        {
            Self::Transient
        } else {
            // Unknown connection/setup refusals (including proxy auth) are
            // final. Never infer their category from a library's wording.
            Self::Permanent
        }
    }
}

/// Preserve classified HTTP body errors and authoritative control errors
/// through the synchronous `Read` adapter. HTTP classifies its disconnects
/// before this boundary; an unclassified reader error is final.
pub(crate) fn read_error(err: std::io::Error) -> EngineError {
    let diagnostic = err.to_string();
    if let Some(inner) = err.into_inner() {
        if let Ok(error) = inner.downcast::<EngineError>() {
            return *error;
        }
    }
    ImageRetryClass::Permanent.failure(diagnostic)
}

/// Add source context/redaction without losing the classification or
/// converting cancellation/deadline errors into transport failures.
pub(crate) fn map_diagnostic(err: EngineError, map: impl FnOnce(String) -> String) -> EngineError {
    match err {
        EngineError::ImageSourceFailure { retry, diagnostic } => retry.failure(map(diagnostic)),
        other => other,
    }
}

/// Keep the acquisition API's existing error hierarchy after retry decisions
/// are complete. Unclassified `Network` errors are never retry inputs.
pub(crate) fn finish(err: EngineError) -> EngineError {
    match err {
        EngineError::ImageSourceFailure { diagnostic, .. } => EngineError::Network(diagnostic),
        other => other,
    }
}

/// The monotonic clock deadlines read unless a caller injects another.
static SYSTEM_CLOCK: crate::data::clock::SystemClock = crate::data::clock::SystemClock;

/// Deadline bookkeeping for one acquisition, on an injectable monotonic
/// clock (the system clock in production, a `ManualClock` in tests).
#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    clock: &'static dyn crate::data::clock::Clock,
    started: Duration,
    budget: Duration,
}

impl Deadline {
    pub fn start(budget: Duration) -> Self {
        Self::start_on(budget, &SYSTEM_CLOCK)
    }

    /// A deadline measured on `clock` (virtual time in tests).
    pub fn start_on(budget: Duration, clock: &'static dyn crate::data::clock::Clock) -> Self {
        Self {
            clock,
            started: clock.monotonic(),
            budget,
        }
    }

    fn elapsed(&self) -> Duration {
        self.clock.monotonic().saturating_sub(self.started)
    }

    /// Time left, `None` when the budget is exhausted.
    pub fn remaining(&self) -> Option<Duration> {
        self.budget
            .checked_sub(self.elapsed())
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
        EngineError::ImageSourceFailure { retry, diagnostic } => retry.failure(format!(
            "{diagnostic} (after {attempts} of {max} attempts against the same source)"
        )),
        other => other,
    }
}

/// The full cause chain of a transport error (`error sending request:
/// client error (Connect): invalid peer certificate: UnknownIssuer`), so
/// the user sees why. This text never participates in retry classification.
/// Callers remove request URLs before formatting the chain.
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

    /// A virtual monotonic clock for one test (leaked: deadlines borrow
    /// their clock for `'static`, as the production system clock is).
    fn manual_clock() -> &'static crate::data::clock::ManualClock {
        Box::leak(Box::new(crate::data::clock::ManualClock::new(
            chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        )))
    }

    #[test]
    fn late_success_and_cancelled_success_are_rejected() {
        let clock = manual_clock();
        let budget = Duration::from_millis(10);
        let err = run_bounded(
            RetryPolicy::NONE,
            Deadline::start_on(budget, clock),
            &CancelToken::new(),
            &*no_sleep(),
            |_| {
                clock.advance(Duration::from_millis(30));
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
        struct Slow(&'static crate::data::clock::ManualClock);
        impl std::io::Read for Slow {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                // The read itself takes (virtual) time past the deadline.
                self.0.advance(Duration::from_millis(30));
                bytes[0] = 42;
                Ok(1)
            }
        }
        let clock = manual_clock();
        let control = OperationControl::new(
            CancelToken::new(),
            Deadline::start_on(Duration::from_millis(10), clock),
        );
        let err =
            std::io::copy(&mut control.reader(Slow(clock)), &mut std::io::sink()).unwrap_err();
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
        for diagnostic in [
            "connection reset",
            "certificate tls handshake unknownissuer invalid peer hostname client setup failed",
            "tls-proxy:latest /tmp/acq-P6TlSt/archive.tar",
            "entirely different diagnostic",
        ] {
            assert!(is_retryable(
                &ImageRetryClass::Transient.failure(diagnostic.into())
            ));
            assert!(!is_retryable(
                &ImageRetryClass::Permanent.failure(diagnostic.into())
            ));
            assert!(!is_retryable(&EngineError::Network(diagnostic.into())));
        }
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
    fn diagnostic_changes_and_redaction_do_not_change_attempt_counts() {
        for class in [ImageRetryClass::Transient, ImageRetryClass::Permanent] {
            for diagnostic in [
                "disconnected",
                "tls certificate handshake",
                "tls-agent:latest",
            ] {
                let mut attempts = 0;
                let err = run_bounded(
                    RetryPolicy {
                        initial_backoff: Duration::ZERO,
                        ..RetryPolicy::DEFAULT
                    },
                    Deadline::start(Duration::from_secs(60)),
                    &CancelToken::new(),
                    &|_| {},
                    |_| {
                        attempts += 1;
                        Err::<(), _>(map_diagnostic(class.failure(diagnostic.into()), |text| {
                            redact(&text, &["tls-agent"])
                        }))
                    },
                )
                .unwrap_err();
                assert_eq!(
                    attempts,
                    if class == ImageRetryClass::Transient {
                        3
                    } else {
                        1
                    }
                );
                assert_eq!(is_retryable(&err), class == ImageRetryClass::Transient);
                assert!(!err.to_string().contains("tls-agent"));
            }
        }
    }

    #[test]
    fn cancellation_and_deadlines_outrank_both_source_classes() {
        for class in [ImageRetryClass::Transient, ImageRetryClass::Permanent] {
            let cancel = CancelToken::new();
            let mut calls = 0;
            let err = run_bounded(
                RetryPolicy::DEFAULT,
                Deadline::start(Duration::from_secs(60)),
                &cancel,
                &|_| {},
                |_| {
                    calls += 1;
                    cancel.cancel();
                    Err::<(), _>(class.failure("source failure".into()))
                },
            )
            .unwrap_err();
            assert_eq!(calls, 1);
            assert!(is_cancelled(&err));

            let clock = manual_clock();
            let mut calls = 0;
            let err = run_bounded(
                RetryPolicy::DEFAULT,
                Deadline::start_on(Duration::from_millis(1), clock),
                &CancelToken::new(),
                &|_| {},
                |_| {
                    calls += 1;
                    clock.advance(Duration::from_millis(10));
                    Err::<(), _>(class.failure("source failure".into()))
                },
            )
            .unwrap_err();
            assert_eq!(calls, 1);
            assert!(matches!(err, EngineError::Network(ref text) if text.contains("deadline")));
        }
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
                Err::<(), _>(ImageRetryClass::Transient.failure("connection reset".into()))
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
                    Err(ImageRetryClass::Transient.failure("disconnected".into()))
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
                Err::<(), _>(ImageRetryClass::Transient.failure("reset".into()))
            },
        )
        .unwrap_err();
        assert!(is_cancelled(&err));
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    /// A 3 s backoff is slept in ≤ 20 ms slices with a cancellation check
    /// before each; a cancel arriving mid-backoff stops it at the next slice.
    /// The sleeper advances virtual time and cancels after the second slice.
    #[test]
    fn cancellation_interrupts_a_long_production_backoff() {
        let clock = manual_clock();
        let token = CancelToken::new();
        let slices = Mutex::new(Vec::new());
        let sleep = |slice: Duration| {
            clock.advance(slice);
            let mut slices = slices.lock().unwrap();
            slices.push(slice);
            if slices.len() == 2 {
                token.cancel();
            }
        };
        let error = run_bounded(
            RetryPolicy {
                initial_backoff: Duration::from_secs(3),
                ..RetryPolicy::DEFAULT
            },
            Deadline::start_on(Duration::from_secs(10), clock),
            &token,
            &sleep,
            |_| Err::<(), _>(ImageRetryClass::Transient.failure("disconnected".into())),
        )
        .unwrap_err();
        assert!(is_cancelled(&error));
        assert_eq!(
            *slices.lock().unwrap(),
            vec![Duration::from_millis(20); 2],
            "the backoff stops at the first slice after the cancel"
        );
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
                Err::<(), _>(ImageRetryClass::Transient.failure("reset".into()))
            },
        )
        .unwrap_err();
        assert_eq!(*calls.lock().unwrap(), 1, "a 10s backoff does not fit 5s");
        assert!(matches!(err, EngineError::ImageSourceFailure { .. }));
        let expired = Deadline::start(Duration::ZERO);
        assert!(expired.expired());
        assert_eq!(expired.request_timeout(None), Duration::from_millis(1));
    }

    #[test]
    fn deadlines_count_only_time_on_their_own_clock() {
        let clock = manual_clock();
        let deadline = Deadline::start_on(Duration::from_secs(5), clock);
        assert_eq!(deadline.remaining(), Some(Duration::from_secs(5)));
        assert_eq!(
            deadline.request_timeout(Some(Duration::from_secs(2))),
            Duration::from_secs(2)
        );
        clock.advance(Duration::from_secs(4));
        assert_eq!(deadline.remaining(), Some(Duration::from_secs(1)));
        assert_eq!(deadline.request_timeout(None), Duration::from_secs(1));
        clock.advance(Duration::from_secs(1));
        assert!(deadline.expired());
        assert!(deadline.check().is_err());
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

#[cfg(test)]
mod behavior_review_tests {
    use super::*;

    #[test]
    fn unreachable_network_kinds_retry_to_the_attempt_cap() {
        for kind in [
            std::io::ErrorKind::NetworkUnreachable,
            std::io::ErrorKind::HostUnreachable,
            std::io::ErrorKind::NetworkDown,
        ] {
            assert!(transient_io_kind(kind));
            let mut calls = 0;
            let err = run_bounded(
                RetryPolicy::DEFAULT,
                Deadline::start(Duration::from_secs(60)),
                &CancelToken::new(),
                &|_| {},
                |_| {
                    calls += 1;
                    Err::<(), _>(ImageRetryClass::Transient.failure(format!("{kind:?}")))
                },
            )
            .unwrap_err();
            assert_eq!(calls, 3);
            assert!(is_retryable(&err));
        }
        assert!(!transient_io_kind(std::io::ErrorKind::PermissionDenied));
    }

    #[test]
    fn request_timeout_and_rate_limit_statuses_remain_final() {
        for status in [
            reqwest::StatusCode::REQUEST_TIMEOUT,
            reqwest::StatusCode::TOO_MANY_REQUESTS,
        ] {
            assert_eq!(ImageRetryClass::status(status), ImageRetryClass::Permanent);
        }
    }
}
