use super::*;
use crate::command::commands::squad::{
    gateway::TaskGateway,
    indicator::SharedSquadHealth,
    supervisor::{SquadStartError, SquadStartupResult},
};
use crate::command::error::CommandError;
use crate::command::instance::backend::*;
use crate::command::instance::{
    CloseTabOutcome, InitialTabOpened, InitialTabSpec, OpenTabRequest, OpenedTab, QuitPlan,
    SubmitRequest, SubmittedCommand,
};
use crate::command::stats_sampler::ContainerStatsSampler;
use crate::data::frontend_protocol::UiRequest;
use crate::data::ids::{InstanceId, TabId};
use crate::data::session::SessionId;
use tokio_util::sync::CancellationToken;
/// Reproduce the daemon submit boundary: it never receives the presenter's
/// local SquadAttachSession. All other operations use the production backend.
struct LeaseLessBackend(Arc<dyn InstanceBackend>, Arc<std::sync::atomic::AtomicBool>);
impl InstanceBackend for LeaseLessBackend {
    fn kind(&self) -> BackendKind {
        self.0.kind()
    }
    fn squad_health(&self) -> SharedSquadHealth {
        self.0.squad_health()
    }
    fn instance_id(&self) -> InstanceId {
        self.0.instance_id()
    }
    fn catalogue(&self) -> &'static CommandCatalogue {
        self.0.catalogue()
    }
    fn open_initial(&self, spec: InitialTabSpec) -> InitialTabOpened {
        self.0.open_initial(spec)
    }
    fn open_tab(&self, request: OpenTabRequest) -> Result<OpenedTab, CommandError> {
        self.0.open_tab(request)
    }
    fn adopt_session(&self, session: &Session) {
        self.0.adopt_session(session)
    }
    fn session_snapshot(&self, session_id: &SessionId) -> Option<Session> {
        self.0.session_snapshot(session_id)
    }
    fn close_tab(&self, session_id: &SessionId) -> CloseTabOutcome {
        self.0.close_tab(session_id)
    }
    fn tab_id(&self, session_id: &SessionId) -> Option<TabId> {
        self.0.tab_id(session_id)
    }
    fn focus_hint(&self, session_id: &SessionId) {
        self.0.focus_hint(session_id)
    }
    fn resize_tab(&self, session_id: &SessionId, pty_size: (u16, u16)) {
        self.0.resize_tab(session_id, pty_size)
    }
    fn admit(&self, path: &[String]) -> Result<(), CommandError> {
        self.0.admit(path)
    }
    fn submit(&self, mut request: SubmitRequest) -> Result<SubmittedCommand, CommandError> {
        request.squad_attach = None;
        self.0.submit(request)
    }
    fn running_jobs(&self) -> usize {
        self.0.running_jobs()
    }
    fn stats_sampler(&self) -> ContainerStatsSampler {
        self.0.stats_sampler()
    }
    fn take_runtime_notices(&self) -> Vec<String> {
        self.0.take_runtime_notices()
    }
    fn ui_request(&self, request: UiRequest) -> UiTicket {
        self.0.ui_request(request)
    }
    fn poll(&self) -> Vec<BackendEvent> {
        self.0.poll()
    }
    fn quit(&self) -> QuitPlan {
        self.0.quit()
    }
    fn health(&self) -> Result<(), BackendLoss> {
        if self.1.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(BackendLoss::Transport("fixture daemon gone".into()));
        }
        self.0.health()
    }
    fn start_squad_indicator(
        &self,
        cancel: CancellationToken,
    ) -> Option<tokio::task::JoinHandle<()>> {
        self.0.start_squad_indicator(cancel)
    }
    fn squad_admission(&self) -> Result<(), SquadStartError> {
        self.0.squad_admission()
    }
    fn squad_enabled_in_config(&self) -> Result<bool, CommandError> {
        self.0.squad_enabled_in_config()
    }
    fn squad_daemon_is_running(&self) -> Result<bool, CommandError> {
        self.0.squad_daemon_is_running()
    }
    fn start_squad_gateway(&self) -> std::sync::mpsc::Receiver<SquadStartupResult> {
        self.0.start_squad_gateway()
    }
    fn open_squad_session(&self) -> Result<Session, SquadStartError> {
        self.0.open_squad_session()
    }
    fn set_squad_gateway(&self, gateway: std::sync::Arc<dyn TaskGateway>) {
        self.0.set_squad_gateway(gateway)
    }
}

