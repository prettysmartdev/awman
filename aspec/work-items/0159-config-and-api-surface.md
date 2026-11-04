# Work Item: Task

Title: One clean config and API surface: naming rules, a single config tree, a resource-oriented route tree
Issue: issuelink

Depends on: none (0145–0158 and the other open items use the names defined here)

## Summary:
- awman's config and API grew one feature at a time, and it shows.
  - **Config keys mix casings and styles.** There are `default_agent`, `terminal_scrollback_lines` and `auto_agent_auth_accepted` next to `agentStuckTimeout`. `work_items.dir` is accepted as an alias for `workItems.dir`.
  - **Related settings are scattered:**
    - `runtime` and `baseImage` are at the root, but `builtin.*` is a section;
    - `agentFlags`, `agentStuckTimeout`, `yoloDisallowedTools`, `auth`, `authRefresh` and `launchMode` are all agent settings at the root;
    - `squad.agentsToModels` / `defaultLeader` / `guidance` duplicate `workflows.dynamic.*`.
  - **Concurrency has five unrelated names:** `maxConcurrentAgents`, `workers`, `squad.maxConcurrentEvaluations`, `workflows.dynamic.maxConcurrentSteps` and the proposed `daemon.maxConcurrent.*`.
  - **Units and value spellings are inconsistent:**
    - units live in key names (`memoryMib`, `rootDiskMib`) or are implied (`agentStuckTimeout` in seconds);
    - runtime values are spelled `apple-containers` and `builtin-experimental`;
    - one setting has three override channels (`maxConcurrentAgents`, `AWMAN_MAX_CONCURRENT_AGENTS`, `--max-concurrent`) whose names don't line up.
  - **Config state that isn't config** (`auto_agent_auth_accepted`) lives in config files, and `awman config migrate` exists only to carry old names.
  - **The API has aliases and duplicates:** `/v1/commands/{id}` and `/v1/commands/{id}/status`, `/v1/workflows/{id}` and `/v1/commands/{id}/workflow`, `/v1/sessions/{id}` and `/status`. Today the squad daemon and the API server both answer `/v1/commands` and `/v1/status` with different meanings. The open daemon work items add `/v1/daemon/*`, `/v1/engine/actions`, per-instance `actions` and `features` in different shapes.
- awman is in beta. This item makes one breaking pass to a surface that a person or an agent can predict from a few rules:
  1. **Naming rules** for every surface: config, API, CLI and environment.
  2. **One config tree**, grouped by domain, with the same shape in every config file. Every key is listed in one reference table, with its type, default and scope.
  3. **One generic override channel** (`--set key=value`, plus mechanically derived `AWMAN_*` environment variables) instead of one-off flags and variables per setting.
  4. **One resource-oriented route tree.** Each feature owns one prefix (`/v1/squad`, `/v1/worker`, `/v1/engine`), shared resources sit at the root, there are no aliases, and actions are resources rather than RPC verbs.
- No compatibility layer. Old keys, routes, flags and environment variables are simply unknown: config validation, the router and clap reject them with their normal errors, which list what *is* valid. `awman config migrate` is deleted.

## User Stories

### User Story 1:
As a: user

I want to:
guess a setting's name, find it with `awman config show`, override it for one run with `awman --set workflows.maxConcurrentSteps=2 exec workflow …`, and see it documented in one table

So I can:
configure awman without hunting through docs for which file, casing, unit or flag a setting uses.

### User Story 2:
As a: API client author (infact, a web frontend, an agent with `awman daemon api --raw`)

I want to:
predict every route from its resource name and its feature's prefix, and decode every list, error, ID, timestamp and duration the same way

So I can:
write one small client and never special-case an alias or a feature-specific response shape.

### User Story 3:
As a: awman developer

I want to:
add a setting or route by adding one row to a typed table that drives validation, `config show`, `--set`, environment overrides, the docs and the router

So I can:
keep the surface consistent without reviewing naming by hand.


