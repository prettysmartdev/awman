# CLI & TUI UX Standards

Binary name: `awman`
Install path: `/usr/local/bin/`
Storage location: `$HOME/.awman/`

This document is the authoritative specification of `awman`'s interaction
design: the conventions every command, flag, prompt, and dialog follows,
across every frontend. It does not enumerate commands or flags — that
surface is generated from `CommandCatalogue`
(`src/command/dispatch/catalogue.rs`) into `docs/14-command-reference.md`,
which can never drift from the code because it is the code, rendered.
Anything below applies to the whole surface; nothing below names a specific
command or flag.

## Design principles

- **Single binary, two modes.** `awman` with no arguments launches a
  Ratatui TUI. `awman <subcommand> …` runs one command and exits, with
  output on stdout/stderr.
- **Catalogue-driven.** Every command, subcommand, flag, default, and
  argument lives in `CommandCatalogue`. Frontends (CLI, TUI, API) project
  from it; none of them hard-codes a command name, flag name, or default —
  see the module-level doc on `src/command/dispatch/projections/`. A
  convention that can't be expressed as catalogue data (a flag's kind,
  default, `implies`/`conflicts_with`, or frontend visibility) doesn't
  belong in a frontend either; it belongs in a new catalogue field.
- **Runtime isolation.** Every agentic operation runs inside an isolated
  container or the builtin Linux microVM. The host never executes agent code
  directly — see `aspec/architecture/security.md`.

## Naming and casing

- Long flag names are lowercase, kebab-case, and describe the value or
  toggle, not the implementation (`--skip-verification`, not `--no-tls-chk`).
- A single-character short alias is reserved for flags common enough to
  type constantly (non-interactive mode, skip-confirmation, follow-logs).
  A short alias is never reused for a different meaning on a different
  command.
- A boolean flag is presence-only: `--foo` sets it, its absence doesn't;
  there is no `--foo=true`/`--foo=false` form.
- A value that repeats is a repeatable flag (`--foo a --foo b`), never a
  single flag with a delimited list — delimiters force the frontend to own
  parsing rules the catalogue can't express as a value kind.
- An enum-valued flag's accepted values live in the catalogue as data (its
  `FlagKind::Enum` payload) and are rendered wherever the flag is
  documented or completed; they are never duplicated as a string in a
  frontend or in prose here.

## Positional vs. flag

