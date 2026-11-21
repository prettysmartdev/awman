//! Streaming writer for a single command run's on-disk logs.
//!
//! A command execution emits a stream of [`ExecutionEvent`]s. Two files are
//! written incrementally as they arrive: `events.log` (one JSON object per
//! line, the machine-readable stream) and `output.log` (the human-readable
//! plain-text rendering). Both are opened in append mode: a command re-queued
//! after a daemon restart adds its next attempt to the same files (WI 0136).
//! This type owns both open file handles so the API
//! frontend's queue worker never touches `tokio::fs` directly
//! (grand-architecture Tenet 2).

use std::io::SeekFrom;
use std::path::Path;

use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::data::error::DataError;
use crate::data::execution_event::ExecutionEvent;
use crate::data::fs::api_db::{FailedStep, PullRequestRef};
use crate::data::fs::api_paths::ApiPaths;

/// Owns the filesystem side of a command's live workflow state so the API
/// frontend never touches the filesystem itself. Synchronous because the
/// engine reports state from its synchronous persistence path.
///
/// It is also the one handle the API frontend and the queue worker share
/// while a command runs, so facts the frontend learns on the way — the kept
/// worktree, and since WI 0137 the [`ResultContributions`] — reach the
/// worker's `CommandResult` through it.
#[derive(Clone)]
pub struct CommandStateWriter {
    worktree: std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
    contributions: std::sync::Arc<std::sync::Mutex<ResultContributions>>,
    mount_receipts: std::sync::Arc<std::sync::Mutex<Vec<tempfile::NamedTempFile>>>,
    paths: ApiPaths,
    session_id: String,
    command_id: String,
}

/// Pieces of a `CommandResult` (WI 0137 §1) that are learned while the
/// command runs, by whoever learns them, and merged into the result by the
/// queue worker when the command ends. Every field is optional; a field that
/// nobody recorded stays absent from the result.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultContributions {
    /// Engine completion, handed to the queue worker which owns the final event.
    pub workflow_outcome: Option<crate::data::execution_event::CommandStatusKind>,
    pub workflow_failure: Option<crate::data::fs::api_db::CommandFailure>,
    pub workflow_main_started: bool,
    pub container_started: bool,
    /// `exec prompt`: the agent's final message, from the output translator.
    pub final_message: Option<String>,
    /// The URL a `create_pull_request` teardown step printed.
    pub pull_request: Option<PullRequestRef>,
    /// The JSON value of a command with a `--json` form.
    pub data: Option<serde_json::Value>,
    /// Failed main, setup and teardown steps, each listed once (the last
    /// record for a phase/index wins, so a retried step keeps its last attempt).
    pub failed_steps: Vec<FailedStep>,
    /// Whether `/awman/output` was actually mounted for this command
    /// (`None` until the command reaches its launch decision). An empty
    /// output directory and a missing mount are therefore distinguishable.
    pub outputs_mounted: Option<bool>,
    /// The command ran an automation execution (WI 0070): its answer is
    /// only ever the native-confirmed one, whatever the outcome shape.
    pub automation: bool,
    /// The frozen execution assurance, recorded when the execution
    /// finishes, so a cancelled or otherwise failed command still reports it.
    pub automation_result: Option<crate::data::automation_records::CommandAutomationResult>,
    /// The durable native answer, kept on cancellation as well as success.
    pub automation_final_message: Option<String>,
}

impl CommandStateWriter {
    /// Record whether `/awman/output` was mounted (see
    /// `Capabilities::mounts_outputs`). A later call replaces an earlier one.
    pub fn record_outputs_mounted(&self, mounted: bool) {
        self.contributions().outputs_mounted = Some(mounted);
    }

    /// Allocate an empty receipt in the actual output directory. Only a
    /// guest that sees that bind can acknowledge it; holding the open inode
    /// lets the guest unlink the name before starting the agent.
    pub fn output_mount_receipt(&self) -> Result<std::path::PathBuf, DataError> {
        let receipt = tempfile::Builder::new()
            .prefix(".awman-mount-")
            .tempfile_in(self.output_dir())
            .map_err(|e| DataError::Other(format!("output mount receipt: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            receipt
                .as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o666))
                .map_err(|e| DataError::Other(e.to_string()))?;
        }
        let guest = std::path::Path::new(crate::data::fs::command_outputs::OUTPUT_MOUNT_PATH)
            .join(receipt.path().file_name().expect("temporary file name"));
        self.mount_receipts.lock().unwrap().push(receipt);
        Ok(guest)
    }

