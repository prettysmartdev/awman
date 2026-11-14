//! Browser pairing policy (WI 0159, Layer 2): who may create, poll and
//! approve a browser link, what a grant may reach, and how grants end.
//!
//! The credential mechanics (codes, secrets, hashing, one-use tables) are
//! Layer 1 (`engine::auth::{device_flow, browser_links, browser_grants}`);
//! the HTTP adapter is `frontend::api::browser`. Every decision below is made
//! from the listener context the router's gate established
//! ([`BrowserContext`]: exact allowed origin + receiving listener), never
//! from anything the browser puts in a body.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::DaemonApi;
use crate::command::daemon_runtime::network::{BrowserRevocation, BrowserRevocationSink};
use crate::data::browser_auth::{
    BrowserGrantIssued, BrowserGrantView, BrowserLinkCreateResponse, BrowserLinkDecisionResponse,
    BrowserLinkPollResponse, BrowserLinkStatus, BrowserLinkView, BrowserReauthChallengeRequest,
    BrowserReauthChallengeResponse, BrowserReauthCompleteRequest, BrowserReauthCompleteResponse,
    BrowserReauthEnrollment, BrowserScope, BrowserTicketRequest, BrowserTicketResponse,
    GrantRevocationReason, TicketRoute, FAILURE_LOCKOUT, GRANT_WATCH_RECHECK, LINK_CODE_PREFIX,
    LINK_CREATES_PER_MINUTE, LINK_POLL_INTERVAL, LOOKUP_FAILURES_PER_MINUTE,
    POLL_FAILURES_PER_MINUTE, REAUTH_ALGORITHM, REAUTH_CHALLENGES_FULL_RETRY,
    REAUTH_CHALLENGES_PER_MINUTE, REAUTH_COMPLETES_PER_MINUTE, REAUTH_SLOTS_FULL_RETRY,
    REAUTH_SLOTS_PER_LISTENER, REAUTH_VERSION, TICKET_TTL,
};
use crate::data::config::network::ListenerTransport;
use crate::data::daemon_state::{FeatureName, Principal};
use crate::data::ids::{BrowserGrantId, BrowserLinkId, InstanceId};
use crate::data::terminal::{TerminalOwner, TerminalRecord};
use crate::data::wire::List;
use crate::engine::auth::browser_grants::{
    BrowserGrantStore, GrantVerdict, GrantWatch, OperationTime, Revoked, SamplePoint,
};
use crate::engine::auth::browser_links::{
    BrowserLinkTable, CollectOutcome, DecideError, LinkCreateError, PollOutcome, TicketBinding,
    TicketBindingRef, UpgradeTicketTable,
};
use crate::engine::auth::browser_reauth::{
    is_id, new_epoch, transcript, Binding, BindingState, Challenge, ChallengeIssueError,
    ChallengeRequest, ChallengeTable, ReauthBindings, ReauthPublicKey, ReauthSignature,
    CREDENTIAL_ID_BYTES, EPOCH_BYTES, NONCE_BYTES,
};
use crate::engine::auth::device_flow::{RateLimiter, ShortCode};
use crate::engine::error::EngineError;

const MINUTE: Duration = Duration::from_secs(60);

/// The browser request context established by the listener gate: the exact
/// allowed `Origin` and the receiving listener's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserContext {
    pub origin: String,
    pub listener: String,
}

/// The authenticated browser behind a request (the `Principal::Browser`
/// fields, typed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserPrincipal {
    pub grant_id: BrowserGrantId,
    pub origin: String,
    pub listener: String,
}

impl BrowserPrincipal {
    /// From the request's `Principal`, when it is a browser.
    pub fn from_principal(principal: &Principal) -> Option<Self> {
        match principal {
            Principal::Browser {
                grant_id,
                origin,
                listener,
            } => Some(Self {
                grant_id: BrowserGrantId::parse(grant_id).ok()?,
                origin: origin.clone(),
                listener: listener.clone(),
            }),
            _ => None,
        }
    }
    pub fn principal(&self) -> Principal {
        Principal::Browser {
            grant_id: self.grant_id.to_string(),
            origin: self.origin.clone(),
            listener: self.listener.clone(),
        }
    }
    fn context(&self) -> BrowserContext {
        BrowserContext {
            origin: self.origin.clone(),
            listener: self.listener.clone(),
        }
    }
}

/// Why a browser operation was refused. The adapter maps each variant to
/// one stable API error code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrowserError {
    /// Route not available here (`not_found`).
    #[error("resource not found")]
    NotFound,
    /// Poll of an unknown/consumed/expired link or with a wrong secret,
    /// origin or listener (`410 link_gone`).
    #[error("browser link is gone")]
    LinkGone,
    /// Owner lookup/decision of an unknown or expired link
    /// (`404 link_not_found`).
    #[error("browser link not found or expired")]
    LinkNotFound,
    /// `409 conflict`.
    #[error("browser link was already decided")]
    AlreadyDecided,
    /// `429 rate_limited` with `Retry-After`.
    #[error("too many requests; retry later")]
    RateLimited(Duration),
    /// `404 grant_not_found`.
    #[error("browser grant not found")]
    GrantNotFound,
    /// The ticket's target is not visible to this grant (`404`).
    #[error("{0}")]
    ResourceNotFound(&'static str),
    #[error("{0}")]
    Internal(String),
    // WI 0160 reauthorization. Each maps to exactly one stable code.
    /// `400 invalid_request`: malformed DTO, key, encoding or version.
    #[error("Invalid browser reauthorization request.")]
    InvalidReauth,
    /// `409 credential_exists`.
    #[error("Browser credential ID is already registered.")]
    CredentialExists,
    /// `409 daemon_restarted`.
    #[error("Daemon generation changed.")]
    DaemonRestarted,
    /// `409 listener_changed`.
    #[error("Listener generation changed.")]
    ListenerChanged,
    /// `401 reauth_failed`: deliberately one variant for unknown/consumed/
    /// expired challenge, wrong key, missing binding, wrong origin or
    /// listener and unapproved or uncollected-timeout candidates.
    #[error("Browser reauthorization proof was not accepted.")]
    ReauthFailed,
    /// `403 authorization_expired`, only after a valid proof.
    #[error("Browser authorization expired.")]
    AuthorizationExpired,
    /// `403 authorization_revoked`, only after a valid proof.
    #[error("Browser authorization was revoked.")]
    AuthorizationRevoked,
    /// `429 rate_limited` on the reauthorization routes, with `Retry-After`.
    #[error("Too many browser reauthorization requests.")]
    ReauthRateLimited(Duration),
}

