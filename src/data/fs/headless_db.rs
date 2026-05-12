//! Sqlite-backed persistence for headless-mode session and command metadata.
//!
//! Schema parity with `oldsrc/commands/headless/db.rs` is preserved so that
//! existing on-disk databases written by prior amux releases can be opened
//! by the new store without losing state.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection};

use crate::data::error::DataError;
use crate::data::fs::headless_paths::HeadlessPaths;

/// Persistable session metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: String,
    pub workdir: String,
    pub created_at: String,
    pub status: String,
    pub closed_at: Option<String>,
}

/// Persistable command metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRecord {
    pub id: String,
    pub session_id: String,
    pub subcommand: String,
    pub args: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub log_path: String,
}

/// Sqlite-backed session and command store.
///
/// Opening the store creates the database and runs migrations idempotently.
pub struct SqliteSessionStore {
    conn: Mutex<Connection>,
}

impl SqliteSessionStore {
    /// Open (or create) a sqlite database at `<root>/amux.db`, run migrations,
    /// and enable WAL mode for concurrent reads.
    pub fn open(root: &Path) -> Result<Self, DataError> {
        std::fs::create_dir_all(root).map_err(|e| DataError::io(root, e))?;
        let db_file = root.join(crate::data::fs::headless_paths::HEADLESS_DB_FILENAME);
        let conn = Connection::open(&db_file)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Convenience constructor that opens at the path resolved from `paths`.
    pub fn open_from_paths(paths: &HeadlessPaths) -> Result<Self, DataError> {
        Self::open(paths.root())
    }

    fn migrate(conn: &Connection) -> Result<(), DataError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id         TEXT PRIMARY KEY,
                workdir    TEXT NOT NULL,
                created_at TEXT NOT NULL,
                status     TEXT NOT NULL DEFAULT 'active',
                closed_at  TEXT
            );

