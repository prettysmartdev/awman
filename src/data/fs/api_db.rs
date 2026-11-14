//! Sqlite-backed persistence for API-mode session and command metadata.
//!
//! The established schema is preserved so that existing on-disk databases
//! written by prior awman releases can be opened without losing state.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{
    params, params_from_iter,
    types::{Type, Value},
    Connection, OptionalExtension,
};

use crate::data::command_status::{CommandCompletion, CommandStatus};
use crate::data::error::DataError;
use crate::data::execution_event::FIRST_ATTEMPT;
use crate::data::fs::api_labels::{LabelSelector, Labels};
use crate::data::fs::api_paths::ApiPaths;
use crate::data::session::SessionKind;
use crate::data::session_setup_event::SessionSetupStatus;
use crate::data::session_status::SessionStatus;

/// Persistable session metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub context_key: Option<String>,
    pub id: String,
    pub workdir: String,
    pub created_at: String,
    pub status: SessionStatus,
    pub closed_at: Option<String>,
    /// Async setup status. Set on session creation; updated by the setup task;
    /// consulted by the job-submission guard and the server-restart cleanup.
    pub setup_status: SessionSetupStatus,
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
    /// WI 0145: the kind (`token`/`basic`) of a caller-supplied session Git
    /// credential. A marker only: the credential itself is never persisted,
    /// so a row with a marker and no in-memory credential is `credentials_lost`.
    pub git_auth_kind: Option<String>,
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
    /// WI 0145: the kind marker of the session credential, never its value.
    pub git_auth_kind: Option<&'a str>,
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
    pub listener_name: Option<String>,
    pub allow_remote_input: bool,
    pub listener: String,
    pub trace_parent: Option<String>,
    pub id: String,
    pub session_id: String,
    pub subcommand: String,
    pub args: String,
    /// Never [`CommandStatus::Pending`]: the store reads that legacy spelling
    /// as `Queued`.
    pub status: CommandStatus,
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

/// Rows used to build the session queue read model. Query order is the store's
/// established newest-first order; Layer 2 assigns the public positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionQueueRecords {
    pub queue_depth: i64,
    pub running: Option<CommandRecord>,
    pub recent: Vec<CommandRecord>,
}

impl CommandRecord {
    /// Decode the stored `args` column, the queue boundary's one decoder.
    pub fn decode_args(&self) -> Result<Vec<String>, DataError> {
        serde_json::from_str(&self.args).map_err(|e| {
            DataError::Other(format!(
                "command {}: stored args are not a JSON string array: {e}",
                self.id
            ))
        })
    }
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
    /// `Pending` and `Queued` select the same rows.
    pub status: Option<CommandStatus>,
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
///
/// The single description of how a command ended (WI 0137 §1): the status
/// route returns it, the one final `command_status` event carries it, and a
/// WSP `result` embeds it unchanged. Every field added by 0137 is optional and
/// filled only where it is known; the pre-0137 fields keep their meaning.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommandResult {
    pub exit_code: Option<i32>,
    /// How the command ended, refining the row's coarse `done | error |
    /// cancelled` status (an `error` row has outcome `error`, `aborted` or
    /// `paused`). Same enum as `command_status.status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<crate::data::execution_event::CommandStatusKind>,
    /// Why the command did not end `done`. Absent when `outcome` is `done`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<CommandFailure>,
    /// Every failed main, setup or teardown step, each listed once with its
    /// last attempt. Absent when no step failed or the command ran no steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_steps: Option<Vec<FailedStep>>,
    /// What the command did to the git checkout it ran in. Absent for
    /// commands without a checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitResult>,
    /// Checkout retained after dispatch, for deliberate resume or cleanup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    /// The pull request a `create_pull_request` teardown step opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<PullRequestRef>,
    /// `exec prompt` only: the agent's final message, from its structured
    /// output stream. Absent for text-only agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_message: Option<String>,
    /// Summary of the command's `/awman/output` directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<OutputsSummary>,
    /// Commands with a `--json` form (`agents status|list|detail|kill`, …)
    /// put their JSON value here as well as in a `StdoutLine`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// Kept: equals `failure.message` when `failure` is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Kept, deprecated: use `failure.code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_status: Option<u16>,
    /// Kept, deprecated: use `failure.stage`. Filled exactly as before WI
    /// 0137 (additive versioning): `"workflow_prerequisites"` for an
    /// unresolved prerequisite, `"push"` for a push-back failure, `"setup"`
    /// for a `/v1/worker/runs` setup failure, and absent otherwise (and on cancel).
    /// Where it is present it equals `failure.stage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// Kept, deprecated: a `/v1/worker/runs` setup failure's own `{stage, message}`
    /// (WI 0135). Setup failures also carry `failure.stage = "setup"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_error: Option<serde_json::Value>,
    /// Kept: when the cancel request was accepted (WI 0135).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_at: Option<String>,
    /// Present (possibly `null`) on every result of a session with
    /// `push: true`; absent when the session does not push.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub pushed: Option<Option<PushedRef>>,
}