impl From<EngineError> for BrowserError {
    fn from(e: EngineError) -> Self {
        Self::Internal(e.to_string())
    }
}

/// The daemon's browser authority: grants, pending links, upgrade tickets,
/// rate limits and which instances each grant created. One per daemon,
/// owned by `DaemonRuntime`.
pub struct BrowserAuth {
    grants: Arc<BrowserGrantStore>,
    links: BrowserLinkTable,
    tickets: UpgradeTicketTable,
    creates: RateLimiter,
    poll_failures: RateLimiter,
    lookup_failures: RateLimiter,
    created_instances: Mutex<HashMap<InstanceId, BrowserGrantId>>,
    // WI 0160 reauthorization.
    /// Fresh from the CSPRNG for every process; never persisted.
    daemon_epoch: String,
    /// The serialization boundary for key-bound grants: link approval, poll
    /// collection, proof release, revocation, expiry and listener
    /// generation changes all happen under this lock. Lock order: this,
    /// then the link/ticket tables and the grant store, never the reverse.
    reauth: Mutex<ReauthState>,
    challenges: ChallengeTable,
    challenge_rate: RateLimiter,
    complete_rate: RateLimiter,
}

/// Listener generations and key bindings, guarded together.
#[derive(Debug, Default)]
struct ReauthState {
    /// Generation of each listener's grant set. Created on first use and
    /// replaced whenever every grant of the listener is revoked.
    listener_epochs: HashMap<String, String>,
    bindings: ReauthBindings,
}

impl ReauthState {
    fn listener_epoch(&mut self, listener: &str) -> String {
        self.listener_epochs
            .entry(listener.to_owned())
            .or_insert_with(|| new_epoch().expect("the OS CSPRNG is available"))
            .clone()
    }
}

impl std::fmt::Debug for BrowserAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserAuth").finish_non_exhaustive()
    }
}

impl BrowserAuth {
    pub fn new(grants: Arc<BrowserGrantStore>) -> Arc<Self> {
        Arc::new(Self {
            grants,
            links: BrowserLinkTable::new(),
            tickets: UpgradeTicketTable::new(),
            creates: RateLimiter::new(LINK_CREATES_PER_MINUTE, MINUTE, MINUTE),
            poll_failures: RateLimiter::new(POLL_FAILURES_PER_MINUTE, MINUTE, FAILURE_LOCKOUT),
            lookup_failures: RateLimiter::new(LOOKUP_FAILURES_PER_MINUTE, MINUTE, FAILURE_LOCKOUT),
            created_instances: Mutex::new(HashMap::new()),
            daemon_epoch: new_epoch().expect("the OS CSPRNG is available"),
            reauth: Mutex::new(ReauthState::default()),
            challenges: ChallengeTable::new(),
            challenge_rate: RateLimiter::new(REAUTH_CHALLENGES_PER_MINUTE, MINUTE, MINUTE),
            complete_rate: RateLimiter::new(REAUTH_COMPLETES_PER_MINUTE, MINUTE, MINUTE),
        })
    }

    /// WI 0160: this process's daemon epoch.
    pub fn daemon_epoch(&self) -> &str {
        &self.daemon_epoch
    }

    /// WI 0160: the current generation of `listener`'s grant set.
    pub fn listener_epoch(&self, listener: &str) -> String {
        self.reauth_state().listener_epoch(listener)
    }

    /// Outstanding reauthorization challenges (tests and diagnostics).
    pub fn outstanding_challenges(&self) -> usize {
        self.challenges.len()
    }

    /// Live upgrade tickets of `grant` (tests and diagnostics).
    pub fn tickets_for_grant(&self, grant: &BrowserGrantId) -> usize {
        self.tickets.count_for_grant(grant)
    }

    fn reauth_state(&self) -> std::sync::MutexGuard<'_, ReauthState> {
        self.reauth.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// End one grant everywhere: disk/memory record (closing sockets), its
    /// tickets and its binding (tombstone per [`ReauthBindings::end_grant`]).
    /// Caller holds the reauth lock.
    fn end_grant_locked(
        &self,
        state: &mut ReauthState,
        grant: &BrowserGrantId,
        reason: GrantRevocationReason,
    ) -> Result<bool, EngineError> {
        let deleted = self.grants.delete(grant, reason);
        self.tickets.purge_grant(grant);
        state.bindings.end_grant(grant, reason);
        deleted
    }

