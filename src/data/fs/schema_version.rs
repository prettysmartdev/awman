//! Shared SQLite schema version for the API database.

use crate::data::error::DataError;
use rusqlite::{Connection, OptionalExtension};

pub const CURRENT_SCHEMA_VERSION: i64 = 2;

/// Apply the WI 0139 identifier migration once, after the API and squad tables
/// have been created. The update is transactional, including the version bump.
pub fn migrate_ids(conn: &Connection) -> Result<(), DataError> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version >= CURRENT_SCHEMA_VERSION {
        let tx = conn.unchecked_transaction()?;
        tx.pragma_update(None, "defer_foreign_keys", true)?;
        // The squad tables can be created after the API store first advances
        // the shared file's version. Their own CREATE path still runs here.
        if tx
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='squad_tasks'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            tx.execute("UPDATE squad_tasks SET id='task_'||replace(id,'-','') WHERE id NOT LIKE 'task\\_%' ESCAPE '\\'", [])?;
        }
        if tx
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='squad_runs'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            tx.execute("UPDATE squad_runs SET id='run_'||replace(id,'-',''), task_id='task_'||replace(task_id,'-',''), session_id=CASE WHEN session_id IS NULL THEN NULL ELSE 'ses_'||replace(session_id,'-','') END WHERE id NOT LIKE 'run\\_%' ESCAPE '\\'", [])?;
        }
        tx.commit()?;
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    tx.pragma_update(None, "defer_foreign_keys", true)?;
    fn has_table(tx: &rusqlite::Transaction<'_>, name: &str) -> Result<bool, DataError> {
        Ok(tx
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
    if has_table(&tx, "sessions")? {
        tx.execute("UPDATE sessions SET id='ses_'||replace(id,'-','') WHERE id NOT LIKE 'ses\\_%' ESCAPE '\\'",[])?;
    }
    if has_table(&tx, "commands")? {
        tx.execute("UPDATE commands SET id='cmd_'||replace(id,'-',''), session_id='ses_'||replace(session_id,'-','') WHERE id NOT LIKE 'cmd\\_%' ESCAPE '\\'",[])?;
    }
    if has_table(&tx, "labels")? {
        tx.execute("UPDATE labels SET owner_id=CASE owner_kind WHEN 'session' THEN 'ses_' ELSE 'cmd_' END||replace(owner_id,'-','') WHERE owner_id NOT LIKE '%\\_%' ESCAPE '\\'",[])?;
    }
    if has_table(&tx, "idempotency_keys")? {
        tx.execute("UPDATE idempotency_keys SET resource_id=CASE resource_kind WHEN 'session' THEN 'ses_' WHEN 'compensating_session' THEN 'ses_' ELSE 'cmd_' END||replace(resource_id,'-','') WHERE resource_id IS NOT NULL AND resource_id NOT LIKE '%\\_%' ESCAPE '\\'",[])?;
        tx.execute("UPDATE idempotency_keys SET scope=CASE WHEN scope LIKE '/v1/commands|%' THEN '/v1/worker/commands|'||substr(scope, length('/v1/commands|') + 1) WHEN scope LIKE '/v1/sessions|%' THEN '/v1/worker/sessions|'||substr(scope, length('/v1/sessions|') + 1) WHEN scope LIKE '/v1/runs|%' THEN '/v1/worker/runs|'||substr(scope, length('/v1/runs|') + 1) ELSE scope END", [])?;
    }
    if has_table(&tx, "command_cancellations")? {
        tx.execute("UPDATE command_cancellations SET command_id='cmd_'||replace(command_id,'-','') WHERE command_id NOT LIKE 'cmd\\_%' ESCAPE '\\'",[])?;
    }
    if has_table(&tx, "runs")? {
        tx.execute("UPDATE runs SET session_id='ses_'||replace(session_id,'-',''), command_id='cmd_'||replace(command_id,'-','') WHERE session_id NOT LIKE 'ses\\_%' ESCAPE '\\'",[])?;
    }
    if has_table(&tx, "squad_tasks")? {
        tx.execute("UPDATE squad_tasks SET id='task_'||replace(id,'-','') WHERE id NOT LIKE 'task\\_%' ESCAPE '\\'",[])?;
    }
    if has_table(&tx, "squad_runs")? {
        tx.execute("UPDATE squad_runs SET id='run_'||replace(id,'-',''), task_id='task_'||replace(task_id,'-',''), session_id=CASE WHEN session_id IS NULL THEN NULL ELSE 'ses_'||replace(session_id,'-','') END WHERE id NOT LIKE 'run\\_%' ESCAPE '\\'",[])?;
    }
    tx.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}
