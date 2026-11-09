//! WI 0142: the TUI as a frontend of an engine instance.
//!
//! - `UiRequest`s for window-management transitions: unclaimed acks leave
//!   today's native behaviour (and frames) untouched; claimed acks update
//!   state only (the tab bar stays collapsed, a maximized slot renders
//!   Minimized).
//! - Engine loss and `Detach` take today's fatal path: the frame equals the
//!   builtin fatal-dialog frame after an engine-thread panic, and the exit
//!   code is the fatal one.
//! - `TerminalRelease { terminal_id: Some }`: the real terminal is released,
//!   bridged to a fake host process through a hub terminal, then restored,
//!   in today's order.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::backend::TestBackend as RatatuiTestBackend;
use ratatui::Terminal;
use tokio_util::sync::CancellationToken;

use super::{make_session, test_runtime_handle};
use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::commands::squad::indicator::SharedSquadHealth;
use crate::command::commands::squad::supervisor::{SquadStartError, SquadStartupResult};
use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::command::instance::backend::{
    BackendEvent, BackendKind, BackendLoss, InstanceBackend, UiTicket,
};
use crate::command::instance::feeds::ContainerSlotIo;
use crate::command::instance::{
    CloseTabOutcome, InitialTabOpened, InitialTabSpec, OpenTabRequest, OpenedTab, QuitPlan,
    SubmitRequest, SubmittedCommand,
};
use crate::command::stats_sampler::ContainerStatsSampler;
use crate::data::daemon_state::FeatureName;
use crate::data::dialog::{DialogRequest, DialogResponse};
use crate::data::frontend_protocol::{
    ContainerWindowStateWire, DetachReasonWire, UiOutcome, UiRequest,
};
use crate::data::ids::{InstanceId, TabId, TerminalId};
use crate::data::instance_snapshot::SlotId;
use crate::data::session::{Session, SessionId};
use crate::data::session_manager::SessionManager;
use crate::data::terminal::{TerminalFrame, TerminalOwner, TerminalOwnerInfo};
use crate::engine::terminal_hub::{TerminalHub, TerminalInput, TerminalRegistration};
use crate::frontend::tui::app::{App, TerminalOp};
use crate::frontend::tui::dialogs::Dialog;
use crate::frontend::tui::event_loop::{
    perform_terminal_op, BridgeInput, Released, TerminalControl,
};
use crate::frontend::tui::remote_tab::{
    engine_lost_body, RemoteTabView, ENGINE_LOST_EXIT_CODE, ENGINE_LOST_TITLE,
};
use crate::frontend::tui::tabs::{ContainerWindowState, Tab};

// ─── A scripted backend over the builtin one ────────────────────────────────

/// The builtin backend, plus what a daemon engine adds: host presenters that
/// claim requests, engine loss, and hub terminals for host processes.
struct ScriptedBackend {
    inner: Arc<dyn InstanceBackend>,
    claim: Mutex<bool>,
    sent: Mutex<Vec<UiRequest>>,
    events: Mutex<VecDeque<BackendEvent>>,
    loss: Mutex<Option<BackendLoss>>,
    next_ticket: AtomicU64,
    hub: Option<Arc<TerminalHub>>,
}

impl ScriptedBackend {
    fn new(inner: Arc<dyn InstanceBackend>, hub: Option<Arc<TerminalHub>>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            claim: Mutex::new(false),
            sent: Mutex::new(Vec::new()),
            events: Mutex::new(VecDeque::new()),
            loss: Mutex::new(None),
            next_ticket: AtomicU64::new(1),
            hub,
        })
    }

    fn claim(&self, claim: bool) {
        *self.claim.lock().unwrap() = claim;
    }

    fn sent(&self) -> Vec<UiRequest> {
        std::mem::take(&mut *self.sent.lock().unwrap())
    }

    fn push(&self, event: BackendEvent) {
        self.events.lock().unwrap().push_back(event);
    }

    fn lose(&self, loss: BackendLoss) {
        *self.loss.lock().unwrap() = Some(loss);
    }
}

