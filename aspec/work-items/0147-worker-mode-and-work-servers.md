# Work Item: Feature

Title: Worker mode: pull work items from work servers, with label affinity and a queue picker
Issue: issuelink

Depends on: 0145 (daemon modes), 0146 (interaction, terminal hub), 0133 (dynamic runs in a fresh clone, `base_ref`, `merge_refs`, push), 0137 (structured results).

## Summary:
- Today something outside awman must push work into it:
  - a person at the TUI or CLI;
  - a squad task;
  - an API client holding a key and reaching awman's loopback port.
  There is no way for awman to **go and get** work. So every API client has to run on the same machine, or tunnel in. A team cannot spread work across laptops and build boxes. A person cannot say "give me the next thing to do".
- Add a **worker mode** to the daemon (0145). A worker connects **out** to one or more **work servers**, registers with labels, and takes work items from a queue the server keeps.
  - **Background daemon** (non-interactive host): items are claimed and run **immediately, in queue order**, up to the worker's capacity.
  - **TUI or CLI**: the worker is optional and runs **manual** claims. Queued items appear in a list, and the person **chooses** one with an interactive picker. It then runs in their TUI exactly as if they had typed `exec workflow --dynamic` themselves.
- **Node affinity.** Each item may carry label selectors, and the server offers it only to workers whose labels match. Labels the worker reports about itself (OS, agents, GPU) are kept apart from labels the server assigns (who owns the worker, which team it serves), so an item can be pinned to one person's machine.
- Define the **Work Server Protocol (WSP) v1**: plain HTTPS+JSON, worker-initiated only, and generic. infact (its work item 0023) is one implementation and the GitHub issues reference server (0148) is another. Anyone can write their own.

## User Stories

### User Story 1:
As a: operator of a build box

I want to:
`awman worker link https://work.example.com` once, then `awman daemon start --worker --background`

So I can:
have the box take the team's queued work items one after another, unattended, from behind NAT

### User Story 2:
As a: developer

I want to:
open the awman TUI, see "3 work items for you" in a queue panel, pick one, and watch the leader and its agents run in my tabs

So I can:
take work from my team's queue on my own machine, with my own credentials, and step in whenever I want

### User Story 3:
As a: work server author

I want to:
mark an item `required: [{key: "owner", op: "In", values: ["github:alice"]}]`

So I can:
make sure only Alice's linked machine can ever see or claim it

### User Story 4:
As a: developer

I want to:
`awman queue list` and `awman queue pick` (or `awman queue pick 01J…`) from a plain terminal

So I can:
take work without the TUI

## Implementation Details:

### Worker configuration and linking
- `awman worker link <server-url> [--name NAME] [--label k=v]... [--token TOKEN]` adds a server:
  - **Device flow (default).** `POST {server}/v1/link` returns `{user_code, verification_uri, link_id, interval}`. awman prints the code and URL (and opens a browser if it can), then polls `POST {server}/v1/link/{link_id}/token` until a person approves the link on the server's side. It receives a refresh token, stored in `<home>/worker/servers/<name>.json`, mode 0600. The server decides who may approve and which server-assigned labels the worker gets, for example `owner`.
  - **Static token** (`--token`, or `AWMAN_WORKER_TOKEN_<NAME>`) for servers that issue tokens out of band.
- `awman worker list|unlink <name>|labels <name> [--set k=v] [--unset k]`.
- Config:
  ```json
  "worker": {
    "servers": [
      { "name": "infact", "url": "https://infact.example/awman", "labels": { "pool": "laptops" },
        "claim": "auto", "maxConcurrent": 1, "interaction": "interactive", "enabled": true }
    ],
    "workRoot": "~/.awman/worker"
  }
  ```
  - `claim` is `auto` (claim and run in order) or `manual` (list only; a person picks).
  - The daemon defaults to `auto`.
  - A TUI- or CLI-hosted worker (below) is always `manual`.

