//! Device-flow mechanics shared by browser linking and (later) work-server
//! linking (WI 0159, D12): short user codes, random secrets, hashing,
//! constant-time comparison and rate-limit buckets.
//!
//! Only mechanics live here. Who may create, poll or approve a request, and
//! which store a resulting credential lands in, is Layer 2 policy.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use ring::rand::{SecureRandom, SystemRandom};

use crate::engine::auth::{constant_time_hex_eq, hash_key};
use crate::engine::error::EngineError;

/// 30 symbols: no I, L, O, U, 0 or 1, so a code read aloud or retyped is
/// unambiguous.
pub const SHORT_CODE_ALPHABET: &[u8; 30] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";
/// Symbols per code, printed as two groups of four.
pub const SHORT_CODE_SYMBOLS: usize = 8;

/// A user-facing code such as `web-K7PM-3Q9X`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ShortCode(String);

impl ShortCode {
    /// A fresh uniformly random code with `prefix` (`web`).
    pub fn generate(prefix: &str) -> Result<Self, EngineError> {
        // Rejection sampling keeps every symbol equally likely: 240 = 8 × 30.
        let mut symbols = String::with_capacity(SHORT_CODE_SYMBOLS + 1);
        let rng = SystemRandom::new();
        while symbols.len() < SHORT_CODE_SYMBOLS {
            let mut buf = [0u8; 16];
            rng.fill(&mut buf)
                .map_err(|_| EngineError::Auth("failed to generate random bytes".into()))?;
            for b in buf {
                if b < 240 && symbols.len() < SHORT_CODE_SYMBOLS {
                    symbols.push(SHORT_CODE_ALPHABET[(b % 30) as usize] as char);
                }
            }
        }
        Ok(Self(format!(
            "{prefix}-{}-{}",
            &symbols[..4],
            &symbols[4..]
        )))
    }

    /// Parse user input: case-insensitive, surrounding whitespace ignored.
    /// `None` unless it is `<prefix>-XXXX-XXXX` over the alphabet.
    pub fn parse(input: &str, prefix: &str) -> Option<Self> {
        let input = input.trim();
        let rest = input
            .get(..prefix.len() + 1)
            .filter(|p| p[..prefix.len()].eq_ignore_ascii_case(prefix) && p.ends_with('-'))
            .map(|_| &input[prefix.len() + 1..])?;
        let upper = rest.to_ascii_uppercase();
        let (a, b) = upper.split_once('-')?;
        let valid = |g: &str| g.len() == 4 && g.bytes().all(|c| SHORT_CODE_ALPHABET.contains(&c));
        (valid(a) && valid(b)).then(|| Self(format!("{prefix}-{a}-{b}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ShortCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Codes are short-lived lookup material, not credentials; printing
        // them in diagnostics is fine.
        f.debug_tuple("ShortCode").field(&self.0).finish()
    }
}

/// `bytes` random bytes as base64url without padding (32 → 43 characters).
pub fn generate_secret(bytes: usize) -> Result<String, EngineError> {
    let mut buf = vec![0u8; bytes];
    SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| EngineError::Auth("failed to generate random bytes".into()))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf))
}

/// SHA-256 of a secret, lowercase hex. Secrets are 256-bit random values, so
/// an unsalted fast digest is sufficient.
pub fn hash_secret(secret: &str) -> String {
    hash_key(secret)
}

/// Does `presented` hash to `stored_hash`? Constant time in the hash.
pub fn secret_matches(presented: &str, stored_hash: &str) -> bool {
    constant_time_hex_eq(&hash_secret(presented), stored_hash)
}

/// Sliding-window counters keyed by string (an origin, a listener, "owner").
///
/// Two uses: [`RateLimiter::try_acquire`] admits at most `limit` events per
/// `window`; [`RateLimiter::record_failure`] counts failures and, on reaching
/// `limit` within `window`, locks the key out for `lockout`
/// ([`RateLimiter::locked`]). Time is injected so tests are deterministic.
#[derive(Debug)]
pub struct RateLimiter {
    limit: u32,
    window: Duration,
    lockout: Duration,
    buckets: Mutex<HashMap<String, Bucket>>,
}

