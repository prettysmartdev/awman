//! Tests for git-sidebar rendering: open/closed width allocation, the
//! green-corner indicator, and the status-bar +/- summary.

use super::*;
use crate::command::commands::squad::commands::SquadCommand;
use crate::command::dispatch::FrontendAction;
use ratatui::style::Color;

// ─── Git sidebar ──────────────────────────────────────────────────────────

fn render_app(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| crate::frontend::tui::render::render_frame(app, frame))
        .unwrap();
    terminal.backend().buffer().clone()
}

/// True if the buffer contains a green rounded top-left corner ('╭'). The
/// only green rounded border in an idle app is the git sidebar (idle tabs
/// are DarkGray), so this uniquely detects a rendered sidebar.
fn has_green_sidebar_corner(buf: &ratatui::buffer::Buffer) -> Option<u16> {
    let area = *buf.area();
    for x in 0..area.width {
        for y in 0..area.height {
            let cell = buf.cell((x, y)).unwrap();
            if cell.symbol() == "\u{256d}" && cell.fg == ratatui::style::Color::Green {
                return Some(x);
            }
        }
    }
    None
}

fn set_summary(app: &App, additions: u32, deletions: u32) {
    use crate::engine::git::GitDiffSummary;
    *app.active_tab().git_diff_summary.lock().unwrap() = Some(GitDiffSummary {
        files: Vec::new(),
        added: additions,
        removed: deletions,
        branch: None,
    });
}

#[test]
fn ctrl_g_toggles_sidebar_twice_returns_to_closed() {
    use crate::frontend::tui::git_sidebar::GitSidebarState;
    let mut app = make_app();
    assert_eq!(
        app.active_tab().git_sidebar_state,
        GitSidebarState::Closed,
        "sidebar starts closed"
    );
    press_key(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert_eq!(app.active_tab().git_sidebar_state, GitSidebarState::Open);
    press_key(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab().git_sidebar_state,
        GitSidebarState::Closed,
        "toggling twice returns to Closed"
    );
}

#[test]
fn render_frame_closed_has_no_sidebar_and_uses_full_width() {
    let mut app = make_app();
    let buf = render_app(&mut app, 80, 24);
    assert!(
        has_green_sidebar_corner(&buf).is_none(),
        "closed sidebar must not render a green border"
    );
    // The vertical layout still spans the full width: the tab bar's rounded
    // top-left corner sits at column 0.
    assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "\u{256d}");
}

#[test]
fn render_frame_open_allocates_at_most_a_quarter_to_the_sidebar() {
    use crate::frontend::tui::git_sidebar::GitSidebarState;
    let mut app = make_app();
    app.active_tab_mut().git_sidebar_state = GitSidebarState::Open;
    let width = 80u16;
    let buf = render_app(&mut app, width, 24);
    let sidebar_x =
        has_green_sidebar_corner(&buf).expect("open sidebar must render a green rounded border");
    let sidebar_width = width - sidebar_x;
    assert!(
        sidebar_width <= width / 4,
        "sidebar width {sidebar_width} must be ≤ 25% of {width}"
    );
    assert_eq!(sidebar_width, 20, "80/4 == 20 columns");
}

#[test]
fn render_frame_narrow_terminal_collapses_sidebar() {
    use crate::frontend::tui::git_sidebar::GitSidebarState;
    let mut app = make_app();
    app.active_tab_mut().git_sidebar_state = GitSidebarState::Open;
    set_summary(&app, 7, 2);
    // 60/4 == 15 < MIN_SIDEBAR_WIDTH (20) → sidebar collapses to nothing.
    let buf = render_app(&mut app, 60, 24);
    assert!(
        has_green_sidebar_corner(&buf).is_none(),
        "sidebar must collapse when a quarter of the width is < 20 columns"
    );
    let text: String = {
        let area = *buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        text.contains("+7") && text.contains("-2"),
        "collapsed sidebar must still show the status-bar summary: {text:?}"
    );
}

#[test]
fn status_bar_shows_plus_minus_when_sidebar_closed_and_summary_present() {
    let mut app = make_app();
    set_summary(&app, 12, 3);
    let buf = render_app(&mut app, 80, 24);
    let text: String = {
        let area = *buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        text.contains("+12"),
        "status bar shows additions: had lines"
    );
    assert!(text.contains("-3"), "status bar shows deletions");
}

#[test]
fn status_bar_omits_summary_when_none() {
    // No summary set → no `+`/`-` diff readout injected into the status bar.
    let mut app = make_app();
    let buf = render_app(&mut app, 80, 24);
    // The idle status hint contains "ctrl-g git" but never a "+N -N" pair.
    let last_rows: String = {
        let area = *buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        !last_rows.contains("+0 -0"),
        "no diff summary must be shown when the summary is None"
    );
}

// ─── squad tab rendering (WI 0102) ──────────────────────────────────────────

fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
    let area = *buf.area();
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buf.cell((x, y)).unwrap().symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn push_squad_tab(app: &mut App) -> usize {
    let tab = crate::frontend::tui::tabs::Tab::new_squad(make_session());
    app.tabs.push(tab);
    let idx = app.tabs.len() - 1;
    app.active_tab = idx;
    idx
}

fn fake_task(name: &str) -> crate::data::fs::task_store::Task {
    use crate::data::fs::task_store::{MountScope, TaskStatus};
    let now = chrono::Utc::now();
    crate::data::fs::task_store::Task {
        id: name.to_string(),
        name: name.to_string(),
        description: "a test task".into(),
        repo_scope: std::path::PathBuf::from("/tmp"),
        mount_scope: MountScope::GitRoot,
        overlays: Vec::new(),
        interval_secs: 300,
        status: TaskStatus::Active,
        agent: None,
        model: None,
        backoff_until: None,
        created_at: now,
        updated_at: now,
        last_run_at: None,
        trigger_requested_at: None,
        last_run_status: None,
        unmet_env: Vec::new(),
    }
}

/// WI 0112 Part 1: the New Tab dialog advertises `Ctrl+S` in its key-hint
/// row, beside Enter/Esc, and no longer in the prompt body.
#[test]
fn the_new_tab_dialog_hint_row_advertises_ctrl_s_to_open_squad() {
    let mut app = make_app();
    press_key(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    let text = buffer_text(&render_app(&mut app, 100, 30));
    assert!(
        text.contains("[Enter] submit   [Esc] cancel   [Ctrl+S] open squad"),
        "the hint row must carry the squad shortcut: {text}"
    );
    assert!(
        !text.contains("Press Ctrl-S"),
        "the prompt body must not repeat the hint: {text}"
    );
}

/// Every other text-input dialog keeps the plain two-key hint.
#[test]
fn a_command_text_input_dialog_keeps_the_plain_hint_row() {
    let mut app = make_app();
    app.active_dialog = Some(Dialog::TextInput {
        title: "Task name".to_string(),
        prompt: "Name:".to_string(),
        editor: crate::frontend::tui::text_edit::TextEdit::new(false),
    });
    let text = buffer_text(&render_app(&mut app, 100, 30));
    assert!(text.contains("[Enter] submit   [Esc] cancel"), "{text}");
    assert!(
        !text.contains("[Ctrl+S] open squad"),
        "only the New Tab dialog advertises the squad shortcut: {text}"
    );
}

#[test]
fn render_frame_squad_tab_no_slots_draws_squad_body_not_execution_window() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    let buf = render_app(&mut app, 80, 24);
    let text = buffer_text(&buf);
    assert!(text.contains("squad"), "the squad body must render: {text}");
    assert!(
        text.contains("enter detail"),
        "the squad key-hint line must render: {text}"
    );
    assert!(
        !text.contains("awman"),
        "the ordinary execution window's idle title must not render for the squad tab: {text}"
    );
}

/// WI 0110: the squad tab's body is the card grid, so the tab's status log —
/// where a failed command's error normally lands — is never on screen. Without
/// this line a failed `squad remove` looked exactly like a key that did nothing,
/// which is how the delete bug stayed invisible.
#[test]
fn a_failed_squad_action_renders_above_the_task_grid() {
    use crate::frontend::tui::tabs::ExecutionPhase;
    let mut app = make_app();
    push_squad_tab(&mut app);
    app.active_tab_mut().execution_phase = ExecutionPhase::Error {
        command: "squad remove task-a".into(),
        message: "remote returned status 400".into(),
    };

    let text = buffer_text(&render_app(&mut app, 100, 24));

    assert!(
        text.contains("squad remove task-a failed"),
        "the failed action must name itself above the grid: {text}"
    );
    assert!(
        text.contains("remote returned status 400"),
        "and carry the daemon's reason: {text}"
    );
    assert!(
        text.contains("enter detail"),
        "the grid and its hints stay on screen underneath: {text}"
    );
}

#[test]
fn render_frame_squad_tab_with_slots_draws_normal_execution_rendering() {
    use crate::frontend::tui::tabs::ExecutionPhase;
    let mut app = make_app();
    push_squad_tab(&mut app);
    app.active_tab_mut()
        .start_container("claude".into(), "awman-abc".into(), 80, 24);
    app.active_tab_mut().execution_phase = ExecutionPhase::Running {
        command: "squad attach task-a".into(),
    };
    let buf = render_app(&mut app, 80, 24);
    let text = buffer_text(&buf);
    assert!(
        !text.contains("enter detail"),
        "the squad body must not render while an attach session owns the tab's slots: {text}"
    );
    assert!(
        text.contains("running: squad attach task-a"),
        "the ordinary execution window must render instead: {text}"
    );
}