### Labels and affinity
- **Worker labels** (`labels`) are a string map from three sources:
  1. **Reported automatically**, under the reserved `awman.dev/` prefix:
     - `os`, `arch`, `version`;
     - `runtime` (docker, apple, builtin);
     - `agent.<name>=true` for each agent with a working image;
     - `interaction.interactive` and `interaction.non-interactive`, as allowed by the worker mode's setting;
     - `host=tui` for a TUI-hosted worker, `host=daemon` otherwise.
  2. **Configured** by the worker's owner (`labels` above).
  3. **Assigned by the server** at link time or later (`server_labels` in responses).
- A worker cannot set server-assigned labels. Servers must apply identity and trust rules **only** to server-assigned labels, and matching uses all three sources.
- **Item affinity**: `{ "required": [Match…], "preferred": [{ "weight": 1–100, "match": [Match…] }] }`. A `Match` is `{ "key", "op": "In"|"NotIn"|"Exists"|"DoesNotExist", "values"? }`, with Kubernetes node-affinity semantics:
  - every `required` entry must hold;
  - `preferred` weights only order eligible items for a worker.
- The **server** evaluates affinity and only ever lists or hands out eligible items. awman re-checks `required` against its own reported labels and refuses (`release` with reason `ineligible`) anything that does not match. For example, it refuses an item needing `awman.dev/agent.codex=true` on a machine without codex.

### Work Server Protocol v1
All calls are from the worker to the server, under the server's base URL, with `Authorization: Bearer <access token>`. Access tokens come from `POST /v1/token {refresh_token}` and are short-lived. Every request carries `x-wsp-version: 1`. Errors are `{error, code}`: a message string and a snake_case code, the same shape as awman's own API errors (0138).

| Call | Purpose |
|---|---|
| `POST /v1/link`, `POST /v1/link/{id}/token`, `POST /v1/token` | Device-flow linking and token refresh |
| `POST /v1/workers/register` | `{name, labels, capacity, features}`, sent on start and when labels change. Returns `{worker_id, server_labels, heartbeat_secs, poll_secs}`. `features` lists what this awman supports (`terminal`, `interactive`, `prompt`, `workflow.dynamic`, `workflow.file`, `outputs`). |
| `GET /v1/queue?limit=50` | Items **eligible for this worker**, in the server's order (priority, then age, then preferred weight). Returns summaries: `{id, title, summary, kind, labels, priority, enqueued_at, assigned_to_me}`. Used by the picker. |
| `POST /v1/claims` | `{item_id?}`: with `item_id`, claim that item (manual pick); without it, the next eligible item. Supports `?wait=30` long-poll. Returns `201 {lease}`, `204` if nothing is eligible, or `409 taken`. |
| `POST /v1/leases/{id}/heartbeat` | `{state: "preparing"\|"running"\|"awaiting_input"\|"pushing", step?, progress?}` → `{expires_at, cancel: bool, cancel_reason?}`. `awaiting_input` means a step dialog or stuck agent is waiting for a person. Missing heartbeats past `expires_at` make the server re-queue the item. |
| `POST /v1/leases/{id}/events` | `{events: [ExecutionEvent…]}` with awman's own event JSON (including 0137's `source` attribution and `step_activity`, and 0146's terminal events), each with its `sequence` and `attempt`. Idempotent by `(attempt, sequence)`. |
| `GET /v1/leases/{id}/terminal` (WebSocket, `awman.terminal.v1`) | The worker opens this only if the lease has `terminal: true`. It multiplexes 0146's terminal frames for every terminal of the run, each tagged with `terminal_id`. The server fans them out to its own viewers. `input` frames flow back only when the lease grants `remote_input` **and** the worker's config allows it. |
| `POST /v1/leases/{id}/result` | The final result (below). Ends the lease. |
| `POST /v1/leases/{id}/release` | `{reason: "user_declined"\|"ineligible"\|"shutdown"\|"error", message?}` hands the item back unrun, or abandons it mid-run with the partial result attached. |

