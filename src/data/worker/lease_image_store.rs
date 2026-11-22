//! Worker-owned lease image records and reservations in the private database.
use std::{collections::BTreeSet, path::Path, sync::Arc};

use rusqlite::params;

use super::{lease_images::LeaseImageRecord, LeaseState};
use crate::data::{error::DataError, fs::api_db::SqliteSessionStore, ids::LeaseId};

#[derive(Clone)]
pub struct LeaseImageStore {
    db: Arc<SqliteSessionStore>,
}

impl LeaseImageStore {
    pub fn open(db: Arc<SqliteSessionStore>) -> Result<Self, DataError> {
        db.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS worker_lease_images (
                    store TEXT NOT NULL, tag TEXT NOT NULL, record TEXT NOT NULL,
                    PRIMARY KEY (store, tag));
                 CREATE TABLE IF NOT EXISTS worker_lease_image_intents (
                    store TEXT NOT NULL, tag TEXT NOT NULL, record TEXT NOT NULL,
                    PRIMARY KEY (store, tag));
                 CREATE TABLE IF NOT EXISTS worker_lease_image_uses (
                    store TEXT NOT NULL, tag TEXT NOT NULL, lease TEXT NOT NULL, server TEXT NOT NULL,
                    PRIMARY KEY (store, tag, lease));
                 CREATE TABLE IF NOT EXISTS worker_lease_image_budget_warnings (
                    store TEXT NOT NULL, server TEXT NOT NULL, PRIMARY KEY(store, server));",
            )?;
            Ok(())
        })?;
        Ok(Self { db })
    }

    /// Serializes reservations, record updates and eviction across processes.
    /// Never hold this while waiting for an image build lease.
    pub fn lock(data_home: &Path) -> Result<std::fs::File, DataError> {
        let dir = data_home.join("locks");
        crate::data::fs::private_files::create_private_dir(&dir)?;
        crate::data::fs::private_files::lock_private_exclusive(
            &dir.join("lease-images.lock"),
            || Ok(()),
        )
    }

    /// Reserve before looking up or building a tag. Even a not-yet-built image
    /// is protected against another lease's end-of-run cleanup.
    pub fn reserve(
        &self,
        store: &str,
        tag: &str,
        lease: &LeaseId,
        server: &str,
    ) -> Result<(), DataError> {
        self.db.with_conn(|conn| {
            conn.execute("INSERT OR IGNORE INTO worker_lease_image_uses (store, tag, lease, server) VALUES (?1,?2,?3,?4)",
                params![store, tag, lease.as_str(), server])?;
            Ok(())
        })
    }

    /// Intent survives cancellation/crash between a build and its ownership record.
    pub fn intent(&self, store: &str, record: &LeaseImageRecord) -> Result<(), DataError> {
        let json = serde_json::to_string(record).map_err(|e| DataError::Other(e.to_string()))?;
        self.db.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO worker_lease_image_intents VALUES (?1,?2,?3)",
                params![store, record.tag, json],
            )?;
            Ok(())
        })
    }
    pub fn intents(&self, store: &str) -> Result<Vec<LeaseImageRecord>, DataError> {
        self.db.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT record FROM worker_lease_image_intents WHERE store=?1")?;
            let rows = stmt
                .query_map(params![store], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows
                .into_iter()
                .filter_map(|row| serde_json::from_str(&row).ok())
                .collect())
        })
    }

    /// Compact completed reservations to one attribution per server and image.
    pub fn compact(&self) -> Result<(), DataError> {
        self.db.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch("INSERT OR IGNORE INTO worker_lease_image_uses
                SELECT store, tag, 'server:' || server, server FROM worker_lease_image_uses u
                WHERE NOT EXISTS (SELECT 1 FROM worker_leases l WHERE l.id=u.lease AND l.state IN ('claimed','preparing','running','awaiting_input','pushing','reporting'));
                DELETE FROM worker_lease_image_uses WHERE lease NOT LIKE 'server:%' AND
                NOT EXISTS (SELECT 1 FROM worker_leases l WHERE l.id=worker_lease_image_uses.lease AND l.state IN ('claimed','preparing','running','awaiting_input','pushing','reporting'));")?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Only a successful worker build confers ownership. Cache hits merely
    /// touch records already owned by this worker; arbitrary installed images
    /// with matching labels are never adopted as cleanup candidates.
    pub fn record(
        &self,
        store: &str,
        record: &LeaseImageRecord,
        built: bool,
    ) -> Result<(), DataError> {
        let json = serde_json::to_string(record).map_err(|e| DataError::Other(e.to_string()))?;
        self.db.with_conn(|conn| {
            if built {
                conn.execute("INSERT OR REPLACE INTO worker_lease_images (store, tag, record) VALUES (?1,?2,?3)", params![store, record.tag, json])?;
            } else {
                conn.execute("UPDATE worker_lease_images SET record=?3 WHERE store=?1 AND tag=?2 AND json_extract(record, '$.image_id')=?4", params![store, record.tag, json, record.image_id])?;
            }
            Ok(())
        })
    }

    /// Server membership is retained after leases end so each server's budget
    /// covers its cache. Shared images are protected by uses from every server.
    pub fn records(
        &self,
        store: &str,
        server: Option<&str>,
    ) -> Result<Vec<LeaseImageRecord>, DataError> {
        let rows = self.db.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT record FROM worker_lease_images i WHERE store=?1 AND (?2 IS NULL OR EXISTS (SELECT 1 FROM worker_lease_image_uses u WHERE u.store=i.store AND u.tag=i.tag AND u.server=?2))")?;
            let rows = stmt.query_map(params![store, server], |r| r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?;
            Ok(rows)
        })?;
        Ok(rows.iter().filter_map(|j| match serde_json::from_str(j) {
            Ok(record) => Some(record),
            Err(error) => { tracing::warn!(%error, "invalid lease image record; keeping image and continuing cleanup"); None }
        }).collect())
    }

    pub fn servers(&self, store: &str) -> Result<Vec<String>, DataError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT DISTINCT server FROM worker_lease_image_uses WHERE store=?1 ORDER BY server")?;
            let rows = stmt.query_map(params![store], |r| r.get(0))?.collect::<Result<Vec<_>,_>>()?;
            Ok(rows)
        })
    }

    /// Uses belonging to leases still held, including preparation/reporting.
    /// No in-memory view or expiry guess can overlook another worker process.
    pub fn in_use(&self, store: &str) -> Result<BTreeSet<String>, DataError> {
        let active: BTreeSet<String> = self.db.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT u.tag, l.state FROM worker_lease_image_uses u JOIN worker_leases l ON l.id=u.lease WHERE u.store=?1")?;
            let rows = stmt.query_map(params![store], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<Result<Vec<_>,_>>()?;
            Ok(rows.into_iter().filter(|(_, state)| LeaseState::ACTIVE.iter().any(|s| s.as_str()==state)).map(|(tag,_)| tag).collect())
        })?;
        Ok(active)
    }

    pub fn forget(&self, store: &str, tag: &str) -> Result<(), DataError> {
        self.db.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM worker_lease_images WHERE store=?1 AND tag=?2",
                params![store, tag],
            )?;
            tx.execute(
                "DELETE FROM worker_lease_image_intents WHERE store=?1 AND tag=?2 AND NOT EXISTS (
                    SELECT 1 FROM worker_lease_image_uses u JOIN worker_leases l ON l.id=u.lease
                    WHERE u.store=?1 AND u.tag=?2 AND l.state IN ('claimed','preparing','running','awaiting_input','pushing','reporting'))",
                params![store, tag],
            )?;
            tx.execute(
                "DELETE FROM worker_lease_image_uses WHERE store=?1 AND tag=?2 AND NOT EXISTS (
                    SELECT 1 FROM worker_leases l WHERE l.id=worker_lease_image_uses.lease
                    AND l.state IN ('claimed','preparing','running','awaiting_input','pushing','reporting'))",
                params![store, tag],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// One warning per over-budget episode, shared by all worker hosts.
    pub fn budget_warning(
        &self,
        store: &str,
        server: &str,
        exceeded: bool,
    ) -> Result<bool, DataError> {
        self.db.with_conn(|conn| {
            if exceeded {
                Ok(conn.execute("INSERT OR IGNORE INTO worker_lease_image_budget_warnings (store,server) VALUES (?1,?2)", params![store,server])? > 0)
            } else {
                conn.execute("DELETE FROM worker_lease_image_budget_warnings WHERE store=?1 AND server=?2", params![store,server])?;
                Ok(false)
            }
        })
    }
}
