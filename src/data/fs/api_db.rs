//! Sqlite-backed persistence for API-mode session and command metadata.
//!
//! Schema parity with `oldsrc/commands/headless/db.rs` is preserved so that
//! existing on-disk databases written by prior awman releases can be opened
//! by the new store without losing state.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension};

use crate::data::error::DataError;
use crate::data::execution_event::FIRST_ATTEMPT;
use crate::data::fs::api_labels::{LabelSelector, Labels};
use crate::data::fs::api_paths::ApiPaths;
use crate::data::session::SessionKind;
use crate::data::session_setup_event::SessionSetupStatus;

/// Persistable session metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: String,
    pub workdir: String,
    pub created_at: String,
    pub status: String,
    pub closed_at: Option<String>,
    /// Async setup status, one of `initializing`, `cloning_repository`,
    /// `setting_up_branch`, `merging_refs`, `running_ready`, `ready`, `failed`. Set on
    /// session creation; updated by the setup task; consulted by the
    /// job-submission guard and the server-restart cleanup.
    pub setup_status: String,
    /// Session type — `local` (workdir mount) or `remote` (cloned repo).
    pub session_type: String,
    /// For `remote` sessions, the resolved clone destination on disk so that
    /// failure cleanup can remove the partial clone.
    pub cloned_path: Option<String>,
    /// For `remote` sessions, the repository URL the clone was made from.
    /// `None` on rows written before WI 0133.
    pub repo_url: Option<String>,
    /// For `remote` sessions, the session branch. `None` when the session
    /// stayed on the default branch, and on rows written before WI 0133.
    pub branch: Option<String>,
    /// For `remote` sessions, the start point a new session branch was cut
    /// from, as the caller gave it.
    pub base_ref: Option<String>,
    /// For `remote` sessions, the branches merged in at setup, in order.
    pub merge_refs: Vec<String>,
    /// For `remote` sessions, whether a successful command pushes the branch.
    pub push: bool,
}

/// The remote-session inputs recorded on a row right after it is inserted
/// (WI 0133). Kept out of [`NewSessionRow`] so local-session callers do not
/// have to spell out five fields that never apply to them.
#[derive(Debug, Clone, Copy)]
pub struct RemoteSessionFields<'a> {
    pub repo_url: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub base_ref: Option<&'a str>,
    pub merge_refs: &'a [String],
    pub push: bool,
}

/// The columns a new session row is created with.
///
/// A struct rather than six positional parameters, two of which were
/// interchangeable `&str`s carrying an enum's spelling (WI 0114 F-48): the
/// call site now names each column, and `setup_status` and `kind` are the
/// types whose values they are, so a typo is a compile error rather than a
/// row nothing matches.
#[derive(Debug, Clone, Copy)]
pub struct NewSessionRow<'a> {
    pub id: &'a str,
    pub workdir: &'a str,
    pub created_at: &'a str,
    pub setup_status: SessionSetupStatus,
    pub kind: SessionKind,
    /// For `remote` sessions, the resolved clone destination on disk.
    pub cloned_path: Option<&'a str>,
}

/// Outcome of the session lifecycle guard for a newly-submitted command.
/// Returned by [`SqliteSessionStore::command_admission`]; the frontend maps
/// each variant to its transport-specific status and message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCommandAdmission {
    /// Session is `active` — accept the command.
    Accepted,
    /// No such session.
    NotFound,
    /// Session is draining (`closing`) and no longer accepts commands.
    Closing,
    /// Session is `closed`.
    Closed,
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
    pub worker_id: Option<String>,
    pub queued_at: Option<String>,
    pub result: Option<String>,
    /// Which execution of the command this is (1-based). A re-queue after a
    /// daemon restart increments it.
    pub attempt: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelOwner {
    Session,
    Command,
}
impl LabelOwner {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Command => "command",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CommandListFilter<'a> {
    pub session_id: Option<&'a str>,
    pub status: Option<&'a str>,
    pub labels: &'a [LabelSelector],
    pub limit: i64,
}

pub const IDEMPOTENCY_TTL_HOURS: u64 = 24;
pub const IDEMPOTENCY_KEY_MAX_BYTES: usize = 255;

#[derive(Debug, Clone, Copy)]
pub struct IdempotencyScope<'a> {
    pub route: &'a str,
    pub principal: &'a str,
}
impl IdempotencyScope<'_> {
    fn key(&self) -> String {
        format!("{}|{}", self.route, self.principal)
    }
}

/// Who holds an unfinished reservation: the daemon process (`instance`, one
/// id per process lifetime) and the request within it. A reservation whose
/// instance is not the caller's belongs to a daemon that has exited.
#[derive(Debug, Clone, Copy)]
pub struct IdempotencyOwner<'a> {
    pub instance: &'a str,
    pub request: &'a str,
}

/// The durable resource whose existence proves a request was admitted.
#[derive(Debug, Clone, Copy)]
pub enum IdempotencyResource<'a> {
    Session(&'a str),
    Command(&'a str),
}

impl IdempotencyResource<'_> {
    fn kind(&self) -> &'static str {
        match self {
            Self::Session(_) => "session",
            Self::Command(_) => "command",
        }
    }

    fn id(&self) -> &str {
        match self {
            Self::Session(id) | Self::Command(id) => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyBegin {
    Fresh,
    Replay { status: u16, body: String },
    InFlight,
    Mismatch,
}

/// Identifies a running worker task.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkerId(pub String);

impl Default for WorkerId {
    fn default() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl WorkerId {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorkerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Result of a completed command execution, serialized as JSON into the
/// `result` column.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommandResult {
    pub exit_code: Option<i32>,
    /// Checkout retained after dispatch, for deliberate resume or cleanup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_status: Option<u16>,
    /// The stage that failed when the failure was not the command itself
    /// (`"push"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// Present (possibly `null`) on every result of a session with
    /// `push: true`; absent when the session does not push.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub pushed: Option<Option<PushedRef>>,
}

/// What the push stage pushed after a successful command (WI 0133). `None`
/// inside [`CommandResult::pushed`] means nothing was pushed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PushedRef {
    /// Always `"origin"` today.
    pub remote: String,
    pub branch: String,
    pub sha: String,
}

/// Read a field that is present (even as `null`) as `Some(..)`, so a double
/// `Option` round-trips absent / `null` / value. Absent fields fall back to
/// `#[serde(default)]` and never reach this.
fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Some)
}

/// Sqlite-backed session and command store.
///
/// Opening the store creates the database and runs migrations idempotently.
pub struct SqliteSessionStore {
    conn: Mutex<Connection>,
}

impl SqliteSessionStore {
    /// Open (or create) a sqlite database at `<root>/awman.db`, run migrations,
    /// and enable WAL mode for concurrent reads.
    pub fn open(root: &Path) -> Result<Self, DataError> {
        Self::open_at(&root.join(crate::data::fs::api_paths::API_DB_FILENAME))
    }

