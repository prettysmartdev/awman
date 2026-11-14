//! `ApiKeyStore` — the network listener's named API keys.
//!
//! Keys are credentials for the optional network listener only; the local
//! listener authenticates its caller by OS identity and never consults this
//! store. The store keeps only SHA-256 hashes on disk
//! (`<data>/daemon/api_keys.json`, mode `0600`) and in memory. The plaintext
//! is returned once, by [`ApiKeyStore::create`], and is never recoverable.
//!
//! Security properties held here, not in a router:
//!
//! * verification hashes the presented key once and compares it against
//!   *every* stored hash in constant time, without an early exit, so comparison work does not stop at the matching key. Total work still
//!   scales with the number of configured keys;
//! * an absent or malformed `Authorization` header costs the same as a wrong
//!   key;
//! * with zero keys every request is rejected ([`KeyVerdict::NoKeys`]);
//! * delete revokes in memory before it touches the disk, so a revoked key is
//!   refused from the next request even if persisting fails.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use chrono::Utc;

use crate::data::config::fields::is_valid_map_name;
use crate::data::fs::api_key_store::{ApiKeyFile, ApiKeyRecord};
use crate::data::ids::KeyId;
use crate::engine::auth::{constant_time_hex_eq, generate_key, hash_key, ApiKey};
use crate::engine::error::EngineError;

/// How often a key's `last_used_at` is written to disk at most. The in-memory
/// value is always current, so `list` shows the latest use.
pub const LAST_USED_FLUSH_INTERVAL: Duration = Duration::from_secs(60);

/// Compared against when the store is empty, so an empty store runs the same
/// hash-and-compare path as a populated one.
const SENTINEL_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// The result of checking a request's `Authorization` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyVerdict {
    /// The header carried a current key.
    Accepted { id: KeyId },
    /// Missing, malformed or unknown key (including a deleted one).
    Rejected,
    /// The store holds no keys, so nothing can be accepted.
    NoKeys,
}

/// The daemon's named API keys. Shared as `Arc<ApiKeyStore>`.
#[derive(Debug)]
pub struct ApiKeyStore {
    path: PathBuf,
    file: RwLock<ApiKeyFile>,
    last_used_flush: Mutex<HashMap<KeyId, Instant>>,
}

impl ApiKeyStore {
    /// Open the store at `path` (normally `DaemonPaths::api_keys_file()`).
    /// An absent file is an empty store; an unsafe or malformed file is an
    /// error, so the daemon refuses to start rather than guess.
    pub fn open(path: PathBuf) -> Result<Arc<Self>, EngineError> {
        let file = ApiKeyFile::load(&path)?;
        Ok(Arc::new(Self {
            path,
            file: RwLock::new(file),
            last_used_flush: Mutex::new(HashMap::new()),
        }))
    }

    /// Where the hashes live.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Mint a new named key. `name` must match `MAP_NAME`
    /// (`[a-z0-9][a-z0-9_-]{0,62}`) and be unique among current keys.
    ///
    /// Returns the stored record and the plaintext key (32 random bytes as
    /// 64 lowercase hex). The plaintext is not kept anywhere; this is the only
    /// time it exists outside the caller. Nothing is added unless the file
    /// was written.
    pub fn create(&self, name: &str) -> Result<(ApiKeyRecord, ApiKey), EngineError> {
        if !is_valid_map_name(name) {
            return Err(EngineError::Auth(format!(
                "invalid key name {name:?}: use 1-63 characters of a-z, 0-9, '_' or '-', \
                 starting with a letter or digit"
            )));
        }
        let key = generate_key()?;
        let record = ApiKeyRecord {
            id: KeyId::new(),
            name: name.to_string(),
            hash: hash_key(key.as_str()),
            created_at: Utc::now(),
            last_used_at: None,
        };
        let mut file = self.write();
        if file.keys.iter().any(|k| k.name == name) {
            return Err(EngineError::Auth(format!(
                "a key named {name:?} already exists"
            )));
        }
        let mut next = file.clone();
        next.keys.push(record.clone());
        next.store(&self.path)?;
        *file = next;
        Ok((record, key))
    }