    /// Finish ending every grant whose expiry was observed anywhere (bearer
    /// check, ticket, watch, proof, admission or the periodic sweep): the
    /// grant record, its tickets, its retained bearer and its instance
    /// ownership go; an `Expired` tombstone stays. Caller holds the reauth
    /// lock.
    fn reap_expired_locked(&self, state: &mut ReauthState) {
        let expired = self.grants.take_expired();
        if expired.is_empty() {
            return;
        }
        for grant in &expired {
            if let Err(e) = self.end_grant_locked(state, grant, GrantRevocationReason::Expired) {
                tracing::warn!(error = %e, "browser grant expiry was not persisted");
            }
        }
        self.created_instances
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|_, g| !expired.contains(g));
    }

    /// [`Self::reap_expired_locked`] under the reauth lock. The daemon runs
    /// it every [`GRANT_WATCH_RECHECK`], so cleanup never depends on new
    /// requests.
    pub fn reap_expired(&self) {
        let mut state = self.reauth_state();
        self.reap_expired_locked(&mut state);
    }

    /// Run [`Self::reap_expired`] every recheck interval of the grant clock
    /// until `stop` (one task per daemon).
    pub fn spawn_expiry_sweeper(self: &Arc<Self>, stop: tokio_util::sync::CancellationToken) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(clock) = weak.upgrade().map(|auth| auth.grants.clock()) else {
                    return;
                };
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = clock.sleep(GRANT_WATCH_RECHECK) => {}
                }
                let Some(auth) = weak.upgrade() else {
                    return;
                };
                auth.reap_expired();
            }
        });
    }

    /// The grant clock's current time.
    fn clock_now(&self) -> (DateTime<Utc>, Instant) {
        self.grants.clock().sample(SamplePoint::Ordinary)
    }

    /// A new generation for `listener`: old challenges and bindings are
    /// gone, so stale proofs answer `409 listener_changed`.
    fn rotate_listener_locked(&self, state: &mut ReauthState, listener: &str) {
        state.listener_epochs.insert(
            listener.to_owned(),
            new_epoch().expect("the OS CSPRNG is available"),
        );
        state.bindings.remove_listener(listener);
        self.challenges.purge_listener(listener);
    }

    /// Open the store at `path` (`DaemonPaths::browser_grants_file()`).
    pub fn open(path: std::path::PathBuf) -> Result<Arc<Self>, EngineError> {
        Ok(Self::new(BrowserGrantStore::open(path)?))
    }

    pub fn grants(&self) -> &Arc<BrowserGrantStore> {
        &self.grants
    }

    /// Live grants bound to `listener` (`daemon status`).
    pub fn count_for_listener(&self, listener: &str) -> usize {
        self.grants.count_for_listener(listener, self.clock_now().0)
    }

    /// Check a browser request's bearer against this listener's grants only.
    /// API keys and work-server tokens are never consulted (D1).
    pub fn authenticate(
        &self,
        authorization: Option<&str>,
        ctx: &BrowserContext,
    ) -> Result<BrowserPrincipal, GrantVerdict> {
        match self.grants.verify(
            authorization,
            &ctx.origin,
            &ctx.listener,
            self.clock_now().0,
        ) {
            GrantVerdict::Accepted { id } => Ok(BrowserPrincipal {
                grant_id: id,
                origin: ctx.origin.clone(),
                listener: ctx.listener.clone(),
            }),
            other => {
                // A refusal may have observed an expiry; finish it now.
                self.reap_expired();
                Err(other)
            }
        }
    }

    /// Consume an upgrade ticket for `route`/`resource`. The ticket is gone
    /// after this call whatever the result; the grant must still be live.
    pub fn consume_ticket(
        &self,
        ticket: &str,
        route: TicketRoute,
        resource: &str,
        ctx: &BrowserContext,
    ) -> Option<BrowserPrincipal> {
        let (now, _) = self.clock_now();
        let grant = self.tickets.consume(
            ticket,
            &TicketBindingRef {
                route,
                resource,
                origin: &ctx.origin,
                listener: &ctx.listener,
            },
            now,
        )?;
        if !self
            .grants
            .is_valid(&grant, &ctx.origin, &ctx.listener, now)
        {
            self.reap_expired();
            return None;
        }
        Some(BrowserPrincipal {
            grant_id: grant,
            origin: ctx.origin.clone(),
            listener: ctx.listener.clone(),
        })
    }

    /// The grant's live authority; sockets close with 1008 when it ends and
    /// admission checks it last. `None` once the grant has ended.
    pub fn watch(&self, grant: &BrowserGrantId) -> Option<GrantWatch> {
        let watch = self.grants.watch(grant);
        if watch.is_none() {
            self.reap_expired();
        }
        watch
    }

    /// Revoke everything matching `pred` with `reason`: grants (closing their
    /// sockets), their tickets, and pending links on the same terms. The
    /// affected IDs are reported even when persisting failed; the in-memory
    /// cleanup and tombstones are complete either way.
    pub fn revoke_where(
        &self,
        pred: impl Fn(&str, &str) -> bool,
        reason: GrantRevocationReason,
    ) -> Revoked {
        let mut state = self.reauth_state();
        self.revoke_where_locked(&mut state, pred, reason)
    }

    fn revoke_where_locked(
        &self,
        state: &mut ReauthState,
        pred: impl Fn(&str, &str) -> bool,
        reason: GrantRevocationReason,
    ) -> Revoked {
        self.reap_expired_locked(state);
        let mut ids = self.links.remove_where(&pred);
        let revoked = self
            .grants
            .revoke_where(|g| pred(&g.origin, &g.listener), reason);
        ids.extend(revoked.ids);
        for id in &ids {
            // Already refused in memory; a second write error adds nothing.
            let _ = self.end_grant_locked(state, id, reason);
        }
        state.bindings.remove_unapproved_where(&pred);
        self.created_instances
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|_, g| !ids.contains(g));
        if let Err(e) = &revoked.persisted {
            tracing::warn!(error = %e, "browser grant revocation was not persisted");
        }
        Revoked {
            ids,
            persisted: revoked.persisted,
        }
    }

    /// Daemon stop: every grant ends, and every binding and challenge with it.
    pub fn revoke_all(&self, reason: GrantRevocationReason) {
        let mut state = self.reauth_state();
        self.revoke_where_locked(&mut state, |_, _| true, reason);
        state.bindings.clear();
        state.listener_epochs.clear();
        drop(state);
        self.challenges.purge_all();
    }

    /// Every grant of `listener` ends and its generation changes (listener
    /// disabled, removed, rebound, transport changed or `engine` removed).
    fn revoke_listener(&self, listener: &str) -> Revoked {
        let mut state = self.reauth_state();
        let revoked = self.revoke_where_locked(
            &mut state,
            |_, l| l == listener,
            GrantRevocationReason::ListenerClosed,
        );
        self.rotate_listener_locked(&mut state, listener);
        revoked
    }

    /// Remember that `grant` created `instance` (resource ownership).
    pub fn record_instance(&self, grant: &BrowserGrantId, instance: &InstanceId) {
        self.created_instances
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(instance.clone(), grant.clone());
    }

    fn created_by(&self, instance: &InstanceId, grant: &BrowserGrantId) -> bool {
        self.created_instances
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(instance)
            == Some(grant)
    }

    /// Approved links nobody collected end with their link; their bindings
    /// are forgotten (a later proof is a generic failure, not an expiry).
    /// Runs under the reauth lock so a proof can never observe the link gone
    /// while the grant still lives.
    fn sweep(&self, now: DateTime<Utc>) {
        let mut state = self.reauth_state();
        self.reap_expired_locked(&mut state);
        for orphan in self.links.sweep_expired(now) {
            self.end_orphan_locked(&mut state, &orphan);
        }
        state.bindings.prune(now);
    }

    /// An approval nobody collected before its link deadline ends; its
    /// binding is forgotten, so a later proof is a generic failure.
    fn end_orphan_locked(&self, state: &mut ReauthState, grant: &BrowserGrantId) {
        let _ = self.grants.delete(grant, GrantRevocationReason::Expired);
        self.tickets.purge_grant(grant);
        state.bindings.forget_grant(grant);
    }
}

