//! In-memory browser pairing state (WI 0159): pending browser links and
//! WebSocket upgrade tickets. Nothing here touches the disk; a daemon
//! restart forgets every pending link and ticket.
//!
//! [`BrowserLinkTable`] holds pending `web-XXXX-XXXX` requests. The polling
//! secret is kept only as a hash; the grant token minted on approval is held
//! until the first poll reads it, then the link is consumed. A wrong secret,
//! origin or listener looks exactly like an unknown link (D5).
//!
//! [`UpgradeTicketTable`] holds one-use `awt_…` tickets bound to a grant,
//! route, resource, origin and listener, at most
//! [`MAX_TICKETS_PER_GRANT`] per grant.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};

use crate::data::browser_auth::{
    BrowserGrantIssued, BrowserLinkStatus, BrowserLinkView, BrowserScope, BrowserSecret,
    TicketRoute, LINK_CODE_PREFIX, LINK_POLL_INTERVAL, LINK_TTL, MAX_PENDING_LINKS_PER_LISTENER,
    MAX_TICKETS_PER_GRANT, TICKET_PREFIX, TICKET_TTL,
};
use crate::data::ids::{BrowserGrantId, BrowserLinkId};
use crate::engine::auth::device_flow::{generate_secret, hash_secret, secret_matches, ShortCode};
use crate::engine::error::EngineError;

/// Polls closer together than this are refused. A tenth below the advertised
/// interval so browser timer jitter never trips it.
pub const MIN_POLL_SPACING: std::time::Duration =
    std::time::Duration::from_millis(LINK_POLL_INTERVAL.as_millis() as u64 * 9 / 10);

fn chrono(d: std::time::Duration) -> chrono::Duration {
    chrono::Duration::from_std(d).expect("short durations fit")
}

#[derive(Debug)]
enum Decision {
    Approved(BrowserGrantIssued),
    Denied,
}

#[derive(Debug)]
struct PendingLink {
    id: BrowserLinkId,
    code: ShortCode,
    secret_hash: String,
    origin: String,
    listener: String,
    scope: BrowserScope,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    decision: Option<Decision>,
    last_poll: Option<DateTime<Utc>>,
}

impl PendingLink {
    fn live(&self, now: DateTime<Utc>) -> bool {
        !crate::engine::auth::device_flow::deadline_expired(now, self.expires_at)
    }
    fn view(&self) -> BrowserLinkView {
        BrowserLinkView {
            id: self.id.clone(),
            code: self.code.as_str().to_owned(),
            origin: self.origin.clone(),
            listener: self.listener.clone(),
            scope: self.scope,
            created_at: self.created_at,
            expires_at: self.expires_at,
            status: match self.decision {
                None => BrowserLinkStatus::Pending,
                Some(Decision::Approved(_)) => BrowserLinkStatus::Approved,
                Some(Decision::Denied) => BrowserLinkStatus::Denied,
            },
        }
    }
}

/// A freshly created link: its owner-facing view and the polling secret,
/// which exists nowhere else.
#[derive(Debug)]
pub struct CreatedLink {
    pub view: BrowserLinkView,
    pub polling_secret: BrowserSecret,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkCreateError {
    #[error("too many pending browser links on this listener")]
    TooManyPending,
    #[error("{0}")]
    Engine(String),
}

/// What one poll observed.
#[derive(Debug, PartialEq, Eq)]
pub enum PollOutcome {
    Pending,
    Approved(BrowserGrantIssued),
    Denied,
    /// Polled again sooner than the interval; nothing consumed.
    TooFast(std::time::Duration),
    /// Unknown, expired, consumed, wrong secret, wrong origin or listener.
    Gone,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecideError {
    #[error("browser link not found or expired")]
    NotFound,
    #[error("browser link was already decided")]
    AlreadyDecided,
    #[error("{0}")]
    Grant(String),
}

/// Pending browser links.
#[derive(Debug, Default)]
pub struct BrowserLinkTable {
    links: Mutex<HashMap<BrowserLinkId, PendingLink>>,
}

impl BrowserLinkTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a link for `origin` on `listener`. At most
    /// [`MAX_PENDING_LINKS_PER_LISTENER`] undecided, unexpired links may
    /// exist per listener.
    pub fn create(
        &self,
        origin: &str,
        listener: &str,
        scope: BrowserScope,
        now: DateTime<Utc>,
    ) -> Result<CreatedLink, LinkCreateError> {
        let mut links = self.lock();
        links.retain(|_, l| l.live(now));
        let pending = links
            .values()
            .filter(|l| l.listener == listener && l.decision.is_none())
            .count();
        if pending >= MAX_PENDING_LINKS_PER_LISTENER {
            return Err(LinkCreateError::TooManyPending);
        }
        let engine = |e: EngineError| LinkCreateError::Engine(e.to_string());
        let code = loop {
            let code = ShortCode::generate(LINK_CODE_PREFIX).map_err(engine)?;
            if !links.values().any(|l| l.code == code) {
                break code;
            }
        };
        let secret = generate_secret(32).map_err(engine)?;
        let link = PendingLink {
            id: BrowserLinkId::new(),
            code,
            secret_hash: hash_secret(&secret),
            origin: origin.to_owned(),
            listener: listener.to_owned(),
            scope,
            created_at: now,
            expires_at: now + chrono(LINK_TTL),
            decision: None,
            last_poll: None,
        };
        let view = link.view();
        links.insert(link.id.clone(), link);
        Ok(CreatedLink {
            view,
            polling_secret: BrowserSecret::new(secret),
        })
    }

