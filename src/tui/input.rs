use crate::commands::new::WorkItemKind;
use crate::tui::state::{App, TabState, ConfigDialogState, ContainerWindowState, Dialog, ExecutionPhase, Focus, PendingCommand};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;
use strsim::levenshtein;

/// Describes what the event loop should do after processing a key press.
pub enum Action {
    None,
    /// User submitted a valid command string.
    Submit(String),
    /// Quit has been confirmed.
    QuitConfirmed,
    /// Mount scope dialog: user chose this path.
    MountScopeChosen(PathBuf),
    /// Agent auth dialog: user accepted.
    AuthAccepted,
    /// Agent auth dialog: user declined.
    AuthDeclined,
    /// Forward these raw bytes to the PTY.
    ForwardToPty(Vec<u8>),
    /// New work item: kind and title have been collected.
    NewWorkItem {
        kind: WorkItemKind,
        title: String,
        interview: bool,
    },
    /// New work item interview summary submitted.
    NewInterviewSummarySubmitted {
        kind: WorkItemKind,
        title: String,
        work_item_number: u32,
        summary: String,
    },
    /// Claws first-run wizard completed: proceed with launch.
    ClawsReadyProceed,
    /// Claws subsequent run: start the stopped container.
    ClawsReadyStartContainer,
    /// Claws subsequent run: restart the specific stopped container by ID.
    ClawsReadyRestartStopped { container_id: String },
    /// Claws subsequent run: restart failed — delete the stopped container and start fresh.
    ClawsReadyDeleteAndStartFresh { container_id: String },
    /// Claws audit confirmation accepted: launch the audit agent.
    ClawsAuditConfirmAccept,
    /// Claws audit confirmation declined: cancel the audit (and setup).
    ClawsAuditConfirmDecline,
    // Tab management actions:
    CreateTab,
    SwitchTabLeft,
    SwitchTabRight,
    CloseCurrentTab,
    NewTabDirectoryChosen(PathBuf),
    /// Workflow: advance to the next step.
    WorkflowAdvance,
    /// Workflow: abort the current workflow run.
    WorkflowAbort,
    /// Workflow: retry the failed step.
    WorkflowRetry,
    /// Workflow control board: restart the current step.
    WorkflowRestartStep,
    /// Workflow control board: cancel current step and return to previous step.
    WorkflowCancelToPrevious,
    /// Workflow control board: mark current step done, start next step in a new container.
    WorkflowNextInNewContainer,
    /// Workflow control board: mark current step done, send next step prompt to the existing PTY.
    WorkflowNextInCurrentContainer,
    /// Workflow control board: mark the last step done and terminate the container.
    WorkflowFinish,
    /// Workflow control board: disable auto-popup of stuck dialog for the current step.
    DisableAutoWorkflowForStep,
    /// Workflow: cancel execution — kill container, revert step to Pending, return tab to idle.
    WorkflowCancelExecution,
    /// Worktree merge prompt: merge the worktree branch into the current branch.
    WorktreeMerge,
    /// Worktree merge prompt: discard the worktree branch and remove the worktree.
    WorktreeDiscard,
    /// Worktree merge prompt: keep the worktree branch as-is without merging.
    WorktreeSkip,
    /// Worktree commit prompt: commit uncommitted files with the given message.
    WorktreeCommitFiles {
        message: String,
        branch: String,
        worktree_path: PathBuf,
        git_root: PathBuf,
    },
    /// Worktree merge confirm: proceed with squash-merge into current HEAD.
    WorktreeMergeConfirmed {
        branch: String,
        worktree_path: PathBuf,
        git_root: PathBuf,
    },
    /// Worktree delete confirm: remove the worktree directory and branch.
    WorktreeDeleteConfirmed {
        branch: String,
        worktree_path: PathBuf,
        git_root: PathBuf,
    },
    /// Worktree delete confirm: keep the worktree and branch as-is after merging.
    WorktreeKeepAfterMerge,
    /// Pre-worktree-creation: abort the implement command entirely.
    WorktreePreCommitAbort,
    /// Pre-worktree-creation: proceed using the last commit (ignore uncommitted files).
    WorktreePreCommitUse,
    /// Pre-worktree-creation: commit all files with the given message, then proceed.
    WorktreePreCommitCommit { message: String },
    /// Copy the current terminal text selection to the system clipboard.
    CopyToClipboard,
    /// Agent setup dialog: user accepted setting up the agent.
    /// `image_only` is true when the Dockerfile exists but the image is not built.
    AgentSetupAccepted { agent: String, image_only: bool },
    /// Agent setup dialog: user declined setup but accepted falling back to the default agent.
    AgentSetupFallbackAccepted { declined_agent: String, default_agent: String },
    /// Agent setup dialog: user declined downloading and building the missing agent Dockerfile.
    AgentSetupDeclined { agent: String },
    /// Ready: user chose to migrate from legacy single-file to modular Dockerfile layout.
    ReadyLegacyMigrate,
    /// Ready: user chose to keep the legacy single-file Dockerfile layout.
    ReadyLegacyKeep,
    /// Ready: user accepted launching the audit container when Dockerfile.dev matches the template.
    ReadyTemplateAuditAccept,
    /// Ready: user declined launching the audit container when Dockerfile.dev matches the template.
    ReadyTemplateAuditDecline,
    /// Init: user confirmed running the agent audit after init (with agent/aspec/replace_aspec state).
    InitAuditAccepted { agent: crate::cli::Agent, aspec: bool, replace_aspec: bool },
    /// Init: user declined the audit after init.
    InitAuditDeclined { agent: crate::cli::Agent, aspec: bool, replace_aspec: bool },
    /// Init: user confirmed replacing the existing aspec folder.
    InitReplaceAspecAccepted { agent: crate::cli::Agent },
    /// Init: user declined replacing the existing aspec folder.
    InitReplaceAspecDeclined { agent: crate::cli::Agent },
    /// Init: all work-items Q&A is complete; launch the init flow.
    InitWorkItemsDone {
        agent: crate::cli::Agent,
        aspec: bool,
        replace_aspec: bool,
        run_audit: bool,
        work_items: Option<crate::config::WorkItemsConfig>,
    },
    /// Remote: user selected a session from the picker.
    RemoteSessionChosen { session_id: String },
    /// Remote: user selected a saved directory from the picker.
    RemoteSavedDirChosen { dir: String },
    /// Remote: user accepted saving the used directory.
    RemoteSaveDirAccepted,
    /// Remote: user declined saving the used directory.
    RemoteSaveDirDeclined,
    /// Remote: user selected a session to kill from the kill picker.
    RemoteSessionKillChosen { session_id: String },
    /// New-tab dialog: user selected a remote session to bind a new tab to.
    NewTabRemoteSessionChosen {
        remote_addr: String,
        session_id: String,
        api_key: Option<String>,
    },
    /// New-tab dialog: user wants to create a new remote session.
    NewTabCreateRemoteSession,
    /// Create-remote-session dialog: user confirmed creation.
    NewRemoteSessionCreated {
        remote_addr: String,
        dir: String,
        api_key: Option<String>,
    },
}

/// Dispatch a key press to the correct handler based on application state.
pub fn handle_key(app: &mut App, key: KeyEvent) -> Action {
    // Any key press on the active tab counts as interaction — clear stuck warning and
    // record user activity to suppress the stuck indicator while the user is engaged.
    // (Tab-switch keys also call acknowledge_stuck on the newly active tab in mod.rs.)
    app.active_tab_mut().acknowledge_stuck();
    app.active_tab_mut().record_user_activity();

    // Ctrl-, closes ConfigShow when it is the active dialog (toggle behavior).
    // This must run before the dialog dispatch below; otherwise handle_config_show
    // would consume the key and the dialog would never close via Ctrl-,.
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && key.code == KeyCode::Char(',')
        && matches!(app.active_tab().dialog, Dialog::ConfigShow(_))
    {
        app.active_tab_mut().dialog = Dialog::None;
        return Action::None;
    }

    // Modal dialogs intercept all input.
    let dialog = app.active_tab().dialog.clone();
    match dialog {
        Dialog::QuitConfirm => return handle_quit_confirm(app.active_tab_mut(), key),
        Dialog::CloseTabConfirm => return handle_close_tab_confirm(app.active_tab_mut(), key),
        Dialog::MountScope { git_root, cwd } => {
            return handle_mount_scope(app.active_tab_mut(), key, git_root, cwd)
        }
        Dialog::AgentAuth { .. } => return handle_agent_auth(app.active_tab_mut(), key),
        Dialog::NewKindSelect { interview } => {
            return handle_new_kind_select(app.active_tab_mut(), key, interview)
        }
        Dialog::NewTitleInput { kind, title, interview } => {
            return handle_new_title_input(app.active_tab_mut(), key, kind, title, interview)
        }
        Dialog::NewInterviewSummary { kind, title, work_item_number, summary, cursor_pos } => {
            return handle_new_interview_summary(
                app.active_tab_mut(),
                key,
                kind,
                title,
                work_item_number,
                summary,
                cursor_pos,
            )
        }
        Dialog::NewTabDirectory { input, remote_sessions, remote_selected_idx, focus_workdir } => {
            return handle_new_tab_directory(app.active_tab_mut(), key, input, remote_sessions, remote_selected_idx, focus_workdir)
        }
        Dialog::NewRemoteSession { remote_addr, api_key, dir_input, saved_dirs, saved_selected_idx, focus_input, .. } => {
            return handle_new_remote_session(app.active_tab_mut(), key, remote_addr, api_key, dir_input, saved_dirs, saved_selected_idx, focus_input)
        }
        Dialog::ClawsAuditConfirm => return handle_claws_audit_confirm(app.active_tab_mut(), key),
        Dialog::ClawsReadyHasForked => return handle_claws_has_forked(app.active_tab_mut(), key),
        Dialog::ClawsReadyUsernameInput { username } => {
            return handle_claws_username_input(app.active_tab_mut(), key, username)
        }
        Dialog::ClawsReadyDockerSocketWarning => {
            return handle_claws_docker_socket_warning(app.active_tab_mut(), key)
        }
        Dialog::ClawsReadyOfferRestartStopped { container_id, .. } => {
            return handle_claws_offer_restart_stopped(app.active_tab_mut(), key, container_id)
        }
        Dialog::ClawsReadyOfferStart => return handle_claws_offer_start(app.active_tab_mut(), key),
        Dialog::ClawsRestartFailedOfferFresh { container_id } => {
            return handle_claws_restart_failed_offer_fresh(app.active_tab_mut(), key, container_id)
        }
        Dialog::ClawsReadySudoConfirm { password } => {
            return handle_claws_sudo_confirm(app.active_tab_mut(), key, password)
        }
        Dialog::AgentSetupConfirm { agent, default_agent, from_workflow: _, image_only: _ } => {
            return handle_agent_setup_confirm(app.active_tab_mut(), key, agent, default_agent)
        }
        Dialog::WorkflowStepConfirm { completed_step, next_steps } => {
            return handle_workflow_step_confirm(app.active_tab_mut(), key, completed_step, next_steps)
        }
        Dialog::WorkflowStepError { failed_step, error } => {
            return handle_workflow_step_error(app.active_tab_mut(), key, failed_step, error)
        }
        Dialog::WorkflowControlBoard { .. } => {
            return handle_workflow_control_board(app.active_tab_mut(), key)
        }
        Dialog::WorkflowYoloCountdown { .. } => {
            return handle_workflow_yolo_countdown(app.active_tab_mut(), key)
        }
        Dialog::WorkflowCancelConfirm => {
            return handle_workflow_cancel_confirm(app.active_tab_mut(), key)
        }
        Dialog::WorktreeMergePrompt { .. } => {
            return handle_worktree_merge_prompt(app.active_tab_mut(), key)
        }
        Dialog::WorktreeCommitPrompt { branch, worktree_path, git_root, uncommitted_files, message, cursor_pos } => {
            return handle_worktree_commit_prompt(
                app.active_tab_mut(), key, branch, worktree_path, git_root,
                uncommitted_files, message, cursor_pos,
            )
        }
        Dialog::WorktreeMergeConfirm { branch, worktree_path, git_root } => {
            return handle_worktree_merge_confirm(app.active_tab_mut(), key, branch, worktree_path, git_root)
        }
        Dialog::WorktreeDeleteConfirm { branch, worktree_path, git_root } => {
            return handle_worktree_delete_confirm(app.active_tab_mut(), key, branch, worktree_path, git_root)
        }
        Dialog::WorktreePreCommitWarning { uncommitted_files } => {
            return handle_worktree_pre_commit_warning(app.active_tab_mut(), key, uncommitted_files)
        }
        Dialog::WorktreePreCommitMessage { uncommitted_files, message, cursor_pos } => {
            return handle_worktree_pre_commit_message(
                app.active_tab_mut(), key, uncommitted_files, message, cursor_pos,
            )
        }
        Dialog::ConfigShow(state) => {
            return handle_config_show(app.active_tab_mut(), key, state)
        }
        Dialog::ReadyLegacyMigration { agent_name } => {
            return handle_ready_legacy_migration(app.active_tab_mut(), key, agent_name)
        }
        Dialog::ReadyTemplateAuditConfirm => {
            return handle_ready_template_audit_confirm(app.active_tab_mut(), key)
        }
        Dialog::InitAuditConfirm { agent, aspec, replace_aspec } => {
            return handle_init_audit_confirm(app.active_tab_mut(), key, agent, aspec, replace_aspec)
        }
        Dialog::InitReplaceAspec { agent } => {
            return handle_init_replace_aspec(app.active_tab_mut(), key, agent)
        }
        Dialog::InitWorkItemsConfirm { agent, aspec, replace_aspec, run_audit } => {
            return handle_init_work_items_confirm(app.active_tab_mut(), key, agent, aspec, replace_aspec, run_audit)
        }
        Dialog::InitWorkItemsDirInput { agent, aspec, replace_aspec, run_audit, input } => {
            return handle_init_work_items_dir_input(app.active_tab_mut(), key, agent, aspec, replace_aspec, run_audit, input)
        }
        Dialog::InitWorkItemsTemplateInput { agent, aspec, replace_aspec, run_audit, dir, input } => {
            return handle_init_work_items_template_input(app.active_tab_mut(), key, agent, aspec, replace_aspec, run_audit, dir, input)
        }
        Dialog::RemoteSessionPicker { sessions, selected, remote_addr, command, follow } => {
            return handle_remote_session_picker(app.active_tab_mut(), key, sessions, selected, remote_addr, command, follow)
        }
        Dialog::RemoteSavedDirPicker { dirs, selected, remote_addr } => {
            return handle_remote_saved_dir_picker(app.active_tab_mut(), key, dirs, selected, remote_addr)
        }
        Dialog::RemoteSaveDirConfirm { dir, remote_addr } => {
            return handle_remote_save_dir_confirm(app.active_tab_mut(), key, dir, remote_addr)
        }
        Dialog::RemoteSessionKillPicker { sessions, selected, remote_addr } => {
            return handle_remote_session_kill_picker(app.active_tab_mut(), key, sessions, selected, remote_addr)
        }
        Dialog::None => {}
    }

    // Tab management keys (only when no dialog active).
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('t') => return Action::CreateTab,
            KeyCode::Char('a') => return Action::SwitchTabLeft,
            KeyCode::Char('d') => return Action::SwitchTabRight,
            KeyCode::Char('w') => {
                let tab = app.active_tab();
                // Guard: only open when workflow is running, no other dialog.
                if tab.dialog == Dialog::None
                    && tab.workflow.is_some()
                    && tab.workflow_current_step.is_some()
                    && matches!(tab.phase, ExecutionPhase::Running { .. })
                {
                    let step = tab.workflow_current_step.clone().unwrap();
                    app.active_tab_mut().dialog = Dialog::WorkflowControlBoard {
                        current_step: step,
                        error: None,
                    };
                }
                return Action::None;
            }
            KeyCode::Char('m') => {
                let tab = app.active_tab_mut();
                match tab.container_window {
                    ContainerWindowState::Maximized => {
                        tab.container_window = ContainerWindowState::Minimized;
                        tab.clear_terminal_selection();
                    }
                    ContainerWindowState::Minimized => {
                        tab.container_window = ContainerWindowState::Maximized;
                        tab.focus = Focus::ExecutionWindow;
                    }
                    ContainerWindowState::Hidden => {}
                }
                return Action::None;
            }
            KeyCode::Char(',') => {
                // Toggle the config dialog: open if closed, close if already open.
                let tab = app.active_tab();
                if matches!(tab.dialog, Dialog::ConfigShow(_)) {
                    app.active_tab_mut().dialog = Dialog::None;
                } else if app.active_tab().dialog == Dialog::None {
                    let cwd = app.active_tab().cwd.clone();
                    let git_root = crate::commands::init_flow::find_git_root_from(&cwd);
                    let global_config = crate::config::load_global_config().unwrap_or_default();
                    let repo_config = git_root
                        .as_deref()
                        .and_then(|r| {
                            let _ = crate::config::migrate_legacy_repo_config(r);
                            crate::config::load_repo_config(r).ok()
                        })
                        .unwrap_or_default();
                    use crate::commands::config::{ALL_FIELDS, FieldScope};
                    let initial_col = match ALL_FIELDS[0].scope {
                        FieldScope::RepoOnly => 1,
                        _ => 0,
                    };
                    app.active_tab_mut().dialog = Dialog::ConfigShow(ConfigDialogState {
                        selected_row: 0,
                        selected_col: initial_col,
                        edit_mode: false,
                        edit_value: String::new(),
                        edit_cursor: 0,
                        git_root,
                        global_config,
                        repo_config,
                        error_msg: None,
                    });
                }
                return Action::None;
            }
            _ => {}
        }
    }

    let num_tabs = app.tabs.len();
    let tab = app.active_tab_mut();
    match tab.focus {
        Focus::ExecutionWindow => handle_window_key(tab, key),
        Focus::CommandBox => handle_input_key(tab, key, num_tabs),
    }
}

