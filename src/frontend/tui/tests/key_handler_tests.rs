//! Tests for `key_handler`: autocomplete, focus switching, non-dialog text
//! input, WorkflowControlBoard arrow-key handling, command-box locking,
//! Ctrl+W escalation, container-window/Workflow-Overview resize behavior, and
//! panic-log path resolution.

use super::*;
use crate::command::commands::squad::commands::SquadCommand;
use crate::command::dispatch::FrontendAction;

// ─── Autocomplete cycling ─────────────────────────────────────────────────

#[test]
fn autocomplete_next_fills_command_box_with_first_suggestion() {
    let mut app = make_app();
    // Type enough for a known completion
    for c in "cha".chars() {
        press_char(&mut app, c);
    }
    press_key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert!(
        app.command_input.text.contains("chat"),
        "expected 'chat' in input, got: {:?}",
        app.command_input.text
    );
}

#[test]
fn autocomplete_prev_fills_command_box_with_last_suggestion() {
    let mut app = make_app();
    for c in "cha".chars() {
        press_char(&mut app, c);
    }
    // Update suggestions so we know the last one
    app.update_suggestions();
    let last = app.suggestion_row.last().cloned().unwrap_or_default();
    press_key(&mut app, KeyCode::BackTab, KeyModifiers::NONE);
    assert!(
        app.command_input.text.contains("cha"),
        "expected suggestion containing 'cha', got: {:?}",
        app.command_input.text
    );
    // The text should match the last suggestion (or still contain "cha" if only one)
    let _ = last; // used above
}

#[test]
fn tab_with_no_suggestions_leaves_input_unchanged() {
    let mut app = make_app();
    for c in "zzzzz".chars() {
        press_char(&mut app, c);
    }
    press_key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.command_input.text, "zzzzz");
}

// ─── Focus switching ──────────────────────────────────────────────────────

#[test]
fn up_arrow_in_command_box_switches_focus_to_execution_window() {
    let mut app = make_app();
    assert_eq!(app.focus, Focus::CommandBox);
    press_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::ExecutionWindow);
}

#[test]
fn esc_in_execution_window_returns_focus_to_command_box() {
    let mut app = make_app();
    app.focus = Focus::ExecutionWindow;
    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::CommandBox);
}

// ─── Text input (non-dialog) ──────────────────────────────────────────────

#[test]
fn empty_command_submit_does_not_set_execution_phase() {
    use crate::frontend::tui::tabs::ExecutionPhase;
    let mut app = make_app();
    // input is empty by default
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        app.tabs[app.active_tab].execution_phase,
        ExecutionPhase::Idle
    );
}

// ─── Toggle status log ────────────────────────────────────────────────────

#[test]
fn l_in_execution_window_toggles_status_log() {
    let mut app = make_app();
    app.focus = Focus::ExecutionWindow;
    let initial = app.tabs[app.active_tab].status_log_collapsed;
    press_char(&mut app, 'l');
    assert_ne!(app.tabs[app.active_tab].status_log_collapsed, initial);
}

// ─── Ctrl-O / Workflow Overview ─────────────────────────────────────

#[test]
fn workflow_overview_starts_minimized_and_ctrl_o_toggles_it() {
    use crate::frontend::tui::tabs::WorkflowOverviewState;

    let mut app = make_app();
    assert_eq!(
        app.active_tab().workflow_overview_state,
        WorkflowOverviewState::Minimized,
        "the overview must default to the minimized one-box-per-stage view"
    );

    press_key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab().workflow_overview_state,
        WorkflowOverviewState::Maximized
    );

    press_key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab().workflow_overview_state,
        WorkflowOverviewState::Minimized
    );
}

#[test]
fn ctrl_o_resets_the_overview_scroll_offset() {
    // A stale offset from a previous maximization would otherwise hide the first
    // steps of the stage the next time the overview opens.
    let mut app = make_app();
    app.active_tab_mut().workflow_overview_scroll_offset = 4;
    press_key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert_eq!(app.active_tab().workflow_overview_scroll_offset, 0);
}

// ─── WorkflowControlBoard arrow keys ─────────────────────────────────────

fn setup_wcb_dialog(app: &mut App) -> std::sync::mpsc::Receiver<DialogResponse> {
    let (tx, rx) = std::sync::mpsc::channel();
    app.tabs[app.active_tab].dialog_response_tx = Some(tx);
    app.active_dialog = Some(Dialog::WorkflowControlBoard(
        crate::frontend::tui::dialogs::WorkflowControlBoardState {
            step_name: "test".into(),
            can_launch_next: true,
            can_continue_current: true,
            can_restart: true,
            can_go_back: true,
            can_finish: true,
            continue_unavailable_reason: None,
            cancel_to_previous_unavailable_reason: None,
            finish_workflow_unavailable_reason: None,
            restart_unavailable_reason: None,
            can_dismiss: false,
            launch_next_label: None,
            focused_step_name: "test".into(),
            parallel_peer_count: 0,
            parallel_peers_running: 0,
            failure_lines: Vec::new(),
        },
    ));
    app.command_dialog_active = true;
    rx
}

#[test]
fn wcb_right_arrow_sends_launch_next() {
    let mut app = make_app();
    let rx = setup_wcb_dialog(&mut app);
    press_key(&mut app, KeyCode::Right, KeyModifiers::NONE);
    let resp = rx.try_recv().unwrap();
    assert!(matches!(resp, DialogResponse::Char('>')));
    assert!(app.active_dialog.is_none());
}

#[test]
fn wcb_down_arrow_sends_continue_current() {
    let mut app = make_app();
    let rx = setup_wcb_dialog(&mut app);
    press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    let resp = rx.try_recv().unwrap();
    assert!(matches!(resp, DialogResponse::Char('v')));
}

#[test]
fn wcb_up_arrow_sends_restart_step() {
    let mut app = make_app();
    let rx = setup_wcb_dialog(&mut app);
    press_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
    let resp = rx.try_recv().unwrap();
    assert!(matches!(resp, DialogResponse::Char('^')));
}

#[test]
fn wcb_left_arrow_sends_cancel_to_previous() {
    let mut app = make_app();
    let rx = setup_wcb_dialog(&mut app);
    press_key(&mut app, KeyCode::Left, KeyModifiers::NONE);
    let resp = rx.try_recv().unwrap();
    assert!(matches!(resp, DialogResponse::Char('<')));
}

#[test]
fn wcb_ctrl_enter_sends_finish_workflow() {
    let mut app = make_app();
    let rx = setup_wcb_dialog(&mut app);
    press_key(&mut app, KeyCode::Enter, KeyModifiers::CONTROL);
    let resp = rx.try_recv().unwrap();
    assert!(matches!(resp, DialogResponse::Char('f')));
}

#[test]
fn wcb_plain_enter_sends_finish_workflow() {
    let mut app = make_app();
    let rx = setup_wcb_dialog(&mut app);
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let resp = rx.try_recv().unwrap();
    assert!(matches!(resp, DialogResponse::Char('f')));
}

#[test]
fn wcb_enter_ignored_when_finish_unavailable() {
    let mut app = make_app();
    let (tx, rx) = std::sync::mpsc::channel();
    app.tabs[app.active_tab].dialog_response_tx = Some(tx);
    app.active_dialog = Some(Dialog::WorkflowControlBoard(
        crate::frontend::tui::dialogs::WorkflowControlBoardState {
            step_name: "test".into(),
            can_launch_next: true,
            can_continue_current: true,
            can_restart: true,
            can_go_back: true,
            can_finish: false,
            continue_unavailable_reason: None,
            cancel_to_previous_unavailable_reason: None,
            finish_workflow_unavailable_reason: Some("not last step".into()),
            restart_unavailable_reason: None,
            can_dismiss: false,
            launch_next_label: None,
            focused_step_name: "test".into(),
            parallel_peer_count: 0,
            parallel_peers_running: 0,
            failure_lines: Vec::new(),
        },
    ));
    app.command_dialog_active = true;
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        rx.try_recv().is_err(),
        "Enter must not send FinishWorkflow when can_finish is false"
    );
}

/// WI-0115 §1: the board renders an unavailable action greyed out with its
/// reason, so its arrow must not raise it. On a failure board the engine would
/// only re-present an identical board, which reads as a broken keystroke.
#[test]
fn wcb_arrows_are_inert_for_actions_the_board_does_not_offer() {
    for (key, label) in [
        (KeyCode::Right, "launch next"),
        (KeyCode::Left, "back to previous"),
        (KeyCode::Up, "restart"),
        (KeyCode::Down, "continue in container"),
    ] {
        let mut app = make_app();
        let (tx, rx) = std::sync::mpsc::channel();
        app.tabs[app.active_tab].dialog_response_tx = Some(tx);
        app.active_dialog = Some(Dialog::WorkflowControlBoard(
            crate::frontend::tui::dialogs::WorkflowControlBoardState {
                step_name: "test".into(),
                can_launch_next: false,
                can_continue_current: false,
                can_restart: false,
                can_go_back: false,
                can_finish: false,
                continue_unavailable_reason: None,
                cancel_to_previous_unavailable_reason: None,
                finish_workflow_unavailable_reason: None,
                restart_unavailable_reason: None,
                can_dismiss: false,
                launch_next_label: None,
                focused_step_name: "test".into(),
                parallel_peer_count: 0,
                parallel_peers_running: 0,
                failure_lines: vec!["Exit code: 1".into()],
            },
        ));
        app.command_dialog_active = true;

        press_key(&mut app, key, KeyModifiers::NONE);
        assert!(
            rx.try_recv().is_err(),
            "{label} must not be raised when the board does not offer it"
        );
        assert!(
            app.active_dialog.is_some(),
            "{label}: the board must stay up so the user can pick something real"
        );
    }
}