/// Grant revocation hooks for live config reloads (origin removed, listener
/// disabled/removed/rebound, engine no longer served).
impl BrowserRevocationSink for BrowserAuth {
    fn revoke(&self, listener: &str, reason: BrowserRevocation) {
        let revoked = match reason {
            BrowserRevocation::OriginRemoved(origin) => {
                let origin = origin.as_str().to_owned();
                self.revoke_where(
                    |o, l| l == listener && o == origin,
                    GrantRevocationReason::OriginRemoved,
                )
            }
            BrowserRevocation::Disabled
            | BrowserRevocation::EngineRemoved
            | BrowserRevocation::TransportChanged
            | BrowserRevocation::ListenerRemoved
            | BrowserRevocation::Rebound => self.revoke_listener(listener),
        };
        if !revoked.ids.is_empty() {
            tracing::info!(
                listener,
                grants = revoked.ids.len(),
                "browser grants revoked by configuration change"
            );
        }
    }
}

fn browser_auth(api: &DaemonApi) -> &BrowserAuth {
    api.runtime.browser_auth()
}

/// Browser links are offered only where a browser can reach the engine:
/// an enabled `loopback_http` listener that serves `engine` (§2.4).
fn eligible(api: &DaemonApi, ctx: &BrowserContext) -> bool {
    api.runtime
        .listener_settings(&ctx.listener)
        .is_some_and(|l| {
            l.enabled
                && l.transport == ListenerTransport::LoopbackHttp
                && l.allowed_origins.iter().any(|o| o.as_str() == ctx.origin)
        })
        && api
            .runtime
            .listener_serves(&ctx.listener, FeatureName::Engine)
}

fn limiter_key(ctx: &BrowserContext) -> String {
    format!("{}\n{}", ctx.listener, ctx.origin)
}

/// `POST /v1/browser-links`: start pairing. Scope is always `engine`.
pub fn links_create(
    api: &DaemonApi,
    ctx: &BrowserContext,
) -> Result<BrowserLinkCreateResponse, BrowserError> {
    let (now, instant) = browser_auth(api).clock_now();
    links_create_at(api, ctx, None, now, instant)
}

/// WI 0160: `POST /v1/browser-links` with `{"reauth":{…}}`: a link whose
/// approval binds this public key. `None` is the legacy keyless link.
pub fn links_create_with(
    api: &DaemonApi,
    ctx: &BrowserContext,
    enrollment: Option<&BrowserReauthEnrollment>,
) -> Result<BrowserLinkCreateResponse, BrowserError> {
    let (now, instant) = browser_auth(api).clock_now();
    links_create_at(api, ctx, enrollment, now, instant)
}

/// A validated enrollment: the credential ID and parsed key.
fn validate_enrollment(
    enrollment: &BrowserReauthEnrollment,
) -> Result<(String, ReauthPublicKey), BrowserError> {
    if enrollment.version != REAUTH_VERSION
        || enrollment.algorithm != REAUTH_ALGORITHM
        || !is_id(&enrollment.credential_id, CREDENTIAL_ID_BYTES)
    {
        return Err(BrowserError::InvalidReauth);
    }
    let key = ReauthPublicKey::parse(&enrollment.public_key).ok_or(BrowserError::InvalidReauth)?;
    Ok((enrollment.credential_id.clone(), key))
}

pub(crate) fn links_create_at(
    api: &DaemonApi,
    ctx: &BrowserContext,
    enrollment: Option<&BrowserReauthEnrollment>,
    now: DateTime<Utc>,
    instant: Instant,
) -> Result<BrowserLinkCreateResponse, BrowserError> {
    if !eligible(api, ctx) {
        return Err(BrowserError::NotFound);
    }
    let enrollment = enrollment.map(validate_enrollment).transpose()?;
    let auth = browser_auth(api);
    auth.sweep(now);
    auth.creates
        .try_acquire(&limiter_key(ctx), instant)
        .map_err(BrowserError::RateLimited)?;
    // Held across link creation so a concurrent enrollment cannot reserve
    // the same credential ID and a listener change cannot interleave.
    let mut state = auth.reauth_state();
    if let Some((credential_id, _)) = &enrollment {
        if state.bindings.contains(credential_id) {
            return Err(BrowserError::CredentialExists);
        }
        if state.bindings.count_for_listener(&ctx.listener) >= REAUTH_SLOTS_PER_LISTENER {
            return Err(BrowserError::RateLimited(REAUTH_SLOTS_FULL_RETRY));
        }
    }
    let created = auth
        .links
        .create(&ctx.origin, &ctx.listener, BrowserScope::Engine, now)
        .map_err(|e| match e {
            LinkCreateError::TooManyPending => BrowserError::RateLimited(MINUTE),
            LinkCreateError::Engine(e) => BrowserError::Internal(e),
        })?;
    let reauth = enrollment.map(|(credential_id, key)| {
        let binding = Binding {
            credential_id,
            key,
            origin: ctx.origin.clone(),
            listener: ctx.listener.clone(),
            listener_epoch: state.listener_epoch(&ctx.listener),
            link_expires_at: created.view.expires_at,
            state: BindingState::Pending {
                link: created.view.id.clone(),
            },
        };
        let public = binding.public_binding(&auth.daemon_epoch);
        state.bindings.insert(binding);
        public
    });
    drop(state);
    tracing::info!(
        link = %created.view.id,
        listener = %ctx.listener,
        origin = %ctx.origin,
        key_bound = reauth.is_some(),
        "browser link requested"
    );
    Ok(BrowserLinkCreateResponse {
        id: created.view.id,
        code: created.view.code,
        polling_secret: created.polling_secret,
        interval_ms: LINK_POLL_INTERVAL.as_millis() as u64,
        expires_at: created.view.expires_at,
        listener: created.view.listener,
        origin: created.view.origin,
        scope: created.view.scope,
        reauth,
    })
}

