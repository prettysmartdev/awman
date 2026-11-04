# Work Item: Feature

Title: Daemon work in Rex: squad tasks and worker jobs as Rex windows
Issue: issuelink

Depends on: 0159 (config and API naming, route tree), 0153 (Rex presenter, ledger, `tui.integrations.rex`), 0154 (the `engine` feature and host presenters), 0145 (one daemon, features, ephemeral enablement), 0146 (terminal hub, `awman attach --read-only`, `take_control`), 0147 (`worker` push and pull).

## Summary:
- 0153 presents a user's own TUI instances in Rex. The daemon also runs work that no TUI instance owns:
  - `squad` tasks;
  - `worker` jobs, either pushed by API and remote clients or pulled from work servers.
  
  Today that work can only be watched with `squad attach` or `agents attach`, one terminal at a time.
- This item lets the engine's Rex presenter (0153) show that work in a **dedicated Rex session** (default label `awman`): one Rex window per squad task run or worker job, tiled with the same layout planner as workflow windows, and with agents as native Rex terminal blocks.
- **This is presentation only.** Squad and worker behaviour is exactly the same whether presented or not. Nothing is scheduled, claimed, cancelled or answered differently.
- **Privacy and safety by default:**
  - only `squad` is shown by default; worker jobs, which can carry other callers' prompts and data, must be opted in;
  - daemon-work blocks are **read-only** unless `allowInput` is set, and then every input grant is audited through 0146.

## User Stories

### User Story 1:
As a: developer who uses squad tasks and lives in Rex

I want to:
see each running squad task as a window in an `awman` Rex session, with its agents as Rex terminals and status in Rex's session picker

So I can:
check on unattended work, including from the Rex iOS app, without running `squad attach` for each task.

### User Story 2:
As a: operator of a build box that pulls jobs from a work server

I want to:
opt in to showing pulled worker jobs in Rex on that box

So I can:
open the box's Rex server from my laptop and watch its jobs as native tabs.

### User Story 3:
As a: security-conscious user

I want to:
daemon-work terminals to be read-only by default, and every input grant to be logged when I enable input

So I can:
make sure Rex's remote access can't quietly be used to type into unattended agents.


## Implementation Details:

