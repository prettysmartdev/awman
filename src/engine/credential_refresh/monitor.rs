//! The credential-refresh monitor: owns the lease registry and the tick loop
//! that keeps every live container's staged credential file fresh.
//!
//! Design priorities, in order:
//! 1. **Never write to a dead session's path.** Three independent defenses:
//!    the RAII lease (INV-6), a monotonic generation re-checked before every
//!    write, and a path-existence check inside the atomic writer (INV-7).
//! 2. **Never miss a live credentialed container.** Registration happens at the
//!    single spawn choke point, before the child process starts.
//! 3. **Survive every exit path.** The lease drops on success, error, panic
//!    unwind and teardown; the monitor loop parks (zero CPU) when no lease is
//!    live and never makes a refresh failure fatal to a session (INV-4).
//!
//! The loop runs on a dedicated background thread with its own current-thread
//! Tokio runtime so it depends on no ambient runtime; it is started lazily on
//! the first lease and parks when the registry empties.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use crate::data::fs::auth_paths::AuthPathResolver;
use crate::data::session::AgentName;
use crate::engine::auth::credential::{
    CredentialBinding, CredentialFile, CredentialFingerprint, CredentialReadError,
};
use crate::engine::auth::keychain::refreshable_spec_for;
use crate::engine::auth::RefreshableCredentialDelivery;
use crate::engine::ready::{HostAgentPinger, HostRefreshOutcome};

use super::lease::{CredentialLease, LeaseRegistry, RegistryShared};

/// Backoff is capped at `tick_interval * 2^MAX_BACKOFF_SHIFT` so a persistently
/// unreachable host does not push the retry interval to absurd lengths.
const MAX_BACKOFF_SHIFT: u32 = 6;

const REMEDIATION: &str = "run `claude` on the host / check login";

const STAGED_WRITE_REMEDIATION: &str =
    "staged credential rewrite failed; check disk/permissions — will retry";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorConfig {
    /// Refresh when `expires_at - now` drops below this. Default 20 min.
    pub refresh_threshold: Duration,
    /// Tick period while at least one lease is live. Default 60 s.
    pub tick_interval: Duration,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            refresh_threshold: Duration::from_secs(20 * 60),
            tick_interval: Duration::from_secs(60),
        }
    }
}

/// A refresh result. Contains no credential-derived identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// Credential was still comfortably valid; nothing done.
    NotNeeded { expires_in: Duration },
    /// Host rotated (or independently changed) and live leases were rewritten.
    Refreshed { leases_written: usize },
    /// Host ping ran but `expiresAt` did not advance. Last-known-good file is
    /// retained; `remediation` is user-facing text.
    Stale { remediation: String },
    /// The host credential could not be read at all.
    Unavailable { reason: CredentialReadError },
}

/// Per-agent health for status surfacing. Never carries a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshStatus {
    pub agent: AgentName,
    pub authority_id: uuid::Uuid,
    pub live_leases: usize,
    pub expires_at: Option<SystemTime>,
    pub last_outcome: Option<RefreshOutcome>,
    /// Consecutive failures; drives the retry backoff.
    pub consecutive_failures: u32,
}

#[derive(Default)]
struct AgentState {
    /// Fingerprint of the snapshot the staged files currently reflect. Change
    /// detection compares against this; a match means no rewrite is needed.
    last_materialized: Option<CredentialFingerprint>,
    last_outcome: Option<RefreshOutcome>,
    expires_at: Option<SystemTime>,
    consecutive_failures: u32,
    /// Tick-loop backoff gate. `None` = attempt now.
    retry_after: Option<Instant>,
}

