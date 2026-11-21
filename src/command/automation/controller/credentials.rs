//! Credential lifecycle operations owned by ExecutionController, including
//! ordinary profile executions. No background refresh task or secret store.
use super::*;
use crate::data::credential_update::*;
use crate::data::secret::CommandSecrets;
use std::{future::Future, pin::Pin};

type DriverFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, &'static str>> + Send + 'a>>;
/// A driver handles only an already resolved execution-private delivery route.
/// It never selects a source, model, account or initial task.
pub trait CredentialDriver: Send + Sync {
    fn stage<'a>(
        &'a self,
        fence: &'a UpdateFence,
        material: Arc<CommandSecrets>,
    ) -> DriverFuture<'a, StagedCredential>;
    /// Remove execution-private material after delivery has stopped. Idempotent.
    fn retire(&self) {}
    fn reread(&self) -> DriverFuture<'_, Arc<CommandSecrets>> {
        Box::pin(async { Err("source_unavailable") })
    }
}
/// Staging proves next-request selection, never token consumption.
#[derive(Debug)]
pub struct StagedCredential {
    pub admission_while_pending: bool,
}
/// Opaque action identity. Late callbacks must match every field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateFence {
    pub command_id: CommandId,
    pub execution_id: ExecutionId,
    pub credential_slot: String,
    pub request_id: Uuid,
    pub generation: u64,
}
/// Native adapters emit consumption only after a correlated authenticated native
/// request, or a generation-bound token-installed acknowledgement. Helper writes
/// and file staging have deliberately no consumption variant.
#[derive(Debug)]
pub enum CredentialConsumption {
    TokenInstalled {
        fence: UpdateFence,
    },
    RequestAuthenticated {
        fence: UpdateFence,
        request_sequence: u64,
    },
}
struct ActiveUpdate {
    fence: UpdateFence,
    material: Option<Arc<CommandSecrets>>,
    exposed: bool,
}
struct BindingState {
    execution_id: ExecutionId,
    active: Option<ActiveUpdate>,
    closing: bool,
    _leases: Vec<ExecutionLease>,
}
/// One execution's enrollment in this controller. Dropping the launch's handle
/// cancels any update and removes delivery authority.
pub struct CredentialExecution {
    controller: Weak<ExecutionController>,
    command: CommandId,
}
impl Drop for CredentialExecution {
    fn drop(&mut self) {
        if let Some(c) = self.controller.upgrade() {
            c.close_credentials(&self.command);
        }
    }
}
pub(super) struct CredentialBinding {
    pub host: AutomationHost,
    state: Mutex<BindingState>,
    driver: Arc<dyn CredentialDriver>,
    redaction: RedactionSet,
    stop: CancellationToken,
}
enum CredentialAdmission {
    Observe,
    Resume,
    Block,
}
impl ExecutionController {
    /// Hold admission through the metadata commit. The durable credential facts
    /// and the matching execution revision must become visible together, before
    /// terminal/event publication or another turn can use the new state.
    fn credential_transition<T>(
        &self,
        id: &ExecutionId,
        action: CredentialAdmission,
        commit: impl FnOnce(Option<&AutomationSnapshot>) -> Result<T, crate::data::error::DataError>,
    ) -> Result<T, &'static str> {
        let cell = self.cell(id);
        let mut state = cell
            .as_ref()
            .map(|cell| cell.state.lock().unwrap_or_else(|p| p.into_inner()));
        let live = state.as_mut().filter(|state| !state.finished);
        let Some(state) = live else {
            return commit(None).map_err(credential_error);
        };
        let previous = state.admission.clone();
        let effects = match action {
            CredentialAdmission::Observe => state.admission.credential_observed(),
            CredentialAdmission::Resume => state.admission.end_update(),
            CredentialAdmission::Block => state
                .admission
                .begin_update(false)
                .unwrap_or_else(|_| state.admission.credential_observed()),
        };
        match commit(Some(state.admission.snapshot())) {
            Ok(result) => {
                apply_effects(cell.as_ref().expect("locked cell"), state, effects);
                if state.persist_failed {
                    Err("credential_update_failed")
                } else {
                    Ok(result)
                }
            }
            Err(error) => {
                state.admission = previous;
                persistence_failed(
                    cell.as_ref().expect("locked cell"),
                    state,
                    "credential metadata commit failed",
                );
                Err(credential_error(error))
            }
        }
    }
    /// Enroll only after the profile lease and runtime ownership have been
    /// acquired. Ordinary profiles use the same operations without AutomationMode.
    pub fn register_credentials(
        self: &Arc<Self>,
        host: AutomationHost,
        execution_id: ExecutionId,
        driver: Arc<dyn CredentialDriver>,
        redaction: RedactionSet,
    ) -> Result<CredentialExecution, &'static str> {
        let record = host
            .store
            .read_execution(&execution_id)
            .map_err(|_| "not_found")?;
        if record.owner_key != host.owner_key
            || record.command_id != host.command_id
            || record.ended_at.is_some()
        {
            return Err("not_found");
        }
        let lease = if self.cell(&execution_id).is_none() {
            host.store
                .acquire_lease(&execution_id)
                .map_err(|_| "credential_update_failed")?
        } else {
            None
        };
        let command = host.command_id.clone();
        let mut entries = self.credentials.lock().unwrap_or_else(|p| p.into_inner());
        if entries.contains_key(&command) {
            return Err("input_not_allowed");
        }
        entries.insert(
            command.clone(),
            Arc::new(CredentialBinding {
                host,
                state: Mutex::new(BindingState {
                    execution_id,
                    active: None,
                    closing: false,
                    _leases: lease.into_iter().collect(),
                }),
                driver,
                redaction,
                stop: CancellationToken::new(),
            }),
        );
        Ok(CredentialExecution {
            controller: Arc::downgrade(self),
            command,
        })
    }
    fn credential_binding(&self, command: &CommandId) -> Option<Arc<CredentialBinding>> {
        self.credentials
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(command)
            .cloned()
    }
    pub fn close_credentials(&self, command: &CommandId) {
        let Some(binding) = self
            .credentials
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(command)
        else {
            return;
        };
        let mut state = binding.state.lock().unwrap_or_else(|p| p.into_inner());
        state.closing = true;
        binding.stop.cancel();
        binding.driver.retire();
        if let Some(active) = state.active.take() {
            let _ = self.credential_transition(
                &active.fence.execution_id,
                CredentialAdmission::Block,
                |snapshot| {
                    binding.host.store.fail_credential_update(
                        &binding.host.owner_key,
                        command,
                        active.fence.request_id,
                        CredentialUpdateFailure {
                            code: "credential_update_failed",
                            reason: "command_ended",
                            exposed: active.exposed,
                        },
                        snapshot,
                    )
                },
            );
        }
    }
    /// Caller ownership/origin is checked again here, after transport admission.
    pub fn stage_update(
        self: &Arc<Self>,
        viewer: &ViewerIdentity,
        host: &AutomationHost,
        request: CredentialUpdateRequest,
        grant: Option<&InputGrant>,
    ) -> Result<CredentialUpdateOperation, &'static str> {
        if !is_local_owner(viewer) || viewer.read_only {
            return Err("forbidden");
        }
        if let Some(op) = host
            .store
            .lookup_credential_update(&host.owner_key, &host.command_id, &request.intent)
            .map_err(credential_error)?
        {
            if let Some(binding) = self.credential_binding(&host.command_id) {
                let state = binding.state.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(active) = state
                    .active
                    .as_ref()
                    .filter(|a| a.fence.request_id == op.request_id)
                {
                    if let (Some(saved), Some(incoming)) = (&active.material, &request.secrets) {
                        if !crate::data::secret::same_bundle(saved, incoming) {
                            return Err("idempotency_conflict");
                        }
                    }
                }
            }
            return Ok(op);
        }
        let binding = self
            .credential_binding(&host.command_id)
            .ok_or("agent_not_ready")?;
        if binding.host.owner_key != host.owner_key {
            return Err("not_found");
        }
        let mut state = binding.state.lock().unwrap_or_else(|p| p.into_inner());
        // Repeat lookup under the same lock that serializes reservation.
        if let Some(op) = host
            .store
            .lookup_credential_update(&host.owner_key, &host.command_id, &request.intent)
            .map_err(credential_error)?
        {
            if let Some(active) = state
                .active
                .as_ref()
                .filter(|a| a.fence.request_id == op.request_id)
            {
                if let (Some(saved), Some(incoming)) = (&active.material, &request.secrets) {
                    if !crate::data::secret::same_bundle(saved, incoming) {
                        return Err("idempotency_conflict");
                    }
                }
            }
            return Ok(op);
        }
        if state.execution_id != request.intent.execution_id {
            return Err("stale_execution");
        }
        if state.closing || state.active.is_some() || binding.stop.is_cancelled() {
            return Err("input_not_allowed");
        }
        let record = host
            .store
            .read_execution(&state.execution_id)
            .map_err(credential_error)?;
        let cell = self.cell(&state.execution_id);
        let mut automation = cell
            .as_ref()
            .map(|c| c.state.lock().unwrap_or_else(|p| p.into_inner()));
        let terminal_id = record.terminal_id.as_ref().or_else(|| {
            automation
                .as_ref()
                .and_then(|a| a.terminal.as_ref().map(|(id, _)| id))
        });
        if let Some(id) = terminal_id {
            if !grant.is_some_and(|g| g.terminal_id() == id && g.is_valid()) {
                return Err("control_held");
            }
        }
        if automation
            .as_ref()
            .is_some_and(|a| a.finished || a.cancelled || a.close_request.is_some())
        {
            return Err("input_not_allowed");
        }
        if let Some(material) = &request.secrets {
            if material.names().map(str::to_owned).collect::<Vec<_>>()
                != request.intent.secret_names
            {
                return Err("invalid_secrets");
            }
            if material
                .values()
                .any(|s| s.expose().chars().count() < 4 || s.expose().chars().any(char::is_control))
            {
                return Err("invalid_secrets");
            }
        }
        let selected = host
            .store
            .effective_credentials(&host.owner_key, &state.execution_id)
            .map_err(credential_error)?
            .slots
            .into_iter()
            .find(|s| s.credential_slot == request.intent.credential_slot)
            .ok_or("not_found")?;
        let delay = selected.route.max_apply_delay.unwrap_or(60_000);
        // The production factory does not yet own stop, drain, launch, terminal
        // transfer and logical command finalization. Refuse before side effects.
        if selected.update_method == CredentialUpdateMethod::Restart {
            return Err("native_capability_unavailable");
        }
        let previous = automation.as_ref().map(|a| a.admission.clone());
        let effects = if let Some(a) = &mut automation {
            a.admission
                .begin_update(selected.update_method == CredentialUpdateMethod::Restart)
                .map_err(|_| "input_not_allowed")?
        } else {
            Vec::new()
        };
        let snapshot = automation.as_ref().map(|a| a.admission.snapshot());
        let op = match host.store.reserve_credential_update_snapshot(
            &host.owner_key,
            &host.command_id,
            &request.intent,
            delay,
            snapshot,
        ) {
            Ok(op) => op,
            Err(error) => {
                if let (Some(a), Some(previous)) = (&mut automation, previous) {
                    a.admission = previous;
                }
                return Err(credential_error(error));
            }
        };
        if let (Some(a), Some(c)) = (&mut automation, &cell) {
            apply_effects(c, a, effects);
            if a.persist_failed {
                return Err("credential_update_failed");
            }
        }
        let fence = UpdateFence {
            command_id: host.command_id.clone(),
            execution_id: state.execution_id.clone(),
            credential_slot: op.credential_slot.clone(),
            request_id: op.request_id,
            generation: op.new_generation,
        };
        state.active = Some(ActiveUpdate {
            fence: fence.clone(),
            material: request.secrets.clone(),
            exposed: false,
        });
        let redaction_result = request.secrets.as_ref().map_or(Ok(()), |m| {
            binding.redaction.try_add_many(m.values().cloned())
        });
        drop(automation);
        drop(state);
        if redaction_result.is_err() {
            self.fail_update(&host.command_id, op.request_id, "output_incomplete");
            return host
                .store
                .get_credential_update(&host.owner_key, &host.command_id, op.request_id)
                .map_err(credential_error)?
                .ok_or("not_found");
        }
        self.publish_credentials(&binding);
        let controller = self.clone();
        let accepted = op.clone();
        tokio::spawn(async move {
            controller
                .apply_credentials(binding, fence, accepted, request.secrets)
                .await;
        });
        Ok(op)
    }
    async fn apply_credentials(
        self: Arc<Self>,
        binding: Arc<CredentialBinding>,
        fence: UpdateFence,
        op: CredentialUpdateOperation,
        material: Option<Arc<CommandSecrets>>,
    ) {
        let remaining = (op.apply_deadline - chrono::Utc::now())
            .to_std()
            .unwrap_or_default();
        let deadline = tokio::time::Instant::now() + remaining;
        let work = async {
            let material = match material {
                Some(m) => m,
                None => tokio::time::timeout_at(deadline, binding.driver.reread())
                    .await
                    .map_err(|_| "source_unavailable")??,
            };
            if material.names().map(str::to_owned).collect::<Vec<_>>()
                != binding
                    .host
                    .store
                    .effective_credentials(&binding.host.owner_key, &fence.execution_id)
                    .map_err(credential_error)?
                    .slots
                    .into_iter()
                    .find(|s| s.credential_slot == fence.credential_slot)
                    .ok_or("not_found")?
                    .secret_names
                || material.values().any(|s| {
                    s.expose().chars().count() < 4 || s.expose().chars().any(char::is_control)
                })
            {
                return Err("invalid_secrets");
            }
            binding
                .redaction
                .try_add_many(material.values().cloned())
                .map_err(|_| "output_incomplete")?;
            self.mark_exposed(&binding, &fence)?;
            let driver = binding.driver.clone();
            let staged_fence = fence.clone();
            let mut task = tokio::spawn(async move { driver.stage(&staged_fence, material).await });
            let admission = tokio::select! {
                result = &mut task => result.map_err(|_| "credential_update_failed")??.admission_while_pending,
                _ = binding.stop.cancelled() => { let _ = task.await; binding.driver.retire(); return Err("command_ended"); },
                _ = tokio::time::sleep_until(deadline) => { self.fail_update(&binding.host.command_id, op.request_id, "application_unconfirmed"); let _ = task.await; binding.driver.retire(); return Err("application_unconfirmed"); },
            };
            self.check_fence(&binding, &fence)?;
            let state = binding.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.closing
                || binding.stop.is_cancelled()
                || !state
                    .active
                    .as_ref()
                    .is_some_and(|active| active.fence == fence)
            {
                return Err("stale_execution");
            }
            self.credential_transition(
                &fence.execution_id,
                if admission {
                    CredentialAdmission::Resume
                } else {
                    CredentialAdmission::Observe
                },
                |snapshot| {
                    binding.host.store.stage_credential_generation(
                        &binding.host.owner_key,
                        &binding.host.command_id,
                        op.request_id,
                        admission,
                        snapshot,
                    )
                },
            )?;
            drop(state);
            self.publish_credentials(&binding);
            Ok::<(), &'static str>(())
        };
        let outcome = work.await;
        if let Err(reason) = outcome {
            if binding.stop.is_cancelled() {
                binding.driver.retire();
            }
            self.fail_update(&binding.host.command_id, op.request_id, reason);
            return;
        }
        // Idle time is part of the original deadline; no synthetic model request.
        tokio::select! {_=binding.stop.cancelled()=>{},_=tokio::time::sleep_until(deadline)=>{self.fail_update(&binding.host.command_id,op.request_id,"application_unconfirmed");}}
    }
    fn mark_exposed(
        &self,
        binding: &CredentialBinding,
        fence: &UpdateFence,
    ) -> Result<(), &'static str> {
        let mut state = binding.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closing || binding.stop.is_cancelled() {
            return Err("command_ended");
        }
        let active = state
            .active
            .as_mut()
            .filter(|a| a.fence == *fence)
            .ok_or("stale_execution")?;
        binding
            .host
            .store
            .mark_credential_update_exposed(
                &binding.host.owner_key,
                &binding.host.command_id,
                fence.request_id,
            )
            .map_err(credential_error)?;
        active.exposed = true;
        Ok(())
    }
    fn check_fence(
        &self,
        binding: &CredentialBinding,
        fence: &UpdateFence,
    ) -> Result<(), &'static str> {
        let state = binding.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closing
            || binding.stop.is_cancelled()
            || !state.active.as_ref().is_some_and(|a| a.fence == *fence)
        {
            return Err("stale_execution");
        }
        Ok(())
    }
    pub fn confirm_update(
        &self,
        observation: CredentialConsumption,
    ) -> Result<CredentialUpdateOperation, &'static str> {
        let fence = match observation {
            CredentialConsumption::TokenInstalled { fence } => fence,
            CredentialConsumption::RequestAuthenticated {
                fence,
                request_sequence,
            } => {
                if request_sequence == 0 {
                    return Err("agent_protocol_error");
                }
                fence
            }
        };
        let binding = self
            .credential_binding(&fence.command_id)
            .ok_or("stale_execution")?;
        let mut state = binding.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closing
            || binding.stop.is_cancelled()
            || !state
                .active
                .as_ref()
                .is_some_and(|a| a.fence == fence && a.exposed)
        {
            return Err("stale_execution");
        }
        let op = self.credential_transition(
            &fence.execution_id,
            CredentialAdmission::Resume,
            |snapshot| {
                binding.host.store.ack_credential_generation(
                    &binding.host.owner_key,
                    &fence.command_id,
                    fence.request_id,
                    &fence.execution_id,
                    fence.generation,
                    snapshot,
                )
            },
        )?;
        state.active = None;
        drop(state);
        if let Some(events) = &binding.host.events {
            events.emit(EventPayload::AgentCredentialsChanged {
                request_id: op.request_id,
                execution_id: fence.execution_id.clone(),
                credential_generation: op.credential_generation,
                refresh_owner: op.refresh_owner,
            });
        }
        self.publish_credentials(&binding);
        Ok(op)
    }
    pub fn fail_update(&self, command: &CommandId, request: Uuid, reason: &'static str) {
        let Some(binding) = self.credential_binding(command) else {
            return;
        };
        let mut state = binding.state.lock().unwrap_or_else(|p| p.into_inner());
        if state
            .active
            .as_ref()
            .is_none_or(|a| a.fence.request_id != request)
        {
            return;
        }
        let active = state.active.take().expect("active update");
        let blocked = active.exposed || binding.redaction.is_retired();
        let _ = self.credential_transition(
            &active.fence.execution_id,
            if blocked {
                CredentialAdmission::Block
            } else {
                CredentialAdmission::Resume
            },
            |snapshot| {
                binding.host.store.fail_credential_update(
                    &binding.host.owner_key,
                    command,
                    request,
                    CredentialUpdateFailure {
                        code: "credential_update_failed",
                        reason,
                        exposed: active.exposed,
                    },
                    snapshot,
                )
            },
        );
        drop(state);
        if active.exposed {
            binding.driver.retire();
        }
        self.publish_credentials(&binding);
    }
}
fn credential_error(e: crate::data::error::DataError) -> &'static str {
    match e {
        crate::data::error::DataError::Other(m) => match m.as_str() {
            "not_found" => "not_found",
            "stale_execution" => "stale_execution",
            "stale_revision" => "stale_revision",
            "input_not_allowed" => "input_not_allowed",
            "input_backpressure" => "input_backpressure",
            "idempotency_conflict" => "idempotency_conflict",
            "invalid_secrets" => "invalid_secrets",
            "native_capability_unavailable" => "native_capability_unavailable",
            "agent_not_ready" => "agent_not_ready",
            _ => "credential_update_failed",
        },
        _ => "credential_update_failed",
    }
}
impl CredentialBinding {
    pub(super) fn execution_matches(&self, id: &ExecutionId) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .execution_id
            == *id
    }
}
impl ExecutionController {
    fn publish_credentials(&self, binding: &CredentialBinding) {
        let execution_id = binding
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .execution_id
            .clone();
        if let Ok(snapshot) = binding
            .host
            .store
            .effective_credentials(&binding.host.owner_key, &execution_id)
        {
            if let Some(cell) = self.cell(&execution_id) {
                let state = cell.state.lock().unwrap_or_else(|p| p.into_inner());
                if let Some((id, hub)) = terminal(&state) {
                    let _ = hub.set_credential_updates(&id, snapshot);
                }
            }
        }
    }
}