### 1. Configuration (scope `global`; extends 0153's `tui.integrations.rex`; rows in 0159's key table)
```json
"tui": { "integrations": { "rex": {
  "daemonWork": { "show": ["squad"], "session": "awman", "server": null, "allowInput": false }
} } }
```
| Key | Values | Default | Meaning |
|---|---|---|---|
| `daemonWork.show` | list of `"squad"`, `"worker_push"`, `"worker_pull"` (0159 enum spelling) | `["squad"]` | Which daemon work is presented. Empty turns the feature off. |
| `daemonWork.session` | string | `"awman"` | The Rex session label. It is found with `session.list` or created with `session.create`, and its ID is kept in the ledger. |
| `daemonWork.server` | host label/address \| null | `null` | The Rex server to present on (see §3). |
| `daemonWork.allowInput` | bool | `false` | Make daemon-work agent blocks writable (through 0146's input grant). |
- Setting any of these keys in a repo config is a validation error naming the key. Unknown values in `show` are rejected with the allowed list.

### 2. Presenter: `src/engine/integrations/rex/daemon_work.rs`
- An extension of 0153's `RexPresenter`, using the same `RexApi`, event streams, layout planner and ledger. It adds the ledger kind `DaemonWork { feature: Squad | WorkerPush | WorkerPull, id }`, where `id` is the squad task run ID or the worker command or lease ID.
- **Inputs:**
  - from the core event bus (0145): squad run started/finished, worker job started/finished (push and pull alike, using 0147's shared job model);
  - 0146 `TerminalOpened`/`TerminalClosed` with 0137 `source` envelopes;
  - 0152 status.
  
  Jobs whose feature isn't in `show` are ignored.
- **Output:**
  - One window per run or job, labelled `<glyph> <task-or-job title> · <repo basename>`, with the glyph taken from the aggregate 0152 status.
  - The window's layout comes from `layout_plan::plan`: one block per agent terminal, tiled. Each block runs plain 0146 attach: `[awman, attach, <terminal-id>, --annotation, rex/view=<ledger-id>, --read-only]` (`term_…`, served by the core `/v1/terminals/{id}`), or the same without `--read-only` when `allowInput` is set. There is no Rex-specific flag. The output filter and OSC 7501 re-encoding come from 0156 (always on when attach writes to a TTY), and block labels are set by the presenter through the Rex API.
  - When the run or job ends, its blocks keep their final screens with a `✓`/`✗` label. Its window closes after 10 minutes, or immediately if `closeFinishedAgents` is set.
- **Overflow:**
  - At most **12** daemon-work windows per Rex session. Further runs are listed in one `awman · more` window running `awman agents status --watch`.
  - When a window frees up, the oldest overflowed run gets a window.
- **Focus:** the presenter never focuses a daemon-work window on its own. Only explicit actions do (§4).

### 3. Which Rex server, and when
- Targets, in order:
  1. `daemonWork.server`;
  2. the Rex server of the most recently connected Rex frontend (0153);
  3. the user's Rex home server (the `rex` CLI default).
- Before presenting, the presenter runs the same identity check as 0153: `server.whoami` must be this user, and the server's blocks must run on the daemon's machine. Otherwise it reports `rex: refused (<reason>)` in `daemon status` and doesn't present.
- **Unreachable** (Rex not running, or the macOS app closed): the presenter sits in `waiting` and retries every 30 s. On first contact it **backfills** windows for work that is already running.
- **Rex restart or session deleted** (`session_destroyed`): ledger entries are pruned and the session is recreated lazily, at most one `session.create` per minute.
- **Daemon restart:** ledger entries whose runs are still live are re-adopted. Blocks for terminals that are gone (0146 `410`) get an exit banner.

### 4. Engine requirement and the ephemeral hold
- The presenter lives in `engine` (0154). Presenting daemon work therefore needs `engine` on, either persistently or ephemerally.
- **Ephemeral hold:** while presented work runs, an ephemeral `engine` (0145) does not idle out. When the last presented run ends and no other frontends or instances remain, the normal idle timer resumes.
- If `engine` is off and was never enabled, nothing is presented. `awman daemon status` reports `rex daemon work: idle (engine off)`. This item never enables `engine` by itself, because only a frontend's hand-off may do that.

### 5. Squad and attach in Rex frontends
- In a TUI frontend claimed by the Rex presenter (its `Hello.annotations` carry `rex/*`, 0153), when `squad` is in `show`:
  - The squad tab's per-task "attach" action, and `awman squad attach <task>` run in a Rex terminal (where Rex detection produces the same annotations), **focus that task's daemon-work window** (`session.focus_window`) instead of attaching inline.
  - If the task isn't presented (it is overflowed, or Rex is unreachable), they fall back to today's inline attach with a one-line notice.
- Outside Rex, and when `squad` isn't in `show`, `squad attach` is unchanged.

### 6. Security
- **Read-only by default.** `--read-only` drops stdin and never resizes the PTY (0146).
  - With `allowInput`, the block requests 0146's input grant with `take_control`. Every grant and release is logged in the run's event log as `TerminalControl{holder: "local", annotations: {"rex/view": …}}` (0146), so the audit trail names the Rex block through generic annotations.
  - The docs state that Rex's remote access (Tailscale, login server, iOS) can reach these blocks.
- **Worker jobs hidden by default** (`worker_push` / `worker_pull` are not in the default `show`), because they may carry other callers' prompts, repository contents and data.
- **Labels are sanitized** with 0153's sanitizer: control characters and bidi stripped, a 64-character cap, and an allow-list of label characters. They are built only from task names, job titles and repo basenames, never from prompt text, tokens, API actor headers or work-item bodies.
- **Agent output reaches the Rex terminals only through 0156's filter.** Argv comes only from awman's binary path, fixed subcommands and validated IDs.

## Edge Case Considerations:
- **A squad task runs every minute:** each run gets a window, which closes 10 minutes after the run finishes. The 12-window cap and the overflow window keep Rex usable.
- **A pulled job is cancelled by the work server:** the window gets the `✗` label and the cancel reason as the block label suffix. Worker behaviour is unchanged.
- **The same Rex session label is already used by the user for something else:** the presenter adopts the session by label, but only ever touches windows recorded in its ledger.
- **A user closes a daemon-work block or window:** that only stops viewing; the run continues. The presenter does not reopen it for that run.
- **`show` is changed at runtime** (config reload): newly hidden features' windows are closed (only ledger entries), and newly shown ones are backfilled.
- **Several Rex frontends on different servers, with `server: null`:** the target follows the most recent frontend's server. Already presented windows stay where they are until their run ends.

## Test Considerations:
- **Unit:**
  - the `show` filter;
  - window label building and sanitization;
  - the overflow cap and promotion order;
  - server target selection;
  - the ephemeral-hold decision;
  - the focus mapping for `squad attach` in a Rex frontend.
- **Integration** (daemon core + `FakeRexApi` + a fake `rex` binary, resolved through `host_cli::program("rex")` without touching the process-wide `PATH`):
  - a squad run is presented with read-only blocks; worker push and pull jobs are hidden by default and presented when listed;
  - `allowInput` → `take_control` is called and the `TerminalControl` event is logged;
  - backfill on first contact; a Rex restart; `session_destroyed`; daemon restart re-adoption;
  - **squad and worker results, events and timings are identical with presentation on and off** (parity).
- **End-to-end:** in the gated `AWMAN_TEST_REX=1` suite, a squad trigger appears as a window in the `awman` Rex session, and `rex capture` shows the agent.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- The code is `src/engine/integrations/rex/daemon_work.rs`. It reuses 0153's `client.rs`, `events.rs`, `layout_plan.rs` and `ledger.rs`; don't fork them. It subscribes to the 0145 core event bus, so it has no dependency on the squad or worker code.
- Config keys go in `src/data/config/integrations/rex.rs` (`RexIntegrationConfig.daemon_work`, 0153), are rows in 0159's key table (`fields.rs`) with scope `global`, and are resolved in `effective.rs`. For one run they can be overridden with `--set tui.integrations.rex.daemonWork.show=squad,worker_pull`.
- The `squad attach` focus mapping lives in the Rex host presenter's `UiRequest` handling. In the CLI, `squad attach` asks the engine (through the integration registry and its detected annotations) whether a presenter shows that task, and if so asks for focus instead of attaching. `squad attach` run in a Rex terminal is itself an engine frontend (0155), so it sends a `UiRequest::FocusWork{target_id: task_…}` over its own frontend connection. The engine routes it to whichever presenter claims that frontend's annotations. The reply is `UiAck{claimed_by_host}`; `false` means fall back to inline attach. API clients can make the same request with `POST /v1/engine/instances/{id}/ui_requests {request: "focus_work", target_id}` (0154, 0159). There is no `actions` route. No Rex-specific flag or subcommand is added, and squad logic itself is untouched.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.

Specifically for this item:
- The Rex guide (`docs/18-rex-integration.md`, from 0153): a "Daemon work in Rex" section covering the keys, read-only blocks and `allowInput`, and remote-access reach.
- `docs/12-squad.md`: watching squad tasks in Rex.
- The workers guide (`docs/17-workers-and-work-servers.md`, from 0147): showing worker jobs in Rex (opt-in).
- `docs/07-configuration.md`: the `daemonWork.*` keys.
