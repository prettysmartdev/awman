//! Keyed durable repo context: typed list/get/put/delete for the core
//! `/v1/contexts/{key}` routes.
//!
//! Layer 2 owns key parsing, path validation (through Layer 0's
//! `ContextKey`/`ContextRelPath`), `If-Match` handling and the concurrency
//! rule: API writes to one key are serialized in-process (one daemon per
//! host), so the precondition check and the rename are atomic among API
//! writers. Agents writing the mounted directory from a container are outside
//! that lock; there the last writer wins, and `context_changes` lets a client
//! detect it.
//!
//! Errors never carry host paths: storage failures become a fixed message and
//! the underlying error goes to the log.
use super::ContextApiError;
use crate::data::{
    context::{
        ContextFileMeta, ContextKey, ContextRelPath, ContextStoreError, Precondition,
        RepoContextStore, WriteOutcome,
    },
    fs::context_dirs::ContextDirResolver,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc, Mutex, OnceLock},
};

/// Capability switch: flip only when keyed push context behavior is complete.
pub const SUPPORTED: bool = true;

/// Per-key write locks. Process-wide, because a service is built per request
/// and two services over one directory must still exclude each other. Keyed
/// by the resolved directory, so two daemons in one process (tests) with
/// different homes never share a lock by accident.
pub struct KeyLocks {
    map: &'static Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl KeyLocks {
    /// Entries kept before idle locks are pruned.
    const PRUNE_AT: usize = 1024;

    fn shared() -> Self {
        static MAP: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
        Self {
            map: MAP.get_or_init(Default::default),
        }
    }

    fn for_dir(&self, dir: &std::path::Path) -> Arc<Mutex<()>> {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= Self::PRUNE_AT {
            map.retain(|_, lock| Arc::strong_count(lock) > 1);
        }
        Arc::clone(map.entry(dir.to_path_buf()).or_default())
    }
}

pub struct RepoContextService {
    resolver: ContextDirResolver,
    locks: KeyLocks,
}

impl RepoContextService {
    pub fn new(resolver: ContextDirResolver) -> Self {
        Self {
            resolver,
            locks: KeyLocks::shared(),
        }
    }

    /// Every regular file under the key, sorted by path. A key with no
    /// directory yet is an empty list, not an error.
    pub fn list(
        &self,
        key: &str,
        cancelled: &AtomicBool,
    ) -> Result<Vec<ContextFileMeta>, ContextApiError> {
        let store = self.store(key)?;
        store.list(cancelled).map_err(store_error)
    }

    pub fn get(
        &self,
        key: &str,
        path: &str,
    ) -> Result<(Vec<u8>, ContextFileMeta), ContextApiError> {
        let store = self.store(key)?;
        let path = parse_path(path)?;
        store.read(&path).map_err(store_error)
    }

    /// Create or replace one file. `if_match` is the raw `If-Match` header:
    /// a strong ETag (the file's sha256, quotes optional) or `*` (the file
    /// must exist). A weak ETag is refused.
    pub fn put(
        &self,
        key: &str,
        path: &str,
        bytes: &[u8],
        if_match: Option<&str>,
    ) -> Result<(ContextFileMeta, WriteOutcome), ContextApiError> {
        let store = self.store(key)?;
        let path = parse_path(path)?;
        let condition = parse_if_match(if_match)?;
        let lock = self.locks.for_dir(store.dir());
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        let precondition = resolve_condition(&store, &path, condition)?;
        store
            .write(&path, bytes, &precondition)
            .map_err(store_error)
    }

    /// Delete one file. A missing file is `NotFound`; `if_match` as for `put`.
    pub fn delete(
        &self,
        key: &str,
        path: &str,
        if_match: Option<&str>,
    ) -> Result<(), ContextApiError> {
        let store = self.store(key)?;
        let path = parse_path(path)?;
        let condition = parse_if_match(if_match)?;
        let lock = self.locks.for_dir(store.dir());
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        let precondition = resolve_condition(&store, &path, condition)?;
        store.delete(&path, &precondition).map_err(store_error)
    }