/// Owns the lease registry and the tick loop.
///
/// The loop runs ONLY while the registry is non-empty: it is started by the
/// first `register()` and parks when the registry empties, so an `awman`
/// invocation that never launches an agent does zero polling.
pub struct CredentialRefreshMonitor {
    config: MonitorConfig,
    registry: LeaseRegistry,
    binding: CredentialBinding,
    lock_root: std::path::PathBuf,
    state: Mutex<HashMap<uuid::Uuid, AgentState>>,
    sources: Mutex<HashMap<uuid::Uuid, (AgentName, CredentialBinding)>>,
    authority_keys: Mutex<HashMap<String, uuid::Uuid>>,
    delivered: Mutex<HashMap<super::LeaseGeneration, CredentialFingerprint>>,
    /// Serializes host-refresh operations so the single-use refresh token is
    /// never raced by the tick loop and a concurrent `refresh_now` (INV-4).
    refresh_gate: tokio::sync::Mutex<()>,
    thread_started: std::sync::Once,
    /// The one handle permitted to execute an agent binary on the host
    /// (F-38). The tick loop's refresh is authorized trigger (b) in
    /// `security.md`.
    host_agent: HostAgentPinger,
}

impl CredentialRefreshMonitor {
    pub fn new(config: MonitorConfig) -> Arc<Self> {
        let resolver = AuthPathResolver::from_process_env()
            .unwrap_or_else(|_| AuthPathResolver::at_home(std::env::temp_dir()));
        Self::with_resolver(config, resolver)
    }

    /// Construct a monitor rooted at an explicit home directory, still reading
    /// whichever credential store the platform uses natively.
    ///
    /// NOTE for tests: on macOS the Claude descriptor reads the Keychain and
    /// ignores the home directory, so this alone does NOT isolate a test from
    /// the developer's real credential. Use [`Self::with_binding`] with
    /// [`CredentialBinding::to_file`] for an isolation that holds on every
    /// platform.
    pub fn with_resolver(config: MonitorConfig, resolver: AuthPathResolver) -> Arc<Self> {
        Self::with_binding_in(
            config,
            CredentialBinding::platform_default(resolver),
            &crate::engine::host_cli::process_env(),
        )
    }

