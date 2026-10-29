# Custom Agents

awman ships with seven built-in agents: `claude`, `codex`, `opencode`, `gemini`, `copilot`, `cline` and `antigravity`. A **custom agent** lets you add any other command-line coding agent without waiting for awman to support it. You describe the agent in a small TOML file — how to install it, how to launch it, which flags it understands — and awman treats it like any other agent: `awman chat --agent pi`, `awman config set agent pi`, a `agent = "pi"` workflow step, and so on.

Custom agents still run only inside containers. Nothing about a definition lets the agent run on your host (the one optional exception, the `[ping]` host check, is covered in [The optional host check](#the-optional-host-check-ping) and is always shown to you before you approve it).

---

## Where definitions live

| Scope | Location | Applies to |
|-------|----------|------------|
| Repo | `<repo>/.awman/agents/<name>.toml` | This repository only |
| Global | `~/.awman/agents/<name>.toml` | Every repository |

The file name (without `.toml`) is the agent name. Names must match `^[a-z0-9][a-z0-9_-]{0,63}$` and cannot be one of the built-in agent names. If the same name exists in both places, the **repo definition wins**.

Referenced Dockerfiles (see [Escape-hatch Dockerfile](#escape-hatch-dockerfile)) live next to the TOML file.

---

## Creating a definition

### `awman new agent`

```sh
awman new agent <name> [--global] [--from crush|maki] [--force]
```

| Flag | Meaning |
|------|---------|
| *(none)* | Write a commented skeleton to `.awman/agents/<name>.toml` in the current repo |
| `--global` | Write to `~/.awman/agents/<name>.toml` instead |
| `--from crush` / `--from maki` | Copy a shipped example definition instead of the skeleton (see [Migrating from crush and maki](#migrating-from-crush-and-maki)). `--from maki` also writes `maki.Dockerfile` next to the TOML |
| `--force` | Overwrite an existing definition (and, with `--from maki`, its Dockerfile) |

`new agent` needs no container runtime. If the target already exists you get `<path> already exists; pass --force to overwrite it` (exit 1). An invalid name, or the name of a built-in agent, exits with status 2.

On success it prints where the file was written and what to do next:

```
Created agent definition (repo): /path/to/repo/.awman/agents/pi.toml
Fill in display_name, [run] and [image], then run: awman ready --agent pi
```

The skeleton is not valid until you fill it in, so it is not pre-approved; the first `awman ready --agent <name>` shows it for review. When you create an example that declares `[ping]` with `--from`, the output also repeats the host command so you can see it up front.

### Letting an agent write it for you

awman ships a builtin skill, `create-custom-agent`, that walks an agent through researching a CLI and writing a valid definition. Pull the builtin skill library once, then mount the skill into a session:

```sh
awman new skill --pull builtin
awman chat --overlay "skill(awman/create-custom-agent)"
```

Then ask the agent, for example: *"Add support for the pi coding agent to awman."* The agent writes `.awman/agents/<name>.toml` (and a Dockerfile if one is needed) inside the container; it cannot run `awman` or touch your `~/.awman`. When it finishes, review the file, then run `awman ready --agent <name>` on your host to approve it and build the image. See [Overlays](08-overlays.md#skillname) for how `skill(awman/...)` resolves.

---

## Definition reference

A definition is a TOML file with `schema = 1`. Unknown keys are rejected, so a typo is an error rather than silently ignored. A top-level `[sandbox]` table is reserved and rejected (custom agents do not support Docker Sandboxes yet).

### Top-level keys

| Key | Type | Required | Meaning |
|-----|------|:--------:|---------|
| `schema` | integer | yes | Must be `1`. A higher value fails with `schema N requires a newer awman (this build supports 1)` |
| `display_name` | string | yes | Name shown in `ready`, status and the TUI |
| `description` | string | no | Short description |
| `homepage` | string | no | Project URL shown in the approval review |
| `[run]` | table | yes | How the agent is launched |
| `[modes]` | table | no | Flags for `--yolo`, `--auto` and `--plan` |
| `[tools]` | table | no | Flags for allowed / disallowed tool lists |
| `[system_prompt]` | table | no | How awman delivers system prompt text (default `delivery = "none"`) |
| `[acp]` | table | no | Presence enables ACP launch mode |
| `[ping]` | table | no | Optional host check run by `awman ready` |
| `[settings]` | table | no | Host settings directory and skills directory |
| `[auth]` | table | no | Host environment variables to pass through |
| `[image]` | table | yes | How to build the agent image |

### `[run]`

| Key | Type | Meaning |
|-----|------|---------|
| `command` | string array | **Required.** The interactive entrypoint, as argv. `command[0]` is a bare binary name or an absolute container path. No shell is involved |
| `non_interactive` | string | Token appended for `-n` / `exec` / workflow steps (for example `"-p"` or `"run"`). Omit it if the agent has no non-interactive mode |
| `output_format` | string | Structured output protocol for non-interactive API runs: `text` (default), `claude_stream_json` or `codex_json` |
| `interactive_only_flags` | string array | Flags appended only to interactive launches |
| `seed_prompt` | string or table | How the initial prompt is passed: `"positional"` (default) or `{ flag = "--prompt" }` |
| `model` | string | `"space"` (default, `--model NAME`), `"equals"` (`--model=NAME`) or `"unsupported"` |
| `env` | table | Literal environment variables always set in the container |

### `[modes]` and `[tools]`

`[modes]` takes optional arrays of argv tokens: `yolo`, `auto` and `plan`. A missing `yolo` or `auto` is a silent no-op; a missing or empty `plan` makes `--plan` fail as unsupported for this agent.

`[tools]` takes optional `allowed_flag` and `disallowed_flag`. If a launch carries a tool list for an agent that declares no matching flag, awman refuses to launch rather than silently dropping the restriction.

### `[system_prompt]`

| `delivery` | Required keys | Behaviour |
|------------|---------------|-----------|
| `none` (default) | — | No system prompt is delivered |
| `append-file` | `flag` | Appends the prompt to the agent's own prompt, from a file passed with `flag` |
| `append-inline` | `flag`, `key` | Appends the prompt through `flag`, as the config value named by `key` |
| `replace` | `flag` | Replaces the agent's default prompt, using a file passed with `flag` |
| `agents-md` | — | Delivers the prompt through an `AGENTS.md` file |
| `env-file` | `env` | Writes the prompt to a file and sets the environment variable `env` to its path |
| `add-dir` | `flag` | Passes the prompt's directory to the agent with `flag` |

Each delivery accepts only its own keys. Agents whose definition uses `delivery = "none"` still get their [context overlays](08-overlays.md#context-overlays-in-depth) mounted, but are not told about them automatically.

### `[acp]`

```toml
[acp]
command = ["pi", "--acp"]
```

The table's presence enables [ACP launch mode](03-agent-sessions.md#acp-launch-mode); `command` is the argv used for ACP launches.

### `[ping]`

See [The optional host check](#the-optional-host-check-ping).

### `[settings]`

| Key | Meaning |
|-----|---------|
| `host_dir` | Directory relative to your host `$HOME` (for example `".config/pi"`), mounted read-write at the same relative path inside the container when it exists |
| `skills_dir` | Directory relative to the container home where `skill()` overlays are mounted. If omitted, skill overlays are skipped for this agent |

`host_dir` is checked against a deny-list of sensitive directories — see [Security](#security-and-approval).

### `[auth]`

```toml
[auth]
env = ["PI_API_KEY"]
```

Names of host environment variables to pass through to the container. Variables that are set on the host are passed (masked as `***` in displayed commands); unset ones are skipped. This replaces the `env(VAR)` overlays you would otherwise need for the same variables. Squad tasks treat them the same way: when a task's agent (or an agent in its pool) is a usable custom definition, the squad daemon asks your terminal for these names exactly as it does for a task's `env(VAR)` overlays, and drops a held value once no task needs it. An unapproved definition asks for nothing.

### `[image]`

Exactly one of **install steps** or an **escape-hatch Dockerfile**:

| Key | Meaning |
|-----|---------|
| `install` | Array of shell commands, one `RUN` step each, run as root during the image build. Each entry must be a single line |
| `apt` | Extra Debian packages to install first |
| `needs_node` | `true` adds Node 20 before your install steps (for `npm install -g …`) |
| `binary` | Binary used for the build's smoke test; defaults to `run.command[0]` |
| `dockerfile` | Path (relative to the TOML) to a hand-written Dockerfile. Cannot be combined with `install`, `apt` or `needs_node` |

### Validation table

awman reports every problem it finds in one pass. The file is capped at 64 KiB and every array at 64 entries.

| Field | Rule |
|-------|------|
| `schema` | Must be `1` |
| argv tokens (`run.command`, mode and tool flags, `acp.command`) | Non-empty, no NUL or newline characters, at most 1024 bytes. `run.command[0]` is a bare name or an absolute path |
| `ping.command` | Non-empty argv, at most 16 tokens. `command[0]` is a bare binary name (looked up on your host `PATH`) or an absolute path; relative paths like `./pi` are rejected |
| `system_prompt` | Each delivery requires exactly the keys shown above |
| `run.env` keys | POSIX environment names; `HOME`, `PATH`, `USER`, `AWMAN_*` and `LD_*` are rejected |
| `auth.env` | POSIX environment names; `AWMAN_*` and `SSH_AUTH_SOCK` are rejected |
| `settings.host_dir` | Relative, no `..`, not empty or `.`, and not on or under the [deny-list](#security-and-approval) |
| `settings.skills_dir` | Relative, no `..` |
| `image.install` entries | Non-empty, single line, at most 4096 bytes |
| `image.apt` | Each entry matches `^[a-z0-9][a-z0-9+.-]*$` |
| `image.dockerfile` | A relative path inside the TOML's own directory (no `..`, no symlink escaping it); exclusive with `install`, `apt` and `needs_node` |
| `[image]` | Needs either `install` or `dockerfile` |
| `[sandbox]` | Rejected: `custom agents do not support Docker Sandboxes yet; remove [sandbox] and use docker, apple or builtin-experimental` |
| `[run].output_format` | One of `text`, `claude_stream_json` or `codex_json` (default `text`) |

---

## Images

### Install steps

With `install`, awman generates `.awman/Dockerfile.<name>` for you from a hardened template: it starts from your project's base image, installs your `apt` packages and optional Node 20, runs each `install` entry as root, creates the unprivileged `awman` user and workspace, and finishes with a smoke test that the agent binary is on `PATH`. A minimal example:

```toml
schema = 1
display_name = "Pi"

[run]
command = ["pi"]
non_interactive = "-p"
output_format = "text"

[modes]
yolo = ["--yolo"]

[image]
needs_node = true
install = ["npm install -g --prefix=/usr/local @example/pi-coding-agent"]
```

`output_format` tells awman how to parse the agent's non-interactive output.
Use `claude_stream_json` for Claude Code's stream-JSON output or `codex_json`
for Codex JSONL output; these modes add the corresponding CLI flags and let
API clients receive `step_activity` events and, for `exec prompt`, the
agent's `final_message` in the command result. Choose `text` (or omit the
field) for ordinary text output. This is a protocol declaration: awman does
not infer the format from the agent name, so the selected CLI must support the
declared format. Interactive and ACP launches use text output.

### Escape-hatch Dockerfile

For multi-stage builds or unusual needs, point `image.dockerfile` at a Dockerfile next to the TOML:

```toml
[image]
dockerfile = "pi.Dockerfile"
```

Start the file with `FROM {{AWMAN_BASE_IMAGE}}` to layer on top of your project's image; awman substitutes the placeholder when it copies the file into `.awman/`. If the line is missing, awman warns: `no FROM {{AWMAN_BASE_IMAGE}}; the project image will not be layered underneath`.

### How images are rendered and rebuilt

The generated file is `.awman/Dockerfile.<name>` and starts with a two-line header recording the hash of the definition it was generated from:

```
# awman-generated from <scope>:<name>.toml — definition sha256: <hash>
# Edit the agent definition and re-run `awman ready`; do not edit this file.
```

- `awman ready --agent <name>` writes the file when it is missing or when the definition changed (different hash), then builds the image. Use `--build` / `--no-cache` as usual to force a rebuild.
- If you change the definition and launch a session without running `ready`, awman warns that the image is out of date and keeps using the image you already have; it does not rebuild automatically.

### Taking ownership of a rendered Dockerfile

If you need to tweak the generated Dockerfile by hand, **remove the header line** (the `# awman-generated from …` line). awman then treats the file as yours: it is never overwritten, and `ready` builds it as-is, noting `using hand-maintained .awman/Dockerfile.<name>; delete it to regenerate from <name>.toml`. To go back to generated output, delete the file and run `awman ready --agent <name>` again.

If the header is still present but the body differs from what the definition would generate, awman warns that the existing Dockerfile will be used and suggests the same two options (remove the header line to take ownership, or delete the file to regenerate it). A Dockerfile you own is a build input like any other — review it yourself, because the approval of the definition does not cover edits you make afterwards.

---

## Runtimes

Custom agents run on **Docker**, **Apple Containers** and **builtin-experimental**. They do **not** run on **Docker Sandboxes** (`docker-sbx-experimental`) yet; `awman ready` fails the agent-image step with a clear message if your runtime is Docker Sandboxes. See [Runtimes](11-runtimes.md).

---

## Security and approval

A custom definition is configuration that awman acts on — it decides which commands run in your image build and which host directory is mounted — so awman asks you to approve it first.

### The approval prompt

The first time a definition is used, awman shows a review and asks you to approve it:

```
awman: Approve agent definition 'pi'?
  …review…
  [y] Approve this definition
  [n] Do not approve
choice [y/n]:
```

The review shows, in order: the host command (if any — see below), the agent's display name, where the definition was loaded from, the homepage, the install steps (or the hand-written Dockerfile's path and size), the settings directory that will be mounted read-write (with a warning if it overlaps a built-in agent's settings directory), the names of the auth variables, the run environment, any note about an existing Dockerfile, and the definition's SHA-256.

In the TUI the same review appears in a scrollable dialog: long commands wrap, ↑/↓ and PgUp/PgDn scroll, and a status line says whether more of the review is below. `y` is accepted only once you have scrolled to the end of the review (on a terminal too small to show it, approve from a larger one or from the CLI); `n` and Esc decline at any time. Control and bidirectional-formatting characters are shown as visible escapes (`\u{1b}`) everywhere in the review and in error messages, and are rejected in definition strings.

- **`y`** records the approval and continues.
- **`n`**, Esc or end-of-input stops with `agent definition '<name>' was not approved` (exit 1).
- **No one to ask** — `--non-interactive`/`-n`, `--json`, no terminal, the HTTP API, or the squad daemon — never prompts. The command fails with `agent definition '<name>' (<path>) has not been approved for this repo; run 'awman ready --agent <name>' interactively to review it` (exit 2, or HTTP 403 through the API). `--yes` does not approve anything.

An unknown agent name exits with status 4 and lists the known agents; a definition that fails validation exits with status 1 and lists every problem.

### The trust store

Approvals are stored per definition in `~/.awman/trusted-agents.json`, keyed by the repository and the **content hash** of the TOML plus any Dockerfile it references. Change either file and the hash changes, so awman asks again.

- **Repo definitions** always need approval, because anyone who can commit to a repository can change them.
- **Global definitions** (`~/.awman/agents/`) are your own files and need no approval — unless they declare `[ping]`, in which case they are reviewed like a repo definition.
- A shipped example copied with `awman new agent --from` is approved for you when you create it, when approval is needed.

Run `awman ready --agent <name>` to review a definition interactively. `ready` reports the result as `agent definition: loaded from <scope>:<path> — trusted ✓` or `— approval not needed`.

### Host-path deny-list

`settings.host_dir` cannot point at, inside, or above these directories under your home: `.ssh`, `.gnupg`, `.aws`, `.azure`, `.config/gcloud`, `.kube`, `.docker`, `.awman`, `.netrc`, `.git-credentials`, `.config/gh`, `.password-store`, `.local/share/keyrings` and `Library/Keychains`. Comparison ignores ASCII case, and a symlink that leads outside your home or onto one of these is refused. Protection follows the real location too: awman's own data directory (wherever it lives, e.g. under `~/.local/share` or `AWMAN_CONFIG_HOME`) and the real target of a symlinked protected directory (say `~/.awman -> ~/state`) cannot be mounted either, nor any directory above them.

### No shell, no host execution

Every command in a definition is an argv array — awman never passes it through a shell on your host. Install steps are written into the image build as exec-form instructions, so they cannot spill into other Dockerfile lines. Environment variables in `run.env` and `auth.env` are only applied inside the container.

### The optional host check (`[ping]`)

Built-in agents get a quick check during `awman ready`: awman runs the agent on your host once, with a fixed greeting, to confirm it is installed and logged in. A custom agent can opt in to the same check by declaring `[ping]`:

```toml
[ping]
command = ["pi", "-p"]
```

- **What runs:** `command` on your **host** — the one place a custom definition can execute outside a container — with one of awman's built-in greetings (for example `Hello`) added as the last argument, from an empty temporary directory outside the repo, with a time limit of 60 seconds. It receives no environment from the definition. It runs only during `awman ready`, never during chat or workflows.
- **You see it first:** the approval prompt begins with the exact command and where it resolves on your `PATH`, for example:

  ```
  ⚠ `awman ready` will run this command ON YOUR HOST (not in a container):
      pi -p '<greeting>'
      (pi resolves to /usr/local/bin/pi on your PATH)
  ```

- **Without `[ping]`,** `awman ready` skips the host check for that agent and says so; the review reads ``no host command: `awman ready` will skip the host check``.
- Global definitions with `[ping]` are reviewed and approved just like repo ones.

Leave `[ping]` out unless you want the check.

---

## Using a custom agent

Once a definition is approved and its image built, the agent works anywhere a built-in agent name is accepted:

```sh
awman ready --agent pi          # review, approve, build the image
awman chat --agent pi
awman config set agent pi       # make it this repo's default
awman config set --global agent pi
awman init --agent pi           # init a repo with it (definition must already exist)
```

In workflows, set `agent = "pi"` on a step. awman checks and asks for approval of every distinct agent before the workflow starts. `config set agent` accepts built-in names plus custom agents defined in the scope you are writing to (repo config also accepts global ones); otherwise it fails with the list of valid agents and `define one with: awman new agent <name>`.

**Workflows and non-interactive mode.** Non-interactive runs rely on `run.non_interactive`. An agent whose definition has none has no print mode to switch to, so in a workflow step it may not exit on its own; you may need to finish or quit the step yourself. Prefer agents with a non-interactive mode for unattended workflows.

Models: `--model` works if `run.model` is `space` or `equals`; with `unsupported`, awman does not pass a model flag.

---

## Migrating from crush and maki

`crush` and `maki` used to be built-in agents. They are now shipped as custom-agent examples so they follow the same rules as any other custom agent. If your config or workflows name them, create the definition once:

```sh
awman new agent crush --from crush          # repo only
awman new agent maki --from maki --global   # or for every repo
awman ready --agent crush                   # review, approve, build
```

Both examples declare `[ping]`, so the approval prompt shows the host command (`crush run '<greeting>'`, `maki --print '<greeting>'`) — approve it, or delete the `[ping]` table to skip the host check. The examples do **not** forward any provider API keys automatically. Add the variables your provider needs as `env(VAR)` overlays, or list them under `[auth] env` in your copy of the definition. The Crush example mounts `~/.config/crush` and uses `run` for non-interactive mode; the Maki example uses a Dockerfile (`maki.Dockerfile`) next to the TOML.

Anything that referred to `crush` or `maki` by name (`agent` in config, `agent = "crush"` in workflow steps, `--agent crush`) keeps working once the definition exists. Without it you get `unknown agent 'crush'; … define one with: awman new agent crush`.