    fn store(&self, key: &str) -> Result<RepoContextStore, ContextApiError> {
        let key = ContextKey::parse(key).map_err(|e| ContextApiError::InvalidKey(e.to_string()))?;
        RepoContextStore::for_key(&self.resolver, &key).map_err(|error| {
            tracing::warn!(%error, key = key.as_str(), "Keyed context directory refused");
            ContextApiError::Data(error)
        })
    }
}

fn parse_path(raw: &str) -> Result<ContextRelPath, ContextApiError> {
    ContextRelPath::parse(raw).map_err(|e| ContextApiError::InvalidPath(e.to_string()))
}

/// What an `If-Match` header asks for.
#[derive(Debug, PartialEq, Eq)]
enum Condition {
    None,
    Exists,
    Sha256(String),
}

fn parse_if_match(raw: Option<&str>) -> Result<Condition, ContextApiError> {
    let Some(raw) = raw.map(str::trim) else {
        return Ok(Condition::None);
    };
    if raw == "*" {
        return Ok(Condition::Exists);
    }
    if raw.starts_with("W/") {
        return Err(ContextApiError::InvalidPath(
            "If-Match must be a strong ETag (the file's sha256)".into(),
        ));
    }
    let tag = raw
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(raw);
    // Anything that is not a sha256 can never match; RFC 9110 answers 412.
    Ok(Condition::Sha256(tag.to_ascii_lowercase()))
}

/// Turn the header into a store precondition. Runs under the key lock, so
/// `*` cannot race another API writer.
fn resolve_condition(
    _store: &RepoContextStore,
    _path: &ContextRelPath,
    condition: Condition,
) -> Result<Precondition, ContextApiError> {
    match condition {
        Condition::None => Ok(Precondition::None),
        Condition::Sha256(hash) => Ok(Precondition::Sha256(hash)),
        Condition::Exists => Ok(Precondition::Exists),
    }
}

/// Fixed, path-free messages for every store failure.
fn store_error(error: ContextStoreError) -> ContextApiError {
    match error {
        ContextStoreError::NotFound => ContextApiError::NotFound("context file not found".into()),
        ContextStoreError::PreconditionFailed { expected, actual } => {
            ContextApiError::PreconditionFailed(match actual {
                Some(actual) => format!(
                    "the file changed: If-Match is \"{expected}\", the current sha256 is \"{actual}\""
                ),
                None => format!("If-Match is \"{expected}\" but the file does not exist"),
            })
        }
        ContextStoreError::TooLarge(message) => ContextApiError::TooLarge(message),
        ContextStoreError::Conflict(message) => ContextApiError::Conflict(message),
        ContextStoreError::Incomplete => {
            ContextApiError::Unavailable("context listing could not complete; retry later".into())
        }
        ContextStoreError::Path(error) => ContextApiError::InvalidPath(error.to_string()),
        // Carries host paths: Layer 3 answers `internal` with a fixed message
        // and never renders this error.
        ContextStoreError::Data(error) => {
            tracing::error!(%error, "Repo context storage failed");
            ContextApiError::Data(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn if_match_forms() {
        assert_eq!(parse_if_match(None).unwrap(), Condition::None);
        assert_eq!(parse_if_match(Some("*")).unwrap(), Condition::Exists);
        assert_eq!(
            parse_if_match(Some("\"ABC\"")).unwrap(),
            Condition::Sha256("abc".into())
        );
        assert_eq!(
            parse_if_match(Some("abc")).unwrap(),
            Condition::Sha256("abc".into())
        );
        assert!(matches!(
            parse_if_match(Some("W/\"abc\"")),
            Err(ContextApiError::InvalidPath(_))
        ));
    }

    #[test]
    fn storage_failures_map_to_internal() {
        let error = store_error(ContextStoreError::Data(crate::data::error::DataError::io(
            "/host/path",
            std::io::Error::other("boom"),
        )));
        assert_eq!(error.code(), "internal");
    }
}
