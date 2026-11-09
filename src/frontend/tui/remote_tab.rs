//! `RemoteTabView` (WI 0142 Part B §2 "On the client side"): the Layer 3
//! presentation adapter between the TUI and its `InstanceBackend`. It is used
//! the same way whichever engine the TUI presents; on the builtin engine the
//! backend acknowledges every request as unclaimed, so nothing here changes
//! what the TUI does.
//!
//! Once per tick ([`RemoteTabView::sync`]) it:
//! - turns `InstanceBackend::health()` errors into today's fatal-error dialog;
//! - applies the backend's [`BackendEvent`]s to `App`/`Tab` view state;
//! - notices every window-management transition the TUI just made natively
//!   (tab opened/closed/switched, container window min/max, overview opened,
//!   workflow window focused) and reports it as a [`UiRequest`];
//! - records which of those transitions a host presenter claimed. A claimed
//!   transition still updates TUI state (the transition already happened),
//!   but the TUI does not show it: the tab bar stays collapsed to the active
//!   tab, a claimed maximized container slot renders Minimized, and a claimed
//!   overview renders minimized.
//!
//! It never selects commands, resolves defaults or drives an engine; the
//! backend is the only thing it calls.
//!
//! Contract: `/awman/context/workflow/0142-design.md` §4.11.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::command::instance::backend::{BackendEvent, BackendLoss, UiTicket};
use crate::data::frontend_protocol::{
    ContainerWindowStateWire, DetachReasonWire, Seq, UiOutcome, UiRequest,
};
use crate::data::ids::TabId;
use crate::data::instance_snapshot::SlotId;
use crate::data::session::SessionId;
use crate::frontend::tui::app::{App, Focus};
use crate::frontend::tui::dialogs::Dialog;
use crate::frontend::tui::tabs::{ContainerWindowState, Tab, WorkflowOverviewState};

/// Title of the dialog shown when the engine is gone (lost or detached).
pub const ENGINE_LOST_TITLE: &str = "awman engine unavailable";

/// The exit code a TUI that lost its engine exits with: the same code as any
/// other fatal TUI error.
pub const ENGINE_LOST_EXIT_CODE: u8 = 1;

/// The fatal-dialog body for a lost engine.
pub fn engine_lost_body(loss: &BackendLoss) -> String {
    match loss {
        BackendLoss::Detached(reason) => detached_body(reason),
        BackendLoss::Transport(detail) => {
            format!("The awman engine is no longer reachable ({detail}). Press Enter to quit.")
        }
    }
}

fn detached_body(reason: &DetachReasonWire) -> String {
    format!(
        "The awman engine ended this session: {}. Press Enter to quit.",
        reason.as_str()
    )
}

/// One tab's window-management facts as of the previous tick.
#[derive(Debug, Clone, PartialEq)]
struct TabMark {
    session_id: SessionId,
    tab_id: Option<TabId>,
    dir: PathBuf,
    window: ContainerWindowState,
    slot: Option<SlotId>,
    overview: WorkflowOverviewState,
}

impl TabMark {
    fn of(app: &App, tab: &Tab) -> Self {
        Self {
            session_id: tab.session_id,
            tab_id: app.backend.tab_id(&tab.session_id),
            dir: tab.session.working_dir().to_path_buf(),
            window: tab.container_window_state,
            slot: tab.focused_slot().map(|slot| SlotId(slot.launch_id)),
            overview: tab.workflow_overview_state,
        }
    }
}

/// The whole window-management state as of the previous tick.
#[derive(Debug, Clone, PartialEq)]
struct Marks {
    tabs: Vec<TabMark>,
    active: usize,
    focus: Focus,
}

impl Marks {
    fn of(app: &App) -> Self {
        Self {
            tabs: app.tabs.iter().map(|tab| TabMark::of(app, tab)).collect(),
            active: app.active_tab,
            focus: app.focus,
        }
    }
}

/// See the module docs.
#[derive(Debug, Default)]
pub struct RemoteTabView {
    /// Container windows a host presenter claimed, with the state the TUI
    /// renders them in.
    claimed_windows: HashMap<(TabId, SlotId), ContainerWindowState>,
    /// Workflow overviews a host presenter claimed (rendered minimized).
    claimed_overviews: HashSet<TabId>,
    /// A host presenter manages the tabs: the tab bar shows the active tab.
    tab_bar_collapsed: bool,
    /// Requests sent and not yet acknowledged, by ticket.
    pending_tickets: HashMap<Seq, UiRequest>,
    /// Tabs the engine closed itself (no `CloseTab` is reported for them).
    closed_by_engine: HashSet<SessionId>,
    /// Session → engine tab id, as last seen (render lookups).
    tab_ids: HashMap<SessionId, TabId>,
    marks: Option<Marks>,
    /// Set once the engine is gone: the exit code `tui::run` returns.
    fatal_exit_code: Option<u8>,
}

