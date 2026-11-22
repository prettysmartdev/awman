//! Durable job metadata beside the command rows: each worker job's origin and
//! scheduling spec, and every command's `outputs_mounted` flag.
//!
//! Like `api_lifecycle_store`, these are side tables reached through
//! `SqliteSessionStore::with_conn`, so the command row (and every struct
//! literal of it) is unchanged. The command row stays the single source of
//! truth for status and result; a [`Job`] is projected from it.

use std::sync::Arc;

use rusqlite::{params, OptionalExtension};

use crate::data::error::DataError;
use crate::data::fs::api_db::{CommandRecord, SqliteSessionStore};
use crate::data::worker::{Job, JobClass, JobOrigin, JobSpec, JobState};

#[derive(Clone)]
pub struct JobStore {
    store: Arc<SqliteSessionStore>,
}

fn json_err(e: serde_json::Error) -> DataError {
    DataError::Other(format!("worker job JSON: {e}"))
}

impl JobStore {
    /// Create the side tables idempotently.
    pub fn open(store: Arc<SqliteSessionStore>) -> Result<Self, DataError> {
        store.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS worker_jobs (
                    command_id  TEXT PRIMARY KEY,
                    origin_kind TEXT NOT NULL CHECK (origin_kind IN ('push','pull')),
                    server      TEXT,
                    lease_id    TEXT,
                    origin      TEXT NOT NULL,
                    spec        TEXT NOT NULL,
                    created_at  TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS worker_jobs_kind ON worker_jobs (origin_kind, created_at);
                CREATE TABLE IF NOT EXISTS command_outputs_mounts (
                    command_id TEXT PRIMARY KEY,
                    mounted    INTEGER NOT NULL
                );",
            )?;
            Ok(())
        })?;
        Ok(Self { store })
    }

    /// Record the origin and spec of a newly admitted job. A repeat for the
    /// same command replaces the earlier entry (re-admission after restart).
    pub fn insert(
        &self,
        command_id: &str,
        origin: &JobOrigin,
        spec: &JobSpec,
        created_at: &str,
    ) -> Result<(), DataError> {
        let (server, lease_id) = match origin {
            JobOrigin::Pull {
                server, lease_id, ..
            } => (Some(server.as_str()), Some(lease_id.as_str())),
            JobOrigin::Push { .. } => (None, None),
        };
        let origin_json = serde_json::to_string(origin).map_err(json_err)?;
        let spec_json = serde_json::to_string(spec).map_err(json_err)?;
        self.store.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO worker_jobs
                    (command_id, origin_kind, server, lease_id, origin, spec, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    command_id,
                    origin.class().as_str(),
                    server,
                    lease_id,
                    origin_json,
                    spec_json,
                    created_at
                ],
            )?;
            Ok(())
        })
    }

    /// The stored origin and spec of `command_id`, if it is a worker job.
    pub fn get(&self, command_id: &str) -> Result<Option<(JobOrigin, JobSpec)>, DataError> {
        let row: Option<(String, String)> = self.store.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT origin, spec FROM worker_jobs WHERE command_id = ?1",
                    params![command_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?)
        })?;
        row.map(|(origin, spec)| {
            Ok((
                serde_json::from_str(&origin).map_err(json_err)?,
                serde_json::from_str(&spec).map_err(json_err)?,
            ))
        })
        .transpose()
    }

    /// Project `record` into a [`Job`], or `None` when it is not a worker job.
    pub fn job(
        &self,
        record: &CommandRecord,
        hint: Option<JobState>,
    ) -> Result<Option<Job>, DataError> {
        Ok(self.get(&record.id)?.map(|(origin, spec)| Job {
            id: record.id.clone(),
            origin,
            spec,
            state: JobState::from_command(record.status, hint),
        }))
    }

    /// Command IDs of one class, oldest first.
    pub fn command_ids(&self, class: JobClass) -> Result<Vec<String>, DataError> {
        self.store.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT command_id FROM worker_jobs WHERE origin_kind = ?1 ORDER BY created_at, rowid",
            )?;
            let ids = stmt
                .query_map(params![class.as_str()], |r| r.get(0))?
                .collect::<Result<Vec<String>, _>>()?;
            Ok(ids)
        })
    }

    /// Record whether `/awman/output` was actually mounted for a command.
    /// The last write wins (a restarted attempt records its own mount).
    pub fn set_outputs_mounted(&self, command_id: &str, mounted: bool) -> Result<(), DataError> {
        self.store.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO command_outputs_mounts (command_id, mounted) VALUES (?1, ?2)",
                params![command_id, mounted],
            )?;
            Ok(())
        })
    }

    /// `None` when nothing was recorded (the command never reached a launch).
    pub fn outputs_mounted(&self, command_id: &str) -> Result<Option<bool>, DataError> {
        self.store.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT mounted FROM command_outputs_mounts WHERE command_id = ?1",
                    params![command_id],
                    |r| r.get(0),
                )
                .optional()?)
        })
    }
}
