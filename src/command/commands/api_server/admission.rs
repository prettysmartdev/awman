//! API session/command admission and crash-safe idempotency ownership.
//!
//! Provisional sessions have a transactional rollback barrier until all admission
//! writes finish. Accepted intents recover fully admitted sessions/runs; command-
//! only admission records its intent before the command row.

use super::{ApiSessionLifecycle, SetupReadiness};
use crate::command::dispatch::catalogue::{CommandCatalogue, FrontendKind};
use crate::command::dispatch::projections::raw_args::ParsedArgs;
use crate::command::dispatch::{CommandFrontend, Dispatch, Engines};
use crate::command::error::CommandError;
use crate::command::session_create::{
    SessionCreatePlan, SessionCreatePolicy, SessionCreateRequest,
};
use crate::data::api_lifecycle_store::{CommandLifecycleStore, RunRecord};
use crate::data::error::DataError;
use crate::data::fs::api_db::{
    IdempotencyBegin, IdempotencyOwner, IdempotencyResource, IdempotencyScope, LabelOwner,
    NewSessionRow, RemoteSessionFields, SessionCommandAdmission, SqliteSessionStore,
};
use crate::data::fs::api_labels::Labels;
use crate::data::fs::api_paths::ApiPaths;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::session::SessionKind;
use crate::data::session_manager::SessionManager;
use crate::data::session_setup_event::SessionSetupStatus;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Session inputs after the host has decoded its request shape.
pub struct SessionSubmission {
    pub request: SessionCreateRequest,
    pub labels: Labels,
}

/// Catalogue command tokens and validated label values, independent of HTTP.
pub struct CommandSubmission {
    pub subcommand: String,
    pub args: Vec<String>,
    pub labels: Labels,
}

/// Durable admission plus the plan the host presents during asynchronous setup.
pub struct AdmittedSession {
    pub session_id: String,
    pub plan: SessionCreatePlan,
    pub run: Option<RunRecord>,
    pub response_body: String,
}

/// Durable queue admission and the exact response recorded for crash recovery.
pub struct AdmittedCommand {
    pub command_id: String,
    pub flags_applied: serde_json::Value,
    pub response_body: String,
}

/// Typed decisions let a host map errors without inspecting their messages.
#[derive(Debug)]
pub enum AdmissionError {
    SessionCreation(CommandError),
    CommandValidation(CommandError),
    CommandAdmission(CommandError),
    Unavailable {
        command: String,
        available: Vec<String>,
    },
    Session(SessionCommandAdmission),
    Setup(SetupReadiness),
    Storage {
        what: &'static str,
        source: DataError,
    },
    ReservationLost,
}

fn storage(what: &'static str, source: DataError) -> AdmissionError {
    AdmissionError::Storage { what, source }
}

impl CommandSubmission {
    /// Keep the original wire value: absent labels and unknown fields have
    /// always participated in fingerprints. Both session-selection forms
    /// share the same fingerprint, including keys written before the upgrade.
    pub fn idempotency_canonical(value: &serde_json::Value, session: &str) -> String {
        let mut value = value.clone();
        if let Some(body) = value.as_object_mut() {
            body.remove("session_id");
        }
        format!("{value}\n{session}")
    }

    fn validate(&self) -> Result<ParsedArgs, AdmissionError> {
        let catalogue = CommandCatalogue::get();
        let path: Vec<&str> = self.subcommand.split_whitespace().collect();
        if let Err(CommandError::NotAvailableForFrontend { command, .. }) =
            catalogue.validate_for_frontend(FrontendKind::Api, &path)
        {
            return Err(AdmissionError::Unavailable {
                command,
                available: catalogue
                    .api_allowed_commands()
                    .into_iter()
                    .map(|(parent, sub)| format!("{parent} {sub}"))
                    .collect(),
            });
        }
        catalogue
            .parse_raw_args_with_profile(&path, &self.args, FrontendKind::Api)
            .map_err(AdmissionError::CommandValidation)
    }
}

/// Owns admission validation, durable write order and failure compensation.
#[derive(Clone)]
pub struct ApiAdmission {
    store: Arc<SqliteSessionStore>,
    lifecycle_store: CommandLifecycleStore,
    paths: ApiPaths,
    engines: Engines,
    sessions: Arc<SessionManager>,
    workdirs: Vec<PathBuf>,
    workdir_roots: Vec<PathBuf>,
}