impl RemoteTabView {
    pub fn new() -> Self {
        Self::default()
    }

    /// One tick: engine health, backend events, then the window-management
    /// transitions since the previous tick.
    pub fn sync(app: &mut App) {
        if app.remote_view.fatal_exit_code.is_none() {
            if let Err(loss) = app.backend.health() {
                Self::show_fatal(app, &loss);
            }
        }
        for event in app.backend.poll() {
            Self::apply(app, event);
        }
        Self::report_transitions(app);
    }

    /// Apply one backend event to the view.
    pub fn apply(app: &mut App, event: BackendEvent) {
        match event {
            BackendEvent::UiAck { ticket, outcome } => {
                app.remote_view.acknowledge(ticket, &outcome);
            }
            BackendEvent::Detached(reason) => {
                Self::show_fatal(app, &BackendLoss::Detached(reason));
            }
            BackendEvent::Notice { text, .. } => {
                // One status-bar line, like a runtime notice.
                app.status_bar.notice = Some(text);
                app.needs_redraw = true;
            }
            BackendEvent::TabClosedByEngine { tab_id, reason } => {
                Self::close_tab_view(app, &tab_id, &reason);
            }
            BackendEvent::DialogClosed { tab_id, .. } => {
                Self::drop_dialog(app, &tab_id);
            }
            BackendEvent::ProgramStatus {
                tab_id,
                slot_id,
                status,
            } => {
                if let Some(tab) = Self::tab_index(app, &tab_id).map(|i| &mut app.tabs[i]) {
                    if let Some(slot) = tab
                        .container_slots
                        .iter_mut()
                        .find(|slot| slot.launch_id == slot_id.0)
                    {
                        slot.program_status = status;
                    }
                }
            }
            BackendEvent::Annotations { .. }
            | BackendEvent::FrontendAttached(_)
            | BackendEvent::FrontendDetached { .. } => {}
        }
    }

    /// Record a request the TUI sent (its ack arrives through `apply`).
    pub fn on_ui_request_sent(&mut self, ticket: UiTicket, request: UiRequest) {
        self.pending_tickets.insert(ticket.0, request);
    }

    /// The state a claimed container window renders in, if it was claimed.
    pub fn window_state_for(&self, tab: &TabId, slot: &SlotId) -> Option<ContainerWindowState> {
        self.claimed_windows.get(&(tab.clone(), *slot)).copied()
    }

    /// The container window state the renderer draws for `tab`.
    pub fn rendered_window(&self, tab: &Tab) -> ContainerWindowState {
        let claimed = self.tab_ids.get(&tab.session_id).and_then(|tab_id| {
            let slot = SlotId(tab.focused_slot()?.launch_id);
            self.window_state_for(tab_id, &slot)
        });
        match (tab.container_window_state, claimed) {
            (ContainerWindowState::Maximized, Some(state)) => state,
            (state, _) => state,
        }
    }

    /// The workflow overview state the renderer draws for `tab`.
    pub fn rendered_overview(&self, tab: &Tab) -> WorkflowOverviewState {
        let claimed = self
            .tab_ids
            .get(&tab.session_id)
            .is_some_and(|tab_id| self.claimed_overviews.contains(tab_id));
        if claimed {
            WorkflowOverviewState::Minimized
        } else {
            tab.workflow_overview_state
        }
    }

    /// The tab bar shows only the active tab while a host manages tabs.
    pub fn tab_bar_collapsed(&self) -> bool {
        self.tab_bar_collapsed
    }

    /// `Some(code)` once the engine is gone.
    pub fn fatal_exit_code(&self) -> Option<u8> {
        self.fatal_exit_code
    }

    // ── Internals ────────────────────────────────────────────────────────

    /// Engine loss and an unasked `Detach` are today's fatal path: the
    /// `FatalError` dialog (Enter, Esc and Ctrl-C quit) over whatever was on
    /// screen, and the fatal exit code. Shown once.
    fn show_fatal(app: &mut App, loss: &BackendLoss) {
        if app.remote_view.fatal_exit_code.is_some() {
            return;
        }
        app.remote_view.fatal_exit_code = Some(ENGINE_LOST_EXIT_CODE);
        app.active_dialog = Some(Dialog::FatalError {
            title: ENGINE_LOST_TITLE.to_string(),
            body: engine_lost_body(loss),
        });
        app.command_dialog_active = false;
        app.command_dialog_deadline = None;
        app.needs_redraw = true;
    }

