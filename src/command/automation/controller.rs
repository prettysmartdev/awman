//! The shared execution controller (WI 0070 B3/B4/B7/B8).
//!
//! One process-wide [`ExecutionController`] owns a serialized lifecycle cell
//! per execution. It implements the Part A [`TerminalAutomation`] seam for
//! every terminal transport, persists through the shared
//! [`AutomationStore`], publishes through the terminal hub and the command's
//! existing event stream, and runs the startup/acknowledgement deadlines.
//!
//! Lock order: cell lock -> store transaction -> hub. No lock is held across
//! an await. Credential rotation builds on [`OwnedExecution`]'s update
//! fence (`begin_update`/`end_update`) and [`ExecutionController::cancel`];
//! it must not create another execution or restart controller.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::admission::{codes, AdmissionCell, AdmissionConfig, Effect, Refusal, TurnPhase};
use super::results::{self, ExecutionFacts, FailureInputs};
use crate::command::terminals::{
    AutomationRefusal, OperationReply, TerminalAutomation, TerminalRefusal,
};
use crate::data::automation::{CleanupStatus, OutputStatus, StatusSupport};
use crate::data::automation_observation::Observation;
use crate::data::automation_records::{
    AutomationFailure, AutomationSnapshot, CloseExecutionRequest, CommandAutomationResult,
    DeliveryPhase, ExecutionProfileBinding, ExecutionRecord, LaunchPhase, SubmitTurnRequest,
    TurnRecord,
};
use crate::data::automation_store::{
    operation_digest, AutomationStore, ExecutionLease, NewExecution, Operation, RequestLookup,
    Reservation, StoredReply,
};
use crate::data::config::agents::AutomationTimeouts;
use crate::data::daemon_state::{CommandListener, Principal, RequestOrigin};
use crate::data::execution_event::EventPayload;
use crate::data::ids::{CommandId, ExecutionId, TerminalId, TurnId};
use crate::data::settings_profile::NativeSessionId;
use crate::data::terminal::{TerminalFrame, ViewerIdentity};
use crate::engine::agent::automation::{AgentAutomationAdapter, PromptRefusal};
use crate::engine::auth::redaction::RedactionSet;
use crate::engine::terminal_hub::{InputGrant, TerminalHub};

/// Where an execution's lifecycle events go: the command's existing event
/// stream (the daemon's bus, or nothing for a foreground run).
pub use crate::command::commands::api_server::event_bus::LifecyclePublisher;

/// The durable host an execution is recorded in: the shared API database
/// with an existing command row (daemon-hosted or a foreground record).
#[derive(Clone)]
pub struct AutomationHost {
    pub store: AutomationStore,
    pub command_id: CommandId,
    pub attempt: u32,
    /// Trusted UID/SID + data-home binding of the local owner.
    pub owner_key: String,
    pub events: Option<LifecyclePublisher>,
    /// Receives the frozen assurance when the execution finishes, so the
    /// command result keeps it on every outcome (cancellation included).
    pub finished: Option<FinishedSink>,
}

/// See [`AutomationHost::finished`].
pub type FinishedSink = Arc<dyn Fn(&CommandAutomationResult, Option<&str>) + Send + Sync>;

impl std::fmt::Debug for AutomationHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutomationHost")
            .field("command_id", &self.command_id)
            .field("attempt", &self.attempt)
            .finish_non_exhaustive()
    }
}

/// The trusted local-owner key for this process and data home.
pub fn local_owner_key(data_home: &std::path::Path) -> String {
    #[cfg(unix)]
    let uid = nix::unistd::geteuid().as_raw().to_string();
    #[cfg(not(unix))]
    // The per-user data home already binds the owner on Windows.
    let uid = String::from("owner");
    format!("uid:{uid}:{}", data_home.display())
}

/// Everything `start` needs.
pub struct ControllerLaunch {
    pub host: AutomationHost,
    /// Adopted from the profile session; never a second ID for one process.
    pub execution_id: ExecutionId,
    pub snapshot: AutomationSnapshot,
    pub config: AdmissionConfig,
    pub adapter: &'static dyn AgentAutomationAdapter,
    pub timeouts: AutomationTimeouts,
    pub requested_native: Option<NativeSessionId>,
    pub profile_binding: ExecutionProfileBinding,
    pub runtime: String,
    /// The interactive initial task, delivered exactly once after readiness.
    pub initial_task: Option<String>,
}

/// How a finished execution ended.
#[derive(Debug, Clone)]
pub struct ExitFacts {
    pub exit_code: Option<i32>,
    pub wait_error: Option<String>,
    pub output_status: OutputStatus,
    pub cleanup_status: CleanupStatus,
}

/// The frozen execution assurance.
#[derive(Debug, Clone)]
pub struct FinishedExecution {
    pub automation: CommandAutomationResult,
    pub failure: Option<AutomationFailure>,
    pub final_message: Option<String>,
    pub cancelled: bool,
}

impl FinishedExecution {
    pub fn storage_failed(&mut self, error: &str) {
        self.automation.completion_verified = false;
        self.final_message = None;
        self.failure = Some(super::admission::failure("storage_error", error));
    }
}

/// One accepted guarded write, waiting for the writer pump. The pump
/// authorizes it (fences rechecked, `WriteStarted` persisted) immediately
/// before handing it to the engine writer, never at enqueue time.
struct PendingWrite {
    turn_id: TurnId,
    bytes: Vec<u8>,
}

/// A forgotten execution's terminal, kept so a retried request still gets
/// its durable reply (or conflict) after the live cell is gone.
#[derive(Clone)]
struct Retired {
    execution_id: ExecutionId,
    store: AutomationStore,
    owner_key: String,
}

/// How many forgotten terminals keep durable request replay.
const RETIRED_TERMINALS: usize = 1024;

struct CellState {
    admission: AdmissionCell,
    host: AutomationHost,
    adapter: &'static dyn AgentAutomationAdapter,
    timeouts: AutomationTimeouts,
    terminal: Option<(TerminalId, Weak<TerminalHub>)>,
    /// The bounded (one whole operation) queue to the writer pump.
    writer: Option<tokio::sync::mpsc::Sender<PendingWrite>>,
    /// The launch's shared redaction set; every durable result and frame
    /// passes through it.
    redaction: RedactionSet,
    /// A durable commit failed: input is stopped and nothing further is
    /// published from in-memory state.
    persist_failed: bool,
    /// This process's liveness lease on the execution.
    lease: Option<ExecutionLease>,
    turns: Vec<TurnRecord>,
    next_ordinal: u64,
    initial_task: Option<String>,
    initial_request: Uuid,
    bootstrap: Option<InputGrant>,
    stop_failure: Option<AutomationFailure>,
    cancelled: bool,
    close_request: Option<Uuid>,
    startup_deadline: Instant,
    ack_deadline: Option<(TurnId, Instant)>,
    identity_hook: Option<Arc<dyn Fn(NativeSessionId) + Send + Sync>>,
    finished: bool,
}

struct Cell {
    execution_id: ExecutionId,
    state: Mutex<CellState>,
    stop: CancellationToken,
    timers: CancellationToken,
    /// Downstream lifecycle consumers (the OSC decoder over the PTY
    /// stream) that must reach their output boundary before the result
    /// is frozen.
    drains: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

/// The process-wide execution registry and admission engine.
#[derive(Default)]
pub struct ExecutionController {
    credentials: Mutex<HashMap<CommandId, Arc<credentials::CredentialBinding>>>,
    cells: Mutex<HashMap<ExecutionId, Arc<Cell>>>,
    terminals: Mutex<HashMap<TerminalId, ExecutionId>>,
    retired: Mutex<std::collections::VecDeque<(TerminalId, Retired)>>,
    /// Leases of finished executions whose command result is not stored
    /// yet; released once it is ([`ExecutionController::command_finalized`]).
    leases: Mutex<HashMap<CommandId, Vec<ExecutionLease>>>,
}

impl std::fmt::Debug for ExecutionController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionController")
            .finish_non_exhaustive()
    }
}

