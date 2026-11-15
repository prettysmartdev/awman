//! API session/command admission and crash-safe idempotency ownership.
//!
//! Provisional sessions have a transactional rollback barrier until all admission
//! writes finish. Accepted intents recover fully admitted sessions/runs; command-
//! only admission records its intent before the command row.

use super::{ApiSessionLifecycle, SetupReadiness};
use crate::command::dispatch::catalogue::{CommandCatalogue, FrontendKind};
use crate::command::dispatch::ephemeral_secrets::SessionGitCredential;
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
    pub secrets: Option<Arc<crate::data::secret::CommandSecrets>>,
    pub allow_remote_input: bool,
    pub interaction: Option<crate::data::config::daemon::Interaction>,
    pub subcommand: String,
    pub args: Vec<String>,
    pub labels: Labels,
    /// Inline work item (WI 0147); validated here and staged privately when
    /// the command is enqueued.
    pub work_item: Option<crate::data::work_item::InlineWorkItemInput>,
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
    FeatureDisabled,
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
    /// The request chose an interaction mode the push feature does not let
    /// it choose (`allowOverride: false`). `400 interaction_override_refused`.
    InteractionRefused(crate::command::interaction::InteractionRefused),
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
        Self::canonicalize_secrets(&mut value);
        format!("{value}\n{session}")
    }

    /// Names participate in idempotency, values (and value hashes) never do.
    pub fn canonicalize_secrets(value: &mut serde_json::Value) {
        if let Some(body) = value.as_object_mut() {
            if let Some(secrets) = body.get_mut("secrets") {
                if let Some(map) = secrets.as_object() {
                    let mut names: Vec<_> = map.keys().cloned().collect();
                    names.sort();
                    *secrets = serde_json::json!(names);
                }
            }
        }
    }

    pub(crate) fn validate(&self) -> Result<ParsedArgs, AdmissionError> {
        self.validate_for(FrontendKind::WORKER_LOCAL)
    }
    fn validate_for(&self, frontend: FrontendKind) -> Result<ParsedArgs, AdmissionError> {
        Labels::new(self.labels.clone().into_inner())
            .map_err(|e| AdmissionError::CommandValidation(CommandError::Other(e.to_string())))?;
        let catalogue = CommandCatalogue::get();
        let path: Vec<&str> = self.subcommand.split_whitespace().collect();
        if let Err(CommandError::NotAvailableForFrontend { command, .. }) =
            catalogue.validate_for_frontend(frontend, &path)
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
        let parsed = catalogue
            .parse_raw_args_with_profile(&path, &self.args, frontend)
            .map_err(AdmissionError::CommandValidation)?;
        self.validate_inline_work_item(&parsed, &catalogue.canonical_path(&path))?;
        Ok(parsed)
    }

    /// An inline work item is read-only input to a command that reads one. It
    /// replaces `--work-item`, never accompanies it. The parsed flag is
    /// checked, not argv text, because prompt text is data.
    fn validate_inline_work_item(
        &self,
        parsed: &ParsedArgs,
        canonical: &[&str],
    ) -> Result<(), AdmissionError> {
        let Some(work_item) = &self.work_item else {
            return Ok(());
        };
        let reject =
            |reason: String| AdmissionError::CommandValidation(CommandError::Other(reason));
        if !matches!(canonical, ["exec", "prompt" | "workflow"]) {
            return Err(reject(format!(
                "work_item is only accepted for exec prompt and exec workflow, not `{}`",
                self.subcommand
            )));
        }
        if parsed.has_flag("work-item") {
            return Err(reject(
                "work_item and --work-item are mutually exclusive: send one work item source"
                    .into(),
            ));
        }
        work_item.validate().map_err(reject)
    }
}

/// Owns admission validation, durable write order and failure compensation.
#[derive(Clone)]
pub struct ApiAdmission {
    request_context: crate::data::daemon_state::CommandRequestContext,
    store: Arc<SqliteSessionStore>,
    lifecycle_store: CommandLifecycleStore,
    paths: ApiPaths,
    engines: Engines,
    sessions: Arc<SessionManager>,
    workdirs: Vec<PathBuf>,
    workdir_roots: Vec<PathBuf>,
}