/// WI 0110: `ctrl-\ detach` must be advertised in the hint bar whenever a
/// squad attach session has the container view on screen — not just for an
/// ordinary command's maximized container. Before this test's fix,
/// `render_status_bar` returned its squad-grid hint for every squad tab
/// regardless of `container_slots`, so the detach hint never appeared during
/// attach even though keys were already going to the PTY.
#[test]
fn squad_attach_session_shows_the_detach_hint_in_the_status_bar() {
    use crate::frontend::tui::tabs::{ContainerWindowState, ExecutionPhase};
    let mut app = make_app();
    push_squad_tab(&mut app);
    app.focus = Focus::ExecutionWindow;
    {
        let tab = app.active_tab_mut();
        tab.start_container("claude".into(), "awman-squad-task-a".into(), 80, 24);
        tab.container_window_state = ContainerWindowState::Maximized;
        tab.execution_phase = ExecutionPhase::Running {
            command: "squad attach task-a".into(),
        };
    }

    let text = buffer_text(&render_app(&mut app, 100, 24));
    assert!(
        text.contains("ctrl-\\ detach"),
        "the detach hint must render during a squad attach session: {text}"
    );
}

#[test]
fn squad_task_detail_modal_renders_over_the_body_and_stays_live() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    {
        let state = app.active_tab().squad.as_ref().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        snap.tasks = vec![fake_task("task-a")];
        snap.loaded = true;
    }
    let task = app
        .active_tab()
        .squad
        .as_ref()
        .unwrap()
        .snapshot
        .lock()
        .unwrap()
        .tasks[0]
        .clone();
    app.active_dialog = Some(Dialog::SquadTaskDetail(
        crate::frontend::tui::dialogs::SquadDetailState {
            name: "task-a".to_string(),
            task,
        },
    ));

    // Mutate the underlying snapshot as the poller would, then let
    // `tick_all_tabs` refresh the open modal from it (app.rs §"WI 0102: keep
    // the squad task-detail modal live").
    {
        let state = app.active_tab().squad.as_ref().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        snap.tasks[0].description = "updated by the poller".to_string();
    }
    app.tick_all_tabs();
    match &app.active_dialog {
        Some(Dialog::SquadTaskDetail(state)) => {
            assert_eq!(state.task.description, "updated by the poller")
        }
        _ => panic!("the modal must remain open across a tick"),
    }

    let buf = render_app(&mut app, 80, 24);
    let text = buffer_text(&buf);
    assert!(
        text.contains("task: task-a"),
        "the detail modal must render over the squad body: {text}"
    );
    assert!(
        text.contains("updated by the poller"),
        "the modal must render the ticked-in snapshot value, not the stale one it opened with: {text}"
    );
}

#[test]
fn tab_bar_shows_squad_label_at_minimum_and_wide_widths() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    let narrow = buffer_text(&render_app(&mut app, 20, 24));
    assert!(
        narrow.contains("squad"),
        "the squad tab label must render even at the minimum tab width: {narrow}"
    );
    let wide = buffer_text(&render_app(&mut app, 100, 24));
    assert!(
        wide.contains("squad"),
        "the squad tab label must render in a wide terminal too: {wide}"
    );
}

#[test]
fn ctrl_g_on_squad_tab_renders_no_git_sidebar() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    press_key(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    let buf = render_app(&mut app, 80, 24);
    assert!(
        has_green_sidebar_corner(&buf).is_none(),
        "Ctrl-G must render no git sidebar for the squad tab (a non-project tab)"
    );
}

// ─── ACP agent windows (WI 0104) ───────────────────────────────────────────

fn push_acp_slot(app: &mut App, agent: &str) -> crate::frontend::tui::tabs::SharedAcpState {
    use crate::frontend::tui::tabs::{AcpSlotState, ContainerSlot};
    let state: crate::frontend::tui::tabs::SharedAcpState =
        std::sync::Arc::new(std::sync::Mutex::new(AcpSlotState::default()));
    app.active_tab_mut()
        .container_slots
        .push(ContainerSlot::new_acp(
            String::new(),
            agent.to_string(),
            state.clone(),
        ));
    state
}

/// Coordinates of the first rounded top-left corner ('╭') rendered in
/// `color`, scanning row-major (top-to-bottom, then left-to-right).
fn find_border_corner_of_color(
    buf: &ratatui::buffer::Buffer,
    color: ratatui::style::Color,
) -> Option<(u16, u16)> {
    let area = *buf.area();
    for y in 0..area.height {
        for x in 0..area.width {
            let cell = buf.cell((x, y)).unwrap();
            if cell.symbol() == "\u{256d}" && cell.fg == color {
                return Some((x, y));
            }
        }
    }
    None
}

#[test]
fn execution_window_border_is_acp_purple_not_green_when_focused_slot_is_acp() {
    use crate::frontend::tui::acp_view::ACP_BORDER_COLOR;
    use crate::frontend::tui::tabs::ExecutionPhase;
    // Full-render companion to the unit-level
    // `window_border_color_done_focused_acp_is_purple`: the focused+Done
    // execution window must actually paint its border in the ACP identity
    // color, not fall back to the stdio green.
    let mut app = make_app();
    push_acp_slot(&mut app, "claude");
    app.focus = Focus::ExecutionWindow;
    app.active_tab_mut().execution_phase = ExecutionPhase::Done {
        command: "chat".into(),
        exit_code: 0,
    };
    let buf = render_app(&mut app, 80, 24);
    assert!(
        find_border_corner_of_color(&buf, ACP_BORDER_COLOR).is_some(),
        "the focused+Done execution window must render its border in the ACP identity color"
    );
    assert!(
        find_border_corner_of_color(&buf, ratatui::style::Color::Green).is_none(),
        "the focused+Done state must not render the stdio green border when the focused slot is ACP"
    );
}

#[test]
fn mixed_parallel_group_renders_one_purple_and_one_green_minimized_bar() {
    use crate::frontend::tui::acp_view::ACP_BORDER_COLOR;
    use crate::frontend::tui::tabs::ContainerWindowState;
    // A two-slot tab mixing stdio + ACP (WI 0104), both minimized: the
    // stdio slot's bar is green (`render_container_bars`), the ACP slot's
    // bar is purple (`render_acp_bars`), and — since the idle execution
    // phase keeps the tab bar and execution window both DarkGray — these
    // are the only green/purple rounded corners in the frame, so finding
    // one of each proves both bars actually drew.
    let mut app = make_app();
    app.active_tab_mut()
        .start_container("claude".into(), "awman-a".into(), 80, 24);
    push_acp_slot(&mut app, "codex");
    app.active_tab_mut().container_window_state = ContainerWindowState::Minimized;

    let buf = render_app(&mut app, 80, 30);
    let green = find_border_corner_of_color(&buf, ratatui::style::Color::Green)
        .expect("the stdio slot must render a green minimized bar");
    let purple = find_border_corner_of_color(&buf, ACP_BORDER_COLOR)
        .expect("the ACP slot must render a purple minimized bar");
    assert!(
        purple.1 > green.1,
        "the bars tile in slot order — stdio (slot 0) above ACP (slot 1): \
         green at {green:?}, purple at {purple:?}"
    );
}

#[test]
fn acp_permission_request_modal_renders_through_the_dialog_framework() {
    // `TuiAcpFrontend::request_permission` opens exactly this `Dialog::Custom`
    // shape (see `per_command/acp_frontend.rs`); this confirms the generic
    // dialog framework actually renders it, title/body/hotkeys included.
    let mut app = make_app();
    app.active_dialog = Some(Dialog::Custom {
        title: "ACP Permission Request".to_string(),
        body: "The agent wants to run:\n\n  Write config.json (edit)\n\nAllow this action?"
            .to_string(),
        keys: vec![('1', "Allow once".to_string()), ('2', "Reject".to_string())],
    });
    let buf = render_app(&mut app, 80, 24);
    let text = buffer_text(&buf);
    assert!(
        text.contains("ACP Permission Request"),
        "modal title must render: {text}"
    );
    assert!(
        text.contains("Write config.json"),
        "modal body must render: {text}"
    );
    assert!(
        text.contains("[1] Allow once"),
        "the first hotkey option must render: {text}"
    );
    assert!(
        text.contains("[2] Reject"),
        "the second hotkey option must render: {text}"
    );
}

// ─── Workflow Overview: minimized / maximized ───────────────────────────

/// Publish a parallel workflow of `n` sibling steps into the active tab.
fn set_parallel_workflow(app: &App, n: usize) {
    use crate::frontend::tui::tabs::{
        StepViewStatus, WorkflowStepKind, WorkflowStepView, WorkflowViewState,
    };
    *app.active_tab().shared.workflow_state.lock().unwrap() = Some(WorkflowViewState {
        steps: (0..n)
            .map(|i| WorkflowStepView {
                name: format!("step-{i}"),
                status: StepViewStatus::Running,
                agent: None,
                model: None,
                depends_on: vec![],
                kind: WorkflowStepKind::Agent,
            })
            .collect(),
        current_step: None,
        max_concurrent: None,
    });
}

#[test]
fn frame_overview_defaults_to_minimized_and_ctrl_o_maximizes_it() {
    let mut app = make_app();
    set_parallel_workflow(&app, 4);

    let minimized = buffer_text(&render_app(&mut app, 80, 40));
    assert!(
        minimized.contains("4 steps\u{2026}"),
        "the default overview summarizes a parallel stage: {minimized}"
    );
    assert!(
        !minimized.contains("step-0"),
        "the minimized overview names no individual step: {minimized}"
    );

    press_key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    let maximized = buffer_text(&render_app(&mut app, 80, 40));
    for i in 0..4 {
        assert!(
            maximized.contains(&format!("step-{i}")),
            "the maximized overview names every parallel step: {maximized}"
        );
    }
}