    /// Construct a monitor bound to an explicit credential source, so the tick
    /// loop never reads a developer's real credential nor spawns a real host
    /// ping (F3) — on any platform.
    pub fn with_binding(config: MonitorConfig, binding: CredentialBinding) -> Arc<Self> {
        let root = binding
            .resolver()
            .home()
            .join(".cache/awman-credential-authorities");
        Self::with_lock_root(config, binding, root)
    }
    pub fn with_binding_in(
        config: MonitorConfig,
        binding: CredentialBinding,
        env: &crate::data::config::env::EnvSnapshot,
    ) -> Arc<Self> {
        let root = super::authority::lock_root(env, binding.resolver().home());
        Self::with_lock_root(config, binding, root)
    }
    fn with_lock_root(
        config: MonitorConfig,
        binding: CredentialBinding,
        lock_root: std::path::PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            lock_root,
            config,
            registry: LeaseRegistry::new(),
            binding,
            state: Mutex::new(HashMap::new()),
            sources: Mutex::new(HashMap::new()),
            authority_keys: Mutex::new(HashMap::new()),
            delivered: Mutex::new(HashMap::new()),
            refresh_gate: tokio::sync::Mutex::new(()),
            thread_started: std::sync::Once::new(),
            host_agent: HostAgentPinger::new(),
        })
    }

    /// Register a live credentialed container. Called from the container
    /// backends' `build()` — the single spawn choke point.
    pub fn register(
        self: &Arc<Self>,
        delivery: &RefreshableCredentialDelivery,
        container: &str,
    ) -> CredentialLease {
        // Seed change detection from the fingerprint the staged file was first
        // written with, so an unchanged first tick performs no rewrite.
        let authority = self.ensure_authority(&delivery.agent, self.binding.clone());
        self.seed_state(authority, delivery);
        let lease = self
            .registry
            .register_bound(delivery, container, authority, None);
        self.ensure_thread();
        tracing::debug!(
            agent = %delivery.agent,
            container = container,
            generation = lease.generation().as_u64(),
            "credential refresh: lease registered"
        );
        lease
    }

    /// Synchronous, bounded refresh — the §5 pre-step guard. Triggers the
    /// descriptor's host refresh, waits at most `timeout`, rewrites live leases
    /// if the snapshot changed, and reports what happened. NEVER fatal.
    pub async fn refresh_now(
        self: &Arc<Self>,
        agent: &AgentName,
        timeout: Duration,
    ) -> RefreshOutcome {
        let authority = self.ensure_authority(agent, self.binding.clone());
        match tokio::time::timeout(
            timeout,
            self.refresh_authority(authority, agent, &self.binding),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_elapsed) => {
                tracing::warn!(
                    agent = %agent,
                    remediation = REMEDIATION,
                    "credential refresh timed out; keeping last-known-good"
                );
                let expires_at = self.recorded_expiry(authority);
                let outcome = RefreshOutcome::Stale {
                    remediation: REMEDIATION.to_string(),
                };
                self.record_status(authority, expires_at, outcome.clone(), true);
                outcome
            }
        }
    }

    /// Per-agent health for status surfacing. Never returns a secret.
    pub fn status(&self) -> Vec<RefreshStatus> {
        let leases = self.registry.snapshot();
        let sources = self.sources.lock().unwrap_or_else(|p| p.into_inner());
        let states = self.state.lock().unwrap_or_else(|p| p.into_inner());
        sources
            .iter()
            .map(|(id, (agent, _))| {
                let st = states.get(id);
                RefreshStatus {
                    agent: agent.clone(),
                    authority_id: *id,
                    live_leases: leases.iter().filter(|l| l.authority == *id).count(),
                    expires_at: st.and_then(|s| s.expires_at),
                    last_outcome: st.and_then(|s| s.last_outcome.clone()),
                    consecutive_failures: st.map_or(0, |s| s.consecutive_failures),
                }
            })
            .collect()
    }

    pub fn live_lease_count(&self) -> usize {
        self.registry.len()
    }

    // ── internals ───────────────────────────────────────────────────────────

    fn seed_state(&self, authority: uuid::Uuid, delivery: &RefreshableCredentialDelivery) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let st = state.entry(authority).or_default();
        if st.last_materialized.is_none() {
            st.last_materialized = Some(delivery.initial_fingerprint);
        }
    }

    fn ensure_thread(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let shared = self.registry.shared();
        self.thread_started.call_once(move || {
            let spawn = std::thread::Builder::new()
                .name("awman-cred-refresh".to_string())
                .spawn(move || run_monitor_loop(weak, shared));
            if let Err(e) = spawn {
                tracing::warn!(error = %e, "credential refresh: could not start monitor thread");
            }
        });
    }

    /// One tick: refresh every agent that has live leases, honouring per-agent
    /// backoff so a persistently unreachable host is retried, not hammered.
    async fn tick(self: &Arc<Self>) {
        self.delivered
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|generation, _| self.registry.is_live(*generation));
        let mut sources = Vec::new();
        for lease in self.registry.snapshot() {
            if !sources.contains(&lease.authority) {
                sources.push(lease.authority);
            }
        }
        for authority in sources {
            if self.in_backoff(authority) {
                continue;
            }
            let source = self
                .sources
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&authority)
                .cloned();
            if let Some((agent, binding)) = source {
                if tokio::time::timeout(
                    self.config.tick_interval,
                    self.refresh_authority(authority, &agent, &binding),
                )
                .await
                .is_err()
                {
                    self.record_status(
                        authority,
                        self.recorded_expiry(authority),
                        RefreshOutcome::Stale {
                            remediation: REMEDIATION.into(),
                        },
                        true,
                    );
                }
            }
        }
    }

    /// Read the host credential, refresh the host if near expiry, and rewrite
    /// every live lease when the snapshot changed. Shared by the tick loop and
    /// `refresh_now`. Never returns an error: a failure is an outcome.
    async fn refresh_authority(
        self: &Arc<Self>,
        authority: uuid::Uuid,
        agent: &AgentName,
        binding: &CredentialBinding,
    ) -> RefreshOutcome {
        // Serialize host-refresh so parallel containers never race the
        // single-use refresh token, and the tick loop never collides with a
        // concurrent `refresh_now`.
        let _gate = self.refresh_gate.lock().await;

        let Some(spec) = refreshable_spec_for(agent) else {
            return RefreshOutcome::Unavailable {
                reason: CredentialReadError::Unsupported,
            };
        };
        let source = binding.source_for(spec);
        let lock = super::authority::lock_source_at(
            &self.lock_root,
            &source,
            spec.agent,
            Duration::from_secs(5),
        )
        .await;
        let _authority = match lock {
            Ok(lock) => Some(lock),
            // Explicit compatibility exception for legacy Windows delivery.
            // Managed slots still require cross-process serialization.
            Err("safe_authority_lock_unavailable")
                if cfg!(windows)
                    && !self
                        .registry
                        .snapshot()
                        .iter()
                        .any(|l| l.authority == authority && l.sink.is_some()) =>
            {
                tracing::warn!("legacy credential refresh uses process-local locking on Windows; cross-process locking unavailable");
                None
            }
            Err(reason) => {
                let outcome = RefreshOutcome::Unavailable {
                    reason: CredentialReadError::Unavailable(reason.into()),
                };
                self.record_status(
                    authority,
                    self.recorded_expiry(authority),
                    outcome.clone(),
                    true,
                );
                tracing::warn!(
                    reason,
                    "credential authority lock refused; keeping last-known-good"
                );
                return outcome;
            }
        };

        let snapshot = match (spec.read)(&source) {
            Ok(snapshot) => snapshot,
            Err(reason) => {
                tracing::warn!(
                    agent = %agent,
                    reason = %reason,
                    remediation = REMEDIATION,
                    "credential refresh: host credential unreadable; keeping last-known-good"
                );
                let outcome = RefreshOutcome::Unavailable {
                    reason: reason.clone(),
                };
                self.record_status(authority, None, outcome.clone(), true);
                return outcome;
            }
        };

        let now = SystemTime::now();
        let near_expiry = match (spec.expiry)(&snapshot) {
            Some(expiry) => {
                expiry <= now
                    || expiry
                        .duration_since(now)
                        .map(|remaining| remaining < self.config.refresh_threshold)
                        .unwrap_or(true)
            }
            None => false,
        };

        // If near expiry, ask the host to rotate and re-read. A host that
        // cannot rotate (asleep, keychain locked, offline, logged out) keeps
        // its last-known-good file — loudly, never silently.
        let mut stale_remediation: Option<String> = None;
        let snapshot = if near_expiry {
            match self.host_agent.refresh_credential(spec, binding).await {
                HostRefreshOutcome::Advanced { .. } => match (spec.read)(&source) {
                    Ok(fresh) => fresh,
                    Err(reason) => {
                        tracing::warn!(
                            agent = %agent,
                            reason = %reason,
                            "credential refresh: post-refresh read failed; keeping last-known-good"
                        );
                        let outcome = RefreshOutcome::Unavailable {
                            reason: reason.clone(),
                        };
                        self.record_status(authority, None, outcome.clone(), true);
                        return outcome;
                    }
                },
                HostRefreshOutcome::NotAdvanced { remediation } => {
                    tracing::warn!(
                        agent = %agent,
                        remediation = %remediation,
                        "credential refresh: host did not rotate; keeping last-known-good"
                    );
                    stale_remediation = Some(remediation);
                    snapshot
                }
                HostRefreshOutcome::PingFailed { result } => {
                    tracing::warn!(
                        agent = %agent,
                        ?result,
                        remediation = REMEDIATION,
                        "credential refresh: host refresh ping failed; keeping last-known-good"
                    );
                    stale_remediation = Some(REMEDIATION.to_string());
                    snapshot
                }
            }
        } else {
            snapshot
        };

        // Reconcile every live lease against the desired materialization each
        // tick. This rewrites a host rotation AND repairs any staged file a
        // container may have corrupted, even when the host fingerprint is
        // unchanged (INV-4 corollary, HIGH-5). Fingerprint compare — never the
        // secret.
        let fingerprint = CredentialFingerprint::of(&snapshot);
        let expiry_now = (spec.expiry)(&snapshot);
        let file = (spec.materialize)(&snapshot);
        let (leases_written, write_failures) =
            self.rewrite_live_leases(authority, agent, &file, &snapshot);
        if leases_written > 0 {
            tracing::info!(
                agent = %agent,
                leases_written,
                "credential refresh: staged credential rewritten"
            );
        }
        // Advance the host-change fingerprint only when every live lease is
        // current. A partial write failure must be retried on the next tick,
        // not permanently masked by a matching host fingerprint (HIGH-4).
        if write_failures == 0 {
            self.set_last_materialized(authority, fingerprint);
        }

        // A stale host is a failure (drives backoff) but never fatal.
        if let Some(remediation) = stale_remediation {
            let outcome = RefreshOutcome::Stale { remediation };
            self.record_status(authority, expiry_now, outcome.clone(), true);
            return outcome;
        }

        // A staged-write failure is likewise a loud, retried failure: it keeps
        // backoff/retry state so the affected container is brought current on a
        // later tick instead of silently stranded on the old token (HIGH-4).
        if write_failures > 0 {
            let outcome = RefreshOutcome::Stale {
                remediation: STAGED_WRITE_REMEDIATION.to_string(),
            };
            self.record_status(authority, expiry_now, outcome.clone(), true);
            return outcome;
        }

        let outcome = if leases_written > 0 {
            RefreshOutcome::Refreshed { leases_written }
        } else {
            let expires_in = expiry_now
                .and_then(|e| e.duration_since(now).ok())
                .unwrap_or(Duration::ZERO);
            RefreshOutcome::NotNeeded { expires_in }
        };
        self.record_status(authority, expiry_now, outcome.clone(), false);
        outcome
    }

    /// Reconcile `file` into every live lease of `agent`, returning
    /// `(written, failures)`.
    ///
    /// This runs every tick, not only when the host token changes, so a staged
    /// file a container may have truncated, replaced, or grown a `refreshToken`
    /// in is repaired even while the host fingerprint is unchanged (INV-4
    /// corollary, HIGH-5). A file whose bytes already equal `file` is left
    /// untouched (no write, no log noise). A target that has gone missing is a
    /// SKIP, never a recreate (INV-7 defense 3, HIGH-7): a missing file means
    /// the lease is racing its own drop.
    ///
    /// Each candidate re-checks the generation (defense 2) immediately before
    /// the write; a lease dropped since the snapshot loses the race safely. A
    /// write error warns, counts as a failure (so the fingerprint is NOT
    /// advanced and the write is retried next tick), and is never fatal.
    fn rewrite_live_leases(
        &self,
        authority: uuid::Uuid,
        agent: &AgentName,
        file: &CredentialFile,
        snapshot: &crate::engine::auth::credential::CredentialSnapshot,
    ) -> (usize, usize) {
        let mut written = 0;
        let mut failures = 0;
        for lease in self.registry.snapshot() {
            if &lease.agent != agent || lease.authority != authority {
                continue;
            }
            // Generation re-check immediately before the write: a lease dropped
            // since the snapshot loses the race safely.
            if !self.registry.is_live(lease.generation) {
                continue;
            }
            if let Some(redaction) = &lease.redaction {
                redaction.add(snapshot.secret.clone());
            }
            if let Some(sink) = &lease.sink {
                let fingerprint = CredentialFingerprint::of(snapshot);
                let delivered = self
                    .delivered
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&lease.generation)
                    .copied();
                if delivered != Some(fingerprint) {
                    if sink.0(Arc::new(snapshot.clone())) {
                        self.delivered
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .insert(lease.generation, fingerprint);
                        written += 1;
                    } else {
                        failures += 1;
                    }
                }
                continue;
            }
            // Skip a target that no longer exists — the monitor never recreates
            // a file a live lease's container may have deleted; that is treated
            // as a dropped-lease race, not repaired (INV-7 defense 3).
            match std::fs::read(&lease.staged_path) {
                Ok(existing) if existing == file.contents => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::debug!(
                        container = %lease.container,
                        "credential refresh: staged file gone; skipping (dropped-lease race)"
                    );
                    continue;
                }
                _ => {}
            }
            // Re-check liveness once more, immediately before the write, to
            // minimize the TOCTOU window between the snapshot and the rename
            // (HIGH-7). Combined with the monotonic, never-reused generation and
            // the process-retained staged TempDir (INV-7), a write that still
            // races a lease drop lands only in that lease's own throwaway dir,
            // which is never recycled for another session.
            if !self.registry.is_live(lease.generation) {
                continue;
            }
            match crate::engine::overlay::write_credential_file_atomic(&lease.staged_root, file) {
                Ok(true) => written += 1,
                Ok(false) => {
                    tracing::debug!(
                        container = %lease.container,
                        "credential refresh: staged path gone; skipping (dropped-lease race)"
                    );
                }
                Err(e) => {
                    failures += 1;
                    tracing::warn!(
                        container = %lease.container,
                        error = %e,
                        "credential refresh: staged rewrite failed; will retry next tick"
                    );
                }
            }
        }
        (written, failures)
    }

    fn set_last_materialized(&self, authority: uuid::Uuid, fingerprint: CredentialFingerprint) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.entry(authority).or_default().last_materialized = Some(fingerprint);
    }

    fn record_status(
        &self,
        authority: uuid::Uuid,
        expires_at: Option<SystemTime>,
        outcome: RefreshOutcome,
        failed: bool,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let st = state.entry(authority).or_default();
        st.expires_at = expires_at;
        st.last_outcome = Some(outcome);
        if failed {
            st.consecutive_failures = st.consecutive_failures.saturating_add(1);
            let shift = st.consecutive_failures.min(MAX_BACKOFF_SHIFT);
            let backoff = self.config.tick_interval.saturating_mul(1u32 << shift);
            st.retry_after = Some(Instant::now() + backoff);
        } else {
            st.consecutive_failures = 0;
            st.retry_after = None;
        }
    }

    fn in_backoff(&self, authority: uuid::Uuid) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&authority)
            .and_then(|s| s.retry_after)
            .map(|t| Instant::now() < t)
            .unwrap_or(false)
    }

    fn recorded_expiry(&self, authority: uuid::Uuid) -> Option<SystemTime> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&authority)
            .and_then(|s| s.expires_at)
    }
}

