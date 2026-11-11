//! The typed operations behind `/v1/terminals` and `awman attach`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::data::daemon_state::CommandListener;
use crate::data::execution_event::{EventPayload, EventSource};
use crate::data::ids::{RunId, TaskId, TerminalId};
use crate::data::terminal::{
    AttachTarget, ControlHolder, DetachReason, TerminalClientFrame, TerminalFrame, TerminalOwner,
    TerminalRecord, TerminalState, ViewerIdentity,
};
use crate::data::wire::{List, ListQuery, WireError};
use crate::engine::terminal_hub::{
    InputGrant, TerminalHub, TerminalHubError, TerminalHubEvent, TerminalSubscription,
};

use super::policy::{AccessPolicy, InputGrants};

/// Why an operation was refused. The API maps these to its error table
/// (`404 terminal_not_found`, `410 terminal_gone`, `403 input_not_allowed`,
/// `409 control_held`, …).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TerminalRefusal {
    #[error("terminal not found")]
    NotFound,
    #[error("terminal gone")]
    Gone,
    #[error("terminal closed")]
    Closed,
    /// The owner's feature is not served on this listener.
    #[error("terminal is not available on the network listener")]
    NetworkRefused,
    /// `remote_input` and/or `allow_remote_input` is off, or `--read-only`.
    #[error("input is not allowed for this viewer")]
    InputNotAllowed,
    /// Another viewer holds the input grant.
    #[error("another viewer holds terminal control")]
    ControlHeld,
    /// The agent is not reading input; its bounded queue is full. The frame
    /// was dropped whole; the viewer keeps the grant and may retry.
    #[error("terminal input queue is full; the agent is not reading input")]
    InputBackpressure,
    /// `cmd_`/`task_` resolved to no running terminal (headless fallback).
    #[error("{0} has no terminal")]
    NoTerminal(String),
    /// `owner_id` is not a `cmd_`, `tab_`, `lease_`, `run_` or `task_` id.
    #[error("invalid owner_id `{0}`")]
    InvalidOwner(String),
    #[error(transparent)]
    Wire(#[from] WireError),
}

impl From<TerminalHubError> for TerminalRefusal {
    /// A well-formed `term_` id this hub does not know was issued by an
    /// earlier daemon process (ids are 128-bit random, the hub forgets
    /// everything on restart and expires old tombstones), so it answers
    /// `Gone`, not `NotFound`: "this terminal existed and will not return".
    fn from(error: TerminalHubError) -> Self {
        match error {
            TerminalHubError::NotFound | TerminalHubError::Gone => Self::Gone,
            TerminalHubError::Closed => Self::Closed,
            TerminalHubError::ControlHeld => Self::ControlHeld,
            TerminalHubError::InputBackpressure => Self::InputBackpressure,
        }
    }
}

/// Where `terminal_control`, `terminal_opened` and `terminal_closed` go: the
/// owner's event log (a command's event bus, an engine tab's host presenters,
/// a squad run's log). Implemented by the daemon runtime.
pub trait OwnerEventLog: Send + Sync {
    fn append(&self, owner: &TerminalOwner, payload: EventPayload);

    /// Append with the terminal's envelope `source` (0137: step, phase,
    /// container, `step_attempt`). Logs that stamp sources override this;
    /// the default drops the source.
    fn append_with_source(
        &self,
        owner: &TerminalOwner,
        source: Option<&EventSource>,
        payload: EventPayload,
    ) {
        let _ = source;
        self.append(owner, payload);
    }
}

/// Owner lookups `awman attach` needs that the hub cannot answer itself.
pub trait OwnerResolver: Send + Sync {
    /// The squad task's active run, if one is running.
    fn active_run(&self, task: &TaskId) -> Option<RunId>;
}

/// The one Layer 2 object every terminal transport uses.
pub struct TerminalOperations {
    hub: Arc<TerminalHub>,
    grants: Mutex<InputGrants>,
    /// Re-read per call so live config applies.
    policy: Arc<dyn Fn() -> AccessPolicy + Send + Sync>,
    events: Arc<dyn OwnerEventLog>,
    resolver: Arc<dyn OwnerResolver>,
}

impl std::fmt::Debug for TerminalOperations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalOperations").finish_non_exhaustive()
    }
}

impl TerminalOperations {
    pub fn new(
        hub: Arc<TerminalHub>,
        policy: Arc<dyn Fn() -> AccessPolicy + Send + Sync>,
        events: Arc<dyn OwnerEventLog>,
        resolver: Arc<dyn OwnerResolver>,
    ) -> Self {
        Self {
            hub,
            grants: Mutex::new(InputGrants::new()),
            policy,
            events,
            resolver,
        }
    }

