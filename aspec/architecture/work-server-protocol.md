# Work Server Protocol (WSP) v1

How an awman worker gets work from a **work server**, runs it, and reports back. This is the design reference; `aspec/work-items/0143-worker-feature-and-work-servers.md` holds the full requirements, and `docs/17-workers-and-work-servers.md` is the user guide and field-by-field reference for server authors.

## 1. Roles and shape

- A **work server** keeps a queue of **work items** and decides which worker may have each one. infact and the GitHub issues reference server are implementations; anyone can write one.
- A **worker** is awman: the background daemon (claims automatically) or a TUI/CLI session (a person picks).
- **Every call is made by the worker.** The server never connects to the worker, so a worker behind NAT needs no inbound port. The one long-lived connection, the terminal WebSocket, is also opened by the worker.
- The protocol is plain HTTPS + JSON under the server's base URL. Loopback HTTP is allowed for local testing. The worker follows no redirects.

```
worker                                   work server
  │  link (once) ────────────────────────▶  a person approves
  │  token refresh ──────────────────────▶  short-lived access token
  │  register {labels, capacity, features}▶  {worker_id, server_labels}
  │  queue / claim ──────────────────────▶  lease {id, epoch, item}
  │  heartbeat, events, terminal ────────▶  (while the item runs)
  │  result  or  release ────────────────▶  lease ends
```

## 2. Wire conventions

- JSON bodies, `snake_case` fields, enum values as single words or `snake_case`.
- Durations are integer milliseconds in `_ms` fields; timestamps are RFC 3339 UTC strings in `_at` fields.
- Lists are `{items, next_cursor}` with `?limit=&cursor=`; cursors are opaque.
- Errors are `{error, code}`. Codes a worker acts on: `taken`, `ineligible`, `lease_lost`, `access_denied`, `expired`.
- Every request carries `x-wsp-version: 1`.
- Every lease-scoped request carries `x-wsp-epoch: <epoch>` (see Fencing).
- Labels the worker reports about itself use the reserved `awman/` prefix.

## 3. Authentication

There are three credentials, each with one job:

| Credential | Issued by | Held where | Used for |
|---|---|---|---|
| Refresh token | Server, at link time | `<data>/work_servers/<name>.json`, mode 0600. State, never config. | Getting access tokens. |
| Access token | `POST /v1/token` | Memory only | `Authorization: Bearer` on every other call. |
| Repo credentials | Inside the lease, or `POST /v1/leases/{id}/credentials` | A private per-lease file outside the clone | Git clone, fetch and push for that lease only. |

**Linking (device flow).** `awman worker link <url>`:

1. `POST /v1/link` → `{user_code, verification_uri, link_id, interval_ms}`. awman prints the code and URL and opens a browser when it can.
2. awman polls `POST /v1/link/{link_id}/token` every `interval_ms`. Pending is `202` or `204`, optionally with a new `interval_ms`.
3. A person approves on the server's side. The server decides who may approve and which **server-assigned labels** (for example `owner`) the worker gets.
4. The poll returns `200 {refresh_token}`. A refusal is `403 access_denied`; an expired link is `410 expired`.

`--token` (or `--token -` for stdin) stores a refresh token issued out of band and skips the flow. The link and token calls are unauthenticated: the token is the credential.

**Access tokens.** `POST /v1/token {refresh_token}` → `{access_token, expires_at?}`. On one `401` the worker refreshes and retries once. A refresh answered `401` or `403` means the link was **revoked**: the worker cancels that server's running leases, stops claiming from it, and reports the server as `revoked` until it is unlinked and linked again.

## 4. Registration, labels and affinity

`POST /v1/workers/register {name, labels, capacity, features}` → `{worker_id, server_labels, heartbeat_ms, poll_ms}`. It is sent on start and whenever the request would change.

- `features` lists what this worker can honour right now (`prompt`, `workflow_dynamic`, `workflow_file`, `terminal`, `interactive`, `outputs`, `lease_credentials`, `lease_read_only`, …). A worker never advertises what its runtime cannot do.
- **Labels come from three sources**, kept apart:
  1. reported by awman under `awman/` (`os`, `arch`, `version`, `runtime`, `host`, `agent.<name>`, `interaction.*`);
  2. configured by the owner (`workServers.<name>.labels`);
  3. assigned by the server (`server_labels`).