    /// Every key, oldest first. Records carry the hash, never the key;
    /// callers project them to `{id, name, created_at, last_used_at}` before
    /// anything leaves the process.
    pub fn list(&self) -> Vec<ApiKeyRecord> {
        let mut keys = self.read().keys.clone();
        keys.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        keys
    }

    /// One key by id.
    pub fn get(&self, id: &KeyId) -> Option<ApiKeyRecord> {
        self.read().keys.iter().find(|k| &k.id == id).cloned()
    }

    /// Revoke a key. `Ok(false)` when no key has that id. The key is refused
    /// from the moment this returns, even when persisting the file fails (the
    /// error is still returned so the caller can report it).
    pub fn delete(&self, id: &KeyId) -> Result<bool, EngineError> {
        let mut file = self.write();
        let before = file.keys.len();
        file.keys.retain(|k| &k.id != id);
        if file.keys.len() == before {
            return Ok(false);
        }
        self.flush_times().remove(id);
        file.store(&self.path)?;
        Ok(true)
    }

    /// How many keys exist. Zero means the network listener rejects every
    /// request.
    pub fn count(&self) -> usize {
        self.read().keys.len()
    }

    /// Check an `Authorization` header value: `Bearer <key>` (prefix matched
    /// case-insensitively) or a bare key. `None` is a request without one.
    ///
    /// The presented key is hashed once and compared, in constant time,
    /// against every stored hash with no early exit; an empty store compares
    /// against a sentinel. On success `last_used_at` is set to now in memory
    /// and written to disk at most once per [`LAST_USED_FLUSH_INTERVAL`] per
    /// key (a failed flush never fails the request).
    pub fn verify(&self, authorization_header: Option<&str>) -> KeyVerdict {
        let presented = hash_key(strip_bearer(authorization_header.unwrap_or("")));
        let matched = {
            let file = self.read();
            if file.keys.is_empty() {
                // Same work as a one-key store; the result is discarded.
                let _ = constant_time_hex_eq(&presented, SENTINEL_HASH);
                return KeyVerdict::NoKeys;
            }
            let mut matched: Option<KeyId> = None;
            for record in &file.keys {
                // Evaluate every comparison; never short-circuit.
                let eq = constant_time_hex_eq(&presented, &record.hash);
                if eq && matched.is_none() {
                    matched = Some(record.id.clone());
                }
            }
            matched
        };
        match matched {
            Some(id) => {
                self.touch(&id);
                KeyVerdict::Accepted { id }
            }
            None => KeyVerdict::Rejected,
        }
    }

    /// Record a use of `id`: in memory always, on disk when the last flush
    /// for this key is older than the interval.
    fn touch(&self, id: &KeyId) {
        let mut file = self.write();
        let Some(record) = file.keys.iter_mut().find(|k| &k.id == id) else {
            // Deleted between the compare and now: nothing to record.
            return;
        };
        record.last_used_at = Some(Utc::now());
        let now = Instant::now();
        let due = {
            let mut flushed = self.flush_times();
            let due = flushed
                .get(id)
                .is_none_or(|t| now.duration_since(*t) >= LAST_USED_FLUSH_INTERVAL);
            if due {
                flushed.insert(id.clone(), now);
            }
            due
        };
        if due {
            if let Err(e) = file.store(&self.path) {
                tracing::warn!(error = %e, "could not record API key last use");
            }
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, ApiKeyFile> {
        self.file.read().unwrap_or_else(|p| p.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, ApiKeyFile> {
        self.file.write().unwrap_or_else(|p| p.into_inner())
    }

    fn flush_times(&self) -> std::sync::MutexGuard<'_, HashMap<KeyId, Instant>> {
        self.last_used_flush
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }
}

/// `Bearer <key>` → `<key>`; anything else is taken as the bare key.
fn strip_bearer(value: &str) -> &str {
    match value.get(..7) {
        Some(prefix) if prefix.eq_ignore_ascii_case("bearer ") => value[7..].trim(),
        _ => value.trim(),
    }
}