/// `POST /v1/browser-links/{id}/poll`.
pub fn links_poll(
    api: &DaemonApi,
    ctx: &BrowserContext,
    id: &str,
    secret: &str,
) -> Result<BrowserLinkPollResponse, BrowserError> {
    let (now, instant) = browser_auth(api).clock_now();
    links_poll_at(api, ctx, id, secret, now, instant)
}

/// [`links_poll`] from the caller's (`now`, `instant`). Collection and the
/// final grant check happen together under the reauth lock, at time
/// sampled there, so a grant that ended (or a deadline crossed) while the
/// request waited is never released.
pub fn links_poll_at(
    api: &DaemonApi,
    ctx: &BrowserContext,
    id: &str,
    secret: &str,
    now: DateTime<Utc>,
    instant: Instant,
) -> Result<BrowserLinkPollResponse, BrowserError> {
    let auth = browser_auth(api);
    let time = OperationTime::start(auth.grants.clock(), now, instant);
    let key = limiter_key(ctx);
    if let Some(wait) = auth.poll_failures.locked(&key, instant) {
        return Err(BrowserError::RateLimited(wait));
    }
    let fail = || {
        auth.poll_failures.record_failure(&key, instant);
        BrowserError::LinkGone
    };
    let Ok(id) = BrowserLinkId::parse(id) else {
        return Err(fail());
    };
    let mut state = auth.reauth_state();
    auth.reap_expired_locked(&mut state);
    let (now, instant) = time.sample(SamplePoint::PollRelease);
    match auth
        .links
        .poll(&id, secret, &ctx.origin, &ctx.listener, now)
    {
        PollOutcome::Pending => Ok(BrowserLinkPollResponse::Pending {
            retry_after_ms: LINK_POLL_INTERVAL.as_millis() as u64,
        }),
        PollOutcome::Approved(grant) => {
            if let Some(reason) = auth.grants.ended_at(&grant.id, now, instant) {
                // Ended before release: the link is consumed, nothing is
                // returned, and an expiry is finished here.
                if reason == GrantRevocationReason::Expired {
                    let _ = auth.end_grant_locked(&mut state, &grant.id, reason);
                }
                tracing::debug!(link = %id, "browser link collected after its grant ended");
                return Err(BrowserError::LinkGone);
            }
            if grant.reauth.is_some() {
                state.bindings.mark_collected(&grant.id);
            }
            tracing::info!(link = %id, grant = %grant.id, "browser link collected");
            Ok(BrowserLinkPollResponse::Approved { grant })
        }
        PollOutcome::Denied => Ok(BrowserLinkPollResponse::Denied),
        PollOutcome::TooFast(wait) => Err(BrowserError::RateLimited(wait)),
        PollOutcome::Gone => {
            drop(state);
            Err(fail())
        }
    }
}

/// `GET /v1/browser-links?code=web-…` (owner socket): the pending request
/// `awman link` shows before asking for confirmation.
pub fn links_lookup(api: &DaemonApi, code: &str) -> Result<BrowserLinkView, BrowserError> {
    let (now, instant) = browser_auth(api).clock_now();
    links_lookup_at(api, code, now, instant)
}

pub(crate) fn links_lookup_at(
    api: &DaemonApi,
    code: &str,
    now: DateTime<Utc>,
    instant: Instant,
) -> Result<BrowserLinkView, BrowserError> {
    let auth = browser_auth(api);
    if let Some(wait) = auth.lookup_failures.locked("owner", instant) {
        return Err(BrowserError::RateLimited(wait));
    }
    let found = ShortCode::parse(code, LINK_CODE_PREFIX)
        .and_then(|code| auth.links.lookup_by_code(&code, now))
        .map(|mut view| {
            view.reauth = auth
                .reauth_state()
                .bindings
                .by_link(&view.id)
                .map(Binding::key_view);
            view
        });
    found.ok_or_else(|| {
        auth.lookup_failures.record_failure("owner", instant);
        BrowserError::LinkNotFound
    })
}