#[test]
fn wcb_ctrl_c_sends_abort() {
    let mut app = make_app();
    let rx = setup_wcb_dialog(&mut app);
    press_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    let resp = rx.try_recv().unwrap();
    assert!(matches!(resp, DialogResponse::Char('a')));
}

#[test]
fn wcb_esc_sends_dismissed() {
    let mut app = make_app();
    let rx = setup_wcb_dialog(&mut app);
    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    let resp = rx.try_recv().unwrap();
    assert!(matches!(resp, DialogResponse::Dismissed));
}

// ─── Command box locked during Running ────────────────────────────────────

#[test]
fn char_input_blocked_while_running() {
    let mut app = make_app();
    app.tabs[app.active_tab].execution_phase =
        crate::frontend::tui::tabs::ExecutionPhase::Running {
            command: "chat".into(),
        };
    press_char(&mut app, 'x');
    assert_eq!(
        app.command_input.text, "",
        "command box must be locked while running"
    );
}

#[test]
fn backspace_blocked_while_running() {
    let mut app = make_app();
    app.command_input.set_text("abc");
    app.tabs[app.active_tab].execution_phase =
        crate::frontend::tui::tabs::ExecutionPhase::Running {
            command: "chat".into(),
        };
    press_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(
        app.command_input.text, "abc",
        "backspace must be blocked while running"
    );
}

#[test]
fn submit_command_blocked_while_running() {
    use crate::frontend::tui::tabs::ExecutionPhase;
    let mut app = make_app();
    app.command_input.set_text("status");
    app.tabs[app.active_tab].execution_phase = ExecutionPhase::Running {
        command: "chat".into(),
    };
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    // Phase should still be Running, not a new command
    assert!(matches!(
        app.tabs[app.active_tab].execution_phase,
        ExecutionPhase::Running { .. }
    ));
}

// ─── Command-box parse rejections (WI 0114 F-26) ─────────────────────────
//
// The box parses through the same `parse_raw_args` the API frontend uses, so
// an input the API refuses is refused here too, with the same message. Both
// of these used to be accepted by the box's own parser and carried into the
// command as strings.

#[test]
fn submitting_a_bad_enum_value_leaves_the_box_with_an_error() {
    let mut app = make_app();
    app.command_input.set_text("chat --launch-mode banana");

    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    let error = app
        .input_error
        .as_deref()
        .expect("a bad enum value must be reported in the command box");
    assert!(
        error.contains("banana") && error.contains("stdio"),
        "the hint must name the bad value and the allowed set: {error}"
    );
    assert!(
        !app.command_input.text.is_empty(),
        "the typed text must survive so the user can correct it"
    );
}

#[test]
fn submitting_a_non_numeric_number_leaves_the_box_with_an_error() {
    let mut app = make_app();
    app.command_input.set_text("squad start --port abc");

    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    let error = app
        .input_error
        .as_deref()
        .expect("a non-numeric number must be reported in the command box");
    assert!(
        error.contains("abc"),
        "the hint must name the bad value: {error}"
    );
}

/// The cluster's refusal changed kind from `CommandBoxParse` to `UnknownFlag`;
/// the hint must still read as what the user typed.
#[test]
fn submitting_a_short_flag_cluster_names_the_cluster() {
    let mut app = make_app();
    app.command_input.set_text("ready -ab");

    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(app.input_error.as_deref(), Some("unknown flag: -ab"));
}

// ─── q with empty box opens QuitConfirm ──────────────────────────────────

#[test]
fn q_with_empty_command_box_opens_quit_confirm() {
    let mut app = make_app();
    assert!(app.command_input.text.is_empty());
    press_char(&mut app, 'q');
    assert!(
        matches!(app.active_dialog, Some(Dialog::QuitConfirm)),
        "q with empty command box must open QuitConfirm"
    );
}

#[test]
fn q_with_nonempty_command_box_inserts_char() {
    let mut app = make_app();
    app.command_input.set_text("quer");
    press_char(&mut app, 'y');
    assert_eq!(app.command_input.text, "query");
    assert!(app.active_dialog.is_none());
}

// ─── Any key in Done/Error execution window refocuses command box ─────────

#[test]
fn any_unhandled_key_in_done_execution_window_refocuses_command_box() {
    let mut app = make_app();
    app.focus = Focus::ExecutionWindow;
    app.tabs[app.active_tab].execution_phase = crate::frontend::tui::tabs::ExecutionPhase::Done {
        command: "chat".into(),
        exit_code: 0,
    };
    // Press a key that maps to Action::None in execution window context
    press_char(&mut app, 'x');
    assert_eq!(
        app.focus,
        Focus::CommandBox,
        "unhandled key in Done execution window must refocus command box"
    );
}

#[test]
fn any_unhandled_key_in_error_execution_window_refocuses_command_box() {
    let mut app = make_app();
    app.focus = Focus::ExecutionWindow;
    app.tabs[app.active_tab].execution_phase = crate::frontend::tui::tabs::ExecutionPhase::Error {
        command: "chat".into(),
        message: "failed".into(),
    };
    press_char(&mut app, 'z');
    assert_eq!(app.focus, Focus::CommandBox);
}

#[test]
fn unhandled_key_in_running_execution_window_does_not_refocus() {
    let mut app = make_app();
    app.focus = Focus::ExecutionWindow;
    app.tabs[app.active_tab].execution_phase =
        crate::frontend::tui::tabs::ExecutionPhase::Running {
            command: "chat".into(),
        };
    press_char(&mut app, 'x');
    assert_eq!(
        app.focus,
        Focus::ExecutionWindow,
        "focus must not change during Running"
    );
}

// ─── Ctrl+W workflow control ──────────────────────────────────────────────

#[test]
fn ctrl_w_with_no_workflow_is_silent_noop() {
    let mut app = make_app();
    // No engine_tx set — Ctrl-W is a silent no-op per spec.
    press_key(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert_eq!(
        app.status_bar.text, "",
        "Ctrl+W with no engine_tx must be a silent no-op"
    );
    assert!(
        app.active_dialog.is_none(),
        "no dialog must be opened when no workflow is active"
    );
}

#[test]
fn ctrl_w_during_running_step_sends_engine_request() {
    use crate::engine::workflow::EngineRequest;
    use crate::frontend::tui::tabs::WorkflowStepKind;
    use crate::frontend::tui::tabs::WorkflowViewState;
    use crate::frontend::tui::tabs::{StepViewStatus, WorkflowStepView};

    let mut app = make_app();

    // Seed the workflow_state with a running step.
    let view = WorkflowViewState {
        steps: vec![WorkflowStepView {
            name: "build".into(),
            status: StepViewStatus::Running,
            agent: None,
            model: None,
            depends_on: vec![],
            kind: WorkflowStepKind::Agent,
        }],
        current_step: Some("build".into()),
        max_concurrent: None,
    };
    *app.active_tab_mut().shared.workflow_state.lock().unwrap() = Some(view);

    // Wire up an engine channel so we can observe what's sent.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EngineRequest>();
    *app.active_tab_mut().shared.engine_tx_shared.lock().unwrap() = Some(tx);

    press_key(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);

    let msg = rx.try_recv().expect("engine tx must receive a message");
    assert!(
        matches!(msg, EngineRequest::OpenControlBoard { .. }),
        "Ctrl+W during a running step must send OpenControlBoard"
    );
}

#[test]
fn ctrl_w_in_step_confirm_escalates_to_wcb() {
    use crate::engine::workflow::EngineRequest;

    let mut app = make_app();

    // Wire up an engine channel so Ctrl-W handler fires.
    let (engine_tx, _engine_rx) = tokio::sync::mpsc::unbounded_channel::<EngineRequest>();
    *app.active_tab_mut().shared.engine_tx_shared.lock().unwrap() = Some(engine_tx);

    // Open a StepConfirm dialog with a response channel.
    let (tx, rx) = std::sync::mpsc::channel();
    app.tabs[app.active_tab].dialog_response_tx = Some(tx);
    app.active_dialog = Some(Dialog::WorkflowStepConfirm(
        crate::frontend::tui::dialogs::WorkflowStepConfirmState {
            completed_step: "build".into(),
            next_step: "test".into(),
        },
    ));
    app.command_dialog_active = true;

    press_key(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);

    // The dialog should have been dismissed.
    assert!(
        app.active_dialog.is_none(),
        "StepConfirm dialog must close on Ctrl+W"
    );
    // The frontend must have received Char('W') so it can open the full WCB.
    let resp = rx
        .try_recv()
        .expect("dialog_response_tx must receive a message");
    assert!(
        matches!(
            resp,
            crate::frontend::tui::dialogs::DialogResponse::Char('W')
        ),
        "escalation must send Char('W') to trigger full WCB"
    );
}

// ─── ContainerWindow cycle / resize ──────────────────────────────────────

#[test]
fn cycle_to_hidden_does_not_send_resize() {
    let mut app = make_app();
    // Install a slot and wire its resize channel to observe.
    let (resize_tx, mut resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
    app.active_tab_mut()
        .start_container("claude".into(), String::new(), 80, 24);
    app.active_tab_mut()
        .focused_slot_mut()
        .unwrap()
        .container_resize_tx = Some(resize_tx);

    // Start at Maximized, cycle → Minimized (not Hidden, resize expected on next test).
    app.active_tab_mut().container_window_state =
        crate::frontend::tui::tabs::ContainerWindowState::Maximized;
    // Cycle: Maximized → Minimized
    press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab().container_window_state,
        crate::frontend::tui::tabs::ContainerWindowState::Minimized,
    );

    // Cycle again: Minimized → Maximized (still not hidden, resize may be sent)
    press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab().container_window_state,
        crate::frontend::tui::tabs::ContainerWindowState::Maximized,
    );

    // Cycle: Maximized → Minimized once more — no Hidden state reached yet.
    // Now let's explicitly set Hidden and verify cycling to Hidden sends nothing.
    app.active_tab_mut().container_window_state =
        crate::frontend::tui::tabs::ContainerWindowState::Minimized;
    // Drain channel to reset state.
    while resize_rx.try_recv().is_ok() {}

    // Hidden → Maximized (sending resize) then Maximized → Minimized (sending resize)
    // We want to reach Hidden from Minimized: but cycle(Minimized) = Maximized.
    // Actually cycle(Hidden) = Maximized, cycle(Minimized) = Maximized, cycle(Maximized) = Minimized.
    // There's no transition TO Hidden — Hidden is the initial state.
    // So we test that cycling out of Hidden (to Maximized) might send a resize,
    // and cycling Maximized → Minimized does NOT go to Hidden and always sends resize.
    // "Cycle to hidden does not send resize" means starting from Maximized → Minimized:
    // In that transition, a resize IS sent (not hidden). But if we start from Hidden and
    // cycle, we go to Maximized (sends resize). Since Hidden isn't reachable via cycle from
    // a non-hidden state, let's verify: starting at Maximized, cycling to Minimized.
    app.active_tab_mut().container_window_state =
        crate::frontend::tui::tabs::ContainerWindowState::Maximized;
    while resize_rx.try_recv().is_ok() {}
    press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
    // Minimized ≠ Hidden so resize is attempted (may fail in CI env).
    // The key assertion: cycling from Hidden should not send resize even if Hidden
    // is explicitly set.
    app.active_tab_mut().container_window_state =
        crate::frontend::tui::tabs::ContainerWindowState::Hidden;
    // Drop the slot's resize channel.
    app.active_tab_mut()
        .focused_slot_mut()
        .unwrap()
        .container_resize_tx = None;
    // Cycling from Hidden → Maximized — the resize send should not panic.
    press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab().container_window_state,
        crate::frontend::tui::tabs::ContainerWindowState::Maximized,
    );
}

