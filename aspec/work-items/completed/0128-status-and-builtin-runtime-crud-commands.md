# Work Item: Enhancement

Title: status and builtin runtime CRUD commands
Issue: issuelink

## Summary:
- This work item aims to enhance the current `awman status` command and add new commands needed to analyse and manage agents running under the builtin-experimental runtime.

1) Add a new top-level subcommand to the catalog: `awman agents` which houses a number of agent-related subcommands: `awman agents status` (replaces the current status top-level, which should be completely removed, no soft migration), `awman agents list`, `awman agents detail <id>`, `awman agents kill <id>`, `awman agents attach <id>`

2) The newly replaced `awman agents status` should be re-written from the ground up. The current command has bad formatting, weird behaviour on the CLI (outputting a bunch of extra garbage when quit), and it defaults to `--watch` which should be an optional flag, not the default. Make its output more informative, include more information, ensure it works seamlessly within the TUI, and generally make it a better user experience.

3) Implement the new `agents` subcommands such that they would feel relatively familiar to a docker, containers, or kubectl user. Decide and verify with me wether it makes sense to enable all of the new subcommands for all agent runtimes, or wether they should be restricted to only the builtin runtime. 

4) `awman agents list` should also have aliases `ps` and `ls` to help with people's muscle memory and `awman agents kill` should also alias to `rm`

**Decision (confirmed with the maintainer):** every `awman agents` subcommand is enabled on **all** agent runtimes (`docker`, `apple-containers`, `docker-sbx-experimental`, `builtin-experimental`). Each subcommand is written once against the cross-runtime `AgentRuntimeEngine` trait, which all four runtimes already implement (`list_running_all`, `stats`, `stop`, `attach`, `remove_agent`). Runtime-specific information, such as builtin VM sizing, network mode and owner process, is optional extra detail. When a runtime cannot do something, the command reports a typed "unsupported on this runtime" error rather than gating the whole command. Restricting the commands to builtin would leave Docker, Apple and SBX users with no status command, because the top-level `status` is being removed.


## User Stories

### User Story 1:
As a: user

I want to:
run `awman agents ls` (or `ps` / `list`) for a one-shot, docker-style table of every running agent, then `awman agents detail <id>` to inspect one of them

So I can:
quickly see what is running, which repo/tab/squad task it belongs to, and how much CPU and memory it uses, with the same commands on every runtime

### User Story 2:
As a: user running agents under the `builtin-experimental` runtime

I want to:
`awman agents attach <id>` to a running agent's live terminal and `awman agents kill <id>` (or `rm`) to stop one that is stuck or no longer needed

So I can:
manage builtin microVM agents without a host runtime CLI (there is no `docker attach`/`docker rm` equivalent for the builtin runtime today)

### User Story 3:
As a: user

I want to:
run `awman agents status` for a clean, informative one-shot summary, and add `--watch` only when I want a live dashboard, whether in my terminal or in a TUI tab

So I can:
check agent health without the command taking over my terminal or leaving garbage on screen when I quit


## Implementation Details:

### 1. Root-cause notes on the current `status` command (what the rewrite must fix)

Read these before deleting `src/command/commands/status.rs`, so the new code does not carry the bugs over:

- **Watch is effectively the default.** `StatusCommand::run_with_frontend` breaks only when `!self.flags.watch && !frontend.should_continue_watching()`. `CliFrontend::should_continue_watching` (`src/frontend/cli/command_frontend.rs:826`) returns `true` until SIGINT, and the TUI impl (`src/frontend/tui/per_command/status.rs:12`) always returns `true`. So `--watch` has no effect: every frontend loops forever.
- **Duplicate/garbage output on quit (CLI).** Each tick writes the dashboard through `write_status_dashboard`, which prints plain-text lines via `write_message`. After Ctrl-C, the returned `StatusOutcome` is rendered *again* by `render_status` (`src/frontend/cli/per_command/render.rs:71`) as a box-drawn table in a different format. A stray `^C` echo and `\x1b[2J\x1b[H` full-screen clears also push old frames into scrollback. Ctrl-C is only noticed after the next 3 s `sleep`, so quitting also lags.
- **Inconsistent formatting.** There are three different renderers: plain-text `G`/`Y` indicator letters in the trait default, a box table with emoji in `render_status`, and a ratatui `Table` in `render_status_dashboard` (`src/frontend/tui/render/execution_window.rs:219`). Each shows different columns.
- **Little information.** There is no runtime name or availability, no uptime (just a raw RFC 3339 `started_at`), and no aggregate counts or totals.

