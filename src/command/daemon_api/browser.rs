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
    BrowserLinkPollResponse, BrowserLinkStatus, BrowserLinkView, BrowserScope,
    BrowserTicketRequest, BrowserTicketResponse, GrantRevocationReason, TicketRoute,
    FAILURE_LOCKOUT, LINK_CODE_PREFIX, LINK_CREATES_PER_MINUTE, LINK_POLL_INTERVAL,
    LOOKUP_FAILURES_PER_MINUTE, POLL_FAILURES_PER_MINUTE, TICKET_TTL,
};
use crate::data::config::network::ListenerTransport;
use crate::data::daemon_state::{FeatureName, Principal};
use crate::data::ids::{BrowserGrantId, BrowserLinkId, InstanceId};
use crate::data::terminal::{TerminalOwner, TerminalRecord};
use crate::data::wire::List;
use crate::engine::auth::browser_grants::{BrowserGrantStore, GrantVerdict, GrantWatch};
use crate::engine::auth::browser_links::{
    BrowserLinkTable, DecideError, LinkCreateError, PollOutcome, TicketBinding, TicketBindingRef,
    UpgradeTicketTable,
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
        })
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
        self.grants.count_for_listener(listener, Utc::now())
    }

    /// Check a browser request's bearer against this listener's grants only.
    /// API keys and work-server tokens are never consulted (D1).
    pub fn authenticate(
        &self,
        authorization: Option<&str>,
        ctx: &BrowserContext,
    ) -> Result<BrowserPrincipal, GrantVerdict> {
        match self
            .grants
            .verify(authorization, &ctx.origin, &ctx.listener, Utc::now())
        {
            GrantVerdict::Accepted { id } => Ok(BrowserPrincipal {
                grant_id: id,
                origin: ctx.origin.clone(),
                listener: ctx.listener.clone(),
            }),
            other => Err(other),
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
        let now = Utc::now();
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
        self.grants
            .is_valid(&grant, &ctx.origin, &ctx.listener, now)
            .then(|| BrowserPrincipal {
                grant_id: grant,
                origin: ctx.origin.clone(),
                listener: ctx.listener.clone(),
            })
    }

    /// Fires when the grant ends (sockets close with 1008).
    pub fn watch(&self, grant: &BrowserGrantId) -> Option<GrantWatch> {
        self.grants.watch(grant)
    }

    /// Revoke everything matching `pred` with `reason`: grants (closing their
    /// sockets), their tickets, and pending links on the same terms.
    pub fn revoke_where(
        &self,
        pred: impl Fn(&str, &str) -> bool,
        reason: GrantRevocationReason,
    ) -> Vec<BrowserGrantId> {
        let mut ids = self.links.remove_where(&pred);
        match self
            .grants
            .revoke_where(|g| pred(&g.origin, &g.listener), reason)
        {
            Ok(revoked) => ids.extend(revoked),
            Err(e) => tracing::warn!(error = %e, "browser grant revocation was not persisted"),
        }
        for id in &ids {
            self.tickets.purge_grant(id);
            let _ = self.grants.delete(id, reason);
        }
        self.created_instances
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|_, g| !ids.contains(g));
        ids
    }

    /// Daemon stop: every grant ends.
    pub fn revoke_all(&self, reason: GrantRevocationReason) {
        self.revoke_where(|_, _| true, reason);
    }

    /// Mint a ticket as of `issued_at` (tests of expiry only).
    #[cfg(any(test, feature = "test-support"))]
    pub fn issue_ticket_for_tests(
        &self,
        grant: &str,
        route: TicketRoute,
        resource: &str,
        origin: &str,
        listener: &str,
        issued_at: DateTime<Utc>,
    ) -> Result<String, EngineError> {
        let grant = BrowserGrantId::parse(grant).map_err(|e| EngineError::Auth(e.to_string()))?;
        Ok(self
            .tickets
            .issue(
                TicketBinding {
                    grant,
                    route,
                    resource: resource.to_owned(),
                    origin: origin.to_owned(),
                    listener: listener.to_owned(),
                },
                issued_at,
            )?
            .expose()
            .to_owned())
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

    fn sweep(&self, now: DateTime<Utc>) {
        for orphan in self.links.sweep_expired(now) {
            let _ = self.grants.delete(&orphan, GrantRevocationReason::Expired);
            self.tickets.purge_grant(&orphan);
        }
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
            | BrowserRevocation::ListenerRemoved => {
                self.revoke_where(|_, l| l == listener, GrantRevocationReason::ListenerClosed)
            }
        };
        if !revoked.is_empty() {
            tracing::info!(
                listener,
                grants = revoked.len(),
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
    links_create_at(api, ctx, Utc::now(), Instant::now())
}

pub(crate) fn links_create_at(
    api: &DaemonApi,
    ctx: &BrowserContext,
    now: DateTime<Utc>,
    instant: Instant,
) -> Result<BrowserLinkCreateResponse, BrowserError> {
    if !eligible(api, ctx) {
        return Err(BrowserError::NotFound);
    }
    let auth = browser_auth(api);
    auth.sweep(now);
    auth.creates
        .try_acquire(&limiter_key(ctx), instant)
        .map_err(BrowserError::RateLimited)?;
    let created = auth
        .links
        .create(&ctx.origin, &ctx.listener, BrowserScope::Engine, now)
        .map_err(|e| match e {
            LinkCreateError::TooManyPending => BrowserError::RateLimited(MINUTE),
            LinkCreateError::Engine(e) => BrowserError::Internal(e),
        })?;
    tracing::info!(
        link = %created.view.id,
        listener = %ctx.listener,
        origin = %ctx.origin,
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
    })
}

/// `POST /v1/browser-links/{id}/poll`.
pub fn links_poll(
    api: &DaemonApi,
    ctx: &BrowserContext,
    id: &str,
    secret: &str,
) -> Result<BrowserLinkPollResponse, BrowserError> {
    links_poll_at(api, ctx, id, secret, Utc::now(), Instant::now())
}

pub(crate) fn links_poll_at(
    api: &DaemonApi,
    ctx: &BrowserContext,
    id: &str,
    secret: &str,
    now: DateTime<Utc>,
    instant: Instant,
) -> Result<BrowserLinkPollResponse, BrowserError> {
    let auth = browser_auth(api);
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
    match auth
        .links
        .poll(&id, secret, &ctx.origin, &ctx.listener, now)
    {
        PollOutcome::Pending => Ok(BrowserLinkPollResponse::Pending {
            retry_after_ms: LINK_POLL_INTERVAL.as_millis() as u64,
        }),
        PollOutcome::Approved(grant) => {
            tracing::info!(link = %id, grant = %grant.id, "browser link collected");
            Ok(BrowserLinkPollResponse::Approved { grant })
        }
        PollOutcome::Denied => Ok(BrowserLinkPollResponse::Denied),
        PollOutcome::TooFast(wait) => Err(BrowserError::RateLimited(wait)),
        PollOutcome::Gone => Err(fail()),
    }
}

/// `GET /v1/browser-links?code=web-…` (owner socket): the pending request
/// `awman link` shows before asking for confirmation.
pub fn links_lookup(api: &DaemonApi, code: &str) -> Result<BrowserLinkView, BrowserError> {
    links_lookup_at(api, code, Utc::now(), Instant::now())
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
        .and_then(|code| auth.links.lookup_by_code(&code, now));
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
    let now = Utc::now();
    let grant_id = auth
        .links
        .decide(&id, approve, now, |link| {
            let (record, token) = auth
                .grants
                .create(&link.origin, &link.listener, link.scope, None, now)
                .map_err(|e| e.to_string())?;
            Ok(BrowserGrantIssued {
                id: record.id,
                token,
                scope: record.scope,
                origin: record.origin,
                listener: record.listener,
                expires_at: record.expires_at,
            })
        })
        .map_err(|e| match e {
            DecideError::NotFound => BrowserError::LinkNotFound,
            DecideError::AlreadyDecided => BrowserError::AlreadyDecided,
            DecideError::Grant(e) => BrowserError::Internal(e),
        })?;
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
    List {
        items: browser_auth(api)
            .grants
            .list(Utc::now())
            .into_iter()
            .map(|g| BrowserGrantView {
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
/// close with 1008 `grant revoked`.
pub fn browsers_delete(api: &DaemonApi, id: &str) -> Result<(), BrowserError> {
    let id = BrowserGrantId::parse(id).map_err(|_| BrowserError::GrantNotFound)?;
    let auth = browser_auth(api);
    let deleted = auth.grants.delete(&id, GrantRevocationReason::Deleted);
    auth.tickets.purge_grant(&id);
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
        Utc::now(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_primary_is_recognised_by_wire_name_only() {
        use crate::data::frontend_protocol::FrontendKindWire;
        assert!(!is_web_primary(None));
        assert!(!is_web_primary(Some(FrontendKindWire::Tui)));
        assert!(!is_web_primary(Some(FrontendKindWire::Cli)));
    }

    #[test]
    fn browser_principal_round_trips_through_principal() {
        let browser = BrowserPrincipal {
            grant_id: BrowserGrantId::new(),
            origin: "https://awman.prettysmart.dev".into(),
            listener: "loopback".into(),
        };
        assert_eq!(
            BrowserPrincipal::from_principal(&browser.principal()),
            Some(browser)
        );
        assert_eq!(BrowserPrincipal::from_principal(&Principal::Owner), None);
        assert_eq!(
            BrowserPrincipal::from_principal(&Principal::ApiKey { id: "key_x".into() }),
            None
        );
    }

    #[tokio::test]
    async fn config_revocations_end_grants_watches_and_pending_links() {
        let tmp = tempfile::tempdir().unwrap();
        let auth =
            BrowserAuth::open(tmp.path().join("daemon").join("browser_grants.json")).unwrap();
        let origin = "https://awman.prettysmart.dev";
        let (a, _) = auth
            .grants()
            .create(origin, "loopback", BrowserScope::Engine, None, Utc::now())
            .unwrap();
        let (b, _) = auth
            .grants()
            .create(
                "http://localhost:3000",
                "loopback",
                BrowserScope::Engine,
                None,
                Utc::now(),
            )
            .unwrap();
        let (other, _) = auth
            .grants()
            .create(origin, "second", BrowserScope::Engine, None, Utc::now())
            .unwrap();
        let link = auth
            .links
            .create(
                "http://localhost:3000",
                "loopback",
                BrowserScope::Engine,
                Utc::now(),
            )
            .unwrap();
        let watch_b = auth.watch(&b.id).unwrap();
        auth.revoke(
            "loopback",
            BrowserRevocation::OriginRemoved(
                crate::data::config::network::BrowserOrigin::parse("http://localhost:3000")
                    .unwrap(),
            ),
        );
        assert_eq!(watch_b.ended().await, GrantRevocationReason::OriginRemoved);
        let code = ShortCode::parse(&link.view.code, "web").unwrap();
        assert!(auth.links.lookup_by_code(&code, Utc::now()).is_none());
        let watch_a = auth.watch(&a.id).unwrap();
        auth.revoke("loopback", BrowserRevocation::EngineRemoved);
        assert_eq!(watch_a.ended().await, GrantRevocationReason::ListenerClosed);
        assert_eq!(auth.count_for_listener("loopback"), 0);
        assert_eq!(auth.count_for_listener("second"), 1);
        auth.revoke_all(GrantRevocationReason::DaemonStopping);
        assert!(auth.watch(&other.id).is_none());
    }
}
