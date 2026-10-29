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
  "agentFlags": {
    "codex": ["--config", "model_reasoning_effort=high"]
  },
  "workItems": {
    "dir": "docs/work-items",
    "template": "docs/work-items/0000-template.md"
  },
  "workflows": {
    "dynamic": {
      "agentsToModels": {
        "claude": ["claude-opus-4-8", "claude-sonnet-4-6"]
      },
      "maxConcurrentSteps": 3,
      "defaultLeader": "claude::claude-opus-4-8",
      "guidance": ["Never spawn more than two agents in parallel."]
    }
  },
  "worktree": {
    "onDirtyMergeTarget": "ask",
    "mergeFix": {
      "onFailure": "agent",
      "promptTemplate": "prompts/merge-fix.md"
    }
  }
}
```

`worktree` controls the merge-back of an isolated workflow worktree. It can be
set in repo config and global config; repo values take precedence. Command-line
flags override both. With no configured value, the frontend's headless policy
applies (for example, API workflows use `fail` for a dirty merge target and
`keep` after a merge failure). These policies apply only at final merge-back.
The API's existing preparation policy still automatically commits a checkout
that is dirty before worktree creation; `fail` does not change that initial
decision.

| JSON key | Type | Values / default | Meaning |
|----------|------|------------------|---------|
| `worktree.onDirtyMergeTarget` | string | `ask`, `commit`, `stash`, `fail`; unset | Action when the merge target has tracked or untracked changes before merge-back. |
| `worktree.mergeFix.onFailure` | string | `ask`, `agent`, `keep`; unset | Action when Git cannot merge the worktree branch. |
| `worktree.mergeFix.promptTemplate` | string | Path; unset | Prompt template path relative to the Git root. Replaces the built-in merge-fix prompt. |

The prompt template supports `{{merge_command}}`, `{{merge_exit_code}}`,
`{{merge_output}}`, `{{branch}}`, `{{target_branch}}`, and
`{{unmerged_paths}}`. The last placeholder contains one path per line, or
`(none)` when Git reports no unmerged paths. Work-item placeholders such as
`{{work_item_number}}` and `{{work_item_content}}` are also substituted when
the workflow has work-item context.

Custom templates must be regular files within the Git root. Absolute paths,
parent traversal, and symlink components are refused. Secure custom-template
reads currently require Unix; on other platforms the built-in prompt is available, while
configured custom templates are refused. Inserted diagnostic values are substituted
once; custom templates should clearly label them as untrusted data. The built-in
prompt uses fences that cannot be closed by the diagnostic contents.

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
  "agentFlags": {
    "codex": ["--config", "model_reasoning_effort=high"],
    "claude": ["--verbose"]
  },
  "workers": 2,
  "api": {
    "workDirs": ["/home/user/my-project"],
    "workDirRoots": ["/home/user/projects"],
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
  "workflows": {
    "dynamic": {
      "defaultLeader": "claude::claude-opus-4-8",
      "guidance": ["Keep workflows focused and include a review step."]
    }
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

## Workflow prerequisites

Workflow prerequisite policy lives under the `workflows.prerequisites` object in the global and repository config files. It controls checks for commit identity, SSH pushes, and GitHub tokens used by workflow teardown steps. Unknown keys are ignored with a warning, so use these exact names and casing:

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
          "timeoutSecs": 30,
          "refresh": "beforeTeardown"
        },
        "verify": true
      },
      "allowApiClientGrants": false
    }
  }
}
```

All fields are optional. `preflight` is `warn` (the default) or `off`; `off` skips probes and does not grant credentials, and repository config cannot turn checks off. `unresolved` is `warn` or `fail`; repository config cannot relax a global `fail`. `fail` stops when a requirement remains missing or needs consent. Each credential's `use` is `ask`, `allow`, or `deny`. `gitIdentity.name` and `email` set a fixed identity; otherwise a granted host identity is used only for that workflow run. An SSH `key` is a suggested key path, not permission to mount it. SSH key use still requires an interactive choice, an explicit `--prerequisite-ssh` flag, or an allowed global policy.