static GLOBAL: OnceLock<Arc<ExecutionController>> = OnceLock::new();

/// The process-wide controller every terminal transport installs.
pub fn global() -> Arc<ExecutionController> {
    GLOBAL
        .get_or_init(|| Arc::new(ExecutionController::default()))
        .clone()
}

/// Whether a viewer is the authenticated local owner on the owner socket
/// (or the local process). Network, browser, work-server, relay and
/// unknown origins never are, even across a local hop.
pub fn is_local_owner(viewer: &ViewerIdentity) -> bool {
    viewer.principal == Principal::Owner
        && viewer.listener == CommandListener::Local
        && matches!(
            viewer.origin,
            RequestOrigin::LocalProcess | RequestOrigin::OwnerSocket
        )
}

fn refusal(request_id: Uuid, code: &str, message: impl Into<String>) -> AutomationRefusal {
    AutomationRefusal {
        request_id,
        code: code.into(),
        error: message.into(),
    }
}

fn store_error(e: crate::data::error::DataError) -> String {
    e.to_string()
}

impl ExecutionController {
    /// Reserve the execution durably and register its cell. The returned
    /// handle owns the execution until [`OwnedExecution::finish`].
    pub fn start(self: &Arc<Self>, launch: ControllerLaunch) -> Result<OwnedExecution, String> {
        if let Some(events) = &launch.host.events {
            events.ensure_available().map_err(store_error)?;
        }
        let ControllerLaunch {
            host,
            execution_id,
            mut snapshot,
            config,
            adapter,
            timeouts,
            requested_native,
            profile_binding,
            runtime,
            initial_task,
        } = launch;
        snapshot.execution_id = execution_id.clone();
        let admission = AdmissionCell::new(snapshot, config, requested_native.clone());
        let snapshot = admission.snapshot().clone();
        // Liveness first: recovery never finalizes an execution whose
        // owner still holds this lease.
        let lease = host
            .store
            .acquire_lease(&execution_id)
            .map_err(store_error)?;
        reserve_execution_record(
            &host,
            &execution_id,
            &snapshot,
            profile_binding,
            runtime,
            requested_native,
            &timeouts,
        )?;
        let next_ordinal = next_turn_ordinal(&host.store, &host.command_id)?;
        let startup = timeouts.startup.to_duration();
        let cell = Arc::new(Cell {
            execution_id: execution_id.clone(),
            state: Mutex::new(CellState {
                admission,
                host,
                adapter,
                timeouts,
                terminal: None,
                writer: None,
                redaction: RedactionSet::default(),
                persist_failed: false,
                lease,
                turns: Vec::new(),
                next_ordinal,
                initial_task,
                initial_request: Uuid::new_v4(),
                bootstrap: None,
                stop_failure: None,
                cancelled: false,
                close_request: None,
                startup_deadline: Instant::now() + startup,
                ack_deadline: None,
                identity_hook: None,
                finished: false,
            }),
            stop: CancellationToken::new(),
            timers: CancellationToken::new(),
            drains: Mutex::new(Vec::new()),
        });
        self.cells
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(execution_id, cell.clone());
        Ok(OwnedExecution {
            controller: self.clone(),
            cell,
        })
    }

    fn cell(&self, id: &ExecutionId) -> Option<Arc<Cell>> {
        self.cells
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
    }

    fn cell_for_terminal(&self, id: &TerminalId) -> Option<Arc<Cell>> {
        let execution = self
            .terminals
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()?;
        self.cell(&execution)
    }

    /// The snapshot of an owned execution (owner checked by the caller's
    /// typed operation).
    pub fn snapshot(&self, execution: &ExecutionId) -> Option<AutomationSnapshot> {
        let cell = self.cell(execution)?;
        let state = cell.state.lock().unwrap_or_else(|p| p.into_inner());
        Some(state.admission.snapshot().clone())
    }

    /// Fold Layer 1 observations into an execution (fenced by execution ID).
    pub fn observe(&self, observations: Vec<Observation>) {
        let mut deliver = Vec::new();
        for observation in observations {
            let Some(cell) = self.cell(&observation.binding.execution_id) else {
                continue;
            };
            let mut state = cell.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.finished {
                continue;
            }
            let effects = state.admission.observe(&observation);
            if apply_effects(&cell, &mut state, effects)
                && !deliver.iter().any(|c: &Arc<Cell>| Arc::ptr_eq(c, &cell))
            {
                deliver.push(cell.clone());
            }
        }
        for cell in deliver {
            deliver_initial(&cell);
        }
    }

    /// Accept cancellation of an execution: invalidate, revoke, stop.
    pub fn cancel(&self, execution: &ExecutionId) {
        let command = self
            .credentials
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .find(|b| b.execution_matches(execution))
            .map(|b| b.host.command_id.clone());
        if let Some(command) = command {
            self.close_credentials(&command);
        }

        if let Some(cell) = self.cell(execution) {
            let mut state = cell.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.finished {
                return;
            }
            state.cancelled = true;
            let effects = state.admission.cancel();
            apply_effects(&cell, &mut state, effects);
            drop(state);
            cell.stop.cancel();
        }
    }
    /// Called once the command's final result is stored (daemon queue
    /// worker, or a foreground record): publish `execution_closed` for a
    /// close request, then the terminal's one logical exit, and release the
    /// command's execution leases.
    pub fn command_finalized(
        &self,
        command: &CommandId,
        result: &crate::data::fs::api_db::CommandResult,
    ) {
        self.close_credentials(command);
        let cells: Vec<Arc<Cell>> = self
            .cells
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect();
        for cell in cells {
            let mut state = cell.state.lock().unwrap_or_else(|p| p.into_inner());
            if &state.host.command_id != command || !state.finished {
                continue;
            }
            let hub = terminal(&state);
            if let (Some(request), Some((id, hub))) = (state.close_request, &hub) {
                if let Ok(frame) = state.host.store.complete_close(
                    &state.host.owner_key,
                    &cell.execution_id,
                    request,
                    result,
                ) {
                    let _ = hub.publish_automation_frame(id, frame);
                }
            }
            if let Some((id, hub)) = hub {
                let _ = hub.finish_automation(&id, result.exit_code.unwrap_or(1));
            }
            state.lease = None;
            drop(state);
            self.forget(&cell.execution_id);
        }
        self.release_leases(command);
    }

    /// Hold a finished execution's lease until its command result is stored.
    pub fn hold_lease(&self, command: &CommandId, lease: Option<ExecutionLease>) {
        if let Some(lease) = lease {
            self.leases
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .entry(command.clone())
                .or_default()
                .push(lease);
        }
    }

    /// The command result is stored: its executions are no longer live.
    pub fn release_leases(&self, command: &CommandId) {
        self.leases
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(command);
    }

    fn forget(&self, execution: &ExecutionId) {
        let cell = self
            .cells
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(execution);
        let mut terminals = self.terminals.lock().unwrap_or_else(|p| p.into_inner());
        let gone: Vec<TerminalId> = terminals
            .iter()
            .filter(|(_, e)| *e == execution)
            .map(|(t, _)| t.clone())
            .collect();
        terminals.retain(|_, e| e != execution);
        drop(terminals);
        // Durable request replay outlives the live cell.
        if let Some(cell) = cell {
            let state = cell.state.lock().unwrap_or_else(|p| p.into_inner());
            let retired = Retired {
                execution_id: execution.clone(),
                store: state.host.store.clone(),
                owner_key: state.host.owner_key.clone(),
            };
            drop(state);
            let mut list = self.retired.lock().unwrap_or_else(|p| p.into_inner());
            for terminal in gone {
                list.retain(|(t, _)| t != &terminal);
                list.push_back((terminal, retired.clone()));
            }
            while list.len() > RETIRED_TERMINALS {
                list.pop_front();
            }
        }
    }

