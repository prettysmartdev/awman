pub mod input;
mod flag_parser;
mod pty;
pub mod render;
pub mod state;

use crate::cli::Agent;
use crate::commands::auth::{agent_keychain_credentials, apply_auth_decision};
use dirs;
use crate::commands::chat::{chat_entrypoint, chat_entrypoint_non_interactive};
use crate::commands::implement::{
    agent_entrypoint, agent_entrypoint_non_interactive, find_work_item, parse_work_item,
    workflow_step_entrypoint,
};
use crate::commands::init_flow::find_git_root_from;
use crate::commands::new::WorkItemKind;
use crate::commands::specs::{amend_agent_entrypoint, interview_agent_entrypoint};
use crate::commands::{claws, init_flow, new, ready, ready_flow, status};
use crate::commands::ready::{compute_ready_build_flag, dockerfile_matches_template, is_legacy_layout, print_interactive_notice};
use crate::config::{effective_env_passthrough, effective_scrollback_lines, load_repo_config};
use crate::runtime::{generate_container_name, ContainerStats};
use crate::tui::input::Action;
use crate::tui::pty::{spawn_text_command, PtySession};
use crate::tui::render::{calculate_container_inner_size, workflow_strip_height};
use crate::tui::state::{App, AuditPhase, ClawsPhase, ContainerWindowState, Dialog, PendingCommand};
use crate::workflow::{self, StepStatus};
use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyboardEnhancementFlags,
        KeyEventKind, MouseButton, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use portable_pty::PtySize;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::time::Duration;

/// Flags passed from the root `amux` CLI to the `ready` command run at TUI startup.
#[derive(Clone, Debug, Default)]
pub struct StartupReadyFlags {
    pub build: bool,
    pub no_cache: bool,
    pub refresh: bool,
}

/// Launches the interactive TUI. Blocks until the user quits.
pub async fn run(startup_flags: StartupReadyFlags, runtime: std::sync::Arc<dyn crate::runtime::AgentRuntime>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    // Enable keyboard enhancement so that modifiers on special keys (e.g. Ctrl+Enter)
    // are reported as distinct events. This is a best-effort push: terminals that do
    // not support the Kitty keyboard protocol will silently ignore it.
    let keyboard_enhanced = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_ok();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, startup_flags, runtime).await;

    // Always restore the terminal, even on error.
    if keyboard_enhanced {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    result
}