// --- Execution window key handling ---

fn handle_window_key(tab: &mut TabState, key: KeyEvent) -> Action {
    match &tab.phase {
        ExecutionPhase::Running { .. } => {
            // Container window maximized: forward all keys to PTY for full interactivity.
            // Use Ctrl-M to toggle the window (see handle_key global block).
            if tab.container_window == ContainerWindowState::Maximized {
                // Ctrl+Y: copy terminal selection to clipboard (Ctrl+C is reserved for PTY interrupt).
                if key.code == KeyCode::Char('y') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    if tab.terminal_selection_start.is_some() {
                        return Action::CopyToClipboard;
                    }
                    // No selection — fall through and forward to PTY.
                }
                // All other keys forwarded to the PTY for full interactivity.
                if let Some(bytes) = key_to_bytes(&key) {
                    return Action::ForwardToPty(bytes);
                }
                return Action::None;
            }

            // Container window minimized: outer window is in focus for scrolling.
            if tab.container_window == ContainerWindowState::Minimized {
                // Ctrl-C while a workflow step is running: ask to cancel.
                if key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && tab.workflow.is_some()
                    && tab.workflow_current_step.is_some()
                {
                    tab.dialog = Dialog::WorkflowCancelConfirm;
                    return Action::None;
                }
                match key.code {
                    KeyCode::Up => {
                        let max = tab.output_lines.len();
                        if tab.scroll_offset < max {
                            tab.scroll_offset = tab.scroll_offset.saturating_add(1);
                        }
                    }
                    KeyCode::Down => {
                        tab.scroll_offset = tab.scroll_offset.saturating_sub(1);
                    }
                    KeyCode::Char('b') => {
                        tab.scroll_offset = tab.output_lines.len();
                    }
                    KeyCode::Char('e') => {
                        tab.scroll_offset = 0;
                    }
                    KeyCode::Esc => {
                        tab.focus = Focus::CommandBox;
                    }
                    _ => {}
                }
                return Action::None;
            }

            // No container window: original behavior.
            if key.code == KeyCode::Esc {
                tab.focus = Focus::CommandBox;
                return Action::None;
            }
            // Ctrl-C cancels a running `status --watch` loop.
            // Only intercept if status_watch_cancel_tx is set; otherwise fall through
            // so the byte is forwarded to any active PTY (e.g. an agent container).
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                if tab.status_watch_cancel_tx.take().is_some() {
                    return Action::None;
                }
            }
            // Forward all other keys to the PTY.
            if let Some(bytes) = key_to_bytes(&key) {
                return Action::ForwardToPty(bytes);
            }
        }
        ExecutionPhase::Done { .. } | ExecutionPhase::Error { .. } => {
            match key.code {
                KeyCode::Up => {
                    // Cap at total lines so we don't scroll past the beginning.
                    let max = tab.output_lines.len();
                    if tab.scroll_offset < max {
                        tab.scroll_offset = tab.scroll_offset.saturating_add(1);
                    }
                }
                KeyCode::Down => {
                    tab.scroll_offset = tab.scroll_offset.saturating_sub(1);
                }
                KeyCode::Char('b') => {
                    // Jump to the beginning (oldest output).
                    tab.scroll_offset = tab.output_lines.len();
                }
                KeyCode::Char('e') => {
                    // Jump to the end (newest output).
                    tab.scroll_offset = 0;
                }
                KeyCode::Esc => {
                    tab.focus = Focus::CommandBox;
                }
                _ => {
                    // Any other key refocuses the command box.
                    tab.focus = Focus::CommandBox;
                }
            }
        }
        ExecutionPhase::Idle => {
            tab.focus = Focus::CommandBox;
        }
    }
    Action::None
}

// --- Command input box key handling ---

fn handle_input_key(tab: &mut TabState, key: KeyEvent, num_tabs: usize) -> Action {
    // Ctrl+C → close tab (if multiple tabs open) or quit confirm (single tab).
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if num_tabs > 1 {
            tab.dialog = Dialog::CloseTabConfirm;
        } else {
            tab.dialog = Dialog::QuitConfirm;
        }
        return Action::None;
    }

    // Up arrow navigates to the execution window regardless of phase.
    if key.code == KeyCode::Up {
        if !tab.output_lines.is_empty() {
            tab.focus = Focus::ExecutionWindow;
        }
        return Action::None;
    }

    // When a command is running, the command box is view-only (block editing input).
    if matches!(tab.phase, ExecutionPhase::Running { .. }) {
        return Action::None;
    }

    if key.code == KeyCode::Char('q') && tab.input.is_empty() {
        tab.dialog = Dialog::QuitConfirm;
        return Action::None;
    }

    // Shift+Enter → insert newline.
    if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT) {
        tab.input.insert(tab.cursor_col, '\n');
        tab.cursor_col += 1;
        tab.suggestions = autocomplete_suggestions(&tab.input);
        return Action::None;
    }

    // Enter → submit command.
    if key.code == KeyCode::Enter {
        let cmd = tab.input.trim().to_string();
        tab.input.clear();
        tab.cursor_col = 0;
        tab.suggestions.clear();
        tab.input_error = None;
        return Action::Submit(cmd);
    }

    // Arrow keys: move cursor.
    match key.code {
        KeyCode::Left => {
            tab.cursor_col = tab.cursor_col.saturating_sub(1);
            return Action::None;
        }
        KeyCode::Right => {
            if tab.cursor_col < tab.input.len() {
                tab.cursor_col += 1;
            }
            return Action::None;
        }
        _ => {}
    }

    // Backspace.
    if key.code == KeyCode::Backspace && tab.cursor_col > 0 {
        tab.cursor_col -= 1;
        tab.input.remove(tab.cursor_col);
        tab.suggestions = autocomplete_suggestions(&tab.input);
        tab.input_error = None;
        return Action::None;
    }

    // Delete.
    if key.code == KeyCode::Delete && tab.cursor_col < tab.input.len() {
        tab.input.remove(tab.cursor_col);
        tab.suggestions = autocomplete_suggestions(&tab.input);
        return Action::None;
    }

    // Regular character.
    if let KeyCode::Char(c) = key.code {
        tab.input.insert(tab.cursor_col, c);
        tab.cursor_col += 1;
        tab.suggestions = autocomplete_suggestions(&tab.input);
        tab.input_error = None;
    }

    Action::None
}

// --- Dialog handlers ---

fn handle_quit_confirm(tab: &mut TabState, key: KeyEvent) -> Action {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        tab.dialog = Dialog::None;
        return Action::QuitConfirmed;
    }
    if key.code == KeyCode::Esc {
        tab.dialog = Dialog::None;
    }
    Action::None
}

fn handle_close_tab_confirm(tab: &mut TabState, key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') => {
                tab.dialog = Dialog::None;
                return Action::QuitConfirmed;
            }
            KeyCode::Char('t') => {
                tab.dialog = Dialog::None;
                return Action::CloseCurrentTab;
            }
            _ => {}
        }
    }
    if key.code == KeyCode::Esc {
        tab.dialog = Dialog::None;
    }
    Action::None
}

fn handle_new_tab_directory(
    tab: &mut TabState,
    key: KeyEvent,
    mut input: String,
    remote_sessions: Option<Result<Vec<crate::commands::remote::RemoteSessionEntry>, String>>,
    remote_selected_idx: Option<usize>,
    focus_workdir: bool,
) -> Action {
    if focus_workdir {
        // Focus is on the workdir text input.
        match key.code {
            KeyCode::Enter => {
                tab.dialog = Dialog::None;
                let path = if input.trim().is_empty() {
                    tab.cwd.clone()
                } else {
                    PathBuf::from(input.trim())
                };
                Action::NewTabDirectoryChosen(path)
            }
            KeyCode::Esc => {
                tab.dialog = Dialog::None;
                tab.remote_sessions_fetch_rx = None;
                Action::None
            }
            KeyCode::Down => {
                // Move focus to the remote sessions list if available.
                let has_entries = match &remote_sessions {
                    Some(Ok(sessions)) => !sessions.is_empty() || true, // always have "+ Create new"
                    _ => false,
                };
                if has_entries {
                    tab.dialog = Dialog::NewTabDirectory {
                        input,
                        remote_sessions,
                        remote_selected_idx: Some(0),
                        focus_workdir: false,
                    };
                }
                Action::None
            }
            KeyCode::Backspace => {
                input.pop();
                tab.dialog = Dialog::NewTabDirectory { input, remote_sessions, remote_selected_idx, focus_workdir: true };
                Action::None
            }
            KeyCode::Char(c) => {
                input.push(c);
                tab.dialog = Dialog::NewTabDirectory { input, remote_sessions, remote_selected_idx, focus_workdir: true };
                Action::None
            }
            _ => Action::None,
        }
    } else {
        // Focus is on the remote sessions list.
        let sessions = match &remote_sessions {
            Some(Ok(s)) => s.clone(),
            _ => vec![],
        };
        // Total entries = sessions + 1 for "+ Create new remote session"
        let total_entries = sessions.len() + 1;
        let idx = remote_selected_idx.unwrap_or(0);

        match key.code {
            KeyCode::Up => {
                if idx == 0 {
                    // Move back to workdir input.
                    tab.dialog = Dialog::NewTabDirectory {
                        input,
                        remote_sessions,
                        remote_selected_idx: Some(0),
                        focus_workdir: true,
                    };
                } else {
                    tab.dialog = Dialog::NewTabDirectory {
                        input,
                        remote_sessions,
                        remote_selected_idx: Some(idx - 1),
                        focus_workdir: false,
                    };
                }
                Action::None
            }
            KeyCode::Down => {
                let new_idx = (idx + 1).min(total_entries.saturating_sub(1));
                tab.dialog = Dialog::NewTabDirectory {
                    input,
                    remote_sessions,
                    remote_selected_idx: Some(new_idx),
                    focus_workdir: false,
                };
                Action::None
            }
            KeyCode::Enter => {
                tab.dialog = Dialog::None;
                tab.remote_sessions_fetch_rx = None;
                if idx < sessions.len() {
                    // User selected an existing session.
                    let session = &sessions[idx];
                    let remote_addr = crate::config::effective_remote_default_addr().unwrap_or_default();
                    let api_key = crate::commands::remote::resolve_api_key(None, &remote_addr);
                    Action::NewTabRemoteSessionChosen {
                        remote_addr,
                        session_id: session.id.clone(),
                        api_key,
                    }
                } else {
                    // "+ Create new remote session"
                    Action::NewTabCreateRemoteSession
                }
            }
            KeyCode::Esc => {
                tab.dialog = Dialog::None;
                tab.remote_sessions_fetch_rx = None;
                Action::None
            }
            _ => Action::None,
        }
    }
}

