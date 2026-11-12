//! `SessionManager` — concurrency-safe collection of `Session` values.
//!
//! The CLI uses `SessionManager::in_memory(..)` and creates exactly one session
//! per invocation. The TUI uses `SessionManager::in_memory(..)` and creates one
//! session per tab. The API server uses `SessionManager::with_persistence(...)`
//! and one session per API session.
//!
//! Every manager is built with the owning process's environment snapshot, and
//! a session it opens without an explicit `env` inherits it. An empty snapshot
//! would make `GlobalConfig` ignore a relocated `AWMAN_HOME` and fall
//! back to defaults, which for the builtin runtime means a public network
//! instead of the user's global policy.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock as StdRwLock};

use tokio::sync::RwLock;

use crate::data::config::env::EnvSnapshot;
use crate::data::error::DataError;
use crate::data::session::{GitRootResolver, Session, SessionId, SessionOpenOptions};

/// A session shared by the TUI, API workers, and squad daemon command
/// dispatch.  The manager owns the collection; callers retain this handle
/// while they need to read or update one entry.
pub type ManagedSession = Arc<RwLock<Session>>;

/// Lightweight filesystem resolver used solely while opening a multi-session
/// frontend tab. It does not invoke git; it finds the nearest checkout marker
/// so `Session::open_or_workdir_fallback` can make the one shared non-git
/// decision.
struct DirectoryGitRootResolver;

impl GitRootResolver for DirectoryGitRootResolver {
    fn resolve(&self, working_dir: &std::path::Path) -> Result<PathBuf, DataError> {
        let mut candidate = working_dir;
        loop {
            if candidate.join(".git").exists() {
                return Ok(candidate.to_path_buf());
            }
            let Some(parent) = candidate.parent() else {
                break;
            };
            if parent == candidate {
                break;
            }
            candidate = parent;
        }
        Err(DataError::GitRootNotFound {
            working_dir: working_dir.to_path_buf(),
        })
    }
}

/// Trait implemented by Layer 0's persistence backends for `SessionManager`.
///
/// Higher layers consume `SessionManager`; they never touch `SessionStore`
/// directly. The trait is `Send + Sync` so that the manager can hold an
/// `Arc<dyn SessionStore>` across tasks.
pub trait SessionStore: Send + Sync {
    /// Persist a newly-created session.
    fn upsert(&self, session: &Session) -> Result<(), DataError>;
    /// Mark a session as removed.
    fn remove(&self, id: SessionId) -> Result<(), DataError>;
}

/// In-memory `SessionStore` used by tests and as a default no-op backend.
#[derive(Debug, Default)]
pub struct InMemorySessionStore {
    captured: std::sync::Mutex<Vec<SessionId>>,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn captured_ids(&self) -> Vec<SessionId> {
        self.captured.lock().expect("mutex poisoned").clone()
    }
}

impl SessionStore for InMemorySessionStore {
    fn upsert(&self, session: &Session) -> Result<(), DataError> {
        self.captured
            .lock()
            .expect("mutex poisoned")
            .push(session.id());
        Ok(())
    }

    fn remove(&self, _id: SessionId) -> Result<(), DataError> {
        Ok(())
    }
}

/// Concurrency-safe owner of a collection of `Session` values.
#[derive(Clone)]
pub struct SessionManager {
    // API session ids are SQLite strings. Keep that representation at the
    // ownership boundary instead of maintaining a second API-only map.
    sessions: Arc<StdRwLock<HashMap<String, ManagedSession>>>,
    store: Option<Arc<dyn SessionStore>>,
    /// The owning process's environment: the default for every session this
    /// manager opens.
    env: EnvSnapshot,
}

impl SessionManager {
    /// Construct an in-memory manager with no persistence backend. `env` is
    /// the owning process's environment snapshot (`EnvSnapshot::empty()` only
    /// in tests).
    pub fn in_memory(env: EnvSnapshot) -> Self {
        Self {
            sessions: Arc::new(StdRwLock::new(HashMap::new())),
            store: None,
            env,
        }
    }

    /// Construct a manager backed by the supplied `SessionStore`.
    pub fn with_persistence(store: Arc<dyn SessionStore>, env: EnvSnapshot) -> Self {
        Self {
            sessions: Arc::new(StdRwLock::new(HashMap::new())),
            store: Some(store),
            env,
        }
    }

    /// The environment snapshot sessions opened here inherit.
    pub fn env(&self) -> &EnvSnapshot {
        &self.env
    }

    /// Fill in the manager's environment when the caller supplied none.
    fn with_env(&self, mut opts: SessionOpenOptions) -> SessionOpenOptions {
        if opts.env.is_none() {
            opts.env = Some(self.env.clone());
        }
        opts
    }

    /// Insert a fully-constructed session, returning its id.
    pub fn create(&self, session: Session) -> Result<SessionId, DataError> {
        let id = session.id();
        self.insert_with_key(id.to_string(), session)?;
        Ok(id)
    }