#[derive(Debug, Default)]
struct Bucket {
    events: VecDeque<Instant>,
    locked_until: Option<Instant>,
}

/// Keys kept before idle buckets are swept.
const MAX_BUCKETS: usize = 1024;

impl RateLimiter {
    pub fn new(limit: u32, window: Duration, lockout: Duration) -> Self {
        Self {
            limit,
            window,
            lockout,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Admit one event for `key`, or say how long until one would be.
    pub fn try_acquire(&self, key: &str, now: Instant) -> Result<(), Duration> {
        let mut buckets = self.lock(now);
        let bucket = buckets.entry(key.to_owned()).or_default();
        if let Some(wait) = Self::lock_remaining(bucket, now) {
            return Err(wait);
        }
        self.prune(bucket, now);
        if bucket.events.len() >= self.limit as usize {
            let oldest = *bucket.events.front().expect("non-empty at limit");
            return Err((oldest + self.window).saturating_duration_since(now));
        }
        bucket.events.push_back(now);
        Ok(())
    }

    /// `Some(remaining)` while `key` is locked out.
    pub fn locked(&self, key: &str, now: Instant) -> Option<Duration> {
        let mut buckets = self.lock(now);
        buckets
            .get_mut(key)
            .and_then(|b| Self::lock_remaining(b, now))
    }

    /// Count a failure; returns `Some(lockout)` when this failure trips it.
    pub fn record_failure(&self, key: &str, now: Instant) -> Option<Duration> {
        let mut buckets = self.lock(now);
        let bucket = buckets.entry(key.to_owned()).or_default();
        self.prune(bucket, now);
        bucket.events.push_back(now);
        if bucket.events.len() >= self.limit as usize {
            bucket.events.clear();
            bucket.locked_until = Some(now + self.lockout);
            return Some(self.lockout);
        }
        None
    }

    fn lock_remaining(bucket: &mut Bucket, now: Instant) -> Option<Duration> {
        match bucket.locked_until {
            Some(until) if until > now => Some(until - now),
            Some(_) => {
                bucket.locked_until = None;
                None
            }
            None => None,
        }
    }

    fn prune(&self, bucket: &mut Bucket, now: Instant) {
        while bucket
            .events
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= self.window)
        {
            bucket.events.pop_front();
        }
    }

    fn lock(&self, now: Instant) -> std::sync::MutexGuard<'_, HashMap<String, Bucket>> {
        let mut buckets = self.buckets.lock().unwrap_or_else(|p| p.into_inner());
        if buckets.len() > MAX_BUCKETS {
            let window = self.window;
            buckets.retain(|_, b| {
                b.locked_until.is_some_and(|u| u > now)
                    || b.events
                        .back()
                        .is_some_and(|t| now.saturating_duration_since(*t) < window)
            });
        }
        buckets
    }
}

/// Seconds for a `Retry-After` header: at least 1, rounded up.
pub fn retry_after_secs(wait: Duration) -> u64 {
    (wait.as_nanos().div_ceil(1_000_000_000) as u64).max(1)
}

/// Deadline and poll pacing shared by both device flows; no authority or store.
#[derive(Debug, Clone)]
pub struct PollSchedule {
    deadline: Instant,
    interval: Duration,
}
impl PollSchedule {
    pub fn new(ttl: Duration, interval: Duration) -> Self {
        Self {
            deadline: Instant::now() + ttl,
            interval: interval.max(Duration::from_millis(1)),
        }
    }
    pub fn expired(&self) -> bool {
        deadline_expired(Instant::now(), self.deadline)
    }
    pub fn interval(&self) -> Duration {
        poll_delay(self.interval, Duration::ZERO)
    }
    pub fn slow_down(&mut self) {
        self.interval = self.interval.saturating_add(Duration::from_secs(5));
    }
}

/// Common expiry boundary: the instant at the deadline is already expired.
pub fn deadline_expired<T: PartialOrd>(now: T, deadline: T) -> bool {
    now >= deadline
}
/// Time still owed before another device-flow poll is allowed.
pub fn poll_delay(interval: Duration, elapsed: Duration) -> Duration {
    interval.saturating_sub(elapsed)
}