fn handle_new_remote_session(
    tab: &mut TabState,
    key: KeyEvent,
    remote_addr: String,
    api_key: Option<String>,
    mut dir_input: String,
    saved_dirs: Vec<String>,
    saved_selected_idx: Option<usize>,
    focus_input: bool,
) -> Action {
    if focus_input {
        match key.code {
            KeyCode::Enter => {
                if dir_input.trim().is_empty() {
                    return Action::None;
                }
                tab.dialog = Dialog::None;
                Action::NewRemoteSessionCreated {
                    remote_addr,
                    dir: dir_input.trim().to_string(),
                    api_key,
                }
            }
            KeyCode::Esc => {
                // Return to new-tab dialog.
                tab.dialog = Dialog::None;
                Action::CreateTab
            }
            KeyCode::Down if !saved_dirs.is_empty() => {
                tab.dialog = Dialog::NewRemoteSession {
                    remote_addr,
                    api_key,
                    dir_input,
                    saved_dirs,
                    saved_selected_idx: Some(0),
                    focus_input: false,
                    creation_error: None,
                };
                Action::None
            }
            KeyCode::Backspace => {
                dir_input.pop();
                tab.dialog = Dialog::NewRemoteSession {
                    remote_addr, api_key, dir_input, saved_dirs, saved_selected_idx, focus_input: true,
                    creation_error: None,
                };
                Action::None
            }
            KeyCode::Char(c) => {
                dir_input.push(c);
                tab.dialog = Dialog::NewRemoteSession {
                    remote_addr, api_key, dir_input, saved_dirs, saved_selected_idx, focus_input: true,
                    creation_error: None,
                };
                Action::None
            }
            _ => Action::None,
        }
    } else {
        let idx = saved_selected_idx.unwrap_or(0);
        match key.code {
            KeyCode::Up => {
                if idx == 0 {
                    tab.dialog = Dialog::NewRemoteSession {
                        remote_addr, api_key, dir_input, saved_dirs, saved_selected_idx: Some(0), focus_input: true,
                        creation_error: None,
                    };
                } else {
                    tab.dialog = Dialog::NewRemoteSession {
                        remote_addr, api_key, dir_input, saved_dirs, saved_selected_idx: Some(idx - 1), focus_input: false,
                        creation_error: None,
                    };
                }
                Action::None
            }
            KeyCode::Down => {
                let new_idx = (idx + 1).min(saved_dirs.len().saturating_sub(1));
                tab.dialog = Dialog::NewRemoteSession {
                    remote_addr, api_key, dir_input, saved_dirs, saved_selected_idx: Some(new_idx), focus_input: false,
                    creation_error: None,
                };
                Action::None
            }
            KeyCode::Enter => {
                // Selecting a saved dir populates the text field and confirms.
                if idx < saved_dirs.len() {
                    let dir = saved_dirs[idx].clone();
                    tab.dialog = Dialog::None;
                    Action::NewRemoteSessionCreated {
                        remote_addr,
                        dir,
                        api_key,
                    }
                } else {
                    Action::None
                }
            }
            KeyCode::Esc => {
                // Return to new-tab dialog.
                tab.dialog = Dialog::None;
                Action::CreateTab
            }
            _ => Action::None,
        }
    }
}

fn handle_mount_scope(
    tab: &mut TabState,
    key: KeyEvent,
    git_root: PathBuf,
    cwd: PathBuf,
) -> Action {
    match key.code {
        KeyCode::Char('r') | KeyCode::Char('R') => {
            tab.dialog = Dialog::None;
            return Action::MountScopeChosen(git_root);
        }
        KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Enter => {
            tab.dialog = Dialog::None;
            return Action::MountScopeChosen(cwd);
        }
        KeyCode::Esc => {
            tab.dialog = Dialog::None;
            tab.input_error = Some("Command cancelled.".into());
        }
        _ => {}
    }
    Action::None
}

fn handle_agent_auth(tab: &mut TabState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            tab.dialog = Dialog::None;
            return Action::AuthAccepted;
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            return Action::AuthDeclined;
        }
        _ => {}
    }
    Action::None
}

fn handle_new_kind_select(tab: &mut TabState, key: KeyEvent, interview: bool) -> Action {
    match key.code {
        KeyCode::Char('1') | KeyCode::Char('f') | KeyCode::Char('F') => {
            tab.dialog = Dialog::NewTitleInput {
                kind: WorkItemKind::Feature,
                title: String::new(),
                interview,
            };
        }
        KeyCode::Char('2') | KeyCode::Char('b') | KeyCode::Char('B') => {
            tab.dialog = Dialog::NewTitleInput {
                kind: WorkItemKind::Bug,
                title: String::new(),
                interview,
            };
        }
        KeyCode::Char('3') | KeyCode::Char('t') | KeyCode::Char('T') => {
            tab.dialog = Dialog::NewTitleInput {
                kind: WorkItemKind::Task,
                title: String::new(),
                interview,
            };
        }
        KeyCode::Char('4') | KeyCode::Char('e') | KeyCode::Char('E') => {
            tab.dialog = Dialog::NewTitleInput {
                kind: WorkItemKind::Enhancement,
                title: String::new(),
                interview,
            };
        }
        KeyCode::Esc => {
            tab.dialog = Dialog::None;
            tab.input_error = Some("Command cancelled.".into());
        }
        _ => {}
    }
    Action::None
}

fn handle_new_title_input(
    tab: &mut TabState,
    key: KeyEvent,
    kind: WorkItemKind,
    mut title: String,
    interview: bool,
) -> Action {
    match key.code {
        KeyCode::Enter => {
            let trimmed = title.trim().to_string();
            if trimmed.is_empty() {
                return Action::None;
            }
            tab.dialog = Dialog::None;
            return Action::NewWorkItem {
                kind,
                title: trimmed,
                interview,
            };
        }
        KeyCode::Esc => {
            tab.dialog = Dialog::None;
            tab.input_error = Some("Command cancelled.".into());
        }
        KeyCode::Backspace => {
            title.pop();
            tab.dialog = Dialog::NewTitleInput { kind, title, interview };
        }
        KeyCode::Char(c) => {
            title.push(c);
            tab.dialog = Dialog::NewTitleInput { kind, title, interview };
        }
        _ => {}
    }
    Action::None
}

fn handle_new_interview_summary(
    tab: &mut TabState,
    key: KeyEvent,
    kind: WorkItemKind,
    title: String,
    work_item_number: u32,
    mut summary: String,
    mut cursor_pos: usize,
) -> Action {
    // Ctrl+Enter or Ctrl+S → submit.
    let is_submit = (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL))
        || (key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL));
    if is_submit {
        let trimmed = summary.trim().to_string();
        if !trimmed.is_empty() {
            tab.dialog = Dialog::None;
            return Action::NewInterviewSummarySubmitted {
                kind,
                title,
                work_item_number,
                summary: trimmed,
            };
        }
        return Action::None;
    }

    match key.code {
        KeyCode::Enter => {
            // Insert newline at cursor position.
            summary.insert(cursor_pos, '\n');
            cursor_pos += 1;
            tab.dialog = Dialog::NewInterviewSummary {
                kind,
                title,
                work_item_number,
                summary,
                cursor_pos,
            };
        }
        KeyCode::Esc => {
            tab.dialog = Dialog::None;
            tab.input_error = Some("Command cancelled.".into());
        }
        KeyCode::Backspace => {
            if cursor_pos > 0 {
                // Find the char boundary before cursor_pos.
                let mut char_start = cursor_pos - 1;
                while char_start > 0 && !summary.is_char_boundary(char_start) {
                    char_start -= 1;
                }
                summary.remove(char_start);
                cursor_pos = char_start;
            }
            tab.dialog = Dialog::NewInterviewSummary {
                kind,
                title,
                work_item_number,
                summary,
                cursor_pos,
            };
        }
        KeyCode::Delete => {
            if cursor_pos < summary.len() {
                // Find the next char boundary.
                let mut char_end = cursor_pos + 1;
                while char_end < summary.len() && !summary.is_char_boundary(char_end) {
                    char_end += 1;
                }
                summary.remove(cursor_pos);
            }
            tab.dialog = Dialog::NewInterviewSummary {
                kind,
                title,
                work_item_number,
                summary,
                cursor_pos,
            };
        }
        KeyCode::Left => {
            if cursor_pos > 0 {
                cursor_pos -= 1;
                while cursor_pos > 0 && !summary.is_char_boundary(cursor_pos) {
                    cursor_pos -= 1;
                }
            }
            tab.dialog = Dialog::NewInterviewSummary {
                kind,
                title,
                work_item_number,
                summary,
                cursor_pos,
            };
        }
        KeyCode::Right => {
            if cursor_pos < summary.len() {
                cursor_pos += 1;
                while cursor_pos < summary.len() && !summary.is_char_boundary(cursor_pos) {
                    cursor_pos += 1;
                }
            }
            tab.dialog = Dialog::NewInterviewSummary {
                kind,
                title,
                work_item_number,
                summary,
                cursor_pos,
            };
        }
        KeyCode::Up => {
            // Navigate to the same column in the previous line.
            let before = &summary[..cursor_pos];
            if let Some(prev_newline) = before.rfind('\n') {
                let col = cursor_pos - prev_newline - 1;
                let line_start = before[..prev_newline].rfind('\n').map(|i| i + 1).unwrap_or(0);
                let line_len = prev_newline - line_start;
                cursor_pos = line_start + col.min(line_len);
            } else {
                cursor_pos = 0;
            }
            tab.dialog = Dialog::NewInterviewSummary {
                kind,
                title,
                work_item_number,
                summary,
                cursor_pos,
            };
        }
        KeyCode::Down => {
            // Navigate to the same column in the next line.
            let before = &summary[..cursor_pos];
            let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
            let col = cursor_pos - line_start;
            if let Some(next_newline) = summary[cursor_pos..].find('\n') {
                let next_line_start = cursor_pos + next_newline + 1;
                let next_line_end = summary[next_line_start..]
                    .find('\n')
                    .map(|i| next_line_start + i)
                    .unwrap_or(summary.len());
                let next_line_len = next_line_end - next_line_start;
                cursor_pos = next_line_start + col.min(next_line_len);
            } else {
                cursor_pos = summary.len();
            }
            tab.dialog = Dialog::NewInterviewSummary {
                kind,
                title,
                work_item_number,
                summary,
                cursor_pos,
            };
        }
        KeyCode::Home => {
            let before = &summary[..cursor_pos];
            let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
            cursor_pos = line_start;
            tab.dialog = Dialog::NewInterviewSummary {
                kind,
                title,
                work_item_number,
                summary,
                cursor_pos,
            };
        }
        KeyCode::End => {
            let after = &summary[cursor_pos..];
            let line_end = after.find('\n').map(|i| cursor_pos + i).unwrap_or(summary.len());
            cursor_pos = line_end;
            tab.dialog = Dialog::NewInterviewSummary {
                kind,
                title,
                work_item_number,
                summary,
                cursor_pos,
            };
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            summary.insert(cursor_pos, c);
            cursor_pos += c.len_utf8();
            tab.dialog = Dialog::NewInterviewSummary {
                kind,
                title,
                work_item_number,
                summary,
                cursor_pos,
            };
        }
        _ => {}
    }
    Action::None
}

// --- Claws dialog handlers ---

fn handle_claws_has_forked(tab: &mut TabState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Char('1') => {
            tab.claws_wizard_already_forked = true;
            tab.dialog = Dialog::ClawsReadyUsernameInput { username: String::new() };
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('2') | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            tab.input_error = Some(
                "Please fork nanoclaw at github.com/qwibitai/nanoclaw, \
                 then run 'claws init' again."
                    .into(),
            );
        }
        _ => {}
    }
    Action::None
}

fn handle_claws_username_input(tab: &mut TabState, key: KeyEvent, mut username: String) -> Action {
    match key.code {
        KeyCode::Enter => {
            let trimmed = username.trim().to_string();
            if trimmed.is_empty() {
                return Action::None;
            }
            tab.claws_wizard_username = Some(trimmed);
            tab.dialog = Dialog::None;
            return Action::ClawsReadyProceed;
        }
        KeyCode::Esc => {
            tab.dialog = Dialog::None;
            tab.input_error = Some("Command cancelled.".into());
        }
        KeyCode::Backspace => {
            username.pop();
            tab.dialog = Dialog::ClawsReadyUsernameInput { username };
        }
        KeyCode::Char(c) => {
            username.push(c);
            tab.dialog = Dialog::ClawsReadyUsernameInput { username };
        }
        _ => {}
    }
    Action::None
}

fn handle_claws_docker_socket_warning(tab: &mut TabState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Char('1') => {
            tab.dialog = Dialog::None;
            if let Some(tx) = tab.claws_docker_accept_response_tx.take() {
                let _ = tx.send(true);
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('2') | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            if let Some(tx) = tab.claws_docker_accept_response_tx.take() {
                let _ = tx.send(false);
            }
        }
        _ => {}
    }
    Action::None
}