/// Production helper delivery for a selected native callback. This supplies
/// staging only; its caller must separately qualify native generation
/// consumption before advertising live support.
#[cfg(unix)]
pub struct HelperCredentialDriver {
    directory: crate::engine::credential_refresh::delivery::HelperDirectory,
    name: crate::data::secret::SecretName,
}
#[cfg(unix)]
impl HelperCredentialDriver {
    pub fn open(path: &std::path::Path, name: &str) -> Result<Arc<Self>, &'static str> {
        Ok(Arc::new(Self {
            directory: crate::engine::credential_refresh::delivery::HelperDirectory::open(path)?,
            name: crate::data::secret::SecretName::parse(name).map_err(|_| "invalid_secrets")?,
        }))
    }
}
#[cfg(unix)]
impl CredentialDriver for HelperCredentialDriver {
    fn retire(&self) {
        let _ = self.directory.retire();
    }
    fn stage<'a>(
        &'a self,
        fence: &'a UpdateFence,
        material: Arc<CommandSecrets>,
    ) -> DriverFuture<'a, StagedCredential> {
        Box::pin(async move {
            if material.names().collect::<Vec<_>>() != vec![self.name.as_str()] {
                return Err("invalid_secrets");
            }
            self.directory.stage(
                fence.generation,
                material.get(self.name.as_str()).ok_or("invalid_secrets")?,
            )?;
            // Cache invalidation and stale-callback exclusion must be proven by
            // the native adapter. A payload rename cannot release admission.
            Ok(StagedCredential {
                admission_while_pending: false,
            })
        })
    }
}

