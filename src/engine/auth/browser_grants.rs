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
//! * each live grant has a [`GrantWatch`] that WebSocket tasks select on; it
//!   fires on revocation (with the reason) or at expiry;
//! * the store is separate from `ApiKeyStore` and from any work-server
//!   credential: nothing else is ever consulted for a browser bearer.
//!
//! A daemon stop revokes every grant, so opening the store discards records
//! a previous daemon left behind (for example after a crash).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use crate::data::browser_auth::{
    BrowserScope, BrowserSecret, GrantRevocationReason, GRANT_TOKEN_PREFIX, GRANT_TTL,
};
use crate::data::fs::browser_grant_store::{BrowserGrantFile, BrowserGrantRecord, MAX_LABEL_CHARS};
use crate::data::ids::BrowserGrantId;
use crate::engine::auth::constant_time_hex_eq;
use crate::engine::auth::device_flow::{generate_secret, hash_secret};
use crate::engine::error::EngineError;

/// How often a grant's `last_used_at` reaches the disk at most.
pub const LAST_USED_FLUSH_INTERVAL: Duration = Duration::from_secs(60);

const SENTINEL_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

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

/// Fires when a grant stops being valid.
#[derive(Debug, Clone)]
pub struct GrantWatch {
    token: CancellationToken,
    reason: Arc<OnceLock<GrantRevocationReason>>,
    expires_at: DateTime<Utc>,
}

impl GrantWatch {
    /// Resolves when the grant is revoked or expires, with the reason.
    pub async fn ended(&self) -> GrantRevocationReason {
        let remaining = (self.expires_at - Utc::now())
            .to_std()
            .unwrap_or(Duration::ZERO);
        tokio::select! {
            _ = self.token.cancelled() => {
                self.reason.get().copied().unwrap_or(GrantRevocationReason::Deleted)
            }
            _ = tokio::time::sleep(remaining) => GrantRevocationReason::Expired,
        }
    }

    /// Already revoked?
    pub fn is_revoked(&self) -> bool {
        self.token.is_cancelled()
    }
}

#[derive(Debug)]
struct Live {
    token: CancellationToken,
    reason: Arc<OnceLock<GrantRevocationReason>>,
}

impl Live {
    fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            reason: Arc::new(OnceLock::new()),
        }
    }
    fn revoke(&self, reason: GrantRevocationReason) {
        let _ = self.reason.set(reason);
        self.token.cancel();
    }
}

