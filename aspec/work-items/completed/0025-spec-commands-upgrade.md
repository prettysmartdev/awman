# Work Item: Feature

Title: spec commands upgrade
Issue: issuelink

## Summary:
- move spec related commands under `specs` subcommand, add `interview` mode to `specs new`, add `edit spec --amend`.

## User Stories

### User Story 1:
As a: user

I want to:
run `specs` related commands to interact with the `aspec`

So I can:
work with the aspec more easily.


## Implementation Details:
- add a new subcommand group, `amux specs`, move `new` to be `amux specs new`.
- add a new flag on `amux specs new`, `--interview`:
	- when `--interview` is passed to `amux specs new`, after the user inputs the work item type and title, create the file but do not open it in vscode. Instead, tell the user their code agent will assist with creating the work item and prompt the user to enter a few-sentence long summary of the work item they'd like to create. (via normal input for CLI, and via a fairly large "freeform textbox dialog" in the TUI)
	- after the user submits their short summary, launch a code agent container (FOREGROUND, INTERACTIVE, PROMPTED) and provide the following prompt: 

"Work item {number} template has been created for {work item type}: {work item title}. Help complete the work item based on the following summary, making sure to include 1-3 concise user stories, detailed implementation plan, edge case considerations, test plan, and codebase integration tips. Only edit the work item markdown file, follow the template format. Do not edit any other files. Do not summarize your work at the end, let the user view the file themselves.

Summary:
{user summary}
"

- the code agent container should be launched identically to `implement`, but with the above prompt (filled in with the correct variables from the user's input)
- once the agent container quits, proceed to open the work item file in vscode as before.

- add `Enhancement` to the list of work item types that can be chosen

- add a net-new subcommand, `amux edit spec <NNNN> --amend`, which uses a code agent to review the final implementation of a work item and ensure the work item remains up to date.
	- the `amend` subcommand launches a code agent container, similar to `implement`, with the following prompt:

"Work item {number} is complete. Review the work that has been done in the codebase and compare it against the work item markdown file. If needed, amend the work item to ensure it matches the final implementation, ensuring completeness and correctness. Only edit the work item markdown file. Be concise and prefer leaving existing text as-is unless it is factually incorrect. Add new details if needed. Summarize the implementation and any corrections or changes that were needed to achieve the desired result in a new `Agent implementation notes` section at the bottom of the file."


## Edge Case Considerations:
- ensure parity between CLI and TUI
- ensure the text box is large enough to display a few sentences of text, show a blinking cursor, and allow the cursor to move via keyboard arrows (including scrolling if text overflows).

## Test Considerations:
- Ensure the behaviour of the code agent container for `--interview` and `amend` closely matches `implement` but with their respective correct prompts.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- re-use modular container launching functions, and ensure TUI behaviour of `--interview` and `amend` closely mirrors that of `implement`
- implement the large freeform text entry box as a modular component that could be re-used.

## Agent implementation notes

### Summary

All three features were implemented as specified:

1. **`amux specs` subcommand group** — `src/cli.rs` defines `Command::Specs { action: SpecsAction }` with `SpecsAction::New { interview }` and `SpecsAction::Amend { work_item, non_interactive, allow_docker }`. The old top-level `new` command was not removed (it was not present as a top-level command in the final codebase; `specs new` is the canonical entry point).

2. **`specs new --interview`** — Implemented in `src/commands/specs.rs` (`run_new_with_sink`). Creates the file first via `create_file_return_number` (extracted from `new.rs`), then prompts for a summary, then launches the agent container with `INTERVIEW_PROMPT_TEMPLATE`. After the agent exits, opens the file in VS Code if applicable.

3. **`edit spec <NNNN> --amend`** — Implemented in `src/commands/specs.rs` (`run_amend` / `run_with_sink_amend`). Launches an agent container with `AMEND_PROMPT_TEMPLATE`, supporting `--non-interactive` and `--allow-docker` flags, matching `implement` behaviour.

4. **`Enhancement` work item type** — Added to `WorkItemKind` enum in `src/commands/new.rs`, with `from_str` shortcuts `"enhancement"`, `"e"`, and `"4"`.

### TUI parity

The TUI (`src/tui/state.rs`, `input.rs`, `render.rs`, `mod.rs`) was updated to support the full `specs new` flow including interview mode:
- `Dialog::NewKindSelect { interview }` and `Dialog::NewTitleInput { ..., interview }` carry the interview flag through the dialog chain.
- `Dialog::NewInterviewSummary { kind, title, work_item_number, summary, cursor_pos }` provides the large freeform text box with cursor tracking.
- `draw_interview_summary_dialog` in `render.rs` renders this as a dedicated full-width dialog (not the generic popup renderer).
- Arrow keys and scrolling for the summary text box are handled in `handle_new_interview_summary` in `input.rs`.
- The `NewInterviewSummarySubmitted` action triggers the agent container launch via `run_new_with_sink` with the summary pre-supplied.

### Implementation notes / corrections

- The prompt templates in `src/commands/specs.rs` match the wording specified in the work item exactly.
- Agent entrypoint helpers (`interview_agent_entrypoint`, `amend_agent_entrypoint`, and their `_non_interactive` variants) cover `claude`, `codex`, and `opencode` agents, consistent with `implement`.
- `create_file_return_number` was added to `src/commands/new.rs` as a shared helper (creates the file and returns the number without opening VS Code), satisfying the modularity requirement.
- The summary text box cursor is tracked as a byte offset (`cursor_pos: usize`) in dialog state; left/right arrow keys move it, and the rendered widget shows a blinking cursor via the `Modifier::SLOW_BLINK` style.
- Comprehensive unit tests were added in `src/commands/specs.rs` (prompt builder and agent entrypoint tests) and `src/commands/new.rs` (Enhancement variant and `create_file_return_number`).
