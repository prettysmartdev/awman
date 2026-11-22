//! Profile admission and closure policy. The OS lease spans a complete run;
//! durable ownership fences a surviving guest after an awman crash.
use crate::command::error::CommandError;
use crate::data::fs::agent_profiles::*;
use crate::data::fs::DataPaths;
use crate::data::ids::ExecutionId;
use crate::data::session::{AgentName, Session};
use crate::data::settings_profile::{NativeSessionId, ProfileState, SettingsProfileName};
use crate::engine::agent::profile_state::{ProfileRunState, ProfileStateSpec};
use crate::engine::agent::{AgentDefinition, AgentRunOptions};
use crate::engine::agent_runtime::AgentRuntimeEngine;
use crate::engine::error::EngineError;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const EXECUTION_LABEL: &str = "awman.profile-execution";
#[derive(Debug, Clone, Default)]
pub struct ProfileRequest {
    pub name: Option<SettingsProfileName>,
    pub agent_session: Option<NativeSessionId>,
}
impl ProfileRequest {
    pub fn parse(name: Option<&str>, id: Option<&str>) -> Result<Self, CommandError> {
        let invalid = |reason: String| CommandError::InvalidFlagValue {
            command: vec![],
            flag: "settings-profile/agent-session".into(),
            reason,
        };
        if id.is_some() && name.is_none() {
            return Err(invalid(
                "--agent-session requires --settings-profile".into(),
            ));
        }
        Ok(Self {
            name: name
                .map(SettingsProfileName::parse)
                .transpose()
                .map_err(|e| invalid(e.to_string()))?,
            agent_session: id
                .map(NativeSessionId::parse)
                .transpose()
                .map_err(|e| invalid(e.to_string()))?,
        })
    }
    pub fn from_context(
        ctx: &crate::command::dispatch::BuildContext<'_>,
    ) -> Result<Self, CommandError> {
        let id = crate::command::dispatch::catalogue::CommandCatalogue::get()
            .lookup(&ctx.path())
            .and_then(|s| s.find_flag("agent-session"))
            .and_then(|_| ctx.flags.str("agent-session"));
        let request = Self::parse(ctx.flags.str("settings-profile"), id)?;
        if request.name.is_some() {
            require_frontend(ctx.caller.frontend())?;
            if !ctx.caller.profile_owner() {
                return Err(CommandError::ProfileOwnerRequired);
            }
        }
        Ok(request)
    }
}
pub fn requires_local_owner(
    flags: &crate::command::dispatch::projections::raw_args::ParsedArgs,
) -> bool {
    ["settings-profile", "agent-session", "automation"]
        .iter()
        .any(|s| flags.has_flag(s))
}
pub fn require_frontend(
    kind: crate::command::dispatch::catalogue::FrontendKind,
) -> Result<(), CommandError> {
    use crate::command::dispatch::catalogue::FrontendKind;
    match kind {
        FrontendKind::Cli
        | FrontendKind::Tui
        | FrontendKind::Daemon {
            feature: crate::data::daemon_state::FeatureName::Worker,
            listener: crate::data::daemon_state::CommandListener::Local,
        } => Ok(()),
        _ => Err(CommandError::ProfileOwnerRequired),
    }
}
type CleanupCallback = Box<dyn FnOnce(bool) -> Result<(), CommandError> + Send>;
struct CleanupCompletion(Option<CleanupCallback>);
impl CleanupCompletion {
    fn finish(mut self, confirmed: bool) -> Result<(), CommandError> {
        self.0.take().expect("profile cleanup callback")(confirmed)
    }
}
impl Drop for CleanupCompletion {
    fn drop(&mut self) {
        if let Some(callback) = self.0.take() {
            if let Err(error) = callback(false) {
                tracing::error!(%error, "abandoned profile completion requires recovery");
            }
        }
    }
}
pub struct ProfileSession {
    completion: std::sync::Mutex<Option<CleanupCompletion>>,
    lease: ProfileLease,
    manifest: ProfileManifest,
    spec: ProfileStateSpec,
    redaction: Option<crate::engine::auth::redaction::RedactionSet>,
}
impl ProfileSession {
    pub(crate) fn on_cleanup(
        &self,
        callback: impl FnOnce(bool) -> Result<(), CommandError> + Send + 'static,
    ) {
        *self.completion.lock().expect("profile completion lock") =
            Some(CleanupCompletion(Some(Box::new(callback))));
    }

