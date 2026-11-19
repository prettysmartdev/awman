//! Execution/turn transactions on the shared API database. Callers decide policy;
//! this store enforces identity, CAS, immutability and crash projection bounds.
use crate::data::{
    automation_records::*,
    error::DataError,
    execution_event::{EventCursor, ExecutionEvent},
    fs::api_db::{CommandResult, SqliteSessionStore},
    ids::{CommandId, ExecutionId, TurnId},
    terminal::TerminalFrame,
};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use uuid::Uuid;
mod credentials;

type Result<T> = std::result::Result<T, DataError>;
fn invalid(message: &str) -> DataError {
    DataError::Other(message.into())
}
fn json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|_| invalid("invalid automation record"))
}
fn decode<T: DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_str(value).map_err(|_| invalid("invalid stored automation record"))
}
fn cas(changed: usize) -> Result<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(invalid("automation compare-and-set failed"))
    }
}
fn enum_text<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_value(value)
        .map_err(|_| invalid("invalid enum"))?
        .as_str()
        .ok_or_else(|| invalid("invalid enum"))?
        .to_owned())
}
pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(include_str!("automation_store/schema.sql"))?;
    let definition: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE name='agent_request_dedup'",
        [],
        |r| r.get(0),
    )?;
    if !definition.contains("credential_update") {
        conn.execute_batch(
            "BEGIN IMMEDIATE;
        ALTER TABLE agent_request_dedup RENAME TO agent_request_dedup_previous;
        DROP INDEX IF EXISTS agent_request_dedup_command;
        DROP INDEX IF EXISTS agent_close_one_pending;",
        )?;
        conn.execute_batch(include_str!("automation_store/schema.sql"))?;
        conn.execute_batch("INSERT INTO agent_request_dedup SELECT * FROM agent_request_dedup_previous; DROP TABLE agent_request_dedup_previous; COMMIT;")?;
    }
    conn.execute_batch(include_str!("automation_store/credentials.sql"))?;
    let has_exposed: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('agent_credential_updates') WHERE name='exposed')", [], |r| r.get(0))?;
    if !has_exposed {
        // Previous versions could deliver while still pending: preserve uncertainty.
        conn.execute_batch("BEGIN IMMEDIATE; ALTER TABLE agent_credential_updates ADD COLUMN exposed INTEGER NOT NULL DEFAULT 0 CHECK(exposed IN (0,1)); UPDATE agent_credential_updates SET exposed=1 WHERE status IN ('pending','applying','restarting'); COMMIT;")?;
    }
    Ok(())
}
#[derive(Clone)]
pub struct AutomationStore {
    conn: Arc<Mutex<Connection>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    SubmitTurn,
    CloseExecution,
    CredentialUpdate,
}
/// Internal acceptance marker is deliberately not a wire frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "frame", rename_all = "snake_case")]
pub enum StoredReply {
    Accepted(Box<TerminalFrame>),
    PendingClose,
    CredentialUpdate,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestLookup {
    Missing,
    Replay {
        response: StoredReply,
        result: Option<TerminalFrame>,
    },
    Conflict,
}
pub struct NewExecution {
    pub record: ExecutionRecord,
}
pub struct Reservation {
    pub turn: TurnRecord,
    pub owner_key: String,
    pub digest: String,
    pub snapshot: AutomationSnapshot,
    pub publication: Option<LifecycleProjection>,
}
/// One bounded, already-redacted projection awaiting the existing log's flush barrier.
#[derive(Debug, Clone)]
pub struct LifecycleProjection {
    pub events: Vec<ExecutionEvent>,
    pub cursor: EventCursor,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnPage {
    pub items: Vec<TurnRecord>,
    pub next_cursor: Option<String>,
}
pub fn operation_digest(
    operation: Operation,
    execution: &ExecutionId,
    revision: u64,
    prompt: &str,
) -> Result<String> {
    if revision > MAX_SAFE_COUNTER {
        return Err(invalid("agent_protocol_error"));
    }
    if operation == Operation::CredentialUpdate {
        return Err(invalid("credential metadata must not be hashed"));
    }
    let revision = revision.to_string();
    let prompt = if operation == Operation::CloseExecution {
        ""
    } else {
        prompt
    };
    let operation = enum_text(&operation)?;
    let mut hash = Sha256::new();
    for part in [
        "awman.turn-operation.v1",
        operation.as_str(),
        execution.as_str(),
        revision.as_str(),
        prompt,
    ] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
// The fixed lists are the sole SQL/typed projection mapping. No client SQL identifiers.
const EXEC_COLUMNS: &[(&str, &str, bool)] = &[
    ("execution_id", "execution_id", false),
    ("command_id", "command_id", false),
    ("attempt", "attempt", false),
    ("owner_key", "owner_key", false),
    ("execution_ordinal", "execution_ordinal", false),
    ("terminal_id", "terminal_id", false),
    ("provider", "provider", false),
    ("runtime", "runtime", false),
    ("provider_version", "provider_version", false),
    ("image_digest", "image_digest", false),
    ("profile_binding_json", "profile_binding", true),
    (
        "requested_native_session_id",
        "requested_native_session_id",
        false,
    ),
    ("native_session_id", "native_session_id", false),
    ("automation_mode", "automation_mode", false),
    ("minimum_status_support", "minimum_status_support", false),
    ("native_start_mode", "native_start_mode", false),
    ("workspace_access", "workspace_access", false),
    ("timeouts_json", "timeouts", true),
    ("snapshot_json", "snapshot", true),
    ("revision", "revision", false),
    ("active_turn_id", "active_turn_id", false),
    ("launch_phase", "launch_phase", false),
    ("process_started_at", "process_started_at", false),
    ("created_at", "created_at", false),
    ("ended_at", "ended_at", false),
    ("end_failure_json", "end_failure", true),
    ("final_assurance_json", "final_assurance", true),
    ("pending_events_json", "pending_events", true),
    ("pending_event_cursor", "pending_event_cursor", true),
];
const TURN_COLUMNS: &[(&str, &str, bool)] = &[
    ("request_id", "request_id", false),
    ("turn_id", "turn_id", false),
    ("execution_id", "execution_id", false),
    ("command_id", "command_id", false),
    ("attempt", "attempt", false),
    ("ordinal", "ordinal", false),
    ("native_session_id", "native_session_id", false),
    ("native_turn_id", "native_turn_id", false),
    ("accepted_at", "accepted_at", false),
    ("started_at", "started_at", false),
    ("completed_at", "completed_at", false),
    ("accepted_revision", "accepted_revision", false),
    ("admission_support", "status_support", false),
    ("admission_source", "status_source", false),
    ("result_json", "result", true),
];
fn read_record<T: DeserializeOwned>(
    conn: &Connection,
    table: &str,
    columns: &[(&str, &str, bool)],
    filter: &str,
    values: &[&dyn rusqlite::ToSql],
) -> Result<Option<T>> {
    let sql = format!(
        "SELECT {} FROM {table} WHERE {filter}",
        columns.iter().map(|c| c.0).collect::<Vec<_>>().join(",")
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(values)?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let mut object = serde_json::Map::new();
    for (i, (_, name, encoded)) in columns.iter().enumerate() {
        use rusqlite::types::ValueRef;
        let value = match row.get_ref(i)? {
            ValueRef::Null => serde_json::Value::Null,
            ValueRef::Integer(n) => serde_json::Value::from(n),
            ValueRef::Text(s) => {
                let s = std::str::from_utf8(s).map_err(|_| invalid("invalid UTF-8 record"))?;
                if *encoded {
                    decode(s)?
                } else {
                    serde_json::Value::from(s)
                }
            }
            _ => return Err(invalid("invalid stored column")),
        };
        object.insert(name.to_string(), value);
    }
    serde_json::from_value(serde_json::Value::Object(object))
        .map(Some)
        .map_err(|_| invalid("invalid stored automation record"))
}
fn insert_record<T: Serialize>(
    conn: &Connection,
    table: &str,
    columns: &[(&str, &str, bool)],
    record: &T,
    extra: &[(&str, rusqlite::types::Value)],
) -> Result<()> {
    let object = serde_json::to_value(record).map_err(|_| invalid("invalid record"))?;
    let mut names = Vec::new();
    let mut values = Vec::new();
    for (column, name, encoded) in columns {
        names.push(*column);
        let value = &object[name];
        values.push(if value.is_null() {
            rusqlite::types::Value::Null
        } else if *encoded {
            json(value)?.into()
        } else if let Some(n) = value.as_u64() {
            i64::try_from(n)
                .map_err(|_| invalid("unsafe counter"))?
                .into()
        } else {
            value
                .as_str()
                .ok_or_else(|| invalid("invalid record field"))?
                .to_owned()
                .into()
        });
    }
    for (name, value) in extra {
        names.push(*name);
        values.push(value.clone());
    }
    let placeholders = vec!["?"; names.len()].join(",");
    conn.execute(
        &format!(
            "INSERT INTO {table} ({}) VALUES ({placeholders})",
            names.join(",")
        ),
        rusqlite::params_from_iter(values),
    )?;
    Ok(())
}
fn execution(conn: &Connection, id: &ExecutionId) -> Result<ExecutionRecord> {
    let record: ExecutionRecord = read_record(
        conn,
        "agent_executions",
        EXEC_COLUMNS,
        "execution_id=?",
        &[id],
    )?
    .ok_or_else(|| invalid("execution not found"))?;
    validate_execution(&record)?;
    Ok(record)
}
fn validate_execution(record: &ExecutionRecord) -> Result<()> {
    if record.revision > MAX_SAFE_COUNTER
        || record.execution_ordinal == 0
        || record.execution_ordinal > MAX_SAFE_COUNTER
        || record.attempt == 0
        || record.owner_key.is_empty()
        || record.provider != record.profile_binding.provider
    {
        return Err(invalid("invalid execution binding"));
    }
    if let Some(snapshot) = &record.snapshot {
        if snapshot.execution_id != record.execution_id
            || snapshot.revision != record.revision
            || snapshot.active_turn_id != record.active_turn_id
            || snapshot.native_session_id != record.native_session_id
            || Some(snapshot.automation_mode) != record.automation_mode
            || Some(snapshot.minimum_status_support) != record.minimum_status_support
        {
            return Err(invalid("inconsistent execution snapshot"));
        }
        validate_snapshot(snapshot)?;
    } else if record.automation_mode.is_some() || record.active_turn_id.is_some() {
        return Err(invalid("missing execution snapshot"));
    }
    if let (Some(requested), Some(observed)) = (
        &record.requested_native_session_id,
        &record.native_session_id,
    ) {
        if requested != observed {
            return Err(invalid("native identity mismatch"));
        }
    }
    Ok(())
}
fn validate_snapshot(snapshot: &AutomationSnapshot) -> Result<()> {
    if snapshot.revision > MAX_SAFE_COUNTER
        || snapshot
            .status_reason
            .as_ref()
            .is_some_and(|s| s.len() > 2048)
        || json(&snapshot.capabilities)?.len() > 65536
    {
        return Err(invalid("automation snapshot exceeds bound"));
    }
    Ok(())
}
fn project(
    conn: &Connection,
    id: &ExecutionId,
    projection: Option<&LifecycleProjection>,
) -> Result<()> {
    if let Some(batch) = projection {
        let execution = execution(conn, id)?;
        for event in &batch.events {
            if event.attempt != execution.attempt
                || event.sequence > MAX_SAFE_COUNTER
                || event
                    .source
                    .as_ref()
                    .is_some_and(|s| s.execution_id.as_ref().is_some_and(|source| source != id))
            {
                return Err(invalid("invalid lifecycle attribution"));
            }
            match &event.payload {
                crate::data::execution_event::EventPayload::AgentState { snapshot }
                    if &snapshot.execution_id == id =>
                {
                    validate_snapshot(snapshot)?
                }
                crate::data::execution_event::EventPayload::AgentIdentity {
                    execution_id, ..
                }
                | crate::data::execution_event::EventPayload::AgentTurnStarted {
                    execution_id,
                    ..
                } if execution_id == id => {}
                crate::data::execution_event::EventPayload::AgentTurnCompleted {
                    execution_id,
                    result,
                    ..
                } if execution_id == id && json(result)?.len() <= 8 * 1024 * 1024 => {}
                _ => return Err(invalid("invalid lifecycle projection payload")),
            }
        }
        let encoded = json(&batch.events)?;
        if batch.events.is_empty()
            || batch.events.len() > 8
            || encoded.len() > 16 * 1024 * 1024
            || batch.events.last().map(ExecutionEvent::cursor) != Some(batch.cursor)
            || batch
                .events
                .windows(2)
                .any(|w| w[0].cursor() >= w[1].cursor())
        {
            return Err(invalid("invalid lifecycle projection"));
        }
        cas(conn.execute(
"UPDATE agent_executions SET pending_events_json=?1,pending_event_cursor=?2 WHERE execution_id=?3 AND pending_events_json IS NULL",
params![
    encoded,
    json(&batch.cursor)?,
    id,
])?)?;
    } else {
        let pending: bool = conn.query_row(
            "SELECT pending_events_json IS NOT NULL FROM agent_executions WHERE execution_id=?",
            [id],
            |r| r.get(0),
        )?;
        if pending {
            return Err(invalid("lifecycle projection not flushed"));
        }
    }
    Ok(())
}
fn transition(
    conn: &Connection,
    expected: u64,
    snapshot: &AutomationSnapshot,
    projection: Option<&LifecycleProjection>,
) -> Result<()> {
    validate_snapshot(snapshot)?;
    let old = execution(conn, &snapshot.execution_id)?;
    if old.launch_phase == LaunchPhase::Ended
        || snapshot.revision != next_counter(expected).map_err(invalid)?
        || old.revision != expected
    {
        return Err(invalid("stale_revision"));
    }
    if old
        .native_session_id
        .as_ref()
        .is_some_and(|id| Some(id) != snapshot.native_session_id.as_ref())
        || old.requested_native_session_id.as_ref().is_some_and(|id| {
            snapshot
                .native_session_id
                .as_ref()
                .is_some_and(|observed| observed != id)
        })
    {
        return Err(invalid("native identity mismatch"));
    }
    if snapshot.provider != old.provider
        || snapshot.native_start_mode != old.native_start_mode
        || snapshot.workspace_access != old.workspace_access
        || Some(snapshot.automation_mode) != old.automation_mode
        || Some(snapshot.minimum_status_support) != old.minimum_status_support
    {
        return Err(invalid("immutable execution policy"));
    }
    let pending: Option<String> = conn
        .query_row(
            "SELECT turn_id FROM agent_turns WHERE execution_id=? AND result_json IS NULL",
            [&snapshot.execution_id],
            |r| r.get(0),
        )
        .optional()?;
    if pending.as_deref() != snapshot.active_turn_id.as_ref().map(|id| id.as_str()) {
        return Err(invalid("inconsistent active turn"));
    }
    project(conn, &snapshot.execution_id, projection)?;
    cas(conn.execute(
"UPDATE agent_executions SET snapshot_json=?1,revision=?2,active_turn_id=?3,native_session_id=?4 WHERE execution_id=?5 AND revision=?6 AND launch_phase!='ended'",
params![
    json(snapshot)?,
    snapshot.revision as i64,
    snapshot.active_turn_id,
    snapshot.native_session_id.as_ref().map(|s|s.as_str()),
    snapshot.execution_id,
    expected as i64,
])?)
}
impl AutomationStore {
    pub fn open(store: Arc<SqliteSessionStore>) -> Result<Self> {
        let conn = store.shared_connection();
        {
            let guard = conn.lock().map_err(|_| invalid("database lock poisoned"))?;
            migrate(&guard)?;
        };
        Ok(Self { conn })
    }
    pub(crate) fn from_store(store: &SqliteSessionStore) -> Self {
        Self {
            conn: store.shared_connection(),
        }
    }
    /// Profile-only identity observation. Automation identities transition with their snapshot.
    pub fn observe_execution_identity(
        &self,
        id: &ExecutionId,
        native: &crate::data::settings_profile::NativeSessionId,
    ) -> Result<()> {
        self.transaction(|c| {
            let e = execution(c, id)?;
            if e.snapshot.is_some()
                || e.launch_phase == LaunchPhase::Ended
                || e.requested_native_session_id
                    .as_ref()
                    .is_some_and(|expected| expected != native)
                || e.native_session_id
                    .as_ref()
                    .is_some_and(|old| old != native)
            {
                return Err(invalid("native identity mismatch"));
            }
            cas(c.execute(
"UPDATE agent_executions SET native_session_id=? WHERE execution_id=? AND launch_phase!='ended'",
params![
    native.as_str(),
    id,
])?)
        })
    }
    fn transaction<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = f(&tx)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn reserve_execution(&self, new: NewExecution) -> Result<()> {
        let record = new.record;
        validate_execution(&record)?;
        if record.launch_phase != LaunchPhase::Reserved
            || record.ended_at.is_some()
            || record.pending_events.is_some()
            || record.final_assurance.is_some()
            || record.active_turn_id.is_some()
            || record.end_failure.is_some()
        {
            return Err(invalid("invalid execution reservation"));
        }
        self.transaction(|conn| {
            let (attempt, previous, status): (u32, Option<String>, String) = conn.query_row(
                "SELECT attempt,active_execution_id,status FROM commands WHERE id=?",
                [&record.command_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            if attempt!=record.attempt {
return Err(invalid("stale command attempt"));
}
            // A finalized command never gains another execution.
            if status != "queued" && status != "running" {
                return Err(invalid("the command is no longer running"));
            }
            if let Some(previous)=previous {
 if execution(conn,
&ExecutionId::parse(&previous).map_err(|_|invalid("invalid execution pointer"))?)?.launch_phase!=LaunchPhase::Ended {
return Err(invalid("previous execution is active"));
}
}
            let last:i64=conn.query_row("SELECT COALESCE(MAX(execution_ordinal),0) FROM agent_executions WHERE command_id=?",
[&record.command_id],
|r|r.get(0))?;
            if record.execution_ordinal!=next_counter(last as u64).map_err(invalid)? {
return Err(invalid("invalid execution ordinal"));
}
            insert_record(conn,
"agent_executions",
EXEC_COLUMNS,
&record,
&[])?;
            cas(conn.execute(
"UPDATE commands SET active_execution_id=? WHERE id=? AND attempt=?",
params![
    record.execution_id,
    record.command_id,
    record.attempt,
])?)
}
)
    }
    pub fn read_execution(&self, id: &ExecutionId) -> Result<ExecutionRecord> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        execution(&conn, id)
    }
    pub fn current_execution(&self, command: &CommandId) -> Result<Option<ExecutionRecord>> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        // One read transaction: the pointer and the row it names come from
        // the same snapshot even when another store connection commits.
        let conn = guard.unchecked_transaction()?;
        let id: Option<String> = conn
            .query_row(
                "SELECT active_execution_id FROM commands WHERE id=?",
                [command],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        id.map(|id| {
            execution(
                &conn,
                &ExecutionId::parse(&id).map_err(|_| invalid("invalid execution pointer"))?,
            )
        })
        .transpose()
    }
    pub fn list_recoverable(&self) -> Result<Vec<ExecutionRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        let mut stmt=conn.prepare("SELECT execution_id FROM agent_executions WHERE launch_phase!='ended' OR pending_events_json IS NOT NULL ORDER BY created_at,execution_ordinal")?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|id| {
                execution(
                    &conn,
                    &ExecutionId::parse(&id).map_err(|_| invalid("invalid execution id"))?,
                )
            })
            .collect()
    }
    pub fn mark_launching(&self, id: &ExecutionId) -> Result<()> {
        self.transaction(|c|cas(c.execute(
"UPDATE agent_executions SET launch_phase='launching' WHERE execution_id=? AND launch_phase='reserved'",
[id])?))
    }
    pub fn mark_running(&self, id: &ExecutionId) -> Result<()> {
        self.transaction(|c|cas(c.execute(
"UPDATE agent_executions SET launch_phase='running',process_started_at=? WHERE execution_id=? AND launch_phase='launching'",
params![
    Utc::now().to_rfc3339(),
    id,
])?))
    }
    pub fn lookup_request(
        &self,
        owner: &str,
        id: &ExecutionId,
        request: Uuid,
        digest: &str,
    ) -> Result<RequestLookup> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        lookup(&conn, owner, id, request, digest)
    }
    pub fn reserve_turn(&self, reservation: Reservation, expected: u64) -> Result<TerminalFrame> {
        let t = &reservation.turn;
        let s = &reservation.snapshot;
        if t.native_turn_id.is_some()
            || t.result.is_some()
            || t.completed_at.is_some()
            || t.started_at.is_some()
            || t.accepted_revision != expected
            || s.execution_id != t.execution_id
            || s.active_turn_id.as_ref() != Some(&t.turn_id)
            // Interactive submissions reserve while `submitting`; a headless
            // one-shot reserves its single turn while still `starting` (B1).
            || !matches!(
                s.state,
                crate::data::automation::AutomationState::Submitting
                    | crate::data::automation::AutomationState::Starting
            )
        {
            return Err(invalid("invalid turn reservation"));
        }
        self.transaction(|c| {
            let e = execution(c, &t.execution_id)?;
            if e.owner_key != reservation.owner_key
                || e.command_id != t.command_id
                || e.attempt != t.attempt
                || e.active_turn_id.is_some()
                || e.native_session_id != t.native_session_id
            {
                return Err(invalid("invalid turn binding"));
            }
            // A close is not another turn: no reservation after close intent.
            let closing: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_request_dedup WHERE execution_id=? AND operation='close_execution' AND result_json IS NULL)",
                [&t.execution_id],
                |r| r.get(0),
            )?;
            if closing
                || e.snapshot.as_ref().is_some_and(|s| {
                    matches!(
                        s.state,
                        crate::data::automation::AutomationState::Stopping
                            | crate::data::automation::AutomationState::Ended
                    )
                })
            {
                return Err(invalid("execution is closing"));
            }
            if lookup(
                c,
                &e.owner_key,
                &t.execution_id,
                t.request_id,
                &reservation.digest,
            )? != RequestLookup::Missing
            {
                return Err(invalid("request already reserved"));
            }
            let last: i64 = c.query_row(
                "SELECT COALESCE(MAX(ordinal),0) FROM agent_turns WHERE command_id=?",
                [&t.command_id],
                |r| r.get(0),
            )?;
            if t.ordinal != next_counter(last as u64).map_err(invalid)? {
                return Err(invalid("invalid turn ordinal"));
            }
            let frame = TerminalFrame::TurnAccepted {
                request_id: t.request_id,
                execution_id: t.execution_id.clone(),
                turn_id: t.turn_id.clone(),
                status_support: t.status_support,
                status_source: t.status_source,
            };
            insert_dedup(
                c,
                &e,
                t.request_id,
                Operation::SubmitTurn,
                &reservation.digest,
                &StoredReply::Accepted(Box::new(frame.clone())),
            )?;
            insert_record(
                c,
                "agent_turns",
                TURN_COLUMNS,
                t,
                &[
                    ("owner_key", e.owner_key.into()),
                    ("delivery_phase", "reserved".to_owned().into()),
                ],
            )?;
            transition(c, expected, s, reservation.publication.as_ref())?;
            Ok(frame)
        })
    }
    pub fn transition_execution(
        &self,
        expected: u64,
        snapshot: &AutomationSnapshot,
        projection: Option<&LifecycleProjection>,
    ) -> Result<()> {
        self.transaction(|c| transition(c, expected, snapshot, projection))
    }
    pub fn mark_delivery(
        &self,
        turn: &TurnId,
        phase: DeliveryPhase,
        expected_execution: &ExecutionId,
    ) -> Result<()> {
        if phase == DeliveryPhase::Terminal {
            return Err(invalid("terminal delivery requires result"));
        }
        self.transaction(|c| {
            let old:String=c.query_row(
"SELECT delivery_phase FROM agent_turns WHERE turn_id=? AND execution_id=? AND result_json IS NULL",
params![
    turn,
    expected_execution,
],
|r|r.get(0))?;
            let old:DeliveryPhase=decode(&json(&old)?)?;
            if phase<old {
return Err(invalid("delivery cannot regress"));
}
            cas(c.execute(
"UPDATE agent_turns SET delivery_phase=? WHERE turn_id=? AND execution_id=? AND result_json IS NULL",
params![
    enum_text(&phase)?,
    turn,
    expected_execution,
])?)
}
)
    }
    pub fn observe_turn(
        &self,
        turn: &TurnId,
        execution_id: &ExecutionId,
        native_session_id: Option<&crate::data::settings_profile::NativeSessionId>,
        native_turn_id: Option<&str>,
        started_at: Option<chrono::DateTime<Utc>>,
    ) -> Result<()> {
        let started_at = started_at.map(|time| {
            chrono::DateTime::from_timestamp_millis(time.timestamp_millis())
                .expect("valid timestamp")
        });
        if let Some(id) = native_turn_id {
            crate::data::settings_profile::NativeSessionId::parse(id)
                .map_err(|_| invalid("invalid native turn id"))?;
        }
        self.transaction(|c| {
            let e=execution(c,
execution_id)?;
            let t:TurnRecord=read_record(c,
"agent_turns",
TURN_COLUMNS,
"turn_id=? AND execution_id=?",
&[turn,
execution_id])?.ok_or_else(||invalid("turn not found"))?;
            if t.result.is_some() || (native_session_id.is_some() && native_session_id!=e.native_session_id.as_ref()) || t.native_session_id.as_ref().is_some_and(|id|Some(id)!=native_session_id) || t.native_turn_id.as_deref().is_some_and(|id|Some(id)!=native_turn_id) || (t.started_at.is_some() && started_at.is_some() && t.started_at!=started_at) {
return Err(invalid("immutable turn observation"));
}
            cas(c.execute(
"UPDATE agent_turns SET native_session_id=COALESCE(native_session_id,?1),native_turn_id=COALESCE(native_turn_id,?2),started_at=COALESCE(started_at,?3) WHERE turn_id=?4 AND execution_id=?5 AND result_json IS NULL",
params![
    native_session_id.map(|id|id.as_str()),
    native_turn_id,
    started_at.map(|t|t.to_rfc3339()),
    turn,
    execution_id,
])?)
}
)
    }
    pub fn complete_turn(
        &self,
        turn: &TurnId,
        result: &TurnResult,
        snapshot: &AutomationSnapshot,
        projection: Option<&LifecycleProjection>,
    ) -> Result<()> {
        let encoded = json(result)?;
        if encoded.len() > 8 * 1024 * 1024 {
            return Err(invalid("turn result exceeds bound"));
        }
        // Deserialize too: validates opaque native identity even for Rust producers.
        let _: TurnResult = decode(&encoded)?;
        self.transaction(|c| {
            let t:TurnRecord=read_record(c,
"agent_turns",
TURN_COLUMNS,
"turn_id=?",
&[turn])?.ok_or_else(||invalid("turn not found"))?;
            if let Some(old)=t.result {
return if old==*result {
Ok(())
}
 else {
Err(invalid("immutable turn result"))
}
;
}
            let e=execution(c,
&t.execution_id)?;
            if snapshot.execution_id!=t.execution_id || e.active_turn_id.as_ref()!=Some(turn) || snapshot.active_turn_id.is_some() || (t.native_turn_id.is_some() && t.native_turn_id!=result.native_turn_id) {
return Err(invalid("invalid turn completion"));
}
            let now=Utc::now().to_rfc3339();
            cas(c.execute(
"UPDATE agent_turns SET result_json=?1,completed_at=?2,delivery_phase='terminal',native_turn_id=COALESCE(native_turn_id,?3) WHERE turn_id=?4 AND result_json IS NULL",
params![
    encoded,
    now,
    result.native_turn_id,
    turn,
])?)?;
            let frame=TerminalFrame::TurnResult{
request_id:t.request_id,
execution_id:t.execution_id.clone(),
turn_id:t.turn_id,
result:result.clone()
}
;
            cas(c.execute(
"UPDATE agent_request_dedup SET result_json=?,completed_at=? WHERE owner_key=? AND execution_id=? AND request_id=? AND result_json IS NULL",
params![
    json(&frame)?,
    now,
    e.owner_key,
    t.execution_id,
    t.request_id.to_string(),
])?)?;
            transition(c,
e.revision,
snapshot,
projection)
}
)
    }
    pub fn reserve_close(
        &self,
        owner: &str,
        request: &CloseExecutionRequest,
        digest: &str,
        snapshot: &AutomationSnapshot,
        projection: Option<&LifecycleProjection>,
    ) -> Result<()> {
        self.transaction(|c| {
            let e = execution(c, &request.execution_id)?;
            if e.owner_key != owner
                || e.active_turn_id.is_some()
                || snapshot.execution_id != request.execution_id
                || snapshot.active_turn_id.is_some()
                || snapshot.state != crate::data::automation::AutomationState::Stopping
            {
                return Err(invalid("invalid close reservation"));
            }
            insert_dedup(
                c,
                &e,
                request.request_id,
                Operation::CloseExecution,
                digest,
                &StoredReply::PendingClose,
            )?;
            transition(c, request.expected_revision, snapshot, projection)
        })
    }
    pub fn complete_close(
        &self,
        owner: &str,
        id: &ExecutionId,
        request: Uuid,
        result: &CommandResult,
    ) -> Result<TerminalFrame> {
        self.transaction(|c| {
            let frame=TerminalFrame::ExecutionClosed{
request_id:request,
execution_id:id.clone(),
result:Box::new(result.clone())
}
;
            let e=execution(c,
id)?;
            let final_result:Option<String>=c.query_row(
"SELECT result FROM commands WHERE id=?",
[&e.command_id],
|r|r.get(0))?;
            if e.launch_phase!=LaunchPhase::Ended || final_result.as_deref().map(decode::<CommandResult>).transpose()?.as_ref()!=Some(result) {
return Err(invalid("close requires final stored command result"));
}
            let old:Option<String>=c.query_row(
"SELECT result_json FROM agent_request_dedup WHERE owner_key=? AND execution_id=? AND request_id=? AND operation='close_execution'",
params![
    owner,
    id,
    request.to_string(),
],
|r|r.get(0))?;
            if let Some(old)=old {
if decode::<TerminalFrame>(&old)?==frame {
return Ok(frame);
}
return Err(invalid("immutable close result"));
}
            cas(c.execute(
"UPDATE agent_request_dedup SET result_json=?,completed_at=? WHERE owner_key=? AND execution_id=? AND request_id=? AND operation='close_execution' AND result_json IS NULL",
params![
    json(&frame)?,
    Utc::now().to_rfc3339(),
    owner,
    id,
    request.to_string(),
])?)?;
 Ok(frame)
}
)
    }
    pub fn finish_execution(
        &self,
        id: &ExecutionId,
        assurance: &CommandAutomationResult,
        failure: Option<&AutomationFailure>,
    ) -> Result<()> {
        self.transaction(|c| {
            let e=execution(c,
id)?;
            if assurance.execution_id!=*id || assurance.native_session_id!=e.native_session_id || e.active_turn_id.is_some() || e.pending_events.is_some() || e.snapshot.as_ref().is_some_and(|s|s.state!=crate::data::automation::AutomationState::Ended) {
return Err(invalid("execution not drained"));
}
            if e.launch_phase==LaunchPhase::Ended {
return if e.final_assurance.as_ref()==Some(assurance) && e.end_failure.as_ref()==failure {
Ok(())
}
 else {
Err(invalid("immutable execution result"))
}
;
}
            cas(c.execute(
"UPDATE agent_executions SET launch_phase='ended',ended_at=?,final_assurance_json=?,end_failure_json=? WHERE execution_id=? AND launch_phase!='ended'",
params![
    Utc::now().to_rfc3339(),
    json(assurance)?,
    failure.map(json).transpose()?,
    id,
])?)
}
)
    }
    /// Uses the established command completion CAS; no second result owner.
    pub fn finalize_command(
        &self,
        command: &CommandId,
        completion: &crate::data::command_status::CommandCompletion,
    ) -> Result<bool> {
        self.transaction(|c| {
            let pending:bool=c.query_row(
"SELECT EXISTS(SELECT 1 FROM agent_executions WHERE command_id=? AND (launch_phase!='ended' OR pending_events_json IS NOT NULL))",
[command],
|r|r.get(0))?;
            if pending {
return Err(invalid("execution not finalized"));
}
            Ok(c.execute(
"UPDATE commands SET status=?,exit_code=?,finished_at=?,result=? WHERE id=? AND status='running' AND attempt=?",
params![
    completion.status.as_str(),
    completion.exit_code,
    completion.finished_at,
    completion.result_json,
    command,
    completion.attempt,
])?==1)
}
)
    }
    /// The durable delivery marker of an unsettled turn (`None` once settled
    /// or unknown). Crash recovery uses it to separate unacknowledged
    /// (`delivery_unknown`) from acknowledged (`execution_lost`) turns.
    pub fn turn_delivery_phase(&self, turn: &TurnId) -> Result<Option<DeliveryPhase>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        let phase: Option<String> = conn
            .query_row(
                "SELECT delivery_phase FROM agent_turns WHERE turn_id=? AND result_json IS NULL",
                [turn],
                |r| r.get(0),
            )
            .optional()?;
        phase.map(|p| decode(&json(&p)?)).transpose()
    }
    pub fn clear_pending_events(&self, id: &ExecutionId, cursor: EventCursor) -> Result<()> {
        self.transaction(|c|cas(c.execute(
"UPDATE agent_executions SET pending_events_json=NULL,pending_event_cursor=NULL WHERE execution_id=? AND pending_event_cursor=?",
params![
    id,
    json(&cursor)?,
])?))
    }
    pub fn list_turns(
        &self,
        command: &CommandId,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<TurnPage> {
        use base64::Engine;
        let last = if let Some(cursor) = cursor {
            if cursor.len() > 1024 {
                return Err(invalid("invalid_request"));
            }
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(cursor)
                .map_err(|_| invalid("invalid_request"))?;
            let (bound, last): (CommandId, u64) =
                serde_json::from_slice(&bytes).map_err(|_| invalid("invalid_request"))?;
            if &bound != command || last > MAX_SAFE_COUNTER {
                return Err(invalid("invalid_request"));
            }
            last
        } else {
            0
        };
        let limit = limit.unwrap_or(50);
        if limit == 0 || limit > 200 {
            return Err(invalid("invalid_request"));
        }
        let guard = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        // The id list and its rows come from one read snapshot: retention on
        // another connection cannot delete rows between the two queries.
        let conn = guard.unchecked_transaction()?;
        let mut stmt=conn.prepare("SELECT turn_id FROM agent_turns WHERE command_id=? AND ordinal>? ORDER BY ordinal LIMIT ?")?;
        let ids = stmt
            .query_map(params![command, last as i64, (limit + 1) as i64,], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let more = ids.len() > limit;
        let items = ids
            .into_iter()
            .take(limit)
            .map(|id| {
                read_record(&conn, "agent_turns", TURN_COLUMNS, "turn_id=?", &[&id])?
                    .ok_or_else(|| invalid("turn not found"))
            })
            .collect::<Result<Vec<TurnRecord>>>()?;
        let next_cursor = if more {
            Some(
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json(&(
                    command,
                    items.last().expect("nonempty page").ordinal,
                ))?),
            )
        } else {
            None
        };
        Ok(TurnPage { items, next_cursor })
    }
}
fn lookup(
    c: &Connection,
    owner: &str,
    id: &ExecutionId,
    request: Uuid,
    digest: &str,
) -> Result<RequestLookup> {
    let row:Option<(Option<String>,
String,
Option<String>)>=c.query_row(
"SELECT operation_digest,response_json,result_json FROM agent_request_dedup WHERE owner_key=? AND execution_id=? AND request_id=?",
params![
    owner,
    id,
    request.to_string(),
],
|r|Ok((r.get(0)?,
r.get(1)?,
r.get(2)?))).optional()?;
    match row {
        None => Ok(RequestLookup::Missing),
        Some((saved, _, _)) if saved.as_deref() != Some(digest) => Ok(RequestLookup::Conflict),
        Some((_, response, result)) => Ok(RequestLookup::Replay {
            response: decode(&response)?,
            result: result.map(|s| decode(&s)).transpose()?,
        }),
    }
}
fn insert_dedup(
    c: &Connection,
    e: &ExecutionRecord,
    request: Uuid,
    operation: Operation,
    digest: &str,
    response: &StoredReply,
) -> Result<()> {
    c.execute(
"INSERT INTO agent_request_dedup(owner_key,execution_id,request_id,command_id,operation,operation_digest,response_json,created_at) VALUES (?,?,?,?,?,?,?,?)",
params![
    e.owner_key,
    e.execution_id,
    request.to_string(),
    e.command_id,
    enum_text(&operation)?,
    digest,
    json(response)?,
    Utc::now().to_rfc3339(),
])?;
    Ok(())
}