- A required, identifying value (a path, a name, a number the command
  can't run without) is a positional argument.
- Everything else — toggles, overrides, output shaping — is a flag, always
  optional at the catalogue level even when a command's own validation
  requires it in practice (so the same flag can be optional for one
  subcommand and required for another without two catalogue entries).

## Cross-cutting flag behavior

- `--json` always implies non-interactive mode: a machine-readable output
  mode can never block on a prompt. Any flag that changes output shape for
  scripting implies the same.
- Non-interactive mode suppresses every prompt. Where a prompt would have
  supplied a required decision and none was given, the command refuses
  with a usage error instead of guessing or blocking on stdin.
- A flag that forces one outcome and thereby makes another flag's request
  impossible to honor is declared as a conflict (`conflicts_with`) rather
  than silently overridden; a flag that forces a prerequisite on for
  correctness is declared as an implication (`implies`) rather than
  silently required.

## Editing existing artifacts (`edit`)

`edit` is a grouping parent with four leaves — `spec`, `workflow`, `skill`,
`agent` — each taking one required positional `ref`. It only changes
artifacts that exist; creation stays under `new`. There is no `specs`
command and no alias for the retired spec-amendment subcommand.

- **Flags.** Every leaf has `--amend`, `--prompt`, `--headless`
  (`-n`), `--json` (implies headless), and the shared agent launch flags
  (`--agent`, `--model`, `--overlay`, `--agent-flag`, `--auto`, `--yolo`,
  `--allow-docker`). `workflow`, `skill` and `agent` also take `--global`.
  `--plan` is not offered.
- **Usage rules.** A headless edit requires `--prompt` unless `--amend` is
  set. An empty `--prompt` is treated as absent.
- **Target resolution** happens before anything launches or is copied.
  Workflows, skills and agents resolve in the repo first; `--global` selects
  the personal store; a miss that would hit in the other scope names the flag
  to use. Ambiguous names list candidates. A path outside the allowed roots,
  a symlink escaping its root, and first-party agents are refused (first-party
  agents are read-only). Work items accept every reference form in the grammar
  below, resolve strictly against the owning provider, and refuse inline items.
- **Persistence** follows where the target lives: in place through the
  `/workspace` mount for artifacts in the checkout; a staged copy under the
  repo context (`scratch/edit/<id>/`) published per file afterwards for
  global, library or out-of-checkout artifacts; a synced draft for remote
  work items, saved while the agent runs. Only the artifact's own files are
  published; other files the agent created are reported, not saved.
- **Outcome rule.** Success requires the agent to exit 0 and the save and
  validation to succeed; an edit with no changes succeeds. When both fail,
  the error reports both. Cancellation is not rollback: whatever was
  written or uploaded remains, and what is on disk is saved. Drafts kept
  after a failed save are retained and their paths reported. An in-repo edit
  that leaves an invalid artifact is left as written with the problems
  listed.
- **Trust.** Editing an agent definition never approves, builds, or runs it;
  the changed definition is pending approval until `ready --agent` reviews it.
- **Amendment** applies uniformly to every kind. Interactive agents clarify
  uncertain findings with the user; headless agents report uncertainty without
  making speculative changes.

## Creation and editing agent sessions

`new spec|workflow|skill|agent` and `edit spec|workflow|skill|agent` always use
an agent to author the selected files. The only built-in authoring questions
are simple title/name, type and summary inputs. There is no workflow step
builder, skill body editor, direct issue-creation shortcut or `--interview`
flag. Scaffolding establishes the target and its format; it is never a
successful alternative to running the agent. Existing files are edited only
through the agent session. Headless and JSON sessions remain supported, with
explicit inputs replacing questions. Skill-library pulls remain import
operations and do not launch an authoring agent.

For creation, `--title` and `--prompt` supply the title/name and summary;
`new agent` takes its name positionally. Headless creation requires these
inputs, except that a spec seeded with `--work-item` can use the source title
and body. `--kind` supplies the work-item type; headless specs default to Task.
All new/edit authoring commands accept `--yolo` and pass it through the normal
agent launch policy. It selects that agent's configured most-permissive mode;
it does not bypass awman's mount scope or definition trust checks.
`new agent --from` is a session seed, not a copy-only operation, and never
records approval. Creation refuses existing files; it has no overwrite mode.
Local spec creation requires its configured directory to remain inside the
mounted repository; it never writes an inaccessible seed outside that scope.

Agents publish their status through the same container
feed as `chat` and `exec prompt`, including remote work-item draft sessions.
Tabs and container windows show reported status and use the usual silence
fallback when an agent does not report. Creation and editing prompts constrain
which files may change but do not suppress the agent's completion summary.

## Work item reference grammar

Commands that accept a work item use the shared `--work-item <REF>` flag,
with `-w <REF>` as its short alias on every command that supports it.
The value is a single reference, not a comma-separated list. Its grammar is:

- A bare value, resolved using `workItems.source` (`local`, `github`, or
  `http:<provider-name>`).
- `local:<N>` or a local Markdown path.
- `github:<N>`, `#<N>`, `owner/repo#<N>`, or a GitHub issue URL.
- `http:<provider-name>:<id>` or a URL belonging to that provider's
  configured base URL.

Prefixed forms (`local:`, `github:`, `http:`) are read first, so an HTTP id may
contain `:`, `/` and `#`. A value with exactly one `/` before a `#`
(`owner/repo#N`) is a GitHub reference: an invalid owner, repository or number
is an error, never a local path. Errors that echo a URL omit its credentials,
query and fragment.

An explicit reference selects its source regardless of the configured
default. A bare value never triggers source probing: resolution uses exactly
the configured source. Unknown sources, missing items and provider errors
fail the command; they do not fall back to local files or another provider.
The `--issue` spelling is removed and frontends that still receive it give a
migration hint to use `--work-item github:<N>`.

## Session worktree selectors

The two single-agent session commands (interactive chat and the one-shot
prompt) accept three mutually exclusive, valued selectors that run the session
in a linked Git worktree of the invoking repository instead of the invoking
checkout:

- The branch selector picks a worktree for a branch. Remote-only: fetch it and
  track the remote branch (adding a fetch mapping for it when the remote's
  refspecs do not cover it). Remote and local: fast-forward the local branch
  when possible, never discarding local commits. Local-only: use it. Neither:
  create it from the invoking checkout's `HEAD`, reporting that uncommitted
  changes are not carried over. A missing `origin` is warned about and treated
  as no remote; network or authentication failures are errors, never a
  fallback to creation.
- The pull-request selector (a number, a github.com pull-request URL, or
  `owner/repo#N`) picks a worktree for a GitHub pull request. A
  same-repository PR uses its head branch like the branch selector; a fork or
  deleted-head PR uses a local `pr/<N>` branch from `refs/pull/<N>/head` and
  warns that pushes to `origin` will not update the PR.
- The existing-worktree selector (a name or a path) reuses an existing linked
  worktree of this repository. It never fetches, checks out, merges or creates.

The selectors are catalogued for the CLI and TUI only; every other frontend
refuses them with the standard "not available" error. The workflow command's
boolean worktree switch is unrelated and keeps its workflow-managed lifecycle.

Flags precede the prompt. The shared catalogue parser rejects standalone known
options in a greedy prompt tail, including global/inherited options, short
aliases and recognized clusters, and `--name=value` forms. It never scans inside
a prose argument. A `--` before the first prompt token marks the entire tail
literal; a later `--` is prompt text and does not disable the guard. This intent
survives CLI/TUI/API forwarding. Errors use usage exit 2 / API `invalid_request`
and explain moving options before the prompt or using a pre-prompt `--`.
Correctly positioned `--agent-flag` consumes one value even if it begins with `-`.

`exec prompt` requires a nonblank task or resolved work-item content in every
interaction mode, before image preparation or launch. Runtime wait failures are
command failures (`runtime_wait_failed`, nonzero exit), and known provider
nonzero exit codes are preserved, including when cancellation races completion.

Host Git that contacts a remote for these selectors refuses repository-scoped
(local or worktree) configuration that selects a transport program, proxy,
credential helper or TLS material; never lets SSH prompt (batch mode, unless
the user chose an SSH command in their environment or global config); and is
bounded by a timeout. The pull-request lookup always names the `github.com`
host explicitly.

Created worktrees live under `~/.awman/worktrees/<repo-key>/` as `br-<branch>`
(with a hash suffix when the branch name had to be sanitised) or `pr-<N>`.
They are retained: the commands report `Worktree kept at <path> (<branch>).`
on exit and never merge, delete or prune.
Agent, trust, config and overlays resolve from the invoking checkout, not the
worktree.

## Credential prerequisite decisions

Credential grants require explicit decisions and are never implied by bulk
approval or automatic progression. Interactive frontends share the same command
prompts and safe dismissal defaults; unattended frontends use policy and explicit
inputs. Remote clients cannot widen the operator's credential ceiling without a
global opt-in. Check-only output exits 0 only when every check is satisfied,
otherwise 1 (including unknown checks). Summary Enter continues to the questions
and Esc or Ctrl-C cancels. Prerequisite answers use ordered numeric keys, then
letters for longer lists. Invalid credential inputs, refused widening and unresolved fail policies
use exit 2; explicit abort uses 130. Per-command syntax belongs in the generated
command reference.

## Exit codes

Exit codes are classes of outcome, not per-command codes:

| Exit code | Class |
|---|---|
| 0 | Success. |
| 1 | Runtime failure inside a lower layer (engine, data, transport). |
| 2 | Invalid usage: bad flag value, missing required input, a conflict, or required interactive input unavailable in a non-interactive context. |
| 3 | The Docker or Apple Containers backend is missing or failing (`ContainerRuntimeUnavailable`, `Container`). Builtin runtime and image-builder failures are class 1. |
| 4 | A referenced resource (file, work item, template) does not exist. |
| 130 | Aborted by the user (Esc in the TUI, Ctrl-C on the CLI). |

A command that introduces a new failure mode maps it to the class it
belongs to rather than inventing a new code.

## Prompt and dialog conventions

- Every interactive choice shown to the user — a CLI stdin prompt or a TUI
  modal alike — is described by one `Prompt<D>`-shaped value that Layer 2
  owns: a title, an optional body, an ordered list of typed choices (each
  with a key, a label, and a value), and an explicit default returned when
  the user dismisses the prompt without choosing (or no default, which
  makes dismissal an abort). A frontend renders that shape — mapping a
  keystroke or a button to a choice's value — and holds no label, hotkey,
  or default of its own.
- A confirmation for a destructive or irreversible action always has a
  flag that skips it for scripting, and refuses instead of guessing when
  neither a TTY nor that flag is available.
- Dismissing a prompt (Esc, or a "No"/"Cancel" choice) never partially
  applies the action it was confirming.
- In the TUI, Ctrl-W opens workflow controls throughout an active workflow,
  including final review after all agent containers have exited. Esc at final
  review only dismisses the board; it does not pause or end the command.
  Ctrl-W reopens it until the user chooses Finish, Pause, or Abort. Builtin
  and daemon backends share this behavior.
  Workflow-control routing belongs to the command lifetime: replacing a
  container must neither invalidate its sender nor discard queued Ctrl-W
  requests. Container-specific status feeds remain scoped to their launch.

## Hint and help-text style

- A command or flag's help text is one sentence: capitalized start,
  trailing period, describing the effect, not the implementation.
- A side effect that isn't obvious from the flag's name — an implied flag,
  a value it forces, a frontend it's hidden from — is stated inline in
  that same sentence, not left for the reader to infer from behavior.
- Long-form help (a second paragraph of context, only where the one-line
  summary isn't enough) is separate catalogue data from the summary, never
  a concatenation the frontend builds itself.

## Command-surface hygiene

- An unrecognized command path is corrected against its actual siblings
  (not a hardcoded top-level list) within a small edit-distance threshold,
  and reported as unknown — never silently guessed — outside that
  threshold. The catalogue owns this lookup so every frontend suggests the
  same correction from the same source.
- A retired flag is recognized before parsing and answered with a
  migration hint, not clap's generic unrecognized-argument error.

## Frontend visibility and parity

- Three frontend kinds exist: CLI, TUI, and API. A command or flag's
  visibility across them is catalogue data, not a frontend-side branch —
  see the visibility field on each catalogue entry and its projection into
  `docs/14-command-reference.md`.
- Each command's API availability is a catalogue value (`ApiAvailability`), never
  a frontend-side branch: `Disabled`, `Standard`, `LocalOwner` (authenticated
  owner on the local listener only) and `LocalAutomationTerminal`. Long-term
  policy still excludes request-owned PTYs: an HTTP request cannot safely own a
  PTY's terminal lifecycle for the duration of the command, so an interactive
  or PTY-bound command stays `Disabled` for the API.
- **Narrow exception — `chat --automation`.** `chat` is `LocalAutomationTerminal`.
  It is reachable through the daemon command API only for the authenticated
  owner on the local listener, only with `--automation`, `--settings-profile`
  and effective interactive mode (admission fails `invalid_request` otherwise,
  `forbidden` for any other principal or listener, including forwarded, network,
  browser and relay origins; the owner/listener check is re-run when a queued
  command starts). The PTY is owned by the daemon's execution, not by the HTTP
  request: it outlives viewers and is driven through the single terminal
  protocol with ownership, revision and idempotency fences. All other
  interactive and PTY-bound commands (`squad attach`, `chat` without
  `--automation`, and so on) remain excluded. `agents profile list|delete|close`
  are `LocalOwner`; `exec prompt` and `exec workflow` remain `Standard` but their
  `--settings-profile`, `--agent-session` and `--automation` flags are owner-local
  and excluded from `remote exec`.
- Where the same decision is available from more than one frontend, its
  label, default, and behavior are identical — verified by parity tests
  that assert against the catalogue and the shared prompt/dialog data, not
  against a second copy of the literals.

## HTTP API

- Every newly generated error body is `{"error": "<message>", "code": "<snake_case>"}`. The
  `code` is stable and documented once, in the error-code table in
  `docs/09-api-and-remote-mode.md`; existing `error` text is preserved for compatibility. Codes
  are defined by the `ApiError` type in `src/frontend/api/`, and the
  asynchronous `result.failure.code` reuses the same vocabulary where the
  conditions overlap. Idempotent replays preserve stored bytes; errors cached
  before error codes were introduced may lack a code until the 24-hour expiry.
- Every new user-visible route, request field or event type adds an entry to
  the `features` list reported by `GET /v1/status` in the same change. The
  list must not contain duplicates; a test asserts this.
- A route is never removed within `v1`. When a resource gets a better path,
  the old path keeps being served and the docs point to the new one.
- The API server binds `127.0.0.1` only, with no option to listen elsewhere;
  a client on another machine reaches it through an SSH tunnel or a reverse
  proxy on the server host. A configurable bind address — with TLS required
  for any non-loopback address — is specified by WI 0140 and is not part of
  the current surface. A remote client whose connection to a non-loopback
  address is refused says this in its error instead of reporting only the
  transport failure.

## Remote execution

- A client command that runs another command on an awman server
  forwards every flag it declares, apart from its own
  transport flags, as the argv the server's API parser reads. A flag the
  server cannot honour for a remote caller is left out of the client
  command's catalogue entry, so it is refused as an unknown flag; it is never
  accepted and dropped.
- A client that follows a remote command to completion exits with that
  command's result (non-zero when it failed, was cancelled, or following
  stopped before it finished). A client that only submits exits 0 once the
  server accepted the command.

## Output and configuration

- Across Docker, Apple Containers and builtin, preparation
  reports work rather than every internal check. Image preparation is quiet
  when an installed image can be reused, including
  when no source is configured. When acquisition is needed, use one progress
  row, `Preparing <tag> (<source kind>)`, through completion or failure. This
  includes acquisition-cache checks and does not claim a transfer or extraction.
  Routine progress omits source references, full digests and duplicate completion
  summaries; warnings and actionable failures remain visible.
- Building runtimes report builds only when needed; reuse must never report a
  completed build. Phase labels describe checks until work is known to be
  needed. The CLI omits phase transitions and relies on work progress and the
  final readiness summary. Raw build output is shown without a `running` suffix
  on every line; there is no additional debug-style completion dump.
- When Docker Sandboxes support is restored, sandbox readiness uses
  `Preparing <agent> kit (sbx)`. Successful `version`,
  `ls` and `kit validate` probes omit command announcements and stdout, while
  stderr warnings and failed commands' diagnostics remain visible. Mutating
  sandbox commands retain their announcements. General networking guidance
  belongs in the runtime guide, not in every preparation log.
- Human-readable output goes to stdout, diagnostics to stderr. A
  machine-readable output mode replaces the human renderer entirely rather
  than appending structured data alongside it.
- The TUI takes over the terminal via Ratatui; a container's own PTY
  output is forwarded through it, ANSI escapes and all.
- Configuration resolves in one order, highest precedence first: an
  explicit flag, then an environment override, then repo-scoped config,
  then global config, then the catalogue's built-in default. A command
  never reads configuration through a path that skips a level of this
  order.

Agent status reporting is controlled by `agents.programStatus.enabled`
(default `true`). TUI host-terminal status emission is controlled separately
by `tui.programStatus.emit` (default `true`); it can be disabled for one run
with `awman --set tui.programStatus.emit=false`. Daemon-managed interactive
terminals start at `daemon.terminal.cols` × `daemon.terminal.rows` (120 × 40
by default), and retain their size after launch. Squad and pushed-worker runs
select `interactive` or `headless` mode through their respective
`daemon.features.*.interaction` setting. `allowOverride` controls whether a
run can request a different mode; the default is `true`. Configuration
accepts the pull-worker interaction setting, but no pull worker runs currently.

## Runtime and image-source configuration

User-facing labels and explanations call the embedded backend the "builtin
runtime". Dependency names belong in internal implementation details and the attribution at the bottom of the runtimes documentation. The
guest hostname for explicitly allowed host ports is `host.builtin.internal`.

`runtime.kind` selects `docker` (default), `apple`, `sbx`, or `builtin`.
`sbx` is enabled experimentally on Apple silicon macOS and Linux x86_64;
unsupported hosts fail explicitly without fallback. `builtin` is experimental. The builtin microVM is an experimental
preview on macOS Apple Silicon and Linux x86_64 GNU builds with read/write
`/dev/kvm` access; Linux ARM64 remains gated. Unsupported hosts fail explicitly
without substituting another runtime. Unknown values are rejected, never
silently mapped to Docker. Configuration uses the nested `runtime` object:

```json
{
  "runtime": {
    "kind": "builtin",
    "builtin": {
      "imageSource": { "type": "registry", "registry": "ghcr.io" }
    }
  }
}
```

`runtime.kind` and `runtime.builtin.stateDir` are process-scoped. The process
resolves them at startup; a command-local override that differs from the
running process is refused and requires restarting with the requested value.
`runtime.builtin.stateDir` resolves from `AWMAN_RUNTIME_BUILTIN_STATE_DIR`,
then global config, then `<data home>/builtin`; a repository value may repeat
that path but cannot change it. Other builtin settings, including `network`,
`vcpus`, `memory`, `rootDisk`, `mountQuota`, image sources, images,
registries and `builder.enabled`, are session-scoped and resolve from each
session's effective config. Repository values override global values by key;
a repository network policy may narrow but never widen the global policy.
Workflow worktree sessions retain the invoking session's repo settings.

Image building is opt-in with `runtime.builtin.builder.enabled` (default
false). The builder can build only through the local Docker Engine selected by
`docker-local` (Unix socket) or the Apple Containers store selected by
`apple-local`, and imports from that same source. Registry, archive, remote
Docker and unconfigured sources do not select a builder. `ready` and API
session setup may build when their policy permits; workflow preflight, dynamic
workflow and squad evaluation use import-only policy and do not build. `awman
init` uses the configured runtime's build operation and does not build through
the builtin runtime; initialize with Docker or Apple Containers, then select
builtin.

The builtin block supports `stateDir`, `vcpus` (default 2), `memory` (default
`4GiB`, minimum `128MiB`), `rootDisk` (default `30GiB`, minimum `256MiB`),
`mountQuota` (default `30GiB`, minimum `1MiB`), `imageSource`, per-image `images`,
per-host `registries`, `builder.enabled`, and `network` (`public`, `allowlist`,
or `none`; optional `allow` and `hostPorts`; global-only `nameservers` and
`trustHostCas`).

```json
{
  "runtime": {
    "kind": "builtin",
    "builtin": {
      "vcpus": 2,
      "memory": "4GiB",
      "imageSource": { "type": "registry", "registry": "ghcr.io" },
      "images": {
        "awman-agent:latest": { "type": "archive", "path": "/images/agent.oci.tar" }
      }
    }
  }
}
```

Image source objects use one explicit `type`: `registry`, `docker-local`,
`apple-local`, or `archive`. Registry settings may select anonymous, named
environment variables, OS keychain, or Docker-config credentials and may
declare per-host CA/insecure-registry settings. Docker-local settings may
select a local or remote Engine endpoint and TLS files. Apple-local acquisition
(macOS) is an in-process, version-gated XPC bridge to the Apple Containers
image helper: it refuses service releases outside the pinned list, never runs
the `container` CLI or reads the private store, and validates the export like
any other archive. Off macOS, or without the bridge, it is reported blocked.
`runtime.builtin.builder.enabled` defaults to false. When enabled, `ready` may build
only through the local Docker Engine or Apple Containers store selected by
that image's resolved source, then imports from that same source. Registry,
archive, remote Docker, and unconfigured sources do not select a builder.
Incompatible requested builds warn and retain normal source import behavior.
API session setup may build a missing compatible-source image. `chat`,
`exec prompt`, workflow preflight (static and dynamic) and squad evaluation
only import from the selected source and never build. The workflow prerequisite
review reports pending preparation using the same input hashes, including
generated recipes and project-image dependencies. With the builder disabled a
launch never acquires.
These checks re-acquire an image that is present but was imported from a different
source than the one now resolved, so an image they accept is the one `ready`
would have accepted. Like `ready`, they refuse to import a default source that
names a single image (an archive or a fixed reference) for any agent other than
the configured default one. The builtin backend does not silently choose another
source or change the selected execution runtime.

An agent that lacks an allowed/disallowed-tools flag is rejected with a
configuration error on every available runtime (Docker, Apple Containers and builtin),
rather than launching with the tool restriction silently dropped. The effective
`yoloDisallowedTools` list is such a restriction on every yolo-mode launch (any
frontend, any workflow step): it is added to the launch's disallowed tools, and
an agent that cannot deliver it is refused with an error naming the setting.

Before a workflow starts, agent images join credentials and tools in one
prerequisite review. Static workflows resolve step, workflow and configured
agent defaults; a step with no resolved agent is rejected during preflight.
Fresh dynamic workflows include the leader and every agent offered to it;
resume checks the saved workflow's agents. Continuing the review approves
preparation of the listed images before any agent starts. Hosts with nobody
to ask use their headless profile. Building runtimes provision missing agent
Dockerfiles and require the project image; importing and sandbox runtimes keep
their own image preparation. There is no separate image dialog or post-leader
build pass. Generation may only narrow the prepared scope.
Sandbox tool lists also require a supported native settings delivery path;
currently only Claude's sandbox startup script applies them.

The internal `awman machine …` first-argument route is reserved for the
same-executable microVM worker and is not a public command. Existing runtime
selection commands and config editing follow the command catalogue; typed
image-source maps are edited in config rather than guessed from Docker CLI
flags. The builtin runtime rejects ambient `MSB_PATH`,
`MSB_LIBKRUNFW_PATH`, `MSB_AGENTD_PATH`, `MSB_HOME`, `MSB_BACKEND`,
`MSB_CONFIG_PATH`, `MSB_PROFILE`, `MSB_CACHE_DIR`, `MSB_SANDBOXES_DIR`,
`MSB_VOLUMES_DIR`, `MSB_SNAPSHOTS_DIR`, `MSB_LOGS_DIR`, or `MSB_SECRETS_DIR`
overrides instead of loading an installed runtime.



## Native modes, preparation and automation options

- **Starting mode and filesystem access are separate options.** `--plan`
  selects only the agent's native starting mode (all eight first-party agents
  declare a mapping: Claude `--permission-mode plan`; Codex `--sandbox read-only`;
  OpenCode 1.x `--agent plan`; OpenCode 2.x a private `default_agent` setting
  interactively and `--agent plan` headlessly; Cursor `--mode plan`; Copilot
  `--plan`; Cline `--plan`; Antigravity `--mode plan`). `--read-only-workspace`
  (on `chat`, `exec prompt`, `exec workflow`) selects only a read-only workspace
  mount. Neither reads the other's input; all four combinations are valid, and
  awman adds no mode lock, so provider mode transitions in the same container
  stay available. An explicit `--plan` conflicts with explicit `--auto` and
  `--yolo`; an inherited yolo/auto level is suppressed with a notice.
  A route that cannot deliver the mode (custom agent without a `[modes]`
  delivery, an ACP agent that cannot select a mode, a sandbox kit that cannot
  carry it) is refused with `native_capability_unavailable` before image setup;
  a contradictory provider selector in `--agent-flag`, `agents.flags` or mounted
  configuration is `agent_mode_conflict` (usage error).