    /// Open (or create) the database at an exact file path. This is the
    /// env-scoped entry point used after the database moved out of the API
    /// daemon directory into `DataPaths`.
    pub fn open_at(db_file: &Path) -> Result<Self, DataError> {
        let parent = db_file.parent().ok_or_else(|| {
            DataError::Other(format!(
                "database path has no parent: {}",
                db_file.display()
            ))
        })?;
        std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
        let conn = Connection::open(db_file)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Convenience constructor that opens at the path resolved from `paths`.
    pub fn open_from_paths(paths: &ApiPaths) -> Result<Self, DataError> {
        Self::open_at(&paths.db_path())
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
        // Additive column migrations: sqlite ALTER TABLE ADD COLUMN is safe
        // and idempotent when we ignore the "duplicate column" error.
        Self::add_column_if_missing(
            conn,
            "sessions",
            "setup_status",
            "TEXT NOT NULL DEFAULT 'ready'",
        )?;
        Self::add_column_if_missing(
            conn,
            "sessions",
            "session_type",
            "TEXT NOT NULL DEFAULT 'local'",
        )?;
        Self::add_column_if_missing(conn, "sessions", "cloned_path", "TEXT")?;
        // WI 0133: the remote-session inputs, so a restored session keeps them.
        // Pre-existing rows read back as no repo_url/branch/base_ref,
        // `merge_refs` NULL → [] and `push` 0 → false.
        Self::add_column_if_missing(conn, "sessions", "repo_url", "TEXT")?;
        Self::add_column_if_missing(conn, "sessions", "branch", "TEXT")?;
        Self::add_column_if_missing(conn, "sessions", "base_ref", "TEXT")?;
        // JSON array of branch names; NULL means none.
        Self::add_column_if_missing(conn, "sessions", "merge_refs", "TEXT")?;
        Self::add_column_if_missing(conn, "sessions", "push", "INTEGER NOT NULL DEFAULT 0")?;
        Self::add_column_if_missing(conn, "commands", "worker_id", "TEXT")?;
        Self::add_column_if_missing(conn, "commands", "queued_at", "TEXT")?;
        Self::add_column_if_missing(conn, "commands", "result", "TEXT")?;
        // WI 0136: SQLite applies the DEFAULT to every existing row on ADD
        // COLUMN, so pre-0136 commands read back as attempt 1. That is the
        // backfill; no separate UPDATE is needed.
        Self::add_column_if_missing(conn, "commands", "attempt", "INTEGER NOT NULL DEFAULT 1")?;
        // WI 0135: labels and idempotency are side tables so old databases
        // migrate additively without changing the stable session/command rows.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS labels (
                owner_kind TEXT NOT NULL CHECK (owner_kind IN ('session','command')),
                owner_id   TEXT NOT NULL,
                key        TEXT NOT NULL,
                value      TEXT NOT NULL,
                PRIMARY KEY (owner_kind, owner_id, key)
            );
            CREATE INDEX IF NOT EXISTS labels_lookup ON labels (owner_kind, key, value);
            CREATE TABLE IF NOT EXISTS idempotency_keys (
                scope        TEXT NOT NULL,
                key          TEXT NOT NULL,
                request_hash TEXT NOT NULL,
                status       INTEGER,
                body         TEXT,
                created_at   TEXT NOT NULL,
                PRIMARY KEY (scope, key)
            );
            CREATE INDEX IF NOT EXISTS idempotency_created ON idempotency_keys (created_at);",
        )?;
        // Reservation ownership (daemon instance + request) and the admission
        // intent: the response the owner will return once the named resource
        // exists. A NULL owner (older rows) reads as a dead owner.
        Self::add_column_if_missing(conn, "idempotency_keys", "owner_instance", "TEXT")?;
        Self::add_column_if_missing(conn, "idempotency_keys", "owner_request", "TEXT")?;
        Self::add_column_if_missing(conn, "idempotency_keys", "intent_status", "INTEGER")?;
        Self::add_column_if_missing(conn, "idempotency_keys", "intent_body", "TEXT")?;
        Self::add_column_if_missing(conn, "idempotency_keys", "resource_kind", "TEXT")?;
        Self::add_column_if_missing(conn, "idempotency_keys", "resource_id", "TEXT")?;
        Ok(())
    }

    fn add_column_if_missing(
        conn: &Connection,
        table: &str,
        column: &str,
        decl: &str,
    ) -> Result<(), DataError> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == column {
                return Ok(());
            }
        }
        drop(rows);
        drop(stmt);
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))?;
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
        self.insert_session_full(NewSessionRow {
            id,
            workdir,
            created_at,
            setup_status: SessionSetupStatus::Ready,
            kind: SessionKind::Local,
            cloned_path: None,
        })
    }

    /// Insert a session row.
    ///
    /// Used by the async-setup pipeline so that the row exists in
    /// `initializing` state before any setup work runs — so that the
    /// server-restart-cleanup pass can find non-terminal sessions even before
    /// any setup_state.json is written.
    pub fn insert_session_full(&self, row: NewSessionRow<'_>) -> Result<(), DataError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO sessions \
                 (id, workdir, created_at, status, setup_status, session_type, cloned_path) \
                 VALUES (?1, ?2, ?3, 'active', ?4, ?5, ?6)",
            params![
                row.id,
                row.workdir,
                row.created_at,
                row.setup_status.as_str(),
                row.kind.as_str(),
                row.cloned_path
            ],
        )?;
        Ok(())
    }

    /// Record a remote session's inputs on its row. Called right after
    /// [`Self::insert_session_full`] and before setup starts. Returns whether
    /// the row existed.
    pub fn set_remote_fields(
        &self,
        id: &str,
        fields: RemoteSessionFields<'_>,
    ) -> Result<bool, DataError> {
        let merge_refs = if fields.merge_refs.is_empty() {
            None
        } else {
            Some(
                serde_json::to_string(fields.merge_refs)
                    .map_err(|e| DataError::Other(format!("encode merge_refs: {e}")))?,
            )
        };
        let conn = self.lock();
        let affected = conn.execute(
            "UPDATE sessions SET repo_url = ?1, branch = ?2, base_ref = ?3, \
                                 merge_refs = ?4, push = ?5 \
             WHERE id = ?6",
            params![
                fields.repo_url,
                fields.branch,
                fields.base_ref,
                merge_refs,
                fields.push,
                id
            ],
        )?;
        Ok(affected > 0)
    }

    /// Update the setup_status for a session. Returns true if the row was
    /// updated (i.e. the session exists).
    pub fn update_setup_status(&self, id: &str, setup_status: &str) -> Result<bool, DataError> {
        let conn = self.lock();
        let affected = conn.execute(
            "UPDATE sessions SET setup_status = ?1 WHERE id = ?2",
            params![setup_status, id],
        )?;
        Ok(affected > 0)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionRecord>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, workdir, created_at, status, closed_at, \
                    setup_status, session_type, cloned_path, \
                    repo_url, branch, base_ref, merge_refs, push \
             FROM sessions WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], session_record_from_row)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Classify whether a session may accept a newly-submitted command, based
    /// on its lifecycle state. This is the state-transition guard that used to
    /// live inline in the API `POST /v1/commands` handler; it belongs on the
    /// store that owns the `status` column so every frontend enforces it
    /// identically. Frontends map the typed outcome to a transport status.
    pub fn command_admission(
        &self,
        session_id: &str,
    ) -> Result<SessionCommandAdmission, DataError> {
        Ok(match self.get_session(session_id)? {
            Some(s) if s.status == "active" => SessionCommandAdmission::Accepted,
            Some(s) if s.status == "closing" => SessionCommandAdmission::Closing,
            Some(_) => SessionCommandAdmission::Closed,
            None => SessionCommandAdmission::NotFound,
        })
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRecord>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, workdir, created_at, status, closed_at, \
                    setup_status, session_type, cloned_path, \
                    repo_url, branch, base_ref, merge_refs, push \
             FROM sessions ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], session_record_from_row)?;
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
            "SELECT id, workdir, created_at, status, closed_at, \
                    setup_status, session_type, cloned_path, \
                    repo_url, branch, base_ref, merge_refs, push \
             FROM sessions WHERE status = ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![status], session_record_from_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn set_labels(
        &self,
        owner: LabelOwner,
        owner_id: &str,
        labels: &Labels,
    ) -> Result<(), DataError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM labels WHERE owner_kind=?1 AND owner_id=?2",
            params![owner.as_str(), owner_id],
        )?;
        for (key, value) in labels.iter() {
            tx.execute(
                "INSERT INTO labels(owner_kind,owner_id,key,value) VALUES(?1,?2,?3,?4)",
                params![owner.as_str(), owner_id, key, value],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn labels_for(&self, owner: LabelOwner, owner_id: &str) -> Result<Labels, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT key,value FROM labels WHERE owner_kind=?1 AND owner_id=?2 ORDER BY key",
        )?;
        let rows = stmt.query_map(params![owner.as_str(), owner_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let map = rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        labels_from_map(map)
    }

    pub fn labels_for_many(
        &self,
        owner: LabelOwner,
        ids: &[String],
    ) -> Result<HashMap<String, Labels>, DataError> {
        let mut result = ids
            .iter()
            .cloned()
            .map(|id| (id, Labels::empty()))
            .collect::<HashMap<_, _>>();
        if ids.is_empty() {
            return Ok(result);
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT owner_id,key,value FROM labels WHERE owner_kind=? AND owner_id IN ({placeholders}) ORDER BY owner_id,key"
        );
        let mut values = vec![Value::from(owner.as_str().to_owned())];
        values.extend(ids.iter().cloned().map(Value::from));
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut maps: HashMap<String, BTreeMap<String, String>> = HashMap::new();
        for row in rows {
            let (id, key, value) = row?;
            maps.entry(id).or_default().insert(key, value);
        }
        for (id, map) in maps {
            result.insert(id, labels_from_map(map)?);
        }
        Ok(result)
    }

    pub fn list_sessions_filtered(
        &self,
        status: Option<&str>,
        labels: &[LabelSelector],
    ) -> Result<Vec<SessionRecord>, DataError> {
        let mut sql = String::from(
            "SELECT id,workdir,created_at,status,closed_at,setup_status,session_type,cloned_path,repo_url,branch,base_ref,merge_refs,push FROM sessions s WHERE 1=1",
        );
        let mut args: Vec<Value> = Vec::new();
        if let Some(status) = status {
            sql.push_str(" AND s.status=?");
            args.push(Value::from(status.to_owned()));
        }
        append_label_predicates(&mut sql, &mut args, "session", "s.id", labels);
        sql.push_str(" ORDER BY s.created_at");
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), session_record_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(DataError::from)
    }

    pub fn list_commands_filtered(
        &self,
        filter: &CommandListFilter<'_>,
    ) -> Result<Vec<CommandRecord>, DataError> {
        let mut sql = String::from(
            "SELECT c.id,c.session_id,c.subcommand,c.args,c.status,c.exit_code,c.started_at,c.finished_at,c.log_path,c.worker_id,c.queued_at,c.result,c.attempt FROM commands c WHERE 1=1",
        );
        let mut args: Vec<Value> = Vec::new();
        if let Some(session) = filter.session_id {
            sql.push_str(" AND c.session_id=?");
            args.push(Value::from(session.to_owned()));
        }
        if let Some(status) = filter.status {
            sql.push_str(" AND c.status=?");
            args.push(Value::from(status.to_owned()));
        }
        append_label_predicates(&mut sql, &mut args, "command", "c.id", filter.labels);
        sql.push_str(" ORDER BY c.queued_at DESC, c.rowid DESC LIMIT ?");
        args.push(Value::from(filter.limit));
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), command_record_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(DataError::from)
    }

    /// Return every session whose `setup_status` is one of the non-terminal
    /// values — `initializing`, `cloning_repository`, `setting_up_branch`, `merging_refs`,
    /// `running_ready`. Used at startup to mark them as `failed` (the setup
    /// task was killed by the restart).
    pub fn list_sessions_with_in_progress_setup(&self) -> Result<Vec<SessionRecord>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, workdir, created_at, status, closed_at, \
                    setup_status, session_type, cloned_path, \
                    repo_url, branch, base_ref, merge_refs, push \
             FROM sessions \
             WHERE setup_status IN ('initializing', 'cloning_repository', \
                                    'setting_up_branch', 'merging_refs', 'running_ready') \
             ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], session_record_from_row)?;
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
            conn.execute(
                "DELETE FROM labels WHERE owner_kind='session' AND owner_id=?1",
                params![sid],
            )?;
            conn.execute("DELETE FROM labels WHERE owner_kind='command' AND owner_id IN (SELECT id FROM commands WHERE session_id=?1)", params![sid])?;
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
                    started_at, finished_at, log_path, worker_id, queued_at, result, attempt
             FROM commands WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], command_record_from_row)?;
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

    /// Reserve `key` for `owner`, or report what an earlier request with the
    /// same key did. An unfinished reservation is never taken over while its
    /// daemon instance is alive; one left by an exited daemon is replayed when
    /// its recorded resource exists (the work was admitted) and otherwise
    /// handed to the new request, since nothing was created.
    pub fn idempotency_begin(
        &self,
        scope: IdempotencyScope<'_>,
        key: &str,
        request_hash: &str,
        now: &str,
        owner: IdempotencyOwner<'_>,
    ) -> Result<IdempotencyBegin, DataError> {
        validate_idempotency_key(key)?;
        let now_dt = parse_timestamp(now)?;
        let ttl_cutoff =
            (now_dt - chrono::Duration::hours(IDEMPOTENCY_TTL_HOURS as i64)).to_rfc3339();
        let scope = scope.key();
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM idempotency_keys WHERE created_at < ?1",
            params![ttl_cutoff],
        )?;
        // An admitted request's recorded response wins over any liveness rule.
        admitted_intent_to_response(&tx, &scope, key)?;
        type ExistingAdmission = (String, Option<u16>, Option<String>, Option<String>);
        let existing: Option<ExistingAdmission> = tx
            .query_row(
                "SELECT request_hash,status,body,owner_instance FROM idempotency_keys WHERE scope=?1 AND key=?2",
                params![scope, key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let outcome = match existing {
            None => {
                tx.execute(
                    "INSERT INTO idempotency_keys(scope,key,request_hash,status,body,created_at,owner_instance,owner_request)
                     VALUES(?1,?2,?3,NULL,NULL,?4,?5,?6)",
                    params![scope, key, request_hash, now, owner.instance, owner.request],
                )?;
                IdempotencyBegin::Fresh
            }
            Some((old_hash, Some(status), Some(body), _)) => {
                if old_hash == request_hash {
                    IdempotencyBegin::Replay { status, body }
                } else {
                    IdempotencyBegin::Mismatch
                }
            }
            Some((old_hash, _, _, instance)) if instance.as_deref() == Some(owner.instance) => {
                if old_hash == request_hash {
                    IdempotencyBegin::InFlight
                } else {
                    IdempotencyBegin::Mismatch
                }
            }
            Some(_) => {
                tx.execute(
                    "UPDATE idempotency_keys SET request_hash=?3,status=NULL,body=NULL,created_at=?4,
                        owner_instance=?5,owner_request=?6,intent_status=NULL,intent_body=NULL,
                        resource_kind=NULL,resource_id=NULL
                     WHERE scope=?1 AND key=?2",
                    params![scope, key, request_hash, now, owner.instance, owner.request],
                )?;
                IdempotencyBegin::Fresh
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Record, before the owner's first durable write, the response it will
    /// return and the resource whose existence means it was admitted. A
    /// restart or a dropped request then recovers the response from that
    /// resource instead of admitting the work twice. `Ok(false)` when `owner`
    /// no longer holds the reservation.
    pub fn idempotency_record_intent(
        &self,
        scope: IdempotencyScope<'_>,
        key: &str,
        owner: IdempotencyOwner<'_>,
        status: u16,
        body: &str,
        resource: IdempotencyResource<'_>,
    ) -> Result<bool, DataError> {
        validate_idempotency_key(key)?;
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE idempotency_keys SET intent_status=?5,intent_body=?6,resource_kind=?7,resource_id=?8
             WHERE scope=?1 AND key=?2 AND owner_instance=?3 AND owner_request=?4 AND status IS NULL",
            params![
                scope.key(),
                key,
                owner.instance,
                owner.request,
                status,
                body,
                resource.kind(),
                resource.id()
            ],
        )?;
        Ok(changed > 0)
    }

    /// Store the final response. Only the reservation's owner can complete
    /// it; `Ok(false)` when `owner` no longer holds an unfinished reservation.
    pub fn idempotency_complete(
        &self,
        scope: IdempotencyScope<'_>,
        key: &str,
        owner: IdempotencyOwner<'_>,
        status: u16,
        body: &str,
    ) -> Result<bool, DataError> {
        validate_idempotency_key(key)?;
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE idempotency_keys SET status=?5,body=?6
             WHERE scope=?1 AND key=?2 AND owner_instance=?3 AND owner_request=?4 AND status IS NULL",
            params![scope.key(), key, owner.instance, owner.request, status, body],
        )?;
        Ok(changed > 0)
    }

    /// Drop `owner`'s unfinished reservation after a request that admitted
    /// nothing (or rolled back what it admitted). Another owner's reservation
    /// is never touched.
    pub fn idempotency_release(
        &self,
        scope: IdempotencyScope<'_>,
        key: &str,
        owner: IdempotencyOwner<'_>,
    ) -> Result<(), DataError> {
        validate_idempotency_key(key)?;
        let conn = self.lock();
        conn.execute(
            "DELETE FROM idempotency_keys
             WHERE scope=?1 AND key=?2 AND owner_instance=?3 AND owner_request=?4 AND status IS NULL",
            params![scope.key(), key, owner.instance, owner.request],
        )?;
        Ok(())
    }

    /// Like [`Self::idempotency_release`], for a request that ended without
    /// storing its response (a panic, a dropped future, a failed store):
    /// when its recorded resource exists the work was admitted, so the
    /// recorded response is stored instead of releasing the key.
    pub fn idempotency_release_unless_admitted(
        &self,
        scope: IdempotencyScope<'_>,
        key: &str,
        owner: IdempotencyOwner<'_>,
    ) -> Result<(), DataError> {
        validate_idempotency_key(key)?;
        let scope = scope.key();
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let owned = tx
            .query_row(
                "SELECT 1 FROM idempotency_keys
                 WHERE scope=?1 AND key=?2 AND owner_instance=?3 AND owner_request=?4 AND status IS NULL",
                params![scope, key, owner.instance, owner.request],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if owned && !admitted_intent_to_response(&tx, &scope, key)? {
            tx.execute(
                "DELETE FROM idempotency_keys WHERE scope=?1 AND key=?2 AND status IS NULL",
                params![scope, key],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn prune_idempotency_keys(&self, ttl_hours: u64, now: &str) -> Result<usize, DataError> {
        let cutoff = (parse_timestamp(now)?
            - chrono::Duration::hours(ttl_hours.min(i64::MAX as u64) as i64))
        .to_rfc3339();
        let conn = self.lock();
        Ok(conn.execute(
            "DELETE FROM idempotency_keys WHERE created_at < ?1",
            params![cutoff],
        )?)
    }

    // ─── Queue operations ─────────────────────────────────────────────────

    pub fn enqueue_command(
        &self,
        id: &str,
        session_id: &str,
        subcommand: &str,
        args: &str,
        log_path: &str,
    ) -> Result<(), DataError> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO commands (id, session_id, subcommand, args, status, log_path, queued_at)
             VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?6)",
            params![id, session_id, subcommand, args, log_path, now],
        )?;
        Ok(())
    }

    /// [`Self::enqueue_command`] and its labels in one transaction, so a
    /// claimable row never exists without its labels (and the row's
    /// existence alone proves the submission was admitted).
    pub fn enqueue_command_with_labels(
        &self,
        id: &str,
        session_id: &str,
        subcommand: &str,
        args: &str,
        log_path: &str,
        labels: &Labels,
    ) -> Result<(), DataError> {
        let now = chrono::Utc::now().to_rfc3339();
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM labels WHERE owner_kind=?1 AND owner_id=?2",
            params![LabelOwner::Command.as_str(), id],
        )?;
        for (key, value) in labels.iter() {
            tx.execute(
                "INSERT INTO labels(owner_kind,owner_id,key,value) VALUES(?1,?2,?3,?4)",
                params![LabelOwner::Command.as_str(), id, key, value],
            )?;
        }
        tx.execute(
            "INSERT INTO commands (id, session_id, subcommand, args, status, log_path, queued_at)
             VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?6)",
            params![id, session_id, subcommand, args, log_path, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn claim_next_command(&self, worker_id: &str) -> Result<Option<CommandRecord>, DataError> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.lock();
        // Atomically claim the next eligible command: must be queued (or legacy pending)
        // and no other command in the same session can be running.
        let mut stmt = conn.prepare(
            "UPDATE commands
             SET status = 'running', worker_id = ?1, started_at = ?2
             WHERE id = (
                 SELECT c.id FROM commands c
                 WHERE c.status IN ('queued', 'pending')
                   AND EXISTS (SELECT 1 FROM sessions s WHERE s.id=c.session_id AND s.setup_status='ready')
                   AND NOT EXISTS (
                       SELECT 1 FROM commands r
                       WHERE r.session_id = c.session_id
                         AND r.status = 'running'
                   )
                 ORDER BY c.queued_at ASC, c.rowid ASC
                 LIMIT 1
             )
             RETURNING id, session_id, subcommand, args, status, exit_code,
                       started_at, finished_at, log_path, worker_id, queued_at, result, attempt",
        )?;
        let mut rows = stmt.query_map(params![worker_id, now], command_record_from_row)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn complete_command(
        &self,
        id: &str,
        status: &str,
        exit_code: Option<i32>,
        result_json: Option<&str>,
    ) -> Result<(), DataError> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.lock();
        conn.execute(
            "UPDATE commands SET status = ?1, exit_code = ?2, finished_at = ?3, result = ?4
             WHERE id = ?5",
            params![status, exit_code, now, result_json, id],
        )?;
        Ok(())
    }

    pub fn list_commands_for_session(
        &self,
        session_id: &str,
        limit: i64,
    ) -> Result<Vec<CommandRecord>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, subcommand, args, status, exit_code,
                    started_at, finished_at, log_path, worker_id, queued_at, result, attempt
             FROM commands
             WHERE session_id = ?1
             ORDER BY queued_at DESC, rowid DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session_id, limit], command_record_from_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn count_queued_for_session(&self, session_id: &str) -> Result<i64, DataError> {
        let conn = self.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM commands WHERE session_id = ?1 AND status = 'queued'",
            params![session_id],
            |r| r.get(0),
        )?;
        Ok(count)
    }

    pub fn running_command_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<CommandRecord>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, subcommand, args, status, exit_code,
                    started_at, finished_at, log_path, worker_id, queued_at, result, attempt
             FROM commands
             WHERE session_id = ?1 AND status = 'running'
             LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![session_id], command_record_from_row)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn cancel_queued_for_session(&self, session_id: &str) -> Result<Vec<String>, DataError> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.lock();
        // First get the IDs
        let ids: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT id FROM commands WHERE session_id = ?1 AND status = 'queued'")?;
            let rows = stmt
                .query_map(params![session_id], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        // Then update
        conn.execute(
            "UPDATE commands SET status = 'cancelled', finished_at = ?1
             WHERE session_id = ?2 AND status = 'queued'",
            params![now, session_id],
        )?;
        Ok(ids)
    }

    pub fn recover_stale_commands(&self, timeout_secs: u64) -> Result<Vec<String>, DataError> {
        let cutoff = chrono::Utc::now() - chrono::Duration::seconds(timeout_secs as i64);
        let cutoff_str = cutoff.to_rfc3339();
        let conn = self.lock();
        let ids: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT id FROM commands
                 WHERE status = 'running' AND started_at IS NOT NULL AND started_at < ?1",
            )?;
            let rows = stmt
                .query_map(params![cutoff_str], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        conn.execute(
            "UPDATE commands SET status = 'queued', worker_id = NULL, started_at = NULL,
                 attempt = attempt + 1
             WHERE status = 'running' AND started_at IS NOT NULL AND started_at < ?1",
            params![cutoff_str],
        )?;
        Ok(ids)
    }

    pub fn queue_position_for_command(
        &self,
        command_id: &str,
        session_id: &str,
    ) -> Result<Option<i64>, DataError> {
        let conn = self.lock();
        // Get the queued_at for this command
        let queued_at: Option<String> = conn
            .query_row(
                "SELECT queued_at FROM commands WHERE id = ?1 AND status = 'queued'",
                params![command_id],
                |r| r.get(0),
            )
            .ok();
        let Some(queued_at) = queued_at else {
            return Ok(None);
        };
        let position: i64 = conn.query_row(
            "SELECT COUNT(*) FROM commands
             WHERE session_id = ?1 AND status = 'queued'
               AND (queued_at < ?2 OR (queued_at = ?2 AND rowid < (SELECT rowid FROM commands WHERE id = ?3)))",
            params![session_id, queued_at, command_id],
            |r| r.get(0),
        )?;
        Ok(Some(position))
    }

    /// Update a session's status to an arbitrary value (e.g. 'closing').
    pub fn update_session_status(&self, id: &str, status: &str) -> Result<bool, DataError> {
        let conn = self.lock();
        let affected = conn.execute(
            "UPDATE sessions SET status = ?1 WHERE id = ?2",
            params![status, id],
        )?;
        Ok(affected > 0)
    }

    /// Close a session: set status to 'closed' and closed_at timestamp.
    /// Unlike close_session which only works on 'active' sessions, this works
    /// from any non-closed status.
    pub fn close_session_force(&self, id: &str, closed_at: &str) -> Result<bool, DataError> {
        let conn = self.lock();
        let affected = conn.execute(
            "UPDATE sessions SET status = 'closed', closed_at = ?1
             WHERE id = ?2 AND status != 'closed'",
            params![closed_at, id],
        )?;
        Ok(affected > 0)
    }
}