impl ApiAdmission {
    pub fn new(
        store: Arc<SqliteSessionStore>,
        lifecycle_store: CommandLifecycleStore,
        paths: ApiPaths,
        engines: Engines,
        sessions: Arc<SessionManager>,
        workdirs: Vec<PathBuf>,
        workdir_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            store,
            lifecycle_store,
            paths,
            engines,
            sessions,
            workdirs,
            workdir_roots,
        }
    }

    fn lifecycle(&self) -> ApiSessionLifecycle {
        ApiSessionLifecycle::new(
            Arc::clone(&self.store),
            self.engines.clone(),
            Arc::clone(&self.sessions),
            self.paths.clone(),
        )
    }

    pub async fn admit_session(
        &self,
        submission: SessionSubmission,
        command: Option<CommandSubmission>,
        close_on_finish: bool,
        journal: Option<&IdempotentAdmission>,
    ) -> Result<AdmittedSession, AdmissionError> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let created_at = chrono::Utc::now().to_rfc3339();
        let session_dir = self.paths.session_dir(&session_id);

        // Validate and plan the session in Layer 2. The route only maps the typed
        // result/error to an HTTP status + JSON envelope — no business logic here.
        let policy = SessionCreatePolicy::new(self.workdirs.clone(), session_dir.clone())
            .with_workdir_roots(self.workdir_roots.clone());
        let plan = submission
            .request
            .validate(&policy, self.engines.git_engine.as_ref())
            .map_err(AdmissionError::SessionCreation)?;
        plan.validate_merge_ref_kinds(Arc::clone(&self.engines.git_engine))
            .await
            .map_err(AdmissionError::SessionCreation)?;
        if let Some(command) = &command {
            command.validate()?;
        }

        // Create session storage directories (Layer 0).
        if let Err(e) = self.paths.prepare_session_dirs(&session_id) {
            tracing::error!(error = %e, "Failed to create session directory");
            return Err(storage("Failed to create session directory", e));
        }

        // Fix the response before durable writes. The provisional insertion
        // replaces this intent with a rollback barrier; only complete admission
        // restores it for crash recovery from the session or command resource.
        let command_id = command.as_ref().map(|_| uuid::Uuid::new_v4().to_string());
        let response_body = match &command_id {
            Some(command_id) => admission_body(
                journal,
                &RunResponse {
                    session_id: session_id.clone(),
                    command_id: command_id.clone(),
                },
                IdempotencyResource::Command(command_id),
            ),
            None => admission_body(
                journal,
                &SessionResponse {
                    session_id: session_id.clone(),
                },
                IdempotencyResource::Session(&session_id),
            ),
        };
        let response_body = match response_body {
            Ok(body) => body,
            Err(error) => return Err(error),
        };

        // Persist the session row with setup_status='initializing' BEFORE spawning
        // the setup task. If the server restarts mid-setup we want the cleanup
        // pass to find this session as non-terminal even if no setup_state.json
        // was written yet.
        let workdir = plan.resolved_workdir.to_string_lossy().into_owned();
        let cloned_path = plan
            .cloned_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let row = NewSessionRow {
            id: &session_id,
            workdir: &workdir,
            created_at: &created_at,
            setup_status: SessionSetupStatus::Initializing,
            kind: plan.kind,
            cloned_path: cloned_path.as_deref(),
        };
        let inserted = match journal {
            Some(journal) => self.store.insert_session_for_admission(
                row,
                journal.scope(),
                &journal.key,
                journal.owner(),
            ),
            None => self.store.insert_session_full(row),
        };
        if let Err(e) = inserted {
            tracing::error!(error = %e, "Failed to insert session");
            return Err(storage("Failed to create session", e));
        }
        // Record the remote inputs before setup starts, so a restored session
        // keeps them (and the push stage can read `push` from the row).
        if plan.kind == SessionKind::Remote {
            if let Err(e) = self.store.set_remote_fields(
                &session_id,
                RemoteSessionFields {
                    repo_url: plan.repo_url.as_deref(),
                    branch: plan.branch.as_deref(),
                    base_ref: plan.base_ref.as_deref(),
                    merge_refs: &plan.merge_refs,
                    push: plan.push,
                },
            ) {
                tracing::error!(error = %e, "Failed to record remote session fields");
                self.compensate_session(&session_id, journal).await;
                return Err(storage("Failed to create session", e));
            }
        }

        if let Err(error) =
            self.store
                .set_labels(LabelOwner::Session, &session_id, &submission.labels)
        {
            tracing::error!(%error, "Failed to store session labels");
            self.compensate_session(&session_id, journal).await;
            return Err(storage("Failed to store session labels", error));
        }
        let run = if let (Some(command), Some(command_id)) = (command, command_id) {
            // The run link precedes the command row: the row is the admission
            // point, and setup/bootstrap always find the link for it.
            let run = RunRecord {
                session_id: session_id.clone(),
                command_id,
                close_on_finish,
                created_at,
            };
            if let Err(error) = self.lifecycle_store.record_run(&run) {
                tracing::error!(%error, "Failed to record run");
                self.compensate_session(&session_id, journal).await;
                return Err(storage("Failed to record run", error));
            }
            if let Err(error) = self.enqueue_command(&run.command_id, &session_id, &command) {
                self.compensate_session(&session_id, journal).await;
                return Err(error);
            }
            Some(run)
        } else {
            None
        };

        // Only fully populated resources may recover an accepted response.
        if let Some(journal) = journal {
            let resource = match &run {
                Some(run) => IdempotencyResource::Command(&run.command_id),
                None => IdempotencyResource::Session(&session_id),
            };
            if let Err(error) = journal.record(&response_body, resource) {
                self.compensate_session(&session_id, Some(journal)).await;
                return Err(error);
            }
        }
        Ok(AdmittedSession {
            session_id,
            plan,
            run,
            response_body,
        })
    }

    async fn compensate_session(&self, session_id: &str, journal: Option<&IdempotentAdmission>) {
        if let Some(journal) = journal {
            if let Err(error) = journal.compensating_session(session_id) {
                tracing::error!(%error, session_id, "Failed to void admission intent before compensation");
                // The insert transaction already established the barrier.
                // Still attempt closure if this refresh fails.
            }
        }
        match self.lifecycle().close(session_id).await {
            Ok(super::CloseOutcome::Closed | super::CloseOutcome::NotFound) => {}
            Ok(outcome) => tracing::error!(
                ?outcome,
                session_id,
                "Admission compensation is still pending"
            ),
            Err(error) => {
                tracing::error!(%error, session_id, "Failed to compensate session admission")
            }
        }
    }

    pub async fn admit_command(
        &self,
        session_id: &str,
        submission: CommandSubmission,
        journal: Option<&IdempotentAdmission>,
    ) -> Result<AdmittedCommand, AdmissionError> {
        let parsed = submission.validate()?;
        match self
            .store
            .command_admission(session_id)
            .map_err(|e| storage("Failed to validate session", e))?
        {
            SessionCommandAdmission::Accepted => {}
            other => return Err(AdmissionError::Session(other)),
        }
        let readiness = self.lifecycle().setup_readiness(session_id).await;
        if !readiness.is_ready() {
            return Err(AdmissionError::Setup(readiness));
        }
        let path_parts: Vec<&str> = submission.subcommand.split_whitespace().collect();
        let command_id = uuid::Uuid::new_v4().to_string();
        let mut flags_applied = serde_json::Value::Object(
            CommandCatalogue::get()
                .frontend_profile(FrontendKind::Api, &path_parts)
                .into_iter()
                .map(|(flag, value)| (flag.replace('-', "_"), flag_default_json(&value)))
                .collect(),
        );
        if let Some(session) = self.sessions.get_by_key(session_id) {
            let dispatch = Dispatch::new(AdmissionInput(parsed), session, self.engines.clone());
            if let Some(policy) = dispatch
                .prerequisite_admission(&path_parts)
                .map_err(AdmissionError::CommandAdmission)?
            {
                flags_applied["prerequisites"] = policy;
            }
        }
        let response_body = admission_body(
            journal,
            &CommandResponse {
                command_id: command_id.clone(),
                flags_applied: flags_applied.clone(),
            },
            IdempotencyResource::Command(&command_id),
        )?;
        self.enqueue_command(&command_id, session_id, &submission)?;
        Ok(AdmittedCommand {
            command_id,
            flags_applied,
            response_body,
        })
    }

    fn enqueue_command(
        &self,
        command_id: &str,
        session_id: &str,
        body: &CommandSubmission,
    ) -> Result<(), AdmissionError> {
        let args_json = serde_json::to_string(&body.args).unwrap_or_else(|_| "[]".to_string());

        if let Err(e) = self.paths.prepare_command_dir(session_id, command_id) {
            tracing::error!(error = %e, "Failed to create command directory");
            return Err(storage("Failed to create command directory", e));
        }

        let log_path = self.paths.command_log_path(session_id, command_id);

        if let Err(e) = self.store.enqueue_command_with_labels(
            command_id,
            session_id,
            &body.subcommand,
            &args_json,
            &log_path.to_string_lossy(),
            &body.labels,
        ) {
            tracing::error!(error = %e, "Failed to enqueue command");
            return Err(storage("Failed to enqueue command", e));
        }

        tracing::info!(
            command_id = %command_id,
            session_id = %session_id,
            subcommand = %body.subcommand,
            "Command enqueued"
        );

        Ok(())
    }
}

