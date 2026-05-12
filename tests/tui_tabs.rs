/// Integration tests for multi-tab TUI support.
///
/// Verifies that the `App` multi-tab manager correctly creates, switches,
/// closes, and isolates state between `TabState` instances.
use amux::tui::state::{App, ContainerWindowState, ExecutionPhase, STUCK_TIMEOUT};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// 1. App starts with exactly one tab
// ---------------------------------------------------------------------------

#[test]
fn app_starts_with_one_tab() {
    let app = App::new(std::path::PathBuf::from("/tmp/proj"));
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.active_tab_idx, 0);
    assert!(!app.should_quit);
}

// ---------------------------------------------------------------------------
// 2. create_tab adds a new tab and returns its index
// ---------------------------------------------------------------------------

#[test]
fn create_tab_adds_tab() {
    let mut app = App::new(std::path::PathBuf::from("/tmp/a"));
    let idx = app.create_tab(std::path::PathBuf::from("/tmp/b"));
    assert_eq!(idx, 1);
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(app.tabs[1].cwd, std::path::PathBuf::from("/tmp/b"));
}

// ---------------------------------------------------------------------------
// 3. close_tab removes a tab and adjusts active index
// ---------------------------------------------------------------------------

#[test]
fn close_tab_removes_tab() {
    let mut app = App::new(std::path::PathBuf::from("/tmp/a"));
    app.create_tab(std::path::PathBuf::from("/tmp/b"));
    assert_eq!(app.tabs.len(), 2);
    app.active_tab_idx = 1;
    app.close_tab(1);
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.active_tab_idx, 0);
}

#[test]
fn close_last_tab_sets_should_quit() {
    let mut app = App::new(std::path::PathBuf::from("/tmp/a"));
    assert_eq!(app.tabs.len(), 1);
    app.close_tab(0);
    assert!(app.should_quit);
}

// ---------------------------------------------------------------------------
// 4. Tabs have independent state
// ---------------------------------------------------------------------------

#[test]
fn tabs_have_independent_output() {
    let mut app = App::new(std::path::PathBuf::from("/tmp/a"));
    app.create_tab(std::path::PathBuf::from("/tmp/b"));

    app.active_tab_idx = 0;
    app.active_tab_mut().push_output("line from tab 0".to_string());

    app.active_tab_idx = 1;
    app.active_tab_mut().push_output("line from tab 1".to_string());

    app.active_tab_idx = 0;
    assert!(app.active_tab().output_lines.iter().any(|l| l == "line from tab 0"));
    assert!(!app.active_tab().output_lines.iter().any(|l| l == "line from tab 1"));

    app.active_tab_idx = 1;
    assert!(app.active_tab().output_lines.iter().any(|l| l == "line from tab 1"));
    assert!(!app.active_tab().output_lines.iter().any(|l| l == "line from tab 0"));
}

#[test]
fn tabs_have_independent_phase() {
    let mut app = App::new(std::path::PathBuf::from("/tmp/a"));
    app.create_tab(std::path::PathBuf::from("/tmp/b"));

    app.active_tab_idx = 0;
    app.active_tab_mut().phase = ExecutionPhase::Running { command: "ready".into() };

    app.active_tab_idx = 1;
    assert!(matches!(app.active_tab().phase, ExecutionPhase::Idle));
}

#[test]
fn tabs_have_independent_input() {
    let mut app = App::new(std::path::PathBuf::from("/tmp/a"));
    app.create_tab(std::path::PathBuf::from("/tmp/b"));

    app.active_tab_idx = 0;
    app.active_tab_mut().input = "implement 0001".to_string();

    app.active_tab_idx = 1;
    assert_eq!(app.active_tab().input, "");
}

// ---------------------------------------------------------------------------
// 5. tab_color reflects phase
// ---------------------------------------------------------------------------

#[test]
fn tab_color_idle_is_dark_gray() {
    use ratatui::style::Color;
    let tab = amux::tui::state::TabState::new(std::path::PathBuf::from("/tmp/proj"));
    assert_eq!(tab.tab_color(true, STUCK_TIMEOUT), Color::DarkGray);
}

#[test]
fn tab_color_running_no_container_is_blue() {
    use ratatui::style::Color;
    let mut tab = amux::tui::state::TabState::new(std::path::PathBuf::from("/tmp/proj"));
    tab.phase = ExecutionPhase::Running { command: "ready".into() };
    assert_eq!(tab.tab_color(true, STUCK_TIMEOUT), Color::Blue);
}