impl ApiAdmission {
    fn frontend_kind(&self) -> FrontendKind {
        FrontendKind::Daemon {
            feature: crate::data::daemon_state::FeatureName::Worker,
            listener: self.request_context.listener,
        }
    }
    /// Set transport metadata once on the admission object. Keeping it separate
    /// from the wire body prevents callers from spoofing their source listener.
    pub fn with_request_context(
        mut self,
        context: crate::data::daemon_state::CommandRequestContext,
    ) -> Self {
        self.request_context = context;
        self
    }

    /// Resolve push interaction once, at submit (WI 0141).
    ///
    /// The request asks through the body's `interaction` or an explicit
    /// `--interactive` / `--headless[=bool]` in `args`; the two must agree.
    /// `command::interaction::resolve_interaction` applies
    /// `daemon.features.worker.push.{interaction,allowOverride}` as they are
    /// now (admission is built from the live config), and a disallowed
    /// override is refused here with `interaction_override_refused` rather
    /// than downgraded later. The resolved choice is recorded in the stored
    /// args as one leading `--headless` / `--interactive` token when the
    /// client did not spell it, so the queue worker, a restart re-queue and
    /// the read projection all see the same decision without a schema change.
    fn resolve_interaction(
        &self,
        mut command: CommandSubmission,
    ) -> Result<CommandSubmission, AdmissionError> {
        use crate::command::interaction::{
            requested_from_flags, resolve_interaction, InteractionSlot,
        };
        use crate::data::config::daemon::{DaemonSettings, Interaction};
        let settings = DaemonSettings::from_merged(
            &self
                .engines
                .global_config
                .daemon
                .clone()
                .unwrap_or_default(),
        );
        let catalogue = CommandCatalogue::get();
        let path: Vec<&str> = command.subcommand.split_whitespace().collect();
        let spec = catalogue.lookup_with_aliases(&path);
        let supports_headless = spec.is_some_and(|s| s.find_flag("headless").is_some());
        let supports_interactive = spec.is_some_and(|s| s.find_flag("interactive").is_some());
        // Explicit flags only (no profile defaults). A malformed argv is
        // reported by validation right after this, so it asks for nothing.
        let from_args = catalogue
            .parse_raw_args(&path, &command.args)
            .ok()
            .and_then(|parsed| {
                requested_from_flags(
                    parsed.flag_bool("interactive"),
                    parsed.flag_bool("headless"),
                )
            });
        let requested = match (command.interaction, from_args) {
            (Some(body), Some(args)) if body != args => {
                return Err(AdmissionError::CommandValidation(
                    CommandError::InvalidFlagValue {
                        command: path.iter().map(|p| (*p).to_string()).collect(),
                        flag: if args == Interaction::Interactive {
                            "interactive".into()
                        } else {
                            "headless".into()
                        },
                        reason: format!(
                            "contradicts the request's \"interaction\": \"{}\"",
                            body.as_str()
                        ),
                    },
                ))
            }
            (body, args) => body.or(args),
        };
        let decision = resolve_interaction(InteractionSlot::WorkerPush, requested, &settings)
            .map_err(AdmissionError::InteractionRefused)?;
        if supports_headless && from_args.is_none() {
            let token = match decision.interaction {
                Interaction::Headless => "--headless",
                Interaction::Interactive if supports_interactive => "--interactive",
                Interaction::Interactive => "--headless=false",
            };
            command.args.insert(0, token.into());
        }
        command.interaction = Some(decision.interaction);
        Ok(command)
    }

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
            request_context: Default::default(),
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
        let command = command.map(|c| self.resolve_interaction(c)).transpose()?;
        let session_id = crate::data::ids::WorkerSessionId::new().to_string();
        let created_at = crate::data::wire::format_timestamp(chrono::Utc::now());
        let session_dir = self.paths.session_dir(&session_id);