impl InstanceBackend for ScriptedBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Daemon {
            instance_id: self.inner.instance_id(),
            pid: 1,
        }
    }
    fn instance_id(&self) -> InstanceId {
        self.inner.instance_id()
    }
    fn catalogue(&self) -> &'static CommandCatalogue {
        self.inner.catalogue()
    }
    fn open_initial(&self, spec: InitialTabSpec) -> InitialTabOpened {
        self.inner.open_initial(spec)
    }
    fn open_tab(&self, request: OpenTabRequest) -> Result<OpenedTab, CommandError> {
        self.inner.open_tab(request)
    }
    fn adopt_session(&self, session: &Session) {
        self.inner.adopt_session(session)
    }
    fn session_snapshot(&self, session_id: &SessionId) -> Option<Session> {
        self.inner.session_snapshot(session_id)
    }
    fn close_tab(&self, session_id: &SessionId) -> CloseTabOutcome {
        self.inner.close_tab(session_id)
    }
    fn tab_id(&self, session_id: &SessionId) -> Option<TabId> {
        self.inner.tab_id(session_id)
    }
    fn focus_hint(&self, session_id: &SessionId) {
        self.inner.focus_hint(session_id)
    }
    fn resize_tab(&self, session_id: &SessionId, pty_size: (u16, u16)) {
        self.inner.resize_tab(session_id, pty_size)
    }
    fn admit(&self, path: &[String]) -> Result<(), CommandError> {
        self.inner.admit(path)
    }
    fn submit(&self, request: SubmitRequest) -> Result<SubmittedCommand, CommandError> {
        self.inner.submit(request)
    }
    fn running_jobs(&self) -> usize {
        self.inner.running_jobs()
    }
    fn stats_sampler(&self) -> ContainerStatsSampler {
        ContainerStatsSampler::inert()
    }
    fn take_runtime_notices(&self) -> Vec<String> {
        Vec::new()
    }
    fn ui_request(&self, request: UiRequest) -> UiTicket {
        let ticket = UiTicket(self.next_ticket.fetch_add(1, Ordering::Relaxed));
        self.sent.lock().unwrap().push(request);
        let outcome = if *self.claim.lock().unwrap() {
            UiOutcome::claimed("rex")
        } else {
            UiOutcome::unclaimed()
        };
        self.push(BackendEvent::UiAck { ticket, outcome });
        ticket
    }
    fn poll(&self) -> Vec<BackendEvent> {
        self.events.lock().unwrap().drain(..).collect()
    }
    fn quit(&self) -> QuitPlan {
        self.inner.quit()
    }
    fn health(&self) -> Result<(), BackendLoss> {
        match self.loss.lock().unwrap().clone() {
            Some(loss) => Err(loss),
            None => Ok(()),
        }
    }
    fn open_host_terminal(
        &self,
        terminal_id: &TerminalId,
    ) -> Result<ContainerSlotIo, CommandError> {
        let hub = self
            .hub
            .clone()
            .ok_or_else(|| CommandError::Other("no hub".into()))?;
        subscribe_in_process(&hub, terminal_id)
    }
    fn squad_health(&self) -> SharedSquadHealth {
        self.inner.squad_health()
    }
    fn start_squad_indicator(
        &self,
        _cancel: CancellationToken,
    ) -> Option<tokio::task::JoinHandle<()>> {
        None
    }
    fn squad_admission(&self) -> Result<(), SquadStartError> {
        self.inner.squad_admission()
    }
    fn squad_daemon_is_running(&self) -> Result<bool, CommandError> {
        self.inner.squad_daemon_is_running()
    }
    fn start_squad_gateway(&self) -> std::sync::mpsc::Receiver<SquadStartupResult> {
        self.inner.start_squad_gateway()
    }
    fn open_squad_session(&self) -> Result<Session, SquadStartError> {
        self.inner.open_squad_session()
    }
    fn set_squad_gateway(&self, gateway: Arc<dyn TaskGateway>) {
        self.inner.set_squad_gateway(gateway)
    }
}