    fn retired(&self, terminal: &TerminalId) -> Option<Retired> {
        self.retired
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|(t, _)| t == terminal)
            .map(|(_, r)| r.clone())
    }

    /// Crash recovery: every execution a dead owner left launching, running
    /// or with pending publication is finalized without replay. An unsettled
    /// turn without provider acknowledgement becomes `delivery_unknown`; an
    /// acknowledged one `execution_lost`. Committed results are preserved.
    /// An execution whose owner is still alive (its OS lease is held, by
    /// another process or a live cell here) is never touched. Ended
    /// executions whose command result was never stored are returned too,
    /// so their commands are finalized from the durable execution. Returns
    /// the commands that need finalization.
    pub fn recover(store: &AutomationStore) -> Result<Vec<CommandId>, String> {
        store.recover_credential_updates().map_err(store_error)?;
        let live = |id: &ExecutionId| -> Result<bool, String> {
            Ok(global().cell(id).is_some_and(|cell| {
                !cell
                    .state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .finished
            }) || store.lease_held(id).map_err(store_error)?)
        };
        let mut commands = Vec::new();
        for record in store.list_recoverable().map_err(store_error)? {
            if record.launch_phase == LaunchPhase::Ended || live(&record.execution_id)? {
                continue;
            }
            // Recovery authority: one recoverer per execution.
            let _authority = match store.acquire_lease(&record.execution_id) {
                Ok(lease) => lease,
                Err(_)
                    if store
                        .lease_held(&record.execution_id)
                        .map_err(store_error)? =>
                {
                    continue
                }
                Err(error) => return Err(store_error(error)),
            };
            if record.automation_mode.is_none() {
                continue;
            }
            let mut snapshot = record
                .snapshot
                .clone()
                .ok_or_else(|| "missing required automation snapshot".to_owned())?;
            let lost = super::admission::failure(
                codes::EXECUTION_LOST,
                "awman stopped while this execution was running; it was not resumed or replayed",
            );
            if let Some(turn) = &record.active_turn_id {
                let acknowledged = store
                    .turn_delivery_phase(turn)
                    .map_err(store_error)?
                    .is_some_and(|p| p >= DeliveryPhase::Acknowledged);
                // A direct lookup: the active turn may be beyond any page.
                let found = store
                    .read_turn(&record.execution_id, turn)
                    .map_err(store_error)?;
                {
                    let t = found
                        .filter(|t| t.result.is_none())
                        .ok_or_else(|| "missing required unsettled turn".to_owned())?;
                    let result = crate::data::automation_records::TurnResult {
                        status: if acknowledged {
                            crate::data::automation::TurnStatus::Failed
                        } else {
                            crate::data::automation::TurnStatus::DeliveryUnknown
                        },
                        status_support: t.status_support,
                        readiness_source: if t.status_source
                            == crate::data::automation::StatusSource::Native
                        {
                            crate::data::automation::ReadinessSource::Native
                        } else {
                            crate::data::automation::ReadinessSource::Osc7501
                        },
                        completion_source: crate::data::automation::CompletionSource::None,
                        completion_verified: false,
                        native_turn_id: t.native_turn_id.clone(),
                        final_message: None,
                        output_status: OutputStatus::Incomplete,
                        failure: Some(if acknowledged {
                            lost.clone()
                        } else {
                            super::admission::failure(
                                codes::DELIVERY_UNKNOWN,
                                "awman stopped before the agent acknowledged the task",
                            )
                        }),
                    };
                    snapshot.revision = record.revision + 1;
                    snapshot.active_turn_id = None;
                    snapshot.state = crate::data::automation::AutomationState::Stopping;
                    store
                        .complete_turn(turn, &result, &snapshot, None)
                        .map_err(store_error)?;
                }
            }
            let current = store
                .read_execution(&record.execution_id)
                .map_err(store_error)?;
            let mut ended = current.snapshot.clone().unwrap_or(snapshot);
            ended.revision = current.revision + 1;
            ended.active_turn_id = None;
            ended.state = crate::data::automation::AutomationState::Ended;
            store
                .transition_execution(current.revision, &ended, None)
                .map_err(store_error)?;
            let assurance = CommandAutomationResult {
                execution_id: record.execution_id.clone(),
                native_session_id: current.native_session_id.clone(),
                last_turn_id: None,
                native_start_mode: record.native_start_mode,
                workspace_access: record.workspace_access,
                status_support: StatusSupport::None,
                completion_verified: false,
                output_status: OutputStatus::Incomplete,
                cleanup_status: CleanupStatus::Pending,
            };
            store
                .finish_execution(&record.execution_id, &assurance, Some(&lost))
                .map_err(store_error)?;
            if !commands.contains(&record.command_id) {
                commands.push(record.command_id.clone());
            }
        }
        // Finished executions whose owner died before storing the command
        // result: the command is finalized from the durable execution.
        for record in store.list_unfinalized_ended().map_err(store_error)? {
            if live(&record.execution_id)? || commands.contains(&record.command_id) {
                continue;
            }
            commands.push(record.command_id.clone());
        }
        Ok(commands)
    }

    #[allow(clippy::too_many_arguments)] // One typed operation; each field is checked.
    fn operation(
        &self,
        terminal_id: &TerminalId,
        viewer: &ViewerIdentity,
        grant: Option<&InputGrant>,
        request_id: Uuid,
        execution_id: &ExecutionId,
        expected_revision: u64,
        prompt: Option<&str>,
    ) -> Result<OperationReply, AutomationRefusal> {
        // 1. Authenticated local owner only; nothing is revealed otherwise.
        if !is_local_owner(viewer) {
            return Err(refusal(
                request_id,
                codes::INPUT_NOT_ALLOWED,
                "guarded terminal operations are available only to the local owner",
            ));
        }
        let Some(cell) = self.cell_for_terminal(terminal_id) else {
            // 2. The execution is gone: durable replay or conflict still
            // answers a retried request; nothing new is admitted.
            if let Some(retired) = self.retired(terminal_id) {
                let replay = replay_lookup(
                    &retired.store,
                    &retired.owner_key,
                    request_id,
                    execution_id,
                    expected_revision,
                    prompt,
                )
                .map_err(|r| refusal(request_id, r.code, r.message))?;
                if let Some(reply) = replay {
                    return Ok(reply);
                }
                return Err(refusal(
                    request_id,
                    codes::STALE_EXECUTION,
                    format!(
                        "execution {} has ended; it is no longer the terminal's current execution",
                        retired.execution_id.as_str()
                    ),
                ));
            }
            return Err(refusal(
                request_id,
                codes::NATIVE_CAPABILITY_UNAVAILABLE,
                "this terminal is not an automation terminal",
            ));
        };
        let mut state = cell.state.lock().unwrap_or_else(|p| p.into_inner());
        let grant_epoch = grant.filter(|g| g.is_valid()).map(InputGrant::epoch);
        let result = submit_locked(
            &cell,
            &mut state,
            request_id,
            execution_id,
            expected_revision,
            prompt,
            grant_epoch,
        );
        drop(state);
        result.map_err(|r| refusal(request_id, r.code, r.message))
    }
}

