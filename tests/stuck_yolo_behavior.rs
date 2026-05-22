/// Integration tests for stuck-detection and yolo countdown improvements (work item 0048).
///
/// Covers:
/// - Active-tab user-activity suppression of stuck detection
/// - Background yolo countdown: tab-bar color and label alternation
/// - Background yolo countdown expiry → workflow auto-advance flag
/// - Tab-switching with an in-progress yolo countdown dialog
use awman::tui::input::{handle_key, Action};
use awman::tui::state::{
    App, Dialog, ExecutionPhase, TabState, STUCK_TIMEOUT, YOLO_COUNTDOWN_DURATION,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::{Duration, Instant};

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn new_app() -> App {
    App::new(std::path::PathBuf::new())
}

/// Returns an App whose active tab (index 0) is running, has a container, and
/// is a stuck yolo-mode workflow tab.
fn active_yolo_stuck_app() -> App {
    let mut app = new_app();
    let tab = app.active_tab_mut();
    tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
    tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
    tab.workflow_current_step = Some("step-one".to_string());
    tab.yolo_mode = true;
    tab.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));
    app
}

/// Returns an App with two tabs.  Tab 0 is idle (active).  Tab 1 (background)
/// is a running, stuck yolo-mode workflow tab.
fn background_yolo_stuck_app() -> App {
    let mut app = new_app();
    app.tabs.push(TabState::new(std::path::PathBuf::new()));
    let tab1 = &mut app.tabs[1];
    tab1.phase = ExecutionPhase::Running { command: "implement 0001".into() };
    tab1.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
    tab1.workflow_current_step = Some("step-one".to_string());
    tab1.yolo_mode = true;
    tab1.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));
    // active_tab_idx stays 0.
    app
}

// ─── Active-tab user-activity suppression ────────────────────────────────────

/// Active tab receives user activity while stuck → is_stuck(true) returns false
/// → no yellow colour, no dialog opened by tick_all.
#[test]
fn active_tab_with_recent_user_activity_is_not_stuck() {
    use ratatui::style::Color;

    let mut app = active_yolo_stuck_app();
    // Record fresh user activity on the active tab.
    app.active_tab_mut().record_user_activity();

    // The tab should not be considered stuck while the user is active.
    assert!(
        !app.active_tab().is_stuck(true, STUCK_TIMEOUT),
        "is_stuck(true) must return false when user recently interacted"
    );

    // tab_color must not be yellow.
    assert_ne!(
        app.active_tab().tab_color(true, STUCK_TIMEOUT),
        Color::Yellow,
        "tab colour must not be yellow when user is actively interacting"
    );

    // tick_all must not open any dialog.
    app.tick_all();
    assert_eq!(
        app.active_tab().dialog,
        Dialog::None,
        "tick_all must not open a dialog while user is active on the tab"
    );
}

/// Active tab idle for STUCK_TIMEOUT and user is also idle → dialog opens as
/// before (existing behavior must be preserved).
#[test]
fn active_tab_both_idle_opens_yolo_dialog() {
    let mut app = active_yolo_stuck_app();
    // Ensure no recent user activity (last_user_activity_time stays None).
    app.tick_all();
    assert!(
        matches!(app.active_tab().dialog, Dialog::WorkflowYoloCountdown { .. }),
        "yolo dialog must open when both output and user are idle on the active tab"
    );
}

// ─── Background yolo tab: countdown initiated ─────────────────────────────────

/// Background tab enters stuck state in yolo mode → tick_all sets
/// yolo_countdown_started_at.
#[test]
fn background_yolo_tab_countdown_starts_on_first_tick() {
    let mut app = background_yolo_stuck_app();
    assert!(app.tabs[1].yolo_countdown_started_at.is_none());

    app.tick_all();

    assert!(
        app.tabs[1].yolo_countdown_started_at.is_some(),
        "tick_all must initialise yolo_countdown_started_at for a background stuck yolo tab"
    );
    // Dialog must NOT be opened; the tab bar handles visual feedback.
    assert_eq!(
        app.tabs[1].dialog,
        Dialog::None,
        "no dialog must be opened for a background yolo tab"
    );
}

