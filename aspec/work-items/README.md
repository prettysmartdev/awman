# awman proposals from infact

These are draft work items for [awman](https://github.com/prettysmartdev/awman), written in awman's own template. They are numbered after awman's highest existing item, which was 0131 at HEAD `58a3d03e` (v0.12.0). Each one removes an infact workaround for an "awman-ism". Renumber them when filing upstream.

The findings below were verified against awman's source. Where noted, they were also checked against a real `awman api` server built from that commit: session setup, allowlist, argument parsing. The machine had no Docker, so no workflow ran end to end.

## What infact had to fix or work around (done)

| Problem in awman 0.12 | infact change | Proposal that removes the workaround |
|---|---|---|
| Dynamic workflows are refused in remote sessions | Node runs use local sessions on leased workspaces (`crates/infact-server/src/adapters/workspaces.rs`); infact pushes the squash-merged branch | 0133 |
| `--workdirs` allowlist is exact-match and fixed at startup | Fixed workspace pool; `infact awman-workdirs` prints the flags; startup check against `GET /v1/workdirs` | 0134 |
| Worktrees keyed by directory name and resumed silently in API mode | Every lease removes linked worktrees and `awman/*` branches | 0134 |
| `ready_summary` / `error` are objects | Client types fixed (`SessionStatus`, `SessionSetupError`) | none needed |
| `exec prompt`'s prompt is a greedy trailing positional | Flags, then `--`, then the prompt (checked against awman's parser) | none needed |
| `event: done` after 300 s while still queued; a queue message borrows the next sequence | `AwmanClient::follow_events` re-subscribes until status is terminal and dedupes by sequence | 0136 |
| `CommandStatus` events are workflow-only and arrive before the merge | Success comes from `GET /v1/commands/{id}/status` | 0137 |
| Work item numbers resolved by directory order | Run refused if two files share the number | 0139 |

## What infact still works around (see the infact work items)

| infact workaround | awman proposal | infact work items it simplifies |
|---|---|---|
| Commit the node spec onto `infact/wi-NNNN` so awman can read it | 0139 (HTTP work item source, `--work-item` from any source in dynamic mode, inline body) | 0004, 0009, 0012 |
| Downstream runs need upstream work: infact merges finished nodes into the default branch | 0133 (`base_ref`, `merge_refs`) as an alternative strategy | 0020 |
| Generated workflows must not push; infact publishes and opens PRs | 0133 (`push: true`, `push_branch -u` fix) | 0004 |
| Files changed and pushed commit computed by infact with git; failed steps from events | 0137 (structured `result`) | 0009, 0012 |
| `<<<INFACT_OPS>>>` sentinels parsed from stdout | 0137 (outputs directory) | 0008 |
| Cancel means close the session, keep the workspace leased and ignore the late result | 0135 (`/cancel`) | 0006, 0009 |
| Reattach after an infact restart from ids infact saved itself | 0135 (labels, idempotency) | 0009 |
| `wait_ready` polling, 409 handling, always-close bookkeeping | 0135 (`POST /v1/runs`) | 0009, 0018 |
| `sequence` resets on re-queue after an awman restart | 0136 | 0019 |
| Project memory in infact's data dir via a `dir()` overlay (`context(repo)` would be keyed by the workspace, not the project) | 0140 (context API, `context_key`) | 0008, 0010 |
| Tokens in a one-shot credential helper inside infact's workspace | 0141 | 0004, 0005 |
| One host on the same filesystem as infact; shared network namespace in compose | 0142 (bind address, capacity), 0133 | 0010, 0014, 0015 |
| Workspace discarded and re-cloned so the planning agent's edits don't persist | 0143 | 0008 |

## Priority for infact

0. **0132** is a small bug fix (one headless answer plus one engine condition) that unblocks every node run; without it, API-mode dynamic workflows abort after their last step. It also adds a dirty-target check and an agent-assisted fix for worktree merge-back.
1. **0133** removes the workspace pool, the same-host requirement and most of 0015's compose contortions.
2. **0134** is small and makes local sessions safe on their own; it is useful even before 0133.
3. **0135** and **0136** make runs cancellable and restarts boring.
4. **0137** and **0139** remove the remaining parsing and file-committing.
5. **0140**, **0141**, **0142** and **0143** matter for multiple hosts, GitHub at scale and polish.

## Added after the first real runs

