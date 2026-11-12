//! A transactional in-memory server with an optional loopback HTTP listener.
use crate::{labels::validate_key, terminal::LeaseTerminalFrame, types::*};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex},
};
use tokio::sync::{broadcast, Notify};
mod http;
pub use http::ServingMock;

/// Deliberately nonconforming server behavior used to prove the probes reject it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockFault {
    IgnorePreferences,
    IgnoreEpoch,
    DuplicateEvents,
    EmptyEvents,
    Wire(&'static str),
}

#[derive(Default)]
pub struct MockWorkServer {
    inner: Mutex<State>,
    changed: Notify,
}
#[derive(Default)]
struct State {
    workers: HashMap<String, Worker>,
    server_owned: HashSet<String>,
    items: Vec<Entry>,
    leases: HashMap<String, ActiveLease>,
    links: HashMap<String, Link>,
    access: HashMap<String, (String, DateTime<Utc>)>,
    results: HashMap<String, LeaseResult<Value>>,
    releases: HashMap<String, ReleaseRequest<Value>>,
    events: HashMap<String, BTreeMap<(u32, u64), Value>>,
    frames: HashMap<String, Vec<LeaseTerminalFrame>>,
    terminal_errors: HashMap<String, String>,
    next_id: u64,
    fault: Option<MockFault>,
}
struct Worker {
    request: RegisterRequest,
    server_labels: BTreeMap<String, String>,
    labels: BTreeMap<String, String>,
}
struct Link {
    labels: BTreeMap<String, String>,
    refresh: Option<String>,
    denied: bool,
    expires_at: DateTime<Utc>,
    revoked: bool,
    worker: Option<String>,
}
struct Entry {
    item: WorkItem,
    enqueued_at: DateTime<Utc>,
    epoch: u64,
    completed: bool,
    excluded: HashSet<String>,
    terminal: bool,
    remote_input: bool,
    next_lease_id: Option<String>,
}
struct ActiveLease {
    lease: Lease,
    worker_id: String,
    cancel: Option<String>,
    heartbeat: Option<HeartbeatRequest>,
    outbound: broadcast::Sender<LeaseTerminalFrame>,
}
fn error(code: &str) -> ApiError {
    ApiError {
        error: code.replace('_', " "),
        code: code.into(),
    }
}
fn expire(state: &mut State, now: DateTime<Utc>) {
    state.leases.retain(|_, l| l.lease.expires_at > now);
}
fn live<'a>(state: &'a mut State, id: &str, epoch: u64) -> Result<&'a mut ActiveLease, ApiError> {
    state
        .leases
        .get_mut(id)
        .filter(|l| l.lease.epoch == epoch)
        .ok_or_else(|| error("lease_lost"))
}
fn eligible<'a>(s: &'a State, worker: &str) -> Result<Vec<&'a Entry>, ApiError> {
    let labels = &s
        .workers
        .get(worker)
        .ok_or_else(|| error("worker_not_found"))?
        .labels;
    let mut entries: Vec<_> = s
        .items
        .iter()
        .filter(|e| {
            !e.completed
                && !e.excluded.contains(worker)
                && e.item.affinity.eligible(labels)
                && !s.leases.values().any(|l| l.lease.item.id == e.item.id)
        })
        .collect();
    let ignore_preferences = s.fault == Some(MockFault::IgnorePreferences);
    entries.sort_by(|a, b| {
        b.item
            .priority
            .cmp(&a.item.priority)
            .then(a.enqueued_at.cmp(&b.enqueued_at))
            .then(if ignore_preferences {
                std::cmp::Ordering::Equal
            } else {
                b.item
                    .affinity
                    .preference_score(labels)
                    .cmp(&a.item.affinity.preference_score(labels))
            })
            .then(a.item.id.cmp(&b.item.id))
    });
    Ok(entries)
}
impl MockWorkServer {
    pub fn new() -> Self {
        Self::default()
    }
    /// Construct a deliberately nonconforming fake to validate conformance probes.
    pub fn with_fault(fault: MockFault) -> Self {
        let server = Self::new();
        server.inner.lock().unwrap().fault = Some(fault);
        server
    }
    pub fn register(
        &self,
        request: RegisterRequest,
        server_labels: BTreeMap<String, String>,
    ) -> RegisterResponse {
        let mut s = self.inner.lock().unwrap();
        s.next_id += 1;
        let worker_id = format!("worker_{}", s.next_id);
        register_worker(&mut s, worker_id, request, server_labels)
    }
    /// Rejects malformed affinity and labels rather than silently changing eligibility.
    pub fn enqueue(&self, item: WorkItem, at: DateTime<Utc>) -> Result<(), ApiError> {
        item.affinity
            .validate()
            .map_err(|_| error("invalid_affinity"))?;
        for k in item.labels.keys() {
            validate_key(k).map_err(|_| error("invalid_label"))?;
        }
        let mut s = self.inner.lock().unwrap();
        if s.items.iter().any(|e| e.item.id == item.id) {
            return Err(error("duplicate_item"));
        }
        s.items.push(Entry {
            item,
            enqueued_at: at,
            epoch: 0,
            completed: false,
            excluded: HashSet::new(),
            terminal: false,
            remote_input: false,
            next_lease_id: None,
        });
        drop(s);
        self.changed.notify_waiters();
        Ok(())
    }
    pub fn queue(&self, worker: &str) -> Result<List<QueueItem>, ApiError> {
        self.queue_at(worker, Utc::now(), 50, None)
    }
    pub fn queue_at(
        &self,
        worker: &str,
        now: DateTime<Utc>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<List<QueueItem>, ApiError> {
        use crate::wire::{Cursor, ListQuery};
        let page = ListQuery {
            limit: Some(limit),
            cursor: cursor.map(str::to_owned),
        }
        .page()
        .map_err(|_| error("invalid_pagination"))?;
        let start = page
            .offset_or_key
            .as_ref()
            .map(|c| c.offset_value().ok_or_else(|| error("invalid_cursor")))
            .transpose()?
            .unwrap_or(0);
        let mut s = self.inner.lock().unwrap();
        expire(&mut s, now);
        let entries = eligible(&s, worker)?;
        let total = entries.len();
        let items = entries
            .into_iter()
            .skip(start)
            .take(limit)
            .map(|e| QueueItem {
                body: Some(e.item.body.clone()),
                source: e.item.source.clone(),
                id: e.item.id.clone(),
                title: e.item.title.clone(),
                summary: e.item.summary.clone(),
                kind: e.item.kind,
                labels: e.item.labels.clone(),
                priority: e.item.priority,
                enqueued_at: e.enqueued_at,
                assigned_to_me: e.item.affinity.required.iter().any(|m| {
                    m.key == "owner"
                        && m.op == crate::MatchOp::In
                        && m.matches(&s.workers[worker].server_labels)
                }),
            })
            .collect::<Vec<_>>();
        let end = start.saturating_add(items.len());
        Ok(List {
            items,
            next_cursor: (end < total).then(|| Cursor::offset(end).to_string()),
        })
    }
    pub fn claim(
        &self,
        worker: &str,
        item_id: Option<&str>,
        now: DateTime<Utc>,
        ttl: Duration,
    ) -> Result<Option<Lease>, ApiError> {
        let mut s = self.inner.lock().unwrap();
        expire(&mut s, now);
        let id = if let Some(id) = item_id {
            let entry = s
                .items
                .iter()
                .find(|e| e.item.id == id)
                .ok_or_else(|| error("not_found"))?;
            if entry.completed || s.leases.values().any(|l| l.lease.item.id == id) {
                return Err(error("taken"));
            }
            if !eligible(&s, worker)?.iter().any(|e| e.item.id == id) {
                return Err(error("ineligible"));
            }
            id.to_owned()
        } else {
            match eligible(&s, worker)?.first() {
                Some(e) => e.item.id.clone(),
                None => return Ok(None),
            }
        };
        let w = &s.workers[worker];
        if s.leases.values().filter(|l| l.worker_id == worker).count() >= w.request.capacity {
            return Err(error("capacity"));
        }
        let supports_terminal = w.request.features.iter().any(|f| f == features::TERMINAL);
        // IDs are opaque and a fresh ID plus epoch fences every re-issue.
        s.next_id += 1;
        let lease_id = format!("server_{}", s.next_id);
        let e = s.items.iter_mut().find(|e| e.item.id == id).unwrap();
        e.epoch += 1;
        let lease = Lease {
            id: e.next_lease_id.take().unwrap_or(lease_id),
            epoch: e.epoch,
            expires_at: now + ttl,
            item: e.item.clone(),
            terminal: e.terminal && supports_terminal,
            remote_input: e.terminal && supports_terminal && e.remote_input,
        };
        s.events.insert(lease.id.clone(), BTreeMap::new());
        s.leases.insert(
            lease.id.clone(),
            ActiveLease {
                lease: lease.clone(),
                worker_id: worker.into(),
                cancel: None,
                heartbeat: None,
                outbound: broadcast::channel(128).0,
            },
        );
        Ok(Some(lease))
    }
    pub fn heartbeat(
        &self,
        id: &str,
        epoch: u64,
        now: DateTime<Utc>,
        ttl: Duration,
        req: &HeartbeatRequest,
    ) -> Result<HeartbeatResponse, ApiError> {
        let mut s = self.inner.lock().unwrap();
        expire(&mut s, now);
        let l = live(&mut s, id, epoch)?;
        l.lease.expires_at = now + ttl;
        l.heartbeat = Some(req.clone());
        Ok(HeartbeatResponse {
            expires_at: l.lease.expires_at,
            cancel: l.cancel.is_some(),
            cancel_reason: l.cancel.clone(),
        })
    }
    pub fn events(
        &self,
        id: &str,
        epoch: u64,
        now: DateTime<Utc>,
        events: Vec<Value>,
    ) -> Result<usize, ApiError> {
        let mut s = self.inner.lock().unwrap();
        expire(&mut s, now);
        live(&mut s, id, epoch)?;
        // Validate the whole batch before changing state; do not truncate attempt integers.
        let keys: Vec<_> = events
            .iter()
            .map(|e| {
                serde_json::from_value::<EventEnvelope>(e.clone())
                    .map(|e| (e.attempt, e.sequence))
                    .map_err(|_| error("invalid_event"))
            })
            .collect::<Result<_, _>>()?;
        if s.fault == Some(MockFault::DuplicateEvents) {
            let stored = s.events.get_mut(id).unwrap();
            let count = events.len();
            for event in events {
                stored.insert((u32::MAX, stored.len() as u64), event);
            }
            return Ok(count);
        }
        let stored = s.events.get_mut(id).unwrap();
        let mut accepted = 0;
        for (key, event) in keys.into_iter().zip(events) {
            if let std::collections::btree_map::Entry::Vacant(slot) = stored.entry(key) {
                slot.insert(event);
                accepted += 1;
            }
        }
        Ok(accepted)
    }
    pub fn result(
        &self,
        id: &str,
        epoch: u64,
        now: DateTime<Utc>,
        result: LeaseResult<Value>,
    ) -> Result<(), ApiError> {
        let mut s = self.inner.lock().unwrap();
        expire(&mut s, now);
        let item = live(&mut s, id, epoch)?.lease.item.id.clone();
        s.items
            .iter_mut()
            .find(|e| e.item.id == item)
            .unwrap()
            .completed = true;
        s.results.insert(item, result);
        s.leases.remove(id);
        drop(s);
        self.changed.notify_waiters();
        Ok(())
    }
    pub fn release(
        &self,
        id: &str,
        epoch: u64,
        now: DateTime<Utc>,
        req: ReleaseRequest<Value>,
    ) -> Result<(), ApiError> {
        let mut s = self.inner.lock().unwrap();
        expire(&mut s, now);
        let l = live(&mut s, id, epoch)?;
        let item = l.lease.item.id.clone();
        let worker = l.worker_id.clone();
        if req.reason == ReleaseReason::Ineligible {
            s.items
                .iter_mut()
                .find(|e| e.item.id == item)
                .unwrap()
                .excluded
                .insert(worker);
        }
        s.releases.insert(id.into(), req);
        s.leases.remove(id);
        drop(s);
        self.changed.notify_waiters();
        Ok(())
    }
    pub fn request_cancel(&self, id: &str, reason: &str) {
        if let Some(l) = self.inner.lock().unwrap().leases.get_mut(id) {
            l.cancel = Some(reason.into());
        }
    }
    pub fn heartbeat_for(&self, id: &str) -> Option<HeartbeatRequest> {
        self.inner
            .lock()
            .unwrap()
            .leases
            .get(id)
            .and_then(|l| l.heartbeat.clone())
    }
    pub fn result_for(&self, item: &str) -> Option<LeaseResult<Value>> {
        self.inner.lock().unwrap().results.get(item).cloned()
    }
    pub fn release_for(&self, id: &str) -> Option<ReleaseRequest<Value>> {
        self.inner.lock().unwrap().releases.get(id).cloned()
    }
    pub fn events_for(&self, id: &str) -> Vec<Value> {
        self.inner
            .lock()
            .unwrap()
            .events
            .get(id)
            .map(|e| e.values().cloned().collect())
            .unwrap_or_default()
    }
    pub fn event_count(&self, id: &str) -> Option<usize> {
        self.inner.lock().unwrap().events.get(id).map(BTreeMap::len)
    }
    pub fn worker_id_for_lease(&self, id: &str) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .leases
            .get(id)
            .map(|l| l.worker_id.clone())
    }
    pub fn expire_leases(&self) {
        self.inner.lock().unwrap().leases.clear();
        self.changed.notify_waiters();
    }
    /// Conformance admin hook: expire a lease and reuse its ID on the next
    /// claim. Its epoch still increases, so fencing cannot rely on fresh IDs.
    pub fn expire_lease_reusing_id(&self, id: &str) {
        let mut s = self.inner.lock().unwrap();
        if let Some(lease) = s.leases.remove(id) {
            if let Some(item) = s
                .items
                .iter_mut()
                .find(|e| e.item.id == lease.lease.item.id)
            {
                item.next_lease_id = Some(id.to_owned());
            }
        }
        drop(s);
        self.changed.notify_waiters();
    }