/// The daemon's browser grants. Shared as `Arc<BrowserGrantStore>`.
#[derive(Debug)]
pub struct BrowserGrantStore {
    path: PathBuf,
    file: RwLock<BrowserGrantFile>,
    live: Mutex<HashMap<BrowserGrantId, Live>>,
    last_used_flush: Mutex<HashMap<BrowserGrantId, Instant>>,
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
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Mint a grant for `origin` on `listener`. Returns the record and the
    /// token (`awb_` + 43 base64url); the token exists nowhere else.
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
        next.grants.retain(|g| g.expires_at > now);
        next.grants.push(record.clone());
        next.store(&self.path)?;
        *file = next;
        self.live_map().insert(record.id.clone(), Live::new());
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
                    && record.expires_at > now
                    && self.is_live(&record.id) =>
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
            .is_some_and(|r| r.origin == origin && r.listener == listener && r.expires_at > now)
            && self.is_live(id)
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
        Ok(!self.revoke_where(|g| &g.id == id, reason)?.is_empty())
    }

    /// Revoke every grant matching `pred`; returns their ids.
    pub fn revoke_where(
        &self,
        pred: impl Fn(&BrowserGrantRecord) -> bool,
        reason: GrantRevocationReason,
    ) -> Result<Vec<BrowserGrantId>, EngineError> {
        let mut file = self.write();
        let revoked: Vec<BrowserGrantId> = file
            .grants
            .iter()
            .filter(|g| pred(g))
            .map(|g| g.id.clone())
            .collect();
        if revoked.is_empty() {
            return Ok(revoked);
        }
        file.grants.retain(|g| !revoked.contains(&g.id));
        {
            let mut live = self.live_map();
            for id in &revoked {
                if let Some(l) = live.remove(id) {
                    l.revoke(reason);
                }
            }
        }
        {
            let mut flush = self.flush_times();
            for id in &revoked {
                flush.remove(id);
            }
        }
        file.store(&self.path)?;
        Ok(revoked)
    }

    /// Live grants bound to `listener` (for `daemon status`).
    pub fn count_for_listener(&self, listener: &str, now: DateTime<Utc>) -> usize {
        self.read()
            .grants
            .iter()
            .filter(|g| g.listener == listener && g.expires_at > now)
            .count()
    }

    /// A watch that fires when `id` is revoked or expires; `None` if `id` is
    /// not a live grant.
    pub fn watch(&self, id: &BrowserGrantId) -> Option<GrantWatch> {
        let record = self.get(id)?;
        let live = self.live_map();
        let l = live.get(id)?;
        Some(GrantWatch {
            token: l.token.clone(),
            reason: l.reason.clone(),
            expires_at: record.expires_at,
        })
    }

    fn is_live(&self, id: &BrowserGrantId) -> bool {
        self.live_map()
            .get(id)
            .is_some_and(|l| !l.token.is_cancelled())
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
    fn live_map(&self) -> std::sync::MutexGuard<'_, HashMap<BrowserGrantId, Live>> {
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

    const ORIGIN: &str = "https://awman.prettysmart.dev";

    fn store_in(dir: &Path) -> Arc<BrowserGrantStore> {
        BrowserGrantStore::open(dir.join("daemon").join("browser_grants.json")).unwrap()
    }

    fn bearer(token: &BrowserSecret) -> String {
        format!("Bearer {}", token.expose())
    }

    #[test]
    fn a_created_grant_verifies_only_for_its_origin_listener_and_lifetime() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let now = Utc::now();
        let (record, token) = store
            .create(
                ORIGIN,
                "loopback",
                BrowserScope::Engine,
                Some("Firefox"),
                now,
            )
            .unwrap();
        assert!(token.expose().starts_with("awb_"));
        assert_eq!(token.expose().len(), 4 + 43);
        assert_eq!(
            record.expires_at - record.created_at,
            chrono::Duration::hours(12)
        );
        let header = bearer(&token);
        assert_eq!(
            store.verify(Some(&header), ORIGIN, "loopback", now),
            GrantVerdict::Accepted {
                id: record.id.clone()
            }
        );
        assert_eq!(
            store.verify(Some(&header), "http://localhost:3000", "loopback", now),
            GrantVerdict::Invalid
        );
        assert_eq!(
            store.verify(Some(&header), ORIGIN, "other", now),
            GrantVerdict::Invalid
        );
        assert_eq!(
            store.verify(
                Some(&header),
                ORIGIN,
                "loopback",
                now + chrono::Duration::hours(12)
            ),
            GrantVerdict::Invalid
        );
        assert_eq!(
            store.verify(None, ORIGIN, "loopback", now),
            GrantVerdict::Missing
        );
        assert_eq!(
            store.verify(Some("Bearer awb_wrong"), ORIGIN, "loopback", now),
            GrantVerdict::Invalid
        );
    }

    #[test]
    fn the_token_is_never_written_to_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let (_, token) = store
            .create(ORIGIN, "loopback", BrowserScope::Engine, None, Utc::now())
            .unwrap();
        let disk = std::fs::read_to_string(store.path()).unwrap();
        assert!(!disk.contains(token.expose()));
        assert!(!disk.contains(&token.expose()[4..]));
    }

    #[test]
    fn delete_revokes_in_memory_even_when_the_disk_write_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let now = Utc::now();
        let (record, token) = store
            .create(ORIGIN, "loopback", BrowserScope::Engine, None, now)
            .unwrap();
        let watch = store.watch(&record.id).unwrap();
        // Make the directory unwritable for the atomic replace.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = store.path().parent().unwrap();
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
            let result = store.delete(&record.id, GrantRevocationReason::Deleted);
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            // Root can write anyway; either way the grant is refused.
            let _ = result;
        }
        #[cfg(not(unix))]
        store
            .delete(&record.id, GrantRevocationReason::Deleted)
            .unwrap();
        assert_eq!(
            store.verify(Some(&bearer(&token)), ORIGIN, "loopback", now),
            GrantVerdict::Invalid
        );
        assert!(watch.is_revoked());
        assert!(!store
            .delete(&record.id, GrantRevocationReason::Deleted)
            .unwrap());
    }

    #[tokio::test]
    async fn revoke_where_fires_watches_with_the_reason_and_counts_follow() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let now = Utc::now();
        let (a, _) = store
            .create(ORIGIN, "loopback", BrowserScope::Engine, None, now)
            .unwrap();
        let (b, _) = store
            .create(
                "http://localhost:3000",
                "loopback",
                BrowserScope::Engine,
                None,
                now,
            )
            .unwrap();
        store
            .create(ORIGIN, "other", BrowserScope::Engine, None, now)
            .unwrap();
        assert_eq!(store.count_for_listener("loopback", now), 2);
        let watch = store.watch(&b.id).unwrap();
        let revoked = store
            .revoke_where(
                |g| g.listener == "loopback" && g.origin == "http://localhost:3000",
                GrantRevocationReason::OriginRemoved,
            )
            .unwrap();
        assert_eq!(revoked, vec![b.id.clone()]);
        assert_eq!(watch.ended().await, GrantRevocationReason::OriginRemoved);
        assert_eq!(store.count_for_listener("loopback", now), 1);
        assert!(store.watch(&a.id).is_some());
        assert_eq!(store.list(now).len(), 2);
    }

    #[test]
    fn reopening_discards_a_previous_daemons_grants() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        store
            .create(ORIGIN, "loopback", BrowserScope::Engine, None, Utc::now())
            .unwrap();
        drop(store);
        let store = store_in(tmp.path());
        assert!(store.list(Utc::now()).is_empty());
    }

    #[test]
    fn labels_are_sanitized_and_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let long = format!("Fire\u{1b}fox{}", "x".repeat(100));
        let (record, _) = store
            .create(
                ORIGIN,
                "loopback",
                BrowserScope::Engine,
                Some(&long),
                Utc::now(),
            )
            .unwrap();
        let label = record.label.unwrap();
        assert!(!label.contains('\u{1b}'));
        assert_eq!(label.chars().count(), 64);
    }

    #[test]
    fn an_api_key_shaped_bearer_never_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        store
            .create(ORIGIN, "loopback", BrowserScope::Engine, None, Utc::now())
            .unwrap();
        assert_eq!(
            store.verify(
                Some(&format!("Bearer {}", "ab".repeat(32))),
                ORIGIN,
                "loopback",
                Utc::now()
            ),
            GrantVerdict::Invalid
        );
    }
}