- **Fencing.** Each lease has an `epoch`. Once a lease expired and was re-issued, calls on the old lease get `409 lease_lost`, and the worker must stop that run (cancel its containers) and discard its result.
- **Lease** = `{lease_id, epoch, expires_at, item: WorkItem, terminal: bool, remote_input: bool}`.
- **`WorkItem`** (generic; servers fill what they need):
  ```json
  {
    "id": "01J…", "title": "Add OAuth login", "kind": "workflow" | "prompt",
    "body": "# Work Item: Feature\n…",            // markdown, used as the work item / issue text
    "source": { "url": "https://…/issues/42" },   // optional, shown to people
    "repo": {
      "url": "https://git.example/team/app.git",
      "base_ref": "main", "branch": "wsp/01J…", "merge_refs": ["wsp/01H…"],
      "credentials": { "kind": "bearer" | "basic" | "none", "token": "…", "username": "…", "expires_at": "…" }
    },
    "exec": {
      "dynamic": true, "leader": "claude::claude-opus-5-5",   // workflow kind
      "workflow_toml": null,                                   // or an inline workflow (file mode)
      "agent": "claude", "model": null,                        // prompt kind
      "read_only": false,                                      // 0143
      "overlays": [], "interaction": "interactive" | "non-interactive" | null,
      "yolo": true,                                            // false: step dialogs wait for a person (manual picks)
      "timeout_secs": 7200
    },
    "push": { "enabled": true, "branch": "wsp/01J…" },
    "outputs": { "max_bytes": 1048576 },                       // 0137 outputs dir returned in the result
    "labels": { "project": "lantern" }, "priority": 0,
    "affinity": { "required": [], "preferred": [] }
  }
  ```
- **Result** = `{result: CommandResult, outputs?: {path: base64}, duration_secs}`. `result` is 0137's `CommandResult`, embedded unchanged (`outcome`, `failure{stage, code, message}`, `failed_steps`, `git`, `pushed`, `pull_request`, `final_message`), so API clients and work servers decode one shape. `outputs` inlines the 0137 outputs directory up to `outputs.max_bytes`. WSP does not define a second status vocabulary (`succeeded|failed`).

### Running a lease
- The worker runs each lease in a fresh clone at `<workRoot>/leases/<lease_id>/`, using 0133's remote-session machinery:
  1. clone `repo.url` with the lease's credentials, through a one-shot credential helper, never in the URL or argv (0141);
  2. check out `branch` from `base_ref` and merge `merge_refs`;
  3. run `exec workflow --dynamic`, with the work item passed in as `body` (the 0139 inline source, so no file is committed), or `exec prompt`;
  4. push `push.branch` on success;
  5. post the result;
  6. delete the clone.
- The run gets the same per-mode interaction setting as any other daemon command (0146), unless `exec.interaction` overrides it and the mode allows overrides.
- **Uninitialised repositories.** If the clone has no `.awman/Dockerfile.<agent>` for an agent the run needs, the worker provisions it the way `awman init` does (headless defaults, 0144) inside the clone, and does not push it unless the item asks to. A work server never needs the repository to be awman-initialised.
- **Capacity.** A worker claims while `running < maxConcurrent` for that server and the daemon's `workers` cap (0145) has room. `auto` claims long-poll so a new item starts within a second of being queued. Items start in the order the server hands them out, which is the queue order; the worker never reorders.
- **Heartbeats** every `heartbeat_secs` (default 15). A `cancel: true` answer cancels the run (0135 semantics), and the worker posts the partial result as `cancelled`.