`githubToken.envVars` lists host environment variables awman may offer; the defaults are `GH_TOKEN` and `GITHUB_TOKEN`. A repo may only narrow that list to names already allowed globally. `verify` enables best-effort token checking and a repository cannot turn global verification off. `acquire.argv` is an argument array, not a shell command string. It runs on the host when selected by an interactive user or allowed by policy. A repo-supplied acquisition command requires approval for its exact argument list and resolved executable; changing the command requires approval again. `timeoutSecs` defaults to 30 (maximum 300). `refresh` is `start` (default) or `beforeTeardown`; the latter lets an already-granted acquisition command refresh a token before teardown.

Repository config cannot grant access to host credentials. It can ask or deny, narrow globally allowed settings, provide a fixed Git identity, suggest an SSH key, and define an acquisition command that requires per-command approval. Put broad grants in `~/.awman/config.json` or choose them when prompted. `allowApiClientGrants` is global-only; when false, API callers cannot use prerequisite flags to grant credentials beyond the configured policy.

Remembered SSH and token choices are stored as decisions in `~/.awman/prerequisite-grants.json`; the file does not contain credentials. A remembered choice applies only to its repository, remote, and credential type. Review or remove entries with `awman config prerequisite-grants list` and `awman config prerequisite-grants revoke <index|all>`.

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

- **List fields replace, they don't merge.** A repo `yoloDisallowedTools` list completely replaces the global one — even an empty list. To inherit the global list, omit the field from the repo config. `agentFlags` follows the same rule per agent: a repo `agentFlags.codex` list replaces the global `agentFlags.codex` list, `--agent-flag` replaces both, and agents a level doesn't mention fall through to the next one.
- **Overlays are additive.** `overlays` entries from global config, repo config, `AWMAN_OVERLAYS`, and `--overlay` flags are all merged. See [Overlays](08-overlays.md).

---

## Managing config from the terminal

```sh
awman config show                          # full table: global, repo, and effective values
awman config get <field>                   # one field, all scopes
awman config set <field> <value>           # write to repo config
awman config set --global <field> <value>  # write to global config
awman config migrate                       # move legacy dynamic-workflow settings
```

- `config show` and `config get` never fail on missing files; absent files are treated as all-unset. `config set` creates the file and its parent directory as needed.
- Each field has a natural scope. Setting a global-only field without `--global` (or a repo-only field with it) is an error that tells you which flag to use.
- Unknown field names get a did-you-mean suggestion list.
- `remote.defaultAPIKey` is masked in `config show`/`config get` output.
- Setting the removed `envPassthrough` field errors with guidance to use `env(VAR)` overlay entries instead.

`awman config migrate` moves a repo's old dynamic-workflow block to
`workflows.dynamic`. It shows the before/after JSON and asks before writing
(in the TUI the preview scrolls with **↑ / ↓ / PgUp / PgDn**; press **y** to move
it, **n** or **Esc** to cancel). Use `--dry-run` to preview the changes, or
`--yes` to apply without confirmation. It keeps the order of every other key in the file,
and refuses a `.awman/` directory or `config.json` that is a symlink. Saving a
repo config (`config set`, the TUI config dialog, `awman init`) also moves the
old repo block automatically and says so. `--global` changes nothing: the
former global `dynamicWorkflows` key was never read, so `awman config migrate
--global` only reports whether it is present. Copy any settings you want from
it into the global `workflows.dynamic` block yourself; awman keeps the old
block in the file until you delete it. Older awman versions do not read
`workflows.dynamic`; teammates on older versions will stop seeing migrated
settings once the config change is committed.

An invalid `workflows.dynamic` block in global config (for example
`maxConcurrentSteps: 0`) is a load error naming `~/.awman/config.json` and the
field. Like any other invalid global setting, it stops every command, including
`awman config show` and `awman config set --global`, so fix it by editing the
file by hand. `config set` refuses an edit that would make either file invalid,
so this only happens after a hand edit.