        // Validate and plan the session in Layer 2. The route only maps the typed
        // result/error to an HTTP status + JSON envelope — no business logic here.
        let policy = SessionCreatePolicy::new(self.workdirs.clone(), session_dir.clone())
            .with_workdir_roots(self.workdir_roots.clone());
        let plan = submission
            .request
            .validate(&policy, self.engines.git_engine.as_ref())
            .map_err(AdmissionError::SessionCreation)?;
        // WI 0145: the caller's credential (scope = this repository) is held
        // in memory only; the merge-ref advertisement already uses it.
        let git_credential = match (&submission.request.git_auth, plan.repo_url.as_deref()) {
            (Some(spec), Some(url)) => Some(Arc::new(SessionGitCredential {
                spec: Arc::clone(spec),
                scope: crate::command::session_git_transport::git_auth_scope(url)
                    .map_err(AdmissionError::SessionCreation)?,
            })),
            _ => None,
        };
        let validation_transport = if plan.merge_refs.is_empty() {
            crate::engine::git::transport::HostGitTransport::user(
                crate::engine::git::transport::CredentialSource::Ambient,
            )
        } else {
            crate::command::session_git_transport::transport_for_credential(
                &self.engines.ephemeral_secrets,
                &crate::command::session_git_transport::credential_socket_dir(&self.engines),
                git_credential.as_ref(),
            )
            .map_err(AdmissionError::SessionCreation)?
        };
        plan.validate_merge_ref_kinds(Arc::clone(&self.engines.git_engine), validation_transport)
            .await
            .map_err(AdmissionError::SessionCreation)?;
        if let Some(command) = &command {
            command.validate_for(self.frontend_kind())?;
        }

        // Create session storage directories (Layer 0).
        if let Err(e) = self.paths.prepare_session_dirs(&session_id) {
            tracing::error!(error = %e, "Failed to create session directory");
            return Err(storage("Failed to create session directory", e));
        }