### TUI and CLI
- **TUI-hosted worker.** `awman --worker` or `tui.worker.enabled: true` makes the TUI register a `manual` worker per configured server, with `awman.dev/host=tui`, for the TUI's lifetime. If a daemon with worker mode is already running for that server, the TUI does not register a second worker. It uses the daemon's registration through the control listener (0145), and manual picks then run **in the TUI process**, with the daemon proxying heartbeats, events and the terminal.
- **Queue panel.**
  - `Ctrl-Q` opens a list of eligible items: title, server, labels, priority, age, and "for you" when an `owner` affinity names this worker's owner.
  - New items raise a badge in the status bar.
  - Enter shows the item's body, and `p` picks it. The claim opens a new tab running the workflow exactly as `exec workflow --dynamic` would, interactive by default, with the usual Workflow Overview and step dialogs. The tab title carries the item's title and server.
  - Closing the tab mid-run asks whether to release the item back to the queue or cancel it.
- **CLI.**
  - `awman queue list [--server NAME] [--json]`.
  - `awman queue pick [ITEM_ID]`: with no ID, an interactive picker (arrow keys, `/` to filter) when stdin is a TTY, otherwise an error that asks for an ID. It runs the item in the foreground like `exec workflow`.
  - `awman queue release <lease>`.
- **Running in your own checkout.** When a picked item's `repo.url` matches the `origin` of the repository the TUI or CLI was started in, awman offers to run it in a worktree of that checkout instead of a fresh clone. That matches what `exec workflow --dynamic` would have done, so the person can inspect and continue locally. The result is still pushed to `push.branch` and reported.

## Edge Case Considerations:
- Two workers claim the same item: the server serialises claims, and the loser gets `409 taken`. In `auto` mode it immediately asks for the next item.
- The worker goes offline mid-run: heartbeats stop, the lease expires, and the server re-queues the item. If the worker comes back and calls with the old lease, it gets `409 lease_lost` and stops the run. Results are never accepted twice.
- Token expiry mid-run: the worker refreshes. If refresh is refused (the link was revoked), it cancels running leases and stops claiming from that server.
- A server lists an item the worker cannot run (the label re-check fails): the worker releases it with `ineligible`, and the server should then exclude this worker for that item.
- Clone credentials expire during a long run: push uses credentials from a fresh `POST /v1/leases/{id}/credentials` call, which servers must support when they issue expiring credentials.
- Manual pick of an item that is no longer eligible (its labels changed): 409 with `ineligible`, and the TUI refreshes the list.
- Daemon shutdown with leases running: release them with `shutdown` and the partial state, unless `daemon stop` waits for them (the default waits, and `--now` releases).

## Test Considerations:
- Protocol conformance suite in a new crate, `awman-wsp`, shared by awman, the reference server (0148) and third parties. It holds the types, a mock server and a mock worker: linking, register, queue filtering by affinity (`In`, `NotIn`, `Exists`, `DoesNotExist`, required plus preferred ordering), claim races, heartbeat expiry and fencing, idempotent events, and the result.
- Worker end to end against the mock server, with a local bare repo over smart HTTP:
  - an `auto` worker runs three items in order;
  - `maxConcurrent: 2` runs two at once;
  - the pushed sha appears in the result;
  - a cancel by heartbeat stops containers.
- TUI picker snapshot tests, `queue pick` with and without a TTY, and the "run in my checkout" path.
- Security: a worker cannot set server-assigned labels, credentials never appear in argv, logs or `.git/config`, and remote input is refused unless both sides allow it.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- New `crates/awman-wsp` (protocol types, client, conformance tests); `src/engine/worker/` (claim loop, lease runner); worker mode as a `DaemonMode` (0145); TUI queue panel under `src/frontend/tui/`; catalogue entries `worker *` and `queue *` (not `api_allowed`).

## Documentation
- New `docs/17-workers-and-work-servers.md`: linking, labels, affinity, claim modes, the picker, and the protocol reference for server authors. Update `docs/02-using-the-tui.md` (queue panel), `docs/14-command-reference.md`, `docs/16-daemon.md`.