    /// Add a session recovered from persistence under its persisted string
    /// key. The session's in-memory UUID is intentionally independent: old
    /// SQLite rows predate `SessionId` persistence.
    pub fn insert_with_key(
        &self,
        key: impl Into<String>,
        session: Session,
    ) -> Result<ManagedSession, DataError> {
        let key = key.into();
        let session_id = session.id();
        let mut guard = self.sessions.write().expect("session manager poisoned");
        if guard.contains_key(&key) {
            return Err(DataError::SessionIdCollision {
                id: session_id.as_uuid(),
            });
        }
        if let Some(store) = self.store.as_ref() {
            store.upsert(&session)?;
        }
        let session = Arc::new(RwLock::new(session));
        guard.insert(key, Arc::clone(&session));
        Ok(session)
    }

    /// Open a directory session and register it. This is the sole session
    /// creation entry point for multi-session frontends. Its non-git behavior
    /// is deliberately the same `open_or_workdir_fallback` policy everywhere.
    pub fn open_or_create(
        &self,
        dir: PathBuf,
        opts: SessionOpenOptions,
    ) -> Result<SessionId, DataError> {
        let metadata = std::fs::metadata(&dir).map_err(|source| DataError::io(&dir, source))?;
        if !metadata.is_dir() {
            return Err(DataError::InvalidPath {
                path: dir,
                reason: "not a directory".to_string(),
            });
        }
        let resolver = DirectoryGitRootResolver;
        let session = Session::open_or_workdir_fallback(dir, &resolver, self.with_env(opts))?;
        self.create(session)
    }

    /// Open a session under an optional persistence key. API restore and setup
    /// supply their SQLite id; interactive callers use the SessionId string.
    pub fn open_or_create_with_key(
        &self,
        key: Option<String>,
        dir: PathBuf,
        opts: SessionOpenOptions,
    ) -> Result<ManagedSession, DataError> {
        let metadata = std::fs::metadata(&dir).map_err(|source| DataError::io(&dir, source))?;
        if !metadata.is_dir() {
            return Err(DataError::InvalidPath {
                path: dir,
                reason: "not a directory".to_string(),
            });
        }
        let resolver = DirectoryGitRootResolver;
        let session = Session::open_or_workdir_fallback(dir, &resolver, self.with_env(opts))?;
        let key = key.unwrap_or_else(|| session.id().to_string());
        self.insert_with_key(key, session)
    }

    /// Fetch a shared session by its typed in-memory id.
    pub fn get(&self, id: &SessionId) -> Option<ManagedSession> {
        self.get_by_key(&id.to_string())
    }

    /// Fetch a shared session by the string identifier stored in SQLite.
    pub fn get_by_key(&self, id: &str) -> Option<ManagedSession> {
        self.sessions
            .read()
            .expect("session manager poisoned")
            .get(id)
            .cloned()
    }

    /// Mutate a session in place via the supplied closure, persisting on success.
    ///
    /// Replaces the unsafe `&mut Session` borrow that an unguarded `get_mut`
    /// would expose. Higher layers call this when they need to update session
    /// state.
    pub async fn update<F, T>(&self, id: SessionId, f: F) -> Result<T, DataError>
    where
        F: FnOnce(&mut Session) -> T,
    {
        let session = self
            .get(&id)
            .ok_or(DataError::SessionNotFound { id: id.as_uuid() })?;
        let mut guard = session.write().await;
        let result = f(&mut guard);
        if let Some(store) = self.store.as_ref() {
            store.upsert(&guard)?;
        }
        Ok(result)
    }

    /// Snapshot every currently-tracked session.
    pub async fn list(&self) -> Vec<Session> {
        let sessions: Vec<_> = self
            .sessions
            .read()
            .expect("session manager poisoned")
            .values()
            .cloned()
            .collect();
        let mut result = Vec::with_capacity(sessions.len());
        for session in sessions {
            result.push(session.read().await.clone());
        }
        result
    }

    /// Number of sessions currently tracked.
    pub async fn len(&self) -> usize {
        let guard = self.sessions.read().expect("session manager poisoned");
        guard.len()
    }

    /// True when no sessions are tracked.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// Remove the session with the given id.
    pub fn remove(&self, id: &SessionId) -> Result<(), DataError> {
        self.remove_by_key(&id.to_string(), *id)
    }

    /// Remove an entry identified by the string key persisted by SQLite.
    pub fn remove_by_key(&self, key: &str, id: SessionId) -> Result<(), DataError> {
        let mut guard = self.sessions.write().expect("session manager poisoned");
        let removed = guard.remove(key);
        if removed.is_none() {
            return Err(DataError::SessionNotFound { id: id.as_uuid() });
        }
        if let Some(store) = self.store.as_ref() {
            store.remove(id)?;
        }
        Ok(())
    }

    /// True when this manager has a persistence backend attached.
    pub fn has_persistence(&self) -> bool {
        self.store.is_some()
    }
}