    fn acknowledge(&mut self, ticket: UiTicket, outcome: &UiOutcome) {
        let Some(request) = self.pending_tickets.remove(&ticket.0) else {
            return;
        };
        let claimed = outcome.claimed_by_host;
        match request {
            UiRequest::OpenTab { .. }
            | UiRequest::NextTab
            | UiRequest::PrevTab
            | UiRequest::CloseTab { .. } => self.tab_bar_collapsed = claimed,
            UiRequest::SetContainerWindow {
                tab_id,
                slot_id,
                state,
            } => {
                if claimed {
                    // The host shows the window; the TUI keeps it minimized.
                    let rendered = match state {
                        ContainerWindowStateWire::Maximized => ContainerWindowState::Minimized,
                        ContainerWindowStateWire::Minimized => ContainerWindowState::Minimized,
                        ContainerWindowStateWire::Hidden => ContainerWindowState::Hidden,
                    };
                    self.claimed_windows.insert((tab_id, slot_id), rendered);
                } else {
                    self.claimed_windows.remove(&(tab_id, slot_id));
                }
            }
            UiRequest::OpenOverview { tab_id } => {
                if claimed {
                    self.claimed_overviews.insert(tab_id);
                } else {
                    self.claimed_overviews.remove(&tab_id);
                }
            }
            UiRequest::FocusWorkflowWindow { .. } | UiRequest::FocusWork { .. } => {}
        }
    }

    /// Diff the window-management state against the previous tick and send
    /// one `UiRequest` per transition. The TUI already performed each one
    /// natively; an unclaimed ack therefore changes nothing.
    fn report_transitions(app: &mut App) {
        let now = Marks::of(app);
        app.remote_view.tab_ids = now
            .tabs
            .iter()
            .filter_map(|mark| Some((mark.session_id, mark.tab_id.clone()?)))
            .collect();
        let Some(before) = app.remote_view.marks.replace(now.clone()) else {
            return;
        };
        if before == now {
            return;
        }
        let requests = transitions(
            &before,
            &now,
            &app.remote_view.closed_by_engine,
            Self::workflow_active(app),
        );
        app.remote_view.closed_by_engine.clear();
        for request in requests {
            if let UiRequest::OpenOverview { tab_id } = &request {
                // A new overview request supersedes an earlier claim.
                app.remote_view.claimed_overviews.remove(tab_id);
            }
            let ticket = app.backend.ui_request(request.clone());
            app.remote_view.on_ui_request_sent(ticket, request);
        }
        // A window closed or an overview minimized drops its claim.
        let live: HashSet<(TabId, SlotId)> = now
            .tabs
            .iter()
            .filter(|mark| mark.window != ContainerWindowState::Hidden)
            .filter_map(|mark| Some((mark.tab_id.clone()?, mark.slot?)))
            .collect();
        app.remote_view
            .claimed_windows
            .retain(|key, _| live.contains(key));
        let open: HashSet<TabId> = now
            .tabs
            .iter()
            .filter(|mark| mark.overview == WorkflowOverviewState::Maximized)
            .filter_map(|mark| mark.tab_id.clone())
            .collect();
        app.remote_view
            .claimed_overviews
            .retain(|tab_id| open.contains(tab_id));
    }

    fn workflow_active(app: &App) -> bool {
        app.tabs
            .get(app.active_tab)
            .is_some_and(|tab| tab.shared.workflow.snapshot().view.is_some())
    }

    fn tab_index(app: &App, tab_id: &TabId) -> Option<usize> {
        app.tabs
            .iter()
            .position(|tab| app.backend.tab_id(&tab.session_id).as_ref() == Some(tab_id))
    }