- A worker cannot set server-assigned labels or configure `awman/` labels. **Servers must base identity and trust only on server-assigned labels**; matching may use all three.
- **Affinity** on an item is `{required: [Match], preferred: [{weight, match}]}` with Kubernetes node-affinity semantics (`In`, `NotIn`, `Exists`, `DoesNotExist`). Every `required` term must hold; `preferred` only orders items.
- The server filters: it only lists or hands out eligible items. The worker re-checks `required` against its own labels after a claim and releases a mismatch as `ineligible`.

## 5. Getting work

- `GET /v1/queue` lists items eligible for this worker, in the server's order (priority, then age, then preferred weight). The picker uses it; rows never carry repository credentials.
- `POST /v1/claims {item_id?}` takes one item:
  - without `item_id`, the next eligible item; `?wait_ms=` (at most 30000) long-polls;
  - with `item_id`, that item (a manual pick).
  - Answers: `201 {lease}`, `204` nothing eligible, `409 taken` (another worker won; an automatic worker asks again at once), `409 ineligible`.
- **Claim modes.** A server entry is `auto` (the daemon claims and runs in queue order) or `manual` (list only; a person picks). TUI- and CLI-hosted workers are always manual.
- **Capacity.** A worker claims only with room under the server entry's `maxConcurrentJobs`, the worker feature's share and the host-wide limit. An automatic claim holds its execution slot while it long-polls, so a claimed item starts at once; the slot goes to a pushed job as soon as one waits for it. Pushed and pulled jobs alternate on the shared slots.

## 6. The lease

A claim returns `Lease {id, epoch, expires_at, item: WorkItem, terminal, remote_input}`. The lease is the worker's right to run the item until `expires_at`.

```
claimed ──▶ preparing ──▶ running ⇄ awaiting_input ──▶ pushing ──▶ result posted
   │            │              │                          │
   └────────────┴──────────────┴──────────────────────────┴──▶ released
                                                               or lease_lost
```

- **Heartbeat.** `POST /v1/leases/{id}/heartbeat {state, step?, progress?}` every `heartbeat_ms` (15 s if absent) and on every state change. The answer `{expires_at, cancel, cancel_reason?}` extends the lease. `cancel: true` cancels the run; the worker keeps heartbeating while it stops and then posts the partial result as `cancelled`.
- **Expiry.** If heartbeats stop, the lease passes `expires_at` and the server re-queues the item.
- **Fencing.** Each lease has an `epoch`, echoed in `x-wsp-epoch`. After a lease expired and was re-issued (with a new ID, or the same ID and a higher epoch), or after it ended, every call on the old lease gets `409 lease_lost`. The worker then stops the run, cancels its containers and discards the result. Results are never accepted twice.
- **Events.** `POST /v1/leases/{id}/events {events: [...]}` carries awman's own execution events, each with `attempt` and `sequence`. Delivery is at-least-once; the server stores each `(attempt, sequence)` once. `workflow_state` events carry complete workflow snapshots, so a follower can rebuild the latest state without polling.
- **Terminal.** If the lease has `terminal: true`, the worker opens `GET /v1/leases/{id}/terminal` as a WebSocket (`awman.terminal.v1`) and multiplexes every terminal of the run by `terminal_id`. Input flows back only when the lease grants `remote_input` **and** the worker's `daemon.terminal.remoteInput` allows it.
- **Ending.** Exactly one of:
  - `POST /v1/leases/{id}/result {result, outputs?, outputs_mounted, duration_ms}`. `result` is awman's `CommandResult`, the same shape push clients get; there is no second status vocabulary.
  - `POST /v1/leases/{id}/release {reason, message?, result?}` with reason `user_declined`, `ineligible`, `shutdown` or `error`. It hands the item back unrun, or abandons it mid-run with the partial result.

## 7. The work item

`WorkItem` is generic; servers fill what they need.

- Identity and display: `id`, `title`, `kind` (`workflow` | `prompt`), `body` (markdown, the work item text), `source.url`, `labels`, `priority`, `affinity`.
- `repo`: `url`, `base_ref`, `branch`, `merge_refs`, `credentials {kind: bearer|basic|none, token, username, expires_at}`.
- `exec`: `dynamic`/`leader` or an inline `workflow_toml` (workflow kind); `agent`/`model` (prompt kind); `read_only`, `overlays`, `interaction`, `yolo`, `timeout_ms`.
- `push {enabled, branch}` (`branch` defaults to `repo.branch`) and `outputs {max_bytes}` (default 1 MiB).