    pub fn record_container_started(&self) {
        self.contributions().container_started = true;
    }
    pub fn record_workflow_main_started(&self) {
        self.contributions().workflow_main_started = true;
    }

    pub fn record_workflow_failure(
        &self,
        failure: Option<crate::data::fs::api_db::CommandFailure>,
    ) {
        self.contributions().workflow_failure = failure;
    }

    pub fn record_workflow_outcome(
        &self,
        outcome: crate::data::execution_event::CommandStatusKind,
    ) {
        self.contributions().workflow_outcome = Some(outcome);
    }

    pub fn new(
        paths: ApiPaths,
        session_id: impl Into<String>,
        command_id: impl Into<String>,
    ) -> Self {
        Self {
            worktree: Default::default(),
            contributions: Default::default(),
            mount_receipts: Default::default(),
            paths,
            session_id: session_id.into(),
            command_id: command_id.into(),
        }
    }

    pub fn command_id(&self) -> &str {
        &self.command_id
    }

    /// The command's `/awman/output` host directory (WI 0137 §3).
    pub fn output_dir(&self) -> std::path::PathBuf {
        self.paths
            .command_output_dir(&self.session_id, &self.command_id)
    }

    /// Record the agent's final message (WI 0137 §4). A later call replaces
    /// an earlier one: an `exec prompt` restart reruns from scratch.
    pub fn record_final_message(&self, message: impl Into<String>) {
        self.contributions().final_message = Some(message.into());
    }

    /// Record that this command runs an automation execution.
    pub fn record_automation(&self) {
        let mut contributions = self.contributions();
        contributions.automation = true;
        contributions.automation_result = None;
        contributions.automation_final_message = None;
    }

    /// Record the frozen automation assurance of the finished execution.
    pub fn record_automation_result(
        &self,
        result: &crate::data::automation_records::CommandAutomationResult,
        final_message: Option<&str>,
    ) {
        let mut contributions = self.contributions();
        contributions.automation = true;
        contributions.automation_result = Some(result.clone());
        contributions.automation_final_message = final_message.map(str::to_owned);
    }

    /// Record the pull request a teardown step opened (WI 0137 §1).
    pub fn record_pull_request(&self, url: impl Into<String>) {
        self.contributions().pull_request = Some(PullRequestRef { url: url.into() });
    }

    /// Record the JSON value of a `--json` command (WI 0137 §1).
    pub fn record_data(&self, value: serde_json::Value) {
        self.contributions().data = Some(value);
    }

    /// Record a failed step, replacing an earlier record of the same index and
    /// phase so a retried step is listed once with its last attempt.
    pub fn record_failed_step(&self, mut step: FailedStep) {
        let mut contributions = self.contributions();
        if let Some(previous) = contributions.failed_steps.iter().find(|existing| {
            existing.index == step.index
                && existing.phase == step.phase
                && existing.step_attempt == step.step_attempt
        }) {
            step.exit_code = step.exit_code.or(previous.exit_code);
            step.error = step.error.or_else(|| previous.error.clone());
        }
        contributions
            .failed_steps
            .retain(|existing| !(existing.index == step.index && existing.phase == step.phase));
        contributions.failed_steps.push(step);
    }

    pub fn record_step_succeeded(
        &self,
        phase: crate::data::fs::api_db::FailedStepPhase,
        index: usize,
    ) {
        self.contributions()
            .failed_steps
            .retain(|step| step.phase != phase || step.index != index);
    }

    /// A snapshot of everything recorded so far.
    pub fn result_contributions(&self) -> ResultContributions {
        let mut result = self.contributions().clone();
        if self
            .mount_receipts
            .lock()
            .unwrap()
            .iter()
            .any(|receipt| receipt.as_file().metadata().is_ok_and(|m| m.len() > 0))
        {
            result.outputs_mounted = Some(true);
        }
        result
    }

    fn contributions(&self) -> std::sync::MutexGuard<'_, ResultContributions> {
        self.contributions
            .lock()
            .expect("result contributions poisoned")
    }

    /// Shared with the queue worker so early errors after preparation still
    /// return the checkout path. Snapshot only surviving directories.
    pub fn record_worktree(&self, path: &std::path::Path) {
        *self.worktree.lock().expect("worktree result poisoned") = Some(path.to_path_buf());
    }

    pub fn clear_worktree(&self) {
        *self.worktree.lock().expect("worktree result poisoned") = None;
    }

    pub fn kept_worktree(&self) -> Option<std::path::PathBuf> {
        self.worktree
            .lock()
            .expect("worktree result poisoned")
            .clone()
            .filter(|path| path.is_dir())
    }

    pub fn write(&self, state: &impl serde::Serialize) -> Result<(), DataError> {
        self.paths
            .write_command_workflow_state(&self.session_id, &self.command_id, state)
    }

    pub fn write_cleanup(
        &self,
        cleanup: &crate::data::workflow_cleanup::WorkflowCleanup,
    ) -> Result<(), DataError> {
        self.paths
            .write_command_cleanup(&self.session_id, &self.command_id, cleanup)
    }
}