/// Why a command did not end `done` (WI 0137 §1). Replaces the mix of an
/// `error` string that is sometimes a code and sometimes a message, an HTTP
/// status stored in an asynchronous result, and a `stage` set for only two
/// failures.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommandFailure {
    /// Post-admission stage: `setup | preflight | workflow_prerequisites |
    /// step | teardown | merge | push`. From `CommandError::stage()` for a
    /// typed error; the queue worker names the stage for engine outcomes,
    /// push failures and cancellation.
    pub stage: String,
    /// snake_case code: `CommandError::code()` for a typed error, or one of
    /// the [`failure_codes`] the queue worker assigns.
    pub code: String,
    /// Human-readable reason; equals the legacy `error` field.
    pub message: String,
    /// The step to blame, when a single one is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
}

/// `CommandFailure::stage` values (the stages listed on [`CommandFailure::stage`]
/// plus WI 0143's lease Git authentication stage).
pub mod failure_stages {
    pub const SETUP: &str = "setup";
    pub const PREFLIGHT: &str = "preflight";
    pub const STEP: &str = "step";
    pub const TEARDOWN: &str = "teardown";
    pub const MERGE: &str = "merge";
    pub const PUSH: &str = "push";
    /// Host Git authentication (clone, fetch, ls-remote or push).
    pub const GIT_AUTH: &str = "git_auth";
}

/// `CommandFailure::code` values that no `CommandError` variant produces:
/// they describe engine outcomes and post-command stages, and the queue
/// worker assigns them.
pub mod failure_codes {
    /// A host Git operation was refused for its credentials (WI 0143).
    /// Stage [`super::failure_stages::GIT_AUTH`]. Never retried.
    pub const GIT_AUTH_FAILED: &str = "git_auth_failed";
    /// A lease push got no response from the remote within the lease window
    /// (bounded retries exhausted). Stage [`super::failure_stages::PUSH`].
    pub const PUSH_UNREACHABLE: &str = "push_unreachable";
    /// The session was created with `git_auth`, but the daemon no longer
    /// holds that in-memory credential (it restarted). Stage
    /// [`super::failure_stages::GIT_AUTH`]. Never retried: open a new session.
    pub const CREDENTIALS_LOST: &str = "credentials_lost";
    /// A main-phase step failed (`WorkflowOutcome::Failed`).
    pub const STEP_FAILED: &str = "step_failed";
    /// A non-execution command returned a nonzero exit status.
    pub const COMMAND_FAILED: &str = "command_failed";
    /// The main phase finished but teardown did not.
    pub const TEARDOWN_FAILED: &str = "teardown_failed";
    /// Remote-session push-back failed.
    pub const PUSH_FAILED: &str = "push_failed";
    /// `/v1/worker/runs` session setup failed before the command ran.
    pub const SETUP_FAILED: &str = "setup_failed";
    /// Stopped by `POST /v1/worker/commands/{id}/cancel`.
    pub const CANCELLED: &str = "cancelled";
    /// The workflow was aborted (`WorkflowOutcome::Aborted`).
    pub const ABORTED: &str = "aborted";
    /// The workflow paused and can be resumed.
    pub const PAUSED: &str = "paused";
    /// The stored queue row could not be decoded, so the command never ran.
    pub const INVALID_QUEUE_RECORD: &str = "invalid_queue_record";
    /// Anything the worker could not classify (a panic, a missing session).
    pub const INTERNAL: &str = "internal";
}

/// Which phase a failed step ran in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailedStepPhase {
    Setup,
    Main,
    Teardown,
}

