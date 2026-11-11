//! Keyboard shortcut definitions — every shortcut is defined here.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Actions produced by the keymap. The event loop matches these to state
/// transitions; no business logic lives here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    // ── Global ──────────────────────────────────────────────────────────
    OpenNewTabDialog,
    PreviousTab,
    NextTab,
    CloseTabOrQuit,
    CycleContainerWindow,
    /// Ctrl-O ("overview"): toggle the Workflow Overview between its minimized
    /// one-box-per-stage view and its maximized every-step view. Independent of
    /// `CycleContainerWindow` (Ctrl-M) — neither min/max affects the other.
    ToggleWorkflowOverview,
    /// Shift-Left — scroll the Workflow Overview one stage to the left while
    /// its columns overflow the available width.
    ScrollWorkflowOverviewLeft,
    /// Shift-Right — scroll the Workflow Overview one stage to the right while
    /// its columns overflow the available width.
    ScrollWorkflowOverviewRight,
    OpenConfigShow,
    /// Ctrl-Q — open the work-queue panel (WI 0143). Terminals that use
    /// Ctrl-Q for XON flow control may swallow it.
    OpenQueuePanel,
    WorkflowControl,
    ToggleGitSidebar,

    // ── Command box ─────────────────────────────────────────────────────
    SubmitCommand,
    AutocompleteNext,
    AutocompletePrev,
    FocusExecutionWindow,

    // ── Execution window ────────────────────────────────────────────────
    FocusCommandBox,
    ScrollUp,
    ScrollDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollToTop,
    ScrollToBottom,
    CopySelection,
    ToggleStatusLog,
    /// Ctrl-\ — leave the container view without signalling any container
    /// (WI 0110). Ends a squad attach session, or minimizes an ordinary
    /// command's maximized container. Never kills or interrupts an agent.
    DetachContainers,

    // ── Dialog ──────────────────────────────────────────────────────────
    DismissDialog,
    /// Ctrl+N in the config dialog: start the add-model-mapping flow.
    NewMapEntry,

    // ── squad list (WI 0102) ─────────────────────────────────────────────
    /// Enter — open the task detail modal for the selected row.
    SquadShowDetail,
    /// h — open the run-history modal for the selected row.
    SquadShowHistory,
    /// a — attach to the selected task's running container(s).
    SquadAttach,
    /// n — create a task (drives the Layer-2 interview dialog chain).
    SquadNew,
    /// e — edit the selected task (WI 0110), through the same interview
    /// dialog chain, prefilled with the task as it stands.
    SquadEdit,
    /// p — pause the selected task.
    SquadPause,
    /// r — resume the selected task.
    SquadResume,
    /// t — evaluate the selected task now, ignoring its schedule.
    SquadTrigger,
    /// c — cancel the selected task's in-progress run.
    SquadCancel,
    /// d — remove the selected task (opens a confirmation first).
    SquadDelete,
    /// Left — move the card grid selection one column left.
    SquadMoveLeft,
    /// Right — move the card grid selection one column right.
    SquadMoveRight,

    // ── Text input ──────────────────────────────────────────────────────
    Char(char),
    Backspace,
    Delete,
    BackspaceWord,
    CursorLeft,
    CursorRight,
    CursorWordLeft,
    CursorWordRight,
    CursorHome,
    CursorEnd,
    InsertNewline,

    // ── Passthrough to PTY ──────────────────────────────────────────────
    ForwardToPty(KeyEvent),

    // ── No-op ───────────────────────────────────────────────────────────
    None,
}

/// The focus context determines which key bindings are active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusContext {
    CommandBox,
    ExecutionWindow,
    Dialog,
    ContainerMaximized,
    /// The squad tab's task list has focus (WI 0102). Only reachable when
    /// the active tab is the squad tab, focus is on the body, and no attach
    /// session owns the tab's container slots.
    SquadList,
}