    /// Another frontend or the API closed a tab: drop its view. The engine
    /// already closed it, so nothing is reported back.
    fn close_tab_view(app: &mut App, tab_id: &TabId, reason: &str) {
        let Some(index) = Self::tab_index(app, tab_id) else {
            return;
        };
        if app.tabs.len() <= 1 {
            app.should_quit = true;
            return;
        }
        let session_id = app.tabs[index].session_id;
        app.remote_view.closed_by_engine.insert(session_id);
        if index == app.active_tab && app.command_dialog_active {
            app.active_dialog = None;
            app.command_dialog_active = false;
        }
        app.tabs.remove(index);
        if app.active_tab > index || app.active_tab >= app.tabs.len() {
            app.active_tab = app.active_tab.saturating_sub(1);
        }
        app.last_active_tab = app.last_active_tab.min(app.tabs.len().saturating_sub(1));
        if !reason.is_empty() {
            app.status_bar.text = reason.to_string();
        }
        app.needs_redraw = true;
    }

    /// A dialog was answered elsewhere: drop it here, unanswered.
    fn drop_dialog(app: &mut App, tab_id: &TabId) {
        let Some(index) = Self::tab_index(app, tab_id) else {
            return;
        };
        let session_id = app.tabs[index].session_id;
        if index == app.active_tab && app.command_dialog_active {
            app.active_dialog = None;
            app.command_dialog_active = false;
            app.command_dialog_deadline = None;
            app.needs_redraw = true;
        } else {
            app.pending_dialog_requests.remove(&session_id);
        }
    }
}

/// The window-management requests between two ticks, in the order the TUI
/// made them: closes, opens, a tab switch, then per-tab window changes.
fn transitions(
    before: &Marks,
    now: &Marks,
    closed_by_engine: &HashSet<SessionId>,
    workflow_active: bool,
) -> Vec<UiRequest> {
    let mut requests = Vec::new();
    let find = |marks: &Marks, id: &SessionId| marks.tabs.iter().position(|m| m.session_id == *id);

    for mark in &before.tabs {
        if find(now, &mark.session_id).is_none() && !closed_by_engine.contains(&mark.session_id) {
            if let Some(tab_id) = &mark.tab_id {
                requests.push(UiRequest::CloseTab {
                    tab_id: tab_id.clone(),
                });
            }
        }
    }
    let mut opened = false;
    for mark in &now.tabs {
        if find(before, &mark.session_id).is_none() {
            opened = true;
            requests.push(UiRequest::OpenTab {
                dir: mark.dir.clone(),
            });
        }
    }

    // A switch between tabs that both existed before: ±1 is Next/Prev (with
    // wrap-around). Focusing a tab that was just opened is part of OpenTab.
    let before_active = before.tabs.get(before.active).map(|m| m.session_id);
    let now_active = now.tabs.get(now.active).map(|m| m.session_id);
    if let (Some(from), Some(to)) = (before_active, now_active) {
        if from != to && !opened {
            if let (Some(old), Some(new)) = (find(now, &from), find(now, &to)) {
                let n = now.tabs.len();
                if new == (old + 1) % n {
                    requests.push(UiRequest::NextTab);
                } else if (new + 1) % n == old {
                    requests.push(UiRequest::PrevTab);
                }
            }
        }
    }

    for mark in &now.tabs {
        let Some(tab_id) = &mark.tab_id else {
            continue;
        };
        let previous = find(before, &mark.session_id).map(|i| &before.tabs[i]);
        if let Some(slot) = mark.slot {
            let changed = match previous {
                Some(prev) => prev.window != mark.window || prev.slot != mark.slot,
                None => mark.window != ContainerWindowState::Hidden,
            };
            if changed && mark.window != ContainerWindowState::Hidden {
                requests.push(UiRequest::SetContainerWindow {
                    tab_id: tab_id.clone(),
                    slot_id: slot,
                    state: wire_state(mark.window),
                });
            }
        }
        let overview_opened = mark.overview == WorkflowOverviewState::Maximized
            && previous.is_none_or(|prev| prev.overview != WorkflowOverviewState::Maximized);
        if overview_opened {
            requests.push(UiRequest::OpenOverview {
                tab_id: tab_id.clone(),
            });
        }
    }

    let focused_exec =
        before.focus != Focus::ExecutionWindow && now.focus == Focus::ExecutionWindow;
    if focused_exec && workflow_active {
        if let Some(tab_id) = now.tabs.get(now.active).and_then(|m| m.tab_id.clone()) {
            requests.push(UiRequest::FocusWorkflowWindow { tab_id });
        }
    }
    requests
}

fn wire_state(state: ContainerWindowState) -> ContainerWindowStateWire {
    match state {
        ContainerWindowState::Hidden => ContainerWindowStateWire::Hidden,
        ContainerWindowState::Minimized => ContainerWindowStateWire::Minimized,
        ContainerWindowState::Maximized => ContainerWindowStateWire::Maximized,
    }
}