    pub fn hub(&self) -> &Arc<TerminalHub> {
        &self.hub
    }

    /// The policy as configured right now.
    pub fn policy(&self) -> AccessPolicy {
        (self.policy)()
    }

    /// The owner whose terminals `task_…` names: the task's active run.
    pub fn owner_for_task(&self, task: &TaskId) -> Option<TerminalOwner> {
        self.resolver.active_run(task).map(TerminalOwner::Run)
    }

    /// Who holds the input grant on `id`, by name.
    pub fn control_holder(&self, id: &TerminalId) -> Option<ControlHolder> {
        if !self.hub.has_input_holder(id).unwrap_or(false) {
            return None;
        }
        self.grants
            .lock()
            .expect("terminal grants poisoned")
            .holder(id)
            .cloned()
    }

    /// `GET /v1/terminals?owner_id=…`: the viewer's admissible terminals,
    /// newest first, finished ones included while the owning record exists.
    pub fn list(
        &self,
        owner: Option<&TerminalOwner>,
        query: &ListQuery,
        viewer: &ViewerIdentity,
    ) -> Result<List<TerminalRecord>, TerminalRefusal> {
        let policy = self.policy();
        let items: Vec<_> = self
            .hub
            .list(owner)
            .into_iter()
            .filter(|record| policy.admit(record, viewer).is_ok())
            .collect();
        Ok(crate::command::daemon_api::core::page(items, query)?)
    }

    /// `GET /v1/terminals/{id}` without an upgrade.
    pub fn get(
        &self,
        id: &TerminalId,
        viewer: &ViewerIdentity,
    ) -> Result<TerminalRecord, TerminalRefusal> {
        let record = self.hub.get(id)?;
        self.policy().admit(&record, viewer)?;
        Ok(record)
    }

    /// Admit a viewer and subscribe. The session owns the grant lifecycle.
    pub fn open(
        self: &Arc<Self>,
        id: &TerminalId,
        viewer: ViewerIdentity,
    ) -> Result<TerminalSession, TerminalRefusal> {
        let record = self.get(id, &viewer)?;
        let subscription = self.hub.subscribe(id)?;
        Ok(TerminalSession::new(
            record,
            viewer,
            subscription,
            self.clone(),
        ))
    }

    /// `awman attach <id>`: `term_` is itself; `cmd_` is the command's
    /// current open terminal (or the `--step` one, matched on
    /// `source.step_name`); `task_` is the task's active run's open terminal.
    /// With no open terminal, the newest finished one is picked so a late
    /// attach still shows the final screen.
    pub fn resolve_target(
        &self,
        target: &AttachTarget,
        step: Option<&str>,
    ) -> Result<TerminalId, TerminalRefusal> {
        let owner = match target {
            AttachTarget::Terminal(id) => {
                self.hub.get(id)?;
                return Ok(id.clone());
            }
            AttachTarget::Command(id) => TerminalOwner::Command(id.clone()),
            AttachTarget::Task(task) => self
                .owner_for_task(task)
                .ok_or_else(|| TerminalRefusal::NoTerminal(task.as_str().to_owned()))?,
        };
        let candidates: Vec<_> = self
            .hub
            .list(Some(&owner))
            .into_iter()
            .filter(|record| {
                step.is_none_or(|step| {
                    record.source.as_ref().and_then(|s| s.step_name.as_deref()) == Some(step)
                })
            })
            .collect();
        candidates
            .iter()
            .find(|r| r.state == TerminalState::Open)
            .or_else(|| candidates.first())
            .map(|r| r.id.clone())
            .ok_or_else(|| TerminalRefusal::NoTerminal(owner.as_str().to_owned()))
    }

    /// Forward hub lifecycle events to the owner's log as `terminal_opened` /
    /// `terminal_closed`. Subscribes when first polled; the daemon uses
    /// [`Self::spawn_lifecycle_publisher`], which subscribes before it
    /// returns so no early registration is missed.
    pub async fn publish_lifecycle_events(self: Arc<Self>) {
        self.install_lifecycle_sink();
        std::future::pending::<()>().await;
    }

    /// Subscribe to hub lifecycle events now and forward them on a task.
    pub fn spawn_lifecycle_publisher(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        self.install_lifecycle_sink();
        let this = self.clone();
        tokio::spawn(async move {
            let _keep_alive = this;
            std::future::pending::<()>().await
        })
    }