fn session_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    Ok(SessionRecord {
        id: row.get(0)?,
        workdir: row.get(1)?,
        created_at: row.get(2)?,
        status: row.get(3)?,
        closed_at: row.get(4)?,
        setup_status: row.get(5)?,
        session_type: row.get(6)?,
        cloned_path: row.get(7)?,
        repo_url: row.get(8)?,
        branch: row.get(9)?,
        base_ref: row.get(10)?,
        merge_refs: row
            .get::<_, Option<String>>(11)?
            .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
            .unwrap_or_default(),
        push: row.get(12)?,
    })
}

fn command_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CommandRecord> {
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
        worker_id: row.get(9)?,
        queued_at: row.get(10)?,
        result: row.get(11)?,
        attempt: u32::try_from(row.get::<_, i64>(12)?).unwrap_or(FIRST_ATTEMPT),
    })
}

fn labels_from_map(map: BTreeMap<String, String>) -> Result<Labels, DataError> {
    Labels::new(map).map_err(|e| DataError::Other(format!("invalid labels in API database: {e}")))
}

fn append_label_predicates(
    sql: &mut String,
    args: &mut Vec<Value>,
    owner: &str,
    owner_expr: &str,
    selectors: &[LabelSelector],
) {
    for selector in selectors {
        sql.push_str(" AND EXISTS (SELECT 1 FROM labels l WHERE l.owner_kind=? AND l.owner_id=");
        sql.push_str(owner_expr);
        sql.push_str(" AND l.key=?");
        args.push(Value::from(owner.to_owned()));
        match selector {
            LabelSelector::Equals { key, value } => {
                sql.push_str(" AND l.value=?");
                args.push(Value::from(key.clone()));
                args.push(Value::from(value.clone()));
            }
            LabelSelector::Exists { key } => args.push(Value::from(key.clone())),
        }
        sql.push(')');
    }
}