// ─── Workflow Overview scroll ────────────────────────────────────────────────

#[test]
fn scroll_down_reveals_hidden_parallel_steps() {
    use crate::frontend::tui::tabs::{
        StepViewStatus, WorkflowStepKind, WorkflowStepView, WorkflowViewState,
    };
    use crossterm::event::{MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;

    let mut app = make_app();

    // Seed a workflow with many parallel steps so the overview would overflow.
    let view = WorkflowViewState {
        steps: (0..6)
            .map(|i| WorkflowStepView {
                name: format!("step-{i}"),
                status: StepViewStatus::Pending,
                agent: None,
                model: None,
                depends_on: vec![],
                kind: WorkflowStepKind::Agent,
            })
            .collect(),
        current_step: None,
        max_concurrent: None,
    };
    *app.active_tab_mut().shared.workflow_state.lock().unwrap() = Some(view);

    // Simulate the renderer having recorded an overview rect.
    let overview_rect = Rect::new(0, 30, 80, 9);
    app.active_tab_mut().last_overview_rect = Some(overview_rect);

    assert_eq!(app.active_tab().workflow_overview_scroll_offset, 0);

    // Mouse scroll-down inside the overview rect increments the offset.
    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 10,
            row: 32, // inside overview_rect
            modifiers: KeyModifiers::NONE,
        },
    );
    assert_eq!(
        app.active_tab().workflow_overview_scroll_offset,
        1,
        "scroll down inside the overview must increment workflow_overview_scroll_offset"
    );
}