/// What the remote backend's hub subscription does, in process: screen and
/// output out, keystrokes in under the input grant.
fn subscribe_in_process(
    hub: &Arc<TerminalHub>,
    terminal_id: &TerminalId,
) -> Result<ContainerSlotIo, CommandError> {
    let mut subscription = hub
        .subscribe(terminal_id)
        .map_err(|e| CommandError::Other(e.to_string()))?;
    let grant = hub
        .take_control(terminal_id)
        .map_err(|e| CommandError::Other(e.to_string()))?;
    let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (resize_tx, mut resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
    test_runtime_handle().spawn(async move {
        loop {
            tokio::select! {
                frame = subscription.next() => match frame {
                    Some(TerminalFrame::Snapshot { screen, .. }) => { let _ = stdout_tx.send(screen); }
                    Some(TerminalFrame::Output { bytes }) => { let _ = stdout_tx.send(bytes); }
                    Some(TerminalFrame::Exit { .. }) | None => return,
                    Some(_) => {}
                },
                bytes = stdin_rx.recv() => match bytes {
                    Some(bytes) => { let _ = grant.send_input(bytes); }
                    None => return,
                },
                size = resize_rx.recv() => {
                    if let Some((cols, rows)) = size { let _ = grant.resize(cols, rows); }
                }
            }
        }
    });
    Ok(ContainerSlotIo {
        stdout_rx,
        stdin_tx,
        resize_tx,
    })
}

fn local_backend() -> Arc<dyn InstanceBackend> {
    App::local_backend(
        CommandCatalogue::get(),
        Engines::for_tests(std::path::Path::new("/tmp")),
        Arc::new(SessionManager::in_memory(
            crate::data::config::env::EnvSnapshot::empty(),
        )),
        &test_runtime_handle(),
    )
}

fn scripted_app(hub: Option<Arc<TerminalHub>>) -> (App, Arc<ScriptedBackend>) {
    let backend = ScriptedBackend::new(local_backend(), hub);
    let app = App::with_backend(
        backend.clone(),
        Tab::new(make_session()),
        test_runtime_handle(),
    );
    (app, backend)
}

/// Another tab (each fixture session has its own identity).
fn second_tab() -> Tab {
    Tab::new(make_session())
}

fn with_slot(tab: &mut Tab) -> SlotId {
    tab.start_container("claude".into(), "c1".into(), 60, 16);
    SlotId(tab.focused_slot().unwrap().launch_id)
}

fn frame(app: &mut App) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(RatatuiTestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::frontend::tui::render::render_frame(app, frame))
        .unwrap();
    terminal.backend().buffer().clone()
}

/// Two ticks: the transition is reported, then its ack is applied.
fn settle(app: &mut App) {
    app.tick_all_tabs();
    app.tick_all_tabs();
}

// ─── UiRequest: unclaimed (builtin) and claimed ─────────────────────────────

/// On the builtin engine every request is acknowledged unclaimed: the TUI's
/// state and frames are exactly what they are without the adapter.
#[test]
fn unclaimed_requests_leave_todays_native_behaviour() {
    let mut app = super::make_app();
    settle(&mut app);
    with_slot(&mut app.tabs[0]);
    app.tabs[0].container_window_state = ContainerWindowState::Maximized;
    app.push_tab(second_tab());
    app.switch_to_next_tab();
    app.switch_to_prev_tab();
    settle(&mut app);

    assert!(!app.remote_view.tab_bar_collapsed());
    assert_eq!(
        app.remote_view.rendered_window(&app.tabs[0]),
        ContainerWindowState::Maximized
    );
    let with_adapter = frame(&mut app);
    app.remote_view = RemoteTabView::new();
    assert_eq!(with_adapter, frame(&mut app));
}

