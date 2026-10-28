//! Command-lifecycle tables of the API database: cancel requests and
//! `/v1/runs` linkage (WI 0135).
//!
//! They sit beside `SqliteSessionStore`'s own tables and reach the database
//! only through its public `with_conn`, so `SqliteSessionStore::migrate` does
//! not need to know about them. `CommandLifecycleStore::open` creates them
//! idempotently; every other method is one statement (or one short loop of
//! them) under the store's connection lock.

use std::sync::Arc;

use rusqlite::{params, OptionalExtension};

use crate::data::command_status::{cancelled_result_json, CANCELLED_EXIT_CODE};
use crate::data::error::DataError;
use crate::data::fs::api_db::SqliteSessionStore;

/// A `/v1/runs` command and the session created for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub session_id: String,
    pub command_id: String,
    /// Close the session once the command reaches a terminal status.
    pub close_on_finish: bool,
    pub created_at: String,
}

/// WI 0135 lifecycle tables beside `SqliteSessionStore`'s own. `open` creates
/// them idempotently; every other method is one statement through `with_conn`.
#[derive(Clone)]
pub struct CommandLifecycleStore {
    store: Arc<SqliteSessionStore>,
}

impl CommandLifecycleStore {
    pub fn open(store: Arc<SqliteSessionStore>) -> Result<Self, DataError> {
        store.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS command_cancellations (
                    command_id   TEXT PRIMARY KEY,
                    requested_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS runs (
                    session_id      TEXT PRIMARY KEY,
                    command_id      TEXT NOT NULL UNIQUE,
                    close_on_finish INTEGER NOT NULL DEFAULT 0,
                    created_at      TEXT NOT NULL
                );",
            )?;
            Ok(())
        })?;
        Ok(Self { store })
    }

    /// Mark a still-queued command `cancelled` (exit 130). `Ok(true)` when a
    /// row changed; `Ok(false)` when it was no longer queued (a worker claimed
    /// it, or it already finished).
    pub fn cancel_if_queued(&self, command_id: &str, finished_at: &str) -> Result<bool, DataError> {
        let result = cancelled_result_json(None);
        self.store.with_conn(|conn| {
            let changed = conn.execute(
                "UPDATE commands SET status = 'cancelled', exit_code = ?1, finished_at = ?2, result = ?3
                 WHERE id = ?4 AND status IN ('queued', 'pending')",
                params![CANCELLED_EXIT_CODE, finished_at, result, command_id],
            )?;
            Ok(changed > 0)
        })
    }

    /// Mark a `running` command that no worker owns `cancelled` (exit 130,
    /// `cancelled_at = requested_at`). Only for a row orphaned by a crash; an
    /// owned row is finalised by its worker. `Ok(true)` when a row changed.
    pub fn cancel_if_running(
        &self,
        command_id: &str,
        finished_at: &str,
        requested_at: &str,
    ) -> Result<bool, DataError> {
        let result = cancelled_result_json(Some(requested_at));
        self.store.with_conn(|conn| {
            let changed = conn.execute(
                "UPDATE commands SET status = 'cancelled', exit_code = ?1, finished_at = ?2, result = ?3
                 WHERE id = ?4 AND status = 'running'",
                params![CANCELLED_EXIT_CODE, finished_at, result, command_id],
            )?;
            Ok(changed > 0)
        })
    }

    /// Record a cancel request. Idempotent: a repeat keeps the first time.
    pub fn request_cancel(&self, command_id: &str, requested_at: &str) -> Result<(), DataError> {
        self.store.with_conn(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO command_cancellations (command_id, requested_at)
                 VALUES (?1, ?2)",
                params![command_id, requested_at],
            )?;
            Ok(())
        })
    }

    pub fn cancel_requested_at(&self, command_id: &str) -> Result<Option<String>, DataError> {
        self.store.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT requested_at FROM command_cancellations WHERE command_id = ?1",
                    params![command_id],
                    |row| row.get(0),
                )
                .optional()?)
        })
    }

    /// Includes previously finalized rows so crash recovery is repeatable.
    pub fn cancelled_command_ids(&self) -> Result<Vec<String>, DataError> {
        self.store.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT c.id FROM commands c JOIN command_cancellations k ON k.command_id=c.id WHERE c.status='cancelled'")?;
            let rows = stmt.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Bootstrap: every `queued`/`pending`/`running` row with a cancel marker
    /// becomes `cancelled` (exit 130, result error "cancelled"), so the stale
    /// recovery that follows never requeues it. Returns the ids.
    pub fn finalize_interrupted_cancellations(
        &self,
        finished_at: &str,
    ) -> Result<Vec<String>, DataError> {
        self.store.with_conn(|conn| {
            let rows: Vec<(String, String)> = {
                let mut stmt = conn.prepare(
                    "SELECT c.id, k.requested_at FROM commands c
                     JOIN command_cancellations k ON k.command_id = c.id
                     WHERE c.status IN ('queued', 'pending', 'running')",
                )?;
                let rows = stmt
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            for (id, requested_at) in &rows {
                conn.execute(
                    "UPDATE commands SET status = 'cancelled', exit_code = ?1, finished_at = ?2, result = ?3
                     WHERE id = ?4 AND status IN ('queued', 'pending', 'running')",
                    params![
                        CANCELLED_EXIT_CODE,
                        finished_at,
                        cancelled_result_json(Some(requested_at)),
                        id
                    ],
                )?;
            }
            Ok(rows.into_iter().map(|(id, _)| id).collect())
        })
    }

    /// Mark a still-queued command `error` (exit 1) with `result_json`. Used
    /// when a `/v1/runs` setup fails; a command that was already cancelled
    /// keeps `cancelled`.
    pub fn fail_if_queued(
        &self,
        command_id: &str,
        finished_at: &str,
        result_json: &str,
    ) -> Result<bool, DataError> {
        self.store.with_conn(|conn| {
            let changed = conn.execute(
                "UPDATE commands SET status = 'error', exit_code = 1, finished_at = ?1, result = ?2
                 WHERE id = ?3 AND status IN ('queued', 'pending')",
                params![finished_at, result_json, command_id],
            )?;
            Ok(changed > 0)
        })
    }

    pub fn record_run(&self, run: &RunRecord) -> Result<(), DataError> {
        self.store.with_conn(|conn| {
            conn.execute(
                "INSERT INTO runs (session_id, command_id, close_on_finish, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    run.session_id,
                    run.command_id,
                    run.close_on_finish,
                    run.created_at
                ],
            )?;
            Ok(())
        })
    }

    /// Every recorded run, oldest first (bootstrap reconciliation).
    pub fn list_runs(&self) -> Result<Vec<RunRecord>, DataError> {
        self.store.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT session_id, command_id, close_on_finish, created_at FROM runs
                 ORDER BY created_at, rowid",
            )?;
            let runs = stmt
                .query_map([], |row| {
                    Ok(RunRecord {
                        session_id: row.get(0)?,
                        command_id: row.get(1)?,
                        close_on_finish: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(runs)
        })
    }

    pub fn run_for_session(&self, session_id: &str) -> Result<Option<RunRecord>, DataError> {
        self.run_where("session_id", session_id)
    }

    pub fn run_for_command(&self, command_id: &str) -> Result<Option<RunRecord>, DataError> {
        self.run_where("command_id", command_id)
    }

    /// `column` is one of the two literal names above, never user input.
    fn run_where(&self, column: &'static str, value: &str) -> Result<Option<RunRecord>, DataError> {
        let sql = format!(
            "SELECT session_id, command_id, close_on_finish, created_at FROM runs WHERE {column} = ?1"
        );
        self.store.with_conn(|conn| {
            Ok(conn
                .query_row(&sql, params![value], |row| {
                    Ok(RunRecord {
                        session_id: row.get(0)?,
                        command_id: row.get(1)?,
                        close_on_finish: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                })
                .optional()?)
        })
    }

    /// Remove rows whose session or command no longer exists (called after
    /// the closed-session purge in bootstrap). Returns how many were removed.
    pub fn prune_orphans(&self) -> Result<usize, DataError> {
        self.store.with_conn(|conn| {
            let cancellations = conn.execute(
                "DELETE FROM command_cancellations
                 WHERE command_id NOT IN (SELECT id FROM commands)",
                [],
            )?;
            let runs = conn.execute(
                "DELETE FROM runs WHERE session_id NOT IN (SELECT id FROM sessions)",
                [],
            )?;
            Ok(cancellations + runs)
        })
    }
}