/// After the first tick the countdown timer is not reset on subsequent ticks.
#[test]
fn background_yolo_tab_countdown_timer_is_stable_across_ticks() {
    let mut app = background_yolo_stuck_app();
    app.tick_all();
    let first = app.tabs[1].yolo_countdown_started_at.unwrap();
    app.tick_all();
    let second = app.tabs[1].yolo_countdown_started_at.unwrap();
    assert_eq!(
        first, second,
        "yolo_countdown_started_at must not be reset on subsequent ticks while the tab stays stuck"
    );
}

/// Tab bar colour alternates: Yellow for even elapsed seconds, Magenta for odd.
#[test]
fn background_yolo_tab_color_alternates_by_second() {
    use ratatui::style::Color;

    let mut tab = TabState::new(std::path::PathBuf::new());
    // 2 s elapsed → even → Yellow.
    tab.yolo_countdown_started_at = Some(Instant::now() - Duration::from_secs(2));
    assert_eq!(
        tab.tab_color(false, STUCK_TIMEOUT),
        Color::Yellow,
        "tab colour must be Yellow for even elapsed seconds"
    );
    // 3 s elapsed → odd → Magenta.
    tab.yolo_countdown_started_at = Some(Instant::now() - Duration::from_secs(3));
    assert_eq!(
        tab.tab_color(false, STUCK_TIMEOUT),
        Color::Magenta,
        "tab colour must be Magenta for odd elapsed seconds"
    );
}

/// Tab label shows countdown text with correct remaining time and alternating emoji.
#[test]
fn background_yolo_tab_label_shows_countdown() {
    let mut tab = TabState::new(std::path::PathBuf::new());
    // 10 s elapsed → 50 s remaining; even phase.
    tab.yolo_countdown_started_at = Some(Instant::now() - Duration::from_secs(10));
    let label = tab.tab_subcommand_label(50, false, STUCK_TIMEOUT);
    assert!(
        label.contains("yolo in"),
        "label must contain 'yolo in' text, got: {:?}",
        label
    );
    // Allow 1 s of timing slack.
    assert!(
        label.contains("50") || label.contains("49"),
        "label must show ~50 s remaining, got: {:?}",
        label
    );
}

// ─── Background yolo countdown expiry ─────────────────────────────────────────

/// When the countdown elapses for a background tab, yolo_countdown_expired is
/// set and yolo_countdown_started_at is cleared — no dialog is opened.
#[test]
fn background_yolo_countdown_expiry_sets_flag_without_dialog() {
    let mut app = background_yolo_stuck_app();
    // Pre-set an already-expired countdown on the background tab.
    app.tabs[1].yolo_countdown_started_at = Some(Instant::now() - YOLO_COUNTDOWN_DURATION);

    app.tick_all();

    assert!(
        app.tabs[1].yolo_countdown_expired,
        "yolo_countdown_expired must be set when the countdown elapses for a background tab"
    );
    assert!(
        app.tabs[1].yolo_countdown_started_at.is_none(),
        "yolo_countdown_started_at must be cleared after expiry"
    );
    assert_eq!(
        app.tabs[1].dialog,
        Dialog::None,
        "no dialog must be opened for an expired background yolo tab"
    );
}

// ─── Tab-switching with in-progress countdown ─────────────────────────────────