/// Every transition is reported, in the order the TUI made it, and an
/// unclaimed ack changes nothing.
#[test]
fn every_window_transition_is_reported_as_a_ui_request() {
    let (mut app, backend) = scripted_app(None);
    settle(&mut app);
    assert!(
        backend.sent().is_empty(),
        "the opening state is no transition"
    );
    let first = app.backend.tab_id(&app.tabs[0].session_id).unwrap();

    app.push_tab(second_tab());
    settle(&mut app);
    assert!(matches!(backend.sent()[..], [UiRequest::OpenTab { .. }]));
    // Three tabs, so a one-step switch is unambiguous.
    app.push_tab(second_tab());
    settle(&mut app);
    assert!(matches!(backend.sent()[..], [UiRequest::OpenTab { .. }]));

    app.switch_to_next_tab();
    settle(&mut app);
    assert_eq!(backend.sent(), vec![UiRequest::NextTab]);
    app.switch_to_prev_tab();
    settle(&mut app);
    assert_eq!(backend.sent(), vec![UiRequest::PrevTab]);

    let slot = with_slot(&mut app.tabs[0]);
    app.tabs[0].container_window_state = ContainerWindowState::Maximized;
    settle(&mut app);
    assert_eq!(
        backend.sent(),
        vec![UiRequest::SetContainerWindow {
            tab_id: first.clone(),
            slot_id: slot,
            state: ContainerWindowStateWire::Maximized,
        }]
    );

    app.tabs[0].workflow_overview_state = app.tabs[0].workflow_overview_state.toggle();
    settle(&mut app);
    assert_eq!(
        backend.sent(),
        vec![UiRequest::OpenOverview {
            tab_id: first.clone()
        }]
    );

    app.active_tab = 1;
    let second = app.backend.tab_id(&app.tabs[1].session_id).unwrap();
    app.close_active_tab();
    settle(&mut app);
    let sent = backend.sent();
    assert!(
        sent.contains(&UiRequest::CloseTab { tab_id: second }),
        "{sent:?}"
    );
    assert!(!app.remote_view.tab_bar_collapsed());
    assert_eq!(
        app.remote_view.rendered_window(&app.tabs[0]),
        ContainerWindowState::Maximized
    );
}

/// A claimed request updates TUI state only: the tab bar stays collapsed to
/// the active tab and the claimed maximized slot renders exactly like a
/// minimized one.
#[test]
fn claimed_requests_update_state_only() {
    let (mut app, backend) = scripted_app(None);
    settle(&mut app);
    backend.claim(true);

    app.push_tab(second_tab());
    settle(&mut app);
    assert_eq!(app.tabs.len(), 2, "the tab exists in TUI state");
    assert!(app.remote_view.tab_bar_collapsed());

    with_slot(&mut app.tabs[0]);
    app.tabs[0].container_window_state = ContainerWindowState::Maximized;
    settle(&mut app);
    assert_eq!(
        app.tabs[0].container_window_state,
        ContainerWindowState::Maximized,
        "state is updated"
    );
    assert_eq!(
        app.remote_view.rendered_window(&app.tabs[0]),
        ContainerWindowState::Minimized
    );

    // The claimed frame equals the native frame of a minimized slot with a
    // single tab in the bar.
    let claimed = frame(&mut app);
    app.remote_view = RemoteTabView::new();
    app.tabs[0].container_window_state = ContainerWindowState::Minimized;
    let parked = app.tabs.remove(1);
    let native = frame(&mut app);
    app.tabs.push(parked);
    assert_eq!(claimed, native);

    // An unclaimed answer to a later request shows the window again.
    backend.claim(false);
    app.tabs[0].container_window_state = ContainerWindowState::Maximized;
    settle(&mut app);
    app.tabs[0].container_window_state = ContainerWindowState::Minimized;
    settle(&mut app);
    app.tabs[0].container_window_state = ContainerWindowState::Maximized;
    settle(&mut app);
    assert_eq!(
        app.remote_view.rendered_window(&app.tabs[0]),
        ContainerWindowState::Maximized
    );
}

// ─── Engine loss: today's fatal path ────────────────────────────────────────

/// The command thread died with the engine: its result channel is gone.
fn with_dead_command(app: &mut App) {
    let (tx, rx) = std::sync::mpsc::channel();
    app.tabs[0].command_result_rx = Some(rx);
    app.tabs[0].execution_phase = crate::frontend::tui::tabs::ExecutionPhase::Running {
        command: "chat".into(),
    };
    drop(tx);
}

