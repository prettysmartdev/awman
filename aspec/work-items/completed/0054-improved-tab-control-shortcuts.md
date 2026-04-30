# Work Item: Enhancement

Title: Improved tab control shortcuts
Issue: issuelink

## Summary:
- Ctrl-W opens the workflow control board regardless of whether the container window is maximized or minimized
- Container window min/max toggle moves to Ctrl-M; Esc is forwarded to the container PTY when maximized; bare 'c' no longer restores the window
- Ctrl-, opens the config show dialog from anywhere in the TUI, mirroring the Cmd-, settings shortcut on macOS

## User Stories

### User Story 1:
As a: user

I want to: open the workflow control board with Ctrl-W at any time while a workflow is running

So I can: manage workflow steps without first having to minimize the container window, reducing the number of keystrokes needed

### User Story 2:
As a: user

I want to: toggle the container window between maximized and minimized using Ctrl-M, and have Esc, Tab, and Shift-Tab forwarded directly to the running agent when the window is maximized

So I can: send Esc, Tab, and Shift-Tab to interactive agents (e.g. vim, fzf, REPL prompts) without accidentally collapsing the window

### User Story 3:
As a: user

I want to: open the amux config dialog instantly with Ctrl-, from anywhere in the TUI

So I can: inspect or edit global and repo config without typing `config show`, even while an agent is running


## Implementation Details:

### Change 1 — Ctrl-W: remove maximized-window guard

In `src/tui/input.rs` the Ctrl-W branch (lines ~264–280) contains this guard:

```rust
&& tab.container_window != ContainerWindowState::Maximized
```

Remove that single condition. Because the global CONTROL key check in `handle_key` runs *before* `handle_window_key`, Ctrl-W will be intercepted and the dialog will open without the key ever reaching the PTY. The remaining guards (workflow present, current step exists, `ExecutionPhase::Running`, no other dialog) should stay unchanged.

### Change 2 — Container window: Ctrl-M toggle, Esc forwarded when maximized

**Remove Esc-minimizes when maximized** (`src/tui/input.rs`, `handle_window_key`, lines ~300–303):

```rust
// DELETE this block:
if key.code == KeyCode::Esc {
    tab.container_window = ContainerWindowState::Minimized;
    tab.clear_terminal_selection();
    return Action::None;
}
```

After removal, Esc falls through to the `key_to_bytes` forward path and is sent to the PTY as `\x1b`, which is the correct byte for interactive terminal applications.

**Remove bare 'c' restores from minimized** (`src/tui/input.rs`, `handle_window_key` minimized branch, line ~322):

```rust
// DELETE this arm:
KeyCode::Char('c') => {
    tab.container_window = ContainerWindowState::Maximized;
    return Action::None;
}
```

**Remove bare 'c' restores from command box** (`src/tui/input.rs`, `handle_input_key`, lines ~427–432):

```rust
// DELETE this block:
if key.code == KeyCode::Char('c')
    && tab.container_window == ContainerWindowState::Minimized
{
    tab.container_window = ContainerWindowState::Maximized;
    tab.focus = Focus::ExecutionWindow;
}
```
- Also ensure the hint text rendered above the command text box is updated to include Ctrl-W tip during workflows, Ctrl-M tip while container is running, and no longer mentions Esc or c, or 'Esc to minimize then Ctrl-W for workflow controls` since that's no longer needed.

**Add Ctrl-M toggle** in the global CONTROL key check block in `handle_key` (lines ~259–283), alongside the existing Ctrl-T/A/D/W cases:

```rust
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
```

Placing this in the global block (before `handle_window_key` / `handle_input_key` dispatch) means it works from any focus state and, critically, intercepts Ctrl-M before it can be forwarded to the PTY. Note: Ctrl-M is `\r` in many terminals; intercepting at this level is safe because the byte never reaches `key_to_bytes` when we return early.

### Change 3 — Ctrl-,: open config show from anywhere

Add a new arm in the global CONTROL key check block in `handle_key`, after the existing cases:

```rust
KeyCode::Char(',') => {
    // Open config dialog from anywhere; if already open, close it (toggle).
    let tab = app.active_tab_mut();
    if matches!(tab.dialog, Dialog::ConfigShow(_)) {
        tab.dialog = Dialog::None;
    } else if tab.dialog == Dialog::None {
        tab.dialog = Dialog::ConfigShow(ConfigDialogState {
            selected_row: 0,
            selected_col: 0,
            edit_mode: false,
            edit_value: String::new(),
            edit_cursor: 0,
            git_root: app.git_root.clone(),
            global_config: app.global_config.clone(),
            repo_config: app.repo_config.clone(),
            error_msg: None,
        });
    }
    return Action::None;
}
```

This intercepts before dialog routing and before `handle_window_key`, so it works when the container window is maximized, minimized, or absent, and in any focus state. The toggle-close behavior (when `ConfigShow` is already the active dialog) avoids the need to add a second Esc pathway.

If the `ConfigDialogState` initialization requires loading config from disk (as `config show` does in `mod.rs`), mirror that same loading logic here, or factor it into a shared helper used by both paths.

### Update help/keybindings display

If the TUI renders a keybindings strip or help overlay, update it to reflect:
- `Ctrl-M` — toggle container window
- `Ctrl-W` — workflow control (always, not conditional on window state)
- `Ctrl-,` — config
- Remove any mention of `Esc` minimizing or bare `c` restoring the window


