# Work Item: Feature

Title: Worker feature: run pushed and pulled jobs (API, remote and work servers), with label affinity and a queue picker
Issue: issuelink

Depends on: 0139 (completed config and API foundation), 0140 (single daemon and worker push), 0141 (terminal, status and interaction foundation), 0142 (singleton engine and TUI/CLI frontends), completed 0133 (fresh-clone dynamic runs), completed 0137 (structured results).

## Daemon completion gate

The 0139–0143 block is one release gate. This item closes it only when the daemon runs squad, worker push and pull, and the singleton engine together on one process and one database; the local and network APIs, terminal hub, status, output filtering, frontend hand-off and key rotation meet their respective acceptance criteria; and 0139's full config, route, CLI, documentation and old-surface audit passes. No later active work item depends on an incomplete daemon slice.

## Summary:
- Today something outside awman must push work into it:
  - a person at the TUI or CLI;
  - a squad task;
  - an API client holding a key and reaching awman's loopback port.

  There is no way for awman to **go and get** work. So every API client has to run on the same machine, or tunnel in. A team cannot spread work across laptops and build boxes. A person cannot say "give me the next thing to do".
- Extend the **`worker`** daemon feature introduced by 0140 into one job model for two arrival paths:
  - **Push.** A client submits the job. 0140 already moves today's API server and `awman remote exec` onto the daemon under `/v1/worker/*` (0139 §5). This item folds those pushed commands into the unified job model. Push is served on the network listener when `worker` is listed in `daemon.network.features`, and always on the local listener.
  - **Pull.** The worker connects **out** to one or more **work servers**, registers with labels, and takes work items from a queue the server keeps.
    - **In the background daemon:** items are claimed and run **immediately, in queue order**, up to the worker's capacity.
    - **In the TUI or CLI:** the worker is optional and runs **manual** claims. Queued items appear in a list, and the person **chooses** one with an interactive picker. It then runs in their TUI exactly as if they had typed `exec workflow --dynamic` themselves.
