# Mouse and TUI Agents

When running interactive agents in awman's TUI, the container window provides a full terminal emulator that supports mouse input. This guide explains how mouse events are handled and how they interact with agent applications.

---

## Overview

The container window can receive mouse events in three forms:
- **Scroll events** — forwarded conditionally to the agent or used for awman's scrollback
- **Click and drag events** — always perform text selection under awman's control
- **Mouse movement** — agents that enable button-motion tracking receive position updates

The key principle: **agents can handle scroll events if they request mouse tracking, but text selection is always under awman's control**.

---

## Agent mouse tracking

When an agent enables mouse tracking via escape sequences (e.g., `CSI ? 1000 h` for X10 button-press mode), awman detects this and begins forwarding scroll events to the agent.

Modern terminal applications (tmux, less, vim plugins, custom TUIs) often enable mouse tracking to support scrolling within panes, file listings, or code diffs. When running these inside awman, you can interact with them naturally using your mouse.

---

## Scroll forwarding

When an agent has mouse tracking enabled and you are viewing live output (not scrolled back in history), mouse scroll events are forwarded to the agent as encoded mouse escape sequences. The agent receives these events and can respond — scrolling a file listing, paging through output, or updating a scrollable panel.

### When scrolling goes to the agent

All of these must be true:
1. Agent has enabled mouse tracking
2. You are at the live output (scrolled to bottom)
3. **Shift is not being held**

If any condition is false, awman handles the scroll for its own scrollback.

### Shift+Scroll escape hatch

Hold **Shift** while scrolling to force awman to handle the scroll, even when the agent has mouse tracking enabled. This is your manual escape hatch when you want to review earlier output without losing the ability to interact with a mouse-active agent TUI.

### Scrollback entry and exit

When an agent has mouse tracking enabled, scrolling is typically forwards to the agent. To review earlier output:

1. **Scroll up** — move away from the bottom of the container window
2. awman immediately switches to **scrollback mode** — all scroll events now control historical view
3. While in scrollback, press **Page Up**, **Page Down**, or continue scrolling to navigate history
4. **Scroll down to the bottom** or press **e** (end) to return to live view
5. Scrolling automatically returns to agent forwarding

The title bar shows `↑ scrollback (N / M lines)` while you are in scrollback mode, making it clear you are reviewing history rather than live output.

---

## Text selection

Mouse click, drag, and release events always perform text selection under awman's control. These events are **never forwarded** to the agent, even if the agent has mouse tracking enabled.

**Why:** Text selection is a fundamental interaction for reviewing and copying output from the terminal. By keeping it under awman's control, we ensure:
- You can always select text from any part of the container window
- Selection does not interfere with agent-side mouse handling
- The copy-to-clipboard flow (select + **Ctrl+Y**) works reliably

### Selecting text

1. **Click and drag** to highlight text (shown with inverted colors)
2. **Ctrl+Y** to copy the selection to clipboard (ANSI color codes are stripped)

**Note:** If you have an active selection and press **Ctrl+Y**, the selection is copied. If you have no selection and press **Ctrl+Y**, the key is forwarded to the agent instead.

---

## Coordinate translation

awman translates mouse coordinates from the terminal window to the agent's PTY grid. This means:
- Scroll events that land on the container's border are discarded
- Scroll events are reported to the agent relative to the container's inner area
- Agents see coordinates as if they are running in a real terminal

No manual translation is needed — awman handles this internally.

---

## Common agent scenarios

### Scrollable file listing (less, more)

When using `less` or `more` inside the agent:
- Scroll up/down to navigate the file
- Shift+Scroll to jump to awman's earlier output
- Text selection works normally — click and drag to select, **Ctrl+Y** to copy

### Vim with plugins

Some vim plugins enable mouse support. When enabled:
- Scroll to navigate within splits or panes
- Click to place the cursor
- Text selection in the terminal (outside vim) still works with click-drag

**Note:** Vim's own mouse mode (`set mouse=a`) operates on vim's internal coordinate system, separate from terminal-level mouse events. Both can coexist.

### TUI code assistants

Code assistants running as TUIs (e.g., Claude Code in interactive mode) may enable mouse tracking to support:
- Scrolling in code panels or diffs
- Clicking to select suggestions or options
- Reviewing earlier output while the TUI is live

Scroll to interact with the agent's UI, Shift+Scroll to return to awman's history, text selection works as always.

### tmux and GNU Screen

When running tmux or Screen inside awman:
- These tools have their own mouse handling and scrollback modes
- awman's scroll forwarding works with them seamlessly
- You can mix awman's Shift+Scroll with tmux's copy-mode (C-b [ for tmux)

---

## Agent detection and real-time updates

awman continuously tracks the agent's mouse protocol mode. This means:
- If an agent **enables** mouse tracking mid-session, forwarding activates immediately
- If an agent **disables** mouse tracking, awman resumes owning all scroll events
- Mouse protocol mode changes are detected in real time — no restart needed

For example, running `vim` inside the agent (which typically enables mouse support on startup) automatically enables scroll forwarding for that vim session.

---

## Troubleshooting

### "My mouse scroll doesn't seem to be working in the agent"

**Check:** Is the agent's application actually using mouse tracking? Not all TUIs enable it by default.
- Some require a flag or configuration (e.g., `less -S` for horizontal scrolling)
- Some only enable it in specific contexts (e.g., vim inside tmux)
- Some don't use mouse tracking at all (e.g., cli cat, echo, traditional shell commands)

**Verify:** While the agent is running, check awman's console or logs to confirm the mouse protocol is active. You can also try Shift+Scroll to confirm awman's scrollback still works — if it does, awman is correctly distinguishing between agent and non-agent scroll events.

### "I accidentally scrolled away from the agent's live view"

Scroll back down or press **e** to return to the end of the scrollback. Once you reach the bottom, scroll forwarding resumes and your next scroll goes to the agent.

### "I want to always control scrolling with awman, regardless of what the agent does"

There is no global toggle for this, by design — agents that enable mouse tracking expect to receive scroll events. However, you can **always use Shift+Scroll** to override agent mouse tracking and control awman's scrollback instead.

If you frequently interact with agents whose mouse support interferes with your workflow, consider discussing this with the agent's maintainers or disabling mouse support in your terminal configuration.

### "Text selection seems inconsistent"

Text selection (click and drag) is always under awman's control. If you are trying to select text inside the agent's TUI and it's not working, the agent may have its own selection mode (e.g., tmux copy-mode, vim visual mode). Use the agent's native selection mechanism instead of the terminal mouse selection.

---

## See also

- [Using the TUI](02-using-the-tui.md) — keyboard shortcuts and container window controls
- [Agent Sessions](03-agent-sessions.md) — running agents interactively

---

[← Context Overlays](14-context-overlays.md) · [Architecture Overview →](12-architecture-overview.md)