fn set_sequential_workflow(app: &App, n: usize, failed: Option<usize>) {
    use crate::frontend::tui::tabs::{
        StepViewStatus, WorkflowStepKind, WorkflowStepView, WorkflowViewState,
    };
    *app.active_tab().shared.workflow_state.lock().unwrap() = Some(WorkflowViewState {
        steps: (0..n)
            .map(|i| WorkflowStepView {
                name: format!("stage-{i}"),
                status: if failed == Some(i) {
                    StepViewStatus::Error
                } else {
                    StepViewStatus::Pending
                },
                agent: Some("agent".into()),
                model: None,
                depends_on: if i == 0 {
                    vec![]
                } else {
                    vec![format!("stage-{}", i - 1)]
                },
                kind: WorkflowStepKind::Agent,
            })
            .collect(),
        current_step: None,
        max_concurrent: None,
    });
}

fn set_running_workflow(app: &mut App, n: usize) {
    set_sequential_workflow(app, n, None);
    app.active_tab_mut().execution_phase = crate::frontend::tui::tabs::ExecutionPhase::Running {
        command: "exec workflow".into(),
    };
}

#[test]
fn wide_workflow_shows_full_visible_names_and_coloured_edge_markers() {
    use ratatui::style::Color;
    let mut app = make_app();
    set_sequential_workflow(&app, 14, Some(13));
    let first = render_app(&mut app, 100, 40);
    let text = buffer_text(&first);
    for name in ["stage-0", "stage-1", "stage-2", "stage-3", "stage-4"] {
        assert!(
            text.contains(name),
            "visible step name {name} is rendered: {text}"
        );
    }
    let layout = app.active_tab().last_overview_hlayout.unwrap();
    let rect = app.active_tab().last_overview_rect.unwrap();
    assert_eq!(layout.visible, 5);
    assert_eq!(
        first
            .cell((rect.x + rect.width - 1, rect.y + 1))
            .unwrap()
            .symbol(),
        "›"
    );
    assert_eq!(
        first
            .cell((rect.x + rect.width - 1, rect.y + 1))
            .unwrap()
            .fg,
        Color::Red,
        "an error in hidden right columns makes the right marker red"
    );
    assert_eq!(
        first.cell((rect.x, rect.y + 1)).unwrap().symbol(),
        " ",
        "no left marker at offset zero"
    );

    app.active_tab_mut().workflow_overview_hscroll_follow = false;
    app.active_tab_mut().workflow_overview_hscroll_offset = 4;
    let middle = render_app(&mut app, 100, 40);
    let rect = app.active_tab().last_overview_rect.unwrap();
    assert_eq!(middle.cell((rect.x, rect.y + 1)).unwrap().symbol(), "‹");
    assert_eq!(
        middle
            .cell((rect.x + rect.width - 1, rect.y + 1))
            .unwrap()
            .symbol(),
        "›"
    );
}

#[test]
fn workflow_hint_tracks_overflow_and_is_shown_with_maximized_container() {
    use crate::frontend::tui::tabs::{ContainerWindowState, WorkflowOverviewState};
    let mut app = make_app();
    set_running_workflow(&mut app, 14);
    let text = buffer_text(&render_app(&mut app, 100, 40));
    assert!(
        text.contains("shift-←/→ scroll stages (1–5 of 14)"),
        "{text}"
    );

    set_sequential_workflow(&app, 3, None);
    let text = buffer_text(&render_app(&mut app, 100, 40));
    assert!(
        !text.contains("shift-←/→ scroll stages"),
        "fit workflow has no hint: {text}"
    );

    set_running_workflow(&mut app, 14);
    app.active_tab_mut().workflow_overview_state = WorkflowOverviewState::Maximized;
    push_stdio_slot(&mut app, "overview-hint");
    app.active_tab_mut().container_window_state = ContainerWindowState::Maximized;
    let text = buffer_text(&render_app(&mut app, 100, 40));
    assert!(
        text.contains("shift-←/→ scroll stages"),
        "maximized-container branch keeps hint: {text}"
    );
}

#[test]
fn workflow_scroll_hint_is_hidden_while_a_dialog_owns_the_keys() {
    let mut app = make_app();
    set_running_workflow(&mut app, 14);
    app.active_dialog = Some(Dialog::Notice {
        title: "Notice".into(),
        body: "Workflow paused".into(),
        copy_key: None,
        copy_zshrc_snippet: None,
    });

    let text = buffer_text(&render_app(&mut app, 100, 40));
    assert!(!text.contains("shift-←/→ scroll stages"), "{text}");
}

#[test]
fn a_new_remote_workflow_invocation_resets_manual_horizontal_scroll() {
    let mut app = make_app();
    set_running_workflow(&mut app, 14);
    let first_id = uuid::Uuid::new_v4();
    *app.active_tab()
        .shared
        .workflow_invocation_id
        .lock()
        .unwrap() = Some(first_id);
    render_app(&mut app, 100, 40);

    app.active_tab_mut().workflow_overview_hscroll_offset = 8;
    app.active_tab_mut().workflow_overview_hscroll_follow = false;
    render_app(&mut app, 100, 40);
    assert_eq!(app.active_tab().workflow_overview_hscroll_offset, 8);

    *app.active_tab()
        .shared
        .workflow_invocation_id
        .lock()
        .unwrap() = Some(uuid::Uuid::new_v4());
    render_app(&mut app, 100, 40);
    assert_eq!(app.active_tab().workflow_overview_hscroll_offset, 0);
    assert!(app.active_tab().workflow_overview_hscroll_follow);
}

#[test]
fn git_sidebar_can_narrow_workflow_overview_into_horizontal_overflow() {
    use crate::frontend::tui::git_sidebar::GitSidebarState;
    let mut app = make_app();
    set_sequential_workflow(&app, 5, None);
    let closed = render_app(&mut app, 100, 40);
    assert!(!app.active_tab().last_overview_hlayout.unwrap().overflows());
    app.active_tab_mut().git_sidebar_state = GitSidebarState::Open;
    let open = render_app(&mut app, 100, 40);
    assert!(app.active_tab().last_overview_hlayout.unwrap().overflows());
    assert!(has_green_sidebar_corner(&open).is_some());
    assert!(has_green_sidebar_corner(&closed).is_none());
}

/// Append one stdio container slot named `container_name`, the way a parallel
/// workflow group fills the tab (`start_container` replaces the whole group,
/// so it cannot build a multi-slot tab).
fn push_stdio_slot(app: &mut App, container_name: &str) {
    use crate::frontend::tui::tabs::ContainerSlot;
    let mut slot = ContainerSlot::new(String::new(), "claude".into(), 0);
    if let Some(info) = slot.container_info.as_mut() {
        info.container_name = container_name.to_string();
    }
    app.active_tab_mut().container_slots.push(slot);
}

#[test]
fn ctrl_o_and_ctrl_m_min_max_independently() {
    use crate::frontend::tui::tabs::{ContainerWindowState, WorkflowOverviewState};
    let mut app = make_app();
    set_parallel_workflow(&app, 3);
    push_stdio_slot(&mut app, "awman-only");
    app.active_tab_mut().container_window_state = ContainerWindowState::Maximized;

    // Maximized container: the single slot owns the PTY overlay.
    // `container_inner_area` is published by the renderer exactly when it
    // draws that overlay.
    render_app(&mut app, 80, 40);
    assert!(
        app.active_tab().container_inner_area.is_some(),
        "a maximized slot draws its PTY overlay while the overview is minimized"
    );

    // Ctrl-O maximizes the overview. The PTY overlay stays up: the two
    // windows share the body rather than displacing each other.
    app.active_tab_mut().container_inner_area = None;
    press_key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    let both_max = buffer_text(&render_app(&mut app, 80, 40));
    assert_eq!(
        app.active_tab().workflow_overview_state,
        WorkflowOverviewState::Maximized
    );
    assert_eq!(
        app.active_tab().container_window_state,
        ContainerWindowState::Maximized,
        "Ctrl-O must not touch the container's own min/max"
    );
    assert!(
        app.active_tab().container_inner_area.is_some(),
        "a maximized overview must not put the PTY overlay away: {both_max}"
    );
    for i in 0..3 {
        assert!(
            both_max.contains(&format!("step-{i}")),
            "the maximized overview names every parallel step: {both_max}"
        );
    }

    // Ctrl-M minimizes the container. The overview stays maximized.
    press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
    app.active_tab_mut().container_inner_area = None;
    let overview_only = buffer_text(&render_app(&mut app, 80, 40));
    assert_eq!(
        app.active_tab().workflow_overview_state,
        WorkflowOverviewState::Maximized,
        "Ctrl-M must not touch the overview's own min/max"
    );
    assert!(
        app.active_tab().container_inner_area.is_none(),
        "a minimized container draws no PTY overlay: {overview_only}"
    );
    assert!(
        overview_only.contains("awman-only"),
        "the minimized container falls back to its status bar: {overview_only}"
    );
    assert!(overview_only.contains("step-2"), "{overview_only}");

    // Ctrl-O minimizes the overview. The container stays minimized.
    press_key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    let both_min = buffer_text(&render_app(&mut app, 80, 40));
    assert_eq!(
        app.active_tab().container_window_state,
        ContainerWindowState::Minimized
    );
    assert!(
        both_min.contains("3 steps\u{2026}") && !both_min.contains("step-2"),
        "the overview is back to its one-box-per-stage summary: {both_min}"
    );
}

#[test]
fn maximized_overview_leaves_the_pty_overlay_its_share_of_the_body() {
    use crate::frontend::tui::tabs::{ContainerWindowState, WorkflowOverviewState};
    let mut app = make_app();
    // 40 parallel steps want 120 rows — far more than the frame has.
    set_parallel_workflow(&app, 40);
    push_stdio_slot(&mut app, "awman-only");
    app.active_tab_mut().container_window_state = ContainerWindowState::Maximized;
    app.active_tab_mut().workflow_overview_state = WorkflowOverviewState::Maximized;

    // 40 rows: 3 tab bar + 5 bottom chrome leaves 32 for the body. With the
    // PTY overlay on screen the overview may take at most half of that (16 →
    // 5 whole boxes), so the overlay keeps the rest.
    let text = buffer_text(&render_app(&mut app, 80, 40));
    assert!(
        text.contains("+ 36 more\u{2026}"),
        "the overview is capped at half the body and says what it hides: {text}"
    );
    let inner = app
        .active_tab()
        .container_inner_area
        .expect("the PTY overlay is still drawn alongside a maximized overview");
    assert!(
        inner.height >= 10,
        "the PTY overlay keeps a usable share of the body, got {inner:?}"
    );
}