#[derive(Serialize)]
struct SessionResponse {
    session_id: String,
}

#[derive(Serialize)]
struct CommandResponse {
    command_id: String,
    /// Server-enforced flags whose values the API frontend always overrides.
    /// Documents to clients that `yolo` and `non_interactive` are forced to
    /// `true` regardless of any value sent in the request body. Empty object
    /// for non-exec routes.
    flags_applied: serde_json::Value,
}

#[derive(Serialize)]
struct RunResponse {
    session_id: String,
    command_id: String,
}

/// One `FlagDefault` as the JSON a client sees in `flags_applied`.
///
/// Rendering only — the values themselves come from the catalogue's frontend
/// profile (WI 0114 F-50).
fn flag_default_json(
    value: &crate::command::dispatch::catalogue::FlagDefault,
) -> serde_json::Value {
    use crate::command::dispatch::catalogue::FlagDefault;
    match value {
        FlagDefault::Bool(b) => serde_json::Value::Bool(*b),
        FlagDefault::Str(s) => serde_json::Value::String((*s).to_string()),
        FlagDefault::U16(n) => serde_json::Value::from(*n),
        FlagDefault::EmptyVec => serde_json::Value::Array(Vec::new()),
        FlagDefault::None => serde_json::Value::Null,
    }
}

