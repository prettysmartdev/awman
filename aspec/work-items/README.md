# awman work items

The tables below give implementation order. Work item numbers are stable identifiers; the completed [0149](completed/0149-workflow-teardown-hardening.md) and [0150](completed/0150-workflows-dynamic-config-migration.md) retain their historical numbers because shipped code and tests cite them. Newly filed 0163–0165 are placed at their dependency points without renumbering existing items or reusing the former 0159–0162 identifiers.

Every active item from 0139 onward uses [the grand architecture](../architecture/2026-grand-architecture.md) as an acceptance gate. Exchangeable types, configuration resolution and filesystem/database access belong to Layer 0; reusable runtime and transport mechanics to Layer 1; Session-based command behavior, catalogue definitions and policy to Layer 2; input and presentation adapters to Layer 3. Lower layers never import higher layers, and frontends never call engine behavior directly. New command paths must preserve builtin/daemon and CLI/TUI/API parity where the command is available. `make architecture-lint` and the relevant parity tests must pass before an item closes; its own layer contract in Codebase Integration specifies the concrete boundary.

The standalone reference server in 0145 applies those separation principles within its own package. It adds no awman command path or frontend parity obligation.

## 0139–0143: complete the daemon

These five items form **one completion gate**. Finish the single process, full config and API migration, local and network listeners, terminals, agent status, TUI/CLI engine, and worker push and pull before starting any later active item. WI0143 closes the gate only after the end-to-end daemon and 0139 surface audits pass. Lease credentials, inline bodies, read-only execution, ordered workflow-state events and lease push transport retries are part of that gate; later items extend or audit the push side. No legacy squad or API daemon remains, and no core daemon capability in this plan is deferred to a later work item. Later API proposals add product features on top of that completed foundation.

| Order | Work item | Scope | Moved from |
|---|---|---|---|
| 1 | [0139 — Config and API foundation](0139-config-and-api-surface.md) | One config tree, generic overrides, resource route contract, CLI names and current-surface migration. WI0143 audits the final daemon surface. | 0159 |
| 2 | [0140 — Single daemon and network API](0140-one-daemon-network-api.md) | One database owner and feature supervisor; squad and worker push together; local and TLS network listeners, hostname verification, capacity and key rotation. | 0145 + 0142 |
| 3 | [0141 — Terminals, interaction, status and hygiene](0141-interaction-terminals-status-and-hygiene.md) | Shared terminal hub for every feature, remote viewing and input grants, per-feature interaction, OSC 7501 status, safe host-terminal output and environment handling. | 0146 + 0152 + 0156 |
| 4 | [0142 — Singleton engine and frontends](0142-engine-and-frontends.md) | Daemon-hosted instances, frontend protocol, TUI and CLI hand-off, dialog relay and builtin/daemon parity. Host and frontends are one acceptance gate. | 0154 + 0155 |
| 5 | [0143 — Worker push and pull](0143-worker-feature-and-work-servers.md) | One job model, Work Server Protocol, label affinity, lease-side execution minimum and the TUI queue picker; closes the full daemon gate. | 0147 |

The five-item split keeps each reviewable while avoiding separate work items for a server with no frontend, a terminal bridge without its safety and status rules, or a config migration stranded after the daemon. The GitHub work server follows the daemon as standalone reference source in this repository; it is built explicitly from a clone and is not part of awman's binary or releases.

## After the complete daemon

Every item below depends on the completed 0139–0143 daemon gate. Additional dependencies are in each spec.

| Order | Work item | Scope | Moved from |
|---|---|---|---|
| 6 | [0163 — Consistent host Git transport](0163-consistent-host-git-transport-and-auth-errors.md) | Audit push-side clone/push credential parity and classify Git authentication failures. | new |
| 7 | [0164 — Retry unreachable push-back](0164-retry-unreachable-push-back.md) | Recover a successful run from short transport outages; distinguish unreachable from rejected pushes. | new |
| 8 | [0144 — Test, CI and release rebuild](0144-test-ci-release-rebuild.md) | Fast feedback and promoted release artifacts. | 0162 |
| 9 | [0145 — GitHub issues work server](0145-github-issues-work-server.md) | Standalone Work Server Protocol reference source, built separately from a repository clone. | 0148 |
| 10 | [0146 — Context API and keys](0146-context-api-and-keys.md) | Keyed push context and server-owned context for pulled leases. | 0140 |
| 11 | [0165 — Workflow context artifacts](0165-workflow-context-artifacts.md) | Read per-run workflow notes over the push API and WSP. | new |
| 12 | [0147 — Per-session credentials and secrets](0147-per-session-credentials-and-secrets.md) | Credential and secret delivery over the finalized worker API. | 0141 |
| 13 | [0148 — External work item sources](0148-external-work-item-sources.md) | HTTP and inline work items. | 0139 |
| 14 | [0151 — Branch, PR and worktree for chat](0151-branch-pr-worktree-for-chat.md) | Chat git workflow. | 0151 |
| 15 | [0152 — Init over the API](0152-init-over-the-api.md) | API-driven project setup. | 0144 |
| 16 | [0153 — Read-only planning sessions](0153-read-only-planning-sessions.md) | Read-only workspaces for planning. | 0143 |
| 17 | [0154 — Rex integration](0154-rex-integration.md) | Present the finished daemon engine in Rex. | 0153 |
| 18 | [0155 — Daemon work in Rex](0155-daemon-work-in-rex.md) | Present squad and worker work in Rex. | 0157 |
| 19 | [0156 — Rex action pack](0156-rex-action-pack.md) | Palette actions over the finalized daemon API. | 0158 |
| 20 | [0157 — OpenCode and Cursor builtins](0157-opencode-v2-support.md) | OpenCode 1.x/2.x and Cursor; retire builtin Gemini. | 0160 |
| 21 | [0158 — Builtin runtime setup and errors](0158-builtin-ready-setup-and-errors.md) | Image-source setup and clear `ready`/Docker errors. | 0161 |

Other open work items [0120](0120-upstream-embedded-kernel-support.md), [0124](0124-builtin-linux-and-docker-gates.md) and [0129](0129-codebase-simplicity-and-rust-quality-review.md) retain their numbers and link to the new daemon items where relevant.