- **`ready --prepare-only`** selects image-only preparation: runtime and
  definition-trust checks, image inspect/build/import and registry auth remain;
  the host agent ping, credential-health read, refresh and audit never run, so
  authentication is reported `not_checked`. It conflicts with `--refresh` and
  composes with `--agent`, `--build`, `--no-cache`, `--headless` and `--json`
  (`preparation_policy`, `images_ready`, `authentication` in the result). The
  security spec's host-ping exception does not apply to it.
- **`--settings-profile NAME`, `--agent-session ID`, `--automation[=auto|require_full]`**
  are explicit invocation intent with no global default. NAME is a validated
  name, never a path. `--agent-session` and `--automation` require
  `--settings-profile`; `--agent-session` is not an awman session ID and never
  selects "latest". `--automation` is a catalogue enum with an optional value
  and missing-value default `auto`; an explicit value uses `=` only (a following
  token is never consumed), unknown/empty/conflicting values fail before
  preparation, and the result reports `flags_applied.automation` as
  `auto|require_full|null`. Optional-valued enums are a catalogue feature
  (`FlagKind::OptionalEnum`) shared by CLI, TUI and API parsing, not clap-only.
  `chat --automation` starts an idle terminal and rejects headless;
  `exec prompt` always needs a nonblank task, including with
  `--automation --interactive`, and refuses an empty one (exit 2) with a hint to
  use `chat --automation`.