    fn install_lifecycle_sink(self: &Arc<Self>) {
        // `Closed` carries only the id, and the record may already be
        // evicted when it arrives: remember each terminal's owner.
        let owners = Mutex::new(HashMap::<TerminalId, (TerminalOwner, Option<EventSource>)>::new());
        let events = self.events.clone();
        self.hub.set_lifecycle_sink(Arc::new(move |event| {
            let mut owners = owners.lock().unwrap_or_else(|p| p.into_inner());
            match event {
                TerminalHubEvent::Opened(record) => {
                    events.append_with_source(
                        &record.owner_id,
                        record.source.as_ref(),
                        EventPayload::TerminalOpened {
                            terminal_id: record.id.clone(),
                            agent: record.agent.clone(),
                            model: record.model.clone(),
                            cols: record.cols,
                            rows: record.rows,
                        },
                    );
                    owners.insert(record.id, (record.owner_id, record.source));
                }
                TerminalHubEvent::Closed { id, exit_code } => {
                    let owner = owners.remove(&id);
                    if let Some((owner, source)) = owner {
                        events.append_with_source(
                            &owner,
                            source.as_ref(),
                            EventPayload::TerminalClosed {
                                terminal_id: id,
                                exit_code,
                            },
                        );
                    }
                }
                TerminalHubEvent::Forgotten(id) => {
                    owners.remove(&id);
                }
            }
        }));
    }

    fn append(&self, owner: &TerminalOwner, source: Option<&EventSource>, payload: EventPayload) {
        self.events.append_with_source(owner, source, payload);
    }
}

/// One admitted viewer. Transport-neutral: the WebSocket handler and the
/// local attach both drive it. Dropping it without [`Self::detach`] counts
/// as a hang-up.
pub struct TerminalSession {
    id: TerminalId,
    owner: TerminalOwner,
    source: Option<EventSource>,
    viewer: ViewerIdentity,
    subscription: TerminalSubscription,
    ops: Arc<TerminalOperations>,
    /// The record as admitted: output admission is rechecked against it
    /// under the live policy for network viewers.
    admitted: TerminalRecord,
    /// Set when a live policy change stopped serving this viewer.
    revoked: bool,
    grant: Option<InputGrant>,
    /// Reason a client declared with a `detached` frame.
    declared_detach: Option<DetachReason>,
    finished: bool,
}

impl std::fmt::Debug for TerminalSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalSession")
            .field("id", &self.id)
            .field("holds_grant", &self.grant.is_some())
            .finish_non_exhaustive()
    }
}

impl TerminalSession {
    fn new(
        record: TerminalRecord,
        viewer: ViewerIdentity,
        subscription: TerminalSubscription,
        ops: Arc<TerminalOperations>,
    ) -> Self {
        Self {
            id: record.id.clone(),
            owner: record.owner_id.clone(),
            source: record.source.clone(),
            admitted: record,
            revoked: false,
            viewer,
            subscription,
            ops,
            grant: None,
            declared_detach: None,
            finished: false,
        }
    }

    pub fn id(&self) -> &TerminalId {
        &self.id
    }

    pub fn viewer(&self) -> &ViewerIdentity {
        &self.viewer
    }

    pub fn holds_grant(&self) -> bool {
        self.grant.is_some() && self.ops.hub.has_input_holder(&self.id).unwrap_or(false)
    }

    /// The reason the client declared with a `detached` frame, if it did.
    pub fn declared_detach(&self) -> Option<DetachReason> {
        self.declared_detach
    }

    /// The next server frame (snapshot first).
    ///
    /// Network viewers are re-admitted under the live policy before every
    /// frame: once their owner's feature is no longer served on the network
    /// listener the session ends (`None`, [`Self::access_revoked`] is true)
    /// and any grant is released.
    pub async fn next_frame(&mut self) -> Option<TerminalFrame> {
        if self.revoked {
            return None;
        }
        let frame = self.subscription.next().await?;
        if matches!(frame, TerminalFrame::Exit { .. }) {
            self.release_grant();
        }
        if self.viewer.listener == CommandListener::Network
            && self
                .ops
                .policy()
                .admit(&self.admitted, &self.viewer)
                .is_err()
        {
            self.revoked = true;
            self.release_grant();
            return None;
        }
        Some(frame)
    }

    /// Whether a live policy change stopped serving this viewer.
    pub fn access_revoked(&self) -> bool {
        self.revoked
    }