/// Owns the two open log files for one command run and appends to them as
/// events arrive.
pub struct CommandLogWriter {
    events_file: File,
    output_file: File,
}

impl CommandLogWriter {
    /// Terminal pair for commands that never had an owning worker. Reconciliation
    /// may call this again after a restart; completed attempts are left alone.
    /// The caller must ensure no worker is concurrently writing these logs.
    pub fn append_terminal(
        paths: &ApiPaths,
        command: &crate::data::fs::api_db::CommandRecord,
        store: &crate::data::fs::api_db::SqliteSessionStore,
        payload: crate::data::execution_event::EventPayload,
    ) -> Result<(), DataError> {
        Self::recover_lifecycle(
            paths,
            command,
            &crate::data::automation_store::AutomationStore::from_store(store),
        )?;
        use crate::data::execution_event::{EventPayload, ExecutionEvent};
        use std::io::{BufRead, Read, Seek, Write};
        paths.prepare_command_dir(&command.session_id, &command.id)?;
        let path = paths.command_events_log_path(&command.session_id, &command.id);
        let append = || -> std::io::Result<()> {
            let mut sequence = 0;
            let mut has_status = false;
            if let Ok(file) = std::fs::File::open(&path) {
                for line in std::io::BufReader::new(file).split(b'\n') {
                    if let Ok(event) = serde_json::from_slice::<ExecutionEvent>(&line?) {
                        if event.attempt == command.attempt {
                            sequence = sequence.max(event.sequence + 1);
                            if matches!(event.payload, EventPayload::Done) {
                                return Ok(());
                            }
                            has_status |=
                                matches!(event.payload, EventPayload::CommandStatus { .. });
                        }
                    }
                }
            }
            let mut events = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            if events.metadata()?.len() > 0 {
                let mut tail = std::fs::File::open(&path)?;
                tail.seek(SeekFrom::End(-1))?;
                let mut byte = [0];
                tail.read_exact(&mut byte)?;
                if byte[0] != b'\n' {
                    events.write_all(b"\n")?;
                }
            }
            let mut output = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(paths.command_log_path(&command.session_id, &command.id))?;
            for payload in [(!has_status).then_some(payload), Some(EventPayload::Done)]
                .into_iter()
                .flatten()
            {
                let event = ExecutionEvent {
                    timestamp: chrono::Utc::now(),
                    attempt: command.attempt,
                    sequence,
                    source: None,
                    payload,
                };
                serde_json::to_writer(&mut events, &event)?;
                events.write_all(b"\n")?;
                if let Some(line) = event.payload.to_plain_text() {
                    writeln!(output, "{line}")?;
                }
                sequence += 1;
            }
            events.flush()?;
            output.flush()?;
            events.sync_data()?;
            output.sync_data()
        };
        append().map_err(|source| DataError::Io { path, source })?;
        sync_command_directories(paths, &command.session_id, &command.id)
    }

    /// Open both log files for `command_id` under `session_id` for appending,
    /// creating them if absent. A re-queued command (attempt > 1) continues
    /// the same files, so the previous attempt's lines stay. If a file is
    /// non-empty and does not end in `'\n'` (a daemon crash mid-write), a
    /// `'\n'` is written first so the next line is not glued to the torn one.
    pub async fn open_append(
        paths: &ApiPaths,
        session_id: &str,
        command_id: &str,
    ) -> Result<Self, DataError> {
        let events_path = paths.command_events_log_path(session_id, command_id);
        let output_path = paths.command_log_path(session_id, command_id);
        let events_file = open_append_on_line_boundary(&events_path).await?;
        let output_file = open_append_on_line_boundary(&output_path).await?;
        Ok(Self {
            events_file,
            output_file,
        })
    }