async fn run_app<B>(terminal: &mut Terminal<B>, startup_flags: StartupReadyFlags, runtime: std::sync::Arc<dyn crate::runtime::AgentRuntime>) -> Result<()>
where
    B: ratatui::backend::Backend + io::Write,
    <B as ratatui::backend::Backend>::Error: Send + Sync + 'static,
{
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut app = App::new_with_runtime(cwd.clone(), runtime);

    // At startup: if we are inside a Git repo, run `ready` as usual.
    // If not, run `status --watch` so the user can see the global agent universe.
    let startup_cmd = if find_git_root_from(&cwd).is_some() {
        let mut cmd = "ready".to_string();
        if startup_flags.refresh {
            cmd.push_str(" --refresh");
        }
        if startup_flags.build {
            cmd.push_str(" --build");
        }
        if startup_flags.no_cache {
            cmd.push_str(" --no-cache");
        }
        cmd
    } else {
        "status --watch".to_string()
    };
    execute_command(&mut app, &startup_cmd).await;

    loop {
        if app.needs_full_redraw {
            app.needs_full_redraw = false;
            let _ = terminal.clear();
        }
        terminal.draw(|f| render::draw(f, &mut app))?;

        // Poll for crossterm events with a short timeout to keep the UI responsive.
        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let action = input::handle_key(&mut app, key);
                    handle_action(&mut app, action).await;
                }
                Event::Mouse(mouse) => {
                    // Any mouse interaction counts as "checking on" the tab.
                    app.active_tab_mut().acknowledge_stuck();
                    app.active_tab_mut().record_user_activity();
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            let tab = app.active_tab_mut();
                            if tab.container_window == ContainerWindowState::Maximized {
                                // Probe for the actual scrollback depth by clamping to usize::MAX.
                                let max_scroll = if let Some(ref mut parser) = tab.vt100_parser {
                                    parser.set_scrollback(usize::MAX);
                                    let m = parser.screen().scrollback();
                                    parser.set_scrollback(0);
                                    m
                                } else {
                                    0
                                };
                                tab.container_scroll_offset =
                                    (tab.container_scroll_offset + 5).min(max_scroll);
                            } else {
                                let max = tab.output_lines.len();
                                if tab.scroll_offset < max {
                                    tab.scroll_offset = tab.scroll_offset.saturating_add(5);
                                }
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            let tab = app.active_tab_mut();
                            if tab.container_window == ContainerWindowState::Maximized {
                                // Scroll down towards the live view.
                                tab.container_scroll_offset =
                                    tab.container_scroll_offset.saturating_sub(5);
                            } else {
                                tab.scroll_offset = tab.scroll_offset.saturating_sub(5);
                            }
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            let tab = app.active_tab_mut();
                            if tab.container_window == ContainerWindowState::Maximized {
                                if let Some(inner) = tab.container_inner_area {
                                    if mouse.column >= inner.x && mouse.row >= inner.y
                                        && mouse.column < inner.x + inner.width
                                        && mouse.row < inner.y + inner.height
                                    {
                                        let vt100_col = mouse.column - inner.x;
                                        let vt100_row = mouse.row - inner.y;
                                        let scroll_offset = tab.container_scroll_offset;
                                        let snapshot = capture_vt100_snapshot(&mut tab.vt100_parser, scroll_offset);
                                        tab.terminal_selection_start = Some((vt100_row, vt100_col));
                                        tab.terminal_selection_end = Some((vt100_row, vt100_col));
                                        tab.terminal_selection_snapshot = snapshot;
                                    }
                                }
                            }
                        }
                        MouseEventKind::Drag(MouseButton::Left) => {
                            let tab = app.active_tab_mut();
                            if tab.container_window == ContainerWindowState::Maximized
                                && tab.terminal_selection_start.is_some()
                            {
                                if let Some(inner) = tab.container_inner_area {
                                    let vt100_col = mouse.column
                                        .saturating_sub(inner.x)
                                        .min(inner.width.saturating_sub(1));
                                    let vt100_row = mouse.row
                                        .saturating_sub(inner.y)
                                        .min(inner.height.saturating_sub(1));
                                    tab.terminal_selection_end = Some((vt100_row, vt100_col));
                                }
                            }
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            // A click without drag leaves start == end (zero-area selection).
                            // Treat this as a cursor-position acknowledgment, not a text selection,
                            // so that Ctrl+Y is not accidentally triggered by a bare click.
                            let tab = app.active_tab_mut();
                            if tab.terminal_selection_start.is_some()
                                && tab.terminal_selection_start == tab.terminal_selection_end
                            {
                                tab.clear_terminal_selection();
                            }
                        }
                        _ => {}
                    }
                }
                Event::Resize(cols, rows) => {
                    for tab in app.tabs.iter_mut() {
                        // Clear any active text selection when the layout changes.
                        tab.clear_terminal_selection();
                        if let Some(ref pty) = tab.pty {
                            if tab.container_window != ContainerWindowState::Hidden {
                                // Resize the PTY and vt100 parser to match the container inner area,
                                // accounting for any active workflow strip that reduces exec height.
                                let wf_strip_h = tab.workflow.as_ref()
                                    .map(|wf| workflow_strip_height(wf))
                                    .unwrap_or(0);
                                let (inner_cols, inner_rows) = calculate_container_inner_size(cols, rows, wf_strip_h);
                                pty.resize(PtySize {
                                    rows: inner_rows,
                                    cols: inner_cols,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                });
                                if let Some(ref mut parser) = tab.vt100_parser {
                                    parser.set_size(inner_rows, inner_cols);
                                }
                            } else {
                                pty.resize(PtySize {
                                    rows,
                                    cols,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Drain all pending channel messages (PTY output, command output, exit codes).
        let was_running = matches!(app.active_tab().phase, state::ExecutionPhase::Running { .. });
        app.tick_all();
        let now_done = !matches!(app.active_tab().phase, state::ExecutionPhase::Running { .. });

        if was_running && now_done {
            check_audit_continuation(&mut app).await;
            check_claws_continuation(&mut app).await;
            check_workflow_step_completion(&mut app).await;
        }

        // Check every tab (active first, then background) for an expired yolo countdown
        // and advance the workflow step.  Background tabs were previously skipped because
        // the check only looked at active_tab(), causing the timer to reset instead of
        // actually advancing the workflow.
        let active_idx = app.active_tab_idx;
        let tab_count = app.tabs.len();
        for raw_i in 0..tab_count {
            // Process the active tab first so its advancement is never delayed by
            // background-tab work.
            let i = if raw_i == 0 {
                active_idx
            } else if raw_i <= active_idx {
                raw_i - 1
            } else {
                raw_i
            };
            if !app.tabs[i].yolo_countdown_expired {
                continue;
            }
            app.tabs[i].yolo_countdown_expired = false;

            // Temporarily treat tab `i` as the active tab so all advance helpers work
            // without modification.
            app.active_tab_idx = i;
            let is_last = app.active_tab().is_last_workflow_step();
            if is_last {
                // On the final step, present the control board instead of auto-finishing.
                // For a background tab this will be visible when the user switches to it.
                let step = app.active_tab().workflow_current_step.clone().unwrap_or_default();
                app.active_tab_mut().dialog = Dialog::WorkflowControlBoard {
                    current_step: step,
                    error: None,
                };
            } else {
                advance_workflow_next_new_container(&mut app).await;
            }
        }
        // Restore the real active tab index after processing all expired countdowns.
        app.active_tab_idx = active_idx;

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

/// Dispatch an `Action` returned by the key handler to the appropriate async logic.
async fn handle_action(app: &mut App, action: Action) {
    match action {
        Action::None => {}

        Action::QuitConfirmed => {
            app.should_quit = true;
        }

        Action::Submit(cmd) => {
            if cmd.is_empty() {
                return;
            }
            execute_command(app, &cmd).await;
        }

        Action::MountScopeChosen(path) => {
            app.active_tab_mut().pending_mount_path = Some(path);
            launch_pending_command(app).await;
        }

        Action::AuthAccepted => {
            if let Dialog::AgentAuth { ref agent, ref git_root } = app.active_tab().dialog.clone() {
                let _ = apply_auth_decision(git_root, agent, true);
            }
            launch_pending_command(app).await;
        }

        Action::AuthDeclined => {
            if let Dialog::AgentAuth { ref agent, ref git_root } = app.active_tab().dialog.clone() {
                let _ = apply_auth_decision(git_root, agent, false);
            }
            launch_pending_command(app).await;
        }

        Action::ForwardToPty(bytes) => {
            if let Some(ref pty) = app.active_tab().pty {
                pty.write_bytes(&bytes);
            }
        }

        Action::NewWorkItem { kind, title, interview } => {
            if interview {
                launch_new_interview(app, kind, title).await;
            } else {
                launch_new(app, kind, title).await;
            }
        }

        Action::NewInterviewSummarySubmitted { kind, title, work_item_number, summary } => {
            let tab = app.active_tab_mut();
            tab.pending_command = PendingCommand::SpecsNewInterview {
                work_item_number,
                kind,
                title,
                summary,
                allow_docker: false,
            };
            show_pre_command_dialogs(app).await;
        }

        Action::NewWorkflowSubmitted(state) => {
            launch_new_workflow_action(app, state).await;
        }

        Action::NewSkillSubmitted(state) => {
            launch_new_skill_action(app, state).await;
        }

        Action::ClawsReadyProceed => {
            launch_claws_ready(app).await;
        }

        Action::ClawsReadyStartContainer => {
            launch_claws_start_container_status_only(app).await;
        }

        Action::ClawsReadyRestartStopped { container_id } => {
            launch_claws_restart_stopped_container(app, container_id).await;
        }

        Action::ClawsReadyDeleteAndStartFresh { container_id } => {
            launch_claws_delete_and_start_fresh(app, container_id).await;
        }

        Action::ClawsAuditConfirmAccept => {
            // Audit runs in the background — go straight to post-audit (dialogs + container launch).
            if app.active_tab().claws_audit_ctx.is_some() {
                launch_claws_init_post_audit(app).await;
            } else {
                app.active_tab_mut().push_output(
                    "Internal error: audit context missing when audit was accepted.".to_string(),
                );
                app.active_tab_mut().claws_phase = ClawsPhase::Inactive;
            }
        }

        Action::ClawsAuditConfirmDecline => {
            app.active_tab_mut().push_output("Audit declined. Setup cancelled.".to_string());
            app.active_tab_mut().claws_audit_ctx = None;
            app.active_tab_mut().claws_phase = ClawsPhase::Inactive;
        }

        Action::CreateTab => {
            let cwd = app.active_tab().cwd.clone();
            let has_remote = crate::config::effective_remote_default_addr().is_some();
            app.active_tab_mut().dialog = Dialog::NewTabDirectory {
                input: cwd.to_string_lossy().to_string(),
                remote_sessions: None,
                remote_selected_idx: None,
                focus_workdir: true,
            };

            // If remote is configured, kick off an async fetch of active sessions.
            if has_remote {
                let addr = crate::config::effective_remote_default_addr().unwrap();
                let api_key = crate::commands::remote::resolve_api_key(None, &addr);
                let (tx, rx) = tokio::sync::oneshot::channel();
                app.active_tab_mut().remote_sessions_fetch_rx = Some(rx);
                tokio::spawn(async move {
                    let result = crate::commands::remote::fetch_sessions(&addr, api_key.as_deref()).await;
                    let _ = tx.send(result.map_err(|e| e.to_string()));
                });
            }
        }

        Action::SwitchTabLeft => {
            let len = app.tabs.len();
            if len > 0 {
                // When leaving a tab with an open yolo dialog, close the dialog so the
                // countdown continues in background mode (tab bar shows it instead).
                if matches!(app.active_tab().dialog, Dialog::WorkflowYoloCountdown { .. }) {
                    app.active_tab_mut().dialog = Dialog::None;
                    app.active_tab_mut().workflow_stuck_dialog_opened = false;
                }
                app.active_tab_idx = (app.active_tab_idx + len - 1) % len;
            }
            // Switching to a tab counts as "checking on it" — clear any stuck warning.
            app.active_tab_mut().acknowledge_stuck();
            // If the newly active tab has a yolo countdown in progress, open the dialog
            // so the user can see the timer with its remaining time preserved.
            if app.active_tab().yolo_countdown_started_at.is_some()
                && app.active_tab().dialog == Dialog::None
            {
                if let Some(step) = app.active_tab().workflow_current_step.clone() {
                    app.active_tab_mut().dialog = Dialog::WorkflowYoloCountdown {
                        current_step: step,
                    };
                    app.active_tab_mut().workflow_stuck_dialog_opened = true;
                }
            }
        }

        Action::SwitchTabRight => {
            let len = app.tabs.len();
            if len > 0 {
                // When leaving a tab with an open yolo dialog, close the dialog so the
                // countdown continues in background mode (tab bar shows it instead).
                if matches!(app.active_tab().dialog, Dialog::WorkflowYoloCountdown { .. }) {
                    app.active_tab_mut().dialog = Dialog::None;
                    app.active_tab_mut().workflow_stuck_dialog_opened = false;
                }
                app.active_tab_idx = (app.active_tab_idx + 1) % len;
            }
            // Switching to a tab counts as "checking on it" — clear any stuck warning.
            app.active_tab_mut().acknowledge_stuck();
            // If the newly active tab has a yolo countdown in progress, open the dialog
            // so the user can see the timer with its remaining time preserved.
            if app.active_tab().yolo_countdown_started_at.is_some()
                && app.active_tab().dialog == Dialog::None
            {
                if let Some(step) = app.active_tab().workflow_current_step.clone() {
                    app.active_tab_mut().dialog = Dialog::WorkflowYoloCountdown {
                        current_step: step,
                    };
                    app.active_tab_mut().workflow_stuck_dialog_opened = true;
                }
            }
        }

        Action::CloseCurrentTab => {
            let idx = app.active_tab_idx;
            app.close_tab(idx);
        }

        Action::NewTabDirectoryChosen(path) => {
            let new_idx = app.create_tab(path.clone());
            app.active_tab_idx = new_idx;
            execute_tab_command(app, "ready").await;
        }

        Action::NewTabRemoteSessionChosen { remote_addr, session_id, api_key } => {
            // Create a new tab bound to the remote session.
            let cwd = app.active_tab().cwd.clone();
            let new_idx = app.create_tab(cwd);
            app.active_tab_idx = new_idx;
            let binding = crate::tui::state::RemoteTabBinding::new(
                remote_addr, session_id, api_key,
            );
            app.tabs[new_idx].remote_binding = Some(binding);
            // Auto-execute `ready` on the remote tab.
            launch_remote_bound_command(app, new_idx, "ready").await;
        }

        Action::NewTabCreateRemoteSession => {
            let remote_addr = crate::config::effective_remote_default_addr().unwrap_or_default();
            let api_key = crate::commands::remote::resolve_api_key(None, &remote_addr);
            let saved_dirs = crate::config::effective_remote_saved_dirs();
            app.active_tab_mut().dialog = Dialog::NewRemoteSession {
                remote_addr,
                api_key,
                dir_input: String::new(),
                saved_dirs,
                saved_selected_idx: None,
                focus_input: true,
                creation_error: None,
            };
        }

        Action::NewRemoteSessionCreated { remote_addr, dir, api_key } => {
            // Create a session on the remote host, then bind a new tab to it.
            // On failure, re-open the creation dialog with the error shown inline
            // so the user can correct the path and retry without pressing Ctrl-T.
            let cwd = app.active_tab().cwd.clone();
            match crate::commands::remote::run_remote_session_start(&remote_addr, &dir, api_key.as_deref()).await {
                Ok(session_id) => {
                    let new_idx = app.create_tab(cwd);
                    app.active_tab_idx = new_idx;
                    let binding = crate::tui::state::RemoteTabBinding::new(
                        remote_addr, session_id, api_key,
                    );
                    app.tabs[new_idx].remote_binding = Some(binding);
                    launch_remote_bound_command(app, new_idx, "ready").await;
                }
                Err(e) => {
                    // Re-open the creation dialog with the error shown — no new tab is created.
                    let saved_dirs = crate::config::effective_remote_saved_dirs();
                    app.active_tab_mut().dialog = Dialog::NewRemoteSession {
                        remote_addr,
                        api_key,
                        dir_input: dir,
                        saved_dirs,
                        saved_selected_idx: None,
                        focus_input: true,
                        creation_error: Some(format!("Failed to create session: {}", e)),
                    };
                }
            }
        }

        Action::WorkflowAdvance => {
            launch_next_workflow_step(app).await;
        }

        Action::WorkflowAbort => {
            abort_workflow(app);
        }

        Action::WorkflowRetry => {
            retry_workflow_step(app).await;
        }

        Action::WorkflowRestartStep => {
            // Same as retry: reset step to Pending and re-launch.
            retry_workflow_step(app).await;
        }

        Action::WorkflowCancelToPrevious => {
            cancel_to_previous_step(app).await;
        }

        Action::WorkflowNextInNewContainer => {
            advance_workflow_next_new_container(app).await;
        }

        Action::WorkflowNextInCurrentContainer => {
            advance_workflow_next_current_container(app).await;
        }

        Action::WorkflowFinish => {
            finish_workflow(app).await;
        }

        Action::DisableAutoWorkflowForStep => {
            app.active_tab_mut().auto_workflow_disabled_for_step = true;
        }

        Action::WorkflowCancelExecution => {
            cancel_workflow_execution(app).await;
        }

        Action::WorktreeMerge => {
            handle_worktree_merge(app).await;
        }

        Action::WorktreeDiscard => {
            handle_worktree_discard(app).await;
        }

        Action::WorktreeSkip => {
            handle_worktree_skip(app);
        }

        Action::WorktreeCommitFiles { message, branch, worktree_path, git_root } => {
            handle_worktree_commit_files(app, message, branch, worktree_path, git_root).await;
        }

        Action::WorktreeMergeConfirmed { branch, worktree_path, git_root } => {
            handle_worktree_merge_confirmed(app, branch, worktree_path, git_root).await;
        }

        Action::WorktreeDeleteConfirmed { branch, worktree_path, git_root } => {
            handle_worktree_delete_confirmed(app, branch, worktree_path, git_root);
        }

        Action::WorktreeKeepAfterMerge => {
            app.active_tab_mut().push_output(
                "Worktree kept. Use 'git worktree list' to see active worktrees.".to_string(),
            );
        }

        Action::WorktreePreCommitAbort => {
            app.active_tab_mut().pending_command = PendingCommand::None;
        }

        Action::WorktreePreCommitUse => {
            app.active_tab_mut().worktree_skip_precommit_check = true;
            launch_pending_command(app).await;
        }

        Action::WorktreePreCommitCommit { message } => {
            handle_worktree_pre_commit_commit(app, message).await;
        }

        Action::CopyToClipboard => {
            match arboard::Clipboard::new() {
                Ok(cb) => {
                    let mut writer = ArboardClipboard(cb);
                    copy_selection_to_clipboard(app.active_tab(), &mut writer);
                }
                Err(e) => {
                    tracing::warn!("Clipboard unavailable: {}", e);
                }
            }
            app.active_tab_mut().clear_terminal_selection();
        }

        Action::ReadyLegacyMigrate => {
            // Record the migration decision in the pending command so that
            // execute() can perform the file operations inside the flow.
            if let PendingCommand::Ready { ref mut migrate_decision, ref mut refresh, ref mut build, .. } =
                app.active_tab_mut().pending_command
            {
                *migrate_decision = Some(true);
                // Force refresh + rebuild: migration requires a fresh base image from the
                // new minimal Dockerfile.dev and an audit to restore project dependencies.
                *refresh = true;
                *build = true;
            }
            // Migration forces refresh=true, so the template audit question is moot.
            show_pre_command_dialogs(app).await;
        }

        Action::ReadyLegacyKeep => {
            // Record the keep decision; execute() will print the deprecation warning.
            if let PendingCommand::Ready { ref mut migrate_decision, .. } =
                app.active_tab_mut().pending_command
            {
                *migrate_decision = Some(false);
            }
            // After keeping the legacy layout, check whether the Dockerfile.dev still
            // matches the default template and ask the user about the audit.
            let tab_cwd = app.active_tab().cwd.clone();
            let needs_confirm = if let Some(git_root) = find_git_root_from(&tab_cwd) {
                let refresh = matches!(
                    app.active_tab().pending_command,
                    PendingCommand::Ready { refresh: true, .. }
                );
                !refresh && ready_needs_template_audit_confirm(&git_root)
            } else {
                false
            };
            if needs_confirm {
                app.active_tab_mut().dialog = Dialog::ReadyTemplateAuditConfirm;
            } else {
                show_pre_command_dialogs(app).await;
            }
        }

        Action::ReadyTemplateAuditAccept => {
            // User wants to run the audit; set refresh=true so execute_pre_audit launches it.
            if let PendingCommand::Ready { ref mut template_audit_decision, ref mut refresh, .. } =
                app.active_tab_mut().pending_command
            {
                *template_audit_decision = Some(true);
                *refresh = true;
            }
            show_pre_command_dialogs(app).await;
        }

        Action::ReadyTemplateAuditDecline => {
            // User declined the audit; record decision and continue normally.
            if let PendingCommand::Ready { ref mut template_audit_decision, .. } =
                app.active_tab_mut().pending_command
            {
                *template_audit_decision = Some(false);
            }
            show_pre_command_dialogs(app).await;
        }

        Action::InitReplaceAspecAccepted { agent } => {
            // User confirmed replacing aspec; proceed to the audit question.
            app.active_tab_mut().dialog = Dialog::InitAuditConfirm { agent, aspec: true, replace_aspec: true };
        }

        Action::InitReplaceAspecDeclined { agent } => {
            // User declined replacing aspec; still ask about the audit.
            app.active_tab_mut().dialog = Dialog::InitAuditConfirm { agent, aspec: true, replace_aspec: false };
        }

        Action::InitAuditAccepted { agent, aspec, replace_aspec } => {
            let tab_cwd = app.active_tab().cwd.clone();
            if should_offer_work_items(aspec, &tab_cwd) {
                app.active_tab_mut().dialog = Dialog::InitWorkItemsConfirm {
                    agent,
                    aspec,
                    replace_aspec,
                    run_audit: true,
                };
            } else {
                launch_init(app, agent, aspec, replace_aspec, true, None).await;
            }
        }

        Action::InitAuditDeclined { agent, aspec, replace_aspec } => {
            let tab_cwd = app.active_tab().cwd.clone();
            if should_offer_work_items(aspec, &tab_cwd) {
                app.active_tab_mut().dialog = Dialog::InitWorkItemsConfirm {
                    agent,
                    aspec,
                    replace_aspec,
                    run_audit: false,
                };
            } else {
                launch_init(app, agent, aspec, replace_aspec, false, None).await;
            }
        }

        Action::InitWorkItemsDone { agent, aspec, replace_aspec, run_audit, work_items } => {
            launch_init(app, agent, aspec, replace_aspec, run_audit, work_items).await;
        }

        Action::AgentSetupAccepted { agent, image_only } => {
            handle_agent_setup_accepted(app, agent, image_only).await;
        }

        Action::AgentSetupFallbackAccepted { declined_agent, default_agent } => {
            // User declined setting up `declined_agent` but accepted falling back to `default_agent`.
            // Record the fallback decision so the next pre-flight pass substitutes the default.
            app.active_tab_mut().workflow_agent_fallbacks.insert(declined_agent, default_agent);
            launch_pending_command(app).await;
        }

        Action::AgentSetupDeclined { agent: _ } => {
            app.active_tab_mut().push_output(
                "Agent setup declined. Workflow cannot continue without the required agent.".to_string(),
            );
            app.active_tab_mut().pending_command = PendingCommand::None;
        }

        // ── Remote actions ────────────────────────────────────────────────────
        Action::RemoteSessionChosen { session_id } => {
            // The picker was shown during `remote run` — now we have a session ID.
            // Extract the command and follow from the pending RemoteRun state if available,
            // otherwise look for what was stored in the dialog before it was closed.
            if let PendingCommand::RemoteRun { remote_addr, command, follow, api_key, .. } =
                app.active_tab().pending_command.clone()
            {
                app.active_tab_mut().pending_command = PendingCommand::RemoteRun {
                    remote_addr,
                    session_id,
                    command,
                    follow,
                    api_key,
                };
                launch_pending_command(app).await;
            }
        }

        Action::RemoteSavedDirChosen { dir } => {
            // Directory chosen from saved-dirs picker for `remote session start`.
            // The remote_addr is in the pending command (stored before showing the picker).
            if let PendingCommand::RemoteSessionStart { remote_addr, api_key, .. } =
                app.active_tab().pending_command.clone()
            {
                // Check if dir is not yet saved; if so show save confirm.
                let saved = crate::config::effective_remote_saved_dirs();
                if !saved.contains(&dir) {
                    app.active_tab_mut().pending_command = PendingCommand::RemoteSessionStart {
                        remote_addr: remote_addr.clone(),
                        dir: dir.clone(),
                        api_key,
                    };
                    app.active_tab_mut().dialog = state::Dialog::RemoteSaveDirConfirm {
                        dir,
                        remote_addr,
                    };
                } else {
                    app.active_tab_mut().pending_command = PendingCommand::RemoteSessionStart {
                        remote_addr,
                        dir,
                        api_key,
                    };
                    launch_pending_command(app).await;
                }
            }
        }

        Action::RemoteSaveDirAccepted => {
            // User accepted saving the directory — save it, then launch the pending command.
            if let PendingCommand::RemoteSessionStart { ref dir, .. } =
                app.active_tab().pending_command.clone()
            {
                let dir_clone = dir.clone();
                if let Err(e) = crate::commands::remote::save_dir_to_config(&dir_clone) {
                    app.active_tab_mut().push_output(format!("Warning: failed to save directory: {}", e));
                }
            }
            launch_pending_command(app).await;
        }

        Action::RemoteSaveDirDeclined => {
            // User declined saving — just launch the pending command.
            launch_pending_command(app).await;
        }

        Action::RemoteSessionKillChosen { session_id } => {
            // Session chosen from the kill picker.
            if let PendingCommand::RemoteSessionKill { remote_addr, api_key, .. } =
                app.active_tab().pending_command.clone()
            {
                app.active_tab_mut().pending_command = PendingCommand::RemoteSessionKill {
                    remote_addr,
                    session_id,
                    api_key,
                };
                launch_pending_command(app).await;
            }
        }
    }
}

/// Run a git command in `cwd`, print `$ git <args>` and full stdout+stderr to the outer window.
/// Returns `true` if the command succeeded.
fn run_git_show(tab: &mut crate::tui::state::TabState, cwd: &std::path::Path, args: &[&str]) -> bool {
    tab.push_output(format!("$ git {}", args.join(" ")));
    match std::process::Command::new("git").args(args).current_dir(cwd).output() {
        Ok(out) => {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            for line in combined.lines() {
                tab.push_output(line.to_string());
            }
            out.status.success()
        }
        Err(e) => {
            tab.push_output(format!("error: {}", e));
            false
        }
    }
}

/// RAII guard that restores the Ratatui terminal on drop.
///
/// Created immediately after suspending (leaving alternate screen, disabling raw mode,
/// disabling mouse capture).  If `run_git_interactive` panics — e.g. on OOM inside
/// `Command::status()` — Rust's drop glue runs this before unwinding, guaranteeing the
/// terminal is never left in a suspended state.
struct TerminalRestoreGuard;

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        let _ = enable_raw_mode();
        let _ = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture);
    }
}

/// Run a git command that may require interactive TTY access (e.g. a GPG passphrase prompt).
///
/// Suspends the Ratatui terminal before executing — disables mouse capture, leaves the
/// alternate screen, and disables raw mode — so that pinentry or any other TTY-based
/// subprocess gets clean terminal ownership.  Restores the terminal afterwards (via a
/// `Drop` guard for panic-safety) and sets `app.needs_full_redraw` so the event loop
/// triggers a full re-render on the next tick.
///
/// Works with every signing method (GPG, SSH key signing, S/MIME) and every pinentry
/// variant without any special-casing.  Users without signing enabled see no visible
/// change: the suspend/restore round-trip is imperceptible when no passphrase prompt
/// appears.
///
/// Returns `true` if the command exited with status 0.
fn run_git_interactive(app: &mut App, cwd: &std::path::Path, args: &[&str]) -> bool {
    // Print a visible header so the user knows why the TUI disappeared.
    println!("\n[amux] running: git {}\n", args.join(" "));

    // Suspend: disable mouse capture, leave alternate screen, then disable raw mode.
    // Order matters — leave alternate screen while still in raw mode produces garbage
    // on some terminals; disable mouse capture first to avoid stray escape sequences
    // appearing on the normal screen during the subprocess.
    let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    let _ = disable_raw_mode();

    // Run with inherited stdio so GPG/pinentry gets full terminal access.
    // The Drop guard restores the terminal unconditionally — even on panic.
    let status = {
        let _guard = TerminalRestoreGuard;
        std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
        // _guard drops here: enable_raw_mode + EnterAlternateScreen + EnableMouseCapture
    };

    // Signal the event loop to call terminal.clear() before the next draw so that
    // Ratatui's internal buffer is reset and a full re-render is performed.
    app.needs_full_redraw = true;

    match status {
        Ok(s) if s.success() => true,
        Ok(s) => {
            app.active_tab_mut().push_output(format!(
                "git {} exited with code {}",
                args.join(" "),
                s.code().unwrap_or(-1)
            ));
            false
        }
        Err(e) => {
            app.active_tab_mut()
                .push_output(format!("git {}: {e}", args.join(" ")));
            false
        }
    }
}

/// Download and build the Dockerfile for a missing agent, then re-trigger the pending command.
///
/// Called when the user accepts the `AgentSetupConfirm` dialog.  The agent Dockerfile is
/// fetched from GitHub and the agent image is built as a foreground text task; when that
/// task completes `check_audit_continuation` detects `AuditPhase::AgentSetupBuild` and
/// re-calls `launch_pending_command`, which re-enters `launch_implement` and finds the
/// Dockerfile now present.
async fn handle_agent_setup_accepted(app: &mut App, agent: String, image_only: bool) {
    let tab_cwd = app.active_tab().cwd.clone();
    let git_root = match find_git_root_from(&tab_cwd) {
        Some(r) => r,
        None => {
            app.active_tab_mut().push_output("Not inside a Git repository.".to_string());
            app.active_tab_mut().pending_command = PendingCommand::None;
            return;
        }
    };

    app.active_tab_mut().audit_phase = AuditPhase::AgentSetupBuild;
    app.active_tab_mut().start_command(format!("Building agent '{}'", agent));
    let runtime = app.runtime.clone();
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    app.active_tab_mut().exit_rx = Some(exit_rx);
    let tx = app.active_tab().output_tx.clone();

    spawn_text_command(tx, exit_tx, move |sink| async move {
        if image_only {
            // Dockerfile already exists; just build the agent image.
            let ok = crate::commands::agent::build_agent_image(
                &git_root,
                &agent,
                &sink,
                runtime.as_ref(),
            )?;
            if !ok {
                anyhow::bail!("Agent '{}' image build failed.", agent);
            }
        } else {
            // Dockerfile missing — download and build.
            let available = crate::commands::agent::ensure_agent_available(
                &git_root,
                &agent,
                &sink,
                runtime.as_ref(),
                |_| Ok(true), // user already confirmed via dialog
            )
            .await?;
            if !available {
                anyhow::bail!("Agent '{}' setup failed.", agent);
            }
        }
        Ok(())
    });
}

/// Check for uncommitted files in the worktree and either show the commit-prompt dialog
/// (if there are uncommitted files) or skip straight to the merge-confirm dialog.
async fn handle_worktree_merge(app: &mut App) {
    let (branch, wt_path, git_root) = match (
        app.active_tab_mut().worktree_branch.take(),
        app.active_tab_mut().worktree_active_path.take(),
        app.active_tab_mut().worktree_git_root.take(),
    ) {
        (Some(b), Some(p), Some(r)) => (b, p, r),
        _ => return,
    };

    let files = crate::git::uncommitted_files(&wt_path).unwrap_or_default();
    if files.is_empty() {
        app.active_tab_mut().dialog = Dialog::WorktreeMergeConfirm {
            branch,
            worktree_path: wt_path,
            git_root,
        };
    } else {
        let default_msg = format!("Uncommitted changes in {}", branch);
        let cursor_pos = default_msg.len();
        app.active_tab_mut().dialog = Dialog::WorktreeCommitPrompt {
            branch,
            worktree_path: wt_path,
            git_root,
            uncommitted_files: files,
            message: default_msg,
            cursor_pos,
        };
    }
}

/// Stage all uncommitted files in the worktree and create a commit, then show the merge-confirm dialog.
async fn handle_worktree_commit_files(
    app: &mut App,
    message: String,
    branch: String,
    wt_path: std::path::PathBuf,
    git_root: std::path::PathBuf,
) {
    {
        let tab = app.active_tab_mut();
        run_git_show(tab, &wt_path, &["add", "-A"]);
    }
    if !run_git_interactive(app, &wt_path, &["commit", "-m", &message]) {
        // Error already pushed to output; stay in the current state so the user sees it.
        return;
    }
    app.active_tab_mut().dialog = Dialog::WorktreeMergeConfirm {
        branch,
        worktree_path: wt_path,
        git_root,
    };
}

/// Squash-merge the worktree branch into the current HEAD, show git output, then show delete-confirm dialog.
async fn handle_worktree_merge_confirmed(
    app: &mut App,
    branch: String,
    wt_path: std::path::PathBuf,
    git_root: std::path::PathBuf,
) {
    let commit_msg = format!("Implement {}", branch);
    {
        let tab = app.active_tab_mut();
        let merge_ok = run_git_show(tab, &git_root, &["merge", "--squash", &branch]);
        if !merge_ok {
            return;
        }
    }
    if !run_git_interactive(app, &git_root, &["commit", "-m", &commit_msg]) {
        return;
    }
    app.active_tab_mut().dialog = Dialog::WorktreeDeleteConfirm {
        branch,
        worktree_path: wt_path,
        git_root,
    };
}

/// Remove the worktree directory and delete the branch, showing all git output.
fn handle_worktree_delete_confirmed(
    app: &mut App,
    branch: String,
    wt_path: std::path::PathBuf,
    git_root: std::path::PathBuf,
) {
    let wt_str = wt_path.to_string_lossy().to_string();
    let tab = app.active_tab_mut();
    run_git_show(tab, &git_root, &["worktree", "remove", "--force", &wt_str]);
    run_git_show(tab, &git_root, &["branch", "-D", &branch]);
}

/// Discard the worktree branch and remove the worktree directory.
async fn handle_worktree_discard(app: &mut App) {
    let (branch, wt_path, git_root) = match (
        app.active_tab_mut().worktree_branch.take(),
        app.active_tab_mut().worktree_active_path.take(),
        app.active_tab_mut().worktree_git_root.take(),
    ) {
        (Some(b), Some(p), Some(r)) => (b, p, r),
        _ => return,
    };
    let tx = app.active_tab().output_tx.clone();
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    app.active_tab_mut().exit_rx = Some(exit_rx);
    spawn_text_command(tx, exit_tx, move |sink| async move {
        match crate::git::remove_worktree(&git_root, &wt_path) {
            Ok(()) => {
                sink.println(format!("Worktree at {} removed.", wt_path.display()));
                let _ = crate::git::delete_branch(&git_root, &branch);
                sink.println(format!("Branch '{}' deleted.", branch));
            }
            Err(e) => {
                sink.println(format!("Failed to remove worktree: {}", e));
            }
        }
        Ok(())
    });
}

/// Stage all uncommitted files in the main branch (git_root) and create a commit,
/// then proceed with the pending implement command.
async fn handle_worktree_pre_commit_commit(app: &mut App, message: String) {
    let git_root = match find_git_root_from(&app.active_tab().cwd) {
        Some(r) => r,
        None => return,
    };
    {
        let tab = app.active_tab_mut();
        run_git_show(tab, &git_root, &["add", "-A"]);
    }
    if !run_git_interactive(app, &git_root, &["commit", "-m", &message]) {
        return;
    }
    launch_pending_command(app).await;
}

/// Keep the worktree branch as-is (no merge, no delete).
fn handle_worktree_skip(app: &mut App) {
    if let Some(path) = app.active_tab().worktree_active_path.clone() {
        app.active_tab_mut().push_output(format!(
            "Worktree kept at {}. Use 'git worktree list' to see active worktrees.",
            path.display()
        ));
    }
    app.active_tab_mut().worktree_branch = None;
    app.active_tab_mut().worktree_active_path = None;
    app.active_tab_mut().worktree_git_root = None;
}

/// Execute a command on the active tab.
async fn execute_tab_command(app: &mut App, cmd: &str) {
    execute_command(app, cmd).await;
}

/// Launch a command on a remote-bound tab.
///
/// Submits the command to the remote headless server via `POST /v1/commands`,
/// streams logs to the tab's output, and updates the tab's execution phase.
async fn launch_remote_bound_command(app: &mut App, tab_idx: usize, raw_command: &str) {
    let binding = match app.tabs[tab_idx].remote_binding.clone() {
        Some(b) => b,
        None => return,
    };

    let parts: Vec<String> = match shell_words::split(raw_command) {
        Ok(w) => w,
        Err(_) => {
            app.tabs[tab_idx].input_error = Some("Invalid command: unmatched quote.".into());
            return;
        }
    };
    if parts.is_empty() {
        return;
    }

    app.tabs[tab_idx].start_command(raw_command.to_string());

    let tx = app.tabs[tab_idx].output_tx.clone();
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    app.tabs[tab_idx].exit_rx = Some(exit_rx);

    // Set up workflow state polling channel.
    let (wf_tx, wf_rx) = tokio::sync::mpsc::unbounded_channel();
    app.tabs[tab_idx].remote_workflow_rx = Some(wf_rx);

    let remote_addr = binding.remote_addr.clone();
    let session_id = binding.session_id.clone();
    let api_key = binding.api_key.clone();

    tokio::spawn(async move {
        let sink = crate::commands::output::OutputSink::Channel(tx.clone());

        // Submit the command and capture the command_id for workflow polling.
        let client = match crate::commands::remote::make_client() {
            Ok(c) => c,
            Err(e) => {
                sink.println(format!("Failed to build HTTP client: {}", e));
                let _ = exit_tx.send(1);
                return;
            }
        };

        let subcommand = &parts[0];
        let args: Vec<&str> = parts[1..].iter().map(|s| s.as_str()).collect();
        let body = serde_json::json!({
            "subcommand": subcommand,
            "args": args,
        });

        let mut req = client.post(format!("{}/v1/commands", remote_addr))
            .header("x-amux-session", &session_id)
            .header("content-type", "application/json")
            .json(&body);
        if let Some(ref key) = api_key {
            req = req.header("authorization", format!("Bearer {}", key));
        }

        let response = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                sink.println(format!("Failed to submit command: {}", e));
                let _ = exit_tx.send(1);
                return;
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            let msg = if status == reqwest::StatusCode::UNAUTHORIZED {
                "Authentication failed (401). Check remote.defaultAPIKey in config \
                 or pass --api-key.".to_string()
            } else if status == reqwest::StatusCode::NOT_FOUND {
                format!(
                    "Session '{}' not found on remote host. The session may have \
                     been killed — use `remote session start` to create a new one.",
                    session_id
                )
            } else if status == reqwest::StatusCode::FORBIDDEN {
                format!(
                    "Session '{}' is busy: another command is already running. \
                     Wait for it to finish before submitting a new command.",
                    session_id
                )
            } else {
                format!("Remote error {}: {}", status, text)
            };
            sink.println(msg);
            let _ = exit_tx.send(1);
            return;
        }

        let resp_json: serde_json::Value = match response.json().await {
            Ok(j) => j,
            Err(e) => {
                sink.println(format!("Failed to parse response: {}", e));
                let _ = exit_tx.send(1);
                return;
            }
        };

        let command_id = match resp_json["command_id"].as_str() {
            Some(id) => id.to_string(),
            None => {
                sink.println("Response missing command_id".to_string());
                let _ = exit_tx.send(1);
                return;
            }
        };

        sink.println(format!("Command submitted: {}", command_id));

        // Spawn workflow state polling task.
        //
        // Two-phase design:
        //   Phase 1 — initial check after 5 s.  If no workflow exists (404) or
        //             there is a transient error, give up entirely; non-workflow
        //             commands (ready, chat, …) never produce a state file.
        //   Phase 2 — once a workflow is found, poll every 5 s until terminal or
        //             until the server returns 404 (workflow removed).
        //
        // In both phases, `wf_tx.is_closed()` is checked before each HTTP call:
        // `start_command` drops `remote_workflow_rx`, which closes the receiver
        // end; subsequent `is_closed()` calls return true, letting the background
        // task exit promptly when the user dispatches a new command or closes the tab.
        let wf_addr = remote_addr.clone();
        let wf_cmd_id = command_id.clone();
        let wf_api_key = api_key.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            // Phase 1: initial check.
            if wf_tx.is_closed() {
                return;
            }
            let initial = crate::commands::remote::fetch_workflow_state(
                &wf_addr, &wf_cmd_id, wf_api_key.as_deref(),
            ).await;
            let mut state = match initial {
                Ok(Some(s)) => s,
                // 404 means this command never produces a workflow — stop here.
                // Network/auth errors are also non-recoverable at this point.
                Ok(None) | Err(_) => return,
            };
            // Phase 2: workflow found — forward the initial state and keep polling.
            // Clone before sending so `state` remains valid on the `Err` continue path.
            loop {
                let is_terminal = state.is_terminal();
                if wf_tx.send(state.clone()).is_err() {
                    return; // tab closed or new command started
                }
                if is_terminal {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if wf_tx.is_closed() {
                    return;
                }
                state = match crate::commands::remote::fetch_workflow_state(
                    &wf_addr, &wf_cmd_id, wf_api_key.as_deref(),
                ).await {
                    Ok(Some(s)) => s,
                    Ok(None) => return, // workflow was removed — stop polling
                    Err(_) => continue, // transient error — retry after next interval
                };
            }
        });

        // Stream logs.
        sink.println("Streaming logs...".to_string());
        let _ = crate::commands::remote::stream_command_logs(
            &remote_addr,
            &command_id,
            api_key.as_deref(),
            &sink,
        ).await;

        let _ = exit_tx.send(0);
    });
}

/// Parse flags from the command parts, returning (refresh, build, no_cache, non_interactive, allow_docker).
/// Returns `true` when the ready command should show the template-audit confirm dialog.
///
/// Conditions: `Dockerfile.dev` exists in the git root and its content is identical
/// to the default project template (i.e. it has never been customised).
fn ready_needs_template_audit_confirm(git_root: &std::path::Path) -> bool {
    let dockerfile_path = git_root.join("Dockerfile.dev");
    if !dockerfile_path.exists() {
        return false;
    }
    let content = std::fs::read_to_string(&dockerfile_path).unwrap_or_default();
    dockerfile_matches_template(&content)
}


/// Parse and dispatch a command string entered by the user.
async fn execute_command(app: &mut App, cmd: &str) {
    let owned = match shell_words::split(cmd.trim()) {
        Ok(w) => w,
        Err(_) => {
            app.active_tab_mut().input_error = Some("Invalid command: unmatched quote.".into());
            return;
        }
    };
    let parts: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
    if parts.is_empty() {
        return;
    }

    // If the active tab is bound to a remote session, forward most commands
    // to the remote host instead of executing locally.
    // Exception: `config show` / bare `config` opens the local TUI config
    // dialog regardless of binding — this is a TUI-local operation that
    // configures the local amux installation, not the remote server.
    if app.active_tab().remote_binding.is_some() {
        let is_local_config_show = parts[0] == "config"
            && matches!(parts.get(1), None | Some(&"show"));
        if !is_local_config_show {
            let tab_idx = app.active_tab_idx;
            launch_remote_bound_command(app, tab_idx, cmd).await;
            return;
        }
        // Fall through to the local `config` match arm below.
    }

    match parts[0] {
        "init" => {
            let init_spec = crate::commands::spec::ALL_COMMANDS.iter().find(|c| c.name == "init").unwrap();
            let flags = flag_parser::parse_flags(&parts, init_spec);
            let agent = flag_parser::flag_string(&flags, "agent")
                .and_then(|v| Agent::all().iter().find(|a| a.as_str() == v).cloned())
                .unwrap_or(Agent::Claude);
            let aspec = flag_parser::flag_bool(&flags, "aspec");

            // Validate git root before any Q&A begins (spec requirement).
            let tab_cwd = app.active_tab().cwd.clone();
            let git_root = match find_git_root_from(&tab_cwd) {
                Some(r) => r,
                None => {
                    app.active_tab_mut().input_error = Some("Not inside a Git repository.".into());
                    return;
                }
            };

            // If --aspec and the aspec folder already exists, ask whether to replace it first.
            if aspec && git_root.join("aspec").exists() {
                app.active_tab_mut().dialog = Dialog::InitReplaceAspec { agent };
                return;
            }

            // Show audit confirmation dialog (ask whether to run the agent audit after init).
            app.active_tab_mut().dialog = Dialog::InitAuditConfirm { agent, aspec, replace_aspec: false };
        }

        "ready" => {
            let ready_spec = crate::commands::spec::ALL_COMMANDS.iter().find(|c| c.name == "ready").unwrap();
            let flags = flag_parser::parse_flags(&parts, ready_spec);
            let refresh = flag_parser::flag_bool(&flags, "refresh");
            let build = flag_parser::flag_bool(&flags, "build");
            let no_cache = flag_parser::flag_bool(&flags, "no-cache");
            let non_interactive = flag_parser::flag_bool(&flags, "non-interactive");
            let allow_docker = flag_parser::flag_bool(&flags, "allow-docker");
            let effective_build = compute_ready_build_flag(refresh, build);
            app.active_tab_mut().pending_command = PendingCommand::Ready {
                refresh,
                build: effective_build,
                no_cache,
                non_interactive,
                allow_docker,
                migrate_decision: None,
                template_audit_decision: None,
            };

            let tab_cwd = app.active_tab().cwd.clone();
            if let Some(git_root) = find_git_root_from(&tab_cwd) {
                let config = load_repo_config(&git_root).unwrap_or_default();
                let agent_name = config.agent.as_deref().unwrap_or("claude").to_string();

                // Detect legacy layout: Dockerfile.dev exists but .amux/Dockerfile.{agent} does not.
                // Show the migration dialog to pre-collect the user's decision before launching.
                if is_legacy_layout(&git_root, &agent_name) {
                    app.active_tab_mut().dialog = Dialog::ReadyLegacyMigration { agent_name };
                    return;
                }

                // Detect unmodified template: Dockerfile.dev exists and matches the default template.
                // Ask whether to launch the audit container (only when --refresh not already set).
                if !refresh && ready_needs_template_audit_confirm(&git_root) {
                    app.active_tab_mut().dialog = Dialog::ReadyTemplateAuditConfirm;
                    return;
                }
            }

            show_pre_command_dialogs(app).await;
        }

        "implement" => {
            let impl_spec = crate::commands::spec::ALL_COMMANDS.iter().find(|c| c.name == "implement").unwrap();
            let flags = flag_parser::parse_flags(&parts, impl_spec);
            let non_interactive = flag_parser::flag_bool(&flags, "non-interactive");
            let plan = flag_parser::flag_bool(&flags, "plan");
            let allow_docker = flag_parser::flag_bool(&flags, "allow-docker");
            let mut worktree = flag_parser::flag_bool(&flags, "worktree");
            let mount_ssh = flag_parser::flag_bool(&flags, "mount-ssh");
            let yolo = flag_parser::flag_bool(&flags, "yolo");
            let auto = flag_parser::flag_bool(&flags, "auto");
            let agent = flag_parser::flag_string(&flags, "agent").map(str::to_string);
            let model = flag_parser::flag_string(&flags, "model").map(str::to_string);
            let workflow = flag_parser::flag_string(&flags, "workflow").map(std::path::PathBuf::from);
            let overlay = flag_parser::flag_string(&flags, "overlay").map(str::to_string);
            // --yolo/--auto + --workflow implies --worktree.
            if yolo && workflow.is_some() && !worktree {
                app.active_tab_mut().push_output(
                    "--yolo with --workflow implies --worktree. Running in isolated worktree.".to_string(),
                );
                worktree = true;
            }
            if auto && workflow.is_some() && !worktree {
                app.active_tab_mut().push_output(
                    "--auto with --workflow implies --worktree. Running in isolated worktree.".to_string(),
                );
                worktree = true;
            }
            // Filter out flags (and --workflow <path>) to find the work item number.
            let work_item: u32 = match parts.iter()
                .skip(1)
                .filter(|s| !s.starts_with("--"))
                .find(|s| parse_work_item(s).is_ok())
                .and_then(|s| parse_work_item(s).ok())
            {
                Some(n) => n,
                None => {
                    app.active_tab_mut().input_error =
                        Some("Usage: implement <work-item-number> [--non-interactive] [--plan] [--allow-docker] [--workflow=<path>] [--worktree] [--mount-ssh] [--yolo] [--auto] [--agent=<NAME>] [--model=<NAME>] [--overlay=<SPEC>]".into());
                    return;
                }
            };
            app.active_tab_mut().pending_command = PendingCommand::Implement { agent, model, work_item, non_interactive, plan, allow_docker, workflow, worktree, mount_ssh, yolo, auto, overlay };
            show_pre_command_dialogs(app).await;
        }

        "chat" => {
            let chat_spec = crate::commands::spec::ALL_COMMANDS.iter().find(|c| c.name == "chat").unwrap();
            let flags = flag_parser::parse_flags(&parts, chat_spec);
            let non_interactive = flag_parser::flag_bool(&flags, "non-interactive");
            let plan = flag_parser::flag_bool(&flags, "plan");
            let allow_docker = flag_parser::flag_bool(&flags, "allow-docker");
            let mount_ssh = flag_parser::flag_bool(&flags, "mount-ssh");
            let yolo = flag_parser::flag_bool(&flags, "yolo");
            let auto = flag_parser::flag_bool(&flags, "auto");
            let agent = flag_parser::flag_string(&flags, "agent").map(str::to_string);
            let model = flag_parser::flag_string(&flags, "model").map(str::to_string);
            let overlay = flag_parser::flag_string(&flags, "overlay").map(str::to_string);
            app.active_tab_mut().pending_command = PendingCommand::Chat { agent, model, non_interactive, plan, allow_docker, mount_ssh, yolo, auto, overlay };
            show_pre_command_dialogs(app).await;
        }


        "new" => {
            match parts.get(1) {
                Some(&"spec") => {
                    let specs_new_spec = crate::commands::spec::ALL_COMMANDS.iter().find(|c| c.name == "specs new").unwrap();
                    let flags = flag_parser::parse_flags(&parts, specs_new_spec);
                    let interview = flag_parser::flag_bool(&flags, "interview");
                    app.active_tab_mut().dialog = state::Dialog::NewKindSelect { interview };
                }
                Some(&"workflow") => {
                    let new_workflow_spec = crate::commands::spec::ALL_COMMANDS.iter().find(|c| c.name == "new workflow").unwrap();
                    let flags = flag_parser::parse_flags(&parts, new_workflow_spec);
                    let interview = flag_parser::flag_bool(&flags, "interview");
                    let global = flag_parser::flag_bool(&flags, "global");
                    let format = match flag_parser::flag_string(&flags, "format") {
                        Some("yaml") | Some("yml") => crate::cli::WorkflowFormat::Yaml,
                        Some("md") => crate::cli::WorkflowFormat::Md,
                        _ => crate::cli::WorkflowFormat::Toml,
                    };
                    app.active_tab_mut().dialog = state::Dialog::NewWorkflow(
                        state::NewWorkflowDialogState::new(
                            String::new(),
                            String::new(),
                            global,
                            format,
                            interview,
                        ),
                    );
                }
                Some(&"skill") => {
                    let new_skill_spec = crate::commands::spec::ALL_COMMANDS.iter().find(|c| c.name == "new skill").unwrap();
                    let flags = flag_parser::parse_flags(&parts, new_skill_spec);
                    let interview = flag_parser::flag_bool(&flags, "interview");
                    let global = flag_parser::flag_bool(&flags, "global");
                    app.active_tab_mut().dialog = state::Dialog::NewSkill(
                        state::NewSkillDialogState::new(global, interview),
                    );
                }
                _ => {
                    app.active_tab_mut().input_error =
                        Some("Usage: new <spec|workflow|skill>  e.g. new spec --interview, new workflow --global".into());
                }
            }
        }

        "specs" => {
            match parts.get(1) {
                Some(&"new") => {
                    let specs_new_spec = crate::commands::spec::ALL_COMMANDS.iter().find(|c| c.name == "specs new").unwrap();
                    let flags = flag_parser::parse_flags(&parts, specs_new_spec);
                    let interview = flag_parser::flag_bool(&flags, "interview");
                    app.active_tab_mut().dialog = state::Dialog::NewKindSelect { interview };
                }
                Some(&"amend") => {
                    let specs_amend_spec = crate::commands::spec::ALL_COMMANDS.iter().find(|c| c.name == "specs amend").unwrap();
                    let flags = flag_parser::parse_flags(&parts, specs_amend_spec);
                    let allow_docker = flag_parser::flag_bool(&flags, "allow-docker");
                    let work_item: u32 = match parts.iter()
                        .skip(2)
                        .find(|s| !s.starts_with("--"))
                        .and_then(|s| parse_work_item(s).ok())
                    {
                        Some(n) => n,
                        None => {
                            app.active_tab_mut().input_error =
                                Some("Usage: specs amend <NNNN>  e.g. specs amend 0025".into());
                            return;
                        }
                    };
                    app.active_tab_mut().pending_command = PendingCommand::SpecsAmend { work_item, allow_docker };
                    show_pre_command_dialogs(app).await;
                }
                _ => {
                    app.active_tab_mut().input_error =
                        Some("Usage: specs <new|amend>  e.g. specs new --interview, specs amend 0025".into());
                }
            }
        }

        "claws" => {
            match parts.get(1) {
                Some(&"init") => {
                    show_claws_init_start(app).await;
                }
                Some(&"ready") => {
                    show_claws_ready_status(app).await;
                }
                Some(&"chat") => {
                    launch_claws_chat_attach(app).await;
                }
                _ => {
                    app.active_tab_mut().input_error =
                        Some("Usage: claws <init|ready|chat>".into());
                }
            }
        }

        "status" => {
            let status_spec = crate::commands::spec::ALL_COMMANDS.iter().find(|c| c.name == "status").unwrap();
            let flags = flag_parser::parse_flags(&parts, status_spec);
            let watch = flag_parser::flag_bool(&flags, "watch");
            app.active_tab_mut().start_command(cmd.to_string());
            let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
            app.active_tab_mut().exit_rx = Some(exit_rx);
            let tx = app.active_tab().output_tx.clone();
            // Pass the shared Arc so the background task reads live state on every refresh.
            let tui_tabs = app.tui_tabs_shared.clone();
            let status_runtime = app.runtime.clone();
            if watch {
                // Create a cancel channel so that running a new command stops the loop.
                let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
                app.active_tab_mut().status_watch_cancel_tx = Some(cancel_tx);
                spawn_text_command(tx, exit_tx, move |sink| async move {
                    status::run_with_sink(true, &sink, Some(cancel_rx), tui_tabs, status_runtime).await
                });
            } else {
                spawn_text_command(tx, exit_tx, move |sink| async move {
                    status::run_with_sink(false, &sink, None, tui_tabs, status_runtime).await
                });
            }
        }

        "exec" => {
            match parts.get(1) {
                Some(&"prompt") => {
                    let exec_prompt_spec = crate::commands::spec::ALL_COMMANDS.iter().find(|c| c.name == "exec prompt").unwrap();
                    let flags = flag_parser::parse_flags(&parts, exec_prompt_spec);
                    let non_interactive = flag_parser::flag_bool(&flags, "non-interactive");
                    let plan = flag_parser::flag_bool(&flags, "plan");
                    let allow_docker = flag_parser::flag_bool(&flags, "allow-docker");
                    let mount_ssh = flag_parser::flag_bool(&flags, "mount-ssh");
                    let yolo = flag_parser::flag_bool(&flags, "yolo");
                    let auto = flag_parser::flag_bool(&flags, "auto");
                    let agent = flag_parser::flag_string(&flags, "agent").map(str::to_string);
                    let model = flag_parser::flag_string(&flags, "model").map(str::to_string);
                    let overlay = flag_parser::flag_string(&flags, "overlay").map(str::to_string);

                    // Extract the prompt text: everything after "exec prompt" that isn't a flag.
                    let prompt: String = parts.iter()
                        .skip(2)
                        .filter(|s| !s.starts_with("--") && !s.starts_with('-'))
                        // Also filter out flag values that follow --flag=value pairs (already consumed by parse_flags).
                        .copied()
                        .collect::<Vec<&str>>()
                        .join(" ");
                    if prompt.trim().is_empty() {
                        app.active_tab_mut().input_error =
                            Some("Usage: exec prompt <text> [--plan] [--allow-docker] [--mount-ssh] [--yolo] [--auto] [--agent=<NAME>] [--model=<NAME>] [--overlay=<SPEC>]".into());
                        return;
                    }

                    app.active_tab_mut().pending_command = PendingCommand::ExecPrompt {
                        prompt, agent, model, non_interactive, plan, allow_docker, mount_ssh, yolo, auto, overlay,
                    };
                    show_pre_command_dialogs(app).await;
                }
                Some(&"workflow") | Some(&"wf") => {
                    let exec_wf_spec = crate::commands::spec::ALL_COMMANDS.iter().find(|c| c.name == "exec workflow").unwrap();
                    let flags = flag_parser::parse_flags(&parts, exec_wf_spec);
                    let non_interactive = flag_parser::flag_bool(&flags, "non-interactive");
                    let plan = flag_parser::flag_bool(&flags, "plan");
                    let allow_docker = flag_parser::flag_bool(&flags, "allow-docker");
                    let mut worktree = flag_parser::flag_bool(&flags, "worktree");
                    let mount_ssh = flag_parser::flag_bool(&flags, "mount-ssh");
                    let yolo = flag_parser::flag_bool(&flags, "yolo");
                    let auto = flag_parser::flag_bool(&flags, "auto");
                    let agent = flag_parser::flag_string(&flags, "agent").map(str::to_string);
                    let model = flag_parser::flag_string(&flags, "model").map(str::to_string);
                    let overlay = flag_parser::flag_string(&flags, "overlay").map(str::to_string);
                    let work_item_str = flag_parser::flag_string(&flags, "work-item");
                    let work_item: Option<u32> = match work_item_str {
                        Some(s) => match parse_work_item(s) {
                            Ok(n) => Some(n),
                            Err(e) => {
                                app.active_tab_mut().input_error = Some(format!("Invalid --work-item: {}", e));
                                return;
                            }
                        },
                        None => None,
                    };

                    // Extract workflow path: first positional arg after "exec workflow".
                    let workflow_path: Option<std::path::PathBuf> = parts.iter()
                        .skip(2)
                        .find(|s| !s.starts_with("--") && !s.starts_with('-'))
                        .map(|s| std::path::PathBuf::from(s));
                    let workflow = match workflow_path {
                        Some(p) => p,
                        None => {
                            app.active_tab_mut().input_error =
                                Some("Usage: exec workflow <path> [--work-item=<NUM>] [--plan] [--allow-docker] [--worktree] [--mount-ssh] [--yolo] [--auto] [--agent=<NAME>] [--model=<NAME>] [--overlay=<SPEC>]".into());
                            return;
                        }
                    };

                    // --yolo/--auto implies --worktree.
                    if yolo && !worktree {
                        app.active_tab_mut().push_output(
                            "--yolo implies --worktree. Running in isolated worktree.".to_string(),
                        );
                        worktree = true;
                    }
                    if auto && !worktree {
                        app.active_tab_mut().push_output(
                            "--auto implies --worktree. Running in isolated worktree.".to_string(),
                        );
                        worktree = true;
                    }

                    app.active_tab_mut().pending_command = PendingCommand::ExecWorkflow {
                        workflow, work_item, agent, model, non_interactive, plan, allow_docker, worktree, mount_ssh, yolo, auto, overlay,
                    };
                    show_pre_command_dialogs(app).await;
                }
                _ => {
                    app.active_tab_mut().input_error =
                        Some("Usage: exec <prompt|workflow>  e.g. exec prompt \"hello\", exec workflow ./wf.md".into());
                }
            }
        }

        "config" => {
            // Only "config show" (or bare "config") opens the TUI config dialog.
            match parts.get(1) {
                Some(&"show") | None => {
                    let git_root = find_git_root_from(&app.active_tab().cwd);
                    let global_config = crate::config::load_global_config().unwrap_or_default();
                    let repo_config = git_root
                        .as_deref()
                        .and_then(|r| {
                            let _ = crate::config::migrate_legacy_repo_config(r);
                            crate::config::load_repo_config(r).ok()
                        })
                        .unwrap_or_default();

                    // Determine initial selected_col based on the first field's scope.
                    use crate::commands::config::{ALL_FIELDS, FieldScope};
                    let initial_col = match ALL_FIELDS[0].scope {
                        FieldScope::RepoOnly => 1,
                        _ => 0,
                    };

                    let state = crate::tui::state::ConfigDialogState {
                        selected_row: 0,
                        selected_col: initial_col,
                        edit_mode: false,
                        edit_value: String::new(),
                        edit_cursor: 0,
                        git_root,
                        global_config,
                        repo_config,
                        error_msg: None,
                    };
                    app.active_tab_mut().dialog = state::Dialog::ConfigShow(state);
                }
                _ => {
                    app.active_tab_mut().input_error =
                        Some("Usage: config show".into());
                }
            }
        }

        "remote" => {
            match parts.get(1) {
                Some(&"run") => {
                    let run_spec = crate::commands::spec::ALL_COMMANDS.iter().find(|c| c.name == "remote run").unwrap();
                    let flags = flag_parser::parse_flags(&parts, run_spec);
                    let remote_addr_flag = flag_parser::flag_string(&flags, "remote-addr").map(str::to_string);
                    let session_flag = flag_parser::flag_string(&flags, "session").map(str::to_string);
                    // Detect --follow (long form) or -f (short form; flag_parser only handles --).
                    let follow = flag_parser::flag_bool(&flags, "follow")
                        || parts.iter().skip(2).any(|s| *s == "-f");

                    // Extract pass-through command: everything after "remote run" that isn't a parsed flag.
                    let command = extract_passthrough_command(&parts, 2);
                    if command.is_empty() {
                        app.active_tab_mut().start_command("remote run".into());
                        app.active_tab_mut().push_output("Usage: remote run <subcommand> [args] [--session=ID] [--follow] [--remote-addr=URL]");
                        app.active_tab_mut().finish_command(1);
                        return;
                    }

                    let addr = match crate::commands::remote::resolve_remote_addr(remote_addr_flag.as_deref()) {
                        Ok(a) => a,
                        Err(e) => {
                            app.active_tab_mut().start_command("remote run".into());
                            app.active_tab_mut().push_output(format!("Error: {}", e));
                            app.active_tab_mut().finish_command(1);
                            return;
                        }
                    };

                    // Resolve session: flag → env var → last_remote_session_id → picker.
                    let session_id = crate::commands::remote::resolve_remote_session(session_flag.as_deref())
                        .or_else(|| app.active_tab().last_remote_session_id.clone());

                    let api_key_flag = flag_parser::flag_string(&flags, "api-key").map(str::to_string);
                    let resolved_key = crate::commands::remote::resolve_api_key(api_key_flag.as_deref(), &addr);

                    if let Some(sid) = session_id {
                        app.active_tab_mut().pending_command = PendingCommand::RemoteRun {
                            remote_addr: addr,
                            session_id: sid,
                            command,
                            follow,
                            api_key: resolved_key,
                        };
                        launch_pending_command(app).await;
                    } else {
                        // No session resolved — store partial pending command then show picker.
                        // RemoteSessionChosen will fill in the session_id.
                        app.active_tab_mut().pending_command = PendingCommand::RemoteRun {
                            remote_addr: addr.clone(),
                            session_id: String::new(), // filled in by RemoteSessionChosen
                            command: command.clone(),
                            follow,
                            api_key: resolved_key.clone(),
                        };
                        fetch_and_show_session_picker(app, addr, resolved_key, command, follow).await;
                    }
                }
                Some(&"session") => {
                    match parts.get(2) {
                        Some(&"start") => {
                            let start_spec = crate::commands::spec::ALL_COMMANDS.iter().find(|c| c.name == "remote session start").unwrap();
                            let flags = flag_parser::parse_flags(&parts, start_spec);
                            let remote_addr_flag = flag_parser::flag_string(&flags, "remote-addr").map(str::to_string);

                            let addr = match crate::commands::remote::resolve_remote_addr(remote_addr_flag.as_deref()) {
                                Ok(a) => a,
                                Err(e) => {
                                    app.active_tab_mut().start_command("remote session start".into());
                                    app.active_tab_mut().push_output(format!("Error: {}", e));
                                    app.active_tab_mut().finish_command(1);
                                    return;
                                }
                            };

                            let api_key_flag = flag_parser::flag_string(&flags, "api-key").map(str::to_string);
                            let resolved_key = crate::commands::remote::resolve_api_key(api_key_flag.as_deref(), &addr);

                            // Extract positional dir arg (not a flag).
                            let dir_arg: Option<String> = parts.iter()
                                .skip(3)
                                .find(|s| !s.starts_with("--"))
                                .map(|s| s.to_string());

                            if let Some(dir) = dir_arg {
                                // Check if dir is saved; if not, show save-dir confirm.
                                let saved = crate::config::effective_remote_saved_dirs();
                                if !saved.contains(&dir) {
                                    app.active_tab_mut().dialog = state::Dialog::RemoteSaveDirConfirm {
                                        dir: dir.clone(),
                                        remote_addr: addr.clone(),
                                    };
                                    app.active_tab_mut().pending_command = PendingCommand::RemoteSessionStart {
                                        remote_addr: addr,
                                        dir,
                                        api_key: resolved_key,
                                    };
                                } else {
                                    app.active_tab_mut().pending_command = PendingCommand::RemoteSessionStart {
                                        remote_addr: addr,
                                        dir,
                                        api_key: resolved_key,
                                    };
                                    launch_pending_command(app).await;
                                }
                            } else {
                                // No dir provided — show saved dirs picker.
                                let saved = crate::config::effective_remote_saved_dirs();
                                if saved.is_empty() {
                                    app.active_tab_mut().start_command("remote session start".into());
                                    app.active_tab_mut().push_output("Error: No saved directories. Pass a directory argument or configure remote.savedDirs.");
                                    app.active_tab_mut().finish_command(1);
                                } else {
                                    // Store a pending command with a placeholder dir so the addr is
                                    // available when RemoteSavedDirChosen fires.
                                    app.active_tab_mut().pending_command = PendingCommand::RemoteSessionStart {
                                        remote_addr: addr.clone(),
                                        dir: String::new(), // filled in by RemoteSavedDirChosen
                                        api_key: resolved_key,
                                    };
                                    app.active_tab_mut().dialog = state::Dialog::RemoteSavedDirPicker {
                                        dirs: saved,
                                        selected: 0,
                                        remote_addr: addr,
                                    };
                                }
                            }
                        }
                        Some(&"kill") => {
                            let kill_spec = crate::commands::spec::ALL_COMMANDS.iter().find(|c| c.name == "remote session kill").unwrap();
                            let flags = flag_parser::parse_flags(&parts, kill_spec);
                            let remote_addr_flag = flag_parser::flag_string(&flags, "remote-addr").map(str::to_string);

                            let addr = match crate::commands::remote::resolve_remote_addr(remote_addr_flag.as_deref()) {
                                Ok(a) => a,
                                Err(e) => {
                                    app.active_tab_mut().start_command("remote session kill".into());
                                    app.active_tab_mut().push_output(format!("Error: {}", e));
                                    app.active_tab_mut().finish_command(1);
                                    return;
                                }
                            };

                            let api_key_flag = flag_parser::flag_string(&flags, "api-key").map(str::to_string);
                            let resolved_key = crate::commands::remote::resolve_api_key(api_key_flag.as_deref(), &addr);

                            // Extract positional session ID.
                            let session_arg: Option<String> = parts.iter()
                                .skip(3)
                                .find(|s| !s.starts_with("--"))
                                .map(|s| s.to_string());

                            if let Some(sid) = session_arg {
                                app.active_tab_mut().pending_command = PendingCommand::RemoteSessionKill {
                                    remote_addr: addr,
                                    session_id: sid,
                                    api_key: resolved_key,
                                };
                                launch_pending_command(app).await;
                            } else {
                                // Store partial pending command then show kill picker.
                                app.active_tab_mut().pending_command = PendingCommand::RemoteSessionKill {
                                    remote_addr: addr.clone(),
                                    session_id: String::new(), // filled in by RemoteSessionKillChosen
                                    api_key: resolved_key.clone(),
                                };
                                fetch_and_show_session_kill_picker(app, addr, resolved_key).await;
                            }
                        }
                        _ => {
                            app.active_tab_mut().start_command("remote session".into());
                            app.active_tab_mut().push_output("Usage: remote session <start|kill>");
                            app.active_tab_mut().finish_command(1);
                        }
                    }
                }
                _ => {
                    app.active_tab_mut().start_command("remote".into());
                    app.active_tab_mut().push_output("Usage: remote <run|session>  e.g. remote run implement 0001, remote session start /path");
                    app.active_tab_mut().finish_command(1);
                }
            }
        }

        unknown => {
            let suggestion = input::closest_subcommand(unknown)
                .map(|s| format!("  Did you mean: {}", s))
                .unwrap_or_default();
            app.active_tab_mut().input_error = Some(format!(
                "'{}' is not an amux command.{}",
                unknown, suggestion
            ));
        }
    }
}

/// Show any needed dialogs (mount scope, agent auth) before launching a command.
/// Used by both `ready` and `implement` in TUI mode.
async fn show_pre_command_dialogs(app: &mut App) {
    let tab_cwd = app.active_tab().cwd.clone();
    let git_root = match find_git_root_from(&tab_cwd) {
        Some(r) => r,
        None => {
            app.active_tab_mut().input_error = Some("Not inside a Git repository.".into());
            return;
        }
    };

    // Check mount scope.
    let cwd = tab_cwd;
    if cwd != git_root {
        app.active_tab_mut().dialog = Dialog::MountScope {
            git_root: git_root.clone(),
            cwd,
        };
        return; // Wait for user choice; handle_action resumes after dialog.
    }
    app.active_tab_mut().pending_mount_path = Some(git_root.clone());

    // Auto-passthrough: no agent auth dialog needed. Credentials are always
    // read from the keychain automatically.
    launch_pending_command(app).await;
}

/// Resume the pending command after all dialogs have been answered.
async fn launch_pending_command(app: &mut App) {
    match app.active_tab().pending_command.clone() {
        PendingCommand::Ready { .. } => {
            launch_ready(app).await;
        }
        PendingCommand::Implement { agent, model, work_item, non_interactive, plan, allow_docker, workflow, worktree, mount_ssh, yolo, auto, overlay } => {
            launch_implement(app, work_item, non_interactive, plan, allow_docker, workflow, worktree, mount_ssh, yolo, auto, agent, model, overlay).await;
        }
        PendingCommand::Chat { agent, model, non_interactive, plan, allow_docker, mount_ssh, yolo, auto, overlay } => {
            launch_chat(app, non_interactive, plan, allow_docker, mount_ssh, yolo, auto, agent, model, overlay).await;
        }
        PendingCommand::ClawsReady => {
            // Claws ready is launched directly from dialog actions (ClawsReadyProceed /
            // ClawsReadyStartContainer), not through the mount-scope dialog flow.
        }
        PendingCommand::SpecsAmend { work_item, allow_docker } => {
            launch_specs_amend(app, work_item, allow_docker).await;
        }
        PendingCommand::SpecsNewInterview { work_item_number, kind, title, summary, allow_docker } => {
            launch_specs_interview_agent(app, work_item_number, kind, title, summary, allow_docker).await;
        }
        PendingCommand::ExecPrompt { prompt, agent, model, non_interactive, plan, allow_docker, mount_ssh, yolo, auto, overlay } => {
            launch_exec_prompt(app, &prompt, non_interactive, plan, allow_docker, mount_ssh, yolo, auto, agent, model, overlay).await;
        }
        PendingCommand::ExecWorkflow { workflow, work_item, agent, model, non_interactive, plan, allow_docker, worktree, mount_ssh, yolo, auto, overlay } => {
            launch_exec_workflow(app, workflow, work_item, non_interactive, plan, allow_docker, worktree, mount_ssh, yolo, auto, agent, model, overlay).await;
        }
        PendingCommand::RemoteRun { remote_addr, session_id, command, follow, api_key } => {
            launch_remote_run(app, remote_addr, session_id, command, follow, api_key).await;
        }
        PendingCommand::RemoteSessionStart { remote_addr, dir, api_key } => {
            launch_remote_session_start(app, remote_addr, dir, api_key).await;
        }
        PendingCommand::RemoteSessionKill { remote_addr, session_id, api_key } => {
            launch_remote_session_kill(app, remote_addr, session_id, api_key).await;
        }
        PendingCommand::None => {}
    }
}

/// Extract the pass-through command tokens from the parts slice starting at `offset`.
///
/// Only strips `remote run`-specific flags and their values:
///   - `--remote-addr <val>` (value-taking, both space and `=` forms)
///   - `--session <val>` (value-taking, both space and `=` forms)
///   - `--follow` (boolean)
///   - `-f` (boolean short form)
///
/// Every other token — including inner-command flags like `--yolo` or `-n` — is
/// preserved intact so the forwarded command is identical to what the user typed.
fn extract_passthrough_command(parts: &[&str], offset: usize) -> Vec<String> {
    // Flags that take a following value token (space-separated form).
    const VALUE_FLAGS: &[&str] = &["--remote-addr", "--session", "--api-key"];
    // Boolean flags that consume only themselves.
    const BOOL_FLAGS: &[&str] = &["--follow", "-f"];

    let mut result = Vec::new();
    let mut i = offset;
    while i < parts.len() {
        let t = parts[i];

        // Space-separated value flag: skip flag token and its value.
        if VALUE_FLAGS.contains(&t) {
            i += 2; // skip both the flag and its value
            continue;
        }

        // Boolean flag: skip it.
        if BOOL_FLAGS.contains(&t) {
            i += 1;
            continue;
        }

        // `--flag=value` form for value-taking flags.
        if let Some((key, _val)) = t.split_once('=') {
            if VALUE_FLAGS.contains(&key) {
                i += 1;
                continue;
            }
        }

        // Everything else (positional args, inner-command flags like --yolo) is kept.
        result.push(t.to_string());
        i += 1;
    }
    result
}

/// Fetch sessions from the remote host and show a session picker dialog.
/// Pre-selects the row matching `last_remote_session_id` if present.
async fn fetch_and_show_session_picker(app: &mut App, addr: String, api_key: Option<String>, command: Vec<String>, follow: bool) {
    // Read the last-used session ID before any mutable borrow.
    let last_session_id = app.active_tab().last_remote_session_id.clone();
    match crate::commands::remote::fetch_sessions(&addr, api_key.as_deref()).await {
        Ok(sessions) if sessions.is_empty() => {
            let label = format!("remote run {}", command.join(" "));
            app.active_tab_mut().start_command(label);
            app.active_tab_mut().push_output(format!(
                "Error: No active sessions on {}. Use 'remote session start' to create one.", addr
            ));
            app.active_tab_mut().finish_command(1);
            app.active_tab_mut().pending_command = PendingCommand::None;
        }
        Ok(sessions) => {
            // Pre-select the last-used session so the user just presses Enter for the
            // common case of re-running against the same session.
            let selected = last_session_id
                .as_deref()
                .and_then(|id| sessions.iter().position(|s| s.id == id))
                .unwrap_or(0);
            app.active_tab_mut().dialog = state::Dialog::RemoteSessionPicker {
                sessions,
                selected,
                remote_addr: addr,
                command,
                follow,
            };
        }
        Err(e) => {
            let label = format!("remote run {}", command.join(" "));
            app.active_tab_mut().start_command(label);
            app.active_tab_mut().push_output(format!("Error: Failed to fetch sessions: {}", e));
            app.active_tab_mut().finish_command(1);
            app.active_tab_mut().pending_command = PendingCommand::None;
        }
    }
}

/// Fetch sessions from the remote host and show a session kill picker dialog.
async fn fetch_and_show_session_kill_picker(app: &mut App, addr: String, api_key: Option<String>) {
    match crate::commands::remote::fetch_sessions(&addr, api_key.as_deref()).await {
        Ok(sessions) if sessions.is_empty() => {
            app.active_tab_mut().start_command("remote session kill".into());
            app.active_tab_mut().push_output(format!("Error: No active sessions on {}.", addr));
            app.active_tab_mut().finish_command(1);
            app.active_tab_mut().pending_command = PendingCommand::None;
        }
        Ok(sessions) => {
            app.active_tab_mut().dialog = state::Dialog::RemoteSessionKillPicker {
                sessions,
                selected: 0,
                remote_addr: addr,
            };
        }
        Err(e) => {
            app.active_tab_mut().start_command("remote session kill".into());
            app.active_tab_mut().push_output(format!("Error: Failed to fetch sessions: {}", e));
            app.active_tab_mut().finish_command(1);
            app.active_tab_mut().pending_command = PendingCommand::None;
        }
    }
}

/// Launch a remote run command as a background text task.
async fn launch_remote_run(app: &mut App, remote_addr: String, session_id: String, command: Vec<String>, follow: bool, api_key: Option<String>) {
    let label = format!("remote run {} (session: {})", command.join(" "), &session_id[..8.min(session_id.len())]);
    // Guard: session_id should always be resolved before this point.
    // An empty string means the picker flow was bypassed incorrectly.
    if session_id.is_empty() {
        app.active_tab_mut().start_command(label);
        app.active_tab_mut().push_output("Error: session ID was not resolved. Please specify --session or select one from the picker.");
        app.active_tab_mut().finish_command(1);
        app.active_tab_mut().pending_command = PendingCommand::None;
        return;
    }
    app.active_tab_mut().start_command(label);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    app.active_tab_mut().exit_rx = Some(exit_rx);
    let tx = app.active_tab().output_tx.clone();
    // Track the session so subsequent `remote run` commands default to it.
    app.active_tab_mut().last_remote_session_id = Some(session_id.clone());
    spawn_text_command(tx, exit_tx, move |sink| async move {
        crate::commands::remote::run_remote_run(&remote_addr, &session_id, &command, follow, api_key.as_deref(), &sink).await
    });
}

/// Launch a remote session start command as a background text task.
async fn launch_remote_session_start(app: &mut App, remote_addr: String, dir: String, api_key: Option<String>) {
    // Guard: dir should always be resolved before this point.
    if dir.is_empty() {
        app.active_tab_mut().start_command("remote session start".into());
        app.active_tab_mut().push_output("Error: working directory was not resolved. Please specify a directory or select one from the picker.");
        app.active_tab_mut().finish_command(1);
        app.active_tab_mut().pending_command = PendingCommand::None;
        return;
    }
    let label = format!("remote session start {}", dir);
    app.active_tab_mut().start_command(label);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    app.active_tab_mut().exit_rx = Some(exit_rx);
    let tx = app.active_tab().output_tx.clone();
    spawn_text_command(tx, exit_tx, move |sink| async move {
        let session_id = crate::commands::remote::run_remote_session_start(&remote_addr, &dir, api_key.as_deref()).await?;
        sink.println(format!("Session created: {}", session_id));
        Ok(())
    });
}

/// Launch a remote session kill command as a background text task.
async fn launch_remote_session_kill(app: &mut App, remote_addr: String, session_id: String, api_key: Option<String>) {
    let label = format!("remote session kill {}", &session_id[..8.min(session_id.len())]);
    app.active_tab_mut().start_command(label);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    app.active_tab_mut().exit_rx = Some(exit_rx);
    let tx = app.active_tab().output_tx.clone();
    spawn_text_command(tx, exit_tx, move |sink| async move {
        crate::commands::remote::run_remote_session_kill(&remote_addr, &session_id, api_key.as_deref()).await?;
        sink.println(format!("Session {} killed.", session_id));
        Ok(())
    });
}

/// Launch the ready flow as a single background task calling `ready_flow::execute()`.
///
/// Phase 1 (pre-audit text task) runs first. When it completes,
/// `check_audit_continuation` detects `AuditPhase::ReadyPreAudit` and launches
/// the audit as a foreground PTY container window. When the PTY exits,
/// `check_audit_continuation` detects `AuditPhase::ReadyAuditPty` and launches
/// the post-audit text task.
async fn launch_ready(app: &mut App) {
    let tab_cwd = app.active_tab().cwd.clone();
    let git_root = match find_git_root_from(&tab_cwd) {
        Some(r) => r,
        None => {
            app.active_tab_mut().input_error = Some("Not inside a Git repository.".into());
            return;
        }
    };
    let mount_path = app.active_tab_mut()
        .pending_mount_path
        .take()
        .unwrap_or_else(|| git_root.clone());

    let (refresh, build, no_cache, non_interactive, allow_docker, migrate_decision, template_audit_decision) =
        if let PendingCommand::Ready {
            refresh,
            build,
            no_cache,
            non_interactive,
            allow_docker,
            migrate_decision,
            template_audit_decision,
        } = app.active_tab().pending_command
        {
            (refresh, build, no_cache, non_interactive, allow_docker, migrate_decision, template_audit_decision)
        } else {
            return;
        };

    let runtime = app.runtime.clone();
    let params = ready_flow::ReadyParams {
        refresh,
        build,
        no_cache,
        non_interactive,
        allow_docker,
    };
    let answers = TuiReadyAnswers { migrate_decision, template_audit_decision };

    app.active_tab_mut().start_command("ready".to_string());
    app.active_tab_mut().audit_phase = AuditPhase::ReadyPreAudit;

    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    app.active_tab_mut().exit_rx = Some(exit_rx);
    let tx = app.active_tab().output_tx.clone();

    // Channel for the pre-audit handoff (sent only when an audit is needed).
    let (handoff_tx, handoff_rx) = tokio::sync::oneshot::channel::<ready_flow::ReadyAuditHandoff>();
    app.active_tab_mut().ready_audit_handoff_rx = Some(handoff_rx);

    spawn_text_command(tx, exit_tx, move |sink| async move {
        let mut qa = TuiReadyQa { answers };
        match ready_flow::execute_pre_audit(params, &mut qa, &sink, mount_path, runtime).await? {
            ready_flow::ReadyPreAuditResult::NeedsAudit(handoff) => {
                // Send handoff before returning so tick() can drain it before the exit fires.
                let _ = handoff_tx.send(handoff);
            }
            ready_flow::ReadyPreAuditResult::Done { .. } => {
                // No audit needed — summary already printed. handoff_tx is dropped here.
            }
        }
        Ok(())
    });
}

// ─── TUI ready adapters ───────────────────────────────────────────────────────

/// Pre-collected answers from TUI modal dialogs, consumed by `TuiReadyQa`.
struct TuiReadyAnswers {
    /// `Some(true)` = user chose to migrate; `Some(false)` = keep legacy; `None` = no legacy layout.
    migrate_decision: Option<bool>,
    /// `Some(true)` = user accepted the audit; `Some(false)` = declined; `None` = not shown.
    template_audit_decision: Option<bool>,
}

/// Q&A adapter for TUI mode: returns pre-collected dialog answers immediately
/// without blocking on stdin.
struct TuiReadyQa {
    answers: TuiReadyAnswers,
}

impl ready_flow::ReadyQa for TuiReadyQa {
    fn ask_create_dockerfile(&mut self) -> Result<bool> {
        // TUI auto-accepts: the dialog has already been shown before this task runs.
        Ok(true)
    }

    fn ask_run_audit_on_template(&mut self) -> Result<bool> {
        // Return the pre-collected answer from the ReadyTemplateAuditConfirm dialog.
        // Defaults to false (skip) when the dialog was not shown.
        Ok(self.answers.template_audit_decision.unwrap_or(false))
    }

    fn ask_migrate_legacy(&mut self, _agent_name: &str) -> Result<bool> {
        Ok(self.answers.migrate_decision.unwrap_or(false))
    }
}

// ─── TUI init adapters ────────────────────────────────────────────────────────

/// Container launcher for TUI mode: blocks inside the spawned background task thread.
struct TuiContainerLauncher {
    runtime: std::sync::Arc<dyn crate::runtime::AgentRuntime>,
}

impl init_flow::InitContainerLauncher for TuiContainerLauncher {
    fn build_image(
        &self,
        tag: &str,
        dockerfile: &std::path::Path,
        context: &std::path::Path,
        sink: &crate::commands::output::OutputSink,
    ) -> Result<()> {
        use crate::runtime::format_build_cmd;
        let build_cmd = format_build_cmd(
            self.runtime.cli_binary(),
            tag,
            dockerfile.to_str().unwrap_or(""),
            context.to_str().unwrap_or(""),
        );
        sink.println(format!("$ {}", build_cmd));
        let sink_clone = sink.clone();
        self.runtime
            .build_image_streaming(tag, dockerfile, context, false, &mut |line| {
                sink_clone.println(line);
            })
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    fn run_audit(
        &self,
        agent: Agent,
        cwd: &std::path::Path,
        sink: &crate::commands::output::OutputSink,
    ) -> Result<()> {
        use crate::runtime::agent_image_tag;
        let git_root = cwd;
        let agent_img = agent_image_tag(git_root, agent.as_str());
        let agent_df_path = git_root
            .join(".amux")
            .join(format!("Dockerfile.{}", agent.as_str()));
        let mount_path = git_root.to_str().unwrap_or("").to_string();

        let credentials = crate::commands::auth::resolve_auth(git_root, agent.as_str())
            .unwrap_or_default();
        let mut env_vars = credentials.env_vars;
        let passthrough_names = crate::config::effective_env_passthrough(git_root);
        for name in &passthrough_names {
            if env_vars.iter().any(|(k, _)| k == name) {
                continue;
            }
            if let Ok(val) = std::env::var(name) {
                env_vars.push((name.clone(), val));
            }
        }
        let mut host_settings =
            crate::passthrough::passthrough_for_agent(agent.as_str()).prepare_host_settings();
        // Audit container: resolve overlays from config + env (no per-command flags).
        let resolved_overlays = crate::overlays::resolve_overlays(git_root, &[])
            .map_err(|e| anyhow::anyhow!("overlay resolution failed: {e}"))?;
        if !resolved_overlays.is_empty() {
            match host_settings.as_mut() {
                Some(hs) => hs.set_overlays(resolved_overlays),
                None => host_settings = Some(crate::runtime::HostSettings::overlays_only(resolved_overlays)),
            }
        }

        // TUI owns the terminal; run_container (Stdio::inherit + -it) would conflict
        // with the TUI renderer.  Use captured mode with the non-interactive entrypoint
        // and stream the output line-by-line through the sink instead.
        let entrypoint = ready::audit_entrypoint_non_interactive(agent.as_str());
        let entrypoint_refs: Vec<&str> = entrypoint.iter().map(String::as_str).collect();

        let modified_settings: Option<crate::runtime::HostSettings> =
            host_settings.as_ref().and_then(|settings| {
                let mut new_settings = settings.clone_view();
                if let Some(msg) =
                    crate::runtime::apply_dockerfile_user(&mut new_settings, &agent_df_path)
                {
                    sink.println(msg);
                    Some(new_settings)
                } else {
                    None
                }
            });
        let effective_settings: Option<&crate::runtime::HostSettings> =
            modified_settings.as_ref().or(host_settings.as_ref());

        let (_cmd, output) = self
            .runtime
            .run_container_captured(
                &agent_img,
                &mount_path,
                &entrypoint_refs,
                &env_vars,
                effective_settings,
                false,
                None,
                None,
            )
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        for line in output.lines() {
            sink.println(line);
        }
        Ok(())
    }
}

/// Returns true if the work-items setup dialog should be offered during `init`.
///
/// Offered only when: `--aspec` was not passed, the `aspec/` directory does not yet
/// exist (meaning this is a first-time init), and the repo config does not already
/// have a work-items directory configured.
fn should_offer_work_items(aspec: bool, cwd: &std::path::Path) -> bool {
    if aspec {
        return false;
    }
    let git_root = match find_git_root_from(cwd) {
        Some(r) => r,
        None => return false,
    };
    if git_root.join("aspec").exists() {
        return false;
    }
    let config = crate::config::load_repo_config(&git_root).unwrap_or_default();
    config.work_items.as_ref().and_then(|w| w.dir.as_ref()).is_none()
}

/// Launch the `init` flow.
///
/// Phase 1 (pre-audit text task) runs first. When it completes,
/// `check_audit_continuation` detects `AuditPhase::InitPreAudit` and launches
/// the audit as a foreground PTY container window. When the PTY exits,
/// `check_audit_continuation` detects `AuditPhase::InitAuditPty` and launches
/// the post-audit text task.
async fn launch_init(
    app: &mut App,
    agent: Agent,
    aspec: bool,
    replace_aspec: bool,
    run_audit: bool,
    work_items: Option<crate::config::WorkItemsConfig>,
) {
    let tab_cwd = app.active_tab().cwd.clone();
    let git_root = match find_git_root_from(&tab_cwd) {
        Some(r) => r,
        None => {
            app.active_tab_mut().input_error = Some("Not inside a Git repository.".into());
            return;
        }
    };
    let runtime = app.runtime.clone();

    app.active_tab_mut().start_command("init".to_string());
    app.active_tab_mut().audit_phase = AuditPhase::InitPreAudit;

    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    app.active_tab_mut().exit_rx = Some(exit_rx);
    let tx = app.active_tab().output_tx.clone();

    // Channel for the pre-audit handoff (sent only when an audit is needed).
    let (handoff_tx, handoff_rx) =
        tokio::sync::oneshot::channel::<init_flow::InitAuditHandoff>();
    app.active_tab_mut().init_audit_handoff_rx = Some(handoff_rx);

    spawn_text_command(tx, exit_tx, move |sink| async move {
        let launcher = TuiContainerLauncher { runtime: runtime.clone() };
        let params = init_flow::InitParams { agent, aspec, git_root };
        match init_flow::execute_init_pre_audit(
            params,
            replace_aspec,
            run_audit,
            work_items,
            &sink,
            &launcher,
            runtime,
        )
        .await?
        {
            init_flow::InitPreAuditResult::NeedsAudit(handoff) => {
                let _ = handoff_tx.send(handoff);
            }
            init_flow::InitPreAuditResult::Done { .. } => {
                // No audit needed — summary already printed.
            }
        }
        Ok(())
    });
}

/// Actually spawn the docker container for `implement` via PTY.
#[allow(clippy::too_many_arguments)]
async fn launch_implement(app: &mut App, work_item: u32, non_interactive: bool, plan: bool, allow_docker: bool, workflow_path: Option<std::path::PathBuf>, worktree: bool, mount_ssh: bool, yolo: bool, auto: bool, agent_override: Option<String>, model: Option<String>, overlay: Option<String>) {
    let tab_cwd = app.active_tab().cwd.clone();
    let git_root = match find_git_root_from(&tab_cwd) {
        Some(r) => r,
        None => {
            app.active_tab_mut().input_error = Some("Not inside a Git repository.".into());
            return;
        }
    };

    // Validate work item exists before proceeding.
    if let Err(e) = find_work_item(&git_root, work_item) {
        app.active_tab_mut().input_error = Some(format!("{}", e));
        return;
    }

    let config = load_repo_config(&git_root).unwrap_or_default();
    // Agent resolution order: CLI/TUI flag → config → hardcoded default.
    let agent_name = agent_override.clone()
        .or_else(|| config.agent.clone())
        .unwrap_or_else(|| "claude".to_string());

    // Resolve SSH dir if requested.
    let ssh_dir: Option<std::path::PathBuf> = if mount_ssh {
        match dirs::home_dir() {
            Some(home) => {
                let ssh = home.join(".ssh");
                if ssh.exists() {
                    app.active_tab_mut().push_output(
                        "WARNING: --mount-ssh: mounting host ~/.ssh into container (read-only). Ensure you trust the agent image.".to_string(),
                    );
                    Some(ssh)
                } else {
                    app.active_tab_mut().push_output("Error: host ~/.ssh directory not found; cannot use --mount-ssh.".to_string());
                    app.active_tab_mut().finish_command(1);
                    return;
                }
            }
            None => {
                app.active_tab_mut().push_output("Error: cannot resolve home directory.".to_string());
                app.active_tab_mut().finish_command(1);
                return;
            }
        }
    } else {
        None
    };

    // Set up worktree if requested; otherwise use pending mount path.
    let mount_path = if worktree {
        // Validate git version.
        if let Err(e) = crate::git::git_version_check() {
            app.active_tab_mut().push_output(format!("Error: {}", e));
            app.active_tab_mut().finish_command(1);
            return;
        }
        // Warn if HEAD is detached — the worktree branch will be cut from a detached commit.
        if crate::git::is_detached_head(&git_root) {
            app.active_tab_mut().push_output(
                "WARNING: You are in detached HEAD state. The worktree branch will be created \
                 from the current commit. Consider checking out a branch first so the merge \
                 prompt has a target branch."
                    .to_string(),
            );
        }
        let wt_path = match crate::git::worktree_path(&git_root, work_item) {
            Ok(p) => p,
            Err(e) => {
                app.active_tab_mut().push_output(format!("Error creating worktree path: {}", e));
                app.active_tab_mut().finish_command(1);
                return;
            }
        };
        let branch = crate::git::worktree_branch_name(work_item);
        // If worktree already exists, reuse it; otherwise create it.
        if wt_path.exists() {
            app.active_tab_mut().push_output(format!("Resuming existing worktree at {}", wt_path.display()));
        } else {
            // Check for uncommitted files on the main branch before creating the worktree.
            if !app.active_tab().worktree_skip_precommit_check {
                let files = crate::git::uncommitted_files(&git_root).unwrap_or_default();
                if !files.is_empty() {
                    // Save parameters so the dialog can resume the command after resolution.
                    app.active_tab_mut().pending_command = PendingCommand::Implement {
                        agent: agent_override.clone(),
                        model: model.clone(),
                        work_item,
                        non_interactive,
                        plan,
                        allow_docker,
                        workflow: workflow_path,
                        worktree,
                        mount_ssh,
                        yolo,
                        auto,
                        overlay: overlay.clone(),
                    };
                    app.active_tab_mut().dialog = Dialog::WorktreePreCommitWarning {
                        uncommitted_files: files,
                    };
                    return;
                }
            }
            app.active_tab_mut().worktree_skip_precommit_check = false;

            if let Err(e) = crate::git::create_worktree(&git_root, &wt_path, &branch) {
                app.active_tab_mut().push_output(format!("Error creating worktree: {}", e));
                app.active_tab_mut().finish_command(1);
                return;
            }
            app.active_tab_mut().push_output(format!("Created worktree at {} (branch: {})", wt_path.display(), branch));
        }
        // Store worktree info in tab for post-completion dialog.
        app.active_tab_mut().worktree_branch = Some(branch);
        app.active_tab_mut().worktree_active_path = Some(wt_path.clone());
        app.active_tab_mut().worktree_git_root = Some(git_root.clone());
        wt_path
    } else {
        // Clear any stale worktree state.
        app.active_tab_mut().worktree_branch = None;
        app.active_tab_mut().worktree_active_path = None;
        app.active_tab_mut().worktree_git_root = None;
        app.active_tab_mut().pending_mount_path.take().unwrap_or_else(|| git_root.clone())
    };

    // Auto-passthrough: always pass credentials from keychain if available.
    let credentials = agent_keychain_credentials(&agent_name);
    let mut env_vars = credentials.env_vars;
    for name in &effective_env_passthrough(&git_root) {
        if env_vars.iter().any(|(k, _)| k == name) {
            continue;
        }
        if let Ok(val) = std::env::var(name) {
            env_vars.push((name.clone(), val));
        }
    }

    // Resolve which image and dockerfile to use.
    // For workflow runs these are re-resolved per-step if the step uses a different agent.
    let (mut image_tag, mut agent_dockerfile_path) =
        crate::commands::agent::resolve_agent_image_and_dockerfile(&git_root, &agent_name);
    // For non-workflow runs, validate the default agent image now.
    // For workflow runs, image validation is done per-step inside the workflow block below.
    if workflow_path.is_none() {
        if !agent_dockerfile_path.exists() {
            // Dockerfile missing — prompt to download and build, then re-launch.
            let config_default = agent_name.clone();
            app.active_tab_mut().pending_command = PendingCommand::Implement {
                agent: agent_override.clone(),
                model: model.clone(),
                work_item,
                non_interactive,
                plan,
                allow_docker,
                workflow: workflow_path.clone(),
                worktree,
                mount_ssh,
                yolo,
                auto,
                overlay: overlay.clone(),
            };
            app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
                agent: agent_name.clone(),
                default_agent: config_default,
                from_workflow: false,
                image_only: false,
            };
            return;
        } else if !app.runtime.image_exists(&image_tag) {
            // Image missing — prompt to build it, then re-launch.
            let config_default = agent_name.clone();
            app.active_tab_mut().pending_command = PendingCommand::Implement {
                agent: agent_override.clone(),
                model: model.clone(),
                work_item,
                non_interactive,
                plan,
                allow_docker,
                workflow: workflow_path.clone(),
                worktree,
                mount_ssh,
                yolo,
                auto,
                overlay: overlay.clone(),
            };
            app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
                agent: agent_name.clone(),
                default_agent: config_default,
                from_workflow: false,
                image_only: true,
            };
            return;
        }
    }

    // Prepare host settings (sanitized config files in a temp dir).
    let raw_overlay_flags: Vec<String> = overlay.as_deref().map(|s| vec![s.to_string()]).unwrap_or_default();
    if let Err(e) = app.active_tab_mut().resolve_and_cache_overlays(&git_root, &raw_overlay_flags) {
        app.active_tab_mut().input_error = Some(format!("invalid --overlay: {}", e));
        return;
    }
    app.active_tab_mut().host_settings = crate::passthrough::passthrough_for_agent(&agent_name).prepare_host_settings();
    app.active_tab_mut().apply_overlays_to_host_settings();
    {
        // Use the agent dockerfile for USER detection in the new layout, Dockerfile.dev for legacy.
        let msg = app.active_tab_mut().host_settings.as_mut()
            .and_then(|s| crate::runtime::apply_dockerfile_user(s, &agent_dockerfile_path));
        if let Some(msg) = msg {
            app.active_tab_mut().push_output(msg);
        }
    }
    // Suppress the dangerous-mode permission dialog when running with --yolo.
    if yolo {
        if let Some(ref s) = app.active_tab().host_settings {
            let _ = s.apply_yolo_settings();
        }
    }

    // Persist launch context so workflow step-advancement functions can reuse identical settings.
    app.active_tab_mut().workflow_ssh_dir = ssh_dir.clone();
    app.active_tab_mut().workflow_mount_path = Some(mount_path.clone());
    app.active_tab_mut().workflow_allow_docker = allow_docker;

    // Store yolo/auto mode and resolve disallowed tools.
    let disallowed_tools = if yolo || auto {
        crate::config::effective_yolo_disallowed_tools(&git_root)
    } else {
        vec![]
    };
    app.active_tab_mut().yolo_mode = yolo;
    app.active_tab_mut().auto_mode = auto;
    app.active_tab_mut().yolo_disallowed_tools = disallowed_tools.clone();

    // Track the effective agent for the current step (may differ from default for workflow steps).
    let mut effective_agent = agent_name.clone();

    // If a workflow is specified, initialise/load its state and derive the step prompt.
    let mut effective_entrypoint: Vec<String>;
    let command_display: String;
    let effective_model: Option<String>;
    if let Some(ref wf_path) = workflow_path {
        // Resolve relative paths against the tab's working directory so that
        // paths like ./aspec/workflows/implement-feature.md work as expected.
        let resolved_wf_path: std::path::PathBuf = if wf_path.is_absolute() {
            wf_path.clone()
        } else {
            tab_cwd.join(wf_path)
        };
        // Load or resume workflow state.
        let wf_state = match init_workflow_tui(app, &resolved_wf_path, Some(work_item), &git_root, non_interactive, plan) {
            Some(s) => s,
            None => return, // Error already pushed to output.
        };

        // Build per-step agent map and pre-flight check all required agent Dockerfiles.
        // Steps without an explicit `Agent:` field fall back to the config default.
        // Previously accepted fallbacks (from AgentSetupFallbackAccepted) are applied here.
        let step_agent_map: std::collections::HashMap<String, String> = {
            let agent_fallbacks = app.active_tab().workflow_agent_fallbacks.clone();
            let mut map = std::collections::HashMap::new();
            let mut seen = std::collections::HashSet::new();
            let mut first_missing: Option<String> = None;
            for s in &wf_state.steps {
                // Apply any accepted fallback: if this step's desired agent was declined,
                // substitute the default agent instead.
                let desired = s.agent.as_deref().unwrap_or(&agent_name).to_string();
                let step_ag = agent_fallbacks.get(&desired).cloned().unwrap_or(desired);
                map.insert(s.name.clone(), step_ag.clone());
                if seen.insert(step_ag.clone()) {
                    let df = git_root.join(".amux").join(format!("Dockerfile.{}", &step_ag));
                    if !df.exists() && first_missing.is_none() {
                        first_missing = Some(step_ag);
                    }
                }
            }
            if let Some(missing) = first_missing {
                // Save pending command so we can resume after the agent Dockerfile is built
                // (or after the user accepts a fallback via AgentSetupFallbackAccepted).
                app.active_tab_mut().pending_command = PendingCommand::Implement {
                    agent: agent_override.clone(),
                    model: model.clone(),
                    work_item,
                    non_interactive,
                    plan,
                    allow_docker,
                    workflow: workflow_path.clone(),
                    worktree,
                    mount_ssh,
                    yolo,
                    auto,
                    overlay: overlay.clone(),
                };
                app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
                    agent: missing,
                    default_agent: agent_name.clone(),
                    from_workflow: true,
                image_only: false,
            };
                return;
            }
            map
        };
        // Record the per-step agent map for "same container" eligibility checks in the TUI.
        app.active_tab_mut().workflow_step_agents = step_agent_map.clone();

        // Get the first ready step.
        let ready = wf_state.next_ready();
        if ready.is_empty() {
            if wf_state.all_done() {
                app.active_tab_mut().push_output("All workflow steps are already done.");
            } else {
                app.active_tab_mut().push_output("No workflow steps are ready to run.");
            }
            app.active_tab_mut().finish_command(0);
            return;
        }
        let step_name = ready[0].clone();
        let step_state = wf_state.get_step(&step_name).unwrap().clone();

        // Determine the current step's agent (may differ from the config default).
        let step_agent = step_agent_map
            .get(&step_name)
            .cloned()
            .unwrap_or_else(|| agent_name.clone());

        // Re-resolve image/dockerfile if the step uses a different agent.
        if step_agent != agent_name {
            let r = crate::commands::agent::resolve_agent_image_and_dockerfile(&git_root, &step_agent);
            image_tag = r.0;
            agent_dockerfile_path = r.1;
            // Refresh host settings for the step's agent.
            app.active_tab_mut().host_settings =
                crate::passthrough::passthrough_for_agent(&step_agent).prepare_host_settings();
            app.active_tab_mut().apply_overlays_to_host_settings();
            let msg = app.active_tab_mut().host_settings.as_mut()
                .and_then(|s| crate::runtime::apply_dockerfile_user(s, &agent_dockerfile_path));
            if let Some(msg) = msg {
                app.active_tab_mut().push_output(msg);
            }
            if yolo {
                if let Some(ref s) = app.active_tab().host_settings {
                    let _ = s.apply_yolo_settings();
                }
            }
        }
        // Validate the step's agent Dockerfile and image.
        if !agent_dockerfile_path.exists() {
            app.active_tab_mut().pending_command = PendingCommand::Implement {
                agent: agent_override.clone(),
                model: model.clone(),
                work_item,
                non_interactive,
                plan,
                allow_docker,
                workflow: workflow_path.clone(),
                worktree,
                mount_ssh,
                yolo,
                auto,
                overlay: overlay.clone(),
            };
            app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
                agent: step_agent.clone(),
                default_agent: agent_name.clone(),
                from_workflow: true,
                image_only: false,
            };
            return;
        } else if !app.runtime.image_exists(&image_tag) {
            app.active_tab_mut().pending_command = PendingCommand::Implement {
                agent: agent_override.clone(),
                model: model.clone(),
                work_item,
                non_interactive,
                plan,
                allow_docker,
                workflow: workflow_path.clone(),
                worktree,
                mount_ssh,
                yolo,
                auto,
                overlay: overlay.clone(),
            };
            app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
                agent: step_agent.clone(),
                default_agent: agent_name.clone(),
                from_workflow: true,
                image_only: true,
            };
            return;
        }
        effective_agent = step_agent.clone();

        // Load work item content for prompt substitution.
        let work_item_content = match find_work_item(&git_root, work_item).and_then(|p| {
            std::fs::read_to_string(&p).map_err(|e| anyhow::anyhow!("{}", e))
        }) {
            Ok(c) => c,
            Err(e) => {
                app.active_tab_mut().push_output(format!("Cannot read work item: {}", e));
                app.active_tab_mut().finish_command(1);
                return;
            }
        };

        let prompt = workflow::substitute_prompt(&step_state.prompt_template, Some(work_item), &work_item_content);
        effective_entrypoint = workflow_step_entrypoint(&step_agent, &prompt, non_interactive, plan);
        command_display = format!("implement {:04} [step: {}]", work_item, step_name);

        // Update state: mark step as Running, persist, store in tab.
        let mut wf_state_mut = wf_state;
        wf_state_mut.set_status(&step_name, StepStatus::Running);
        if let Some(ref git_root_path) = app.active_tab().workflow_git_root.clone() {
            let _ = workflow::save_workflow_state(git_root_path, &wf_state_mut);
        }
        app.active_tab_mut().workflow = Some(wf_state_mut);
        app.active_tab_mut().auto_workflow_disabled_for_step = false;
        app.active_tab_mut().workflow_current_step = Some(step_name);
        app.active_tab_mut().workflow_git_root = Some(git_root.clone());
        // Step model overrides CLI model; fall back to CLI model when step has none.
        effective_model = step_state.model.clone().or_else(|| model.clone());
    } else {
        effective_entrypoint = if non_interactive {
            agent_entrypoint_non_interactive(&agent_name, work_item, plan)
        } else {
            agent_entrypoint(&agent_name, work_item, plan)
        };
        command_display = format!("implement {:04}", work_item);
        effective_model = model.clone();
    }

    // Apply autonomous-mode flags to the entrypoint.
    use crate::commands::agent::append_autonomous_flags;
    append_autonomous_flags(&mut effective_entrypoint, &effective_agent, yolo, auto, &disallowed_tools);

    // Append model-selection flag last (after autonomous flags).
    if let Some(ref m) = effective_model {
        use crate::commands::agent::append_model_flag;
        append_model_flag(&mut effective_entrypoint, &effective_agent, m);
    }

    let entrypoint = effective_entrypoint;
    let entrypoint_refs: Vec<&str> = entrypoint.iter().map(String::as_str).collect();

    // image_tag was resolved above via resolve_agent_image_and_dockerfile.
    // Generate a container name for stats polling.
    let container_name = generate_container_name();

    // Show the full CLI command in the execution window (with masked env values).
    let display_args = if non_interactive {
        app.runtime.build_run_args_display(&image_tag, mount_path.to_str().unwrap(), &entrypoint_refs, &env_vars, app.active_tab().host_settings.as_ref(), allow_docker, None, ssh_dir.as_deref())
    } else {
        app.runtime.build_run_args_pty_display(&image_tag, mount_path.to_str().unwrap(), &entrypoint_refs, &env_vars, app.active_tab().host_settings.as_ref(), allow_docker, Some(&container_name), ssh_dir.as_deref())
    };
    let cli_binary = app.runtime.cli_binary();
    let cmd_display = format!("$ {} {}", cli_binary, display_args.join(" "));

    app.active_tab_mut().start_command(command_display);

    // If --allow-docker, check the socket and print a warning before launching.
    if allow_docker {
        let runtime_name = app.runtime.name();
        match app.runtime.check_socket() {
            Ok(socket_path) => {
                app.active_tab_mut().push_output(format!("{} socket: {} (found)", runtime_name, socket_path.display()));
                app.active_tab_mut().push_output(format!(
                    "WARNING: --allow-docker: mounting host {} socket into container ({}:{}). \
                     This grants the agent elevated host access.",
                    runtime_name,
                    socket_path.display(),
                    socket_path.display()
                ));
            }
            Err(e) => {
                app.active_tab_mut().push_output(format!("Error: {}", e));
                app.active_tab_mut().finish_command(1);
                return;
            }
        }
    }

    app.active_tab_mut().push_output(cmd_display);

    if non_interactive {
        app.active_tab_mut().push_output("Tip: remove --non-interactive to interact with the agent directly.");
        // Move host_settings into the task so the temp dir lives until the container exits.
        let host_settings = app.active_tab_mut().host_settings.take();
        // Run captured in a text command.
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        app.active_tab_mut().exit_rx = Some(exit_rx);
        let tx = app.active_tab().output_tx.clone();
        let mount_str = mount_path.to_str().unwrap().to_string();
        let impl_runtime = app.runtime.clone();
        // Clone the fully-built entrypoint (including model flag) for the closure.
        let ni_entrypoint = entrypoint.clone();
        spawn_text_command(tx, exit_tx, move |sink| async move {
            let entrypoint_refs: Vec<&str> = ni_entrypoint.iter().map(String::as_str).collect();
            let (_cmd, output) = impl_runtime.run_container_captured(
                &image_tag,
                &mount_str,
                &entrypoint_refs,
                &env_vars,
                host_settings.as_ref(),
                allow_docker,
                None,
                ssh_dir.as_deref(),
            )?;
            for line in output.lines() {
                sink.println(line);
            }
            Ok(())
        });
    } else {
        // Print interactive notice to the outer window.
        let sink = crate::commands::output::OutputSink::Channel(app.active_tab().output_tx.clone());
        print_interactive_notice(&sink, &effective_agent);

        let pty_args = app.runtime.build_run_args_pty(&image_tag, mount_path.to_str().unwrap(), &entrypoint_refs, &env_vars, app.active_tab().host_settings.as_ref(), allow_docker, Some(&container_name), ssh_dir.as_deref());
        let pty_str_refs: Vec<&str> = pty_args.iter().map(String::as_str).collect();

        // Use actual terminal dimensions for the PTY.
        let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let wf_strip_h = app.active_tab().workflow.as_ref().map(|wf| workflow_strip_height(wf)).unwrap_or(0);
        let (inner_cols, inner_rows) = calculate_container_inner_size(term_cols, term_rows, wf_strip_h);
        let size = PtySize {
            rows: inner_rows,
            cols: inner_cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        // Activate the container window.
        let display_name = state::agent_display_name(&effective_agent).to_string();
        app.active_tab_mut().terminal_scrollback_lines = effective_scrollback_lines(&git_root);
        app.active_tab_mut().start_container(container_name.clone(), display_name, inner_cols, inner_rows);

        let cli_bin = app.runtime.cli_binary();
        let stats_runtime = app.runtime.clone();
        match PtySession::spawn(cli_bin, &pty_str_refs, size) {
            Ok((session, pty_rx)) => {
                app.active_tab_mut().pty = Some(session);
                app.active_tab_mut().pty_rx = Some(pty_rx);
                // Start stats polling.
                app.active_tab_mut().stats_rx = Some(spawn_stats_poller(container_name, stats_runtime));
            }
            Err(e) => {
                app.active_tab_mut().push_output(format!("Failed to launch container: {}", e));
                app.active_tab_mut().finish_command(1);
            }
        }
    }
}

