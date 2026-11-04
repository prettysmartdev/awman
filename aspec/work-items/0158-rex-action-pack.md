# Work Item: Feature

Title: Rex action pack and the generic `awman integration` command
Issue: issuelink

Depends on: 0159 (config and API naming, route tree, client-side `awman daemon api` actions), 0153 (Rex integration), 0154 (`engine` instances, annotations, integration registry), 0155 (frontend protocol, hand-off).

## Summary:
- Rex lets users define custom actions in Lua (`rex.action{…}` in `init.lua`). Actions appear in Rex's command palette, can be bound to keys, and can run programs with `rex.exec`.
- awman can generate a small **action pack** that puts common awman operations in Rex's palette:
  - new awman tab;
  - attach to an agent;
  - open the workflow overview;
  - approve the current workflow step;
  - show status.
- **No Rex-specific commands or flags.** Installing the pack uses a new **generic** integration lifecycle command:
  - `awman integration status` (no name) lists the compiled-in integrations with their detection and setup state, and `awman integration status <name>` shows one;
  - `awman integration setup <name>` and `awman integration remove <name>` install or remove an integration's host-side files.
  
  There are no other verbs and no per-integration commands.
  
  Each integration in 0154's registry may implement `setup`/`remove`. Rex is the first to do so: `awman integration setup rex` writes the pack under `~/.awman/integrations/rex/`.
- Every action is just an **awman CLI invocation**, so it goes through the same daemon `engine` and the same checks as typing the command. The pack holds no logic, secrets or free-text channels.
- Actions find "the awman instance for this window" through the generic annotation filter, `awman daemon api instances.list annotation.rex/block=$REX_BLOCK --json`, where the block ID is the focused block's (`$REX_BLOCK` when run from that block) (0154).
- This item adds two client-side `awman daemon api` compositions (0159 §6), `steps.approve` and `overview.open`, over the engine's `dialogs` and `ui_requests` routes. No new subcommands and no server routes are added.
- awman **never edits `init.lua`**. `setup` writes the pack file and prints the one line the user adds.

## User Stories

### User Story 1:
As a: Rex user

I want to:
run `awman integration setup rex` once, then open Rex's command palette and pick "awman: new tab", "awman: attach agent" or "awman: workflow overview"

So I can:
drive awman from Rex's own UI and key bindings.

### User Story 2:
As a: user watching a workflow from another Rex window, or from Rex on my phone

I want to:
run "awman: approve step" for the step that's waiting

So I can:
keep the workflow moving without switching to the awman window.

### User Story 3:
As a: security-conscious user

I want to:
know the pack only ever runs the awman CLI, with typed arguments, against my own awman instances, and that awman has no integration-specific control surface

So I can:
install it without opening a new control channel into awman.


## Implementation Details:

### 1. `awman integration <status|setup|remove> [<name>]` (generic, engine-free, runs locally)
- **Registry-driven.** The command iterates 0154's integration registry (`src/engine/integrations/mod.rs`). Each entry declares:
  - `name()`;
  - `detect()`, used by `status`;
  - optional `setup(opts) -> SetupReport`;
  - optional `remove(opts) -> RemoveReport`;
  - optional `setup_status() -> SetupState`.
  
  An integration without `setup` reports "nothing to set up". An unknown name is an error that lists the known integrations.
- **`status [<name>] [--json]`:** with no name, one row per compiled-in integration (name, a one-line description, whether it has setup, its `tui.integrations.<name>.enabled` value, detection and setup state). With a name, the full detail:
  - the detection result in the current terminal (`present`, `absent`, `unavailable: <reason>` or `disabled`);
  - the setup state (`installed`, `out of date`, `edited`, `not installed`);
  - integration-specific hints. For Rex, that's whether Rex has loaded the pack's actions (`rex actions`, through `host_cli::program("rex")`), plus the `dofile` line again if they aren't loaded.
- **`setup <name> [--force]` and `remove <name> [--force]`:** they write or remove files only under `~/.awman/integrations/<name>/`, and never touch the host program's own configuration.
- These are catalogue entries under a new `integration` group: CLI-only, not API-allowed, and not TUI command-box commands.

