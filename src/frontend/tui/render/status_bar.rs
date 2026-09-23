//! Status hint bar rendering (the 1-row bar above the command box).

use super::*;

pub(super) fn render_status_bar(app: &App, area: Rect, frame: &mut Frame, sidebar_visible: bool) {
    use crate::frontend::tui::tabs::{ContainerWindowState, ExecutionPhase};

    let tab = app.active_tab();
    let workflow_active = tab
        .shared
        .workflow_state
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false);

    // A squad tab renders the card grid in place of the execution window, so
    // the execution-window hints below ("Exit code", "press ↑ to focus the
    // window") describe something that is not on screen. What is worth saying
    // there instead is the last squad action that failed: every squad key
    // binding dispatches through `spawn_command`, whose failure lands in the
    // tab's `ExecutionPhase` and in a status log the grid never renders. Put
    // it here and a key that "did nothing" says why.
    //
    // Once an attach session owns the tab's slots the container view is on
    // screen instead of the grid (see `render.rs`'s matching
    // `container_slots.is_empty()` check), so fall through to the ordinary
    // hints below — including `ctrl-\ detach` — instead of returning early.
    if tab.is_squad && tab.container_slots.is_empty() {
        let spans = match &tab.execution_phase {
            ExecutionPhase::Error { command, message } => {
                vec![Span::styled(
                    format!(" {} ", squad_failure_text(command, message)),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )]
            }
            _ => vec![Span::styled(
                " \u{00b7} ctrl-g git ",
                Style::default().fg(Color::DarkGray),
            )],
        };
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black)),
            area,
        );
        return;
    }

    let mut spans: Vec<Span> = match (&tab.execution_phase, app.focus, tab.container_window_state) {
        // Running + ExecWindow + Maximized container
        (
            ExecutionPhase::Running { .. },
            Focus::ExecutionWindow,
            ContainerWindowState::Maximized,
        ) => {
            // WI 0110: `ctrl-\ detach` is advertised wherever keys are being
            // forwarded to a container, because that is exactly where Ctrl-C
            // would otherwise be the only way out — and Ctrl-C reaches the
            // agent.
            if workflow_active {
                vec![Span::styled(
                    " ctrl-m minimize  \u{00b7}  ctrl-\\ detach  \u{00b7}  ctrl-w workflow controls ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )]
            } else {
                vec![Span::styled(
                    " ctrl-m minimize  \u{00b7}  ctrl-\\ detach  \u{00b7}  scroll \u{2195} history ",
                    Style::default().fg(Color::Yellow),
                )]
            }
        }
        // Running + ExecWindow + Minimized container
        (
            ExecutionPhase::Running { .. },
            Focus::ExecutionWindow,
            ContainerWindowState::Minimized,
        ) => {
            vec![Span::styled(
                " \u{2191}/\u{2193} scroll  \u{00b7}  b/e jump  \u{00b7}  ctrl-m restore container  \u{00b7}  Esc deselect ",
                Style::default().fg(Color::DarkGray),
            )]
        }
        // Running + ExecWindow + no container
        (ExecutionPhase::Running { .. }, Focus::ExecutionWindow, ContainerWindowState::Hidden) => {
            vec![Span::styled(
                " Press Esc to deselect the window ",
                Style::default().fg(Color::Yellow),
            )]
        }
        // Running + CommandBox
        (ExecutionPhase::Running { .. }, Focus::CommandBox, _) => {
            if workflow_active {
                vec![Span::styled(
                    " Press ctrl-w for workflow controls ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )]
            } else {
                vec![Span::styled(
                    " Press \u{2191} to focus the window ",
                    Style::default().fg(Color::DarkGray),
                )]
            }
        }
        // Done + ExecWindow
        (ExecutionPhase::Done { .. }, Focus::ExecutionWindow, _) => vec![Span::styled(
            " \u{2191}/\u{2193} scroll  \u{00b7}  b/e jump  \u{00b7}  Esc deselect ",
            Style::default().fg(Color::DarkGray),
        )],
        // Done + CommandBox
        (ExecutionPhase::Done { .. }, Focus::CommandBox, _) => vec![Span::styled(
            " Press \u{2191} to focus the window ",
            Style::default().fg(Color::DarkGray),
        )],
        // Error + ExecWindow
        (ExecutionPhase::Error { .. }, Focus::ExecutionWindow, _) => {
            let exit_code = match &tab.execution_phase {
                ExecutionPhase::Error { .. } => -1,
                ExecutionPhase::Done { exit_code, .. } => *exit_code,
                _ => 0,
            };
            vec![
                Span::styled(
                    format!(" Exit code: {} ", exit_code),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " \u{00b7}  \u{2191}/\u{2193} scroll  \u{00b7}  b/e jump  \u{00b7}  Esc deselect ",
                    Style::default().fg(Color::DarkGray),
                ),
            ]
        }
        // Error + CommandBox
        (ExecutionPhase::Error { .. }, Focus::CommandBox, _) => {
            let exit_code = match &tab.execution_phase {
                ExecutionPhase::Error { .. } => -1,
                ExecutionPhase::Done { exit_code, .. } => *exit_code,
                _ => 0,
            };
            vec![
                Span::styled(
                    format!(" Exit code: {} ", exit_code),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " \u{00b7}  Press \u{2191} to focus the window ",
                    Style::default().fg(Color::DarkGray),
                ),
            ]
        }
        // Idle: just the git-sidebar hint.
        _ => vec![Span::styled(
            " \u{00b7} ctrl-g git ",
            Style::default().fg(Color::DarkGray),
        )],
    };

    // A live workflow always has the Workflow Overview on screen, so advertise
    // the Ctrl-O min/max in whichever direction it can currently go.
    if workflow_active {
        let label = if tab.workflow_overview_state.is_maximized() {
            " \u{00b7} ctrl-o minimize workflow overview "
        } else {
            " \u{00b7} ctrl-o maximize workflow overview "
        };
        spans.push(Span::styled(label, Style::default().fg(Color::DarkGray)));
    }

    // When the sidebar is not visible (closed or collapsed for a narrow
    // terminal), show the compact `+A -D` diff summary at the far right of the
    // 1-row status bar (green `+`, red `-`).
    if !sidebar_visible {
        if let Some(summary) = tab.git_diff_summary.lock().ok().and_then(|g| g.clone()) {
            let git_spans = vec![
                Span::styled(
                    format!("+{}", summary.added),
                    Style::default().fg(Color::Green),
                ),
                Span::raw(" "),
                Span::styled(
                    format!("-{} ", summary.removed),
                    Style::default().fg(Color::Red),
                ),
            ];
            let left_w: usize = spans.iter().map(|s| s.content.chars().count()).sum();
            let git_w: usize = git_spans.iter().map(|s| s.content.chars().count()).sum();
            let total = area.width as usize;
            if total > left_w + git_w {
                spans.push(Span::raw(" ".repeat(total - left_w - git_w)));
                spans.extend(git_spans);
            }
        }
    }

    let bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black));
    frame.render_widget(bar, area);
}

/// The hint-bar text for a squad action that failed. `command` is the command
/// line the key binding dispatched (`squad pause nightly`); it is empty only
/// when a failure arrives before one was recorded, which is why the fallback
/// still names squad rather than reading as a bare error.
pub(crate) fn squad_failure_text(command: &str, message: &str) -> String {
    if command.is_empty() {
        format!("squad action failed: {message}")
    } else {
        format!("{command} failed: {message}")
    }
}