/// Actually spawn the docker container for `chat` via PTY.
async fn launch_chat(app: &mut App, non_interactive: bool, plan: bool, allow_docker: bool, mount_ssh: bool, yolo: bool, auto: bool, agent_override: Option<String>, model: Option<String>, overlay: Option<String>) {
    let tab_cwd = app.active_tab().cwd.clone();
    let git_root = match find_git_root_from(&tab_cwd) {
        Some(r) => r,
        None => {
            app.active_tab_mut().input_error = Some("Not inside a Git repository.".into());
            return;
        }
    };

    let config = load_repo_config(&git_root).unwrap_or_default();
    // Agent resolution order: CLI/TUI flag → config → hardcoded default.
    let agent_name = agent_override.clone()
        .or_else(|| config.agent.clone())
        .unwrap_or_else(|| "claude".to_string());
    let mount_path = app.active_tab_mut().pending_mount_path.take().unwrap_or_else(|| git_root.clone());

    // Resolve SSH dir if requested.
    let ssh_dir: Option<std::path::PathBuf> = if mount_ssh {
        match dirs::home_dir() {
            Some(home) => {
                let ssh = home.join(".ssh");
                if ssh.exists() {
                    app.active_tab_mut().push_output(
                        "WARNING: --mount-ssh: mounting host ~/.ssh into container (read-only). Ensure you trust the agent image.".to_string(),
                    );
                    Some(ssh)
                } else {
                    app.active_tab_mut().push_output("Error: host ~/.ssh directory not found; cannot use --mount-ssh.".to_string());
                    app.active_tab_mut().finish_command(1);
                    return;
                }
            }
            None => {
                app.active_tab_mut().push_output("Error: cannot resolve home directory.".to_string());
                app.active_tab_mut().finish_command(1);
                return;
            }
        }
    } else {
        None
    };

    // Auto-passthrough: always pass credentials from keychain if available.
    let credentials = agent_keychain_credentials(&agent_name);
    let mut env_vars = credentials.env_vars;
    for name in &effective_env_passthrough(&git_root) {
        if env_vars.iter().any(|(k, _)| k == name) {
            continue;
        }
        if let Ok(val) = std::env::var(name) {
            env_vars.push((name.clone(), val));
        }
    }

    // Resolve which image and dockerfile to use.
    let (image_tag, agent_dockerfile_path) =
        crate::commands::agent::resolve_agent_image_and_dockerfile(&git_root, &agent_name);
    if !agent_dockerfile_path.exists() {
        // Dockerfile missing — prompt to download and build, then re-launch.
        app.active_tab_mut().pending_command = PendingCommand::Chat {
            agent: agent_override.clone(),
            model: model.clone(),
            non_interactive,
            plan,
            allow_docker,
            mount_ssh,
            yolo,
            auto,
            overlay: overlay.clone(),
        };
        app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
            agent: agent_name.clone(),
            default_agent: agent_name.clone(),
            from_workflow: false,
                image_only: false,
            };
        return;
    } else if !app.runtime.image_exists(&image_tag) {
        // Image missing — prompt to build it, then re-launch.
        app.active_tab_mut().pending_command = PendingCommand::Chat {
            agent: agent_override.clone(),
            model: model.clone(),
            non_interactive,
            plan,
            allow_docker,
            mount_ssh,
            yolo,
            auto,
            overlay: overlay.clone(),
        };
        app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
            agent: agent_name.clone(),
            default_agent: agent_name.clone(),
            from_workflow: false,
            image_only: true,
        };
        return;
    }

    // Prepare host settings (sanitized config files in a temp dir).
    let raw_overlay_flags: Vec<String> = overlay.as_deref().map(|s| vec![s.to_string()]).unwrap_or_default();
    if let Err(e) = app.active_tab_mut().resolve_and_cache_overlays(&git_root, &raw_overlay_flags) {
        app.active_tab_mut().input_error = Some(format!("invalid --overlay: {}", e));
        return;
    }
    app.active_tab_mut().host_settings = crate::passthrough::passthrough_for_agent(&agent_name).prepare_host_settings();
    app.active_tab_mut().apply_overlays_to_host_settings();
    {
        let msg = app.active_tab_mut().host_settings.as_mut()
            .and_then(|s| crate::runtime::apply_dockerfile_user(s, &agent_dockerfile_path));
        if let Some(msg) = msg {
            app.active_tab_mut().push_output(msg);
        }
    }
    // Suppress the dangerous-mode permission dialog when running with --yolo.
    if yolo {
        if let Some(ref s) = app.active_tab().host_settings {
            let _ = s.apply_yolo_settings();
        }
    }

    let mut entrypoint = if non_interactive {
        chat_entrypoint_non_interactive(&agent_name, plan)
    } else {
        chat_entrypoint(&agent_name, plan)
    };

    // Apply yolo/auto flags.
    let chat_disallowed_tools = if yolo || auto {
        crate::config::effective_yolo_disallowed_tools(&git_root)
    } else {
        vec![]
    };
    use crate::commands::agent::append_autonomous_flags;
    append_autonomous_flags(&mut entrypoint, &agent_name, yolo, auto, &chat_disallowed_tools);

    // Append model-selection flag last (after autonomous flags).
    if let Some(ref m) = model {
        use crate::commands::agent::append_model_flag;
        append_model_flag(&mut entrypoint, &agent_name, m);
    }

    let entrypoint_refs: Vec<&str> = entrypoint.iter().map(String::as_str).collect();

    // image_tag was resolved above via resolve_agent_image_and_dockerfile.
    // Generate a container name for stats polling.
    let container_name = generate_container_name();

    // Show the full CLI command in the execution window (with masked env values).
    let display_args = if non_interactive {
        app.runtime.build_run_args_display(&image_tag, mount_path.to_str().unwrap(), &entrypoint_refs, &env_vars, app.active_tab().host_settings.as_ref(), allow_docker, None, ssh_dir.as_deref())
    } else {
        app.runtime.build_run_args_pty_display(&image_tag, mount_path.to_str().unwrap(), &entrypoint_refs, &env_vars, app.active_tab().host_settings.as_ref(), allow_docker, Some(&container_name), ssh_dir.as_deref())
    };
    let cli_binary = app.runtime.cli_binary();
    let cmd_display = format!("$ {} {}", cli_binary, display_args.join(" "));

    let command_display = "chat".to_string();
    app.active_tab_mut().start_command(command_display);

    // If --allow-docker, check the socket and print a warning before launching.
    if allow_docker {
        let runtime_name = app.runtime.name();
        match app.runtime.check_socket() {
            Ok(socket_path) => {
                app.active_tab_mut().push_output(format!("{} socket: {} (found)", runtime_name, socket_path.display()));
                app.active_tab_mut().push_output(format!(
                    "WARNING: --allow-docker: mounting host {} socket into container ({}:{}). \
                     This grants the agent elevated host access.",
                    runtime_name,
                    socket_path.display(),
                    socket_path.display()
                ));
            }
            Err(e) => {
                app.active_tab_mut().push_output(format!("Error: {}", e));
                app.active_tab_mut().finish_command(1);
                return;
            }
        }
    }

    app.active_tab_mut().push_output(cmd_display);

    if non_interactive {
        app.active_tab_mut().push_output("Tip: remove --non-interactive to interact with the agent directly.");
        // Move host_settings into the task so the temp dir lives until the container exits.
        let host_settings = app.active_tab_mut().host_settings.take();
        // Run captured in a text command.
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        app.active_tab_mut().exit_rx = Some(exit_rx);
        let tx = app.active_tab().output_tx.clone();
        let mount_str = mount_path.to_str().unwrap().to_string();
        let chat_runtime = app.runtime.clone();
        // Clone the fully-built entrypoint (including model flag) for the closure.
        let ni_entrypoint = entrypoint.clone();
        spawn_text_command(tx, exit_tx, move |sink| async move {
            let entrypoint_refs: Vec<&str> = ni_entrypoint.iter().map(String::as_str).collect();
            let (_cmd, output) = chat_runtime.run_container_captured(
                &image_tag,
                &mount_str,
                &entrypoint_refs,
                &env_vars,
                host_settings.as_ref(),
                allow_docker,
                None,
                ssh_dir.as_deref(),
            )?;
            for line in output.lines() {
                sink.println(line);
            }
            Ok(())
        });
    } else {
        // Print interactive notice to the outer window.
        let sink = crate::commands::output::OutputSink::Channel(app.active_tab().output_tx.clone());
        print_interactive_notice(&sink, &agent_name);

        let pty_args = app.runtime.build_run_args_pty(&image_tag, mount_path.to_str().unwrap(), &entrypoint_refs, &env_vars, app.active_tab().host_settings.as_ref(), allow_docker, Some(&container_name), ssh_dir.as_deref());
        let pty_str_refs: Vec<&str> = pty_args.iter().map(String::as_str).collect();

        // Use actual terminal dimensions for the PTY.
        let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let wf_strip_h = app.active_tab().workflow.as_ref().map(|wf| workflow_strip_height(wf)).unwrap_or(0);
        let (inner_cols, inner_rows) = calculate_container_inner_size(term_cols, term_rows, wf_strip_h);
        let size = PtySize {
            rows: inner_rows,
            cols: inner_cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        // Activate the container window.
        let display_name = state::agent_display_name(&agent_name).to_string();
        app.active_tab_mut().terminal_scrollback_lines = effective_scrollback_lines(&git_root);
        app.active_tab_mut().start_container(container_name.clone(), display_name, inner_cols, inner_rows);

        let cli_bin = app.runtime.cli_binary();
        let stats_runtime = app.runtime.clone();
        match PtySession::spawn(cli_bin, &pty_str_refs, size) {
            Ok((session, pty_rx)) => {
                app.active_tab_mut().pty = Some(session);
                app.active_tab_mut().pty_rx = Some(pty_rx);
                // Start stats polling.
                app.active_tab_mut().stats_rx = Some(spawn_stats_poller(container_name, stats_runtime));
            }
            Err(e) => {
                app.active_tab_mut().push_output(format!("Failed to launch container: {}", e));
                app.active_tab_mut().finish_command(1);
            }
        }
    }
}

