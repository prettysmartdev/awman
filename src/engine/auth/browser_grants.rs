//! `BrowserGrantStore` — credentials for paired browsers (WI 0159).
//!
//! A grant is an `awb_…` bearer token bound to one origin and one listener
//! name (D2), valid for 12 hours from approval (D7). Only the SHA-256 of the
//! token is kept, in memory and in `<data>/daemon/browser_grants.json`
//! (`0600`). Security properties held here:
//!
//! * verification hashes the presented token once and compares it against
//!   every stored hash in constant time with no early exit, then checks the
//!   origin, listener and expiry;
//! * revocation happens in memory before the disk write, so a revoked grant
//!   is refused from the next request even if persisting fails;
//! * each grant has one shared lifecycle ([`GrantWatch`]). The **first**
//!   ending it observes is final: an expiry seen by a bearer check, ticket,
//!   watch, proof or admission latches `Expired`, so a later backward clock
//!   correction cannot revive the grant (WI 0160 SR-02). Expired grants are
//!   queued so the policy layer can finish their cleanup
//!   ([`BrowserGrantStore::take_expired`]);
//! * the store is separate from `ApiKeyStore` and from any work-server
//!   credential: nothing else is ever consulted for a browser bearer.
//!
//! Time comes from an injectable [`GrantClock`] so tests can model suspends,
//! clock corrections and deadlines crossed during an operation.
//!
//! A daemon stop revokes every grant, so opening the store discards records
//! a previous daemon left behind (for example after a crash).

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use crate::data::browser_auth::{
    BrowserScope, BrowserSecret, GrantRevocationReason, GRANT_TOKEN_PREFIX, GRANT_TTL,
    GRANT_WATCH_RECHECK,
};
use crate::data::fs::browser_grant_store::{BrowserGrantFile, BrowserGrantRecord, MAX_LABEL_CHARS};
use crate::data::ids::BrowserGrantId;
use crate::engine::auth::constant_time_hex_eq;
use crate::engine::auth::device_flow::{generate_secret, hash_secret};
use crate::engine::error::EngineError;

/// How often a grant's `last_used_at` reaches the disk at most.
pub const LAST_USED_FLUSH_INTERVAL: Duration = Duration::from_secs(60);

const SENTINEL_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

// ─── Clock ──────────────────────────────────────────────────────────────────

/// Where a time sample is taken. Production ignores it; [`ManualClock`]
/// can move time at exactly one named point, which is how tests place a
/// deadline crossing inside an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplePoint {
    Ordinary,
    /// Proof release, after signature verification, under the reauth lock.
    ProofRelease,
    /// Poll collection, under the reauth lock.
    PollRelease,
    /// A frontend admission's final grant check, under the instance lock.
    Admission,
}

/// Browser authorization's source of time: wall clock, monotonic clock and
/// a monotonic timer.
pub trait GrantClock: Send + Sync + std::fmt::Debug {
    /// The current (wall, monotonic) time.
    fn sample(&self, point: SamplePoint) -> (DateTime<Utc>, Instant);
    /// Sleep for `duration` of monotonic time. Like a real timer, it does
    /// not advance while the machine is suspended.
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// The real clocks.
#[derive(Debug, Default)]
pub struct SystemClock;

impl GrantClock for SystemClock {
    fn sample(&self, _: SamplePoint) -> (DateTime<Utc>, Instant) {
        (Utc::now(), Instant::now())
    }
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(duration))
    }
}

/// A deterministic clock for tests. It starts at the real time and moves
/// only when told to: [`Self::advance`] (both clocks, timers fire),
/// [`Self::jump_wall`] (a suspend or a wall-clock correction; timers do not
/// fire) and [`Self::jump_at`] (a one-shot move when `point` is sampled).
#[derive(Debug)]
pub struct ManualClock {
    base: Instant,
    state: Mutex<ManualState>,
    mono: tokio::sync::watch::Sender<Duration>,
}

#[derive(Debug)]
struct ManualState {
    wall: DateTime<Utc>,
    mono: Duration,
    jumps: Vec<(SamplePoint, chrono::Duration, Duration)>,
}