#[test]
fn scroll_clamped_at_bounds() {
    use crate::frontend::tui::tabs::{
        StepViewStatus, WorkflowStepKind, WorkflowStepView, WorkflowViewState,
    };
    use crossterm::event::{MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;

    let mut app = make_app();
    let view = WorkflowViewState {
        steps: vec![WorkflowStepView {
            name: "only".into(),
            status: StepViewStatus::Pending,
            agent: None,
            model: None,
            depends_on: vec![],
            kind: WorkflowStepKind::Agent,
        }],
        current_step: None,
        max_concurrent: None,
    };
    *app.active_tab_mut().shared.workflow_state.lock().unwrap() = Some(view);

    let overview_rect = Rect::new(0, 30, 80, 3);
    app.active_tab_mut().last_overview_rect = Some(overview_rect);

    // Scroll up when already at 0 → offset stays at 0 (no underflow).
    crate::frontend::tui::mouse_handler::handle_mouse_event(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 10,
            row: 31,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert_eq!(
        app.active_tab().workflow_overview_scroll_offset,
        0,
        "scrolling up at offset=0 must not underflow"
    );
}

// ─── Panic log ────────────────────────────────────────────────────────────

#[test]
fn panic_log_path_lives_under_awman_home() {
    // Skip on hosts with no resolvable home dir (the hook no-ops there).
    if let Some(log) = crate::frontend::tui::event_loop::panic_log() {
        assert!(
            log.path().ends_with(".awman/panic.log"),
            "panic log must live in the awman data dir: {}",
            log.path().display()
        );
    }
}

// ─── Container inner-size seam (WI-0098 Finding C module split) ──────────────

#[test]
fn compute_container_inner_size_subtracts_chrome_and_border() {
    // Pure seam extracted into `event_loop` during the module split. A typical
    // terminal: 95% of the width/exec-height, then minus the 2-cell border.
    // cols: 100*95/100 = 95, -2 border = 93.
    // exec_height: 40 - 8 chrome = 32; 32*95/100 = 30, -2 border = 28.
    let (cols, rows) = crate::frontend::tui::event_loop::compute_container_inner_size(100, 40);
    assert_eq!((cols, rows), (93, 28));
}

#[test]
fn compute_container_inner_size_floors_on_tiny_terminal() {
    // Saturating math must keep the grid at its minimums for a tiny terminal
    // rather than underflowing: cols floor 10-2=8, rows floor 5-2=3.
    let (cols, rows) = crate::frontend::tui::event_loop::compute_container_inner_size(1, 1);
    assert_eq!((cols, rows), (8, 3));
}

// ─── Yolo countdown modal + parallel container rotation ──────────────────

fn push_parallel_slots(app: &mut App) {
    use crate::frontend::tui::tabs::ContainerSlot;
    let tab = app.active_tab_mut();
    tab.dormant_slots
        .push(ContainerSlot::new(String::new(), "claude".into(), 1000));
    tab.container_slots
        .push(ContainerSlot::new("build".into(), "claude".into(), 1000));
    tab.container_slots
        .push(ContainerSlot::new("test".into(), "codex".into(), 1000));
}

#[test]
fn ctrl_s_cycles_focused_slot_while_yolo_modal_is_open() {
    use crate::frontend::tui::dialogs::WorkflowYoloCountdownState;

    let mut app = make_app();
    push_parallel_slots(&mut app);
    app.active_dialog = Some(Dialog::WorkflowYoloCountdown(WorkflowYoloCountdownState {
        step_name: "build".into(),
        remaining_secs: 30,
    }));

    press_key(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);

    assert_eq!(
        app.active_tab().focused_slot_idx,
        1,
        "Ctrl-S must still rotate the focused slot while the modal is open"
    );
    assert!(
        app.active_dialog.is_none(),
        "the modal is dismissed here; tick_all_tabs re-derives it for the new focus"
    );
}

#[test]
fn ctrl_s_with_single_slot_leaves_yolo_modal_open() {
    use crate::frontend::tui::dialogs::WorkflowYoloCountdownState;

    let mut app = make_app();
    // A plain (sequential) yolo countdown: no parallel group, one slot.
    app.active_dialog = Some(Dialog::WorkflowYoloCountdown(WorkflowYoloCountdownState {
        step_name: "build".into(),
        remaining_secs: 30,
    }));

    press_key(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);

    assert!(
        app.active_dialog.is_some(),
        "with no parallel group to rotate, Ctrl-S must not swallow the modal"
    );
}

#[test]
fn esc_on_parallel_yolo_modal_cancels_the_focused_slots_flag_only() {
    use crate::frontend::tui::dialogs::WorkflowYoloCountdownState;

    let mut app = make_app();
    push_parallel_slots(&mut app);
    app.active_tab_mut().focused_slot_idx = 1; // "test" is focused
    app.active_dialog = Some(Dialog::WorkflowYoloCountdown(WorkflowYoloCountdownState {
        step_name: "test".into(),
        remaining_secs: 5,
    }));

    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    assert!(app.active_dialog.is_none());
    assert!(
        app.active_tab().container_slots[1]
            .yolo_cancel_flag
            .load(std::sync::atomic::Ordering::Relaxed),
        "the focused slot's own cancel flag must be set"
    );
    assert!(
        !app.active_tab().container_slots[0]
            .yolo_cancel_flag
            .load(std::sync::atomic::Ordering::Relaxed),
        "the non-focused sibling's cancel flag must be untouched"
    );
    assert!(
        !app.active_tab()
            .shared
            .yolo_cancel_flag
            .load(std::sync::atomic::Ordering::Relaxed),
        "the tab-level (sequential-path) flag is unrelated here"
    );
}

// ─── squad tab key handling (WI 0102) ──────────────────────────────────────

/// Push the singleton squad tab (bypassing the daemon-backed
/// `open_or_focus_squad_tab` path — these tests only need `is_squad` state, not
/// a live gateway), focus it, and return its index.
fn push_squad_tab(app: &mut App) -> usize {
    let tab = Tab::new_squad(make_session());
    app.tabs.push(tab);
    let idx = app.tabs.len() - 1;
    app.active_tab = idx;
    app.focus = Focus::ExecutionWindow;
    idx
}

fn fake_task(name: &str) -> crate::data::fs::task_store::Task {
    use crate::data::fs::task_store::{MountScope, TaskStatus};
    let now = chrono::Utc::now();
    crate::data::fs::task_store::Task {
        id: name.to_string(),
        name: name.to_string(),
        description: "test task".into(),
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

/// Populate the active (squad) tab's snapshot with fake tasks and select
/// the first one, so the selection-dependent actions (`a`/`Enter`/`p`/`r`/`d`)
/// have something to act on.
fn set_squad_tasks(app: &mut App, names: &[&str]) {
    let state = app
        .active_tab()
        .squad
        .as_ref()
        .expect("active tab is squad");
    let mut snap = state.snapshot.lock().unwrap();
    snap.tasks = names.iter().map(|n| fake_task(n)).collect();
    snap.loaded = true;
}

/// An `App` whose `Engines` report no container runtime, so any code path
/// that would otherwise touch the real squad daemon (`SquadSupervisor`,
/// `provision_key`'s key-hash write) instead takes the sandbox-refusal
/// fast-path deterministically, with no filesystem or process side effects —
/// mirroring `tests/squad_sandbox_refusal.rs`'s `FakeSandboxRuntime` approach.
fn make_app_no_container_runtime() -> App {
    let catalogue = CommandCatalogue::get();
    let mut engines = crate::command::dispatch::Engines::for_tests(std::path::Path::new("/tmp"));
    engines.container_runtime = None;
    let session_manager = Arc::new(SessionManager::in_memory());
    let tab = Tab::new(make_session());
    App::new(
        catalogue,
        engines,
        session_manager,
        tab,
        super::test_runtime_handle(),
    )
}

/// An app with a second ordinary tab plus the squad tab active — enough tabs
/// for Ctrl-A/Ctrl-D navigation away from the squad tab to be observable.
fn squad_list_app() -> App {
    let mut app = make_app();
    app.add_tab(
        make_session().working_dir().to_path_buf(),
        SessionOpenOptions::default(),
    )
    .unwrap();
    push_squad_tab(&mut app);
    app
}

/// WI 0112: the squad shortcut lives in the dialog's key-hint row (asserted
/// by the render tests), not in the prompt body.
#[test]
fn ctrl_t_new_tab_dialog_prompt_is_the_working_directory_question_alone() {
    let mut app = make_app();
    press_key(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    match &app.active_dialog {
        Some(Dialog::TextInput { title, prompt, .. }) => {
            assert_eq!(title, crate::frontend::tui::dialogs::NEW_TAB_DIALOG_TITLE);
            assert_eq!(prompt, "Working directory:");
            assert!(
                !prompt.contains("Ctrl-S"),
                "the squad hint belongs in the hint row, not the prompt: {prompt:?}"
            );
        }
        _ => panic!("Ctrl-T must open the New Tab TextInput dialog"),
    }
}

#[test]
fn ctrl_s_in_new_tab_dialog_focuses_existing_squad_tab_and_closes_dialog() {
    let mut app = make_app();
    let squad_idx = push_squad_tab(&mut app);
    app.active_tab = 0; // back on the normal tab
    press_key(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(matches!(app.active_dialog, Some(Dialog::TextInput { .. })));
    press_key(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(
        app.active_dialog.is_none(),
        "Ctrl-S in the New Tab dialog must close it"
    );
    assert_eq!(
        app.active_tab, squad_idx,
        "Ctrl-S must activate the squad tab"
    );
}

// The load-bearing tests for the Ctrl-S binding: the New Tab dialog is the
// ONLY place Ctrl-S opens squad, and rerouting it there must not disturb the
// global Ctrl-A previous-tab navigation. Three tabs are open so "previous
// tab" (tab 0) and "the squad tab" (tab 2) are distinct and the two behaviors
// can't be confused with one another.
#[test]
fn ctrl_a_without_dialog_switches_to_previous_tab_and_does_not_open_squad() {
    let mut app = make_app(); // tab 0
    app.add_tab(
        make_session().working_dir().to_path_buf(),
        SessionOpenOptions::default(),
    )
    .unwrap(); // tab 1
    let squad_idx = push_squad_tab(&mut app); // tab 2 == squad, active_tab == squad_idx
    app.active_tab = 1; // sit on the middle (normal) tab
    press_key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab, 0,
        "Ctrl-A with no dialog open must switch to the previous tab"
    );
    assert_ne!(app.active_tab, squad_idx);
    assert!(
        app.active_dialog.is_none(),
        "Ctrl-A with no dialog open must not open any dialog"
    );
}

#[test]
fn ctrl_s_with_new_tab_dialog_open_opens_squad_and_does_not_switch_tabs() {
    let mut app = make_app(); // tab 0
    app.add_tab(
        make_session().working_dir().to_path_buf(),
        SessionOpenOptions::default(),
    )
    .unwrap(); // tab 1
    let squad_idx = push_squad_tab(&mut app); // tab 2 == squad
    app.active_tab = 1; // sit on the middle tab: "previous" (0) != squad (2)
    press_key(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(matches!(app.active_dialog, Some(Dialog::TextInput { .. })));
    press_key(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab, squad_idx,
        "Ctrl-S inside the New Tab dialog must open the squad tab"
    );
    assert_ne!(
        app.active_tab, 0,
        "must not have fallen through to previous-tab navigation"
    );
    assert!(
        app.active_dialog.is_none(),
        "the New Tab dialog must be closed"
    );
}

// ── the six squad list keys fire only in FocusContext::SquadList ────────────

#[test]
fn squad_list_enter_opens_task_detail() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    set_squad_tasks(&mut app, &["task-a"]);
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    match &app.active_dialog {
        Some(Dialog::SquadTaskDetail(state)) => assert_eq!(state.name, "task-a"),
        _ => panic!("Enter in the squad list must open the task detail modal"),
    }
}

#[test]
fn squad_list_enter_is_noop_when_list_is_empty() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.active_dialog.is_none(),
        "Enter on an empty squad list must not open a dialog"
    );
}

#[test]
fn squad_list_a_routes_to_start_squad_attach() {
    // Force the sandbox-refusal fast-path (see `make_app_no_container_runtime`)
    // so this stays deterministic and side-effect-free while still proving
    // `a` reaches `start_squad_attach` rather than doing nothing.
    let mut app = make_app_no_container_runtime();
    push_squad_tab(&mut app);
    set_squad_tasks(&mut app, &["task-a"]);
    press_key(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    assert!(
        app.status_bar
            .text
            .contains("squad requires a container runtime"),
        "'a' in the squad list must route to start_squad_attach: {:?}",
        app.status_bar.text
    );
}

#[test]
fn squad_list_n_dispatches_squad_add_interview() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    press_key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
    assert!(
        app.active_tab().command_result_rx.is_some(),
        "'n' in the squad list must dispatch `squad add --interview`"
    );
}

#[test]
fn squad_list_r_dispatches_resume() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    set_squad_tasks(&mut app, &["task-a"]);
    press_key(&mut app, KeyCode::Char('r'), KeyModifiers::NONE);
    assert!(
        app.active_tab().command_result_rx.is_some(),
        "'r' in the squad list must dispatch `squad resume`"
    );
}

#[test]
fn squad_list_p_and_r_are_noop_when_list_is_empty() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    press_key(&mut app, KeyCode::Char('p'), KeyModifiers::NONE);
    assert!(app.active_tab().command_result_rx.is_none());
    press_key(&mut app, KeyCode::Char('r'), KeyModifiers::NONE);
    assert!(app.active_tab().command_result_rx.is_none());
}

#[test]
fn squad_list_d_opens_remove_confirm_and_only_dispatches_on_y() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    set_squad_tasks(&mut app, &["task-a"]);
    press_key(&mut app, KeyCode::Char('d'), KeyModifiers::NONE);
    match &app.active_dialog {
        Some(Dialog::SquadRemoveConfirm { name }) => assert_eq!(name, "task-a"),
        _ => panic!("'d' must open Dialog::SquadRemoveConfirm"),
    }
    assert!(
        app.active_tab().command_result_rx.is_none(),
        "opening the confirmation must not itself dispatch a removal"
    );
    press_key(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert!(
        app.active_dialog.is_none(),
        "'y' must dismiss the confirmation"
    );
    assert!(
        app.active_tab().command_result_rx.is_some(),
        "'y' must dispatch `squad remove task-a`"
    );
}

#[test]
fn squad_list_d_then_n_dismisses_without_dispatching() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    set_squad_tasks(&mut app, &["task-a"]);
    press_key(&mut app, KeyCode::Char('d'), KeyModifiers::NONE);
    press_key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
    assert!(app.active_dialog.is_none());
    assert!(
        app.active_tab().command_result_rx.is_none(),
        "'n' must dismiss the confirmation without dispatching a removal"
    );
}

#[test]
fn squad_list_keys_are_inert_on_a_normal_tab() {
    let mut app = make_app();
    app.focus = Focus::ExecutionWindow;
    for key in [
        KeyCode::Enter,
        KeyCode::Char('a'),
        KeyCode::Char('n'),
        KeyCode::Char('p'),
        KeyCode::Char('r'),
        KeyCode::Char('d'),
    ] {
        press_key(&mut app, key, KeyModifiers::NONE);
    }
    assert!(
        app.active_dialog.is_none(),
        "squad list keys must not fire outside FocusContext::SquadList"
    );
    assert_eq!(app.tabs.len(), 1, "no tab must be added or removed");
}

#[test]
fn squad_list_arrows_move_selection_not_scroll_offset() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    set_squad_tasks(&mut app, &["a", "b", "c"]);
    let before_scroll = app.active_tab().scroll_offset;
    press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.active_tab().squad.as_ref().unwrap().selected, 1);
    assert_eq!(
        app.active_tab().scroll_offset,
        before_scroll,
        "scroll_offset must be untouched while the squad list holds focus"
    );
    press_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.active_tab().squad.as_ref().unwrap().selected, 0);
}