/// `POST /v1/browser-links/{id}/decision` (owner socket). Approval creates
/// the grant before answering; its token is released only to the poller.
pub fn links_decide(
    api: &DaemonApi,
    id: &str,
    approve: bool,
) -> Result<BrowserLinkDecisionResponse, BrowserError> {
    let Ok(id) = BrowserLinkId::parse(id) else {
        return Err(BrowserError::LinkNotFound);
    };
    let auth = browser_auth(api);
    let (now, _) = auth.clock_now();
    // Approval binds the enrolled key before any credential is released.
    let mut state = auth.reauth_state();
    // Earlier grants' observed expiries end through the shared lifecycle
    // before another grant is minted, so their tombstones survive.
    auth.reap_expired_locked(&mut state);
    let public = state
        .bindings
        .by_link(&id)
        .map(|b| b.public_binding(&auth.daemon_epoch));
    let mut minted: Option<BrowserGrantIssued> = None;
    let grant_id = auth
        .links
        .decide(&id, approve, now, |link| {
            let (record, token) = auth
                .grants
                .create(&link.origin, &link.listener, link.scope, None, now)
                .map_err(|e| e.to_string())?;
            let issued = BrowserGrantIssued {
                id: record.id,
                token,
                scope: record.scope,
                origin: record.origin,
                listener: record.listener,
                expires_at: record.expires_at,
                reauth: public,
            };
            minted = Some(issued.clone());
            Ok(issued)
        })
        .map_err(|e| match e {
            DecideError::NotFound => BrowserError::LinkNotFound,
            DecideError::AlreadyDecided => BrowserError::AlreadyDecided,
            DecideError::Grant(e) => BrowserError::Internal(e),
        })?;
    match minted {
        Some(issued) if issued.reauth.is_some() => {
            state
                .bindings
                .approve(&id, issued.id, issued.token, issued.expires_at);
        }
        Some(_) => {}
        None => state.bindings.remove_pending_link(&id),
    }
    drop(state);
    tracing::info!(link = %id, approved = approve, "browser link decided by owner");
    Ok(BrowserLinkDecisionResponse {
        status: if approve {
            BrowserLinkStatus::Approved
        } else {
            BrowserLinkStatus::Denied
        },
        grant_id,
    })
}

/// `GET /v1/browsers` (owner socket): live grants, never hash or token.
pub fn browsers_list(api: &DaemonApi) -> List<BrowserGrantView> {
    let auth = browser_auth(api);
    auth.reap_expired();
    let grants = auth.grants.list(auth.clock_now().0);
    let state = auth.reauth_state();
    List {
        items: grants
            .into_iter()
            .map(|g| BrowserGrantView {
                reauth: state.bindings.by_grant(&g.id).map(Binding::key_view),
                id: g.id,
                origin: g.origin,
                listener: g.listener,
                scope: g.scope,
                created_at: g.created_at,
                expires_at: g.expires_at,
                last_used_at: g.last_used_at,
                label: g.label,
            })
            .collect(),
        next_cursor: None,
    }
}

/// `DELETE /v1/browsers/{id}` (owner socket): revoke at once; live sockets
/// close with 1008 `grant revoked`, and the key can no longer recover it.
pub fn browsers_delete(api: &DaemonApi, id: &str) -> Result<(), BrowserError> {
    let id = BrowserGrantId::parse(id).map_err(|_| BrowserError::GrantNotFound)?;
    let auth = browser_auth(api);
    let mut state = auth.reauth_state();
    let deleted = auth.end_grant_locked(&mut state, &id, GrantRevocationReason::Deleted);
    drop(state);
    match deleted {
        Ok(true) => {
            tracing::info!(grant = %id, "browser grant revoked by owner");
            Ok(())
        }
        Ok(false) => Err(BrowserError::GrantNotFound),
        // Already refused in memory; report the persistence failure.
        Err(e) => Err(BrowserError::Internal(e.to_string())),
    }
}

/// WI 0160 `POST /v1/browser-reauth/challenge`: a one-use challenge for a
/// presented credential ID. Epoch mismatches answer before any lookup
/// (daemon first); otherwise known, unknown, expired and revoked IDs all get
/// the same response, which carries no grant state.
pub fn reauth_challenge(
    api: &DaemonApi,
    ctx: &BrowserContext,
    request: &BrowserReauthChallengeRequest,
) -> Result<BrowserReauthChallengeResponse, BrowserError> {
    let (now, instant) = browser_auth(api).clock_now();
    reauth_challenge_at(api, ctx, request, now, instant)
}

pub fn reauth_challenge_at(
    api: &DaemonApi,
    ctx: &BrowserContext,
    request: &BrowserReauthChallengeRequest,
    now: DateTime<Utc>,
    instant: Instant,
) -> Result<BrowserReauthChallengeResponse, BrowserError> {
    if !eligible(api, ctx) {
        return Err(BrowserError::NotFound);
    }
    let auth = browser_auth(api);
    auth.challenge_rate
        .try_acquire(&limiter_key(ctx), instant)
        .map_err(BrowserError::ReauthRateLimited)?;
    if request.version != REAUTH_VERSION
        || !is_id(&request.credential_id, CREDENTIAL_ID_BYTES)
        || !is_id(&request.daemon_epoch, EPOCH_BYTES)
        || !is_id(&request.listener_epoch, EPOCH_BYTES)
        || !is_id(&request.client_nonce, NONCE_BYTES)
    {
        return Err(BrowserError::InvalidReauth);
    }
    if request.daemon_epoch != auth.daemon_epoch {
        return Err(BrowserError::DaemonRestarted);
    }
    let listener_epoch = auth.listener_epoch(&ctx.listener);
    if request.listener_epoch != listener_epoch {
        return Err(BrowserError::ListenerChanged);
    }
    let fields = auth
        .challenges
        .issue(
            ChallengeRequest {
                credential_id: &request.credential_id,
                daemon_epoch: &auth.daemon_epoch,
                listener_epoch: &listener_epoch,
                origin: &ctx.origin,
                listener: &ctx.listener,
                client_nonce: &request.client_nonce,
            },
            now,
            instant,
        )
        .map_err(|e| match e {
            ChallengeIssueError::ListenerFull => {
                BrowserError::ReauthRateLimited(REAUTH_CHALLENGES_FULL_RETRY)
            }
            ChallengeIssueError::Engine(e) => BrowserError::Internal(e),
        })?;
    Ok(BrowserReauthChallengeResponse {
        version: REAUTH_VERSION,
        credential_id: fields.credential_id,
        daemon_epoch: fields.daemon_epoch,
        listener_epoch: fields.listener_epoch,
        origin: fields.origin,
        listener: fields.listener,
        challenge_id: fields.challenge_id,
        nonce: fields.nonce,
        client_nonce: fields.client_nonce,
        expires_at: fields.expires_at,
    })
}