**Decoding rule.** Unknown fields on the lease, the item, `repo` and `push` are ignored, so servers can carry their own data. `exec`, `repo.credentials`, `outputs` and `affinity` are what the worker must act on, and are strict: an unknown field there, or any requirement the chosen runtime cannot honour, fails closed with `release(ineligible)` **before any work starts**. Fields are never silently ignored.

## 8. How a worker runs a lease

1. **Admit.** Re-check affinity, interaction policy, runtime support and capacity. Refuse with `release(ineligible)` if anything cannot be honoured.
2. **Clone** `repo.url` into `<workRoot>/leases/<lease_id>/`. Credentials reach Git only through a one-shot credential helper: never in the URL, argv, logs or `.git/config`. One transport configuration, with no system or user gitconfig, is used for clone, fetch, `ls-remote` and push.
3. **Check out** `branch` from `base_ref` and merge `merge_refs`.
4. **Prepare.** If a built-in agent has no `.awman/Dockerfile.<agent>` in the clone, generate it there (it is not pushed).
5. **Run** `exec workflow --dynamic` (or the inline workflow, or `exec prompt`) inside containers, through the same command path as the CLI and TUI. `body` is passed as a private owner-only temporary file; nothing is committed for it. `exec.read_only` mounts the workspace read-only.
6. **Push** `push.branch` on success. If the lease's repo credentials are near expiry, fetch fresh ones from `POST /v1/leases/{id}/credentials` first.
7. **Report** the result, then **delete** the clone (also on failure, cancel and panic).

A manual pick runs the same steps in the person's TUI or terminal, interactive by default. When the item's repository is the checkout awman was started in, awman offers a worktree of that checkout instead of a fresh clone.

**Interaction** is the pull default (`daemon.features.worker.pull.interaction`), or the server entry's, unless `exec.interaction` overrides it and `allowOverride` permits.

## 9. Failure handling

| Situation | What happens |
|---|---|
| Two workers claim one item | The server serialises; the loser gets `409 taken`. |
| Worker goes offline mid-run | Lease expires, item is re-queued; the returning worker gets `409 lease_lost` and stops. |
| Server asks to cancel | Heartbeat returns `cancel: true`; partial result posted as `cancelled`. |
| Link revoked | Token refresh refused; running leases cancelled, claiming stops, server shown as `revoked`. |
| Item the worker cannot run | `release(ineligible)`; the server should stop offering it to that worker. |
| Worker's own policy or capacity changed during the claim | `release(user_declined)`; the item stays eligible for this worker. |
| Git authentication fails | `failure.stage: "git_auth"`, `code: "git_auth_failed"`; never retried. |
| Push cannot reach the remote | Retried only when no remote response was received, with short bounded backoff inside the lease's validity; then `push_unreachable`. An already-published head counts as success. |
| Daemon stops | Default waits for leases; `--now` releases them with `shutdown` and the partial result. |
| Server unlinked with leases running | Claiming stops at once; leases finish and report, then the token is deleted. |

## 10. Security properties

- Agents and work-item code run only inside containers, never on the host.
- The clone is the only directory mounted into the run; lease secrets live outside it.
- Tokens and repo credentials never appear in config, argv, logs or `.git/config`.
- A worker cannot give itself identity: only server-assigned labels carry trust.
- Remote terminal input needs consent from both sides (lease and worker config).
- A lost or expired lease can change nothing: fencing covers heartbeats, events, credentials, result, release and the terminal.

## 11. Where the code lives

| Layer | Path | Owns |
|---|---|---|
| Protocol crate | `crates/awman-wsp` | Wire types, HTTP client, mock server, mock worker, conformance suite. No worker policy. |
| Layer 0 | `src/data/worker/` | Job and lease records, labels, link/token store. |
| Layer 1 | `src/engine/worker/` | WSP session, heartbeats, event delivery, lease workspace, push retry, terminal relay. |
| Layer 2 | `src/command/worker/` | Claim and admission policy, lease runner, manual picks, linking. |
| Layer 3 | `src/frontend/api`, `src/frontend/tui`, `src/frontend/cli` | `/v1/worker/*` routes, the queue panel, `awman queue`. |

A server author can run the conformance suite against any base URL: implement `ConformanceAdmin` (fixture provisioning and link approval) and call `awman_wsp::conformance::run(base_url, &admin)`.
