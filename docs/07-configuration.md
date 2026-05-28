# Configuration

awman uses two JSON config files: a per-repository config and a global config. Most settings can live in either; the per-repo config takes precedence.

You can view and edit configuration from the terminal using the `awman config` subcommand — no need to manually edit JSON files.

---

## Per-repository config

**Path:** `GITROOT/.awman/config.json`

This file is created by `awman init` and should be committed to source control. It configures awman for a specific project.

```json
{
  "agent": "claude",
  "terminal_scrollback_lines": 10000,
  "yoloDisallowedTools": ["Bash", "computer"],
  "envPassthrough": ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"],
  "overlays": {
    "skills": true,
    "directories": [
      { "host": "/data/fixtures", "container": "/mnt/fixtures", "permission": "ro" }
    ]
  },
  "workItems": {
    "dir": "docs/work-items",
    "template": "docs/work-items/0000-template.md"
  }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `agent` | string | `"claude"` | Agent to use for this repository: `claude`, `codex`, `opencode`, `maki`, `gemini`, `copilot`, `crush`, or `cline` |
| `terminal_scrollback_lines` | integer | `10000` | Number of scrollback lines in the container terminal emulator. Overrides the global value |
| `base_image` | string | (from global or `make build` default) | Container image tag for workflow setup/teardown phases. Overrides the global value if set. Only override if you have a repo-specific custom base image. See [Workflows — Setup and Teardown](04-workflows.md#setup-and-teardown-phases) |
| `yoloDisallowedTools` | string array | `[]` | Tools the agent cannot use when `--yolo` is active. Overrides the global list entirely |
| `envPassthrough` | string array | `[]` | Host environment variable names to inject into agent containers at launch. Overrides the global list entirely. See [`envPassthrough`](#envpassthrough) |
| `overlays.skills` | boolean | false | When `true`, mount your global awman skills directory (`~/.awman/skills/`) into the agent container as its native slash commands. **Additive** with the global config; either scope setting it to `true` enables the mount. See [`overlays`](#overlays) |
| `overlays.directories` | object array | `[]` | Host directories to mount into agent containers automatically. **Additive** with the global list — entries from both scopes are merged, not replaced. See [`overlays`](#overlays) |
| `workItems.dir` | string | (not set) | Path to the work items directory, relative to repo root. See [Work item paths](#work-item-paths) |
| `workItems.template` | string | (not set) | Path to the work item template file, relative to repo root. See [Work item paths](#work-item-paths) |

---

## Global config

**Path:** `$HOME/.awman/config.json`

Applies to all projects on the machine unless overridden by a per-repo config.

```json
{
  "default_agent": "claude",
  "terminal_scrollback_lines": 10000,
  "runtime": "docker",
  "yoloDisallowedTools": ["Bash"],
  "envPassthrough": ["ANTHROPIC_API_KEY"],
  "overlays": {
    "skills": true,
    "directories": [
      { "host": "~/personal-prompts", "container": "/mnt/prompts", "permission": "ro" }
    ]
  },
  "api": {
    "workDirs": ["/home/user/my-project"],
    "alwaysNonInteractive": false,
    "workers": 2
  },
  "remote": {
    "defaultAddr": "http://build-server.example.com:9876",
    "defaultAPIKey": "a3f8b2c1...64-char-hex...",
    "savedDirs": ["/home/user/my-project"]
  }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `default_agent` | string | `"claude"` | Default agent when no per-repo agent is configured: `claude`, `codex`, `opencode`, `maki`, `gemini`, `copilot`, `crush`, or `cline` |
| `terminal_scrollback_lines` | integer | `10000` | Default scrollback lines for all repos unless overridden |
| `runtime` | string | `"docker"` | Container runtime: `"docker"` or `"apple-containers"` (macOS 26+ only) |
| `base_image` | string | (from `make build`) | Default container image tag for workflow setup/teardown phases. Overridden by per-repo `base_image` if set. Only override if you have a custom base image. See [Workflows — Setup and Teardown](04-workflows.md#setup-and-teardown-phases) |
| `yoloDisallowedTools` | string array | `[]` | Global fallback list of tools forbidden when `--yolo` is active |
| `envPassthrough` | string array | `[]` | Host environment variable names to inject into agent containers at launch. See [`envPassthrough`](#envpassthrough) |
| `overlays.skills` | boolean | false | When `true`, mount your global awman skills directory (`~/.awman/skills/`) into every agent container as its native slash commands. **Additive** — enable it here, per-repo config, `AWMAN_OVERLAYS`, or `--overlay` flags. See [`overlays`](#overlays) |
| `overlays.directories` | object array | `[]` | Host directories to mount into agent containers automatically across all projects. **Additive** with per-repo overlays — both lists are merged. See [`overlays`](#overlays) |
| `api.workDirs` | string array | `[]` | Working directories pre-approved for API mode session creation. Merged with `--workdirs` flags at server startup. See [API Mode](09-api-mode.md#working-directory-allowlist) |
| `api.alwaysNonInteractive` | boolean | `false` | When `true`, all dispatched commands automatically run in non-interactive mode. Useful for API servers where no TTY is available. See [API Mode](09-api-mode.md#alwaysnoninteractive) |
| `api.workers` | integer | `2` | Number of worker tasks that process the command queue in parallel. Each worker claims one queued command at a time and executes it. Higher values allow more concurrent command execution across sessions (one command per session). See [API Mode: Job Queue](09-api-mode.md#job-queue) |
| `remote.defaultAddr` | string | (not set) | Default address of the remote API awman server (e.g. `http://host:9876`). Overridden by `--remote-addr` or `AWMAN_REMOTE_ADDR`. See [Remote Mode](10-remote-mode.md#connecting-to-a-remote-host) |
| `remote.defaultAPIKey` | string | (not set) | API key sent with every request to `remote.defaultAddr`. **Only sent when the target address exactly matches `remote.defaultAddr`** — never forwarded to other hosts. Overridden by `--api-key` or `AWMAN_API_KEY`. See [Remote Mode](10-remote-mode.md#api-key-authentication) |
| `remote.savedDirs` | string array | `[]` | Absolute paths (on the remote host) shown in the TUI saved-dir picker for `remote session start`. See [Remote Mode](10-remote-mode.md#configuration) |

**Note:** `runtime` is a global (machine-level) setting only. It is not available in the per-repo config — container runtime is a property of the machine, not the project.

---

## Config precedence

| Field | Precedence |
|-------|-----------|
| `agent` / `default_agent` | Per-repo → Global → Built-in default (`claude`) |
| `terminal_scrollback_lines` | Per-repo → Global → Built-in default (10,000) |
| `base_image` | Per-repo → Global → Built-in default (from `make build`) |
| `yoloDisallowedTools` | Per-repo → Global → Empty list (no restriction) |
| `envPassthrough` | Per-repo → Global → Empty list (no passthrough) |
| `overlays.skills` | **Additive OR**: if true in global, per-repo, `AWMAN_OVERLAYS`, or `--overlay` flags, the mount is enabled |
| `overlays.directories` | **Additive**: global + per-repo + `AWMAN_OVERLAYS` env var + `--overlay` flags, merged with conflict resolution |
| `runtime` | Global only |
| `workItems.dir` / `workItems.template` | Per-repo only |
| `api.workDirs` | Global only (merged with `--workdirs` flags at startup) |
| `api.alwaysNonInteractive` | Global only |
| `api.workers` | Global only |
| `remote.defaultAddr` | Global only (overridden per-invocation by `--remote-addr` or `AWMAN_REMOTE_ADDR`) |
| `remote.defaultAPIKey` | Global only (overridden per-invocation by `--api-key` or `AWMAN_API_KEY`; only sent to `remote.defaultAddr`) |
| `remote.savedDirs` | Global only |

For `yoloDisallowedTools` and `envPassthrough`, if a per-repo list is set it **replaces** the global list entirely — lists are not merged. To inherit the global list for a repo, omit the field from the repo config.

`overlays.skills` and `overlays.directories` behave differently: all sources are **additive**. Entries from global config, per-repo config, the `AWMAN_OVERLAYS` env var, and `--overlay` CLI flags are all merged into the final configuration. For `overlays.skills`, if any source sets it to `true`, the skills mount is enabled — there is no hierarchy; it's an OR operation. For `overlays.directories`, entries with different host paths are kept independent; when the same host path appears in multiple sources, conflict resolution applies (higher-priority source wins for the container path; more restrictive permission always wins). See [`overlays`](#overlays) for the full resolution rules.

A 10,000-line scrollback buffer at 80 columns uses approximately 3 MB per tab. Increase for long-running build or test sessions; decrease when running many simultaneous tabs.

---

## Managing config from the terminal

The `awman config` subcommand lets you view and edit configuration without opening any JSON files. It understands both scopes, shows built-in defaults for unset fields, and warns you when one scope is silently overriding another.

### `awman config show`

Displays every configuration field — even fields not set in either file — as a table showing the global value, repo value, effective (applied) value, and whether the repo is overriding the global:

```
Field                            Global              Repo              Effective          Override
───────────────────────────────  ──────────────────  ────────────────  ─────────────────  ────────
default_agent                    claude (built-in)   N/A               claude             —
runtime                          docker (built-in)   N/A               docker             —
terminal_scrollback_lines        10000 (built-in)    5000              5000               yes
yolo_disallowed_tools            (empty)             (not set)         (empty)            —
env_passthrough                  HOME, PATH          (not set)         HOME, PATH         —
overlays.skills                  true                (not set)         true (enabled)     —
overlays.directories             1 entry             1 entry           2 entries (merged) —
agent                            N/A                 codex             codex              yes
auto_agent_auth_accepted         N/A                 true (read-only)  true               —
work_items.dir                   N/A                 docs/work-items   docs/work-items    —
work_items.template              N/A                 (not set)         (not set)          —
api.workDirs                (not set)           N/A               (not set)          —
api.alwaysNonInteractive    false (built-in)    N/A               false              —
remote.defaultAddr               (not set)           N/A               (not set)          —
remote.defaultAPIKey             (not set)           N/A               (not set)          —
remote.savedDirs                 (empty)             N/A               (empty)            —
```

Column meanings:

| Column | Meaning |
|--------|---------|
| **Global** | Value from `~/.awman/config.json`, with `(built-in)` suffix when not set in the file. `N/A` for repo-only fields |
| **Repo** | Value from `.awman/config.json`, or `(not set)` when absent. `N/A` for global-only fields |
| **Effective** | The value awman actually uses, after applying precedence rules |
| **Override** | `yes` when the repo value is set and differs from the global value; `—` otherwise |

When run outside a git repository, `config show` succeeds and shows global fields only, with a note that repo config is unavailable.

### `awman config get <field>`

Shows the global, repo, and effective values for a single field, with an explicit note about which scope wins:

```sh
awman config get terminal_scrollback_lines
```

```
Field: terminal_scrollback_lines
  Global:     10000 (built-in default)
  Repo:       5000
  Effective:  5000  ← repo overrides global
```

When neither scope has the field set, the built-in default is shown for both Global and Effective, and Repo is marked `(not set)`.

Passing an unknown field name prints a helpful error listing all valid names:

```
error: Unknown config field 'scrollback'. Valid fields: default_agent, runtime, terminal_scrollback_lines, yolo_disallowed_tools, env_passthrough, overlays.skills, overlays.directories, agent, auto_agent_auth_accepted, api.workDirs, api.alwaysNonInteractive, remote.defaultAddr, remote.defaultAPIKey, remote.savedDirs
```

### `awman config set [--global] <field> <value>`

Writes a config value at the repo level (default) or global level (`--global`):

```sh
# Set agent for this repo
awman config set agent codex

# Set default agent globally
awman config set --global default_agent gemini

# Set scrollback lines globally
awman config set --global terminal_scrollback_lines 20000

# Set disallowed tools for this repo
awman config set yolo_disallowed_tools "Bash,computer"

# Clear disallowed tools for this repo (empty string sets an empty list)
awman config set yolo_disallowed_tools ""

# Set work items directory for this repo
awman config set work_items.dir docs/work-items

# Set work item template for this repo
awman config set work_items.template docs/work-items/0000-template.md

# Enable skills overlay for this repo
awman config set overlays.skills true

# Enable skills overlay globally
awman config set --global overlays.skills true

# Configure API working directories globally
awman config set --global api.workDirs "/home/user/my-project,/home/user/other-project"

# Enable always-non-interactive mode for API server use
awman config set --global api.alwaysNonInteractive true

# Set the default remote API server address
awman config set --global remote.defaultAddr http://build-server.example.com:9876

# Store the API key for the default remote server
awman config set --global remote.defaultAPIKey <your-api-key>

# Configure saved directories for the remote session start picker
awman config set --global remote.savedDirs "/home/user/my-project,/home/user/other-project"
```

After writing, `config set` prints a confirmation showing the new effective value:

```
Set agent = codex (repo config)
Effective: codex
```

**Scope enforcement**: each field has a natural scope. Writing across scopes produces an error:

```
error: 'runtime' is a global-only field. Use --global to set it.

error: 'agent' is a repo-only field. Cannot be set with --global.

error: 'work_items.dir' is a repo-only field. Cannot be set with --global.

error: 'api.workDirs' is a global-only field. Use --global to set it.

error: 'remote.defaultAddr' is a global-only field. Use --global to set it.

error: 'remote.defaultAPIKey' is a global-only field. Use --global to set it.

error: 'remote.savedDirs' is a global-only field. Use --global to set it.
```

**Override warnings**: if the value you're setting will be silently shadowed, `config set` warns you:

```
Warning: repo config overrides this field; the new global value will not take effect in this repo.

Note: repo value matches global; no override is active.
```

**Clearing list fields**: passing an empty string (`""`) for `yolo_disallowed_tools` or `env_passthrough` sets the field to an empty list — it does not remove the field from the config. This matters because an empty repo list actively overrides a non-empty global list. To stop overriding the global list, omit the field from the repo config entirely (edit the file directly).

**Read-only field**: `auto_agent_auth_accepted` is managed by the agent auth flow and cannot be set via `awman config set`. Attempting it exits with:

```
error: 'auto_agent_auth_accepted' is managed by the agent auth flow and cannot be set via 'awman config set'.
```

**Platform note**: setting `runtime = apple-containers` on Linux or Windows emits a warning that this value is unsupported on the current platform and will fall back to `docker` at runtime, but the value is still written.

**Missing config files**: `config show` and `config get` never error on missing files — absent files are treated as all-unset. `config set` creates the file and its parent directory (`<git-root>/.awman/` or `$HOME/.awman/`) as needed.

---

## Work item paths

By default, awman looks for work items in `aspec/work-items/` and uses `aspec/work-items/0000-template.md` as the template. If your repo doesn't use the `aspec/` directory structure, you can configure custom paths via `work_items.dir` and `work_items.template`.

### Configuring a custom work items directory

```sh
awman config set work_items.dir docs/work-items
```

Once set, work items are loaded from that directory instead of `aspec/work-items/`. The path may be relative to the repo root (recommended) or absolute.

### Configuring a custom template

```sh
awman config set work_items.template docs/work-items/0000-template.md
```

When set, this file is used as the template for new work items created with `new spec`. If the path is set but the file doesn't exist, awman warns and falls back to auto-discovery.

### Template auto-discovery

If no template is configured (and no legacy `aspec/work-items/0000-template.md` exists), the work item creation command scans the work items directory for any file whose name ends in `template.md`. If it finds a candidate, it prompts:

```
Found potential template: docs/work-items/my-template.md. Use it? [Y/n]
```

If you confirm, the path is saved to repo config automatically so you won't be prompted again. If multiple `*template.md` files exist, awman uses the lexicographically first one and notes how many were found so you can pick manually if needed.

If no template is found and you decline, the new work item is created with a minimal stub:

```markdown
# Feature: My New Feature
```

### Path resolution order

| Step | Directory | Template |
|------|-----------|----------|
| 1 | `work_items.dir` in repo config | `work_items.template` in repo config |
| 2 | `aspec/work-items/` (legacy fallback, if it exists) | `aspec/work-items/0000-template.md` (legacy fallback, if it exists) |
| 3 | Error: run `awman config set work_items.dir <path>` | Auto-discovery (`*template.md` in work items dir), then minimal stub |

### Graceful degradation

If neither `work_items.dir` is configured nor `aspec/work-items/` exists, work item creation fails with a helpful message:

```
Work items directory not configured.
Run `awman config set work_items.dir <path>` to configure one,
or run `awman init --aspec` to set up the aspec folder.
```

`awman ready` shows a warning (not a failure) in this state:

```
│  work items config │ ⚠ not configured                │
```

and prints a tip: `run 'awman config set work_items.dir <path>' to configure a work items directory.`

### Security

Paths are validated to stay within the git root. Paths that would escape the repo (e.g. `../../outside`) are rejected:

```
error: path escapes the repository root
```

### Setting up during `awman init`

When running `awman init` without `--aspec` in a repo that has no `aspec/` folder, awman offers to configure a work items directory interactively:

```
Would you like to configure a work items directory? [y/N]
Work items directory path (relative to repo root): docs/work-items
Work item template path (leave blank to skip): docs/work-items/my-template.md
Work items directory configured: docs/work-items
```

If `work_items.dir` is already configured, the prompt is skipped silently. The result is shown in the init summary table:

```
│    Work items │ ✓ configured                 │
```

---

## `envPassthrough`

`envPassthrough` is an allowlist of host environment variable names that awman reads from your current shell and injects into agent containers at launch time. It applies to all agents — not just maki — but is the primary way to authenticate agents that use API keys rather than a system keychain.

### Why an allowlist?

awman deliberately cannot forward your entire host environment into a container. You must name each variable explicitly. This preserves the security principle that containers receive only the minimum secrets they need.

### Configuration

Add the field to your global config to apply it to all projects:

```json
{
  "envPassthrough": ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]
}
```

Or add it to a per-repo config to apply it to one project only:

```json
{
  "agent": "maki",
  "envPassthrough": ["ANTHROPIC_API_KEY", "ZHIPU_API_KEY"]
}
```

When a variable is listed but not present in your shell environment, it is silently skipped — no error or warning is produced. This is intentional: you may list variables that are only set in some contexts (e.g. CI vs. local).

### Using maki with `envPassthrough`

Maki authenticates exclusively via API keys. There is no system keychain integration. A typical maki setup looks like:

**Global config** (`~/.awman/config.json`):
```json
{
  "envPassthrough": ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]
}
```

**Per-repo config** (`.awman/config.json`):
```json
{
  "agent": "maki"
}
```

With this setup, `awman chat` reads `ANTHROPIC_API_KEY` and `OPENAI_API_KEY` from your shell and passes them into the maki container as `-e` flags on the `docker run` invocation. The values are masked (`***`) in all displayed Docker commands.

### Using gemini with `envPassthrough`

Gemini supports API-key-based authentication via `envPassthrough`. A typical setup for users with a Google AI Studio key:

**Global config** (`~/.awman/config.json`):
```json
{
  "envPassthrough": ["GEMINI_API_KEY"]
}
```

**Per-repo config** (`.awman/config.json`):
```json
{
  "agent": "gemini"
}
```

For Vertex AI, include the relevant variables:

```json
{
  "envPassthrough": ["GOOGLE_API_KEY", "GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_LOCATION", "GOOGLE_GENAI_USE_VERTEXAI"]
}
```

In addition to `envPassthrough`, awman automatically copies `~/.gemini/` (your OAuth token directory) into a temporary directory and mounts it at `/root/.gemini` inside the container. This means that if you've already authenticated gemini on the host (`gemini auth login`), the container picks up your session automatically with no extra config. See [Gemini authentication](02-agent-sessions.md#gemini-authentication) for the full auth details.

### Using copilot with `envPassthrough`

GitHub Copilot CLI authenticates via a GitHub token. There is no config directory to mount — auth is entirely token-based.

**Global config** (`~/.awman/config.json`):
```json
{
  "envPassthrough": ["COPILOT_GITHUB_TOKEN"]
}
```

**Per-repo config** (`.awman/config.json`):
```json
{
  "agent": "copilot"
}
```

`COPILOT_GITHUB_TOKEN` takes highest precedence. Alternatively, `GH_TOKEN` (standard GitHub CLI token) or `GITHUB_TOKEN` (fallback) are also accepted by copilot. For GitHub Enterprise users, add `COPILOT_GH_HOST`:

```json
{
  "envPassthrough": ["COPILOT_GITHUB_TOKEN", "COPILOT_GH_HOST"]
}
```

The token must have the "Copilot Requests" fine-grained PAT permission, or be a standard GitHub OAuth token from `gh auth token`.

### Using crush with `envPassthrough`

Crush authenticates entirely via provider API keys. Add whichever key(s) match your chosen model provider:

**Global config** (`~/.awman/config.json`):
```json
{
  "envPassthrough": ["ANTHROPIC_API_KEY"]
}
```

**Per-repo config** (`.awman/config.json`):
```json
{
  "agent": "crush"
}
```

Multiple providers can be listed simultaneously — crush selects the appropriate key based on the model chosen:

```json
{
  "envPassthrough": ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GEMINI_API_KEY"]
}
```

See [Crush authentication](02-agent-sessions.md#crush-authentication) for the full list of supported provider variables.

### Using cline with `envPassthrough`

Cline stores API keys in `~/.cline/data/secrets.json` (written by `cline auth`). awman automatically copies and mounts this directory into the container — no `envPassthrough` configuration is needed.

**Per-repo config** (`.awman/config.json`):
```json
{
  "agent": "cline"
}
```

Set up credentials on the host before running awman sessions:

```sh
cline auth -p anthropic -k <your-api-key> -m claude-sonnet-4-6
```

awman copies `~/.cline/data/` (excluding task history and workspace state) into a temporary directory and mounts it at `/home/awman/.cline/data` inside the container. If the directory does not exist on the host, an empty directory is mounted and cline will prompt for authentication on first use. See [Cline authentication](02-agent-sessions.md#cline-authentication) for full details.

### Precedence and deduplication

Per-repo config wins entirely over global config — lists are not merged. To use the global list for a specific repo, omit `envPassthrough` from the repo config.

If a variable name appears in both `envPassthrough` and the agent's keychain credentials (e.g. a user who configured `CLAUDE_CODE_OAUTH_TOKEN` in both places), the keychain value takes precedence and the passthrough entry for that name is skipped.

---

## Overlays

Overlays let you mount host directories, environment variables, and skills into agent containers. All overlay sources — global config, per-repo config, `AWMAN_OVERLAYS`, `--overlay` flags, and per-step workflow overlays — are **additive**, giving you precise control over what each step can access.

For comprehensive documentation on overlay syntax, configuration, merge semantics, and common use cases, see [Overlays](08-overlays.md).

---

## Runtime selection

awman supports two container runtimes. Switching runtimes requires no changes to your `Dockerfile.dev`, workflow files, or any other project config.

| Runtime | Value | Platform | Requirement |
|---------|-------|----------|-------------|
| Docker | `"docker"` | macOS, Linux, Windows | Docker daemon running |
| Apple Containers | `"apple-containers"` | macOS 26+ only | `container` CLI in PATH |

Set the runtime in your global config:

```json
{ "runtime": "apple-containers" }
```

An unrecognised value (e.g. a typo) falls back to `"docker"` with a warning — your workflow is not broken, but you should fix the value.

### Verifying runtime

`awman ready` validates the configured runtime before any other checks and prints which is active:

```
Runtime: docker (daemon running)
```

If the runtime is unavailable, `ready` exits immediately with a clear message:

```
error: runtime 'apple-containers' is not available: 'container' not found in PATH.
Install Apple Containers (macOS 26+) or set "runtime": "docker" in your config.
```

### Apple Containers runtime

Apple Containers (`container` CLI, macOS 26+) is an OCI-compatible container runtime. It supports Dockerfiles natively and awman maps every operation to the equivalent `container` CLI invocation. The user experience is identical to the Docker runtime.

**Limitations:**

- **`--allow-docker`**: Docker socket passthrough is not meaningful under Apple Containers. Passing `--allow-docker` produces a warning and the socket is not mounted. If your task needs Docker-in-container, switch to the Docker runtime.
- **macOS only**: If `"apple-containers"` is configured on Linux or Windows, awman exits with an error at startup rather than silently falling back to Docker.

---

## Build & development

```sh
make all                      # cargo build --release
make install                  # build + install to /usr/local/bin/ (may need sudo)
make test                     # cargo test
make clean                    # cargo clean
make release VERSION=v1.0.0   # create and publish a release
```

### Releasing

`make release VERSION=vx.y.z` automates the full release process:

1. Switches to `main`, pulls latest, and verifies a clean working tree
2. Creates `docs/releases/vx.y.z.md` with a release notes template
3. Launches `awman chat` to prompt an agent to write release notes
4. Runs all tests locally
5. Commits the release notes and tags the commit with the version
6. Pushes the commit and tag to `main`
7. Creates a GitHub Release with the release notes via `gh`

The tag push triggers the release CI pipeline, which builds binaries for all platforms and uploads them to the GitHub Release.

---

[← Headless Mode](06-headless-mode.md) · [Next: Overlays →](08-overlays.md)