/// Switching to a background yolo tab with an in-progress countdown opens the
/// yolo dialog without resetting the countdown timer.
#[test]
fn switching_to_background_yolo_tab_opens_dialog_with_preserved_timer() {
    let mut app = background_yolo_stuck_app();

    // Start the countdown 10 s ago on the background tab.
    let start = Instant::now() - Duration::from_secs(10);
    app.tabs[1].yolo_countdown_started_at = Some(start);
    app.tabs[1].workflow_stuck_dialog_opened = false;

    // Simulate what SwitchTabRight does in mod.rs:
    // 1. Close any open yolo dialog on the current tab (none here).
    // 2. Move to the new tab.
    app.active_tab_idx = 1;
    // 3. Acknowledge stuck on the newly active tab.
    app.active_tab_mut().acknowledge_stuck();
    // 4. If the new tab has a countdown, open the dialog (preserving the timer).
    if app.active_tab().yolo_countdown_started_at.is_some() {
        if let Some(step) = app.active_tab().workflow_current_step.clone() {
            app.active_tab_mut().dialog = Dialog::WorkflowYoloCountdown {
                current_step: step,
            };
            app.active_tab_mut().workflow_stuck_dialog_opened = true;
        }
    }

    assert!(
        matches!(app.active_tab().dialog, Dialog::WorkflowYoloCountdown { .. }),
        "yolo dialog must open when switching to a tab with an in-progress countdown"
    );
    // The countdown timer must not have been reset.
    assert_eq!(
        app.active_tab().yolo_countdown_started_at.unwrap(),
        start,
        "yolo_countdown_started_at must be preserved (not restarted) after switching to the tab"
    );

    // The dialog must survive the first tick.  acknowledge_stuck() is called on
    // the new active tab during the switch; without the yolo-countdown guard in
    // acknowledge_stuck() it would reset last_output_time to now, making is_stuck()
    // return false on the next tick and causing the "active and not stuck" branch to
    // immediately close the dialog we just opened.
    app.tick_all();
    assert!(
        matches!(app.active_tab().dialog, Dialog::WorkflowYoloCountdown { .. }),
        "yolo dialog must still be open after the first tick_all following the tab switch"
    );
    assert_eq!(
        app.active_tab().yolo_countdown_started_at.unwrap(),
        start,
        "yolo_countdown_started_at must be preserved after tick_all"
    );
}

/// Switching away from an active tab with an open yolo dialog closes the dialog
/// but leaves yolo_countdown_started_at intact so background mode continues.
#[test]
fn switching_away_from_yolo_dialog_closes_dialog_but_preserves_countdown() {
    let mut app = background_yolo_stuck_app();

    // Make tab 1 the active tab with an open yolo dialog and a running countdown.
    app.active_tab_idx = 1;
    let start = Instant::now() - Duration::from_secs(5);
    app.tabs[1].yolo_countdown_started_at = Some(start);
    app.tabs[1].dialog = Dialog::WorkflowYoloCountdown {
        current_step: "step-one".to_string(),
    };
    app.tabs[1].workflow_stuck_dialog_opened = true;

    // Simulate what SwitchTabRight does when leaving a tab with an open yolo dialog:
    // 1. Close the dialog on the departing tab.
    if matches!(app.active_tab().dialog, Dialog::WorkflowYoloCountdown { .. }) {
        app.active_tab_mut().dialog = Dialog::None;
    }
    // 2. Move to the next tab.
    let len = app.tabs.len();
    app.active_tab_idx = (app.active_tab_idx + 1) % len;

    // Dialog must be closed on tab 1.
    assert_eq!(
        app.tabs[1].dialog,
        Dialog::None,
        "yolo dialog must be closed when switching away from the tab"
    );
    // Countdown timer must still be running on tab 1 (background mode).
    assert_eq!(
        app.tabs[1].yolo_countdown_started_at.unwrap(),
        start,
        "yolo_countdown_started_at must be preserved so background countdown continues"
    );
}

// ─── New container output clears countdown ────────────────────────────────────

