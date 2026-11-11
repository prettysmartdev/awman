//! Durable awman lease resources (`lease_…`) in the daemon database.
//!
//! One row per claimed lease. The item snapshot is stored without its
//! repository credentials (see [`LeaseRecord::claimed`]); `insert`/`update`
//! strip them again so no caller can persist a token by accident.

use std::sync::Arc;

use rusqlite::{params, OptionalExtension, Row};

use crate::data::error::DataError;
use crate::data::fs::api_db::SqliteSessionStore;
use crate::data::ids::LeaseId;
use crate::data::worker::{LeaseRecord, LeaseState};

#[derive(Clone)]
pub struct LeaseStore {
    store: Arc<SqliteSessionStore>,
}

/// Filter for [`LeaseStore::list`]. Results are newest claim first; the
/// cursor is the last returned lease ID.
#[derive(Debug, Clone, Default)]
pub struct LeaseFilter<'a> {
    pub server: Option<&'a str>,
    pub states: Option<&'a [LeaseState]>,
    pub limit: usize,
    pub cursor: Option<&'a str>,
}

fn json_err(e: serde_json::Error) -> DataError {
    DataError::Other(format!("lease JSON: {e}"))
}

fn strip(lease: &LeaseRecord) -> LeaseRecord {
    let mut lease = lease.clone();
    lease.item.repo.credentials = None;
    lease
}

fn decode(row: &Row<'_>) -> rusqlite::Result<String> {
    row.get(0)
}

impl LeaseStore {
    pub fn open(store: Arc<SqliteSessionStore>) -> Result<Self, DataError> {
        store.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS worker_leases (
                    id              TEXT PRIMARY KEY,
                    server          TEXT NOT NULL,
                    server_lease_id TEXT NOT NULL,
                    epoch           INTEGER NOT NULL,
                    item_id         TEXT NOT NULL,
                    job_id          TEXT,
                    state           TEXT NOT NULL,
                    claimed_at      TEXT NOT NULL,
                    record          TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS worker_leases_server ON worker_leases (server, state);
                CREATE INDEX IF NOT EXISTS worker_leases_job ON worker_leases (job_id);",
            )?;
            Ok(())
        })?;
        Ok(Self { store })
    }

    pub fn insert(&self, lease: &LeaseRecord) -> Result<(), DataError> {
        self.write(lease, true)
    }

    pub fn update(&self, lease: &LeaseRecord) -> Result<(), DataError> {
        self.write(lease, false)
    }

    fn write(&self, lease: &LeaseRecord, insert: bool) -> Result<(), DataError> {
        awman_wsp::repo_url::validate(&lease.item.repo.url)
            .map_err(|message| DataError::Other(message.into()))?;
        let lease = strip(lease);
        let record = serde_json::to_string(&lease).map_err(json_err)?;
        let claimed_at = crate::data::wire::format_timestamp(lease.claimed_at);
        self.store.with_conn(|conn| {
            let sql = if insert {
                "INSERT INTO worker_leases
                    (id, server, server_lease_id, epoch, item_id, job_id, state, claimed_at, record)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
            } else {
                "UPDATE worker_leases SET server = ?2, server_lease_id = ?3, epoch = ?4, item_id = ?5,
                    job_id = ?6, state = ?7, claimed_at = ?8, record = ?9 WHERE id = ?1"
            };
            let changed = conn.execute(
                sql,
                params![
                    lease.id.as_str(),
                    lease.server,
                    lease.server_lease_id,
                    lease.epoch as i64,
                    lease.item_id,
                    lease.job_id,
                    lease.state.as_str(),
                    claimed_at,
                    record
                ],
            )?;
            if changed == 0 {
                return Err(DataError::Other(format!("lease {} not found", lease.id)));
            }
            Ok(())
        })
    }

    pub fn get(&self, id: &LeaseId) -> Result<Option<LeaseRecord>, DataError> {
        self.one(
            "SELECT record FROM worker_leases WHERE id = ?1",
            id.as_str(),
        )
    }

    fn one(&self, sql: &str, key: &str) -> Result<Option<LeaseRecord>, DataError> {
        let json: Option<String> = self
            .store
            .with_conn(|conn| Ok(conn.query_row(sql, params![key], decode).optional()?))?;
        json.map(|j| serde_json::from_str(&j).map_err(json_err))
            .transpose()
    }

    /// Leases matching `filter`, newest claim first.
    pub fn list(&self, filter: &LeaseFilter<'_>) -> Result<Vec<LeaseRecord>, DataError> {
        let rows: Vec<String> = self.store.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT record FROM worker_leases
                 WHERE (?1 IS NULL OR server = ?1)
                 ORDER BY claimed_at DESC, id DESC",
            )?;
            let rows = stmt
                .query_map(params![filter.server], decode)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let mut leases = rows
            .iter()
            .map(|j| serde_json::from_str::<LeaseRecord>(j).map_err(json_err))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(states) = filter.states {
            leases.retain(|l| states.contains(&l.state));
        }
        if let Some(cursor) = filter.cursor {
            if let Some(pos) = leases.iter().position(|l| l.id.as_str() == cursor) {
                leases.drain(..=pos);
            }
        }
        if filter.limit > 0 {
            leases.truncate(filter.limit);
        }
        Ok(leases)
    }

    /// Leases of `server` still held (claimed through reporting).
    pub fn active_for_server(&self, server: &str) -> Result<Vec<LeaseRecord>, DataError> {
        self.list(&LeaseFilter {
            server: Some(server),
            states: Some(LeaseState::ACTIVE),
            ..Default::default()
        })
    }

    /// Every lease still held, across servers (start-up recovery).
    pub fn active(&self) -> Result<Vec<LeaseRecord>, DataError> {
        self.list(&LeaseFilter {
            states: Some(LeaseState::ACTIVE),
            ..Default::default()
        })
    }
}