| Proposal | What it fixes | Status |
|---|---|---|
| [0132](completed/0132-api-finishes-yolo-workflows.md) | **Bug**: API-mode yolo workflows (every `--dynamic` run) abort after their last step and skip teardown; also a dirty-target check and an agent-assisted fix for worktree merge-back, and a working `GET /v1/workflows/{id}` (nothing wrote its file) with setup/teardown steps listed from the start | ready to implement |
| [0137](completed/0137-structured-command-results.md) | Output events carry no step or container, and non-interactive agents emit only a final message (the former "Step-attributed, live output" proposal, now folded into 0137) | needed for exact per-agent output in infact's workflow view (0002) |
| [0145](0145-one-daemon-one-api-with-features.md) | Squad and the API server are separate daemons that cannot run together | design: one daemon with one API and three independently switchable features: `squad` (unchanged), `worker` (push and pull jobs) and `engine` (a singleton awman instance); local and network listeners; ephemeral enablement; a minimal `awman daemon start|stop|status|logs|api` surface (settings through `awman config set daemon.…`, 0159) with no compatibility spellings |
| [0146](0146-interaction-modes-and-live-terminals.md) | Squad is interactive-only, the API is non-interactive-only, and PTYs can only be attached locally | design: interaction per feature, a terminal hub in the daemon core with network viewers, `awman attach --read-only`; needed for infact 0024 |
| [0147](0147-worker-feature-and-work-servers.md) | Nothing lets awman pull work; every client must reach awman's loopback port | design: the `worker` feature runs pushed jobs (today's API and `awman remote`) and pulled jobs (Work Server Protocol v1, label affinity, TUI queue picker) under one job model; infact 0023 and 0025 build on it |
| [0148](0148-github-issues-work-server.md) | No work server exists to try 0147 with | design: reference server serving GitHub issues |
| [0138](completed/0138-api-consistency-pass.md) | API audit: errors have no machine-readable code, per-command routes split across two roots, session records hide `setup_status`, docs contradict admission, dead OpenAPI projection | small consistency fixes; pairs with 0137 |
| [0144](0144-init-over-the-api.md) | `init` is not API-allowed, and dynamic workflows refuse agents without `.awman/Dockerfile.<agent>`, so a repository awman has never seen cannot run a node | worked around: infact runs `awman init` with the CLI in the workspace before a project's first node run |

## Config and API surface (0159)

| Proposal | What it fixes | Status |
|---|---|---|
| [0159](0159-config-and-api-surface.md) | Config keys mix casings, units and scopes; one setting has three differently named override channels; the API has aliases, duplicates and two daemons answering the same paths | design: naming rules for every surface, one config tree with a single key table, generic `--set` and derived `AWMAN_*` overrides, and a resource-oriented route tree with one prefix per daemon feature (`/v1/squad`, `/v1/worker`, `/v1/engine`). No compatibility layer. Every other open item uses its names, so it lands first. |

## The daemon and its features (0145–0148)

These proposals replace "infact pushes into an awman API server on its own host" with "awman workers pull from infact", and make the daemon the one long-lived awman process. awman is in beta, so this is a clean break: `awman api …` and `awman squad start|stop|status|logs` are removed with no aliases or config migrations, the daemon is driven by `awman daemon start|stop|status|logs|api`, and its settings go through `awman config set daemon.…` like every other setting (0159).
- **0145:** awman gets one daemon with one API and three features:
  - `squad`: today's squad, unchanged;
  - `worker`: runs jobs that are pushed (today's API and `awman remote`) or pulled (work servers);
  - `engine`: a singleton awman instance for TUIs, Rex and future frontends; see 0154–0155.
- **0146:** each feature's interaction is configurable, and live terminals are viewable over the network.
- **0147:** the worker's pull side speaks the Work Server Protocol, with label affinity and a queue picker in the TUI.
- **0148:** a reference GitHub-issues work server.

On infact's side the matching items are 0023 (infact is a work server; the worker's pull side is its only path to awman), 0024 (live agent terminals in the webapp) and 0025 (nodes taken on by a person in their own awman).

Once 0147 lands, proposals 0133, 0134 and 0139 no longer matter to infact; the worker uses 0133's clone machinery internally. The same goes for 0142, and for 0144's API route: workers provision missing agent Dockerfiles themselves. 0135, 0137, 0141 and 0143 are still needed; the WSP `cancel`, `result`, `credentials` and `read_only` fields are their worker-side form.

## The engine, frontends and Rex (0152–0158)

| Proposal | What it adds | Status |
|---|---|---|
| [0152](0152-ghb30-support-osc-7501-status-codes.md) | Agents report their own state through OSC 7501, which replaces silence-based stuck detection for agents that report | design |
| [0153](0153-rex-integration.md) | Rex integration: in Rex, awman uses the daemon `engine` and presents tabs as Rex windows, maximized agents as Rex blocks, and parallel steps as tiled windows, with parity with the normal TUI | design |
| [0154](0154-engine-feature-singleton-awman-instance.md) | Engine feature: a singleton awman instance for many frontends, with a per-instance environment and a parity contract | design |
| [0155](0155-frontend-protocol-tui-and-cli-as-engine-frontends.md) | Frontend protocol: TUI and CLI as engine frontends (`awman.frontend.v1`, dialog relay, hand-off, `tui.backend`) | design |
| [0156](0156-host-terminal-hygiene.md) | Host-terminal hygiene: output filter, OSC 7501 emission, env deny-list | design; independent, can ship first |
| [0157](0157-daemon-work-in-rex.md) | Daemon work in Rex: squad tasks and worker jobs in a dedicated Rex session | design |
| [0158](0158-rex-action-pack.md) | Rex action pack: awman actions in Rex's command palette, installed with the generic `awman integration setup rex`; actions use `awman daemon api` | design |

**Order:**
0. 0159, first: every other item uses its key and route names.
1. 0156, at any time.
2. 0145.
3. 0146, then 0147, then 0148.
4. 0152.
5. 0154, then 0155.
6. 0153.
7. 0157 and 0158.