/// Only the catalogue accessors used by Dispatch's admission policy. It never
/// executes a command or constructs an execution frontend.
struct AdmissionInput(ParsedArgs);
impl UserMessageSink for AdmissionInput {
    fn write_message(&mut self, _message: UserMessage) {}
    fn replay_queued(&mut self) {}
}
impl CommandFrontend for AdmissionInput {
    fn kind(&self) -> FrontendKind {
        FrontendKind::Api
    }
    fn flag_bool(&self, _path: &[&str], name: &str) -> Result<Option<bool>, CommandError> {
        Ok(self.0.flag_bool(name))
    }
    fn flag_string(&self, _path: &[&str], name: &str) -> Result<Option<String>, CommandError> {
        Ok(self.0.flag_string(name))
    }
    fn flag_strings(&self, _path: &[&str], name: &str) -> Result<Vec<String>, CommandError> {
        Ok(self.0.flag_strings(name))
    }
    fn flag_path(&self, _path: &[&str], name: &str) -> Result<Option<PathBuf>, CommandError> {
        Ok(self.0.flag_path(name))
    }
    fn flag_enum(&self, _path: &[&str], name: &str) -> Result<Option<String>, CommandError> {
        Ok(self.0.flag_enum(name))
    }
    fn flag_u16(&self, _path: &[&str], name: &str) -> Result<Option<u16>, CommandError> {
        Ok(self.0.flag_u16(name))
    }
    fn flag_usize(&self, _path: &[&str], name: &str) -> Result<Option<usize>, CommandError> {
        Ok(self.0.flag_usize(name))
    }
    fn argument(&self, _path: &[&str], name: &str) -> Result<Option<String>, CommandError> {
        Ok(self.0.argument(name))
    }
    fn arguments(&self, _path: &[&str], name: &str) -> Result<Vec<String>, CommandError> {
        Ok(self.0.arguments(name))
    }
}

