# awman work items

The tables below give implementation order. Active items after 0143 are numbered in that order; completed [0149](completed/0149-workflow-teardown-hardening.md), [0150](completed/0150-workflows-dynamic-config-migration.md), and [0151](completed/0151-branch-pr-worktree-for-chat.md) keep their shipped historical identifiers. The gap between 0148 and 0152 is intentional.

Every active item from 0139 onward uses [the grand architecture](../architecture/2026-grand-architecture.md) as an acceptance gate. Exchangeable types, configuration resolution and filesystem/database access belong to Layer 0; reusable runtime and transport mechanics to Layer 1; Session-based command behavior, catalogue definitions and policy to Layer 2; input and presentation adapters to Layer 3. Lower layers never import higher layers, and frontends never call engine behavior directly. New command paths must preserve builtin/daemon and CLI/TUI/API parity where the command is available. `make architecture-lint` and the relevant parity tests must pass before an item closes; its own layer contract in Codebase Integration specifies the concrete boundary.

The standalone reference server in 0153 applies those separation principles within its own package. It adds no awman command path or frontend parity obligation.

## 0139–0143: complete the daemon

These five items form **one completion gate**. Finish the single process, full config and API migration, local and network listeners, terminals, agent status, TUI/CLI engine, and worker push and pull before starting any later active item. WI0143 closes the gate only after the end-to-end daemon and 0139 surface audits pass. Lease credentials, inline bodies, read-only execution, ordered workflow-state events and lease push transport retries are part of that gate; later items extend or audit the push side. No legacy squad or API daemon remains, and no core daemon capability in this plan is deferred to a later work item. Later API proposals add product features on top of that completed foundation.

| Order | Work item | Scope |
|---|---|---|
| 1 | [0139 — Config and API foundation](0139-config-and-api-surface.md) | One config tree, generic overrides, resource route contract, CLI names and current-surface migration. WI0143 audits the final daemon surface. |
| 2 | [0140 — Single daemon and network API](0140-one-daemon-network-api.md) | One database owner and feature supervisor; squad and worker push together; local and TLS network listeners, hostname verification, capacity and key rotation. |
| 3 | [0141 — Terminals, interaction, status and hygiene](0141-interaction-terminals-status-and-hygiene.md) | Shared terminal hub for every feature, remote viewing and input grants, per-feature interaction, OSC 7501 status, safe host-terminal output and environment handling. |
| 4 | [0142 — Singleton engine and frontends](0142-engine-and-frontends.md) | Daemon-hosted instances, frontend protocol, TUI and CLI hand-off, dialog relay and builtin/daemon parity. Host and frontends are one acceptance gate. |
| 5 | [0143 — Worker push and pull](0143-worker-feature-and-work-servers.md) | One job model, Work Server Protocol, label affinity, lease-side execution minimum and the TUI queue picker; closes the full daemon gate. |

The five-item split keeps each reviewable while avoiding separate work items for a server with no frontend, a terminal bridge without its safety and status rules, or a config migration stranded after the daemon. The GitHub work server is a later standalone reference source in this repository; it is built explicitly from a clone and is not part of awman's binary or releases.

## After the complete daemon

Every item below depends on the completed 0139–0143 daemon gate. Additional dependencies are in each spec.

| Order | Work item | Scope |
|---|---|---|
| 6 | [0144 — Complete Rust test rewrite, CI and release rebuild](0144-rust-tests-ci-release-rebuild.md) | Ground-up rewrite of all Rust tests, then trustworthy CI gates and promoted release artifacts. |
| 7 | [0145 — Git transport, retry, credentials and secrets](0145-git-transport-retry-credentials-and-secrets.md) | Fix push-side Git transport and auth classification, add bounded push retry, then per-session credentials and per-command secrets. |
| 8 | [0146 — Context API and workflow artifacts](0146-context-api-and-workflow-artifacts.md) | Keyed durable repo context and per-run workflow notes over the push API and WSP. |
| 9 | [0147 — External work item sources](0147-external-work-item-sources.md) | HTTP and inline work items. |
| 10 | [0148 — Init over the API](0148-init-over-the-api.md) | API-driven project setup. |
| 11 | [0152 — Read-only planning sessions](0152-read-only-planning-sessions.md) | Read-only workspaces for planning. |
| 12 | [0153 — GitHub issues work server](0153-github-issues-work-server.md) | Later standalone Work Server Protocol reference source, built separately from a repository clone. |
| 13 | [0154 — Rex integration](integrations/0154-rex-integration.md) | Present the finished daemon engine in Rex. |
| 14 | [0155 — Daemon work in Rex](integrations/0155-daemon-work-in-rex.md) | Present squad and worker work in Rex. |
| 15 | [0156 — Rex action pack](integrations/0156-rex-action-pack.md) | Palette actions over the finalized daemon API. |
| 16 | [0157 — OpenCode and Cursor builtins](0157-opencode-v2-support.md) | OpenCode 1.x/2.x and Cursor; retire builtin Gemini. |
| 17 | [0158 — Builtin runtime setup and errors](0158-builtin-ready-setup-and-errors.md) | Image-source setup and clear `ready`/Docker errors. |
| 18 | [0159 — Webapp v0](webapp/0159-webapp-v0.md) | Same-machine browser frontend; named daemon listeners keep engine on loopback and worker access separate. |

Other open work items [0120](0120-upstream-embedded-kernel-support.md), [0124](0124-builtin-linux-and-docker-gates.md) and [0129](0129-codebase-simplicity-and-rust-quality-review.md) retain their numbers and link to the new daemon items where relevant.