/// The durable dedup lookup for one request: `Some(reply)` replays the
/// recorded reply/result, a different operation under the same request ID
/// is a conflict, and `None` means a new request.
fn replay_lookup(
    store: &AutomationStore,
    owner_key: &str,
    request_id: Uuid,
    execution_id: &ExecutionId,
    expected_revision: u64,
    prompt: Option<&str>,
) -> Result<Option<OperationReply>, Refusal> {
    let operation = if prompt.is_some() {
        Operation::SubmitTurn
    } else {
        Operation::CloseExecution
    };
    let digest = operation_digest(
        operation,
        execution_id,
        expected_revision,
        prompt.unwrap_or(""),
    )
    .map_err(|_| Refusal::new(codes::INVALID_REQUEST, "invalid revision"))?;
    match store.lookup_request(owner_key, execution_id, request_id, &digest) {
        Ok(RequestLookup::Missing) => Ok(None),
        Ok(RequestLookup::Conflict) => Err(Refusal::new(
            codes::IDEMPOTENCY_CONFLICT,
            "this request ID was already used for a different operation",
        )),
        Ok(RequestLookup::Replay { response, result }) => {
            let mut frames = Vec::new();
            if let StoredReply::Accepted(frame) = response {
                frames.push(*frame);
            }
            frames.extend(result);
            Ok(Some(OperationReply::Replay(frames)))
        }
        Err(error) => Err(Refusal::new("storage_error", error.to_string())),
    }
}

/// The next command-wide turn ordinal.
pub(super) fn next_turn_ordinal(
    store: &AutomationStore,
    command: &CommandId,
) -> Result<u64, String> {
    let mut cursor = None;
    let mut last = 0;
    loop {
        let page = store
            .list_turns(command, cursor.as_deref(), Some(200))
            .map_err(store_error)?;
        if let Some(t) = page.items.last() {
            last = t.ordinal;
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(last + 1),
        }
    }
}

/// Reserve one execution row bound to the command's current attempt, with
/// its initial (revision 0) snapshot. Shared by interactive and headless
/// launches so both are recovered the same way after a crash.
pub(super) fn reserve_execution_record(
    host: &AutomationHost,
    execution_id: &ExecutionId,
    snapshot: &AutomationSnapshot,
    profile_binding: ExecutionProfileBinding,
    runtime: String,
    requested_native: Option<NativeSessionId>,
    timeouts: &AutomationTimeouts,
) -> Result<(), String> {
    let previous = host
        .store
        .current_execution(&host.command_id)
        .map_err(store_error)?;
    let ordinal = previous.map_or(1, |p| p.execution_ordinal + 1);
    let record = ExecutionRecord {
        execution_id: execution_id.clone(),
        command_id: host.command_id.clone(),
        attempt: host.attempt,
        owner_key: host.owner_key.clone(),
        execution_ordinal: ordinal,
        terminal_id: None,
        provider: snapshot.provider.clone(),
        runtime,
        provider_version: None,
        image_digest: None,
        profile_binding,
        requested_native_session_id: requested_native,
        native_session_id: None,
        automation_mode: Some(snapshot.automation_mode),
        minimum_status_support: Some(snapshot.minimum_status_support),
        native_start_mode: snapshot.native_start_mode,
        workspace_access: snapshot.workspace_access,
        timeouts: timeouts.clone(),
        snapshot: Some(snapshot.clone()),
        revision: snapshot.revision,
        active_turn_id: None,
        launch_phase: LaunchPhase::Reserved,
        process_started_at: None,
        created_at: chrono::Utc::now(),
        ended_at: None,
        end_failure: None,
        final_assurance: None,
        pending_events: None,
        pending_event_cursor: None,
    };
    host.store
        .reserve_execution(NewExecution { record })
        .map_err(store_error)
}
/// Dedup, admission, reservation, publication and the single guarded write,
/// under the cell lock. `prompt` is `None` for close.
fn submit_locked(
    cell: &Arc<Cell>,
    state: &mut CellState,
    request_id: Uuid,
    execution_id: &ExecutionId,
    expected_revision: u64,
    prompt: Option<&str>,
    grant_epoch: Option<u64>,
) -> Result<OperationReply, Refusal> {
    // 2. Dedup before any current-state check.
    if let Some(reply) = replay_lookup(
        &state.host.store,
        &state.host.owner_key,
        request_id,
        execution_id,
        expected_revision,
        prompt,
    )? {
        return Ok(reply);
    }
    let operation = if prompt.is_some() {
        Operation::SubmitTurn
    } else {
        Operation::CloseExecution
    };
    let digest = operation_digest(
        operation,
        execution_id,
        expected_revision,
        prompt.unwrap_or(""),
    )
    .map_err(|_| Refusal::new(codes::INVALID_REQUEST, "invalid revision"))?;
    if state.persist_failed {
        return Err(Refusal::new(
            codes::AGENT_NOT_READY,
            "the execution's durable state could not be committed; input is stopped",
        ));
    }
    // 3. Current-state checks for a new request.
    state.admission.check_operation(
        execution_id,
        grant_epoch,
        expected_revision,
        prompt.is_none(),
    )?;
    let Some(prompt) = prompt else {
        return close_locked(cell, state, request_id, &digest);
    };
    // 4. Validate and encode the complete task.
    let encoded = state.adapter.encode_prompt(prompt).map_err(|e| match e {
        PromptRefusal::Invalid(m) => Refusal::new(codes::INVALID_REQUEST, m),
        PromptRefusal::Unavailable(m) => Refusal::new(codes::NATIVE_CAPABILITY_UNAVAILABLE, m),
    })?;
    // Reserve the writer's whole-operation capacity before acceptance: a
    // stalled writer is genuine backpressure, never an accepted task.
    let permit = match state.writer.as_ref().map(|w| w.clone().try_reserve_owned()) {
        Some(Ok(permit)) => permit,
        Some(Err(tokio::sync::mpsc::error::TrySendError::Full(_))) => {
            return Err(Refusal::new(
                codes::INPUT_BACKPRESSURE,
                "the agent's input writer is still delivering an earlier operation",
            ))
        }
        _ => {
            return Err(Refusal::new(
                codes::INPUT_BACKPRESSURE,
                "the agent's input writer is not available",
            ))
        }
    };
    // 5. Reserve turn + dedup + submitting snapshot atomically.
    let before = state.admission.snapshot().clone();
    let turn_id = TurnId::new();
    let _committed_by_reservation =
        state
            .admission
            .accept_submit(turn_id.clone(), encoded.digest().to_owned(), grant_epoch);
    let snapshot = state.admission.snapshot().clone();
    let turn = TurnRecord {
        request_id,
        turn_id: turn_id.clone(),
        execution_id: execution_id.clone(),
        command_id: state.host.command_id.clone(),
        attempt: state.host.attempt,
        ordinal: state.next_ordinal,
        native_session_id: snapshot.native_session_id.clone(),
        native_turn_id: None,
        accepted_at: chrono::DateTime::from_timestamp_millis(chrono::Utc::now().timestamp_millis())
            .expect("valid time"),
        started_at: None,
        completed_at: None,
        accepted_revision: before.revision,
        status_support: before.status_support,
        status_source: before.status_source,
        result: None,
    };
    let frame = match state.host.store.reserve_turn(
        Reservation {
            turn: turn.clone(),
            owner_key: state.host.owner_key.clone(),
            digest,
            snapshot: snapshot.clone(),
            publication: None,
        },
        before.revision,
    ) {
        Ok(frame) => frame,
        Err(e) => {
            state.admission.abandon_reservation(&turn_id, before);
            persistence_failed(cell, state, &e.to_string());
            return Err(Refusal::new(
                codes::AGENT_NOT_READY,
                format!("the turn could not be reserved: {e}"),
            ));
        }
    };
    state.next_ordinal += 1;
    state.turns.push(turn);
    // Wire order (A2): the durable acceptance first, then the submitting
    // state it caused; only then may the writer see the task.
    if let Some((id, hub)) = terminal(state) {
        let _ = hub.publish_automation_frame(&id, frame);
    }
    publish_state(state);
    // The pump authorizes the write at actual writer ingress.
    permit.send(PendingWrite {
        turn_id,
        bytes: encoded.into_bytes(),
    });
    Ok(OperationReply::PendingPublication)
}