#[test]
fn maximized_overview_wins_space_and_truncates_the_container_status_bars() {
    use crate::frontend::tui::tabs::{ContainerWindowState, WorkflowOverviewState};
    let mut app = make_app();
    set_parallel_workflow(&app, 6);
    for i in 0..6 {
        push_stdio_slot(&mut app, &format!("awman-c{i}"));
    }
    app.active_tab_mut().container_window_state = ContainerWindowState::Minimized;
    app.active_tab_mut().workflow_overview_state = WorkflowOverviewState::Maximized;

    // 30 rows: 3 tab bar + 5 bottom chrome leaves 22 for the body. No PTY
    // overlay is up, so the overview gets the whole body: it asks for 18 (6
    // boxes) and is served first; the execution window keeps its 5-row floor
    // out of the 4 left, so no container bar fits at all.
    let text = buffer_text(&render_app(&mut app, 80, 30));
    assert!(
        text.contains("step-5"),
        "the overview is served its full height first: {text}"
    );
    assert!(
        !text.contains("awman-c0"),
        "container status bars are truncated into whatever the overview leaves: {text}"
    );

    // Given room for both, the bars come back.
    let roomy = buffer_text(&render_app(&mut app, 80, 60));
    assert!(roomy.contains("step-5"), "{roomy}");
    assert!(roomy.contains("awman-c0"), "{roomy}");
}

#[test]
fn maximized_overview_never_grows_past_the_space_between_tab_bar_and_command_box() {
    use crate::frontend::tui::tabs::WorkflowOverviewState;
    let mut app = make_app();
    set_parallel_workflow(&app, 40);
    app.active_tab_mut().workflow_overview_state = WorkflowOverviewState::Maximized;

    // 40 steps want 120 rows; the frame has 24 - 3 - 5 = 16 to give, which is
    // 5 whole boxes. The command box must still render at the bottom.
    let text = buffer_text(&render_app(&mut app, 80, 24));
    assert!(
        text.contains("+ 36 more\u{2026}"),
        "the clipped stage advertises how many steps it is hiding: {text}"
    );
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[lines.len() - 3..]
            .iter()
            .any(|l| l.contains("\u{256d}") || l.contains("\u{2570}")),
        "the command box keeps its rows at the bottom of the frame: {text}"
    );
}

/// WI 0106 Part 5: the card carries the task name, a summary, the **last-run
/// outcome** and time, and the next evaluation — the outcome being what
/// actually happened on the last run, not whether the task is scheduled.
#[test]
fn squad_task_cards_render_rounded_borders_and_the_last_run_outcome() {
    use crate::data::fs::task_store::{RunStatus, TaskStatus};
    let mut app = make_app();
    push_squad_tab(&mut app);
    {
        let state = app.active_tab().squad.as_ref().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        let mut triggered = fake_task("issue-triage");
        triggered.description = "watch the issue tracker".into();
        triggered.last_run_at = Some(chrono::Utc::now());
        triggered.last_run_status = Some(RunStatus::WorkflowExecuted);

        let mut quiet = fake_task("nightly-sweep");
        quiet.last_run_at = Some(chrono::Utc::now());
        quiet.last_run_status = Some(RunStatus::NotTriggered);
        // Paused is scheduling state, reported separately from the outcome.
        quiet.status = TaskStatus::Paused;

        let never = fake_task("brand-new");

        snap.tasks = vec![triggered, quiet, never];
        snap.loaded = true;
    }

    let buf = render_app(&mut app, 100, 30);
    let text = buffer_text(&buf);

    assert!(
        text.contains('\u{256d}') && text.contains('\u{256f}'),
        "cards must use ratatui's rounded borders: {text}"
    );
    assert!(text.contains("issue-triage"), "{text}");
    assert!(text.contains("watch the issue tracker"), "{text}");
    assert!(
        text.contains("workflow executed"),
        "a card must show its last run's outcome, not its active/paused state: {text}"
    );
    assert!(
        text.contains("not triggered"),
        "an un-triggered last run must read as such: {text}"
    );
    assert!(
        text.contains("never run"),
        "a task that has never run must say so rather than showing a blank outcome: {text}"
    );
    assert!(
        text.contains("paused"),
        "paused remains visible, as scheduling state alongside the outcome: {text}"
    );
    assert!(text.contains("Next:"), "{text}");
}

#[test]
fn a_squad_tab_with_no_tasks_renders_an_empty_state_instead_of_empty_cards() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    {
        let state = app.active_tab().squad.as_ref().unwrap();
        state.snapshot.lock().unwrap().loaded = true;
    }
    let text = buffer_text(&render_app(&mut app, 80, 24));
    assert!(
        text.contains("No squad tasks yet"),
        "an empty grid must render its empty state: {text}"
    );
}

/// The detail modal repeats the per-task action keys, so a user who opened it
/// does not have to close it to remember or trigger them.
#[test]
fn the_squad_detail_modal_shows_the_task_scoped_action_tooltip() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    let task = fake_task("issue-triage");
    {
        let state = app.active_tab().squad.as_ref().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        snap.tasks = vec![task.clone()];
        snap.loaded = true;
    }
    app.active_dialog = Some(Dialog::SquadTaskDetail(
        crate::frontend::tui::dialogs::SquadDetailState {
            name: "issue-triage".to_string(),
            task,
        },
    ));

    let text = buffer_text(&render_app(&mut app, 100, 30));
    for key in ["h history", "a attach", "p pause", "r resume", "d delete"] {
        assert!(
            text.contains(key),
            "the modal's action tooltip must offer {key:?}: {text}"
        );
    }
}

// ─── the run history lives in its own modal ─────────────────────────────────

/// The bug this split fixes: a task whose description ran long pushed the run
/// history off the bottom of the detail modal. The detail modal now shows no
/// history at all — a long description simply uses the room the table used to
/// take — and the tail of the description is still on screen.
#[test]
fn the_squad_detail_modal_shows_no_run_history_and_gives_a_long_description_the_room() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    let mut task = fake_task("issue-triage");
    task.description = format!("{} TAIL-MARKER", "wordy ".repeat(60));
    {
        let state = app.active_tab().squad.as_ref().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        snap.tasks = vec![task.clone()];
        snap.loaded = true;
    }
    app.active_dialog = Some(Dialog::SquadTaskDetail(
        crate::frontend::tui::dialogs::SquadDetailState {
            name: "issue-triage".to_string(),
            task,
        },
    ));

    let text = buffer_text(&render_app(&mut app, 100, 40));
    assert!(
        !text.contains("Run history"),
        "the detail modal must not render the run history any more: {text}"
    );
    assert!(
        text.contains("TAIL-MARKER"),
        "the end of a long description must survive now the table is gone: {text}"
    );
    assert!(
        text.contains("Updated:"),
        "the field block must still be laid out under the description: {text}"
    );
}

/// The history modal renders the run table for its own task, plus the Esc
/// wording for where it was opened from.
#[test]
fn the_squad_history_modal_renders_the_run_table_and_its_esc_wording() {
    use crate::data::fs::task_store::{Run, RunStatus};
    let run = Run {
        id: "run-1".into(),
        task_id: "issue-triage".into(),
        status: RunStatus::Failed,
        workflow_path: None,
        workflow_state_path: None,
        session_id: None,
        started_at: chrono::Utc::now(),
        finished_at: None,
        error: Some("boom".into()),
        reason: Some("no new issues".into()),
        unmet_env: Vec::new(),
    };

    for (from_detail, expected) in [(true, "esc back to detail"), (false, "esc close")] {
        let mut app = make_app();
        push_squad_tab(&mut app);
        {
            let state = app.active_tab().squad.as_ref().unwrap();
            let mut snap = state.snapshot.lock().unwrap();
            snap.tasks = vec![fake_task("issue-triage")];
            snap.runs = vec![run.clone()];
            snap.loaded = true;
        }
        app.active_dialog = Some(Dialog::SquadTaskHistory(
            crate::frontend::tui::dialogs::SquadHistoryState {
                name: "issue-triage".to_string(),
                runs: vec![run.clone()],
                scroll: 0,
                from_detail,
            },
        ));

        let text = buffer_text(&render_app(&mut app, 100, 30));
        assert!(
            text.contains("run history: issue-triage"),
            "the history modal must title itself with its task: {text}"
        );
        for column in ["Started", "Status", "Reason", "Finished", "Error"] {
            assert!(
                text.contains(column),
                "the history modal must render the {column:?} column: {text}"
            );
        }
        assert!(
            text.contains("boom"),
            "the history modal must render its runs: {text}"
        );
        assert!(
            text.contains(expected),
            "from_detail={from_detail} must hint {expected:?}: {text}"
        );
    }
}