/// Map a key event + focus context to an [`Action`].
pub fn map_key(key: KeyEvent, ctx: FocusContext, overview_hscroll_active: bool) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // Agents lose Shift-arrows only while a too-wide overview is on screen.
    // Exactly Shift: Alt/Ctrl+Shift-arrows still reach the command box / PTY.
    // Ctrl-[ / Ctrl-] cannot provide a safe pair: terminals send Ctrl-[ as ESC.
    if overview_hscroll_active
        && key.modifiers == KeyModifiers::SHIFT
        && matches!(
            ctx,
            FocusContext::CommandBox
                | FocusContext::ExecutionWindow
                | FocusContext::ContainerMaximized
        )
    {
        match key.code {
            KeyCode::Left => return Action::ScrollWorkflowOverviewLeft,
            KeyCode::Right => return Action::ScrollWorkflowOverviewRight,
            _ => {}
        }
    }

    // Global shortcuts — available in most contexts including maximized container.
    // Tab switching (Ctrl-A/D) is suppressed in Dialog context to prevent
    // dialogs from leaking across tabs (TUI-2). The yolo countdown dialog
    // is handled specially in the event loop.
    if ctrl {
        match key.code {
            KeyCode::Char('t') => return Action::OpenNewTabDialog,
            KeyCode::Char('a') if ctx != FocusContext::Dialog => return Action::PreviousTab,
            KeyCode::Char('d') if ctx != FocusContext::Dialog => return Action::NextTab,
            KeyCode::Char('m') if ctx != FocusContext::Dialog => {
                return Action::CycleContainerWindow;
            }
            // Ctrl-O (SI, 0x0f) is intercepted in every context — including
            // ContainerMaximized, before the ForwardToPty path below — so the
            // workflow overview is always one keystroke away.
            KeyCode::Char('o') if ctx != FocusContext::Dialog => {
                return Action::ToggleWorkflowOverview;
            }
            // Ctrl-Q (XON, 0x11) is intercepted in every context but a dialog,
            // before the ContainerMaximized ForwardToPty path, like Ctrl-G.
            KeyCode::Char('q') if ctx != FocusContext::Dialog => return Action::OpenQueuePanel,
            KeyCode::Char('w') => return Action::WorkflowControl,
            // Ctrl-G (BEL, 0x07) is rarely used by terminal programs, so we
            // intercept it here — before the ContainerMaximized ForwardToPty
            // path below — in every focus context so it never reaches the PTY.
            KeyCode::Char('g') => return Action::ToggleGitSidebar,
            // Ctrl-\ (FS, 0x1c) detaches from whatever container view is on
            // screen, leaving every container running (WI 0110). It is
            // intercepted in every context — like Ctrl-O and Ctrl-G — so it
            // never reaches the PTY: the whole point is a way out that does
            // *not* signal the agent the way Ctrl-C does.
            //
            // A terminal without the kitty keyboard protocol enhancement
            // reports the raw FS byte, and crossterm's legacy decoder maps
            // 0x1C..=0x1F to Ctrl+'4'..'7' (not the literal key), so Ctrl-\
            // arrives here as Ctrl+'4'. Match both encodings, or Ctrl-\ only
            // works on the minority of terminals that support kitty.
            KeyCode::Char('\\') | KeyCode::Char('4') => return Action::DetachContainers,
            _ => {}
        }
    }

    // Ctrl-C: forward to PTY when the container is maximized (so the
    // signal reaches the process inside the container); otherwise
    // trigger the close-tab / quit dialog.
    if ctrl && key.code == KeyCode::Char('c') {
        if ctx == FocusContext::ContainerMaximized {
            return Action::ForwardToPty(key);
        }
        return Action::CloseTabOrQuit;
    }
    if key.code == KeyCode::Char(',') && ctrl {
        return Action::OpenConfigShow;
    }

    match ctx {
        FocusContext::CommandBox => map_command_box_key(key, ctrl, shift),
        FocusContext::ExecutionWindow => map_execution_window_key(key, ctrl),
        FocusContext::Dialog => map_dialog_key(key, ctrl),
        FocusContext::ContainerMaximized => {
            if ctrl && key.code == KeyCode::Char('y') {
                Action::CopySelection
            } else {
                Action::ForwardToPty(key)
            }
        }
        FocusContext::SquadList => map_squad_list_key(key, ctrl),
    }
}

pub(super) fn is_workflow_context_copy_key(key: KeyEvent) -> bool {
    key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        && matches!(key.code, KeyCode::Char('c' | 'C'))
}