/// An OS advisory lease proving that a live process owns one execution.
/// The owner holds it from reservation until its command result is stored;
/// crash recovery skips any execution whose lease is still held, so a shared
/// data-home database never lets one process finalize another's live run.
/// Dropping the lease (or the owning process dying) releases it.
#[derive(Debug)]
pub struct ExecutionLease {
    _file: std::fs::File,
    path: std::path::PathBuf,
}

impl Drop for ExecutionLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn lease_io(path: &std::path::Path, error: std::io::Error) -> DataError {
    DataError::Other(format!("execution lease {}: {error}", path.display()))
}

impl AutomationStore {
    /// `<db dir>/automation-leases/<execution>.lock`; `None` for an
    /// in-memory database (one process by construction).
    fn lease_path(&self, id: &ExecutionId) -> Option<std::path::PathBuf> {
        let conn = self.conn.lock().ok()?;
        let db = conn.path().filter(|p| !p.is_empty())?;
        let dir = std::path::Path::new(db).parent()?.join("automation-leases");
        Some(dir.join(format!("{}.lock", id.as_str())))
    }

    fn open_lease(path: &std::path::Path, create: bool) -> std::io::Result<std::fs::File> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(create);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        options.open(path)
    }

    /// Take this execution's lease (exclusive, non-blocking).
    pub fn acquire_lease(&self, id: &ExecutionId) -> Result<Option<ExecutionLease>> {
        let Some(path) = self.lease_path(id) else {
            return Ok(None);
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| lease_io(dir, e))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
        }
        let file = Self::open_lease(&path, true).map_err(|e| lease_io(&path, e))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(ExecutionLease { _file: file, path })),
            Err(std::fs::TryLockError::WouldBlock) => Err(invalid("execution lease is held")),
            Err(std::fs::TryLockError::Error(e)) => Err(lease_io(&path, e)),
        }
    }

    /// Whether a live process (this one included, through another handle)
    /// holds this execution's lease now.
    pub fn lease_held(&self, id: &ExecutionId) -> Result<bool> {
        let Some(path) = self.lease_path(id) else {
            return Ok(false);
        };
        let file = match Self::open_lease(&path, false) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(lease_io(&path, e)),
        };
        match file.try_lock() {
            Ok(()) => Ok(false),
            Err(std::fs::TryLockError::WouldBlock) => Ok(true),
            Err(std::fs::TryLockError::Error(e)) => Err(lease_io(&path, e)),
        }
    }

    /// Ended executions whose command is still running: the owner finished
    /// the execution but never stored the command result (a crash in that
    /// window). Only each command's latest execution is considered.
    pub fn list_unfinalized_ended(&self) -> Result<Vec<ExecutionRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| invalid("database lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT e.execution_id FROM agent_executions e JOIN commands c ON c.id = e.command_id
             WHERE e.launch_phase = 'ended' AND e.pending_events_json IS NULL AND c.status = 'running'
               AND e.execution_ordinal = (SELECT MAX(x.execution_ordinal) FROM agent_executions x WHERE x.command_id = e.command_id)
             ORDER BY e.created_at",
        )?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|id| {
                execution(
                    &conn,
                    &ExecutionId::parse(&id).map_err(|_| invalid("invalid execution id"))?,
                )
            })
            .collect()
    }

    /// One turn of an execution, by ID (any page of the command history).
    pub fn read_turn(&self, execution: &ExecutionId, turn: &TurnId) -> Result<Option<TurnRecord>> {
        let page_owner = {
            let conn = self
                .conn
                .lock()
                .map_err(|_| invalid("database lock poisoned"))?;
            conn.query_row(
                "SELECT command_id, ordinal FROM agent_turns WHERE turn_id = ? AND execution_id = ?",
                params![turn, execution],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?
        };
        let Some((command, ordinal)) = page_owner else {
            return Ok(None);
        };
        let command = CommandId::parse(&command).map_err(|_| invalid("invalid command id"))?;
        // Read exactly that row through the established decoder: the page
        // that starts just before its ordinal.
        use base64::Engine;
        let cursor = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&(&command, (ordinal - 1).max(0) as u64))
                .map_err(|_| invalid("invalid turn cursor"))?,
        );
        let page = self.list_turns(&command, Some(&cursor), Some(1))?;
        Ok(page.items.into_iter().find(|t| &t.turn_id == turn))
    }
}