/// The leader verdict's reason gets its own history column right after the
/// status, and the modal grows wide enough to show a long one in full.
#[test]
fn the_squad_history_modal_shows_the_verdict_reason_in_full() {
    use crate::data::fs::task_store::{Run, RunStatus};
    let reason =
        "no new issues were opened against the repository since the previous evaluation ran";
    let run = Run {
        id: "run-1".into(),
        task_id: "issue-triage".into(),
        status: RunStatus::NotTriggered,
        workflow_path: None,
        workflow_state_path: None,
        session_id: None,
        started_at: chrono::Utc::now(),
        finished_at: Some(chrono::Utc::now()),
        error: None,
        reason: Some(reason.into()),
        unmet_env: Vec::new(),
    };
    let mut app = make_app();
    push_squad_tab(&mut app);
    app.active_dialog = Some(Dialog::SquadTaskHistory(
        crate::frontend::tui::dialogs::SquadHistoryState {
            name: "issue-triage".to_string(),
            runs: vec![run],
            scroll: 0,
            from_detail: false,
        },
    ));

    let text = buffer_text(&render_app(&mut app, 220, 30));
    assert!(
        text.contains(reason),
        "a wide terminal must show the whole reason: {text}"
    );
    let header = text
        .lines()
        .find(|line| line.contains("Started"))
        .expect("the history table header");
    let status_at = header.find("Status").unwrap();
    let reason_at = header.find("Reason").expect("a Reason column");
    let error_at = header.find("Error").unwrap();
    assert!(
        status_at < reason_at && reason_at < error_at,
        "Reason sits after Status and before Error: {header}"
    );
}

/// On a terminal too short for the whole field block, the row that says how to
/// leave the modal is the one thing that must not be clipped — it is reserved
/// before the body is laid out.
#[test]
fn the_squad_modals_keep_their_key_hint_row_on_a_short_terminal() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    let mut task = fake_task("issue-triage");
    task.description = "wordy ".repeat(80);
    {
        let state = app.active_tab().squad.as_ref().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        snap.tasks = vec![task.clone()];
        snap.loaded = true;
    }
    app.active_dialog = Some(Dialog::SquadTaskDetail(
        crate::frontend::tui::dialogs::SquadDetailState {
            name: "issue-triage".to_string(),
            task,
        },
    ));
    let text = buffer_text(&render_app(&mut app, 100, 18));
    assert!(
        text.contains("esc close"),
        "the detail modal's tooltip must survive a short terminal: {text}"
    );
    assert!(
        text.contains("c cancel") && text.contains("d delete"),
        "a tooltip too long for one row wraps rather than dropping actions: {text}"
    );

    app.active_dialog = Some(Dialog::SquadTaskHistory(
        crate::frontend::tui::dialogs::SquadHistoryState {
            name: "issue-triage".to_string(),
            runs: Vec::new(),
            scroll: 0,
            from_detail: true,
        },
    ));
    let text = buffer_text(&render_app(&mut app, 100, 12));
    assert!(
        text.contains("esc back to detail"),
        "the history modal's hint row must survive a short terminal: {text}"
    );
}

/// A task that has never run gets a plain empty state rather than a bare
/// header row.
#[test]
fn the_squad_history_modal_says_so_when_a_task_has_never_run() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    {
        let state = app.active_tab().squad.as_ref().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        snap.tasks = vec![fake_task("issue-triage")];
        snap.loaded = true;
    }
    app.active_dialog = Some(Dialog::SquadTaskHistory(
        crate::frontend::tui::dialogs::SquadHistoryState {
            name: "issue-triage".to_string(),
            runs: Vec::new(),
            scroll: 0,
            from_detail: false,
        },
    ));

    let text = buffer_text(&render_app(&mut app, 100, 30));
    assert!(
        text.contains("has not run yet"),
        "an empty history must say so: {text}"
    );
}

// ─── every task-interview modal shows its key bindings ──────────────────────

/// The dialogs the squad task interview raises must each render the row that
/// says which keys do what.
///
/// Two of them did not. `TextInput` reserved no row for its hint at all, so
/// `[Enter] submit / [Esc] cancel` was laid out one row past the bottom of the
/// dialog and never drawn — and `TextInput` is what collects the interval, the
/// leader agent, the leader model and every overlay. `YesNo` and `KindSelect`
/// sized themselves one and two rows short respectively, which was invisible
/// for a one-line body and clipped the hint the moment the body was longer —
/// which every squad confirmation's body is.
///
/// The dialogs are driven directly rather than through the interview, because
/// the interview blocks on a frontend thread; what is under test is the
/// rendering, and these are the exact shapes `per_command/squad.rs` builds.
#[test]
fn every_modal_in_the_squad_task_interview_renders_its_key_bindings() {
    use crate::frontend::tui::text_edit::TextEdit;

    let mut editor = TextEdit::new(false);
    editor.set_text("6h");
    let mut multiline = TextEdit::new(true);
    multiline.set_text("watch the issue tracker");

    let cases: Vec<(&str, Dialog, Vec<&str>)> = vec![
        (
            "the interval / agent / model / overlay prompt",
            Dialog::TextInput {
                title: "Evaluation interval".into(),
                prompt: "How often to evaluate (e.g. 6h, 1d):".into(),
                editor,
            },
            vec!["[Enter] submit", "[Esc] cancel"],
        ),
        (
            "the description editor",
            Dialog::MultilineInput {
                title: "Edit squad task description".into(),
                prompt: "Describe when this task fires and what squad should do.\n\
                         (Ctrl+Enter to submit)"
                    .into(),
                editor: multiline,
            },
            vec!["submit", "[Enter] newline", "[Esc] cancel"],
        ),
        (
            // The real body: three lines once the question is included, which
            // is exactly the case the old height clipped.
            "the replace-overlays / agent-pool confirmation",
            Dialog::YesNo {
                title: "Agents and models".into(),
                body: "A global squad configuration exists.\n\n\
                       Use those settings for this task? \
                       (No = give this task its own agents and models)"
                    .into(),
            },
            vec!["[y] Yes", "[n] No", "[Esc] Cancel"],
        ),
        (
            "the workspace-choice picker",
            Dialog::KindSelect {
                title: "Task Workspace".into(),
                options: vec![
                    ("1".into(), "Default Task Workspace".into()),
                    ("2".into(), "Custom Folder / Repo".into()),
                ],
            },
            vec!["[1-9] select", "[Esc] cancel"],
        ),
    ];

    for (label, dialog, expected) in cases {
        let mut app = make_app();
        app.active_dialog = Some(dialog);
        let text = buffer_text(&render_app(&mut app, 100, 30));
        for hint in expected {
            assert!(
                text.contains(hint),
                "{label} must show its {hint:?} key binding:\n{text}"
            );
        }
    }
}

/// The squad key-setup notice must offer a way to copy the key or the
/// zshrc snippet — the body text itself cannot be selected/copied by mouse
/// in a terminal UI, so a keybinding is the only way to get the key out.
/// A notice unrelated to a key (e.g. "daemon did not start") must not show
/// copy hints that would do nothing.
#[test]
fn the_squad_key_notice_shows_copy_hints_only_when_it_has_something_to_copy() {
    let mut app = make_app();
    app.active_dialog = Some(Dialog::Notice {
        title: "squad authentication".into(),
        body: "╔══╗\n║ deadbeef ║\n╚══╝".into(),
        copy_key: Some("deadbeef".into()),
        copy_zshrc_snippet: Some("export AWMAN_SQUAD_KEY=deadbeef".into()),
    });
    let text = buffer_text(&render_app(&mut app, 100, 30));
    assert!(text.contains("[c] copy key"), "{text}");
    assert!(text.contains("[z] copy .zshrc snippet"), "{text}");
    assert!(text.contains("[Enter] dismiss"), "{text}");

    let mut app = make_app();
    app.active_dialog = Some(Dialog::Notice {
        title: "squad daemon did not start".into(),
        body: "failed to start the squad daemon: did not become ready".into(),
        copy_key: None,
        copy_zshrc_snippet: None,
    });
    let text = buffer_text(&render_app(&mut app, 100, 30));
    assert!(
        !text.contains("[c] copy key") && !text.contains("[z] copy .zshrc snippet"),
        "a notice with nothing to copy must not offer copy hints: {text}"
    );
}

// ─── card labels, the pending trigger, and where failures are reported ──────

/// Every value on a card is introduced by a grey label. The description's
/// label sits on its own row so the text still gets the card's full width;
/// the last-run *timestamp* — previously an unlabelled indented continuation
/// of the outcome line — now says what it is.
#[test]
fn squad_task_cards_label_the_description_and_the_last_run_timestamp() {
    use crate::data::fs::task_store::RunStatus;
    let mut app = make_app();
    push_squad_tab(&mut app);
    {
        let state = app.active_tab().squad.as_ref().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        let mut task = fake_task("issue-triage");
        task.description = "watch the issue tracker".into();
        task.last_run_at = Some(chrono::Utc::now());
        task.last_run_status = Some(RunStatus::WorkflowExecuted);
        snap.tasks = vec![task];
        snap.loaded = true;
    }

    let text = buffer_text(&render_app(&mut app, 100, 30));

    assert!(
        text.contains("Description"),
        "the description must carry a label: {text}"
    );
    assert!(
        text.contains("watch the issue tracker"),
        "labelling the description must not cost it the width it needs: {text}"
    );
    let last_run = chrono::Utc::now().format("%Y-%m-%d %H:%M").to_string();
    assert!(
        text.contains(&format!("Last run: {last_run}")),
        "the last-run timestamp must be labelled, not left as a bare date: {text}"
    );
    assert!(
        text.contains("Outcome: workflow executed"),
        "the outcome keeps a label of its own: {text}"
    );
}

/// A triggered task must look triggered. Leaving the card showing its ordinary
/// next-evaluation time would make `t` read as a key that did nothing.
#[test]
fn a_task_with_a_pending_trigger_says_so_instead_of_its_scheduled_time() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    {
        let state = app.active_tab().squad.as_ref().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        let mut task = fake_task("issue-triage");
        task.last_run_at = Some(chrono::Utc::now());
        task.trigger_requested_at = Some(chrono::Utc::now());
        snap.tasks = vec![task];
        snap.loaded = true;
    }

    let text = buffer_text(&render_app(&mut app, 100, 30));
    assert!(
        text.contains("triggered"),
        "a pending trigger must be visible on the card: {text}"
    );
}