/// WI 0160 `POST /v1/browser-reauth/complete`: verify a signed challenge
/// and return the **same** grant (ID, bearer, original expiry).
///
/// The challenge is consumed by the first structurally valid request,
/// whatever follows. Release, revocation and expiry are serialized by the
/// reauth lock; the grant and (for an uncollected approval) its link
/// deadline are re-checked at that point.
pub fn reauth_complete(
    api: &DaemonApi,
    ctx: &BrowserContext,
    request: &BrowserReauthCompleteRequest,
) -> Result<BrowserReauthCompleteResponse, BrowserError> {
    let (now, instant) = browser_auth(api).clock_now();
    reauth_complete_at(api, ctx, request, now, instant)
}

/// [`reauth_complete`] from the caller's (`now`, `instant`): verification,
/// then release.
pub fn reauth_complete_at(
    api: &DaemonApi,
    ctx: &BrowserContext,
    request: &BrowserReauthCompleteRequest,
    now: DateTime<Utc>,
    instant: Instant,
) -> Result<BrowserReauthCompleteResponse, BrowserError> {
    let proof = reauth_verify_at(api, ctx, request, now, instant)?;
    reauth_release(api, ctx, proof)
}

/// A proof whose signature verified against its binding, not yet released.
pub struct VerifiedProof {
    challenge: Challenge,
    key: ReauthPublicKey,
    time: OperationTime,
}

impl std::fmt::Debug for VerifiedProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedProof").finish_non_exhaustive()
    }
}

/// The first half of a completion: consume the challenge and verify the
/// signature at the caller's time. Every refusal here is generic.
pub fn reauth_verify_at(
    api: &DaemonApi,
    ctx: &BrowserContext,
    request: &BrowserReauthCompleteRequest,
    now: DateTime<Utc>,
    instant: Instant,
) -> Result<VerifiedProof, BrowserError> {
    if !eligible(api, ctx) {
        return Err(BrowserError::NotFound);
    }
    let auth = browser_auth(api);
    let time = OperationTime::start(auth.grants.clock(), now, instant);
    auth.complete_rate
        .try_acquire(&limiter_key(ctx), instant)
        .map_err(BrowserError::ReauthRateLimited)?;
    // Not structurally valid: refused without consuming anything.
    if request.version != REAUTH_VERSION || !is_id(&request.challenge_id, NONCE_BYTES) {
        return Err(BrowserError::InvalidReauth);
    }
    // Structurally valid: the challenge is gone from here on.
    let challenge = auth.challenges.take(&request.challenge_id, now, instant);
    let signature =
        ReauthSignature::parse(request.signature.expose()).ok_or(BrowserError::InvalidReauth)?;
    let Some(challenge) = challenge else {
        tracing::debug!(listener = %ctx.listener, "browser reauthorization: no live challenge");
        return Err(BrowserError::ReauthFailed);
    };
    let fields = &challenge.fields;
    let mut state = auth.reauth_state();
    let current_epoch = state.listener_epoch(&ctx.listener);
    let binding = state.bindings.get(&fields.credential_id).cloned();
    drop(state);
    let context_ok = fields.origin == ctx.origin
        && fields.listener == ctx.listener
        && fields.daemon_epoch == auth.daemon_epoch
        && fields.listener_epoch == current_epoch
        && binding.as_ref().is_some_and(|b| {
            b.origin == ctx.origin
                && b.listener == ctx.listener
                && b.listener_epoch == fields.listener_epoch
        });
    // Exactly one verification whatever the binding: unknown credentials
    // use a fixed dummy key, so they cost the same as a wrong key.
    let key = match (&binding, context_ok) {
        (Some(b), true) => b.key.clone(),
        _ => ReauthPublicKey::dummy(),
    };
    if !(key.verify(&transcript(fields), &signature) && context_ok) {
        tracing::debug!(listener = %ctx.listener, "browser reauthorization proof refused");
        return Err(BrowserError::ReauthFailed);
    }
    Ok(VerifiedProof {
        challenge,
        key,
        time,
    })
}

/// The second half of a completion: the release boundary. Under the reauth
/// lock (serialized with approval, collection, revocation and expiry) and
/// at time sampled **there**, re-check the challenge deadline, the binding
/// and generation, an uncollected approval's link deadline and the grant,
/// then return the same grant. Only a verified proof reaches the precise
/// `authorization_expired` / `authorization_revoked` answers.
pub fn reauth_release(
    api: &DaemonApi,
    ctx: &BrowserContext,
    proof: VerifiedProof,
) -> Result<BrowserReauthCompleteResponse, BrowserError> {
    let auth = browser_auth(api);
    let mut state = auth.reauth_state();
    auth.reap_expired_locked(&mut state);
    let (now, instant) = proof.time.sample(SamplePoint::ProofRelease);
    let fields = &proof.challenge.fields;
    if !proof.challenge.live(now, instant) {
        tracing::debug!(listener = %ctx.listener, "browser reauthorization: challenge expired before release");
        return Err(BrowserError::ReauthFailed);
    }
    // The binding may have changed since verification (listener generation
    // rotated, binding removed or replaced).
    let current_epoch = state.listener_epoch(&ctx.listener);
    let Some(binding) = state
        .bindings
        .get(&fields.credential_id)
        .filter(|b| {
            b.key == proof.key
                && b.listener_epoch == fields.listener_epoch
                && fields.listener_epoch == current_epoch
        })
        .cloned()
    else {
        return Err(BrowserError::ReauthFailed);
    };
    let (grant, bearer, expires_at, uncollected) = match binding.state.clone() {
        BindingState::Pending { .. } => return Err(BrowserError::ReauthFailed),
        BindingState::Ended { reason, .. } => return Err(termination_error(reason)),
        BindingState::Approved {
            grant,
            bearer,
            expires_at,
            uncollected,
        } => (grant, bearer, expires_at, uncollected),
    };
    if uncollected.is_some() {
        match auth.links.collect_approved(&grant, now) {
            CollectOutcome::Collected | CollectOutcome::Absent => {
                state.bindings.mark_collected(&grant)
            }
            CollectOutcome::Expired => {
                // An abandoned approval ends at its link deadline (F1). That
                // is a generic failure, not the twelve-hour expiry.
                auth.end_orphan_locked(&mut state, &grant);
                return Err(BrowserError::ReauthFailed);
            }
        }
    }
    match auth.grants.ended_at(&grant, now, instant) {
        None => {}
        Some(GrantRevocationReason::Expired) => {
            let _ = auth.end_grant_locked(&mut state, &grant, GrantRevocationReason::Expired);
            return Err(BrowserError::AuthorizationExpired);
        }
        // Every revocation path records a tombstone under this lock, so a
        // missing grant without one is not a state to explain.
        Some(_) => return Err(BrowserError::ReauthFailed),
    }
    let scope = auth
        .grants
        .get(&grant)
        .map_or(BrowserScope::Engine, |r| r.scope);
    tracing::info!(grant = %grant, listener = %ctx.listener, "browser grant recovered by proof");
    Ok(BrowserReauthCompleteResponse {
        grant: BrowserGrantIssued {
            id: grant,
            token: bearer,
            scope,
            origin: binding.origin.clone(),
            listener: binding.listener.clone(),
            expires_at,
            reauth: Some(binding.public_binding(&auth.daemon_epoch)),
        },
    })
}

