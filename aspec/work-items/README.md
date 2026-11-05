# awman work items

Active work items are numbered in implementation order. The completed [0149](completed/0149-workflow-teardown-hardening.md) and [0150](completed/0150-workflows-dynamic-config-migration.md) retain their historical numbers because shipped code and tests cite them.

Every active item from 0139 onward uses [the grand architecture](../architecture/2026-grand-architecture.md) as an acceptance gate. Exchangeable types, configuration resolution and filesystem/database access belong to Layer 0; reusable runtime and transport mechanics to Layer 1; Session-based command behavior, catalogue definitions and policy to Layer 2; input and presentation adapters to Layer 3. Lower layers never import higher layers, and frontends never call engine behavior directly. New command paths must preserve builtin/daemon and CLI/TUI/API parity where the command is available. `make architecture-lint` and the relevant parity tests must pass before an item closes; its own layer contract in Codebase Integration specifies the concrete boundary.

## 0139–0143: complete the daemon

These five items form **one completion gate**. Finish the single process, full config and API migration, local and network listeners, terminals, agent status, TUI/CLI engine, and worker push and pull before starting any later active item. WI0143 closes the gate only after the end-to-end daemon and 0139 surface audits pass. No legacy squad or API daemon remains, and no core daemon capability in this plan is deferred to a later work item. Later API proposals add product features on top of that completed foundation.

| Order | Work item | Scope | Moved from |
|---|---|---|---|
| 1 | [0139 — Config and API foundation](0139-config-and-api-surface.md) | One config tree, generic overrides, resource route contract, CLI names and current-surface migration. WI0143 audits the final daemon surface. | 0159 |
| 2 | [0140 — Single daemon and network API](0140-one-daemon-network-api.md) | One database owner and feature supervisor; squad and worker push together; local and TLS network listeners, hostname verification, capacity and key rotation. | 0145 + 0142 |
| 3 | [0141 — Terminals, interaction, status and hygiene](0141-interaction-terminals-status-and-hygiene.md) | Shared terminal hub for every feature, remote viewing and input grants, per-feature interaction, OSC 7501 status, safe host-terminal output and environment handling. | 0146 + 0152 + 0156 |
| 4 | [0142 — Singleton engine and frontends](0142-engine-and-frontends.md) | Daemon-hosted instances, frontend protocol, TUI and CLI hand-off, dialog relay and builtin/daemon parity. Host and frontends are one acceptance gate. | 0154 + 0155 |
| 5 | [0143 — Worker push and pull](0143-worker-feature-and-work-servers.md) | One job model, Work Server Protocol, label affinity and the TUI queue picker; closes the full daemon gate. | 0147 |

The five-item split keeps each reviewable while avoiding separate work items for a server with no frontend, a terminal bridge without its safety and status rules, or a config migration stranded after the daemon. The reference GitHub work server is an external service and follows the daemon.

## After the complete daemon

Every item below depends on the completed 0139–0143 daemon gate. Additional dependencies are in each spec.

| Order | Work item | Scope | Moved from |
|---|---|---|---|
| 6 | [0144 — Test, CI and release rebuild](0144-test-ci-release-rebuild.md) | Fast feedback and promoted release artifacts. | 0162 |
| 7 | [0145 — GitHub issues work server](0145-github-issues-work-server.md) | Reference Work Server Protocol implementation. | 0148 |
| 8 | [0146 — Context API and keys](0146-context-api-and-keys.md) | Context access over the finalized worker API. | 0140 |
| 9 | [0147 — Per-session credentials and secrets](0147-per-session-credentials-and-secrets.md) | Credential and secret delivery over the finalized worker API. | 0141 |
| 10 | [0148 — External work item sources](0148-external-work-item-sources.md) | HTTP and inline work items. | 0139 |
| 11 | [0151 — Branch, PR and worktree for chat](0151-branch-pr-worktree-for-chat.md) | Chat git workflow. | 0151 |
| 12 | [0152 — Init over the API](0152-init-over-the-api.md) | API-driven project setup. | 0144 |
| 13 | [0153 — Read-only planning sessions](0153-read-only-planning-sessions.md) | Read-only workspaces for planning. | 0143 |
| 14 | [0154 — Rex integration](0154-rex-integration.md) | Present the finished daemon engine in Rex. | 0153 |
| 15 | [0155 — Daemon work in Rex](0155-daemon-work-in-rex.md) | Present squad and worker work in Rex. | 0157 |
| 16 | [0156 — Rex action pack](0156-rex-action-pack.md) | Palette actions over the finalized daemon API. | 0158 |
| 17 | [0157 — OpenCode and Cursor builtins](0157-opencode-v2-support.md) | OpenCode 1.x/2.x and Cursor; retire builtin Gemini. | 0160 |
| 18 | [0158 — Builtin runtime setup and errors](0158-builtin-ready-setup-and-errors.md) | Image-source setup and clear `ready`/Docker errors. | 0161 |

Other open work items [0120](0120-upstream-embedded-kernel-support.md), [0124](0124-builtin-linux-and-docker-gates.md) and [0129](0129-codebase-simplicity-and-rust-quality-review.md) retain their numbers and link to the new daemon items where relevant.
