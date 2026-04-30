# WI-0064 Review Notes — Minor / Trivial Issues

These items were identified during the parity review pass but were not fixed
because they are cosmetic or low-risk.  A future review or polish pass should
address them.

---

## 1. No cursor indicator in `new workflow` dialog fields (minor)

**File**: `src/tui/render.rs` — `draw_new_workflow_dialog`

The workflow dialog renders field values as plain text (e.g. `> Name: my-wf`)
with no `█` cursor indicator at the current byte offset.  Compare with the
`NewTabDirectory` dialog which appends `█` when focused.  Adding a cursor
indicator would improve the editing experience, especially for long field values.

Affected fields: `Name`, `Title`, `StepName`, `StepAgent`, `StepModel`,
`StepDependsOn`, `StepPrompt`, `Summary`.

**Suggested fix**: In the `label(text, focused)` closure, when `focused == true`
insert `█` at `state.<field>_cursor` byte position inside the value string.

---

## 2. No cursor indicator in `new skill` dialog fields (minor)

**File**: `src/tui/render.rs` — `draw_new_skill_dialog`

Same issue as above.  Fields affected: `Name`, `Description`, `Body`, `Summary`.

---

## 3. `SkillField::prev()` method is interview-mode-unaware (trivial)

**File**: `src/tui/state.rs`

`SkillField::prev()` returns `Body` for `SkillField::Name`, which is incorrect
in interview mode where `Body` is never shown.  The method is **no longer called
directly** for navigation — the input handler now uses explicit context-aware
matches — but the method is still a public API surface that could mislead a
future caller.

Consider either:
- Removing `SkillField::prev()` entirely (since all callers now use explicit
  matches), or
- Renaming it `prev_non_interview()` with a doc comment clarifying it is only
  valid in non-interview mode.

---

## 4. `WorkflowField::Summary` reachable in non-interview mode via `next_step` fallback (trivial)

**File**: `src/tui/state.rs`

`WorkflowField::next_step(Summary)` and `prev_step(Summary)` now return `Name`
as a graceful recovery.  The `Summary` variant should only appear when
`interview == true`; in non-interview mode it should be unreachable.  If the
dialog state somehow ends up with `focused_field = Summary` in non-interview
mode, the `Name` fallback means the user gets silently moved to `Name` rather
than seeing an error.  This is safe but could mask a future bug.  A `debug_assert`
guarding against this condition would make bugs visible during development.

---

## 5. `new workflow` TUI command does not accept a positional name argument (trivial)

**File**: `src/tui/mod.rs` — `execute_command`, `"new"` arm, `Some(&"workflow")` branch

CLI usage: `amux new workflow` always prompts for the name interactively.
TUI usage: `new workflow` opens a dialog with an empty `Name` field.

Unlike `implement 42` (which extracts the work item number as a positional
argument), `new workflow <name>` does not pre-populate the `Name` field.
Typing `new workflow my-wf` in the TUI command box opens the dialog with
`Name: ` empty.

This is consistent with the current spec (work item 0064 does not define a
positional `name` argument for the CLI), but could be a future UX improvement:
parse the first non-flag positional token after `workflow` and pre-populate
the Name field.

---

## 6. Multi-line `StepPrompt` and `Body`/`Summary` fields are not scrollable (minor)

**File**: `src/tui/render.rs`

Long multi-line content in `StepPrompt` (workflow) and `Body`/`Summary` (skill)
is rendered with `Wrap { trim: false }` but the paragraph is not scrollable —
if the text exceeds the dialog height the earlier lines are lost off-screen.
For now the dialog is tall enough for short inputs, but a multi-paragraph
prompt would overflow.

A simple mitigation: compute the required number of visible lines and clamp the
paragraph scroll to show the region around `step_prompt_cursor`.

---

## 7. `new workflow --interview` dialog footer still shows old text on some codepaths (trivial)

**File**: `src/tui/render.rs` — `draw_new_workflow_dialog`

The interview mode footer was updated to `[Tab] next field  [Ctrl-Enter] start
interview  [Esc] cancel` as part of the parity fix.  Verify the rendered output
matches the actual key bindings (Tab now cycles Name ↔ Summary).

---

## Test gaps identified (for future test-writing agent)

The following tests should be added to close coverage gaps for WI-0064:

### TUI dialog state tests

- `WorkflowField::next_step(Name)` returns `Title` in non-interview mode.
- `WorkflowField::prev_step(Title)` returns `Name`.
- `WorkflowField::prev_step(Name)` returns `StepPrompt` (wrap-around).
- `NewWorkflowDialogState::new(...)` always starts with `focused_field = Name`.
- In interview mode, Tab from `Name` moves to `Summary`; Tab from `Summary` moves back to `Name`.
- In interview mode, BackTab from `Summary` moves to `Name`; BackTab from `Name` moves to `Summary`.
- In non-interview mode, submitting with empty name sets `error = Some("Workflow name cannot be empty")`.
- In non-interview mode, submitting with non-empty name and title but no steps sets `error = Some("At least one step is required")`.
- `handle_new_skill` Tab cycling in interview mode: `Name → Description → Summary → Name`.
- `handle_new_skill` BackTab in interview mode: `Name → Summary → Description → Name`.
- `handle_new_skill` BackTab from `Name` in interview mode goes to `Summary`, not `Body`.

### Parity / integration tests

- `amux new spec` and `amux specs new` produce identical output files.
- `amux new workflow` (non-interview) writes a parseable `.toml` file.
- `amux new workflow --format yaml` writes a `.yaml` file.
- `amux new workflow --global` writes to a temp `~/.amux/workflows/` directory.
- `amux new skill` (non-interview) writes a valid `SKILL.md` to `.claude/skills/<name>/`.
- `amux new skill --global` writes to `~/.amux/skills/<name>/SKILL.md`.