/// A failed squad action is reported in the hint bar above the command box —
/// the row that is on screen for every tab — and not as a header above the
/// card grid, which pushed every card down a row.
#[test]
fn a_failed_squad_action_is_reported_in_the_hint_bar_not_above_the_card_grid() {
    use crate::frontend::tui::tabs::ExecutionPhase;
    let mut app = make_app();
    push_squad_tab(&mut app);
    {
        let state = app.active_tab().squad.as_ref().unwrap();
        let mut snap = state.snapshot.lock().unwrap();
        snap.tasks = vec![fake_task("issue-triage")];
        snap.loaded = true;
    }
    app.active_tab_mut().execution_phase = ExecutionPhase::Error {
        command: "squad remove issue-triage".to_string(),
        message: "task \"issue-triage\" was not found".to_string(),
    };

    let buf = render_app(&mut app, 100, 30);
    let text = buffer_text(&buf);
    assert!(
        text.contains("squad remove issue-triage failed"),
        "the failure must be reported somewhere: {text}"
    );
    assert!(
        text.contains("was not found"),
        "the reason travels with it: {text}"
    );
    // The hint bar is the row directly above the command box's top border.
    let rows: Vec<String> = text.lines().map(str::to_string).collect();
    let hint_row = rows
        .iter()
        .position(|row| row.contains("squad remove issue-triage failed"))
        .expect("the failure renders");
    assert!(
        rows[hint_row + 1].contains("command"),
        "the failure belongs in the hint bar above the command box, not in the \
         grid header:\n{text}"
    );
    assert!(
        !text.contains("Exit code"),
        "a squad tab has no execution window, so its exit-code hint is noise: {text}"
    );
}

/// The missing-key recovery has to explain three things a 401 does not: that
/// the key is unrecoverable, what accepting will do, and what it costs.
#[test]
fn the_missing_key_dialog_explains_the_recovery_and_its_key_bindings() {
    let mut app = make_app();
    app.active_dialog = Some(Dialog::SquadKeyMissing);

    let text = buffer_text(&render_app(&mut app, 100, 30));

    assert!(
        text.contains("AWMAN_SQUAD_KEY"),
        "the variable to set must be named: {text}"
    );
    assert!(
        text.contains("only once"),
        "why the key cannot simply be looked up must be stated: {text}"
    );
    assert!(
        text.contains("[y]") && text.contains("[n / Esc]"),
        "both answers must be offered: {text}"
    );
    assert!(
        text.contains("old key will stop working"),
        "the cost of refreshing must be stated before it is accepted: {text}"
    );
}

// ─── WI 0112: squad indicator, card colours, inactive command box ──────────

/// Render and also report whether the terminal cursor was left visible.
fn render_app_with_cursor(
    app: &mut App,
    width: u16,
    height: u16,
) -> (ratatui::buffer::Buffer, bool) {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| crate::frontend::tui::render::render_frame(app, frame))
        .unwrap();
    let visible = terminal.backend().cursor_visible();
    (terminal.backend().buffer().clone(), visible)
}