#[test]
fn engine_loss_takes_the_builtin_fatal_path() {
    let loss = BackendLoss::Transport("the engine closed the socket".into());

    // Daemon engine: the backend reports the loss on the next tick.
    let (mut remote, backend) = scripted_app(None);
    with_dead_command(&mut remote);
    backend.lose(loss.clone());
    remote.tick_all_tabs();
    assert_eq!(
        remote.remote_view.fatal_exit_code(),
        Some(ENGINE_LOST_EXIT_CODE)
    );

    // Builtin engine: the engine thread panicked (same dead channel) and the
    // TUI shows its fatal dialog.
    let mut builtin = super::make_app();
    with_dead_command(&mut builtin);
    builtin.tick_all_tabs();
    builtin.active_dialog = Some(Dialog::FatalError {
        title: ENGINE_LOST_TITLE.into(),
        body: engine_lost_body(&loss),
    });

    assert!(matches!(
        remote.active_dialog,
        Some(Dialog::FatalError { .. })
    ));
    assert_eq!(frame(&mut remote), frame(&mut builtin));

    // Shown once; Enter quits like every fatal dialog.
    remote.tick_all_tabs();
    super::press_key(
        &mut remote,
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    );
    assert!(remote.should_quit);
}

#[test]
fn engine_disabled_detach_is_the_same_fatal_notice() {
    let (mut app, backend) = scripted_app(None);
    backend.push(BackendEvent::Detached(DetachReasonWire::EngineDisabled));
    app.tick_all_tabs();
    let Some(Dialog::FatalError { title, body }) = &app.active_dialog else {
        panic!("expected the fatal dialog");
    };
    assert_eq!(title, ENGINE_LOST_TITLE);
    assert!(body.contains("engine disabled"), "{body}");
    assert_eq!(
        app.remote_view.fatal_exit_code(),
        Some(ENGINE_LOST_EXIT_CODE)
    );
}

// ─── TerminalRelease through a hub terminal ─────────────────────────────────

/// Records what the event loop does to the "real" terminal; keystrokes come
/// from a channel and output lands in a buffer.
struct FakeTerminal {
    calls: Arc<Mutex<Vec<String>>>,
    keys: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    screen: Arc<Mutex<Vec<u8>>>,
}

struct ChannelInput(std::sync::mpsc::Receiver<Vec<u8>>);