/// One id per daemon process. A different instance can recover a dead
/// process's intent, but cannot take over another live request here.
fn daemon_instance() -> &'static str {
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

/// Owns a request's reservation, including release when its future is dropped
/// or panics. Only a Fresh begin activates the guard; replay and mismatch
/// must never release another request's reservation.
pub struct IdempotentAdmission {
    store: Arc<SqliteSessionStore>,
    route: &'static str,
    principal: String,
    key: String,
    request: String,
    active: AtomicBool,
}

impl IdempotentAdmission {
    pub fn new(
        store: Arc<SqliteSessionStore>,
        route: &'static str,
        principal: &str,
        key: &str,
    ) -> Self {
        Self {
            store,
            route,
            principal: principal.into(),
            key: key.into(),
            request: uuid::Uuid::new_v4().to_string(),
            active: AtomicBool::new(false),
        }
    }

    fn scope(&self) -> IdempotencyScope<'_> {
        IdempotencyScope {
            route: self.route,
            principal: &self.principal,
        }
    }

    fn owner(&self) -> IdempotencyOwner<'_> {
        IdempotencyOwner {
            instance: daemon_instance(),
            request: &self.request,
        }
    }

    pub fn begin(&self, canonical: &str) -> Result<IdempotencyBegin, DataError> {
        let hash: String = Sha256::digest(canonical.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let outcome = self.store.idempotency_begin(
            self.scope(),
            &self.key,
            &hash,
            &chrono::Utc::now().to_rfc3339(),
            self.owner(),
        )?;
        if matches!(outcome, IdempotencyBegin::Fresh) {
            self.active.store(true, Ordering::Relaxed);
        }
        Ok(outcome)
    }

    /// Record the accepted response before creating the resource that makes
    /// admission durable. Its existence lets a retry replay after a crash.
    pub fn record(
        &self,
        body: &str,
        resource: IdempotencyResource<'_>,
    ) -> Result<(), AdmissionError> {
        match self.store.idempotency_record_intent(
            self.scope(),
            &self.key,
            self.owner(),
            202,
            body,
            resource,
        ) {
            Ok(true) => Ok(()),
            Ok(false) => Err(AdmissionError::ReservationLost),
            Err(e) => Err(storage("Failed to record idempotent admission", e)),
        }
    }

    fn compensating_session(&self, session_id: &str) -> Result<(), DataError> {
        if !self.store.idempotency_compensating_session(
            self.scope(),
            &self.key,
            self.owner(),
            session_id,
        )? {
            return Err(DataError::Other(
                "Idempotency reservation lost before compensation".into(),
            ));
        }
        Ok(())
    }

    /// Store the transport's final status/body, including rejected requests.
    /// A failed write keeps Drop armed so admitted work remains replayable.
    pub fn complete(&self, status: u16, body: &str) -> Result<bool, DataError> {
        let completed =
            self.store
                .idempotency_complete(self.scope(), &self.key, self.owner(), status, body)?;
        self.active.store(false, Ordering::Relaxed);
        Ok(completed)
    }

    /// Release a request whose admission failed and was compensated.
    pub fn release(&self) -> Result<(), DataError> {
        self.active.store(false, Ordering::Relaxed);
        self.store
            .idempotency_release(self.scope(), &self.key, self.owner())
    }
}

impl Drop for IdempotentAdmission {
    fn drop(&mut self) {
        if self.active.load(Ordering::Relaxed) {
            if let Err(error) = self.store.idempotency_release_unless_admitted(
                self.scope(),
                &self.key,
                self.owner(),
            ) {
                tracing::error!(%error, "Failed to release idempotency reservation");
            }
        }
    }
}

fn admission_body(
    journal: Option<&IdempotentAdmission>,
    body: &impl Serialize,
    resource: IdempotencyResource<'_>,
) -> Result<String, AdmissionError> {
    let body = serde_json::to_string(body)
        .map_err(|e| storage("Failed to encode response", DataError::Other(e.to_string())))?;
    if let Some(journal) = journal {
        journal.record(&body, resource)?;
    }
    Ok(body)
}

#[cfg(test)]
mod tests;