/// One failed step of a workflow (WI 0137 §1). An object, not a name, so it
/// carries the phase, the retry and the error without another lookup.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailedStep {
    pub name: String,
    /// The step's position in the workflow definition (main steps) or in its
    /// phase's step list (setup/teardown steps).
    pub index: usize,
    pub phase: FailedStepPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// 1-based; the last attempt when `on_failure` retried the step.
    pub step_attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What a command did to the checkout it ran in (WI 0137 §1).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitResult {
    /// Branch holding the result: the merge target after merge-back, else
    /// the kept worktree's branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// `HEAD` when the command started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// `HEAD` of `branch` at the end, after merge-back and push.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    /// Uncommitted changes remain in the checkout.
    pub dirty: bool,
    /// `base_sha` compared with the final working tree (`git diff --numstat
    /// -M <base>` plus untracked files), so uncommitted edits count. Capped
    /// at [`CHANGED_FILES_CAP`] entries.
    #[serde(default)]
    pub changed_files: Vec<ChangedFile>,
    /// `true` when the cap was hit and `changed_files` is incomplete.
    #[serde(default)]
    pub changed_files_truncated: bool,
    /// Diff collection failed; branch/head/dirty remain available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_files_error: Option<String>,
}

/// Most entries `GitResult::changed_files` lists before it is truncated.
pub const CHANGED_FILES_CAP: usize = 500;

/// How one path changed relative to the base commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangedFileStatus {
    /// New, including untracked files.
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Unmerged,
}

/// One changed path. Binary files report `added`/`deleted` as `null`, and so
/// do untracked files.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: ChangedFileStatus,
    /// The previous path of a renamed or copied file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    #[serde(default)]
    pub added: Option<u64>,
    #[serde(default)]
    pub deleted: Option<u64>,
}

/// The pull request a `create_pull_request` teardown step opened. An object
/// so `number` and `state` can be added later without another field.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PullRequestRef {
    pub url: String,
}

