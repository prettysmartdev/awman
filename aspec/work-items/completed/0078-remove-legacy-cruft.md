# Work Item: Task

Title: Remove legacy cruft — specs new alias, implement command, and claw agent management
Issue: issuelink

## Summary:
- Remove the `specs new` alias (canonical form is `new spec`)
- Remove the `implement` command, which has been superseded by `exec workflow`
- Remove all claw agent management code (`claws` command, `engine::claws`, `data::claws_paths`, nanoclaw Dockerfile template, and associated docs)
- This removal reflects a deliberate strategic decision: amux is doubling down on its core mission — the most secure and developer-friendly way to run code agents — and will no longer attempt to manage persistent background agents

## User Stories

### User Story 1:
As a: developer using amux daily

I want to:
use a lean, focused CLI that doesn't carry dead commands

So I can:
discover the right commands quickly and avoid confusion from aliases and duplicates that add noise without adding value

### User Story 2:
As a: developer who previously used `amux implement`

I want to:
have clear guidance that `amux exec workflow` is the canonical replacement

So I can:
migrate my scripts and muscle memory without needing to dig through source code to understand what changed

### User Story 3:
As a: developer evaluating amux

I want to:
see a tool with a clear, coherent scope — secure, sandboxed code agent execution — rather than one that also tries to manage persistent background agents

So I can:
trust that amux will be excellent at what it does rather than mediocre at many things


## Implementation Details:

### 1. Remove the `specs new` alias

The alias `specs new` → `new spec` was a convenience shortcut; `new spec` is the canonical command and remains untouched.

- **`src/command/dispatch/catalogue.rs`**: Remove the `SPECS_NEW` CommandSpec definition (lines ~418–449) and the `SPECS` command entry that wraps it (lines ~408–449). Remove the path alias entry `specs new` → `new spec` (line ~257).
- **`src/command/dispatch/mod.rs`**: Remove the `["specs", "new"]` dispatch arm (lines ~313–330). Remove associated tests `alias_specs_new_dispatches_to_new_spec` and `specs_new_and_new_spec_build_commands_with_same_interview_flag` (lines ~997–1004, ~1360–1399).
- **`src/command/commands/specs.rs`**: Remove `SpecsSubcommand::New`, `SpecsNewFlags`, and `SpecsNewOutcome`. The `create_new_spec` function and its helper utilities (`next_work_item_number`, `apply_work_item_template`, `slugify`) are still used by the `new spec` command path via `NewCommand`, so they must be kept. Prune any test cases that exercise `SpecsSubcommand::New` directly (the shared `create_new_spec` tests remain via `new spec` coverage). Update the `SpecsCommand::run_with_frontend` match to remove the `New` arm.

### 2. Remove the `implement` command

`amux implement WORK_ITEM` is fully superseded by `amux exec workflow WORKFLOW_FILE`. No behavior is lost.

- **`src/command/commands/implement.rs`**: Delete the entire file.
- **`src/command/commands/mod.rs`**: Remove `pub mod implement;`.
- **`src/command/dispatch/catalogue.rs`**: Remove the `IMPLEMENT` CommandSpec (lines ~379–392).
- **`src/command/dispatch/mod.rs`**: Remove the `["implement"]` dispatch arm (lines ~293–304), the `read_implement_flags` function (lines ~772–793), and all `build_implement_*` tests (lines ~1079–1116).
- **Frontend implementations**: Remove all `implement`-specific frontend glue. Search across `src/frontend/` for any per-command module or match arm that handles `ImplementCommand` / `ImplementCommandFrontend` and delete those. Check `src/frontend/cli/`, `src/frontend/tui/`, and `src/frontend/headless/`.
- **`src/command/commands/implement_prompts.rs`** (if it exists as a standalone module): Verify whether `render_default_prompt` / `render_interview_prompt` / `render_amend_prompt` are still needed by `specs.rs` or `new spec`. If they are, keep the module; if `implement` was the only consumer of `render_default_prompt`, remove that function but leave the others.

### 3. Remove all claw agent management

Delete every file and reference related to `claws`. This is the largest change.

**Files to delete entirely:**
- `src/command/commands/claws.rs`
- `src/engine/claws/mod.rs`, `src/engine/claws/frontend.rs`, `src/engine/claws/phase.rs`, `src/engine/claws/summary.rs` — delete the entire `src/engine/claws/` directory
- `src/data/claws_paths.rs`

**Module registrations to remove:**
- `src/command/commands/mod.rs`: Remove `pub mod claws;`
- `src/engine/mod.rs` (or wherever `engine::claws` is declared): Remove the `pub mod claws;` line
- `src/data/mod.rs`: Remove `pub mod claws_paths;`
- `src/data/templates/mod.rs`: Remove the nanoclaw Dockerfile constant and its `pub fn` accessor (lines ~30+)

**Dispatch and catalogue:**
- `src/command/dispatch/catalogue.rs`: Remove `CLAWS`, `CLAWS_INIT`, `CLAWS_READY`, `CLAWS_CHAT` CommandSpec definitions (lines ~491–529)
- `src/command/dispatch/mod.rs`: Remove the `["claws", sub]` dispatch arm (lines ~356–368) and the `build_claws_init_ready_chat_succeed` test (lines ~1178–1188)