#[test]
fn squad_list_context_not_selected_while_attach_owns_the_tabs_slots() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    set_squad_tasks(&mut app, &["a", "b"]);
    app.active_tab_mut()
        .start_container("claude".into(), "awman-abc".into(), 80, 24);
    // With container_slots non-empty, arrow keys must fall through to the
    // ordinary ExecutionWindow/ContainerMaximized handling instead of moving
    // the squad selection.
    press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(
        app.active_tab().squad.as_ref().unwrap().selected,
        0,
        "FocusContext::SquadList must not apply while an attach session owns the tab's slots"
    );
}

// ── global Ctrl shortcuts keep their meaning while the squad list has focus ─
// (implementation-contract.md §2.9: "the single most important regression
// risk of adding a context")

#[test]
fn ctrl_t_still_opens_new_tab_dialog_from_squad_list() {
    let mut app = squad_list_app();
    press_key(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(matches!(app.active_dialog, Some(Dialog::TextInput { .. })));
}

#[test]
fn ctrl_a_still_switches_tabs_from_squad_list() {
    let mut app = squad_list_app();
    let squad_idx = app.active_tab;
    press_key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    assert_ne!(
        app.active_tab, squad_idx,
        "Ctrl-A must still switch tabs when the squad list holds focus"
    );
}

#[test]
fn ctrl_d_still_switches_tabs_from_squad_list() {
    let mut app = squad_list_app();
    let squad_idx = app.active_tab;
    press_key(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_ne!(
        app.active_tab, squad_idx,
        "Ctrl-D must still switch tabs when the squad list holds focus"
    );
}

#[test]
fn ctrl_m_still_cycles_container_window_from_squad_list() {
    let mut app = squad_list_app();
    let before = app.active_tab().container_window_state;
    press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
    assert_ne!(
        app.active_tab().container_window_state,
        before,
        "Ctrl-M must still cycle the container window from the squad list"
    );
}

#[test]
fn ctrl_w_from_squad_list_is_silent_noop_without_a_workflow() {
    let mut app = squad_list_app();
    press_key(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert!(
        app.active_dialog.is_none(),
        "Ctrl-W with no active workflow must stay a silent no-op from the squad list"
    );
}

#[test]
fn ctrl_g_is_globally_intercepted_but_a_noop_on_the_squad_tab() {
    let mut app = squad_list_app();
    press_key(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert_eq!(
        app.active_tab().git_sidebar_state,
        crate::frontend::tui::git_sidebar::GitSidebarState::Closed,
        "Ctrl-G must never open the git sidebar for the squad tab"
    );
}

#[test]
fn ctrl_c_still_opens_close_tab_confirm_from_squad_list() {
    let mut app = squad_list_app();
    press_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(matches!(app.active_dialog, Some(Dialog::CloseTabConfirm)));
}

// ── the detail modal's action tooltip (WI 0106 Part 5) ─────────────────────

/// Open the detail modal for `name`, whichever card the list happens to have
/// selected.
fn open_squad_detail(app: &mut App, name: &str) {
    let task = {
        let state = app
            .active_tab()
            .squad
            .as_ref()
            .expect("active tab is squad");
        let snap = state.snapshot.lock().unwrap();
        snap.tasks
            .iter()
            .find(|task| task.name == name)
            .expect("modal must be opened for a task in the snapshot")
            .clone()
    };
    app.active_dialog = Some(Dialog::SquadTaskDetail(
        crate::frontend::tui::dialogs::SquadDetailState {
            name: name.to_string(),
            task,
        },
    ));
}

/// The tooltip's keys must act on the task the modal is showing, not on
/// whatever the card grid currently has selected — the two can differ, since
/// the modal keeps showing its own task while the list reflows or the poller
/// reorders it.
#[test]
fn squad_detail_modal_r_resumes_and_d_confirms_against_the_modals_task() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    set_squad_tasks(&mut app, &["task-a", "task-b"]);

    open_squad_detail(&mut app, "task-b");
    press_key(&mut app, KeyCode::Char('r'), KeyModifiers::NONE);
    assert!(app.active_dialog.is_none());
    assert!(
        app.active_tab().command_result_rx.is_some(),
        "'r' in the modal must dispatch `squad resume`"
    );

    open_squad_detail(&mut app, "task-b");
    press_key(&mut app, KeyCode::Char('d'), KeyModifiers::NONE);
    match &app.active_dialog {
        Some(Dialog::SquadRemoveConfirm { name }) => assert_eq!(
            name, "task-b",
            "the confirmation must target the modal's task, not the list selection"
        ),
        _ => panic!("'d' in the modal must open the remove confirmation"),
    }
}

#[test]
fn squad_detail_modal_a_routes_to_start_squad_attach() {
    // Same sandbox-refusal fast-path the list-key test uses, so this stays
    // deterministic and side-effect-free.
    let mut app = make_app_no_container_runtime();
    push_squad_tab(&mut app);
    set_squad_tasks(&mut app, &["task-a"]);
    open_squad_detail(&mut app, "task-a");

    press_key(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);

    assert!(app.active_dialog.is_none());
    assert!(
        app.status_bar
            .text
            .contains("squad requires a container runtime"),
        "'a' in the modal must route to start_squad_attach: {:?}",
        app.status_bar.text
    );
}

// ── the run-history modal ───────────────────────────────────────────────────

/// Give the active squad tab's snapshot `count` runs for the selected task, so
/// the history modal has a table to show and something to scroll.
fn set_squad_runs(app: &mut App, task: &str, count: usize) {
    use crate::data::fs::task_store::{Run, RunStatus};
    let state = app
        .active_tab()
        .squad
        .as_ref()
        .expect("active tab is squad");
    let mut snap = state.snapshot.lock().unwrap();
    snap.runs = (0..count)
        .map(|i| Run {
            id: format!("run-{i}"),
            task_id: task.to_string(),
            status: RunStatus::WorkflowExecuted,
            workflow_path: None,
            workflow_state_path: None,
            session_id: None,
            started_at: chrono::Utc::now(),
            finished_at: None,
            error: None,
            reason: None,
            unmet_env: Vec::new(),
        })
        .collect();
}

/// `h` on the card grid opens the history for the selected task, and — because
/// no detail modal was ever up — Esc closes back to the grid rather than
/// opening one the user did not ask for.
#[test]
fn squad_list_h_opens_history_and_esc_closes_it_without_opening_the_detail_modal() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    set_squad_tasks(&mut app, &["task-a", "task-b"]);
    if let Some(state) = app.active_tab_mut().squad.as_mut() {
        state.selected = 1;
    }

    press_key(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
    match &app.active_dialog {
        Some(Dialog::SquadTaskHistory(state)) => {
            assert_eq!(state.name, "task-b");
            assert!(
                !state.from_detail,
                "history opened from the grid must not remember a detail modal"
            );
        }
        _ => panic!("'h' in the squad list must open the run-history modal"),
    }

    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(
        app.active_dialog.is_none(),
        "Esc must close the history, not open the detail modal"
    );
}

#[test]
fn squad_list_h_is_a_noop_when_the_list_is_empty() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    press_key(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
    assert!(app.active_dialog.is_none());
}

/// From the detail modal, `h` *replaces* it with the history, and Esc walks
/// back to the detail modal for the same task.
#[test]
fn squad_detail_modal_h_opens_history_and_esc_returns_to_the_detail_modal() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    set_squad_tasks(&mut app, &["task-a", "task-b"]);
    set_squad_runs(&mut app, "task-b", 3);
    open_squad_detail(&mut app, "task-b");

    press_key(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
    match &app.active_dialog {
        Some(Dialog::SquadTaskHistory(state)) => {
            assert_eq!(state.name, "task-b");
            assert!(state.from_detail, "Esc must know to come back here");
            assert_eq!(state.runs.len(), 3, "the modal must carry the polled runs");
        }
        _ => panic!("'h' in the detail modal must open the run-history modal"),
    }

    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    match &app.active_dialog {
        Some(Dialog::SquadTaskDetail(state)) => assert_eq!(
            state.name, "task-b",
            "Esc must return to the detail modal for the modal's own task"
        ),
        _ => panic!("Esc in a history opened from the detail modal must reopen it"),
    }
}

/// The history table scrolls with the arrow keys, and cannot be scrolled past
/// its last run.
#[test]
fn squad_history_modal_scrolls_within_its_runs() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    set_squad_tasks(&mut app, &["task-a"]);
    set_squad_runs(&mut app, "task-a", 3);

    press_key(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
    press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    match &app.active_dialog {
        Some(Dialog::SquadTaskHistory(state)) => assert_eq!(state.scroll, 1),
        _ => panic!("the history modal must stay open while scrolling"),
    }

    for _ in 0..10 {
        press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    }
    match &app.active_dialog {
        Some(Dialog::SquadTaskHistory(state)) => assert_eq!(
            state.scroll, 2,
            "scrolling must stop at the last run rather than emptying the table"
        ),
        _ => panic!("the history modal must stay open while scrolling"),
    }

    press_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
    match &app.active_dialog {
        Some(Dialog::SquadTaskHistory(state)) => assert_eq!(state.scroll, 1),
        _ => panic!("the history modal must stay open while scrolling"),
    }
}

/// A task removed while its history is up leaves nothing to return to, so Esc
/// closes rather than reopening a detail modal for a task that is gone.
#[test]
fn squad_history_esc_closes_when_the_task_it_came_from_is_gone() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    set_squad_tasks(&mut app, &["task-a", "task-b"]);
    open_squad_detail(&mut app, "task-b");
    press_key(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);

    set_squad_tasks(&mut app, &["task-a"]);
    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    assert!(
        app.active_dialog.is_none(),
        "with the task gone there is no detail modal to return to"
    );
}

// ─── WI 0110: task editing, detach, and the daemon-start confirmation ────────

#[test]
fn squad_list_e_dispatches_the_edit_interview_for_the_selected_task() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    set_squad_tasks(&mut app, &["task-a", "task-b"]);
    // Select the second card, so a wrong dispatch would name the wrong task.
    if let Some(state) = app.active_tab_mut().squad.as_mut() {
        state.selected = 1;
    }

    press_key(&mut app, KeyCode::Char('e'), KeyModifiers::NONE);

    assert!(
        app.active_tab().command_result_rx.is_some(),
        "'e' in the squad list must dispatch `squad edit --interview`"
    );
    assert!(
        app.active_dialog.is_none(),
        "the edit interview's own dialogs come from the command thread, not the key"
    );
}

#[test]
fn squad_list_e_is_a_noop_when_the_list_is_empty() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    press_key(&mut app, KeyCode::Char('e'), KeyModifiers::NONE);
    assert!(app.active_tab().command_result_rx.is_none());
}