impl Drop for CredentialRefreshMonitor {
    fn drop(&mut self) {
        // Wake the parked background thread so it observes shutdown and exits.
        self.registry.request_shutdown();
    }
}

/// The tick loop body, run on the dedicated background thread. Holds only a
/// `Weak` to the monitor, so a monitor that is dropped (e.g. in tests) lets the
/// loop exit rather than leaking the thread.
fn run_monitor_loop(weak: Weak<CredentialRefreshMonitor>, shared: Arc<RegistryShared>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "credential refresh: monitor runtime build failed");
            return;
        }
    };
    loop {
        // Park (zero CPU) until at least one lease is live, or shutdown.
        if !shared.wait_until_active() {
            break;
        }
        let Some(monitor) = weak.upgrade() else {
            break;
        };
        let tick_interval = monitor.config.tick_interval;
        // A panic in a tick must not silently kill the refresh thread and strand
        // every live session unrefreshed (F2). Catch it, log, and continue; the
        // `state` mutex is accessed poison-tolerantly so a poisoned lock from an
        // earlier panic does not cascade into permanent tick death.
        let tick_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(monitor.tick());
        }));
        if tick_result.is_err() {
            tracing::error!("credential refresh: tick panicked; continuing next tick");
        }
        drop(monitor);
        if shared.sleep_or_shutdown(tick_interval) {
            break;
        }
    }
}

