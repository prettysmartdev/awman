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
| Work item numbers resolved by directory order | Run refused if two files share the number | 0138 |

## What infact still works around (see the infact work items)

| infact workaround | awman proposal | infact work items it simplifies |
|---|---|---|
| Commit the node spec onto `infact/wi-NNNN` so awman can read it | 0138 (HTTP issue source, `--issue` in dynamic mode, inline body) | 0004, 0009, 0012 |
| Downstream runs need upstream work: infact merges finished nodes into the default branch | 0133 (`base_ref`, `merge_refs`) as an alternative strategy | 0020 |
| Generated workflows must not push; infact publishes and opens PRs | 0133 (`push: true`, `push_branch -u` fix) | 0004 |
| Files changed and pushed commit computed by infact with git; failed steps from events | 0137 (structured `result`) | 0009, 0012 |
| `<<<INFACT_OPS>>>` sentinels parsed from stdout | 0137 (outputs directory) | 0008 |
| Cancel means close the session, keep the workspace leased and ignore the late result | 0135 (`/cancel`) | 0006, 0009 |
| Reattach after an infact restart from ids infact saved itself | 0135 (labels, idempotency) | 0009 |
| `wait_ready` polling, 409 handling, always-close bookkeeping | 0135 (`POST /v1/runs`) | 0009, 0018 |
| `sequence` resets on re-queue after an awman restart | 0136 | 0019 |
| Project memory in infact's data dir via a `dir()` overlay (`context(repo)` would be keyed by the workspace, not the project) | 0139 (context API, `context_key`) | 0008, 0010 |
| Tokens in a one-shot credential helper inside infact's workspace | 0140 | 0004, 0005 |
| One host on the same filesystem as infact; shared network namespace in compose | 0141 (bind address, capacity), 0133 | 0010, 0014, 0015 |
| Workspace discarded and re-cloned so the planning agent's edits don't persist | 0142 | 0008 |

## Priority for infact

0. **0132** is a small bug fix (one headless answer plus one engine condition) that unblocks every node run; without it, API-mode dynamic workflows abort after their last step. It also adds a dirty-target check and an agent-assisted fix for worktree merge-back.
1. **0133** removes the workspace pool, the same-host requirement and most of 0015's compose contortions.
2. **0134** is small and makes local sessions safe on their own; it is useful even before 0133.
3. **0135** and **0136** make runs cancellable and restarts boring.
4. **0137** and **0138** remove the remaining parsing and file-committing.
5. **0139**, **0140**, **0141** and **0142** matter for multiple hosts, GitHub at scale and polish.

## Added after the first real runs

| Proposal | What it fixes | Status |
|---|---|---|
| [0132](0132-api-finishes-yolo-workflows.md) | **Bug**: API-mode yolo workflows (every `--dynamic` run) abort after their last step and skip teardown; also a dirty-target check and an agent-assisted fix for worktree merge-back, and a working `GET /v1/workflows/{id}` (nothing wrote its file) with setup/teardown steps listed from the start | ready to implement |
| [0137](0137-structured-command-results.md) | Output events carry no step or container, and non-interactive agents emit only a final message (formerly 0143, now folded into 0137) | needed for exact per-agent output in infact's workflow view (0002) |
| [0145](0145-one-daemon-with-modes.md) | Squad and the API server are separate daemons that cannot run together | design: one daemon, any mix of squad, API and worker modes |
| [0146](0146-interaction-modes-and-live-terminals.md) | Squad is interactive-only, the API is non-interactive-only, and PTYs can only be attached locally | design: interaction per mode, a terminal hub with network viewers; needed for infact 0024 |
| [0147](0147-worker-mode-and-work-servers.md) | Nothing lets awman pull work; every client must reach awman's loopback port | design: worker mode, Work Server Protocol v1, label affinity, TUI queue picker; infact 0023 and 0025 build on it |
| [0148](0148-github-issues-work-server.md) | No work server exists to try 0147 with | design: reference server serving GitHub issues |
| [0151](0151-api-consistency-pass.md) | API audit: errors have no machine-readable code, per-command routes split across two roots, session records hide `setup_status`, docs contradict admission, dead OpenAPI projection | small consistency fixes; pairs with 0137 |
| [0144](0144-init-over-the-api.md) | `init` is not API-allowed, and dynamic workflows refuse agents without `.awman/Dockerfile.<agent>`, so a repository awman has never seen cannot run a node | worked around: infact runs `awman init` with the CLI in the workspace before a project's first node run |

## Worker mode (0145–0148): the pull model

These four proposals replace "infact pushes into an awman API server on its own host" with "awman workers pull from infact". awman gets one daemon with squad, API and worker modes (0145). Each mode is configurable as interactive or non-interactive, and live terminals are viewable over the network (0146). A worker mode pulls work items from any server speaking the Work Server Protocol, with label affinity and a queue picker in the TUI (0147). A reference GitHub-issues work server comes with it (0148).

On infact's side the matching items are 0023 (infact is a work server; worker mode is its only path to awman), 0024 (live agent terminals in the webapp) and 0025 (nodes taken on by a person in their own awman).

Once 0147 lands, proposals 0133, 0134 and 0138 no longer matter to infact; the worker uses 0133's clone machinery internally. The same goes for 0141, and for 0144's API route: workers provision missing agent Dockerfiles themselves. 0135, 0137, 0140 and 0142 are still needed; the WSP `cancel`, `result`, `credentials` and `read_only` fields are their worker-side form. Order: 0145 → 0146 → 0147 → 0148.
