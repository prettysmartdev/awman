# Configuration

awman reads two JSON config files — one per repository, one global — and merges them with command-line flags and environment variables. You rarely need to edit the files by hand: the `awman config` subcommand can view and change almost everything.

---

## The two config files

### Per-repository config

**Path:** `<git_root>/.awman/config.json`

Created by `awman init`; commit it so the whole team shares the same setup.

```json
{
  "agent": "claude",
  "launchMode": "stdio",
  "terminal_scrollback_lines": 10000,
  "dockerfile": "docker/Dockerfile.base",
  "yoloDisallowedTools": ["Bash"],
  "agentStuckTimeout": 60,
  "maxConcurrentAgents": 3,
  "overlays": ["env(ANTHROPIC_API_KEY)", "dir(/data/fixtures:/mnt/fixtures:ro)"],
  "workItems": {
    "dir": "docs/work-items",
    "template": "docs/work-items/0000-template.md"
  },
  "dynamicWorkflows": {
    "agentsToModels": {
      "claude": ["claude-opus-4-8", "claude-sonnet-4-6"]
    },
    "maxConcurrentSteps": 3,
    "defaultLeader": "claude::claude-opus-4-8",
    "guidance": ["Never spawn more than two agents in parallel."]
  }
}
```

### Global config

**Path:** `$HOME/.awman/config.json` (relocatable — see [Where global files live](#where-global-files-live))

Applies to every project on the machine unless a repo overrides it.

```json
{
  "default_agent": "claude",
  "launchModeFallback": "error",
  "runtime": "docker",
  "terminal_scrollback_lines": 10000,
  "yoloDisallowedTools": ["Bash"],
  "overlays": ["skill(*)", "env(ANTHROPIC_API_KEY)"],
  "agentStuckTimeout": 30,
  "maxConcurrentAgents": 2,
  "workers": 2,
  "api": {
    "workDirs": ["/home/user/my-project"],
    "alwaysNonInteractive": false
  },
  "squad": {
    "agentsToModels": {
      "claude": ["claude-opus-4-8", "claude-sonnet-4-6"]
    },
    "maxConcurrentEvaluations": 2,
    "defaultLeader": "claude::claude-opus-4-8",
    "guidance": ["Keep automated changes focused."]
  },
  "remote": {
    "defaultAddr": "http://build-server.example.com:9876",
    "defaultAPIKey": "a3f8b2c1...",
    "savedDirs": ["/home/user/my-project"]
  }
}
```

### A config file that doesn't parse

A `config.json` that isn't valid JSON, or that fails schema validation, is no
longer silently ignored in favor of defaults you didn't write. `awman api
start` and `awman squad start` refuse to start against a malformed config,
failing with `config parse error in <path>: <reason>` — naming the file and
the offending key — instead of starting a daemon on settings you never set.
Fix the file (or restore it from git, for the per-repo one) and try again.

### Squad daemon configuration

The optional `squad` block is global, so it belongs in
`~/.awman/config.json` (or the relocated global config file). It controls the
agents and models your squad may use and the guidance it passes to task
evaluations:

Task-specific workspace, interval, and overlay choices are configured with
`awman squad add` rather than in this global block. See [Squad](12-squad.md)
for the durable workspace, task-creation, and daemon details. The squad daemon
and API server cannot run at the same time because they share the awman
database; see [API server and squad daemon](09-api-and-remote-mode.md#api-server-and-squad-daemon).

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `agentsToModels` | object (agent name → non-empty string array) | unset | Models available for each agent; every map key must be a valid agent name |
| `maxConcurrentEvaluations` | positive integer | `2` | Maximum number of task evaluations running at once |
| `defaultLeader` | string (`agent::model`) | unset | Default agent and model for evaluations when a task does not specify them |
| `guidance` | non-empty string array | unset | Instructions added to every task evaluation and generated workflow |
| `envPersistence` | `"keychain"` \| `"none"` | `"keychain"` | Where the daemon persists the values it holds for `env()` overlays across an OS-initiated restart (launchd/systemd), so a task's variables don't have to be pushed again from a live shell after one. `"none"` opts out entirely — nothing is persisted anywhere, matching squad's behaviour before this setting existed. See [Squad: Task environment values](12-squad.md#task-environment-values) |

All five keys are optional. Configuration is rejected when
`maxConcurrentEvaluations` is zero; an `agentsToModels` entry has no models or
contains an empty model name; `defaultLeader` is not exactly two non-empty
components in `agent::model` form, has surrounding whitespace in either
component, or uses an invalid agent name; or `guidance` has an empty or
whitespace-only entry. A `guidance` list may contain at most 50 entries, and
each entry is limited to 1,000 characters. Agent names use ASCII letters,
digits, `-`, and `_`, and are 1–64 characters long.

`envPersistence` describes the daemon process itself, not any one task, so —
like `maxConcurrentEvaluations` — only the value in the **global** `squad`
block takes effect; a per-task `config.json` cannot override it. If the
keychain is configured but unusable on this machine (no keychain backend, a
missing `secret-tool`, a locked collection, or a probe that fails or times
out), the daemon falls back to persisting nothing for that run rather than
failing to start — `awman squad status` and `awman squad env` report the
fallback and the reason as `persistence: unavailable(<reason>)`. An explicit
`"none"` is never probed for and never produces that fallback message,
because it's a deliberate choice rather than a degradation.

#### Per-task squad settings

A squad task can carry its own copy of this block, in a `config.json` beside its
durable workspace:

```text
~/.awman/squad/tasks/<name>/config.json
```

The file has the same shape as `~/.awman/config.json` — only its `squad` block
means anything for a task — and is held to the same validation rules:

```json
{ "squad": { "agentsToModels": { "claude": ["claude-opus-4-8"] } } }
```

A field the task sets wins for that task; a field it omits is inherited from the
global block, so a task can narrow its agent pool while keeping the standing
`guidance` every task gets. `maxConcurrentEvaluations` and `envPersistence`
are the exceptions: both describe the daemon process as a whole rather than
any one task, so each is always read from the global block and ignored in a
task file.

The file is optional, and awman writes it for you when you answer the agent and
model questions during `awman squad add` — see [Choosing a task's agents and
models](12-squad.md#choosing-a-tasks-agents-and-models). Edits take effect on the
daemon's next scheduling tick, the same as edits to the global block. A task file
that does not parse fails that one task's next run with an error naming the file,
rather than being silently ignored.

> **Upgrading from an old config?** The `envPassthrough` field was removed. Express environment passthrough as `env(VAR)` entries in the `overlays` array instead — see [Overlays](08-overlays.md). The old object-style `overlays` block (`{"skills": …, "directories": …}`) is also gone; `overlays` is now a flat array of overlay specs and the old format produces a parse error.

---

## How precedence works

For any setting, the highest-priority source that defines it wins:

```
flags  >  environment variables  >  repo config  >  global config  >  built-in default
```

Examples:

- `awman chat --agent codex` beats `agent` in repo config, which beats `default_agent` in global config.
- `AWMAN_REMOTE_ADDR` beats `remote.defaultAddr` in global config; `--remote-addr` beats both.
- `awman exec workflow ... --max-concurrent 4` beats `AWMAN_MAX_CONCURRENT_AGENTS`, which beats `maxConcurrentAgents` in repo config, which beats `maxConcurrentAgents` in global config.
- With nothing set anywhere, built-in defaults apply: 10,000 scrollback lines, 30-second agent-stuck timeout, 2 API workers, API port 9876, unlimited concurrent agents per workflow.

Launch mode follows the same order where those sources apply:

```
--launch-mode  >  AWMAN_LAUNCH_MODE  >  repo launchMode  >  stdio
```

`launchMode` is a repository setting; there is no global `launchMode` value.
`launchModeFallback` is global-only and follows `global launchModeFallback >
error`. Its setting matters when ACP was requested but the selected agent does
not support ACP: `error` stops the launch, while `stdio` permits a visible
fallback to the regular mode.

Two wrinkles:

- **List fields replace, they don't merge.** A repo `yoloDisallowedTools` list completely replaces the global one — even an empty list. To inherit the global list, omit the field from the repo config.
- **Overlays are additive.** `overlays` entries from global config, repo config, `AWMAN_OVERLAYS`, and `--overlay` flags are all merged. See [Overlays](08-overlays.md).

---

## Managing config from the terminal

```sh
awman config show                          # full table: global, repo, and effective values
awman config get <field>                   # one field, all scopes
awman config set <field> <value>           # write to repo config
awman config set --global <field> <value>  # write to global config
```

- `config show` and `config get` never fail on missing files; absent files are treated as all-unset. `config set` creates the file and its parent directory as needed.
- Each field has a natural scope. Setting a global-only field without `--global` (or a repo-only field with it) is an error that tells you which flag to use.
- Unknown field names get a did-you-mean suggestion list.
- `remote.defaultAPIKey` is masked in `config show`/`config get` output.
- Setting the removed `envPassthrough` field errors with guidance to use `env(VAR)` overlay entries instead.

The full list of accepted field names and their scopes is in the [Reference](#reference).

---

## Common recipes

### Set the default agent

```sh
awman config set --global default_agent gemini   # for all projects
awman config set agent codex                     # for this repo only
```

Valid agents: `claude`, `codex`, `gemini`, `opencode`, `crush`, `cline`, `copilot`, `maki`, `antigravity`. Anything else is rejected at write time.

### Adjust terminal scrollback

```sh
awman config set --global terminal_scrollback_lines 20000   # all projects
awman config set terminal_scrollback_lines 5000             # this repo
```

A 10,000-line buffer at 80 columns uses roughly 3 MB per tab. Increase for long build logs; decrease when running many tabs.

### Pass API keys into agent containers

awman never forwards your whole environment into a container — name each variable explicitly as an `env()` overlay:

```sh
awman config set --global overlays "env(ANTHROPIC_API_KEY),env(OPENAI_API_KEY)"
```

Or per-invocation: `awman chat --overlay "env(ANTHROPIC_API_KEY)"`. See [Overlays](08-overlays.md) for the full syntax and [Agent Sessions](03-agent-sessions.md) for per-agent authentication details.

### Switch container runtime

```sh
awman config set --global runtime docker                  # default
awman config set --global runtime apple-containers        # macOS only
awman config set --global runtime docker-sbx-experimental # experimental
```

See [Runtimes](#runtimes) below.

### Choose ACP launch mode

To use ACP by default in this repository, add the following field to
`.awman/config.json`:

```json
{ "launchMode": "acp" }
```

The allowed values are `"stdio"` (the default) and `"acp"`. To choose what
happens when a workflow step's agent does not support ACP, add
`launchModeFallback` to `$HOME/.awman/config.json`:

```json
{ "launchModeFallback": "stdio" }
```

Its allowed values are `"error"` (the default) and `"stdio"`. The command-line
flag takes priority over both the environment variable and repository setting;
see [ACP launch mode](03-agent-sessions.md#acp-launch-mode).

### Custom work item paths

By default awman looks for work items in `aspec/work-items/` and uses `aspec/work-items/0000-template.md` as the template for `awman new spec`. To use different paths:

```sh
awman config set work_items.dir docs/work-items
awman config set work_items.template docs/work-items/0000-template.md
```

Paths may be relative to the repo root (recommended) or absolute. Note the CLI names are `work_items.dir` / `work_items.template`, but they are stored in the JSON file under a single `workItems` block.

### Cap concurrent agents in a workflow

```sh
awman config set --global maxConcurrentAgents 2   # machine-wide default
awman config set maxConcurrentAgents 4            # this repo only
```

Or per-invocation, without touching any config file:

```sh
awman exec workflow aspec/workflows/implement-hard.toml --max-concurrent 2
AWMAN_MAX_CONCURRENT_AGENTS=2 awman exec workflow aspec/workflows/implement-hard.toml
```

Leaving `maxConcurrentAgents` unset (the default) means unlimited — every step whose dependencies are satisfied launches immediately. Lower it to match your machine's CPU/memory budget or your Docker daemon's capacity. See [Parallel workflows](05-workflows.md#parallel-workflows) for how the engine uses this cap to schedule steps.

### Configure dynamic workflow agents, models, and leader

```sh
awman config set dynamicWorkflows.defaultLeader claude::claude-opus-4-8
awman config set dynamicWorkflows.maxConcurrentSteps 3
```

`dynamicWorkflows.agentsToModels` is set one agent at a time — `awman config set dynamicWorkflows.agentsToModels.<agentName> "model-a, model-b"` (an empty value removes the mapping), or inline in the TUI config dialog where **Ctrl+N** adds a new mapping.

`dynamicWorkflows.guidance` is a list of instructions the leader agent must follow whenever it designs a workflow. It's set one entry at a time, addressed by index — `awman config set dynamicWorkflows.guidance.0 "Never spawn more than two agents in parallel."` (an empty value removes that entry and re-indexes the rest) — or inline in the TUI config dialog, where **Ctrl+N** appends a new entry.

See [Dynamic Workflows](06-dynamic-workflows.md#configuring-dynamic-workflows) for the full reference.

### Custom Dockerfile path

By default awman builds the project base image from `<git_root>/Dockerfile.dev`. To use a Dockerfile elsewhere, set `dockerfile` in `.awman/config.json` directly (it is not settable via `config set`):

```json
{ "dockerfile": "docker/Dockerfile.base" }
```

If the configured file doesn't exist, commands report the exact configured path rather than silently falling back to the default. `awman init` also offers to point at an existing Dockerfile interactively when no `Dockerfile.dev` is found.

### Restrict tools in yolo mode

```sh
awman config set yoloDisallowedTools "Bash,computer"   # this repo
awman config set yoloDisallowedTools ""                # set an empty list
```

An empty repo list actively overrides a non-empty global list. To stop overriding, remove the field from the repo config file. See [Permission modes](03-agent-sessions.md#permission-modes).

### Control credential injection (`auth` mode)

By default awman injects host keychain credentials into agent containers
(`keychain` mode). Two alternatives are available for harnesses that supply
credentials through other means:

```json
{ "auth": "passthrough" }
```

| Value | Behaviour |
|-------|-----------|
| `keychain` (default) | Inject host keychain credentials. When the repo also declares `env(ANTHROPIC_API_KEY)` (or another credential that covers the same provider) **and that variable is set on the host**, the keychain OAuth token for that provider is automatically suppressed at injection time — the container receives exactly one set of credentials per provider. If the declared passthrough var is not set on the host, the keychain credential is retained so the container is not left with zero credentials for that provider. |
| `passthrough` | No KEYCHAIN credential injection; declared `env()` overlays still apply. Supply credentials via `env(VAR)` overlays. |
| `none` | No KEYCHAIN credential injection; declared `env()` overlays still apply. |

Set `auth` in `.awman/config.json` directly (it is not settable via `config set`). The field is per-repo only — cloud harnesses that do not declare an anthropic env var remain on the default `keychain` path and continue to receive keychain OAuth unaffected.

### Control credential refresh (`authRefresh`)

Under `keychain` auth mode, awman keeps Claude's containerized OAuth credential fresh for the life of a session: it periodically pings your host's Claude Code installation to rotate the token, then rewrites the staged credential file — see [Live credential refresh](04-security-and-isolation.md#live-credential-refresh). This is on by default. Tune it, or turn it off, with the optional `authRefresh` block:

```json
{
  "authRefresh": {
    "enabled": true,
    "thresholdMinutes": 20,
    "tickSeconds": 60
  }
}
```

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `enabled` | bool | `true` | `false` is the kill switch: it restores the legacy behavior of injecting Claude's OAuth token once as an env var at container start, with no live refresh |
| `thresholdMinutes` | integer (≥ 1) | `20` | Trigger a host-side refresh once the token has fewer than this many minutes left |
| `tickSeconds` | integer (≥ 1) | `60` | How often the refresh monitor checks token expiry while a credentialed session is live |

All three keys are optional, and `authRefresh` may appear in either config file. Precedence is per field, not per block: a repository value overrides the matching global value, which overrides the built-in default — so a repository `authRefresh` object that sets only `enabled` still inherits `thresholdMinutes`/`tickSeconds` from the global block (or the built-in defaults) if the global block doesn't set them either. Like `auth`, `authRefresh` is file-edit-only; it is not available through `awman config set`.

**The squad daemon reads `authRefresh` from the global config only.** It builds its refresh monitor once, when the daemon starts, before any request has named a repository — so a repository-level `authRefresh` block does not apply to work the squad daemon runs, and its credential leases begin at daemon boot rather than at the first request. Set `authRefresh` in `$HOME/.awman/config.json` if you want it to govern squad.

---

## Runtimes

The global `runtime` key selects how agent processes are isolated from your host machine:

| Value | Platform | Notes |
|-------|----------|-------|
| `docker` (default) | Linux, macOS, Windows | Standard Docker; ephemeral containers torn down when the session ends |
| `apple-containers` | macOS 26+ only | Native `container` CLI; same user experience as Docker. On Linux/Windows this value is an error, not a silent fallback. `--allow-docker` is not supported under this runtime |
| `docker-sbx-experimental` | macOS arm64, Windows x86_64 | Docker Sandboxes (persistent microVMs per session; hypervisor-grade isolation). Requires the `sbx` CLI and a Docker account. Linux is blocked by an upstream virtiofs bug. See [Runtimes](11-runtimes.md) |

An unrecognized value (e.g. a typo) is a fatal error — awman never falls back to a different isolation model than the one you configured. CLI commands print the invalid value and the list of valid values, then exit; the TUI shows the same message in a startup modal (Enter quits). Fix the value in `$HOME/.awman/config.json` and relaunch.

`awman ready` validates the configured runtime before any other check and reports which one is active. For full details on platform support, setup, credential registration, and the persistent-sandbox lifecycle see [Runtimes](11-runtimes.md).

---

## Where global files live

awman keeps global config and data (workflows, skills, worktrees, API state) under one home directory, `~/.awman/` by default. You can relocate it:

| Priority | Variable | Config goes to | Data goes to |
|----------|----------|----------------|--------------|
| 1 | `AWMAN_CONFIG_HOME` | `$AWMAN_CONFIG_HOME/` | `$AWMAN_CONFIG_HOME/` |
| 2 | `XDG_CONFIG_HOME` / `XDG_DATA_HOME` | `$XDG_CONFIG_HOME/awman/` | `$XDG_DATA_HOME/awman/` |
| 3 | (none set) | `~/.awman/` | `~/.awman/` |

- `AWMAN_CONFIG_HOME` overrides everything; XDG variables are then ignored.
- The XDG variables are independent — if only one is set, the other falls back to `~/.awman/`.
- An XDG variable set to an empty string is treated as unset.
- awman does **not** migrate existing data when you change these variables; move `~/.awman/` contents yourself if needed.
- The shared SQLite database is `~/.awman/data/awman.db` at the default location. API logs, PID files, credentials, and session files remain under `~/.awman/api/`.
- The API server's storage root can be moved independently with `AWMAN_API_ROOT`; this does not move the shared database.

---

## Reference

### Per-repo config fields (`<git_root>/.awman/config.json`)

| JSON key | Type | Default | Meaning | Settable via `config set` |
|----------|------|---------|---------|---------------------------|
| `agent` | string | (unset → global `default_agent`) | Agent for this repo | yes (repo or global scope) |
| `launchMode` | `"stdio"` \| `"acp"` | `"stdio"` | Default agent launch mode for this repo | no (edit file) |
| `auto_agent_auth_accepted` | bool | (unset) | Records that you accepted the agent auth consent prompt; managed by awman, shown read-only | no (managed) |
| `terminal_scrollback_lines` | integer | 10000 | Scrollback lines in the container terminal | yes |
| `yoloDisallowedTools` | string array | `[]` | Tools forbidden under `--yolo`; replaces the global list entirely | yes |
| `workItems.dir` | string | `aspec/work-items` | Work items directory (relative to repo root or absolute) | yes, as `work_items.dir` |
| `workItems.template` | string | `<workItems.dir>/0000-template.md` | Template for new work items | yes, as `work_items.template` |
| `overlays` | string array | `[]` | Overlay specs (`dir(…)`, `env(…)`, `skill(…)`); merged with all other overlay sources | yes |
| `agentStuckTimeout` | integer (seconds) | 30 | Inactivity period before an agent is flagged as stuck | yes |
| `maxConcurrentAgents` | integer | (unset → unlimited) | Cap on concurrently-running workflow steps; overridden by `--max-concurrent` / `AWMAN_MAX_CONCURRENT_AGENTS` — see [Parallel workflows](05-workflows.md#parallel-workflows) | yes |
| `baseImage` | string | (unset → global) | Image tag for workflow setup/teardown containers — see [Workflows](05-workflows.md) | no (edit file) |
| `dockerfile` | string | `Dockerfile.dev` | Path to the project base Dockerfile, relative to repo root or absolute | no (edit file) |
| `dynamicWorkflows.agentsToModels` | object (agent → string array) | (unset → Dockerfile discovery) | Restricts a dynamic workflow's leader to this agent/model set — see [Dynamic Workflows](06-dynamic-workflows.md#configuring-dynamic-workflows) | yes, per agent as `dynamicWorkflows.agentsToModels.<agentName>` (comma-separated; empty value removes) |
| `dynamicWorkflows.maxConcurrentSteps` | integer | (unset → unlimited) | Advisory cap on concurrent workflow steps passed to the leader prompt | yes, as `dynamicWorkflows.maxConcurrentSteps` |
| `dynamicWorkflows.defaultLeader` | string (`agent::model`) | (unset) | Default leader agent/model for `exec workflow --dynamic`; overridden by `--leader` | yes, as `dynamicWorkflows.defaultLeader` |
| `dynamicWorkflows.guidance` | string array | (unset → no guidance block) | Project-specific instructions injected into the leader prompt as a bullet list — see [Dynamic Workflows](06-dynamic-workflows.md#configuring-dynamic-workflows) | yes, per entry as `dynamicWorkflows.guidance.<index>` (empty value removes) |
| `auth` | `"keychain"` \| `"passthrough"` \| `"none"` | `"keychain"` | Credential injection mode — see [Control credential injection](#control-credential-injection-auth-mode) | no (edit file) |
| `authRefresh` | object | (unset) | Live credential-refresh settings for Claude — see [Control credential refresh](#control-credential-refresh-authrefresh) | no (edit file) |

### Global config fields (`$HOME/.awman/config.json`)

| JSON key | Type | Default | Meaning | Settable via `config set --global` |
|----------|------|---------|---------|-------------------------------------|
| `default_agent` | string | (unset) | Agent used when no repo agent is configured | yes |
| `terminal_scrollback_lines` | integer | 10000 | Default scrollback for all repos | yes |
| `runtime` | string | `docker` | Container runtime: `docker`, `apple-containers`, `docker-sbx-experimental` | yes |
| `yoloDisallowedTools` | string array | `[]` | Machine-wide yolo tool denylist (unless a repo overrides it) | yes |
| `overlays` | string array | `[]` | Overlay specs applied to every project; additive with other sources | yes |
| `agentStuckTimeout` | integer (seconds) | 30 | Default agent-stuck timeout | yes |
| `maxConcurrentAgents` | integer | (unset → unlimited) | Machine-wide default cap on concurrently-running workflow steps (unless a repo overrides it) — see [Parallel workflows](05-workflows.md#parallel-workflows) | yes |
| `workers` | integer | 2 | API server worker tasks processing the command queue in parallel — see [API mode](09-api-and-remote-mode.md) | no (edit file) |
| `baseImage` | string | (unset) | Default image tag for workflow setup/teardown containers | no (edit file) |
| `api.workDirs` | string array | `[]` | Directories pre-approved for API session creation; merged with `--workdirs` at server start | yes |
| `api.alwaysNonInteractive` | bool | `false` | Force non-interactive mode for all dispatched commands (useful on API servers with no TTY) | no (edit file) |
| `remote.defaultAddr` | string | (unset) | Default remote awman API server address | yes |
| `remote.defaultAPIKey` | string | (unset) | API key for the default remote server; only sent when the target address matches `remote.defaultAddr` | yes |
| `remote.savedDirs` | string array | `[]` | Remote-host paths shown in the `remote session start` picker — see [Remote mode](09-api-and-remote-mode.md) | no (edit file) |
| `squad` | object | (unset) | Global squad daemon settings; see [Squad daemon configuration](#squad-daemon-configuration) | no (edit file) |
| `launchModeFallback` | `"stdio"` \| `"error"` | `"error"` | What to do when a requested ACP launch uses an agent without ACP support | no (edit file) |
| `authRefresh` | object | (unset) | Machine-wide default live credential-refresh settings for Claude (unless a repo overrides a field) — see [Control credential refresh](#control-credential-refresh-authrefresh) | no (edit file) |

### `awman config` subcommands

| Command | Effect |
|---------|--------|
| `awman config show` | Table of every known field: global, repo, and effective values |
| `awman config get <field>` | Global, repo, and effective value of one field |
| `awman config set <field> <value>` | Write a field to repo config |
| `awman config set --global <field> <value>` | Write a field to global config |

### Field names accepted by `config set` / `config get`

| Field name | Scope |
|------------|-------|
| `agent` | repo or global |
| `auto_agent_auth_accepted` | global only (read-only; managed by the auth flow) |
| `terminal_scrollback_lines` | repo or global |
| `yoloDisallowedTools` | repo or global |
| `workItems` | repo only |
| `overlays` | repo or global |
| `agentStuckTimeout` | repo or global |
| `maxConcurrentAgents` | repo or global |
| `runtime` | global only |
| `default_agent` | global only |
| `api` | global only |
| `remote` | global only in practice (see note) |
| `work_items.dir` | repo only |
| `work_items.template` | repo only |
| `api.workDirs` | global only |
| `api.port` | global only (default 9876) |
| `api.background` | global only |
| `remote.defaultAddr` | global only in practice (see note) |
| `remote.defaultAPIKey` | global only in practice (see note) |
| `dynamicWorkflows.defaultLeader` | repo only |
| `dynamicWorkflows.maxConcurrentSteps` | repo only |
| `dynamicWorkflows.agentsToModels` (and `.<agentName>`) | repo only |
| `dynamicWorkflows.guidance` (and `.<index>`) | repo only |

> **Note on `remote.*` scope.** `config set` accepts these at repo scope, but only the **global** file is ever read back — always pass `--global` when setting them.

`launchMode` and `launchModeFallback` are currently config-file-only fields;
they are not accepted by `config set` or `config get`. Edit the JSON files
shown above directly.

Value handling:

- `yoloDisallowedTools`, `overlays`, `api.workDirs` — comma-separated values are stored as arrays; an empty string stores an empty array.
- `terminal_scrollback_lines`, `agentStuckTimeout`, `api.port`, `maxConcurrentAgents`, `dynamicWorkflows.maxConcurrentSteps` — must be positive integers; `0` is rejected.
- `agent`, `default_agent` — validated against the supported agent list.
- `dynamicWorkflows.defaultLeader` — must be in `agent::model` format (exactly two non-empty, non-whitespace components).
- `dynamicWorkflows.guidance.<index>` — a single free-form instruction string, capped at 1,000 characters and 50 entries total; empty or whitespace-only values are rejected on load and coerced to a removal when set via `config set`.
- `envPassthrough` — removed; the error message points you to `env(VAR)` overlay entries.

### Environment variables

| Variable | Purpose |
|----------|---------|
| `AWMAN_CONFIG_HOME` | Relocate the entire global home (config + data); overrides XDG variables |
| `XDG_CONFIG_HOME` | Global config goes to `$XDG_CONFIG_HOME/awman/` |
| `XDG_DATA_HOME` | Global data (workflows, skills, worktrees, API state, and the shared database) goes to `$XDG_DATA_HOME/awman/` |
| `AWMAN_API_ROOT` | Relocate only the API server storage root |
| `AWMAN_OVERLAYS` | Comma-separated overlay specs (e.g. `env(TOKEN),dir(/a:/b:ro)`); merged with config and flags — see [Overlays](08-overlays.md) |
| `AWMAN_LAUNCH_MODE` | Choose `stdio` or `acp`; overrides repo `launchMode` and is overridden by `--launch-mode` |
| `AWMAN_MAX_CONCURRENT_AGENTS` | Cap on concurrently-running workflow steps; beats `maxConcurrentAgents` in repo/global config, beaten by `--max-concurrent` — see [Parallel workflows](05-workflows.md#parallel-workflows) |
| `AWMAN_REMOTE_ADDR` | Remote API server address; beats `remote.defaultAddr`, beaten by `--remote-addr` |
| `AWMAN_API_KEY` | Remote API key; beats `remote.defaultAPIKey`, beaten by `--api-key` |
| `AWMAN_SQUAD_KEY` | Bearer key the CLI and TUI authenticate to the squad daemon with; printed once as a shell snippet on the daemon's first start — see [squad: Authenticating to the daemon](12-squad.md#authenticating-to-the-daemon) |
| `AWMAN_REMOTE_SESSION` | Sticky session id for `remote exec` commands; beaten by `--session` |
| `AWMAN_ATTACH_DIR` | Relocate the directory attach sockets live in; defaults to `~/.awman/attach/` |
| `AWMAN_API_VERBOSE_SETUP` | Demote the API server's per-session setup logging from `info` to `debug`; set to `0`, `false`, `no` or `off` (case-insensitive) to quiet it. Verbose is the default. |
| `AWMAN_TEST_ISOLATION` | Set to `1` (or `true`, `yes`, `on`) to keep awman off your per-user OS resources: keychain reads, writes and deletes stay inside the awman process instead of reaching the macOS Keychain or Linux Secret Service; the TUI's copy actions go to an in-process buffer instead of the system clipboard; daemons start as plain background processes instead of through `launchd` or `systemd --user`; and downloads from the internet (the aspec template, agent Dockerfiles, GitHub issues and CI status) fail as if offline. Agent credentials stored in the keychain are not found, the squad daemon's `envPersistence` item lives only as long as the daemon, and a daemon started this way is not restarted at login. Intended for test runs; `make test` sets it. |
| `AWMAN_TEST_DOCKER` / `AWMAN_TEST_APPLE_CONTAINER` / `AWMAN_TEST_SBX` | Only matter when `AWMAN_TEST_ISOLATION` is set, where `docker`, Apple's `container` and `sbx` are treated as not installed. Set one to `1` to let awman use that real CLI again. `make test-full` sets `AWMAN_TEST_DOCKER`. |

When the API or squad daemon is started as a background service (`systemd --user` on Linux, `launchd` on macOS), the launching process forwards a small, non-secret allowlist of variables into that service's environment, so the daemon resolves the same config and storage root as the process that started it: `PATH`, `HOME`, `RUST_LOG`, `AWMAN_CONFIG_HOME`, `AWMAN_API_ROOT`, `AWMAN_SQUAD_ROOT`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `AWMAN_OVERLAYS`, `AWMAN_MAX_CONCURRENT_AGENTS` and `AWMAN_LAUNCH_MODE`. Bearer keys (`AWMAN_API_KEY`, `AWMAN_SQUAD_KEY`) are never forwarded this way — the daemon authenticates against the key hash it reads from its storage root instead.

---

[← Dynamic Workflows](06-dynamic-workflows.md) · [Next: Overlays →](08-overlays.md)