/// The grant epoch that may deliver now: the current hub holder, or the
/// foreground command's own input path (no terminal; epoch 0).
fn current_epoch(state: &CellState, submitted: Option<u64>) -> Option<u64> {
    match terminal(state) {
        Some((id, hub)) => hub.input_holder_epoch(&id),
        None if state.terminal.is_none() => submitted,
        None => None,
    }
}

/// The writer pump's ingress decision for one queued operation, under the
/// cell lock, immediately before the bytes reach the engine writer: every
/// execution/source/grant/cancel/update fence is rechecked and
/// `WriteStarted` must be durable. A refusal ends the turn with zero bytes.
fn write_locked(
    cell: &Arc<Cell>,
    state: &mut CellState,
    write: PendingWrite,
    engine: &EngineInput,
) {
    if state.finished {
        return;
    }
    let submitted = state.admission.active_turn_grant(&write.turn_id);
    let epoch = current_epoch(state, submitted);
    let effects = match state.admission.writer_ingress(&write.turn_id, epoch) {
        Ok(effects) => effects,
        Err((_refusal, effects)) => {
            // Invalidation won: the queued permit is revoked, zero bytes.
            apply_effects(cell, state, effects);
            return;
        }
    };
    if let Err(e) = state.host.store.mark_delivery(
        &write.turn_id,
        DeliveryPhase::WriteStarted,
        &cell.execution_id,
    ) {
        persistence_failed(cell, state, &e.to_string());
        return;
    }
    apply_effects(cell, state, effects);
    if state.persist_failed {
        return;
    }
    let ok = engine.send(write.bytes).is_ok();
    if ok {
        if let Err(e) = state.host.store.mark_delivery(
            &write.turn_id,
            DeliveryPhase::Written,
            &cell.execution_id,
        ) {
            persistence_failed(cell, state, &e.to_string());
        }
        state.ack_deadline = Some((
            write.turn_id.clone(),
            Instant::now() + state.timeouts.startup.to_duration(),
        ));
    }
    let effects = state.admission.write_finished(&write.turn_id, ok);
    apply_effects(cell, state, effects);
}

/// The engine-side input channel the pump writes to.
type EngineInput = tokio::sync::mpsc::UnboundedSender<Vec<u8>>;

/// Run the writer pump for one execution until its queue closes.
fn spawn_writer_pump(
    cell: Arc<Cell>,
    mut queue: tokio::sync::mpsc::Receiver<PendingWrite>,
    engine: EngineInput,
) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    // Weak: the cell owns the queue's sender, so a strong reference here
    // would keep the cell (and its store and lease) alive forever.
    let weak = Arc::downgrade(&cell);
    drop(cell);
    runtime.spawn(async move {
        while let Some(write) = queue.recv().await {
            let Some(cell) = weak.upgrade() else {
                return;
            };
            let mut state = cell.state.lock().unwrap_or_else(|p| p.into_inner());
            write_locked(&cell, &mut state, write, &engine);
        }
    });
}

/// A durable commit failed: stop input and the execution; nothing further
/// is published from uncommitted in-memory state.
fn persistence_failed(cell: &Arc<Cell>, state: &mut CellState, error: &str) {
    if !state.persist_failed {
        tracing::error!(execution = %cell.execution_id.as_str(), %error, "automation state could not be committed; stopping the execution");
    }
    state.persist_failed = true;
    state.writer = None;
    if let Some(events) = &state.host.events {
        events.abort(error);
    }
    {
        state.stop_failure = Some(super::admission::failure(
            "storage_error",
            &format!("the execution's durable state could not be committed: {error}"),
        ));
    }
    release_bootstrap(state);
    cell.stop.cancel();
}

fn close_locked(
    cell: &Arc<Cell>,
    state: &mut CellState,
    request_id: Uuid,
    digest: &str,
) -> Result<OperationReply, Refusal> {
    let before = state.admission.snapshot().clone();
    let effects = state.admission.accept_close();
    let snapshot = state.admission.snapshot().clone();
    let request = CloseExecutionRequest {
        request_id,
        execution_id: cell.execution_id.clone(),
        expected_revision: before.revision,
    };
    if let Err(e) =
        state
            .host
            .store
            .reserve_close(&state.host.owner_key, &request, digest, &snapshot, None)
    {
        persistence_failed(cell, state, &e.to_string());
        return Err(Refusal::new(
            codes::AGENT_NOT_READY,
            format!("the close could not be reserved: {e}"),
        ));
    }
    let _ = effects;
    state.close_request = Some(request_id);
    publish_state(state);
    cell.stop.cancel();
    Ok(OperationReply::PendingPublication)
}

fn terminal(state: &CellState) -> Option<(TerminalId, Arc<TerminalHub>)> {
    state
        .terminal
        .as_ref()
        .and_then(|(id, hub)| hub.upgrade().map(|h| (id.clone(), h)))
}

fn emit(state: &CellState, payload: EventPayload) {
    if state.persist_failed {
        return;
    }
    if let Some(events) = &state.host.events {
        events.emit(payload);
    }
}

/// Publish the current snapshot to the terminal and the event stream
/// (never after a failed durable commit).
fn publish_state(state: &CellState) {
    if state.persist_failed {
        return;
    }
    let snapshot = state.admission.snapshot().clone();
    if let Some((id, hub)) = terminal(state) {
        let _ = hub.update_automation(&id, snapshot.clone());
    }
    emit(
        state,
        EventPayload::AgentState {
            snapshot: Box::new(snapshot),
        },
    );
}

/// Redact launch secrets from everything an effect would persist or publish.
fn redact_effect(redaction: &RedactionSet, effect: Effect) -> Effect {
    if redaction.is_empty() {
        return effect;
    }
    let failure = |f: AutomationFailure| AutomationFailure {
        code: f.code,
        message: redaction.redact(&f.message),
    };
    match effect {
        Effect::TurnCompleted {
            turn_id,
            mut result,
        } => {
            result.final_message = result.final_message.map(|m| redaction.redact(&m));
            result.failure = result.failure.map(failure);
            Effect::TurnCompleted { turn_id, result }
        }
        Effect::Stop(f) => Effect::Stop(failure(f)),
        other => other,
    }
}