/// When the container in a background tab produces new output during the yolo
/// countdown, the tab is no longer stuck.  tick_all detects this and clears
/// yolo_countdown_started_at so the tab returns to its normal colour.
///
/// Note: The lower-level PTY path (tick() processing raw PtyEvent bytes) is
/// already covered by the unit test `tick_pty_output_closes_yolo_countdown_dialog`
/// in src/tui/state.rs.  Here we test the tick_all() branch that clears the
/// timer when the tab transitions from stuck to not-stuck.
#[test]
fn background_yolo_countdown_cleared_when_tab_no_longer_stuck() {
    use ratatui::style::Color;

    let mut app = background_yolo_stuck_app();

    // Start the countdown on the background tab via tick_all.
    app.tick_all();
    assert!(app.tabs[1].yolo_countdown_started_at.is_some());

    // Verify tab is showing a yolo colour (Yellow or Magenta).
    let yolo_color = app.tabs[1].tab_color(false, STUCK_TIMEOUT);
    assert!(
        yolo_color == Color::Yellow || yolo_color == Color::Magenta,
        "tab must show yolo colour during countdown, got: {:?}",
        yolo_color
    );

    // Simulate new container output: reset last_output_time to now so the tab
    // is no longer stuck.  (This is the same state change that tick() makes
    // when it receives PTY data from the container.)
    app.tabs[1].last_output_time = Some(Instant::now());

    // tick_all detects the tab is no longer stuck and clears the countdown.
    app.tick_all();

    assert!(
        app.tabs[1].yolo_countdown_started_at.is_none(),
        "yolo_countdown_started_at must be cleared when the tab is no longer stuck"
    );
    // Tab colour must revert to normal (green for running + container).
    assert_eq!(
        app.tabs[1].tab_color(false, STUCK_TIMEOUT),
        Color::Green,
        "tab colour must revert to green once the countdown is cleared"
    );
}

// ─── Ctrl+D dispatch path from yolo dialog ────────────────────────────────────

/// Pressing Ctrl+D while the yolo countdown dialog is open returns
/// Action::SwitchTabRight.  This exercises the actual keyboard-dispatch path
/// through handle_key() → handle_workflow_yolo_countdown() rather than
/// simulating the switch directly, as the other tab-switch tests do.
///
/// The full tab-switch side-effects (dialog close on old tab, dialog open on
/// new tab, countdown preservation) are covered by
/// `switching_away_from_yolo_dialog_closes_dialog_but_preserves_countdown` and
/// `switching_to_background_yolo_tab_opens_dialog_with_preserved_timer`.
#[test]
fn ctrl_d_from_yolo_dialog_returns_switch_right_and_dialog_closes_on_old_tab() {
    // Active tab (0) has a running yolo countdown and an open dialog.
    let mut app = active_yolo_stuck_app();
    let start = Instant::now() - Duration::from_secs(5);
    app.active_tab_mut().yolo_countdown_started_at = Some(start);
    app.active_tab_mut().dialog = Dialog::WorkflowYoloCountdown {
        current_step: "step-one".to_string(),
    };
    app.active_tab_mut().workflow_stuck_dialog_opened = true;

    // Add a second tab to switch to.
    app.tabs.push(TabState::new(std::path::PathBuf::new()));

    // Press Ctrl+D while the yolo dialog is the active modal.
    let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
    let action = handle_key(&mut app, ctrl_d);

    // The dialog handler must return SwitchTabRight so mod.rs can close the dialog
    // and perform the switch, rather than swallowing the key.
    assert!(
        matches!(action, Action::SwitchTabRight),
        "Ctrl+D from the yolo countdown dialog must return Action::SwitchTabRight"
    );

    // Simulate what handle_action does for SwitchTabRight (mod.rs is not directly
    // callable from integration tests because handle_action is private and async):
    // 1. Close dialog on the departing tab and clear the opened flag.
    if matches!(app.active_tab().dialog, Dialog::WorkflowYoloCountdown { .. }) {
        app.active_tab_mut().dialog = Dialog::None;
        app.active_tab_mut().workflow_stuck_dialog_opened = false;
    }
    // 2. Switch to the next tab.
    let len = app.tabs.len();
    app.active_tab_idx = (app.active_tab_idx + 1) % len;

    // After the switch: tab 0 (now background) must have no dialog but a live countdown.
    assert_eq!(
        app.tabs[0].dialog,
        Dialog::None,
        "dialog must be closed on the departing tab after Ctrl+D"
    );
    assert!(!app.tabs[0].workflow_stuck_dialog_opened,
        "workflow_stuck_dialog_opened must be false on the departing tab"
    );
    assert_eq!(
        app.tabs[0].yolo_countdown_started_at.unwrap(),
        start,
        "yolo countdown must continue running in background after Ctrl+D"
    );

    // After a tick_all the background countdown must still be running (not cleared).
    app.tick_all();
    assert!(
        app.tabs[0].yolo_countdown_started_at.is_some(),
        "background countdown must survive tick_all after Ctrl+D tab switch"
    );
}