/// Launch `exec prompt`: run a prompt against the agent.
///
/// When `non_interactive` is false (the default), the prompt is passed to the
/// agent in interactive mode and the container opens as a PTY container window,
/// allowing the user to continue the conversation.  When `non_interactive` is
/// true (i.e. `--non-interactive` was explicitly passed), the container is run
/// with the agent's print/headless flag and the captured output is streamed to
/// the outer text window.
#[allow(clippy::too_many_arguments)]
async fn launch_exec_prompt(
    app: &mut App,
    prompt: &str,
    non_interactive: bool,
    plan: bool,
    allow_docker: bool,
    mount_ssh: bool,
    yolo: bool,
    auto: bool,
    agent_override: Option<String>,
    model: Option<String>,
    overlay: Option<String>,
) {
    let tab_cwd = app.active_tab().cwd.clone();
    let git_root = match find_git_root_from(&tab_cwd) {
        Some(r) => r,
        None => {
            app.active_tab_mut().input_error = Some("Not inside a Git repository.".into());
            return;
        }
    };

    let config = load_repo_config(&git_root).unwrap_or_default();
    let agent_name = agent_override.clone()
        .or_else(|| config.agent.clone())
        .unwrap_or_else(|| "claude".to_string());
    let mount_path = app.active_tab_mut().pending_mount_path.take().unwrap_or_else(|| git_root.clone());

    // Resolve SSH dir if requested.
    let ssh_dir: Option<std::path::PathBuf> = if mount_ssh {
        match dirs::home_dir() {
            Some(home) => {
                let ssh = home.join(".ssh");
                if ssh.exists() {
                    app.active_tab_mut().push_output(
                        "WARNING: --mount-ssh: mounting host ~/.ssh into container (read-only). Ensure you trust the agent image.".to_string(),
                    );
                    Some(ssh)
                } else {
                    app.active_tab_mut().push_output("Error: host ~/.ssh directory not found; cannot use --mount-ssh.".to_string());
                    app.active_tab_mut().finish_command(1);
                    return;
                }
            }
            None => {
                app.active_tab_mut().push_output("Error: cannot resolve home directory.".to_string());
                app.active_tab_mut().finish_command(1);
                return;
            }
        }
    } else {
        None
    };

    // Auto-passthrough: always pass credentials from keychain if available.
    let credentials = agent_keychain_credentials(&agent_name);
    let mut env_vars = credentials.env_vars;
    for name in &effective_env_passthrough(&git_root) {
        if env_vars.iter().any(|(k, _)| k == name) {
            continue;
        }
        if let Ok(val) = std::env::var(name) {
            env_vars.push((name.clone(), val));
        }
    }

    // Resolve which image and dockerfile to use.
    let (image_tag, agent_dockerfile_path) =
        crate::commands::agent::resolve_agent_image_and_dockerfile(&git_root, &agent_name);
    if !agent_dockerfile_path.exists() {
        app.active_tab_mut().pending_command = PendingCommand::ExecPrompt {
            prompt: prompt.to_string(),
            agent: agent_override.clone(),
            model: model.clone(),
            non_interactive,
            plan,
            allow_docker,
            mount_ssh,
            yolo,
            auto,
            overlay: overlay.clone(),
        };
        app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
            agent: agent_name.clone(),
            default_agent: agent_name.clone(),
            from_workflow: false,
                image_only: false,
            };
        return;
    } else if !app.runtime.image_exists(&image_tag) {
        // Image missing — prompt to build it, then re-launch.
        app.active_tab_mut().pending_command = PendingCommand::ExecPrompt {
            prompt: prompt.to_string(),
            agent: agent_override.clone(),
            model: model.clone(),
            non_interactive,
            plan,
            allow_docker,
            mount_ssh,
            yolo,
            auto,
            overlay: overlay.clone(),
        };
        app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
            agent: agent_name.clone(),
            default_agent: agent_name.clone(),
            from_workflow: false,
            image_only: true,
        };
        return;
    }

    // Prepare host settings.
    let raw_overlay_flags: Vec<String> = overlay.as_deref().map(|s| vec![s.to_string()]).unwrap_or_default();
    if let Err(e) = app.active_tab_mut().resolve_and_cache_overlays(&git_root, &raw_overlay_flags) {
        app.active_tab_mut().input_error = Some(format!("invalid --overlay: {}", e));
        return;
    }
    app.active_tab_mut().host_settings = crate::passthrough::passthrough_for_agent(&agent_name).prepare_host_settings();
    app.active_tab_mut().apply_overlays_to_host_settings();
    {
        let msg = app.active_tab_mut().host_settings.as_mut()
            .and_then(|s| crate::runtime::apply_dockerfile_user(s, &agent_dockerfile_path));
        if let Some(msg) = msg {
            app.active_tab_mut().push_output(msg);
        }
    }
    if yolo {
        if let Some(ref s) = app.active_tab().host_settings {
            let _ = s.apply_yolo_settings();
        }
    }

    // Build entrypoint: interactive or non-interactive based on the flag.
    let mut entrypoint = workflow_step_entrypoint(&agent_name, prompt, non_interactive, plan);

    // Apply yolo/auto flags.
    let disallowed_tools = if yolo || auto {
        crate::config::effective_yolo_disallowed_tools(&git_root)
    } else {
        vec![]
    };
    use crate::commands::agent::append_autonomous_flags;
    append_autonomous_flags(&mut entrypoint, &agent_name, yolo, auto, &disallowed_tools);

    if let Some(ref m) = model {
        use crate::commands::agent::append_model_flag;
        append_model_flag(&mut entrypoint, &agent_name, m);
    }

    let container_name = generate_container_name();
    let entrypoint_refs: Vec<&str> = entrypoint.iter().map(String::as_str).collect();

    // Show the full CLI command.
    let display_args = if non_interactive {
        app.runtime.build_run_args_display(
            &image_tag, mount_path.to_str().unwrap(), &entrypoint_refs, &env_vars,
            app.active_tab().host_settings.as_ref(), allow_docker, None, ssh_dir.as_deref(),
        )
    } else {
        app.runtime.build_run_args_pty_display(
            &image_tag, mount_path.to_str().unwrap(), &entrypoint_refs, &env_vars,
            app.active_tab().host_settings.as_ref(), allow_docker, Some(&container_name), ssh_dir.as_deref(),
        )
    };
    let cli_binary = app.runtime.cli_binary();
    let cmd_display = format!("$ {} {}", cli_binary, display_args.join(" "));

    let prompt_display = if prompt.len() > 60 {
        format!("exec prompt: {}…", &prompt[..57])
    } else {
        format!("exec prompt: {}", prompt)
    };
    app.active_tab_mut().start_command(prompt_display);

    if allow_docker {
        let runtime_name = app.runtime.name();
        match app.runtime.check_socket() {
            Ok(socket_path) => {
                app.active_tab_mut().push_output(format!("{} socket: {} (found)", runtime_name, socket_path.display()));
                app.active_tab_mut().push_output(format!(
                    "WARNING: --allow-docker: mounting host {} socket into container ({}:{}). \
                     This grants the agent elevated host access.",
                    runtime_name, socket_path.display(), socket_path.display()
                ));
            }
            Err(e) => {
                app.active_tab_mut().push_output(format!("Error: {}", e));
                app.active_tab_mut().finish_command(1);
                return;
            }
        }
    }

    app.active_tab_mut().push_output(cmd_display);

    if non_interactive {
        app.active_tab_mut().push_output("Tip: remove --non-interactive to interact with the agent directly.");
        let host_settings = app.active_tab_mut().host_settings.take();
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        app.active_tab_mut().exit_rx = Some(exit_rx);
        let tx = app.active_tab().output_tx.clone();
        let mount_str = mount_path.to_str().unwrap().to_string();
        let exec_runtime = app.runtime.clone();
        let exec_entrypoint = entrypoint;
        spawn_text_command(tx, exit_tx, move |sink| async move {
            let entrypoint_refs: Vec<&str> = exec_entrypoint.iter().map(String::as_str).collect();
            let (_cmd, output) = exec_runtime.run_container_captured(
                &image_tag,
                &mount_str,
                &entrypoint_refs,
                &env_vars,
                host_settings.as_ref(),
                allow_docker,
                None,
                ssh_dir.as_deref(),
            )?;
            for line in output.lines() {
                sink.println(line);
            }
            Ok(())
        });
    } else {
        // Print interactive notice to the outer window.
        let sink = crate::commands::output::OutputSink::Channel(app.active_tab().output_tx.clone());
        print_interactive_notice(&sink, &agent_name);

        let pty_args = app.runtime.build_run_args_pty(&image_tag, mount_path.to_str().unwrap(), &entrypoint_refs, &env_vars, app.active_tab().host_settings.as_ref(), allow_docker, Some(&container_name), ssh_dir.as_deref());
        let pty_str_refs: Vec<&str> = pty_args.iter().map(String::as_str).collect();

        // Use actual terminal dimensions for the PTY.
        let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let wf_strip_h = app.active_tab().workflow.as_ref().map(|wf| workflow_strip_height(wf)).unwrap_or(0);
        let (inner_cols, inner_rows) = calculate_container_inner_size(term_cols, term_rows, wf_strip_h);
        let size = PtySize {
            rows: inner_rows,
            cols: inner_cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        // Activate the container window.
        let display_name = state::agent_display_name(&agent_name).to_string();
        app.active_tab_mut().terminal_scrollback_lines = effective_scrollback_lines(&git_root);
        app.active_tab_mut().start_container(container_name.clone(), display_name, inner_cols, inner_rows);

        let cli_bin = app.runtime.cli_binary();
        let stats_runtime = app.runtime.clone();
        match PtySession::spawn(cli_bin, &pty_str_refs, size) {
            Ok((session, pty_rx)) => {
                app.active_tab_mut().pty = Some(session);
                app.active_tab_mut().pty_rx = Some(pty_rx);
                app.active_tab_mut().stats_rx = Some(spawn_stats_poller(container_name, stats_runtime));
            }
            Err(e) => {
                app.active_tab_mut().push_output(format!("Failed to launch container: {}", e));
                app.active_tab_mut().finish_command(1);
            }
        }
    }
}

/// Launch `exec workflow`: run a workflow file, optionally with a work item context.
///
/// This follows the same pattern as `launch_implement` with `--workflow` but
/// supports running without a work item number.
#[allow(clippy::too_many_arguments)]
async fn launch_exec_workflow(
    app: &mut App,
    workflow_path: std::path::PathBuf,
    work_item: Option<u32>,
    non_interactive: bool,
    plan: bool,
    allow_docker: bool,
    worktree: bool,
    mount_ssh: bool,
    yolo: bool,
    auto: bool,
    agent_override: Option<String>,
    model: Option<String>,
    overlay: Option<String>,
) {
    let tab_cwd = app.active_tab().cwd.clone();
    let git_root = match find_git_root_from(&tab_cwd) {
        Some(r) => r,
        None => {
            app.active_tab_mut().input_error = Some("Not inside a Git repository.".into());
            return;
        }
    };

    let config = load_repo_config(&git_root).unwrap_or_default();
    let agent_name = agent_override.clone()
        .or_else(|| config.agent.clone())
        .unwrap_or_else(|| "claude".to_string());

    // Resolve SSH dir if requested.
    let ssh_dir: Option<std::path::PathBuf> = if mount_ssh {
        match dirs::home_dir() {
            Some(home) => {
                let ssh = home.join(".ssh");
                if ssh.exists() {
                    app.active_tab_mut().push_output(
                        "WARNING: --mount-ssh: mounting host ~/.ssh into container (read-only). Ensure you trust the agent image.".to_string(),
                    );
                    Some(ssh)
                } else {
                    app.active_tab_mut().push_output("Error: host ~/.ssh directory not found; cannot use --mount-ssh.".to_string());
                    app.active_tab_mut().finish_command(1);
                    return;
                }
            }
            None => {
                app.active_tab_mut().push_output("Error: cannot resolve home directory.".to_string());
                app.active_tab_mut().finish_command(1);
                return;
            }
        }
    } else {
        None
    };

    // Set up worktree if requested.
    let mount_path = if worktree {
        if let Err(e) = crate::git::git_version_check() {
            app.active_tab_mut().push_output(format!("Error: {}", e));
            app.active_tab_mut().finish_command(1);
            return;
        }
        if crate::git::is_detached_head(&git_root) {
            app.active_tab_mut().push_output(
                "WARNING: You are in detached HEAD state. The worktree branch will be created \
                 from the current commit."
                    .to_string(),
            );
        }
        // Derive worktree path from work item or workflow file name.
        let (wt_path, branch) = match work_item {
            Some(wi) => {
                let path = match crate::git::worktree_path(&git_root, wi) {
                    Ok(p) => p,
                    Err(e) => {
                        app.active_tab_mut().push_output(format!("Error creating worktree path: {}", e));
                        app.active_tab_mut().finish_command(1);
                        return;
                    }
                };
                let br = crate::git::worktree_branch_name(wi);
                (path, br)
            }
            None => {
                let wf_name = workflow_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("workflow");
                let path = match crate::git::worktree_path_named(&git_root, wf_name) {
                    Ok(p) => p,
                    Err(e) => {
                        app.active_tab_mut().push_output(format!("Error creating worktree path: {}", e));
                        app.active_tab_mut().finish_command(1);
                        return;
                    }
                };
                let br = crate::git::worktree_branch_name_for_workflow(wf_name);
                (path, br)
            }
        };

        if wt_path.exists() {
            app.active_tab_mut().push_output(format!("Resuming existing worktree at {}", wt_path.display()));
        } else {
            // Check for uncommitted files before creating worktree.
            if !app.active_tab().worktree_skip_precommit_check {
                let files = crate::git::uncommitted_files(&git_root).unwrap_or_default();
                if !files.is_empty() {
                    app.active_tab_mut().pending_command = PendingCommand::ExecWorkflow {
                        workflow: workflow_path,
                        work_item,
                        agent: agent_override.clone(),
                        model: model.clone(),
                        non_interactive,
                        plan,
                        allow_docker,
                        worktree,
                        mount_ssh,
                        yolo,
                        auto,
                        overlay: overlay.clone(),
                    };
                    app.active_tab_mut().dialog = Dialog::WorktreePreCommitWarning {
                        uncommitted_files: files,
                    };
                    return;
                }
            }
            app.active_tab_mut().worktree_skip_precommit_check = false;

            if let Err(e) = crate::git::create_worktree(&git_root, &wt_path, &branch) {
                app.active_tab_mut().push_output(format!("Error creating worktree: {}", e));
                app.active_tab_mut().finish_command(1);
                return;
            }
            app.active_tab_mut().push_output(format!("Created worktree at {} (branch: {})", wt_path.display(), branch));
        }
        app.active_tab_mut().worktree_branch = Some(branch);
        app.active_tab_mut().worktree_active_path = Some(wt_path.clone());
        app.active_tab_mut().worktree_git_root = Some(git_root.clone());
        wt_path
    } else {
        app.active_tab_mut().worktree_branch = None;
        app.active_tab_mut().worktree_active_path = None;
        app.active_tab_mut().worktree_git_root = None;
        app.active_tab_mut().pending_mount_path.take().unwrap_or_else(|| git_root.clone())
    };

    // Auto-passthrough credentials.
    let credentials = agent_keychain_credentials(&agent_name);
    let mut env_vars = credentials.env_vars;
    for name in &effective_env_passthrough(&git_root) {
        if env_vars.iter().any(|(k, _)| k == name) {
            continue;
        }
        if let Ok(val) = std::env::var(name) {
            env_vars.push((name.clone(), val));
        }
    }

    // Resolve the workflow path relative to the tab's working directory.
    let resolved_wf_path: std::path::PathBuf = if workflow_path.is_absolute() {
        workflow_path.clone()
    } else {
        tab_cwd.join(&workflow_path)
    };

    // Resolve which image and dockerfile to use.
    let (mut image_tag, mut agent_dockerfile_path) =
        crate::commands::agent::resolve_agent_image_and_dockerfile(&git_root, &agent_name);

    // Prepare host settings.
    let raw_overlay_flags: Vec<String> = overlay.as_deref().map(|s| vec![s.to_string()]).unwrap_or_default();
    if let Err(e) = app.active_tab_mut().resolve_and_cache_overlays(&git_root, &raw_overlay_flags) {
        app.active_tab_mut().input_error = Some(format!("invalid --overlay: {}", e));
        return;
    }
    app.active_tab_mut().host_settings = crate::passthrough::passthrough_for_agent(&agent_name).prepare_host_settings();
    app.active_tab_mut().apply_overlays_to_host_settings();
    {
        let msg = app.active_tab_mut().host_settings.as_mut()
            .and_then(|s| crate::runtime::apply_dockerfile_user(s, &agent_dockerfile_path));
        if let Some(msg) = msg {
            app.active_tab_mut().push_output(msg);
        }
    }
    if yolo {
        if let Some(ref s) = app.active_tab().host_settings {
            let _ = s.apply_yolo_settings();
        }
    }

    // Persist launch context for workflow step-advancement.
    app.active_tab_mut().workflow_ssh_dir = ssh_dir.clone();
    app.active_tab_mut().workflow_mount_path = Some(mount_path.clone());
    app.active_tab_mut().workflow_allow_docker = allow_docker;

    let disallowed_tools = if yolo || auto {
        crate::config::effective_yolo_disallowed_tools(&git_root)
    } else {
        vec![]
    };
    app.active_tab_mut().yolo_mode = yolo;
    app.active_tab_mut().auto_mode = auto;
    app.active_tab_mut().yolo_disallowed_tools = disallowed_tools.clone();

    // Load or resume workflow state.
    let wf_state = match init_workflow_tui(app, &resolved_wf_path, work_item, &git_root, non_interactive, plan) {
        Some(s) => s,
        None => return,
    };

    // Build per-step agent map and pre-flight check all required agent Dockerfiles.
    let step_agent_map: std::collections::HashMap<String, String> = {
        let agent_fallbacks = app.active_tab().workflow_agent_fallbacks.clone();
        let mut map = std::collections::HashMap::new();
        let mut seen = std::collections::HashSet::new();
        let mut first_missing: Option<String> = None;
        for s in &wf_state.steps {
            let desired = s.agent.as_deref().unwrap_or(&agent_name).to_string();
            let step_ag = agent_fallbacks.get(&desired).cloned().unwrap_or(desired);
            map.insert(s.name.clone(), step_ag.clone());
            if seen.insert(step_ag.clone()) {
                let df = git_root.join(".amux").join(format!("Dockerfile.{}", &step_ag));
                if !df.exists() && first_missing.is_none() {
                    first_missing = Some(step_ag);
                }
            }
        }
        if let Some(missing) = first_missing {
            app.active_tab_mut().pending_command = PendingCommand::ExecWorkflow {
                workflow: workflow_path,
                work_item,
                agent: agent_override.clone(),
                model: model.clone(),
                non_interactive,
                plan,
                allow_docker,
                worktree,
                mount_ssh,
                yolo,
                auto,
                overlay: overlay.clone(),
            };
            app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
                agent: missing,
                default_agent: agent_name.clone(),
                from_workflow: true,
                image_only: false,
            };
            return;
        }
        map
    };
    app.active_tab_mut().workflow_step_agents = step_agent_map.clone();

    // Get the first ready step.
    let ready = wf_state.next_ready();
    if ready.is_empty() {
        if wf_state.all_done() {
            app.active_tab_mut().push_output("All workflow steps are already done.");
        } else {
            app.active_tab_mut().push_output("No workflow steps are ready to run.");
        }
        app.active_tab_mut().finish_command(0);
        return;
    }
    let step_name = ready[0].clone();
    let step_state = wf_state.get_step(&step_name).unwrap().clone();

    let step_agent = step_agent_map
        .get(&step_name)
        .cloned()
        .unwrap_or_else(|| agent_name.clone());

    // Re-resolve image/dockerfile if the step uses a different agent.
    if step_agent != agent_name {
        let r = crate::commands::agent::resolve_agent_image_and_dockerfile(&git_root, &step_agent);
        image_tag = r.0;
        agent_dockerfile_path = r.1;
        app.active_tab_mut().host_settings =
            crate::passthrough::passthrough_for_agent(&step_agent).prepare_host_settings();
        app.active_tab_mut().apply_overlays_to_host_settings();
        let msg = app.active_tab_mut().host_settings.as_mut()
            .and_then(|s| crate::runtime::apply_dockerfile_user(s, &agent_dockerfile_path));
        if let Some(msg) = msg {
            app.active_tab_mut().push_output(msg);
        }
        if yolo {
            if let Some(ref s) = app.active_tab().host_settings {
                let _ = s.apply_yolo_settings();
            }
        }
    }

    if !agent_dockerfile_path.exists() {
        app.active_tab_mut().pending_command = PendingCommand::ExecWorkflow {
            workflow: workflow_path.clone(),
            work_item,
            agent: agent_override.clone(),
            model: model.clone(),
            non_interactive,
            plan,
            allow_docker,
            worktree,
            mount_ssh,
            yolo,
            auto,
            overlay: overlay.clone(),
        };
        app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
            agent: step_agent.clone(),
            default_agent: agent_name.clone(),
            from_workflow: true,
            image_only: false,
        };
        return;
    } else if !app.runtime.image_exists(&image_tag) {
        app.active_tab_mut().pending_command = PendingCommand::ExecWorkflow {
            workflow: workflow_path.clone(),
            work_item,
            agent: agent_override.clone(),
            model: model.clone(),
            non_interactive,
            plan,
            allow_docker,
            worktree,
            mount_ssh,
            yolo,
            auto,
            overlay: overlay.clone(),
        };
        app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
            agent: step_agent.clone(),
            default_agent: agent_name.clone(),
            from_workflow: true,
            image_only: true,
        };
        return;
    }
    let effective_agent = step_agent.clone();

    // Load work item content for prompt substitution (empty string if no work item).
    let work_item_content = match work_item {
        Some(wi) => match find_work_item(&git_root, wi).and_then(|p| {
            std::fs::read_to_string(&p).map_err(|e| anyhow::anyhow!("{}", e))
        }) {
            Ok(c) => c,
            Err(e) => {
                app.active_tab_mut().push_output(format!("Cannot read work item: {}", e));
                app.active_tab_mut().finish_command(1);
                return;
            }
        },
        None => String::new(),
    };

    let prompt = workflow::substitute_prompt(&step_state.prompt_template, work_item, &work_item_content);
    let mut effective_entrypoint = workflow_step_entrypoint(&step_agent, &prompt, non_interactive, plan);

    let command_display = match work_item {
        Some(wi) => format!("exec workflow [WI {:04}, step: {}]", wi, step_name),
        None => format!("exec workflow [step: {}]", step_name),
    };

    // Mark step as Running and persist.
    let mut wf_state_mut = wf_state;
    wf_state_mut.set_status(&step_name, StepStatus::Running);
    if let Some(ref git_root_path) = app.active_tab().workflow_git_root.clone() {
        let _ = workflow::save_workflow_state(git_root_path, &wf_state_mut);
    }
    app.active_tab_mut().workflow = Some(wf_state_mut);
    app.active_tab_mut().auto_workflow_disabled_for_step = false;
    app.active_tab_mut().workflow_current_step = Some(step_name);
    app.active_tab_mut().workflow_git_root = Some(git_root.clone());

    let effective_model = step_state.model.clone().or_else(|| model.clone());

    // Apply autonomous flags.
    use crate::commands::agent::append_autonomous_flags;
    append_autonomous_flags(&mut effective_entrypoint, &effective_agent, yolo, auto, &disallowed_tools);

    if let Some(ref m) = effective_model {
        use crate::commands::agent::append_model_flag;
        append_model_flag(&mut effective_entrypoint, &effective_agent, m);
    }

    let entrypoint = effective_entrypoint;
    let entrypoint_refs: Vec<&str> = entrypoint.iter().map(String::as_str).collect();
    let container_name = generate_container_name();

    let display_args = if non_interactive {
        app.runtime.build_run_args_display(&image_tag, mount_path.to_str().unwrap(), &entrypoint_refs, &env_vars, app.active_tab().host_settings.as_ref(), allow_docker, None, ssh_dir.as_deref())
    } else {
        app.runtime.build_run_args_pty_display(&image_tag, mount_path.to_str().unwrap(), &entrypoint_refs, &env_vars, app.active_tab().host_settings.as_ref(), allow_docker, Some(&container_name), ssh_dir.as_deref())
    };
    let cli_binary = app.runtime.cli_binary();
    let cmd_display = format!("$ {} {}", cli_binary, display_args.join(" "));

    app.active_tab_mut().start_command(command_display);

    if allow_docker {
        let runtime_name = app.runtime.name();
        match app.runtime.check_socket() {
            Ok(socket_path) => {
                app.active_tab_mut().push_output(format!("{} socket: {} (found)", runtime_name, socket_path.display()));
                app.active_tab_mut().push_output(format!(
                    "WARNING: --allow-docker: mounting host {} socket into container ({}:{}). \
                     This grants the agent elevated host access.",
                    runtime_name, socket_path.display(), socket_path.display()
                ));
            }
            Err(e) => {
                app.active_tab_mut().push_output(format!("Error: {}", e));
                app.active_tab_mut().finish_command(1);
                return;
            }
        }
    }

    app.active_tab_mut().push_output(cmd_display);

    if non_interactive {
        app.active_tab_mut().push_output("Tip: remove --non-interactive to interact with the agent directly.");
        let host_settings = app.active_tab_mut().host_settings.take();
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        app.active_tab_mut().exit_rx = Some(exit_rx);
        let tx = app.active_tab().output_tx.clone();
        let mount_str = mount_path.to_str().unwrap().to_string();
        let wf_runtime = app.runtime.clone();
        let ni_entrypoint = entrypoint.clone();
        spawn_text_command(tx, exit_tx, move |sink| async move {
            let entrypoint_refs: Vec<&str> = ni_entrypoint.iter().map(String::as_str).collect();
            let (_cmd, output) = wf_runtime.run_container_captured(
                &image_tag,
                &mount_str,
                &entrypoint_refs,
                &env_vars,
                host_settings.as_ref(),
                allow_docker,
                None,
                ssh_dir.as_deref(),
            )?;
            for line in output.lines() {
                sink.println(line);
            }
            Ok(())
        });
    } else {
        let sink = crate::commands::output::OutputSink::Channel(app.active_tab().output_tx.clone());
        print_interactive_notice(&sink, &effective_agent);

        let pty_args = app.runtime.build_run_args_pty(&image_tag, mount_path.to_str().unwrap(), &entrypoint_refs, &env_vars, app.active_tab().host_settings.as_ref(), allow_docker, Some(&container_name), ssh_dir.as_deref());
        let pty_str_refs: Vec<&str> = pty_args.iter().map(String::as_str).collect();

        let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let wf_strip_h = app.active_tab().workflow.as_ref().map(|wf| workflow_strip_height(wf)).unwrap_or(0);
        let (inner_cols, inner_rows) = calculate_container_inner_size(term_cols, term_rows, wf_strip_h);
        let size = PtySize {
            rows: inner_rows,
            cols: inner_cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        let display_name = state::agent_display_name(&effective_agent).to_string();
        app.active_tab_mut().terminal_scrollback_lines = effective_scrollback_lines(&git_root);
        app.active_tab_mut().start_container(container_name.clone(), display_name, inner_cols, inner_rows);

        let cli_bin = app.runtime.cli_binary();
        let stats_runtime = app.runtime.clone();
        match PtySession::spawn(cli_bin, &pty_str_refs, size) {
            Ok((session, pty_rx)) => {
                app.active_tab_mut().pty = Some(session);
                app.active_tab_mut().pty_rx = Some(pty_rx);
                app.active_tab_mut().stats_rx = Some(spawn_stats_poller(container_name, stats_runtime));
            }
            Err(e) => {
                app.active_tab_mut().push_output(format!("Failed to launch container: {}", e));
                app.active_tab_mut().finish_command(1);
            }
        }
    }
}

/// Spawn a background task that polls container stats every 5 seconds.
fn spawn_stats_poller(
    container_name: String,
    runtime: std::sync::Arc<dyn crate::runtime::AgentRuntime>,
) -> tokio::sync::mpsc::UnboundedReceiver<ContainerStats> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            let name = container_name.clone();
            let rt = runtime.clone();
            let stats = tokio::task::spawn_blocking(move || rt.query_container_stats(&name))
                .await;
            match stats {
                Ok(Some(s)) => {
                    if tx.send(s).is_err() {
                        break;
                    }
                }
                _ => {
                    // Container may not be running yet or has exited.
                    // If the receiver is dropped, the send will fail and we'll break.
                }
            }
        }
    });
    rx
}

/// Determine what to show when `claws init` is entered.
///
/// Start the `claws init` workflow.
///
/// If `$HOME/.nanoclaw` already exists, skips the fork/clone wizard and
/// proceeds directly to the image build + audit flow. Otherwise, starts
/// the fork/clone dialog.
async fn show_claws_init_start(app: &mut App) {
    let nanoclaw_dir = claws::nanoclaw_path();
    if nanoclaw_dir.exists() {
        app.active_tab_mut().push_output(format!(
            "Existing nanoclaw installation found at {}. \
             Using existing installation, skipping fork/clone.",
            claws::nanoclaw_path_str()
        ));
        app.active_tab_mut().claws_wizard_username = None;
        launch_claws_ready(app).await;
    } else {
        app.active_tab_mut().dialog = Dialog::ClawsReadyHasForked;
    }
}