### 2. Rex's `setup` / `remove`
- **`setup rex`:**
  - Writes `~/.awman/integrations/rex/awman.lua` (0644) from a template embedded in the binary. The template is versioned, with a header comment carrying the awman version, the absolute awman binary path recorded at setup, and a content hash.
  - Prints `dofile(os.getenv("HOME") .. "/.awman/integrations/rex/awman.lua")` with instructions to add it to `init.lua`, then `rex config reload` (`server.config_reload`).
  - Refuses to overwrite a pack the user edited (the hash differs) unless `--force` is given.
  - **Never** writes to `init.lua` or any other Rex file.
- **`remove rex`:** deletes the pack (only if it is awman's, or with `--force`) and reminds the user to remove the `dofile` line.

### 3. The generated Lua pack
- Actions, each `rex.action{ name = "awman_…", title = "awman: …", category = "awman", run = function(ctx) … end }`:

  | Action | Runs |
  |---|---|
  | `awman_new_tab` | A new Rex window with `awman` in the focused block's cwd. Typing `awman` there starts a new TUI instance (0153 parity). |
  | `awman_attach_agent` | `awman agents list --json` → a Rex picker over the results → a split running `awman agents attach <id>`. |
  | `awman_workflow_overview` | `awman daemon api overview.open instance=<ins_id>` for the awman instance owning the focused Rex window. |
  | `awman_approve_step` | `awman daemon api steps.approve instance=<ins_id>` for the step waiting in that instance, after a Rex confirm prompt that shows the step name (read from `dialogs.list`). |
  | `awman_status` | A floating layer running `awman agents status --watch`. |
- **Finding the instance:** an action resolves "the awman instance for this window" with the generic annotation filter, `awman daemon api instances.list annotation.rex/block=$REX_BLOCK --json` (the focused block's ID from Rex's `ctx`, passed as the value).
  - 0153's presenter records `rex/window` on tabs, and frontends carry `rex/block` in their `Hello.annotations`, so the engine answers from annotations it already holds. No Rex-specific lookup exists.
  - If there is no awman instance, the action shows a Rex notification ("No awman instance in this window") and does nothing.
- **Argument hygiene:**
  - Every call uses `rex.exec{ argv… }` with an argv list and no shell (`io.popen`/`os.execute` are unavailable in Rex Lua anyway).
  - Arguments are only IDs taken from awman's own JSON output, validated against awman's ID grammar inside the pack before use, plus annotation values taken from Rex's own `ctx` block ID.
  - No user-typed text is ever passed.

### 4. The actions the pack uses (client-side, 0159 §6)
- The server has no action registry and no `actions` route. The pack calls `awman daemon api` actions, which are rows in the **client-side** action table (`src/command/commands/daemon/actions.rs`). Each maps to the 0159 routes, and `awman daemon api --list` shows the mapping:

  | Action | Route(s) |
  |---|---|
  | `instances.list annotation.rex/block=<id>` | `GET /v1/engine/instances?annotation=rex/block=<id>` (0154) |
  | `dialogs.list instance=<id>` / `dialogs.answer instance=<id> dialog=<id> response=…` | `GET /v1/engine/instances/{id}/dialogs` / `POST /v1/engine/instances/{id}/dialogs/{id}/answer` |
  | `ui.request instance=<id> request=open_overview` | `POST /v1/engine/instances/{id}/ui_requests` |
  | **`steps.approve instance=<id>`** (added by this item) | A composition: `dialogs.list` filtered to the one pending dialog of kind `workflow_step_confirm`, then `dialogs.answer response=yes` |
  | **`overview.open instance=<id>`** (added by this item) | A composition: `ui.request request=open_overview` |

  The compositions are documented in `--list`, so `awman daemon api --raw` users can reproduce them exactly.
- **Bodies are typed.** `dialogs.answer` sends `{response}` matching the dialog's kind. `ui_requests` takes the closed `UiRequest` enum in snake_case (0155). Unknown values → `400 bad_request`. There is no free-text or argv field.
- **Approve-step semantics:**
  - Approving answers the instance's pending step-confirm dialog (`WorkflowStepConfirm` / the control board's "advance") **exactly as if the user pressed the key in the TUI**. It uses the same `DialogResponse` path (0155), so the first answer wins and the other frontends get `DialogClosed`.
  - If no step-confirm dialog is pending, `steps.approve` exits nonzero with "step is not waiting" and sends nothing. An answer to a dialog that has just closed gets `409 dialog_closed`.