fn handle_claws_audit_confirm(tab: &mut TabState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Char('1') => {
            tab.dialog = Dialog::None;
            return Action::ClawsAuditConfirmAccept;
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('2') | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            return Action::ClawsAuditConfirmDecline;
        }
        _ => {}
    }
    Action::None
}

fn handle_claws_offer_start(tab: &mut TabState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Char('1') => {
            tab.dialog = Dialog::None;
            return Action::ClawsReadyStartContainer;
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('2') | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            tab.claws_attach_after_start = false;
            tab.input_error = Some("Container not started.".into());
        }
        _ => {}
    }
    Action::None
}

fn handle_claws_offer_restart_stopped(
    tab: &mut TabState,
    key: KeyEvent,
    container_id: String,
) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Char('1') => {
            tab.dialog = Dialog::None;
            return Action::ClawsReadyRestartStopped { container_id };
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('2') | KeyCode::Esc => {
            // User declined to restart stopped container — offer fresh start instead.
            tab.dialog = Dialog::ClawsReadyOfferStart;
        }
        _ => {}
    }
    Action::None
}

fn handle_claws_restart_failed_offer_fresh(
    tab: &mut TabState,
    key: KeyEvent,
    container_id: String,
) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Char('1') => {
            tab.dialog = Dialog::None;
            return Action::ClawsReadyDeleteAndStartFresh { container_id };
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('2') | KeyCode::Esc => {
            tab.dialog = Dialog::None;
        }
        _ => {}
    }
    Action::None
}

fn handle_claws_sudo_confirm(tab: &mut TabState, key: KeyEvent, mut password: String) -> Action {
    match key.code {
        KeyCode::Enter => {
            tab.dialog = Dialog::None;
            if let Some(tx) = tab.claws_sudo_response_tx.take() {
                let _ = tx.send(Some(password));
            }
        }
        KeyCode::Esc => {
            tab.dialog = Dialog::None;
            if let Some(tx) = tab.claws_sudo_response_tx.take() {
                let _ = tx.send(None);
            }
        }
        KeyCode::Backspace => {
            password.pop();
            tab.dialog = Dialog::ClawsReadySudoConfirm { password };
        }
        KeyCode::Char(c) => {
            password.push(c);
            tab.dialog = Dialog::ClawsReadySudoConfirm { password };
        }
        _ => {}
    }
    Action::None
}

// --- Agent setup dialog handler ---

fn handle_agent_setup_confirm(tab: &mut TabState, key: KeyEvent, agent: String, default_agent: String) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            let image_only = matches!(tab.dialog, Dialog::AgentSetupConfirm { image_only: true, .. });
            tab.dialog = Dialog::None;
            Action::AgentSetupAccepted { agent, image_only }
        }
        KeyCode::Char('f') | KeyCode::Char('F') if agent != default_agent => {
            // Offer fallback to the default agent without attempting download.
            tab.dialog = Dialog::None;
            Action::AgentSetupFallbackAccepted {
                declined_agent: agent,
                default_agent,
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::AgentSetupDeclined { agent }
        }
        _ => Action::None,
    }
}

// --- Workflow dialog handlers ---

fn handle_workflow_step_confirm(
    tab: &mut TabState,
    key: KeyEvent,
    _completed_step: String,
    _next_steps: Vec<String>,
) -> Action {
    match key.code {
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Char('1') => {
            tab.dialog = Dialog::None;
            Action::WorkflowAdvance
        }
        KeyCode::Char('q') | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('2')
        | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::WorkflowAbort
        }
        _ => Action::None,
    }
}

fn handle_workflow_step_error(
    tab: &mut TabState,
    key: KeyEvent,
    _failed_step: String,
    _error: String,
) -> Action {
    match key.code {
        KeyCode::Char('r') | KeyCode::Char('R') | KeyCode::Char('1') => {
            tab.dialog = Dialog::None;
            Action::WorkflowRetry
        }
        KeyCode::Char('q') | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('2')
        | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::WorkflowAbort
        }
        _ => Action::None,
    }
}

fn handle_workflow_control_board(tab: &mut TabState, key: KeyEvent) -> Action {
    let last_step = tab.is_last_workflow_step();
    // "Continue in same container" is only valid when the next step uses the same agent.
    let same_container_blocked = !last_step && tab.next_step_different_agent().is_some();
    match key.code {
        KeyCode::Up => {
            tab.dialog = Dialog::None;
            Action::WorkflowRestartStep
        }
        KeyCode::Left => {
            tab.dialog = Dialog::None;
            Action::WorkflowCancelToPrevious
        }
        KeyCode::Right => {
            if last_step {
                return Action::None; // disabled on last step
            }
            tab.dialog = Dialog::None;
            Action::WorkflowNextInNewContainer
        }
        KeyCode::Down => {
            if last_step || same_container_blocked {
                return Action::None; // disabled on last step or when agents differ
            }
            tab.dialog = Dialog::None;
            Action::WorkflowNextInCurrentContainer
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
            tab.dialog = Dialog::None;
            Action::WorkflowFinish
        }
        KeyCode::Esc => {
            tab.dismiss_stuck_dialog();
            Action::None
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl-C: open the workflow cancel confirmation dialog.
            tab.dialog = Dialog::WorkflowCancelConfirm;
            Action::None
        }
        KeyCode::Char('c') => {
            // Plain 'c': dismiss the dialog (with backoff) and restore the container window.
            tab.dismiss_stuck_dialog();
            if tab.container_window == ContainerWindowState::Minimized {
                tab.container_window = ContainerWindowState::Maximized;
            }
            Action::None
        }
        KeyCode::Char('d') => {
            // Disable auto-popup for the current step (still dismisses dialog with backoff).
            tab.dialog = Dialog::None;
            Action::DisableAutoWorkflowForStep
        }
        _ => Action::None, // dialog stays open
    }
}

fn handle_workflow_yolo_countdown(tab: &mut TabState, key: KeyEvent) -> Action {
    // Ctrl+A / Ctrl+D: switch tabs while keeping the yolo countdown running in the
    // background.  The tab-switching handler in mod.rs closes the dialog on the old
    // tab and opens it on the new tab (if it also has a countdown), preserving time.
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('a') => return Action::SwitchTabLeft,
            KeyCode::Char('d') => return Action::SwitchTabRight,
            _ => {}
        }
    }
    match key.code {
        KeyCode::Esc => {
            // Dismiss with backoff so the countdown won't immediately re-open.
            tab.dismiss_stuck_dialog();
            Action::None
        }
        _ => Action::None, // dialog stays open
    }
}

fn handle_workflow_cancel_confirm(tab: &mut TabState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            tab.dialog = Dialog::None;
            Action::WorkflowCancelExecution
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_worktree_merge_prompt(tab: &mut TabState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Char('m') | KeyCode::Char('M') => {
            tab.dialog = Dialog::None;
            Action::WorktreeMerge
        }
        KeyCode::Char('d') | KeyCode::Char('D') => {
            tab.dialog = Dialog::None;
            Action::WorktreeDiscard
        }
        KeyCode::Char('s') | KeyCode::Char('S') | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::WorktreeSkip
        }
        _ => Action::None,
    }
}

fn handle_worktree_commit_prompt(
    tab: &mut TabState,
    key: KeyEvent,
    branch: String,
    worktree_path: PathBuf,
    git_root: PathBuf,
    uncommitted_files: Vec<String>,
    mut message: String,
    mut cursor_pos: usize,
) -> Action {
    // Ctrl+Enter or Ctrl+S → submit commit
    let is_submit = (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL))
        || (key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL));
    if is_submit {
        let trimmed = message.trim().to_string();
        if !trimmed.is_empty() {
            tab.dialog = Dialog::None;
            return Action::WorktreeCommitFiles { message: trimmed, branch, worktree_path, git_root };
        }
        return Action::None;
    }

    match key.code {
        KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::None
        }
        KeyCode::Backspace => {
            if cursor_pos > 0 {
                let mut char_start = cursor_pos - 1;
                while char_start > 0 && !message.is_char_boundary(char_start) {
                    char_start -= 1;
                }
                message.remove(char_start);
                cursor_pos = char_start;
            }
            tab.dialog = Dialog::WorktreeCommitPrompt { branch, worktree_path, git_root, uncommitted_files, message, cursor_pos };
            Action::None
        }
        KeyCode::Delete => {
            if cursor_pos < message.len() {
                let mut char_end = cursor_pos + 1;
                while char_end < message.len() && !message.is_char_boundary(char_end) {
                    char_end += 1;
                }
                message.remove(cursor_pos);
            }
            tab.dialog = Dialog::WorktreeCommitPrompt { branch, worktree_path, git_root, uncommitted_files, message, cursor_pos };
            Action::None
        }
        KeyCode::Left => {
            if cursor_pos > 0 {
                cursor_pos -= 1;
                while cursor_pos > 0 && !message.is_char_boundary(cursor_pos) {
                    cursor_pos -= 1;
                }
            }
            tab.dialog = Dialog::WorktreeCommitPrompt { branch, worktree_path, git_root, uncommitted_files, message, cursor_pos };
            Action::None
        }
        KeyCode::Right => {
            if cursor_pos < message.len() {
                cursor_pos += 1;
                while cursor_pos < message.len() && !message.is_char_boundary(cursor_pos) {
                    cursor_pos += 1;
                }
            }
            tab.dialog = Dialog::WorktreeCommitPrompt { branch, worktree_path, git_root, uncommitted_files, message, cursor_pos };
            Action::None
        }
        KeyCode::Home => {
            cursor_pos = 0;
            tab.dialog = Dialog::WorktreeCommitPrompt { branch, worktree_path, git_root, uncommitted_files, message, cursor_pos };
            Action::None
        }
        KeyCode::End => {
            cursor_pos = message.len();
            tab.dialog = Dialog::WorktreeCommitPrompt { branch, worktree_path, git_root, uncommitted_files, message, cursor_pos };
            Action::None
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            message.insert(cursor_pos, c);
            cursor_pos += c.len_utf8();
            tab.dialog = Dialog::WorktreeCommitPrompt { branch, worktree_path, git_root, uncommitted_files, message, cursor_pos };
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_worktree_merge_confirm(
    tab: &mut TabState,
    key: KeyEvent,
    branch: String,
    worktree_path: PathBuf,
    git_root: PathBuf,
) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            tab.dialog = Dialog::None;
            Action::WorktreeMergeConfirmed { branch, worktree_path, git_root }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_worktree_delete_confirm(
    tab: &mut TabState,
    key: KeyEvent,
    branch: String,
    worktree_path: PathBuf,
    git_root: PathBuf,
) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            tab.dialog = Dialog::None;
            Action::WorktreeDeleteConfirmed { branch, worktree_path, git_root }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::WorktreeKeepAfterMerge
        }
        _ => Action::None,
    }
}

fn handle_worktree_pre_commit_warning(
    tab: &mut TabState,
    key: KeyEvent,
    uncommitted_files: Vec<String>,
) -> Action {
    match key.code {
        KeyCode::Char('c') | KeyCode::Char('C') => {
            let default_msg = "WIP: pre-worktree commit".to_string();
            let cursor_pos = default_msg.len();
            tab.dialog = Dialog::WorktreePreCommitMessage {
                uncommitted_files,
                message: default_msg,
                cursor_pos,
            };
            Action::None
        }
        KeyCode::Char('u') | KeyCode::Char('U') => {
            tab.dialog = Dialog::None;
            Action::WorktreePreCommitUse
        }
        KeyCode::Char('a') | KeyCode::Char('A') | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::WorktreePreCommitAbort
        }
        _ => Action::None,
    }
}

fn handle_worktree_pre_commit_message(
    tab: &mut TabState,
    key: KeyEvent,
    uncommitted_files: Vec<String>,
    mut message: String,
    mut cursor_pos: usize,
) -> Action {
    let is_submit = (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL))
        || (key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL));
    if is_submit {
        let trimmed = message.trim().to_string();
        if !trimmed.is_empty() {
            tab.dialog = Dialog::None;
            return Action::WorktreePreCommitCommit { message: trimmed };
        }
        return Action::None;
    }

    match key.code {
        KeyCode::Esc => {
            tab.dialog = Dialog::WorktreePreCommitWarning { uncommitted_files };
            Action::None
        }
        KeyCode::Backspace => {
            if cursor_pos > 0 {
                let mut char_start = cursor_pos - 1;
                while char_start > 0 && !message.is_char_boundary(char_start) {
                    char_start -= 1;
                }
                message.remove(char_start);
                cursor_pos = char_start;
            }
            tab.dialog = Dialog::WorktreePreCommitMessage { uncommitted_files, message, cursor_pos };
            Action::None
        }
        KeyCode::Delete => {
            if cursor_pos < message.len() {
                let mut char_end = cursor_pos + 1;
                while char_end < message.len() && !message.is_char_boundary(char_end) {
                    char_end += 1;
                }
                message.remove(cursor_pos);
            }
            tab.dialog = Dialog::WorktreePreCommitMessage { uncommitted_files, message, cursor_pos };
            Action::None
        }
        KeyCode::Left => {
            if cursor_pos > 0 {
                cursor_pos -= 1;
                while cursor_pos > 0 && !message.is_char_boundary(cursor_pos) {
                    cursor_pos -= 1;
                }
            }
            tab.dialog = Dialog::WorktreePreCommitMessage { uncommitted_files, message, cursor_pos };
            Action::None
        }
        KeyCode::Right => {
            if cursor_pos < message.len() {
                cursor_pos += 1;
                while cursor_pos < message.len() && !message.is_char_boundary(cursor_pos) {
                    cursor_pos += 1;
                }
            }
            tab.dialog = Dialog::WorktreePreCommitMessage { uncommitted_files, message, cursor_pos };
            Action::None
        }
        KeyCode::Home => {
            cursor_pos = 0;
            tab.dialog = Dialog::WorktreePreCommitMessage { uncommitted_files, message, cursor_pos };
            Action::None
        }
        KeyCode::End => {
            cursor_pos = message.len();
            tab.dialog = Dialog::WorktreePreCommitMessage { uncommitted_files, message, cursor_pos };
            Action::None
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            message.insert(cursor_pos, c);
            cursor_pos += c.len_utf8();
            tab.dialog = Dialog::WorktreePreCommitMessage { uncommitted_files, message, cursor_pos };
            Action::None
        }
        _ => Action::None,
    }
}