    /// The pending, unexpired, undecided link with this code.
    pub fn lookup_by_code(&self, code: &ShortCode, now: DateTime<Utc>) -> Option<BrowserLinkView> {
        self.lock()
            .values()
            .find(|l| &l.code == code && l.live(now) && l.decision.is_none())
            .map(PendingLink::view)
    }

    /// Record the owner's decision. On approval `mint` creates the grant
    /// (before this returns); its token is held for the poller.
    pub fn decide(
        &self,
        id: &BrowserLinkId,
        approve: bool,
        now: DateTime<Utc>,
        mint: impl FnOnce(&BrowserLinkView) -> Result<BrowserGrantIssued, String>,
    ) -> Result<Option<BrowserGrantId>, DecideError> {
        let mut links = self.lock();
        let link = links
            .get_mut(id)
            .filter(|l| l.live(now))
            .ok_or(DecideError::NotFound)?;
        if link.decision.is_some() {
            return Err(DecideError::AlreadyDecided);
        }
        if !approve {
            link.decision = Some(Decision::Denied);
            return Ok(None);
        }
        let grant = mint(&link.view()).map_err(DecideError::Grant)?;
        let grant_id = grant.id.clone();
        link.decision = Some(Decision::Approved(grant));
        Ok(Some(grant_id))
    }

    /// One poll by the browser. Approved and denied results are returned
    /// once; the link is consumed by that read.
    pub fn poll(
        &self,
        id: &BrowserLinkId,
        secret: &str,
        origin: &str,
        listener: &str,
        now: DateTime<Utc>,
    ) -> PollOutcome {
        let mut links = self.lock();
        let Some(link) = links.get_mut(id) else {
            // Same hashing work as a known link.
            let _ = secret_matches(secret, &"0".repeat(64));
            return PollOutcome::Gone;
        };
        let secret_ok = secret_matches(secret, &link.secret_hash);
        if !secret_ok || link.origin != origin || link.listener != listener || !link.live(now) {
            return PollOutcome::Gone;
        }
        if let Some(last) = link.last_poll {
            let since = (now - last).to_std().unwrap_or_default();
            if since < MIN_POLL_SPACING {
                return PollOutcome::TooFast(crate::engine::auth::device_flow::poll_delay(
                    LINK_POLL_INTERVAL,
                    since,
                ));
            }
        }
        link.last_poll = Some(now);
        if link.decision.is_none() {
            return PollOutcome::Pending;
        }
        match links.remove(id).and_then(|l| l.decision) {
            Some(Decision::Approved(grant)) => PollOutcome::Approved(grant),
            Some(Decision::Denied) => PollOutcome::Denied,
            None => PollOutcome::Gone,
        }
    }

    /// Drop expired links. Returns the grants that were approved but never
    /// collected, so the caller can revoke them.
    pub fn sweep_expired(&self, now: DateTime<Utc>) -> Vec<BrowserGrantId> {
        let mut orphaned = Vec::new();
        self.lock().retain(|_, l| {
            if l.live(now) {
                return true;
            }
            if let Some(Decision::Approved(grant)) = &l.decision {
                orphaned.push(grant.id.clone());
            }
            false
        });
        orphaned
    }