/// Persist and publish one step's effects. Every durable commit must
/// succeed before anything derived from it is published; a failed commit
/// stops input and the execution (fail closed). Returns whether the initial
/// task is now deliverable.
fn apply_effects(cell: &Arc<Cell>, state: &mut CellState, effects: Vec<Effect>) -> bool {
    if state.persist_failed {
        return false;
    }
    let effects: Vec<Effect> = effects
        .into_iter()
        .map(|e| {
            let mut e = redact_effect(&state.redaction, e);
            if let Some(events) = &state.host.events {
                e = redact_effect(&events.redaction(), e);
            }
            if let Effect::TurnCompleted { result, .. } = &mut e {
                let _ = results::bound_turn_result(result);
            }
            e
        })
        .collect();
    let mut deliver_initial = false;
    let state_changed = effects.contains(&Effect::State);
    let completion = effects.iter().find_map(|e| match e {
        Effect::TurnCompleted { turn_id, result } => Some((turn_id.clone(), result.clone())),
        _ => None,
    });
    // Acknowledgement first: the store's delivery marker is monotone.
    for effect in &effects {
        if let Effect::TurnStarted {
            turn_id,
            native_turn_id,
            source,
        } = effect
        {
            let store = state.host.store.clone();
            let snapshot = state.admission.snapshot().clone();
            let committed = store
                .mark_delivery(turn_id, DeliveryPhase::Acknowledged, &cell.execution_id)
                .and_then(|()| {
                    store.observe_turn(
                        turn_id,
                        &cell.execution_id,
                        snapshot.native_session_id.as_ref(),
                        native_turn_id.as_deref(),
                        Some(chrono::Utc::now()),
                    )
                });
            state.ack_deadline = None;
            release_bootstrap(state);
            if let Err(e) = committed {
                persistence_failed(cell, state, &e.to_string());
                return false;
            }
            let request_id = state
                .turns
                .iter()
                .find(|t| &t.turn_id == turn_id)
                .map(|t| t.request_id);
            let Some(request_id) = request_id else {
                persistence_failed(cell, state, "missing required turn for observation");
                return false;
            };
            emit(
                state,
                EventPayload::AgentTurnStarted {
                    request_id,
                    execution_id: cell.execution_id.clone(),
                    turn_id: turn_id.clone(),
                    native_session_id: snapshot.native_session_id.clone(),
                    native_turn_id: native_turn_id.clone(),
                    status_support: snapshot.status_support,
                    status_source: *source,
                },
            );
            if let Some(t) = state.turns.iter_mut().find(|t| &t.turn_id == turn_id) {
                t.native_turn_id = native_turn_id.clone();
            }
        }
    }
    let snapshot = state.admission.snapshot().clone();
    // Whether the turn result committed (and may therefore be published).
    let mut completion_committed = false;
    if let Some((turn_id, result)) = &completion {
        let committed = (|| -> Result<(), crate::data::error::DataError> {
            let request_id = state
                .turns
                .iter()
                .find(|t| &t.turn_id == turn_id)
                .ok_or_else(|| {
                    crate::data::error::DataError::Other("missing required turn".into())
                })?
                .request_id;
            let projection = state
                .host
                .events
                .as_ref()
                .map(|events| {
                    events.reserve(vec![
                        EventPayload::AgentTurnCompleted {
                            request_id,
                            execution_id: cell.execution_id.clone(),
                            turn_id: turn_id.clone(),
                            result: result.clone(),
                        },
                        EventPayload::AgentState {
                            snapshot: Box::new(snapshot.clone()),
                        },
                    ])
                })
                .transpose()?;
            state
                .host
                .store
                .complete_turn(turn_id, result, &snapshot, projection.as_ref())?;
            if let (Some(events), Some(projection)) = (&state.host.events, projection) {
                events.publish(&projection)?;
                state
                    .host
                    .store
                    .clear_pending_events(&cell.execution_id, projection.cursor)?;
            }
            Ok(())
        })();
        match committed {
            Ok(()) => {
                completion_committed = true;
                let t = state
                    .turns
                    .iter_mut()
                    .find(|t| &t.turn_id == turn_id)
                    .expect("validated turn");
                t.result = Some(result.clone());
                t.completed_at = Some(chrono::Utc::now());
            }
            Err(e) => persistence_failed(cell, state, &e.to_string()),
        }
        if state
            .ack_deadline
            .as_ref()
            .is_some_and(|(t, _)| t == turn_id)
        {
            state.ack_deadline = None;
        }
        release_bootstrap(state);
    } else if state_changed && snapshot.state != crate::data::automation::AutomationState::Ended {
        let committed = state
            .host
            .store
            .read_execution(&cell.execution_id)
            .and_then(|current| {
                if current.revision + 1 == snapshot.revision {
                    state
                        .host
                        .store
                        .transition_execution(current.revision, &snapshot, None)
                } else if current.snapshot.as_ref() == Some(&snapshot) {
                    Ok(())
                } else {
                    Err(crate::data::error::DataError::Other(
                        "stale_revision".into(),
                    ))
                }
            });
        if let Err(e) = committed {
            persistence_failed(cell, state, &e.to_string());
        }
    }
    if state_changed && snapshot.state != crate::data::automation::AutomationState::Ended {
        if completion_committed {
            if let Some((id, hub)) = terminal(state) {
                let _ = hub.update_automation(&id, snapshot.clone());
            }
        } else {
            publish_state(state);
        }
    }
    for effect in effects {
        match effect {
            Effect::Identity(id) => {
                emit(
                    state,
                    EventPayload::AgentIdentity {
                        execution_id: cell.execution_id.clone(),
                        native_session_id: id.clone(),
                    },
                );
                if let Some(hook) = &state.identity_hook {
                    hook(id);
                }
            }
            Effect::TurnCompleted { turn_id, result } => {
                if !completion_committed || state.persist_failed {
                    continue;
                }
                let request_id = state
                    .turns
                    .iter()
                    .find(|t| t.turn_id == turn_id)
                    .map(|t| t.request_id)
                    .expect("completion transaction validated required turn");
                if let Some((id, hub)) = terminal(state) {
                    let _ = hub.publish_automation_frame(
                        &id,
                        TerminalFrame::TurnResult {
                            request_id,
                            execution_id: cell.execution_id.clone(),
                            turn_id,
                            result,
                        },
                    );
                }
            }
            Effect::StartupSatisfied => {
                deliver_initial = state.initial_task.is_some() && !state.persist_failed;
            }
            Effect::Stop(failure) => {
                if state.stop_failure.is_none() {
                    state.stop_failure = Some(failure);
                }
                release_bootstrap(state);
                cell.stop.cancel();
            }
            Effect::State | Effect::TurnStarted { .. } => {}
        }
    }
    deliver_initial
}

fn release_bootstrap(state: &mut CellState) {
    if let Some(grant) = state.bootstrap.take() {
        grant.release();
    }
}

/// Submit the retained initial task exactly once through the guarded path.
fn deliver_initial(cell: &Arc<Cell>) {
    let mut state = cell.state.lock().unwrap_or_else(|p| p.into_inner());
    let Some(task) = state.initial_task.take() else {
        return;
    };
    let request = state.initial_request;
    let revision = state.admission.revision();
    let epoch = match &state.bootstrap {
        Some(grant) if grant.is_valid() => Some(grant.epoch()),
        Some(_) => None,
        // Foreground: the command itself holds the only input path.
        None if state.terminal.is_none() => Some(0),
        None => None,
    };
    let execution = cell.execution_id.clone();
    let result = submit_locked(
        cell,
        &mut state,
        request,
        &execution,
        revision,
        Some(&task),
        epoch,
    );
    if let Err(refusal) = result {
        // The initial task could not be admitted: the strict launch fails
        // without any task bytes.
        if state.stop_failure.is_none() {
            state.stop_failure = Some(refusal.failure());
        }
        let effects = state.admission.cancel();
        apply_effects(cell, &mut state, effects);
        release_bootstrap(&mut state);
        drop(state);
        cell.stop.cancel();
    }
}

impl TerminalAutomation for ExecutionController {
    fn submit(
        &self,
        terminal_id: &TerminalId,
        viewer: &ViewerIdentity,
        grant: Option<&InputGrant>,
        request: SubmitTurnRequest,
    ) -> Result<OperationReply, AutomationRefusal> {
        self.operation(
            terminal_id,
            viewer,
            grant,
            request.request_id,
            &request.execution_id,
            request.expected_revision,
            Some(&request.prompt),
        )
    }

    fn close(
        &self,
        terminal_id: &TerminalId,
        viewer: &ViewerIdentity,
        grant: Option<&InputGrant>,
        request: CloseExecutionRequest,
    ) -> Result<OperationReply, AutomationRefusal> {
        self.operation(
            terminal_id,
            viewer,
            grant,
            request.request_id,
            &request.execution_id,
            request.expected_revision,
            None,
        )
    }

    fn with_control_change(
        &self,
        terminal_id: &TerminalId,
        _viewer: &ViewerIdentity,
        action: &mut dyn FnMut() -> Result<(), TerminalRefusal>,
    ) -> Result<(), TerminalRefusal> {
        let Some(cell) = self.cell_for_terminal(terminal_id) else {
            return action();
        };
        let mut state = cell.state.lock().unwrap_or_else(|p| p.into_inner());
        let result = action();
        if !state.finished {
            let effects = state.admission.control_changed(None);
            apply_effects(&cell, &mut state, effects);
        }
        result
    }