        // Fix the response before durable writes. The provisional insertion
        // replaces this intent with a rollback barrier; only complete admission
        // restores it for crash recovery from the session or command resource.
        let command_id = command
            .as_ref()
            .map(|_| crate::data::ids::CommandId::new().to_string());
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
                &{
                    let mut body = serde_json::json!({"id":session_id,"workdir":plan.resolved_workdir,"created_at":created_at,"status":"active","setup_status":"initializing","session_type":plan.kind.as_str(),"repo_url":plan.repo_url,"branch":plan.branch,"base_ref":plan.base_ref,"merge_refs":plan.merge_refs,"push":plan.push,"labels":submission.labels});
                    if let Some(key) = &plan.context_key {
                        body["context_key"] = serde_json::json!(key.as_str());
                    }
                    // The kind only: the response never echoes a credential.
                    if let Some(credential) = &git_credential {
                        body["git_auth"] =
                            serde_json::json!({"kind": credential.spec.kind().as_str()});
                    }
                    body
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
        self.store
            .set_context_key(&session_id, plan.context_key.as_ref())
            .map_err(|e| storage("Failed to store context key", e))?;
        if let Some(credential) = &git_credential {
            self.engines.ephemeral_secrets.insert_session_git_auth(
                &session_id,
                SessionGitCredential {
                    spec: Arc::clone(&credential.spec),
                    scope: credential.scope.clone(),
                },
            );
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
                    git_auth_kind: git_credential.as_ref().map(|c| c.spec.kind().as_str()),
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
                session_id: crate::data::ids::WorkerSessionId::parse(&session_id)
                    .expect("generated session id"),
                command_id: crate::data::ids::CommandId::parse(&command_id)
                    .expect("generated command id"),
                close_on_finish,
                created_at,
            };
            if let Err(error) = self.lifecycle_store.record_run(&run) {
                tracing::error!(%error, "Failed to record run");
                self.compensate_session(&session_id, journal).await;
                return Err(storage("Failed to record run", error));
            }
            if let Err(error) = self.enqueue_command(run.command_id.as_str(), &session_id, &command)
            {
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
                Some(run) => IdempotencyResource::Command(run.command_id.as_str()),
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
        self.engines
            .ephemeral_secrets
            .remove_session_git_auth(session_id);
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
        // Compensation can cancel an already-enqueued run before a worker
        // ever owns it. Release those values as part of admission rollback.
        if let Ok(Some(run)) = crate::data::ids::WorkerSessionId::parse(session_id)
            .map_err(|e| e.to_string())
            .and_then(|id| {
                self.lifecycle_store
                    .run_for_session(&id)
                    .map_err(|e| e.to_string())
            })
        {
            self.engines.ephemeral_secrets.remove(&run.command_id);
        }
    }

    pub async fn admit_command(
        &self,
        session_id: &str,
        submission: CommandSubmission,
        journal: Option<&IdempotentAdmission>,
    ) -> Result<AdmittedCommand, AdmissionError> {
        let submission = self.resolve_interaction(submission)?;
        let parsed = submission.validate_for(self.frontend_kind())?;
        match self
            .store
            .command_admission(session_id)
            .map_err(|e| storage("Failed to validate session", e))?
        {
            SessionCommandAdmission::Accepted => {}
            other => return Err(AdmissionError::Session(other)),
        }
        // WI 0145: a session whose in-memory Git credential was lost (daemon
        // restart) cannot publish; refuse before anything is queued.
        if let Some(session) = self
            .store
            .get_session(session_id)
            .map_err(|e| storage("Failed to validate session", e))?
        {
            crate::command::session_git_transport::check_session_credentials(
                &self.engines,
                session_id,
                session.git_auth_kind.as_deref(),
            )
            .map_err(AdmissionError::CommandAdmission)?;
        }
        let readiness = self.lifecycle().setup_readiness(session_id).await;
        if !readiness.is_ready() {
            return Err(AdmissionError::Setup(readiness));
        }
        let path_parts: Vec<&str> = submission.subcommand.split_whitespace().collect();
        let command_id = crate::data::ids::CommandId::new().to_string();
        let mut flags_applied = serde_json::Value::Object(
            CommandCatalogue::get()
                .frontend_profile(self.frontend_kind(), &path_parts)
                .into_iter()
                .map(|(flag, value)| (flag.replace('-', "_"), flag_default_json(&value)))
                .collect(),
        );
        if flags_applied.get("headless").is_some() {
            flags_applied["headless"] = serde_json::json!(
                submission.interaction
                    != Some(crate::data::config::daemon::Interaction::Interactive)
            );
        }
        if let Some(session) = self.sessions.get_by_key(session_id) {
            let dispatch = Dispatch::new(
                AdmissionInput(parsed, submission.interaction, self.frontend_kind()),
                session,
                self.engines.clone(),
            );
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
                session_id: session_id.to_string(),
                status: "queued",
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

        let id = crate::data::ids::CommandId::parse(command_id).expect("generated command id");
        // Stage an inline work item privately before the row exists. The
        // queued command id is the durable anchor that finds it again.
        let inline_store = super::inline_work_item::InlineWorkItemStore::new(self.paths.clone());
        if let Some(work_item) = &body.work_item {
            inline_store
                .stage(session_id, command_id, work_item)
                .map_err(|e| storage("Failed to stage the inline work item", e))?;
        }
        if let Some(secrets) = body.secrets.as_ref().filter(|s| !s.is_empty()) {
            self.lifecycle_store
                .record_secret_names(&id, &secrets.names().collect::<Vec<_>>())
                .map_err(|e| {
                    inline_store.discard(session_id, command_id);
                    storage("Failed to record command secret names", e)
                })?;
            self.engines
                .ephemeral_secrets
                .insert(id.clone(), secrets.clone());
        }

        if let Err(e) = self.store.enqueue_command_with_terminal_input(
            command_id,
            session_id,
            &body.subcommand,
            &args_json,
            &log_path.to_string_lossy(),
            &body.labels,
            &self.request_context,
            body.allow_remote_input,
        ) {
            inline_store.discard(session_id, command_id);
            self.engines.ephemeral_secrets.remove(&id);
            let _ = self.lifecycle_store.remove_secret_names(&id);
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
struct CommandResponse {
    session_id: String,
    status: &'static str,
    #[serde(rename = "id")]
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
struct AdmissionInput(
    ParsedArgs,
    Option<crate::data::config::daemon::Interaction>,
    FrontendKind,
);
impl UserMessageSink for AdmissionInput {
    fn write_message(&mut self, _message: UserMessage) {}
    fn replay_queued(&mut self) {}
}
impl CommandFrontend for AdmissionInput {
    fn input_available(&self) -> bool {
        self.1 == Some(crate::data::config::daemon::Interaction::Interactive)
    }
    fn kind(&self) -> FrontendKind {
        self.2
    }
    fn flag_bool(&self, _path: &[&str], name: &str) -> Result<Option<bool>, CommandError> {
        if name == "headless" {
            if let Some(mode) = self.1 {
                return Ok(Some(
                    mode == crate::data::config::daemon::Interaction::Headless,
                ));
            }
        }
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
    INSTANCE.get_or_init(|| crate::data::ids::CommandId::new().to_string())
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
            request: crate::data::ids::CommandId::new().to_string(),
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
            if matches!(resource, IdempotencyResource::Session(_)) {
                201
            } else {
                202
            },
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

/// The first step of every idempotent API write: whether the request runs,
/// and with which reservation, or what the host answers instead.
///
/// A host (HTTP today) reads the raw key and canonical request, calls
/// [`IdempotencyGate::open`], and maps the result to its own status codes.
/// The reservation state machine — key policy, principal scoping, replay,
/// in-flight and mismatch decisions, release on failure or drop, and storing
/// the final response — stays here, so a second host cannot sequence it
/// differently.
pub enum IdempotencyGate {
    /// No key was sent: run the handler without a journal.
    Unkeyed,
    /// This request now holds the key: run the handler with
    /// [`IdempotentRun::admission`], then report its result through
    /// [`IdempotentRun::finish`]. Dropping the run (cancelled or panicking
    /// handler) releases the key unless admission already became durable.
    Fresh(IdempotentRun),
    /// An earlier request with this key and body finished: answer with its
    /// stored status and body verbatim.
    Replay { status: u16, body: String },
    /// Another request with this key still holds the reservation.
    InFlight,
    /// The key was already used with a different request body.
    Mismatch,
}

/// Why [`IdempotencyGate::open`] could not decide.
#[derive(Debug)]
pub enum IdempotencyGateError {
    /// The key is empty, too long, or not visible ASCII.
    InvalidKey,
    /// The reservation could not be read or written.
    Reserve(DataError),
}

impl IdempotencyGate {
    /// Validate `key` and reserve it for `route` under the caller's
    /// principal (the API key hash, or one shared anonymous principal when
    /// auth is disabled), comparing `canonical` with any earlier request.
    pub fn open(
        store: &Arc<SqliteSessionStore>,
        route: &'static str,
        principal: &str,
        key: Option<&str>,
        canonical: &str,
    ) -> Result<Self, IdempotencyGateError> {
        let Some(key) = key else {
            return Ok(Self::Unkeyed);
        };
        if !is_valid_idempotency_key(key) {
            return Err(IdempotencyGateError::InvalidKey);
        }
        let journal = Arc::new(IdempotentAdmission::new(
            Arc::clone(store),
            route,
            principal,
            key,
        ));
        Ok(
            match journal
                .begin(canonical)
                .map_err(IdempotencyGateError::Reserve)?
            {
                IdempotencyBegin::Fresh => Self::Fresh(IdempotentRun { journal }),
                IdempotencyBegin::Replay { status, body } => Self::Replay { status, body },
                IdempotencyBegin::InFlight => Self::InFlight,
                IdempotencyBegin::Mismatch => Self::Mismatch,
            },
        )
    }
}

/// An `Idempotency-Key` is 1 to [`IDEMPOTENCY_KEY_MAX_BYTES`] bytes of
/// visible ASCII (no spaces or control characters).
///
/// [`IDEMPOTENCY_KEY_MAX_BYTES`]: crate::data::fs::api_db::IDEMPOTENCY_KEY_MAX_BYTES
fn is_valid_idempotency_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= crate::data::fs::api_db::IDEMPOTENCY_KEY_MAX_BYTES
        && key.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// A request that holds its key's reservation (see [`IdempotencyGate::Fresh`]).
pub struct IdempotentRun {
    journal: Arc<IdempotentAdmission>,
}

/// How a reserved request ended, as [`IdempotentRun::finish`] records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdempotentResult<'a> {
    /// A server-side failure: release the key so a retry runs again.
    /// Unfinished admission compensation keeps its own barrier.
    ServerError,
    /// Any other final answer, stored so a retry replays it.
    Final { status: u16, body: &'a str },
}

impl IdempotentRun {
    /// The journal admission writes record their intent through.
    pub fn admission(&self) -> Arc<IdempotentAdmission> {
        Arc::clone(&self.journal)
    }

    /// Record how the request ended. A storage failure here is logged, not
    /// returned: the work and its answer already happened, and a failed
    /// completion keeps the drop guard armed so admitted work stays
    /// replayable.
    pub fn finish(self, result: IdempotentResult<'_>) {
        match result {
            IdempotentResult::ServerError => {
                if let Err(error) = self.journal.release() {
                    tracing::error!(%error, "Failed to release idempotency reservation");
                }
            }
            IdempotentResult::Final { status, body } => match self.journal.complete(status, body) {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!("Idempotency reservation was no longer held when completing");
                }
                Err(error) => {
                    tracing::error!(%error, "Failed to save idempotent response");
                }
            },
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