    /// Drop every link matching `pred` (listener closed, origin removed).
    /// Returns grants that were approved but never collected.
    pub fn remove_where(&self, pred: impl Fn(&str, &str) -> bool) -> Vec<BrowserGrantId> {
        let mut orphaned = Vec::new();
        self.lock().retain(|_, l| {
            if !pred(&l.origin, &l.listener) {
                return true;
            }
            if let Some(Decision::Approved(grant)) = &l.decision {
                orphaned.push(grant.id.clone());
            }
            false
        });
        orphaned
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<BrowserLinkId, PendingLink>> {
        self.links.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// What a ticket is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketBinding {
    pub grant: BrowserGrantId,
    pub route: TicketRoute,
    pub resource: String,
    pub origin: String,
    pub listener: String,
}

#[derive(Debug)]
struct Ticket {
    binding: TicketBinding,
    issued_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

/// One-use WebSocket upgrade tickets, keyed by the ticket's hash.
#[derive(Debug, Default)]
pub struct UpgradeTicketTable {
    tickets: Mutex<HashMap<String, Ticket>>,
}

impl UpgradeTicketTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a ticket (`awt_` + 43 base64url) for `binding`, valid for
    /// [`TICKET_TTL`]. The grant's oldest ticket is evicted beyond
    /// [`MAX_TICKETS_PER_GRANT`].
    pub fn issue(
        &self,
        binding: TicketBinding,
        now: DateTime<Utc>,
    ) -> Result<BrowserSecret, EngineError> {
        let ticket = format!("{TICKET_PREFIX}{}", generate_secret(32)?);
        let mut tickets = self.lock();
        tickets.retain(|_, t| t.expires_at > now);
        let mut mine: Vec<(String, DateTime<Utc>)> = tickets
            .iter()
            .filter(|(_, t)| t.binding.grant == binding.grant)
            .map(|(h, t)| (h.clone(), t.issued_at))
            .collect();
        if mine.len() >= MAX_TICKETS_PER_GRANT {
            mine.sort_by_key(|(_, issued)| *issued);
            for (hash, _) in mine.iter().take(mine.len() + 1 - MAX_TICKETS_PER_GRANT) {
                tickets.remove(hash);
            }
        }
        tickets.insert(
            hash_secret(&ticket),
            Ticket {
                binding,
                issued_at: now,
                expires_at: now + chrono(TICKET_TTL),
            },
        );
        Ok(BrowserSecret::new(ticket))
    }

    /// Remove `ticket` and return its grant if it matches `expected` and has
    /// not expired. The ticket is gone after this call whatever the result,
    /// so a ticket can never be tried twice.
    pub fn consume(
        &self,
        ticket: &str,
        expected: &TicketBindingRef<'_>,
        now: DateTime<Utc>,
    ) -> Option<BrowserGrantId> {
        let found = self.lock().remove(&hash_secret(ticket))?;
        let b = &found.binding;
        (found.expires_at > now
            && b.route == expected.route
            && b.resource == expected.resource
            && b.origin == expected.origin
            && b.listener == expected.listener)
            .then(|| b.grant.clone())
    }

    /// Drop every ticket of `grant` (revocation).
    pub fn purge_grant(&self, grant: &BrowserGrantId) {
        self.lock().retain(|_, t| &t.binding.grant != grant);
    }

    /// Live tickets for `grant` (tests and diagnostics).
    pub fn count_for_grant(&self, grant: &BrowserGrantId) -> usize {
        self.lock()
            .values()
            .filter(|t| &t.binding.grant == grant)
            .count()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Ticket>> {
        self.tickets.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// What an upgrade request presents a ticket for.
#[derive(Debug, Clone, Copy)]
pub struct TicketBindingRef<'a> {
    pub route: TicketRoute,
    pub resource: &'a str,
    pub origin: &'a str,
    pub listener: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: &str = "https://awman.prettysmart.dev";

    fn grant(view: &BrowserLinkView) -> Result<BrowserGrantIssued, String> {
        Ok(BrowserGrantIssued {
            id: BrowserGrantId::new(),
            token: BrowserSecret::new("awb_token"),
            scope: view.scope,
            origin: view.origin.clone(),
            listener: view.listener.clone(),
            expires_at: view.expires_at,
        })
    }

    fn secs(n: i64) -> chrono::Duration {
        chrono::Duration::seconds(n)
    }

    #[test]
    fn approved_link_is_read_once_then_gone() {
        let table = BrowserLinkTable::new();
        let t0 = Utc::now();
        let link = table
            .create(ORIGIN, "loopback", BrowserScope::Engine, t0)
            .unwrap();
        let secret = link.polling_secret.expose().to_owned();
        assert_eq!(link.view.expires_at - t0, secs(300));
        assert_eq!(
            table.poll(&link.view.id, &secret, ORIGIN, "loopback", t0 + secs(2)),
            PollOutcome::Pending
        );
        let code = ShortCode::parse(&link.view.code, "web").unwrap();
        assert!(table.lookup_by_code(&code, t0).is_some());
        let grant_id = table
            .decide(&link.view.id, true, t0 + secs(3), grant)
            .unwrap()
            .unwrap();
        assert!(table.lookup_by_code(&code, t0).is_none());
        assert_eq!(
            table.decide(&link.view.id, true, t0 + secs(3), grant),
            Err(DecideError::AlreadyDecided)
        );
        match table.poll(&link.view.id, &secret, ORIGIN, "loopback", t0 + secs(4)) {
            PollOutcome::Approved(g) => assert_eq!(g.id, grant_id),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            table.poll(&link.view.id, &secret, ORIGIN, "loopback", t0 + secs(6)),
            PollOutcome::Gone
        );
    }

    #[test]
    fn denied_link_is_read_once_then_gone() {
        let table = BrowserLinkTable::new();
        let t0 = Utc::now();
        let link = table
            .create(ORIGIN, "loopback", BrowserScope::Engine, t0)
            .unwrap();
        let s = link.polling_secret.expose().to_owned();
        assert_eq!(table.decide(&link.view.id, false, t0, grant), Ok(None));
        assert_eq!(
            table.poll(&link.view.id, &s, ORIGIN, "loopback", t0),
            PollOutcome::Denied
        );
        assert_eq!(
            table.poll(&link.view.id, &s, ORIGIN, "loopback", t0 + secs(2)),
            PollOutcome::Gone
        );
    }

    #[test]
    fn wrong_secret_origin_listener_and_expiry_are_all_gone_without_consuming() {
        let table = BrowserLinkTable::new();
        let t0 = Utc::now();
        let link = table
            .create(ORIGIN, "loopback", BrowserScope::Engine, t0)
            .unwrap();
        let s = link.polling_secret.expose().to_owned();
        let id = &link.view.id;
        assert_eq!(
            table.poll(id, "wrong", ORIGIN, "loopback", t0),
            PollOutcome::Gone
        );
        assert_eq!(
            table.poll(id, &s, "http://localhost:3000", "loopback", t0),
            PollOutcome::Gone
        );
        assert_eq!(table.poll(id, &s, ORIGIN, "other", t0), PollOutcome::Gone);
        assert_eq!(
            table.poll(&BrowserLinkId::new(), &s, ORIGIN, "loopback", t0),
            PollOutcome::Gone
        );
        assert_eq!(
            table.poll(id, &s, ORIGIN, "loopback", t0),
            PollOutcome::Pending
        );
        assert_eq!(
            table.poll(id, &s, ORIGIN, "loopback", t0 + secs(300)),
            PollOutcome::Gone
        );
        assert_eq!(
            table.decide(id, true, t0 + secs(300), grant),
            Err(DecideError::NotFound)
        );
    }

    #[test]
    fn polling_faster_than_the_interval_is_refused_without_consuming() {
        let table = BrowserLinkTable::new();
        let t0 = Utc::now();
        let link = table
            .create(ORIGIN, "loopback", BrowserScope::Engine, t0)
            .unwrap();
        let s = link.polling_secret.expose().to_owned();
        let id = &link.view.id;
        table.decide(id, false, t0, grant).unwrap();
        assert_eq!(
            table.poll(id, &s, ORIGIN, "loopback", t0),
            PollOutcome::Denied
        );
        let link = table
            .create(ORIGIN, "loopback", BrowserScope::Engine, t0)
            .unwrap();
        let s = link.polling_secret.expose().to_owned();
        let id = &link.view.id;
        assert_eq!(
            table.poll(id, &s, ORIGIN, "loopback", t0),
            PollOutcome::Pending
        );
        assert!(matches!(
            table.poll(
                id,
                &s,
                ORIGIN,
                "loopback",
                t0 + chrono::Duration::milliseconds(500)
            ),
            PollOutcome::TooFast(_)
        ));
        table.decide(id, false, t0, grant).unwrap();
        assert_eq!(
            table.poll(id, &s, ORIGIN, "loopback", t0 + secs(2)),
            PollOutcome::Denied
        );
    }

    #[test]
    fn pending_links_are_capped_per_listener() {
        let table = BrowserLinkTable::new();
        let t0 = Utc::now();
        for _ in 0..3 {
            table
                .create(ORIGIN, "loopback", BrowserScope::Engine, t0)
                .unwrap();
        }
        assert_eq!(
            table
                .create(ORIGIN, "loopback", BrowserScope::Engine, t0)
                .unwrap_err(),
            LinkCreateError::TooManyPending
        );
        assert!(table
            .create(ORIGIN, "other", BrowserScope::Engine, t0)
            .is_ok());
        // Expired links stop counting.
        assert!(table
            .create(ORIGIN, "loopback", BrowserScope::Engine, t0 + secs(301))
            .is_ok());
    }

    #[test]
    fn expired_approved_links_report_their_uncollected_grants() {
        let table = BrowserLinkTable::new();
        let t0 = Utc::now();
        let link = table
            .create(ORIGIN, "loopback", BrowserScope::Engine, t0)
            .unwrap();
        let id = table
            .decide(&link.view.id, true, t0, grant)
            .unwrap()
            .unwrap();
        assert!(table.sweep_expired(t0 + secs(10)).is_empty());
        assert_eq!(table.sweep_expired(t0 + secs(300)), vec![id]);
    }

    fn binding(grant: &BrowserGrantId) -> TicketBinding {
        TicketBinding {
            grant: grant.clone(),
            route: TicketRoute::Frontend,
            resource: "ins_1".into(),
            origin: ORIGIN.into(),
            listener: "loopback".into(),
        }
    }

    fn expected() -> TicketBindingRef<'static> {
        TicketBindingRef {
            route: TicketRoute::Frontend,
            resource: "ins_1",
            origin: ORIGIN,
            listener: "loopback",
        }
    }

    #[test]
    fn tickets_are_one_use_bound_and_short_lived() {
        let table = UpgradeTicketTable::new();
        let g = BrowserGrantId::new();
        let t0 = Utc::now();
        let ticket = table.issue(binding(&g), t0).unwrap();
        assert!(ticket.expose().starts_with("awt_"));
        assert_eq!(ticket.expose().len(), 47);
        assert_eq!(
            table.consume(ticket.expose(), &expected(), t0),
            Some(g.clone())
        );
        assert_eq!(table.consume(ticket.expose(), &expected(), t0), None);

        // A mismatch burns the ticket too.
        let ticket = table.issue(binding(&g), t0).unwrap();
        let mut wrong = expected();
        wrong.route = TicketRoute::Terminal;
        assert_eq!(table.consume(ticket.expose(), &wrong, t0), None);
        assert_eq!(table.consume(ticket.expose(), &expected(), t0), None);

        for mismatch in [
            TicketBindingRef {
                resource: "ins_2",
                ..expected()
            },
            TicketBindingRef {
                origin: "http://localhost:3000",
                ..expected()
            },
            TicketBindingRef {
                listener: "other",
                ..expected()
            },
        ] {
            let ticket = table.issue(binding(&g), t0).unwrap();
            assert_eq!(table.consume(ticket.expose(), &mismatch, t0), None);
        }

        let ticket = table.issue(binding(&g), t0).unwrap();
        assert_eq!(
            table.consume(ticket.expose(), &expected(), t0 + secs(30)),
            None
        );
    }

    #[test]
    fn tickets_are_capped_per_grant_and_purged_with_it() {
        let table = UpgradeTicketTable::new();
        let g = BrowserGrantId::new();
        let t0 = Utc::now();
        let first = table.issue(binding(&g), t0).unwrap();
        for i in 1..=64 {
            table
                .issue(binding(&g), t0 + chrono::Duration::milliseconds(i))
                .unwrap();
        }
        assert_eq!(table.count_for_grant(&g), 64);
        assert_eq!(table.consume(first.expose(), &expected(), t0), None);
        table.purge_grant(&g);
        assert_eq!(table.count_for_grant(&g), 0);
    }

    #[test]
    fn a_ticket_is_consumed_exactly_once_under_concurrent_upgrades() {
        let table = std::sync::Arc::new(UpgradeTicketTable::new());
        let g = BrowserGrantId::new();
        let t0 = Utc::now();
        let ticket = table.issue(binding(&g), t0).unwrap().expose().to_owned();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let table = table.clone();
                let ticket = ticket.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    table.consume(&ticket, &expected(), t0).is_some()
                })
            })
            .collect();
        let wins = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(wins, 1);
    }
}