// --- Remote dialog handlers ---

fn handle_remote_session_picker(
    tab: &mut TabState,
    key: KeyEvent,
    sessions: Vec<crate::commands::remote::RemoteSessionEntry>,
    mut selected: usize,
    remote_addr: String,
    command: Vec<String>,
    follow: bool,
) -> Action {
    match key.code {
        KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::None
        }
        KeyCode::Up => {
            if selected > 0 { selected -= 1; }
            tab.dialog = Dialog::RemoteSessionPicker { sessions, selected, remote_addr, command, follow };
            Action::None
        }
        KeyCode::Down => {
            if selected + 1 < sessions.len() { selected += 1; }
            tab.dialog = Dialog::RemoteSessionPicker { sessions, selected, remote_addr, command, follow };
            Action::None
        }
        KeyCode::Enter => {
            if let Some(s) = sessions.get(selected) {
                let session_id = s.id.clone();
                tab.dialog = Dialog::None;
                Action::RemoteSessionChosen { session_id }
            } else {
                tab.dialog = Dialog::None;
                Action::None
            }
        }
        _ => {
            tab.dialog = Dialog::RemoteSessionPicker { sessions, selected, remote_addr, command, follow };
            Action::None
        }
    }
}

fn handle_remote_saved_dir_picker(
    tab: &mut TabState,
    key: KeyEvent,
    dirs: Vec<String>,
    mut selected: usize,
    remote_addr: String,
) -> Action {
    match key.code {
        KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::None
        }
        KeyCode::Up => {
            if selected > 0 { selected -= 1; }
            tab.dialog = Dialog::RemoteSavedDirPicker { dirs, selected, remote_addr };
            Action::None
        }
        KeyCode::Down => {
            if selected + 1 < dirs.len() { selected += 1; }
            tab.dialog = Dialog::RemoteSavedDirPicker { dirs, selected, remote_addr };
            Action::None
        }
        KeyCode::Enter => {
            if let Some(dir) = dirs.get(selected) {
                let dir = dir.clone();
                tab.dialog = Dialog::None;
                Action::RemoteSavedDirChosen { dir }
            } else {
                tab.dialog = Dialog::None;
                Action::None
            }
        }
        _ => {
            tab.dialog = Dialog::RemoteSavedDirPicker { dirs, selected, remote_addr };
            Action::None
        }
    }
}

fn handle_remote_save_dir_confirm(
    tab: &mut TabState,
    key: KeyEvent,
    _dir: String,
    _remote_addr: String,
) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            tab.dialog = Dialog::None;
            Action::RemoteSaveDirAccepted
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Enter => {
            // Decline saving the directory but proceed with the remote session start.
            tab.dialog = Dialog::None;
            Action::RemoteSaveDirDeclined
        }
        KeyCode::Esc => {
            // Cancel entirely: close the dialog and abort the pending session start.
            tab.dialog = Dialog::None;
            tab.pending_command = PendingCommand::None;
            Action::None
        }
        _ => Action::None
    }
}

fn handle_remote_session_kill_picker(
    tab: &mut TabState,
    key: KeyEvent,
    sessions: Vec<crate::commands::remote::RemoteSessionEntry>,
    mut selected: usize,
    remote_addr: String,
) -> Action {
    match key.code {
        KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::None
        }
        KeyCode::Up => {
            if selected > 0 { selected -= 1; }
            tab.dialog = Dialog::RemoteSessionKillPicker { sessions, selected, remote_addr };
            Action::None
        }
        KeyCode::Down => {
            if selected + 1 < sessions.len() { selected += 1; }
            tab.dialog = Dialog::RemoteSessionKillPicker { sessions, selected, remote_addr };
            Action::None
        }
        KeyCode::Enter => {
            if let Some(s) = sessions.get(selected) {
                let session_id = s.id.clone();
                tab.dialog = Dialog::None;
                Action::RemoteSessionKillChosen { session_id }
            } else {
                tab.dialog = Dialog::None;
                Action::None
            }
        }
        _ => {
            tab.dialog = Dialog::RemoteSessionKillPicker { sessions, selected, remote_addr };
            Action::None
        }
    }
}

// --- Autocomplete ---

const SUBCOMMANDS: &[&str] = &["init", "ready", "implement", "chat", "exec", "specs", "claws", "status", "config", "remote"];

/// Return suggestions for the current input string.
pub fn autocomplete_suggestions(input: &str) -> Vec<String> {
    if input.trim().is_empty() {
        return SUBCOMMANDS.iter().map(|s| s.to_string()).collect();
    }

    // Split on the FIRST space to separate command from arguments.
    // Use the raw input (not trimmed) so a trailing space signals "show flags".
    let tokens: Vec<&str> = input.splitn(2, ' ').collect();
    let cmd = tokens[0].trim();

    // If there is content after the first space (even empty), the user has
    // committed to a subcommand — show its flag suggestions.
    if tokens.len() == 2 {
        return flag_suggestions_for(cmd);
    }

    // Otherwise, suggest subcommands that start with the typed prefix.
    SUBCOMMANDS
        .iter()
        .filter(|s| s.starts_with(cmd))
        .map(|s| s.to_string())
        .collect()
}

fn flag_suggestions_for(cmd: &str) -> Vec<String> {
    // Positional argument and subcommand hints are handwritten per command.
    let mut suggestions: Vec<String> = match cmd {
        "implement" => vec![
            "implement <NNNN>  e.g. implement 0001".into(),
        ],
        "specs" => vec![
            "specs new".into(),
            "specs new --interview".into(),
            "specs amend <NNNN>  e.g. specs amend 0025".into(),
        ],
        "claws" => vec![
            "claws init   (first-time setup: clone, build image, launch container)".into(),
            "claws ready  (check status; start container if stopped)".into(),
            "claws chat   (attach to running nanoclaw container)".into(),
        ],
        "exec" => vec![
            "exec prompt <text>     (send a one-shot prompt to the agent)".into(),
            "exec workflow <path>   (run a workflow file without a work item)".into(),
        ],
        "config" => vec![
            "config show    (view all config fields in a table dialog)".into(),
        ],
        _ => vec![],
    };

    // Flag hints are generated from the canonical CommandSpec registry.
    use crate::commands::spec::ALL_COMMANDS;
    if let Some(spec) = ALL_COMMANDS.iter().find(|c| c.name == cmd) {
        for f in spec.flags {
            if f.takes_value {
                suggestions.push(format!("--{} <{}>  \u{2014} {}", f.name, f.value_name, f.hint));
            } else {
                suggestions.push(format!("--{}  \u{2014} {}", f.name, f.hint));
            }
        }
    }

    suggestions
}

/// Return the subcommand name most similar to `input` (for typo correction).
pub fn closest_subcommand(input: &str) -> Option<String> {
    let word = input.trim().split_whitespace().next()?;
    // Already an exact match.
    if SUBCOMMANDS.contains(&word) {
        return None;
    }
    SUBCOMMANDS
        .iter()
        .map(|&s| (s, levenshtein(word, s)))
        .filter(|(_, d)| *d <= 4) // only suggest if "close enough"
        .min_by_key(|(_, d)| *d)
        .map(|(s, _)| s.to_string())
}

/// Convert a crossterm key event to the raw bytes that a terminal would send.
pub fn key_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                // Ctrl+letter → ASCII control code.
                let n = (c as u8).to_ascii_lowercase();
                if n >= b'a' && n <= b'z' {
                    return Some(vec![n - b'a' + 1]);
                }
            }
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Enter => Some(b"\r".to_vec()),
        KeyCode::Backspace => Some(b"\x7f".to_vec()),
        KeyCode::Tab => Some(b"\t".to_vec()),
        KeyCode::Esc => Some(b"\x1b".to_vec()),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::F(n) => Some(format!("\x1b[{}~", n).into_bytes()),
        _ => None,
    }
}

// ── Config dialog key handler ─────────────────────────────────────────────────

/// Handle key events for the `Dialog::ConfigShow` modal.
pub fn handle_config_show(
    tab: &mut TabState,
    key: KeyEvent,
    mut state: crate::tui::state::ConfigDialogState,
) -> Action {
    use crate::commands::config::{
        ALL_FIELDS, apply_to_global, apply_to_repo, validate_value, FieldScope,
        find_field, global_display, repo_display,
    };
    use crate::config::{
        load_global_config, load_repo_config, save_global_config, save_repo_config,
        migrate_legacy_repo_config,
    };

    if state.edit_mode {
        // ── Edit mode key handling ────────────────────────────────────────────
        match key.code {
            KeyCode::Esc => {
                state.edit_mode = false;
                state.edit_value = String::new();
                state.edit_cursor = 0;
                state.error_msg = None;
                tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
            }
            KeyCode::Enter => {
                // Validate and save the value.
                let field_key = ALL_FIELDS[state.selected_row].key;
                let field = match find_field(field_key) {
                    Some(f) => f,
                    None => {
                        state.edit_mode = false;
                        tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
                        return Action::None;
                    }
                };
                let value = state.edit_value.trim().to_string();
                // Validate.
                if let Err(e) = validate_value(field, &value) {
                    state.error_msg = Some(e.to_string());
                    state.edit_mode = false;
                    tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
                    return Action::None;
                }
                // Determine write scope (0=Global, 1=Repo).
                let write_global = state.selected_col == 0;
                if write_global {
                    let mut global = load_global_config().unwrap_or_default();
                    apply_to_global(field, &value, &mut global);
                    if let Err(e) = save_global_config(&global) {
                        state.error_msg = Some(e.to_string());
                        state.edit_mode = false;
                        tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
                        return Action::None;
                    }
                } else {
                    if let Some(ref root) = state.git_root.clone() {
                        let _ = migrate_legacy_repo_config(root);
                        let mut repo = load_repo_config(root).unwrap_or_default();
                        apply_to_repo(field, &value, &mut repo);
                        if let Err(e) = save_repo_config(root, &repo) {
                            state.error_msg = Some(e.to_string());
                            state.edit_mode = false;
                            tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
                            return Action::None;
                        }
                    }
                }
                // Reload configs.
                state.global_config = load_global_config().unwrap_or_default();
                if let Some(ref root) = state.git_root.clone() {
                    state.repo_config = load_repo_config(root).unwrap_or_default();
                }
                state.edit_mode = false;
                state.edit_value = String::new();
                state.edit_cursor = 0;
                state.error_msg = None;
                tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
            }
            KeyCode::Backspace => {
                // Delete char before cursor.
                if state.edit_cursor > 0 {
                    let cursor = state.edit_cursor;
                    // Find the previous char boundary.
                    let prev = state.edit_value[..cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    state.edit_value.remove(prev);
                    state.edit_cursor = prev;
                }
                tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
            }
            KeyCode::Left => {
                if state.edit_cursor > 0 {
                    let prev = state.edit_value[..state.edit_cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    state.edit_cursor = prev;
                }
                tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
            }
            KeyCode::Right => {
                if state.edit_cursor < state.edit_value.len() {
                    let next = state.edit_value[state.edit_cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| state.edit_cursor + i)
                        .unwrap_or(state.edit_value.len());
                    state.edit_cursor = next;
                }
                tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let cursor = state.edit_cursor;
                state.edit_value.insert(cursor, c);
                state.edit_cursor += c.len_utf8();
                tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
            }
            _ => {
                tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
            }
        }
    } else {
        // ── Normal (navigation) mode ──────────────────────────────────────────
        let num_fields = ALL_FIELDS.len();
        match key.code {
            KeyCode::Esc => {
                // Close dialog.
                tab.dialog = crate::tui::state::Dialog::None;
                return Action::None;
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+Enter also closes.
                tab.dialog = crate::tui::state::Dialog::None;
                return Action::None;
            }
            KeyCode::Up => {
                if state.selected_row > 0 {
                    state.selected_row -= 1;
                    // Constrain selected_col to the new row's scope.
                    constrain_col(&mut state);
                }
                tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
            }
            KeyCode::Down => {
                if state.selected_row + 1 < num_fields {
                    state.selected_row += 1;
                    constrain_col(&mut state);
                }
                tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
            }
            KeyCode::Left => {
                // Move to Global column (col 0) if the field scope allows it.
                let scope = ALL_FIELDS[state.selected_row].scope;
                if scope == FieldScope::Both {
                    state.selected_col = 0;
                }
                tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
            }
            KeyCode::Right => {
                // Move to Repo column (col 1) if the field scope allows it.
                let scope = ALL_FIELDS[state.selected_row].scope;
                if scope == FieldScope::Both {
                    state.selected_col = 1;
                }
                tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
            }
            KeyCode::Char('e') => {
                // Enter edit mode for the selected cell if allowed.
                let field = &ALL_FIELDS[state.selected_row];
                if !field.settable {
                    // Read-only field: show a transient error.
                    state.error_msg = Some(format!(
                        "'{}' is read-only and cannot be edited here.",
                        field.key
                    ));
                    tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
                    return Action::None;
                }
                // Scope check: can't edit Global column for repo-only, or Repo col for global-only.
                let write_global = state.selected_col == 0;
                if write_global && field.scope == FieldScope::RepoOnly {
                    state.error_msg = Some(format!("'{}' is repo-only; use the Repo column.", field.key));
                    tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
                    return Action::None;
                }
                if !write_global && field.scope == FieldScope::GlobalOnly {
                    state.error_msg = Some(format!("'{}' is global-only; use the Global column.", field.key));
                    tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
                    return Action::None;
                }
                if !write_global && state.git_root.is_none() {
                    state.error_msg = Some("Repo config unavailable (not in a git repo).".to_string());
                    tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
                    return Action::None;
                }
                // Pre-fill with the current value.
                let prefill = if write_global {
                    let raw = global_display(field, &state.global_config);
                    // Strip " (built-in)" suffix so the user edits a clean value.
                    let stripped = raw.trim_end_matches(" (built-in)");
                    // Placeholder values: start blank so user types directly.
                    if stripped == "(empty)" || stripped == "(not set)" || stripped.is_empty() {
                        String::new()
                    } else {
                        stripped.to_string()
                    }
                } else {
                    let rv = repo_display(field, Some(&state.repo_config));
                    if rv == "(not set)" || rv == "(empty)" || rv.ends_with("(read-only)") {
                        String::new()
                    } else {
                        rv
                    }
                };
                let prefill_len = prefill.len();
                state.edit_mode = true;
                state.edit_value = prefill;
                state.edit_cursor = prefill_len;
                state.error_msg = None;
                tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
            }
            _ => {
                tab.dialog = crate::tui::state::Dialog::ConfigShow(state);
            }
        }
    }
    Action::None
}

/// Constrain `selected_col` so it is valid for the current row's scope.
fn constrain_col(state: &mut crate::tui::state::ConfigDialogState) {
    use crate::commands::config::{ALL_FIELDS, FieldScope};
    let scope = ALL_FIELDS[state.selected_row].scope;
    match scope {
        FieldScope::GlobalOnly => state.selected_col = 0,
        FieldScope::RepoOnly => state.selected_col = 1,
        FieldScope::Both => {} // keep current col
    }
}

// ── Ready legacy-migration dialog ─────────────────────────────────────────────

fn handle_ready_legacy_migration(tab: &mut TabState, key: KeyEvent, _agent_name: String) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('1') => {
            tab.dialog = Dialog::None;
            Action::ReadyLegacyMigrate
        }
        KeyCode::Char('n') | KeyCode::Char('2') | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::ReadyLegacyKeep
        }
        _ => Action::None,
    }
}