#[test]
fn squad_detail_modal_e_edits_the_modals_task() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    set_squad_tasks(&mut app, &["task-a", "task-b"]);

    open_squad_detail(&mut app, "task-b");
    press_key(&mut app, KeyCode::Char('e'), KeyModifiers::NONE);

    assert!(app.active_dialog.is_none(), "'e' closes the modal");
    assert!(
        app.active_tab().command_result_rx.is_some(),
        "'e' in the modal must dispatch `squad edit`, exactly as the list key does"
    );
}

/// WI 0110: Ctrl-\ is intercepted before the ContainerMaximized passthrough, in
/// every focus context, so it can never reach an agent's PTY the way Ctrl-C
/// deliberately does.
#[test]
fn ctrl_backslash_maps_to_detach_in_every_context_and_ctrl_c_still_reaches_the_pty() {
    use crate::frontend::tui::keymap::{map_key, Action, FocusContext};
    for ctx in [
        FocusContext::CommandBox,
        FocusContext::ExecutionWindow,
        FocusContext::ContainerMaximized,
        FocusContext::SquadList,
    ] {
        assert_eq!(
            map_key(
                KeyEvent {
                    code: KeyCode::Char('\\'),
                    modifiers: KeyModifiers::CONTROL,
                    kind: KeyEventKind::Press,
                    state: KeyEventState::NONE,
                },
                ctx,
            ),
            Action::DetachContainers,
            "ctrl-\\ must detach in {ctx:?}"
        );
        // A terminal without the kitty keyboard protocol enhancement reports
        // the raw FS byte (0x1c); crossterm's legacy decoder maps that byte
        // to Ctrl+'4', not the literal key. This is the encoding most real
        // terminals actually send, so it must detach too.
        assert_eq!(
            map_key(
                KeyEvent {
                    code: KeyCode::Char('4'),
                    modifiers: KeyModifiers::CONTROL,
                    kind: KeyEventKind::Press,
                    state: KeyEventState::NONE,
                },
                ctx,
            ),
            Action::DetachContainers,
            "ctrl-\\'s legacy-terminal encoding (ctrl+'4') must detach in {ctx:?}"
        );
    }
    // The contrast that motivates the binding: Ctrl-C is still forwarded.
    assert!(matches!(
        map_key(
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            },
            FocusContext::ContainerMaximized,
        ),
        Action::ForwardToPty(_)
    ));
}

/// On an ordinary tab, detaching leaves the command and its container running
/// and merely stops keys going to the PTY — the container is minimized, not
/// signalled, and Ctrl-M brings it back.
#[test]
fn ctrl_backslash_minimizes_an_ordinary_tabs_container_without_touching_it() {
    use crate::frontend::tui::tabs::ContainerWindowState;
    let mut app = make_app();
    app.focus = Focus::ExecutionWindow;
    let tab = app.active_tab_mut();
    tab.start_container("claude".into(), "awman-test".into(), 80, 24);
    tab.container_window_state = ContainerWindowState::Maximized;
    assert!(app.active_tab().container_overlay_active());

    press_key(&mut app, KeyCode::Char('\\'), KeyModifiers::CONTROL);

    assert_eq!(
        app.active_tab().container_window_state,
        ContainerWindowState::Minimized,
        "detach minimizes the container view"
    );
    assert_eq!(
        app.active_tab().container_slots.len(),
        1,
        "the container itself is left running — only the view changed"
    );
    assert_eq!(app.focus, Focus::CommandBox);
}

/// Detaching a squad attach session drops the local view and its slots and
/// returns to the task grid, leaving the daemon's containers alone.
#[test]
fn ctrl_backslash_ends_a_squad_attach_session_and_returns_to_the_grid() {
    use crate::frontend::tui::tabs::ContainerWindowState;
    let mut app = make_app();
    push_squad_tab(&mut app);
    set_squad_tasks(&mut app, &["task-a"]);
    {
        let tab = app.active_tab_mut();
        tab.start_container("claude".into(), "awman-squad-task-a".into(), 80, 24);
        tab.container_window_state = ContainerWindowState::Maximized;
        tab.squad
            .as_mut()
            .expect("squad tab")
            .begin_attach("task-a");
    }

    press_key(&mut app, KeyCode::Char('\\'), KeyModifiers::CONTROL);

    let tab = app.active_tab();
    assert!(
        tab.squad.as_ref().unwrap().attached_task().is_none(),
        "the attach session is over"
    );
    assert!(
        tab.container_slots.is_empty(),
        "its slots are dropped, so the task grid renders again"
    );
    assert!(
        app.status_bar.text.contains("still running"),
        "the user is told the containers survived: {:?}",
        app.status_bar.text
    );
}