### 2. Catalogue changes (`src/command/dispatch/catalogue/`)

- Add a new catalogue file `src/command/dispatch/catalogue/agents.rs`, following the layout of `squad.rs`. It defines:
  - `AGENTS`: top-level `CommandSpec { name: "agents", aliases: &[], help: "Inspect and manage running code agents.", subcommands: &[&AGENTS_STATUS, &AGENTS_LIST, &AGENTS_DETAIL, &AGENTS_KILL, &AGENTS_ATTACH], requires_runtime: true, requires_container_tier: false, gateway_need: GatewayNeed::None, .. }`.
  - `AGENTS_STATUS`: `name: "status"`. Flags: `--watch` (Bool, default false, help "Keep refreshing the dashboard until interrupted.") and `--json` (Bool, implies `non-interactive` where that flag exists on the path, conflicts with `--watch`). `api_allowed: true`; the API frontend gets a single snapshot, and `--watch` is hidden from the API (`frontends: CliAndTui`).
  - `AGENTS_LIST`: `name: "list"`, `aliases: &["ps", "ls"]`. Flags:
    - `--all`/`-a` (Bool, "Include stopped agents that are still eligible for cleanup."), which uses `list_stopped` on runtimes that support it.
    - `--quiet`/`-q` (Bool, "Print only agent IDs.")
    - `--json` (Bool), which conflicts with `--quiet`.
    - `--no-trunc` (Bool, "Do not truncate IDs or image names.")
    - `api_allowed: true`.
  - `AGENTS_DETAIL`: `name: "detail"`, `aliases: &["inspect"]`, which is optional but recommended for docker/kubectl muscle memory. It takes a required positional `id` (`ArgumentKind::String`, help "Agent ID, unique ID prefix, or name."). Flags: `--json`. `api_allowed: true`.
  - `AGENTS_KILL`: `name: "kill"`, `aliases: &["rm"]`, required positional `id`. Flags: `--yes`/`-y` (Bool, skip confirmation, same wording as the existing `yes` flag in `core.rs:19`). `api_allowed: true`, and the API requires `yes` because it cannot prompt.
  - `AGENTS_ATTACH`: `name: "attach"`, required positional `id`. `api_allowed: false`. This is PTY-bound, so it is excluded from the API per `aspec/uxui/cli.md` "Frontend visibility and parity", in the same way as `SQUAD_ATTACH`.
- Register `&AGENTS` in the top-level command list (`catalogue/mod.rs`, near line 557) and **delete** `STATUS` from `core.rs` and from that list. Also update the `core.rs` module doc comment, which lists `status`.
- No soft migration: do **not** add a removed-command or migration hint for `awman status`. It becomes an unknown command and goes through the existing sibling-based "unknown command" correction path. `status` must not resolve as an alias of anything.
- Make sure the existing short flags `-a` and `-q` are not already used with a different meaning elsewhere in the catalogue (`aspec/uxui/cli.md` "Naming and casing": a short alias is never reused for a different meaning). Check this in `catalogue/tests.rs`. If they clash, drop the short form and keep only the long flag.
- Regenerate `docs/14-command-reference.md` from the markdown projection. The `COMMITTED_REFERENCE` test in `projections/markdown.rs` enforces a byte-for-byte match.

### 3. Command layer (`src/command/commands/agents/`)

Replace `status.rs` with a module directory, and move `status_tips.rs` into it as `agents/tips.rs`:

```
src/command/commands/agents/
  mod.rs          // re-exports, shared types
  snapshot.rs     // AgentSnapshot + collection from the runtime (shared by status/list/detail)
  resolve.rs      // id/prefix/name → AgentHandle resolution (shared by detail/kill/attach)
  status.rs       // AgentsStatusCommand
  list.rs         // AgentsListCommand
  detail.rs       // AgentsDetailCommand
  kill.rs         // AgentsKillCommand
  attach.rs       // AgentsAttachCommand
  tips.rs         // moved from status_tips.rs: TIPS + select_random_tip
```