    pub fn set_credentials(&self, item: &str, creds: Credentials) {
        let mut s = self.inner.lock().unwrap();
        if let Some(e) = s.items.iter_mut().find(|e| e.item.id == item) {
            e.item.repo.credentials = Some(creds.clone());
        }
        for l in s.leases.values_mut().filter(|l| l.lease.item.id == item) {
            l.lease.item.repo.credentials = Some(creds.clone());
        }
    }
    /// Inject a handshake-only refusal without changing heartbeat ownership.
    pub fn refuse_terminal(&self, item: &str, code: &str) {
        self.inner
            .lock()
            .unwrap()
            .terminal_errors
            .insert(item.into(), code.into());
    }
    pub fn set_terminal(&self, item: &str, terminal: bool, remote_input: bool) {
        if let Some(e) = self
            .inner
            .lock()
            .unwrap()
            .items
            .iter_mut()
            .find(|e| e.item.id == item)
        {
            e.terminal = terminal;
            e.remote_input = remote_input;
        }
    }
    pub fn terminal_frames(&self, id: &str) -> Vec<LeaseTerminalFrame> {
        self.inner
            .lock()
            .unwrap()
            .frames
            .get(id)
            .cloned()
            .unwrap_or_default()
    }
    pub fn send_terminal(&self, id: &str, frame: LeaseTerminalFrame) -> Result<(), ApiError> {
        let mut s = self.inner.lock().unwrap();
        expire(&mut s, Utc::now());
        let l = s.leases.get(id).ok_or_else(|| error("lease_lost"))?;
        if !l.lease.remote_input
            && matches!(
                frame.frame,
                crate::terminal::TerminalFrame::Input { .. }
                    | crate::terminal::TerminalFrame::Resize { .. }
                    | crate::terminal::TerminalFrame::TakeControl
            )
        {
            return Err(error("remote_input_refused"));
        }
        l.outbound
            .send(frame)
            .map_err(|_| error("terminal_not_connected"))?;
        Ok(())
    }
    pub fn link(&self) -> LinkStart {
        let mut s = self.inner.lock().unwrap();
        s.next_id += 1;
        let id = format!("link_{}", s.next_id);
        s.links.insert(
            id.clone(),
            Link {
                labels: BTreeMap::new(),
                refresh: None,
                denied: false,
                expires_at: Utc::now() + Duration::minutes(10),
                revoked: false,
                worker: None,
            },
        );
        LinkStart {
            user_code: id.clone(),
            verification_uri: "https://example.invalid/approve".into(),
            link_id: id,
            interval_ms: 100,
        }
    }
    pub fn approve_link(&self, id: &str, labels: BTreeMap<String, String>) {
        let mut s = self.inner.lock().unwrap();
        s.server_owned.extend(labels.keys().cloned());
        if let Some(l) = s.links.get_mut(id) {
            l.labels = labels.clone();
            l.refresh.get_or_insert_with(secret);
            let worker = l.worker.clone();
            if let Some(w) = worker.and_then(|id| s.workers.get_mut(&id)) {
                w.server_labels = labels;
            }
        }
        refresh_labels(&mut s);
        drop(s);
        self.changed.notify_waiters();
    }