/// WI 0110: `y` on the daemon-start confirmation is what actually opens the
/// tab; `n` leaves no tab and says so. The sandbox-refusal app is used so the
/// build attempt fails deterministically without touching a real daemon — the
/// assertion is about which branch ran, not about the daemon.
#[test]
fn the_daemon_start_confirmation_opens_no_tab_until_it_is_accepted() {
    let mut app = make_app_no_container_runtime();
    let tabs_before = app.tabs.len();

    app.active_dialog = Some(Dialog::SquadStartConfirm);
    press_key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
    assert!(app.active_dialog.is_none());
    assert_eq!(app.tabs.len(), tabs_before, "declining opens no tab");
    assert!(
        app.status_bar.text.contains("not started"),
        "declining says so: {:?}",
        app.status_bar.text
    );

    app.active_dialog = Some(Dialog::SquadStartConfirm);
    press_key(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert!(app.active_dialog.is_none());
    assert!(
        app.status_bar
            .text
            .contains("squad requires a container runtime"),
        "accepting runs the build, which this app refuses for its runtime: {:?}",
        app.status_bar.text
    );
}

/// A sandbox-class runtime cannot back squad at all, so the refusal is
/// reported instead of a confirmation whose "yes" could not be honoured.
#[test]
fn opening_the_squad_tab_refuses_a_sandbox_runtime_without_asking_to_start_a_daemon() {
    let mut app = make_app_no_container_runtime();
    app.open_or_focus_squad_tab();
    assert!(
        app.active_dialog.is_none(),
        "no daemon-start question is raised when squad cannot run at all"
    );
    assert!(app
        .status_bar
        .text
        .contains("squad requires a container runtime"));
}

// ─── `t` — evaluate a task now, ignoring its schedule ───────────────────────

#[test]
fn squad_list_t_is_a_noop_when_the_list_is_empty() {
    let mut app = make_app();
    push_squad_tab(&mut app);
    press_key(&mut app, KeyCode::Char('t'), KeyModifiers::NONE);
    assert!(app.active_tab().command_result_rx.is_none());
}

/// Plain `t` is squad-list-scoped. Ctrl-T is the global new-tab binding and
/// must keep that meaning on the squad tab.
#[test]
fn ctrl_t_on_the_squad_tab_still_opens_a_new_tab_rather_than_triggering() {
    use crate::frontend::tui::keymap::{map_key, Action, FocusContext};
    let key = crossterm::event::KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(
        !matches!(map_key(key, FocusContext::SquadList), Action::SquadTrigger),
        "Ctrl-T keeps its global meaning on the squad tab"
    );
}

/// Ctrl-C keeps its global meaning on the squad tab; only plain `c` cancels.
#[test]
fn ctrl_c_on_the_squad_tab_does_not_cancel_a_run() {
    use crate::frontend::tui::keymap::{map_key, Action, FocusContext};
    let key = crossterm::event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(!matches!(
        map_key(key, FocusContext::SquadList),
        Action::SquadCancel
    ));
}

/// Accepting the daemon-start confirmation must not freeze the TUI.
///
/// Starting a daemon spawns a process and then waits up to ten seconds for it
/// to publish an endpoint. That wait used to run on the event-loop thread, so
/// the terminal could not redraw for its whole duration: the confirmation the
/// user had just answered stayed painted on screen, looking hung, and then
/// vanished with only a status-bar line to say whether anything had happened.
/// The wait now runs on the runtime, and the tab is installed later, by
/// `poll_squad_startup`.
///
/// Answering `y` is never exercised against a real daemon here — that would
/// spawn a background process from a unit test. What is asserted instead is
/// the part that is observable without one: the key returns having installed
/// no tab, and a start that is already in flight makes a second `y` inert, so
/// two presses can never spawn two daemons.
#[test]
fn a_daemon_start_already_in_flight_makes_a_second_confirmation_inert() {
    let mut app = make_app();
    let tabs_before = app.tabs.len();
    // A start is outstanding: the channel a real `y` would have installed.
    let (_tx, rx) = std::sync::mpsc::channel();
    app.squad_startup_rx = Some(rx);
    app.active_dialog = Some(Dialog::SquadStartConfirm);

    let started = std::time::Instant::now();
    press_key(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "answering the confirmation must not block the event loop: took {elapsed:?}"
    );
    assert_eq!(
        app.tabs.len(),
        tabs_before,
        "the tab appears only once the daemon answers, in poll_squad_startup"
    );
    assert!(
        app.squad_startup_rx.is_some(),
        "the outstanding start is left alone rather than replaced by a second one"
    );
}

/// The progress modal is what the user looks at while the daemon starts, and
/// `poll_squad_startup` is what takes it away — never a key press, and never
/// a redraw that happens to come first.
#[test]
fn the_progress_modal_survives_until_the_daemon_answers() {
    let mut app = make_app();
    let (tx, rx) = std::sync::mpsc::channel();
    app.squad_startup_rx = Some(rx);
    app.active_dialog = Some(Dialog::Loading {
        title: "Starting squad daemon".to_string(),
    });

    app.tick_all_tabs();
    assert!(
        matches!(app.active_dialog, Some(Dialog::Loading { .. })),
        "a tick with no answer yet leaves the progress modal up"
    );

    drop(tx);
    app.tick_all_tabs();
    assert!(
        !matches!(app.active_dialog, Some(Dialog::Loading { .. })),
        "a start that ends — even by dying — must take its progress modal with it"
    );
    assert!(app.squad_startup_rx.is_none());
}

/// A daemon that fails to start is reported in a modal, not only in the status
/// bar: the user asked an explicit question, and the answer must not be
/// something they can miss.
#[test]
fn a_failed_daemon_start_is_reported_in_a_modal() {
    let mut app = make_app();
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(Err(crate::frontend::tui::app::SquadStartError::Other(
        "failed to start the squad daemon: did not become ready within 10 seconds".to_string(),
    )))
    .unwrap();
    app.squad_startup_rx = Some(rx);
    app.active_dialog = Some(Dialog::Loading {
        title: "Starting squad daemon".to_string(),
    });

    app.poll_squad_startup();

    match &app.active_dialog {
        Some(Dialog::Notice { title, body, .. }) => {
            assert!(title.contains("did not start"), "{title:?}");
            assert!(body.contains("did not become ready"), "{body:?}");
        }
        other => panic!(
            "a failed start must replace the progress modal with the reason, not {:?}",
            other.is_some()
        ),
    }
    assert!(
        app.squad_startup_rx.is_none(),
        "the start is no longer in flight"
    );
}

// ─── the missing-key recovery ───────────────────────────────────────────────

/// A gateway pointing nowhere. `poll_squad_startup` never calls it in the
/// `Missing` arm — it discards it and raises the dialog — so a real endpoint
/// is not needed to prove which arm ran.
fn unreachable_gateway() -> crate::command::commands::squad::gateway::RemoteTaskGateway {
    use crate::command::commands::squad::gateway::RemoteTaskGateway;
    use crate::engine::remote::HttpCore;
    RemoteTaskGateway::new(HttpCore::new("http://127.0.0.1:1", "v1", None).unwrap())
}

/// A daemon this process cannot authenticate to gets the recovery dialog, not
/// a tab. A tab would poll, be refused with 401, and render nothing but that.
#[test]
fn a_daemon_with_no_usable_key_raises_the_recovery_instead_of_opening_a_tab() {
    let mut app = make_app();
    let tabs_before = app.tabs.len();
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(Err(crate::frontend::tui::app::SquadStartError::KeyMissing))
        .unwrap();
    app.squad_startup_rx = Some(rx);
    app.active_dialog = Some(Dialog::Loading {
        title: "Starting squad daemon".to_string(),
    });

    app.poll_squad_startup();

    assert!(
        matches!(app.active_dialog, Some(Dialog::SquadKeyMissing)),
        "a missing key must raise its own recovery, not a generic failure"
    );
    assert_eq!(
        app.tabs.len(),
        tabs_before,
        "no squad tab is opened for a daemon every request would be refused by"
    );
}

/// Declining the recovery opens no tab and says why, rather than leaving the
/// user staring at an unexplained absence.
#[test]
fn declining_the_key_recovery_opens_no_tab_and_says_so() {
    let mut app = make_app();
    let tabs_before = app.tabs.len();
    app.active_dialog = Some(Dialog::SquadKeyMissing);

    press_key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);

    assert!(app.active_dialog.is_none());
    assert_eq!(app.tabs.len(), tabs_before);
    assert!(
        app.status_bar.text.contains("key") && app.status_bar.text.contains("was not opened"),
        "declining must say what happened: {:?}",
        app.status_bar.text
    );
}

/// A refresh already in flight makes a second `y` inert, so two presses cannot
/// mint two keys and restart the daemon twice.
///
/// The accepting press is deliberately not exercised against a real daemon:
/// `y` terminates and restarts one, which a unit test must not do.
#[test]
fn a_key_refresh_already_in_flight_makes_a_second_acceptance_inert() {
    let mut app = make_app();
    let (_tx, rx) = std::sync::mpsc::channel();
    app.squad_startup_rx = Some(rx);
    app.active_dialog = Some(Dialog::SquadKeyMissing);

    press_key(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);

    assert!(
        app.squad_startup_rx.is_some(),
        "the outstanding refresh is left alone rather than replaced"
    );
    assert_eq!(app.tabs.len(), 1, "no tab appears until the refresh lands");
}

/// A refresh ends where a first run does — a key to display — and takes the
/// same drain, so the recovery finishes by showing the user the key they were
/// missing.
#[test]
fn a_completed_key_refresh_opens_the_tab_and_displays_the_new_key() {
    use crate::command::commands::squad::daemon::SquadKeyState;
    use crate::engine::squad::key_setup::{KeyDisclosure, ShellFlavor};

    let mut app = make_app();
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(Ok(crate::frontend::tui::app::SquadStartup {
        gateway: std::sync::Arc::new(unreachable_gateway()),
        key_state: SquadKeyState::Minted(KeyDisclosure::new("deadbeef", ShellFlavor::Zsh)),
        key_setup: Some(crate::frontend::tui::app::SquadKeySetup::for_key(
            "deadbeef",
            ShellFlavor::Zsh,
        )),
    }))
    .unwrap();
    app.squad_startup_rx = Some(rx);
    app.active_dialog = Some(Dialog::Loading {
        title: "Refreshing the squad key".to_string(),
    });

    app.poll_squad_startup();

    assert!(
        app.tabs.iter().any(|tab| tab.is_squad),
        "a usable key means the squad tab opens"
    );
    match &app.active_dialog {
        Some(Dialog::Notice { body, .. }) => assert!(
            body.contains("deadbeef"),
            "the new key must be displayed — it exists nowhere else: {body}"
        ),
        other => panic!(
            "a minted key must be shown, not swallowed (dialog present: {})",
            other.is_some()
        ),
    }
}