/// The last row of the buffer, trimmed of trailing spaces.
fn bottom_row(buf: &ratatui::buffer::Buffer) -> String {
    let area = *buf.area();
    let y = area.height - 1;
    (0..area.width)
        .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Colour of the cell holding the first occurrence of `symbol` on row `y`.
fn fg_of_symbol_on_row(buf: &ratatui::buffer::Buffer, y: u16, symbol: &str) -> Option<Color> {
    let area = *buf.area();
    (0..area.width)
        .map(|x| buf.cell((x, y)).unwrap())
        .find(|c| c.symbol() == symbol)
        .map(|c| c.fg)
}

#[test]
fn workflow_context_footer_shows_host_path_with_cwd_or_worktree_and_squad() {
    let mut app = make_app();
    let path = "/home/user/.awman/context/workflow/invocation";
    activate_workflow_context(&mut app, path);
    for worktree in [None, Some(std::path::PathBuf::from("/worktrees/current"))] {
        *app.active_tab().shared.active_worktree_path.lock().unwrap() = worktree.clone();
        let row = bottom_row(&render_app(&mut app, 200, 24));
        assert!(row.contains(path), "{row}");
        assert!(row.contains("Context:"), "{row}");
        assert!(row.contains("[Ctrl-Shift-C copy]"), "{row}");
        assert!(row.ends_with("squad ●"), "{row}");
        if worktree.is_some() {
            assert!(row.contains("Using worktree: /worktrees/current"), "{row}");
        } else {
            assert!(row.contains("CWD:"), "{row}");
        }
    }
}

#[test]
fn workflow_context_footer_preserves_hint_and_squad_on_narrow_screens() {
    let mut app = make_app();
    *app.active_tab().shared.active_worktree_path.lock().unwrap() = Some("/work".into());
    activate_workflow_context(
        &mut app,
        "/home/user/very-long-host-directory/上下文/.awman/context/workflow/invocation",
    );
    for width in [80, 100, 120] {
        let row = bottom_row(&render_app(&mut app, width, 24));
        assert!(row.contains("Using worktree: /work"), "{row}");
        assert!(row.contains("Context:"), "{row}");
        assert!(row.contains("[Ctrl-Shift-C copy]"), "{row}");
        assert!(row.contains('…'), "{row}");
        assert!(row.ends_with("squad ●"), "{row}");
    }
    for width in 1..40 {
        render_app(&mut app, width, 24);
    }
}

#[test]
fn workflow_context_footer_yields_to_the_full_directory_and_squad_indicator() {
    let mut app = make_app();
    for worktree in [
        None,
        Some(std::path::PathBuf::from(
            "/worktrees/a-long-project/feature-branch",
        )),
        Some(std::path::PathBuf::from("/worktrees/上下文/feature-branch")),
    ] {
        *app.active_tab().shared.active_worktree_path.lock().unwrap() = worktree.clone();
        let directory = worktree
            .as_deref()
            .unwrap_or(app.active_tab().session.working_dir());
        let label = if worktree.is_some() {
            "  Using worktree: "
        } else {
            "  CWD: "
        };
        let primary = format!("{label}{}", directory.display());
        let primary_width = unicode_width::UnicodeWidthStr::width(primary.as_str());
        let primary_cells: String = primary
            .chars()
            .map(|character| {
                let padding = unicode_width::UnicodeWidthChar::width(character)
                    .unwrap_or(0)
                    .saturating_sub(1);
                format!("{character}{}", " ".repeat(padding))
            })
            .collect();

        for width in [6, 30, 60, 80, 120, 200] {
            *app.active_tab()
                .shared
                .workflow_context_path
                .lock()
                .unwrap() = None;
            let without_context = render_app(&mut app, width, 24);
            activate_workflow_context(
                &mut app,
                "/host/context/workflow/very-long-invocation-directory",
            );
            let with_context = render_app(&mut app, width, 24);
            let row = bottom_row(&with_context);
            if width as usize >= primary_width + 10 {
                assert!(row.contains(&primary_cells), "{row}");
            }
            if (width as usize).saturating_sub(8) < primary_width + 2 + 30 {
                assert_eq!(bottom_row(&without_context), row);
            } else {
                assert!(row.contains("Context:"), "{row}");
                assert!(row.contains("[Ctrl-Shift-C copy]"), "{row}");
            }
            assert!(row.ends_with('●'), "{row}");
        }
    }
}

#[test]
fn workflow_context_footer_tracks_the_active_tab_and_execution_lifecycle() {
    use crate::frontend::tui::tabs::ExecutionPhase;

    let mut app = make_app();
    activate_workflow_context(&mut app, "/host/context/first");
    app.tabs.push(Tab::new(make_session()));
    app.active_tab = 1;
    app.active_tab_mut().execution_phase = ExecutionPhase::Running {
        command: "exec workflow".into(),
    };
    assert!(!bottom_row(&render_app(&mut app, 200, 24)).contains("Context:"));
    activate_workflow_context(&mut app, "/host/context/second");
    assert!(bottom_row(&render_app(&mut app, 200, 24)).contains("/host/context/second"));
    app.active_tab = 0;
    assert!(bottom_row(&render_app(&mut app, 200, 24)).contains("/host/context/first"));

    for phase in [
        ExecutionPhase::Idle,
        ExecutionPhase::Done {
            command: "exec workflow".into(),
            exit_code: 0,
        },
        ExecutionPhase::Error {
            command: "exec workflow".into(),
            message: "failed".into(),
        },
    ] {
        app.active_tab_mut().execution_phase = phase;
        let row = bottom_row(&render_app(&mut app, 200, 24));
        assert!(!row.contains("Context:"), "{row}");
        assert!(!row.contains("Ctrl-Shift-C"), "{row}");
    }
}

#[test]
fn workflow_context_footer_remains_visible_with_suggestions() {
    let mut app = make_app();
    activate_workflow_context(&mut app, "/host/context/current");
    app.focus = Focus::CommandBox;
    app.suggestion_row = vec!["chat".into()];
    let row = bottom_row(&render_app(&mut app, 160, 24));
    assert!(row.contains("chat"), "{row}");
    assert!(row.contains("/host/context/current"), "{row}");
    assert!(row.contains("[Ctrl-Shift-C copy]"), "{row}");
    assert!(row.ends_with("squad ●"), "{row}");
}

fn set_indicator(app: &App, state: crate::engine::squad::SquadHealth) {
    *app.squad_indicator.lock().unwrap() = state;
}

#[test]
fn the_squad_indicator_is_pinned_to_the_right_of_the_bottom_row_in_every_state() {
    use crate::engine::squad::SquadHealth as SquadIndicator;
    for (state, colour) in [
        (SquadIndicator::Unknown, Color::DarkGray),
        (SquadIndicator::NotRunning, Color::DarkGray),
        (SquadIndicator::Unreachable, Color::Yellow),
        (SquadIndicator::Failed, Color::Red),
        // WI 0116 §6c: the seventh state, sharing `Unreachable`'s yellow.
        (SquadIndicator::EnvUnmet, Color::Yellow),
        (SquadIndicator::Running, Color::Blue),
        (SquadIndicator::Healthy, Color::Green),
    ] {
        let mut app = make_app();
        set_indicator(&app, state);
        let buf = render_app(&mut app, 80, 24);
        let row = bottom_row(&buf);
        assert!(
            row.ends_with("squad \u{25cf}"),
            "{state:?}: the indicator must be the last thing on the bottom row: {row:?}"
        );
        assert_eq!(
            fg_of_symbol_on_row(&buf, 23, "\u{25cf}"),
            Some(colour),
            "{state:?}: circle colour"
        );
        assert!(
            row.contains("CWD:"),
            "{state:?}: the CWD context still renders to the left: {row:?}"
        );
    }
}

#[test]
fn the_squad_indicator_renders_on_the_squad_tab_too() {
    use crate::engine::squad::SquadHealth as SquadIndicator;
    let mut app = make_app();
    push_squad_tab(&mut app);
    set_indicator(&app, SquadIndicator::Failed);
    let buf = render_app(&mut app, 80, 24);
    assert!(bottom_row(&buf).ends_with("squad \u{25cf}"));
    assert_eq!(fg_of_symbol_on_row(&buf, 23, "\u{25cf}"), Some(Color::Red));
}

#[test]
fn the_squad_indicator_stays_right_aligned_when_suggestions_are_showing() {
    use crate::engine::squad::SquadHealth as SquadIndicator;
    let mut app = make_app();
    set_indicator(&app, SquadIndicator::Healthy);
    for c in "cha".chars() {
        press_char(&mut app, c);
    }
    app.update_suggestions();
    assert!(!app.suggestion_row.is_empty(), "test needs suggestions");
    let buf = render_app(&mut app, 60, 24);
    let row = bottom_row(&buf);
    assert!(row.contains("chat"), "suggestions render: {row:?}");
    assert!(
        row.ends_with("squad \u{25cf}"),
        "the indicator still owns the right edge: {row:?}"
    );
}

#[test]
fn a_long_cwd_is_truncated_so_the_squad_indicator_fits() {
    let mut app = make_app();
    let buf = render_app(&mut app, 30, 24);
    let row = bottom_row(&buf);
    assert!(row.ends_with("squad \u{25cf}"), "{row:?}");
    assert!(row.chars().count() <= 30);
}

#[test]
fn a_row_too_narrow_for_the_word_draws_only_the_circle() {
    let mut app = make_app();
    let buf = render_app(&mut app, 6, 24);
    let row = bottom_row(&buf);
    assert!(row.ends_with('\u{25cf}'), "{row:?}");
    assert!(!row.contains("squad"), "{row:?}");
}

/// Cells of the card whose title contains `name`: the top border row `y`
/// and the x-range of the card, found from the rounded corners around the
/// title.
fn card_frame(buf: &ratatui::buffer::Buffer, name: &str) -> (u16, u16, u16) {
    let area = *buf.area();
    for y in 0..area.height {
        let row: String = (0..area.width)
            .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
            .collect();
        if let Some(pos) = row.find(name) {
            let chars: Vec<&str> = (0..area.width)
                .map(|x| buf.cell((x, y)).unwrap().symbol())
                .collect();
            let title_x = row[..pos].chars().count();
            let left = (0..title_x)
                .rev()
                .find(|&x| chars[x] == "\u{256d}")
                .expect("card has a top-left corner");
            let right = (title_x..chars.len())
                .find(|&x| chars[x] == "\u{256e}")
                .expect("card has a top-right corner");
            return (y, left as u16, right as u16);
        }
    }
    panic!("no card titled {name}");
}

fn squad_app_with(tasks: Vec<crate::data::fs::task_store::Task>, selected: usize) -> App {
    let mut app = make_app();
    push_squad_tab(&mut app);
    {
        let state = app.active_tab_mut().squad.as_mut().unwrap();
        state.selected = selected;
        let mut snap = state.snapshot.lock().unwrap();
        snap.tasks = tasks;
        snap.loaded = true;
    }
    app
}

/// WI 0112 Part 3: unselected cards are dashed, the selected card is solid
/// and carries the `➡` marker; neither axis expresses the other.
#[test]
fn the_selected_card_is_solid_with_an_arrow_and_the_others_are_dashed() {
    let mut app = squad_app_with(vec![fake_task("alpha"), fake_task("beta")], 1);
    let buf = render_app(&mut app, 100, 30);
    let text = buffer_text(&buf);
    assert!(
        text.contains("\u{27a1} beta"),
        "selected title carries the arrow: {text}"
    );
    assert!(
        !text.contains("\u{27a1} alpha"),
        "unselected title does not: {text}"
    );

    let (y_a, left_a, right_a) = card_frame(&buf, "alpha");
    let (y_b, left_b, right_b) = card_frame(&buf, "beta");
    // Top edge just left of the top-right corner (the title occupies the
    // cells after the top-left one), and the side edge one row down.
    assert_eq!(
        buf.cell((right_a - 1, y_a)).unwrap().symbol(),
        "\u{254c}",
        "dashed top"
    );
    assert_eq!(
        buf.cell((left_a, y_a + 1)).unwrap().symbol(),
        "\u{2506}",
        "dashed side"
    );
    assert_eq!(
        buf.cell((right_b - 1, y_b)).unwrap().symbol(),
        "\u{2500}",
        "solid top"
    );
    assert_eq!(
        buf.cell((left_b, y_b + 1)).unwrap().symbol(),
        "\u{2502}",
        "solid side"
    );
}

#[test]
fn card_border_colour_follows_task_state_for_selected_and_unselected_cards() {
    use crate::data::fs::task_store::{RunStatus, TaskStatus};
    let now = chrono::Utc::now();
    let mut paused = fake_task("paused-one");
    paused.status = TaskStatus::Paused;
    let mut running = fake_task("running-one");
    running.last_run_at = Some(now);
    running.last_run_status = Some(RunStatus::Running);
    let mut triggered = fake_task("trig-one");
    triggered.last_run_at = Some(now);
    triggered.last_run_status = Some(RunStatus::WorkflowExecuted);
    triggered.trigger_requested_at = Some(now);
    let mut failed = fake_task("failed-one");
    failed.last_run_at = Some(now);
    failed.last_run_status = Some(RunStatus::Failed);
    let never = fake_task("never-one");
    let mut active = fake_task("active-one");
    active.last_run_at = Some(now);
    active.last_run_status = Some(RunStatus::WorkflowExecuted);

    let expected = [
        ("paused-one", Color::DarkGray),
        ("running-one", Color::Blue),
        ("trig-one", Color::Magenta),
        ("failed-one", Color::Red),
        ("never-one", Color::Yellow),
        ("active-one", Color::Green),
    ];
    for selected in [0usize, 3] {
        let mut app = squad_app_with(
            vec![
                paused.clone(),
                running.clone(),
                triggered.clone(),
                failed.clone(),
                never.clone(),
                active.clone(),
            ],
            selected,
        );
        let buf = render_app(&mut app, 120, 40);
        for (name, colour) in expected {
            let (y, left, _) = card_frame(&buf, name);
            assert_eq!(
                buf.cell((left, y)).unwrap().fg,
                colour,
                "{name} border colour (selected index {selected})"
            );
        }
    }
}

#[test]
fn a_paused_card_is_dashed_when_unselected_and_solid_when_selected_in_grey_both_times() {
    use crate::data::fs::task_store::TaskStatus;
    let mut paused = fake_task("paused-one");
    paused.status = TaskStatus::Paused;
    let other = fake_task("other-one");

    let mut app = squad_app_with(vec![paused.clone(), other.clone()], 1);
    let buf = render_app(&mut app, 100, 30);
    let (y, left, right) = card_frame(&buf, "paused-one");
    assert_eq!(buf.cell((right - 1, y)).unwrap().symbol(), "\u{254c}");
    assert_eq!(buf.cell((left, y)).unwrap().fg, Color::DarkGray);

    let mut app = squad_app_with(vec![paused, other], 0);
    let buf = render_app(&mut app, 100, 30);
    let (y, left, right) = card_frame(&buf, "paused-one");
    assert_eq!(buf.cell((right - 1, y)).unwrap().symbol(), "\u{2500}");
    assert_eq!(buf.cell((left, y)).unwrap().fg, Color::DarkGray);
}

/// WI 0112 Part 4: the command box is inactive on the squad grid, explains
/// the keys, and never places the cursor.
#[test]
fn the_command_box_is_inactive_and_cursorless_on_the_squad_tab() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    app.tick_all_tabs();
    let (buf, cursor_visible) = render_app_with_cursor(&mut app, 100, 30);
    let text = buffer_text(&buf);
    assert!(text.contains("command (inactive)"), "{text}");
    assert!(
        text.contains(
            "Use \u{2191} \u{2193} \u{2190} \u{2192} and Enter to navigate the squad tab"
        ),
        "{text}"
    );
    assert!(!cursor_visible, "no cursor on the squad grid");

    // A normal tab is unchanged: focused box, cursor placed.
    app.active_tab = 0;
    app.tick_all_tabs();
    let (buf, cursor_visible) = render_app_with_cursor(&mut app, 100, 30);
    let text = buffer_text(&buf);
    assert!(text.contains(" command "), "{text}");
    assert!(!text.contains("navigate the squad tab"), "{text}");
    assert!(cursor_visible, "the ordinary command box shows its cursor");
}

