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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn squad_id_migration_preserves_foreign_keys_even_after_the_api_version_bump() {
        for version in [0, CURRENT_SCHEMA_VERSION] {
            let connection = Connection::open_in_memory().unwrap();
            connection.execute_batch(
                "PRAGMA foreign_keys=ON;
                 CREATE TABLE squad_tasks(id TEXT PRIMARY KEY);
                 CREATE TABLE squad_runs(id TEXT PRIMARY KEY, task_id TEXT REFERENCES squad_tasks(id), session_id TEXT);
                 INSERT INTO squad_tasks VALUES('11111111-1111-4111-8111-111111111111');
                 INSERT INTO squad_runs VALUES('22222222-2222-4222-8222-222222222222', '11111111-1111-4111-8111-111111111111', NULL);"
            ).unwrap();
            connection
                .pragma_update(None, "user_version", version)
                .unwrap();
            migrate_ids(&connection).unwrap();
            migrate_ids(&connection).unwrap();
            let (task, run): (String, String) = connection
                .query_row("SELECT task_id, id FROM squad_runs", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .unwrap();
            assert_eq!(task, "task_11111111111141118111111111111111");
            assert_eq!(run, "run_22222222222242228222222222222222");
            let violations = connection
                .prepare("PRAGMA foreign_key_check")
                .unwrap()
                .query([])
                .unwrap()
                .next()
                .unwrap()
                .is_some();
            assert!(!violations);
        }
    }

    #[test]
    fn migration_is_idempotent_and_sets_version() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE sessions(id TEXT PRIMARY KEY); CREATE TABLE commands(id TEXT PRIMARY KEY, session_id TEXT, status TEXT, queued_at TEXT); CREATE TABLE labels(owner_kind TEXT,owner_id TEXT,key TEXT,value TEXT); INSERT INTO sessions VALUES('00000000-0000-0000-0000-000000000001'); INSERT INTO commands VALUES('00000000-0000-0000-0000-000000000002','00000000-0000-0000-0000-000000000001','running','2026-01-01T00:00:00Z'); INSERT INTO labels VALUES('command','00000000-0000-0000-0000-000000000002','team/owner','alice');").unwrap();
        migrate_ids(&c).unwrap();
        migrate_ids(&c).unwrap();
        let id: String = c
            .query_row("SELECT id FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(id, "ses_00000000000000000000000000000001");
        let (cmd, session, status, queued): (String, String, String, String) = c
            .query_row(
                "SELECT id,session_id,status,queued_at FROM commands",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(cmd, "cmd_00000000000000000000000000000002");
        assert_eq!(session, id);
        assert_eq!(status, "running");
        assert_eq!(queued, "2026-01-01T00:00:00Z");
        let label: String = c
            .query_row("SELECT owner_id FROM labels", [], |r| r.get(0))
            .unwrap();
        assert_eq!(label, cmd);
        let old_lookup: Option<String> = c
            .query_row(
                "SELECT id FROM commands WHERE id=?1",
                ["00000000-0000-0000-0000-000000000002"],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(old_lookup, None);
        assert_eq!(
            c.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn migration_updates_legacy_idempotency_scopes_and_compensating_session_ids() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE sessions(id TEXT PRIMARY KEY); CREATE TABLE commands(id TEXT PRIMARY KEY, session_id TEXT, status TEXT, queued_at TEXT); CREATE TABLE idempotency_keys(scope TEXT, key TEXT, resource_kind TEXT, resource_id TEXT); INSERT INTO sessions VALUES('11111111-1111-1111-1111-111111111111'); INSERT INTO idempotency_keys VALUES('/v1/commands|principal','command-key','command','22222222-2222-2222-2222-222222222222'); INSERT INTO idempotency_keys VALUES('/v1/sessions|principal','session-key','compensating_session','11111111-1111-1111-1111-111111111111'); INSERT INTO idempotency_keys VALUES('/v1/runs|principal','run-key','command','33333333-3333-3333-3333-333333333333');").unwrap();
        migrate_ids(&c).unwrap();
        let rows: Vec<(String, String, String)> = c
            .prepare("SELECT scope, resource_kind, resource_id FROM idempotency_keys ORDER BY key")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(
            rows,
            vec![
                (
                    "/v1/worker/commands|principal".into(),
                    "command".into(),
                    "cmd_22222222222222222222222222222222".into()
                ),
                (
                    "/v1/worker/runs|principal".into(),
                    "command".into(),
                    "cmd_33333333333333333333333333333333".into()
                ),
                (
                    "/v1/worker/sessions|principal".into(),
                    "compensating_session".into(),
                    "ses_11111111111111111111111111111111".into()
                ),
            ]
        );
    }
}