/// A tombstone's first termination reason → its one public outcome.
fn termination_error(reason: GrantRevocationReason) -> BrowserError {
    match reason {
        GrantRevocationReason::Expired => BrowserError::AuthorizationExpired,
        GrantRevocationReason::Deleted | GrantRevocationReason::OriginRemoved => {
            BrowserError::AuthorizationRevoked
        }
        // Listener-wide endings and daemon stop keep no tombstone.
        GrantRevocationReason::ListenerClosed | GrantRevocationReason::DaemonStopping => {
            BrowserError::ReauthFailed
        }
    }
}

/// `POST /v1/browser-tickets`: a one-use upgrade ticket for a resource this
/// grant may use. Ownership is checked here and again at upgrade.
pub fn tickets_create(
    api: &DaemonApi,
    browser: &BrowserPrincipal,
    request: &BrowserTicketRequest,
) -> Result<BrowserTicketResponse, BrowserError> {
    match request {
        BrowserTicketRequest::Frontend { instance_id } => {
            if !may_touch_instance(api, browser, instance_id) {
                return Err(BrowserError::ResourceNotFound("instance not found"));
            }
        }
        BrowserTicketRequest::Terminal { terminal_id } => {
            let visible = api
                .terminals
                .hub()
                .get(terminal_id)
                .ok()
                .is_some_and(|record| may_view_terminal(api, browser, &record));
            if !visible {
                return Err(BrowserError::ResourceNotFound("terminal not found"));
            }
        }
    }
    let ctx = browser.context();
    let ticket = browser_auth(api).tickets.issue(
        TicketBinding {
            grant: browser.grant_id.clone(),
            route: request.route(),
            resource: request.resource().to_owned(),
            origin: ctx.origin,
            listener: ctx.listener,
        },
        browser_auth(api).clock_now().0,
    )?;
    Ok(BrowserTicketResponse {
        ticket,
        expires_in_ms: TICKET_TTL.as_millis() as u64,
    })
}

/// A browser may touch an engine instance only if this grant created it or
/// its primary frontend is a web frontend (D3). Instances driven by a TUI or
/// CLI are invisible.
pub fn may_touch_instance(api: &DaemonApi, browser: &BrowserPrincipal, id: &InstanceId) -> bool {
    let Some(detail) = api.runtime.engine_host().get(id) else {
        return false;
    };
    browser_auth(api).created_by(id, &browser.grant_id)
        || is_web_primary(detail.summary.primary_frontend_kind)
}

/// `primary_frontend_kind == web`, by wire name so it holds for every kind
/// the protocol defines.
fn is_web_primary(kind: Option<crate::data::frontend_protocol::FrontendKindWire>) -> bool {
    kind.and_then(|k| serde_json::to_value(k).ok())
        .is_some_and(|v| v == "web")
}

/// Engine instances a browser may see (`GET /v1/engine/instances`).
pub fn visible_instance(
    api: &DaemonApi,
    browser: &BrowserPrincipal,
    summary: &crate::data::instance_snapshot::InstanceSummary,
) -> bool {
    browser_auth(api).created_by(&summary.id, &browser.grant_id)
        || is_web_primary(summary.primary_frontend_kind)
}

/// A browser viewer may open a terminal owned by a tab of an instance it
/// may touch, or a squad run's when squad is served on its listener (D11).
/// Worker commands and leases are never visible to a browser (v0).
pub fn may_view_terminal(
    api: &DaemonApi,
    browser: &BrowserPrincipal,
    record: &TerminalRecord,
) -> bool {
    match &record.owner_id {
        TerminalOwner::Tab(tab) => instance_of_tab(api, tab)
            .is_some_and(|instance| may_touch_instance(api, browser, &instance)),
        TerminalOwner::Run(_) => api
            .runtime
            .listener_serves(&browser.listener, FeatureName::Squad),
        TerminalOwner::Command(_) | TerminalOwner::Lease(_) => false,
    }
}

fn instance_of_tab(api: &DaemonApi, tab: &crate::data::ids::TabId) -> Option<InstanceId> {
    let host = api.runtime.engine_host();
    let all = host.list(
        &Default::default(),
        &crate::data::wire::Page {
            limit: usize::MAX,
            offset_or_key: None,
        },
    );
    all.items
        .into_iter()
        .map(|s| s.id)
        .find(|id| host.tab(id, tab).is_some())
}