## Implementation Details:

### 1. Naming rules (normative for all work items)
| Surface | Rule | Example |
|---|---|---|
| Config keys | `camelCase` path segments, grouped by domain (§2). No abbreviations except `id`, `url`, `tls`, `api`. | `agents.stuckTimeout` |
| Config enum values | One lowercase word where possible; multiword values are `snake_case`. The same spelling is used in config, CLI and API. | `interaction: "headless"`, `runtime.kind: "builtin"` |
| Durations | Config: strings with a unit (`"500ms"`, `"30s"`, `"15m"`, `"2h"`). API: integers in milliseconds, with the field suffix `_ms`. | `agents.stuckTimeout: "30s"`, `duration_ms` |
| Sizes | Config: strings with a binary unit (`"512MiB"`, `"4GiB"`). API: integers in bytes, with the suffix `_bytes`. | `runtime.builtin.memory: "4GiB"` |
| Timestamps | API: RFC 3339 UTC strings, with the suffix `_at`. | `created_at` |
| Concurrency | `maxConcurrent<Noun>`, where the noun is what's counted. | `maxConcurrentSteps`, `maxConcurrentJobs` |
| Toggles | `enabled: bool` inside the thing being toggled, never `useX`/`disableX`/`allowX` at a distance. | `daemon.features.squad.enabled` |
| Named collections | Maps keyed by name, never arrays of `{name, …}`, so every entry is addressable as `section.<name>.key`. | `workServers.infact.url` |
| API paths | `/v1/<resource>` for shared resources, and `/v1/<feature>/<resource>` for resources a feature owns. Resources are plural nouns, sub-resources are nested, and there are no verbs except a few state transitions as sub-resources (`/cancel`, `/answer`). | `/v1/worker/commands/{id}/cancel` |
| API fields | `snake_case`. IDs are always `id`, and foreign keys `<thing>_id`. | `session_id` |
| IDs | Opaque strings with a type prefix. | `ses_…`, `cmd_…`, `ins_…`, `tab_…`, `term_…`, `task_…`, `lease_…`, `key_…` |
| Lists | `{ "items": [...], "next_cursor": "…" \| null }`, with `?limit=` (default 50, max 500) and `?cursor=`. | |
| Errors | `{ "error": "<message>", "code": "<snake_case>" }` (0138's shape, kept), with one code table for the whole API. | `feature_disabled` |
| WebSocket subprotocols | `awman.<name>.v1` | `awman.terminal.v1`, `awman.frontend.v1` |
| Labels and annotations | `<namespace>/<name>`, lowercase `[a-z0-9.-]`. `awman/` is reserved for awman; integrations use their registry name. | `awman/os`, `rex/block` |
| CLI | Command and flag names are `kebab-case`. A flag exists only for per-invocation intent (`--yolo`, `--leader`, `--remote`). Anything that is also a setting uses `--set`. | |
| Environment | `AWMAN_` plus the config path in `SCREAMING_SNAKE` (dots and camel humps both become `_`), generated from the key table. Only the non-config process variables are listed separately (§3). | `AWMAN_WORKFLOWS_MAX_CONCURRENT_STEPS` |

### 2. The config tree
- **One shape for every config file:** global `~/.awman/config.json`, repo `<git_root>/.awman/config.json`, and squad per-task `~/.awman/squad/tasks/<name>/config.json`.
  - Each key has a **scope**: `global`, `repo` (global and repo, with repo winning per leaf key), or `task` (global, repo and task, where the task file wins).
  - A key set in a file its scope doesn't allow is a validation error that names the key and the files it may appear in.
- **Precedence**, lowest to highest: built-in default → global → repo → task (squad) → `AWMAN_*` environment → `--set`.
- **Merging:** objects merge per leaf. Arrays and maps *replace*, except the two listed as additive: `overlays` and `workflows.dynamic.guidance`. Those concatenate in precedence order, with duplicates dropped.
- **The tree.** "Was" lists every old key, for the docs' one-time upgrade note only. No alias is accepted.

```jsonc
{
  "agents": {
    "default": "claude",                       // was default_agent (global) / agent (repo)
    "stuckTimeout": "30s",                     // was agentStuckTimeout (integer seconds)
    "launchMode": "stdio",                     // stdio | acp            (was launchMode)
    "acpFallback": "error",                    // stdio | error          (was launchModeFallback)
    "flags": { "claude": ["--verbose"] },      // was agentFlags
    "yolo": { "disallowedTools": [] },         // was yoloDisallowedTools
    "auth": {
      "mode": "keychain",                      // keychain | passthrough | none   (was auth)
      "refresh": { /* unchanged fields */ }    // was authRefresh
    },
    "programStatus": { "enabled": true }       // 0152: parse OSC 7501 (was agentProgramStatus)
  },
  "runtime": {
    "kind": "docker",                          // docker | apple | sbx | builtin  (was runtime: docker | apple-containers | docker-sbx-experimental | builtin-experimental)
    "baseImage": null,                         // was baseImage
    "dockerfile": "Dockerfile.dev",            // was dockerfile (repo)
    "builtin": {
      "stateDir": null, "vcpus": 4,
      "memory": "4GiB", "rootDisk": "20GiB", "mountQuota": "8GiB",   // were memoryMib, rootDiskMib, mountQuotaMib
      "builder": { "enabled": true },
      "imageSource": {}, "images": {}, "registries": {}, "network": {}
    }
  },
  "overlays": [],                              // additive
  "workItems": {
    "dir": "aspec/work-items", "template": null,   // the work_items.* alias is removed
    "source": null,                                // 0139: null (files) | github | <provider name>
    "providers": { "tracker": { "baseUrl": "https://…", "tokenEnv": "TRACKER_TOKEN" } }   // 0139: global scope, keyed by name
  },
  "workflows": {
    "maxConcurrentSteps": null,                // enforced cap (was maxConcurrentAgents); the leader prompt gets the same number
    "prerequisites": { /* unchanged subtree */ },
    "dynamic": {
      "defaultLeader": null, "agentsToModels": {}, "guidance": []   // guidance is additive
      // workflows.dynamic.maxConcurrentSteps (advisory) is removed: the leader is told workflows.maxConcurrentSteps
    }
  },
  "worktree": { "onDirtyMergeTarget": "ask", "mergeFix": { "onFailure": "ask", "promptTemplate": null } },
  "tui": {
    "scrollbackLines": 10000,                  // was terminal_scrollback_lines
    "backend": "auto",                         // 0155: auto | daemon | builtin
    "programStatus": { "emit": true },         // 0156
    "worker": { "enabled": false },            // 0147: TUI-hosted manual worker
    "integrations": { "rex": { /* 0153, 0157 */ } }
  },
  "daemon": {
    "maxConcurrentJobs": 4,                    // host-wide cap (was workers)
    "terminal": { "cols": 120, "rows": 40, "remoteInput": false },
    "network": { "enabled": false, "listen": "127.0.0.1:9876", "tlsSans": [], "features": ["worker"] },  // listen replaces bind + port
    "features": {
      "squad": {
        "enabled": false, "interaction": "interactive", "maxConcurrentJobs": 1,
        "maxConcurrentEvaluations": 2,         // was squad.maxConcurrentEvaluations
        "envPersistence": "keychain"           // keychain | none   (was squad.envPersistence)
        // squad.agentsToModels / defaultLeader / guidance are removed: evaluations use workflows.dynamic.*, and task files override them
      },
      "worker": {
        "enabled": false, "maxConcurrentJobs": 2,
        "push": {
          "enabled": true, "interaction": "headless", "allowOverride": true,
          "workDirs": { "allow": [], "roots": [] }   // were api.workDirs, api.workDirRoots
        },
        "pull": { "enabled": true, "interaction": "interactive", "allowOverride": true, "workRoot": null }   // workRoot was worker.workRoot
      },
      "engine": { "enabled": false, "allowEphemeral": true, "idleTimeout": "15m", "maxConcurrentJobs": 4 }
    }
  },
  "workServers": {                             // 0147 pull sources, keyed by name (was worker.servers[])
    "infact": { "url": "https://…", "enabled": true, "claim": "auto", "maxConcurrentJobs": 1, "interaction": null, "labels": {} }
  },
  "remote": {                                  // push targets for `awman remote`, keyed by name
    "default": "prod",                         // was remote.defaultAddr (now a target name)
    "targets": { "prod": { "url": "https://…:9876", "apiKey": "…" } }   // was remote.defaultAPIKey; masked in output
  }
}
```

- **Value changes that come with the move:**
  - `interaction` values are `interactive` | `headless` (was `non-interactive`) everywhere: config, `--set`, API `interaction` fields and WSP.
  - Runtime kinds lose their maturity suffixes (`-experimental`). Maturity is stated in the docs and `awman config show`, not in values.
- **Not config any more:**
  - `auto_agent_auth_accepted` moves to per-user state, `<data>/repos/<repo-hash>/state.json`. It is shown read-only by `awman config show` under a separate "state" heading, which also names the file. Deleting that file clears it; there is no subcommand.
  - `api.port` was never config, and now isn't needed: `daemon.network.listen` covers it.
  - `api.alwaysNonInteractive` becomes `daemon.features.worker.push.allowOverride: false` with `interaction: "headless"`.
  - `envPassthrough` stays removed.
- **Key table.** `src/data/config/fields.rs` becomes the single source of truth. One row per leaf gives `path`, type, default, scope, sensitivity (masked), whether it applies live to a running daemon, and a doc string. Validation, `config show/get/set/unset`, `--set` parsing, `AWMAN_*` lookup, `awman daemon` live reload and the generated reference table in `docs/07-configuration.md` all read it. New keys from other work items are added as rows; they never add parsing code of their own.

### 3. Overrides and process environment
- `--set <key>=<value>` is a repeatable global flag that applies to one invocation, at the top of the precedence order. Values are parsed by the key's type, with arrays comma-separated as `config set` does.
- **Renamed:** the `--non-interactive` flag on `exec`/`chat` becomes `--headless`, matching the `headless` interaction value. `--interactive` is unchanged.
- **Removed, now covered by `--set`:**
  - `--max-concurrent` → `--set workflows.maxConcurrentSteps=…`;
  - `--launch-mode` → `--set agents.launchMode=…`;
  - 0155's `--engine` → `--set tui.backend=…`;
  - 0153's `--integration` / `--no-integrations` → `--set tui.integrations.rex.enabled=off`;
  - `--remote-addr` / `--api-key` → `--remote <target>` (intent: which target) plus `remote.targets.*`.
- **`AWMAN_*` config variables** are derived from the key table. For example, `AWMAN_WORKFLOWS_MAX_CONCURRENT_STEPS`, `AWMAN_AGENTS_LAUNCH_MODE` and `AWMAN_TUI_BACKEND`. Removed hand-named variables: `AWMAN_MAX_CONCURRENT_AGENTS`, `AWMAN_LAUNCH_MODE`, `AWMAN_REMOTE_ADDR`, and `AWMAN_API_KEY` (→ `AWMAN_REMOTE_TARGETS_<NAME>_API_KEY`). `AWMAN_OVERLAYS` keeps its name, because the derived name for `overlays` is the same.
- **Process variables that are not config** stay, in one documented list:
  - `AWMAN_HOME`, which replaces `AWMAN_CONFIG_HOME`, `AWMAN_API_ROOT`, `AWMAN_SQUAD_ROOT` and `AWMAN_ATTACH_DIR`: one home holds `config.json` and `data/`, and everything else is laid out beneath it. XDG variables are still honoured when `AWMAN_HOME` is unset.
  - `AWMAN_BUILTIN_STATE_DIR` is removed (→ `AWMAN_RUNTIME_BUILTIN_STATE_DIR`, derived).
  - The `AWMAN_TEST_*` isolation variables are unchanged.
- **Squad key:** `AWMAN_SQUAD_KEY` is removed. The CLI and TUI reach the daemon over the local listener with the owner key from the 0600 server-meta sidecar (0145), so nobody pastes a key into a shell any more.

### 4. Config commands
- `awman config [show]`, `awman config get <key>`, `awman config set <key> <value>`, `awman config unset <key>`.
  - **Scope is automatic.** A `global`-scope key is written to the global file. A `repo`/`task`-scope key is written to the repo file when run inside a repo, else to the global file. `--global` / `--repo` force the scope, and `--task <name>` writes a squad task file. The command always prints which file it wrote.
  - **Live apply.** If the key is marked live in the key table and a daemon is running, `config set`/`unset` notifies it (`POST /v1/config/reload` over the local listener) and prints `applied to the running daemon`. Otherwise it prints `takes effect on next start`.
  - Turning a daemon feature off drains its work. Cancelling running work immediately is a separate, explicit action (`awman daemon api jobs.cancel feature=worker`).
- **Removed:** `awman config migrate`, the `work_items.*` alias, the "empty value removes" convention (use `unset`), and `awman daemon set` (0145) — `awman config set daemon.…` is the one config writer.
- `awman config prerequisite-grants list|revoke` manages daemon state, not config. It becomes `awman daemon api grants.list` / `grants.revoke` over `/v1/grants` (below).

### 5. The API route tree
One tree, served by the daemon on its local listener and, for the features in `daemon.network.features`, on the network listener (0145). Every route belongs to the core or to exactly one feature. A disabled feature's routes answer `503 feature_disabled`. On the network listener, routes of unlisted features answer `404 not_found`.

**Core (always on):**

| Route | Purpose |
|---|---|
| `GET /v1/status` | Build, protocol versions, features and their state (`on`, `off`, `ephemeral`, `draining`), capacity (`max_concurrent_jobs`, running and queued per feature), and listeners. |
| `GET /v1/health` | Unauthenticated `{status}` (0142). |
| `GET /v1/features`, `GET /v1/features/{feature}`, `PATCH /v1/features/{feature}` | List and inspect features. `PATCH {enabled, ephemeral: true}` is the local-only ephemeral switch (0145/0155). Persistent changes go through config. |
| `GET /v1/config`, `POST /v1/config/reload` | The effective daemon config (sensitive values masked), and reload after `config set`. Local listener only. |
| `GET /v1/keys`, `POST /v1/keys`, `DELETE /v1/keys/{id}` | Network-listener API keys (0142). Owner key only. |
| `GET /v1/grants`, `DELETE /v1/grants/{id}` | Prerequisite grants (was `config prerequisite-grants`). |
| `GET /v1/terminals?owner_id=…`, `GET /v1/terminals/{id}` | Hub terminals for every feature (0146). The WebSocket upgrade uses `awman.terminal.v1`. Access follows the owning resource's rules. |
| `/v1/contexts/{key}`, `/v1/contexts/{key}/files/{path}` | 0140's context store, shared by every feature (was `/v1/context/repo/{key}`). |

**`squad`:**

| Route | Purpose |
|---|---|
| `GET /v1/squad/tasks`, `POST /v1/squad/tasks`, `GET|PATCH|DELETE /v1/squad/tasks/{id}` | Task management. These are today's gateway commands as resources: add, edit, show, remove, and pause/resume as `PATCH {paused}`. |
| `POST /v1/squad/tasks/{id}/runs`, `GET /v1/squad/tasks/{id}/runs`, `POST /v1/squad/runs/{id}/cancel` | Trigger, history, cancel. |
| `GET /v1/squad/tasks/{id}/workflow` | Was `/v1/tasks/{name}/workflow`. |
| `GET|PUT /v1/squad/env` | Task env values (was `/v1/daemon/env`). |

Today's squad gateway forwards CLI command lines (`POST /v1/commands` on the squad daemon). It is replaced by these resources, and `awman squad …` subcommands call them directly.

**`worker`, push:**

| Route | Purpose |
|---|---|
| `GET /v1/worker/workdirs` | Allowed workdirs (was `/v1/workdirs`). |
| `GET|POST /v1/worker/sessions`, `GET|DELETE /v1/worker/sessions/{id}`, `GET /v1/worker/sessions/{id}/queue` | Was `/v1/sessions*`. The `/status` alias is gone: `GET` returns status. |
| `GET|POST /v1/worker/commands`, `GET /v1/worker/commands/{id}` | Was `/v1/commands*`. The `/status` alias is gone. |
| `POST /v1/worker/commands/{id}/cancel` | |
| `GET /v1/worker/commands/{id}/events` | SSE of execution events (was `/logs`). |
| `GET /v1/worker/commands/{id}/workflow` | The only workflow route; `/v1/workflows/{id}` is gone. |
| `GET /v1/worker/commands/{id}/outputs[/{path}]` | |
| `GET /v1/worker/commands/{id}/context_changes` | 0140. |
| `POST /v1/worker/runs` | Session + command in one call (0135). |

**`worker`, pull:**

| Route | Purpose |
|---|---|
| `GET /v1/worker/servers`, `GET /v1/worker/servers/{name}` | Connection state per `workServers` entry (registered, labels, last claim). |
| `GET /v1/worker/queue?server=…` | Eligible items (what `awman queue` shows). |
| `POST /v1/worker/queue/{item_id}/claim` | A manual pick. |
| `GET /v1/worker/leases`, `GET /v1/worker/leases/{id}`, `POST /v1/worker/leases/{id}/release` | |

The Work Server Protocol (0147) is a separate API that awman *calls*. It follows the same naming rules (§1): `snake_case` fields, `items`/`next_cursor` lists, the `{error, code}` shape, and `awman/` label prefixes (was `awman.dev/`).

**`engine`:**

| Route | Purpose |
|---|---|
| `GET /v1/engine/instances?annotation=k=v`, `POST /v1/engine/instances`, `GET|DELETE /v1/engine/instances/{id}` | Instances (0154). `DELETE` is quit, with today's exit policy. |
| `GET /v1/engine/instances/{id}/frontend` | WebSocket `awman.frontend.v1` (0155). Local only, unless `engine` is persistently listed in `network.features`. |
| `GET /v1/engine/instances/{id}/tabs`, `GET /v1/engine/instances/{id}/tabs/{id}` | Tabs, with their sessions, container slots and annotations. |
| `GET /v1/engine/instances/{id}/dialogs`, `POST /v1/engine/instances/{id}/dialogs/{id}/answer` | Pending dialogs and answering one. The first answer wins, exactly as from a frontend. This replaces 0158's `step.approve` route. |
| `POST /v1/engine/instances/{id}/ui_requests` | `{request: "open_overview" \| …}`, for integrations' palette actions. This replaces `overview.open`. |
| `PATCH /v1/engine/instances/{id}`, `PATCH /v1/engine/instances/{id}/tabs/{id}` | `{annotations}` only, for host presenters (0153) through the API. |

- **Removed, with no alias:**
  - `/v1/sessions*`, `/v1/commands*`, `/v1/runs` and `/v1/workdirs` at the root (now under `/v1/worker`);
  - `/v1/workflows/{id}`, and the `…/status` aliases;
  - `/v1/tasks/{name}/workflow`;
  - `/v1/daemon/*`, `/v1/engine/actions`, `/v1/engine/instances/{id}/actions` and `/v1/engine/terminals/*` (now `/v1/terminals`);
  - `/v1/commands/{id}/terminals*` (now `/v1/terminals?owner_id=cmd_…`).

### 6. `awman daemon api`: actions are named routes, not a second API
- `awman daemon api <action> [key=value]... [--json]` runs a **client-side** table of friendly names. Each entry maps to exactly one route above.
  - `--list` prints each action with its arguments and the route it calls, for example `keys.create name=…  → POST /v1/keys`.
  - `awman daemon api --raw <METHOD> <path> [--data <json>|@file|-]` calls any route directly.
  - Both use the local listener and the owner key.
- **Names are `<resource>.<verb>`**, with verbs `list`, `get`, `create`, `update`, `delete` and the named transitions (`cancel`, `answer`, `release`):
  - `features.list`, `features.get`
  - `instances.list` (`annotation.<k>=<v>` filters), `instances.get`
  - `dialogs.list`, `dialogs.answer`
  - `ui.request`
  - `keys.list|create|delete`
  - `grants.list|delete`
  - `jobs.cancel` (`feature=`, or `id=`)
  - `leases.list|release`
  - `terminals.list`
- **Small conveniences** compose these and are named for the user's intent:
  - `steps.approve instance=…` = `dialogs.list` filtered to the pending `workflow_step_confirm`, then `dialogs.answer response=yes`;
  - `overview.open instance=…` = `ui.request request=open_overview`.
  
  They are documented as compositions, so `--raw` users can reproduce them.
- **The server has no action registry and no `actions` route.** 0145, 0154 and 0158's action routes are removed, and `src/command/commands/daemon/actions.rs` is a client-side table.

### 7. CLI commands touched by this pass
The table lists every command group, with the verbs it ends up with, after 0145–0158 and this item.

| Group | Verbs |
|---|---|
| `awman daemon` | `start [--background]`, `stop [--now]`, `status [--json]`, `logs [-f]`, `api …` (§6). Config goes through `awman config` (§4). |
| `awman config` | `show`, `get`, `set`, `unset` |
| `awman integration` | `status [<name>]`, `setup <name>`, `remove <name>` |
| `awman worker` | `link <url> [--label k=v]...`. Server entries are edited with `awman config … workServers.<name>.*`. |
| `awman queue` | `[pick [<item-id>]]` |
| `awman remote` | The existing verbs, with `--remote <target>`. Requests go to the target's `/v1/worker/*`. |
| `awman squad` | Task management only (`add`, `edit`, `list`, `show`, `remove`, `pause`, `resume`, `trigger`, `cancel`, `attach`, `env`). Lifecycle and status live in `awman daemon`. |
| `awman attach` | `<terminal-id \| command-id \| task-id> [--read-only] [--annotation k=v]...` |
| Global flags | `--set key=value` (repeatable), `--remote <target>`, and the per-invocation intents each command already has. |

## Edge Case Considerations:
- **Old keys in existing files** fail validation with the normal unknown-key error. The message lists valid sibling keys, and for a key that moved it also names the new path (`agentStuckTimeout moved to agents.stuckTimeout ("30s")`). The hint is an error message only, never accepted as input. Its static moved-keys table is deleted one release after this item ships.
- **Key paths with user-chosen names** (`workServers.<name>`, `remote.targets.<name>`, `agents.flags.<agent>`, `workflows.dynamic.agentsToModels.<agent>`): names must match `[a-z0-9][a-z0-9_-]{0,62}`, so derived environment variable names are unambiguous (`-` becomes `_`). Two names that differ only by `-` versus `_` are rejected as duplicates.
- **Derived environment names colliding with process variables** (`AWMAN_HOME` versus a hypothetical `home` key): the key table forbids top-level keys that would derive to a reserved process variable, enforced by a unit test.
- **Live apply of a key that can't change live** (`daemon.network.listen` while the listener runs): `config set` writes the file and says `restart the daemon to apply` (`awman daemon stop && awman daemon start`). It never half-applies.
- **Task-scope keys in a repo file** (`workflows.dynamic.*`) are allowed: task scope includes repo. Daemon-process keys (`daemon.*`) in a task file are rejected.
- **ID prefixes** on existing stored records: the store migrates IDs on first start of the new version, with a schema bump in `awman.db` (data, not config, so a migration is fine). External clients must re-read IDs; awman is in beta.
- **`--set` on a daemon-served command** (`awman --set … exec workflow` with `tui.backend: daemon`): the override travels in the frontend `Hello`'s argv (0155) and applies to that instance only, exactly as in-process.

## Test Considerations:
- **Key table tests:**
  - every row's path follows §1 (casing, `maxConcurrent<Noun>`, `enabled`, units in values);
  - every row has a default, scope and doc string;
  - derived environment names are unique and don't hit reserved process variables;
  - the generated docs table matches the committed docs (a golden test).
- **Precedence tests** across default, global, repo, task, environment and `--set`, for scalars, objects (per-leaf merge), replacing arrays, and the two additive arrays.
- **Scope tests:** global-only keys rejected in repo files, `daemon.*` rejected in task files, and automatic scope selection by `config set`.
- **Old-key tests:** each removed key fails with the moved-to hint and is never applied.
- **Router tests:**
  - every route in §5 exists, belongs to exactly one feature or the core, and nothing else is routed (a table test against the router);
  - removed paths return 404;
  - list responses carry `items`/`next_cursor`;
  - every error carries `code`;
  - IDs carry their prefix;
  - timestamps and durations follow §1 (a serde round-trip on every response type).
- **Network listener tests:** unlisted features → 404, a disabled feature → 503, owner-only routes refused with API keys.
- **CLI tests:** `awman daemon api --list` covers every action, and each action's request equals its documented `--raw` equivalent. `--set` parsing for each type. Removed flags fail with clap's unknown-flag error.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- **Config:**
  - `src/data/config/{global,repo}.rs` collapse into one `ConfigFile` struct with the §2 tree. Scope rules come from the key table in `src/data/config/fields.rs`, and `effective.rs` resolves precedence.
  - Section structs live in `src/data/config/{agents,runtime,workflows,worktree,tui,daemon,work_servers,remote}.rs`. Integration configs live in `src/data/config/integrations/<name>.rs`.
  - Delete `migrations.rs` and the `config migrate` command (`src/command/commands/config.rs`).
- **Overrides:** `--set` and `AWMAN_*` resolution in `src/data/config/env.rs` (`EnvSnapshot`), driven by the key table. Remove the per-setting flags and environment variables named in §3 from the dispatch catalogue (`src/command/dispatch/catalogue/`) and from `EnvSnapshot`.
- **API:**
  - the router in `src/frontend/api/routes.rs` mounts `/v1/squad`, `/v1/worker` and `/v1/engine` sub-routers from each `DaemonFeature::routes()` (0145), plus core routes;
  - `src/frontend/squad/routes.rs` is merged in and the squad gateway is deleted;
  - shared `List<T>`, `ApiError`, ID newtypes with prefixes (`src/data/ids.rs`) and `Duration`/`Timestamp` serde helpers (`src/data/wire.rs`).
- **Client:** `awman remote` and the daemon client (`src/command/commands/remote_client.rs`) move to the new paths. `awman daemon api`'s action table lives in `src/command/commands/daemon/actions.rs`.
- **Other work items** use these names. 0139–0158 and README.md are aligned in the same change set as this item.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.

Specifically for this item:
- `docs/07-configuration.md` is rewritten around the §2 tree:
  - the precedence and merge rules;
  - a generated reference table (key, type, default, scope, live, environment variable);
  - a one-time "upgrading from earlier betas" table of moved keys.
- `docs/09-api-and-remote-mode.md` gets the §1 conventions and the §5 route tree as the API reference.
- `docs/14-command-reference.md`: §7, `--set`, and the removed flags.
- Every doc that shows a config key, route, flag or environment variable is updated to the new names (`docs/03`, `05`, `06`, `08`, `11`, `12`, `16`–`19`).
