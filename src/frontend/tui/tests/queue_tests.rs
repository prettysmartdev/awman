//! Work-queue panel (WI 0143): panel, badge, body view, pick and the
//! close-tab prompt, against a fake controller.

use super::*;
use crate::frontend::tui::queue::{
    LeaseCloseChoice, PickError, PickRun, QueueBadge, QueueEntry, QueueSnapshot, QueueSource,
    TabLease,
};
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Default)]
struct FakeSource {
    picked: Mutex<Option<PickRun>>,
    snapshot: Mutex<QueueSnapshot>,
    generation: std::sync::atomic::AtomicU64,
    pick_result: Mutex<Option<Result<PickRun, PickError>>>,
    finished: Mutex<Vec<(String, LeaseCloseChoice)>>,
    refreshes: std::sync::atomic::AtomicUsize,
    finish_error: Mutex<Option<String>>,
}

impl QueueSource for FakeSource {
    fn prepared_run(&self, id: &str) -> Option<PickRun> {
        self.picked
            .lock()
            .unwrap()
            .clone()
            .filter(|run| run.lease_id == id)
    }
    fn snapshot(&self) -> QueueSnapshot {
        self.snapshot.lock().unwrap().clone()
    }
    fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn mark_seen(&self) {
        self.snapshot.lock().unwrap().new_since_seen = 0;
    }
    fn refresh(&self) {
        self.refreshes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    fn pick(&self, _server: &str, _item_id: &str) -> Result<PickRun, PickError> {
        let result = self
            .pick_result
            .lock()
            .unwrap()
            .take()
            .unwrap_or(Err(PickError::Failed("none".into())));
        if let Ok(run) = &result {
            *self.picked.lock().unwrap() = Some(run.clone());
        }
        result
    }
    fn finish_lease(&self, lease_id: &str, choice: LeaseCloseChoice) -> Result<(), String> {
        self.finished
            .lock()
            .unwrap()
            .push((lease_id.into(), choice));
        match self.finish_error.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn entry(id: &str, title: &str, for_you: bool) -> QueueEntry {
    let mut labels = BTreeMap::new();
    labels.insert("team".to_string(), "core".to_string());
    QueueEntry {
        id: id.into(),
        server: "acme".into(),
        title: title.into(),
        summary: Some("Fix the flaky test".into()),
        kind: "workflow".into(),
        labels,
        priority: 5,
        age_ms: 125_000,
        for_you,
        body: None,
        source_url: None,
    }
}

fn app_with_queue(items: Vec<QueueEntry>) -> (App, Arc<FakeSource>) {
    let mut app = make_app();
    let source = Arc::new(FakeSource::default());
    {
        let mut snap = source.snapshot.lock().unwrap();
        snap.for_you = items.iter().filter(|e| e.for_you).count();
        snap.items = items;
        snap.loaded = true;
    }
    app.queue = Some(source.clone());
    (app, source)
}

fn render(app: &mut App, width: u16, height: u16) -> String {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| crate::frontend::tui::render::render_frame(app, frame))
        .unwrap();
    let buf = terminal.backend().buffer().clone();
    (0..buf.area().height)
        .map(|y| {
            (0..buf.area().width)
                .map(|x| buf.cell((x, y)).unwrap().symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn ctrl_q(app: &mut App) {
    press_key(app, KeyCode::Char('q'), KeyModifiers::CONTROL);
}

#[test]
fn ctrl_q_opens_the_panel_listing_title_server_labels_priority_age_and_for_you() {
    let (mut app, _) = app_with_queue(vec![
        entry("1", "Fix flaky test", true),
        entry("2", "Bump deps", false),
    ]);
    ctrl_q(&mut app);
    assert!(matches!(app.active_dialog, Some(Dialog::QueuePanel(_))));
    let text = render(&mut app, 120, 30);
    for want in [
        "Work queue",
        "TITLE",
        "SERVER",
        "LABELS",
        "PRI",
        "AGE",
        "FOR YOU",
    ] {
        assert!(text.contains(want), "missing {want}: {text}");
    }
    assert!(
        text.contains("Fix flaky test") && text.contains("Bump deps"),
        "{text}"
    );
    assert!(
        text.contains("acme") && text.contains("team=core"),
        "{text}"
    );
    assert!(text.contains("2m"), "age 125s renders as 2m: {text}");
    assert_eq!(text.matches("for you").count(), 1, "{text}");
    assert!(text.contains("[p] pick"), "{text}");
}

#[test]
fn ctrl_q_without_a_worker_says_so_and_opens_nothing() {
    let mut app = make_app();
    ctrl_q(&mut app);
    assert!(app.active_dialog.is_none());
    assert!(
        app.status_bar.text.contains("work queue"),
        "{}",
        app.status_bar.text
    );
}

#[test]
fn ctrl_q_does_not_reach_the_container_pty() {
    use crate::frontend::tui::keymap::{map_key, Action, FocusContext};
    let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
    for ctx in [
        FocusContext::CommandBox,
        FocusContext::ExecutionWindow,
        FocusContext::ContainerMaximized,
        FocusContext::SquadList,
    ] {
        assert_eq!(map_key(key, ctx, false), Action::OpenQueuePanel, "{ctx:?}");
    }
}

#[test]
fn an_empty_loaded_queue_says_so() {
    let (mut app, _) = app_with_queue(vec![]);
    ctrl_q(&mut app);
    assert!(render(&mut app, 100, 24).contains("No eligible work items."));
}

#[test]
fn the_status_bar_shows_the_badge_and_the_queue_hint() {
    let (mut app, source) = app_with_queue(vec![entry("1", "A", true)]);
    source.snapshot.lock().unwrap().new_since_seen = 2;
    source
        .generation
        .store(1, std::sync::atomic::Ordering::SeqCst);
    app.tick_all_tabs();
    assert_eq!(app.queue_badge, QueueBadge { for_you: 1, new: 2 });
    let text = render(&mut app, 120, 30);
    assert!(text.contains("2 new work items"), "{text}");
    assert!(text.contains("1 for you"), "{text}");
    assert!(text.contains("ctrl-q queue"), "{text}");
    // Opening the panel clears it.
    ctrl_q(&mut app);
    assert!(app.queue_badge.is_empty());
}

#[test]
fn enter_shows_the_body_view_and_esc_returns_to_the_list() {
    let mut e = entry("1", "Fix flaky test", false);
    e.body = Some("Line one of the body\nLine two".into());
    let (mut app, _) = app_with_queue(vec![e]);
    ctrl_q(&mut app);
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let text = render(&mut app, 120, 30);
    for want in [
        "Fix flaky test",
        "Line one of the body",
        "Line two",
        "kind      workflow",
        "[p] pick",
    ] {
        assert!(text.contains(want), "missing {want}: {text}");
    }
    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(matches!(&app.active_dialog, Some(Dialog::QueuePanel(s)) if !s.showing_body));
    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.active_dialog.is_none());
}

#[test]
fn a_body_the_queue_does_not_carry_is_explained() {
    let (mut app, _) = app_with_queue(vec![entry("1", "T", false)]);
    ctrl_q(&mut app);
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(render(&mut app, 120, 30).contains("arrives with the claim"));
}

#[test]
fn arrows_move_the_selection() {
    let (mut app, _) = app_with_queue(vec![entry("1", "A", false), entry("2", "B", false)]);
    ctrl_q(&mut app);
    press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    let Some(Dialog::QueuePanel(state)) = &app.active_dialog else {
        panic!()
    };
    assert_eq!(state.selected, 1);
}

#[test]
fn ineligible_pick_refreshes_the_list_with_a_message() {
    let (mut app, source) = app_with_queue(vec![entry("1", "A", false)]);
    *source.pick_result.lock().unwrap() = Some(Err(PickError::Ineligible("labels changed".into())));
    ctrl_q(&mut app);
    let before = source.refreshes.load(std::sync::atomic::Ordering::SeqCst);
    // The server dropped the item.
    source.snapshot.lock().unwrap().items.clear();
    press_char(&mut app, 'p');
    assert!(source.refreshes.load(std::sync::atomic::Ordering::SeqCst) > before);
    let Some(Dialog::QueuePanel(state)) = &app.active_dialog else {
        panic!("panel closed")
    };
    assert!(state.rows.is_empty());
    assert!(state
        .message
        .as_deref()
        .unwrap()
        .contains("no longer eligible"));
    assert!(render(&mut app, 100, 24).contains("Item no longer eligible: labels changed"));
}

#[test]
fn a_successful_pick_opens_a_titled_tab_that_holds_the_lease() {
    let dir = tempfile::tempdir().unwrap();
    let (mut app, source) = app_with_queue(vec![entry("1", "Fix flaky test", false)]);
    *source.pick_result.lock().unwrap() = Some(Ok(PickRun {
        lease_id: "lease_1".into(),
        title: "Fix flaky test".into(),
        server: "acme".into(),
        cwd: dir.path().to_path_buf(),
        command_line: "exec workflow --dynamic".into(),
    }));
    ctrl_q(&mut app);
    press_char(&mut app, 'p');
    assert!(app.active_dialog.is_none());
    assert_eq!(app.tabs.len(), 2, "the pick opens a second tab");
    assert_eq!(app.active_tab, 1);
    let tab = app.active_tab();
    assert_eq!(
        tab.label_override.as_deref(),
        Some("Fix flaky test \u{00b7} acme")
    );
    assert_eq!(
        tab.lease.as_ref().map(|l| l.lease_id.as_str()),
        Some("lease_1")
    );
    assert!(
        source.finished.lock().unwrap().is_empty(),
        "a successful pick keeps the lease"
    );
}

fn running_lease_tab(app: &mut App) {
    app.push_tab(Tab::new(make_session()));
    app.active_tab = 1;
    let tab = app.active_tab_mut();
    tab.execution_phase = crate::frontend::tui::tabs::ExecutionPhase::Running {
        command: "exec workflow".into(),
    };
    tab.lease = Some(TabLease {
        lease_id: "lease_9".into(),
        server: "acme".into(),
        title: "Fix flaky test".into(),
    });
}

#[test]
fn closing_a_running_item_tab_asks_release_or_cancel() {
    let (mut app, _) = app_with_queue(vec![]);
    running_lease_tab(&mut app);
    press_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(matches!(
        app.active_dialog,
        Some(Dialog::LeaseCloseConfirm(_))
    ));
    let text = render(&mut app, 100, 30);
    for want in [
        "Close work item tab?",
        "Fix flaky test",
        "[r] Release it back to the queue",
        "[c] Cancel it",
        "Keep it running",
    ] {
        assert!(text.contains(want), "missing {want}: {text}");
    }
}

#[test]
fn releasing_from_the_close_prompt_releases_and_closes_the_tab() {
    let (mut app, source) = app_with_queue(vec![]);
    running_lease_tab(&mut app);
    press_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    press_char(&mut app, 'r');
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(
        source.finished.lock().unwrap().as_slice(),
        &[("lease_9".to_string(), LeaseCloseChoice::Release)]
    );
}

#[test]
fn cancelling_from_the_close_prompt_cancels_and_closes_the_tab() {
    let (mut app, source) = app_with_queue(vec![]);
    running_lease_tab(&mut app);
    press_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    press_char(&mut app, 'c');
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(
        source.finished.lock().unwrap()[0].1,
        LeaseCloseChoice::Cancel
    );
}

#[test]
fn esc_on_the_close_prompt_keeps_the_run() {
    let (mut app, source) = app_with_queue(vec![]);
    running_lease_tab(&mut app);
    press_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.active_dialog.is_none());
    assert_eq!(app.tabs.len(), 2);
    assert!(source.finished.lock().unwrap().is_empty());
}

#[test]
fn a_finished_item_tab_closes_without_the_prompt() {
    let (mut app, _) = app_with_queue(vec![]);
    running_lease_tab(&mut app);
    app.active_tab_mut().execution_phase = crate::frontend::tui::tabs::ExecutionPhase::Idle;
    press_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(matches!(app.active_dialog, Some(Dialog::CloseTabConfirm)));
}

#[test]
fn a_picked_tab_shows_its_label_in_the_tab_bar() {
    let (mut app, _) = app_with_queue(vec![]);
    running_lease_tab(&mut app);
    app.active_tab_mut().label_override = Some("Fix flaky test \u{00b7} acme".into());
    assert!(render(&mut app, 120, 30).contains("Fix flaky test"));
}

fn picked(app: &mut App, source: &FakeSource, dir: &std::path::Path) {
    *source.pick_result.lock().unwrap() = Some(Ok(PickRun {
        lease_id: "lease_1".into(),
        title: "Fix flaky test".into(),
        server: "acme".into(),
        cwd: dir.to_path_buf(),
        command_line: "exec workflow --dynamic".into(),
    }));
    ctrl_q(app);
    press_char(app, 'p');
}

#[test]
fn a_picked_tab_runs_the_parsed_exec_workflow_dynamic_command() {
    let dir = tempfile::tempdir().unwrap();
    // Keep the execution thread parked so this assertion observes admission,
    // independent of host runtimes and command completion timing. Execution
    // and publication are covered by worker_daemon's foreground tests.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let source = Arc::new(FakeSource::default());
    source.snapshot.lock().unwrap().items = vec![entry("1", "Fix flaky test", false)];
    let backend = crate::command::instance::local::LocalInstanceBackend::open(
        crate::command::instance::InstanceParts {
            catalogue: CommandCatalogue::get(),
            engines: Engines::for_tests(dir.path()),
            session_manager: Arc::new(SessionManager::in_memory(
                crate::data::config::env::EnvSnapshot::empty(),
            )),
            runtime: runtime.handle().clone(),
            env: Arc::new(crate::data::config::env::EnvSnapshot::empty),
            presentation: Arc::new(crate::command::instance::frontend::PlainPresentation),
        },
    );
    backend.instance().install_queue_for_test(source.clone());
    let mut app = App::with_backend(
        backend.clone(),
        Tab::new(make_session()),
        runtime.handle().clone(),
    );
    app.queue = Some(source.clone());
    picked(&mut app, &source, dir.path());
    assert_eq!(app.tabs.len(), 2);
    app.tick_all_tabs();
    use crate::command::instance::backend::InstanceBackend;
    assert_eq!(
        backend
            .session_snapshot(&app.active_tab().session_id)
            .unwrap()
            .state()
            .current_command
            .as_ref()
            .unwrap()
            .subcommand,
        "exec workflow"
    );
    // `spawn_command` ran: the tab left Idle for the picked command.
    let phase = &app.active_tab().execution_phase;
    assert!(
        !matches!(phase, crate::frontend::tui::tabs::ExecutionPhase::Idle),
        "the picked command was not started: {phase:?}"
    );
}

#[test]
fn a_command_line_that_does_not_parse_hands_the_lease_back_without_a_tab() {
    let dir = tempfile::tempdir().unwrap();
    let (mut app, source) = app_with_queue(vec![entry("1", "A", false)]);
    *source.pick_result.lock().unwrap() = Some(Ok(PickRun {
        lease_id: "lease_2".into(),
        title: "A".into(),
        server: "acme".into(),
        cwd: dir.path().to_path_buf(),
        command_line: "definitely-not-a-command".into(),
    }));
    ctrl_q(&mut app);
    press_char(&mut app, 'p');
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(
        source.finished.lock().unwrap().as_slice(),
        &[("lease_2".to_string(), LeaseCloseChoice::Release)]
    );
}

#[test]
fn r_asks_the_source_to_refresh_and_shows_the_new_rows() {
    let (mut app, source) = app_with_queue(vec![entry("1", "A", false)]);
    ctrl_q(&mut app);
    let before = source.refreshes.load(std::sync::atomic::Ordering::SeqCst);
    source
        .snapshot
        .lock()
        .unwrap()
        .items
        .push(entry("2", "Brand new", false));
    press_char(&mut app, 'r');
    assert_eq!(
        source.refreshes.load(std::sync::atomic::Ordering::SeqCst),
        before + 1
    );
    assert!(render(&mut app, 100, 24).contains("Brand new"));
}

#[test]
fn page_keys_jump_ten_rows_and_clamp() {
    let rows: Vec<_> = (0..25)
        .map(|i| entry(&i.to_string(), &format!("Item {i}"), false))
        .collect();
    let (mut app, _) = app_with_queue(rows);
    ctrl_q(&mut app);
    press_key(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
    let selected = |app: &App| match &app.active_dialog {
        Some(Dialog::QueuePanel(s)) => s.selected,
        _ => panic!("panel closed"),
    };
    assert_eq!(selected(&app), 10);
    press_key(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
    press_key(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
    assert_eq!(selected(&app), 24);
    press_key(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    assert_eq!(selected(&app), 14);
}

#[test]
fn p_from_the_body_view_picks_the_item() {
    let dir = tempfile::tempdir().unwrap();
    let (mut app, source) = app_with_queue(vec![entry("1", "Fix flaky test", false)]);
    *source.pick_result.lock().unwrap() = Some(Ok(PickRun {
        lease_id: "lease_3".into(),
        title: "Fix flaky test".into(),
        server: "acme".into(),
        cwd: dir.path().to_path_buf(),
        command_line: "exec workflow --dynamic".into(),
    }));
    ctrl_q(&mut app);
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press_char(&mut app, 'p');
    assert!(app.active_dialog.is_none());
    assert_eq!(
        app.active_tab().lease.as_ref().map(|l| l.lease_id.as_str()),
        Some("lease_3")
    );
}

#[test]
fn a_taken_item_refreshes_the_list_and_says_so() {
    let (mut app, source) = app_with_queue(vec![entry("1", "A", false)]);
    *source.pick_result.lock().unwrap() = Some(Err(PickError::Taken));
    ctrl_q(&mut app);
    let before = source.refreshes.load(std::sync::atomic::Ordering::SeqCst);
    press_char(&mut app, 'p');
    assert!(source.refreshes.load(std::sync::atomic::Ordering::SeqCst) > before);
    assert!(render(&mut app, 100, 24).contains("Item was claimed by another worker."));
    assert_eq!(app.tabs.len(), 1);
}

#[test]
fn a_failed_pick_shows_the_reason_and_opens_no_tab() {
    let (mut app, source) = app_with_queue(vec![entry("1", "A", false)]);
    *source.pick_result.lock().unwrap() = Some(Err(PickError::Failed("server offline".into())));
    ctrl_q(&mut app);
    press_char(&mut app, 'p');
    assert!(render(&mut app, 100, 24).contains("Pick failed: server offline"));
    assert_eq!(app.tabs.len(), 1);
}

#[test]
fn a_failed_release_keeps_the_tab_and_reports_the_error() {
    let (mut app, source) = app_with_queue(vec![]);
    *source.finish_error.lock().unwrap() = Some("server unreachable".into());
    running_lease_tab(&mut app);
    press_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    press_char(&mut app, 'r');
    assert_eq!(app.tabs.len(), 2, "closing would orphan the lease");
    assert!(
        app.status_bar.text.contains("server unreachable"),
        "{}",
        app.status_bar.text
    );
}

#[test]
fn a_new_snapshot_keeps_the_selection_on_the_same_item() {
    use crate::frontend::tui::queue::QueuePanelState;
    let mut state = QueuePanelState::from_snapshot(QueueSnapshot {
        items: vec![entry("1", "A", false), entry("2", "B", false)],
        loaded: true,
        ..QueueSnapshot::default()
    });
    state.move_by(1);
    // A new item sorts in front of the selected one.
    state.apply(QueueSnapshot {
        items: vec![
            entry("0", "New", false),
            entry("1", "A", false),
            entry("2", "B", false),
        ],
        loaded: true,
        ..QueueSnapshot::default()
    });
    assert_eq!(state.selected_entry().unwrap().id, "2");
    // The selected item vanishing falls back to a valid row.
    state.apply(QueueSnapshot {
        items: vec![entry("0", "New", false)],
        loaded: true,
        ..QueueSnapshot::default()
    });
    assert_eq!(state.selected, 0);
    state.apply(QueueSnapshot::default());
    assert!(state.selected_entry().is_none() && !state.showing_body);
}

#[test]
fn a_tick_applies_a_new_snapshot_to_the_open_panel() {
    let (mut app, source) = app_with_queue(vec![entry("1", "A", false)]);
    ctrl_q(&mut app);
    source
        .snapshot
        .lock()
        .unwrap()
        .items
        .push(entry("2", "Arrived while open", false));
    source
        .generation
        .store(7, std::sync::atomic::Ordering::SeqCst);
    app.tick_all_tabs();
    assert!(render(&mut app, 100, 24).contains("Arrived while open"));
}

#[test]
fn helpers_format_age_labels_badge_and_tab_label() {
    use crate::frontend::tui::queue::{format_age, format_labels, tab_label};
    assert_eq!(format_age(0), "0s");
    assert_eq!(format_age(59_999), "59s");
    assert_eq!(format_age(60_000), "1m");
    assert_eq!(format_age(3_600_000), "1h");
    assert_eq!(format_age(172_800_000), "2d");
    assert_eq!(format_labels(&BTreeMap::new()), "-");
    assert_eq!(tab_label("T", "s"), "T \u{00b7} s");
    assert_eq!(QueueBadge::default().text(), None);
    assert_eq!(
        QueueBadge { for_you: 0, new: 1 }.text().as_deref(),
        Some("1 new work item")
    );
    assert_eq!(
        QueueBadge { for_you: 2, new: 3 }.text().as_deref(),
        Some("3 new work items \u{00b7} 2 for you")
    );
}

// --- picker snapshots: empty, populated, filtered-by-owner, "for you" ---

/// The panel's table lines (trimmed of the frame border) for exact comparison.
fn panel_lines(app: &mut App) -> Vec<String> {
    render(app, 120, 30)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

#[test]
fn snapshot_empty_picker() {
    let (mut app, _) = app_with_queue(vec![]);
    ctrl_q(&mut app);
    let lines = panel_lines(&mut app);
    assert!(lines.iter().any(|l| l.contains("Work queue")), "{lines:?}");
    assert!(
        lines.iter().any(|l| l.contains("No eligible work items.")),
        "{lines:?}"
    );
    assert!(!lines.iter().any(|l| l.contains("for you")), "{lines:?}");
}

#[test]
fn snapshot_populated_picker_lists_one_line_per_item_in_order() {
    let (mut app, _) = app_with_queue(vec![
        entry("1", "Fix flaky test", false),
        entry("2", "Bump deps", false),
    ]);
    ctrl_q(&mut app);
    let lines = panel_lines(&mut app);
    let first = lines
        .iter()
        .position(|l| l.contains("Fix flaky test"))
        .unwrap();
    let second = lines.iter().position(|l| l.contains("Bump deps")).unwrap();
    assert_eq!(second, first + 1, "{lines:?}");
    for line in [&lines[first], &lines[second]] {
        assert!(
            line.contains("acme")
                && line.contains("team=core")
                && line.contains('5')
                && line.contains("2m"),
            "{line}"
        );
    }
}

#[test]
fn snapshot_for_you_marker_sits_on_the_owned_row_only() {
    let (mut app, _) = app_with_queue(vec![entry("1", "Mine", true), entry("2", "Theirs", false)]);
    ctrl_q(&mut app);
    let lines = panel_lines(&mut app);
    let mine = lines.iter().find(|l| l.contains("Mine")).unwrap();
    let theirs = lines.iter().find(|l| l.contains("Theirs")).unwrap();
    assert!(mine.contains("for you"), "{mine}");
    assert!(!theirs.contains("for you"), "{theirs}");
}

#[test]
fn snapshot_of_a_queue_that_failed_to_load_shows_the_error_not_an_empty_list() {
    let (mut app, source) = app_with_queue(vec![]);
    {
        let mut snap = source.snapshot.lock().unwrap();
        snap.error = Some("work server unreachable".into());
    }
    ctrl_q(&mut app);
    assert!(render(&mut app, 100, 24).contains("work server unreachable"));
}
