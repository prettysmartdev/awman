//! `SessionManager` — concurrency-safe collection of `Session` values.
//!
//! The CLI uses `SessionManager::in_memory()` and creates exactly one session
//! per invocation. The TUI uses `SessionManager::in_memory()` and creates one
//! session per tab. The headless server uses `SessionManager::with_persistence(...)`
//! and one session per API session.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::data::error::DataError;
use crate::data::session::{Session, SessionId};

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
    sessions: Arc<RwLock<HashMap<SessionId, Session>>>,
    store: Option<Arc<dyn SessionStore>>,
}

impl SessionManager {
    /// Construct an in-memory manager with no persistence backend.
    pub fn in_memory() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            store: None,
        }
    }

    /// Construct a manager backed by the supplied `SessionStore`.
    pub fn with_persistence(store: Arc<dyn SessionStore>) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            store: Some(store),
        }
    }

    /// Insert a fully-constructed session, returning its id.
    pub async fn create(&self, session: Session) -> Result<SessionId, DataError> {
        let id = session.id();
        let mut guard = self.sessions.write().await;
        if guard.contains_key(&id) {
            return Err(DataError::SessionIdCollision { id: id.as_uuid() });
        }
        if let Some(store) = self.store.as_ref() {
            store.upsert(&session)?;
        }
        guard.insert(id, session);
        Ok(id)
    }

    /// Fetch a clone of the session with the given id.
    pub async fn get(&self, id: SessionId) -> Result<Session, DataError> {
        let guard = self.sessions.read().await;
        guard
            .get(&id)
            .cloned()
            .ok_or(DataError::SessionNotFound { id: id.as_uuid() })
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
        let mut guard = self.sessions.write().await;
        let session = guard
            .get_mut(&id)
            .ok_or(DataError::SessionNotFound { id: id.as_uuid() })?;
        let result = f(session);
        if let Some(store) = self.store.as_ref() {
            store.upsert(session)?;
        }
        Ok(result)
    }

    /// Snapshot every currently-tracked session.
    pub async fn list(&self) -> Vec<Session> {
        let guard = self.sessions.read().await;
        guard.values().cloned().collect()
    }

    /// Number of sessions currently tracked.
    pub async fn len(&self) -> usize {
        let guard = self.sessions.read().await;
        guard.len()
    }

    /// True when no sessions are tracked.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// Remove the session with the given id.
    pub async fn remove(&self, id: SessionId) -> Result<(), DataError> {
        let mut guard = self.sessions.write().await;
        let removed = guard.remove(&id);
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

impl Default for SessionManager {
    fn default() -> Self {
        Self::in_memory()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::{EnvSnapshot, AMUX_CONFIG_HOME};
    use crate::data::fs::headless_db::SqliteSessionStore;
    use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};

    // ─── helpers ──────────────────────────────────────────────────────────────

    fn make_session(git_root: &std::path::Path, home_dir: &std::path::Path) -> Session {
        let env = EnvSnapshot::with_overrides([(AMUX_CONFIG_HOME, home_dir.to_str().unwrap())]);
        let resolver = StaticGitRootResolver::new(git_root);
        let opts = SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        };
        Session::open(git_root.to_path_buf(), &resolver, opts).unwrap()
    }

    struct TestEnv {
        git_root: tempfile::TempDir,
        home_dir: tempfile::TempDir,
    }

    impl TestEnv {
        fn new() -> Self {
            Self {
                git_root: tempfile::tempdir().unwrap(),
                home_dir: tempfile::tempdir().unwrap(),
            }
        }

        fn make_session(&self) -> Session {
            make_session(self.git_root.path(), self.home_dir.path())
        }
    }

    // ─── CRUD happy paths ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn create_and_get_happy_path() {
        let env = TestEnv::new();
        let manager = SessionManager::in_memory();
        let session = env.make_session();
        let expected_id = session.id();

        let returned_id = manager.create(session).await.unwrap();
        assert_eq!(returned_id, expected_id);

        let retrieved = manager.get(returned_id).await.unwrap();
        assert_eq!(retrieved.id(), expected_id);
    }

    #[tokio::test]
    async fn list_returns_all_created_sessions() {
        let env = TestEnv::new();
        let manager = SessionManager::in_memory();

        let s1 = env.make_session();
        let s2 = env.make_session();
        let s3 = env.make_session();
        let id1 = manager.create(s1).await.unwrap();
        let id2 = manager.create(s2).await.unwrap();
        let id3 = manager.create(s3).await.unwrap();

        assert_eq!(manager.len().await, 3);
        let listed: Vec<SessionId> = manager.list().await.iter().map(|s| s.id()).collect();
        assert!(listed.contains(&id1));
        assert!(listed.contains(&id2));
        assert!(listed.contains(&id3));
    }

    #[tokio::test]
    async fn update_mutates_session_and_is_visible_in_get() {
        let env = TestEnv::new();
        let manager = SessionManager::in_memory();
        let session = env.make_session();
        let id = manager.create(session).await.unwrap();

        manager
            .update(id, |s| s.state_mut().record_error("oops"))
            .await
            .unwrap();

        let after = manager.get(id).await.unwrap();
        assert_eq!(after.state().errors.len(), 1);
        assert_eq!(after.state().errors[0].message, "oops");
    }

    #[tokio::test]
    async fn remove_happy_path_then_get_returns_not_found() {
        let env = TestEnv::new();
        let manager = SessionManager::in_memory();
        let session = env.make_session();
        let id = manager.create(session).await.unwrap();

        manager.remove(id).await.unwrap();

        let err = manager.get(id).await.unwrap_err();
        assert!(matches!(err, DataError::SessionNotFound { .. }));
        assert!(manager.is_empty().await);
    }

    #[tokio::test]
    async fn remove_nonexistent_returns_session_not_found() {
        let manager = SessionManager::in_memory();
        let fake_id = SessionId::new();
        let err = manager.remove(fake_id).await.unwrap_err();
        assert!(
            matches!(err, DataError::SessionNotFound { .. }),
            "expected SessionNotFound, got {err:?}"
        );
    }

    #[tokio::test]
    async fn get_nonexistent_returns_session_not_found() {
        let manager = SessionManager::in_memory();
        let fake_id = SessionId::new();
        let err = manager.get(fake_id).await.unwrap_err();
        assert!(matches!(err, DataError::SessionNotFound { .. }));
    }

    #[tokio::test]
    async fn in_memory_is_empty_initially() {
        let manager = SessionManager::in_memory();
        assert!(manager.is_empty().await);
        assert_eq!(manager.len().await, 0);
    }

    // ─── Persistence ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn with_persistence_calls_store_on_create() {
        let env = TestEnv::new();
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::with_persistence(Arc::clone(&store) as Arc<dyn SessionStore>);
        assert!(manager.has_persistence());

        let session = env.make_session();
        let id = manager.create(session).await.unwrap();

        let captured = store.captured_ids();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0], id);
    }

    #[tokio::test]
    async fn with_persistence_calls_store_on_update() {
        let env = TestEnv::new();
        let store = Arc::new(InMemorySessionStore::new());
        let manager = SessionManager::with_persistence(Arc::clone(&store) as Arc<dyn SessionStore>);

        let session = env.make_session();
        let id = manager.create(session).await.unwrap();

        // Create calls upsert once; update should call it again.
        manager.update(id, |s| s.touch()).await.unwrap();

        let captured = store.captured_ids();
        assert_eq!(
            captured.len(),
            2,
            "upsert should be called on create AND update"
        );
        assert_eq!(captured[0], id);
        assert_eq!(captured[1], id);
    }

    #[tokio::test]
    async fn in_memory_has_no_persistence_flag() {
        let manager = SessionManager::in_memory();
        assert!(!manager.has_persistence());
    }

    // ─── Concurrent create ────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_create_produces_n_distinct_sessions() {
        let git_tmp = tempfile::tempdir().unwrap();
        let home_tmp = tempfile::tempdir().unwrap();
        let manager = Arc::new(SessionManager::in_memory());

        const N: usize = 10;
        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let manager = Arc::clone(&manager);
            let git_root = git_tmp.path().to_path_buf();
            let home_dir = home_tmp.path().to_path_buf();
            handles.push(tokio::spawn(async move {
                let session = make_session(&git_root, &home_dir);
                manager.create(session).await.unwrap()
            }));
        }

        let mut ids = Vec::with_capacity(N);
        for handle in handles {
            ids.push(handle.await.unwrap());
        }

        assert_eq!(ids.len(), N);
        // All IDs must be distinct.
        let unique: std::collections::HashSet<SessionId> = ids.into_iter().collect();
        assert_eq!(
            unique.len(),
            N,
            "concurrent creates produced duplicate session IDs"
        );
        assert_eq!(manager.len().await, N);
    }

    // ─── Layer-0-internal integration: SessionManager + SqliteSessionStore ────

    /// Adapter that makes `SqliteSessionStore` compatible with `SessionStore`.
    struct SqliteStoreAdapter(Arc<SqliteSessionStore>);

    impl SessionStore for SqliteStoreAdapter {
        fn upsert(&self, session: &Session) -> Result<(), DataError> {
            let id = session.id().to_string();
            let workdir = session.working_dir().to_string_lossy().to_string();
            let created_at = chrono::Utc::now().to_rfc3339();
            // This test adapter only ever inserts (no real UPDATE path), so a
            // duplicate-key error on the second call (from `update`) is expected
            // and harmless.  Swallow all SQLite errors here; the round-trip test
            // verifies correctness through `list_sessions`, not through error
            // propagation.
            match self.0.insert_session(&id, &workdir, &created_at) {
                Ok(()) => Ok(()),
                Err(DataError::Sqlite(_)) => Ok(()),
                Err(other) => Err(other),
            }
        }

        fn remove(&self, id: SessionId) -> Result<(), DataError> {
            let now = chrono::Utc::now().to_rfc3339();
            self.0.close_session(&id.to_string(), &now)?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn session_manager_sqlite_round_trip() {
        let db_tmp = tempfile::tempdir().unwrap();
        let git_tmp = tempfile::tempdir().unwrap();
        let home_tmp = tempfile::tempdir().unwrap();

        // Phase 1: create N sessions through the manager backed by SQLite.
        let mut created_ids: Vec<String> = Vec::new();
        {
            let raw = Arc::new(SqliteSessionStore::open(db_tmp.path()).unwrap());
            let adapter: Arc<dyn SessionStore> = Arc::new(SqliteStoreAdapter(Arc::clone(&raw)));
            let manager = SessionManager::with_persistence(adapter);

            for _ in 0..3 {
                let session = make_session(git_tmp.path(), home_tmp.path());
                let id = manager.create(session).await.unwrap();
                created_ids.push(id.to_string());
            }
        }
        // Phase 2: reopen the store and verify all 3 sessions are present.
        let store2 = SqliteSessionStore::open(db_tmp.path()).unwrap();
        let records = store2.list_sessions().unwrap();
        assert_eq!(
            records.len(),
            3,
            "expected 3 sessions in the reopened store"
        );

        let record_ids: Vec<String> = records.iter().map(|r| r.id.clone()).collect();
        for created_id in &created_ids {
            assert!(
                record_ids.contains(created_id),
                "session {created_id} not found after reopen"
            );
        }
        // All sessions should have 'active' status.
        for record in &records {
            assert_eq!(record.status, "active");
        }
    }
}