// WI 0114 F-38 deleted the process-global `OnceLock<Arc<CredentialRefreshMonitor>>`
// and its `install_global`/`global` pair. The monitor now travels as an
// explicit `Arc<dyn CredentialLeaseFactory>` on `ResolvedContainerOptions`;
// options built without one take no leases, which is exactly what the absent
// global used to mean — including for the `authRefresh.enabled: false` kill
// switch, which now simply passes no factory.

impl CredentialRefreshMonitor {
    fn ensure_authority(&self, agent: &AgentName, binding: CredentialBinding) -> uuid::Uuid {
        let Some(spec) = refreshable_spec_for(agent) else {
            return uuid::Uuid::nil();
        };
        let key = super::authority::source_key(&binding.source_for(spec), spec.agent);
        let id = *self
            .authority_keys
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(key)
            .or_insert_with(uuid::Uuid::new_v4);
        self.sources
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(id)
            .or_insert_with(|| (agent.clone(), binding));
        id
    }
    /// Bind a selected private source independently of the agent name. The
    /// callback reserves through ExecutionController and returns false while
    /// busy, so another profile's acceptance cannot mark this lease delivered.
    pub fn register_managed(
        self: &Arc<Self>,
        delivery: &RefreshableCredentialDelivery,
        container: &str,
        binding: CredentialBinding,
        authority: uuid::Uuid,
        sink: super::lease::SourceSink,
    ) -> Result<CredentialLease, &'static str> {
        // Refuse enrollment itself until this platform has a cross-process
        // primitive. A managed lease cannot join an already-running legacy
        // refresh after that refresh selected its compatibility exception.
        if cfg!(not(unix)) {
            return Err("safe_authority_lock_unavailable");
        }
        if authority.is_nil() {
            return Err("invalid_authority");
        }
        let spec = refreshable_spec_for(&delivery.agent).ok_or("native_capability_unavailable")?;
        let key = super::authority::source_key(&binding.source_for(spec), spec.agent);
        let mut keys = self
            .authority_keys
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if keys.get(&key).is_some_and(|id| *id != authority)
            || keys.iter().any(|(k, id)| k != &key && *id == authority)
        {
            return Err("authority_conflict");
        }
        keys.insert(key, authority);
        self.sources
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(authority)
            .or_insert_with(|| (delivery.agent.clone(), binding));
        drop(keys);
        self.seed_state(authority, delivery);
        let lease = self
            .registry
            .register_bound(delivery, container, authority, Some(sink));
        self.ensure_thread();
        Ok(lease)
    }
    pub async fn refresh_bound(
        self: &Arc<Self>,
        agent: &AgentName,
        binding: CredentialBinding,
        timeout: Duration,
    ) -> RefreshOutcome {
        let authority = self.ensure_authority(agent, binding.clone());
        tokio::time::timeout(timeout, self.refresh_authority(authority, agent, &binding))
            .await
            .unwrap_or_else(|_| RefreshOutcome::Stale {
                remediation: REMEDIATION.into(),
            })
    }
}

impl CredentialRefreshMonitor {
    /// Legacy file delivery keeps its behavior while every refreshed value is
    /// enrolled in the launch's existing streaming redactor before publication.
    pub fn register_redacted(
        self: &Arc<Self>,
        delivery: &RefreshableCredentialDelivery,
        container: &str,
        redaction: crate::engine::auth::redaction::RedactionSet,
    ) -> CredentialLease {
        let authority = self.ensure_authority(&delivery.agent, self.binding.clone());
        self.seed_state(authority, delivery);
        let lease =
            self.registry
                .register_protected(delivery, container, authority, None, Some(redaction));
        self.ensure_thread();
        lease
    }
}