impl ExecutionController {
    /// Native adapters call only after bound initial-token consumption. Neither
    /// a spawn nor a helper callback/file write may call this operation.
    pub fn confirm_initial_credentials(
        &self,
        command: &CommandId,
        execution: &ExecutionId,
        slot: &str,
    ) -> Result<(), &'static str> {
        let binding = self.credential_binding(command).ok_or("stale_execution")?;
        let state = binding.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closing
            || state.active.is_some()
            || state.execution_id != *execution
            || binding.stop.is_cancelled()
        {
            return Err("stale_execution");
        }
        let cell = self.cell(execution);
        let mut automation = cell
            .as_ref()
            .map(|c| c.state.lock().unwrap_or_else(|p| p.into_inner()));
        let previous = automation.as_ref().map(|a| a.admission.clone());
        let effects = automation
            .as_mut()
            .map(|a| a.admission.credential_observed())
            .unwrap_or_default();
        let snapshot = automation.as_ref().map(|a| a.admission.snapshot());
        if let Err(error) = binding.host.store.ack_initial_credential_generation(
            &binding.host.owner_key,
            execution,
            slot,
            snapshot,
        ) {
            if let (Some(a), Some(previous)) = (&mut automation, previous) {
                a.admission = previous;
            }
            return Err(credential_error(error));
        }
        if let (Some(a), Some(c)) = (&mut automation, &cell) {
            apply_effects(c, a, effects);
        }
        drop(automation);
        drop(state);
        self.publish_credentials(&binding);
        Ok(())
    }
}