    /// Fallible lifecycle append; the caller owns ordering and its deadline.
    pub async fn try_write_event(&mut self, event: &ExecutionEvent) -> std::io::Result<()> {
        let json = serde_json::to_string(event)?;
        self.events_file
            .write_all(format!("{json}\n").as_bytes())
            .await?;
        if let Some(text) = event.payload.to_plain_text() {
            self.output_file
                .write_all(format!("{text}\n").as_bytes())
                .await?;
        }
        Ok(())
    }
    pub async fn try_flush(&mut self) -> std::io::Result<()> {
        self.events_file.flush().await?;
        self.output_file.flush().await?;
        self.events_file.sync_data().await?;
        self.output_file.sync_data().await
    }
    /// Repair only the owning execution's reserved projection, before opening publication.
    /// Exact cursor/content matches replay harmlessly; conflicting or later final records fail.
    pub fn recover_lifecycle(
        paths: &ApiPaths,
        command: &crate::data::fs::api_db::CommandRecord,
        store: &crate::data::automation_store::AutomationStore,
    ) -> Result<(), DataError> {
        use crate::data::execution_event::EventPayload;
        use std::io::{Read, Seek, Write};
        let pending = store
            .list_recoverable()?
            .into_iter()
            .filter(|e| e.command_id.as_str() == command.id && e.pending_events.is_some())
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return Ok(());
        }
        paths.prepare_command_dir(&command.session_id, &command.id)?;
        let path = paths.command_events_log_path(&command.session_id, &command.id);
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|e| DataError::io(&path, e))?;
        for execution in pending {
            let batch = execution.pending_events.expect("filtered batch");
            // Scan by byte lines: a crash may tear the last UTF-8 scalar as
            // well as JSON. Keep only matches for this bounded projection.
            use std::io::BufRead;
            let reader = std::io::BufReader::new(
                std::fs::File::open(&path).map_err(|e| DataError::io(&path, e))?,
            );
            let mut found = std::collections::HashSet::new();
            let mut latest = None;
            let mut finalized = false;
            for line in reader.split(b'\n') {
                let line = line.map_err(|e| DataError::io(&path, e))?;
                let Ok(saved) = serde_json::from_slice::<ExecutionEvent>(&line) else {
                    continue;
                };
                latest = Some(latest.map_or(
                    saved.cursor(),
                    |cursor: crate::data::execution_event::EventCursor| cursor.max(saved.cursor()),
                ));
                finalized |= saved.attempt == execution.attempt
                    && matches!(
                        saved.payload,
                        EventPayload::CommandStatus { .. } | EventPayload::Done
                    );
                if let Some(expected) = batch.iter().find(|event| event.cursor() == saved.cursor())
                {
                    if serde_json::to_value(&saved).map_err(|e| DataError::Other(e.to_string()))?
                        != serde_json::to_value(expected)
                            .map_err(|e| DataError::Other(e.to_string()))?
                    {
                        return Err(DataError::Other("conflicting lifecycle cursor".into()));
                    }
                    found.insert(saved.cursor());
                }
            }
            for event in &batch {
                if !found.contains(&event.cursor()) {
                    if latest.is_some_and(|cursor| cursor >= event.cursor()) || finalized {
                        return Err(DataError::Other(
                            "lifecycle event follows finalization or unflushed cursor".into(),
                        ));
                    }
                    if file.metadata().map_err(|e| DataError::io(&path, e))?.len() > 0 {
                        let mut tail =
                            std::fs::File::open(&path).map_err(|e| DataError::io(&path, e))?;
                        tail.seek(SeekFrom::End(-1))
                            .map_err(|e| DataError::io(&path, e))?;
                        let mut byte = [0];
                        tail.read_exact(&mut byte)
                            .map_err(|e| DataError::io(&path, e))?;
                        if byte[0] != b'\n' {
                            file.write_all(b"\n").map_err(|e| DataError::io(&path, e))?;
                        }
                    }
                    let encoded = serde_json::to_vec(event)
                        .map_err(|_| DataError::Other("invalid lifecycle event".into()))?;
                    file.write_all(&encoded)
                        .and_then(|_| file.write_all(b"\n"))
                        .map_err(|e| DataError::io(&path, e))?;
                    latest = Some(event.cursor());
                }
            }
            file.flush()
                .and_then(|_| file.sync_data())
                .map_err(|e| DataError::io(&path, e))?;
            sync_command_directories(paths, &command.session_id, &command.id)?;
            store.clear_pending_events(
                &execution.execution_id,
                execution
                    .pending_event_cursor
                    .ok_or_else(|| DataError::Other("missing lifecycle cursor".into()))?,
            )?;
        }
        Ok(())
    }
    /// Append one event: its JSON line to `events.log`, and — when the payload
    /// has a plain-text rendering — that text to `output.log`. Write errors are
    /// swallowed (best-effort logging), matching the previous inline behavior.
    pub async fn write_event(&mut self, event: &ExecutionEvent) {
        if let Ok(json) = serde_json::to_string(event) {
            let _ = self
                .events_file
                .write_all(format!("{json}\n").as_bytes())
                .await;
        }
        if let Some(text) = event.payload.to_plain_text() {
            let _ = self
                .output_file
                .write_all(format!("{text}\n").as_bytes())
                .await;
        }
    }

    /// Flush both files to disk.
    pub async fn flush(&mut self) {
        let _ = self.events_file.flush().await;
        let _ = self.output_file.flush().await;
    }
}