    /// Retain the launch's shared old/new values through every publication.
    pub fn bind_redaction(
        &mut self,
        redaction: crate::engine::auth::redaction::RedactionSet,
    ) -> Result<(), CommandError> {
        self.manifest
            .owner
            .as_mut()
            .ok_or_else(|| CommandError::SettingsProfileBusy("profile has no owner".into()))?
            .redaction_required = true;
        self.lease.write_manifest(&self.manifest)?;
        self.redaction = Some(redaction);
        Ok(())
    }
    pub fn execution_id(&self) -> &ExecutionId {
        &self
            .manifest
            .owner
            .as_ref()
            .expect("active profile owner")
            .execution_id
    }
    /// Bind the runtime's configured identity before any guest can start.
    pub fn bind_resource(&mut self, name: &str) -> Result<(), CommandError> {
        self.manifest
            .owner
            .as_mut()
            .expect("active profile owner")
            .resource_name = name.into();
        self.lease.write_manifest(&self.manifest)?;
        Ok(())
    }
    /// Immutable profile identity copied into the execution record.
    pub fn execution_binding(&self) -> crate::data::automation_records::ExecutionProfileBinding {
        crate::data::automation_records::ExecutionProfileBinding {
            repository_key: self.manifest.repository.key.clone(),
            root_sha256: self.manifest.repository.root_sha256.clone(),
            provider: self.manifest.provider.clone(),
            name: self.manifest.name.clone(),
        }
    }
    pub fn run_state(&self) -> ProfileRunState {
        ProfileRunState {
            run_dir: self.lease.run_dir(self.execution_id()),
            execution_id: self.execution_id().clone(),
        }
    }
    pub fn reject_native_identity(&mut self) -> Result<(), CommandError> {
        self.manifest
            .owner
            .as_mut()
            .expect("profile owner")
            .native_identity_rejected = true;
        self.lease.write_manifest(&self.manifest)?;
        Ok(())
    }
    pub fn observe_native_session(&mut self, id: NativeSessionId) -> Result<(), CommandError> {
        let owner = self.manifest.owner.as_mut().expect("active profile owner");
        if owner
            .agent_session
            .as_ref()
            .is_some_and(|expected| expected != &id)
        {
            owner.native_identity_rejected = true;
            self.lease.write_manifest(&self.manifest)?;
            return Err(CommandError::NativeSessionNotFound);
        }
        if !owner.observed_native_sessions.contains(&id) {
            owner.observed_native_sessions.push(id);
            self.lease.write_manifest(&self.manifest)?;
        }
        Ok(())
    }
    /// Closing consumes the session; a replacement cannot reactivate it later.
    /// ```compile_fail
    /// async fn closed(mut session: awman::command::commands::launch_policy::settings_profile::ProfileSession,
    ///     runtime: std::sync::Arc<dyn awman::engine::agent_runtime::AgentRuntimeEngine>,
    ///     proof: awman::command::commands::launch_policy::settings_profile::ProfileReplacementProof) {
    ///     session.leave(runtime, awman::data::fs::agent_profiles::TurnStateAtExit::Complete).await.unwrap();
    ///     session.replace_execution(proof, awman::data::ids::ExecutionId::new()).unwrap();
    /// }
    /// ```
    pub async fn leave(
        mut self,
        runtime: Arc<dyn AgentRuntimeEngine>,
        turn: TurnStateAtExit,
    ) -> Result<ProfileManifest, CommandError> {
        let completion = self
            .completion
            .get_mut()
            .expect("profile completion lock")
            .take();
        let result = self.leave_inner(runtime, turn).await;
        if let Some(completion) = completion {
            let confirmed = result.as_ref().is_ok_and(|m| {
                m.state.admits_writer()
                    && m.history_unavailable.as_deref() != Some("output_incomplete")
            });
            completion.finish(confirmed)?;
        }
        result
    }
    async fn leave_inner(
        mut self,
        runtime: Arc<dyn AgentRuntimeEngine>,
        turn: TurnStateAtExit,
    ) -> Result<ProfileManifest, CommandError> {
        // A refused replacement can already have published and closed the old
        // run. Closing the logical command must keep that completed cleanup.
        if self.manifest.state == ProfileState::Inactive && self.manifest.owner.is_none() {
            return Ok(self.manifest);
        }

        mark_closing(
            &self.lease,
            &mut self.manifest,
            CleanupReason::Publishing,
            None,
        )?;
        reconcile_inner(
            &self.lease,
            self.manifest,
            ReconcileMode::VerifyOnly,
            runtime,
            Some(self.spec.clone()),
            turn,
            self.redaction,
        )
        .await
    }
}
pub async fn enter(
    session: &Session,
    agent: &AgentName,
    request: &ProfileRequest,
    run: &AgentRunOptions,
    def: &AgentDefinition,
    runtime: Arc<dyn AgentRuntimeEngine>,
) -> Result<Option<ProfileSession>, CommandError> {
    if request.name.is_some() {
        reconcile_repository(session, runtime.clone()).await?;
    }
    let session = session.clone();
    let agent = agent.clone();
    let request = request.clone();
    let run = run.clone();
    let def = def.clone();
    tokio::task::spawn_blocking(move || enter_sync(&session, &agent, &request, &run, &def, runtime))
        .await
        .map_err(|e| CommandError::Other(format!("profile admission worker failed: {e}")))?
}
pub fn enter_sync(
    session: &Session,
    agent: &AgentName,
    request: &ProfileRequest,
    run: &AgentRunOptions,
    def: &AgentDefinition,
    runtime: Arc<dyn AgentRuntimeEngine>,
) -> Result<Option<ProfileSession>, CommandError> {
    let Some(name) = &request.name else {
        return Ok(None);
    };
    let spec = def
        .profile_state
        .as_ref()
        .ok_or_else(|| unsupported(agent, "agent declares no profile state layout"))?;
    if runtime.capabilities().kit_declarative || !runtime.capabilities().arbitrary_host_mounts {
        return Err(unsupported(
            agent,
            "runtime cannot mount private profile state",
        ));
    }
    let mut resume_run = run.clone();
    resume_run.native_session = request.agent_session.clone();
    if request.agent_session.is_some() {
        def.resume_selector(&resume_run)?;
        def.verify_resume_flags(
            &resume_run,
            &crate::engine::agent::resolve_agent_flags(session, agent, run),
        )?;
    }
    let repo = ProfileRepoBinding::from_session(session)?;
    let paths = DataPaths::from_env(session.env())?;
    let layout = ProfileLayout::new(&paths, &repo, agent, name);
    let profile_root = crate::data::fs::path_guard::canonicalize_lenient(layout.root());
    let workspace = crate::data::fs::path_guard::canonicalize_lenient(session.git_root());
    if profile_root.starts_with(&workspace) {
        return Err(unsupported(
            agent,
            "profile data home must be outside the mounted workspace",
        ));
    }
    if request.agent_session.is_some() && !layout.exists() {
        return Err(CommandError::NativeSessionNotFound);
    }
    let lease = layout.try_lock(Duration::ZERO)?.ok_or_else(|| {
        CommandError::SettingsProfileBusy("in use by another awman process".into())
    })?;
    let mut manifest = match lease.read_manifest()? {
        Some(m) => m,
        None => {
            if request.agent_session.is_some() {
                return Err(CommandError::NativeSessionNotFound);
            }
            let mut m = ProfileManifest::new(&repo, agent, name);
            m.state_layout = spec.layout_key();
            // Seed before creating the durable profile record: malformed deny
            // policy must never publish a partially initialized generation.
            let stage = lease.begin_staging()?;
            let snapshot = match spec.seed(
                session.env().home_dir().as_deref(),
                Some(paths.home()),
                &stage,
            ) {
                Ok(s) => s,
                Err(e) => {
                    lease.remove_uninitialized()?;
                    return Err(e.into());
                }
            };
            m.current_generation = 1;
            m.history_unavailable = snapshot.history_unavailable;
            m.last_publication = Some(ProfilePublication {
                generation: 1,
                at: now(),
                status: PublicationStatus::Published,
                reason: None,
            });
            lease.publish(stage, &m)?;
            m
        }
    };
    if manifest.state_layout.is_empty() {
        // Schema-1 profiles from before layout identity was recorded are
        // adopted only once, under their current selected definition.
        manifest.state_layout = spec.layout_key();
        lease.write_manifest(&manifest)?;
    } else if manifest.state_layout != spec.layout_key() {
        return Err(unsupported(
            agent,
            "profile was created for a different provider state layout; select or create a matching profile",
        ));
    }
    if manifest.native_sessions.len() >= MAX_NATIVE_SESSIONS {
        return Err(CommandError::Other(format!(
            "profile has reached its {MAX_NATIVE_SESSIONS} native-session record limit; delete this profile to start a fresh history"
        )));
    }
    if !manifest.state.admits_writer() {
        if manifest.state == ProfileState::Active {
            mark_closing(&lease, &mut manifest, CleanupReason::OwnerLost, None)?;
        }
        return Err(CommandError::SettingsProfileBusy(
            "previous execution cleanup is unconfirmed".into(),
        ));
    }
    if let Some(id) = &request.agent_session {
        if let Some(reason) = &manifest.history_unavailable {
            return Err(unsupported(agent, reason));
        }
        let record = manifest
            .native_sessions
            .iter()
            .find(|r| &r.id == id)
            .ok_or(CommandError::NativeSessionNotFound)?;
        if !spec
            .list_sessions(&lease.generation_tree(manifest.current_generation))?
            .contains(id)
        {
            return Err(CommandError::NativeSessionNotFound);
        }
        if run.initial_prompt.is_none()
            && !matches!(record.turn_state, TurnStateAtExit::Complete)
            && !def.resume_selector(&resume_run)?.no_auto_continue
        {
            return Err(unsupported(
                agent,
                "interrupted conversation requires an explicit follow-up task",
            ));
        }
    }
    let timeouts = session.effective_config().automation_timeouts()?;
    let id = ExecutionId::new();
    manifest.state = ProfileState::Active;
    manifest.owner = Some(ProfileOwner {
        run_initialized: false,
        native_identity_rejected: false,
        redaction_required: false,
        execution_id: id.clone(),
        session_id: session.id().to_string(),
        command_id: None,
        runtime: runtime.runtime_name().into(),
        resource_name: format!("awman-profile-{}", id.as_str()),
        resource_label: format!("{EXECUTION_LABEL}={}", id.as_str()),
        owner_pid: std::process::id(),
        started_at: now(),
        cancel_timeout: timeouts.cancel.as_str().into(),
        output_drain_timeout: timeouts.output_drain.as_str().into(),
        base_generation: manifest.current_generation,
        agent_session: request.agent_session.clone(),
        observed_native_sessions: vec![],
    });
    manifest.cleanup = Some(ProfileCleanup {
        reason: CleanupReason::Running,
        since: now(),
        last_attempt: None,
        last_error: None,
    });
    lease.write_manifest(&manifest)?;
    let result = (|| {
        let dir = lease.create_run_dir(&id)?;
        spec.import(&lease.generation_tree(manifest.current_generation), &dir)
    })();
    if let Err(e) = result {
        mark_closing(
            &lease,
            &mut manifest,
            CleanupReason::SnapshotIo,
            Some(e.to_string()),
        )?;
        return Err(e.into());
    }
    manifest
        .owner
        .as_mut()
        .expect("active profile owner")
        .run_initialized = true;
    lease.write_manifest(&manifest)?;
    Ok(Some(ProfileSession {
        lease,
        manifest,
        spec: spec.clone(),
        redaction: None,
        completion: std::sync::Mutex::new(None),
    }))
}
fn unsupported(agent: &AgentName, reason: &str) -> CommandError {
    EngineError::NativeResumeUnsupported {
        agent: agent.as_str().into(),
        reason: reason.into(),
    }
    .into()
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReconcileMode {
    VerifyOnly,
    StopOwned,
}
fn mark_closing(
    lease: &ProfileLease,
    m: &mut ProfileManifest,
    reason: CleanupReason,
    error: Option<String>,
) -> Result<(), CommandError> {
    let preserve_diagnostic = m.state == ProfileState::Closing && m.cleanup.is_some();
    m.state = ProfileState::Closing;
    if preserve_diagnostic {
        if let Some(cleanup) = &mut m.cleanup {
            cleanup.last_attempt = Some(now());
        }
    } else {
        m.cleanup = Some(ProfileCleanup {
            reason,
            since: now(),
            last_attempt: Some(now()),
            last_error: error,
        });
    }
    lease.write_manifest(m)?;
    Ok(())
}
fn inactive(m: &mut ProfileManifest) {
    m.state = ProfileState::Inactive;
    m.owner = None;
    m.cleanup = None;
}
/// Reconciliation is bounded by the saved cancellation budget. A timed-out
/// runtime operation may still finish, but durable closing state fences entry.
pub async fn reconcile(
    lease: &ProfileLease,
    m: ProfileManifest,
    mode: ReconcileMode,
    runtime: Arc<dyn AgentRuntimeEngine>,
    spec: &ProfileStateSpec,
    turn: TurnStateAtExit,
) -> Result<ProfileManifest, CommandError> {
    reconcile_inner(lease, m, mode, runtime, Some(spec.clone()), turn, None).await
}

/// Close an owned run when its provider definition has been removed or made
/// unavailable. Liveness and cleanup do not need the export layout; retain the
/// last complete generation and discard the private run tree after the writer
/// is confirmed gone.
pub async fn reconcile_retaining_previous(
    lease: &ProfileLease,
    m: ProfileManifest,
    mode: ReconcileMode,
    runtime: Arc<dyn AgentRuntimeEngine>,
) -> Result<ProfileManifest, CommandError> {
    reconcile_inner(
        lease,
        m,
        mode,
        runtime,
        None,
        TurnStateAtExit::Unknown,
        None,
    )
    .await
}

async fn reconcile_inner(
    lease: &ProfileLease,
    mut m: ProfileManifest,
    mode: ReconcileMode,
    runtime: Arc<dyn AgentRuntimeEngine>,
    spec: Option<ProfileStateSpec>,
    turn: TurnStateAtExit,
    redaction: Option<crate::engine::auth::redaction::RedactionSet>,
) -> Result<ProfileManifest, CommandError> {
    if m.state.admits_writer() {
        return Ok(m);
    }
    let owner = m
        .owner
        .clone()
        .ok_or_else(|| CommandError::Other("profile owner absent".into()))?;
    let budget = owner
        .cancel_timeout
        .parse::<crate::data::config::units::DurationStr>()
        .map_err(CommandError::Other)?
        .to_duration();
    let deadline = Instant::now() + budget;
    let owner_probe = owner.clone();
    let operation = tokio::task::spawn_blocking(move || -> Result<bool, EngineError> {
        if runtime.runtime_name() != owner_probe.runtime {
            return Err(EngineError::Config(
                "recorded profile runtime is unavailable".into(),
            ));
        }
        runtime.availability()?;
        let running = runtime.list_running_all()?;
        let stopped = runtime.list_stopped()?;
        let resources = running.iter().chain(stopped.iter()).collect::<Vec<_>>();
        let mut found = None;
        let mut same_name_without_label = false;
        for handle in &resources {
            let label = runtime.resource_label(handle, EXECUTION_LABEL)?;
            if label.as_deref() == Some(owner_probe.execution_id.as_str()) {
                if found.is_some() {
                    return Ok(false);
                }
                found = Some(*handle);
            }
            if handle.name == owner_probe.resource_name && label.is_none() {
                same_name_without_label = true;
            }
        }
        if let Some(handle) = found {
            let mut stop_error = None;
            if running.iter().any(|h| h.id == handle.id) {
                if mode == ReconcileMode::VerifyOnly {
                    return Ok(false);
                }
                stop_error = runtime.stop(handle).err();
            }
            // Stopping can race automatic cleanup or name reuse. Re-resolve
            // both generation and execution ownership before deleting anything.
            let running_now = runtime.list_running_all()?;
            let stopped_now = runtime.list_stopped()?;
            if let Some(current) = running_now
                .iter()
                .chain(stopped_now.iter())
                .find(|h| h.id == handle.id)
            {
                if current.started_at != handle.started_at || current.name != handle.name {
                    return Ok(false);
                }
                if runtime.resource_label(current, EXECUTION_LABEL)?.as_deref()
                    != Some(owner_probe.execution_id.as_str())
                {
                    return Ok(false);
                }
                if let Some(error) = stop_error {
                    return Err(error);
                }
                if running_now.iter().any(|h| h.id == current.id) {
                    return Ok(false);
                }
                let removed =
                    runtime.remove_agent_checked(current, owner_probe.execution_id.as_str());
                // An already absent instance is success only when a fresh
                // enumeration proves that the owned execution has gone.
                let remaining_running = runtime.list_running_all()?;
                let remaining_stopped = runtime.list_stopped()?;
                for remaining in remaining_running.iter().chain(remaining_stopped.iter()) {
                    let label = runtime.resource_label(remaining, EXECUTION_LABEL)?;
                    if label.as_deref() == Some(owner_probe.execution_id.as_str())
                        || (remaining.name == owner_probe.resource_name && label.is_none())
                    {
                        removed?;
                        return Ok(false);
                    }
                }
            } else {
                // Absence of the old id alone is insufficient if ownership
                // moved to another resource while stop was in progress.
                for remaining in running_now.iter().chain(stopped_now.iter()) {
                    let label = runtime.resource_label(remaining, EXECUTION_LABEL)?;
                    if label.as_deref() == Some(owner_probe.execution_id.as_str())
                        || (remaining.name == owner_probe.resource_name && label.is_none())
                    {
                        return Ok(false);
                    }
                }
            }
        }
        if same_name_without_label {
            // The expected name exists but the runtime cannot tell us which
            // execution owns it. Preserve the profile until that is resolved.
            return Ok(false);
        }
        // list_running_all/list_stopped completed successfully and no resource
        // carries this execution label, so the recorded execution is gone even
        // if the backend's resource name differs from the preview name.
        Ok(true)
    });
    match tokio::time::timeout(budget, operation).await {
        Ok(Ok(Ok(true))) => {}
        Ok(Ok(Ok(false))) => {
            mark_closing(
                lease,
                &mut m,
                CleanupReason::LivenessUnknown,
                Some("execution still running".into()),
            )?;
            return Ok(m);
        }
        Ok(Ok(Err(e))) => {
            mark_closing(
                lease,
                &mut m,
                CleanupReason::RuntimeUnavailable,
                Some(e.to_string()),
            )?;
            return Ok(m);
        }
        _ => {
            mark_closing(lease, &mut m, CleanupReason::CleanupTimeout, None)?;
            return Ok(m);
        }
    }
    if Instant::now() > deadline {
        mark_closing(lease, &mut m, CleanupReason::CleanupTimeout, None)?;
        return Ok(m);
    }
    let run = lease.run_dir(&owner.execution_id);
    // publish() commits the next complete generation while the owner remains
    // closing. A crash after that commit must finish cleanup without exporting
    // the run tree a second time.
    if m.current_generation > owner.base_generation {
        lease.remove_run_dir(&owner.execution_id)?;
        inactive(&mut m);
        lease.write_manifest(&m)?;
        lease.prune_generations(&[m.current_generation, m.current_generation.saturating_sub(1)])?;
        return Ok(m);
    }
    if !run.exists() {
        inactive(&mut m);
        lease.write_manifest(&m)?;
        return Ok(m);
    }
    if !owner.run_initialized {
        lease.remove_run_dir(&owner.execution_id)?;
        m.last_publication = Some(ProfilePublication {
            generation: m.current_generation,
            at: now(),
            status: PublicationStatus::RetainedPrevious,
            reason: Some("private run initialization was interrupted".into()),
        });
        inactive(&mut m);
        lease.write_manifest(&m)?;
        return Ok(m);
    }
    if owner.native_identity_rejected {
        if mode == ReconcileMode::StopOwned {
            lease.remove_run_dir(&owner.execution_id)?;
            m.last_publication = Some(ProfilePublication {
                generation: m.current_generation,
                at: now(),
                status: PublicationStatus::RetainedPrevious,
                reason: Some("provider did not confirm the requested conversation".into()),
            });
            inactive(&mut m);
            lease.write_manifest(&m)?;
        } else {
            mark_closing(
                lease,
                &mut m,
                CleanupReason::SnapshotRejected,
                Some("provider did not confirm the requested conversation".into()),
            )?;
        }
        return Ok(m);
    }
    let output_incomplete =
        owner.redaction_required && redaction.as_ref().is_none_or(|r| r.is_retired());
    let spec = spec.filter(|_| !output_incomplete);
    let Some(spec) = spec else {
        lease.remove_run_dir(&owner.execution_id)?;
        m.last_publication = Some(ProfilePublication {
            generation: m.current_generation,
            at: now(),
            status: PublicationStatus::RetainedPrevious,
            reason: Some(
                "provider profile layout is unavailable; previous generation retained".into(),
            ),
        });
        if output_incomplete {
            m.history_unavailable = Some("output_incomplete".into());
            if let Some(publication) = &mut m.last_publication {
                publication.reason = Some("output_incomplete".into());
            }
        }
        inactive(&mut m);
        lease.write_manifest(&m)?;
        return Ok(m);
    };
    spec.purge_excluded(&run)?;
    mark_closing(lease, &mut m, CleanupReason::Publishing, None)?;
    let stage = lease.begin_staging()?;
    let export_spec = spec.clone();
    let export_run = run.clone();
    let export_stage = stage.clone();
    let export_redaction = redaction.clone();
    let mut exported = tokio::task::spawn_blocking(move || {
        export_spec.export_redacted(&export_run, &export_stage, export_redaction.as_ref())
    })
    .await
    .map_err(|e| CommandError::Other(format!("profile export worker failed: {e}")))?;
    if mode == ReconcileMode::StopOwned && exported.is_err() {
        let retry_stage = lease.begin_staging()?;
        let retry_spec = spec.clone();
        let retry_run = run.clone();
        let retry_redaction = redaction.clone();
        exported = tokio::task::spawn_blocking(move || {
            retry_spec.export_redacted(&retry_run, &retry_stage, retry_redaction.as_ref())
        })
        .await
        .map_err(|e| CommandError::Other(format!("profile export worker failed: {e}")))?;
    }
    match exported {
        Ok(snapshot) => {
            let generation = m.current_generation + 1;
            let mut records = Vec::new();
            for id in snapshot.sessions {
                if id.as_str().starts_with('-') && !spec.allow_leading_dash {
                    continue;
                }
                let previous = m.native_sessions.iter().find(|r| r.id == id);
                if let Some(previous) = previous.filter(|_| {
                    owner.agent_session.as_ref() != Some(&id)
                        && !owner.observed_native_sessions.contains(&id)
                }) {
                    let mut previous = previous.clone();
                    previous.last_generation = generation;
                    records.push(previous);
                    continue;
                }
                records.push(NativeSessionRecord {
                    id,
                    first_generation: previous.map_or(generation, |r| r.first_generation),
                    last_generation: generation,
                    created_at: previous.map_or_else(now, |r| r.created_at.clone()),
                    last_used_at: now(),
                    execution_id: owner.execution_id.clone(),
                    turn_state: turn.clone(),
                });
            }
            if records.len() > MAX_NATIVE_SESSIONS {
                mark_closing(
                    lease,
                    &mut m,
                    CleanupReason::SnapshotRejected,
                    Some("native session record budget exceeded".into()),
                )?;
                return Ok(m);
            }
            m.native_sessions = records;
            m.current_generation = generation;
            m.history_unavailable = snapshot.history_unavailable;
            m.last_publication = Some(ProfilePublication {
                generation,
                at: now(),
                status: PublicationStatus::Published,
                reason: None,
            });
            // Cleanup is committed before inactive. If this crashes, next
            // reconciliation sees no run tree and safely finishes this step.
            let mut pending = m.clone();
            pending.state = ProfileState::Closing;
            lease.publish(stage, &pending)?;
            lease.remove_run_dir(&owner.execution_id)?;
            inactive(&mut m);
            lease.write_manifest(&m)?;
            lease.prune_generations(&[generation, generation.saturating_sub(1)])?;
        }
        Err(e) => {
            let io = snapshot_io(&e);
            if mode == ReconcileMode::StopOwned {
                // Explicit close acknowledges discarding an unpublishable run.
                lease.remove_run_dir(&owner.execution_id)?;
                m.last_publication = Some(ProfilePublication {
                    generation: m.current_generation,
                    at: now(),
                    status: PublicationStatus::RetainedPrevious,
                    reason: Some(
                        "snapshot rejected; previous consistent generation retained".into(),
                    ),
                });
                inactive(&mut m);
                lease.write_manifest(&m)?;
            } else {
                mark_closing(
                    lease,
                    &mut m,
                    if io {
                        CleanupReason::SnapshotIo
                    } else {
                        CleanupReason::SnapshotRejected
                    },
                    Some(e.to_string()),
                )?;
            }
        }
    }
    Ok(m)
}

/// Existing daemon lifecycle hooks call this bounded owner-local pass. No
/// process is stopped by automatic reconciliation.
pub async fn reconcile_repository(
    session: &Session,
    runtime: Arc<dyn AgentRuntimeEngine>,
) -> Result<(), CommandError> {
    let repo = ProfileRepoBinding::from_session(session)?;
    let paths = DataPaths::from_env(session.env())?;
    for row in list_profiles(&paths, &repo, None)? {
        if row.state.admits_writer() {
            continue;
        }
        let agent = AgentName::new(&row.agent)?;
        let name = SettingsProfileName::parse(&row.name)
            .map_err(|e| CommandError::Other(e.to_string()))?;
        let layout = ProfileLayout::new(&paths, &repo, &agent, &name);
        let reconciled = (|| -> Result<Option<(ProfileLease, ProfileManifest)>, CommandError> {
            let Some(lease) = layout.try_lock(Duration::ZERO)? else {
                return Ok(None);
            };
            Ok(lease.read_manifest()?.map(|m| (lease, m)))
        })();
        match reconciled {
            Ok(Some((lease, m))) => {
                if let Ok(def) =
                    crate::engine::agent::AgentRegistry::resolve_for_session(session, &agent)
                {
                    if let Some(spec) = &def.profile_state {
                        let recorded = recorded_runtime(session, &m, runtime.clone());
                        if let Err(error) = reconcile(
                            &lease,
                            m,
                            ReconcileMode::VerifyOnly,
                            recorded,
                            spec,
                            TurnStateAtExit::Unknown,
                        )
                        .await
                        {
                            tracing::warn!(profile=%row.name, %error, "profile reconciliation failed; continuing with other profiles");
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(profile=%row.name, %error, "profile reconciliation failed; continuing with other profiles")
            }
        }
    }
    Ok(())
}

fn snapshot_io(error: &EngineError) -> bool {
    match error {
        EngineError::Io { source, .. } => !is_unsafe_path_error(source),
        _ => false,
    }
}

/// A refused path (symlink, non-directory, invalid name) rather than real I/O.
#[cfg(unix)]
fn is_unsafe_path_error(source: &std::io::Error) -> bool {
    matches!(
        source.raw_os_error(),
        Some(libc::ELOOP | libc::ENOTDIR | libc::EINVAL)
    )
}

/// Windows has no `ELOOP`; match the portable kinds for the other two.
#[cfg(not(unix))]
fn is_unsafe_path_error(source: &std::io::Error) -> bool {
    matches!(
        source.kind(),
        std::io::ErrorKind::NotADirectory | std::io::ErrorKind::InvalidInput
    )
}

/// Reopen the recorded runtime when the user's current default changed.
/// Construction failure is reconciled truthfully as runtime_unavailable.
pub fn recorded_runtime(
    session: &Session,
    m: &ProfileManifest,
    current: Arc<dyn AgentRuntimeEngine>,
) -> Arc<dyn AgentRuntimeEngine> {
    let Some(owner) = &m.owner else {
        return current;
    };
    if owner.runtime == current.runtime_name() {
        return current;
    }
    use crate::data::config::runtime::RuntimeSelection;
    let kind = match owner.runtime.as_str() {
        "docker" => RuntimeSelection::Docker,
        "apple-containers" => RuntimeSelection::AppleContainers,
        "builtin" => RuntimeSelection::BuiltinExperimental,
        _ => return current,
    };
    let mut file = session.effective_config().config().clone();
    file.runtime.get_or_insert_with(Default::default).kind = Some(kind);
    crate::data::config::effective::EffectiveConfig::from_files(
        file,
        Default::default(),
        Default::default(),
        session.env().clone(),
    )
    .ok()
    .and_then(|c| crate::engine::agent_runtime::detect_effective(&c).ok())
    .map(|r| r.engine())
    .unwrap_or(current)
}

/// Sanitized intermediate publication while retaining the same profile OS lease.
/// This is a profile proof, not permission to launch or acknowledge credentials.
pub struct ProfileReplacementProof {
    owner: ProfileOwner,
    native: NativeSessionId,
    generation: u64,
    profile: SettingsProfileName,
    repository: String,
}
impl ProfileReplacementProof {
    pub fn native_session_id(&self) -> &NativeSessionId {
        &self.native
    }
}
impl ProfileSession {
    /// Called by ExecutionController's launch factory after its old writer,
    /// private services and output readers are stopped. No stop/restart loop
    /// lives here; reconciliation verifies ownership and publishes history.
    pub async fn quiesce_for_replacement(
        &mut self,
        runtime: Arc<dyn AgentRuntimeEngine>,
        native: NativeSessionId,
        turn: TurnStateAtExit,
        no_auto_continue_verified: bool,
    ) -> Result<ProfileReplacementProof, CommandError> {
        if !no_auto_continue_verified {
            return Err(unsupported(
                &AgentName::new(&self.manifest.provider)?,
                "no-auto-continue resume is unconfirmed",
            ));
        }
        let mut owner = self.manifest.owner.clone().ok_or_else(|| {
            CommandError::SettingsProfileBusy("profile has no active owner".into())
        })?;
        if owner.native_identity_rejected
            || owner.agent_session.as_ref().is_some_and(|id| id != &native)
        {
            return Err(CommandError::NativeSessionNotFound);
        }
        if !owner.observed_native_sessions.contains(&native) {
            owner.observed_native_sessions.push(native.clone());
        }
        // Credential replacement always requires the retained command set,
        // including when a future factory forgot to bind it before quiescing.
        owner.redaction_required = true;
        self.manifest.owner = Some(owner.clone());
        mark_closing(
            &self.lease,
            &mut self.manifest,
            CleanupReason::Publishing,
            None,
        )?;
        let published = reconcile_inner(
            &self.lease,
            self.manifest.clone(),
            ReconcileMode::VerifyOnly,
            runtime,
            Some(self.spec.clone()),
            turn,
            self.redaction.clone(),
        )
        .await?;
        self.manifest = published;
        if self.manifest.state != ProfileState::Inactive
            || self.manifest.history_unavailable.is_some()
            || !self
                .manifest
                .last_publication
                .as_ref()
                .is_some_and(|p| matches!(p.status, PublicationStatus::Published))
        {
            return Err(CommandError::SettingsProfileBusy(
                "replacement history or cleanup is unconfirmed".into(),
            ));
        }
        if !self.manifest.native_sessions.iter().any(|s| s.id == native) {
            return Err(CommandError::NativeSessionNotFound);
        }
        Ok(ProfileReplacementProof {
            owner,
            native,
            generation: self.manifest.current_generation,
            profile: self.manifest.name.clone(),
            repository: self.manifest.repository.key.clone(),
        })
    }
    /// Consume one intermediate publication and reserve a fresh private tree.
    /// The lease is never left/re-entered and the original prompt is absent.
    pub fn replace_execution(
        &mut self,
        proof: ProfileReplacementProof,
        new_id: ExecutionId,
    ) -> Result<ProfileRunState, CommandError> {
        if self.manifest.state != ProfileState::Inactive
            || self.manifest.owner.is_some()
            || self.manifest.current_generation != proof.generation
            || self.manifest.name != proof.profile
            || self.manifest.repository.key != proof.repository
            || new_id == proof.owner.execution_id
        {
            return Err(CommandError::SettingsProfileBusy(
                "stale profile replacement proof".into(),
            ));
        }
        if !self
            .spec
            .list_sessions(&self.lease.generation_tree(proof.generation))?
            .contains(&proof.native)
        {
            return Err(CommandError::NativeSessionNotFound);
        }
        let mut owner = proof.owner;
        owner.execution_id = new_id.clone();
        owner.agent_session = Some(proof.native);
        owner.observed_native_sessions.clear();
        owner.base_generation = proof.generation;
        owner.run_initialized = false;
        owner.native_identity_rejected = false;
        owner.started_at = now();
        owner.resource_name = format!("awman-profile-{}", new_id.as_str());
        owner.resource_label = format!("{EXECUTION_LABEL}={}", new_id.as_str());
        self.manifest.owner = Some(owner);
        self.manifest.state = ProfileState::Active;
        self.manifest.cleanup = Some(ProfileCleanup {
            reason: CleanupReason::Running,
            since: now(),
            last_attempt: None,
            last_error: None,
        });
        self.lease.write_manifest(&self.manifest)?;
        let result = (|| {
            let dir = self.lease.create_run_dir(&new_id)?;
            self.spec
                .import(&self.lease.generation_tree(proof.generation), &dir)
        })();
        if let Err(error) = result {
            mark_closing(
                &self.lease,
                &mut self.manifest,
                CleanupReason::SnapshotIo,
                Some("replacement import failed".into()),
            )?;
            return Err(error.into());
        }
        self.manifest
            .owner
            .as_mut()
            .expect("replacement owner")
            .run_initialized = true;
        self.lease.write_manifest(&self.manifest)?;
        Ok(self.run_state())
    }
}