## Edge Case Considerations:

- **Ctrl-M as carriage return**: `\r` is produced by Enter on some terminals and by Ctrl-M. Intercepting it in the amux key handler before PTY forwarding is safe, but document the trade-off: any running agent cannot receive Ctrl-M as a raw byte sequence. In practice, agents use Enter (which produces `\r\n` or just `\n`), so this should not cause problems.
- **Ctrl-W with workflow dialogs already open**: The existing guard `tab.dialog == Dialog::None` prevents double-opening. Confirm that the guard still holds after removing only the maximized-window guard.
- **Ctrl-, when a blocking dialog is active**: The new Ctrl-, arm sits inside the `Dialog::None` branch of the guard (`tab.dialog == Dialog::None`), so it will not fire while, e.g., a `QuitConfirm` dialog is shown. This is intentional — all blocking dialogs remain exclusive.
- **Ctrl-, toggle when ConfigShow is active**: The dialog intercept at the top of `handle_key` sends all keys to `handle_config_show` when `Dialog::ConfigShow` is active. Adding the toggle check *before* that intercept (at the top of `handle_key`, before the `match dialog` block) allows Ctrl-, to close the dialog cleanly. Alternatively, handling it within `handle_config_show` as an Esc-equivalent is simpler but less consistent.
- **Esc and Tab and Shift-tab forwarded to PTY when maximized**: Agents that use Esc to cancel prompts (e.g. vim insert mode, fzf) will work correctly. However, users who previously relied on Esc to minimize the window must learn Ctrl-M. Update the help strip and docs accordingly.
- **Bare 'c' removal**: Users currently type 'c' from the minimized window or command box to restore the container. After this change, those keypresses either insert 'c' into the input field or do nothing. Ensure the help strip is updated so users discover Ctrl-M.
- **ConfigDialogState initialization**: `ConfigDialogState` includes live config values (`global_config`, `repo_config`, `git_root`). These must be populated at dialog-open time from the current `App` state, exactly as the `config show` command path does in `mod.rs`. Stale or default values would show incorrect config.
- **Hidden container window and Ctrl-M**: When `container_window == Hidden` (no container ever launched on the tab), Ctrl-M should be a no-op. The match arm above handles this explicitly.


## Test Considerations:

- **Unit — Ctrl-W when maximized**: Construct an `App` with `container_window = Maximized`, a running workflow, current step set, and `ExecutionPhase::Running`. Synthesize a Ctrl-W key event and call `handle_key`. Assert that `app.active_tab().dialog` is `Dialog::WorkflowControlBoard`.
- **Unit — Ctrl-W when minimized**: Same setup with `Minimized`. Assert the dialog opens identically.
- **Unit — Ctrl-W with no workflow**: Assert the dialog does *not* open (no workflow, no current step).
- **Unit — Ctrl-M maximized → minimized**: Container window `Maximized`, call `handle_key` with Ctrl-M. Assert `container_window == Minimized` and terminal selection cleared.
- **Unit — Ctrl-M minimized → maximized**: Container window `Minimized`, call `handle_key` with Ctrl-M. Assert `container_window == Maximized` and `focus == ExecutionWindow`.
- **Unit — Ctrl-M hidden is no-op**: Container window `Hidden`, assert state unchanged after Ctrl-M.
- **Unit — Esc forwarded to PTY when maximized**: Container window `Maximized`, `ExecutionPhase::Running`. Synthesize Esc key. Assert the returned `Action` is `Action::ForwardToPty(b"\x1b".to_vec())` (or equivalent), not `Action::None` with a state mutation.
- **Unit — bare 'c' does not restore**: Container window `Minimized`, focus `ExecutionWindow`. Synthesize bare 'c'. Assert `container_window` remains `Minimized`.
- **Unit — Ctrl-, opens ConfigShow when idle**: No dialog, idle tab. Synthesize Ctrl-,. Assert `dialog == Dialog::ConfigShow(_)`.
- **Unit — Ctrl-, opens ConfigShow when container maximized**: Container window `Maximized`, running workflow. Synthesize Ctrl-,. Assert `dialog == Dialog::ConfigShow(_)`.
- **Unit — Ctrl-, toggles off**: Dialog already `ConfigShow`. Synthesize Ctrl-,. Assert `dialog == Dialog::None`.
- **Unit — Ctrl-, no-op when other dialog active**: Dialog is `QuitConfirm`. Synthesize Ctrl-,. Assert dialog remains `QuitConfirm`.
- **Integration — Ctrl-C unchanged**: Verify Ctrl-C still triggers `CloseTabConfirm` or `QuitConfirm` from command box focus, and still forwards to PTY (or cancels `status --watch`) from execution window focus, after all changes.


## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- All key handling changes belong in `src/tui/input.rs`; do not touch `mod.rs` unless the `config show` initialization logic needs to be extracted into a shared helper.
- The global CONTROL key check block in `handle_key` (lines ~259–283) is the correct insertion point for Ctrl-M and Ctrl-,; this block runs after dialog routing for non-None dialogs, but before focus dispatch, so new entries here are automatically "global" within the no-active-dialog context.
- If a help/keybindings strip is rendered in `src/tui/render.rs`, update it in the same PR so the UI stays self-documenting.
- Follow the existing pattern for toggle-style shortcuts: check the current state in `app.active_tab()`, mutate via `app.active_tab_mut()`, return `Action::None`.