/// Key bindings for the squad task list (WI 0102). Reached only through
/// `FocusContext::SquadList`; the global `ctrl` block in `map_key` runs first,
/// so `Ctrl-T`/`Ctrl-A`/`Ctrl-D`/`Ctrl-M`/`Ctrl-O`/`Ctrl-W`/`Ctrl-G`/`Ctrl-C`/
/// `Ctrl-,` keep their global meaning here.
fn map_squad_list_key(key: KeyEvent, ctrl: bool) -> Action {
    match key.code {
        // WI 0112: the command box is permanently inactive on the squad tab,
        // so there is nothing for Esc to hand focus to. It is deliberately
        // unmapped rather than `FocusCommandBox`.
        KeyCode::Esc => Action::None,
        KeyCode::Up => Action::ScrollUp,
        KeyCode::Down => Action::ScrollDown,
        KeyCode::Left if !ctrl => Action::SquadMoveLeft,
        KeyCode::Right if !ctrl => Action::SquadMoveRight,
        KeyCode::PageUp => Action::ScrollPageUp,
        KeyCode::PageDown => Action::ScrollPageDown,
        KeyCode::Enter => Action::SquadShowDetail,
        KeyCode::Char('a') if !ctrl => Action::SquadAttach,
        KeyCode::Char('n') if !ctrl => Action::SquadNew,
        KeyCode::Char('e') if !ctrl => Action::SquadEdit,
        KeyCode::Char('h') if !ctrl => Action::SquadShowHistory,
        KeyCode::Char('p') if !ctrl => Action::SquadPause,
        KeyCode::Char('r') if !ctrl => Action::SquadResume,
        KeyCode::Char('t') if !ctrl => Action::SquadTrigger,
        KeyCode::Char('c') if !ctrl => Action::SquadCancel,
        KeyCode::Char('d') if !ctrl => Action::SquadDelete,
        KeyCode::Char('y') if ctrl => Action::CopySelection,
        _ => Action::None,
    }
}

fn map_command_box_key(key: KeyEvent, ctrl: bool, shift: bool) -> Action {
    match key.code {
        KeyCode::Enter if ctrl || shift => Action::InsertNewline,
        KeyCode::Enter => Action::SubmitCommand,
        // Yank an execution-window mouse selection without moving focus
        // (no-op when no selection exists).
        KeyCode::Char('y') if ctrl => Action::CopySelection,
        KeyCode::BackTab => Action::AutocompletePrev,
        KeyCode::Tab if shift => Action::AutocompletePrev,
        KeyCode::Tab => Action::AutocompleteNext,
        KeyCode::Up => Action::FocusExecutionWindow,
        KeyCode::Backspace if ctrl => Action::BackspaceWord,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::Delete,
        KeyCode::Left if ctrl => Action::CursorWordLeft,
        KeyCode::Right if ctrl => Action::CursorWordRight,
        KeyCode::Left => Action::CursorLeft,
        KeyCode::Right => Action::CursorRight,
        KeyCode::Home => Action::CursorHome,
        KeyCode::End => Action::CursorEnd,
        KeyCode::Char(c) if !ctrl => Action::Char(c),
        _ => Action::None,
    }
}

fn map_execution_window_key(key: KeyEvent, ctrl: bool) -> Action {
    match key.code {
        KeyCode::Esc => Action::FocusCommandBox,
        KeyCode::Up => Action::ScrollUp,
        KeyCode::Down => Action::ScrollDown,
        KeyCode::PageUp => Action::ScrollPageUp,
        KeyCode::PageDown => Action::ScrollPageDown,
        KeyCode::Char('b') if !ctrl => Action::ScrollToTop,
        KeyCode::Char('e') if !ctrl => Action::ScrollToBottom,
        KeyCode::Char('l') if !ctrl => Action::ToggleStatusLog,
        KeyCode::Char('y') if ctrl => Action::CopySelection,
        _ => Action::None,
    }
}

fn map_dialog_key(key: KeyEvent, ctrl: bool) -> Action {
    if key.code == KeyCode::Esc {
        return Action::DismissDialog;
    }
    match key.code {
        KeyCode::Char('n') if ctrl => Action::NewMapEntry,
        // Other Ctrl chords must not fall through as literal characters
        // (e.g. Ctrl+X inserting 'x' into an inline editor).
        KeyCode::Char(c) if !ctrl => Action::Char(c),
        KeyCode::Enter => Action::SubmitCommand,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::Delete,
        KeyCode::Home => Action::CursorHome,
        KeyCode::End => Action::CursorEnd,
        KeyCode::Up => Action::ScrollUp,
        KeyCode::Down => Action::ScrollDown,
        KeyCode::PageUp => Action::ScrollPageUp,
        KeyCode::PageDown => Action::ScrollPageDown,
        KeyCode::Left => Action::CursorLeft,
        KeyCode::Right => Action::CursorRight,
        _ => Action::None,
    }
}