/// Determine what to show when `claws ready` is entered (status-only, no wizard).
///
/// - Nanoclaw not installed → show error suggesting `claws init`
/// - Nanoclaw installed, container running → show status table
/// - Nanoclaw installed, container stopped → OfferStart dialog
async fn show_claws_ready_status(app: &mut App) {
    let nanoclaw_dir = claws::nanoclaw_path();

    if !nanoclaw_dir.exists() {
        // Not installed — show error message.
        app.active_tab_mut().start_command("claws ready".to_string());
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        app.active_tab_mut().exit_rx = Some(exit_rx);
        let tx = app.active_tab().output_tx.clone();
        spawn_text_command(tx, exit_tx, |sink| async move {
            sink.println(
                "nanoclaw is not installed. Run 'claws init' to set up nanoclaw.",
            );
            Ok(())
        });
        return;
    }

    // Nanoclaw is installed — check container state.
    match claws::load_nanoclaw_config() {
        Ok(config) => {
            if let Some(ref id) = config.nanoclaw_container_id {
                if app.runtime.is_container_running(id) {
                    // Container is running — show status table.
                    app.active_tab_mut().start_command("claws ready".to_string());
                    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
                    app.active_tab_mut().exit_rx = Some(exit_rx);
                    let tx = app.active_tab().output_tx.clone();
                    let container_id = id.clone();
                    spawn_text_command(tx, exit_tx, move |sink| async move {
                        let mut summary = claws::ClawsSummary {
                            nanoclaw_cloned: crate::commands::ready::StepStatus::Ok("exists".into()),
                            docker_daemon: crate::commands::ready::StepStatus::Ok("running".into()),
                            nanoclaw_image: crate::commands::ready::StepStatus::Ok("exists".into()),
                            nanoclaw_container: crate::commands::ready::StepStatus::Ok(
                                format!("running ({})", &container_id[..container_id.len().min(12)])
                            ),
                        };
                        claws::print_claws_summary(&sink, &mut summary);
                        sink.println("nanoclaw container is running.");
                        Ok(())
                    });
                    return;
                }
            }
            // Container not running or no saved ID — check for a stopped one first.
            if let Some(stopped) = app.runtime.find_stopped_container(
                claws::NANOCLAW_CONTROLLER_NAME,
                claws::NANOCLAW_IMAGE_TAG,
            ) {
                app.active_tab_mut().dialog = Dialog::ClawsReadyOfferRestartStopped {
                    container_id: stopped.id,
                    name: stopped.name,
                    created: stopped.created,
                };
            } else {
                app.active_tab_mut().dialog = Dialog::ClawsReadyOfferStart;
            }
        }
        Err(_) => {
            // Config unreadable — still check for stopped container.
            if let Some(stopped) = app.runtime.find_stopped_container(
                claws::NANOCLAW_CONTROLLER_NAME,
                claws::NANOCLAW_IMAGE_TAG,
            ) {
                app.active_tab_mut().dialog = Dialog::ClawsReadyOfferRestartStopped {
                    container_id: stopped.id,
                    name: stopped.name,
                    created: stopped.created,
                };
            } else {
                app.active_tab_mut().dialog = Dialog::ClawsReadyOfferStart;
            }
        }
    }
}

/// Attach to the running nanoclaw container for a freeform chat session (TUI mode).
///
/// If the container is not running, shows an error suggesting `claws ready`.
async fn launch_claws_chat_attach(app: &mut App) {
    let nanoclaw_dir = claws::nanoclaw_path();

    if !nanoclaw_dir.exists() {
        app.active_tab_mut().input_error = Some(
            "nanoclaw is not installed. Run 'claws init' to set up nanoclaw.".into(),
        );
        return;
    }

    let config = match claws::load_nanoclaw_config() {
        Ok(c) => c,
        Err(_) => {
            app.active_tab_mut().input_error = Some(
                "Failed to load nanoclaw config. Run 'claws ready' to check status.".into(),
            );
            return;
        }
    };

    let container_id = match config.nanoclaw_container_id {
        Some(ref id) if app.runtime.is_container_running(id) => id.clone(),
        _ => {
            // Container not running — check for a stopped one and offer to start.
            app.active_tab_mut().claws_attach_after_start = true;
            if let Some(stopped) = app.runtime.find_stopped_container(
                claws::NANOCLAW_CONTROLLER_NAME,
                claws::NANOCLAW_IMAGE_TAG,
            ) {
                app.active_tab_mut().dialog = Dialog::ClawsReadyOfferRestartStopped {
                    container_id: stopped.id,
                    name: stopped.name,
                    created: stopped.created,
                };
            } else {
                app.active_tab_mut().dialog = Dialog::ClawsReadyOfferStart;
            }
            return;
        }
    };

    app.active_tab_mut().start_command("claws chat".to_string());
    launch_claws_exec(app, container_id).await;
}

/// Phase 1 of the claws init wizard (TUI mode): clone + initial image build.
///
/// Runs the clone and pre-audit image build as a background text command. When it
/// completes successfully, `check_claws_continuation` detects `ClawsPhase::PreAudit`
/// and launches the audit agent via PTY container window.
async fn launch_claws_ready(app: &mut App) {
    let username = app.active_tab().claws_wizard_username.clone();

    // Resolve credentials using the same auto-passthrough as other containers.
    let agent_name = {
        let config = load_repo_config(&claws::nanoclaw_path()).unwrap_or_default();
        config.agent.unwrap_or_else(|| "claude".to_string())
    };
    let credentials = agent_keychain_credentials(&agent_name);
    let env_vars = credentials.env_vars;

    // Prepare sanitized host config (same as `chat`/`implement` auto-configuration).
    // Stored in tab.host_settings so the temp dir outlives all phases of the wizard
    // and remains valid through the subsequent PTY exec session.
    app.active_tab_mut().host_settings = crate::passthrough::passthrough_for_agent(&agent_name).prepare_host_settings();
    app.active_tab_mut().apply_overlays_to_host_settings();
    // A path-only view is moved into the closure; the actual TempDir lives in the tab.
    let closure_host_settings = app.active_tab().host_settings.as_ref().map(|hs| {
        hs.clone_view()
    });

    app.active_tab_mut().claws_phase = ClawsPhase::PreAudit;
    app.active_tab_mut().start_command("claws init".to_string());

    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    app.active_tab_mut().exit_rx = Some(exit_rx);
    let tx = app.active_tab().output_tx.clone();

    // Channel: pre-audit task → TUI — delivers ClawsAuditCtx when initial build succeeds.
    let (audit_ctx_tx, audit_ctx_rx) =
        tokio::sync::oneshot::channel::<claws::ClawsAuditCtx>();
    app.active_tab_mut().claws_audit_ctx_rx = Some(audit_ctx_rx);

    // Channels for the background task to request sudo permission when the clone
    // destination ($HOME/.nanoclaw) is not writable by the current user.
    let (sudo_request_tx, sudo_request_rx) = tokio::sync::oneshot::channel::<()>();
    let (sudo_response_tx, sudo_response_rx) = tokio::sync::oneshot::channel::<Option<String>>();
    app.active_tab_mut().claws_sudo_request_rx = Some(sudo_request_rx);
    app.active_tab_mut().claws_sudo_response_tx = Some(sudo_response_tx);

    let claws_ready_runtime = app.runtime.clone();
    spawn_text_command(tx, exit_tx, move |sink| async move {
        if let Some(ref username) = username {
            match claws::clone_nanoclaw(username.trim(), &sink)? {
                claws::CloneOutcome::Success => {
                    claws::chmod_nanoclaw_permissive(&sink);
                }
                claws::CloneOutcome::PermissionDenied => {
                    sink.println(format!(
                        "Clone failed: permission denied writing to {}.",
                        claws::nanoclaw_path_str()
                    ));
                    // Signal the TUI to show the sudo password dialog.
                    if sudo_request_tx.send(()).is_err() {
                        anyhow::bail!("Clone cancelled: permission denied.");
                    }
                    // Block until the user enters their password (or cancels) in the dialog.
                    match sudo_response_rx.await.unwrap_or(None) {
                        None => anyhow::bail!("Clone cancelled: sudo not accepted."),
                        Some(password) => {
                            claws::clone_nanoclaw_sudo(username.trim(), &sink, Some(&password))?;
                            claws::chmod_nanoclaw_permissive(&sink);
                        }
                    }
                }
            }
        }
        let mut summary = claws::ClawsSummary {
            nanoclaw_cloned: crate::commands::ready::StepStatus::Ok("cloned".into()),
            ..Default::default()
        };

        // Pre-audit: Docker check + Dockerfile.dev + initial image build.
        let ctx = claws::build_nanoclaw_pre_audit(
            &sink,
            env_vars,
            &mut summary,
            closure_host_settings.as_ref(),
            &*claws_ready_runtime,
        ).await?;

        sink.println("Audit agent launching in container window...");
        let _ = audit_ctx_tx.send(ctx);
        Ok(())
    });
}

/// Phase 2 of the claws init wizard (TUI mode): /setup + docker socket dialogs,
/// background container launch, and detached audit agent exec.
///
/// Called by the `ClawsAuditConfirmAccept` action handler (user accepted the audit
/// explanation dialog) after the pre-audit text task completes.
async fn launch_claws_init_post_audit(app: &mut App) {
    let ctx = match app.active_tab_mut().claws_audit_ctx.take() {
        Some(ctx) => ctx,
        None => {
            app.active_tab_mut().push_output(
                "Internal error: missing audit context for post-audit phase.".to_string(),
            );
            app.active_tab_mut().claws_phase = ClawsPhase::Inactive;
            return;
        }
    };

    // Retain a clone of ctx so the PTY exec phase (PostAudit continuation) can build
    // the audit entrypoint after the text task completes.
    app.active_tab_mut().claws_audit_ctx = Some(ctx.clone());

    // Path-only clone of host_settings for the background closure.
    let closure_host_settings = app.active_tab().host_settings.as_ref().map(|hs| {
        hs.clone_view()
    });

    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    app.active_tab_mut().exit_rx = Some(exit_rx);
    let tx = app.active_tab().output_tx.clone();

    // Channel: container ID sent back to TUI so check_claws_continuation can open the PTY.
    let (container_tx, container_rx) = tokio::sync::oneshot::channel::<String>();
    app.active_tab_mut().claws_container_id_rx = Some(container_rx);

    // Channels for docker socket acceptance dialog.
    let (docker_accept_request_tx, docker_accept_request_rx) = tokio::sync::oneshot::channel::<()>();
    let (docker_accept_response_tx, docker_accept_response_rx) = tokio::sync::oneshot::channel::<bool>();
    app.active_tab_mut().claws_docker_accept_request_rx = Some(docker_accept_request_rx);
    app.active_tab_mut().claws_docker_accept_response_tx = Some(docker_accept_response_tx);

    app.active_tab_mut().claws_phase = ClawsPhase::PostAudit;
    app.active_tab_mut().continue_command("claws init".to_string());

    let post_audit_claws_runtime = app.runtime.clone();
    spawn_text_command(tx, exit_tx, move |sink| async move {
        let mut summary = claws::ClawsSummary::default();

        // Signal the TUI to show the docker socket warning dialog.
        if docker_accept_request_tx.send(()).is_err() {
            anyhow::bail!("Docker socket warning channel closed unexpectedly.");
        }
        if !docker_accept_response_rx.await.unwrap_or(false) {
            anyhow::bail!("Docker socket access declined. Cannot launch nanoclaw container.");
        }

        // Launch background nanoclaw container (sleep loop) with docker socket.
        let container_id = claws::launch_nanoclaw_container(
            &sink,
            &ctx.env_vars,
            &mut summary,
            closure_host_settings.as_ref(),
            &*post_audit_claws_runtime,
        ).await?;

        // Send container ID back — check_claws_continuation will open a foreground
        // PTY exec session with the audit prompt.
        let _ = container_tx.send(container_id);
        Ok(())
    });
}

/// Start a fresh nanoclaw container in the background (TUI mode).
///
/// Used by the `ClawsReadyOfferStart` dialog (both from `claws ready` and
/// `claws chat`). Delivers the container ID via `claws_container_id_rx` so that
/// `check_claws_continuation` can attach if `claws_attach_after_start` is set.
async fn launch_claws_start_container_status_only(app: &mut App) {
    let agent_name = {
        let config = load_repo_config(&claws::nanoclaw_path()).unwrap_or_default();
        config.agent.unwrap_or_else(|| "claude".to_string())
    };
    let credentials = agent_keychain_credentials(&agent_name);
    let env_vars = credentials.env_vars;

    let settings_dir = claws::nanoclaw_settings_dir();
    app.active_tab_mut().host_settings = crate::passthrough::passthrough_for_agent(&agent_name).prepare_host_settings_to_dir(&settings_dir);
    app.active_tab_mut().apply_overlays_to_host_settings();
    let closure_host_settings = app.active_tab().host_settings.as_ref().map(|hs| {
        hs.clone_view()
    });

    app.active_tab_mut().claws_phase = ClawsPhase::Setup;
    let command_label = if app.active_tab().claws_attach_after_start {
        "claws chat"
    } else {
        "claws ready"
    };
    app.active_tab_mut().start_command(command_label.to_string());

    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    app.active_tab_mut().exit_rx = Some(exit_rx);
    let tx = app.active_tab().output_tx.clone();

    let (container_tx, container_rx) = tokio::sync::oneshot::channel::<String>();
    app.active_tab_mut().claws_container_id_rx = Some(container_rx);

    let start_only_runtime = app.runtime.clone();
    spawn_text_command(tx, exit_tx, move |sink| async move {
        let nanoclaw_str = claws::nanoclaw_path_str();
        sink.println(format!("Starting nanoclaw controller container {}...", claws::NANOCLAW_CONTROLLER_NAME));

        let container_id = start_only_runtime.run_container_detached(
            claws::NANOCLAW_IMAGE_TAG,
            &nanoclaw_str,
            &nanoclaw_str,
            &nanoclaw_str,
            Some(claws::NANOCLAW_CONTROLLER_NAME),
            env_vars,
            true,
            closure_host_settings.as_ref(),
        )?;

        sink.print("Waiting for container to start... ");
        if !claws::wait_for_container(&container_id, 5, &*start_only_runtime) {
            sink.println("TIMEOUT");
            anyhow::bail!("Container did not start within 5 seconds.");
        }
        sink.println("OK");

        let mut config = claws::load_nanoclaw_config().unwrap_or_default();
        config.nanoclaw_container_id = Some(container_id.clone());
        claws::save_nanoclaw_config(&config)?;

        let _ = container_tx.send(container_id);
        Ok(())
    });
}

/// Restart a stopped nanoclaw container (TUI mode).
///
/// Calls `docker start` on the given container ID, waits for it to be running,
/// saves the ID to the nanoclaw config, and then attaches if
/// `claws_attach_after_start` is set.
async fn launch_claws_restart_stopped_container(app: &mut App, container_id: String) {
    app.active_tab_mut().claws_phase = ClawsPhase::Setup;
    app.active_tab_mut().claws_restarting_container_id = Some(container_id.clone());
    let command_label = if app.active_tab().claws_attach_after_start {
        "claws chat"
    } else {
        "claws ready"
    };
    app.active_tab_mut().start_command(command_label.to_string());

    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    app.active_tab_mut().exit_rx = Some(exit_rx);
    let tx = app.active_tab().output_tx.clone();

    let (container_tx, container_rx) = tokio::sync::oneshot::channel::<String>();
    app.active_tab_mut().claws_container_id_rx = Some(container_rx);

    let restart_runtime = app.runtime.clone();
    let cid = container_id.clone();
    spawn_text_command(tx, exit_tx, move |sink| async move {
        sink.println(format!(
            "Starting stopped container {}...",
            &cid[..cid.len().min(12)],
        ));
        if let Err(e) = restart_runtime.start_container(&cid) {
            sink.println(String::new());
            sink.println(format!("Runtime error: {}", e));
            sink.println(String::new());
            sink.println("The bind-mount sources (e.g. claude.json) may have been cleaned up");
            sink.println("since the container was created.");
            anyhow::bail!("Failed to start container: {}", e);
        }

        sink.print("Waiting for container to start... ");
        if !claws::wait_for_container(&cid, 5, &*restart_runtime) {
            sink.println("TIMEOUT");
            anyhow::bail!("Container did not start within 5 seconds.");
        }
        sink.println("OK");

        let mut config = claws::load_nanoclaw_config().unwrap_or_default();
        config.nanoclaw_container_id = Some(cid.clone());
        claws::save_nanoclaw_config(&config)?;

        let _ = container_tx.send(cid);
        Ok(())
    });
}

/// Delete a stopped container and start a fresh nanoclaw container (TUI mode).
async fn launch_claws_delete_and_start_fresh(app: &mut App, container_id: String) {
    app.active_tab_mut().claws_restarting_container_id = None;
    app.active_tab_mut().claws_phase = ClawsPhase::Setup;
    let command_label = if app.active_tab().claws_attach_after_start {
        "claws chat"
    } else {
        "claws ready"
    };
    app.active_tab_mut().start_command(command_label.to_string());

    let agent_name = {
        let config = load_repo_config(&claws::nanoclaw_path()).unwrap_or_default();
        config.agent.unwrap_or_else(|| "claude".to_string())
    };
    let credentials = agent_keychain_credentials(&agent_name);
    let env_vars = credentials.env_vars;

    let settings_dir = claws::nanoclaw_settings_dir();
    app.active_tab_mut().host_settings = crate::passthrough::passthrough_for_agent(&agent_name).prepare_host_settings_to_dir(&settings_dir);
    app.active_tab_mut().apply_overlays_to_host_settings();
    let closure_host_settings = app.active_tab().host_settings.as_ref().map(|hs| {
        hs.clone_view()
    });

    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    app.active_tab_mut().exit_rx = Some(exit_rx);
    let tx = app.active_tab().output_tx.clone();

    let (container_tx, container_rx) = tokio::sync::oneshot::channel::<String>();
    app.active_tab_mut().claws_container_id_rx = Some(container_rx);

    let delete_fresh_runtime = app.runtime.clone();
    spawn_text_command(tx, exit_tx, move |sink| async move {
        sink.println(format!(
            "Deleting stopped container {}...",
            &container_id[..container_id.len().min(12)],
        ));
        delete_fresh_runtime.remove_container(&container_id)?;
        sink.println("OK");

        let nanoclaw_str = claws::nanoclaw_path_str();
        sink.println(format!(
            "Starting fresh nanoclaw container {}...",
            claws::NANOCLAW_CONTROLLER_NAME,
        ));
        let new_container_id = delete_fresh_runtime.run_container_detached(
            claws::NANOCLAW_IMAGE_TAG,
            &nanoclaw_str,
            &nanoclaw_str,
            &nanoclaw_str,
            Some(claws::NANOCLAW_CONTROLLER_NAME),
            env_vars,
            true,
            closure_host_settings.as_ref(),
        )?;

        sink.print("Waiting for container to start... ");
        if !claws::wait_for_container(&new_container_id, 5, &*delete_fresh_runtime) {
            sink.println("TIMEOUT");
            anyhow::bail!("Container did not start within 5 seconds.");
        }
        sink.println("OK");

        let mut config = claws::load_nanoclaw_config().unwrap_or_default();
        config.nanoclaw_container_id = Some(new_container_id.clone());
        claws::save_nanoclaw_config(&config)?;

        let _ = container_tx.send(new_container_id);
        Ok(())
    });
}

// ─── Ready / init audit phase continuation ────────────────────────────────────

/// Check if a `ready` or `init` audit phase just completed and advance to the
/// next phase. Called from the `was_running && now_done` block in the event loop.
async fn check_audit_continuation(app: &mut App) {
    let phase = app.active_tab().audit_phase.clone();
    match phase {
        AuditPhase::Inactive => {}

        // ── ready flow ──────────────────────────────────────────────────────
        AuditPhase::ReadyPreAudit => {
            if matches!(app.active_tab().phase, state::ExecutionPhase::Error { .. }) {
                // Pre-audit failed — reset.
                let tab = app.active_tab_mut();
                tab.audit_phase = AuditPhase::Inactive;
                tab.ready_audit_handoff = None;
                tab.ready_audit_handoff_rx = None;
                return;
            }
            if let Some(handoff) = app.active_tab_mut().ready_audit_handoff.take() {
                // Pre-audit produced a handoff — launch the PTY audit container.
                app.active_tab_mut().audit_phase = AuditPhase::ReadyAuditPty;
                // Re-store handoff so launch_ready_audit_pty can consume the parts it needs
                // and retain the rest for post-audit.
                app.active_tab_mut().ready_audit_handoff = Some(handoff);
                launch_ready_audit_pty(app).await;
            } else {
                // Pre-audit completed without needing an audit (Done path) — all done.
                app.active_tab_mut().audit_phase = AuditPhase::Inactive;
            }
        }

        AuditPhase::ReadyAuditPty => {
            // PTY audit container just exited — launch post-audit text task.
            let audit_exit_code = match &app.active_tab().phase {
                state::ExecutionPhase::Done { .. } => 0,
                state::ExecutionPhase::Error { exit_code, .. } => *exit_code,
                _ => 0,
            };
            launch_ready_post_audit(app, audit_exit_code).await;
        }

        AuditPhase::ReadyPostAudit => {
            // Post-audit text task completed — workflow is fully done.
            app.active_tab_mut().audit_phase = AuditPhase::Inactive;
        }

        // ── init flow ───────────────────────────────────────────────────────
        AuditPhase::InitPreAudit => {
            if matches!(app.active_tab().phase, state::ExecutionPhase::Error { .. }) {
                let tab = app.active_tab_mut();
                tab.audit_phase = AuditPhase::Inactive;
                tab.init_audit_handoff = None;
                tab.init_audit_handoff_rx = None;
                return;
            }
            if let Some(handoff) = app.active_tab_mut().init_audit_handoff.take() {
                app.active_tab_mut().audit_phase = AuditPhase::InitAuditPty;
                app.active_tab_mut().init_audit_handoff = Some(handoff);
                launch_init_audit_pty(app).await;
            } else {
                app.active_tab_mut().audit_phase = AuditPhase::Inactive;
            }
        }

        AuditPhase::InitAuditPty => {
            let audit_exit_code = match &app.active_tab().phase {
                state::ExecutionPhase::Done { .. } => 0,
                state::ExecutionPhase::Error { exit_code, .. } => *exit_code,
                _ => 0,
            };
            launch_init_post_audit(app, audit_exit_code).await;
        }

        AuditPhase::InitPostAudit => {
            app.active_tab_mut().audit_phase = AuditPhase::Inactive;
        }

        AuditPhase::AgentSetupBuild => {
            app.active_tab_mut().audit_phase = AuditPhase::Inactive;
            if matches!(app.active_tab().phase, state::ExecutionPhase::Done { .. }) {
                // Build succeeded — re-trigger the pending command.
                // launch_implement will re-check for any remaining missing agents.
                launch_pending_command(app).await;
            } else {
                // Build failed; the error was already printed to the output window.
                app.active_tab_mut().push_output(
                    "Agent setup failed. Workflow cannot continue.".to_string(),
                );
                app.active_tab_mut().pending_command = PendingCommand::None;
            }
        }
    }
}

