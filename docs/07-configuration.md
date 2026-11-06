# Configuration

awman has one configuration tree. Every setting is a dotted key such as `agents.stuckTimeout` or `daemon.features.worker.push.workDirs.allow`, and the same key means the same thing everywhere it appears: in the JSON config files, in `awman config`, in `--set`, in the TUI config dialog, in a derived `AWMAN_*` environment variable, and in the daemon's `GET /v1/config` response. You rarely need to edit the files by hand: `awman config` can view and change almost everything.

---

## The config files

| File | Path | Holds |
|---|---|---|
| Global | `$AWMAN_HOME/config.json` (default `~/.awman/config.json`) | Settings for every project on this machine, and the machine-level settings (runtime, daemon, remotes) |
| Repository | `<git_root>/.awman/config.json` | Settings for one project; created by `awman init`; commit it so the team shares the setup |
| Squad task | `<AWMAN_HOME>/squad/tasks/<name>/config.json` | Settings for one [squad](12-squad.md) task; written when you answer the agent and model questions in `awman squad add` |

A running TUI tab or API session reloads the global and repository files before each command. Edits made after the session opened take effect on the next command. If a file is invalid, that command fails and the session keeps its last valid configuration until you fix the file.

Each file is JSON that mirrors the dotted keys as nested objects. A key written as `agents.stuckTimeout` is stored as:

```json
{ "agents": { "stuckTimeout": "30s" } }
```

A repository file:

```json
{
  "agents": {
    "default": "claude",
    "launchMode": "stdio",
    "stuckTimeout": "60s",
    "yolo": { "disallowedTools": ["Bash"] },
    "flags": { "codex": ["--config", "model_reasoning_effort=high"] }
  },
  "runtime": { "dockerfile": "docker/Dockerfile.base" },
  "overlays": ["env(ANTHROPIC_API_KEY)", "dir(/data/fixtures:/mnt/fixtures:ro)"],
  "tui": { "scrollbackLines": 10000 },
  "workItems": {
    "dir": "docs/work-items",
    "template": "docs/work-items/0000-template.md"
  },
  "workflows": {
    "maxConcurrentSteps": 3,
    "dynamic": {
      "agentsToModels": { "claude": ["claude-opus-4-8", "claude-sonnet-4-6"] },
      "defaultLeader": "claude::claude-opus-4-8",
      "guidance": ["Never spawn more than two agents in parallel."]
    }
  },
  "worktree": {
    "onDirtyMergeTarget": "ask",
    "mergeFix": { "onFailure": "agent", "promptTemplate": "prompts/merge-fix.md" }
  }
}
```

A global file:

```json
{
  "agents": { "default": "claude", "acpFallback": "error" },
  "runtime": { "kind": "docker" },
  "overlays": ["skill(*)", "env(ANTHROPIC_API_KEY)"],
  "daemon": {
    "maxConcurrentJobs": 2,
    "features": {
      "worker": {
        "enabled": true,
        "push": { "workDirs": { "allow": ["/home/user/my-project"], "roots": ["/home/user/projects"] } }
      }
    }
  },
  "remote": {
    "default": "prod",
    "targets": { "prod": { "url": "https://ci.example.com:9876", "apiKey": "a3f8b2c1..." } }
  }
}
```

### A config file that doesn't parse

A `config.json` that isn't valid JSON, that contains an unknown key, or that fails validation is an error — awman never falls back to defaults you didn't write, and it never silently ignores a key it doesn't recognise. The error names the file and the key, and lists the valid keys at that level. If the key is one that moved in an earlier release, the message says where it went:

```text
unknown config key `agentStuckTimeout` in /repo/.awman/config.json; valid keys here: agents, daemon, overlays, remote, runtime, tui, workItems, workServers, workflows, worktree; `agentStuckTimeout` moved to `agents.stuckTimeout` ("30s")
```

Fix the file (or restore the repository one from git) and try again. A key placed in a file that may not hold it (a `daemon.*` key in a repository file, say) is rejected the same way, naming the files that may. `config set` refuses any edit that would make a file invalid, so this only happens after a hand edit.

---

## Precedence and merge rules

For any setting, the highest-priority source that defines it wins:

```text
command-line flag  >  --set  >  environment variable  >  squad task file  >  repo file  >  global file  >  built-in default
```