- **Admission and ready semantics.** `--automation=require_full` snapshots
  `minimum_status_support = full` and is checked under the execution lock
  immediately before each automated task delivery, never against a cached
  answer; it is an admission guarantee, not a completion guarantee.
  Version metadata never gates launch (only a confirmed wrong OpenCode major
  does). Readiness for a task comes from the agent's actual native interfaces
  or OSC reports; `status_support_insufficient`, `startup_timeout` and
  `agent_not_ready` describe refusals, and an uncertain delivery is never
  retried.
- **Timeouts** are config keys, not flags: `agents.automation.startupTimeout`
  (120s), `cancelTimeout` (10s) and `outputDrainTimeout` (5s, at most the cancel
  timeout), each 1s–1h, snapshotted at launch. `agents.stuckTimeout` stays advisory.
- **Credential updates.** Each agent's credential has one refresh owner; awman
  never starts a second refresher. Live replacement and restart-based rotation are
  discoverable per agent through the capability declarations and are advertised
  only when implemented and evidenced; currently neither is advertised and a
  restart request is refused before the agent is stopped.
### Named private agent settings and native resume

`chat` and `exec prompt` accept `--settings-profile NAME` and `--agent-session ID`.
`exec workflow` accepts `--settings-profile NAME`, overridden by a step's
`settings_profile`. Names bind to the invoking repository and provider; selection
alone does not resume. The exact native ID requires a selected profile, its
manifest record and retained provider history. No `--native-session` alias exists.
Provider resume argv uses the definition's route selector and the shared argv
builder. Command projections report `settings_profile` and `agent_session`,
independently of the awman `session_id`.

`agents profile list [--agent NAME] [--json]`, `delete NAME --agent NAME --yes
[--json]`, and `close NAME --agent NAME [--json]` are local data operations with
no image requirement. Daemon use requires the authenticated owner on the local
listener, rechecked on queued execution; network, browser and relay callers are
refused. Listing exposes state and counts, never native IDs, host paths or history.
Deletion requires inactive state under the lease. Close is idempotent for inactive
profiles; it cannot displace a live awman lease. Runtime uncertainty preserves
closing state, blocking both deletion and another writer.