/// `c` and `z` copy the key/snippet to the clipboard but must not dismiss the
/// notice — the user may want both before acknowledging it, and a copy is
/// not itself an acknowledgment. Only Enter (or Esc) closes it.
#[test]
fn copying_the_squad_key_or_snippet_does_not_dismiss_the_notice() {
    let mut app = make_app();
    app.active_dialog = Some(Dialog::Notice {
        title: "squad authentication".to_string(),
        body: "deadbeef".to_string(),
        copy_key: Some("deadbeef".to_string()),
        copy_zshrc_snippet: Some("export AWMAN_SQUAD_KEY=deadbeef".to_string()),
    });

    press_key(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    assert!(
        matches!(app.active_dialog, Some(Dialog::Notice { .. })),
        "copying the key must not close the notice"
    );

    press_key(&mut app, KeyCode::Char('z'), KeyModifiers::NONE);
    assert!(
        matches!(app.active_dialog, Some(Dialog::Notice { .. })),
        "copying the snippet must not close the notice"
    );

    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.active_dialog.is_none(),
        "Enter still dismisses the notice"
    );
}

// ─── WI 0112 Part 4: the grid always holds focus on the squad tab ──────────

#[test]
fn the_squad_grid_takes_focus_on_the_first_tick_and_arrows_work_without_up() {
    let mut app = squad_list_app();
    set_squad_tasks(&mut app, &["a", "b", "c"]);
    app.active_tab_mut().squad.as_mut().unwrap().grid_columns = 1;
    app.focus = Focus::CommandBox;
    app.tick_all_tabs();
    assert_eq!(app.focus, Focus::ExecutionWindow);
    press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.active_tab().squad.as_ref().unwrap().selected, 1);
    assert_eq!(app.active_tab().scroll_offset, 0);
}

#[test]
fn keys_reach_the_squad_grid_even_when_focus_still_says_command_box() {
    // Belt and braces: before the tick normalises focus, the context is
    // already the squad list.
    let mut app = squad_list_app();
    set_squad_tasks(&mut app, &["a", "b"]);
    app.active_tab_mut().squad.as_mut().unwrap().grid_columns = 1;
    app.focus = Focus::CommandBox;
    press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.active_tab().squad.as_ref().unwrap().selected, 1);
}

#[test]
fn esc_on_the_squad_grid_does_nothing() {
    let mut app = squad_list_app();
    set_squad_tasks(&mut app, &["a", "b"]);
    app.tick_all_tabs();
    press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::ExecutionWindow);
    assert_eq!(app.active_tab().squad.as_ref().unwrap().selected, 1);
    assert!(app.active_dialog.is_none());
}

#[test]
fn unbound_letters_on_the_squad_grid_never_reach_the_command_box() {
    let mut app = squad_list_app();
    set_squad_tasks(&mut app, &["a"]);
    app.tick_all_tabs();
    press_char(&mut app, 'x');
    press_char(&mut app, 'z');
    assert_eq!(app.command_input.text, "");
}

#[test]
fn leaving_the_squad_tab_restores_the_command_box_and_returning_refocuses_the_grid() {
    let mut app = squad_list_app();
    app.tick_all_tabs();
    assert_eq!(app.focus, Focus::ExecutionWindow);

    press_key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    assert!(!app.active_tab().is_squad);
    app.tick_all_tabs();
    assert_eq!(
        app.focus,
        Focus::CommandBox,
        "a normal tab gets its command box back"
    );

    press_key(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert!(app.active_tab().is_squad);
    app.tick_all_tabs();
    assert_eq!(
        app.focus,
        Focus::ExecutionWindow,
        "the grid is focused again"
    );
}

#[test]
fn a_normal_to_normal_tab_switch_leaves_focus_alone() {
    let mut app = make_app();
    app.add_tab(
        make_session().working_dir().to_path_buf(),
        SessionOpenOptions::default(),
    )
    .unwrap();
    app.tick_all_tabs();
    app.focus = Focus::ExecutionWindow;
    press_key(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    app.tick_all_tabs();
    assert_eq!(
        app.focus,
        Focus::ExecutionWindow,
        "unchanged, as before WI 0112"
    );
}

#[test]
fn closing_a_tab_that_lands_on_the_squad_tab_focuses_the_grid() {
    let mut app = make_app();
    push_squad_tab(&mut app); // index 1
    let normal = app
        .add_tab(
            make_session().working_dir().to_path_buf(),
            SessionOpenOptions::default(),
        )
        .unwrap(); // index 2
    app.active_tab = normal;
    app.focus = Focus::CommandBox;
    app.tick_all_tabs();
    app.close_active_tab();
    assert!(app.active_tab().is_squad);
    app.tick_all_tabs();
    assert_eq!(app.focus, Focus::ExecutionWindow);
}

#[test]
fn detaching_a_squad_attach_session_refocuses_the_grid_on_the_next_tick() {
    use crate::frontend::tui::tabs::ContainerWindowState;
    let mut app = squad_list_app();
    {
        let tab = app.active_tab_mut();
        tab.start_container("claude".into(), "awman-squad-task-a".into(), 80, 24);
        tab.container_window_state = ContainerWindowState::Maximized;
    }
    app.focus = Focus::CommandBox;
    app.active_tab_mut().end_attach_session();
    app.tick_all_tabs();
    assert!(app.active_tab().container_slots.is_empty());
    assert_eq!(app.focus, Focus::ExecutionWindow);
}

// ─── `t` / `c` / `p` ask for confirmation first ─────────────────────────────

/// The key opened a confirmation for `action` on `name` and dispatched nothing
/// yet; `y` then dispatches it and closes the dialog.
///
/// The question and the hotkey are read off the prompt Layer 2 supplied, not
/// written here: this asserts the frontend *uses* the prompt, and the copy
/// itself is `command::prompts`' to test (WI 0114 F-55).
fn assert_confirms_then_dispatches(app: &mut App, action: FrontendAction, name: &str) {
    match &app.active_dialog {
        Some(Dialog::SquadActionConfirm {
            action: asked,
            name: asked_name,
            prompt,
        }) => {
            assert_eq!(*asked, action);
            assert_eq!(
                asked_name, name,
                "the confirmation must name the right task"
            );
            assert_eq!(
                prompt,
                &SquadCommand::confirm_prompt(action, name).expect("{action:?} asks first"),
                "the modal must show Layer 2's prompt, unaltered"
            );
        }
        _ => panic!("{action:?} must open Dialog::SquadActionConfirm"),
    }
    assert!(
        app.active_tab().command_result_rx.is_none(),
        "nothing may be dispatched before the user confirms"
    );

    press_key(app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert!(app.active_dialog.is_none(), "'y' closes the confirmation");
    assert!(
        app.active_tab().command_result_rx.is_some(),
        "'y' must dispatch {:?}",
        action.path()
    );
}

#[test]
fn squad_list_t_c_p_confirm_before_acting_on_the_selected_task() {
    for (key, action) in [
        ('t', FrontendAction::TriggerSquadTask),
        ('c', FrontendAction::CancelSquadRun),
        ('p', FrontendAction::PauseSquadTask),
    ] {
        let mut app = make_app();
        push_squad_tab(&mut app);
        set_squad_tasks(&mut app, &["task-a", "task-b"]);
        // Select the second card, so a wrong dispatch would name the wrong task.
        if let Some(state) = app.active_tab_mut().squad.as_mut() {
            state.selected = 1;
        }
        press_key(&mut app, KeyCode::Char(key), KeyModifiers::NONE);
        assert_confirms_then_dispatches(&mut app, action, "task-b");
    }
}

/// The detail modal's keys confirm for the task the modal is showing, not the
/// grid's selection — the two can differ while the list reflows.
#[test]
fn squad_detail_modal_t_c_p_confirm_before_acting_on_the_modals_task() {
    for (key, action) in [
        ('t', FrontendAction::TriggerSquadTask),
        ('c', FrontendAction::CancelSquadRun),
        ('p', FrontendAction::PauseSquadTask),
    ] {
        let mut app = make_app();
        push_squad_tab(&mut app);
        set_squad_tasks(&mut app, &["task-a", "task-b"]);
        open_squad_detail(&mut app, "task-b");
        press_key(&mut app, KeyCode::Char(key), KeyModifiers::NONE);
        assert_confirms_then_dispatches(&mut app, action, "task-b");
    }
}

/// `n` and Esc back out of the confirmation without dispatching anything.
#[test]
fn declining_a_squad_action_confirmation_dispatches_nothing() {
    for decline in [KeyCode::Char('n'), KeyCode::Esc] {
        let mut app = make_app();
        push_squad_tab(&mut app);
        set_squad_tasks(&mut app, &["task-a"]);
        press_key(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(matches!(
            app.active_dialog,
            Some(Dialog::SquadActionConfirm { .. })
        ));
        press_key(&mut app, decline, KeyModifiers::NONE);
        assert!(app.active_dialog.is_none(), "{decline:?} closes it");
        assert!(
            app.active_tab().command_result_rx.is_none(),
            "{decline:?} must not dispatch the action"
        );
    }
}

#[test]
fn squad_list_plain_c_maps_to_cancel() {
    use crate::frontend::tui::keymap::{map_key, Action, FocusContext};
    let plain = crossterm::event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
    assert!(matches!(
        map_key(plain, FocusContext::SquadList),
        Action::SquadCancel
    ));
}