#[tokio::test]
async fn ctrl_backslash_detaches_squad_instead_of_minimizing() {
    use crate::command::instance::feeds::ContainerSlotEvent;
    let s = TestSandbox::for_test(TestCase::component(
        "frontend::tui::contracts::squad_follow::ctrl_backslash_detaches_squad_instead_of_minimizing",
    ));
    for (workflow, key) in [(false, '\\'), (true, '\\'), (false, '4'), (true, '4')] {
        let mut app = squad_app(&s);
        app.backend = Arc::new(LeaseLessBackend(
            app.backend.clone(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        ));
        let task = serde_json::from_value(serde_json::json!({
            "id": crate::data::ids::TaskId::new(), "name": "selected-task",
            "description": "fixture", "repo_scope": s.work(), "mount_scope": "directory",
            "overlays": [], "interval_secs": 60, "status": "active",
            "created_at": chrono::Utc::now(), "updated_at": chrono::Utc::now()
        }))
        .unwrap();
        app.active_tab()
            .squad
            .as_ref()
            .unwrap()
            .snapshot
            .lock()
            .unwrap()
            .tasks
            .push(task);
        press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
        // The bridge publishes this shape after dispatch resolves a terminal.
        let events = app.active_tab().shared.container.clone();
        for step in if workflow {
            vec!["build", "review"]
        } else {
            vec!["leader"]
        } {
            events.push(ContainerSlotEvent::Launched {
                step_name: step.into(),
                agent: "fixture".into(),
                model: None,
                io: None,
            });
            events.push(ContainerSlotEvent::ContainerName {
                step_name: step.into(),
                container_name: step.into(),
            });
        }
        app.active_tab_mut().drain_container_slot_events();
        if workflow {
            app.active_tab()
                .shared
                .workflow
                .update_view(|v| *v = Some(Default::default()));
        }
        let (input, mut received) = tokio::sync::mpsc::unbounded_channel();
        app.active_tab_mut()
            .focused_slot_mut()
            .unwrap()
            .container_stdin_tx = Some(input);
        app.active_tab_mut().container_window_state = tabs::ContainerWindowState::Maximized;
        app.active_tab_mut().execution_phase = tabs::ExecutionPhase::Running {
            command: "squad attach selected-task".into(),
        };
        assert!(app
            .active_tab()
            .squad
            .as_ref()
            .unwrap()
            .attached_task()
            .is_some());
        press(&mut app, KeyCode::Char(key), KeyModifiers::CONTROL);
        assert_eq!(
            app.active_tab().container_window_state,
            tabs::ContainerWindowState::Hidden
        );
        assert!(app.status_bar.text.contains("selected-task"));
        assert!(app.squad_grid_active());
        assert!(app.active_tab().container_slots.is_empty());
        assert!(app.active_tab().shared.workflow.snapshot().view.is_none());
        assert!(received.try_recv().is_err());
        drop(app);
    }
    s.finish();
}

#[tokio::test]
async fn ordinary_detach_still_minimizes_its_running_container() {
    let s = TestSandbox::for_test(TestCase::component(
        "frontend::tui::contracts::squad_follow::ordinary_detach_still_minimizes_its_running_container",
    ));
    let mut app = app(&s);
    app.active_tab_mut()
        .start_container("fixture".into(), "owned".into(), 80, 24);
    app.active_tab_mut().container_window_state = tabs::ContainerWindowState::Maximized;
    app.active_tab_mut().execution_phase = tabs::ExecutionPhase::Running {
        command: "attach term_fixture".into(),
    };
    let cancel = app.active_tab().shared.command_cancel.clone();
    let (input, mut received) = tokio::sync::mpsc::unbounded_channel();
    app.active_tab_mut()
        .focused_slot_mut()
        .unwrap()
        .container_stdin_tx = Some(input);
    press(&mut app, KeyCode::Char('\\'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab().container_window_state,
        tabs::ContainerWindowState::Minimized
    );
    assert!(app.status_bar.text.contains("ctrl-m to return"));
    assert!(!cancel.is_cancelled());
    assert!(received.try_recv().is_err());
    drop(app);
    s.finish();
}

#[cfg(unix)]
mod daemon_journey;