/// Size of a command's `/awman/output` directory: regular files only,
/// symlinks never followed (WI 0137 §3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OutputsSummary {
    pub files: u64,
    pub bytes: u64,
    /// Traversal hit a resource limit or an I/O error; totals are a lower bound.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
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
    conn: std::sync::Arc<Mutex<Connection>>,
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
            conn: std::sync::Arc::new(Mutex::new(conn)),
        })
    }
    pub(crate) fn shared_connection(&self) -> std::sync::Arc<Mutex<Connection>> {
        self.conn.clone()
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
                status      TEXT NOT NULL DEFAULT 'queued',
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
        // WI 0145: kind marker of an in-memory session credential (no value).
        Self::add_column_if_missing(conn, "sessions", "git_auth_kind", "TEXT")?;
        Self::add_column_if_missing(conn, "sessions", "context_key", "TEXT")?;
        Self::add_column_if_missing(conn, "commands", "worker_id", "TEXT")?;
        Self::add_column_if_missing(conn, "commands", "queued_at", "TEXT")?;
        Self::add_column_if_missing(conn, "commands", "result", "TEXT")?;
        // WI 0136: SQLite applies the DEFAULT to every existing row on ADD
        // COLUMN, so pre-0136 commands read back as attempt 1. That is the
        // backfill; no separate UPDATE is needed.
        Self::add_column_if_missing(conn, "commands", "attempt", "INTEGER NOT NULL DEFAULT 1")?;
        Self::add_column_if_missing(
            conn,
            "commands",
            "listener",
            "TEXT NOT NULL DEFAULT 'local'",
        )?;
        Self::add_column_if_missing(conn, "commands", "trace_parent", "TEXT")?;
        Self::add_column_if_missing(conn, "commands", "listener_name", "TEXT")?;
        Self::add_column_if_missing(
            conn,
            "commands",
            "allow_remote_input",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        // WI 0138: old databases may contain the retired status. Convert rows
        // in place without touching queued_at or rowid, which define claim order.
        conn.execute(
            "UPDATE commands SET status = 'queued' WHERE status = 'pending'",
            [],
        )?;
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
        crate::data::fs::schema_version::migrate_ids(conn)?;
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
        insert_session_row(&conn, row)
    }

    /// Publish the provisional session and its rollback barrier atomically.
    /// Until admission finishes, a crash or failed metadata write cannot turn
    /// this row into an accepted response or release its reservation.
    pub fn insert_session_for_admission(
        &self,
        row: NewSessionRow<'_>,
        scope: IdempotencyScope<'_>,
        key: &str,
        owner: IdempotencyOwner<'_>,
    ) -> Result<(), DataError> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE idempotency_keys SET intent_status=NULL,intent_body=NULL,
                resource_kind='compensating_session',resource_id=?5
             WHERE scope=?1 AND key=?2 AND owner_instance=?3 AND owner_request=?4 AND status IS NULL",
            params![scope.key(), key, owner.instance, owner.request, row.id],
        )?;
        if changed != 1 {
            return Err(DataError::Other(
                "Idempotency reservation lost before session insertion".into(),
            ));
        }
        insert_session_row(&tx, row)?;
        tx.commit()?;
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
                                 merge_refs = ?4, push = ?5, git_auth_kind = ?6 \
             WHERE id = ?7",
            params![
                fields.repo_url,
                fields.branch,
                fields.base_ref,
                merge_refs,
                fields.push,
                fields.git_auth_kind,
                id
            ],
        )?;
        Ok(affected > 0)
    }

    /// Update the setup_status for a session. Returns true if the row was
    /// updated (i.e. the session exists).
    pub fn update_setup_status(
        &self,
        id: &str,
        setup_status: SessionSetupStatus,
    ) -> Result<bool, DataError> {
        let conn = self.lock();
        let affected = conn.execute(
            "UPDATE sessions SET setup_status = ?1 WHERE id = ?2",
            params![setup_status.as_str(), id],
        )?;
        Ok(affected > 0)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionRecord>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, workdir, created_at, status, closed_at, \
                    setup_status, session_type, cloned_path, \
                    repo_url, branch, base_ref, merge_refs, push, git_auth_kind, context_key \
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
    /// live inline in the API `POST /v1/worker/commands` handler; it belongs on the
    /// store that owns the `status` column so every frontend enforces it
    /// identically. Frontends map the typed outcome to a transport status.
    pub fn command_admission(
        &self,
        session_id: &str,
    ) -> Result<SessionCommandAdmission, DataError> {
        Ok(match self.get_session(session_id)? {
            Some(s) if s.status == SessionStatus::Active => SessionCommandAdmission::Accepted,
            Some(s) if s.status == SessionStatus::Closing => SessionCommandAdmission::Closing,
            Some(_) => SessionCommandAdmission::Closed,
            None => SessionCommandAdmission::NotFound,
        })
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRecord>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, workdir, created_at, status, closed_at, \
                    setup_status, session_type, cloned_path, \
                    repo_url, branch, base_ref, merge_refs, push, git_auth_kind, context_key \
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
        status: Option<SessionStatus>,
    ) -> Result<Vec<SessionRecord>, DataError> {
        let Some(status) = status else {
            return self.list_sessions();
        };
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, workdir, created_at, status, closed_at, \
                    setup_status, session_type, cloned_path, \
                    repo_url, branch, base_ref, merge_refs, push, git_auth_kind, context_key \
             FROM sessions WHERE status = ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![status.as_str()], session_record_from_row)?;
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
        status: Option<SessionStatus>,
        setup_status: Option<SessionSetupStatus>,
        labels: &[LabelSelector],
    ) -> Result<Vec<SessionRecord>, DataError> {
        let mut sql = String::from(
            "SELECT id,workdir,created_at,status,closed_at,setup_status,session_type,cloned_path,repo_url,branch,base_ref,merge_refs,push,git_auth_kind,context_key FROM sessions s WHERE 1=1",
        );
        let mut args: Vec<Value> = Vec::new();
        if let Some(status) = status {
            sql.push_str(" AND s.status=?");
            args.push(Value::from(status.as_str().to_owned()));
        }
        if let Some(setup_status) = setup_status {
            sql.push_str(" AND s.setup_status=?");
            args.push(Value::from(setup_status.as_str().to_owned()));
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
            "SELECT c.id,c.session_id,c.subcommand,c.args,c.status,c.exit_code,c.started_at,c.finished_at,c.log_path,c.worker_id,c.queued_at,c.result,c.attempt,c.listener,c.trace_parent,c.allow_remote_input,c.listener_name FROM commands c WHERE 1=1",
        );
        let mut args: Vec<Value> = Vec::new();
        if let Some(session) = filter.session_id {
            sql.push_str(" AND c.session_id=?");
            args.push(Value::from(session.to_owned()));
        }
        if let Some(status) = filter.status {
            if status.canonical() == CommandStatus::Queued {
                sql.push_str(" AND c.status IN ('queued','pending')");
            } else {
                sql.push_str(" AND c.status=?");
                args.push(Value::from(status.as_str().to_owned()));
            }
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
                    repo_url, branch, base_ref, merge_refs, push, git_auth_kind, context_key \
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
             VALUES (?1, ?2, ?3, ?4, 'queued', ?5)",
            params![id, session_id, subcommand, args, log_path],
        )?;
        Ok(())
    }

    /// A pulled lease owns its executor permit before this row exists.
    /// Insert as running atomically; it never enters the push queue.
    pub fn insert_running_lease_command(
        &self,
        id: &str,
        session: &str,
        command: &str,
        args: &str,
        allow_remote_input: bool,
    ) -> Result<(), DataError> {
        self.lock().execute(
            "INSERT INTO commands (id, session_id, subcommand, args, status, log_path, started_at, worker_id, allow_remote_input) VALUES (?1, ?2, ?3, ?4, 'running', '', ?5, 'pull', ?6)",
            params![id, session, command, args, chrono::Utc::now().to_rfc3339(), allow_remote_input],
        )?;
        Ok(())
    }

    pub fn get_command(&self, id: &str) -> Result<Option<CommandRecord>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, subcommand, args, status, exit_code, \
                    started_at, finished_at, log_path, worker_id, queued_at, result, attempt, listener, trace_parent, allow_remote_input, listener_name
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
        status: CommandStatus,
        exit_code: Option<i32>,
        finished_at: &str,
    ) -> Result<(), DataError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE commands SET status = ?1, exit_code = ?2, finished_at = ?3 WHERE id = ?4",
            params![status.as_str(), exit_code, finished_at, id],
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
             WHERE session_id = ?1 AND status IN ('queued', 'running')",
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
        // A failed admission must finish compensation before this key can
        // create another session, including after a daemon restart.
        let compensating: Option<(String, bool)> = tx
            .query_row(
                "SELECT request_hash, EXISTS(SELECT 1 FROM sessions
                WHERE id=resource_id AND status!='closed')
             FROM idempotency_keys WHERE scope=?1 AND key=?2
                AND resource_kind='compensating_session'",
                params![scope, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((old_hash, pending)) = compensating {
            if pending {
                tx.commit()?;
                return Ok(if old_hash == request_hash {
                    IdempotencyBegin::InFlight
                } else {
                    IdempotencyBegin::Mismatch
                });
            }
            tx.execute(
                "DELETE FROM idempotency_keys WHERE scope=?1 AND key=?2",
                params![scope, key],
            )?;
        }
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
             WHERE scope=?1 AND key=?2 AND owner_instance=?3 AND owner_request=?4 AND status IS NULL
                AND resource_kind IS NOT 'compensating_session'",
            params![
                scope.key(),
                key,
                owner.instance,
                owner.request,
                status,
                body
            ],
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
             WHERE scope=?1 AND key=?2 AND owner_instance=?3 AND owner_request=?4 AND status IS NULL
                AND (resource_kind IS NOT 'compensating_session' OR NOT EXISTS
                    (SELECT 1 FROM sessions WHERE id=resource_id AND status!='closed'))",
            params![scope.key(), key, owner.instance, owner.request],
        )?;
        Ok(())
    }

    /// Void the accepted intent before asynchronous rollback. Keep a durable
    /// barrier until this session is closed; an interrupted rollback is never
    /// mistaken for a successful admission or a request that created nothing.
    pub fn idempotency_compensating_session(
        &self,
        scope: IdempotencyScope<'_>,
        key: &str,
        owner: IdempotencyOwner<'_>,
        session_id: &str,
    ) -> Result<bool, DataError> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE idempotency_keys SET intent_status=NULL,intent_body=NULL,
                resource_kind='compensating_session',resource_id=?5
             WHERE scope=?1 AND key=?2 AND owner_instance=?3 AND owner_request=?4 AND status IS NULL",
            params![scope.key(), key, owner.instance, owner.request, session_id],
        )?;
        Ok(changed > 0)
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
                "DELETE FROM idempotency_keys WHERE scope=?1 AND key=?2 AND status IS NULL
                    AND (resource_kind IS NOT 'compensating_session' OR NOT EXISTS
                        (SELECT 1 FROM sessions WHERE id=resource_id AND status!='closed'))",
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
            "INSERT INTO commands (id, session_id, subcommand, args, status, log_path, queued_at, listener, trace_parent)
             VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?6, ?7, ?8)",
            params![id, session_id, subcommand, args, log_path, now,"local",Option::<String>::None],
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
        self.enqueue_command_with_context(
            id,
            session_id,
            subcommand,
            args,
            log_path,
            labels,
            &Default::default(),
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_command_with_context(
        &self,
        id: &str,
        session_id: &str,
        subcommand: &str,
        args: &str,
        log_path: &str,
        labels: &Labels,
        context: &crate::data::daemon_state::CommandRequestContext,
    ) -> Result<(), DataError> {
        self.enqueue_command_with_terminal_input(
            id, session_id, subcommand, args, log_path, labels, context, false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_command_with_terminal_input(
        &self,
        id: &str,
        session_id: &str,
        subcommand: &str,
        args: &str,
        log_path: &str,
        labels: &Labels,
        context: &crate::data::daemon_state::CommandRequestContext,
        allow_remote_input: bool,
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
            "INSERT INTO commands (id, session_id, subcommand, args, status, log_path, queued_at, listener, trace_parent, allow_remote_input, listener_name)
             VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?6, ?7, ?8, ?9, ?10)",
            params![id, session_id, subcommand, args, log_path, now,context.listener.as_str(),context.trace_parent, allow_remote_input, context.listener_name],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn claim_next_command(&self, worker_id: &str) -> Result<Option<CommandRecord>, DataError> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.lock();
        // Atomically claim the next eligible queued command and ensure no
        // other command in the same session is already running.
        let mut stmt = conn.prepare(
            "UPDATE commands
             SET status = 'running', worker_id = ?1, started_at = ?2
             WHERE id = (
                 SELECT c.id FROM commands c
                 WHERE c.status IN ('queued','pending')
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
                       started_at, finished_at, log_path, worker_id, queued_at, result, attempt, listener, trace_parent, allow_remote_input, listener_name",
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
        status: CommandStatus,
        exit_code: Option<i32>,
        result_json: Option<&str>,
    ) -> Result<(), DataError> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.lock();
        conn.execute(
            "UPDATE commands SET status = ?1, exit_code = ?2, finished_at = COALESCE(finished_at, ?3), result = ?4
             WHERE id = ?5",
            params![status.as_str(), exit_code, now, result_json, id],
        )?;
        Ok(())
    }

    /// Finalize a row left `running` from its terminal sidecar, keeping the
    /// recorded finish time. Only the attempt that wrote the sidecar qualifies;
    /// returns whether the row changed.
    pub fn finalize_running_command(
        &self,
        id: &str,
        completion: &CommandCompletion,
    ) -> Result<bool, DataError> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE commands SET status = ?1, exit_code = ?2, finished_at = ?3, result = ?4
             WHERE id = ?5 AND status = 'running' AND attempt = ?6",
            params![
                completion.status.as_str(),
                completion.exit_code,
                completion.finished_at,
                completion.result_json,
                id,
                completion.attempt
            ],
        )?;
        Ok(changed > 0)
    }

    pub fn list_commands_for_session(
        &self,
        session_id: &str,
        limit: i64,
    ) -> Result<Vec<CommandRecord>, DataError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, subcommand, args, status, exit_code,
                    started_at, finished_at, log_path, worker_id, queued_at, result, attempt, listener, trace_parent, allow_remote_input, listener_name
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

    /// Load the persisted rows needed by the session queue read model.
    pub fn session_queue_records(
        &self,
        session_id: &str,
    ) -> Result<SessionQueueRecords, DataError> {
        Ok(SessionQueueRecords {
            queue_depth: self.count_queued_for_session(session_id)?,
            running: self.running_command_for_session(session_id)?,
            recent: self.list_commands_for_session(session_id, 100)?,
        })
    }

    pub fn count_queued_for_session(&self, session_id: &str) -> Result<i64, DataError> {
        let conn = self.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM commands WHERE session_id = ?1 AND status IN ('queued','pending')",
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
                    started_at, finished_at, log_path, worker_id, queued_at, result, attempt, listener, trace_parent, allow_remote_input, listener_name
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
        // One statement, so the returned IDs are exactly the rows cancelled. An
        // older writer can insert `pending` after this store's startup migration.
        let mut stmt = conn.prepare(
            "UPDATE commands SET status = 'cancelled', finished_at = ?1
             WHERE session_id = ?2 AND status IN ('queued','pending')
             RETURNING id",
        )?;
        let ids = stmt
            .query_map(params![now, session_id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(ids)
    }

    /// Re-queue pushed commands a dead worker left `running`. Pulled-lease
    /// rows (`worker_id = 'pull'`) are never re-queued: they cannot run
    /// without their lease, and a live TUI or CLI host may still own one.
    pub fn recover_stale_commands(&self, timeout_secs: u64) -> Result<Vec<String>, DataError> {
        let cutoff = chrono::Utc::now() - chrono::Duration::seconds(timeout_secs as i64);
        let cutoff_str = cutoff.to_rfc3339();
        let conn = self.lock();
        let ids: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT id FROM commands
                 WHERE status = 'running' AND started_at IS NOT NULL AND started_at < ?1
                   AND worker_id IS NOT 'pull'",
            )?;
            let rows = stmt
                .query_map(params![cutoff_str], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        conn.execute(
            "UPDATE commands SET status = 'queued', worker_id = NULL, started_at = NULL,
                 attempt = attempt + 1
             WHERE status = 'running' AND started_at IS NOT NULL AND started_at < ?1
                   AND worker_id IS NOT 'pull'",
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
        let position: Option<i64> = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM commands c
                WHERE c.session_id = ?2 AND c.status IN ('queued','pending')
                  AND ((c.queued_at IS NULL AND target.queued_at IS NOT NULL)
                    OR c.queued_at < target.queued_at
                    OR (c.queued_at IS target.queued_at AND c.rowid < target.rowid)))
             FROM commands target WHERE target.id = ?1 AND target.status IN ('queued','pending')",
                params![command_id, session_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(position)
    }

    /// Update a session's status.
    pub fn update_session_status(
        &self,
        id: &str,
        status: SessionStatus,
    ) -> Result<bool, DataError> {
        let conn = self.lock();
        let affected = conn.execute(
            "UPDATE sessions SET status = ?1 WHERE id = ?2",
            params![status.as_str(), id],
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

/// Read a status column through its enum's `parse`. An unknown stored value is
/// an error naming the column and value, never a default: the row's real state
/// is unknown, and guessing could rerun or abandon its work.
fn status_column<T>(
    row: &rusqlite::Row<'_>,
    index: usize,
    column: &str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> rusqlite::Result<T> {
    let value: String = row.get(index)?;
    parse(&value).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            Type::Text,
            format!("unknown {column} value {value:?}").into(),
        )
    })
}

fn session_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    Ok(SessionRecord {
        id: row.get(0)?,
        workdir: row.get(1)?,
        created_at: row.get(2)?,
        status: status_column(row, 3, "sessions.status", SessionStatus::parse)?,
        closed_at: row.get(4)?,
        setup_status: status_column(row, 5, "sessions.setup_status", SessionSetupStatus::parse)?,
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
        git_auth_kind: row.get(13)?,
        context_key: row.get(14)?,
    })
}

fn command_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CommandRecord> {
    Ok(CommandRecord {
        listener_name: row.get(16)?,
        allow_remote_input: row.get(15)?,
        listener: row.get(13)?,
        trace_parent: row.get(14)?,
        id: row.get(0)?,
        session_id: row.get(1)?,
        subcommand: row.get(2)?,
        args: row.get(3)?,
        status: status_column(row, 4, "commands.status", CommandStatus::parse)?.canonical(),
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
    Ok(Labels::from_stored(map))
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

fn insert_session_row(conn: &Connection, row: NewSessionRow<'_>) -> Result<(), DataError> {
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

impl SqliteSessionStore {
    pub fn set_context_key(
        &self,
        id: &str,
        key: Option<&crate::data::context::ContextKey>,
    ) -> Result<(), DataError> {
        self.conn.lock().unwrap().execute(
            "UPDATE sessions SET context_key=?1 WHERE id=?2",
            params![key.map(|k| k.as_str()), id],
        )?;
        Ok(())
    }
}