**Tips are kept.** The list in `status_tips.rs` was refreshed ahead of this work item. Its tests check every `` `awman …` `` command and flag a tip names against the `CommandCatalogue`, so this work item's catalogue change will fail them until the tips are updated. When moving the file to `agents/tips.rs`:

- Rewrite the two `awman status` tips for the new commands: `` `awman agents status` `` (one-shot) and `` `awman agents status --watch` `` (live, Ctrl-C to stop).
- Add tips for the new subcommands:
  - `` `awman agents ls` `` / `ps`, and `` `awman agents ls -q` `` for scripting
  - `` `awman agents detail <id>` `` / `inspect`
  - `` `awman agents attach <id>` `` (detaching never stops the agent)
  - `` `awman agents rm <id>` `` / `kill` with `-y`
- Keep `select_random_tip`. In watch mode, pick the tip once per watch run rather than every tick, so the footer doesn't flicker between tips.
- Tips stay at 100 characters or fewer (enforced by `every_tip_fits_on_one_line`).

**Shared data (`snapshot.rs`)**, which is serde-serialisable so that `--json` and the API return the same shape:

```rust
pub struct AgentSnapshot {
    pub id: String,              // full runtime id
    pub short_id: String,        // first 12 chars, docker-style
    pub name: String,
    pub image: String,
    pub state: AgentState,       // Running | Stuck | Stopped
    pub started_at: DateTime<Utc>,
    pub uptime_secs: u64,        // computed once at collection time
    pub source: AgentSource,     // Session | Squad(slug)  (moved from status.rs ContainerSource; still derived from the name only)
    pub tab_number: Option<u32>, // TUI only
    pub command_label: Option<String>,
    pub cpu_percent: Option<f64>,
    pub memory_mb: Option<f64>,
}

pub struct AgentsOverview {
    pub runtime: String,          // runtime_name()
    pub runtime_display: String,  // display_name()
    pub available: bool,
    pub agents: Vec<AgentSnapshot>,
    pub totals: AgentTotals,      // running, stuck, cpu sum, mem sum
    pub collected_at: DateTime<Utc>,
    pub tip: String,              // from tips::select_random_tip, chosen once per run
}
```

- `collect_overview(runtime: &dyn AgentRuntimeEngine, tui: Option<&AgentsTuiContext>) -> Result<AgentsOverview, CommandError>` is the single collection function used by `status` and `list`. Stats remain best-effort per row (`None` on error), as they are today. If `availability()` fails, the function returns an overview with `available: false` and no rows, and the command exits with the runtime's error class (exit 3 for Docker/Apple, 1 for builtin per `aspec/uxui/cli.md`). It must not panic or print a half-table.
- Rename `StatusCommandTuiContext`/`TuiTabSnapshot` to `AgentsTuiContext`/`TuiTabSnapshot` and move them here. Keep the doc comment explaining why this is a per-tick frontend hook rather than `CallerContext` (WI 0114 F-49).

**Agent reference resolution (`resolve.rs`):**

- Add `resolve_agent(runtime, reference: &str, include_stopped: bool) -> Result<AgentHandle, CommandError>`. It matches in docker's order: exact id, then exact name, then unique id prefix. It also accepts the `short_id` printed by `list`.
- No match → `CommandError` mapped to exit **4** (resource does not exist). The message names the reference and suggests `awman agents ls`.
- More than one prefix match → usage error (exit **2**) that lists the matching short IDs and names.
- Generalise the matching logic in `squad::attach::resolve_attach_target` (`src/command/commands/squad/attach.rs:498`) into this helper so both use the same rules. Squad may keep its task-scoped candidate filtering on top of it.

**`AgentsStatusCommand` (`status.rs`):**

- One-shot by default: collect once, call `frontend.render_overview(&overview)` once, and return `AgentsStatusOutcome { overview }`.
- With `--watch`: loop `collect → frontend.render_overview → wait`. The wait is a `tokio::select!` between a 2 s interval tick and a cancellation token that the frontend supplies through `frontend.watch_cancel()`. Ctrl-C must therefore exit immediately rather than after the next sleep. The loop decision depends **only** on `flags.watch` and the cancel token. Remove `should_continue_watching` entirely, so no frontend can turn a one-shot into a watch.
- A watch run returns no outcome: cancelling it is a user abort (`CommandError::Aborted`, exit 130), so the CLI renderer prints **nothing** after it, because the frontend has already drawn the last frame. This removes the duplicate table on quit. (As implemented; an earlier draft returned a `watched: true` outcome, which became unreachable.)
- Content of the overview (shared by all renderers; layout is up to each frontend):
  - Header: the runtime display name, an availability indicator, the agent count broken down by state (`3 running · 1 stuck`), and total CPU and memory.
  - Rows: state glyph, short id, name, source (`session`, `squad:<slug>`, `tab 2`), image, uptime (humanised, e.g. `4m12s`, `2h03m`), CPU %, and memory.
  - Sorting: stuck agents first, then by uptime descending.
  - Empty state: "No agents running on <runtime>." plus one next-step line (`awman chat` / `awman exec workflow <file>`).
  - Footer: `Tip: <tip>`, one tip per run.

