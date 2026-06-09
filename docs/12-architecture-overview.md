# Architecture Overview

A short tour of how awman is put together — enough to orient curious users and
would-be contributors. For the full reference, see [architecture.md](architecture.md).

## The Big Picture

awman is a single Rust binary organized into layers, with strict one-way
dependencies: each layer may only call into the layers below it.

```
frontend  →  command  →  engine  →  data
(TUI/CLI/API) (dispatch +   (containers,   (sessions, config,
               business     workflows,      filesystem,
               logic)       git, overlays)  database)
```

Whichever way you invoke awman — the interactive TUI (`awman` with no
arguments), a one-shot CLI command (`awman chat`, `awman exec workflow …`), or
the HTTP API (`awman api start`) — the path is the same:

1. A **frontend** collects your input. Frontends are presentation-only: they
   render output and prompt for choices, but contain no business logic.
2. The frontend hands the input to **command dispatch**, which owns the
   canonical catalogue of commands and flags and routes to the matching
   command handler. All business logic lives here.
3. Commands drive the **engines** — container lifecycle, workflow execution,
   git operations, overlays, and agent/auth management.
4. Engines read and write state through the **data** layer: sessions, merged
   configuration, files under `.awman/` and `~/.awman/`, and the API
   server's SQLite session store.

Because every frontend funnels into the same dispatch layer, the same command
behaves identically in the TUI, on the command line, and over the API. And per
awman's core security rule, agents themselves never run on your host — engines
launch them inside containers with only your project directory mounted (see
[Security and Isolation](04-security-and-isolation.md)).

## Reference

| Layer | Location | Responsibility |
|---|---|---|
| Frontend | `src/frontend/` | TUI (Ratatui), CLI, and API server — input and rendering only |
| Command | `src/command/` | Command catalogue, dispatch, and per-command business logic |
| Engine | `src/engine/` | Containers, workflows, git, overlays, agents, auth |
| Data | `src/data/` | Sessions, configuration, filesystem, and database access |

`src/main.rs` is the thin entry point that wires a frontend to the rest.
Layering is enforced in CI by `make architecture-lint`, which fails if a lower
layer imports from a higher one.

Going deeper:

- [architecture.md](architecture.md) — the detailed layer-by-layer reference
- `aspec/architecture/design.md` and `aspec/architecture/security.md` — the
  governing design and security specs

---

[← Previous: Remote Mode](11-remote-mode.md) · [Next: GitHub Integration →](13-github-integration.md)
