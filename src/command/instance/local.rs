//! `LocalInstanceBackend`: the builtin engine. Builds an
//! [`super::EngineInstance`] in-process and exposes it through
//! [`super::backend::InstanceBackend`]; `UiRequest`s are always unclaimed, so a
//! TUI on this backend behaves exactly as before the refactor.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::commands::squad::indicator::SharedSquadHealth;
use crate::command::commands::squad::supervisor::{SquadStartError, SquadStartupResult};
use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::error::CommandError;
use crate::command::stats_sampler::ContainerStatsSampler;
use crate::data::frontend_protocol::{UiOutcome, UiRequest};
use crate::data::ids::{InstanceId, TabId};
use crate::data::session::{Session, SessionId};

use super::backend::{BackendEvent, BackendKind, BackendLoss, InstanceBackend, UiTicket};
use super::{
    CloseTabOutcome, EngineInstance, InitialTabOpened, InitialTabSpec, InstanceParts,
    OpenTabRequest, OpenedTab, QuitPlan, SubmitRequest, SubmittedCommand,
};

pub struct LocalInstanceBackend {
    instance: Arc<EngineInstance>,
    acks: Mutex<VecDeque<BackendEvent>>,
    next_ticket: AtomicU64,
}

impl LocalInstanceBackend {
    /// Build the in-process instance.
    pub fn open(parts: InstanceParts) -> Arc<Self> {
        Arc::new(Self {
            instance: EngineInstance::new(parts),
            acks: Mutex::new(VecDeque::new()),
            next_ticket: AtomicU64::new(1),
        })
    }

    pub fn instance(&self) -> &Arc<EngineInstance> {
        &self.instance
    }
}

impl InstanceBackend for LocalInstanceBackend {
    fn queue_source(
        &self,
        session: &Session,
    ) -> Result<Option<crate::command::worker::queue_source::SharedQueueSource>, String> {
        self.instance.queue_source(session)
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Builtin
    }

    fn instance_id(&self) -> InstanceId {
        self.instance.id().clone()
    }

    fn catalogue(&self) -> &'static CommandCatalogue {
        self.instance.catalogue()
    }

    fn open_initial(&self, spec: InitialTabSpec) -> InitialTabOpened {
        self.instance.open_initial(spec)
    }

    fn open_tab(&self, request: OpenTabRequest) -> Result<OpenedTab, CommandError> {
        self.instance.open_tab(request)
    }

    fn adopt_session(&self, session: &Session) {
        self.instance.adopt_session(session);
    }

    fn session_snapshot(&self, session_id: &SessionId) -> Option<Session> {
        self.instance.session_snapshot(session_id)
    }

    fn close_tab(&self, session_id: &SessionId) -> CloseTabOutcome {
        self.instance.close_tab(session_id)
    }

    fn tab_id(&self, session_id: &SessionId) -> Option<TabId> {
        self.instance.tab_id_for(session_id)
    }

    fn focus_hint(&self, session_id: &SessionId) {
        self.instance.set_active_tab_hint(session_id);
    }

    fn resize_tab(&self, session_id: &SessionId, pty_size: (u16, u16)) {
        self.instance.resize_tab(session_id, pty_size);
    }

    fn admit(&self, path: &[String]) -> Result<(), CommandError> {
        self.instance.admit(path)
    }

    fn submit(&self, request: SubmitRequest) -> Result<SubmittedCommand, CommandError> {
        self.instance.submit(request)
    }

    fn running_jobs(&self) -> usize {
        self.instance.running_jobs()
    }

    fn stats_sampler(&self) -> ContainerStatsSampler {
        self.instance.stats_sampler()
    }

    fn take_runtime_notices(&self) -> Vec<String> {
        self.instance.take_runtime_notices()
    }

    fn ui_request(&self, _request: UiRequest) -> UiTicket {
        let ticket = UiTicket(self.next_ticket.fetch_add(1, Ordering::Relaxed));
        if let Ok(mut acks) = self.acks.lock() {
            acks.push_back(BackendEvent::UiAck {
                ticket,
                outcome: UiOutcome {
                    claimed_by_host: false,
                    claimed_by: None,
                    detail: None,
                },
            });
        }
        ticket
    }

    fn poll(&self) -> Vec<BackendEvent> {
        self.acks
            .lock()
            .map(|mut acks| acks.drain(..).collect())
            .unwrap_or_default()
    }

    fn quit(&self) -> QuitPlan {
        self.instance.quit()
    }

    fn health(&self) -> Result<(), BackendLoss> {
        Ok(())
    }

    fn squad_health(&self) -> SharedSquadHealth {
        self.instance.squad_health()
    }

    fn start_squad_indicator(
        &self,
        cancel: CancellationToken,
    ) -> Option<tokio::task::JoinHandle<()>> {
        Some(self.instance.start_squad_indicator(cancel))
    }

    fn squad_admission(&self) -> Result<(), SquadStartError> {
        self.instance.squad_admission()
    }

    fn squad_enabled_in_config(&self) -> Result<bool, CommandError> {
        self.instance.squad_enabled_in_config()
    }

    fn squad_daemon_is_running(&self) -> Result<bool, CommandError> {
        self.instance.squad_daemon_is_running()
    }

    fn start_squad_gateway(&self) -> std::sync::mpsc::Receiver<SquadStartupResult> {
        self.instance.start_squad_gateway()
    }

    fn open_squad_session(&self) -> Result<Session, SquadStartError> {
        self.instance.open_squad_session()
    }

    fn set_squad_gateway(&self, gateway: Arc<dyn TaskGateway>) {
        self.instance.set_squad_gateway(gateway);
    }
}