- **Flags** such as `--agent`, `--model` or `--yolo` are per-invocation intent and beat everything for the setting they name.
- **`--set key=value`** overrides one config key for one invocation. It is repeatable and accepted by every command. See [`--set`](#--set-and-derived-environment-variables).
- **Environment variables** named after the key (`AWMAN_AGENTS_STUCK_TIMEOUT`) override files.
- **The squad task file** only exists for squad evaluations and only holds task-scope keys.
- **Repo and global files** are merged key by key: a leaf the repo sets replaces the global leaf; leaves the repo doesn't mention are inherited.

How values combine:

- **Scalars, lists and maps replace; they don't merge.** A repo `agents.yolo.disallowedTools` list completely replaces the global one — even an empty list. To inherit the global list, omit the key from the repo file. A map such as `agents.flags` or `remote.targets` is replaced as a whole by a higher layer that sets it; to change one entry, set that entry's key (`agents.flags.codex`) — in a file, an entry the higher layer doesn't mention falls through only when the higher layer doesn't define the map at all.
- **Two keys are additive.** `overlays` and `workflows.dynamic.guidance` concatenate across layers (global, then repo, then task, then environment, then `--set`), with duplicates dropped, keeping the first occurrence. `AWMAN_OVERLAYS=ssh()` therefore *adds* to the overlays in your files. The table below marks them "(additive)". `workflows.dynamic.guidance` may hold at most 50 entries after merging.
- **`null` resets.** A `null` from a higher layer (`--set agents.default=null`) puts the key back to its built-in default.
- **Scope limits where a key may live.** Each key has a scope. A `global` key may appear only in the global file. A `repo` key may appear in the global or repo file. A `task` key may also appear in a squad task file. Credential policy (`agents.auth.*`), `runtime.*`, `tui.*`, `daemon.*`, `workServers.*` and `remote.*` are never task scope.
- **Repo files can only narrow trust-sensitive settings.** A repository file ships with the checkout and may be untrusted, so it cannot widen credential and network policy the global file sets: it may only narrow `runtime.builtin.network.*` (a stricter `mode`, a subset of the global `allow` list and `hostPorts`), and `workflows.prerequisites.*` follows the narrowing rules in [Workflow prerequisites](#workflow-prerequisites).

### Live and restart-only keys

The **Live** column of the [reference table](#reference-all-keys) says whether a running daemon picks a change up without restarting. `awman config set` of a live key tells the running daemon to reload (`POST /v1/config/reload`) and prints `applied to the running daemon`. A key that is not live prints `restart the daemon to apply`. A daemon keeps the `--set` overrides it was started with across reloads, and a reload that finds an invalid file leaves the running configuration untouched.

---

## Managing config from the terminal

```sh
awman config                                  # same as `config show`
awman config show [--json]                    # every key: global, repo, effective value, source, type
awman config get <key> [--json]               # one key, all scopes, and where the effective value comes from
awman config set <key> <value>                # write to the file the key's scope selects
awman config set <key> <value> --global       # force the global file
awman config set <key> <value> --repo         # force this repository's file
awman config set <key> <value> --task <name>  # a squad task's file (task-scope keys only)
awman config unset <key> [--global|--repo|--task <name>]
```

- `show` prints one row per key in a fixed order. `Source` names the layer that won: `default`, `global`, `repo`, `task`, `env` or `set`. `--task <name>` adds the task column. A `state` block at the end shows per-repository state (see [state.json](#statejson)), and a note says which runtimes are experimental.
- `config set` picks the file by scope: a `global` key goes to the global file; a `repo` or `task` key goes to the repository file inside a Git repository and to the global file outside one. `--global`, `--repo` and `--task <name>` force a file. `--repo` outside a repository, and `--task` for a key that isn't task scope, are errors that name the files the key may live in. The command prints `wrote <path>`.
- Values are parsed by the key's type: lists are comma-separated (`a,b,c`), maps are `k=v,k2=v2`, durations look like `500ms`, `30s`, `15m`, `2h`, and sizes look like `512MiB` or `4GiB`. A value that doesn't parse is rejected before anything is written.
- Registry hosts and image tags are map names that may contain dots (`runtime.builtin.registries.ghcr.io`): everything after the map's prefix is one name, so `get`, `set` and `unset` address it as written. Once a named entry such as `workServers.prod` exists, `show` and `get` report the table default for each of its missing settings (source `default`).
- Map entries are addressed by name: `awman config set workServers.ci.url https://…`, `awman config set remote.targets.prod.apiKey …`. `config unset workServers.ci` removes the whole entry; `config unset workServers.ci.url` removes one field.
- Object-valued keys (`runtime.builtin.imageSource`, `runtime.builtin.images.*`, `runtime.builtin.registries.*`, `tui.integrations.*`) and per-agent `agents.flags.*` lists that begin with `-` are edited in the JSON file; `config set` says so.
- Sensitive values (`remote.targets.*.apiKey`) are masked in `show`, `get` and `set` output as `••••` plus the last four characters; the stored value is unchanged.
- Unknown keys list their siblings, and an old key name suggests its replacement.

### `--set` and derived environment variables

`--set` works on any command, anywhere on the line, and beats config files and environment variables for that one invocation:

```sh
awman exec workflow wf.toml --set workflows.maxConcurrentSteps=2
awman chat --agent cline --set agents.launchMode=acp
awman --set agents.stuckTimeout=2m --set overlays=env(MY_TOKEN) chat
```

An unknown key, or a value of the wrong type, is a usage error before the command starts. Error messages name the key and the expected type, never the rejected value of a sensitive key such as `remote.targets.<name>.apiKey`. A `--set` that would leave two map names differing only by `-` versus `_` (`in-fact` and `in_fact`) is refused, as is the same clash between two config layers. `remote.*` settings given with `--set` configure the invoking process only; `--remote` never sends them to the server. In the TUI's command box, `--set` applies to that command only. API clients that submit commands pass `--set` in `args`.

Every key with an **Env** entry in the table has an environment variable derived from its path: `AWMAN_` plus the path in SCREAMING_SNAKE_CASE. Dots and capital-letter boundaries become underscores (`agents.stuckTimeout` → `AWMAN_AGENTS_STUCK_TIMEOUT`, `daemon.features.worker.push.workDirs.allow` → `AWMAN_DAEMON_FEATURES_WORKER_PUSH_WORK_DIRS_ALLOW`). For a named map entry the name is upper-cased with `-` turned into `_`: `AWMAN_REMOTE_TARGETS_PROD_API_KEY`, `AWMAN_AGENTS_FLAGS_CLAUDE`. Lists are comma-separated, like `config set`. An empty variable counts as unset, a value that doesn't parse is an error naming the variable, and an `AWMAN_*` variable that matches no key is ignored with a logged warning — so check `awman config show` if a variable seems to have no effect.

When a daemon is started as a background service (`systemd --user` on Linux, `launchd` on macOS), awman forwards every `AWMAN_*` config variable present at launch, plus `PATH`, `HOME`, `RUST_LOG`, `XDG_CONFIG_HOME` and `XDG_DATA_HOME`, so the service resolves the same configuration as the process that started it. Bearer keys are never forwarded this way.

---

## Process variables that are not config

These variables select where awman lives or switch behavior at the process level. They are not config keys, are not derived from the table, and have no `--set` form.

| Variable | Purpose |
|---|---|
| `AWMAN_HOME` | Relocate the entire awman home — config file and all data — to one directory. Overrides the XDG variables |
| `XDG_CONFIG_HOME`, `XDG_DATA_HOME` | When `AWMAN_HOME` is unset: config goes to `$XDG_CONFIG_HOME/awman/`, data to `$XDG_DATA_HOME/awman/` |
| `HOME`, `SHELL`, `PATH`, `RUST_LOG` | Read as usual |
| `AWMAN_REMOTE_SESSION` | Sticky session ID for `remote exec` commands; beaten by `--session` |
| `AWMAN_SQUAD_KEY` | Bearer key the CLI and TUI authenticate to the squad daemon with; printed once as a shell snippet on the daemon's first start — see [Squad: Authenticating to the daemon](12-squad.md#authenticating-to-the-daemon) |
| `GITHUB_TOKEN`, `GIT_SSH`, `GIT_SSH_COMMAND` | Read when workflow prerequisites offer a host credential |
| `DOCKER_HOST`, `DOCKER_TLS_VERIFY`, `DOCKER_CERT_PATH` | Read by Docker, and by the builtin runtime when importing from a `docker-local` source that sets no `host` |
| `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY` | Honored when the builtin runtime pulls from a `registry` source |
| `MSB_PATH`, `MSB_LIBKRUNFW_PATH`, `MSB_AGENTD_PATH`, `MSB_HOME`, `MSB_BACKEND`, `MSB_CONFIG_PATH`, `MSB_PROFILE`, `MSB_CACHE_DIR`, `MSB_SANDBOXES_DIR`, `MSB_VOLUMES_DIR`, `MSB_SNAPSHOTS_DIR`, `MSB_LOGS_DIR`, `MSB_SECRETS_DIR` | Refused when set while starting the builtin runtime; unset them before launching builtin sessions |
| `AWMAN_TEST_ISOLATION` | Set to `1` (or `true`, `yes`, `on`) to keep awman off your per-user OS resources: keychain reads and writes stay inside the awman process, the TUI's copy actions go to an in-process buffer, daemons start as plain background processes, and downloads from the internet fail as if offline. Intended for test runs; `make test` sets it |
| `AWMAN_TEST_DOCKER`, `AWMAN_TEST_APPLE_CONTAINER`, `AWMAN_TEST_SBX`, `AWMAN_TEST_BUILTIN`, `AWMAN_TEST_YOLO_COUNTDOWN_MS` | Test-run switches that only matter under `AWMAN_TEST_ISOLATION`; ignored in every normal run |

`AWMAN_GIT_TOKEN` is something awman sets *inside* agent containers, not a variable you set on the host.

### Where awman keeps its files

Everything lives under one home directory:

| Priority | Variable | Config goes to | Data goes to |
|---|---|---|---|
| 1 | `AWMAN_HOME` | `$AWMAN_HOME/` | `$AWMAN_HOME/` |
| 2 | `XDG_CONFIG_HOME` / `XDG_DATA_HOME` | `$XDG_CONFIG_HOME/awman/` | `$XDG_DATA_HOME/awman/` |
| 3 | (none set) | `~/.awman/` | `~/.awman/` |

```text
$AWMAN_HOME/
  config.json                      global config
  data/awman.db                    sessions, commands, runs and squad tasks
  data/repos/<repo-hash>/state.json
  api/                             API server storage: sessions, TLS material, key hash, logs
  squad/                           squad storage: task workspaces and config, builds, logs
  attach/                          attach sockets
  builtin/                         default runtime.builtin.stateDir
  prerequisite_grants.json         remembered workflow credential choices
  workflows/  skills/  worktrees/  agents/  logs/ …
```

- `AWMAN_HOME` overrides everything; the XDG variables are then ignored.
- The XDG variables are independent — if only one is set, the other falls back to `~/.awman/`. An empty XDG variable counts as unset.
- awman does **not** move existing data when you change these variables; move the directory yourself if needed.
- Your approvals of [custom agent](15-custom-agents.md) definitions are recorded in `<home>/trusted-agents.json`, and global agent definitions live in `<home>/agents/<name>.toml`; both follow the relocated home.

### `state.json`

Facts awman remembers about a repository that are not settings live outside the config files, in `<AWMAN_HOME>/data/repos/<repo-hash>/state.json`. Today it records `autoAgentAuthAccepted` (consent for automatic agent authentication), which `awman config show` prints under its `state` heading together with the file's path. Delete the file to clear it. It is never read as configuration, so it cannot be set with `config set` or `--set`.

---

## Workflow prerequisites

Workflow prerequisite policy lives under `workflows.prerequisites.*`. It controls checks for commit identity, SSH pushes, and GitHub tokens used by workflow teardown steps. Use these exact names and casing:

```json
{
  "workflows": {
    "prerequisites": {
      "preflight": "warn",
      "unresolved": "fail",
      "gitIdentity": { "use": "ask", "name": "Workflow Bot", "email": "bot@example.com" },
      "ssh": { "use": "ask", "key": "~/.ssh/id_ed25519" },
      "githubToken": {
        "use": "ask",
        "envVars": ["GH_TOKEN", "GITHUB_TOKEN"],
        "acquire": {
          "argv": ["op", "read", "op://Engineering/GitHub/token"],
          "timeout": "30s",
          "refresh": "before_teardown"
        },
        "verify": true
      },
      "allowApiClientGrants": false
    }
  }
}
```

All keys are optional. `preflight` is `warn` (the default) or `off`; `off` skips probes and does not grant credentials, and repository config cannot turn checks off. `unresolved` is `warn` or `fail`; repository config cannot relax a global `fail`. `fail` stops when a requirement remains missing or needs consent. Each credential's `use` is `ask`, `allow`, or `deny`. `gitIdentity.name` and `email` set a fixed identity; otherwise a granted host identity is used only for that workflow run. An SSH `key` is a suggested key path, not permission to mount it. SSH key use still requires an interactive choice, an explicit `--prerequisite-ssh` flag, or an allowed global policy.

`githubToken.envVars` lists host environment variables awman may offer; the defaults are `GH_TOKEN` and `GITHUB_TOKEN`. A repo may only narrow that list to names already allowed globally. `verify` enables best-effort token checking and a repository cannot turn global verification off. `acquire.argv` is an argument array, not a shell command string. It runs on the host when selected by an interactive user or allowed by policy. A repo-supplied acquisition command requires approval for its exact argument list and resolved executable; changing the command requires approval again. `acquire.timeout` defaults to `30s` (maximum `5m`). `acquire.refresh` is `start` (default) or `before_teardown`; the latter lets an already-granted acquisition command refresh a token before teardown. `acquire` is global-scope: it cannot be set in a repository file.

Repository config cannot grant access to host credentials. It can ask or deny, narrow globally allowed settings, provide a fixed Git identity, and suggest an SSH key. Put broad grants in the global file or choose them when prompted. `allowApiClientGrants` is global-only; when false, API callers cannot use prerequisite flags to grant credentials beyond the configured policy.

Remembered SSH and token choices are stored as decisions in `<AWMAN_HOME>/prerequisite_grants.json`; the file does not contain credentials. A remembered choice applies only to its repository, remote, and credential type. Each remembered choice has a `grant_…` ID. Review or remove them with the daemon API:

```sh
awman daemon api grants.list
awman daemon api grants.delete id=grant_0123456789abcdef0123456789abcdef
awman daemon api grants.delete all
```

See [API and remote mode](09-api-and-remote-mode.md#awman-daemon-api).

| Key (`workflows.prerequisites.` prefix) | Default | Where it may be set |
|---|---|---|
| `preflight` | `warn` | Global or repo; repo cannot set `off` |
| `unresolved` | Headless static workflows: `fail`; interactive, dynamic, squad: `warn` | Global or repo; repo may only set `fail` |
| `gitIdentity.use`, `.name`, `.email` | `ask`, unset, unset | Global or repo; a fixed repo identity is non-secret |
| `ssh.use`, `.key` | `ask`, unset | Global or repo; a repo key is only a suggestion and a repo cannot grant `allow` |
| `githubToken.use`, `.envVars`, `.verify` | `ask`, `["GH_TOKEN","GITHUB_TOKEN"]`, `true` | Global or repo; repo may only narrow |
| `githubToken.acquire.*` | unset, `30s`, `start` | Global only |
| `allowApiClientGrants` | `false` | Global only |

Squad evaluations resolve `workflows.prerequisites` through the same chain as everything else — global, then the repo, then the task file. A task's own decisions override only the credentials it explicitly chooses, and `--prerequisite-token acquire` keeps the trusted global acquisition command.

## Squad daemon configuration

Squad settings are ordinary keys: the daemon's behavior lives under `daemon.features.squad.*`, and the agents, models, leader and guidance used for task evaluations are the same `workflows.dynamic.*` keys that govern [dynamic workflows](06-dynamic-workflows.md#configuring-dynamic-workflows). Task-specific workspace, interval, and overlay choices are set with `awman squad add`; see [Squad](12-squad.md). The squad daemon and API server cannot run at the same time because they share the awman database; see [API server and squad daemon](09-api-and-remote-mode.md#api-server-and-squad-daemon).

| Key | Default | Meaning |
|---|---|---|
| `daemon.features.squad.enabled` | `false` | Whether the daemon runs the squad feature |
| `daemon.features.squad.maxConcurrentEvaluations` | `2` | Maximum number of task evaluations running at once |
| `daemon.features.squad.maxConcurrentJobs` | `1` | Maximum number of task runs executing at once |
| `daemon.features.squad.interaction` | `interactive` | `interactive` or `headless` for squad runs |
| `daemon.features.squad.envPersistence` | `keychain` | Where the daemon persists the values it holds for `env()` overlays across an OS-initiated restart (launchd/systemd). `none` opts out entirely: nothing is persisted anywhere. See [Squad: Task environment values](12-squad.md#task-environment-values) |
| `workflows.dynamic.agentsToModels.<agent>` | unset | Models available for each agent; every map key must be a valid agent name |
| `workflows.dynamic.defaultLeader` | unset | Default agent and model (`agent::model`) for evaluations when a task does not specify them |
| `workflows.dynamic.guidance` | unset | Instructions added to every task evaluation and generated workflow |

Configuration is rejected when a concurrency value is zero; an `agentsToModels` entry has no models or contains an empty model name; `defaultLeader` is not exactly two non-empty components in `agent::model` form, has surrounding whitespace in either component, or uses an invalid agent name; or `guidance` has an empty or whitespace-only entry. A `guidance` list may contain at most 50 entries after merging, and each entry is limited to 1,000 characters. Agent names use ASCII letters, digits, `-`, and `_`, and are 1–64 characters long.

`daemon.features.squad.envPersistence` describes the daemon process itself, not any one task, so it is global scope and a task file cannot override it. If the keychain is configured but unusable on this machine (no keychain backend, a missing `secret-tool`, a locked collection, or a probe that fails or times out), the daemon falls back to persisting nothing for that run rather than failing to start — `awman squad status` and `awman squad env` report the fallback and the reason as `persistence: unavailable(<reason>)`. An explicit `none` is never probed for and never produces that fallback message, because it's a deliberate choice rather than a degradation.

### Per-task squad settings

A squad task can carry its own config file, beside its durable workspace:

```text
<AWMAN_HOME>/squad/tasks/<name>/config.json
```

It is a normal config file that may hold **task-scope** keys only (`workflows.*`, `overlays`, `agents.default`, `agents.flags.*`, `agents.yolo.disallowedTools`, `agents.stuckTimeout`, `agents.launchMode`):

```json
{ "workflows": { "dynamic": { "agentsToModels": { "claude": ["claude-opus-4-8"] } } } }
```

A key the task sets wins for that task; keys it omits are inherited from the repo and global files, so a task can narrow its agent pool while keeping the standing `guidance` every task gets. Edit it with `awman config set <key> <value> --task <name>`. Daemon-wide keys such as `daemon.features.squad.maxConcurrentEvaluations` and `envPersistence` are rejected in a task file.

awman writes the file for you when you answer the agent and model questions during `awman squad add` — see [Choosing a task's agents and models](12-squad.md#choosing-a-tasks-agents-and-models). Edits take effect on the daemon's next scheduling tick. A task file that does not parse fails that one task's next run with an error naming the file, rather than being silently ignored.

---

## Common recipes

### Set the default agent

```sh
awman config set agents.default gemini --global   # for all projects
awman config set agents.default codex             # for this repo only
```

Valid agents: `claude`, `codex`, `gemini`, `opencode`, `cline`, `copilot`, `antigravity`, or the name of a [custom agent](15-custom-agents.md). Custom definitions are TOML files:

| Path | Scope |
|---|---|
| `<git_root>/.awman/agents/<name>.toml` | This repo (wins over a global definition of the same name) |
| `<AWMAN_HOME>/agents/<name>.toml` | Every repo |

A repo-scope write accepts custom agents defined in the repo or globally; a `--global` write accepts only globally defined ones. Anything else is rejected at write time with the list of valid agents.

### Adjust terminal scrollback

```sh
awman config set tui.scrollbackLines 20000 --global   # all projects
awman config set tui.scrollbackLines 5000             # this repo
```

A 10,000-line buffer at 80 columns uses roughly 3 MB per tab. Increase for long build logs; decrease when running many tabs.

### Pass API keys into agent containers

awman never forwards your whole environment into a container — name each variable explicitly as an `env()` overlay:

```sh
awman config set overlays "env(ANTHROPIC_API_KEY),env(OPENAI_API_KEY)" --global
```

Or per-invocation: `awman chat --overlay "env(ANTHROPIC_API_KEY)"`. See [Overlays](08-overlays.md) for the full syntax and [Agent Sessions](03-agent-sessions.md) for per-agent authentication details.

### Switch container runtime

```sh
awman config set runtime.kind docker     # default
awman config set runtime.kind apple      # macOS only (Apple Containers)
awman config set runtime.kind sbx        # experimental (Docker Sandboxes)
awman config set runtime.kind builtin    # experimental; Apple Silicon only
```

See [Runtimes](#runtimes) below. The `builtin` runtime also has its own `runtime.builtin.*` settings (VM size, image sources); see [Builtin runtime settings](#builtin-runtime-settings).

### Choose ACP launch mode

To use ACP by default in this repository:

```sh
awman config set agents.launchMode acp
```

For one run, `--set agents.launchMode=acp` (or `AWMAN_AGENTS_LAUNCH_MODE=acp`) does the same without touching a file. The allowed values are `stdio` (the default) and `acp`. To choose what happens when a workflow step's agent does not support ACP, set `agents.acpFallback`:

```sh
awman config set agents.acpFallback stdio
```

Its allowed values are `error` (the default) and `stdio`. See [ACP launch mode](03-agent-sessions.md#acp-launch-mode).

### Custom work item paths

By default awman looks for work items in `aspec/work-items/` and uses `aspec/work-items/0000-template.md` as the template for `awman new spec`. To use different paths:

```sh
awman config set workItems.dir docs/work-items
awman config set workItems.template docs/work-items/0000-template.md
```

Paths may be relative to the repo root (recommended) or absolute.

### Cap concurrent agents in a workflow

```sh
awman config set workflows.maxConcurrentSteps 2 --global   # machine-wide default
awman config set workflows.maxConcurrentSteps 4            # this repo only
```

Or per-invocation, without touching any config file:

```sh
awman exec workflow aspec/workflows/implement-hard.toml --set workflows.maxConcurrentSteps=2
AWMAN_WORKFLOWS_MAX_CONCURRENT_STEPS=2 awman exec workflow aspec/workflows/implement-hard.toml
```

Leaving it unset (the default) means unlimited — every step whose dependencies are satisfied launches immediately. Lower it to match your machine's CPU/memory budget or your Docker daemon's capacity. The same key is the cap the dynamic-workflow leader is told about. Interactive final yolo groups launch all remaining agents together so they can stay alive through review. See [Parallel workflows](05-workflows.md#parallel-workflows).

### Configure dynamic workflow agents, models, and leader

```sh
awman config set workflows.dynamic.defaultLeader claude::claude-opus-4-8
awman config set workflows.dynamic.agentsToModels.claude "claude-opus-4-8, claude-sonnet-4-6"
awman config set workflows.dynamic.guidance "Never spawn more than two agents in parallel."
```

`workflows.dynamic.agentsToModels` is a map from agent name to a list of models; set one agent at a time, as above (`config unset workflows.dynamic.agentsToModels.claude` removes the mapping). `workflows.dynamic.guidance` is a list of instructions the leader agent must follow whenever it designs a workflow; layers add to each other. Use `--global` to write to global config, or the TUI config dialog's Global or Repo column.

See [Dynamic Workflows](06-dynamic-workflows.md#configuring-dynamic-workflows) for the full reference.

### Custom Dockerfile path

By default awman builds the project base image from `<git_root>/Dockerfile.dev`. To use a Dockerfile elsewhere:

```sh
awman config set runtime.dockerfile docker/Dockerfile.base
```

If the configured file doesn't exist, commands report the exact configured path rather than silently falling back to the default. `awman init` also offers to point at an existing Dockerfile interactively when no `Dockerfile.dev` is found.

### Restrict tools in yolo mode

```sh
awman config set agents.yolo.disallowedTools "Bash,computer"   # this repo
awman config set agents.yolo.disallowedTools ""                # an empty list
```

An empty repo list actively overrides a non-empty global list. To stop overriding, `awman config unset agents.yolo.disallowedTools`. See [Permission modes](03-agent-sessions.md#permission-modes).

### Pass extra flags to an agent

To pass an agent a flag awman doesn't add on its own (for example, one a new agent release needs before awman knows about it), add it under `agents.flags`, keyed by agent name, in either config file:

```json
{
  "agents": {
    "flags": {
      "codex": ["--config", "model_reasoning_effort=high"],
      "claude": []
    }
  }
}
```

awman appends each agent's list to the command it launches, after the flags awman adds itself (mode, model, tools, system prompt) and before the initial prompt. With `--yolo`, the printed command for an interactive codex session looks like:

```text
codex --no-daemon --dangerously-bypass-approvals-and-sandbox --config model_reasoning_effort=high "<prompt>"
```

The list applies to every launch of that agent, interactive and headless (`codex exec`, `claude --print`). So pick flags both modes accept, or set them per run with `--agent-flag`. awman already runs interactive Codex with `--no-daemon`, so don't add it here: awman drops it from interactive launches rather than pass it twice, warns at launch, and `codex exec` rejects it.

- **One entry per argument.** A flag that takes a value is two entries (`["--config", "key=value"]`) or one entry in its `=` form (`["--config=key=value"]`). Entries are passed exactly as written; there is no shell parsing.
- **Repo replaces global, per agent.** In the example, the repo's `"claude": []` turns off a global `claude` entry for this repo. Because maps replace as a whole when a higher layer sets them, `agents.flags` entries come from one layer at a time per agent: set the entry in the layer you want it from.
- **For one run, use `--agent-flag`.** `awman chat --agent-flag=--search` replaces the configured list for the agent that command launches. See [`--agent-flag`](03-agent-sessions.md#--agent-flag-arg).
- **Workflows:** every step gets the entry for the agent that step runs, including steps a dynamic workflow's leader generates, and the leader itself. Runs in a worktree (`--worktree`, `--yolo`, `--auto`, `--dynamic`) read `agents.flags` and the repo's `agents.default` from the repository you launched from, so an edit you haven't committed still applies.
- **Remote runs:** `remote exec prompt` and `remote exec workflow` forward `--agent-flag` to the remote host. Without it, the remote host uses its own `agents.flags`.
- **Docker Sandboxes runtime:** only agents with an argv channel (`kind: agent` kits) receive the flags; for the others awman prints a warning and launches without them.

`agents.flags` is edited in the config file; `config set agents.flags.<agent>` works for values that don't start with `-`.

### Control credential injection (`agents.auth.mode`)

By default awman injects host keychain credentials into agent containers (`keychain` mode). Two alternatives are available for harnesses that supply credentials through other means:

```sh
awman config set agents.auth.mode passthrough
```

| Value | Behaviour |
|---|---|
| `keychain` (default) | Inject host keychain credentials. When the repo also declares `env(ANTHROPIC_API_KEY)` (or another credential that covers the same provider) **and that variable is set on the host**, the keychain OAuth token for that provider is automatically suppressed at injection time — the container receives exactly one set of credentials per provider. If the declared passthrough var is not set on the host, the keychain credential is retained so the container is not left with zero credentials for that provider. |
| `passthrough` | No keychain credential injection; declared `env()` overlays still apply. Supply credentials via `env(VAR)` overlays. |
| `none` | No keychain credential injection; declared `env()` overlays still apply. |

`agents.auth.*` is repo scope (global or repo file), never task scope. Cloud harnesses that do not declare an anthropic env var remain on the default `keychain` path and continue to receive keychain OAuth unaffected.

### Control credential refresh (`agents.auth.refresh.*`)

Under `keychain` auth mode, awman keeps Claude's containerized OAuth credential fresh for the life of a session: it periodically pings your host's Claude Code installation to rotate the token, then rewrites the staged credential file — see [Live credential refresh](04-security-and-isolation.md#live-credential-refresh). This is on by default. Tune it, or turn it off:

```json
{
  "agents": { "auth": { "refresh": { "enabled": true, "threshold": "20m", "tick": "60s" } } }
}
```

| Key | Type | Default | Meaning |
|---|---|---|---|
| `agents.auth.refresh.enabled` | bool | `true` | `false` is the kill switch: it restores the legacy behavior of injecting Claude's OAuth token once as an env var at container start, with no live refresh |
| `agents.auth.refresh.threshold` | duration | `20m` | Trigger a host-side refresh once the token has less than this left |
| `agents.auth.refresh.tick` | duration | `60s` | How often the refresh monitor checks token expiry while a credentialed session is live |

Precedence is per key: a repository value overrides the matching global value, which overrides the built-in default. **The squad daemon reads these from the global config only.** It builds its refresh monitor once, when the daemon starts, before any request has named a repository — so a repository-level value does not apply to work the squad daemon runs. Set them in the global file if you want them to govern squad.

---

## Runtimes

`runtime.kind` selects how agent processes are isolated from your host machine:

| Value | Platform | Notes |
|---|---|---|
| `docker` (default) | Linux, macOS, Windows | Standard Docker; ephemeral containers torn down when the session ends |
| `apple` | macOS 26+ only | Native `container` CLI; same user experience as Docker. On Linux/Windows this value is an error, not a silent fallback. `--allow-docker` is not supported under this runtime |
| `sbx` | macOS arm64, Windows x86_64 (experimental) | Docker Sandboxes (persistent microVMs per session; hypervisor-grade isolation). Requires the `sbx` CLI and a Docker account. Linux is blocked by an upstream virtiofs bug. See [Runtimes](11-runtimes.md) |
| `builtin` | macOS on Apple Silicon only (experimental) | MicroVM per session embedded in the awman binary; needs no Docker, `container`, or `sbx` to run agents. Included in the `awman-macos-arm64` release, which is ad-hoc signed (see [Gatekeeper and permissions](11-runtimes.md#the-macos-binary-gatekeeper-and-permissions)). Must be set explicitly in the global config. `awman ready` imports images from a configured source (and, with `runtime.builtin.builder.enabled`, can build them first in a local Docker or Apple Containers store). See [Runtimes](11-runtimes.md#builtin-microvm-experimental) |

`sbx` and `builtin` are experimental; the value itself never says so. An unrecognized value (e.g. a typo, or one of the old spellings) is a fatal error — awman never falls back to a different isolation model than the one you configured. CLI commands print the invalid value and the list of valid values, then exit; the TUI shows the same message in a startup modal (Enter quits). `runtime.kind` is global scope: fix it in the global file and relaunch.

`awman ready` validates the configured runtime before any other check and reports which one is active. For full details on platform support, setup, credential registration, and the persistent-sandbox lifecycle see [Runtimes](11-runtimes.md).

### Builtin runtime settings

The `runtime.builtin.*` keys are read only when `runtime.kind` is `builtin`. They can appear in both the global and the repo config; repo values win per key, and the `images` and `registries` maps are replaced as a whole when the repo sets them. Unknown keys are rejected. `network`, `vcpus`, `memory`, `rootDisk`, `mountQuota`, `registries`, the image sources and `builder.enabled` are resolved for each session from that session's own repo, so the TUI, the API and squad each honour the repo a session runs in, even when several repos share one awman process. A workflow worktree keeps the `runtime.builtin` settings of the checkout you launched from, so a narrowing you haven't committed still applies. An invalid repo setting (for example one that widens the global `network`) fails that repo's launches only; it doesn't stop a TUI or API server started in that repo, and other runtimes never read it.

`runtime.builtin.stateDir` is the exception: the state directory is process-wide. If a repo's `stateDir` resolves to a different directory than the one the awman process uses, the launch is refused with an error naming both paths (`this repository's runtime.builtin.stateDir resolves to <repo path>, but this awman process uses the builtin state directory <process path>`). Remove the repo's `stateDir` or set it to the process's path. `AWMAN_RUNTIME_BUILTIN_STATE_DIR` and `--set` outrank every file, so they never cause this mismatch.

```json
{
  "runtime": {
    "kind": "builtin",
    "builtin": {
      "vcpus": 4,
      "memory": "8GiB",
      "builder": { "enabled": true },
      "imageSource": { "type": "docker-local" },
      "images": {
        "awman-myproject-claude:latest": { "type": "archive", "path": "/opt/images/claude.tar" }
      },
      "registries": {
        "registry.example.com": {
          "caCert": "/etc/ssl/corp-ca.pem",
          "auth": { "type": "env", "usernameVar": "REG_USER", "passwordVar": "REG_PASS" }
        }
      },
      "network": { "mode": "public", "hostPorts": [8765] }
    }
  }
}
```

To build with Apple Containers instead of Docker, use an `apple-local` source:

```json
{
  "runtime": {
    "kind": "builtin",
    "builtin": { "builder": { "enabled": true }, "imageSource": { "type": "apple-local" } }
  }
}
```

| Key | Default | Meaning |
|---|---|---|
| `runtime.builtin.stateDir` | `<AWMAN_HOME>/builtin` | Private state root for imported images, catalog, control sockets and VM/session state; mode `0700`, no symlinks, keep it short. Process-wide (see above) |
| `runtime.builtin.vcpus` | `2` | Whole vCPUs per session VM (1–255) |
| `runtime.builtin.memory` | `4GiB` | Guest memory, 128MiB or more |
| `runtime.builtin.rootDisk` | `30GiB` | Writable root disk (everything outside the mounts), 256MiB or more; sparse, so it only uses host space as the agent writes |
| `runtime.builtin.mountQuota` | `30GiB` | How much the agent may add to each host mount, the workspace included, beyond what it already held (1MiB or more) |
| `runtime.builtin.imageSource` | unset | Default image source: an object whose `type` is `registry`, `docker-local`, `archive`, or `apple-local` (macOS) |
| `runtime.builtin.builder.enabled` | `false` | Opt in to local source-matched Docker or Apple Containers builds during `awman ready`, and when a `chat` or `exec prompt` launch finds its agent image missing; see [Building from a local image source](11-runtimes.md#building-from-a-local-image-source). Repository value overrides global |
| `runtime.builtin.images.<tag>` | `{}` | Per-image source; wins over `imageSource` for that tag |
| `runtime.builtin.registries.<host>` | `{}` | Per-registry `insecure`, `caCert`, and `auth` |
| `runtime.builtin.network.*` | `mode: public` | Guest network policy: `mode` (`public`/`allowlist`/`none`), `allow` (allowlist names), `hostPorts` (host loopback ports the guest may reach), `nameservers` and `trustHostCas` (global config only) — see [Runtimes: Network](11-runtimes.md#network) |

Every source object, its fields, the registry settings, and the supported `apple-local` (Apple Containers) releases are described in [Runtimes: Image sources](11-runtimes.md#image-sources). There is no default image source: with none configured, `awman ready` explains how to provide one instead of guessing.

The scalar keys can be set with `awman config set`; the object-valued ones (`imageSource`, `images.*`, `registries.*`) show as read-only in `awman config show` and are edited in the JSON file. A setting that fails validation (zero vCPUs or memory, a root disk under 256MiB, a zero mount quota, an empty source field, a `tls` block on a non-`tcp://` Docker host, an unsupported Docker host scheme) is an error that names the offending key.

`runtime.builtin.network` layers global and repo config so that a repo block may only narrow what the global block allows (a stricter `mode`, a subset of the global `allow` list, a subset of the global `hostPorts`) — never widen it, since repo config ships with the checkout and may be untrusted. This effective policy applies to every builtin launch in that repo, including TUI, API, and squad sessions. `nameservers` and `trustHostCas` are read from the global config only. A block that widens the ceiling, mixes `allow`/`hostPorts` with an incompatible `mode`, or names an IP/CIDR/`localhost` in `allow` is a validation error naming the offending key.

---

## Reference: all keys

This table is generated from the same key table awman validates against, so it is always current. **Scope** says which files may hold the key (`global`: the global file only; `repo`: global or repo file; `task`: global, repo or a squad task file). **Live** says whether a running daemon applies a change without a restart. **Env** is the derived environment variable (`<NAME>` stands for a map entry's name, upper-cased, `-` as `_`); `—` means the key is not settable from the environment because it holds an object. Durations are written `500ms`, `30s`, `15m`, `2h`; sizes `512MiB`, `4GiB`.

<!-- key-table:start -->
| Key | Type | Default | Scope | Sensitive | Live | Env | Meaning |
|---|---|---|---|---|---|---|---|
| `agents.default` | string | `null` | task | no | no | `AWMAN_AGENTS_DEFAULT` | Agent used when no --agent flag names one. |
| `agents.stuckTimeout` | duration | `"30s"` | task | no | no | `AWMAN_AGENTS_STUCK_TIMEOUT` | Inactivity after which an agent is reported as stuck. |
| `agents.launchMode` | enum `stdio\|acp` | `"stdio"` | task | no | no | `AWMAN_AGENTS_LAUNCH_MODE` | How an agent process is connected to awman. |
| `agents.acpFallback` | enum `stdio\|error` | `"error"` | repo | no | no | `AWMAN_AGENTS_ACP_FALLBACK` | What to do when ACP is requested for an agent that does not support it. |
| `agents.flags.*` | list<string> | `[]` | task | no | no | `AWMAN_AGENTS_FLAGS_<NAME>` | Extra arguments appended to the named agent's launch command. |
| `agents.yolo.disallowedTools` | list<string> | `[]` | task | no | no | `AWMAN_AGENTS_YOLO_DISALLOWED_TOOLS` | Tools an agent may not use in yolo mode. |
| `agents.auth.mode` | enum `keychain\|passthrough\|none` | `"keychain"` | repo | no | no | `AWMAN_AGENTS_AUTH_MODE` | How host credentials are injected into agent containers. |
| `agents.auth.refresh.enabled` | bool | `true` | repo | no | no | `AWMAN_AGENTS_AUTH_REFRESH_ENABLED` | Refresh agent credentials while an agent runs. |
| `agents.auth.refresh.threshold` | duration | `"20m"` | repo | no | no | `AWMAN_AGENTS_AUTH_REFRESH_THRESHOLD` | Refresh a credential when it expires sooner than this. |
| `agents.auth.refresh.tick` | duration | `"60s"` | repo | no | no | `AWMAN_AGENTS_AUTH_REFRESH_TICK` | How often credential expiry is checked. |
| `agents.programStatus.enabled` | bool | `true` | repo | no | no | `AWMAN_AGENTS_PROGRAM_STATUS_ENABLED` | Collect program status from agents (read from 0141). |
| `runtime.kind` | enum `docker\|apple\|sbx\|builtin` | `"docker"` | global | no | no | `AWMAN_RUNTIME_KIND` | Container runtime agents run in; sbx and builtin are experimental. |
| `runtime.baseImage` | string | `null` | repo | no | no | `AWMAN_RUNTIME_BASE_IMAGE` | Base image tag for setup and teardown containers. |
| `runtime.dockerfile` | string | `"Dockerfile.dev"` | repo | no | no | `AWMAN_RUNTIME_DOCKERFILE` | Dockerfile, relative to the repo root, that agent images are built from. |
| `runtime.builtin.stateDir` | string | `null` | repo | no | no | `AWMAN_RUNTIME_BUILTIN_STATE_DIR` | Absolute private state directory of the builtin runtime (default <AWMAN_HOME>/builtin). |
| `runtime.builtin.vcpus` | int 1..255 | `2` | repo | no | no | `AWMAN_RUNTIME_BUILTIN_VCPUS` | Whole vCPUs per builtin agent VM. |
| `runtime.builtin.memory` | size | `"4GiB"` | repo | no | no | `AWMAN_RUNTIME_BUILTIN_MEMORY` | Guest memory per builtin agent VM (at least 128MiB). |
| `runtime.builtin.rootDisk` | size | `"30GiB"` | repo | no | no | `AWMAN_RUNTIME_BUILTIN_ROOT_DISK` | Writable root disk per builtin agent VM (at least 256MiB). |
| `runtime.builtin.mountQuota` | size | `"30GiB"` | repo | no | no | `AWMAN_RUNTIME_BUILTIN_MOUNT_QUOTA` | How much a builtin guest may add to each host mount (at least 1MiB). |
| `runtime.builtin.builder.enabled` | bool | `false` | repo | no | no | `AWMAN_RUNTIME_BUILTIN_BUILDER_ENABLED` | Let awman ready build missing builtin images locally. |
| `runtime.builtin.imageSource` | object | `null` | repo | no | no | — | Where awman ready acquires builtin images that are not cached. |
| `runtime.builtin.images.*` | object | `{}` | repo | no | no | — | Per-image source overrides keyed by awman image tag. |
| `runtime.builtin.registries.*` | object | `{}` | repo | no | no | — | Registry host settings keyed by host[:port]. |
| `runtime.builtin.network.mode` | enum `none\|allowlist\|public` | `null` | repo | no | no | `AWMAN_RUNTIME_BUILTIN_NETWORK_MODE` | Builtin guest network policy; a repo may only narrow the global one. |
| `runtime.builtin.network.allow` | list<string> | `null` | repo | no | no | `AWMAN_RUNTIME_BUILTIN_NETWORK_ALLOW` | Names a builtin guest may reach in allowlist mode. |
| `runtime.builtin.network.hostPorts` | list<int> | `null` | repo | no | no | `AWMAN_RUNTIME_BUILTIN_NETWORK_HOST_PORTS` | Host ports a builtin guest may reach. |
| `runtime.builtin.network.nameservers` | list<string> | `null` | repo | no | no | `AWMAN_RUNTIME_BUILTIN_NETWORK_NAMESERVERS` | DNS servers a builtin guest uses. |
| `runtime.builtin.network.trustHostCas` | bool | `null` | repo | no | no | `AWMAN_RUNTIME_BUILTIN_NETWORK_TRUST_HOST_CAS` | Install the host's trusted CA certificates in builtin guests. |
| `overlays` (additive) | list<string> | `[]` | task | no | no | `AWMAN_OVERLAYS` | Overlays applied to every agent container; layers add to each other. |
| `workItems.dir` | string | `"aspec/work-items"` | repo | no | no | `AWMAN_WORK_ITEMS_DIR` | Work items directory, relative to the repo root. |
| `workItems.template` | string | `null` | repo | no | no | `AWMAN_WORK_ITEMS_TEMPLATE` | Work item template (default <dir>/0000-template.md). |
| `workflows.maxConcurrentSteps` | int ≥ 1 | `null` | task | no | no | `AWMAN_WORKFLOWS_MAX_CONCURRENT_STEPS` | Cap on concurrently running workflow steps (null = unlimited). |
| `workflows.prerequisites.preflight` | enum `warn\|off` | `"warn"` | task | no | no | `AWMAN_WORKFLOWS_PREREQUISITES_PREFLIGHT` | Whether workflow prerequisites are checked before a run. |
| `workflows.prerequisites.unresolved` | enum `warn\|fail` | `null` | task | no | no | `AWMAN_WORKFLOWS_PREREQUISITES_UNRESOLVED` | What an unresolved prerequisite does (null = frontend default). |
| `workflows.prerequisites.gitIdentity.use` | enum `ask\|allow\|deny` | `"ask"` | task | no | no | `AWMAN_WORKFLOWS_PREREQUISITES_GIT_IDENTITY_USE` | Whether workflows may use a git identity. |
| `workflows.prerequisites.gitIdentity.name` | string | `null` | task | no | no | `AWMAN_WORKFLOWS_PREREQUISITES_GIT_IDENTITY_NAME` | Fixed git author name for workflows. |
| `workflows.prerequisites.gitIdentity.email` | string | `null` | task | no | no | `AWMAN_WORKFLOWS_PREREQUISITES_GIT_IDENTITY_EMAIL` | Fixed git author email for workflows. |
| `workflows.prerequisites.ssh.use` | enum `ask\|allow\|deny` | `"ask"` | task | no | no | `AWMAN_WORKFLOWS_PREREQUISITES_SSH_USE` | Whether workflows may use an SSH key. |
| `workflows.prerequisites.ssh.key` | string | `null` | task | no | no | `AWMAN_WORKFLOWS_PREREQUISITES_SSH_KEY` | SSH key under ~/.ssh offered to workflows. |
| `workflows.prerequisites.githubToken.use` | enum `ask\|allow\|deny` | `"ask"` | task | no | no | `AWMAN_WORKFLOWS_PREREQUISITES_GITHUB_TOKEN_USE` | Whether workflows may use a GitHub token. |
| `workflows.prerequisites.githubToken.envVars` | list<string> | `[]` | task | no | no | `AWMAN_WORKFLOWS_PREREQUISITES_GITHUB_TOKEN_ENV_VARS` | Host variables a GitHub token is read from (empty = GH_TOKEN, GITHUB_TOKEN). |
| `workflows.prerequisites.githubToken.verify` | bool | `true` | task | no | no | `AWMAN_WORKFLOWS_PREREQUISITES_GITHUB_TOKEN_VERIFY` | Verify a GitHub token before a run. |
| `workflows.prerequisites.githubToken.acquire.argv` | list<string> | `null` | global | no | no | `AWMAN_WORKFLOWS_PREREQUISITES_GITHUB_TOKEN_ACQUIRE_ARGV` | Command that prints a GitHub token. |
| `workflows.prerequisites.githubToken.acquire.timeout` | duration | `"30s"` | global | no | no | `AWMAN_WORKFLOWS_PREREQUISITES_GITHUB_TOKEN_ACQUIRE_TIMEOUT` | How long the token command may run (at most 300s). |
| `workflows.prerequisites.githubToken.acquire.refresh` | enum `start\|before_teardown` | `"start"` | global | no | no | `AWMAN_WORKFLOWS_PREREQUISITES_GITHUB_TOKEN_ACQUIRE_REFRESH` | When the token command runs. |
| `workflows.prerequisites.allowApiClientGrants` | bool | `false` | global | no | no | `AWMAN_WORKFLOWS_PREREQUISITES_ALLOW_API_CLIENT_GRANTS` | Let API clients rely on stored prerequisite grants. |
| `workflows.dynamic.defaultLeader` | string | `null` | task | no | yes | `AWMAN_WORKFLOWS_DYNAMIC_DEFAULT_LEADER` | Default dynamic-workflow leader as agent::model. |
| `workflows.dynamic.agentsToModels.*` | list<string> | `[]` | task | no | yes | `AWMAN_WORKFLOWS_DYNAMIC_AGENTS_TO_MODELS_<NAME>` | Models a dynamic-workflow leader may schedule for the named agent. |
| `workflows.dynamic.guidance` (additive) | list<string> | `[]` | task | no | yes | `AWMAN_WORKFLOWS_DYNAMIC_GUIDANCE` | Instructions the dynamic-workflow leader follows; layers add (at most 50). |
| `worktree.onDirtyMergeTarget` | enum `ask\|commit\|stash\|fail` | `"ask"` | repo | no | no | `AWMAN_WORKTREE_ON_DIRTY_MERGE_TARGET` | What to do when the merge target has uncommitted changes. |
| `worktree.mergeFix.onFailure` | enum `ask\|agent\|keep` | `"ask"` | repo | no | no | `AWMAN_WORKTREE_MERGE_FIX_ON_FAILURE` | What to do when merging a worktree branch fails. |
| `worktree.mergeFix.promptTemplate` | string | `null` | repo | no | no | `AWMAN_WORKTREE_MERGE_FIX_PROMPT_TEMPLATE` | Prompt template, relative to the repo root, for the merge-fix agent. |
| `tui.scrollbackLines` | int ≥ 0 | `10000` | repo | no | no | `AWMAN_TUI_SCROLLBACK_LINES` | Scrollback lines kept per container terminal. |
| `tui.backend` | enum `auto\|daemon\|builtin` | `"auto"` | repo | no | no | `AWMAN_TUI_BACKEND` | Where the TUI runs commands (read from 0142). |
| `tui.programStatus.emit` | bool | `true` | repo | no | no | `AWMAN_TUI_PROGRAM_STATUS_EMIT` | Emit program status from the TUI (read from 0141). |
| `tui.worker.enabled` | bool | `false` | repo | no | no | `AWMAN_TUI_WORKER_ENABLED` | Run a pull worker inside the TUI (read from 0143). |
| `tui.integrations.*` | object | `{}` | repo | no | no | — | Per-integration TUI settings (read from 0154). |
| `daemon.maxConcurrentJobs` | int ≥ 1 | `2` | global | no | yes | `AWMAN_DAEMON_MAX_CONCURRENT_JOBS` | Jobs the daemon runs at once. |
| `daemon.verboseSetup` | bool | `false` | global | no | yes | `AWMAN_DAEMON_VERBOSE_SETUP` | Log session setup in detail. |
| `daemon.terminal.cols` | int ≥ 1 | `120` | global | no | yes | `AWMAN_DAEMON_TERMINAL_COLS` | Default daemon terminal width (read from 0141). |
| `daemon.terminal.rows` | int ≥ 1 | `40` | global | no | yes | `AWMAN_DAEMON_TERMINAL_ROWS` | Default daemon terminal height (read from 0141). |
| `daemon.terminal.remoteInput` | bool | `false` | global | no | yes | `AWMAN_DAEMON_TERMINAL_REMOTE_INPUT` | Accept terminal input from network clients (read from 0141). |
| `daemon.network.enabled` | bool | `false` | global | no | no | `AWMAN_DAEMON_NETWORK_ENABLED` | Serve the API on the network listener (read from 0140). |
| `daemon.network.listen` | string | `"127.0.0.1:9876"` | global | no | no | `AWMAN_DAEMON_NETWORK_LISTEN` | host:port the API listener binds; changing it needs a daemon restart. |
| `daemon.network.tlsSans` | list<string> | `[]` | global | no | no | `AWMAN_DAEMON_NETWORK_TLS_SANS` | Extra names in the listener's TLS certificate (read from 0140). |
| `daemon.network.features` | list<string> | `["worker"]` | global | no | no | `AWMAN_DAEMON_NETWORK_FEATURES` | Features served on the network listener (read from 0140). |
| `daemon.features.squad.enabled` | bool | `false` | global | no | yes | `AWMAN_DAEMON_FEATURES_SQUAD_ENABLED` | Run the squad scheduler. |
| `daemon.features.squad.interaction` | enum `interactive\|headless` | `"interactive"` | global | no | yes | `AWMAN_DAEMON_FEATURES_SQUAD_INTERACTION` | How squad evaluations interact. |
| `daemon.features.squad.maxConcurrentJobs` | int ≥ 1 | `1` | global | no | yes | `AWMAN_DAEMON_FEATURES_SQUAD_MAX_CONCURRENT_JOBS` | Squad runs at once. |
| `daemon.features.squad.maxConcurrentEvaluations` | int ≥ 1 | `2` | global | no | yes | `AWMAN_DAEMON_FEATURES_SQUAD_MAX_CONCURRENT_EVALUATIONS` | Squad evaluations at once. |
| `daemon.features.squad.envPersistence` | enum `keychain\|none` | `"keychain"` | global | no | no | `AWMAN_DAEMON_FEATURES_SQUAD_ENV_PERSISTENCE` | Where the squad daemon persists pushed task environment. |
| `daemon.features.worker.enabled` | bool | `false` | global | no | yes | `AWMAN_DAEMON_FEATURES_WORKER_ENABLED` | Run the worker feature. |
| `daemon.features.worker.maxConcurrentJobs` | int ≥ 1 | `2` | global | no | yes | `AWMAN_DAEMON_FEATURES_WORKER_MAX_CONCURRENT_JOBS` | Worker jobs at once. |
| `daemon.features.worker.push.enabled` | bool | `true` | global | no | yes | `AWMAN_DAEMON_FEATURES_WORKER_PUSH_ENABLED` | Accept jobs pushed by API clients. |
| `daemon.features.worker.push.interaction` | enum `interactive\|headless` | `"headless"` | global | no | yes | `AWMAN_DAEMON_FEATURES_WORKER_PUSH_INTERACTION` | Default interaction of pushed jobs. |
| `daemon.features.worker.push.allowOverride` | bool | `true` | global | no | yes | `AWMAN_DAEMON_FEATURES_WORKER_PUSH_ALLOW_OVERRIDE` | Let a pushed job choose its own interaction. |
| `daemon.features.worker.push.workDirs.allow` | list<string> | `[]` | global | no | yes | `AWMAN_DAEMON_FEATURES_WORKER_PUSH_WORK_DIRS_ALLOW` | Directories pushed jobs may run in. |
| `daemon.features.worker.push.workDirs.roots` | list<string> | `[]` | global | no | yes | `AWMAN_DAEMON_FEATURES_WORKER_PUSH_WORK_DIRS_ROOTS` | Directories whose children pushed jobs may run in. |
| `daemon.features.worker.pull.enabled` | bool | `true` | global | no | yes | `AWMAN_DAEMON_FEATURES_WORKER_PULL_ENABLED` | Claim jobs from work servers (read from 0143). |
| `daemon.features.worker.pull.interaction` | enum `interactive\|headless` | `"interactive"` | global | no | yes | `AWMAN_DAEMON_FEATURES_WORKER_PULL_INTERACTION` | Default interaction of pulled jobs (read from 0143). |
| `daemon.features.worker.pull.allowOverride` | bool | `true` | global | no | yes | `AWMAN_DAEMON_FEATURES_WORKER_PULL_ALLOW_OVERRIDE` | Let a pulled job choose its own interaction (read from 0143). |
| `daemon.features.worker.pull.workRoot` | string | `null` | global | no | yes | `AWMAN_DAEMON_FEATURES_WORKER_PULL_WORK_ROOT` | Directory pulled jobs check out into (read from 0143). |
| `daemon.features.engine.enabled` | bool | `false` | global | no | yes | `AWMAN_DAEMON_FEATURES_ENGINE_ENABLED` | Serve TUI engines from the daemon (read from 0142). |
| `daemon.features.engine.allowEphemeral` | bool | `true` | global | no | yes | `AWMAN_DAEMON_FEATURES_ENGINE_ALLOW_EPHEMERAL` | Start the engine feature on demand (read from 0142). |
| `daemon.features.engine.idleTimeout` | duration | `"15m"` | global | no | yes | `AWMAN_DAEMON_FEATURES_ENGINE_IDLE_TIMEOUT` | Idle time after which an on-demand engine stops (read from 0142). |
| `daemon.features.engine.maxConcurrentJobs` | int ≥ 1 | `4` | global | no | yes | `AWMAN_DAEMON_FEATURES_ENGINE_MAX_CONCURRENT_JOBS` | Engine jobs at once (read from 0142). |
| `workServers.*.url` | string | `null` | global | no | yes | `AWMAN_WORK_SERVERS_<NAME>_URL` | Work server URL; required for every entry (read from 0143). |
| `workServers.*.enabled` | bool | `true` | global | no | yes | `AWMAN_WORK_SERVERS_<NAME>_ENABLED` | Claim work from this server (read from 0143). |
| `workServers.*.claim` | enum `auto\|manual` | `"auto"` | global | no | yes | `AWMAN_WORK_SERVERS_<NAME>_CLAIM` | Whether work is claimed automatically (read from 0143). |
| `workServers.*.maxConcurrentJobs` | int ≥ 1 | `1` | global | no | yes | `AWMAN_WORK_SERVERS_<NAME>_MAX_CONCURRENT_JOBS` | Jobs from this server at once (read from 0143). |
| `workServers.*.interaction` | enum `interactive\|headless` | `null` | global | no | yes | `AWMAN_WORK_SERVERS_<NAME>_INTERACTION` | Interaction override for this server's jobs (read from 0143). |
| `workServers.*.labels` | map<string,string> | `{}` | global | no | yes | `AWMAN_WORK_SERVERS_<NAME>_LABELS` | Labels this worker advertises to the server (read from 0143). |
| `remote.default` | string | `null` | global | no | no | `AWMAN_REMOTE_DEFAULT` | Target used when --remote names none. |
| `remote.targets.*.url` | string | `null` | global | no | no | `AWMAN_REMOTE_TARGETS_<NAME>_URL` | Target API URL (https://host:port); required for every entry. |
| `remote.targets.*.apiKey` | string | `null` | global | yes | no | `AWMAN_REMOTE_TARGETS_<NAME>_API_KEY` | Bearer API key for the target. |
<!-- key-table:end -->

---

## Upgrading from earlier betas

Earlier betas used a different, flatter set of key names, flags and variables. They are no longer accepted: a file that still has an old key fails to load with an error that names the new key, and an old flag is an unknown-flag error. Rename each entry once — nothing is migrated for you, and `awman config migrate` no longer exists.

### Config keys

| Old key | New key |
|---|---|
| `default_agent`, `agent` | `agents.default` |
| `agentStuckTimeout` (seconds) | `agents.stuckTimeout` (duration, e.g. `"30s"`) |
| `launchMode` | `agents.launchMode` |
| `launchModeFallback` | `agents.acpFallback` |
| `agentFlags` | `agents.flags` |
| `yoloDisallowedTools` | `agents.yolo.disallowedTools` |
| `auth` | `agents.auth.mode` |
| `authRefresh.enabled` | `agents.auth.refresh.enabled` |
| `authRefresh.thresholdMinutes` (number) | `agents.auth.refresh.threshold` (`"20m"`) |
| `authRefresh.tickSeconds` (number) | `agents.auth.refresh.tick` (`"60s"`) |
| `runtime` (`docker`, `apple-containers`, `docker-sbx-experimental`, `builtin-experimental`) | `runtime.kind` (`docker`, `apple`, `sbx`, `builtin`) |
| `baseImage` | `runtime.baseImage` |
| `dockerfile` | `runtime.dockerfile` |
| `builtin.*` | `runtime.builtin.*` |
| `builtin.memoryMib` (number) | `runtime.builtin.memory` (`"4GiB"`) |
| `builtin.rootDiskMib` (number) | `runtime.builtin.rootDisk` (`"30GiB"`) |
| `builtin.mountQuotaMib` (number) | `runtime.builtin.mountQuota` (`"30GiB"`) |
| `work_items.dir`, `work_items.template` | `workItems.dir`, `workItems.template` |
| `maxConcurrentAgents` | `workflows.maxConcurrentSteps` |
| `workflows.dynamic.maxConcurrentSteps` | `workflows.maxConcurrentSteps` (one key now serves both) |
| `dynamicWorkflows` | `workflows.dynamic` |
| `workflows.prerequisites.githubToken.acquire.timeoutSecs` | `workflows.prerequisites.githubToken.acquire.timeout` (`"30s"`); `refresh` value `beforeTeardown` is now `before_teardown` |
| `terminal_scrollback_lines` | `tui.scrollbackLines` |
| `workers` | `daemon.maxConcurrentJobs` |
| `api.workDirs` | `daemon.features.worker.push.workDirs.allow` |
| `api.workDirRoots` | `daemon.features.worker.push.workDirs.roots` |
| `api.alwaysNonInteractive` | `daemon.features.worker.push.interaction: "headless"` with `allowOverride: false` |
| `api.port` | `daemon.network.listen` (`"127.0.0.1:9876"`) |
| `squad.maxConcurrentEvaluations`, `squad.envPersistence` | `daemon.features.squad.maxConcurrentEvaluations`, `daemon.features.squad.envPersistence` |
| `squad.agentsToModels`, `squad.defaultLeader`, `squad.guidance` | `workflows.dynamic.agentsToModels`, `workflows.dynamic.defaultLeader`, `workflows.dynamic.guidance` |
| `squad.workflows.prerequisites` | `workflows.prerequisites` (resolved through the normal global → repo → task chain) |
| `remote.defaultAddr` + `remote.defaultAPIKey` | `remote.targets.<name>.url` + `remote.targets.<name>.apiKey`, and `remote.default = <name>` |
| `remote.savedDirs` | removed |
| `auto_agent_auth_accepted` | not config; see [state.json](#statejson) |
| `envPassthrough` | `env(VAR)` entries in `overlays` |

### Flags

| Old flag | Replacement |
|---|---|
| `--non-interactive`, `-n` | `--headless`, `-n` |
| `--max-concurrent <N>` | `--set workflows.maxConcurrentSteps=N` |
| `--launch-mode <mode>` | `--set agents.launchMode=<mode>` |
| `--remote-addr`, `--api-key` | `--remote <target>`, with the target defined under `remote.targets.<name>` |
| `api start --port`, `--workdirs`, `--workdir-root` | `daemon.network.listen`, `daemon.features.worker.push.workDirs.allow`, `….roots` (or `--set`) |
| `awman config migrate` | none — rename keys using the table above |
| `awman config prerequisite-grants list\|revoke` | `awman daemon api grants.list`, `grants.delete id=…` |

### Environment variables

| Old variable | Replacement |
|---|---|
| `AWMAN_CONFIG_HOME`, `AWMAN_API_ROOT`, `AWMAN_SQUAD_ROOT`, `AWMAN_ATTACH_DIR` | `AWMAN_HOME` (one directory for everything) |
| `AWMAN_MAX_CONCURRENT_AGENTS` | `AWMAN_WORKFLOWS_MAX_CONCURRENT_STEPS` |
| `AWMAN_LAUNCH_MODE` | `AWMAN_AGENTS_LAUNCH_MODE` |
| `AWMAN_REMOTE_ADDR`, `AWMAN_API_KEY` | `AWMAN_REMOTE_TARGETS_<NAME>_URL`, `AWMAN_REMOTE_TARGETS_<NAME>_API_KEY` |
| `AWMAN_BUILTIN_STATE_DIR` | `AWMAN_RUNTIME_BUILTIN_STATE_DIR` |
| `AWMAN_API_VERBOSE_SETUP` | `AWMAN_DAEMON_VERBOSE_SETUP` (default is now `false`; set `true` for per-session setup logging at `info`) |
| `AWMAN_OVERLAYS` | unchanged (now one of the derived variables) |

The old HTTP routes and response fields are covered in the [API upgrade table](09-api-and-remote-mode.md#upgrading-from-earlier-betas).

---

[← Dynamic Workflows](06-dynamic-workflows.md) · [Next: Overlays →](08-overlays.md)