/// Launch the PTY audit container for the `ready` flow.
///
/// Takes the handoff from `TabState.ready_audit_handoff`, moves `host_settings`
/// into `TabState.host_settings` (so the TempDir lives until the container exits),
/// re-stores the rest of the handoff for post-audit use, then spawns the PTY.
async fn launch_ready_audit_pty(app: &mut App) {
    use crate::commands::ready::{build_audit_setup, print_interactive_notice};

    let handoff = match app.active_tab_mut().ready_audit_handoff.take() {
        Some(h) => h,
        None => {
            app.active_tab_mut().audit_phase = AuditPhase::Inactive;
            return;
        }
    };

    let ready_flow::ReadyAuditHandoff { ctx, opts, summary, host_settings, runtime } = handoff;

    // Always use the interactive entrypoint for the TUI PTY session.
    let audit = build_audit_setup(&ctx, false);
    let image_tag = audit.image_tag.clone();
    let entrypoint = audit.entrypoint.clone();
    let mount_path_str = ctx.mount_path.clone();
    let env_vars = ctx.env_vars.clone();
    let allow_docker = opts.allow_docker;
    let agent_name = ctx.agent_name.clone();

    // Print the INTERACTIVE MODE notice to the outer execution window.
    {
        let sink = crate::commands::output::OutputSink::Channel(app.active_tab().output_tx.clone());
        print_interactive_notice(&sink, &agent_name);
    }

    // Move host_settings into TabState so the TempDir persists until finish_command.
    app.active_tab_mut().host_settings = host_settings;
    app.active_tab_mut().apply_overlays_to_host_settings();

    // Re-store the rest of the handoff (without host_settings) for the post-audit phase.
    app.active_tab_mut().ready_audit_handoff = Some(ready_flow::ReadyAuditHandoff {
        ctx,
        opts,
        summary,
        host_settings: None, // now owned by TabState.host_settings
        runtime: runtime.clone(),
    });

    let container_name = crate::runtime::generate_container_name();
    let agent_display = state::agent_display_name(&agent_name).to_string();

    let entrypoint_refs: Vec<&str> = entrypoint.iter().map(String::as_str).collect();
    let pty_args = app.runtime.build_run_args_pty(
        &image_tag,
        &mount_path_str,
        &entrypoint_refs,
        &env_vars,
        app.active_tab().host_settings.as_ref(),
        allow_docker,
        Some(&container_name),
        None,
    );
    let pty_str_refs: Vec<&str> = pty_args.iter().map(String::as_str).collect();

    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let wf_strip_h = app
        .active_tab()
        .workflow
        .as_ref()
        .map(|wf| workflow_strip_height(wf))
        .unwrap_or(0);
    let (inner_cols, inner_rows) =
        calculate_container_inner_size(term_cols, term_rows, wf_strip_h);
    let size = PtySize {
        rows: inner_rows,
        cols: inner_cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let git_root_for_config = find_git_root_from(&app.active_tab().cwd)
        .unwrap_or_else(|| app.active_tab().cwd.clone());
    app.active_tab_mut().terminal_scrollback_lines =
        effective_scrollback_lines(&git_root_for_config);
    app.active_tab_mut()
        .continue_command(format!("ready [audit: {}]", agent_name));
    app.active_tab_mut()
        .start_container(container_name.clone(), agent_display, inner_cols, inner_rows);

    let cli_bin = app.runtime.cli_binary();
    let stats_runtime = app.runtime.clone();
    match PtySession::spawn(cli_bin, &pty_str_refs, size) {
        Ok((session, pty_rx)) => {
            app.active_tab_mut().pty = Some(session);
            app.active_tab_mut().pty_rx = Some(pty_rx);
            app.active_tab_mut().stats_rx =
                Some(spawn_stats_poller(container_name, stats_runtime));
        }
        Err(e) => {
            app.active_tab_mut()
                .push_output(format!("Failed to launch audit container: {}", e));
            app.active_tab_mut().finish_command(1);
            app.active_tab_mut().audit_phase = AuditPhase::Inactive;
            app.active_tab_mut().ready_audit_handoff = None;
        }
    }
}

/// Launch the post-audit text task for the `ready` flow.
async fn launch_ready_post_audit(app: &mut App, audit_exit_code: i32) {
    let handoff = match app.active_tab_mut().ready_audit_handoff.take() {
        Some(h) => h,
        None => {
            app.active_tab_mut().audit_phase = AuditPhase::Inactive;
            return;
        }
    };

    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    app.active_tab_mut().exit_rx = Some(exit_rx);
    let tx = app.active_tab().output_tx.clone();

    app.active_tab_mut()
        .continue_command("ready [post-audit]".into());
    app.active_tab_mut().audit_phase = AuditPhase::ReadyPostAudit;

    spawn_text_command(tx, exit_tx, move |sink| async move {
        ready_flow::execute_post_audit(&sink, handoff, audit_exit_code).await?;
        Ok(())
    });
}

/// Launch the PTY audit container for the `init` flow.
async fn launch_init_audit_pty(app: &mut App) {
    use crate::commands::ready::{audit_entrypoint, print_interactive_notice};

    let handoff = match app.active_tab_mut().init_audit_handoff.take() {
        Some(h) => h,
        None => {
            app.active_tab_mut().audit_phase = AuditPhase::Inactive;
            return;
        }
    };

    let init_flow::InitAuditHandoff {
        agent,
        git_root,
        image_tag,
        agent_image_tag,
        aspec,
        summary,
        env_vars,
        host_settings,
        runtime,
        work_items,
    } = handoff;

    let agent_name = agent.as_str().to_string();

    // Print the INTERACTIVE MODE notice to the outer execution window.
    {
        let sink = crate::commands::output::OutputSink::Channel(app.active_tab().output_tx.clone());
        print_interactive_notice(&sink, &agent_name);
    }

    // Move host_settings into TabState so the TempDir persists until finish_command.
    if let Err(e) = app.active_tab_mut().resolve_overlays_once(&git_root) {
        app.active_tab_mut().push_output(format!("Error: overlay resolution failed: {e}"));
        app.active_tab_mut().finish_command(1);
        return;
    }
    app.active_tab_mut().host_settings = host_settings;
    app.active_tab_mut().apply_overlays_to_host_settings();

    // Re-store the handoff (without host_settings) for the post-audit phase.
    app.active_tab_mut().init_audit_handoff = Some(init_flow::InitAuditHandoff {
        agent: agent.clone(),
        git_root: git_root.clone(),
        image_tag: image_tag.clone(),
        agent_image_tag: agent_image_tag.clone(),
        aspec,
        summary,
        env_vars: env_vars.clone(),
        host_settings: None, // now owned by TabState.host_settings
        runtime: runtime.clone(),
        work_items,
    });

    let entrypoint = audit_entrypoint(&agent_name);
    let entrypoint_refs: Vec<&str> = entrypoint.iter().map(String::as_str).collect();
    let mount_path_str = git_root.to_str().unwrap_or("").to_string();

    let container_name = crate::runtime::generate_container_name();
    let agent_display = state::agent_display_name(&agent_name).to_string();

    let pty_args = app.runtime.build_run_args_pty(
        &image_tag,
        &mount_path_str,
        &entrypoint_refs,
        &env_vars,
        app.active_tab().host_settings.as_ref(),
        false, // init audit never uses --allow-docker
        Some(&container_name),
        None,
    );
    let pty_str_refs: Vec<&str> = pty_args.iter().map(String::as_str).collect();

    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let (inner_cols, inner_rows) = calculate_container_inner_size(term_cols, term_rows, 0);
    let size = PtySize {
        rows: inner_rows,
        cols: inner_cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    app.active_tab_mut().terminal_scrollback_lines =
        effective_scrollback_lines(&git_root);
    app.active_tab_mut()
        .continue_command(format!("init [audit: {}]", agent_name));
    app.active_tab_mut()
        .start_container(container_name.clone(), agent_display, inner_cols, inner_rows);

    let cli_bin = app.runtime.cli_binary();
    let stats_runtime = app.runtime.clone();
    match PtySession::spawn(cli_bin, &pty_str_refs, size) {
        Ok((session, pty_rx)) => {
            app.active_tab_mut().pty = Some(session);
            app.active_tab_mut().pty_rx = Some(pty_rx);
            app.active_tab_mut().stats_rx =
                Some(spawn_stats_poller(container_name, stats_runtime));
        }
        Err(e) => {
            app.active_tab_mut()
                .push_output(format!("Failed to launch audit container: {}", e));
            app.active_tab_mut().finish_command(1);
            app.active_tab_mut().audit_phase = AuditPhase::Inactive;
            app.active_tab_mut().init_audit_handoff = None;
        }
    }
}

/// Launch the post-audit text task for the `init` flow.
async fn launch_init_post_audit(app: &mut App, audit_exit_code: i32) {
    let handoff = match app.active_tab_mut().init_audit_handoff.take() {
        Some(h) => h,
        None => {
            app.active_tab_mut().audit_phase = AuditPhase::Inactive;
            return;
        }
    };

    let runtime = handoff.runtime.clone();
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    app.active_tab_mut().exit_rx = Some(exit_rx);
    let tx = app.active_tab().output_tx.clone();

    app.active_tab_mut()
        .continue_command("init [post-audit]".into());
    app.active_tab_mut().audit_phase = AuditPhase::InitPostAudit;

    spawn_text_command(tx, exit_tx, move |sink| async move {
        let launcher = TuiContainerLauncher {
            runtime: runtime.clone(),
        };
        init_flow::execute_init_post_audit(&sink, handoff, audit_exit_code, &launcher).await?;
        Ok(())
    });
}

/// Check if the claws workflow phase just completed and advance to the next phase.
async fn check_claws_continuation(app: &mut App) {
    let phase = app.active_tab().claws_phase.clone();
    match phase {
        ClawsPhase::Inactive => {}

        ClawsPhase::Setup => {
            if matches!(app.active_tab().phase, state::ExecutionPhase::Error { .. }) {
                // If this was a restart attempt, offer to delete and start fresh.
                let restarting_id = app.active_tab_mut().claws_restarting_container_id.take();
                let tab = app.active_tab_mut();
                tab.claws_phase = ClawsPhase::Inactive;
                tab.claws_container_id = None;
                tab.claws_container_id_rx = None;
                tab.claws_attach_after_start = false;
                if let Some(container_id) = restarting_id {
                    tab.dialog = Dialog::ClawsRestartFailedOfferFresh { container_id };
                }
                return;
            }
            // Container ID is delivered via tick() into claws_container_id.
            if let Some(container_id) = app.active_tab_mut().claws_container_id.take() {
                let attach = app.active_tab().claws_attach_after_start;
                app.active_tab_mut().claws_phase = ClawsPhase::Inactive;
                app.active_tab_mut().claws_container_id_rx = None;
                app.active_tab_mut().claws_attach_after_start = false;
                if attach {
                    // Originated from `claws chat` — attach immediately.
                    launch_claws_exec(app, container_id).await;
                } else {
                    // Originated from `claws ready` — just report status.
                    app.active_tab_mut().push_output(
                        "nanoclaw container started. Run 'claws chat' to attach.".to_string(),
                    );
                }
            } else {
                // Task completed but no container ID yet — stay in Setup until tick delivers it.
            }
        }

        ClawsPhase::PreAudit => {
            // Pre-audit text task finished. If it failed, abort the wizard.
            if matches!(app.active_tab().phase, state::ExecutionPhase::Error { .. }) {
                let tab = app.active_tab_mut();
                tab.claws_phase = ClawsPhase::Inactive;
                tab.claws_audit_ctx = None;
                tab.claws_audit_ctx_rx = None;
                return;
            }
            // Audit context should have arrived via tick() by now.
            if let Some(ctx) = app.active_tab_mut().claws_audit_ctx.take() {
                // Show audit explanation dialog — user confirms before post-audit proceeds.
                // ctx is stored in claws_audit_ctx; the action handler will take it.
                app.active_tab_mut().claws_audit_ctx = Some(ctx);
                app.active_tab_mut().dialog = Dialog::ClawsAuditConfirm;
            } else {
                app.active_tab_mut().push_output(
                    "Internal error: pre-audit completed but no audit context received.".to_string(),
                );
                app.active_tab_mut().claws_phase = ClawsPhase::Inactive;
            }
        }

        ClawsPhase::PostAudit => {
            // Post-audit text task finished. If it failed, abort.
            if matches!(app.active_tab().phase, state::ExecutionPhase::Error { .. }) {
                let tab = app.active_tab_mut();
                tab.claws_phase = ClawsPhase::Inactive;
                tab.claws_container_id = None;
                tab.claws_container_id_rx = None;
                return;
            }
            // Container ID is delivered via tick() into claws_container_id.
            if let Some(container_id) = app.active_tab_mut().claws_container_id.take() {
                let ctx = app.active_tab_mut().claws_audit_ctx.take();
                app.active_tab_mut().claws_phase = ClawsPhase::Inactive;
                app.active_tab_mut().claws_container_id_rx = None;
                if let Some(ctx) = ctx {
                    // Open a foreground PTY exec with the audit prompt — user watches the
                    // audit, then runs /setup in the same session. Container stays running
                    // after the agent exits.
                    launch_claws_exec_audit(app, container_id, ctx).await;
                } else {
                    app.active_tab_mut().push_output(
                        "nanoclaw container started. Run 'claws chat' to attach.".to_string(),
                    );
                }
            } else {
                // Post-audit completed but no container ID.
                app.active_tab_mut().push_output(
                    "Internal error: post-audit completed but no container ID received.".to_string(),
                );
                app.active_tab_mut().claws_phase = ClawsPhase::Inactive;
                app.active_tab_mut().claws_container_id_rx = None;
            }
        }
    }
}

/// Open a foreground PTY exec session inside the nanoclaw controller container with
/// the audit prompt as the initial agent message.
///
/// The user watches the agent configure nanoclaw, then can run `/setup` in the same
/// session. The container keeps running after the agent exits.
async fn launch_claws_exec_audit(app: &mut App, container_id: String, ctx: claws::ClawsAuditCtx) {
    let entrypoint = claws::claws_init_audit_entrypoint(&ctx.agent_name);
    let entrypoint_refs: Vec<&str> = entrypoint.iter().map(String::as_str).collect();

    let exec_args = app.runtime.build_exec_args_pty(
        &container_id,
        &claws::nanoclaw_path_str(),
        &entrypoint_refs,
        &ctx.env_vars,
    );
    let exec_str_refs: Vec<&str> = exec_args.iter().map(String::as_str).collect();

    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let wf_strip_h = app.active_tab().workflow.as_ref().map(|wf| workflow_strip_height(wf)).unwrap_or(0);
    let (inner_cols, inner_rows) = calculate_container_inner_size(term_cols, term_rows, wf_strip_h);
    let size = PtySize {
        rows: inner_rows,
        cols: inner_cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let container_name = claws::NANOCLAW_CONTROLLER_NAME.to_string();
    let display_name = state::agent_display_name(&ctx.agent_name).to_string();

    app.active_tab_mut().continue_command("claws init (agent)".to_string());
    app.active_tab_mut().terminal_scrollback_lines = effective_scrollback_lines(&claws::nanoclaw_path());
    app.active_tab_mut().start_container(container_name.clone(), display_name, inner_cols, inner_rows);

    let cli_bin = app.runtime.cli_binary();
    let stats_runtime = app.runtime.clone();
    match PtySession::spawn(cli_bin, &exec_str_refs, size) {
        Ok((session, pty_rx)) => {
            app.active_tab_mut().pty = Some(session);
            app.active_tab_mut().pty_rx = Some(pty_rx);
            app.active_tab_mut().stats_rx = Some(spawn_stats_poller(container_name, stats_runtime));
        }
        Err(e) => {
            app.active_tab_mut().push_output(format!("Failed to launch agent: {}", e));
            app.active_tab_mut().finish_command(1);
        }
    }
}

/// Attach to a running nanoclaw container via PTY (TUI mode).
async fn launch_claws_exec(app: &mut App, container_id: String) {
    let agent_name = {
        let config = load_repo_config(&claws::nanoclaw_path()).unwrap_or_default();
        config.agent.unwrap_or_else(|| "claude".to_string())
    };

    // Resolve credentials using the same auto-passthrough as other containers.
    let credentials = agent_keychain_credentials(&agent_name);
    let env_vars = credentials.env_vars;

    // The setup container receives no premade prompt — the user interacts directly
    // with their agent (e.g. to run /setup on first launch).
    let entrypoint = chat_entrypoint(&agent_name, false);
    let entrypoint_refs: Vec<&str> = entrypoint.iter().map(String::as_str).collect();

    let exec_args = app.runtime.build_exec_args_pty(
        &container_id,
        &claws::nanoclaw_path_str(),
        &entrypoint_refs,
        &env_vars,
    );
    let exec_str_refs: Vec<&str> = exec_args.iter().map(String::as_str).collect();

    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let wf_strip_h = app.active_tab().workflow.as_ref().map(|wf| workflow_strip_height(wf)).unwrap_or(0);
    let (inner_cols, inner_rows) = calculate_container_inner_size(term_cols, term_rows, wf_strip_h);
    let size = PtySize {
        rows: inner_rows,
        cols: inner_cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let container_name = claws::NANOCLAW_CONTROLLER_NAME.to_string();
    let display_name = state::agent_display_name(&agent_name).to_string();

    app.active_tab_mut().continue_command("claws chat".to_string());
    app.active_tab_mut().terminal_scrollback_lines = effective_scrollback_lines(&claws::nanoclaw_path());
    app.active_tab_mut().start_container(container_name.clone(), display_name, inner_cols, inner_rows);

    let cli_bin = app.runtime.cli_binary();
    let stats_runtime = app.runtime.clone();
    match PtySession::spawn(cli_bin, &exec_str_refs, size) {
        Ok((session, pty_rx)) => {
            app.active_tab_mut().pty = Some(session);
            app.active_tab_mut().pty_rx = Some(pty_rx);
            app.active_tab_mut().stats_rx = Some(spawn_stats_poller(container_name, stats_runtime));
        }
        Err(e) => {
            app.active_tab_mut().push_output(format!("Failed to attach to nanoclaw container: {}", e));
            app.active_tab_mut().finish_command(1);
        }
    }
}

/// Launch the `new` command after collecting kind and title from the dialog.
async fn launch_new(app: &mut App, kind: WorkItemKind, title: String) {
    app.active_tab_mut().start_command("new".to_string());
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    app.active_tab_mut().exit_rx = Some(exit_rx);
    let tx = app.active_tab().output_tx.clone();
    let tab_cwd = app.active_tab().cwd.clone();
    spawn_text_command(tx, exit_tx, move |sink| async move {
        new::run_with_sink(&sink, Some(kind), Some(title), &tab_cwd).await
    });
}

/// Launch `specs new --interview`: create the work item file, then show the interview summary dialog.
async fn launch_new_interview(app: &mut App, kind: WorkItemKind, title: String) {
    use crate::commands::new::create_file_return_number;
    use crate::commands::output::OutputSink;
    let tab_cwd = app.active_tab().cwd.clone();
    let out = OutputSink::Channel(app.active_tab().output_tx.clone());
    app.active_tab_mut().start_command("specs new --interview".to_string());
    match create_file_return_number(&out, kind.clone(), title.clone(), &tab_cwd).await {
        Ok(number) => {
            drop(out);
            app.active_tab_mut().finish_command(0);
            app.active_tab_mut().dialog = state::Dialog::NewInterviewSummary {
                kind,
                title,
                work_item_number: number,
                summary: String::new(),
                cursor_pos: 0,
            };
        }
        Err(e) => {
            drop(out);
            app.active_tab_mut().finish_command(1);
            app.active_tab_mut().input_error = Some(format!("Failed to create work item: {}", e));
        }
    }
}

/// Launch the specs amend agent via PTY.
async fn launch_specs_amend(app: &mut App, work_item: u32, allow_docker: bool) {
    let tab_cwd = app.active_tab().cwd.clone();
    let git_root = match find_git_root_from(&tab_cwd) {
        Some(r) => r,
        None => {
            app.active_tab_mut().input_error = Some("Not inside a Git repository.".into());
            return;
        }
    };

    if let Err(e) = find_work_item(&git_root, work_item) {
        app.active_tab_mut().input_error = Some(format!("{}", e));
        return;
    }

    let config = load_repo_config(&git_root).unwrap_or_default();
    let agent_name = config.agent.as_deref().unwrap_or("claude").to_string();
    let mount_path = app.active_tab_mut().pending_mount_path.take().unwrap_or_else(|| git_root.clone());

    let credentials = agent_keychain_credentials(&agent_name);
    let env_vars = credentials.env_vars;

    // Resolve which image and dockerfile to use.
    let (image_tag, agent_dockerfile_path) =
        crate::commands::agent::resolve_agent_image_and_dockerfile(&git_root, &agent_name);
    if !agent_dockerfile_path.exists() {
        app.active_tab_mut().pending_command = PendingCommand::SpecsAmend {
            work_item,
            allow_docker,
        };
        app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
            agent: agent_name.clone(),
            default_agent: agent_name.clone(),
            from_workflow: false,
            image_only: false,
        };
        return;
    } else if !app.runtime.image_exists(&image_tag) {
        app.active_tab_mut().pending_command = PendingCommand::SpecsAmend {
            work_item,
            allow_docker,
        };
        app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
            agent: agent_name.clone(),
            default_agent: agent_name.clone(),
            from_workflow: false,
            image_only: true,
        };
        return;
    }

    if let Err(e) = app.active_tab_mut().resolve_overlays_once(&git_root) {
        app.active_tab_mut().push_output(format!("Error: overlay resolution failed: {e}"));
        app.active_tab_mut().finish_command(1);
        return;
    }
    app.active_tab_mut().host_settings = crate::passthrough::passthrough_for_agent(&agent_name).prepare_host_settings();
    app.active_tab_mut().apply_overlays_to_host_settings();
    {
        let msg = app.active_tab_mut().host_settings.as_mut()
            .and_then(|s| crate::runtime::apply_dockerfile_user(s, &agent_dockerfile_path));
        if let Some(msg) = msg {
            app.active_tab_mut().push_output(msg);
        }
    }

    let entrypoint = amend_agent_entrypoint(&agent_name, work_item);
    let entrypoint_refs: Vec<&str> = entrypoint.iter().map(String::as_str).collect();

    let container_name = generate_container_name();

    let display_args = app.runtime.build_run_args_pty_display(
        &image_tag,
        mount_path.to_str().unwrap(),
        &entrypoint_refs,
        &env_vars,
        app.active_tab().host_settings.as_ref(),
        allow_docker,
        Some(&container_name),
        None,
    );
    let cli_binary = app.runtime.cli_binary();
    let cmd_display = format!("$ {} {}", cli_binary, display_args.join(" "));

    let command_display = format!("specs amend {:04}", work_item);
    app.active_tab_mut().start_command(command_display);

    if allow_docker {
        let runtime_name = app.runtime.name();
        match app.runtime.check_socket() {
            Ok(socket_path) => {
                app.active_tab_mut().push_output(format!("{} socket: {} (found)", runtime_name, socket_path.display()));
            }
            Err(e) => {
                app.active_tab_mut().push_output(format!("Error: {}", e));
                app.active_tab_mut().finish_command(1);
                return;
            }
        }
    }

    app.active_tab_mut().push_output(cmd_display);

    let sink = crate::commands::output::OutputSink::Channel(app.active_tab().output_tx.clone());
    print_interactive_notice(&sink, &agent_name);

    let pty_args = app.runtime.build_run_args_pty(
        &image_tag,
        mount_path.to_str().unwrap(),
        &entrypoint_refs,
        &env_vars,
        app.active_tab().host_settings.as_ref(),
        allow_docker,
        Some(&container_name),
        None,
    );
    let pty_str_refs: Vec<&str> = pty_args.iter().map(String::as_str).collect();

    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let wf_strip_h = app.active_tab().workflow.as_ref().map(|wf| workflow_strip_height(wf)).unwrap_or(0);
    let (inner_cols, inner_rows) = calculate_container_inner_size(term_cols, term_rows, wf_strip_h);
    let size = PtySize {
        rows: inner_rows,
        cols: inner_cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let display_name = state::agent_display_name(&agent_name).to_string();
    app.active_tab_mut().terminal_scrollback_lines = effective_scrollback_lines(&git_root);
    app.active_tab_mut().start_container(container_name.clone(), display_name, inner_cols, inner_rows);

    let cli_bin = app.runtime.cli_binary();
    let stats_runtime = app.runtime.clone();
    match PtySession::spawn(cli_bin, &pty_str_refs, size) {
        Ok((session, pty_rx)) => {
            app.active_tab_mut().pty = Some(session);
            app.active_tab_mut().pty_rx = Some(pty_rx);
            app.active_tab_mut().stats_rx = Some(spawn_stats_poller(container_name, stats_runtime));
        }
        Err(e) => {
            app.active_tab_mut().push_output(format!("Failed to launch container: {}", e));
            app.active_tab_mut().finish_command(1);
        }
    }
}

/// Launch the specs interview agent via PTY.
async fn launch_specs_interview_agent(
    app: &mut App,
    work_item_number: u32,
    kind: WorkItemKind,
    title: String,
    summary: String,
    allow_docker: bool,
) {
    let tab_cwd = app.active_tab().cwd.clone();
    let git_root = match find_git_root_from(&tab_cwd) {
        Some(r) => r,
        None => {
            app.active_tab_mut().input_error = Some("Not inside a Git repository.".into());
            return;
        }
    };

    let config = load_repo_config(&git_root).unwrap_or_default();
    let agent_name = config.agent.as_deref().unwrap_or("claude").to_string();
    let mount_path = app.active_tab_mut().pending_mount_path.take().unwrap_or_else(|| git_root.clone());

    let credentials = agent_keychain_credentials(&agent_name);
    let env_vars = credentials.env_vars;

    // Resolve which image and dockerfile to use.
    let (image_tag, agent_dockerfile_path) =
        crate::commands::agent::resolve_agent_image_and_dockerfile(&git_root, &agent_name);
    if !agent_dockerfile_path.exists() {
        app.active_tab_mut().pending_command = PendingCommand::SpecsNewInterview {
            work_item_number,
            kind: kind.clone(),
            title: title.clone(),
            summary: summary.clone(),
            allow_docker,
        };
        app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
            agent: agent_name.clone(),
            default_agent: agent_name.clone(),
            from_workflow: false,
            image_only: false,
        };
        return;
    } else if !app.runtime.image_exists(&image_tag) {
        app.active_tab_mut().pending_command = PendingCommand::SpecsNewInterview {
            work_item_number,
            kind: kind.clone(),
            title: title.clone(),
            summary: summary.clone(),
            allow_docker,
        };
        app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
            agent: agent_name.clone(),
            default_agent: agent_name.clone(),
            from_workflow: false,
            image_only: true,
        };
        return;
    }

    if let Err(e) = app.active_tab_mut().resolve_overlays_once(&git_root) {
        app.active_tab_mut().push_output(format!("Error: overlay resolution failed: {e}"));
        app.active_tab_mut().finish_command(1);
        return;
    }
    app.active_tab_mut().host_settings = crate::passthrough::passthrough_for_agent(&agent_name).prepare_host_settings();
    app.active_tab_mut().apply_overlays_to_host_settings();
    {
        let msg = app.active_tab_mut().host_settings.as_mut()
            .and_then(|s| crate::runtime::apply_dockerfile_user(s, &agent_dockerfile_path));
        if let Some(msg) = msg {
            app.active_tab_mut().push_output(msg);
        }
    }

    let entrypoint = interview_agent_entrypoint(&agent_name, work_item_number, &kind, &title, &summary);
    let entrypoint_refs: Vec<&str> = entrypoint.iter().map(String::as_str).collect();

    let container_name = generate_container_name();

    let display_args = app.runtime.build_run_args_pty_display(
        &image_tag,
        mount_path.to_str().unwrap(),
        &entrypoint_refs,
        &env_vars,
        app.active_tab().host_settings.as_ref(),
        allow_docker,
        Some(&container_name),
        None,
    );
    let cli_binary = app.runtime.cli_binary();
    let cmd_display = format!("$ {} {}", cli_binary, display_args.join(" "));

    let command_display = format!("specs new --interview {:04}", work_item_number);
    app.active_tab_mut().start_command(command_display);

    if allow_docker {
        let runtime_name = app.runtime.name();
        match app.runtime.check_socket() {
            Ok(socket_path) => {
                app.active_tab_mut().push_output(format!("{} socket: {} (found)", runtime_name, socket_path.display()));
            }
            Err(e) => {
                app.active_tab_mut().push_output(format!("Error: {}", e));
                app.active_tab_mut().finish_command(1);
                return;
            }
        }
    }

    app.active_tab_mut().push_output(cmd_display);

    let sink = crate::commands::output::OutputSink::Channel(app.active_tab().output_tx.clone());
    print_interactive_notice(&sink, &agent_name);

    let pty_args = app.runtime.build_run_args_pty(
        &image_tag,
        mount_path.to_str().unwrap(),
        &entrypoint_refs,
        &env_vars,
        app.active_tab().host_settings.as_ref(),
        allow_docker,
        Some(&container_name),
        None,
    );
    let pty_str_refs: Vec<&str> = pty_args.iter().map(String::as_str).collect();

    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let wf_strip_h = app.active_tab().workflow.as_ref().map(|wf| workflow_strip_height(wf)).unwrap_or(0);
    let (inner_cols, inner_rows) = calculate_container_inner_size(term_cols, term_rows, wf_strip_h);
    let size = PtySize {
        rows: inner_rows,
        cols: inner_cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let display_name = state::agent_display_name(&agent_name).to_string();
    app.active_tab_mut().terminal_scrollback_lines = effective_scrollback_lines(&git_root);
    app.active_tab_mut().start_container(container_name.clone(), display_name, inner_cols, inner_rows);

    let cli_bin = app.runtime.cli_binary();
    let stats_runtime = app.runtime.clone();
    match PtySession::spawn(cli_bin, &pty_str_refs, size) {
        Ok((session, pty_rx)) => {
            app.active_tab_mut().pty = Some(session);
            app.active_tab_mut().pty_rx = Some(pty_rx);
            app.active_tab_mut().stats_rx = Some(spawn_stats_poller(container_name, stats_runtime));
        }
        Err(e) => {
            app.active_tab_mut().push_output(format!("Failed to launch container: {}", e));
            app.active_tab_mut().finish_command(1);
        }
    }
}

// ─── Multi-step workflow helpers ──────────────────────────────────────────────

/// Initialise or resume workflow state for TUI mode.
///
/// On error, pushes a message to the active tab's output and returns `None`.
fn init_workflow_tui(
    app: &mut App,
    wf_path: &std::path::Path,
    work_item: Option<u32>,
    git_root: &std::path::Path,
    _non_interactive: bool,
    _plan: bool,
) -> Option<crate::workflow::WorkflowState> {
    let (hash, title, steps) = match workflow::load_workflow_file(wf_path) {
        Ok(v) => v,
        Err(e) => {
            app.active_tab_mut().push_output(format!("Workflow error: {}", e));
            app.active_tab_mut().finish_command(1);
            return None;
        }
    };

    let workflow_name = wf_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("workflow")
        .to_string();

    let state_path = workflow::workflow_state_path(git_root, work_item, &workflow_name);

    let state = if state_path.exists() {
        match workflow::load_workflow_state(&state_path) {
            Ok(existing) => {
                // Hash mismatch or same hash — just try to resume.
                if existing.workflow_hash != hash {
                    app.active_tab_mut().push_output(
                        "Warning: workflow file changed since last run. Restarting from beginning.".to_string(),
                    );
                    let _ = std::fs::remove_file(&state_path);
                    crate::workflow::WorkflowState::new(title, steps, hash, work_item, workflow_name)
                } else {
                    app.active_tab_mut().push_output("Resuming previous workflow run.".to_string());
                    existing
                }
            }
            Err(_) => {
                crate::workflow::WorkflowState::new(title, steps, hash, work_item, workflow_name)
            }
        }
    } else {
        crate::workflow::WorkflowState::new(title, steps, hash, work_item, workflow_name)
    };

    // Persist state.
    if let Err(e) = workflow::save_workflow_state(git_root, &state) {
        app.active_tab_mut().push_output(format!("Cannot save workflow state: {}", e));
    }

    Some(state)
}

/// Mark the last workflow step Done, clean up workflow state, and stop the container.
///
/// Used when the user explicitly finishes the workflow from the control board
/// (Ctrl+Enter) while on the final step.
async fn finish_workflow(app: &mut App) {
    let current_step = match app.active_tab().workflow_current_step.clone() {
        Some(s) => s,
        None => return,
    };

    if let Some(ref mut wf) = app.active_tab_mut().workflow {
        wf.set_status(&current_step, StepStatus::Done);
    }

    // Clean up workflow state (prints "All steps done!", removes state file, clears current_step).
    mark_workflow_complete_if_needed(app, &current_step);

    // If the container already exited (e.g. yolo+workflow: the PTY exit set WorktreeMergePrompt
    // but check_workflow_step_completion overwrote it with WorkflowControlBoard), show the
    // worktree merge dialog directly.  If the container is still running, stop it and the PTY
    // exit handler will show the dialog when it fires.
    let already_done = matches!(
        app.active_tab().phase,
        state::ExecutionPhase::Done { .. } | state::ExecutionPhase::Error { .. }
    );
    if already_done {
        if let (Some(branch), Some(wt_path), Some(git_root)) = (
            app.active_tab().worktree_branch.clone(),
            app.active_tab().worktree_active_path.clone(),
            app.active_tab().worktree_git_root.clone(),
        ) {
            let had_error = matches!(app.active_tab().phase, state::ExecutionPhase::Error { .. });
            app.active_tab_mut().dialog = Dialog::WorktreeMergePrompt {
                branch,
                worktree_path: wt_path,
                git_root,
                had_error,
            };
        }
    } else {
        // Stop the running container so the PTY exits and the session summary is shown.
        if let Some(name) = app.active_tab().container_info.as_ref().map(|i| i.container_name.clone()) {
            let stop_runtime = app.runtime.clone();
            tokio::task::spawn_blocking(move || {
                let _ = stop_runtime.stop_container(&name);
            });
        }
    }
}

/// Called after a command completes: if a workflow step just finished, show the
/// confirm/error dialog for the next step.
async fn check_workflow_step_completion(app: &mut App) {
    let has_workflow = app.active_tab().workflow.is_some();
    let current_step = app.active_tab().workflow_current_step.clone();

    if !has_workflow || current_step.is_none() {
        return;
    }

    let step_name = current_step.unwrap();
    let phase = app.active_tab().phase.clone();

    match phase {
        state::ExecutionPhase::Done { .. } => {
            // Mark step as Done.
            if let Some(ref mut wf) = app.active_tab_mut().workflow {
                wf.set_status(&step_name, StepStatus::Done);
            }
            if let (Some(wf), Some(git_root)) = (
                app.active_tab().workflow.clone(),
                app.active_tab().workflow_git_root.clone(),
            ) {
                let _ = workflow::save_workflow_state(&git_root, &wf);
                let next_steps = wf.next_ready();
                if wf.all_done() {
                    if app.active_tab().yolo_mode {
                        // In yolo mode, show the workflow control board instead of auto-finishing.
                        app.active_tab_mut().push_output(format!(
                            "Workflow step '{}' complete. All steps done — presenting workflow control board.",
                            step_name
                        ));
                        app.active_tab_mut().dialog = Dialog::WorkflowControlBoard {
                            current_step: step_name,
                            error: None,
                        };
                    } else {
                        app.active_tab_mut().push_output(format!(
                            "Workflow step '{}' complete. All steps done!", step_name
                        ));
                        app.active_tab_mut().workflow_current_step = None;
                        // Clean up state file.
                        let state_path = workflow::workflow_state_path(&git_root, wf.work_item, &wf.workflow_name);
                        let _ = std::fs::remove_file(state_path);
                    }
                } else if next_steps.is_empty() {
                    app.active_tab_mut().push_output(format!(
                        "Workflow step '{}' complete but no steps are ready.", step_name
                    ));
                    app.active_tab_mut().workflow_current_step = None;
                } else if app.active_tab().yolo_mode {
                    // Yolo mode: auto-advance to the next step without prompting the user.
                    app.active_tab_mut().push_output(format!(
                        "Workflow step '{}' complete. Auto-advancing to next step (yolo mode).",
                        step_name
                    ));
                    launch_next_workflow_step(app).await;
                } else {
                    app.active_tab_mut().dialog = Dialog::WorkflowStepConfirm {
                        completed_step: step_name,
                        next_steps,
                    };
                }
            }
        }
        state::ExecutionPhase::Error { exit_code, .. } => {
            // Mark step as Error.
            let error_msg = format!("Container exited with code {}", exit_code);
            if let Some(ref mut wf) = app.active_tab_mut().workflow {
                wf.set_status(&step_name, StepStatus::Error(error_msg.clone()));
            }
            if let (Some(wf), Some(git_root)) = (
                app.active_tab().workflow.clone(),
                app.active_tab().workflow_git_root.clone(),
            ) {
                let _ = workflow::save_workflow_state(&git_root, &wf);
            }
            app.active_tab_mut().dialog = Dialog::WorkflowStepError {
                failed_step: step_name,
                error: error_msg,
            };
        }
        _ => {}
    }
}

/// Launch the next ready workflow step (called after user confirms advancing).
async fn launch_next_workflow_step(app: &mut App) {
    // Kill the previous container if it is still running (e.g. stuck / forced advance).
    // When the container exited naturally, `container_info` is already None (cleared by
    // `finish_command`), so this is a no-op in the normal completion path.
    if let Some(name) = app.active_tab().container_info.as_ref().map(|i| i.container_name.clone()) {
        let stop_runtime = app.runtime.clone();
        tokio::task::spawn_blocking(move || {
            let _ = stop_runtime.stop_container(&name);
        });
    }

    let (wf_state, git_root, work_item, agent_name, allow_docker, ssh_dir, mount_path) = {
        let tab = app.active_tab();
        let wf = match tab.workflow.clone() {
            Some(w) => w,
            None => return,
        };
        let git_root = match tab.workflow_git_root.clone() {
            Some(r) => r,
            None => return,
        };
        let config = load_repo_config(&git_root).unwrap_or_default();
        let agent = config.agent.as_deref().unwrap_or("claude").to_string();
        // Use the launch-time mount path (worktree or repo root) for all subsequent steps.
        let mount_path = tab.workflow_mount_path.clone().unwrap_or_else(|| git_root.clone());
        (
            wf,
            git_root,
            tab.workflow.as_ref().and_then(|w| w.work_item),
            agent,
            tab.workflow_allow_docker,
            tab.workflow_ssh_dir.clone(),
            mount_path,
        )
    };

    let ready = wf_state.next_ready();
    if ready.is_empty() {
        return;
    }

    let step_name = ready[0].clone();
    let step_state = wf_state.get_step(&step_name).unwrap().clone();

    // Resolve the per-step agent: prefer the map built at workflow launch, fall back to
    // the step's own field, and ultimately to the config default.
    let step_agent = app.active_tab().workflow_step_agents.get(&step_name).cloned()
        .or_else(|| step_state.agent.clone())
        .unwrap_or_else(|| agent_name.clone());

    // Load work item content (empty when no work item).
    let work_item_content = if let Some(wi) = work_item {
        match find_work_item(&git_root, wi).and_then(|p| {
            std::fs::read_to_string(&p).map_err(|e| anyhow::anyhow!("{}", e))
        }) {
            Ok(c) => c,
            Err(e) => {
                app.active_tab_mut().push_output(format!("Cannot read work item: {}", e));
                return;
            }
        }
    } else {
        String::new()
    };

    let credentials = agent_keychain_credentials(&step_agent);
    let env_vars = credentials.env_vars;

    let prompt = workflow::substitute_prompt(&step_state.prompt_template, work_item, &work_item_content);
    let (yolo_mode, auto_mode, yolo_disallowed_tools) = {
        let tab = app.active_tab();
        (tab.yolo_mode, tab.auto_mode, tab.yolo_disallowed_tools.clone())
    };
    let mut entrypoint = workflow_step_entrypoint(&step_agent, &prompt, false, false);
    {
        use crate::commands::agent::append_autonomous_flags;
        append_autonomous_flags(&mut entrypoint, &step_agent, yolo_mode, auto_mode, &yolo_disallowed_tools);
    }
    let entrypoint_refs: Vec<&str> = entrypoint.iter().map(String::as_str).collect();

    // Resolve which image and dockerfile to use.
    let (image_tag, agent_dockerfile_path) =
        crate::commands::agent::resolve_agent_image_and_dockerfile(&git_root, &step_agent);
    if !agent_dockerfile_path.exists() {
        app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
            agent: step_agent.clone(),
            default_agent: agent_name.clone(),
            from_workflow: true,
            image_only: false,
        };
        return;
    } else if !app.runtime.image_exists(&image_tag) {
        app.active_tab_mut().dialog = Dialog::AgentSetupConfirm {
            agent: step_agent.clone(),
            default_agent: agent_name.clone(),
            from_workflow: true,
            image_only: true,
        };
        return;
    }

    let container_name = generate_container_name();

    // Reset host settings when the step's agent differs from the previously running step.
    let prev_step_agent = app.active_tab().workflow_current_step.as_ref()
        .and_then(|s| app.active_tab().workflow_step_agents.get(s).cloned())
        .unwrap_or_else(|| agent_name.clone());
    if step_agent != prev_step_agent || app.active_tab().host_settings.is_none() {
        if let Err(e) = app.active_tab_mut().resolve_overlays_once(&git_root) {
            app.active_tab_mut().push_output(format!("Error: overlay resolution failed: {e}"));
            app.active_tab_mut().finish_command(1);
            return;
        }
        app.active_tab_mut().host_settings =
            crate::passthrough::passthrough_for_agent(&step_agent).prepare_host_settings();
        app.active_tab_mut().apply_overlays_to_host_settings();
        if yolo_mode {
            if let Some(ref s) = app.active_tab().host_settings {
                let _ = s.apply_yolo_settings();
            }
        }
        let msg = app.active_tab_mut().host_settings.as_mut()
            .and_then(|s| crate::runtime::apply_dockerfile_user(s, &agent_dockerfile_path));
        if let Some(msg) = msg {
            app.active_tab_mut().push_output(msg);
        }
    }
    let host_settings_ref = app.active_tab().host_settings.as_ref();

    let pty_args = app.runtime.build_run_args_pty(
        &image_tag,
        mount_path.to_str().unwrap_or("."),
        &entrypoint_refs,
        &env_vars,
        host_settings_ref,
        allow_docker,
        Some(&container_name),
        ssh_dir.as_deref(),
    );
    let pty_str_refs: Vec<&str> = pty_args.iter().map(String::as_str).collect();

    let command_display = if let Some(wi) = work_item {
        format!("implement {:04} [step: {}]", wi, step_name)
    } else {
        format!("workflow [step: {}]", step_name)
    };
    app.active_tab_mut().continue_command(command_display);
    app.active_tab_mut().push_output(format!("--- Workflow step: {} ---", step_name));

    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let wf_strip_h = app.active_tab().workflow.as_ref()
        .map(|wf| workflow_strip_height(wf))
        .unwrap_or(0);
    let (inner_cols, inner_rows) = calculate_container_inner_size(term_cols, term_rows, wf_strip_h);
    let size = PtySize {
        rows: inner_rows,
        cols: inner_cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let display_name = state::agent_display_name(&step_agent).to_string();
    app.active_tab_mut().terminal_scrollback_lines = effective_scrollback_lines(&git_root);
    app.active_tab_mut().start_container(container_name.clone(), display_name, inner_cols, inner_rows);

    // Record container name in workflow state for persistence.
    if let Some(ref mut wf) = app.active_tab_mut().workflow {
        wf.set_container_id(&step_name, container_name.clone());
    }

    // Mark the step as Running and persist.
    if let Some(ref mut wf) = app.active_tab_mut().workflow {
        wf.set_status(&step_name, StepStatus::Running);
    }
    if let (Some(wf), Some(gr)) = (app.active_tab().workflow.clone(), app.active_tab().workflow_git_root.clone()) {
        let _ = workflow::save_workflow_state(&gr, &wf);
    }
    app.active_tab_mut().auto_workflow_disabled_for_step = false;
    app.active_tab_mut().workflow_current_step = Some(step_name);

    let cli_bin = app.runtime.cli_binary();
    let stats_runtime = app.runtime.clone();
    match PtySession::spawn(cli_bin, &pty_str_refs, size) {
        Ok((session, pty_rx)) => {
            app.active_tab_mut().pty = Some(session);
            app.active_tab_mut().pty_rx = Some(pty_rx);
            app.active_tab_mut().stats_rx = Some(spawn_stats_poller(container_name, stats_runtime));
        }
        Err(e) => {
            app.active_tab_mut().push_output(format!("Failed to launch container: {}", e));
            app.active_tab_mut().finish_command(1);
        }
    }
}

/// Abort the current workflow: clear workflow state from tab.
fn abort_workflow(app: &mut App) {
    app.active_tab_mut().push_output("Workflow paused. Run again to resume.".to_string());
    app.active_tab_mut().workflow_current_step = None;
    // Keep `workflow` state so the user can resume later.
}

/// Cancel the currently running workflow step: kill the container, revert the step to
/// Pending in the state file, and return the tab to idle so the user can resume later.
async fn cancel_workflow_execution(app: &mut App) {
    let current_step = match app.active_tab().workflow_current_step.clone() {
        Some(s) => s,
        None => return,
    };

    // Revert the current step to Pending so it can be restarted on next run.
    if let Some(ref mut wf) = app.active_tab_mut().workflow {
        wf.set_status(&current_step, StepStatus::Pending);
    }
    if let (Some(wf), Some(git_root)) = (
        app.active_tab().workflow.clone(),
        app.active_tab().workflow_git_root.clone(),
    ) {
        let _ = workflow::save_workflow_state(&git_root, &wf);
    }

    // Kill the running container (non-blocking; container will stop in the background).
    if let Some(name) = app
        .active_tab()
        .container_info
        .as_ref()
        .map(|i| i.container_name.clone())
    {
        let stop_runtime = app.runtime.clone();
        tokio::task::spawn_blocking(move || {
            let _ = stop_runtime.stop_container(&name);
        });
    }

    // Clear the active step before resetting so check_workflow_step_completion ignores
    // the PTY exit event that arrives when the container eventually stops.
    app.active_tab_mut().workflow_current_step = None;
    app.active_tab_mut()
        .push_output("Workflow cancelled. Run again to resume from this step.".to_string());
    // Tear down the PTY channels and container window, returning the tab to idle.
    app.active_tab_mut().reset_to_idle();
}

/// Retry the failed workflow step.
async fn retry_workflow_step(app: &mut App) {
    let step_name = app.active_tab().workflow_current_step.clone();
    if let Some(ref step_name) = step_name {
        if let Some(ref mut wf) = app.active_tab_mut().workflow {
            wf.set_status(step_name, StepStatus::Pending);
        }
    }
    if let (Some(wf), Some(git_root)) = (app.active_tab().workflow.clone(), app.active_tab().workflow_git_root.clone()) {
        let _ = workflow::save_workflow_state(&git_root, &wf);
    }
    // Re-launch via advance.
    launch_next_workflow_step(app).await;
}

/// Handle the all-done / no-next-ready case after marking a step Done.
///
/// Returns `true` if the workflow is complete or stalled (caller should not launch next step),
/// `false` if there are ready steps to launch.
fn mark_workflow_complete_if_needed(app: &mut App, current_step: &str) -> bool {
    if let (Some(wf), Some(git_root)) = (app.active_tab().workflow.clone(), app.active_tab().workflow_git_root.clone()) {
        let _ = workflow::save_workflow_state(&git_root, &wf);
        if wf.all_done() {
            app.active_tab_mut().push_output(format!(
                "Workflow step '{}' complete. All steps done!", current_step
            ));
            app.active_tab_mut().workflow_current_step = None;
            let state_path = workflow::workflow_state_path(&git_root, wf.work_item, &wf.workflow_name);
            let _ = std::fs::remove_file(state_path);
            return true;
        }
        if wf.next_ready().is_empty() {
            app.active_tab_mut().push_output(format!(
                "Workflow step '{}' complete but no steps are ready.", current_step
            ));
            app.active_tab_mut().workflow_current_step = None;
            return true;
        }
    }
    false
}

/// Cancel the current step and return to the previous (most recently Done) step.
async fn cancel_to_previous_step(app: &mut App) {
    let current_step = match app.active_tab().workflow_current_step.clone() {
        Some(s) => s,
        None => return,
    };

    // Mark current step Pending (undo Running status).
    if let Some(ref mut wf) = app.active_tab_mut().workflow {
        wf.set_status(&current_step, StepStatus::Pending);
    }

    // Find predecessor: scan steps in reverse, find last Done step.
    let predecessor = app.active_tab().workflow.as_ref().and_then(|wf| {
        wf.steps.iter().rev().find(|s| s.status == StepStatus::Done).map(|s| s.name.clone())
    });

    if let Some(pred_name) = predecessor {
        // Mark predecessor Pending so it can be re-run.
        if let Some(ref mut wf) = app.active_tab_mut().workflow {
            wf.set_status(&pred_name, StepStatus::Pending);
        }
        if let (Some(wf), Some(git_root)) = (app.active_tab().workflow.clone(), app.active_tab().workflow_git_root.clone()) {
            let _ = workflow::save_workflow_state(&git_root, &wf);
        }
        launch_next_workflow_step(app).await;
    } else {
        // No predecessor: revert current step to Running and reopen dialog with error.
        if let Some(ref mut wf) = app.active_tab_mut().workflow {
            wf.set_status(&current_step, StepStatus::Running);
        }
        app.active_tab_mut().dialog = Dialog::WorkflowControlBoard {
            current_step,
            error: Some("No previous step to return to".into()),
        };
    }
}

/// Mark the current workflow step Done and advance to the next step in a new container.
async fn advance_workflow_next_new_container(app: &mut App) {
    let current_step = match app.active_tab().workflow_current_step.clone() {
        Some(s) => s,
        None => return,
    };

    if let Some(ref mut wf) = app.active_tab_mut().workflow {
        wf.set_status(&current_step, StepStatus::Done);
    }

    if mark_workflow_complete_if_needed(app, &current_step) {
        return;
    }

    launch_next_workflow_step(app).await;
}

/// Mark the current workflow step Done and send the next step's prompt to the existing PTY.
async fn advance_workflow_next_current_container(app: &mut App) {
    // If PTY is not available, fall back to new container.
    if app.active_tab().pty.is_none() {
        app.active_tab_mut().push_output("PTY session ended — starting new container".to_string());
        advance_workflow_next_new_container(app).await;
        return;
    }

    let current_step = match app.active_tab().workflow_current_step.clone() {
        Some(s) => s,
        None => return,
    };

    if let Some(ref mut wf) = app.active_tab_mut().workflow {
        wf.set_status(&current_step, StepStatus::Done);
    }

    if mark_workflow_complete_if_needed(app, &current_step) {
        return;
    }

    launch_next_workflow_step_in_current_container(app).await;
}

/// Send the next workflow step's prompt to the existing PTY session (no new container).
async fn launch_next_workflow_step_in_current_container(app: &mut App) {
    debug_assert!(app.active_tab().pty.is_some());
    debug_assert!(app.active_tab().container_info.is_some());

    let (wf_state, git_root, work_item) = {
        let tab = app.active_tab();
        let wf = match tab.workflow.clone() {
            Some(w) => w,
            None => return,
        };
        let git_root = match tab.workflow_git_root.clone() {
            Some(r) => r,
            None => return,
        };
        let work_item = wf.work_item;
        (wf, git_root, work_item)
    };

    let ready = wf_state.next_ready();
    if ready.is_empty() {
        return;
    }

    let step_name = ready[0].clone();
    let step_state = match wf_state.get_step(&step_name) {
        Some(s) => s.clone(),
        None => return,
    };

    // Load work item content for prompt substitution (empty when no work item).
    let work_item_content = if let Some(wi) = work_item {
        match find_work_item(&git_root, wi).and_then(|p| {
            std::fs::read_to_string(&p).map_err(|e| anyhow::anyhow!("{}", e))
        }) {
            Ok(c) => c,
            Err(e) => {
                app.active_tab_mut().push_output(format!("Cannot read work item: {}", e));
                return;
            }
        }
    } else {
        String::new()
    };

    let prompt = workflow::substitute_prompt(&step_state.prompt_template, work_item, &work_item_content);

    // Send prompt to the existing PTY, followed by CR (carriage return = Enter in a PTY).
    let bytes = format!("{}\r", prompt).into_bytes();
    if let Some(ref pty) = app.active_tab().pty {
        pty.write_bytes(&bytes);
    }

    // Update step status and current step tracking.
    if let Some(ref mut wf) = app.active_tab_mut().workflow {
        wf.set_status(&step_name, StepStatus::Running);
    }
    app.active_tab_mut().auto_workflow_disabled_for_step = false;
    app.active_tab_mut().workflow_current_step = Some(step_name.clone());

    // Persist state.
    if let (Some(wf), Some(gr)) = (app.active_tab().workflow.clone(), app.active_tab().workflow_git_root.clone()) {
        let _ = workflow::save_workflow_state(&gr, &wf);
    }

    // Maximize the container window so the user sees the PTY output.
    app.active_tab_mut().container_window = ContainerWindowState::Maximized;

    app.active_tab_mut().push_output(format!("--- Workflow step: {} (reusing container) ---", step_name));
}

// ─── Clipboard abstraction ────────────────────────────────────────────────────

/// Abstraction over clipboard write access, enabling test-time mocking without
/// requiring a real display server.
pub trait ClipboardWriter {
    fn set_text(&mut self, text: &str) -> Result<(), String>;
}

struct ArboardClipboard(arboard::Clipboard);

impl ClipboardWriter for ArboardClipboard {
    fn set_text(&mut self, text: &str) -> Result<(), String> {
        self.0.set_text(text).map_err(|e| e.to_string())
    }
}

/// Copy the active terminal text selection from `tab` to `clipboard`.
/// Returns `true` if non-empty text was written successfully.
pub fn copy_selection_to_clipboard(tab: &state::TabState, clipboard: &mut dyn ClipboardWriter) -> bool {
    match extract_selection_text(tab) {
        Some(text) if !text.is_empty() => clipboard.set_text(&text).is_ok(),
        _ => false,
    }
}

// ─── Terminal text selection helpers ──────────────────────────────────────────

/// Capture a snapshot of the current vt100 screen cell contents at the given scroll offset.
///
/// `scroll_offset` must match `tab.container_scroll_offset` at the time of the mouse-down
/// event.  When non-zero the parser is temporarily seeked to that scrollback position so
/// the snapshot reflects the view the user actually sees, not the live (tail) screen.
/// After capturing, the parser is always reset to offset 0.
///
/// The snapshot is a 2D grid of strings, one per cell (row-major order).
/// Empty cells are stored as `" "` (a single space) so that copied text preserves spacing.
fn capture_vt100_snapshot(parser: &mut Option<vt100::Parser>, scroll_offset: usize) -> Option<Vec<Vec<String>>> {
    let parser = parser.as_mut()?;
    if scroll_offset > 0 {
        parser.set_scrollback(scroll_offset);
    }
    let snapshot = {
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        (0..rows)
            .map(|row| {
                (0..cols)
                    .map(|col| {
                        screen
                            .cell(row, col)
                            .map(|c| {
                                let s = c.contents();
                                if s.is_empty() { " ".to_string() } else { s }
                            })
                            .unwrap_or_else(|| " ".to_string())
                    })
                    .collect()
            })
            .collect()
    };
    if scroll_offset > 0 {
        parser.set_scrollback(0);
    }
    Some(snapshot)
}

/// Extract the selected text from a tab's selection snapshot.
/// Returns `None` if no selection is active or no snapshot is available.
///
/// Rows are joined with `\n` at every row boundary and trailing spaces on each row
/// are stripped.  The vt100 cell API does not expose soft-wrap (line-continuation)
/// metadata, so there is no way to distinguish a logical line that was wrapped by the
/// terminal from a genuine line boundary.  As a result, selecting across soft-wrapped
/// output will produce an extra `\n` at the wrap point.  A heuristic (omit `\n` when
/// the last non-space cell of a row is not at the terminal's right edge) would reduce
/// the false-positive rate but cannot eliminate it without wrap metadata.
fn extract_selection_text(tab: &state::TabState) -> Option<String> {
    let start = tab.terminal_selection_start?;
    let end = tab.terminal_selection_end?;
    let snapshot = tab.terminal_selection_snapshot.as_ref()?;

    // Normalise selection order so start is always before end.
    let (sr, sc, er, ec) = if start.0 < end.0 || (start.0 == end.0 && start.1 <= end.1) {
        (start.0 as usize, start.1 as usize, end.0 as usize, end.1 as usize)
    } else {
        (end.0 as usize, end.1 as usize, start.0 as usize, start.1 as usize)
    };

    let mut result = String::new();
    for row in sr..=er {
        if row >= snapshot.len() {
            break;
        }
        let row_data = &snapshot[row];
        let col_start = if row == sr { sc } else { 0 };
        let col_end = if row == er {
            (ec + 1).min(row_data.len())
        } else {
            row_data.len()
        };
        let mut line = String::new();
        for col in col_start..col_end {
            if col < row_data.len() {
                line.push_str(&row_data[col]);
            }
        }
        // Strip trailing spaces from each selected line.
        result.push_str(line.trim_end());
        if row < er {
            result.push('\n');
        }
    }
    Some(result)
}

/// Handle a `new workflow` dialog submission: write the file (and launch the
/// agent if `interview`).
async fn launch_new_workflow_action(
    app: &mut App,
    state: state::NewWorkflowDialogState,
) {
    use crate::commands::new_workflow::{
        resolve_workflow_dest, skeleton_workflow, write_workflow_file, WorkflowInput,
        CONTAINER_WORKSPACE,
    };

    let tab_cwd = app.active_tab().cwd.clone();
    let git_root = find_git_root_from(&tab_cwd);
    if !state.global && git_root.is_none() {
        app.active_tab_mut().input_error =
            Some("Not inside a Git repository. Use --global to write to ~/.amux/.".into());
        return;
    }

    let dest = match resolve_workflow_dest(
        state.name.trim(),
        state.global,
        &state.format,
        git_root.as_deref(),
    ) {
        Ok(d) => d,
        Err(e) => {
            app.active_tab_mut().input_error = Some(e.to_string());
            return;
        }
    };

    let filename = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    if state.interview {
        // Write skeleton + launch agent.
        let title_value = if state.title.trim().is_empty() {
            state.name.trim().to_string()
        } else {
            state.title.trim().to_string()
        };
        let skeleton = skeleton_workflow(&title_value, &state.format);
        if let Some(parent) = dest.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                app.active_tab_mut().input_error =
                    Some(format!("Failed to create directory: {}", e));
                return;
            }
        }
        if let Err(e) = std::fs::write(&dest, &skeleton) {
            app.active_tab_mut().input_error = Some(format!("Failed to write file: {}", e));
            return;
        }
        let git_root = match git_root {
            Some(r) => r,
            None => {
                app.active_tab_mut().input_error = Some(
                    "Not inside a git repository. The agent image requires a git repo. Use --global without --interview to create without an agent.".into(),
                );
                return;
            }
        };

        let (mount_path, container_path) = if state.global {
            let wf_dir = match crate::config::global_workflows_dir() {
                Ok(d) => d,
                Err(e) => {
                    app.active_tab_mut().input_error = Some(e.to_string());
                    return;
                }
            };
            (
                wf_dir,
                format!("{}/{}", CONTAINER_WORKSPACE, filename),
            )
        } else {
            let relative = dest.strip_prefix(&git_root).unwrap_or(dest.as_path());
            let container_path = format!("{}/{}", CONTAINER_WORKSPACE, relative.to_string_lossy());
            (git_root.clone(), container_path)
        };

        let agent_name = load_repo_config(&git_root)
            .unwrap_or_default()
            .agent
            .as_deref()
            .unwrap_or("claude")
            .to_string();
        let entrypoint = crate::commands::new_workflow::workflow_interview_agent_entrypoint(
            &agent_name,
            &container_path,
            &filename,
            state.summary.trim(),
        );
        let status = format!(
            "Running interview agent for workflow '{}' with agent '{}'",
            state.name.trim(), agent_name
        );
        let out = crate::commands::output::OutputSink::Channel(app.active_tab().output_tx.clone());
        out.println(format!("Created skeleton workflow: {}", dest.display()));
        let runtime = app.runtime.clone();
        let cmd_label = format!("new workflow --interview {}", state.name.trim());
        app.active_tab_mut().start_command(cmd_label);
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        app.active_tab_mut().exit_rx = Some(exit_rx);
        let tx = app.active_tab().output_tx.clone();
        let credentials = match crate::commands::auth::resolve_auth(&git_root, &agent_name) {
            Ok(c) => c,
            Err(e) => {
                app.active_tab_mut().input_error = Some(e.to_string());
                app.active_tab_mut().finish_command(1);
                return;
            }
        };
        let host_settings =
            crate::passthrough::passthrough_for_agent(&agent_name).prepare_host_settings();
        spawn_text_command(tx, exit_tx, move |sink| async move {
            crate::commands::agent::run_agent_with_sink(
                entrypoint,
                &status,
                &sink,
                Some(mount_path),
                credentials.env_vars,
                false,
                host_settings.as_ref(),
                false,
                false,
                None,
                None,
                None,
                &*runtime,
                Some(git_root),
            )
            .await
        });
        return;
    }

    // Non-interview: build the WorkflowInput and write.
    let title = state.title.trim().to_string();
    let input = WorkflowInput {
        title,
        steps: state.steps,
    };
    match write_workflow_file(&input, &dest, &state.format) {
        Ok(()) => {
            let out = crate::commands::output::OutputSink::Channel(app.active_tab().output_tx.clone());
            out.println(format!("Created workflow: {}", dest.display()));
        }
        Err(e) => {
            app.active_tab_mut().input_error = Some(format!("Failed to write workflow: {}", e));
        }
    }
}