impl BridgeInput for ChannelInput {
    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        match self.0.recv_timeout(timeout) {
            Ok(bytes) => {
                buf[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(0),
            Err(_) => Err(io::ErrorKind::UnexpectedEof.into()),
        }
    }
    fn size(&self) -> Option<(u16, u16)> {
        Some((90, 20))
    }
}

struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl TerminalControl for FakeTerminal {
    fn release(&mut self, title: &str) {
        self.calls.lock().unwrap().push(format!("release: {title}"));
    }
    fn reenter(&mut self) -> io::Result<()> {
        self.calls.lock().unwrap().push("reenter".into());
        Ok(())
    }
    fn bridge_ends(&mut self) -> io::Result<(Box<dyn BridgeInput>, Box<dyn io::Write + Send>)> {
        self.calls.lock().unwrap().push("bridge".into());
        let keys = self.keys.take().expect("one bridge");
        Ok((
            Box::new(ChannelInput(keys)),
            Box::new(SharedWriter(Arc::clone(&self.screen))),
        ))
    }
    fn end_bridge(&mut self) {
        self.calls.lock().unwrap().push("end bridge".into());
    }
}

fn eventually(mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    check()
}

#[test]
fn terminal_release_bridges_the_real_terminal_to_a_hub_terminal() {
    // A fake host process in an engine PTY: prompts, then echoes a line.
    let hub = Arc::new(TerminalHub::new(Default::default()));
    let (stdin, mut process_stdin) = crate::engine::terminal_hub::input_channel();
    let handle = hub.register(
        TerminalRegistration {
            owner: TerminalOwnerInfo {
                owner: TerminalOwner::Tab(TabId::new()),
                feature: FeatureName::Engine,
                source: None,
                attempt: 1,
                allow_remote_input: false,
            },
            agent: "gh auth login".into(),
            model: None,
            cols: 80,
            rows: 24,
        },
        TerminalInput {
            stdin,
            resize: None,
        },
    );
    handle.push_output(b"? Paste your token: ");
    let process = handle.clone();
    let typed = Arc::new(Mutex::new(Vec::new()));
    let typed_by_process = Arc::clone(&typed);
    test_runtime_handle().spawn(async move {
        while let Some(bytes) = process_stdin.recv().await {
            typed_by_process.lock().unwrap().extend_from_slice(&bytes);
            if bytes.ends_with(b"\r") {
                process.push_output(b"\r\nLogged in.\r\n");
            }
        }
    });

    let (mut app, _backend) = scripted_app(Some(Arc::clone(&hub)));
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (response_tx, response_rx) = std::sync::mpsc::channel();
    app.tabs[0].dialog_request_rx = Some(request_rx);
    app.tabs[0].dialog_response_tx = Some(response_tx);

    let calls = Arc::new(Mutex::new(Vec::new()));
    let screen = Arc::new(Mutex::new(Vec::new()));
    let (keys_tx, keys_rx) = std::sync::mpsc::channel();
    let mut terminal = FakeTerminal {
        calls: Arc::clone(&calls),
        keys: Some(keys_rx),
        screen: Arc::clone(&screen),
    };
    let mut released: Option<Released> = None;

    // Release: the command thread names the hub terminal.
    request_tx
        .send(DialogRequest::TerminalRelease {
            title: "gh auth login".into(),
            terminal_id: Some(handle.id().clone()),
        })
        .unwrap();
    app.poll_dialog_requests();
    let op = app.terminal_op.take().expect("release parked for the loop");
    assert!(matches!(&op, TerminalOp::Release { terminal_id: Some(id), .. } if id == handle.id()));
    perform_terminal_op(&mut app, op, &mut terminal, &mut released).unwrap();
    assert_eq!(response_rx.recv().unwrap(), DialogResponse::Yes);
    assert!(released.is_some());

    // The host's prompt reaches the terminal; keystrokes reach the host.
    assert!(eventually(|| String::from_utf8_lossy(
        &screen.lock().unwrap()
    )
    .contains("Paste your token")));
    // Input is accepted once the grant is in place; resend until it lands.
    assert!(eventually(|| {
        if typed.lock().unwrap().is_empty() {
            let _ = keys_tx.send(b"tok\r".to_vec());
        }
        typed.lock().unwrap().ends_with(b"tok\r")
    }));
    assert!(eventually(|| String::from_utf8_lossy(
        &screen.lock().unwrap()
    )
    .contains("Logged in.")));

    // Restore: the bridge stops before the TUI takes the terminal back.
    request_tx.send(DialogRequest::TerminalRestore).unwrap();
    app.poll_dialog_requests();
    let op = app.terminal_op.take().expect("restore parked for the loop");
    perform_terminal_op(&mut app, op, &mut terminal, &mut released).unwrap();
    assert_eq!(response_rx.recv().unwrap(), DialogResponse::Dismissed);
    assert!(released.is_none());
    assert_eq!(
        *calls.lock().unwrap(),
        vec![
            "release: gh auth login".to_string(),
            "bridge".into(),
            "end bridge".into(),
            "reenter".into(),
        ]
    );

    // Nothing the host prints after the restore reaches the terminal.
    let before = screen.lock().unwrap().len();
    handle.push_output(b"late output");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(screen.lock().unwrap().len(), before);
    handle.exit(0);
}

/// The builtin engine's release (no terminal id) is unchanged: release,
/// answer, restore, answer — no bridge.
#[test]
fn builtin_terminal_release_is_unchanged() {
    let mut app = super::make_app();
    let (response_tx, response_rx) = std::sync::mpsc::channel();
    app.tabs[0].dialog_response_tx = Some(response_tx);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut terminal = FakeTerminal {
        calls: Arc::clone(&calls),
        keys: None,
        screen: Arc::new(Mutex::new(Vec::new())),
    };
    let mut released = None;
    perform_terminal_op(
        &mut app,
        TerminalOp::Release {
            title: "security".into(),
            terminal_id: None,
        },
        &mut terminal,
        &mut released,
    )
    .unwrap();
    perform_terminal_op(&mut app, TerminalOp::Restore, &mut terminal, &mut released).unwrap();
    assert_eq!(response_rx.recv().unwrap(), DialogResponse::Yes);
    assert_eq!(response_rx.recv().unwrap(), DialogResponse::Dismissed);
    assert_eq!(
        *calls.lock().unwrap(),
        vec!["release: security".to_string(), "reenter".into()]
    );
}