#[test]
fn the_command_box_is_ordinary_during_a_squad_attach_session() {
    use crate::frontend::tui::tabs::{ContainerWindowState, ExecutionPhase};
    let mut app = make_app();
    push_squad_tab(&mut app);
    {
        let tab = app.active_tab_mut();
        tab.start_container("claude".into(), "awman-squad-task-a".into(), 80, 24);
        tab.container_window_state = ContainerWindowState::Maximized;
        tab.execution_phase = ExecutionPhase::Running {
            command: "squad attach task-a".into(),
        };
    }
    let text = buffer_text(&render_app(&mut app, 100, 30));
    assert!(
        !text.contains("navigate the squad tab"),
        "attach sessions use the ordinary command box: {text}"
    );
}

// ─── WI-0115 §1: the step-failure control board ──────────────────────────

/// A `WorkflowControlBoardState` with no failure and everything switched off.
fn plain_control_board() -> crate::frontend::tui::dialogs::WorkflowControlBoardState {
    crate::frontend::tui::dialogs::WorkflowControlBoardState {
        step_name: "implement".into(),
        focused_step_name: "implement".into(),
        can_launch_next: true,
        can_continue_current: false,
        can_restart: true,
        can_go_back: true,
        can_finish: false,
        continue_unavailable_reason: None,
        cancel_to_previous_unavailable_reason: None,
        finish_workflow_unavailable_reason: None,
        restart_unavailable_reason: None,
        can_dismiss: false,
        launch_next_label: None,
        parallel_peer_count: 0,
        parallel_peers_running: 0,
        failure_lines: Vec::new(),
        retry_failed_step: None,
        in_parallel_group: false,
    }
}

#[test]
fn failure_control_board_names_the_failed_step_and_shows_the_error() {
    let mut app = make_app();
    let mut state = plain_control_board();
    state.failure_lines = vec!["Exit code: 1".into(), "Ran for 214s".into()];
    state.launch_next_label = Some("Skip to 'review' (new container)".into());
    app.active_dialog = Some(Dialog::WorkflowControlBoard(state));

    let text = buffer_text(&render_app(&mut app, 90, 30));
    assert!(text.contains("step failed"), "title must say so: {text}");
    assert!(
        text.contains("Failed step: implement"),
        "the failed step must be named: {text}"
    );
    assert!(text.contains("Exit code: 1"), "error detail: {text}");
    assert!(
        text.contains("Restart failed step"),
        "restart must be offered: {text}"
    );
    assert!(
        text.contains("Cancel to prev"),
        "back must be offered: {text}"
    );
    assert!(
        text.contains("Skip to 'review'"),
        "the next step must be named: {text}"
    );
    assert!(
        text.contains("[^C] Cancel workflow"),
        "Ctrl-C is the way out of a failure board: {text}"
    );
    assert!(
        !text.contains("Finish workflow"),
        "a failure board must never offer Finish: {text}"
    );
}

#[test]
fn control_board_without_a_failure_keeps_its_ordinary_title() {
    let mut app = make_app();
    app.active_dialog = Some(Dialog::WorkflowControlBoard(plain_control_board()));

    let text = buffer_text(&render_app(&mut app, 90, 30));
    assert!(text.contains("Workflow Control"), "{text}");
    assert!(!text.contains("step failed"), "{text}");
    assert!(text.contains("Restart current step"), "{text}");
}

/// On a terminal with room for it, the detail modal is wide enough that every
/// action hint sits on one row.
#[test]
fn the_squad_detail_modal_fits_every_hint_on_one_row() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    let task = fake_task("issue-triage");
    app.active_dialog = Some(Dialog::SquadTaskDetail(
        crate::frontend::tui::dialogs::SquadDetailState {
            name: "issue-triage".to_string(),
            task,
        },
    ));
    let text = buffer_text(&render_app(&mut app, 140, 40));
    assert!(
        text.lines().any(|line| line.contains("h history")
            && line.contains("c cancel")
            && line.contains("esc close")),
        "every hint must share one row: {text}"
    );
}

/// The trigger/cancel/pause confirmation draws the prompt it was handed:
/// the title, the question and every choice under its own hotkey.
///
/// The words are asserted against the prompt rather than spelled here — the
/// copy is `command::prompts`' (WI 0114 F-55), and a render test that
/// repeated it would pin the frontend to a wording it does not own.
#[test]
fn the_squad_action_confirmation_draws_layer_2s_prompt() {
    let action = FrontendAction::CancelSquadRun;
    let prompt =
        SquadCommand::confirm_prompt(action, "issue-triage").expect("cancelling a run asks first");
    let mut app = make_app();
    push_squad_tab(&mut app);
    app.active_dialog = Some(Dialog::SquadActionConfirm {
        action,
        name: "issue-triage".to_string(),
        prompt: prompt.clone(),
    });
    let text = buffer_text(&render_app(&mut app, 120, 30));
    assert!(text.contains(&prompt.title), "{text}");
    assert!(text.contains(&prompt.body), "{text}");
    for choice in &prompt.choices {
        assert!(
            text.contains(&format!("[{}] {}", choice.key, choice.label))
                || text.contains(&format!("[{} / Esc] {}", choice.key, choice.label)),
            "choice {:?} is not drawn: {text}",
            choice.key
        );
    }
    // Esc answers the dismissing choice, and is advertised only there.
    assert!(text.contains("/ Esc] back"), "{text}");
}

/// A board opened mid-parallel-group after a peer failed offers a red
/// `(r)etry failed step {name}` line below the arrow options and above the
/// bottom key row; everything else is unchanged.
#[test]
fn control_board_offers_retry_of_a_failed_parallel_peer() {
    let mut app = make_app();
    let mut state = plain_control_board();
    // What the engine sets on a board opened while peers still run.
    state.can_restart = false;
    state.can_go_back = false;
    state.restart_unavailable_reason =
        Some("Restart applies only to the focused container. Switch with Ctrl-S.".into());
    state.cancel_to_previous_unavailable_reason =
        Some("Cannot go back while other agents in this group are still running.".into());
    state.finish_workflow_unavailable_reason =
        Some("Cannot finish while other agents in this group are still running.".into());
    state.can_dismiss = true;
    state.retry_failed_step = Some("lint".into());
    app.active_dialog = Some(Dialog::WorkflowControlBoard(state));

    let buf = render_app(&mut app, 90, 30);
    let text = buffer_text(&buf);
    let rows: Vec<&str> = text.lines().collect();
    let row_of = |needle: &str| {
        rows.iter()
            .position(|r| r.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle:?}: {text}"))
    };
    let retry_row = row_of("(r)etry failed step lint");
    assert!(retry_row > row_of("Next: same container"), "{text}");
    assert!(retry_row < row_of("[^C] Abort"), "{text}");
    assert!(text.contains("Restart current step"), "{text}");

    let byte = rows[retry_row].find("(r)etry").unwrap();
    let x = rows[retry_row][..byte].chars().count() as u16;
    let cell = buf.cell((x, retry_row as u16)).unwrap();
    assert_eq!(cell.fg, ratatui::style::Color::Red, "the retry line is red");
}

#[test]
fn control_board_without_a_failed_peer_has_no_retry_line() {
    let mut app = make_app();
    app.active_dialog = Some(Dialog::WorkflowControlBoard(plain_control_board()));
    let text = buffer_text(&render_app(&mut app, 90, 30));
    assert!(!text.contains("(r)etry"), "{text}");
}

/// Mid-group, the arrows act on the whole group and say so; a single-step
/// board keeps its ordinary labels.
#[test]
fn control_board_labels_are_group_aware() {
    let mut app = make_app();
    let mut state = plain_control_board();
    state.can_dismiss = true;
    state.in_parallel_group = true;
    state.finish_workflow_unavailable_reason =
        Some("Cannot finish while other agents in this group are still running.".into());
    app.active_dialog = Some(Dialog::WorkflowControlBoard(state));
    let text = buffer_text(&render_app(&mut app, 90, 30));
    assert!(text.contains("Restart group / agent"), "{text}");
    assert!(text.contains("Back (cancel group)"), "{text}");
    assert!(text.contains("Next (cancel group)"), "{text}");

    let mut app = make_app();
    app.active_dialog = Some(Dialog::WorkflowControlBoard(plain_control_board()));
    let text = buffer_text(&render_app(&mut app, 90, 30));
    assert!(text.contains("Restart current step"), "{text}");
    assert!(text.contains("Cancel to prev"), "{text}");
    assert!(text.contains("Next: new container"), "{text}");
}

// ─── Container window title for setup/teardown steps ────────────────────────

/// A setup/teardown step's container is titled with the step, not the agent,
/// and still says it is containerized.
#[test]
fn a_setup_step_container_window_is_titled_with_the_step() {
    use crate::frontend::tui::tabs::ContainerWindowState;
    let mut app = make_app();
    {
        let tab = app.active_tab_mut();
        tab.start_container("Claude Code".into(), "awman-abc".into(), 80, 24);
        tab.container_window_state = ContainerWindowState::Maximized;
        let info = tab
            .focused_slot_mut()
            .and_then(|s| s.container_info.as_mut())
            .unwrap();
        info.phase_step_title = Some("[setup] run_shell: make deps".into());
    }

    let text = buffer_text(&render_app(&mut app, 120, 30));
    assert!(
        text.contains("[setup] run_shell: make deps (containerized)"),
        "title must name the step: {text}"
    );
    assert!(
        !text.contains("Claude Code"),
        "a shell step's window must not be titled with the agent: {text}"
    );
}

#[test]
fn an_agent_container_window_keeps_the_agent_title() {
    use crate::frontend::tui::tabs::ContainerWindowState;
    let mut app = make_app();
    {
        let tab = app.active_tab_mut();
        tab.start_container("Claude Code".into(), "awman-abc".into(), 80, 24);
        tab.container_window_state = ContainerWindowState::Maximized;
    }

    let text = buffer_text(&render_app(&mut app, 120, 30));
    assert!(text.contains("Claude Code (containerized)"), "{text}");
}