- **One job model.** Pushed commands and pulled leases are both `Job { id, origin: Push{session_id, command_id} | Pull{server, lease_id, epoch}, spec, state }`. They share:
  - the executor share (`daemon.features.worker.maxConcurrentJobs`);
  - the event stream (execution events with 0137 envelopes);
  - terminals (0141);
  - results (0137's `CommandResult`).

  A push client and a work server see the same states and the same result shape.
- **Node affinity.** Each pulled item may carry label selectors, and the server offers it only to workers whose labels match. Labels the worker reports about itself (OS, agents, GPU) are kept apart from labels the server assigns (who owns the worker, which team it serves), so an item can be pinned to one person's machine.
- Define the **Work Server Protocol (WSP) v1**: plain HTTPS+JSON, worker-initiated only, and generic. It follows 0139's wire rules: `snake_case` fields, `{items, next_cursor}` lists, `{error, code}` errors, `_ms` durations, `_at` timestamps and `awman/` label prefixes. infact is one implementation and the GitHub issues reference server (0153) is another. Anyone can write their own.

## User Stories

### User Story 1:
As a: operator of a build box

I want to:
`awman worker link https://work.example.com` once, then `awman config set daemon.features.worker.enabled true` and `awman daemon start --background`

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
`awman queue` (to list) and `awman queue pick` (or `awman queue pick 01J…`) from a plain terminal

So I can:
take work without the TUI

## Implementation Details:

### The worker feature
- `worker` is a Layer 1 `DaemonFeature` (0140) whose typed Layer 2 operations are exposed by the Layer 3 API adapter at `/v1/worker`. It has two sides, each switchable on its own (live keys in 0139's table):
  - `daemon.features.worker.push.enabled`: the push routes;
  - `daemon.features.worker.pull.enabled`: the claim loop over `workServers`.

  `awman config set daemon.features.worker.enabled true` turns the feature on, persisted and applied live to a running daemon (0139 §4). Both sides start as configured. For example, `awman config set daemon.features.worker.pull.enabled false` switches one side off live.
- **Push** is today's API server code moved behind the feature. Its behaviour (admission, session FIFO, SSE, cancel, outputs, errors) is unchanged, at 0139's paths:
  - `GET /v1/worker/workdirs`, allowed by `daemon.features.worker.push.workDirs.{allow,roots}`;
  - `GET|POST /v1/worker/sessions`, `GET|DELETE /v1/worker/sessions/{id}`, `GET /v1/worker/sessions/{id}/queue`;
  - `GET|POST /v1/worker/commands`, `GET /v1/worker/commands/{id}`, `POST …/cancel`, `GET …/events` (SSE), `GET …/workflow`, `GET …/outputs[/{path}]`;
  - `POST /v1/worker/runs`.

  `awman remote …` uses these against the target chosen with `--remote <target>` (0139 `remote.targets`). There is no `awman api` command. The push routes are served on the network listener when `daemon.network.enabled` is on and `worker` is in `daemon.network.features` (0140).
- **Pull** is exposed on the daemon's own API (0139 §5), for the CLI, the TUI and `awman daemon api`:
  - `GET /v1/worker/servers`, `GET /v1/worker/servers/{name}`: connection state per `workServers` entry (registered, labels, server labels, last claim, error);
  - `GET /v1/worker/queue?server=…`: eligible items (what `awman queue` shows), as `{items, next_cursor}`;
  - `POST /v1/worker/queue/{item_id}/claim`: a manual pick;
  - `GET /v1/worker/leases`, `GET /v1/worker/leases/{id}`, `POST /v1/worker/leases/{id}/release`.
    - awman's lease resources have `lease_…` IDs and carry the server's own lease ID as `server_lease_id`.
- **Interaction** follows 0141: `daemon.features.worker.push.interaction` (default `headless`) and `daemon.features.worker.pull.interaction` (default `interactive`), each with `allowOverride`.
- **Status.** `/v1/status` reports `worker` in 0140's per-feature shape (`state`, `max_concurrent_jobs`, `running`, `queued`, `oldest_queued_age_ms`), plus `push: {running, queued}` and `pull: {servers: [{name, state, running, queued_eligible}]}`. `awman daemon status` shows the same.
- **`awman daemon api` actions** this item adds (0139 §6): `leases.list` and `leases.release id=…`. `jobs.cancel` (0140) covers worker jobs.
- **Code.** `src/data/worker/` and the pure type portion of `crates/awman-wsp` own serializable `Job`, lease and WSP types and durable state. `src/engine/worker/` owns protocol transport, heartbeats, executor mechanics and terminal streams without importing command or frontend modules; the WSP crate cannot contain worker policy or HTTP route construction. `src/command/worker/` owns push/pull admission, claim and retry policy, job construction through Dispatch, linking and manual-pick behavior. `src/frontend/api/` maps HTTP to typed Layer 2 operations; it does not own the worker runtime.

### Work servers: configuration and linking
- `awman worker link <server-url> [--name NAME] [--label k=v]... [--token TOKEN | --token -]` adds a server:
  - **Device flow (default).** `POST {server}/v1/link` returns `{user_code, verification_uri, link_id, interval_ms}`.
    - awman prints the code and URL, and opens a browser if it can.
    - It then polls `POST {server}/v1/link/{link_id}/token` until a person approves the link on the server's side.
    - It receives a refresh token, stored in `<data>/work_servers/<name>.json` (0600). Tokens are state, not config.
    - The server decides who may approve and which server-assigned labels the worker gets, for example `owner`.
  - **Static token** (`--token TOKEN`, or `--token -` to read it from stdin) for servers that issue tokens out of band.
  - `link` writes the `workServers.<name>` config entry (global scope): `url`, `enabled: true`, and any `--label`s. `--name` defaults to the server's host name, normalised to 0139's name rule.
- Linked servers are managed as ordinary config (0139 §4), with no extra subcommands:
  - list them with `awman config get workServers`;
  - change labels with `awman config set workServers.<name>.labels.<k> <v>`;
  - disable one with `awman config set workServers.<name>.enabled false`;
  - unlink with `awman config unset workServers.<name>`, which also deletes `<data>/work_servers/<name>.json`, so the stored refresh token goes with it.

  `awman worker link` is the only `worker` subcommand, because the device flow is interactive.
- Config (0139 §2; all keys `global`, live):
  ```json
  "workServers": {
    "infact": { "url": "https://infact.example/awman", "enabled": true, "claim": "auto",
                "maxConcurrentJobs": 1, "interaction": null, "labels": { "pool": "laptops" } }
  },
  "daemon": { "features": { "worker": { "pull": { "enabled": true, "interaction": "interactive",
                                                  "allowOverride": true, "workRoot": null } } } }
  ```
  - `claim` is `auto` (claim and run in order) or `manual` (list only; a person picks). The daemon uses the entry's `claim`. A TUI- or CLI-hosted worker (below) is always `manual`.
  - `interaction: null` means the pull default. `interactive` or `headless` overrides it for this server's items, still subject to `allowOverride`.
  - `workRoot: null` means `<data>/worker`.

### Labels and affinity
- **Worker labels** (`labels`) are a string map from three sources:
  1. **Reported automatically**, under the reserved `awman/` prefix (0139 §1):
     - `awman/os`, `awman/arch`, `awman/version`;
     - `awman/runtime` (`docker`, `apple`, `sbx`, `builtin`; 0139's `runtime.kind` values);
     - `awman/agent.<name>=true` for each agent with a working image;
     - `awman/interaction.interactive` and `awman/interaction.headless`, as allowed by the pull interaction setting and its `allowOverride`;
     - `awman/host=tui` for a TUI-hosted worker, `awman/host=daemon` otherwise.
  2. **Configured** by the worker's owner (`workServers.<name>.labels`).
  3. **Assigned by the server** at link time or later (`server_labels` in responses).
- A worker cannot set server-assigned labels, or configure labels under `awman/`. Servers must apply identity and trust rules **only** to server-assigned labels, and matching uses all three sources.
- **Item affinity**: `{ "required": [Match…], "preferred": [{ "weight": 1–100, "match": [Match…] }] }`. A `Match` is `{ "key", "op": "In"|"NotIn"|"Exists"|"DoesNotExist", "values"? }`, with Kubernetes node-affinity semantics:
  - every `required` entry must hold;
  - `preferred` weights only order eligible items for a worker.
- The **server** evaluates affinity and only ever lists or hands out eligible items. awman re-checks `required` against its own reported labels and refuses (`release` with reason `ineligible`) anything that does not match. For example, it refuses an item needing `awman/agent.codex=true` on a machine without codex.

### Work Server Protocol v1
- **Requests.** All calls are from the worker to the server, under the server's base URL, with `Authorization: Bearer <access token>`.
  - Access tokens come from `POST /v1/token {refresh_token}` and are short-lived.
  - Every request carries `x-wsp-version: 1`.
- **Conventions:** 0139 §1's wire conventions.
  - Errors are `{error, code}`, the same shape and code style as awman's own API.
  - Lists are `{items, next_cursor}` with `?limit=&cursor=`.
  - Durations are `_ms` integers and timestamps are `_at` RFC 3339 strings.
  - Enum values are single words or `snake_case`.

| Call | Purpose |
|---|---|
| `POST /v1/link`, `POST /v1/link/{id}/token`, `POST /v1/token` | Device-flow linking and token refresh. |
| `POST /v1/workers/register` | `{name, labels, capacity, features}`, sent on start and when labels change. Returns `{worker_id, server_labels, heartbeat_ms, poll_ms}`. `features` is a subset of status `capabilities` using the same names, including `terminal`, `interactive`, `prompt`, `workflow_dynamic`, `workflow_file`, `outputs`, `outputs_mount_state`, `workflow_state_events`, `lease_credentials`, `lease_inline_work_item`, `lease_read_only`, `lease_git_auth_failure`, and `lease_push_retry` only when the worker can honor them for eligible runtimes. Re-check each claimed item against its actual runtime. |
| `GET /v1/queue?limit=50&cursor=…` | Items **eligible for this worker**, in the server's order (priority, then age, then preferred weight). Returns `{items: [{id, title, summary, kind, labels, priority, enqueued_at, assigned_to_me}], next_cursor}`. Used by the picker. |
| `POST /v1/claims` | `{item_id?}`: with `item_id`, claim that item (manual pick); without it, the next eligible item. Supports long-polling with `?wait_ms=30000`. Returns `201 {lease}`, `204` if nothing is eligible, or `409 taken`. |
| `POST /v1/leases/{id}/heartbeat` | `{state: "preparing"\|"running"\|"awaiting_input"\|"pushing", step?, progress?}` → `{expires_at, cancel: bool, cancel_reason?}`. `awaiting_input` means a step dialog or stuck agent is waiting for a person. Missing heartbeats past `expires_at` make the server re-queue the item. |
| `POST /v1/leases/{id}/events` | `{events: [ExecutionEvent…]}`, using awman's own event JSON (including 0137's `source` attribution and `step_activity`, 0141's terminal events, and `workflow_state` snapshots), each with its `sequence` and `attempt`. Idempotent by `(attempt, sequence)`. |
| `GET /v1/leases/{id}/terminal` (WebSocket, `awman.terminal.v1`) | The worker opens this only if the lease has `terminal: true`. It multiplexes 0141's terminal frames for every terminal of the run, each tagged with `terminal_id`. The server fans them out to its own viewers. `input` frames flow back only when the lease grants `remote_input` **and** `daemon.terminal.remoteInput` allows it. |
| `POST /v1/leases/{id}/result` | The final result (below). Ends the lease. |
| `POST /v1/leases/{id}/release` | `{reason: "user_declined"\|"ineligible"\|"shutdown"\|"error", message?}` hands the item back unrun, or abandons it mid-run with the partial result attached. |
| `POST /v1/leases/{id}/credentials` | Fresh repo credentials for a long run (see Edge Cases). |

- **Fencing.** Each lease has an `epoch`. Once a lease expired and was re-issued, calls on the old lease get `409 lease_lost`, and the worker must stop that run (cancel its containers) and discard its result.
- **Lease** = `{id, epoch, expires_at, item: WorkItem, terminal: bool, remote_input: bool}`.
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
      "read_only": false,                                      // lease minimum here; 0152 generalizes
      "overlays": [], "interaction": "interactive" | "headless" | null,
      "yolo": true,                                            // false: step dialogs wait for a person (manual picks)
      "timeout_ms": 7200000
    },
    "push": { "enabled": true, "branch": "wsp/01J…" },
    "outputs": { "max_bytes": 1048576 },                       // 0137 outputs dir returned in the result
    "labels": { "project": "lantern" }, "priority": 0,
    "affinity": { "required": [], "preferred": [] }
  }
  ```
- **Result** = `{result: CommandResult, outputs?: {path: base64}, outputs_mounted: bool, duration_ms}`.
  - `result` is 0137's `CommandResult`, embedded unchanged (`outcome`, `failure{stage, code, message}`, `failed_steps`, `git`, `pushed`, `pull_request`, `final_message`), so push clients and work servers decode one shape.
  - `outputs` inlines the 0137 outputs directory up to `outputs.max_bytes`.
  - `outputs_mounted` reports whether `/awman/output` was actually mounted for this command. `outputs` may be advertised at registration only for runtimes that mount it; a request requiring outputs on an unsupported runtime is released as `ineligible`. Push command records report the same boolean. An empty output directory is distinguishable from a missing mount.
  - Advertise `outputs_mount_state` in `capabilities` in `/v1/status` and WSP `register.features` when the boolean is accurate for all commands; advertise `outputs` only for runtimes that can mount the directory.
  - WSP does not define a second status vocabulary (`succeeded|failed`).

- **Workflow-state events.** Whenever `workflow.state.json` is persisted, publish an `ExecutionEvent` with `type: "workflow_state"` and `data` equal to the complete `GET /v1/worker/commands/{id}/workflow` snapshot (phases, planned steps, status and `invocation_id`). It carries the normal `sequence` and `attempt` envelope. Both push SSE and WSP event delivery use the same ordered event log, including the initial and final snapshots. Advertise `workflow_state_events` in status and WSP registration. Followers can reconstruct the latest workflow state without polling.
- Advertise `worker_pull`, `workflow_dynamic`, `workflow_file`, `prompt`, and the applicable `terminal`/`interactive` capabilities in `GET /v1/status` as the contracts ship. WSP registration uses the same spellings and may omit capabilities unavailable to that worker. `lease_push_retry` is likewise present in status when the lease retry behavior ships.

### Running a lease
- The worker runs each lease in a fresh clone at `<workRoot>/leases/<lease_id>/`, using 0133's remote-session machinery:
  1. clone `repo.url` with the lease's credentials, through a one-shot credential helper, never in the URL or argv; this lease minimum ships here, before 0145 extends the helper to push sessions;
  2. check out `branch` from `base_ref` and merge `merge_refs`;
  3. run `exec workflow --dynamic`, with the work item passed in as `body` through a private temporary inline source (no file is committed), or `exec prompt`; 0147 later exposes the generalized inline source to push clients and CLI;
  4. push `push.branch` on success;
  5. post the result;
  6. delete the clone.
- **Lease-side minimum before the gate closes.** `repo.credentials`, `body`, and `exec.read_only` are executable contracts in this item. The shared host Git transport builder used for clone, setup fetches, merge-ref and push-back `ls-remote`, and push applies the same Git configuration and lease credential helper to every operation, with no system gitconfig; WI0145 verifies and repairs push-session parity. A private owner-only temp source carries `body` into the run. `exec.read_only` mounts the workspace read-only on every runtime used for leases, with writable scratch/agent home; 0152 exposes the general CLI and push-session option. If a runtime cannot enforce it, release the lease as `ineligible` and omit `lease_read_only` when registering for that runtime. Unsupported credential, inline-body, output, or execution requirements likewise fail closed before work starts, with `release(ineligible)`; do not ignore fields. Advertise `lease_credentials`, `lease_inline_work_item`, and `lease_read_only` in status and WSP registration only after their paths work.
- **Lease push reliability.** Retry only failures before a remote response with short bounded backoff, record each retry in the event stream, recognize an already-published `head` as success, and cancel during backoff. Bound the retry window by heartbeat and lease expiry; if the lease cannot remain valid, release or report the distinct `push_unreachable` failure, never continue on an expired epoch. Lease Git authentication failures use `failure.stage: "git_auth"` and `failure.code: "git_auth_failed"`; authentication and remote refusals are not retried. Advertise `lease_git_auth_failure` and `lease_push_retry` in status and WSP registration when they work; WI0145 applies the auth classification and extends and audits the retry policy for pushed sessions.
- **Interaction.** The run gets `daemon.features.worker.pull.interaction` (0141), or the server entry's `interaction`, unless `exec.interaction` overrides it and `allowOverride` permits it.
- **Uninitialised pulled clones.** During pull-lease workspace preparation, if a first-party agent the run needs has no `.awman/Dockerfile.<agent>`, Layer 2 invokes the existing `provision_agent_dockerfile` behavior with headless defaults inside that disposable clone, before dynamic preflight; Layer 0 performs the file write. It does not push the generated file unless the item asks to. Custom agents and local/push sessions with missing Dockerfiles fail preflight before the leader runs, naming the missing agent and `awman init --agent <name>` remedy. WI0148 makes that explicit init command available to API clients; no later item is required for pull leases to work.
- **Capacity.** A worker claims while all of these have room:
  - `running < workServers.<name>.maxConcurrentJobs`;
  - the worker feature's share, `daemon.features.worker.maxConcurrentJobs`;
  - the host-wide `daemon.maxConcurrentJobs` (0140).

  Pushed and pulled jobs draw on the same share. `auto` claims long-poll, so a new item starts within a second of being queued. Items start in the order the server hands them out, which is the queue order; the worker never reorders.
- **Heartbeats** every `heartbeat_ms` (the server's value; 15 s if absent). A `cancel: true` answer cancels the run (0135 semantics), and the worker posts the partial result as `cancelled`.

### TUI and CLI
- **TUI-hosted worker.** `tui.worker.enabled: true` (or `--set tui.worker.enabled=true` for one run, 0139 §3) makes a Layer 2 worker controller register a `manual` worker per enabled `workServers` entry, with `awman/host=tui`, for the TUI's lifetime. The TUI only presents queue data and returns a typed selection.
  - If the daemon's `worker` feature is already pulling from that server, the TUI does not register a second worker. It uses the daemon's registration through `/v1/worker/*` on the local listener (0140).
  - Where a manual pick runs:
    - **with the builtin engine**, in the TUI process, with the daemon proxying heartbeats, events and the terminal;
    - **when the TUI is an engine frontend** (0142, including every TUI in Rex), in the TUI's engine instance (0142). The lease is still held by the worker feature, and the instance's tab is the job's frontend.

    Either way the person sees the same tab, dialogs and overview.
- **Queue panel.**
  - `Ctrl-Q` opens a list of eligible items: title, server, labels, priority, age, and "for you" when an `owner` affinity names this worker's owner.
  - New items raise a badge in the status bar.
  - Enter shows the item's body, and `p` picks it. The claim opens a new tab running the workflow exactly as `exec workflow --dynamic` would, interactive by default, with the usual Workflow Overview and step dialogs. The tab title carries the item's title and server.
  - Closing the tab mid-run asks whether to release the item back to the queue or cancel it.
- **CLI** (0139 §7):
  - `awman queue [--server NAME] [--json]` lists eligible items (no subcommand).
  - `awman queue pick [<item-id>] [--json]`: with no ID, an interactive picker (arrow keys, `/` to filter) when stdin is a TTY, otherwise an error that asks for an ID. It runs the item in the foreground like `exec workflow`.
  - Releasing a lease is `awman daemon api leases.release id=<lease_…>` (→ `POST /v1/worker/leases/{id}/release`). There is no `queue release`.
- **Running in your own checkout.** When a picked item's `repo.url` matches the `origin` of the repository the TUI or CLI was started in, awman offers to run it in a worktree of that checkout instead of a fresh clone. That matches what `exec workflow --dynamic` would have done, so the person can inspect and continue locally. The result is still pushed to `push.branch` and reported.

## Edge Case Considerations:
- Two workers claim the same item: the server serialises claims, and the loser gets `409 taken`. In `auto` mode it immediately asks for the next item.
- The worker goes offline mid-run: heartbeats stop, the lease expires, and the server re-queues the item. If the worker comes back and calls with the old lease, it gets `409 lease_lost` and stops the run. Results are never accepted twice.
- Token expiry mid-run: the worker refreshes. If refresh is refused (the link was revoked), it cancels running leases and stops claiming from that server.
- A server lists an item the worker cannot run (the label re-check fails): the worker releases it with `ineligible`, and the server should then exclude this worker for that item.
- Clone credentials expire during a long run: push uses credentials from a fresh `POST /v1/leases/{id}/credentials` call, which servers must support when they issue expiring credentials.
- A lease field or required capability that the chosen runtime cannot honor is rejected before execution with `release(ineligible)`; the corresponding feature is omitted at registration when it cannot be offered at all.
- Manual pick of an item that is no longer eligible (its labels changed): `409 ineligible`, and the TUI refreshes the list.
- Daemon shutdown with leases running: release them with `shutdown` and the partial state, unless `awman daemon stop` waits for them (the default waits, and `--now` releases).
- `awman config set daemon.features.worker.enabled false` while jobs run: push submissions get `503 feature_disabled` and pull stops claiming. Running jobs of both kinds finish. `awman daemon api jobs.cancel feature=worker` cancels them, and pulled ones are released with their partial result.
- `awman config unset workServers.<name>` while that server has running leases: claiming stops at once. Running leases finish and report, then the token file is deleted.
- Push and pull at once under load: both draw on `daemon.features.worker.maxConcurrentJobs`. The executor alternates between pushed and pulled jobs, so a long push queue cannot stop a worker from honouring leases it has already claimed, and vice versa.

## Test Considerations:
- **Protocol conformance suite** in a new crate, `awman-wsp`, shared by awman, the reference server (0153) and third parties. It holds the types, a mock server and a mock worker, and covers:
  - linking and register;
  - queue filtering by affinity (`In`, `NotIn`, `Exists`, `DoesNotExist`, required plus preferred ordering);
  - claim races;
  - heartbeat expiry and fencing;
  - idempotent events;
  - the result;
  - 0139 wire conventions on every message (`items`/`next_cursor`, `_ms`, `_at`, `{error, code}`, `awman/` labels).
- **Worker end to end** against the mock server, with a local bare repo over smart HTTP:
  - an `auto` worker runs three items in order;
  - `maxConcurrentJobs: 2` runs two at once;
  - the pushed sha appears in the result;
  - lease credentials are used for clone, fetch, `ls-remote`, and push without system gitconfig or secret exposure; lease auth failures return `git_auth_failed`; inline body runs without a repo file; read-only denies workspace writes on each supported runtime;
  - workflow snapshots arrive in order on push SSE and WSP after each persisted state; unsupported lease requirements are released as `ineligible`; `outputs_mounted` reflects the actual runtime mount;
  - a lease push retries only pre-response transport failures within its expiry, logs retries, honors cancellation and never retries auth or remote rejection;
  - a cancel by heartbeat stops containers.
- **Config:**
  - `awman worker link` writes `workServers.<name>` and the token file;
  - `awman config unset workServers.<name>` removes both;
  - labels under `awman/` are refused in config;
  - names violating 0139's name rule are refused.
- **Daemon pull routes:** `/v1/worker/servers`, `/v1/worker/queue`, claim, `/v1/worker/leases` and release, with `lease_` IDs and `server_lease_id`. `leases.release` matches its `--raw` equivalent.
- **TUI and CLI:** picker snapshot tests, `queue pick` with and without a TTY, and the "run in my checkout" path.
- **Security:**
  - a worker cannot set server-assigned labels;
  - credentials never appear in argv, logs, config or `.git/config`;
  - remote input is refused unless both sides allow it.
- **Push regression:** today's API and `awman remote` e2e suites pass against `/v1/worker/*`, with only the paths changed. A pushed job and a pulled job run concurrently with one event stream shape and one `CommandResult` shape.

## Codebase Integration:
- Follow `aspec/architecture/2026-grand-architecture.md`: Layer 0 owns WSP/job data, Layer 1 transport/execution, Layer 2 worker policy and Dispatch, and Layer 3 HTTP/TUI projection. Pull, push and manual selection share one command path and never import upward.
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- **Protocol:** new `crates/awman-wsp` (protocol types, client, conformance tests), using 0139's shared wire helpers (`List<T>`, `ApiError`, duration and timestamp serde).
- **Layers:** `src/data/worker/` owns wire and persisted job/lease types; `src/engine/worker/` owns WSP transport and execution primitives; `src/command/worker/` owns push/pull policy and calls existing command constructors through Dispatch; `src/frontend/api/` binds the worker HTTP routes to Layer 2. The Layer 1 `DaemonFeature` has no router and never wraps `src/frontend/api/` or `src/command/`.
- Every worker command is instantiated from a Layer 0 `Session` through the same Layer 2 Dispatch/catalogue path as CLI and TUI commands. The WSP `Job` and lease describe scheduling; they are not a second command or session model. Conformance checks compare push, pull, manual and builtin outcomes for the same typed command.
- **Config:** `src/data/config/work_servers.rs` (the `workServers` section) and the `daemon.features.worker.*` rows in 0139's key table.
- **TUI and CLI:**
  - the TUI queue panel lives under `src/frontend/tui/`;
  - the catalogue entries are `worker link` and `queue` (with its optional `pick`), neither `api_allowed`;
  - `leases.list|release` go in the client-side action table, `src/command/commands/daemon/actions.rs` (0139 §6);
  - push commands keep their existing `api_allowed` rules under `FrontendKind::Daemon { feature: Worker, listener }`.

## Documentation
- New `docs/17-workers-and-work-servers.md`:
  - linking;
  - `workServers.*` config;
  - labels and affinity;
  - claim modes;
  - the picker;
  - the protocol reference for server authors.
- Update:
  - `docs/02-using-the-tui.md`: the queue panel and `tui.worker.enabled`;
  - `docs/14-command-reference.md`: `worker link` and `queue`;
  - `docs/16-daemon.md`: the worker feature, push and pull;
  - `docs/09-api-and-remote-mode.md`: API and remote are worker push, under `/v1/worker/*`.