/// Open `path` for appending (creating it if absent) and make sure the next
/// write starts on a fresh line.
async fn open_append_on_line_boundary(path: &Path) -> Result<File, DataError> {
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .await
        .map_err(|e| DataError::io(path, e))?;
    let len = file
        .metadata()
        .await
        .map_err(|e| DataError::io(path, e))?
        .len();
    if len > 0 {
        let mut last = [0u8; 1];
        file.seek(SeekFrom::Start(len - 1))
            .await
            .map_err(|e| DataError::io(path, e))?;
        file.read_exact(&mut last)
            .await
            .map_err(|e| DataError::io(path, e))?;
        // Append mode writes at the end regardless of the read position.
        if last[0] != b'\n' {
            file.write_all(b"\n")
                .await
                .map_err(|e| DataError::io(path, e))?;
        }
    }
    Ok(file)
}

impl CommandStateWriter {
    pub fn paths(&self) -> &ApiPaths {
        &self.paths
    }
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// The event bus's ordered persistence seam. A flush acknowledges stable storage,
/// not merely enqueueing a write. Implementations must keep append errors sticky.
pub trait EventLog: Send {
    fn append(&mut self, event: &ExecutionEvent) -> std::io::Result<()>;
    fn flush(&mut self) -> std::io::Result<()>;
}

/// Synchronous writer for the synchronous lifecycle transaction/publication path.
/// The bus owns it under its publication lock, so progress cannot overtake a batch.
pub struct OrderedCommandLog {
    events: std::io::BufWriter<std::fs::File>,
    output: std::io::BufWriter<std::fs::File>,
}
impl OrderedCommandLog {
    pub fn open(paths: &ApiPaths, session: &str, command: &str) -> Result<Self, DataError> {
        use std::io::{Read, Seek, Write};
        fn open(path: &Path) -> Result<std::io::BufWriter<std::fs::File>, DataError> {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .append(true)
                .open(path)
                .map_err(|e| DataError::io(path, e))?;
            if file.metadata().map_err(|e| DataError::io(path, e))?.len() > 0 {
                file.seek(SeekFrom::End(-1))
                    .map_err(|e| DataError::io(path, e))?;
                let mut byte = [0];
                file.read_exact(&mut byte)
                    .map_err(|e| DataError::io(path, e))?;
                if byte[0] != b'\n' {
                    file.write_all(b"\n").map_err(|e| DataError::io(path, e))?;
                }
            }
            Ok(std::io::BufWriter::new(file))
        }
        paths.prepare_command_dir(session, command)?;
        let writer = Self {
            events: open(&paths.command_events_log_path(session, command))?,
            output: open(&paths.command_log_path(session, command))?,
        };
        sync_command_directories(paths, session, command)?;
        Ok(writer)
    }
}
impl EventLog for OrderedCommandLog {
    fn append(&mut self, event: &ExecutionEvent) -> std::io::Result<()> {
        use std::io::Write;
        serde_json::to_writer(&mut self.events, event)?;
        self.events.write_all(b"\n")?;
        if let Some(text) = event.payload.to_plain_text() {
            writeln!(self.output, "{text}")?;
        }
        Ok(())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        use std::io::Write;
        self.events.flush()?;
        self.output.flush()?;
        self.events.get_ref().sync_data()?;
        self.output.get_ref().sync_data()
    }
}

/// File sync acknowledges contents; Unix also requires syncing newly created
/// directory entries before the pending projection can be retired.
fn sync_command_directories(
    paths: &ApiPaths,
    session: &str,
    command: &str,
) -> Result<(), DataError> {
    #[cfg(unix)]
    for directory in paths.command_dir(session, command).ancestors() {
        std::fs::File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(|e| DataError::io(directory, e))?;
        if directory == paths.root() {
            break;
        }
    }
    #[cfg(not(unix))]
    let _ = (paths, session, command);
    Ok(())
}