    /// Re-check, under the live policy, that this session may still drive
    /// the PTY. A grant whose permission was revoked (`remoteInput` turned
    /// off, the feature no longer served on the network, the terminal
    /// exited) is released here rather than trusted until the viewer
    /// reconnects.
    fn revalidate_grant(&mut self) -> Result<(), TerminalRefusal> {
        if self.grant.is_none() {
            return Err(TerminalRefusal::InputNotAllowed);
        }
        let policy = self.ops.policy();
        let allowed = match self.ops.hub.get(&self.id) {
            Ok(record) => {
                record.state == TerminalState::Open
                    && policy.admit(&record, &self.viewer).is_ok()
                    && policy.input_allowed(&record, &self.viewer)
            }
            Err(_) => false,
        };
        if allowed {
            Ok(())
        } else {
            self.release_grant();
            Err(TerminalRefusal::InputNotAllowed)
        }
    }

    /// Apply one client frame under policy: `TakeControl` takes the grant
    /// (and logs `terminal_control`); `Input`/`Resize` are honoured only for
    /// the holder; `Detached` releases the grant and records the reason the
    /// following [`Self::detach`] logs.
    pub fn handle_client_frame(
        &mut self,
        frame: TerminalClientFrame,
    ) -> Result<(), TerminalRefusal> {
        match frame {
            TerminalClientFrame::TakeControl => self.take_control(),
            TerminalClientFrame::Input { bytes } => {
                self.revalidate_grant()?;
                match &self.grant {
                    Some(grant) => Ok(grant.send_input(bytes)?),
                    None => Err(TerminalRefusal::InputNotAllowed),
                }
            }
            TerminalClientFrame::Resize { cols, rows } => {
                self.revalidate_grant()?;
                match &self.grant {
                    Some(grant) => Ok(grant.resize(cols, rows)?),
                    None => Err(TerminalRefusal::InputNotAllowed),
                }
            }
            TerminalClientFrame::Detached { reason } => {
                self.declared_detach = Some(reason);
                self.release_grant();
                Ok(())
            }
        }
    }

    fn take_control(&mut self) -> Result<(), TerminalRefusal> {
        if self.grant.is_some() {
            // A repeated request is answered under the live policy too.
            return self.revalidate_grant();
        }
        let record = self.ops.hub.get(&self.id)?;
        if record.state != TerminalState::Open {
            return Err(TerminalRefusal::Closed);
        }
        let policy = self.ops.policy();
        if policy.admit(&record, &self.viewer).is_err()
            || !policy.input_allowed(&record, &self.viewer)
        {
            return Err(TerminalRefusal::InputNotAllowed);
        }
        let holder = self.viewer.holder();
        {
            // The hub capability is the exclusive part; the name book only
            // follows it, under the same lock so events see one order.
            let mut grants = self.ops.grants.lock().expect("terminal grants poisoned");
            let grant = self.ops.hub.take_control(&self.id)?;
            grants.clear(&self.id);
            grants.take(&self.id, holder.clone())?;
            self.grant = Some(grant);
            self.ops.append(
                &self.owner,
                self.source.as_ref(),
                EventPayload::TerminalControl {
                    terminal_id: self.id.clone(),
                    holder: holder.as_str().to_owned(),
                    annotations: self.viewer.annotations.clone(),
                },
            );
        }
        Ok(())
    }

    /// Release the grant if held, logging `terminal_control` with an empty
    /// `holder` (nobody holds input now). The capability is dropped first,
    /// so the PTY is back at its fallback size before anyone reads the log.
    fn release_grant(&mut self) {
        let Some(grant) = self.grant.take() else {
            return;
        };
        {
            let mut grants = self.ops.grants.lock().expect("terminal grants poisoned");
            grants.release(&self.id, &self.viewer.holder());
            drop(grant);
            self.ops.append(
                &self.owner,
                self.source.as_ref(),
                EventPayload::TerminalControl {
                    terminal_id: self.id.clone(),
                    holder: String::new(),
                    annotations: self.viewer.annotations.clone(),
                },
            );
        }
    }

    /// End the session: release the grant (+`terminal_control`) and log
    /// `terminal_viewer_detached{reason}` for local attaches. A reason the
    /// client declared with a `detached` frame wins over `reason`.
    pub fn detach(mut self, reason: DetachReason) {
        let reason = self.declared_detach.unwrap_or(reason);
        self.finish(reason);
    }

    fn finish(&mut self, reason: DetachReason) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.release_grant();
        if self.viewer.listener == CommandListener::Local {
            self.ops.append(
                &self.owner,
                self.source.as_ref(),
                EventPayload::TerminalViewerDetached {
                    terminal_id: self.id.clone(),
                    reason,
                    annotations: self.viewer.annotations.clone(),
                },
            );
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let reason = self.declared_detach.unwrap_or(DetachReason::Hangup);
        self.finish(reason);
    }
}