    pub fn deny_link(&self, id: &str) {
        if let Some(l) = self.inner.lock().unwrap().links.get_mut(id) {
            l.denied = true;
        }
    }
    pub fn expire_link(&self, id: &str) {
        if let Some(l) = self.inner.lock().unwrap().links.get_mut(id) {
            l.expires_at = Utc::now();
        }
    }
    pub fn revoke(&self, refresh: &str) {
        for l in self
            .inner
            .lock()
            .unwrap()
            .links
            .values_mut()
            .filter(|l| l.refresh.as_deref() == Some(refresh))
        {
            l.revoked = true;
        }
    }
    pub fn invalidate_access_tokens(&self) {
        self.inner.lock().unwrap().access.clear();
    }
    pub fn token(&self, refresh: &str) -> Result<AccessToken, ApiError> {
        let mut s = self.inner.lock().unwrap();
        let id = s
            .links
            .iter()
            .find(|(_, l)| l.refresh.as_deref() == Some(refresh) && !l.revoked && !l.denied)
            .map(|(id, _)| id.clone())
            .ok_or_else(|| error("unauthorized"))?;
        let token = secret();
        let expires = Utc::now() + Duration::minutes(5);
        s.access.insert(token.clone(), (id, expires));
        Ok(AccessToken {
            access_token: token,
            expires_at: Some(expires),
        })
    }
    pub async fn serve(self: Arc<Self>) -> Result<ServingMock, std::io::Error> {
        http::serve(self).await
    }