#[test]
fn tab_color_running_with_container_is_green() {
    use ratatui::style::Color;
    let mut tab = amux::tui::state::TabState::new(std::path::PathBuf::from("/tmp/proj"));
    tab.phase = ExecutionPhase::Running { command: "exec workflow deploy.toml".into() };
    tab.container_window = ContainerWindowState::Maximized;
    assert_eq!(tab.tab_color(true, STUCK_TIMEOUT), Color::Green);
}

#[test]
fn tab_color_error_is_red() {
    use ratatui::style::Color;
    let mut tab = amux::tui::state::TabState::new(std::path::PathBuf::from("/tmp/proj"));
    tab.phase = ExecutionPhase::Error { command: "ready".into(), exit_code: 1 };
    assert_eq!(tab.tab_color(true, STUCK_TIMEOUT), Color::Red);
}


// ---------------------------------------------------------------------------
// 6. tab_display_name format and new split methods
// ---------------------------------------------------------------------------

#[test]
fn tab_display_name_idle_shows_project() {
    let tab = amux::tui::state::TabState::new(std::path::PathBuf::from("/home/user/myproject"));
    assert_eq!(tab.tab_display_name(), "myproject");
}

#[test]
fn tab_display_name_running_shows_first_word_of_command() {
    let mut tab = amux::tui::state::TabState::new(std::path::PathBuf::from("/home/user/proj"));
    tab.phase = ExecutionPhase::Running { command: "chat".into() };
    // "proj | chat" = 11 chars, fits within 14-char limit
    assert_eq!(tab.tab_display_name(), "proj | chat");
}

#[test]
fn tab_display_name_truncates_long_names() {
    let tab = amux::tui::state::TabState::new(
        std::path::PathBuf::from("/home/user/a-very-long-project-name")
    );
    let name = tab.tab_display_name();
    assert!(
        name.chars().count() <= 14,
        "Display name exceeds 14 chars: '{}' ({})",
        name,
        name.chars().count()
    );
}

#[test]
fn tab_project_name_idle() {
    let tab = amux::tui::state::TabState::new(std::path::PathBuf::from("/home/user/myproject"));
    assert_eq!(tab.tab_project_name(), "myproject");
}

#[test]
fn tab_subcommand_label_idle_is_empty() {
    let tab = amux::tui::state::TabState::new(std::path::PathBuf::from("/home/user/myproject"));
    assert_eq!(tab.tab_subcommand_label(20, true, STUCK_TIMEOUT), "");
}

#[test]
fn tab_subcommand_label_running_full_command() {
    let mut tab = amux::tui::state::TabState::new(std::path::PathBuf::from("/home/user/proj"));
    tab.phase = ExecutionPhase::Running { command: "exec prompt".into() };
    assert_eq!(tab.tab_subcommand_label(20, true, STUCK_TIMEOUT), "exec prompt");
}

#[test]
fn tab_subcommand_label_truncates_long_command() {
    let mut tab = amux::tui::state::TabState::new(std::path::PathBuf::from("/home/user/proj"));
    tab.phase = ExecutionPhase::Running { command: "exec workflow --some-very-long-flag".into() };
    // tab_width=20, max_chars=16; command is 33 chars so it must be truncated
    let label = tab.tab_subcommand_label(20, true, STUCK_TIMEOUT);
    assert!(label.ends_with('…'), "expected truncation ellipsis, got: {}", label);
    assert!(label.chars().count() <= 16);
}

// ---------------------------------------------------------------------------
// 6b. tab_color is yellow when stuck; reverts to normal after acknowledge
// ---------------------------------------------------------------------------

#[test]
fn tab_color_stuck_container_is_yellow() {
    use ratatui::style::Color;
    let mut tab = amux::tui::state::TabState::new(std::path::PathBuf::from("/tmp/proj"));
    tab.phase = ExecutionPhase::Running { command: "exec workflow deploy.toml".into() };
    tab.container_window = ContainerWindowState::Maximized;
    // Simulate 61 seconds of silence.
    tab.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));
    assert_eq!(tab.tab_color(true, STUCK_TIMEOUT), Color::Yellow);
}

#[test]
fn tab_color_reverts_to_green_after_acknowledge() {
    use ratatui::style::Color;
    let mut tab = amux::tui::state::TabState::new(std::path::PathBuf::from("/tmp/proj"));
    tab.phase = ExecutionPhase::Running { command: "exec workflow deploy.toml".into() };
    tab.container_window = ContainerWindowState::Maximized;
    tab.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));
    assert_eq!(tab.tab_color(true, STUCK_TIMEOUT), Color::Yellow);

    tab.acknowledge_stuck();
    // Container is still running → should revert to green (not yellow).
    assert_eq!(tab.tab_color(true, STUCK_TIMEOUT), Color::Green);
}