/// If the unfinished reservation `(scope, key)` recorded an intent whose
/// resource exists, store that intent as its response. Returns whether it did.
fn admitted_intent_to_response(
    conn: &Connection,
    scope: &str,
    key: &str,
) -> Result<bool, DataError> {
    let intent: Option<(String, String)> = conn
        .query_row(
            "SELECT resource_kind,resource_id FROM idempotency_keys
             WHERE scope=?1 AND key=?2 AND status IS NULL AND intent_status IS NOT NULL",
            params![scope, key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((kind, id)) = intent else {
        return Ok(false);
    };
    let sql = match kind.as_str() {
        "session" => "SELECT 1 FROM sessions WHERE id=?1",
        "command" => "SELECT 1 FROM commands WHERE id=?1",
        _ => return Ok(false),
    };
    let exists = conn
        .query_row(sql, params![id], |_| Ok(()))
        .optional()?
        .is_some();
    if exists {
        conn.execute(
            "UPDATE idempotency_keys SET status=intent_status,body=intent_body
             WHERE scope=?1 AND key=?2 AND status IS NULL",
            params![scope, key],
        )?;
    }
    Ok(exists)
}

fn validate_idempotency_key(key: &str) -> Result<(), DataError> {
    if key.is_empty() || key.len() > IDEMPOTENCY_KEY_MAX_BYTES || key.chars().any(char::is_control)
    {
        return Err(DataError::Other(format!(
            "invalid Idempotency-Key (must be 1..={IDEMPOTENCY_KEY_MAX_BYTES} bytes without control characters)"
        )));
    }
    Ok(())
}

fn parse_timestamp(value: &str) -> Result<chrono::DateTime<chrono::Utc>, DataError> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|v| v.with_timezone(&chrono::Utc))
        .map_err(|e| DataError::Other(format!("invalid RFC3339 timestamp '{value}': {e}")))
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
    fn labels_round_trip_and_filter_sessions_and_commands() {
        let (_tmp, store) = make_store();
        store
            .insert_session("s-label", "/work", "2026-01-01T00:00:00Z")
            .unwrap();
        store
            .insert_command("c-label", "s-label", "chat", "[]", "/tmp/log")
            .unwrap();
        store
            .insert_session("s-partial", "/work-2", "2026-01-02T00:00:00Z")
            .unwrap();
        store
            .insert_command("c-partial", "s-partial", "chat", "[]", "/tmp/log-2")
            .unwrap();
        let labels = Labels::new(BTreeMap::from([
            ("env".to_owned(), "prod".to_owned()),
            ("tier".to_owned(), "api".to_owned()),
        ]))
        .unwrap();
        store
            .set_labels(LabelOwner::Session, "s-label", &labels)
            .unwrap();
        store
            .set_labels(LabelOwner::Command, "c-label", &labels)
            .unwrap();
        let partial = Labels::new(BTreeMap::from([("env".to_owned(), "prod".to_owned())])).unwrap();
        store
            .set_labels(LabelOwner::Session, "s-partial", &partial)
            .unwrap();
        store
            .set_labels(LabelOwner::Command, "c-partial", &partial)
            .unwrap();
        assert_eq!(
            store.labels_for(LabelOwner::Session, "s-label").unwrap(),
            labels
        );
        assert_eq!(
            store
                .labels_for_many(LabelOwner::Command, &["c-label".into(), "missing".into()])
                .unwrap()["c-label"],
            labels
        );
        let selectors = [
            LabelSelector::parse("env=prod").unwrap(),
            LabelSelector::parse("tier").unwrap(),
        ];
        assert_eq!(
            store
                .list_sessions_filtered(Some("active"), &selectors)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .list_sessions_filtered(Some("active"), &[selectors[0].clone()])
                .unwrap()
                .len(),
            2,
            "one selector matches both rows"
        );
        assert_eq!(
            store
                .list_commands_filtered(&CommandListFilter {
                    session_id: Some("s-label"),
                    status: Some("pending"),
                    labels: &selectors,
                    limit: 10
                })
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .list_commands_filtered(&CommandListFilter {
                    session_id: None,
                    status: Some("pending"),
                    labels: &[selectors[0].clone()],
                    limit: 10
                })
                .unwrap()
                .len(),
            2,
            "one selector matches both commands"
        );
    }

    #[test]
    fn idempotency_replays_until_24_hour_expiry_and_is_scope_isolated() {
        let (_tmp, store) = make_store();
        let commands = IdempotencyScope {
            route: "/v1/commands",
            principal: "key-a",
        };
        let other_principal = IdempotencyScope {
            route: "/v1/commands",
            principal: "key-b",
        };
        let other_route = IdempotencyScope {
            route: "/v1/runs",
            principal: "key-a",
        };
        let owner = IdempotencyOwner {
            instance: "daemon",
            request: "request-1",
        };
        let retry = IdempotencyOwner {
            instance: "daemon",
            request: "request-2",
        };
        let t0 = "2026-01-01T00:00:00Z";

        assert_eq!(
            store
                .idempotency_begin(commands, "key", "body", t0, owner)
                .unwrap(),
            IdempotencyBegin::Fresh
        );
        assert!(store
            .idempotency_complete(commands, "key", owner, 202, r#"{"command_id":"c1"}"#)
            .unwrap());
        assert_eq!(
            store
                .idempotency_begin(commands, "key", "body", "2026-01-01T23:59:59Z", retry)
                .unwrap(),
            IdempotencyBegin::Replay {
                status: 202,
                body: r#"{"command_id":"c1"}"#.into()
            }
        );

        // The deterministic timestamp is the injected clock: just beyond
        // +24h the completed reservation is pruned and the key can be admitted.
        assert_eq!(
            store
                .idempotency_begin(commands, "key", "body", "2026-01-02T00:00:01Z", retry)
                .unwrap(),
            IdempotencyBegin::Fresh
        );
        assert_eq!(
            store
                .idempotency_begin(other_principal, "key", "body", t0, owner)
                .unwrap(),
            IdempotencyBegin::Fresh
        );
        assert_eq!(
            store
                .idempotency_begin(other_route, "key", "body", t0, owner)
                .unwrap(),
            IdempotencyBegin::Fresh
        );
    }

    #[test]
    fn idempotency_replays_mismatches_expires_and_releases() {
        let (_tmp, store) = make_store();
        let scope = IdempotencyScope {
            route: "/v1/commands",
            principal: "local",
        };
        let a = IdempotencyOwner {
            instance: "daemon-1",
            request: "a",
        };
        let b = IdempotencyOwner {
            instance: "daemon-1",
            request: "b",
        };
        let t0 = "2026-01-01T00:00:00Z";
        assert_eq!(
            store
                .idempotency_begin(scope, "key", "hash", t0, a)
                .unwrap(),
            IdempotencyBegin::Fresh
        );
        assert_eq!(
            store
                .idempotency_begin(scope, "key", "hash", t0, b)
                .unwrap(),
            IdempotencyBegin::InFlight
        );
        assert_eq!(
            store
                .idempotency_begin(scope, "key", "other", t0, b)
                .unwrap(),
            IdempotencyBegin::Mismatch
        );
        // A live owner is never displaced, however long it takes.
        assert_eq!(
            store
                .idempotency_begin(scope, "key", "hash", "2026-01-01T00:05:00Z", b)
                .unwrap(),
            IdempotencyBegin::InFlight
        );
        assert!(!store
            .idempotency_complete(scope, "key", b, 202, "{}")
            .unwrap());
        assert!(store
            .idempotency_complete(scope, "key", a, 202, r#"{"ok":true}"#)
            .unwrap());
        assert_eq!(
            store
                .idempotency_begin(scope, "key", "hash", t0, b)
                .unwrap(),
            IdempotencyBegin::Replay {
                status: 202,
                body: r#"{"ok":true}"#.into()
            }
        );
        // A completed key stays bound to its body for the whole TTL.
        assert_eq!(
            store
                .idempotency_begin(scope, "key", "new", "2026-01-01T00:01:01Z", b)
                .unwrap(),
            IdempotencyBegin::Mismatch
        );
        // Release is owner-fenced and leaves completed responses alone.
        store.idempotency_release(scope, "key", a).unwrap();
        assert!(matches!(
            store
                .idempotency_begin(scope, "key", "hash", t0, b)
                .unwrap(),
            IdempotencyBegin::Replay { .. }
        ));
        assert_eq!(
            store
                .idempotency_begin(scope, "other", "x", "2026-01-01T00:02:00Z", a)
                .unwrap(),
            IdempotencyBegin::Fresh
        );
        store.idempotency_release(scope, "other", b).unwrap();
        assert_eq!(
            store
                .idempotency_begin(scope, "other", "x", "2026-01-01T00:02:00Z", b)
                .unwrap(),
            IdempotencyBegin::InFlight
        );
        store.idempotency_release(scope, "other", a).unwrap();
        assert_eq!(
            store
                .idempotency_begin(scope, "other", "y", "2026-01-01T00:02:00Z", b)
                .unwrap(),
            IdempotencyBegin::Fresh
        );
        // A reservation left by an exited daemon that admitted nothing is
        // handed over; its old owner can then no longer complete it.
        let restarted = IdempotencyOwner {
            instance: "daemon-2",
            request: "c",
        };
        assert_eq!(
            store
                .idempotency_begin(scope, "other", "z", "2026-01-01T00:02:30Z", restarted)
                .unwrap(),
            IdempotencyBegin::Fresh
        );
        assert!(!store
            .idempotency_complete(scope, "other", b, 202, "{}")
            .unwrap());
        assert_eq!(
            store
                .prune_idempotency_keys(24, "2026-01-03T00:02:00Z")
                .unwrap(),
            2
        );
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

    // ─── WI 0136: attempt column ──────────────────────────────────────────────

    fn insert_running(store: &SqliteSessionStore, id: &str) {
        store
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO commands (id, session_id, subcommand, args, status, log_path,
                             started_at, worker_id, queued_at)
                     VALUES (?1, 's1', 'exec prompt', '[]', 'running', '/logs/x',
                             '2020-01-01T00:00:00Z', 'dead-worker', '2020-01-01T00:00:00Z')",
                    params![id],
                )
                .map_err(DataError::from)
            })
            .unwrap();
    }

    #[test]
    fn new_commands_start_at_attempt_one() {
        let (_tmp, store) = make_store();
        store
            .insert_session("s1", "/w", "2026-01-01T00:00:00Z")
            .unwrap();
        store
            .enqueue_command("c1", "s1", "chat", "[]", "/l")
            .unwrap();
        assert_eq!(store.get_command("c1").unwrap().unwrap().attempt, 1);
        let claimed = store.claim_next_command("w").unwrap().unwrap();
        assert_eq!(claimed.attempt, 1);
    }

    #[test]
    fn recover_stale_commands_increments_attempt_each_time() {
        let (_tmp, store) = make_store();
        store
            .insert_session("s1", "/w", "2026-01-01T00:00:00Z")
            .unwrap();
        insert_running(&store, "crash");
        assert_eq!(store.recover_stale_commands(1).unwrap(), ["crash"]);
        let row = store.get_command("crash").unwrap().unwrap();
        assert_eq!((row.status.as_str(), row.attempt), ("queued", 2));

        // The re-queued claim reports the incremented attempt.
        let claimed = store.claim_next_command("w2").unwrap().unwrap();
        assert_eq!((claimed.id.as_str(), claimed.attempt), ("crash", 2));

        // A second crash while running attempt 2 moves it to attempt 3.
        store
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE commands SET started_at = '2020-01-01T00:00:00Z' WHERE id = 'crash'",
                    [],
                )
                .map_err(DataError::from)
            })
            .unwrap();
        assert_eq!(store.recover_stale_commands(1).unwrap(), ["crash"]);
        assert_eq!(store.get_command("crash").unwrap().unwrap().attempt, 3);
    }

    #[test]
    fn recover_stale_commands_leaves_other_rows_at_their_attempt() {
        let (_tmp, store) = make_store();
        store
            .insert_session("s1", "/w", "2026-01-01T00:00:00Z")
            .unwrap();
        store
            .enqueue_command("queued", "s1", "chat", "[]", "/l")
            .unwrap();
        assert!(store.recover_stale_commands(0).unwrap().is_empty());
        assert_eq!(store.get_command("queued").unwrap().unwrap().attempt, 1);
    }

    #[test]
    fn migration_backfills_attempt_one_on_pre_existing_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("awman.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY, workdir TEXT NOT NULL, created_at TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'active', closed_at TEXT
                );
                CREATE TABLE commands (
                    id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
                    subcommand TEXT NOT NULL, args TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'pending', exit_code INTEGER,
                    started_at TEXT, finished_at TEXT, log_path TEXT NOT NULL
                );
                INSERT INTO sessions (id, workdir, created_at) VALUES ('s', '/w', '2023-01-01');
                INSERT INTO commands (id, session_id, subcommand, args, status, log_path)
                    VALUES ('old-1', 's', 'chat', '[]', 'running', '/l1'),
                           ('old-2', 's', 'chat', '[]', 'done', '/l2');",
            )
            .unwrap();
        }
        let store = SqliteSessionStore::open(tmp.path()).unwrap();
        for id in ["old-1", "old-2"] {
            assert_eq!(store.get_command(id).unwrap().unwrap().attempt, 1, "{id}");
        }
        // Opening again is idempotent and keeps later increments.
        store
            .with_conn(|conn| {
                conn.execute("UPDATE commands SET attempt = 4 WHERE id = 'old-1'", [])
                    .map_err(DataError::from)
            })
            .unwrap();
        drop(store);
        let store = SqliteSessionStore::open(tmp.path()).unwrap();
        assert_eq!(store.get_command("old-1").unwrap().unwrap().attempt, 4);
        assert_eq!(store.get_command("old-2").unwrap().unwrap().attempt, 1);
    }

    // ─── Schema compatibility with legacy DB ──────────────────────────────────
    //
    // Creates a DB using the exact SQL from oldsrc/commands/headless/db.rs,
    // inserts data, then opens it with SqliteSessionStore to verify that the
    // new store can read existing on-disk databases (user-upgrade path).

    #[test]
    fn legacy_schema_db_is_readable_by_new_store() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("awman.db");

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

            // Insert rows as the old awman code would have.
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

        // WI 0135 lifecycle tables are additive too and can be opened on the
        // same database created by the prior schema.
        let lifecycle = crate::data::api_lifecycle_store::CommandLifecycleStore::open(
            std::sync::Arc::new(SqliteSessionStore::open(tmp.path()).unwrap()),
        )
        .unwrap();
        assert!(lifecycle
            .cancel_if_queued("cmd-1", "2026-10-28T00:00:00Z")
            .unwrap());

        // Step 3: verify the legacy rows are readable.
        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "legacy-id-1");
        assert_eq!(sessions[0].workdir, "/old/repo");
        assert_eq!(sessions[0].status, "active");
        // WI 0133: a pre-existing row restores with no remote fields.
        assert_eq!(sessions[0].repo_url, None);
        assert_eq!(sessions[0].branch, None);
        assert_eq!(sessions[0].base_ref, None);
        assert!(sessions[0].merge_refs.is_empty());
        assert!(!sessions[0].push);

        let cmd = store.get_command("cmd-1").unwrap().unwrap();
        assert_eq!(cmd.session_id, "legacy-id-1");
        assert_eq!(cmd.subcommand, "chat");
        assert_eq!(cmd.status, "cancelled");
        assert_eq!(
            cmd.exit_code,
            Some(crate::data::command_status::CANCELLED_EXIT_CODE)
        );
    }

    // ─── WI 0133: remote session fields ───────────────────────────────────────

    fn insert_remote(store: &SqliteSessionStore, id: &str) {
        store
            .insert_session_full(NewSessionRow {
                id,
                workdir: "/clones/repo",
                created_at: "2026-10-27T00:00:00Z",
                setup_status: SessionSetupStatus::Initializing,
                kind: SessionKind::Remote,
                cloned_path: Some("/clones/repo"),
            })
            .unwrap();
    }

    #[test]
    fn remote_fields_round_trip() {
        let (_tmp, store) = make_store();
        insert_remote(&store, "r1");
        let merge_refs = vec!["feat-a".to_string(), "feat-b".to_string()];
        assert!(store
            .set_remote_fields(
                "r1",
                RemoteSessionFields {
                    repo_url: Some("https://host.invalid/o/r.git"),
                    branch: Some("job-1"),
                    base_ref: Some("v1.0"),
                    merge_refs: &merge_refs,
                    push: true,
                },
            )
            .unwrap());
        let rec = store.get_session("r1").unwrap().unwrap();
        assert_eq!(
            rec.repo_url.as_deref(),
            Some("https://host.invalid/o/r.git")
        );
        assert_eq!(rec.branch.as_deref(), Some("job-1"));
        assert_eq!(rec.base_ref.as_deref(), Some("v1.0"));
        assert_eq!(rec.merge_refs, merge_refs);
        assert!(rec.push);
        // Every SELECT reads the same columns.
        assert_eq!(store.list_sessions().unwrap()[0], rec);
        assert_eq!(
            store.list_sessions_by_status(Some("active")).unwrap()[0],
            rec
        );
        assert_eq!(
            store.list_sessions_with_in_progress_setup().unwrap()[0],
            rec
        );
    }

    #[test]
    fn remote_fields_default_when_unset_and_missing_row_reports_false() {
        let (_tmp, store) = make_store();
        insert_remote(&store, "r2");
        let rec = store.get_session("r2").unwrap().unwrap();
        assert_eq!(rec.repo_url, None);
        assert!(rec.merge_refs.is_empty());
        assert!(!rec.push);
        let fields = RemoteSessionFields {
            repo_url: None,
            branch: None,
            base_ref: None,
            merge_refs: &[],
            push: false,
        };
        assert!(!store.set_remote_fields("nope", fields).unwrap());
    }

    #[test]
    fn merging_refs_is_an_in_progress_setup_status() {
        let (_tmp, store) = make_store();
        insert_remote(&store, "r3");
        store
            .update_setup_status("r3", SessionSetupStatus::MergingRefs.as_str())
            .unwrap();
        let ids: Vec<_> = store
            .list_sessions_with_in_progress_setup()
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec!["r3".to_string()]);
    }

    #[test]
    fn command_result_serialises_pushed_as_absent_null_or_value() {
        let base = CommandResult {
            worktree_path: None,
            exit_code: Some(0),
            error: None,
            error_status: None,
            stage: None,
            pushed: None,
        };
        assert_eq!(serde_json::to_string(&base).unwrap(), r#"{"exit_code":0}"#);
        let null = CommandResult {
            pushed: Some(None),
            ..base.clone()
        };
        let json = serde_json::to_string(&null).unwrap();
        assert_eq!(json, r#"{"exit_code":0,"pushed":null}"#);
        assert_eq!(serde_json::from_str::<CommandResult>(&json).unwrap(), null);
        let pushed = CommandResult {
            pushed: Some(Some(PushedRef {
                remote: "origin".into(),
                branch: "b".into(),
                sha: "abc".into(),
            })),
            ..base.clone()
        };
        let json = serde_json::to_string(&pushed).unwrap();
        assert_eq!(
            serde_json::from_str::<CommandResult>(&json).unwrap(),
            pushed
        );
        assert_eq!(
            serde_json::from_str::<CommandResult>(r#"{"exit_code":0}"#).unwrap(),
            base
        );
    }
}