    /// Serve on a listener already owned by the caller. No port reservation
    /// is released between allocation and serving.
    pub async fn serve_on(
        self: Arc<Self>,
        listener: tokio::net::TcpListener,
    ) -> Result<ServingMock, std::io::Error> {
        http::serve_on(self, listener).await
    }
}
fn secret() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
fn register_worker(
    s: &mut State,
    id: String,
    request: RegisterRequest,
    server_labels: BTreeMap<String, String>,
) -> RegisterResponse {
    s.server_owned.extend(server_labels.keys().cloned());
    s.workers.insert(
        id.clone(),
        Worker {
            request,
            server_labels: server_labels.clone(),
            labels: BTreeMap::new(),
        },
    );
    refresh_labels(s);
    RegisterResponse {
        worker_id: id,
        server_labels,
        heartbeat_ms: 1000,
        poll_ms: 100,
    }
}

// Ownership survives removal of an assignment. Existing registrations lose
// conflicting worker claims as soon as a key becomes server-owned.
fn refresh_labels(s: &mut State) {
    for worker in s.workers.values_mut() {
        worker.labels = worker
            .request
            .labels
            .iter()
            .filter(|(key, _)| !s.server_owned.contains(*key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        worker.labels.extend(worker.server_labels.clone());
    }
}

#[cfg(test)]
mod state_model;