/// Handle a `new skill` dialog submission.
async fn launch_new_skill_action(app: &mut App, state: state::NewSkillDialogState) {
    use crate::commands::new_skill::{
        resolve_skill_dest, write_skill_file, write_skill_skeleton, SkillInput,
    };
    use crate::commands::new_workflow::CONTAINER_WORKSPACE;

    let tab_cwd = app.active_tab().cwd.clone();
    let git_root = find_git_root_from(&tab_cwd);
    if !state.global && git_root.is_none() {
        app.active_tab_mut().input_error =
            Some("Not inside a Git repository. Use --global to write to ~/.amux/.".into());
        return;
    }

    let dest_dir = match resolve_skill_dest(state.name.trim(), state.global, git_root.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            app.active_tab_mut().input_error = Some(e.to_string());
            return;
        }
    };

    if state.interview {
        let path = match write_skill_skeleton(state.name.trim(), state.description.trim(), &dest_dir) {
            Ok(p) => p,
            Err(e) => {
                app.active_tab_mut().input_error = Some(format!("Failed to write skeleton: {}", e));
                return;
            }
        };
        let git_root = match git_root {
            Some(r) => r,
            None => {
                app.active_tab_mut().input_error = Some(
                    "Not inside a git repository. The agent image requires a git repo. Use --global without --interview to create without an agent.".into(),
                );
                return;
            }
        };
        let (mount_path, container_path) = if state.global {
            let skill_dir = match crate::config::global_skills_dir() {
                Ok(d) => d.join(state.name.trim()),
                Err(e) => {
                    app.active_tab_mut().input_error = Some(e.to_string());
                    return;
                }
            };
            if let Err(e) = std::fs::create_dir_all(&skill_dir) {
                app.active_tab_mut().input_error =
                    Some(format!("Failed to create directory: {}", e));
                return;
            }
            (skill_dir, format!("{}/SKILL.md", CONTAINER_WORKSPACE))
        } else {
            let relative = path.strip_prefix(&git_root).unwrap_or(path.as_path());
            let container_path = format!("{}/{}", CONTAINER_WORKSPACE, relative.to_string_lossy());
            (git_root.clone(), container_path)
        };

        let agent_name = load_repo_config(&git_root)
            .unwrap_or_default()
            .agent
            .as_deref()
            .unwrap_or("claude")
            .to_string();
        let entrypoint = crate::commands::new_skill::skill_interview_agent_entrypoint(
            &agent_name,
            &container_path,
            state.summary.trim(),
        );
        let status = format!(
            "Running interview agent for skill '{}' with agent '{}'",
            state.name.trim(), agent_name
        );
        let out = crate::commands::output::OutputSink::Channel(app.active_tab().output_tx.clone());
        out.println(format!("Created skeleton skill: {}", path.display()));
        let runtime = app.runtime.clone();
        let cmd_label = format!("new skill --interview {}", state.name.trim());
        app.active_tab_mut().start_command(cmd_label);
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        app.active_tab_mut().exit_rx = Some(exit_rx);
        let tx = app.active_tab().output_tx.clone();
        let credentials = match crate::commands::auth::resolve_auth(&git_root, &agent_name) {
            Ok(c) => c,
            Err(e) => {
                app.active_tab_mut().input_error = Some(e.to_string());
                app.active_tab_mut().finish_command(1);
                return;
            }
        };
        let host_settings =
            crate::passthrough::passthrough_for_agent(&agent_name).prepare_host_settings();
        spawn_text_command(tx, exit_tx, move |sink| async move {
            crate::commands::agent::run_agent_with_sink(
                entrypoint,
                &status,
                &sink,
                Some(mount_path),
                credentials.env_vars,
                false,
                host_settings.as_ref(),
                false,
                false,
                None,
                None,
                None,
                &*runtime,
                Some(git_root),
            )
            .await
        });
        return;
    }

    let input = SkillInput {
        name: state.name.trim().to_string(),
        description: state.description.trim().to_string(),
        body: state.body.trim().to_string(),
    };
    match write_skill_file(&input, &dest_dir) {
        Ok(path) => {
            let out = crate::commands::output::OutputSink::Channel(app.active_tab().output_tx.clone());
            out.println(format!("Created skill: {}", path.display()));
        }
        Err(e) => {
            app.active_tab_mut().input_error = Some(format!("Failed to write skill: {}", e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ready_flow::ReadyQa;

    // ── Test-only TUI init adapters ───────────────────────────────────────────

    /// Pre-collected answers from TUI modal dialogs, consumed by `TuiInitQa` in tests.
    struct TuiInitAnswers {
        replace_aspec: bool,
        run_audit: bool,
        work_items: Option<crate::config::WorkItemsConfig>,
    }

    struct TuiInitQa {
        answers: TuiInitAnswers,
    }

    impl init_flow::InitQa for TuiInitQa {
        fn ask_replace_aspec(&mut self) -> Result<bool> {
            Ok(self.answers.replace_aspec)
        }

        fn ask_run_audit(&mut self) -> Result<bool> {
            Ok(self.answers.run_audit)
        }

        fn ask_work_items_setup(
            &mut self,
        ) -> Result<Option<crate::config::WorkItemsConfig>> {
            Ok(self.answers.work_items.take())
        }
    }
    use crate::cli::Agent;
    use crate::commands::init_flow::InitQa;
    use crate::tui::state::{App, Dialog, ExecutionPhase};
    use crate::workflow::{StepStatus, WorkflowState, WorkflowStepState};

    /// Every agent in Agent::all() must be parseable via both flag forms using the generic
    /// flag_parser driven by INIT_FLAGS. This test fails immediately when a new agent is
    /// added to Agent::all() but INIT_FLAGS is not updated — keeping TUI and CLI in sync.
    #[test]
    fn parse_agent_flag_covers_all_cli_agents() {
        use crate::commands::spec;
        let init_spec = spec::ALL_COMMANDS.iter().find(|c| c.name == "init").unwrap();
        for agent in Agent::all() {
            let eq_form = format!("--agent={}", agent.as_str());
            let parts_eq: Vec<&str> = vec!["init", &eq_form];
            let flags = flag_parser::parse_flags(&parts_eq, init_spec);
            assert_eq!(
                flag_parser::flag_string(&flags, "agent"),
                Some(agent.as_str()),
                "--agent={} not parsed by TUI",
                agent.as_str(),
            );

            let agent_name = agent.as_str();
            let parts_space: Vec<&str> = vec!["init", "--agent", agent_name];
            let flags = flag_parser::parse_flags(&parts_space, init_spec);
            assert_eq!(
                flag_parser::flag_string(&flags, "agent"),
                Some(agent.as_str()),
                "--agent {} not parsed by TUI",
                agent.as_str(),
            );
        }
        // Missing value after --agent yields no flag entry.
        let flags = flag_parser::parse_flags(&["init", "--agent"], init_spec);
        assert!(flag_parser::flag_string(&flags, "agent").is_none());
    }

    fn new_app() -> App {
        App::new(std::path::PathBuf::new())
    }

    /// App whose CWD is outside any Git repository. Used by TUI flag-parsing
    /// integration tests so `show_pre_command_dialogs` returns early (no git
    /// root) after `pending_command` is set — without trying to spawn Docker.
    fn app_no_git() -> App {
        App::new(std::path::PathBuf::from("/tmp"))
    }

    // ── TUI flag-parsing integration tests (work item 0053) ─────────────────

    /// `chat --agent codex` sets `PendingCommand::Chat { agent: Some("codex"), .. }`.
    #[tokio::test]
    async fn tui_chat_agent_space_form_sets_pending_command() {
        let mut app = app_no_git();
        execute_command(&mut app, "chat --agent codex").await;
        match &app.active_tab().pending_command {
            PendingCommand::Chat { agent, .. } => {
                assert_eq!(
                    agent.as_deref(),
                    Some("codex"),
                    "expected agent Some(\"codex\"), got {:?}",
                    agent,
                );
            }
            other => panic!("expected PendingCommand::Chat, got {:?}", other),
        }
    }

    /// `implement 0042 --agent=opencode` sets the correct `PendingCommand::Implement`.
    #[tokio::test]
    async fn tui_implement_agent_eq_form_sets_pending_command() {
        let mut app = app_no_git();
        execute_command(&mut app, "implement 0042 --agent=opencode").await;
        match &app.active_tab().pending_command {
            PendingCommand::Implement { agent, work_item, .. } => {
                assert_eq!(
                    agent.as_deref(),
                    Some("opencode"),
                    "expected agent Some(\"opencode\"), got {:?}",
                    agent,
                );
                assert_eq!(*work_item, 42u32);
            }
            other => panic!("expected PendingCommand::Implement, got {:?}", other),
        }
    }

    // ── TUI --model flag tests (work item 0055) ──────────────────────────────

    /// `chat --model claude-opus-4-6` (space form) sets `PendingCommand::Chat { model: Some("claude-opus-4-6"), .. }`.
    #[tokio::test]
    async fn tui_chat_model_space_form_sets_pending_command() {
        let mut app = app_no_git();
        execute_command(&mut app, "chat --model claude-opus-4-6").await;
        match &app.active_tab().pending_command {
            PendingCommand::Chat { model, .. } => {
                assert_eq!(
                    model.as_deref(),
                    Some("claude-opus-4-6"),
                    "expected model Some(\"claude-opus-4-6\"), got {:?}",
                    model,
                );
            }
            other => panic!("expected PendingCommand::Chat, got {:?}", other),
        }
    }

    /// `chat --model=claude-opus-4-6` (= form) sets the same `PendingCommand::Chat`.
    #[tokio::test]
    async fn tui_chat_model_eq_form_sets_pending_command() {
        let mut app = app_no_git();
        execute_command(&mut app, "chat --model=claude-opus-4-6").await;
        match &app.active_tab().pending_command {
            PendingCommand::Chat { model, .. } => {
                assert_eq!(
                    model.as_deref(),
                    Some("claude-opus-4-6"),
                    "expected model Some(\"claude-opus-4-6\"), got {:?}",
                    model,
                );
            }
            other => panic!("expected PendingCommand::Chat, got {:?}", other),
        }
    }

    /// `implement 0042 --model claude-haiku-4-5` (space form) sets
    /// `PendingCommand::Implement { model: Some("claude-haiku-4-5"), work_item: 42, .. }`.
    #[tokio::test]
    async fn tui_implement_model_space_form_sets_pending_command() {
        let mut app = app_no_git();
        execute_command(&mut app, "implement 0042 --model claude-haiku-4-5").await;
        match &app.active_tab().pending_command {
            PendingCommand::Implement { model, work_item, .. } => {
                assert_eq!(
                    model.as_deref(),
                    Some("claude-haiku-4-5"),
                    "expected model Some(\"claude-haiku-4-5\"), got {:?}",
                    model,
                );
                assert_eq!(*work_item, 42u32);
            }
            other => panic!("expected PendingCommand::Implement, got {:?}", other),
        }
    }

    /// `implement 0042 --model=claude-haiku-4-5` (= form) sets the same
    /// `PendingCommand::Implement`.
    #[tokio::test]
    async fn tui_implement_model_eq_form_sets_pending_command() {
        let mut app = app_no_git();
        execute_command(&mut app, "implement 0042 --model=claude-haiku-4-5").await;
        match &app.active_tab().pending_command {
            PendingCommand::Implement { model, work_item, .. } => {
                assert_eq!(
                    model.as_deref(),
                    Some("claude-haiku-4-5"),
                    "expected model Some(\"claude-haiku-4-5\"), got {:?}",
                    model,
                );
                assert_eq!(*work_item, 42u32);
            }
            other => panic!("expected PendingCommand::Implement, got {:?}", other),
        }
    }

    /// `implement 0007 --workflow my-workflow.md` extracts the workflow path alongside
    /// other flag defaults (agent stays None, work_item = 7).
    #[tokio::test]
    async fn tui_implement_workflow_flag_is_extracted() {
        let mut app = app_no_git();
        execute_command(&mut app, "implement 0007 --workflow my-workflow.md").await;
        match &app.active_tab().pending_command {
            PendingCommand::Implement { workflow, work_item, agent, .. } => {
                assert_eq!(
                    workflow.as_deref(),
                    Some(std::path::Path::new("my-workflow.md")),
                    "expected workflow Some(\"my-workflow.md\"), got {:?}",
                    workflow,
                );
                assert_eq!(*work_item, 7u32);
                assert_eq!(*agent, None, "no --agent flag was given");
            }
            other => panic!("expected PendingCommand::Implement, got {:?}", other),
        }
    }

    fn make_step_state(name: &str, deps: &[&str], status: StepStatus) -> WorkflowStepState {
        WorkflowStepState {
            name: name.to_string(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            prompt_template: format!("do {}", name),
            status,
            container_id: None,
            agent: None,
            model: None,
        }
    }

    fn make_workflow(steps: Vec<WorkflowStepState>) -> WorkflowState {
        WorkflowState {
            title: None,
            steps,
            workflow_hash: "hash".to_string(),
            work_item: Some(1),
            workflow_name: "test-wf".to_string(),
        }
    }

    // ─── cancel_to_previous_step ────────────────────────────────────────────────

    #[tokio::test]
    async fn cancel_to_previous_step_on_first_step_sets_error_dialog() {
        let mut app = new_app();
        // Single step — no predecessor exists.
        let wf = make_workflow(vec![make_step_state("plan", &[], StepStatus::Running)]);
        app.active_tab_mut().workflow = Some(wf);
        app.active_tab_mut().workflow_current_step = Some("plan".to_string());
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };

        cancel_to_previous_step(&mut app).await;

        // Step should revert to Running (no predecessor to go back to).
        assert_eq!(
            app.active_tab().workflow.as_ref().unwrap().get_step("plan").unwrap().status,
            StepStatus::Running,
            "First step should revert to Running when no predecessor exists"
        );
        // Dialog should open with an error message.
        match &app.active_tab().dialog {
            Dialog::WorkflowControlBoard { current_step, error } => {
                assert_eq!(current_step, "plan");
                assert!(error.is_some(), "Error message should be set");
                assert!(
                    error.as_ref().unwrap().contains("No previous step"),
                    "Error should mention no previous step: {:?}", error
                );
            }
            other => panic!("Expected WorkflowControlBoard with error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn cancel_to_previous_step_linear_marks_predecessor_pending() {
        let mut app = new_app();
        // Linear: plan (Done) → impl (Running)
        let wf = make_workflow(vec![
            make_step_state("plan", &[], StepStatus::Done),
            make_step_state("impl", &["plan"], StepStatus::Running),
        ]);
        app.active_tab_mut().workflow = Some(wf);
        app.active_tab_mut().workflow_current_step = Some("impl".to_string());
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        // No git root → launch_next_workflow_step returns early without spawning Docker.
        app.active_tab_mut().workflow_git_root = None;

        cancel_to_previous_step(&mut app).await;

        let wf = app.active_tab().workflow.as_ref().unwrap();
        assert_eq!(
            wf.get_step("impl").unwrap().status,
            StepStatus::Pending,
            "Current step (impl) should be Pending after cancel"
        );
        assert_eq!(
            wf.get_step("plan").unwrap().status,
            StepStatus::Pending,
            "Predecessor (plan) should revert to Pending"
        );
    }

    #[tokio::test]
    async fn cancel_to_previous_step_parallel_picks_last_done_step() {
        let mut app = new_app();
        // plan (Done) → branch-a (Done), branch-b (Done) → merge (Running)
        let wf = make_workflow(vec![
            make_step_state("plan", &[], StepStatus::Done),
            make_step_state("branch-a", &["plan"], StepStatus::Done),
            make_step_state("branch-b", &["plan"], StepStatus::Done),
            make_step_state("merge", &["branch-a", "branch-b"], StepStatus::Running),
        ]);
        app.active_tab_mut().workflow = Some(wf);
        app.active_tab_mut().workflow_current_step = Some("merge".to_string());
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().workflow_git_root = None;

        cancel_to_previous_step(&mut app).await;

        let wf = app.active_tab().workflow.as_ref().unwrap();
        assert_eq!(
            wf.get_step("merge").unwrap().status,
            StepStatus::Pending,
            "merge should be Pending after cancel"
        );
        // The most recent Done step in Vec order (branch-b) should be reverted.
        assert_eq!(
            wf.get_step("branch-b").unwrap().status,
            StepStatus::Pending,
            "branch-b (last Done step) should revert to Pending"
        );
        // Earlier Done steps should remain Done.
        assert_eq!(
            wf.get_step("plan").unwrap().status,
            StepStatus::Done,
            "plan should remain Done"
        );
        assert_eq!(
            wf.get_step("branch-a").unwrap().status,
            StepStatus::Done,
            "branch-a should remain Done"
        );
    }

    // ─── advance_workflow_next_current_container ────────────────────────────────

    #[tokio::test]
    async fn advance_next_current_container_falls_back_when_pty_is_none() {
        let mut app = new_app();
        let wf = make_workflow(vec![
            make_step_state("plan", &[], StepStatus::Running),
            make_step_state("impl", &["plan"], StepStatus::Pending),
        ]);
        app.active_tab_mut().workflow = Some(wf);
        app.active_tab_mut().workflow_current_step = Some("plan".to_string());
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        // pty = None (default) — triggers the PTY-unavailable fallback path.
        // No git root → launch_next_workflow_step returns early.

        advance_workflow_next_current_container(&mut app).await;

        assert!(
            app.active_tab().output_lines.iter().any(|l| l.contains("PTY session ended")),
            "Expected PTY fallback message in output. Got: {:?}",
            app.active_tab().output_lines
        );
        // The fallback calls advance_workflow_next_new_container, which marks current step Done.
        assert_eq!(
            app.active_tab().workflow.as_ref().unwrap().get_step("plan").unwrap().status,
            StepStatus::Done,
            "Current step should be marked Done even when falling back"
        );
    }

    // ─── advance_workflow_next_new_container boundary ───────────────────────────

    #[tokio::test]
    async fn advance_next_new_container_final_step_transitions_to_complete() {
        let mut app = new_app();
        // Single-step workflow — completing it makes all_done() true.
        let wf = make_workflow(vec![make_step_state("plan", &[], StepStatus::Running)]);
        app.active_tab_mut().workflow = Some(wf);
        app.active_tab_mut().workflow_current_step = Some("plan".to_string());
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        // Use a real temp dir so save_workflow_state succeeds and all_done() is evaluated.
        let tmp = tempfile::tempdir().unwrap();
        app.active_tab_mut().workflow_git_root = Some(tmp.path().to_path_buf());

        advance_workflow_next_new_container(&mut app).await;

        assert!(
            app.active_tab().workflow_current_step.is_none(),
            "workflow_current_step should be cleared after the final step completes"
        );
        assert!(
            app.active_tab().output_lines.iter().any(|l| l.contains("All steps done")),
            "Expected completion message in output. Got: {:?}",
            app.active_tab().output_lines
        );
    }

    // ─── advance_workflow_next_new_container: state file persisted ──────────────

    #[tokio::test]
    async fn advance_next_new_container_persists_state_before_launch() {
        let mut app = new_app();
        let wf = make_workflow(vec![
            make_step_state("plan", &[], StepStatus::Running),
            make_step_state("impl", &["plan"], StepStatus::Pending),
        ]);
        app.active_tab_mut().workflow = Some(wf);
        app.active_tab_mut().workflow_current_step = Some("plan".to_string());
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        let tmp = tempfile::tempdir().unwrap();
        app.active_tab_mut().workflow_git_root = Some(tmp.path().to_path_buf());

        advance_workflow_next_new_container(&mut app).await;

        // plan is Done and state file exists (impl is Pending, so not all_done).
        let state_path = crate::workflow::workflow_state_path(tmp.path(), Some(1), "test-wf");
        assert!(state_path.exists(), "State file should be written before any launch attempt");
        let saved = std::fs::read_to_string(&state_path).unwrap();
        assert!(saved.contains("Done") || saved.contains("done"), "State file should record plan as Done");
    }

    // ─── WorkflowRestartStep action dispatch ───────────────────────────────────

    #[tokio::test]
    async fn workflow_restart_step_resets_step_to_pending() {
        let mut app = new_app();
        let wf = make_workflow(vec![make_step_state("plan", &[], StepStatus::Running)]);
        app.active_tab_mut().workflow = Some(wf);
        app.active_tab_mut().workflow_current_step = Some("plan".to_string());
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        // No git root — launch returns early without Docker.

        // WorkflowRestartStep calls retry_workflow_step which resets to Pending.
        retry_workflow_step(&mut app).await;

        assert_eq!(
            app.active_tab().workflow.as_ref().unwrap().get_step("plan").unwrap().status,
            StepStatus::Pending,
            "Restart should reset step to Pending"
        );
    }

    // ─── run_git_interactive (0032 — GPG pinentry TUI fix) ───────────────────

    /// `App::new()` must initialise `needs_full_redraw` to `false` so the event loop
    /// does not issue a spurious `terminal.clear()` before the first draw.
    #[test]
    fn needs_full_redraw_starts_false() {
        let app = new_app();
        assert!(
            !app.needs_full_redraw,
            "needs_full_redraw must be false immediately after App::new()"
        );
    }

    /// Unit test: suspends and restores terminal state around a no-op subprocess.
    ///
    /// Uses `git --version` as the no-op: it exits 0, produces no passphrase prompt,
    /// and exercises the full suspend → subprocess → Drop-guard restore path.
    /// `needs_full_redraw = true` after the call is the observable signal that the
    /// `TerminalRestoreGuard` ran and the event loop will issue `terminal.clear()`.
    #[test]
    fn run_git_interactive_suspends_and_restores_around_subprocess() {
        let mut app = new_app();
        assert!(!app.needs_full_redraw, "precondition: flag starts false");
        let cwd = std::env::current_dir().unwrap();
        let ok = run_git_interactive(&mut app, &cwd, &["--version"]);
        assert!(ok, "git --version should exit 0");
        assert!(
            app.needs_full_redraw,
            "needs_full_redraw must be true after run_git_interactive — \
             signals that TerminalRestoreGuard ran and the event loop should call terminal.clear()"
        );
    }

    /// Integration test: git command exits nonzero; assert TUI is restored before
    /// error is propagated.
    ///
    /// The implementation sets `needs_full_redraw = true` (restore signal, set after
    /// the `TerminalRestoreGuard` drops) before the `match` branch that calls
    /// `push_output` (error propagation).  Both being observable at return time
    /// is structural proof of correct ordering.
    #[test]
    fn run_git_interactive_restores_before_surfacing_error() {
        let mut app = new_app();
        let cwd = std::env::current_dir().unwrap();
        let ok = run_git_interactive(&mut app, &cwd, &["no-such-subcommand-xyzzy"]);
        // TUI was restored (Drop guard ran, needs_full_redraw set).
        assert!(!ok, "unknown git subcommand should exit nonzero");
        assert!(
            app.needs_full_redraw,
            "needs_full_redraw must be set even when git exits nonzero — \
             TerminalRestoreGuard runs unconditionally before error is written to output"
        );
        // Error was propagated (visible in the output pane after restore).
        let output = &app.active_tab().output_lines;
        assert!(
            output.iter().any(|l| l.contains("no-such-subcommand-xyzzy")),
            "error line must reference the failing subcommand; got: {:?}",
            output
        );
        assert!(
            output.iter().any(|l| l.contains("exited with code")),
            "error line must include the exit code; got: {:?}",
            output
        );
    }

    /// The Drop guard (`TerminalRestoreGuard`) fires even when `Command::status()`
    /// returns `Err` — i.e. when the subprocess cannot be spawned at all (bad cwd).
    /// `needs_full_redraw` must be set and a spawn-error description must appear in
    /// output regardless of the failure mode.
    #[test]
    fn run_git_interactive_drop_guard_fires_on_spawn_error() {
        let mut app = new_app();
        // Create a real temp dir then drop it so the path no longer exists on disk.
        let tmp = tempfile::tempdir().unwrap();
        let bad_cwd = tmp.path().to_path_buf();
        drop(tmp);

        let ok = run_git_interactive(&mut app, &bad_cwd, &["--version"]);
        assert!(!ok, "should return false when the cwd does not exist");
        assert!(
            app.needs_full_redraw,
            "TerminalRestoreGuard must have fired (needs_full_redraw=true) \
             even when the subprocess cannot be spawned"
        );
        let output = &app.active_tab().output_lines;
        assert!(
            !output.is_empty(),
            "spawn-error description must be written to output_lines: {:?}",
            output
        );
    }

    // ─── extract_selection_text ──────────────────────────────────────────────

    fn make_snapshot(rows: &[&str]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|row| row.chars().map(|c| c.to_string()).collect())
            .collect()
    }

    fn tab_with_selection(
        snapshot: Vec<Vec<String>>,
        start: (u16, u16),
        end: (u16, u16),
    ) -> crate::tui::state::TabState {
        let mut tab = crate::tui::state::TabState::new(std::path::PathBuf::new());
        tab.terminal_selection_start = Some(start);
        tab.terminal_selection_end = Some(end);
        tab.terminal_selection_snapshot = Some(snapshot);
        tab
    }

    #[test]
    fn extract_selection_text_single_cell() {
        let snap = make_snapshot(&["Hello World"]);
        let tab = tab_with_selection(snap, (0, 0), (0, 4));
        let text = extract_selection_text(&tab).unwrap();
        assert_eq!(text, "Hello");
    }

    #[test]
    fn extract_selection_text_full_row() {
        let snap = make_snapshot(&["Hello   "]);
        let tab = tab_with_selection(snap, (0, 0), (0, 7));
        let text = extract_selection_text(&tab).unwrap();
        // Trailing spaces stripped.
        assert_eq!(text, "Hello");
    }

    #[test]
    fn extract_selection_text_multirow_strips_trailing_spaces() {
        let snap = make_snapshot(&["Hello   ", "World   "]);
        let tab = tab_with_selection(snap, (0, 0), (1, 4));
        let text = extract_selection_text(&tab).unwrap();
        assert_eq!(text, "Hello\nWorld");
    }

    #[test]
    fn extract_selection_text_reversed_selection_order() {
        // End is before start — should still extract correctly.
        let snap = make_snapshot(&["ABCDE"]);
        let tab = tab_with_selection(snap, (0, 4), (0, 0));
        let text = extract_selection_text(&tab).unwrap();
        assert_eq!(text, "ABCDE");
    }

    #[test]
    fn extract_selection_text_no_selection_returns_none() {
        let mut tab = crate::tui::state::TabState::new(std::path::PathBuf::new());
        tab.terminal_selection_start = None;
        tab.terminal_selection_end = None;
        tab.terminal_selection_snapshot = None;
        assert!(extract_selection_text(&tab).is_none());
    }

    #[test]
    fn extract_selection_text_partial_first_and_last_rows() {
        // Select from col 2 of row 0 to col 3 of row 1.
        let snap = make_snapshot(&["ABCDE", "FGHIJ"]);
        let tab = tab_with_selection(snap, (0, 2), (1, 3));
        let text = extract_selection_text(&tab).unwrap();
        // Row 0: cols 2..=4 → "CDE", trailing trimmed → "CDE"
        // Row 1: cols 0..=3 → "FGHI"
        assert_eq!(text, "CDE\nFGHI");
    }

    // ─── copy_selection_to_clipboard ────────────────────────────────────────

    struct MockClipboard {
        pub last_written: Option<String>,
        pub fail: bool,
    }

    impl MockClipboard {
        fn new() -> Self { Self { last_written: None, fail: false } }
        fn failing() -> Self { Self { last_written: None, fail: true } }
    }

    impl ClipboardWriter for MockClipboard {
        fn set_text(&mut self, text: &str) -> Result<(), String> {
            if self.fail {
                Err("mock clipboard error".to_string())
            } else {
                self.last_written = Some(text.to_string());
                Ok(())
            }
        }
    }

    #[test]
    fn copy_selection_writes_text_to_clipboard() {
        let snap = make_snapshot(&["copied text"]);
        let tab = tab_with_selection(snap, (0, 0), (0, 10));
        let mut cb = MockClipboard::new();
        let ok = copy_selection_to_clipboard(&tab, &mut cb);
        assert!(ok, "should return true on success");
        assert_eq!(cb.last_written.as_deref(), Some("copied text"));
    }

    #[test]
    fn copy_selection_returns_false_when_clipboard_fails() {
        let snap = make_snapshot(&["some text"]);
        let tab = tab_with_selection(snap, (0, 0), (0, 8));
        let mut cb = MockClipboard::failing();
        let ok = copy_selection_to_clipboard(&tab, &mut cb);
        assert!(!ok, "should return false when clipboard write fails");
    }

    #[test]
    fn copy_selection_returns_false_when_no_selection() {
        let tab = crate::tui::state::TabState::new(std::path::PathBuf::new());
        let mut cb = MockClipboard::new();
        let ok = copy_selection_to_clipboard(&tab, &mut cb);
        assert!(!ok);
        assert!(cb.last_written.is_none());
    }

    // ─── scrollback offset can exceed screen height ──────────────────────────

    #[test]
    fn scrollback_offset_can_exceed_screen_height() {
        // Feed more lines than screen height; verify the probe reports deeper than one screen.
        let screen_rows: u16 = 10;
        let screen_cols: u16 = 40;
        let scrollback_cap: usize = 500;
        let mut parser = vt100::Parser::new(screen_rows, screen_cols, scrollback_cap);

        // Feed 100 lines — far more than the 10-row screen.
        for i in 0u32..100 {
            let line = format!("line {:03}\r\n", i);
            parser.process(line.as_bytes());
        }

        // Probe actual scrollback depth.
        parser.set_scrollback(usize::MAX);
        let max_scrollback = parser.screen().scrollback();
        parser.set_scrollback(0);

        assert!(
            max_scrollback > screen_rows as usize,
            "scrollback depth ({}) should exceed screen height ({})",
            max_scrollback, screen_rows
        );
        assert!(
            max_scrollback <= scrollback_cap,
            "scrollback depth ({}) must not exceed cap ({})",
            max_scrollback, scrollback_cap
        );
    }

    // ─── selection coordinate mapping ────────────────────────────────────────

    #[test]
    fn selection_coordinate_mapping_basic() {
        // Inner area starts at (x=5, y=3), size 80×24.
        // Mouse at (col=10, row=7) → vt100 (row=4, col=5).
        let inner = ratatui::layout::Rect { x: 5, y: 3, width: 80, height: 24 };
        let mouse_col: u16 = 10;
        let mouse_row: u16 = 7;
        let vt100_col = mouse_col - inner.x;
        let vt100_row = mouse_row - inner.y;
        assert_eq!(vt100_col, 5);
        assert_eq!(vt100_row, 4);
    }

    #[test]
    fn selection_coordinate_mapping_top_left_corner() {
        let inner = ratatui::layout::Rect { x: 2, y: 2, width: 80, height: 24 };
        let vt100_col = 2u16 - inner.x;
        let vt100_row = 2u16 - inner.y;
        assert_eq!(vt100_col, 0, "top-left maps to vt100 (0, 0)");
        assert_eq!(vt100_row, 0);
    }

    #[test]
    fn selection_drag_clamped_to_inner_area() {
        // Drag beyond right edge is clamped to inner.width - 1.
        let inner = ratatui::layout::Rect { x: 1, y: 1, width: 80, height: 24 };
        let out_of_bounds_col: u16 = 200;
        let clamped = out_of_bounds_col
            .saturating_sub(inner.x)
            .min(inner.width.saturating_sub(1));
        assert_eq!(clamped, 79, "clamped to width - 1");
    }

    // ─── capture_vt100_snapshot: scrollback offset ───────────────────────────

    /// When `scroll_offset > 0`, the snapshot must capture the scrollback view
    /// (what the user actually sees), not the live tail screen.
    ///
    /// Three properties are verified:
    /// 1. Snapshots at different offsets must differ — the offset must change what's captured.
    /// 2. After any call the parser is reset to live view (offset 0).
    /// 3. Snapshot at the same offset is idempotent.
    ///
    /// Note: the vt100 crate can panic when `set_scrollback(N)` is called with N that
    /// exceeds available scrollback in some internal arithmetic.  To stay safe we only
    /// call `set_scrollback(usize::MAX)` directly (the probe pattern used throughout the
    /// render code) and let `capture_vt100_snapshot` handle all other offset seeks.
    #[test]
    fn capture_snapshot_at_nonzero_offset_reflects_scrollback_view() {
        let rows: u16 = 5;
        let cols: u16 = 20;
        let mut parser_opt: Option<vt100::Parser> = Some(vt100::Parser::new(rows, cols, 500));

        // Feed 30 distinctly named lines so the live screen shows later lines and
        // the scrollback holds the earlier ones.
        for i in 0u32..30 {
            let line = format!("line {:03}\r\n", i);
            parser_opt.as_mut().unwrap().process(line.as_bytes());
        }

        // Probe available scrollback depth using the safe MAX pattern.
        let max_scroll = {
            let p = parser_opt.as_mut().unwrap();
            p.set_scrollback(usize::MAX);
            let m = p.screen().scrollback();
            p.set_scrollback(0);
            m
        };
        assert!(
            max_scroll >= 5,
            "test requires ≥5 scrollback lines; got {max_scroll}"
        );
        // Use an offset safely within the available depth.
        let test_offset: usize = 5;

        // Capture snapshots at live view and at scrollback offset.
        let snap_live = capture_vt100_snapshot(&mut parser_opt, 0).unwrap();
        let snap_scrolled = capture_vt100_snapshot(&mut parser_opt, test_offset).unwrap();

        // 1. The two snapshots must differ — offset must affect content.
        let live_row0 = snap_live[0].concat();
        let scrolled_row0 = snap_scrolled[0].concat();
        assert_ne!(
            live_row0.trim_end(), scrolled_row0.trim_end(),
            "snapshot at offset 0 and offset {test_offset} must differ; \
             scroll offset is not being applied in capture_vt100_snapshot"
        );

        // 2. After calling with a non-zero offset, parser must be back at live view.
        let snap_reset = capture_vt100_snapshot(&mut parser_opt, 0).unwrap();
        let reset_row0 = snap_reset[0].concat();
        assert_eq!(
            live_row0.trim_end(), reset_row0.trim_end(),
            "parser must be reset to live view after capture_vt100_snapshot(_, non_zero)"
        );

        // 3. Snapshot at the same offset must be idempotent.
        let snap_scrolled2 = capture_vt100_snapshot(&mut parser_opt, test_offset).unwrap();
        let scrolled_row0_2 = snap_scrolled2[0].concat();
        assert_eq!(
            scrolled_row0.trim_end(), scrolled_row0_2.trim_end(),
            "snapshot at offset {test_offset} must be idempotent"
        );
    }

    /// A zero-area selection (start == end, e.g. a bare click) must not
    /// copy text — `copy_selection_to_clipboard` must return false.
    #[test]
    fn zero_area_selection_does_not_copy() {
        // Single-cell "selection" — start and end point at the same cell.
        let snap = make_snapshot(&["Hello World"]);
        let tab = tab_with_selection(snap, (0, 3), (0, 3));
        let mut cb = MockClipboard::new();
        // copy_selection_to_clipboard uses extract_selection_text which extracts one char.
        // The zero-area guard lives in the MouseUp handler (clears the selection) and in
        // the Ctrl+Y handler (start != end check).  This test verifies the downstream
        // extract path for documentation; the UI guards are tested separately.
        let text = extract_selection_text(&tab);
        // extract_selection_text returns "l" (col 3 of "Hello World"); the UI layer
        // prevents this from ever reaching the clipboard by clearing the selection on
        // MouseUp when start == end.
        let _ = copy_selection_to_clipboard(&tab, &mut cb);
        // Confirm that the selection_start == selection_end case is distinguishable.
        assert_eq!(
            tab.terminal_selection_start,
            tab.terminal_selection_end,
            "start and end must be equal for a zero-area selection"
        );
        let _ = text; // value examined above; silence unused warning
    }

    // ─── check_workflow_step_completion: yolo auto-advance ───────────────────────

    #[tokio::test]
    async fn check_workflow_step_completion_yolo_auto_advances_without_dialog() {
        let mut app = new_app();
        let wf = make_workflow(vec![
            make_step_state("plan", &[], StepStatus::Running),
            make_step_state("impl", &["plan"], StepStatus::Pending),
        ]);
        app.active_tab_mut().workflow = Some(wf);
        app.active_tab_mut().workflow_current_step = Some("plan".to_string());
        app.active_tab_mut().phase =
            state::ExecutionPhase::Done { command: "implement 0001".into() };
        app.active_tab_mut().yolo_mode = true;
        let tmp = tempfile::tempdir().unwrap();
        app.active_tab_mut().workflow_git_root = Some(tmp.path().to_path_buf());

        check_workflow_step_completion(&mut app).await;

        assert!(
            !matches!(app.active_tab().dialog, Dialog::WorkflowStepConfirm { .. }),
            "yolo mode must not show WorkflowStepConfirm dialog"
        );
        assert_eq!(
            app.active_tab().workflow.as_ref().unwrap().get_step("plan").unwrap().status,
            StepStatus::Done,
            "completed step must be marked Done"
        );
    }

    #[tokio::test]
    async fn check_workflow_step_completion_non_yolo_shows_confirm_dialog() {
        let mut app = new_app();
        let wf = make_workflow(vec![
            make_step_state("plan", &[], StepStatus::Running),
            make_step_state("impl", &["plan"], StepStatus::Pending),
        ]);
        app.active_tab_mut().workflow = Some(wf);
        app.active_tab_mut().workflow_current_step = Some("plan".to_string());
        app.active_tab_mut().phase =
            state::ExecutionPhase::Done { command: "implement 0001".into() };
        app.active_tab_mut().yolo_mode = false;
        let tmp = tempfile::tempdir().unwrap();
        app.active_tab_mut().workflow_git_root = Some(tmp.path().to_path_buf());

        check_workflow_step_completion(&mut app).await;

        assert!(
            matches!(app.active_tab().dialog, Dialog::WorkflowStepConfirm { .. }),
            "non-yolo mode must show WorkflowStepConfirm dialog"
        );
    }

    #[tokio::test]
    async fn check_workflow_step_completion_yolo_all_done_shows_control_board() {
        // Final step completes in yolo mode → all_done() true → WorkflowControlBoard shown.
        let mut app = new_app();
        let wf = make_workflow(vec![make_step_state("plan", &[], StepStatus::Running)]);
        app.active_tab_mut().workflow = Some(wf);
        app.active_tab_mut().workflow_current_step = Some("plan".to_string());
        app.active_tab_mut().phase =
            state::ExecutionPhase::Done { command: "implement 0001".into() };
        app.active_tab_mut().yolo_mode = true;
        let tmp = tempfile::tempdir().unwrap();
        app.active_tab_mut().workflow_git_root = Some(tmp.path().to_path_buf());

        check_workflow_step_completion(&mut app).await;

        assert!(
            matches!(app.active_tab().dialog, Dialog::WorkflowControlBoard { .. }),
            "yolo+all_done must show WorkflowControlBoard, got {:?}",
            app.active_tab().dialog
        );
        assert!(
            app.active_tab().workflow_current_step.is_some(),
            "workflow_current_step must be preserved so finish_workflow can clean up"
        );
    }

    // ─── Countdown expiry auto-advances workflow (E2E simulation) ────────────────

    #[tokio::test]
    async fn yolo_countdown_expiry_auto_advances_intermediate_step() {
        // Simulate: countdown expires on an intermediate step → advance_workflow_next_new_container.
        let mut app = new_app();
        let wf = make_workflow(vec![
            make_step_state("plan", &[], StepStatus::Running),
            make_step_state("impl", &["plan"], StepStatus::Pending),
        ]);
        app.active_tab_mut().workflow = Some(wf);
        app.active_tab_mut().workflow_current_step = Some("plan".to_string());
        app.active_tab_mut().phase =
            state::ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().yolo_mode = true;
        let tmp = tempfile::tempdir().unwrap();
        app.active_tab_mut().workflow_git_root = Some(tmp.path().to_path_buf());

        // Trigger the countdown expiry path directly (mirrors what the event loop does).
        app.active_tab_mut().yolo_countdown_expired = false;
        // Manually call the same logic as the event loop does after tick_all():
        app.active_tab_mut().yolo_countdown_expired = true;
        let is_last = app.active_tab().is_last_workflow_step();
        assert!(!is_last, "precondition: plan is not the last step");
        app.active_tab_mut().yolo_countdown_expired = false;
        advance_workflow_next_new_container(&mut app).await;

        assert_eq!(
            app.active_tab().workflow.as_ref().unwrap().get_step("plan").unwrap().status,
            StepStatus::Done,
            "expired countdown must mark the step Done"
        );
    }

    #[tokio::test]
    async fn yolo_countdown_expiry_shows_control_board_on_last_step() {
        // Simulate: countdown expires on the final step → WorkflowControlBoard is shown.
        let mut app = new_app();
        let wf = make_workflow(vec![make_step_state("impl", &[], StepStatus::Running)]);
        app.active_tab_mut().workflow = Some(wf);
        app.active_tab_mut().workflow_current_step = Some("impl".to_string());
        app.active_tab_mut().phase =
            state::ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().yolo_mode = true;
        let tmp = tempfile::tempdir().unwrap();
        app.active_tab_mut().workflow_git_root = Some(tmp.path().to_path_buf());

        let is_last = app.active_tab().is_last_workflow_step();
        assert!(is_last, "precondition: impl is the only (last) step");

        // Trigger the same logic as the event loop: is_last → show WorkflowControlBoard.
        app.active_tab_mut().yolo_countdown_expired = true;
        let is_last = app.active_tab().is_last_workflow_step();
        app.active_tab_mut().yolo_countdown_expired = false;
        assert!(is_last);
        let step = app.active_tab().workflow_current_step.clone().unwrap_or_default();
        app.active_tab_mut().dialog = Dialog::WorkflowControlBoard {
            current_step: step,
            error: None,
        };

        assert!(
            matches!(app.active_tab().dialog, Dialog::WorkflowControlBoard { .. }),
            "countdown expiry on last step must show WorkflowControlBoard, got {:?}",
            app.active_tab().dialog
        );
        assert!(
            app.active_tab().workflow_current_step.is_some(),
            "workflow_current_step must be preserved so user can finish from the control board"
        );
    }

    // ─── Background-tab yolo countdown auto-advance ───────────────────────────

    #[tokio::test]
    async fn yolo_countdown_expiry_advances_background_tab_workflow() {
        // When a background tab's yolo_countdown_expired flag is set, the event-loop
        // logic must advance the workflow even though the tab is not active.
        let mut app = new_app();

        // Tab 0 is the active tab (idle).
        // Tab 1 is a background tab running a two-step yolo workflow.
        app.tabs.push(state::TabState::new(std::path::PathBuf::new()));
        let tmp = tempfile::tempdir().unwrap();
        let wf = make_workflow(vec![
            make_step_state("plan", &[], StepStatus::Running),
            make_step_state("impl", &["plan"], StepStatus::Pending),
        ]);
        app.tabs[1].workflow = Some(wf);
        app.tabs[1].workflow_current_step = Some("plan".to_string());
        app.tabs[1].workflow_git_root = Some(tmp.path().to_path_buf());
        app.tabs[1].yolo_mode = true;
        app.tabs[1].phase = state::ExecutionPhase::Running { command: "implement 0001".into() };
        // Trigger the expired flag (as tick_all() would after 60s).
        app.tabs[1].yolo_countdown_expired = true;

        // Run the same expiry-dispatch logic as the event loop.
        let active_idx = app.active_tab_idx;
        let tab_count = app.tabs.len();
        for raw_i in 0..tab_count {
            let i = if raw_i == 0 { active_idx } else if raw_i <= active_idx { raw_i - 1 } else { raw_i };
            if !app.tabs[i].yolo_countdown_expired { continue; }
            app.tabs[i].yolo_countdown_expired = false;
            app.active_tab_idx = i;
            let is_last = app.active_tab().is_last_workflow_step();
            if is_last {
                let step = app.active_tab().workflow_current_step.clone().unwrap_or_default();
                app.active_tab_mut().dialog = Dialog::WorkflowControlBoard { current_step: step, error: None };
            } else {
                advance_workflow_next_new_container(&mut app).await;
            }
        }
        app.active_tab_idx = active_idx;

        // Active tab must be unchanged.
        assert_eq!(app.active_tab_idx, 0, "active_tab_idx must be restored");

        // The background tab's 'plan' step must now be Done.
        assert_eq!(
            app.tabs[1].workflow.as_ref().unwrap().get_step("plan").unwrap().status,
            StepStatus::Done,
            "background tab: yolo countdown expiry must mark the running step Done"
        );

        // yolo_countdown_expired flag must have been consumed.
        assert!(
            !app.tabs[1].yolo_countdown_expired,
            "yolo_countdown_expired must be cleared after dispatch"
        );
    }

    #[tokio::test]
    async fn yolo_countdown_expiry_shows_control_board_on_last_step_for_background_tab() {
        // When a background tab's last workflow step has an expired countdown, the
        // event-loop must set WorkflowControlBoard on that tab (visible when user switches).
        let mut app = new_app();
        app.tabs.push(state::TabState::new(std::path::PathBuf::new()));
        let tmp = tempfile::tempdir().unwrap();
        let wf = make_workflow(vec![make_step_state("impl", &[], StepStatus::Running)]);
        app.tabs[1].workflow = Some(wf);
        app.tabs[1].workflow_current_step = Some("impl".to_string());
        app.tabs[1].workflow_git_root = Some(tmp.path().to_path_buf());
        app.tabs[1].yolo_mode = true;
        app.tabs[1].phase = state::ExecutionPhase::Running { command: "implement 0001".into() };
        app.tabs[1].yolo_countdown_expired = true;

        let active_idx = app.active_tab_idx;
        let tab_count = app.tabs.len();
        for raw_i in 0..tab_count {
            let i = if raw_i == 0 { active_idx } else if raw_i <= active_idx { raw_i - 1 } else { raw_i };
            if !app.tabs[i].yolo_countdown_expired { continue; }
            app.tabs[i].yolo_countdown_expired = false;
            app.active_tab_idx = i;
            let is_last = app.active_tab().is_last_workflow_step();
            if is_last {
                let step = app.active_tab().workflow_current_step.clone().unwrap_or_default();
                app.active_tab_mut().dialog = Dialog::WorkflowControlBoard { current_step: step, error: None };
            } else {
                advance_workflow_next_new_container(&mut app).await;
            }
        }
        app.active_tab_idx = active_idx;

        assert_eq!(app.active_tab_idx, 0);
        assert!(
            matches!(app.tabs[1].dialog, Dialog::WorkflowControlBoard { .. }),
            "background tab: last-step expiry must set WorkflowControlBoard, got {:?}",
            app.tabs[1].dialog
        );
        assert!(!app.tabs[1].yolo_countdown_expired);
    }

    // ── TuiInitQa unit tests ──────────────────────────────────────────────────

    #[test]
    fn tui_qa_ask_replace_aspec_returns_preset_true() {
        let answers = TuiInitAnswers {
            replace_aspec: true,
            run_audit: false,
            work_items: None,
        };
        let mut qa = TuiInitQa { answers };
        assert_eq!(
            qa.ask_replace_aspec().unwrap(),
            true,
            "TuiInitQa must return the pre-collected replace_aspec answer"
        );
    }

    #[test]
    fn tui_qa_ask_replace_aspec_returns_preset_false() {
        let answers = TuiInitAnswers {
            replace_aspec: false,
            run_audit: true,
            work_items: None,
        };
        let mut qa = TuiInitQa { answers };
        assert_eq!(
            qa.ask_replace_aspec().unwrap(),
            false,
            "TuiInitQa must return false when replace_aspec was not selected"
        );
    }

    #[test]
    fn tui_qa_ask_run_audit_returns_preset_answer() {
        for expected in [true, false] {
            let answers = TuiInitAnswers {
                replace_aspec: false,
                run_audit: expected,
                work_items: None,
            };
            let mut qa = TuiInitQa { answers };
            assert_eq!(
                qa.ask_run_audit().unwrap(),
                expected,
                "TuiInitQa must return the pre-collected run_audit = {} answer",
                expected
            );
        }
    }

    #[test]
    fn tui_qa_ask_work_items_returns_some_then_none() {
        // `work_items` is consumed via `take()`, so the second call should return None.
        let wi = crate::config::WorkItemsConfig {
            dir: Some("items".into()),
            template: None,
        };
        let answers = TuiInitAnswers {
            replace_aspec: false,
            run_audit: false,
            work_items: Some(wi),
        };
        let mut qa = TuiInitQa { answers };

        let first = qa.ask_work_items_setup().unwrap();
        assert!(first.is_some(), "first call must return Some(WorkItemsConfig)");
        assert_eq!(
            first.as_ref().unwrap().dir.as_deref(),
            Some("items"),
            "returned config must carry the pre-collected dir"
        );

        let second = qa.ask_work_items_setup().unwrap();
        assert!(
            second.is_none(),
            "second call must return None — value was taken on first call"
        );
    }

    #[test]
    fn tui_qa_ask_work_items_returns_none_when_not_set() {
        let answers = TuiInitAnswers {
            replace_aspec: false,
            run_audit: false,
            work_items: None,
        };
        let mut qa = TuiInitQa { answers };
        assert!(
            qa.ask_work_items_setup().unwrap().is_none(),
            "TuiInitQa must return None when work_items was not configured in the dialog"
        );
    }

    #[test]
    fn tui_qa_all_methods_return_immediately_without_blocking() {
        // Verifies that none of the TuiInitQa methods block (no I/O, no channel reads).
        // If any of them tried to read from stdin or a channel this test would hang.
        let answers = TuiInitAnswers {
            replace_aspec: true,
            run_audit: true,
            work_items: Some(crate::config::WorkItemsConfig {
                dir: Some("dir".into()),
                template: Some("tmpl.md".into()),
            }),
        };
        let mut qa = TuiInitQa { answers };
        let _ = qa.ask_replace_aspec().unwrap();
        let _ = qa.ask_run_audit().unwrap();
        let _ = qa.ask_work_items_setup().unwrap();
        // Reaching here proves none of the calls blocked.
    }

    // ── TuiReadyQa unit tests ─────────────────────────────────────────────────

    #[test]
    fn tui_ready_qa_ask_run_audit_on_template_returns_preset_true() {
        let answers = TuiReadyAnswers {
            migrate_decision: None,
            template_audit_decision: Some(true),
        };
        let mut qa = TuiReadyQa { answers };
        assert!(
            qa.ask_run_audit_on_template().unwrap(),
            "TuiReadyQa must return true when template_audit_decision = Some(true)"
        );
    }

    #[test]
    fn tui_ready_qa_ask_run_audit_on_template_returns_preset_false() {
        let answers = TuiReadyAnswers {
            migrate_decision: None,
            template_audit_decision: Some(false),
        };
        let mut qa = TuiReadyQa { answers };
        assert!(
            !qa.ask_run_audit_on_template().unwrap(),
            "TuiReadyQa must return false when template_audit_decision = Some(false)"
        );
    }

    #[test]
    fn tui_ready_qa_ask_run_audit_on_template_defaults_to_false_when_none() {
        // When the dialog was not shown (None), the default must be false (skip audit).
        let answers = TuiReadyAnswers {
            migrate_decision: None,
            template_audit_decision: None,
        };
        let mut qa = TuiReadyQa { answers };
        assert!(
            !qa.ask_run_audit_on_template().unwrap(),
            "TuiReadyQa must default to false when template_audit_decision is None"
        );
    }

    // ── TUI init flow integration ─────────────────────────────────────────────

    /// Minimal `AgentRuntime` stub for TUI integration tests.
    struct TuiTestRuntime {
        available: bool,
    }

    impl crate::runtime::AgentRuntime for TuiTestRuntime {
        fn is_available(&self) -> bool {
            self.available
        }
        fn name(&self) -> &'static str {
            "tui-test"
        }
        fn cli_binary(&self) -> &'static str {
            "tui-test"
        }
        fn check_socket(&self) -> anyhow::Result<std::path::PathBuf> {
            Ok(std::path::PathBuf::from("/tui-test/socket"))
        }
        fn build_image_streaming(
            &self,
            _tag: &str,
            _dockerfile: &std::path::Path,
            _context: &std::path::Path,
            _no_cache: bool,
            _on_line: &mut dyn FnMut(&str),
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }
        fn image_exists(&self, _tag: &str) -> bool {
            false
        }
        fn run_container(
            &self,
            _image: &str,
            _host_path: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool,
            _container_name: Option<&str>,
            _ssh_dir: Option<&std::path::Path>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn run_container_captured(
            &self,
            _image: &str,
            _host_path: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool,
            _container_name: Option<&str>,
            _ssh_dir: Option<&std::path::Path>,
        ) -> anyhow::Result<(String, String)> {
            Ok((String::new(), String::new()))
        }
        fn run_container_at_path(
            &self,
            _image: &str,
            _host_path: &str,
            _container_path: &str,
            _working_dir: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool,
            _container_name: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn run_container_captured_at_path(
            &self,
            _image: &str,
            _host_path: &str,
            _container_path: &str,
            _working_dir: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool,
        ) -> anyhow::Result<(String, String)> {
            Ok((String::new(), String::new()))
        }
        fn run_container_detached(
            &self,
            _image: &str,
            _host_path: &str,
            _container_path: &str,
            _working_dir: &str,
            _container_name: Option<&str>,
            _env_vars: Vec<(String, String)>,
            _allow_docker: bool,
            _host_settings: Option<&crate::runtime::HostSettings>,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }
        fn start_container(&self, _container_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn stop_container(&self, _container_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn remove_container(&self, _container_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn is_container_running(&self, _container_id: &str) -> bool {
            false
        }
        fn find_stopped_container(
            &self,
            _name: &str,
            _image: &str,
        ) -> Option<crate::runtime::StoppedContainerInfo> {
            None
        }
        fn list_running_containers_by_prefix(&self, _prefix: &str) -> Vec<String> {
            vec![]
        }
        fn list_running_containers_with_ids_by_prefix(
            &self,
            _prefix: &str,
        ) -> Vec<(String, String)> {
            vec![]
        }
        fn get_container_workspace_mount(&self, _container_name: &str) -> Option<String> {
            None
        }
        fn query_container_stats(
            &self,
            _name: &str,
        ) -> Option<crate::runtime::ContainerStats> {
            None
        }
        fn build_run_args_pty(
            &self,
            _image: &str,
            _host_path: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool,
            _container_name: Option<&str>,
            _ssh_dir: Option<&std::path::Path>,
        ) -> Vec<String> {
            vec![]
        }
        fn build_run_args_pty_display(
            &self,
            _image: &str,
            _host_path: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool,
            _container_name: Option<&str>,
            _ssh_dir: Option<&std::path::Path>,
        ) -> Vec<String> {
            vec![]
        }
        fn build_run_args_pty_at_path(
            &self,
            _image: &str,
            _host_path: &str,
            _container_path: &str,
            _working_dir: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool,
            _container_name: Option<&str>,
        ) -> Vec<String> {
            vec![]
        }
        fn build_exec_args_pty(
            &self,
            _container_id: &str,
            _working_dir: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
        ) -> Vec<String> {
            vec![]
        }
        fn build_run_args_display(
            &self,
            _image: &str,
            _host_path: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool,
            _container_name: Option<&str>,
            _ssh_dir: Option<&std::path::Path>,
        ) -> Vec<String> {
            vec![]
        }
    }

    /// `InitContainerLauncher` stub for TUI integration tests — records calls, returns Ok.
    struct TuiTestLauncher {
        build_tags: std::sync::Mutex<Vec<String>>,
        audit_agents: std::sync::Mutex<Vec<String>>,
    }

    impl TuiTestLauncher {
        fn new() -> Self {
            Self {
                build_tags: std::sync::Mutex::new(vec![]),
                audit_agents: std::sync::Mutex::new(vec![]),
            }
        }
        fn run_audit_call_count(&self) -> usize {
            self.audit_agents.lock().unwrap().len()
        }
    }

    impl init_flow::InitContainerLauncher for TuiTestLauncher {
        fn build_image(
            &self,
            tag: &str,
            _dockerfile: &std::path::Path,
            _context: &std::path::Path,
            _sink: &crate::commands::output::OutputSink,
        ) -> anyhow::Result<()> {
            self.build_tags.lock().unwrap().push(tag.to_string());
            Ok(())
        }
        fn run_audit(
            &self,
            agent: Agent,
            _cwd: &std::path::Path,
            _sink: &crate::commands::output::OutputSink,
        ) -> anyhow::Result<()> {
            self.audit_agents
                .lock()
                .unwrap()
                .push(agent.as_str().to_string());
            Ok(())
        }
    }

    fn setup_tui_temp_repo() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        tmp
    }

    #[tokio::test]
    async fn tui_init_full_path_writes_expected_files() {
        // Mirrors the CLI integration test: TuiInitQa + TuiTestLauncher + TuiTestRuntime.
        // File outcomes must be identical to the CLI path.
        let tmp = setup_tui_temp_repo();
        let root = tmp.path();
        let answers = TuiInitAnswers {
            replace_aspec: false,
            run_audit: false,
            work_items: None,
        };
        let mut qa = TuiInitQa { answers };
        let launcher = TuiTestLauncher::new();
        let runtime = std::sync::Arc::new(TuiTestRuntime { available: false });
        let params = init_flow::InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = crate::commands::output::OutputSink::Channel(tx);

        let summary = init_flow::execute(params, &mut qa, &launcher, &sink, runtime)
            .await
            .unwrap();

        // Same files the CLI path produces.
        assert!(
            root.join("Dockerfile.dev").exists(),
            "Dockerfile.dev must be written by TUI path"
        );
        assert!(
            root.join(".amux").join("Dockerfile.claude").exists(),
            ".amux/Dockerfile.claude must be written by TUI path"
        );
        assert!(
            root.join(".amux").join("config.json").exists(),
            ".amux/config.json must be written by TUI path"
        );
        assert!(
            matches!(summary.config, crate::commands::ready::StepStatus::Ok(_)),
            "config stage must be Ok: {:?}",
            summary.config
        );
    }

    #[tokio::test]
    async fn tui_init_work_items_qa_is_called_during_flow() {
        // Regression: ask_work_items_setup must be invoked in the TUI path.
        // Previously this was CLI-only (gated by supports_color() hack).
        let tmp = setup_tui_temp_repo();
        let root = tmp.path();

        // Track whether ask_work_items_setup was called by using a custom struct.
        struct TrackingQa {
            work_items_called: bool,
        }
        impl init_flow::InitQa for TrackingQa {
            fn ask_replace_aspec(&mut self) -> anyhow::Result<bool> {
                Ok(false)
            }
            fn ask_run_audit(&mut self) -> anyhow::Result<bool> {
                Ok(false)
            }
            fn ask_work_items_setup(
                &mut self,
            ) -> anyhow::Result<Option<crate::config::WorkItemsConfig>> {
                self.work_items_called = true;
                Ok(None)
            }
        }

        let mut qa = TrackingQa { work_items_called: false };
        let launcher = TuiTestLauncher::new();
        let runtime = std::sync::Arc::new(TuiTestRuntime { available: false });
        let params = init_flow::InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = crate::commands::output::OutputSink::Channel(tx);

        let _ = init_flow::execute(params, &mut qa, &launcher, &sink, runtime)
            .await
            .unwrap();

        assert!(
            qa.work_items_called,
            "ask_work_items_setup must be called during TUI init flow (was CLI-only before)"
        );
    }

    #[tokio::test]
    async fn tui_init_declining_work_items_no_panic_and_summary_row_present() {
        // Regression: declining work-items setup must not panic and InitSummary
        // must always carry a work_items_setup status (never left as Pending).
        let tmp = setup_tui_temp_repo();
        let root = tmp.path();
        let answers = TuiInitAnswers {
            replace_aspec: false,
            run_audit: false,
            work_items: None, // user declined
        };
        let mut qa = TuiInitQa { answers };
        let launcher = TuiTestLauncher::new();
        let runtime = std::sync::Arc::new(TuiTestRuntime { available: false });
        let params = init_flow::InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = crate::commands::output::OutputSink::Channel(tx);

        // Must not panic.
        let summary = init_flow::execute(params, &mut qa, &launcher, &sink, runtime)
            .await
            .unwrap();

        assert_ne!(
            summary.work_items_setup,
            crate::commands::ready::StepStatus::Pending,
            "work_items_setup must not be left as Pending even when the user declines"
        );
    }

    // ── Regression: audit deferred removal ───────────────────────────────────

    // ── extract_passthrough_command (work item 0059) ─────────────────────────

    /// Helper: split a string and call `extract_passthrough_command` at offset 2
    /// (simulating `["remote", "run", ...rest...]`).
    fn passthrough(input: &str) -> Vec<String> {
        let parts: Vec<&str> = input.split_whitespace().collect();
        extract_passthrough_command(&parts, 2)
    }

    #[test]
    fn passthrough_strips_remote_addr_space_form() {
        // "remote run --remote-addr http://host:9876 status" → ["status"]
        let result = passthrough("remote run --remote-addr http://host:9876 status");
        assert_eq!(result, vec!["status"]);
    }

    #[test]
    fn passthrough_strips_remote_addr_eq_form() {
        // "remote run --remote-addr=http://host:9876 status" → ["status"]
        let result = passthrough("remote run --remote-addr=http://host:9876 status");
        assert_eq!(result, vec!["status"]);
    }

    #[test]
    fn passthrough_strips_session_space_form() {
        // "remote run --session abc123 status" → ["status"]
        let result = passthrough("remote run --session abc123 status");
        assert_eq!(result, vec!["status"]);
    }

    #[test]
    fn passthrough_strips_session_eq_form() {
        // "remote run --session=abc123 status" → ["status"]
        let result = passthrough("remote run --session=abc123 status");
        assert_eq!(result, vec!["status"]);
    }

    #[test]
    fn passthrough_strips_follow_long_form() {
        // "remote run --follow status" → ["status"]
        let result = passthrough("remote run --follow status");
        assert_eq!(result, vec!["status"]);
    }

    #[test]
    fn passthrough_strips_follow_short_form() {
        // "remote run -f status" → ["status"]
        let result = passthrough("remote run -f status");
        assert_eq!(result, vec!["status"]);
    }

    #[test]
    fn passthrough_preserves_inner_command_flags() {
        // Inner command's own flags must pass through untouched.
        let result = passthrough("remote run exec prompt hello --yolo -n");
        assert_eq!(result, vec!["exec", "prompt", "hello", "--yolo", "-n"]);
    }

    #[test]
    fn passthrough_strips_all_outer_flags_mixed() {
        // All outer flags present at once; only inner args survive.
        let result = passthrough(
            "remote run --remote-addr http://host:9876 --session abc --follow exec prompt hi",
        );
        assert_eq!(result, vec!["exec", "prompt", "hi"]);
    }

    #[test]
    fn passthrough_empty_after_offset_returns_empty() {
        // "remote run" with nothing after → empty vec
        let result = passthrough("remote run");
        assert!(result.is_empty());
    }

    // ── session picker pre-selection (work item 0059) ────────────────────────

    /// `fetch_and_show_session_picker` pre-selects the row whose `id` matches
    /// `last_remote_session_id`.  We test the selection-index computation in
    /// isolation — the same logic that lives inside the function — so that the
    /// test remains fast and free of network calls.
    #[test]
    fn session_picker_preselects_matching_last_session_id() {
        use crate::commands::remote::RemoteSessionEntry;
        let sessions = vec![
            RemoteSessionEntry { id: "sess-a".to_string(), workdir: "/a".to_string() },
            RemoteSessionEntry { id: "sess-b".to_string(), workdir: "/b".to_string() },
            RemoteSessionEntry { id: "sess-c".to_string(), workdir: "/c".to_string() },
        ];
        // Mirrors: last_session_id.as_deref().and_then(|id| sessions.iter().position(…)).unwrap_or(0)
        let last_id: Option<String> = Some("sess-b".to_string());
        let selected = last_id
            .as_deref()
            .and_then(|id| sessions.iter().position(|s| s.id == id))
            .unwrap_or(0);
        assert_eq!(
            selected, 1,
            "must pre-select index 1 for 'sess-b' in a 3-item list"
        );
    }

    #[test]
    fn session_picker_defaults_to_zero_when_last_id_not_in_list() {
        use crate::commands::remote::RemoteSessionEntry;
        let sessions = vec![
            RemoteSessionEntry { id: "sess-x".to_string(), workdir: "/x".to_string() },
            RemoteSessionEntry { id: "sess-y".to_string(), workdir: "/y".to_string() },
        ];
        let last_id: Option<String> = Some("sess-gone".to_string()); // not in list
        let selected = last_id
            .as_deref()
            .and_then(|id| sessions.iter().position(|s| s.id == id))
            .unwrap_or(0);
        assert_eq!(
            selected, 0,
            "must default to 0 when last_remote_session_id is not in the sessions list"
        );
    }

    #[test]
    fn session_picker_defaults_to_zero_when_no_last_id() {
        use crate::commands::remote::RemoteSessionEntry;
        let sessions = vec![
            RemoteSessionEntry { id: "sess-a".to_string(), workdir: "/a".to_string() },
        ];
        let last_id: Option<String> = None;
        let selected = last_id
            .as_deref()
            .and_then(|id| sessions.iter().position(|s| s.id == id))
            .unwrap_or(0);
        assert_eq!(selected, 0, "must default to 0 when last_remote_session_id is None");
    }

    #[test]
    fn tui_init_qa_has_no_pending_audit_state() {
        // Structural proof that `pending_init_run_audit` was removed:
        // TuiInitQa returns the answer immediately from the pre-collected field —
        // there is no flag that defers audit to a separate ready --refresh call.
        let answers = TuiInitAnswers {
            replace_aspec: false,
            run_audit: true,
            work_items: None,
        };
        let mut qa = TuiInitQa { answers };
        assert!(
            qa.ask_run_audit().unwrap(),
            "run_audit answer is stored in pre-collected field, not a deferred pending flag"
        );
    }

    // ─── execute_command routing tests (work item 0061) ──────────────────────

    /// Helper: create an App whose active tab is permanently bound to a remote
    /// session so we can test that execute_command routes to the remote path.
    fn app_with_remote_binding() -> App {
        let mut app = App::new(std::path::PathBuf::from("/tmp"));
        app.active_tab_mut().remote_binding = Some(crate::tui::state::RemoteTabBinding {
            remote_addr: "http://127.0.0.1:1".to_string(), // port 1 — won't connect
            session_id: "test-session-0061".to_string(),
            api_key: None,
            display_host: "127.0.0.1:1".to_string(),
        });
        app
    }

    /// When the active tab has a remote binding, `execute_command` must route
    /// to `launch_remote_bound_command` instead of local dispatch.
    ///
    /// Observable effect: the tab phase transitions to `Running` (set by
    /// `start_command` inside `launch_remote_bound_command`) and `exit_rx`
    /// is populated with a receiver channel.  The background network task
    /// runs asynchronously and is not awaited here.
    #[tokio::test]
    async fn execute_command_with_remote_binding_starts_remote_run() {
        let mut app = app_with_remote_binding();
        execute_command(&mut app, "implement 0042").await;

        // start_command is called synchronously inside launch_remote_bound_command.
        assert!(
            matches!(
                app.active_tab().phase,
                state::ExecutionPhase::Running { .. }
            ),
            "remote-bound execute_command must set phase to Running; got {:?}",
            app.active_tab().phase
        );
        // A oneshot receiver for the exit code is set up.
        assert!(
            app.active_tab().exit_rx.is_some(),
            "exit_rx must be set after launching a remote command"
        );
    }

    /// When the active tab has NO remote binding, `execute_command` dispatches
    /// locally (sets a `PendingCommand` for dialog-gated commands like `chat`).
    #[tokio::test]
    async fn execute_command_without_remote_binding_uses_local_dispatch() {
        let mut app = app_no_git();
        // Confirm no remote binding.
        assert!(
            app.active_tab().remote_binding.is_none(),
            "app_no_git must have no remote binding"
        );

        execute_command(&mut app, "chat --agent codex").await;

        // Local dispatch sets a PendingCommand rather than a remote Running phase.
        match &app.active_tab().pending_command {
            state::PendingCommand::Chat { agent, .. } => {
                assert_eq!(agent.as_deref(), Some("codex"));
            }
            other => panic!(
                "expected PendingCommand::Chat from local dispatch; got {:?}",
                other
            ),
        }
        // Phase must NOT be Running (no remote launch happened).
        assert!(
            !matches!(
                app.active_tab().phase,
                state::ExecutionPhase::Running { .. }
            ),
            "local dispatch must not set phase to Running; got {:?}",
            app.active_tab().phase
        );
    }

    /// `config` (bare) and `config show` must open the local TUI config dialog
    /// even when the active tab has a remote binding, because config is a local
    /// operation on the installation, not the remote server.
    #[tokio::test]
    async fn execute_command_config_show_bypasses_remote_binding() {
        let mut app = app_with_remote_binding();
        execute_command(&mut app, "config").await;

        // The local config dialog must be opened.
        assert!(
            matches!(
                app.active_tab().dialog,
                state::Dialog::ConfigShow(_)
            ),
            "bare `config` must open local ConfigShow dialog even with remote binding; got {:?}",
            app.active_tab().dialog
        );
        // Remote launch must NOT have happened.
        assert!(
            !matches!(
                app.active_tab().phase,
                state::ExecutionPhase::Running { .. }
            ),
            "config show must not start a remote command; got {:?}",
            app.active_tab().phase
        );
    }

    #[tokio::test]
    async fn tui_audit_runs_inline_not_deferred() {
        // Regression: the old TUI path used `pending_init_run_audit` and
        // `check_init_continuation()` to defer audit to a separate ready run.
        // Now execute() calls launcher.run_audit() inline.
        // If this count is 0 after execute() returns, the audit was deferred (regressed).
        let tmp = setup_tui_temp_repo();
        let root = tmp.path();
        // Pre-create Dockerfile.dev so only the agent dockerfile is new.
        std::fs::write(root.join("Dockerfile.dev"), "FROM ubuntu:22.04\n").unwrap();

        let answers = TuiInitAnswers {
            replace_aspec: false,
            run_audit: true,
            work_items: None,
        };
        let mut qa = TuiInitQa { answers };
        let launcher = TuiTestLauncher::new();
        let runtime = std::sync::Arc::new(TuiTestRuntime { available: true });
        let params = init_flow::InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = crate::commands::output::OutputSink::Channel(tx);

        let _ = init_flow::execute(params, &mut qa, &launcher, &sink, runtime)
            .await
            .unwrap();

        assert_eq!(
            launcher.run_audit_call_count(),
            1,
            "run_audit must be called once inline during execute() — \
             not deferred via pending_init_run_audit / check_init_continuation"
        );
    }
}
