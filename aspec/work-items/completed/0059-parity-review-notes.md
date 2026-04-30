# 0059 Parity Review — Minor & Trivial Issues

> These issues were deferred during the parity review pass. Critical/high/medium issues
> have already been fixed. This document is for a future review agent or developer.

---

## Minor Issues

### M1 — `flag_parser::parse_flags` receives the full `parts` slice instead of the post-command subslice

**Files:** `src/tui/mod.rs` — all three `remote` branch handler blocks.

**Detail:** The spec shows the flag parser called with `&parts[2..]` (after "remote run"),
`&parts[3..]` (after "remote session start"), etc. The implementation passes `&parts`
(the whole tokenized command, including the leading "remote" / "session" / "run" tokens).

**Why it currently works:** `flag_parser::parse_flags` silently ignores any token that does
not start with `--`, so the extra positional tokens ("remote", "run", etc.) are harmlessly
skipped. No incorrect flag values result.

**Suggested fix:** Change the three `parse_flags(&parts, …)` calls to pass the correct
subslice so the intent is explicit and the function contract is honoured:

```rust
// remote run
let flags = flag_parser::parse_flags(&parts[2..], run_spec);

// remote session start
let flags = flag_parser::parse_flags(&parts[3..], start_spec);

// remote session kill
let flags = flag_parser::parse_flags(&parts[3..], kill_spec);
```

---

### M2 — `RemoteSessionChosen` action handler does not eagerly update `last_remote_session_id`

**File:** `src/tui/mod.rs` — `Action::RemoteSessionChosen` arm (~line 684).

**Detail:** The spec says:
> `RemoteSessionChosen`: update `TabState.last_remote_session_id`, set `PendingCommand::RemoteRun`
> with the chosen session, call `launch_pending_command`.

The current implementation only updates `last_remote_session_id` inside `launch_remote_run`
(after the command starts), not in the action handler. In practice this is fine because
`launch_remote_run` runs immediately, but it means the value is not available at picker-close
time (e.g. for a second picker that might be shown before the first command starts).

**Suggested fix:** Add `app.active_tab_mut().last_remote_session_id = Some(session_id.clone());`
to the `RemoteSessionChosen` match arm before calling `launch_pending_command`.

---

### M3 — `Action::RemoteSaveDirAccepted` and `Action::RemoteSaveDirDeclined` do not carry `dir`/`remote_addr` fields

**File:** `src/tui/input.rs`

**Detail:** The spec defines these variants as:
```rust
RemoteSaveDirAccepted { dir: String, remote_addr: String },
RemoteSaveDirDeclined { dir: String, remote_addr: String },
```
The implementation uses bare unit-like variants:
```rust
RemoteSaveDirAccepted,
RemoteSaveDirDeclined,
```

The action handler recovers `dir` from `app.active_tab().pending_command`, so nothing breaks.
This is a structural inconsistency with the spec that could cause confusion when reading the
code alongside the spec.

**Suggested fix:** Either add the fields to match the spec exactly (preferred for consistency),
or add a comment in the spec that the fields were intentionally omitted and the data is
sourced from `PendingCommand`.

---

### M4 — `Enter` key in `RemoteSaveDirConfirm` acts as "decline" (spec does not define `Enter`)

**File:** `src/tui/input.rs` — `handle_remote_save_dir_confirm`.

**Detail:** The spec defines `y`, `n`, and `Esc`. `Enter` is not mentioned. The
implementation maps `Enter` → `RemoteSaveDirDeclined` (proceed with session start but don't
save). This is reasonable UX but is undocumented.

**Suggested fix:** Either document the behaviour in a code comment, or—for full consistency
with other y/n dialogs—leave `Enter` unhandled (fall through to `_ => Action::None`).

---

### M5 — Empty-list state in session pickers is handled before dialog open (not inside the dialog render)

**File:** `src/tui/mod.rs` — `fetch_and_show_session_picker` and `fetch_and_show_session_kill_picker`.

**Detail:** The spec says the picker modal should render an "empty list" message inside the
dialog itself:
> Empty list: show "No active sessions on `<addr>`. Run `remote session start` first."

The implementation instead handles the empty case by setting `tab.input_error` and never
opening the dialog. The render functions for `RemoteSessionPicker` and `RemoteSessionKillPicker`
have no empty-list rendering path.

**Effect:** The user sees an error in the command input bar rather than a modal dialog. This
is functionally equivalent but visually inconsistent with the spec.

**Suggested fix:** Either document this design choice, or add an empty-list branch to
`draw_remote_picker` and open the dialog even when the list is empty.

---

## Suggested New Tests (for future test-writing agent)

These test cases are not yet written but are needed for complete coverage of the 0059 feature.

### TUI unit tests — `extract_passthrough_command`

1. `remote run implement 0001 --yolo` → passthrough = `["implement", "0001", "--yolo"]`
   (inner command flag `--yolo` must not be stripped)
2. `remote run implement 0001 --yolo --session abc123` → passthrough = `["implement", "0001", "--yolo"]`
   (session flag AND its value `abc123` must both be stripped)
3. `remote run implement 0001 --remote-addr=http://host:9876 --yolo` →
   passthrough = `["implement", "0001", "--yolo"]`
   (`--flag=value` form stripped correctly)
4. `remote run -f implement 0001` → passthrough = `["implement", "0001"]`
   (`-f` stripped; `--follow` handled separately)
5. `remote run implement 0001 -n` → passthrough = `["implement", "0001", "-n"]`
   (inner command short flag preserved)

### TUI unit tests — `-f` short flag

1. Command `remote run implement 0001 -f` → `follow = true`.
2. Command `remote run implement 0001 --follow` → `follow = true`.
3. Command `remote run implement 0001` → `follow = false`.

### TUI unit tests — `RemoteSaveDirConfirm` Esc cancels session start

1. Esc in `RemoteSaveDirConfirm` → `dialog = Dialog::None`, `pending_command = PendingCommand::None`,
   action = `Action::None` (no session start launched).
2. `n` in `RemoteSaveDirConfirm` → dialog closed, `Action::RemoteSaveDirDeclined` returned,
   session start proceeds (pending command NOT cleared).

### TUI unit tests — session picker pre-selection

1. `fetch_and_show_session_picker` with `last_remote_session_id = Some("abc")` and sessions list
   containing an entry with `id = "abc"` at index 2 → dialog opens with `selected = 2`.
2. `fetch_and_show_session_picker` with `last_remote_session_id = Some("unknown")` →
   `selected = 0` (fallback to first).
3. `fetch_and_show_session_picker` with `last_remote_session_id = None` → `selected = 0`.

### TUI unit tests — launch guards

1. `launch_remote_run` with `session_id = ""` → `input_error` is set, no command launched.
2. `launch_remote_session_start` with `dir = ""` → `input_error` is set, no command launched.