**Frontend glue:**
- Search `src/frontend/` for any match arm or per-command module handling `ClawsCommand` / claws variants. The TUI likely renders a "claws" tab in purple — find and remove it from `src/frontend/tui/tabs.rs` or equivalent.

### 4. Update documentation to reflect the strategic refocus

- **Delete `docs/06-nanoclaw.md`** entirely.
- **Update `docs/05-yolo-mode.md`**: Replace all `amux implement 0027 --yolo` examples with the `amux exec workflow` equivalent. Remove any references to `implement` as a command name.
- **Update `docs/09-remote-mode.md`**: Replace `amux remote run implement 0059` examples with the `exec workflow` form. Remove `implement` from the command table.
- **Update `docs/04-workflows.md`** (if it references `implement` as a gateway to workflow execution).
- **Update `docs/01-using-the-tui.md`** (if it shows a nanoclaw/claws tab).
- Check remaining docs for any stray `claws`, `implement`, or `specs new` references and remove or rewrite them.
- The strategic narrowing of amux scope should be reflected naturally through accurate, up-to-date user docs — not through a dedicated announcement doc.


## Edge Case Considerations:

- **`specs amend` is unaffected** — only `specs new` (alias) is removed; `specs amend` remains. The `SpecsCommand` struct, its `Amend` arm, and all amend-related types stay.
- **`new spec` continues to work** — the canonical `new spec` command is untouched. `create_new_spec`, `next_work_item_number`, `apply_work_item_template`, `slugify`, and the `SpecsCommandFrontend` trait all remain in `specs.rs` because `NewCommand` calls into them.
- **`implement_prompts` shared usage** — `render_interview_prompt` and `render_amend_prompt` are called by `specs.rs`; do not delete the module. Only remove `render_default_prompt` if it was exclusively used by the `implement` command.
- **TUI tab state** — if the TUI hard-codes a claws/nanoclaw tab index or purple tab color in its tab bar, removing it may shift the indices of other tabs. Audit `tabs.rs` and any tab-index constants to ensure the remaining tabs re-index cleanly.
- **Headless and remote mode** — `implement` is referenced in remote mode docs and examples. Verify that the headless command frontend does not have a lingering `Implement` variant in its command enum that would cause a compile error after the command is removed.
- **Existing user scripts** — users may have scripts calling `amux implement` or `amux claws`. These will break. The deprecation is intentional; no shim or error-redirect is required, but the docs update (removing examples, noting `exec workflow` as the replacement) is the user-facing mitigation.
- **`src/data/templates`** — the nanoclaw Dockerfile is embedded as a compile-time template. After removing it, confirm the templates module compiles cleanly and no other path in the codebase references the nanoclaw Dockerfile constant.


## Test Considerations:

- **Dispatch tests**: After removing the `["specs", "new"]` arm, the `["claws", ...]` arm, and the `["implement"]` arm from dispatch, run `make test` to confirm no remaining dispatch tests reference those paths.
- **Catalogue tests**: Verify the catalogue no longer advertises `specs new`, `implement`, or `claws` subcommands. If there are catalogue snapshot tests or help-text golden files, update them.
- **`SpecsCommand` unit tests**: The existing `specs_new_*` tests in `specs.rs` exercise `SpecsSubcommand::New` directly and must be removed. The `specs_amend_*` tests must be kept. The `create_new_spec` function is still exercised indirectly through `NewCommand` tests elsewhere.
- **Compile-time check**: The primary correctness gate for a deletion work item is a clean `make all`. After each deletion step, confirm the build passes before moving to the next step.
- **Frontend smoke tests**: If any integration or end-to-end tests invoke `implement` or `claws` commands against a real or mock dispatch, update or delete those tests.
- **No new tests needed**: This is a pure deletion. The goal is a green build and test suite with no references to the removed commands.


## Codebase Integration:

- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- Work through deletions one logical group at a time (alias → implement → claws) so that the build stays green between steps; this makes it easier to bisect if a compile error appears.
- After all deletions, run `grep -rn "claws\|implement\b" src/` to catch any stray references — pay attention to doc comments and `use` statements that may silently linger after the primary deletion.
- The `src/command/dispatch/catalogue.rs` file defines the help text tree; after removing commands, verify the help output (`amux --help`) still renders correctly and does not show removed commands.
- Check `src/frontend/tui/tabs.rs` and any related tab-ordering constants carefully — tab indices are likely positional, and removing a tab shifts subsequent indices.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Delete `docs/06-nanoclaw.md`** — this feature no longer exists
- **Rewrite `docs/05-yolo-mode.md`** to use `exec workflow` instead of `implement` throughout
- **Update `docs/09-remote-mode.md`** to replace `implement` examples with `exec workflow`
- **Audit all remaining docs** for `claws`, `implement`, and `specs new` and remove or correct each reference
- **Never create work-item-specific docs** — the docs changes are updates to user guides, not implementation notes
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