// ── Ready template-audit confirm dialog ──────────────────────────────────────

fn handle_ready_template_audit_confirm(tab: &mut TabState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('1') => {
            tab.dialog = Dialog::None;
            Action::ReadyTemplateAuditAccept
        }
        KeyCode::Char('n') | KeyCode::Char('2') | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::ReadyTemplateAuditDecline
        }
        _ => Action::None,
    }
}

// ── Init audit / replace-aspec dialogs ───────────────────────────────────────

fn handle_init_audit_confirm(
    tab: &mut TabState,
    key: KeyEvent,
    agent: crate::cli::Agent,
    aspec: bool,
    replace_aspec: bool,
) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('1') => {
            tab.dialog = Dialog::None;
            Action::InitAuditAccepted { agent, aspec, replace_aspec }
        }
        KeyCode::Char('n') | KeyCode::Char('2') | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::InitAuditDeclined { agent, aspec, replace_aspec }
        }
        _ => Action::None,
    }
}

fn handle_init_replace_aspec(tab: &mut TabState, key: KeyEvent, agent: crate::cli::Agent) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('1') => {
            tab.dialog = Dialog::None;
            Action::InitReplaceAspecAccepted { agent }
        }
        KeyCode::Char('n') | KeyCode::Char('2') | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::InitReplaceAspecDeclined { agent }
        }
        _ => Action::None,
    }
}

// ── Init work-items dialogs ───────────────────────────────────────────────────

fn handle_init_work_items_confirm(
    tab: &mut TabState,
    key: KeyEvent,
    agent: crate::cli::Agent,
    aspec: bool,
    replace_aspec: bool,
    run_audit: bool,
) -> Action {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('1') => {
            // Advance to dir-input dialog.
            tab.dialog = Dialog::InitWorkItemsDirInput {
                agent,
                aspec,
                replace_aspec,
                run_audit,
                input: String::new(),
            };
            Action::None
        }
        KeyCode::Char('n') | KeyCode::Char('2') | KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::InitWorkItemsDone {
                agent,
                aspec,
                replace_aspec,
                run_audit,
                work_items: None,
            }
        }
        _ => Action::None,
    }
}