impl ManualClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            base: Instant::now(),
            state: Mutex::new(ManualState {
                wall: Utc::now(),
                mono: Duration::ZERO,
                jumps: Vec::new(),
            }),
            mono: tokio::sync::watch::channel(Duration::ZERO).0,
        })
    }

    /// Ordinary time passing: both clocks move and due timers fire.
    pub fn advance(&self, by: Duration) {
        let mut state = self.lock();
        state.wall += chrono::Duration::from_std(by).expect("test duration fits");
        state.mono += by;
        self.mono.send_replace(state.mono);
    }

    /// Only the wall clock moves (forward for a suspend, backward for a
    /// correction). Monotonic time and timers stand still.
    pub fn jump_wall(&self, by: chrono::Duration) {
        self.lock().wall += by;
    }

    /// The next sample taken at `point` first moves the wall clock by
    /// `wall` and the monotonic clock by `mono`.
    pub fn jump_at(&self, point: SamplePoint, wall: chrono::Duration, mono: Duration) {
        self.lock().jumps.push((point, wall, mono));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ManualState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl GrantClock for ManualClock {
    fn sample(&self, point: SamplePoint) -> (DateTime<Utc>, Instant) {
        let mut state = self.lock();
        if let Some(i) = state.jumps.iter().position(|(p, _, _)| *p == point) {
            let (_, wall, mono) = state.jumps.remove(i);
            state.wall += wall;
            state.mono += mono;
            self.mono.send_replace(state.mono);
        }
        (state.wall, self.base + state.mono)
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let mut rx = self.mono.subscribe();
        let target = *rx.borrow() + duration;
        Box::pin(async move {
            while *rx.borrow_and_update() < target {
                if rx.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        })
    }
}

/// One operation's view of time. It starts at the caller's (`now`,
/// `instant`) and a later [`Self::sample`] moves forward by however far the
/// clock moved since the start (never backward), so deadlines checked at a
/// release boundary use fresh time even when the caller supplied the start.
#[derive(Debug, Clone)]
pub struct OperationTime {
    now: DateTime<Utc>,
    instant: Instant,
    wall_start: DateTime<Utc>,
    mono_start: Instant,
    clock: Arc<dyn GrantClock>,
}

impl OperationTime {
    pub fn start(clock: Arc<dyn GrantClock>, now: DateTime<Utc>, instant: Instant) -> Self {
        let (wall_start, mono_start) = clock.sample(SamplePoint::Ordinary);
        Self {
            now,
            instant,
            wall_start,
            mono_start,
            clock,
        }
    }

    /// The operation's start time.
    pub fn started(&self) -> (DateTime<Utc>, Instant) {
        (self.now, self.instant)
    }

    /// Fresh time at `point`. The wall clock advances by the larger of the
    /// wall and monotonic elapsed time, so neither a suspend nor a backward
    /// correction during the operation can hide a deadline.
    pub fn sample(&self, point: SamplePoint) -> (DateTime<Utc>, Instant) {
        let (wall, mono) = self.clock.sample(point);
        let mono_elapsed = mono.saturating_duration_since(self.mono_start);
        let wall_elapsed = (wall - self.wall_start).to_std().unwrap_or_default();
        let wall_advance = chrono::Duration::from_std(wall_elapsed.max(mono_elapsed))
            .unwrap_or(chrono::Duration::MAX);
        (self.now + wall_advance, self.instant + mono_elapsed)
    }
}

// ─── Grant lifecycle ────────────────────────────────────────────────────────

/// The result of checking a browser request's bearer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantVerdict {
    Accepted {
        id: BrowserGrantId,
    },
    /// No `Authorization` header at all.
    Missing,
    /// Malformed, unknown, revoked, expired, or bound to another origin or
    /// listener. Deliberately one variant.
    Invalid,
}

/// One grant's shared lifecycle. `ended` holds the first ending; it is set
/// once and never cleared.
#[derive(Debug)]
struct GrantLife {
    id: BrowserGrantId,
    expires_at: DateTime<Utc>,
    /// The monotonic twelve-hour deadline recorded at approval; a backward
    /// wall-clock change cannot extend a grant past it.
    deadline: Instant,
    ended: Mutex<Option<GrantRevocationReason>>,
    token: CancellationToken,
    clock: Arc<dyn GrantClock>,
    /// The store's queue of grants whose expiry was observed here.
    expired: Arc<Mutex<Vec<BrowserGrantId>>>,
}

impl GrantLife {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<GrantRevocationReason>> {
        self.ended.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// End with `reason` unless already ended (first reason wins).
    fn end(&self, reason: GrantRevocationReason) {
        let mut ended = self.lock();
        if ended.is_none() {
            *ended = Some(reason);
            self.token.cancel();
        }
    }

    /// The ending at (`now`, `instant`), latching an expiry it observes.
    fn check_locked(
        &self,
        ended: &mut Option<GrantRevocationReason>,
        now: DateTime<Utc>,
        instant: Instant,
    ) -> Option<GrantRevocationReason> {
        if ended.is_none() && (now >= self.expires_at || instant >= self.deadline) {
            *ended = Some(GrantRevocationReason::Expired);
            self.token.cancel();
            self.expired
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(self.id.clone());
        }
        *ended
    }

    fn check(&self, now: DateTime<Utc>, instant: Instant) -> Option<GrantRevocationReason> {
        let mut ended = self.lock();
        self.check_locked(&mut ended, now, instant)
    }
}

/// A live browser authority: one grant's ID and lifecycle. WebSocket tasks
/// select on [`Self::ended`]; admission runs its final check with
/// [`Self::while_live`].
#[derive(Debug, Clone)]
pub struct GrantWatch {
    life: Arc<GrantLife>,
}

impl GrantWatch {
    pub fn grant_id(&self) -> &BrowserGrantId {
        &self.life.id
    }

    /// Resolves when the grant is revoked or expires, with the reason.
    ///
    /// Expiry is the earlier of the wall-clock `expires_at` and the
    /// monotonic deadline recorded at approval. A single timer would not do:
    /// timers stop during a system suspend while wall time advances, so
    /// both clocks are re-evaluated at least every [`GRANT_WATCH_RECHECK`].
    pub async fn ended(&self) -> GrantRevocationReason {
        let clock = &self.life.clock;
        loop {
            let (now, instant) = clock.sample(SamplePoint::Ordinary);
            if let Some(reason) = self.check(now, instant) {
                return reason;
            }
            let wall = (self.life.expires_at - now)
                .to_std()
                .unwrap_or(Duration::ZERO);
            let mono = self.life.deadline.saturating_duration_since(instant);
            let wait = wall.min(mono).min(GRANT_WATCH_RECHECK);
            tokio::select! {
                _ = self.life.token.cancelled() => {}
                _ = clock.sleep(wait) => {}
            }
        }
    }

    /// The reason the grant has ended at (`now`, `instant`), if it has. An
    /// observed expiry is final.
    pub fn check(&self, now: DateTime<Utc>, instant: Instant) -> Option<GrantRevocationReason> {
        self.life.check(now, instant)
    }

    /// [`Self::check`] at the grant clock's current time.
    pub fn check_now(&self) -> Option<GrantRevocationReason> {
        let (now, instant) = self.life.clock.sample(SamplePoint::Ordinary);
        self.check(now, instant)
    }

    /// Run `f` only while the grant is live, serialized with its revocation
    /// and expiry: no ending can be recorded while `f` runs. `f` must be
    /// short and must not call back into the grant store.
    pub fn while_live<T>(&self, f: impl FnOnce() -> T) -> Result<T, GrantRevocationReason> {
        let mut ended = self.life.lock();
        let (now, instant) = self.life.clock.sample(SamplePoint::Admission);
        match self.life.check_locked(&mut ended, now, instant) {
            Some(reason) => Err(reason),
            None => Ok(f()),
        }
    }

    /// Already ended (revoked or an observed expiry)?
    pub fn is_revoked(&self) -> bool {
        self.life.token.is_cancelled()
    }
}

/// What a bulk revocation did. The affected IDs are reported whether or
/// not the disk write succeeded: they are refused in memory either way.
#[derive(Debug)]
pub struct Revoked {
    pub ids: Vec<BrowserGrantId>,
    pub persisted: Result<(), EngineError>,
}

/// The daemon's browser grants. Shared as `Arc<BrowserGrantStore>`.
#[derive(Debug)]
pub struct BrowserGrantStore {
    path: PathBuf,
    file: RwLock<BrowserGrantFile>,
    live: Mutex<HashMap<BrowserGrantId, Arc<GrantLife>>>,
    last_used_flush: Mutex<HashMap<BrowserGrantId, Instant>>,
    clock: RwLock<Arc<dyn GrantClock>>,
    expired: Arc<Mutex<Vec<BrowserGrantId>>>,
}

impl BrowserGrantStore {
    /// Open the store at `path` (normally `DaemonPaths::browser_grants_file()`).
    /// An unsafe or malformed file is an error; records left by a previous
    /// daemon are discarded (a daemon stop revokes every grant).
    pub fn open(path: PathBuf) -> Result<Arc<Self>, EngineError> {
        let mut file = BrowserGrantFile::load(&path)?;
        if !file.grants.is_empty() {
            file.grants.clear();
            file.store(&path)?;
        }
        Ok(Arc::new(Self {
            path,
            file: RwLock::new(file),
            live: Mutex::new(HashMap::new()),
            last_used_flush: Mutex::new(HashMap::new()),
            clock: RwLock::new(Arc::new(SystemClock)),
            expired: Arc::new(Mutex::new(Vec::new())),
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The clock every grant check reads.
    pub fn clock(&self) -> Arc<dyn GrantClock> {
        self.clock.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Replace the clock (tests). Grants created earlier keep theirs.
    pub fn set_clock(&self, clock: Arc<dyn GrantClock>) {
        *self.clock.write().unwrap_or_else(|p| p.into_inner()) = clock;
    }

    /// Mint a grant for `origin` on `listener`. Returns the record and the
    /// token (`awb_` + 43 base64url); the token exists nowhere else.
    ///
    /// Expired records are not pruned here: they end only through the
    /// termination lifecycle, so their reason is never lost (SR-05).
    pub fn create(
        &self,
        origin: &str,
        listener: &str,
        scope: BrowserScope,
        label: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(BrowserGrantRecord, BrowserSecret), EngineError> {
        let token = format!("{GRANT_TOKEN_PREFIX}{}", generate_secret(32)?);
        let record = BrowserGrantRecord {
            id: BrowserGrantId::new(),
            hash: hash_secret(&token),
            origin: origin.to_owned(),
            listener: listener.to_owned(),
            scope,
            created_at: now,
            expires_at: now + chrono::Duration::from_std(GRANT_TTL).expect("12h fits"),
            last_used_at: None,
            label: label.map(sanitize_label).filter(|l| !l.is_empty()),
        };
        let mut file = self.write();
        let mut next = file.clone();
        next.grants.push(record.clone());
        next.store(&self.path)?;
        *file = next;
        let clock = self.clock();
        let (_, instant) = clock.sample(SamplePoint::Ordinary);
        self.live_map().insert(
            record.id.clone(),
            Arc::new(GrantLife {
                id: record.id.clone(),
                expires_at: record.expires_at,
                deadline: instant + GRANT_TTL,
                ended: Mutex::new(None),
                token: CancellationToken::new(),
                clock,
                expired: self.expired.clone(),
            }),
        );
        Ok((record, BrowserSecret::new(token)))
    }

    /// Check a request's `Authorization` header for a grant bound to
    /// `origin` and `listener`. Only `Bearer awb_…` can match; anything else
    /// (an API key, a work-server token) is `Invalid` after the same work.
    pub fn verify(
        &self,
        authorization_header: Option<&str>,
        origin: &str,
        listener: &str,
        now: DateTime<Utc>,
    ) -> GrantVerdict {
        let Some(header) = authorization_header else {
            return GrantVerdict::Missing;
        };
        let presented = strip_bearer(header);
        let well_formed = presented.starts_with(GRANT_TOKEN_PREFIX);
        let hash = hash_secret(presented);
        let matched = {
            let file = self.read();
            let mut matched: Option<BrowserGrantRecord> = None;
            let _ = constant_time_hex_eq(&hash, SENTINEL_HASH);
            for record in &file.grants {
                let eq = constant_time_hex_eq(&hash, &record.hash);
                if eq && matched.is_none() {
                    matched = Some(record.clone());
                }
            }
            matched
        };
        match matched {
            Some(record)
                if well_formed
                    && record.origin == origin
                    && record.listener == listener
                    && self.live_at(&record.id, now) =>
            {
                self.touch(&record.id, now);
                GrantVerdict::Accepted { id: record.id }
            }
            _ => GrantVerdict::Invalid,
        }
    }

    /// Is `id` a live grant bound to `origin` and `listener` (tickets)?
    pub fn is_valid(
        &self,
        id: &BrowserGrantId,
        origin: &str,
        listener: &str,
        now: DateTime<Utc>,
    ) -> bool {
        self.get(id)
            .is_some_and(|r| r.origin == origin && r.listener == listener)
            && self.live_at(id, now)
    }

    /// Live (unexpired, unrevoked) grants, oldest first.
    pub fn list(&self, now: DateTime<Utc>) -> Vec<BrowserGrantRecord> {
        let mut grants: Vec<_> = self
            .read()
            .grants
            .iter()
            .filter(|g| g.expires_at > now)
            .cloned()
            .collect();
        grants.retain(|g| self.live_at(&g.id, now));
        grants.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        grants
    }

    pub fn get(&self, id: &BrowserGrantId) -> Option<BrowserGrantRecord> {
        self.read().grants.iter().find(|g| &g.id == id).cloned()
    }

    /// Revoke one grant. `Ok(false)` when unknown. Refused from the moment
    /// this returns even when the disk write fails (the error is returned).
    pub fn delete(
        &self,
        id: &BrowserGrantId,
        reason: GrantRevocationReason,
    ) -> Result<bool, EngineError> {
        let revoked = self.revoke_where(|g| &g.id == id, reason);
        revoked.persisted?;
        Ok(!revoked.ids.is_empty())
    }

    /// Revoke every grant matching `pred`. Memory is updated (and watches
    /// fire) before the disk write; the IDs are returned either way.
    pub fn revoke_where(
        &self,
        pred: impl Fn(&BrowserGrantRecord) -> bool,
        reason: GrantRevocationReason,
    ) -> Revoked {
        let mut file = self.write();
        let ids: Vec<BrowserGrantId> = file
            .grants
            .iter()
            .filter(|g| pred(g))
            .map(|g| g.id.clone())
            .collect();
        if ids.is_empty() {
            return Revoked {
                ids,
                persisted: Ok(()),
            };
        }
        file.grants.retain(|g| !ids.contains(&g.id));
        {
            let mut live = self.live_map();
            for id in &ids {
                if let Some(life) = live.remove(id) {
                    life.end(reason);
                }
            }
        }
        {
            let mut flush = self.flush_times();
            for id in &ids {
                flush.remove(id);
            }
        }
        let persisted = file.store(&self.path).map_err(EngineError::from);
        Revoked { ids, persisted }
    }

    /// Live grants bound to `listener` (for `daemon status`).
    pub fn count_for_listener(&self, listener: &str, now: DateTime<Utc>) -> usize {
        self.list(now)
            .iter()
            .filter(|g| g.listener == listener)
            .count()
    }

    /// The live authority of `id`; `None` if `id` is unknown or has ended.
    pub fn watch(&self, id: &BrowserGrantId) -> Option<GrantWatch> {
        let life = self.live_map().get(id)?.clone();
        let watch = GrantWatch { life };
        watch.check_now().is_none().then_some(watch)
    }

    /// WI 0160: why `id` is no longer valid at (`now`, `instant`), or `None`
    /// while it is live. Unknown or already-removed grants report `Deleted`.
    pub fn ended_at(
        &self,
        id: &BrowserGrantId,
        now: DateTime<Utc>,
        instant: Instant,
    ) -> Option<GrantRevocationReason> {
        let life = self.live_map().get(id).cloned();
        match life {
            Some(life) => life.check(now, instant),
            None => Some(GrantRevocationReason::Deleted),
        }
    }

    /// Observe every grant at the current time (latching any expiry), then
    /// hand over every grant whose expiry has been observed since the last
    /// call, so the caller can finish their cleanup.
    pub fn take_expired(&self) -> Vec<BrowserGrantId> {
        let (now, instant) = self.clock().sample(SamplePoint::Ordinary);
        let lives: Vec<Arc<GrantLife>> = self.live_map().values().cloned().collect();
        for life in lives {
            life.check(now, instant);
        }
        std::mem::take(&mut *self.expired.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// Unended at `now` and the clock's monotonic time. Observing an
    /// expiry here latches it.
    fn live_at(&self, id: &BrowserGrantId, now: DateTime<Utc>) -> bool {
        let Some(life) = self.live_map().get(id).cloned() else {
            return false;
        };
        let (_, instant) = life.clock.sample(SamplePoint::Ordinary);
        life.check(now, instant).is_none()
    }

    fn touch(&self, id: &BrowserGrantId, now: DateTime<Utc>) {
        let mut file = self.write();
        let Some(record) = file.grants.iter_mut().find(|g| &g.id == id) else {
            return;
        };
        record.last_used_at = Some(now);
        let instant = Instant::now();
        let due = {
            let mut flushed = self.flush_times();
            let due = flushed
                .get(id)
                .is_none_or(|t| instant.duration_since(*t) >= LAST_USED_FLUSH_INTERVAL);
            if due {
                flushed.insert(id.clone(), instant);
            }
            due
        };
        if due {
            if let Err(e) = file.store(&self.path) {
                tracing::warn!(error = %e, "could not record browser grant last use");
            }
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BrowserGrantFile> {
        self.file.read().unwrap_or_else(|p| p.into_inner())
    }
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BrowserGrantFile> {
        self.file.write().unwrap_or_else(|p| p.into_inner())
    }
    fn live_map(&self) -> std::sync::MutexGuard<'_, HashMap<BrowserGrantId, Arc<GrantLife>>> {
        self.live.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn flush_times(&self) -> std::sync::MutexGuard<'_, HashMap<BrowserGrantId, Instant>> {
        self.last_used_flush
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }
}

/// `Bearer <token>` → `<token>`; anything else is taken as-is.
fn strip_bearer(value: &str) -> &str {
    match value.get(..7) {
        Some(prefix) if prefix.eq_ignore_ascii_case("bearer ") => value[7..].trim(),
        _ => value.trim(),
    }
}

/// Printable, at most 64 characters.
fn sanitize_label(label: &str) -> String {
    label
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_LABEL_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: &str = "http://127.0.0.1:3000";

    use awman_test_support::{TestCase, TestSandbox};

    fn sandbox(id: &'static str) -> TestSandbox {
        TestSandbox::for_test(TestCase::component(id))
    }

    fn store(dir: &TestSandbox, clock: &Arc<ManualClock>) -> Arc<BrowserGrantStore> {
        let path = dir.work().join("daemon").join("browser_grants.json");
        let store = BrowserGrantStore::open(path).unwrap();
        store.set_clock(clock.clone());
        store
    }

    fn twelve_hours() -> chrono::Duration {
        chrono::Duration::from_std(GRANT_TTL).unwrap()
    }

    /// The pre-WI 0160 watch: one sleep until the earlier deadline. Kept
    /// here only as the negative control for the suspend test below.
    async fn single_sleep_ended(watch: &GrantWatch) -> GrantRevocationReason {
        let clock = &watch.life.clock;
        let (now, instant) = clock.sample(SamplePoint::Ordinary);
        let wall = (watch.life.expires_at - now).to_std().unwrap_or_default();
        let mono = watch.life.deadline.saturating_duration_since(instant);
        tokio::select! {
            _ = watch.life.token.cancelled() => {}
            _ = clock.sleep(wall.min(mono)) => {}
        }
        let (now, instant) = clock.sample(SamplePoint::Ordinary);
        watch
            .check(now, instant)
            .unwrap_or(GrantRevocationReason::Deleted)
    }

    #[tokio::test]
    async fn a_watch_ends_after_a_suspend_within_one_recheck_and_a_single_sleep_would_not() {
        let dir = sandbox("engine::auth::browser_grants::tests::a_watch_ends_after_a_suspend_within_one_recheck_and_a_single_sleep_would_not");
        let clock = ManualClock::new();
        let store = store(&dir, &clock);
        let (now, _) = clock.sample(SamplePoint::Ordinary);
        let (record, _) = store
            .create(ORIGIN, "l", BrowserScope::Engine, None, now)
            .unwrap();
        let watch = store.watch(&record.id).unwrap();
        let rechecking = tokio::spawn({
            let watch = watch.clone();
            async move { watch.ended().await }
        });
        let single = tokio::spawn({
            let watch = watch.clone();
            async move { single_sleep_ended(&watch).await }
        });
        tokio::task::yield_now().await;
        // Suspend: the wall clock passes expiry, timers do not run.
        clock.jump_wall(twelve_hours() + chrono::Duration::hours(1));
        // After resume, one recheck interval of real (timer) time passes.
        clock.advance(GRANT_WATCH_RECHECK);
        let ended = tokio::time::timeout(Duration::from_secs(5), rechecking)
            .await
            .expect("the rechecking watch must fire")
            .unwrap();
        assert_eq!(ended, GrantRevocationReason::Expired);
        // Negative control: the old single sleep is still waiting for
        // twelve hours of timer time, so it would keep the socket open.
        // (The rechecking watch latched the expiry, which cancels the token;
        // so the control is evaluated on a second, fresh grant.)
        single.abort();
        let (now, _) = clock.sample(SamplePoint::Ordinary);
        let (other, _) = store
            .create(ORIGIN, "l", BrowserScope::Engine, None, now)
            .unwrap();
        let other = store.watch(&other.id).unwrap();
        let single = tokio::spawn({
            let other = other.clone();
            async move { single_sleep_ended(&other).await }
        });
        tokio::task::yield_now().await;
        clock.jump_wall(twelve_hours() + chrono::Duration::hours(1));
        clock.advance(GRANT_WATCH_RECHECK);
        assert!(
            tokio::time::timeout(Duration::from_millis(300), single)
                .await
                .is_err(),
            "a single-sleep watch misses the suspend"
        );
    }

    #[test]
    fn an_observed_expiry_is_final_after_a_backward_clock_correction() {
        let dir = sandbox("engine::auth::browser_grants::tests::an_observed_expiry_is_final_after_a_backward_clock_correction");
        let clock = ManualClock::new();
        let store = store(&dir, &clock);
        let bearer = |store: &BrowserGrantStore, token: &BrowserSecret, now| {
            store.verify(
                Some(&format!("Bearer {}", token.expose())),
                ORIGIN,
                "l",
                now,
            )
        };
        let (now, _) = clock.sample(SamplePoint::Ordinary);
        let after = now + twelve_hours() + chrono::Duration::hours(1);
        // Each observation path, on its own grant.
        for path in ["bearer", "ticket", "watch", "proof", "admission"] {
            let (record, token) = store
                .create(ORIGIN, "l", BrowserScope::Engine, None, now)
                .unwrap();
            let id = record.id.clone();
            let watch = store.watch(&id).unwrap();
            clock.jump_wall(after - now);
            match path {
                "bearer" => assert_eq!(bearer(&store, &token, after), GrantVerdict::Invalid),
                "ticket" => assert!(!store.is_valid(&id, ORIGIN, "l", after)),
                "watch" => assert!(watch.check_now().is_some()),
                "proof" => assert!(store.ended_at(&id, after, Instant::now()).is_some()),
                _ => assert_eq!(watch.while_live(|| ()), Err(GrantRevocationReason::Expired)),
            }
            // The clock is corrected back: nothing revives the grant.
            clock.jump_wall(now - after);
            assert_eq!(bearer(&store, &token, now), GrantVerdict::Invalid, "{path}");
            assert!(!store.is_valid(&id, ORIGIN, "l", now), "{path}");
            assert_eq!(
                watch.check_now(),
                Some(GrantRevocationReason::Expired),
                "{path}"
            );
            assert!(watch.is_revoked(), "{path}");
            assert!(store.watch(&id).is_none(), "{path}");
            assert_eq!(
                store.ended_at(&id, now, Instant::now()),
                Some(GrantRevocationReason::Expired),
                "{path}"
            );
            assert_eq!(watch.while_live(|| ()), Err(GrantRevocationReason::Expired));
            assert_eq!(store.take_expired(), vec![id], "{path}");
        }
    }

    #[test]
    fn creating_a_grant_keeps_an_expired_record_for_the_termination_lifecycle() {
        let dir = sandbox("engine::auth::browser_grants::tests::creating_a_grant_keeps_an_expired_record_for_the_termination_lifecycle");
        let clock = ManualClock::new();
        let store = store(&dir, &clock);
        let (now, _) = clock.sample(SamplePoint::Ordinary);
        let (a, _) = store
            .create(ORIGIN, "l", BrowserScope::Engine, None, now)
            .unwrap();
        let later = now + twelve_hours() + chrono::Duration::hours(1);
        store
            .create(ORIGIN, "l", BrowserScope::Engine, None, later)
            .unwrap();
        assert!(store.get(&a.id).is_some());
        assert_eq!(
            store.ended_at(&a.id, later, Instant::now()),
            Some(GrantRevocationReason::Expired)
        );
    }

    #[test]
    fn a_failed_revocation_write_still_reports_and_refuses_every_grant() {
        let dir = sandbox("engine::auth::browser_grants::tests::a_failed_revocation_write_still_reports_and_refuses_every_grant");
        let clock = ManualClock::new();
        let store = store(&dir, &clock);
        let (now, _) = clock.sample(SamplePoint::Ordinary);
        let (a, _) = store
            .create(ORIGIN, "l", BrowserScope::Engine, None, now)
            .unwrap();
        let watch = store.watch(&a.id).unwrap();
        std::fs::remove_file(store.path()).unwrap();
        std::fs::create_dir(store.path()).unwrap();
        let revoked = store.revoke_where(|_| true, GrantRevocationReason::OriginRemoved);
        assert_eq!(revoked.ids, vec![a.id.clone()]);
        assert!(revoked.persisted.is_err());
        assert_eq!(
            watch.check_now(),
            Some(GrantRevocationReason::OriginRemoved)
        );
        assert!(!store.is_valid(&a.id, ORIGIN, "l", now));
    }

    #[test]
    fn while_live_is_serialized_with_revocation() {
        let dir = sandbox(
            "engine::auth::browser_grants::tests::while_live_is_serialized_with_revocation",
        );
        let clock = ManualClock::new();
        let store = store(&dir, &clock);
        let (now, _) = clock.sample(SamplePoint::Ordinary);
        let (a, _) = store
            .create(ORIGIN, "l", BrowserScope::Engine, None, now)
            .unwrap();
        let watch = store.watch(&a.id).unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let admitting = std::thread::spawn({
            let watch = watch.clone();
            move || {
                watch.while_live(|| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    // Nothing can end the grant while this runs.
                    watch.is_revoked()
                })
            }
        });
        entered_rx.recv().unwrap();
        let revoking = std::thread::spawn({
            let store = store.clone();
            let id = a.id.clone();
            move || store.delete(&id, GrantRevocationReason::Deleted).unwrap()
        });
        // The revocation has taken the grant tables and now waits for the
        // admission to finish.
        awman_test_support::wait_for(
            "revocation to reach the grant",
            Duration::from_secs(5),
            || store.live.try_lock().is_err().then_some(()),
        );
        assert!(!watch.is_revoked(), "revocation waits for the admission");
        release_tx.send(()).unwrap();
        assert_eq!(admitting.join().unwrap(), Ok(false));
        assert!(revoking.join().unwrap());
        assert_eq!(watch.while_live(|| ()), Err(GrantRevocationReason::Deleted));
    }
}