**`AgentsListCommand` (`list.rs`):** collects the same snapshots as `status` but renders a flat, docker `ps`-style table: `AGENT ID  NAME  IMAGE  STATUS  UPTIME  SOURCE`. With `--quiet`, it prints only the short ids, one per line, so `awman agents ls -q | xargs -n1 awman agents rm -y` works. With `--all`, it also includes `list_stopped()` results, shown with state `Stopped`. On runtimes where `list_stopped` returns empty (Apple), this is silent and not an error. `list` does **not** fetch per-agent stats; it calls only `list_running_all`, so it stays fast. Stats belong to `status`/`detail`.

**`AgentsDetailCommand` (`detail.rs`):**

- Add a new trait method on `AgentRuntimeEngine` (`src/engine/agent_runtime/mod.rs`): `fn inspect(&self, handle: &AgentHandle) -> Result<AgentDetail, EngineError>`. Its default implementation builds an `AgentDetail` from the handle and `stats()`, so every runtime works without changes.
- `AgentDetail` lives in `src/engine/agent_runtime/execution.rs` next to `AgentStats`. It has the common fields plus `runtime_extras: BTreeMap<String, String>`, an ordered, display-ready key/value list that keeps Layer 2 free of runtime-type matching.
- Builtin override (`src/engine/container/builtin/backend.rs`): uses `driver.get(&handle.id)` labels (`awman.owner.pid`, `awman.version`, `awman.worker.protocol`), the sandbox name (`naming::sandbox_name_for`), the vCPUs/memory MiB and network mode the sandbox was launched with, and whether the owner process is still alive. Whether the owner is alive decides whether `attach` can succeed (see Edge Cases).
  - If the driver does not currently keep launch sizing or network mode on the summary, record them as additional reserved labels at launch in `naming::labels`, for example `awman.vcpus`, `awman.memory_mib`, `awman.network.mode`. Add them to the reserved-key list too.
- Docker override (optional, nice to have): mounts and working directory from `docker inspect`.
- The human output is a two-column key/value block grouped as **Identity** (id, name, image, source), **Lifecycle** (state, started, uptime), **Resources** (CPU, mem), and **Runtime** (`runtime_extras`).

**`AgentsKillCommand` (`kill.rs`):**

1. Resolve the agent with `resolve.rs`. Running agents only; `--all` semantics do not apply.
2. Confirm through a Layer-2 `Prompt<D>` ("Stop and remove agent <name> (<short id>)?" with Yes and No choices, default No). `--yes` skips the prompt. In non-interactive mode or without a TTY and without `--yes`, refuse with exit 2 (`aspec/uxui/cli.md` "Prompt and dialog conventions").
3. `runtime.stop(&handle)`. Container runtimes already stop and remove. For the builtin and sandbox runtimes, follow with best-effort `runtime.remove_agent(&handle.id)` so `rm` actually removes the agent. Ignore `UnsupportedOnRuntime` here.
4. Return `AgentsKillOutcome { id, name, removed: bool }`. The CLI prints only the short id on stdout (docker parity), and messages go to stderr.

- The builtin `stop` already allows a user-initiated stop of an agent launched by another awman process, and protects against generation reuse (`backend.rs:427`). Do not bypass that check. Surface "refers to an older sandbox generation" as a clear "agent was replaced; re-run `awman agents ls`" error.

**`AgentsAttachCommand` (`attach.rs`):**