fn handle_init_work_items_dir_input(
    tab: &mut TabState,
    key: KeyEvent,
    agent: crate::cli::Agent,
    aspec: bool,
    replace_aspec: bool,
    run_audit: bool,
    mut input: String,
) -> Action {
    match key.code {
        KeyCode::Enter => {
            let trimmed = input.trim().to_string();
            if trimmed.is_empty() {
                // Empty input — treat as declined.
                tab.dialog = Dialog::None;
                return Action::InitWorkItemsDone {
                    agent,
                    aspec,
                    replace_aspec,
                    run_audit,
                    work_items: None,
                };
            }
            // Advance to template-input dialog.
            tab.dialog = Dialog::InitWorkItemsTemplateInput {
                agent,
                aspec,
                replace_aspec,
                run_audit,
                dir: trimmed,
                input: String::new(),
            };
            Action::None
        }
        KeyCode::Esc => {
            tab.dialog = Dialog::None;
            Action::InitWorkItemsDone {
                agent,
                aspec,
                replace_aspec,
                run_audit,
                work_items: None,
            }
        }
        KeyCode::Backspace => {
            input.pop();
            tab.dialog = Dialog::InitWorkItemsDirInput {
                agent,
                aspec,
                replace_aspec,
                run_audit,
                input,
            };
            Action::None
        }
        KeyCode::Char(c) => {
            input.push(c);
            tab.dialog = Dialog::InitWorkItemsDirInput {
                agent,
                aspec,
                replace_aspec,
                run_audit,
                input,
            };
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_init_work_items_template_input(
    tab: &mut TabState,
    key: KeyEvent,
    agent: crate::cli::Agent,
    aspec: bool,
    replace_aspec: bool,
    run_audit: bool,
    dir: String,
    mut input: String,
) -> Action {
    match key.code {
        KeyCode::Enter | KeyCode::Esc => {
            let template = if input.trim().is_empty() {
                None
            } else {
                Some(input.trim().to_string())
            };
            tab.dialog = Dialog::None;
            Action::InitWorkItemsDone {
                agent,
                aspec,
                replace_aspec,
                run_audit,
                work_items: Some(crate::config::WorkItemsConfig {
                    dir: Some(dir),
                    template,
                }),
            }
        }
        KeyCode::Backspace => {
            input.pop();
            tab.dialog = Dialog::InitWorkItemsTemplateInput {
                agent,
                aspec,
                replace_aspec,
                run_audit,
                dir,
                input,
            };
            Action::None
        }
        KeyCode::Char(c) => {
            input.push(c);
            tab.dialog = Dialog::InitWorkItemsTemplateInput {
                agent,
                aspec,
                replace_aspec,
                run_audit,
                dir,
                input,
            };
            Action::None
        }
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_empty_input_returns_all() {
        let suggestions = autocomplete_suggestions("");
        assert!(suggestions.contains(&"init".to_string()));
        assert!(suggestions.contains(&"ready".to_string()));
        assert!(suggestions.contains(&"implement".to_string()));
        assert!(suggestions.contains(&"claws".to_string()));
    }

    #[test]
    fn suggestions_prefix_filters_correctly() {
        let suggestions = autocomplete_suggestions("im");
        assert_eq!(suggestions, vec!["implement"]);
    }

    #[test]
    fn suggestions_prefix_init() {
        let suggestions = autocomplete_suggestions("in");
        assert_eq!(suggestions, vec!["init"]);
    }

    #[test]
    fn suggestions_full_command_with_space_shows_flags() {
        let suggestions = autocomplete_suggestions("init ");
        assert!(suggestions.iter().any(|s| s.contains("--agent")));
    }

    // ── flag_suggestions_for ──────────────────────────────────────────────────

    #[test]
    fn flag_suggestions_for_chat_contains_agent() {
        let suggestions = flag_suggestions_for("chat");
        assert!(
            suggestions.iter().any(|s| s.contains("--agent")),
            "flag_suggestions_for(\"chat\") must contain an --agent entry; got: {:?}",
            suggestions,
        );
    }

    #[test]
    fn flag_suggestions_for_implement_contains_agent_and_workflow() {
        let suggestions = flag_suggestions_for("implement");
        assert!(
            suggestions.iter().any(|s| s.contains("--agent")),
            "flag_suggestions_for(\"implement\") must contain --agent; got: {:?}",
            suggestions,
        );
        assert!(
            suggestions.iter().any(|s| s.contains("--workflow")),
            "flag_suggestions_for(\"implement\") must contain --workflow; got: {:?}",
            suggestions,
        );
    }

    #[test]
    fn closest_subcommand_corrects_typo() {
        assert_eq!(closest_subcommand("implemnt"), Some("implement".into()));
        assert_eq!(closest_subcommand("redy"), Some("ready".into()));
        assert_eq!(closest_subcommand("int"), Some("init".into()));
    }

    #[test]
    fn closest_subcommand_exact_returns_none() {
        assert_eq!(closest_subcommand("ready"), None);
    }

    #[test]
    fn closest_subcommand_gibberish_returns_none() {
        // "xyzxyzxyz" is too far from any subcommand.
        assert_eq!(closest_subcommand("xyzxyzxyz"), None);
    }

    #[test]
    fn key_to_bytes_regular_char() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty());
        assert_eq!(key_to_bytes(&key), Some(b"a".to_vec()));
    }

    #[test]
    fn key_to_bytes_enter_is_cr() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
        assert_eq!(key_to_bytes(&key), Some(b"\r".to_vec()));
    }

    #[test]
    fn key_to_bytes_arrow_up() {
        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::empty());
        assert_eq!(key_to_bytes(&key), Some(b"\x1b[A".to_vec()));
    }

    #[test]
    fn key_to_bytes_ctrl_c() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(key_to_bytes(&key), Some(vec![3]));
    }

    fn new_app() -> App {
        App::new(std::path::PathBuf::new())
    }

    #[test]
    fn arrow_up_scrolls_in_done_state_with_window_focused() {
        let mut app = new_app();
        for i in 0..50 {
            app.active_tab_mut().output_lines.push(format!("line {}", i));
        }
        app.active_tab_mut().phase = ExecutionPhase::Done { command: "ready".into() };
        app.active_tab_mut().focus = Focus::ExecutionWindow;
        app.active_tab_mut().scroll_offset = 0;

        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::empty());
        let action = handle_key(&mut app, key);
        assert!(matches!(action, Action::None));
        assert_eq!(app.active_tab().scroll_offset, 1, "Up should increment scroll_offset");
        assert_eq!(app.active_tab().focus, Focus::ExecutionWindow, "Focus should stay on window");

        // Press Down to go back.
        let key = KeyEvent::new(KeyCode::Down, KeyModifiers::empty());
        let action = handle_key(&mut app, key);
        assert!(matches!(action, Action::None));
        assert_eq!(app.active_tab().scroll_offset, 0, "Down should decrement scroll_offset");
    }

    // --- Container window input tests ---

    #[test]
    fn esc_forwarded_to_pty_when_container_maximized() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().focus = Focus::ExecutionWindow;
        app.active_tab_mut().container_window = ContainerWindowState::Maximized;

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
        let action = handle_key(&mut app, key);
        // Esc is forwarded to the PTY as \x1b — use Ctrl-M to toggle the window.
        assert!(
            matches!(action, Action::ForwardToPty(ref b) if b == b"\x1b"),
            "Esc should be forwarded to PTY when container is maximized"
        );
        assert_eq!(app.active_tab().container_window, ContainerWindowState::Maximized);
    }

    #[test]
    fn c_key_does_not_restore_container_window_when_minimized() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().focus = Focus::ExecutionWindow;
        app.active_tab_mut().container_window = ContainerWindowState::Minimized;

        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::empty());
        let action = handle_key(&mut app, key);
        assert!(matches!(action, Action::None));
        // bare 'c' no longer restores — use Ctrl-M instead.
        assert_eq!(app.active_tab().container_window, ContainerWindowState::Minimized);
    }

    #[test]
    fn esc_from_minimized_outer_window_goes_to_command_box() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().focus = Focus::ExecutionWindow;
        app.active_tab_mut().container_window = ContainerWindowState::Minimized;

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
        let action = handle_key(&mut app, key);
        assert!(matches!(action, Action::None));
        assert_eq!(app.active_tab().focus, Focus::CommandBox);
    }

    #[test]
    fn keys_forwarded_to_pty_when_container_maximized() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().focus = Focus::ExecutionWindow;
        app.active_tab_mut().container_window = ContainerWindowState::Maximized;

        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty());
        let action = handle_key(&mut app, key);
        assert!(matches!(action, Action::ForwardToPty(_)));
    }

    #[test]
    fn arrow_keys_scroll_outer_when_container_minimized() {
        let mut app = new_app();
        for i in 0..50 {
            app.active_tab_mut().output_lines.push(format!("line {}", i));
        }
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().focus = Focus::ExecutionWindow;
        app.active_tab_mut().container_window = ContainerWindowState::Minimized;
        app.active_tab_mut().scroll_offset = 0;

        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::empty());
        handle_key(&mut app, key);
        assert_eq!(app.active_tab().scroll_offset, 1, "Up should scroll outer window when container minimized");
    }

    #[test]
    fn up_arrow_from_command_box_focuses_outer_regardless_of_container_state() {
        let mut app = new_app();
        app.active_tab_mut().output_lines.push("some output".into());
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().focus = Focus::CommandBox;
        app.active_tab_mut().container_window = ContainerWindowState::Minimized;

        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::empty());
        handle_key(&mut app, key);
        assert_eq!(app.active_tab().focus, Focus::ExecutionWindow);
    }

    #[test]
    fn suggestions_claws_prefix() {
        let suggestions = autocomplete_suggestions("cl");
        assert!(suggestions.contains(&"claws".to_string()), "cl should match claws: {:?}", suggestions);
    }

    #[test]
    fn suggestions_claws_space_shows_ready() {
        let suggestions = autocomplete_suggestions("claws ");
        assert!(
            suggestions.iter().any(|s| s.contains("ready")),
            "claws  should show ready suggestion: {:?}",
            suggestions
        );
    }

    #[test]
    fn arrow_up_from_command_box_focuses_window_then_scrolls() {
        let mut app = new_app();
        for i in 0..50 {
            app.active_tab_mut().output_lines.push(format!("line {}", i));
        }
        app.active_tab_mut().phase = ExecutionPhase::Done { command: "ready".into() };
        app.active_tab_mut().focus = Focus::CommandBox;
        app.active_tab_mut().scroll_offset = 0;

        // First Up: should move focus to ExecutionWindow but NOT scroll.
        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::empty());
        handle_key(&mut app, key);
        assert_eq!(app.active_tab().focus, Focus::ExecutionWindow);
        assert_eq!(app.active_tab().scroll_offset, 0, "First Up only focuses, doesn't scroll");

        // Second Up: now that we're in ExecutionWindow, should scroll.
        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::empty());
        handle_key(&mut app, key);
        assert_eq!(app.active_tab().focus, Focus::ExecutionWindow);
        assert_eq!(app.active_tab().scroll_offset, 1, "Second Up should scroll");
    }

    #[test]
    fn sudo_confirm_dialog_enter_sends_password_and_clears_dialog() {
        let mut app = new_app();
        app.active_tab_mut().dialog = Dialog::ClawsReadySudoConfirm { password: "s3cr3t".to_string() };
        let (tx, mut rx) = tokio::sync::oneshot::channel::<Option<String>>();
        app.active_tab_mut().claws_sudo_response_tx = Some(tx);

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
        handle_key(&mut app, key);

        assert_eq!(app.active_tab().dialog, Dialog::None);
        assert!(app.active_tab().claws_sudo_response_tx.is_none());
        assert_eq!(rx.try_recv().unwrap(), Some("s3cr3t".to_string()));
    }

    #[test]
    fn sudo_confirm_dialog_esc_sends_none_and_clears_dialog() {
        let mut app = new_app();
        app.active_tab_mut().dialog = Dialog::ClawsReadySudoConfirm { password: "abc".to_string() };
        let (tx, mut rx) = tokio::sync::oneshot::channel::<Option<String>>();
        app.active_tab_mut().claws_sudo_response_tx = Some(tx);

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
        handle_key(&mut app, key);

        assert_eq!(app.active_tab().dialog, Dialog::None);
        assert_eq!(rx.try_recv().unwrap(), None);
    }

    #[test]
    fn sudo_confirm_dialog_char_appends_to_password() {
        let mut app = new_app();
        app.active_tab_mut().dialog = Dialog::ClawsReadySudoConfirm { password: String::new() };
        let (tx, _rx) = tokio::sync::oneshot::channel::<Option<String>>();
        app.active_tab_mut().claws_sudo_response_tx = Some(tx);

        for c in "pass".chars() {
            let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty());
            handle_key(&mut app, key);
        }
        assert_eq!(app.active_tab().dialog, Dialog::ClawsReadySudoConfirm { password: "pass".to_string() });
    }

    #[test]
    fn sudo_confirm_dialog_backspace_removes_last_char() {
        let mut app = new_app();
        app.active_tab_mut().dialog = Dialog::ClawsReadySudoConfirm { password: "abc".to_string() };
        let (tx, _rx) = tokio::sync::oneshot::channel::<Option<String>>();
        app.active_tab_mut().claws_sudo_response_tx = Some(tx);

        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty());
        handle_key(&mut app, key);

        assert_eq!(app.active_tab().dialog, Dialog::ClawsReadySudoConfirm { password: "ab".to_string() });
    }

    #[test]
    fn sudo_confirm_dialog_enter_with_empty_password_sends_some_empty() {
        let mut app = new_app();
        app.active_tab_mut().dialog = Dialog::ClawsReadySudoConfirm { password: String::new() };
        let (tx, mut rx) = tokio::sync::oneshot::channel::<Option<String>>();
        app.active_tab_mut().claws_sudo_response_tx = Some(tx);

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
        handle_key(&mut app, key);

        assert_eq!(app.active_tab().dialog, Dialog::None);
        // Empty password is allowed (e.g. NOPASSWD sudo configs).
        assert_eq!(rx.try_recv().unwrap(), Some(String::new()));
    }

    #[test]
    fn suggestions_empty_input_includes_specs() {
        let suggestions = autocomplete_suggestions("");
        assert!(suggestions.contains(&"specs".to_string()), "Empty input should include specs: {:?}", suggestions);
    }

    #[test]
    fn suggestions_specs_space_shows_subcommands() {
        let suggestions = autocomplete_suggestions("specs ");
        assert!(
            suggestions.iter().any(|s| s.contains("new")),
            "specs  should show new suggestion: {:?}",
            suggestions
        );
        assert!(
            suggestions.iter().any(|s| s.contains("amend")),
            "specs  should show amend suggestion: {:?}",
            suggestions
        );
    }

    // ─── Workflow control board: dialog state transitions ────────────────────────

    fn make_test_workflow_state() -> crate::workflow::WorkflowState {
        crate::workflow::WorkflowState::new(
            None,
            vec![crate::workflow::parser::WorkflowStep {
                name: "step-one".to_string(),
                depends_on: vec![],
                prompt_template: "do step one".to_string(),
                agent: None,
                model: None,
            }],
            "hash".to_string(),
            Some(1),
            "test-wf".to_string(),
        )
    }

    fn setup_running_workflow_app() -> App {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().focus = Focus::ExecutionWindow;
        app.active_tab_mut().container_window = ContainerWindowState::Minimized;
        app.active_tab_mut().workflow = Some(make_test_workflow_state());
        app.active_tab_mut().workflow_current_step = Some("step-one".to_string());
        app
    }

    #[test]
    fn workflow_control_board_up_returns_restart_and_clears_dialog() {
        let mut app = new_app();
        app.active_tab_mut().dialog = Dialog::WorkflowControlBoard {
            current_step: "step-one".to_string(),
            error: None,
        };
        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::empty());
        let action = handle_key(&mut app, key);
        assert!(matches!(action, Action::WorkflowRestartStep));
        assert_eq!(app.active_tab().dialog, Dialog::None);
    }

    #[test]
    fn workflow_control_board_left_returns_cancel_and_clears_dialog() {
        let mut app = new_app();
        app.active_tab_mut().dialog = Dialog::WorkflowControlBoard {
            current_step: "step-one".to_string(),
            error: None,
        };
        let key = KeyEvent::new(KeyCode::Left, KeyModifiers::empty());
        let action = handle_key(&mut app, key);
        assert!(matches!(action, Action::WorkflowCancelToPrevious));
        assert_eq!(app.active_tab().dialog, Dialog::None);
    }

    #[test]
    fn workflow_control_board_right_returns_next_new_container_and_clears_dialog() {
        let mut app = new_app();
        app.active_tab_mut().dialog = Dialog::WorkflowControlBoard {
            current_step: "step-one".to_string(),
            error: None,
        };
        let key = KeyEvent::new(KeyCode::Right, KeyModifiers::empty());
        let action = handle_key(&mut app, key);
        assert!(matches!(action, Action::WorkflowNextInNewContainer));
        assert_eq!(app.active_tab().dialog, Dialog::None);
    }

    #[test]
    fn workflow_control_board_down_returns_next_current_container_and_clears_dialog() {
        let mut app = new_app();
        app.active_tab_mut().dialog = Dialog::WorkflowControlBoard {
            current_step: "step-one".to_string(),
            error: None,
        };
        let key = KeyEvent::new(KeyCode::Down, KeyModifiers::empty());
        let action = handle_key(&mut app, key);
        assert!(matches!(action, Action::WorkflowNextInCurrentContainer));
        assert_eq!(app.active_tab().dialog, Dialog::None);
    }

    #[test]
    fn workflow_control_board_esc_returns_none_and_clears_dialog() {
        let mut app = new_app();
        app.active_tab_mut().dialog = Dialog::WorkflowControlBoard {
            current_step: "step-one".to_string(),
            error: None,
        };
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
        let action = handle_key(&mut app, key);
        assert!(matches!(action, Action::None));
        assert_eq!(app.active_tab().dialog, Dialog::None);
    }

    #[test]
    fn workflow_control_board_non_arrow_key_leaves_dialog_open() {
        let mut app = new_app();
        app.active_tab_mut().dialog = Dialog::WorkflowControlBoard {
            current_step: "step-one".to_string(),
            error: None,
        };
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
        handle_key(&mut app, key);
        assert!(
            matches!(app.active_tab().dialog, Dialog::WorkflowControlBoard { .. }),
            "Dialog should remain open for non-arrow keys"
        );
    }

    // ─── Ctrl+W guard conditions ─────────────────────────────────────────────────

    #[test]
    fn ctrl_w_opens_workflow_control_board_when_all_guards_pass() {
        let mut app = setup_running_workflow_app();
        let key = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        handle_key(&mut app, key);
        match &app.active_tab().dialog {
            Dialog::WorkflowControlBoard { current_step, error } => {
                assert_eq!(current_step, "step-one");
                assert_eq!(*error, None);
            }
            other => panic!("Expected WorkflowControlBoard dialog, got {:?}", other),
        }
    }

    #[test]
    fn ctrl_w_does_not_open_dialog_when_workflow_is_none() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        // workflow is None (default)
        app.active_tab_mut().workflow_current_step = Some("step-one".to_string());
        app.active_tab_mut().container_window = ContainerWindowState::Minimized;
        let key = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        handle_key(&mut app, key);
        assert_eq!(app.active_tab().dialog, Dialog::None);
    }

    #[test]
    fn ctrl_w_does_not_open_dialog_when_current_step_is_none() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().workflow = Some(make_test_workflow_state());
        // workflow_current_step is None (default)
        app.active_tab_mut().container_window = ContainerWindowState::Minimized;
        let key = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        handle_key(&mut app, key);
        assert_eq!(app.active_tab().dialog, Dialog::None);
    }

    #[test]
    fn ctrl_w_opens_dialog_when_container_is_maximized() {
        let mut app = setup_running_workflow_app();
        app.active_tab_mut().container_window = ContainerWindowState::Maximized;
        let key = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        handle_key(&mut app, key);
        // Ctrl-W opens the workflow control board regardless of container window state.
        assert!(
            matches!(app.active_tab().dialog, Dialog::WorkflowControlBoard { .. }),
            "expected WorkflowControlBoard dialog"
        );
    }

    #[test]
    fn ctrl_w_does_not_open_dialog_when_another_dialog_is_active() {
        let mut app = setup_running_workflow_app();
        app.active_tab_mut().dialog = Dialog::QuitConfirm;
        let key = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        handle_key(&mut app, key);
        // Dialog should remain QuitConfirm, not become WorkflowControlBoard.
        assert_eq!(app.active_tab().dialog, Dialog::QuitConfirm);
    }

    // ─── Auto-advance: input routing over maximized container ────────────────────

    /// When a WorkflowControlBoard dialog is open over a maximized container window,
    /// keystrokes must be dispatched to the dialog handler, not forwarded to the PTY.
    /// handle_key dispatches dialogs before reaching handle_window_key, so this
    /// property holds regardless of container_window state.
    #[test]
    fn keys_route_to_dialog_not_pty_when_dialog_open_over_maximized_container() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().focus = Focus::ExecutionWindow;
        app.active_tab_mut().container_window = ContainerWindowState::Maximized;
        app.active_tab_mut().dialog = Dialog::WorkflowControlBoard {
            current_step: "step-one".to_string(),
            error: None,
        };

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
        let action = handle_key(&mut app, key);

        // Esc must not leak through to the PTY.
        assert!(
            !matches!(action, Action::ForwardToPty(_)),
            "Esc should not be forwarded to the PTY when a dialog is open"
        );
        // The dialog handler consumed Esc and cleared the dialog.
        assert_eq!(app.active_tab().dialog, Dialog::None);
    }

    #[test]
    fn c_key_in_workflow_control_board_dismisses_dialog_and_restores_minimized_container() {
        let mut app = setup_running_workflow_app();
        // setup_running_workflow_app sets container_window = Minimized
        app.active_tab_mut().dialog = Dialog::WorkflowControlBoard {
            current_step: "step-one".to_string(),
            error: None,
        };

        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::empty());
        handle_key(&mut app, key);

        assert_eq!(app.active_tab().dialog, Dialog::None, "Dialog should be dismissed");
        assert_eq!(
            app.active_tab().container_window,
            ContainerWindowState::Maximized,
            "Container window should be restored to Maximized"
        );
    }

    #[test]
    fn c_key_in_workflow_control_board_dismisses_dialog_when_container_not_minimized() {
        let mut app = setup_running_workflow_app();
        app.active_tab_mut().container_window = ContainerWindowState::Maximized;
        app.active_tab_mut().dialog = Dialog::WorkflowControlBoard {
            current_step: "step-one".to_string(),
            error: None,
        };

        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::empty());
        handle_key(&mut app, key);

        assert_eq!(app.active_tab().dialog, Dialog::None, "Dialog should be dismissed");
        assert_eq!(
            app.active_tab().container_window,
            ContainerWindowState::Maximized,
            "Container window should remain Maximized"
        );
    }

    /// Confirm the same holds for an arrow key (↑ = WorkflowRestartStep).
    #[test]
    fn arrow_key_routes_to_dialog_not_pty_when_dialog_open_over_maximized_container() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().focus = Focus::ExecutionWindow;
        app.active_tab_mut().container_window = ContainerWindowState::Maximized;
        app.active_tab_mut().dialog = Dialog::WorkflowControlBoard {
            current_step: "step-one".to_string(),
            error: None,
        };

        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::empty());
        let action = handle_key(&mut app, key);

        assert!(
            !matches!(action, Action::ForwardToPty(_)),
            "Up arrow should not be forwarded to the PTY when a dialog is open"
        );
        assert!(matches!(action, Action::WorkflowRestartStep));
    }

    /// 'c' from CommandBox focus (e.g. after pressing Esc from minimized container)
    /// must restore the container window even when no dialog is open.
    #[test]
    fn c_key_from_command_box_does_not_restore_minimized_container_during_workflow() {
        let mut app = setup_running_workflow_app();
        // Simulate user having pressed Esc to move focus to CommandBox
        app.active_tab_mut().focus = Focus::CommandBox;
        assert_eq!(app.active_tab().container_window, ContainerWindowState::Minimized);
        assert_eq!(app.active_tab().dialog, Dialog::None);

        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::empty());
        handle_key(&mut app, key);

        // bare 'c' no longer restores the container — use Ctrl-M instead.
        assert_eq!(
            app.active_tab().container_window,
            ContainerWindowState::Minimized,
            "'c' should not restore the minimized container"
        );
        assert_eq!(
            app.active_tab().focus,
            Focus::CommandBox,
            "focus should remain on CommandBox"
        );
    }

    // ─── Ctrl-M container window toggle ──────────────────────────────────────

    #[test]
    fn ctrl_m_maximized_to_minimized_clears_terminal_selection() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().focus = Focus::ExecutionWindow;
        app.active_tab_mut().container_window = ContainerWindowState::Maximized;
        // Set a terminal selection to verify it is cleared on minimize.
        app.active_tab_mut().terminal_selection_start = Some((0, 0));
        app.active_tab_mut().terminal_selection_end = Some((0, 5));

        let key = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL);
        let action = handle_key(&mut app, key);

        assert!(matches!(action, Action::None));
        assert_eq!(app.active_tab().container_window, ContainerWindowState::Minimized);
        assert!(
            app.active_tab().terminal_selection_start.is_none(),
            "terminal_selection_start should be cleared on minimize"
        );
        assert!(
            app.active_tab().terminal_selection_end.is_none(),
            "terminal_selection_end should be cleared on minimize"
        );
    }

    #[test]
    fn ctrl_m_minimized_to_maximized_focuses_execution_window() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
        app.active_tab_mut().focus = Focus::CommandBox;
        app.active_tab_mut().container_window = ContainerWindowState::Minimized;

        let key = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL);
        let action = handle_key(&mut app, key);

        assert!(matches!(action, Action::None));
        assert_eq!(app.active_tab().container_window, ContainerWindowState::Maximized);
        assert_eq!(app.active_tab().focus, Focus::ExecutionWindow);
    }

    #[test]
    fn ctrl_m_hidden_is_noop() {
        let mut app = new_app();
        app.active_tab_mut().phase = ExecutionPhase::Idle;
        app.active_tab_mut().focus = Focus::CommandBox;
        app.active_tab_mut().container_window = ContainerWindowState::Hidden;

        let key = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL);
        let action = handle_key(&mut app, key);

        assert!(matches!(action, Action::None));
        assert_eq!(app.active_tab().container_window, ContainerWindowState::Hidden);
        assert_eq!(app.active_tab().focus, Focus::CommandBox);
    }

    // ─── Ctrl-, config show toggle ────────────────────────────────────────────

    #[test]
    fn ctrl_comma_opens_config_show_when_idle() {
        let mut app = new_app();
        // Default state: Idle, CommandBox, Hidden, no dialog.
        assert_eq!(app.active_tab().dialog, Dialog::None);

        let key = KeyEvent::new(KeyCode::Char(','), KeyModifiers::CONTROL);
        let action = handle_key(&mut app, key);

        assert!(matches!(action, Action::None));
        assert!(
            matches!(app.active_tab().dialog, Dialog::ConfigShow(_)),
            "Ctrl-, should open ConfigShow when idle; got {:?}",
            app.active_tab().dialog,
        );
    }

    #[test]
    fn ctrl_comma_opens_config_show_when_container_maximized() {
        let mut app = setup_running_workflow_app();
        app.active_tab_mut().container_window = ContainerWindowState::Maximized;

        let key = KeyEvent::new(KeyCode::Char(','), KeyModifiers::CONTROL);
        let action = handle_key(&mut app, key);

        assert!(matches!(action, Action::None));
        assert!(
            matches!(app.active_tab().dialog, Dialog::ConfigShow(_)),
            "Ctrl-, should open ConfigShow even when container is maximized; got {:?}",
            app.active_tab().dialog,
        );
    }

    #[test]
    fn ctrl_comma_toggles_off_config_show() {
        let mut app = new_app();
        let key = KeyEvent::new(KeyCode::Char(','), KeyModifiers::CONTROL);
        // First press opens ConfigShow.
        handle_key(&mut app, key);
        assert!(
            matches!(app.active_tab().dialog, Dialog::ConfigShow(_)),
            "first Ctrl-, should open ConfigShow"
        );
        // Second press closes it.
        handle_key(&mut app, key);
        assert_eq!(
            app.active_tab().dialog,
            Dialog::None,
            "second Ctrl-, should close ConfigShow"
        );
    }

    #[test]
    fn ctrl_comma_noop_when_other_dialog_active() {
        let mut app = new_app();
        app.active_tab_mut().dialog = Dialog::QuitConfirm;

        let key = KeyEvent::new(KeyCode::Char(','), KeyModifiers::CONTROL);
        handle_key(&mut app, key);

        assert_eq!(
            app.active_tab().dialog,
            Dialog::QuitConfirm,
            "Ctrl-, should not affect other active dialogs"
        );
    }

    // ── SUBCOMMANDS / autocomplete (work item 0059) ──────────────────────────

    #[test]
    fn subcommands_list_includes_remote() {
        assert!(
            SUBCOMMANDS.contains(&"remote"),
            "SUBCOMMANDS must include 'remote'; current list: {SUBCOMMANDS:?}"
        );
    }

    #[test]
    fn closest_subcommand_corrects_remote_typo() {
        // "remte" is distance 2 from "remote" (well within the threshold of 4).
        assert_eq!(
            closest_subcommand("remte"),
            Some("remote".to_string()),
            "closest_subcommand should correct 'remte' → 'remote'"
        );
    }

    // ── handle_remote_session_picker (work item 0059) ────────────────────────

    fn make_sessions() -> Vec<crate::commands::remote::RemoteSessionEntry> {
        vec![
            crate::commands::remote::RemoteSessionEntry {
                id: "sess-aaa".to_string(),
                workdir: "/workspace/a".to_string(),
            },
            crate::commands::remote::RemoteSessionEntry {
                id: "sess-bbb".to_string(),
                workdir: "/workspace/b".to_string(),
            },
        ]
    }

    #[test]
    fn remote_session_picker_esc_closes_dialog() {
        let mut tab = crate::tui::state::TabState::new(std::path::PathBuf::new());
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
        let action = handle_remote_session_picker(
            &mut tab,
            key,
            make_sessions(),
            0,
            "http://localhost:9876".to_string(),
            vec!["status".to_string()],
            false,
        );
        assert!(matches!(action, Action::None), "Esc must return Action::None");
        assert_eq!(tab.dialog, Dialog::None, "Esc must close the dialog");
    }

    #[test]
    fn remote_session_picker_down_increments_selection() {
        let mut tab = crate::tui::state::TabState::new(std::path::PathBuf::new());
        let key = KeyEvent::new(KeyCode::Down, KeyModifiers::empty());
        let action = handle_remote_session_picker(
            &mut tab,
            key,
            make_sessions(),
            0,
            "http://localhost:9876".to_string(),
            vec![],
            false,
        );
        assert!(matches!(action, Action::None));
        match &tab.dialog {
            Dialog::RemoteSessionPicker { selected, .. } => {
                assert_eq!(*selected, 1, "Down must increment selected from 0 to 1");
            }
            other => panic!("expected RemoteSessionPicker dialog, got {:?}", other),
        }
    }

    #[test]
    fn remote_session_picker_down_does_not_exceed_last() {
        let mut tab = crate::tui::state::TabState::new(std::path::PathBuf::new());
        let key = KeyEvent::new(KeyCode::Down, KeyModifiers::empty());
        // selected = 1 (last index for 2-item list)
        handle_remote_session_picker(
            &mut tab,
            key,
            make_sessions(),
            1,
            "http://localhost:9876".to_string(),
            vec![],
            false,
        );
        match &tab.dialog {
            Dialog::RemoteSessionPicker { selected, .. } => {
                assert_eq!(*selected, 1, "Down at last item must not exceed bounds");
            }
            other => panic!("expected RemoteSessionPicker dialog, got {:?}", other),
        }
    }

    #[test]
    fn remote_session_picker_up_decrements_selection() {
        let mut tab = crate::tui::state::TabState::new(std::path::PathBuf::new());
        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::empty());
        handle_remote_session_picker(
            &mut tab,
            key,
            make_sessions(),
            1,
            "http://localhost:9876".to_string(),
            vec![],
            false,
        );
        match &tab.dialog {
            Dialog::RemoteSessionPicker { selected, .. } => {
                assert_eq!(*selected, 0, "Up must decrement selected from 1 to 0");
            }
            other => panic!("expected RemoteSessionPicker dialog, got {:?}", other),
        }
    }

    #[test]
    fn remote_session_picker_up_does_not_underflow() {
        let mut tab = crate::tui::state::TabState::new(std::path::PathBuf::new());
        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::empty());
        handle_remote_session_picker(
            &mut tab,
            key,
            make_sessions(),
            0,
            "http://localhost:9876".to_string(),
            vec![],
            false,
        );
        match &tab.dialog {
            Dialog::RemoteSessionPicker { selected, .. } => {
                assert_eq!(*selected, 0, "Up at index 0 must not underflow");
            }
            other => panic!("expected RemoteSessionPicker dialog, got {:?}", other),
        }
    }

    #[test]
    fn remote_session_picker_enter_returns_chosen_session() {
        let mut tab = crate::tui::state::TabState::new(std::path::PathBuf::new());
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
        let action = handle_remote_session_picker(
            &mut tab,
            key,
            make_sessions(),
            1,
            "http://localhost:9876".to_string(),
            vec!["status".to_string()],
            false,
        );
        match action {
            Action::RemoteSessionChosen { session_id } => {
                assert_eq!(session_id, "sess-bbb", "Enter must return the highlighted session id");
            }
            _ => panic!("expected Action::RemoteSessionChosen, got something else"),
        }
        assert_eq!(tab.dialog, Dialog::None, "Enter must close the dialog");
    }

    #[test]
    fn remote_session_picker_enter_on_empty_list_returns_none() {
        let mut tab = crate::tui::state::TabState::new(std::path::PathBuf::new());
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
        let action = handle_remote_session_picker(
            &mut tab,
            key,
            vec![],
            0,
            "http://localhost:9876".to_string(),
            vec![],
            false,
        );
        assert!(matches!(action, Action::None), "Enter on empty list must return Action::None");
        assert_eq!(tab.dialog, Dialog::None);
    }

    // ── handle_remote_save_dir_confirm (work item 0059) ──────────────────────

    #[test]
    fn remote_save_dir_confirm_y_returns_accepted() {
        let mut tab = crate::tui::state::TabState::new(std::path::PathBuf::new());
        let key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::empty());
        let action = handle_remote_save_dir_confirm(
            &mut tab,
            key,
            "/workspace/project".to_string(),
            "http://localhost:9876".to_string(),
        );
        assert!(
            matches!(action, Action::RemoteSaveDirAccepted),
            "'y' must return RemoteSaveDirAccepted"
        );
        assert_eq!(tab.dialog, Dialog::None, "'y' must close the dialog");
    }

    #[test]
    fn remote_save_dir_confirm_uppercase_y_returns_accepted() {
        let mut tab = crate::tui::state::TabState::new(std::path::PathBuf::new());
        let key = KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::empty());
        let action = handle_remote_save_dir_confirm(
            &mut tab,
            key,
            "/workspace/project".to_string(),
            "http://localhost:9876".to_string(),
        );
        assert!(matches!(action, Action::RemoteSaveDirAccepted), "'Y' must return RemoteSaveDirAccepted");
    }

    #[test]
    fn remote_save_dir_confirm_n_returns_declined() {
        let mut tab = crate::tui::state::TabState::new(std::path::PathBuf::new());
        let key = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::empty());
        let action = handle_remote_save_dir_confirm(
            &mut tab,
            key,
            "/workspace/project".to_string(),
            "http://localhost:9876".to_string(),
        );
        assert!(
            matches!(action, Action::RemoteSaveDirDeclined),
            "'n' must return RemoteSaveDirDeclined"
        );
        assert_eq!(tab.dialog, Dialog::None, "'n' must close the dialog");
    }

    #[test]
    fn remote_save_dir_confirm_enter_returns_declined() {
        let mut tab = crate::tui::state::TabState::new(std::path::PathBuf::new());
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
        let action = handle_remote_save_dir_confirm(
            &mut tab,
            key,
            "/workspace/project".to_string(),
            "http://localhost:9876".to_string(),
        );
        assert!(
            matches!(action, Action::RemoteSaveDirDeclined),
            "Enter must return RemoteSaveDirDeclined (proceed without saving)"
        );
    }

    #[test]
    fn remote_save_dir_confirm_esc_cancels_and_clears_pending_command() {
        let mut tab = crate::tui::state::TabState::new(std::path::PathBuf::new());
        // Set a pending command so we can verify Esc clears it.
        tab.pending_command = PendingCommand::RemoteSessionStart {
            dir: "/workspace/project".to_string(),
            remote_addr: "http://localhost:9876".to_string(),
            api_key: None,
        };
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
        let action = handle_remote_save_dir_confirm(
            &mut tab,
            key,
            "/workspace/project".to_string(),
            "http://localhost:9876".to_string(),
        );
        assert!(matches!(action, Action::None), "Esc must return Action::None (abort entirely)");
        assert_eq!(tab.dialog, Dialog::None, "Esc must close the dialog");
        assert!(
            matches!(tab.pending_command, PendingCommand::None),
            "Esc must clear pending_command to abort the session start"
        );
    }
}