The full list of accepted field names and their scopes is in the [Reference](#reference).

---

## Common recipes

### Set the default agent

```sh
awman config set --global default_agent gemini   # for all projects
awman config set agent codex                     # for this repo only
```

Valid agents: `claude`, `codex`, `gemini`, `opencode`, `cline`, `copilot`, `antigravity`, or the name of a [custom agent](15-custom-agents.md). A repo-scope write accepts custom agents defined in the repo or globally; a `--global` write accepts only globally defined ones. Anything else is rejected at write time with the list of valid agents.

### Custom agent definitions and approvals

`agent` and `default_agent` accept the names of [custom agents](15-custom-agents.md). Definitions are TOML files:

| Path | Scope |
|------|-------|
| `<git_root>/.awman/agents/<name>.toml` | This repo (wins over a global definition of the same name) |
| `~/.awman/agents/<name>.toml` | Every repo |

Your approvals of custom definitions are recorded in `~/.awman/trusted-agents.json`. Each entry is tied to the repo and the content hash of the definition, so editing a definition asks you to approve it again. The global paths follow your global files if you [relocate them](#where-global-files-live).

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
awman config set --global runtime builtin-experimental     # experimental; Apple Silicon only
```

See [Runtimes](#runtimes) below. The `builtin-experimental` runtime also has its own `builtin` block of settings (VM size, image sources); see [Builtin runtime settings](#builtin-runtime-settings).

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
awman config set workflows.dynamic.defaultLeader claude::claude-opus-4-8
awman config set workflows.dynamic.maxConcurrentSteps 3
```

`workflows.dynamic.agentsToModels` is set one agent at a time —
`awman config set workflows.dynamic.agentsToModels.<agentName> "model-a, model-b"`
(an empty value removes the mapping). Add `--global` to write to global config,
or use the TUI config dialog's Global or Repo column; **Ctrl+N** adds a mapping
to the selected scope.

`workflows.dynamic.guidance` is a list of instructions the leader agent must
follow whenever it designs a workflow. It's set one entry at a time, addressed
by index — `awman config set workflows.dynamic.guidance.0 "Never spawn more than
two agents in parallel."` (an empty value removes that entry and re-indexes the
rest). Use `--global` or select the Global column in the TUI to edit global
guidance; **Ctrl+N** appends to the selected scope.

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

### Pass extra flags to an agent

To pass an agent a flag awman doesn't add on its own (for example, one a new agent release needs before awman knows about it), add it under `agentFlags`, keyed by agent name, in either config file:

```json
{
  "agentFlags": {
    "codex": ["--config", "model_reasoning_effort=high"],
    "claude": []
  }
}
```

awman appends each agent's list to the command it launches, after the flags awman adds itself (mode, model, tools, system prompt) and before the initial prompt. With `--yolo`, the printed command for an interactive codex session looks like:

```
codex --no-daemon --dangerously-bypass-approvals-and-sandbox --config model_reasoning_effort=high "<prompt>"
```

The list applies to every launch of that agent, interactive and non-interactive (`codex exec`, `claude --print`). So pick flags both modes accept, or set them per run with `--agent-flag`. awman already runs interactive Codex with `--no-daemon`, so don't add it here: awman drops it from interactive launches rather than pass it twice, warns at launch, and `codex exec` rejects it.

- **One entry per argument.** A flag that takes a value is two entries (`["--config", "key=value"]`) or one entry in its `=` form (`["--config=key=value"]`). Entries are passed exactly as written; there is no shell parsing.
- **Repo replaces global, per agent.** In the example, the repo's `"claude": []` turns off a global `claude` entry for this repo; agents the repo doesn't list keep their global entries.
- **For one run, use `--agent-flag`.** `awman chat --agent-flag=--search` replaces the configured list for the agent that command launches. See [`--agent-flag`](03-agent-sessions.md#--agent-flag-arg).
- **Workflows:** every step gets the entry for the agent that step runs, including steps a dynamic workflow's leader generates, and the leader itself. Runs in a worktree (`--worktree`, `--yolo`, `--auto`, `--dynamic`) read `agentFlags` and the repo's default `agent` from the repository you launched from, so an edit you haven't committed still applies.
- **Remote runs:** `remote exec prompt` and `remote exec workflow` accept `--agent-flag` but don't forward it; awman warns, and the remote host uses its own `agentFlags`.
- **Docker Sandboxes runtime:** only agents with an argv channel (`kind: agent` kits) receive the flags; for the others awman prints a warning and launches without them.

`agentFlags` is edited in the config file; it is not settable via `config set`.

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
| `builtin-experimental` | macOS on Apple Silicon only (experimental) | MicroVM per session embedded in the awman binary; needs no Docker, `container`, or `sbx` to run agents. Included in the `awman-macos-arm64` release, which is ad-hoc signed (see [Gatekeeper and permissions](11-runtimes.md#the-macos-binary-gatekeeper-and-permissions)). Must be set explicitly in the global config. `awman ready` imports images from a configured source instead of building them. See [Runtimes](11-runtimes.md#builtin-microvm-experimental) |

An unrecognized value (e.g. a typo) is a fatal error — awman never falls back to a different isolation model than the one you configured. CLI commands print the invalid value and the list of valid values, then exit; the TUI shows the same message in a startup modal (Enter quits). Fix the value in `$HOME/.awman/config.json` and relaunch.

`awman ready` validates the configured runtime before any other check and reports which one is active. For full details on platform support, setup, credential registration, and the persistent-sandbox lifecycle see [Runtimes](11-runtimes.md).

### Builtin runtime settings

The optional `builtin` object is read only when `runtime` is `builtin-experimental` (the settings object keeps the name `builtin`). It can appear in both the global and the repo config; repo values win per field, and the `images` and `registries` maps merge per key. Unknown keys are rejected. `network`, `vcpus`, `memoryMib`, `rootDiskMib`, `mountQuotaMib`, `registries`, the image sources and `builder.enabled` are resolved for each session from that session's own repo, so the TUI, the API and squad each honour the repo a session runs in, even when several repos share one awman process. A workflow worktree keeps the `builtin` block of the checkout you launched from, so a narrowing you haven't committed still applies. An invalid repo block (for example one that widens the global `network`) fails that repo's launches only; it doesn't stop a TUI or API server started in that repo, and other runtimes never read it.

`builtin.stateDir` is the exception: the state directory is process-wide. If a repo's `builtin.stateDir` resolves to a different directory than the one the awman process uses, the launch is refused with an error naming both paths (`this repository's builtin.stateDir resolves to <repo path>, but this awman process uses the builtin state directory <process path>`). Remove the repo's `stateDir` or set it to the process's path. `AWMAN_BUILTIN_STATE_DIR` outranks every config layer, so it never causes this mismatch.

```json
{
  "runtime": "builtin-experimental",
  "builtin": {
    "vcpus": 4,
    "memoryMib": 8192,
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
    "network": {
      "mode": "public",
      "hostPorts": [8765]
    }
  }
}
```

To build with Apple Containers instead of Docker, use an `apple-local` source:

```json
{
  "runtime": "builtin-experimental",
  "builtin": {
    "builder": { "enabled": true },
    "imageSource": { "type": "apple-local" }
  }
}
```

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `builtin.stateDir` | absolute path | `<data home>/builtin` (`~/.awman/builtin`) | Private state root for imported images, catalog, control sockets and VM/session state; mode `0700`, no symlinks, keep it short. Process-wide: one state directory serves every builtin session in the awman process, so a repo value that resolves to a different directory is rejected (see below). `AWMAN_BUILTIN_STATE_DIR` overrides it |
| `builtin.vcpus` | integer 1–255 | `2` | Whole vCPUs per session VM |
| `builtin.memoryMib` | integer ≥ 128 | `4096` | Guest memory in MiB |
| `builtin.rootDiskMib` | integer ≥ 256 | `30720` | Writable root disk in MiB (everything outside the mounts); sparse, so it only uses host space as the agent writes |
| `builtin.mountQuotaMib` | integer ≥ 1 | `30720` | MiB the agent may add to each host mount, the workspace included, beyond what it already held |
| `builtin.imageSource` | object | unset | Default image source: an object whose `type` is `registry`, `docker-local`, `archive`, or `apple-local` (macOS) |
| `builtin.builder.enabled` | boolean | `false` | Opt in to local source-matched Docker or Apple Containers builds during `awman ready`, and when a `chat` or `exec prompt` launch finds its agent image missing, under `builtin-experimental`; see [Building from a local image source](11-runtimes.md#building-from-a-local-image-source). Repository value overrides global. |
| `builtin.images` | object (image tag → source) | `{}` | Per-image source; wins over `imageSource` for that tag |
| `builtin.registries` | object (`host[:port]` → settings) | `{}` | Per-registry `insecure`, `caCert`, and `auth` |
| `builtin.network` | object | `{"mode":"public"}` | Guest network policy: `mode` (`public`/`allowlist`/`none`), `allow` (allowlist names), `hostPorts` (host loopback ports the guest may reach), `nameservers` and `trustHostCas` (global config only) — see [Runtimes: Network](11-runtimes.md#network) |

Every source object, its fields, the registry settings, and the supported `apple-local` (Apple Containers) releases are described in [Runtimes: Image sources](11-runtimes.md#image-sources). There is no default image source: with none configured, `awman ready` explains how to provide one instead of guessing.

`builtin.vcpus`, `builtin.memoryMib`, `builtin.rootDiskMib`, `builtin.mountQuotaMib`, and `builtin.stateDir` can be set with `awman config set`. The four object-valued keys appear read-only in `awman config show` and must be edited in the JSON file. A `builtin` block that fails validation (zero vCPUs or memory, a root disk under 256 MiB, a zero mount quota, an empty source field, a `tls` block on a non-`tcp://` Docker host, an unsupported Docker host scheme) is an error that names the offending key.

`builtin.network` layers global and repo config the same way: a repo block may only narrow what the global block allows (a stricter `mode`, a subset of the global `allow` list, a subset of the global `hostPorts`) — never widen it, since repo config ships with the checkout and may be untrusted. This effective policy applies to every builtin launch in that repo, including TUI, API, and squad sessions. `nameservers` and `trustHostCas` are read from the global config only. A block that widens the ceiling, mixes `allow`/`hostPorts` with an incompatible `mode`, or names an IP/CIDR/`localhost` in `allow` is a validation error naming the offending key.

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
| `worktree.onDirtyMergeTarget` | string | (unset → frontend default) | `ask`, `commit`, `stash`, or `fail` when the merge target has changes — see [Worktree merge-back](05-workflows.md#worktree-merge-back) | yes |
| `worktree.mergeFix.onFailure` | string | (unset → frontend default) | `ask`, `agent`, or `keep` after a worktree merge fails — see [Worktree merge-back](05-workflows.md#worktree-merge-back) | yes |
| `worktree.mergeFix.promptTemplate` | string | (unset → built-in prompt) | Merge-fix agent prompt template path relative to the Git root | yes |
| `dockerfile` | string | `Dockerfile.dev` | Path to the project base Dockerfile, relative to repo root or absolute | no (edit file) |
| `workflows.dynamic.agentsToModels` | object (agent → string array) | (unset → Dockerfile discovery) | Repo map replaces global map; global agents without a Dockerfile in this repo are skipped — see [Dynamic Workflows](06-dynamic-workflows.md#global-and-repo-settings) | yes, per agent as `workflows.dynamic.agentsToModels.<agentName>` (comma-separated; empty value removes) |
| `workflows.dynamic.maxConcurrentSteps` | integer | (unset → unlimited) | Advisory cap on concurrent workflow steps passed to the leader prompt | yes, as `workflows.dynamic.maxConcurrentSteps` |
| `workflows.dynamic.defaultLeader` | string (`agent::model`) | (unset) | Default leader agent/model for `exec workflow --dynamic`; overridden by `--leader` | yes, as `workflows.dynamic.defaultLeader` |
| `workflows.dynamic.guidance` | string array | (unset → no guidance block) | Global and repo instructions are combined in order, with duplicates dropped — see [Dynamic Workflows](06-dynamic-workflows.md#global-and-repo-settings) | yes, per entry as `workflows.dynamic.guidance.<index>` (empty value removes) |
| `builtin` | object | (unset) | Repo overrides for the builtin runtime block, merged over the global one per field — see [Builtin runtime settings](#builtin-runtime-settings) | scalars only (`builtin.stateDir`, `builtin.vcpus`, `builtin.memoryMib`, `builtin.rootDiskMib`, `builtin.mountQuotaMib`) |
| `auth` | `"keychain"` \| `"passthrough"` \| `"none"` | `"keychain"` | Credential injection mode — see [Control credential injection](#control-credential-injection-auth-mode) | no (edit file) |
| `authRefresh` | object | (unset) | Live credential-refresh settings for Claude — see [Control credential refresh](#control-credential-refresh-authrefresh) | no (edit file) |
| `agentFlags` | object (agent → string array) | (unset) | Extra arguments appended to that agent's command; each entry replaces the global entry for the same agent — see [Pass extra flags to an agent](#pass-extra-flags-to-an-agent) | no (edit file) |

### Global config fields (`$HOME/.awman/config.json`)

| JSON key | Type | Default | Meaning | Settable via `config set --global` |
|----------|------|---------|---------|-------------------------------------|
| `default_agent` | string | (unset) | Agent used when no repo agent is configured | yes |
| `terminal_scrollback_lines` | integer | 10000 | Default scrollback for all repos | yes |
| `runtime` | string | `docker` | Container runtime: `docker`, `apple-containers`, `docker-sbx-experimental`, `builtin-experimental` (Apple Silicon only) | yes |
| `builtin` | object | (unset) | Builtin microVM runtime settings; see [Builtin runtime settings](#builtin-runtime-settings) | scalars only (`builtin.stateDir`, `builtin.vcpus`, `builtin.memoryMib`, `builtin.rootDiskMib`, `builtin.mountQuotaMib`); edit the rest in the file |
| `yoloDisallowedTools` | string array | `[]` | Machine-wide yolo tool denylist (unless a repo overrides it) | yes |
| `overlays` | string array | `[]` | Overlay specs applied to every project; additive with other sources | yes |
| `agentStuckTimeout` | integer (seconds) | 30 | Default agent-stuck timeout | yes |
| `maxConcurrentAgents` | integer | (unset → unlimited) | Machine-wide default cap on concurrently-running workflow steps (unless a repo overrides it) — see [Parallel workflows](05-workflows.md#parallel-workflows) | yes |
| `workers` | integer | 2 | API server worker tasks processing the command queue in parallel — see [API mode](09-api-and-remote-mode.md) | no (edit file) |
| `baseImage` | string | (unset) | Default image tag for workflow setup/teardown containers | no (edit file) |
| `worktree.onDirtyMergeTarget` | string | (unset → frontend default) | Default dirty merge-target action: `ask`, `commit`, `stash`, or `fail` | yes |
| `worktree.mergeFix.onFailure` | string | (unset → frontend default) | Default merge-failure action: `ask`, `agent`, or `keep` | yes |
| `worktree.mergeFix.promptTemplate` | string | (unset → built-in prompt) | Default merge-fix prompt template path relative to the Git root | yes |
| `api.workDirs` | string array | `[]` | Directories pre-approved for API session creation; merged with `--workdirs` at server start | yes |
| `api.workDirRoots` | string array | `[]` | Root directories; any git repository cloned directly beneath a root may be used as a local session workdir. Merged with `--workdir-root` at server start. See [Working directory allowlist](09-api-and-remote-mode.md#working-directory-allowlist) | yes |
| `api.alwaysNonInteractive` | bool | `false` | Force non-interactive mode for all dispatched commands (useful on API servers with no TTY) | no (edit file) |
| `remote.defaultAddr` | string | (unset) | Default remote awman API server address | yes |
| `remote.defaultAPIKey` | string | (unset) | API key for the default remote server; only sent when the target address matches `remote.defaultAddr` | yes |
| `remote.savedDirs` | string array | `[]` | Remote-host paths shown in the `remote session start` picker — see [Remote mode](09-api-and-remote-mode.md) | no (edit file) |
| `squad` | object | (unset) | Global squad daemon settings; see [Squad daemon configuration](#squad-daemon-configuration) | no (edit file) |
| `launchModeFallback` | `"stdio"` \| `"error"` | `"error"` | What to do when a requested ACP launch uses an agent without ACP support | no (edit file) |
| `authRefresh` | object | (unset) | Machine-wide default live credential-refresh settings for Claude (unless a repo overrides a field) — see [Control credential refresh](#control-credential-refresh-authrefresh) | no (edit file) |
| `agentFlags` | object (agent → string array) | (unset) | Machine-wide extra arguments appended to that agent's command (unless a repo sets an entry for the same agent) — see [Pass extra flags to an agent](#pass-extra-flags-to-an-agent) | no (edit file) |
| `workflows.dynamic.agentsToModels` | object (agent → string array) | (unset → Dockerfile discovery) | Machine-wide agent/model map; entries without a Dockerfile in a repo are skipped — see [Dynamic Workflows](06-dynamic-workflows.md#global-and-repo-settings) | yes, per agent as `workflows.dynamic.agentsToModels.<agentName>` |
| `workflows.dynamic.maxConcurrentSteps` | integer | (unset → unlimited) | Advisory cap passed to the dynamic workflow leader | yes, as `workflows.dynamic.maxConcurrentSteps` |
| `workflows.dynamic.defaultLeader` | string (`agent::model`) | (unset) | Default leader agent/model, unless a repo or `--leader` overrides it | yes, as `workflows.dynamic.defaultLeader` |
| `workflows.dynamic.guidance` | string array | (unset → no guidance block) | Machine-wide leader instructions, combined with repo guidance — see [Dynamic Workflows](06-dynamic-workflows.md#global-and-repo-settings) | yes, per entry as `workflows.dynamic.guidance.<index>` |

### `awman config` subcommands

| Command | Effect |
|---------|--------|
| `awman config show` | Table of every known field: global, repo, and effective values |
| `awman config get <field>` | Global, repo, and effective value of one field |
| `awman config set <field> <value>` | Write a field to repo config |
| `awman config set --global <field> <value>` | Write a field to global config |
| `awman config migrate [--global] [--dry-run] [--yes]` | Move legacy repo dynamic-workflow settings to `workflows.dynamic`; `--global` only reports the never-read global key |

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
| `builtin.stateDir` | repo or global |
| `builtin.vcpus` | repo or global |
| `builtin.memoryMib` | repo or global |
| `builtin.rootDiskMib` | repo or global |
| `builtin.mountQuotaMib` | repo or global |
| `builtin.imageSource`, `builtin.images`, `builtin.registries`, `builtin.network` | repo or global; shown read-only, edit the JSON |
| `default_agent` | global only |
| `api` | global only |
| `remote` | global only in practice (see note) |
| `work_items.dir` | repo only |
| `work_items.template` | repo only |
| `api.workDirs` | global only |
| `api.workDirRoots` | global only |
| `api.port` | global only (default 9876) |
| `api.background` | global only |
| `remote.defaultAddr` | global only in practice (see note) |
| `remote.defaultAPIKey` | global only in practice (see note) |
| `workflows.dynamic.defaultLeader` | repo or global |
| `workflows.dynamic.maxConcurrentSteps` | repo or global |
| `workflows.dynamic.agentsToModels` (and `.<agentName>`) | repo or global |
| `workflows.dynamic.guidance` (and `.<index>`) | repo or global |

> **Note on `remote.*` scope.** `config set` accepts these at repo scope, but only the **global** file is ever read back — always pass `--global` when setting them.

`launchMode` and `launchModeFallback` are currently config-file-only fields;
they are not accepted by `config set` or `config get`. Edit the JSON files
shown above directly.

Value handling:

- `yoloDisallowedTools`, `overlays`, `api.workDirs`, `api.workDirRoots` — comma-separated values are stored as arrays; an empty string stores an empty array.
- `terminal_scrollback_lines`, `agentStuckTimeout`, `api.port`, `maxConcurrentAgents`, `workflows.dynamic.maxConcurrentSteps` — must be positive integers; `0` is rejected.
- `agent`, `default_agent` — validated against the built-in agents and the custom agents defined in the scope being written.
- `workflows.dynamic.defaultLeader` — must be in `agent::model` format (exactly two non-empty, non-whitespace components).
- `workflows.dynamic.guidance.<index>` — a single free-form instruction string, capped at 1,000 characters; the combined global and repo list is limited to 50 entries. Empty or whitespace-only values are rejected on load and coerced to a removal when set via `config set`.
- `envPassthrough` — removed; the error message points you to `env(VAR)` overlay entries.

### Environment variables

| Variable | Purpose |
|----------|---------|
| `AWMAN_CONFIG_HOME` | Relocate the entire global home (config + data); overrides XDG variables |
| `XDG_CONFIG_HOME` | Global config goes to `$XDG_CONFIG_HOME/awman/` |
| `XDG_DATA_HOME` | Global data (workflows, skills, worktrees, API state, and the shared database) goes to `$XDG_DATA_HOME/awman/` |
| `AWMAN_API_ROOT` | Relocate only the API server storage root |
| `AWMAN_BUILTIN_STATE_DIR` | Relocate the builtin runtime's private state directory; beats `builtin.stateDir` |
| `MSB_PATH`, `MSB_LIBKRUNFW_PATH`, `MSB_AGENTD_PATH`, `MSB_HOME`, `MSB_BACKEND`, `MSB_CONFIG_PATH`, `MSB_PROFILE`, `MSB_CACHE_DIR`, `MSB_SANDBOXES_DIR`, `MSB_VOLUMES_DIR`, `MSB_SNAPSHOTS_DIR`, `MSB_LOGS_DIR`, `MSB_SECRETS_DIR` | Refused when set while starting the builtin runtime; unset the variable before launching builtin sessions |
| `DOCKER_HOST`, `DOCKER_TLS_VERIFY`, `DOCKER_CERT_PATH` | Read only when importing a builtin image from a `docker-local` source that sets no `host` |
| `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY` | Honored when the builtin runtime pulls from a `registry` source |
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
| `AWMAN_TEST_DOCKER` / `AWMAN_TEST_APPLE_CONTAINER` / `AWMAN_TEST_SBX` | Only matter when `AWMAN_TEST_ISOLATION` is set, where `docker`, Apple's `container` and `sbx` are treated as not installed. Set one to `1` to let awman use that real CLI again. `make test-full` sets `AWMAN_TEST_DOCKER`. `AWMAN_TEST_BUILTIN` does the same for the builtin runtime, which is otherwise disabled under isolation; `make test-builtin` sets it. |

When the API or squad daemon is started as a background service (`systemd --user` on Linux, `launchd` on macOS), the launching process forwards a small, non-secret allowlist of variables into that service's environment, so the daemon resolves the same config and storage root as the process that started it: `PATH`, `HOME`, `RUST_LOG`, `AWMAN_CONFIG_HOME`, `AWMAN_API_ROOT`, `AWMAN_SQUAD_ROOT`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `AWMAN_OVERLAYS`, `AWMAN_MAX_CONCURRENT_AGENTS` and `AWMAN_LAUNCH_MODE`. Bearer keys (`AWMAN_API_KEY`, `AWMAN_SQUAD_KEY`) are never forwarded this way — the daemon authenticates against the key hash it reads from its storage root instead.

---

[← Dynamic Workflows](06-dynamic-workflows.md) · [Next: Overlays →](08-overlays.md)

### Workflow prerequisite field reference

| Field (`workflows.prerequisites.` prefix) | Default | `config set` scope |
|---|---|---|
| `preflight` | `warn` | Global or repo; repo cannot set `off` |
| `unresolved` | Headless static: `fail`; interactive, dynamic, squad: `warn` | Global or repo; repo may only set `fail` |
| `gitIdentity.use`, `.name`, `.email` | `ask`, unset, unset | Global or repo; a fixed repo identity is non-secret |
| `ssh.use`, `.key` | `ask`, unset | Global or repo; repo key is a suggestion and repo cannot grant `allow` |
| `githubToken.use`, `.envVars`, `.verify` | `ask`, `["GH_TOKEN","GITHUB_TOKEN"]`, `true` | Global or repo; repo may only narrow |
| `githubToken.acquire` | unset | GlobalOnly in `config set`; repo JSON commands require exact-executable approval and cannot replace a global command |
| `allowApiClientGrants` | `false` | GlobalOnly |

Global `squad.workflows.prerequisites` uses the same shape for daemon-wide
defaults. Sparse task decisions override only the credentials explicitly chosen;
`--prerequisite-token acquire` retains the trusted global acquisition command.
Task-directory `config.json` cannot supply this trusted workflow policy.
Remembered decisions use atomic per-key updates under a private cross-process
lock. Revocation cannot be undone by a stale writer remembering another key.