- `open_overview` asks the instance's primary frontend to open the workflow overview, the same as Ctrl-O. With no frontend connected, the result is `409 no_frontend`.
- **Ownership:** these routes are on the daemon's **local listener** and accept only the daemon's own user (peer UID on the Unix socket, or the owner key). They are never served on the network listener unless `engine` is persistently listed in `daemon.network.features` (0154).

## Edge Case Considerations:
- **The pack is loaded but awman isn't on Rex's `PATH`** (a Rex server started without the user's shell `PATH`): the pack uses the absolute awman path recorded at setup time. `integration status rex` reports a mismatch if that binary is gone.
- **An older pack after an awman upgrade:** `status` reports "out of date". Actions still work as long as the `awman daemon api` action names are unchanged, because the template is versioned and the action table is stable.
- **Several instances in one Rex session:** actions always resolve the instance from the focused window's block annotation, never "the latest instance".
- **Approve-step races** (the user presses the key in the TUI while the action runs): the first answer wins and the other gets `409`/`DialogClosed`. Both outcomes are reported.
- **The engine is off** (no awman frontend running): the actions exit nonzero with "no running awman daemon". They never start the daemon.
- **The user edits the pack:** respected. `setup` refuses to overwrite it without `--force`.
- **`setup` for an integration that has nothing to set up:** prints "nothing to set up" and exits 0.

## Test Considerations:
- **Unit:**
  - registry-driven `status` (with no name it lists all; with a name, one), including an unknown name listing the known integrations;
  - Rex template rendering (the version header, binary path and hash) and edit detection;
  - `setup rex`, `status rex` and `remove rex` file behaviour in a temp `$HOME`, with no writes outside `~/.awman/integrations/rex/`;
  - argv building in the pack, by snapshotting the generated Lua;
  - the client-side action table: `steps.approve` and `overview.open` issue exactly their documented `--raw` requests; an unknown `ui_requests` value → 400 and extra fields are rejected.
- **Integration** (daemon core + in-process frontends):
  - `steps.approve` answers a pending `WorkflowStepConfirm` identically to the key path, with the same events and outcome;
  - no pending step-confirm dialog → nonzero exit with nothing sent; answering an already-closed dialog → `409 dialog_closed`;
  - a wrong peer UID or key → refused;
  - the `/v1/engine/*` routes are absent on the network listener;
  - `awman daemon api instances.list annotation.rex/block=<id>` returns exactly the instance whose frontend carries that annotation.
- **Lua:** run the generated pack under a Lua 5.1 interpreter with a stub `rex` table. Assert the exact `rex.exec` argv for each action, and that no string concatenation of user input occurs.
- **End-to-end** (gated `AWMAN_TEST_REX=1`): `awman integration setup rex`, add the `dofile` line, `rex config reload`, `rex actions` lists the awman actions, and `rex do awman_status` opens the status layer.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- The `integration` command (one command with three verbs) follows the `Command` trait pattern (`src/command/commands/command_trait.rs`), registered in `src/command/dispatch/catalogue/` so the clap, markdown and TUI-hint projections pick them up. The two compositions are rows in `awman daemon api`'s client-side action table (`src/command/commands/daemon/actions.rs`), not commands or server routes. No catalogue entry is named after an integration.
- Rex's `setup`/`remove` live in `src/engine/integrations/rex/setup.rs`, behind the registry entry.
- The Lua template is an embedded asset under `src/assets/integrations/rex/` (as the other templates are), rendered by pure code in the Rex integration module.
- The actions route is part of the `engine` feature's router (0154). It reuses 0155's `DialogResponse` path and doesn't add a second way to answer dialogs.
- Rex calls (`rex actions`, `server.config_reload`) go through `host_cli::program("rex")` and 0153's `RexApi`.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.

Specifically for this item:
- The Rex guide (`docs/18-rex-integration.md`, from 0153): an "awman in Rex's command palette" section covering `awman integration setup rex`, the `dofile` line, the action list, and approve-step behaviour.
- `docs/19-engine-and-frontends.md` (from 0154/0155): `awman integration <status|setup|remove>` and the `annotation.<k>=<v>` filter on `awman daemon api instances.list`.
- `docs/14-command-reference.md`: `awman integration <status|setup|remove> [<name>]`, and the `steps.approve` and `overview.open` actions of `awman daemon api` (with their route compositions).