- Resolve the agent, then call `runtime.attach(&handle)`. This returns a `Box<dyn AgentInstance>`, which is driven through the existing `run_with_frontend(Box<dyn AgentFrontend>)` path exactly as `squad attach` does. Reuse the CLI and TUI attach frontends from `src/frontend/cli/per_command/squad_attach.rs` and `src/frontend/tui/per_command/squad.rs` rather than writing new PTY plumbing. Factor the shared, non-squad parts out if needed.
- Detaching (the frontend's detach key or Ctrl-C on the attach session) must never stop the target. Ownership stays with the launching process, as `AgentRuntimeEngine::attach` documents.
- Before attaching, check `runtime.reattach_after_owner_exit()`. When it is `false` (Apple, builtin) and `inspect` reports that the owner process is gone, fail fast with "this agent's launching awman process has exited; the builtin and Apple runtimes can only attach while it is running". Do not let the attach socket connect fail with an opaque error.

### 4. Dispatch wiring (`src/command/dispatch/`)

- `build.rs`: remove `status`, and add `agents_status`, `agents_list`, `agents_detail`, `agents_kill` and `agents_attach` builders, each calling `XCommand::from_input(ctx)`.
- `mod.rs`: replace `BuiltCommand::Status` / `CommandOutcome::Status` with one variant per subcommand. An alternative is a single `Agents(AgentsOutcome)` enum, in the same way squad groups its outcomes; this is preferred because it keeps the top-level enum small.
- `frontend_action.rs`: change `FrontendAction::WatchStatus` to `path: &["agents", "status"], bool_flags: &["watch"]`. The TUI still auto-starts a live dashboard in non-Git directories, now explicitly through `--watch`.

### 5. Frontends

**Frontend traits:** add a single `AgentsCommandFrontend: UserMessageSink + Send + Sync` trait with:

- `tui_context() -> Option<AgentsTuiContext>` (default `None`)
- `watch_cancel() -> CancellationToken`
- `render_overview(&AgentsOverview)`
- `confirm(Prompt<KillChoice>) -> KillChoice`

A single trait across subcommands is preferred to five near-empty ones. Only `status --watch` needs streaming. The other subcommands return outcomes that the per-frontend renderers draw.

**CLI (`src/frontend/cli/`):**

- Move the renderers into a new `per_command/agents.rs` and remove `render_status` and the `StatusCommandFrontend for CliFrontend` impl, including the `WATCH_INTERRUPTED` and `WATCH_HANDLER_INSTALLED` statics.
- One-shot `status`/`list`/`detail`: render from the outcome via `render.rs`, reusing `format_table`. Status glyphs are colored only when stdout is a TTY (`●` green/yellow/gray). Avoid emoji: their width breaks column alignment.
- `status --watch` on a TTY:
  - Enter the terminal's alternate screen and hide the cursor (crossterm is already a dependency through ratatui). Redraw in place each tick with cursor-home plus clear-to-end.
  - On Ctrl-C, cancel the token, leave the alternate screen and restore the cursor, then print **one** final non-watch snapshot to the normal screen so the user keeps a record. Install a guard (`Drop`) so the terminal is restored even on error or panic.
  - Disable the `^C` echo while in watch mode, or erase it with a cursor reset.
- `status --watch` without a TTY (piped): print one snapshot per tick separated by a blank line, with no escape codes.
- `--json`: `status` prints the `AgentsOverview` object, `list` prints an array of `AgentSnapshot`, `detail` prints an `AgentDetail`, and `kill` prints the outcome. JSON replaces the human renderer entirely.

**TUI (`src/frontend/tui/`):**

- Rename `StatusDashboardData` / `SharedStatusDashboard` (`tabs.rs:244`) to `AgentsDashboardData { overview: AgentsOverview }`. Update `per_command/status.rs` → `per_command/agents.rs`, `app.rs::refresh_status_context` → `refresh_agents_context`, and `clear_active_tab_for_new_command`.
- Rewrite `render_status_dashboard` → `render_agents_dashboard` in `render/execution_window.rs`:
  - A header line with the runtime and totals.
  - A ratatui `Table` with the same columns and order as the CLI `status` output (parity), including humanised uptime and a TAB column.
  - The footer line shows "Refreshing every 2s · Esc to stop" when watching and the collection timestamp otherwise.
  - Keep the tip as the dashboard's last line, chosen once per run.
- One-shot `agents status` in the TUI renders the table once and the tab goes to `Done`. It must **not** keep looping. Remove the TUI's `should_continue_watching` override.
- `--watch` in the TUI stops when the tab starts a new command, the tab is closed, or the user presses Esc/Ctrl-C in the execution window. Connect `watch_cancel()` to the tab's existing command cancellation.
- `agents list` / `detail` / `kill` in the TUI render through the normal status log (reuse `format_table`). `kill` confirmation uses the shared dialog router, with the same `Prompt` value as the CLI. `agents attach` reuses the squad-attach container view (maximisable container window).

**API:** `agents status` (snapshot only), `list`, `detail` and `kill` (requires `yes`) are exposed through `/v1/commands`. `attach` and `--watch` are excluded at the catalogue level. No new hand-written routes.

### 6. Cleanup of references to the old command

- `src/main.rs:198`, `src/frontend/cli/mod.rs:558/582/601/650`, `src/frontend/cli/command_frontend.rs` tests at lines 915–1348, `src/frontend/tui/tests/*`, `src/frontend/tui/command_box.rs:67/126`, `src/frontend/tui/hints.rs:53`, `src/frontend/tui/per_command/mount_scope.rs:38` and `catalogue/tests.rs:357` all use `["status"]` as a convenient sample command. Switch them to `["agents", "status"]`, or to another simple command when the test is not about status.
- `grep -rn '"status"' src` must leave only unrelated hits: `squad status`, `api status`, JSON `"status"` keys, and the new `agents status` leaf.


## Edge Case Considerations:

- **Runtime unavailable** (Docker daemon down, Apple `container system` stopped, builtin hypervisor unavailable or `test_isolation`): `status` and `list` print a single clear error naming the runtime and exit with its class (3 for Docker/Apple, 1 for builtin). `--watch` keeps retrying each tick and shows an "unavailable" header instead of exiting, so a daemon restart is picked up live.
- **Stats failures / sandbox runtime:** SBX `stats` returns zeros and builtin stats can fail transiently. Render `-` for missing values, and treat SBX zeros as "not reported" (`-`) rather than `0.0%`. Use `Capabilities` or a documented `Option` in `AgentStats`; do not match on the runtime name.
- **Agent exits between list and action:** `kill`/`attach`/`detail` resolve and then act. A not-found error from the runtime at action time maps to exit 4 with "agent <ref> is no longer running".
- **Ambiguous ID prefix:** exit 2 listing the candidates. A reference that exactly matches one agent's name and another agent's id prefix resolves to the exact name match (exact matches always beat prefixes).
- **Very short prefix:** accept any length, as docker does, but ambiguity is reported as above. An empty string is a usage error.
- **Builtin generation reuse:** a name reused by a newer sandbox makes `stop` fail the generation check. Report "agent was replaced" and never stop the replacement.
- **Builtin/Apple attach after the owner exits:** `reattach_after_owner_exit()` is `false` and the attach socket lives in the launcher. Detect this through `inspect`, which checks whether the owner pid is alive, and fail with an explanatory error before trying to connect. Do not leave a dangling attach.
- **Attach must never stop the target** on detach, Ctrl-C, or a frontend crash. The attaching process does not own the agent.
- **Killing an agent owned by a live TUI tab / workflow / squad task:** allowed (docker parity). The owning process sees the agent exit and reports it through its normal Done/Error path. When `source` is a squad task or a TUI tab, the confirmation prompt says so ("This agent belongs to squad task `issue-triage`; the squad run will fail.").
- **`kill` without a TTY and without `--yes`:** refuse with exit 2. With `--json`, also refuse unless `--yes` is given.
- **`rm` alias vs. stopped agents:** `kill`/`rm` targets running agents. Stopped-agent cleanup remains `awman clean`'s job. If the reference matches only a stopped agent, say so and point at `awman clean` rather than silently removing it. Keep this simple: no `--force` flag.
- **Squad and session naming:** the source is still derived from the container **name**, never labels, so it works the same on Apple. Unchanged from today.
- **Wide/narrow terminals:** the CLI table truncates IMAGE then NAME with `…` to fit the terminal width (unless `--no-trunc`). The TUI `Table` uses `Constraint::Min`/`Fill`, so a narrow tab never wraps rows.
- **Many agents:** the TUI table scrolls with the existing execution-window scroll keys. The CLI watch mode truncates to the visible height with a "+N more" footer.
- **Non-Git directory TUI startup:** this now dispatches `agents status --watch` explicitly (via `FrontendAction::WatchStatus`). It must still work without a repo, because `agents status` holds no session.
- **Old muscle memory:** `awman status` now fails as an unknown command (no migration shim, by design). The unknown-command suggester must not suggest `squad status`/`api status` in a confusing way, so check what the edit-distance lookup produces and keep its output sensible.
- **Alias collisions:** `ls`, `ps`, `rm`, `inspect` and `kill` are scoped under `agents` only. Note that `SQUAD_STOP` already uses the alias `kill` under `squad`, which is a different parent, so there is no clash. The catalogue tests must still assert that sibling names and aliases are unique within `agents`.


## Test Considerations:

**Unit tests (Layer 2, `src/command/commands/agents/*`)** using a fake `AgentRuntimeEngine`. Pattern: the fakes in `src/engine/container/background.rs:459` / `src/engine/agent/mod.rs:2626`.

- `collect_overview`:
  - Totals and state counts.
  - Sort order: stuck first, then uptime descending.
  - Stats errors become `None`.
  - SBX-style zero stats render as "not reported".
  - TUI context enrichment attaches the tab number, stuck flag and command label. This is ported from the existing `tui_context_enriches_row_with_matching_tab` tests, which should test the real function instead of re-implementing the matching inline as they do today.
  - Source classification: session vs `squad:<slug>`.
- `resolve_agent`:
  - Exact id, exact name and unique prefix.
  - Exact match beats prefix.
  - Ambiguous → exit 2 with candidates.
  - Unknown → exit 4.
  - Empty → exit 2.
  - Stopped-only match on `kill` → points to `awman clean`.
- `AgentsStatusCommand`:
  - Without `--watch`, it renders exactly once and returns, even when the frontend would happily loop. This is a regression test for the "watch is default" bug.
  - With `--watch` and a pre-cancelled token, it renders once and returns promptly. It must not wait out the interval; use `tokio::time::pause`.
  - With `--watch`, it renders N times for N ticks.
- `AgentsKillCommand`:
  - Confirmation No → nothing stopped.
  - `--yes` → `stop` then `remove_agent`.
  - `UnsupportedOnRuntime` from `remove_agent` is ignored.
  - Non-interactive without `--yes` → exit 2.
  - Generation-mismatch error message.
- `AgentsAttachCommand`:
  - `reattach_after_owner_exit() == false` with a dead owner → explanatory error, and `attach()` is never called.
  - Attach success drives the instance through the frontend.
  - Detach never calls `stop`.
- `agents/tips.rs`: the existing tips tests (catalogue check, no duplicates, 100-character limit) pass with the rewritten `agents` tips, and no tip still names `awman status`. In watch mode, the same tip is shown on every tick of one run.
- `AgentsDetailCommand`: the default `inspect` impl fills the common fields, and a runtime's `runtime_extras` are passed through in order.

**Unit tests (Layer 1):**

- Builtin `inspect` reads the owner pid, version, vCPUs, memory and network mode from labels. Use `builtin/fake.rs`.
- Any new reserved labels are rejected when supplied by the caller (`naming::labels`).
- The default trait `inspect` is used by Docker, Apple and SBX unless overridden.

**Catalogue / projection tests:**

- `agents` exists with exactly the five leaves.
- `ps`/`ls` resolve to `list`, `rm` to `kill`, and `inspect` to `detail` through the clap projection.
- `awman status` no longer parses and has no removed-command hint.
- `--json` conflicts with `--watch` on `status` and with `--quiet` on `list`.
- `attach` and `--watch` are not visible to the API projection, and `api_schema` matches.
- The short flags `-a`/`-q`/`-y` are unique in meaning across the catalogue.
- The `docs/14-command-reference.md` byte-for-byte test passes after regeneration.
- `parity_test.rs` covers the kill confirmation prompt (same labels and default in the CLI and the TUI).

**Frontend tests:**

- CLI renderers:
  - Snapshot tests of `status` (empty, mixed states, stuck, squad source), `list` (normal, `--quiet`, `--no-trunc`, `--all` with a stopped row), and `detail`.
  - JSON round-trips for each outcome.
  - Assert that a watch ends in `Aborted` and renders no outcome. This is a regression test for the duplicate table on quit.
- CLI watch terminal guard: the alternate-screen enter and leave sequences are emitted in pairs, also on the error path. Test this with a writer abstraction rather than real stdout.
- TUI:
  - `render_agents_dashboard` with a `TestBackend`: header totals, column order matching the CLI, empty state, and the footer text for watch vs one-shot.
  - A one-shot `agents status` tab reaches `Done`, and starting a new command in a watching tab cancels the watch.
  - `startup_for` a non-Git session dispatches `["agents","status"]` with `watch=true` (update `frontend_action.rs:272`).

**Integration / end-to-end:**

- `make test` e2e: `awman agents ls`, `awman agents ps --json` and `awman agents status` against the Docker runtime. These are gated like the existing Docker integration tests. Each exits 0 and prints exactly one table. `awman status` exits non-zero with an unknown-command message.
- Builtin native validation (macOS on Apple Silicon, behind the existing builtin native gates from WI 0124):
  - Launch a chat agent, then `agents ls` shows it and `agents detail <prefix>` shows the vCPUs, memory and network mode.
  - `agents attach` from a second terminal works while the owner is alive. Detaching leaves the agent running.
  - After the owner exits, attach fails with the explanatory error.
  - `agents rm -y` stops and removes the agent, and `agents ls -a` no longer shows it.


## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- **Catalogue is the only source of command names, flags, aliases and visibility** (`aspec/uxui/cli.md` "Catalogue-driven"). No frontend may hard-code `"agents"`, `"ps"` and so on. The TUI startup path goes through `FrontendAction` (`src/command/dispatch/frontend_action.rs`).
- **Layering** (`aspec/architecture/design.md`):
  - Layer 0 holds any new label constants or state helpers, if needed.
  - Layer 1 (`src/engine/agent_runtime/`) holds `AgentDetail` and the `inspect` trait method with a default implementation, plus runtime overrides in each backend.
  - Layer 2 (`src/command/commands/agents/`) holds the collection, resolution, confirmation and outcome types.
  - Layer 3 holds rendering only.
  - Layer 2 must branch on `capabilities()` / `reattach_after_owner_exit()`, never on the concrete runtime type or `runtime_name()` string comparisons.
- **Do not add a new `Capabilities` flag** unless something actually branches on it. `capabilities.rs` documents that unread flags are deleted (WI 0126 C7).
- **Reuse rather than duplicate:**
  - `format_table` (`src/frontend/cli/per_command/render.rs`)
  - the squad-attach CLI and TUI attach frontends
  - `parse_squad_task_slug` for source classification
  - the existing `yes` flag wording and `Prompt<D>` confirmation shape
  - the existing tab command cancellation for TUI watch-stop
- **Security** (`aspec/architecture/security.md`): `attach` connects only to awman-owned agents through the runtime's own channel (Docker `attach`, the awman attach socket for Apple and builtin, `sbx` for sandboxes). It never execs a sibling shell or runs anything on the host. `kill` only targets agents returned by awman's own listing (label or name-prefix filtered), so it can never stop a non-awman container.
- **Exit codes:** map new failures to the existing classes: not found → 4, ambiguous/usage/needs `--yes` → 2, runtime down → 3 (Docker/Apple) or 1 (builtin), user abort → 130.
- Keep each new file small and readable by an intermediate Rust developer (Principle 1). Prefer one `AgentsOutcome` enum and one `AgentsCommandFrontend` trait over five parallel trait and outcome hierarchies.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

Specific docs that currently reference `awman status` and must be updated to `awman agents …`:

- `docs/01-concepts.md`: the command table and the source-marker paragraph.
- `docs/02-using-the-tui.md`: non-Git startup now runs `agents status --watch`, and the Esc-to-stop behaviour.
- `docs/03-agent-sessions.md`: rewrite the status section around `agents status` / `list` / `detail` / `kill` / `attach`, including aliases, `--watch`, `--json` and `-q` scripting examples.
- `docs/11-runtimes.md`: builtin and SBX notes. Cover the attach-only-while-the-launcher-is-alive limitation for builtin and Apple, and stats being unavailable on SBX.
- `docs/12-squad.md` and `docs/09-api-and-remote-mode.md`: any status mentions, plus which `agents` subcommands the API exposes.
- `docs/14-command-reference.md`: regenerated from the catalogue, not hand-edited.
- `README.md`: any `awman status` examples.

See `CLAUDE.md` for more guidance on documentation standards.