            CREATE TABLE IF NOT EXISTS commands (
                id          TEXT PRIMARY KEY,
                session_id  TEXT NOT NULL REFERENCES sessions(id),
                subcommand  TEXT NOT NULL,
                args        TEXT NOT NULL,
                status      TEXT NOT NULL DEFAULT 'pending',
                exit_code   INTEGER,
                started_at  TEXT,
                finished_at TEXT,
                log_path    TEXT NOT NULL
            );",
        )?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("session store mutex poisoned")
    }

    // ─── Session operations ────────────────────────────────────────────────

    pub fn insert_session(
        &self,
        id: &str,
        workdir: &str,
        created_at: &str,
    ) -> Result<(), DataError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO sessions (id, workdir, created_at, status) VALUES (?1, ?2, ?3, 'active')",
            params![id, workdir, created_at],
        )?;
        Ok(())
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionRecord>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, workdir, created_at, status, closed_at FROM sessions WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], |row| {
            Ok(SessionRecord {
                id: row.get(0)?,
                workdir: row.get(1)?,
                created_at: row.get(2)?,
                status: row.get(3)?,
                closed_at: row.get(4)?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRecord>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, workdir, created_at, status, closed_at FROM sessions ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SessionRecord {
                id: row.get(0)?,
                workdir: row.get(1)?,
                created_at: row.get(2)?,
                status: row.get(3)?,
                closed_at: row.get(4)?,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn list_sessions_by_status(
        &self,
        status: Option<&str>,
    ) -> Result<Vec<SessionRecord>, DataError> {
        let Some(status) = status else {
            return self.list_sessions();
        };
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, workdir, created_at, status, closed_at FROM sessions \
             WHERE status = ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![status], |row| {
            Ok(SessionRecord {
                id: row.get(0)?,
                workdir: row.get(1)?,
                created_at: row.get(2)?,
                status: row.get(3)?,
                closed_at: row.get(4)?,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn close_session(&self, id: &str, closed_at: &str) -> Result<bool, DataError> {
        let conn = self.lock();
        let affected = conn.execute(
            "UPDATE sessions SET status = 'closed', closed_at = ?1 \
             WHERE id = ?2 AND status = 'active'",
            params![closed_at, id],
        )?;
        Ok(affected > 0)
    }

    pub fn count_active_sessions(&self) -> Result<i64, DataError> {
        let conn = self.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE status = 'active'",
            [],
            |r| r.get(0),
        )?;
        Ok(count)
    }

    /// Delete sessions closed more than `hours` hours ago, returning a list of
    /// `(session_id, deleted_command_count)` pairs.
    pub fn delete_closed_sessions_older_than(
        &self,
        hours: u64,
    ) -> Result<Vec<(String, usize)>, DataError> {
        let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours as i64);
        let cutoff_str = cutoff.to_rfc3339();
        let conn = self.lock();
        let session_ids: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT id FROM sessions \
                 WHERE status = 'closed' AND closed_at IS NOT NULL AND closed_at < ?1",
            )?;
            let rows = stmt
                .query_map(params![cutoff_str], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };

        let mut deleted = Vec::with_capacity(session_ids.len());
        for sid in &session_ids {
            let cmd_count: usize = conn.query_row(
                "SELECT COUNT(*) FROM commands WHERE session_id = ?1",
                params![sid],
                |r| r.get::<_, i64>(0),
            )? as usize;
            conn.execute("DELETE FROM commands WHERE session_id = ?1", params![sid])?;
            conn.execute("DELETE FROM sessions WHERE id = ?1", params![sid])?;
            deleted.push((sid.clone(), cmd_count));
        }
        Ok(deleted)
    }

    // ─── Command operations ────────────────────────────────────────────────

    pub fn insert_command(
        &self,
        id: &str,
        session_id: &str,
        subcommand: &str,
        args: &str,
        log_path: &str,
    ) -> Result<(), DataError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO commands (id, session_id, subcommand, args, status, log_path)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
            params![id, session_id, subcommand, args, log_path],
        )?;
        Ok(())
    }

    pub fn get_command(&self, id: &str) -> Result<Option<CommandRecord>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, subcommand, args, status, exit_code, \
                    started_at, finished_at, log_path
             FROM commands WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], |row| {
            Ok(CommandRecord {
                id: row.get(0)?,
                session_id: row.get(1)?,
                subcommand: row.get(2)?,
                args: row.get(3)?,
                status: row.get(4)?,
                exit_code: row.get(5)?,
                started_at: row.get(6)?,
                finished_at: row.get(7)?,
                log_path: row.get(8)?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn update_command_started(&self, id: &str, started_at: &str) -> Result<(), DataError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE commands SET status = 'running', started_at = ?1 WHERE id = ?2",
            params![started_at, id],
        )?;
        Ok(())
    }

    pub fn update_command_finished(
        &self,
        id: &str,
        status: &str,
        exit_code: Option<i32>,
        finished_at: &str,
    ) -> Result<(), DataError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE commands SET status = ?1, exit_code = ?2, finished_at = ?3 WHERE id = ?4",
            params![status, exit_code, finished_at, id],
        )?;
        Ok(())
    }

    pub fn count_running_commands(&self) -> Result<i64, DataError> {
        let conn = self.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM commands WHERE status = 'running'",
            [],
            |r| r.get(0),
        )?;
        Ok(count)
    }

    pub fn has_running_command_for_session(&self, session_id: &str) -> Result<bool, DataError> {
        let conn = self.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM commands \
             WHERE session_id = ?1 AND status IN ('pending', 'running')",
            params![session_id],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// Borrow the underlying connection for ad-hoc reads.
    pub fn with_conn<R>(
        &self,
        f: impl FnOnce(&Connection) -> Result<R, DataError>,
    ) -> Result<R, DataError> {
        let conn = self.lock();
        f(&conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> (tempfile::TempDir, SqliteSessionStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(tmp.path()).unwrap();
        (tmp, store)
    }

    // ─── open / migrations ────────────────────────────────────────────────────

    #[test]
    fn open_fresh_db_succeeds() {
        let (_tmp, _store) = make_store();
        // If we reach here without panic/error, the open + migration succeeded.
    }

    #[test]
    fn open_is_idempotent_on_populated_db() {
        let tmp = tempfile::tempdir().unwrap();
        // Open twice on the same directory — migrations must be idempotent.
        let store1 = SqliteSessionStore::open(tmp.path()).unwrap();
        store1
            .insert_session("s1", "/work", "2024-01-01T00:00:00Z")
            .unwrap();
        drop(store1);

        let store2 = SqliteSessionStore::open(tmp.path()).unwrap();
        let records = store2.list_sessions().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "s1");
    }

    // ─── Session CRUD ─────────────────────────────────────────────────────────

    #[test]
    fn session_insert_and_get() {
        let (_tmp, store) = make_store();
        store
            .insert_session("s1", "/work", "2024-01-01T00:00:00Z")
            .unwrap();

        let record = store.get_session("s1").unwrap().expect("session not found");
        assert_eq!(record.id, "s1");
        assert_eq!(record.workdir, "/work");
        assert_eq!(record.created_at, "2024-01-01T00:00:00Z");
        assert_eq!(record.status, "active");
        assert!(record.closed_at.is_none());
    }

    #[test]
    fn get_session_returns_none_when_not_found() {
        let (_tmp, store) = make_store();
        let result = store.get_session("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn list_sessions_returns_all_inserted() {
        let (_tmp, store) = make_store();
        store
            .insert_session("s1", "/a", "2024-01-01T00:00:00Z")
            .unwrap();
        store
            .insert_session("s2", "/b", "2024-01-02T00:00:00Z")
            .unwrap();
        store
            .insert_session("s3", "/c", "2024-01-03T00:00:00Z")
            .unwrap();

        let records = store.list_sessions().unwrap();
        assert_eq!(records.len(), 3);
        let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"s1"));
        assert!(ids.contains(&"s2"));
        assert!(ids.contains(&"s3"));
    }

    #[test]
    fn close_session_changes_status_and_sets_closed_at() {
        let (_tmp, store) = make_store();
        store
            .insert_session("s1", "/work", "2024-01-01T00:00:00Z")
            .unwrap();

        let closed = store.close_session("s1", "2024-01-02T00:00:00Z").unwrap();
        assert!(closed);

        let record = store.get_session("s1").unwrap().unwrap();
        assert_eq!(record.status, "closed");
        assert_eq!(record.closed_at.as_deref(), Some("2024-01-02T00:00:00Z"));
    }

    #[test]
    fn close_session_already_closed_returns_false() {
        let (_tmp, store) = make_store();
        store
            .insert_session("s1", "/work", "2024-01-01T00:00:00Z")
            .unwrap();
        store.close_session("s1", "2024-01-02T00:00:00Z").unwrap();

        // Closing again should return false (no rows updated).
        let closed_again = store.close_session("s1", "2024-01-03T00:00:00Z").unwrap();
        assert!(!closed_again);
    }

    #[test]
    fn count_active_sessions() {
        let (_tmp, store) = make_store();
        assert_eq!(store.count_active_sessions().unwrap(), 0);

        store
            .insert_session("s1", "/a", "2024-01-01T00:00:00Z")
            .unwrap();
        store
            .insert_session("s2", "/b", "2024-01-02T00:00:00Z")
            .unwrap();
        assert_eq!(store.count_active_sessions().unwrap(), 2);

        store.close_session("s1", "2024-01-03T00:00:00Z").unwrap();
        assert_eq!(store.count_active_sessions().unwrap(), 1);
    }

    #[test]
    fn list_sessions_by_status_active() {
        let (_tmp, store) = make_store();
        store
            .insert_session("s1", "/a", "2024-01-01T00:00:00Z")
            .unwrap();
        store
            .insert_session("s2", "/b", "2024-01-02T00:00:00Z")
            .unwrap();
        store.close_session("s1", "2024-01-03T00:00:00Z").unwrap();

        let active = store.list_sessions_by_status(Some("active")).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "s2");

        let closed = store.list_sessions_by_status(Some("closed")).unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].id, "s1");
    }

    // ─── Command CRUD ─────────────────────────────────────────────────────────

    #[test]
    fn command_insert_and_get() {
        let (_tmp, store) = make_store();
        store
            .insert_session("s1", "/work", "2024-01-01T00:00:00Z")
            .unwrap();
        store
            .insert_command("c1", "s1", "chat", "[]", "/logs/c1.log")
            .unwrap();

        let cmd = store.get_command("c1").unwrap().expect("command not found");
        assert_eq!(cmd.id, "c1");
        assert_eq!(cmd.session_id, "s1");
        assert_eq!(cmd.subcommand, "chat");
        assert_eq!(cmd.status, "pending");
        assert_eq!(cmd.log_path, "/logs/c1.log");
        assert!(cmd.exit_code.is_none());
    }

    #[test]
    fn update_command_started_sets_status_running() {
        let (_tmp, store) = make_store();
        store
            .insert_session("s1", "/work", "2024-01-01T00:00:00Z")
            .unwrap();
        store
            .insert_command("c1", "s1", "chat", "[]", "/logs/c1.log")
            .unwrap();

        store
            .update_command_started("c1", "2024-01-01T01:00:00Z")
            .unwrap();

        let cmd = store.get_command("c1").unwrap().unwrap();
        assert_eq!(cmd.status, "running");
        assert_eq!(cmd.started_at.as_deref(), Some("2024-01-01T01:00:00Z"));
    }

    #[test]
    fn update_command_finished_sets_status_and_exit_code() {
        let (_tmp, store) = make_store();
        store
            .insert_session("s1", "/work", "2024-01-01T00:00:00Z")
            .unwrap();
        store
            .insert_command("c1", "s1", "chat", "[]", "/logs/c1.log")
            .unwrap();
        store
            .update_command_started("c1", "2024-01-01T01:00:00Z")
            .unwrap();

        store
            .update_command_finished("c1", "done", Some(0), "2024-01-01T02:00:00Z")
            .unwrap();

        let cmd = store.get_command("c1").unwrap().unwrap();
        assert_eq!(cmd.status, "done");
        assert_eq!(cmd.exit_code, Some(0));
        assert_eq!(cmd.finished_at.as_deref(), Some("2024-01-01T02:00:00Z"));
    }

    #[test]
    fn count_running_commands() {
        let (_tmp, store) = make_store();
        store
            .insert_session("s1", "/work", "2024-01-01T00:00:00Z")
            .unwrap();
        assert_eq!(store.count_running_commands().unwrap(), 0);

        store
            .insert_command("c1", "s1", "chat", "[]", "/logs/c1.log")
            .unwrap();
        store
            .update_command_started("c1", "2024-01-01T01:00:00Z")
            .unwrap();
        assert_eq!(store.count_running_commands().unwrap(), 1);

        store
            .update_command_finished("c1", "done", Some(0), "2024-01-01T02:00:00Z")
            .unwrap();
        assert_eq!(store.count_running_commands().unwrap(), 0);
    }

    #[test]
    fn has_running_command_for_session() {
        let (_tmp, store) = make_store();
        store
            .insert_session("s1", "/work", "2024-01-01T00:00:00Z")
            .unwrap();
        store
            .insert_command("c1", "s1", "chat", "[]", "/logs/c1.log")
            .unwrap();

        // Pending counts as "in-flight".
        assert!(store.has_running_command_for_session("s1").unwrap());
        // Finish the command.
        store
            .update_command_started("c1", "2024-01-01T01:00:00Z")
            .unwrap();
        store
            .update_command_finished("c1", "done", Some(0), "2024-01-01T02:00:00Z")
            .unwrap();
        assert!(!store.has_running_command_for_session("s1").unwrap());
    }

    // ─── Schema compatibility with legacy DB ──────────────────────────────────
    //
    // Creates a DB using the exact SQL from oldsrc/commands/headless/db.rs,
    // inserts data, then opens it with SqliteSessionStore to verify that the
    // new store can read existing on-disk databases (user-upgrade path).

    #[test]
    fn legacy_schema_db_is_readable_by_new_store() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("amux.db");

        // Step 1: create the DB using the exact legacy schema.
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS sessions (
                    id         TEXT PRIMARY KEY,
                    workdir    TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    status     TEXT NOT NULL DEFAULT 'active',
                    closed_at  TEXT
                );

                CREATE TABLE IF NOT EXISTS commands (
                    id          TEXT PRIMARY KEY,
                    session_id  TEXT NOT NULL REFERENCES sessions(id),
                    subcommand  TEXT NOT NULL,
                    args        TEXT NOT NULL,
                    status      TEXT NOT NULL DEFAULT 'pending',
                    exit_code   INTEGER,
                    started_at  TEXT,
                    finished_at TEXT,
                    log_path    TEXT NOT NULL
                );",
            )
            .unwrap();

            // Insert rows as the old amux code would have.
            conn.execute(
                "INSERT INTO sessions (id, workdir, created_at, status) \
                 VALUES ('legacy-id-1', '/old/repo', '2023-06-01T10:00:00Z', 'active')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO commands (id, session_id, subcommand, args, status, log_path) \
                 VALUES ('cmd-1', 'legacy-id-1', 'chat', '[]', 'pending', '/logs/cmd-1.log')",
                [],
            )
            .unwrap();
        }

        // Step 2: open with SqliteSessionStore (triggers idempotent migration).
        let store = SqliteSessionStore::open(tmp.path()).unwrap();

        // Step 3: verify the legacy rows are readable.
        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "legacy-id-1");
        assert_eq!(sessions[0].workdir, "/old/repo");
        assert_eq!(sessions[0].status, "active");

        let cmd = store.get_command("cmd-1").unwrap().unwrap();
        assert_eq!(cmd.session_id, "legacy-id-1");
        assert_eq!(cmd.subcommand, "chat");
        assert_eq!(cmd.status, "pending");
    }
}