    fn with_manual_input(
        &self,
        terminal_id: &TerminalId,
        _viewer: &ViewerIdentity,
        _grant: &InputGrant,
        _bytes: &[u8],
        action: &mut dyn FnMut() -> Result<(), TerminalRefusal>,
    ) -> Result<(), TerminalRefusal> {
        let Some(cell) = self.cell_for_terminal(terminal_id) else {
            return action();
        };
        let mut state = cell.state.lock().unwrap_or_else(|p| p.into_inner());
        match state.admission.manual_input() {
            Ok(effects) => {
                apply_effects(&cell, &mut state, effects);
                if state.persist_failed {
                    Err(TerminalRefusal::InputNotAllowed)
                } else {
                    action()
                }
            }
            Err(_) => Err(TerminalRefusal::InputNotAllowed),
        }
    }
}

/// The owner's handle on one execution. Clones share the execution.
#[derive(Clone)]
pub struct OwnedExecution {
    controller: Arc<ExecutionController>,
    cell: Arc<Cell>,
}

impl std::fmt::Debug for OwnedExecution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedExecution")
            .field("execution_id", &self.cell.execution_id)
            .finish_non_exhaustive()
    }
}

impl OwnedExecution {
    pub fn execution_id(&self) -> &ExecutionId {
        &self.cell.execution_id
    }

    pub fn controller(&self) -> &Arc<ExecutionController> {
        &self.controller
    }