#[test]
fn tab_subcommand_label_shows_warning_when_stuck() {
    let mut tab = amux::tui::state::TabState::new(std::path::PathBuf::from("/tmp/proj"));
    tab.phase = ExecutionPhase::Running { command: "exec workflow deploy.toml".into() };
    tab.container_window = ContainerWindowState::Maximized;
    tab.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));

    let label = tab.tab_subcommand_label(30, true, STUCK_TIMEOUT);
    assert!(
        label.contains('⚠'),
        "expected warning symbol in stuck label, got: {:?}",
        label
    );
}

#[test]
fn tab_subcommand_label_clears_warning_after_acknowledge() {
    let mut tab = amux::tui::state::TabState::new(std::path::PathBuf::from("/tmp/proj"));
    tab.phase = ExecutionPhase::Running { command: "exec workflow deploy.toml".into() };
    tab.container_window = ContainerWindowState::Maximized;
    tab.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));

    // Stuck warning is visible.
    assert!(tab.tab_subcommand_label(30, true, STUCK_TIMEOUT).contains('⚠'));

    // User acknowledges (switches to tab / provides input).
    tab.acknowledge_stuck();
    assert!(!tab.tab_subcommand_label(30, true, STUCK_TIMEOUT).contains('⚠'));
}

#[test]
fn stuck_state_is_independent_per_tab() {
    use ratatui::style::Color;
    let mut app = App::new(std::path::PathBuf::from("/tmp/a"));
    app.create_tab(std::path::PathBuf::from("/tmp/b"));

    // Tab 0: make it stuck.
    app.tabs[0].phase = ExecutionPhase::Running { command: "implement 0001".into() };
    app.tabs[0].container_window = ContainerWindowState::Maximized;
    app.tabs[0].last_output_time =
        Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));

    // Tab 1: running with fresh output.
    app.tabs[1].phase = ExecutionPhase::Running { command: "implement 0002".into() };
    app.tabs[1].container_window = ContainerWindowState::Maximized;
    app.tabs[1].last_output_time = Some(Instant::now());

    assert_eq!(app.tabs[0].tab_color(true, STUCK_TIMEOUT), Color::Yellow, "tab 0 should be stuck (yellow)");
    assert_eq!(app.tabs[1].tab_color(false, STUCK_TIMEOUT), Color::Green, "tab 1 should not be stuck (green)");
}

#[test]
fn acknowledging_one_tab_does_not_affect_other() {
    use ratatui::style::Color;
    let mut app = App::new(std::path::PathBuf::from("/tmp/a"));
    app.create_tab(std::path::PathBuf::from("/tmp/b"));

    let old_time = Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1));
    app.tabs[0].phase = ExecutionPhase::Running { command: "implement 0001".into() };
    app.tabs[0].container_window = ContainerWindowState::Maximized;
    app.tabs[0].last_output_time = Some(old_time);

    app.tabs[1].phase = ExecutionPhase::Running { command: "implement 0002".into() };
    app.tabs[1].container_window = ContainerWindowState::Maximized;
    app.tabs[1].last_output_time = Some(old_time);

    // Acknowledge only tab 1.
    app.tabs[1].acknowledge_stuck();

    // Tab 0 is still stuck; tab 1 is no longer stuck.
    assert_eq!(app.tabs[0].tab_color(true, STUCK_TIMEOUT), Color::Yellow);
    assert_eq!(app.tabs[1].tab_color(false, STUCK_TIMEOUT), Color::Green);
}

// ---------------------------------------------------------------------------
// 7. tick_all drives all tabs
// ---------------------------------------------------------------------------

#[test]
fn tick_all_drains_all_tab_channels() {
    let mut app = App::new(std::path::PathBuf::from("/tmp/a"));
    app.create_tab(std::path::PathBuf::from("/tmp/b"));

    // Send output to tab 0's channel.
    let _ = app.tabs[0].output_tx.send("tick0".to_string());
    // Send output to tab 1's channel.
    let _ = app.tabs[1].output_tx.send("tick1".to_string());

    app.tick_all();

    assert!(app.tabs[0].output_lines.iter().any(|l| l == "tick0"));
    assert!(app.tabs[1].output_lines.iter().any(|l| l == "tick1"));
}