    pub fn snapshot(&self) -> AutomationSnapshot {
        self.state().admission.snapshot().clone()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, CellState> {
        self.cell.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Fires when the controller wants the execution stopped (startup or
    /// acknowledgement expiry, protocol failure, close, cancel).
    pub fn stop_requested(&self) -> CancellationToken {
        self.cell.stop.clone()
    }

    /// Called with the native identity when it is first observed.
    pub fn on_identity(&self, hook: Arc<dyn Fn(NativeSessionId) + Send + Sync>) {
        self.state().identity_hook = Some(hook);
    }

    /// The runtime launch operation begins: start the absolute startup
    /// deadline here (queue time and image preparation are excluded).
    pub fn launching(&self) {
        let mut state = self.state();
        if let Err(e) = state.host.store.mark_launching(&self.cell.execution_id) {
            persistence_failed(&self.cell, &mut state, &e.to_string());
        }
        state.startup_deadline = Instant::now() + state.timeouts.startup.to_duration();
        drop(state);
        self.spawn_timers();
    }

    /// The provider process is running.
    pub fn running(&self) {
        let mut state = self.state();
        if let Err(error) = state.host.store.mark_running(&self.cell.execution_id) {
            persistence_failed(&self.cell, &mut state, &error.to_string());
        }
    }

    /// Bind the engine-side input writer (the agent's stdin channel). Guarded
    /// tasks reach it only through this execution's writer pump, which holds
    /// capacity for one whole operation and authorizes each write at ingress.
    pub fn bind_writer(&self, writer: tokio::sync::mpsc::UnboundedSender<Vec<u8>>) {
        let (queue, pending) = tokio::sync::mpsc::channel(1);
        self.state().writer = Some(queue);
        spawn_writer_pump(self.cell.clone(), pending, writer);
    }

    /// Register a lifecycle consumer task the owner joins before freezing.
    pub fn register_drain(&self, task: tokio::task::JoinHandle<()>) {
        self.cell
            .drains
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(task);
    }

    /// Join every registered lifecycle consumer through its output
    /// boundary within `budget`. On expiry the remaining consumers are
    /// aborted (the finished cell fences anything they would still report)
    /// and the output is incomplete.
    pub async fn join_drains(&self, budget: Duration) -> OutputStatus {
        let tasks: Vec<_> =
            std::mem::take(&mut *self.cell.drains.lock().unwrap_or_else(|p| p.into_inner()));
        let deadline = tokio::time::Instant::now() + budget;
        let mut status = OutputStatus::Complete;
        for mut task in tasks {
            match tokio::time::timeout_at(deadline, &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => status = OutputStatus::Incomplete,
                Err(_) => {
                    task.abort();
                    status = OutputStatus::Incomplete;
                }
            }
        }
        status
    }

    /// The launch's shared redaction set: applied to every durable result,
    /// failure message and frame this execution produces.
    pub fn set_redaction(&self, redaction: RedactionSet) {
        self.state().redaction = redaction;
    }

    /// Bind the registered terminal. With an initial task, the command
    /// holds a bootstrap input grant until initial delivery settles.
    pub fn bind_terminal(&self, id: &TerminalId, hub: &Arc<TerminalHub>) {
        // Any hub hosting an automation terminal routes raw input from
        // transports without typed operations (binary attach) through this
        // controller's B4 gate.
        hub.set_raw_input_gate(self.controller.clone());
        let mut state = self.state();
        state.terminal = Some((id.clone(), Arc::downgrade(hub)));
        if state.initial_task.is_some() {
            match hub.take_control(id) {
                Ok(grant) => state.bootstrap = Some(grant),
                Err(error) => {
                    state.stop_failure = Some(super::admission::failure(
                        codes::AGENT_NOT_READY,
                        &format!("cannot acquire initial input control: {error}"),
                    ));
                    self.cell.stop.cancel();
                }
            }
        }
        self.controller
            .terminals
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.clone(), self.cell.execution_id.clone());
    }

    /// A Layer 1 observation for this execution.
    pub fn observe(&self, observations: Vec<Observation>) {
        self.controller.observe(observations);
    }

    /// Gate one chunk of direct (CLI/TUI) raw input.
    pub fn manual_input(&self) -> Result<(), Refusal> {
        let mut state = self.state();
        let effects = state.admission.manual_input()?;
        apply_effects(&self.cell, &mut state, effects);
        if state.persist_failed {
            return Err(Refusal::new(
                "storage_error",
                "automation persistence failed",
            ));
        }
        Ok(())
    }

    /// Cancel this execution (the command's own cancellation path).
    pub fn cancel(&self) {
        self.controller.cancel(&self.cell.execution_id);
    }

    /// Begin a credential update (B8): `credential_updating` or
    /// `restarting`; the underlying turn/blocked state is retained.
    pub fn begin_update(&self, restarting: bool) -> Result<(), Refusal> {
        let mut state = self.state();
        let effects = state.admission.begin_update(restarting)?;
        apply_effects(&self.cell, &mut state, effects);
        Ok(())
    }

    /// End a credential update; fresh readiness is required afterwards.
    pub fn end_update(&self) {
        let mut state = self.state();
        let effects = state.admission.end_update();
        apply_effects(&self.cell, &mut state, effects);
    }

    fn spawn_timers(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        // Weak: the timers never extend the execution's lifetime past its
        // owners, so an execution nobody holds releases its store at once.
        let weak = Arc::downgrade(&self.cell);
        let cancelled = self.cell.timers.clone();
        runtime.spawn(async move {
            loop {
                tokio::select! {
                    _ = cancelled.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
                let Some(cell) = weak.upgrade() else {
                    return;
                };
                let mut state = cell.state.lock().unwrap_or_else(|p| p.into_inner());
                if state.finished {
                    return;
                }
                let now = Instant::now();
                if !state.admission.startup_satisfied() && now >= state.startup_deadline {
                    let effects = state.admission.startup_expired();
                    apply_effects(&cell, &mut state, effects);
                }
                if let Some((turn, at)) = state.ack_deadline.clone() {
                    if now >= at {
                        state.ack_deadline = None;
                        let effects = state.admission.acknowledgement_expired(&turn);
                        apply_effects(&cell, &mut state, effects);
                    }
                }
                drop(state);
            }
        });
    }

    /// Whether a close request ended this execution.
    pub fn closed_by_request(&self) -> bool {
        self.state().close_request.is_some()
    }

    /// The primary failure the controller recorded (if any).
    pub fn stop_failure(&self) -> Option<AutomationFailure> {
        self.state().stop_failure.clone()
    }

    /// Finalize: end any outstanding turn without replay, persist `ended`
    /// and the frozen assurance, publish the final state. For a terminal
    /// without a pending close the one logical exit is published now; with a
    /// close request it follows the stored command result
    /// ([`ExecutionController::command_finalized`]). Owners whose command
    /// result is stored afterwards use [`Self::finish_deferred`].
    pub fn finish(&self, exit: ExitFacts) -> FinishedExecution {
        let code = exit.exit_code;
        let finished = self.finish_deferred(exit);
        if !self.closed_by_request() {
            self.publish_exit(if finished.failure.is_some() {
                1
            } else {
                code.unwrap_or(1)
            });
        }
        finished
    }

    /// Freeze the execution's result without publishing the terminal's
    /// exit: the owner stores the command result first and then calls
    /// [`Self::publish_exit`] (foreground) or the queue worker calls
    /// [`ExecutionController::command_finalized`] (daemon). The execution's
    /// liveness lease is kept until then, so recovery never races the
    /// owner's own finalization.
    pub fn finish_deferred(&self, mut exit: ExitFacts) -> FinishedExecution {
        self.cell.timers.cancel();
        let mut state = self.state();
        if state.redaction.is_retired() {
            exit.output_status = OutputStatus::Incomplete;
        }
        let effects = state.admission.process_ended();
        apply_effects(&self.cell, &mut state, effects);
        let effects = state.admission.end();
        apply_effects(&self.cell, &mut state, effects);
        release_bootstrap(&mut state);
        let snapshot = state.admission.snapshot().clone();
        let facts = ExecutionFacts {
            execution_id: self.cell.execution_id.clone(),
            native_session_id: snapshot.native_session_id.clone(),
            native_start_mode: snapshot.native_start_mode,
            workspace_access: snapshot.workspace_access,
            established: state.admission.established_support(),
        };
        let turns = state.turns.clone();
        let mut automation =
            results::aggregate(&turns, &facts, exit.output_status, exit.cleanup_status);
        let mut execution_failure = state.stop_failure.clone();
        if execution_failure.is_none() {
            if let Some(e) = &exit.wait_error {
                execution_failure = Some(super::admission::failure(codes::RUNTIME_WAIT_FAILED, e));
            }
        }
        let mut failure = results::command_failure(
            &turns,
            &FailureInputs {
                execution_failure,
                cancelled: state.cancelled,
                output_status: Some(exit.output_status),
                cleanup_status: Some(exit.cleanup_status),
            },
        );
        if state.persist_failed {
            // Nothing uncommitted is reported as a verified result.
            automation.completion_verified = false;
            failure = state.stop_failure.clone();
        }
        if state.cancelled && failure.is_none() {
            failure = Some(super::admission::failure(codes::CANCELLED, "cancelled"));
        }
        let saved = if state.persist_failed {
            Err(crate::data::error::DataError::Other(
                "automation persistence already failed".into(),
            ))
        } else {
            (|| -> Result<(), crate::data::error::DataError> {
                let projection = state
                    .host
                    .events
                    .as_ref()
                    .map(|events| {
                        events.reserve(vec![EventPayload::AgentState {
                            snapshot: Box::new(snapshot.clone()),
                        }])
                    })
                    .transpose()?;
                state.host.store.finish_execution_at(
                    &snapshot,
                    &automation,
                    failure.as_ref(),
                    projection.as_ref(),
                )?;
                failure = state
                    .host
                    .store
                    .read_execution(&self.cell.execution_id)?
                    .end_failure;
                state.cancelled |= failure.as_ref().is_some_and(|f| f.code == codes::CANCELLED);
                if let (Some(events), Some(projection)) = (&state.host.events, projection) {
                    events.publish(&projection)?;
                    state
                        .host
                        .store
                        .clear_pending_events(&self.cell.execution_id, projection.cursor)?;
                }
                Ok(())
            })()
        };
        if let Err(e) = saved {
            persistence_failed(&self.cell, &mut state, &e.to_string());
            automation.completion_verified = false;
            failure = state.stop_failure.clone();
        } else {
            if let Some((id, hub)) = terminal(&state) {
                let _ = hub.update_automation(&id, snapshot);
            }
            if let Some(sink) = &state.host.finished {
                sink(&automation, results::final_message(&turns).as_deref());
            }
        }
        state.finished = true;
        // The last turn's confirmed answer travels with every outcome
        // (failure and cancellation included); never an uncommitted one.
        let final_message = if state.persist_failed {
            None
        } else {
            results::final_message(&turns)
        };
        let cancelled = state.cancelled;
        let persist_failed = state.persist_failed;
        let lease = state.lease.take();
        let lease = if persist_failed {
            drop(lease);
            None
        } else {
            lease
        };
        let command = state.host.command_id.clone();
        drop(state);
        self.controller.hold_lease(&command, lease);
        if persist_failed {
            self.controller.forget(&self.cell.execution_id);
        }
        FinishedExecution {
            automation,
            failure,
            final_message,
            cancelled,
        }
    }

    /// After the command result is stored: publish the terminal's one
    /// logical exit, release the liveness lease and forget the execution.
    pub fn publish_exit(&self, exit_code: i32) {
        let state = self.state();
        let terminal = terminal(&state);
        let command = state.host.command_id.clone();
        drop(state);
        if let Some((id, hub)) = terminal {
            let _ = hub.finish_automation(&id, exit_code);
        }
        self.controller.release_leases(&command);
        self.controller.forget(&self.cell.execution_id);
    }

    /// The owning task died without finalizing (a panic in a long-lived
    /// process): drop ownership and the liveness lease so crash recovery
    /// can finalize the execution. Nothing is published or replayed.
    pub fn abandon(self) {
        self.cell.timers.cancel();
        self.cell.stop.cancel();
        let mut state = self.state();
        release_bootstrap(&mut state);
        state.writer = None;
        state.finished = true;
        state.lease = None;
        self.controller.release_leases(&state.host.command_id);
        drop(state);
        self.controller
            .cells
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.cell.execution_id);
        self.controller
            .terminals
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|_, e| e != &self.cell.execution_id);
    }

    /// The turns recorded so far (in-memory mirror of the durable rows).
    pub fn turns(&self) -> Vec<TurnRecord> {
        self.state().turns.clone()
    }

    /// The current phase of the outstanding turn, if any.
    pub fn active_turn_phase(&self) -> Option<TurnPhase> {
        self.state().admission.active_turn().map(|(_, p)| p)
    }
}

/// The binary attach transport's control changes and raw bytes pass the
/// same B4 decisions as the typed terminal operations.
impl crate::engine::terminal_hub::RawInputGate for ExecutionController {
    fn control_changed(&self, terminal: &TerminalId) {
        let Some(cell) = self.cell_for_terminal(terminal) else {
            return;
        };
        let mut state = cell.state.lock().unwrap_or_else(|p| p.into_inner());
        if !state.finished {
            let effects = state.admission.control_changed(None);
            apply_effects(&cell, &mut state, effects);
        }
    }

    fn admit_raw_input(&self, terminal: &TerminalId) -> bool {
        let Some(cell) = self.cell_for_terminal(terminal) else {
            return true;
        };
        let mut state = cell.state.lock().unwrap_or_else(|p| p.into_inner());
        match state.admission.manual_input() {
            Ok(effects) => {
                apply_effects(&cell, &mut state, effects);
                !state.persist_failed
            }
            Err(_) => false,
        }
    }
}

mod credentials;
pub use credentials::{
    CredentialConsumption, CredentialDriver, CredentialExecution, StagedCredential, UpdateFence,
};

#[cfg(unix)]
pub use credentials::HelperCredentialDriver;
