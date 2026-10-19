# Work Item: Bug

Title: Squad daemon environment — bootstrap forwarding, in-memory secret push, default-on keychain persistence
Issue: n/a

## Summary:
- `env(VAR)` overlays are resolved **by name**, at argv-construction time, by
  whichever process builds the `docker run` command line
  (`src/engine/container/docker.rs:1287`). For a squad task that process is the
  **daemon**, which does not inherit the environment of the shell the user
  created the task from. A task created with `--overlay "env(GITHUB_TOKEN)"`
  therefore runs its agents with no token, silently: the emission gate is
  `if let Ok(value) = std::env::var(...)`, so a missing variable is dropped with
  no warning at task creation, no warning at run start, and no marker on the run.
- The daemon's environment differs by platform, and nothing documents it.
  `spawn_background` (`src/data/fs/daemon_process.rs:353`) has three paths:
  `systemd-run --user` forwards **nothing** (no `--setenv` is passed, so the
  transient unit runs with the user manager's environment, not the caller's);
  `launchctl bootstrap` forwards only the eight names in `FORWARDED_ENV`
  (`daemon_process.rs:407`); the `double_fork_spawn` fallback forwards the
  **whole** parent environment. `FORWARDED_ENV` is
  `#[cfg_attr(not(target_os = "macos"), allow(dead_code))]` — it is consulted by
  the launchd path only.
- The Linux consequence is worse than lost overlays: `AWMAN_SQUAD_ROOT` and
  `AWMAN_CONFIG_HOME` are not forwarded either, so a systemd-started daemon can
  resolve a different storage root than the process waiting for it — exactly the
  failure `FORWARDED_ENV`'s own doc comment exists to prevent.
- Three further variables are lost on the allowlist paths: `AWMAN_OVERLAYS` (so
  *every* overlay configured that way vanishes for squad runs, not only `env()`
  ones), `AWMAN_MAX_CONCURRENT_AGENTS`, and `AWMAN_LAUNCH_MODE`.
- Independently of overlays, `src/engine/workflow/poll_ci.rs:29` and
  `src/data/issue/github.rs:331` read `GITHUB_TOKEN` straight from the process
  environment. Those run **in the daemon**, so squad's CI polling and GitHub
  issue sourcing degrade to unauthenticated regardless of what a task declares.
- Related exposure, same code path: `build_run_argv` emits
  `-e NAME=VALUE`, putting secrets in a world-readable `/proc/<pid>/cmdline`.
  The credential block immediately below it already does the right thing —
  `-e NAME` name-only, value supplied through the spawned child's own
  environment.

## User Stories

### User Story 1:
As a: user

I want to:
a task I created with `--overlay "env(GITHUB_TOKEN)"` to actually receive that
token in its containers, on macOS and Linux alike

So I can:
give squad agents the credentials they need without discovering days later that
every run has been silently unauthenticated.

### User Story 2:
As a: user

I want to:
be told, at the moment I create or edit a task, that the running daemon has no
value for a variable the task names

So I can:
fix it before the first scheduled run instead of debugging an agent that fails
for no visible reason.

### User Story 3:
As a: admin

I want to:
the daemon to resolve the same storage root, `PATH`, and overlay configuration
as the process that started it, whichever way the OS started it

So I can:
trust that `AWMAN_SQUAD_ROOT` and `AWMAN_OVERLAYS` mean the same thing to the
CLI and to the daemon it launches.

### User Story 4:
As a: admin

I want to:
secrets bound for squad containers to never be written anywhere in plaintext —
not to a file, not to a launchd plist, not to a process command line — and to
be stored only in the OS keychain, encrypted at rest

So I can:
run squad on a shared machine without a readable copy of my tokens sitting in
`~/Library/LaunchAgents`, in `~/.awman/`, or in `ps` output.

### User Story 5:
As a: admin

I want to:
a daemon the OS restarted — at login, or after a crash — to still have the
values its tasks need, without waiting for me to run an awman command

So I can:
trust a scheduled task that fires at 3am on a machine I last touched yesterday.

## Implementation Details:

The environment the daemon needs splits into two classes with different
constraints, and conflating them is what produced the current design. They are
treated separately throughout.

**Bootstrap class** — non-secret, needed *before* the daemon can open its
storage root or listen: `PATH`, `HOME`, `RUST_LOG`, `AWMAN_CONFIG_HOME`,
`AWMAN_API_ROOT`, `AWMAN_SQUAD_ROOT`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`,
`AWMAN_OVERLAYS`, `AWMAN_MAX_CONCURRENT_AGENTS`, `AWMAN_LAUNCH_MODE`. Must
arrive at spawn time; safe to place in a plist or a unit property.

**Payload class** — the values named by `env(VAR)` overlays plus the host-side
token reads. Needed only when a *run* starts, which is always after the daemon
is listening. Never in plaintext on disk, never in a plist, never in argv. The
only place a payload value is persisted at all is the OS keychain, encrypted at
rest, and only for the purpose of surviving an OS-initiated restart (§5).

### 1. Name-only `-e` for env passthrough

- `build_run_argv` (`docker.rs:1219`) emits `-e NAME` for each
  `env_passthrough` entry whose value resolves, and supplies the value on the
  `docker` child's own `Command` environment — matching the credential block
  directly below it, which already documents this exact rationale.
- Emission stays gated on the value resolving, so
  `dedup_credentials_by_declared_env`'s "a declared but unset passthrough does
  not suppress a keychain credential" invariant (`options.rs:225-254`) is
  unchanged.
- `env_literal` keeps `-e KEY=VALUE`: those values come from awman itself
  (e.g. `COPILOT_OFFLINE=true`), never from a user secret.

This is independent of everything below and can land first.

### 2. Layer 0 — `DaemonEnv`, one overlay over the process environment

`src/data/config/env.rs` gains a host-value accessor and a daemon-owned
overlay:

```rust
/// Resolve a host environment value: the daemon overlay first, then the real
/// process environment. Every production read of a *host-supplied* value goes
/// through this; `std::env::var` remains correct for awman's own invariants.
pub fn host_var(name: &str) -> Option<String>;

/// Install/replace the daemon's payload environment. The squad daemon is the
/// only writer; every other process leaves the overlay empty and `host_var`
/// degrades to `std::env::var`.
pub fn set_daemon_overlay(vars: DaemonEnvMap);
```

- Backed by a `OnceLock<RwLock<HashMap<String, String>>>`. This deliberately
  replaces one process-global (the environment) with a controlled one rather
  than introducing new global state: it needs no `std::env::set_var` — which
  would be a genuine data race on a POST handler thread, and which the
  `publish_key_to_process_env` precedent (`squad/daemon.rs:465`) justifies only
  because it happens at startup before any reader exists — and it makes these
  reads testable, which `std::env::var` is not (see the serialising mutex at
  `src/engine/overlay/mod.rs:1304`).
- `DaemonEnvMap` wraps `HashMap<String, String>` with a redacting `Debug`
  (names only, never values) and a `From<&EnvSnapshot>`-style capture helper.
- Call sites converted to `host_var`: the production closures at
  `options.rs:474` and `options.rs:480`, `dsbx/backend.rs:315`,
  `dsbx/session_config.rs:89`, `exec_workflow.rs:3822`, `poll_ci.rs:29`, and
  `github.rs:331`. The `&dyn Fn(&str) -> Option<String>` lookup convention that
  already exists in `options.rs` is kept as-is — only what the *production*
  closure calls changes, so tests keep injecting hermetic closures exactly as
  they do today.

### 3. Bootstrap class — fix all three spawn paths

- `FORWARDED_ENV` loses its macOS-only `cfg_attr` and gains `AWMAN_OVERLAYS`,
  `AWMAN_MAX_CONCURRENT_AGENTS` and `AWMAN_LAUNCH_MODE`. The existing exclusion
  of `AWMAN_API_KEY` / `AWMAN_SQUAD_KEY` stands, and its unit test with it.
- `try_systemd_run` (`daemon_process.rs:442`) passes one
  `--setenv=NAME=VALUE` per entry of `forwarded_env()`. Safe for this class
  precisely because it holds no secrets; the payload class never goes here.
- `try_launchd` keeps `EnvironmentVariables`, now carrying the same list.
- `double_fork_spawn` is unchanged (full inheritance already covers these).

The invariant to state in the module docs: **`FORWARDED_ENV` is a
non-secret allowlist, and nothing that could hold a secret may ever be added
to it.**

### 4. Payload class — pushed over the authenticated loopback socket

- New route `POST /v1/daemon/env` in `src/frontend/squad/routes.rs:36`, behind
  the existing `auth_middleware`. A **dedicated** route, not the
  `{subcommand, args: Vec<String>}` envelope of `/v1/commands`: CLI-arg-shaped
  strings drift into tracing spans and error text, a typed body with a redacting
  `Debug` does not. Body is `{"vars": {NAME: VALUE, ...}}`; the response reports
  the names accepted, never values.
- The handler calls `env::set_daemon_overlay`. Values live in memory only.
- `RemoteTaskGateway` (`gateway.rs:691`) gains `push_env(&DaemonEnvMap)` using
  `HttpCore::post_command(&["daemon", "env"], ...)`.
- `SquadSupervisor::ensure_running` (`squad/daemon.rs:381`) pushes immediately
  after it has a gateway — on both the already-running and the freshly-spawned
  branch, so a long-lived daemon picks up a rotated token from the next command
  without a restart.
- **Which names to send.** The daemon knows what it needs; the client knows the
  values. `GET /v1/status` (or the push response) reports the daemon's
  `required_env` — the union of every `env(NAME)` across the task store, the
  daemon's own global/repo config and `AWMAN_OVERLAYS`, plus a fixed set of
  host-side names (`GITHUB_TOKEN`). The client sends the intersection of that
  list with its own environment. Nothing else is transmitted, so the daemon
  never receives values it has no declared use for, and `TaskStore` stays
  daemon-only as documented at `src/data/fs/task_store.rs:298`.
- First push has a bootstrapping wrinkle: the client cannot know `required_env`
  before the daemon answers. Resolve it by pushing in the same round trip that
  reads it — `push_env` sends nothing on a cold call, receives `required_env`,
  and re-sends once with the intersection. Two small requests over loopback,
  and it self-corrects whenever the task set changes.

#### 4a. How often values actually cross the socket

Not on every command. As first drafted this section implied exactly that — every
`awman squad list` re-sending every token — which is needless secret movement,
needless work, and makes the socket traffic proportional to how often someone
looks at a list rather than to how often anything changes.

**Every squad command performs a coverage *check*; almost none perform a
*push*.** The daemon reports, per required name, a salted digest of the value it
currently holds:

- At startup the daemon generates a random 32-byte salt, held in memory for its
  lifetime and returned with the coverage response.
- For each name it holds, it returns `sha256(salt ‖ name ‖ value)` truncated to
  16 hex characters (`sha2` is already a dependency).
- The client digests its own value the same way and compares. **Equal → send
  nothing.** Different, or the daemon holds none → include in `vars`. The client
  has none → include in `absent` (§4b).

The salt is what makes this safe to expose: without one, a digest of a
low-entropy value could be attacked offline, and digests would be comparable
across daemons and machines. Rotating it per lifetime also means a restarted
daemon re-learns coverage from the first client rather than trusting stale
digests. It is only ever handed to an already-authenticated caller.

**Steady state is therefore zero pushes and zero secrets on the wire.** A push
happens on exactly three occasions: the first command after a daemon starts, a
value actually changing (a rotated token), and a new `env()` name entering
`required_env`. The check itself piggybacks on the status response where a
command is already fetching one, so the common path adds no round trip at all.

This also makes `awman squad env` (§6d) honest: it can report whether the daemon
agrees with this shell without either side transmitting a value.

#### 4b. Names the client cannot supply

Most pushes are partial: a user runs `awman squad list` from a shell that has
`GITHUB_TOKEN` but not `AWS_PROFILE`, or from a fresh terminal that has neither.
Handling this correctly is what decides whether the mechanism helps or hurts, so
the semantics are specified rather than left to the obvious implementation.

**The push is a per-name merge, not a whole-map replace.** A replace is the
obvious implementation and it is wrong: a client that lacks a name would delete
the value a better-equipped client pushed earlier, so running `awman squad list`
from the wrong terminal would silently disarm every scheduled task — the exact
failure this work item exists to remove, reintroduced by its own fix.

The body therefore carries three states per name in `required_env`, not two:

```json
{"vars": {"GITHUB_TOKEN": "..."}, "absent": ["AWS_PROFILE"]}
```

- **Provided** (`vars`) — the client has it. Replaces whatever the daemon holds,
  so a rotated token propagates on the next command with no restart.
- **Absent** (`absent`) — the client was asked and cannot supply it. **Leaves any
  existing value untouched.** Never a deletion: "I don't have it" is not "nobody
  should have it".
- **Unmentioned** — not in `required_env`, so the client was never asked and says
  nothing. Names outside `required_env` are never transmitted.

A value is only ever removed from the overlay when the name **leaves
`required_env`** — the last task referencing it was deleted or edited. That is
the one unambiguous signal that nothing needs it any more, and it garbage-collects
the keychain item's contents at the same time.

**An empty value counts as absent**, not as a provision. This matches the rule
already in force at `options.rs:263`, where an `env_passthrough` entry only
counts as covering a credential when its host value is set *and non-empty*; two
different answers to "is this variable usable" in one codebase would be a bug
waiting to happen.

**Coverage is judged against what the daemon effectively has** — the in-memory
overlay unioned with whatever `KeychainStore` loaded at startup (§5) — never
against the most recent push alone. A name supplied by the keychain after a
restart is covered, and no client needs to know that.

**Per-name state.** The daemon tracks `last_provided_at` and `unmet_since` for
each required name. That is what makes the difference between "never supplied
by anyone" and "supplied last Tuesday, absent since" reportable, and it costs
one small map.

**Escalation, once each, in the place the user is already looking:**

1. **At task create/edit** — the gateway warns with the unresolved names (§6).
   This is the earliest and most actionable moment.
2. **In `squad status` / `squad show` / the TUI tab** — unmet names are listed
   with `unmet_since`. The discoverable surface; nobody should need a log.
3. **At run start** — unmet names are recorded on the run row, so a run that
   behaved oddly can be explained afterwards.
4. **In the daemon log** — one `warn!` the first time a given name goes unmet,
   `debug!` on repeats. A scheduled task evaluating every 5 minutes must not
   produce a warning every 5 minutes.

**A run with unmet names proceeds; it is not blocked.** Today a missing variable
is silently dropped, and every existing task was created under that behaviour —
some legitimately treat a variable as optional. Failing those runs closed would
be a regression dressed as a fix. The gain here is that the omission is
*visible* at four points instead of zero. A per-task `requireEnv: [...]` that
fails a run closed is a reasonable future addition and is deliberately **out of
scope** for this work item.

**Names, never values, leave the daemon.** `required_env` discloses which
variable names the daemon wants to an already-authenticated client; the push
response reports accepted and unmet *names* only. No endpoint ever returns a
payload value, so a compromised bearer key cannot read secrets back out of the
daemon — it could only overwrite them, which it could do anyway.

### 5. Keychain persistence (default on, degrading to no storage)

The push model leaves one gap: the OS restarting the daemon with no client
present (launchd `RunAtLoad` at the next login, a systemd restart). The daemon
comes back with an empty payload overlay and stays that way until some CLI/TUI
invocation happens to push; scheduled tasks in that window run without tokens.
Closing that gap is the *normal* case rather than an advanced one — a squad
daemon exists to run unattended — so keychain persistence is **on by default**
and falls back to no storage only when the platform cannot provide it.

The trade this makes, stated plainly because it is a default: a user who never
asked for persistence gets their `env()` values written to the OS keychain,
encrypted at rest, surviving until something removes them. That is a real
change from "nothing is persisted anywhere", and it is why the fallback is
loud (below), why the stored item is removable (§5c), and why `envPersistence:
"none"` remains available for anyone who wants the old behaviour.

- `src/data/fs/daemon_env.rs` defines the seam:

  ```rust
  trait DaemonEnvStore {
      fn store(&self, vars: &DaemonEnvMap) -> Result<(), DataError>;
      fn load(&self) -> Result<Option<DaemonEnvMap>, DataError>;
      fn clear(&self) -> Result<(), DataError>;
  }
  ```

- `KeychainStore` — **the default.** One item, service `awman-squad`, account `daemon-env`,
  value a JSON map. Reuses the existing shell-out shims and the
  `go-keyring-base64:` envelope handling in `src/engine/auth/keychain.rs`
  (`run_macos_keychain_lookup:161`, `run_linux_secret_lookup:184`,
  `decode_go_keyring_payload:204`), which stay the only keychain access in the
  tree. Writing is new: **`security -i` on macOS**, `secret-tool store` on Linux
  (which reads the secret from stdin by design).
- **The macOS write path is settled by measurement**, via
  `tools/probe-keychain-stdin.sh`. The secret must never appear as a
  `-w <value>` argument — argv is visible to the same user through `ps` — and
  the probe shows the alternatives behave as follows:
  - `security add-generic-password ... -w` with the secret **piped** is
    rejected. It does not read stdin from a terminal: it opens `/dev/tty` and
    prompts there. Under launchd (no tty) it *does* fall back to stdin, but
    demands the value twice for confirmation, hits EOF on the second read, and
    reports `passwords don't match` — while still exiting 0 with nothing
    stored. Two different wrong behaviours by context, and a silent failure in
    the daemon's own case.
  - **`security -i` works in both contexts.** Commands are read from stdin, so
    the value never enters any process's argv; `ps` sees only `security -i`.
    Confirmed writing and reading back a sentinel from a launchd job with no
    controlling terminal, with no GUI authorization dialog and no prompt of any
    kind.
- **The stored value is base64-wrapped** with the existing
  `go-keyring-base64:` envelope before it reaches `security -i`. This is not
  cosmetic: the payload is a JSON map, so it always contains `"` and may
  contain spaces and backslashes, and `security -i` parses its stdin as command
  lines. Base64's alphabet passes that parser untouched, which removes the
  escaping question by construction rather than by careful quoting — and the
  read side already unwraps this exact envelope via `decode_go_keyring_payload`.
  Verified end to end by the probe's test 4: a 589-byte JSON map containing
  `"`, `\`, `$`, a backtick, `;`, `|`, `&` and spaces round-tripped
  byte-identical as an 806-byte item, **in both contexts** — from a terminal
  and from a launchd job with no controlling terminal. The realistic size is
  part of the result: it rules out a line-length limit in the interactive
  parser that a short sentinel would not have found.
- `NoStore` — the fallback, never the configured default. Push-only; nothing is
  persisted anywhere. Selected automatically when the platform cannot provide a
  keychain (§5a), or explicitly by config.
- `squad.envPersistence: "keychain" | "none"` on `SquadConfig`
  (`src/data/config/repo.rs:80`), **defaulting to `"keychain"`**, validated in
  `SquadConfig::validate` alongside the existing checks. `"none"` opts out
  entirely: no probe, no warning, no stored item — an explicit choice is never
  second-guessed and never nagged about.
- Three non-negotiable rules, because an unattended daemon must not be able to
  hang and must not lose data to a store it cannot reach:
  1. **Never block.** Every keychain call runs under a short timeout (5s). A
     locked login keychain, an absent Secret Service, or any prompt that does
     appear is a timeout that logs a warning and continues with whatever the
     daemon already has. It is never an error that fails a start or a run.
  2. **Never authoritative.** A pushed value always replaces a stored one. The
     store is a cold-start hint, not a source of truth.
  3. **Never fatal.** A store or load failure degrades this daemon to `NoStore`
     for its lifetime. Squad keeps working exactly as it does with persistence
     off — the only thing lost is surviving an OS-initiated restart.

#### 5a. Resolving the backend, and the fallback warning

- `DaemonEnvStore::resolve(&SquadConfig) -> (Box<dyn DaemonEnvStore>, Option<FallbackReason>)`
  runs **once at daemon startup**, before the scheduler, under the same 5s cap.
  It probes rather than assumes: a `load` attempt that returns a value, returns
  cleanly-empty, or reports "no such item" all count as available; a missing
  `security`/`secret-tool` binary, a timeout, a locked collection, or an
  unsupported platform do not.
- On fallback the daemon logs **one** `tracing::warn!` naming the reason and
  the consequence, at startup only:

  > `squad env persistence unavailable (secret-tool not found); continuing without it. Task env() values will be supplied by the next awman command and will not survive a daemon restart. Set squad.envPersistence to "none" to silence this.`

- **Warn once per daemon lifetime, not per write.** A daemon writes on every env
  push; warning per attempt would bury the daemon log, which is the same log
  `awman squad logs` prints and the same one a failed start tells users to read.
  The resolved backend is decided at startup and not re-probed, so there is
  exactly one line to find.
- A *later* store/load failure on a backend that probed available (the keychain
  became locked mid-session, say) logs at `warn!` the first time and `debug!`
  thereafter, then applies rule 3 and degrades for the rest of the lifetime.
- The degraded state is reported on `GET /v1/status` as
  `env_persistence: "keychain" | "none" | "unavailable(<reason>)"`, so
  `awman squad status` and the TUI tab can show it without anyone reading a
  log file. This is the discoverable surface; the log line is the detail.

#### 5b. Platform expectations

Fallback is the *expected* outcome in two of the four cases, not an error
condition, and the warning is worded so neither reads as a malfunction:

| Platform | Outcome |
|---|---|
| macOS | `KeychainStore` via `security -i` (measured; see above) |
| Linux with a Secret Service | `KeychainStore` via `secret-tool store` |
| Linux headless (no D-Bus/Secret Service) | fallback to `NoStore`, warned once |
| Windows | fallback to `NoStore`, warned once — no backend exists |

#### 5c. Removing a stored item

Persistence is now default-on, so there must be an obvious way to undo it:

- `awman clean` gains squad's daemon-env item to the set it removes, alongside
  the artefacts it already cleans (`src/command/commands/clean.rs:194`). This is
  the recommended route and the one the docs point at.
- Setting `squad.envPersistence: "none"` makes the daemon `clear()` any existing
  item on its next start, so opting out actually removes what opting in stored
  rather than merely stopping future writes.
- The manual escape hatch is documented for completeness:
  `security delete-generic-password -s awman-squad -a daemon-env` on macOS,
  `secret-tool clear service awman-squad account daemon-env` on Linux.

An earlier draft of this work item warned that keychain ACL trust binds to the
binary's identity, so reinstalling awman would invalidate it. **That does not
apply here** and the caveat has been removed: awman never touches the keychain
itself, it shells out to `/usr/bin/security`, so `security` is the trusted
application recorded on the item. It is Apple-signed and stable across awman
rebuilds. The probe confirms it — a launchd job wrote and read the item back
with no authorization prompt. Rule 1 still stands as the guard for the cases
that remain (a locked login keychain, an absent Secret Service).

### 6. UX for unmet variables

Everything above is mechanism. This is what a user actually sees, and it is the
part that decides whether the feature is worth having: an unmet variable that
only a protocol knows about is no better than today's silent drop.

The design principle is **one clear moment of truth, then a quiet standing
indicator** — a warning at the point of action, a durable marker anywhere the
task is displayed, and an explicit command for the whole picture. Nothing
repeats on a timer.

#### 6a. At the moment of action — `squad add` / `squad edit`

The gateway runs in the daemon, so it can answer authoritatively at create and
edit time. It checks each parsed `env(NAME)` against effective coverage (§4b)
and returns the unmet names; the CLI and TUI render one
`MessageLevel::Warning` `UserMessage` (`src/data/message.rs:18`) verbatim:

```
⚠ Task "nightly-triage" was created, but the squad daemon has no value for
  GITHUB_TOKEN.

  Its containers will start without it until it is supplied. To fix:
      export GITHUB_TOKEN=...      # in any shell
      awman squad env --push       # or just run any awman squad command

  Check state at any time with `awman squad env`.
```

The task **is created**. This is a warning, not a rejection: the variable may
legitimately arrive later, and refusing to create a task because the current
shell is under-equipped would be its own kind of silent failure — the user
would simply lose the task.

#### 6b. Standing indicator — `squad list`, the TUI card, `squad status`

- **`squad list`** appends a marker to the affected row: `⚠ env` after the task
  name, expanded in a footer line: `⚠ 1 task is missing an env value; see
  awman squad env`.
- **The TUI task card** gains one labelled row, in the existing labelled-record
  style the card already uses (`render_task_card`,
  `src/frontend/tui/render/squad.rs:317`):

  ```
  Env         ⚠ GITHUB_TOKEN unmet
  ```

  **It does not become a `CardStatus` variant.** That precedence table
  (`render/squad.rs:277`) answers "what is this task's run state", and it is
  documented and tested as such; an unmet variable is orthogonal — a *paused*
  task can have one too. Overloading the card colour would make two unrelated
  facts compete for one channel.
- **`squad status`** extends its existing one-liner
  (`src/frontend/cli/per_command/squad.rs:157`) with a clause only when
  non-zero, so the common case is unchanged:

  ```
  squad daemon running (PID 4213) at http://127.0.0.1:8919; 6 tasks (5 active);
  last tick 2026-09-08T14:02:11Z; 1 env value unmet
  ```

#### 6c. The app-level indicator — `squad ●` in the bottom row

The bottom-row indicator (`src/frontend/tui/squad_indicator.rs`) is the only
squad surface visible from *every* tab, including from a user who has never
opened the squad tab. An unmet variable is precisely the condition it exists to
advertise: persistent, actionable, and invisible until someone goes looking.

- `SquadIndicator` gains a seventh variant, `EnvUnmet` — "reachable, nothing
  failed, but at least one task is missing an env value" — painted
  `Color::Yellow` in `squad_indicator_color`
  (`src/frontend/tui/render/command_box.rs:293`). The module doc's "one of six
  states" becomes seven.
- **Precedence: `Failed` > `EnvUnmet` > `Running` > `Healthy`.** This extends
  the existing rule rather than inventing one. `classify` already documents
  "red beats blue: a failure needs attention and persists; a running task is
  transient" — an unmet variable is likewise persistent and actionable, so it
  beats blue for the same reason, and loses to red because a failed run is the
  more urgent fact (and is frequently *caused* by the unmet variable, which the
  user will find on arriving at the tab either way).
- Yellow is already `Unreachable`'s colour, and that is fine: both mean "needs
  a look, not broken", they are mutually exclusive in practice — an unreachable
  daemon cannot report coverage — and `Unreachable` outranks `EnvUnmet` anyway.
  Adding an eighth colour to distinguish two states that never co-occur would
  cost more than it explains.
- **The data rides on the task list the poller already fetches.** No second
  round trip: the list response carries `unmet_env: Vec<String>` per task,
  derived and never stored, exactly as `last_run_status` already is
  (`src/data/fs/task_store.rs:51` documents that precedent and its reason —
  the two are always read together, and one query per card would be an N+1
  across the daemon's HTTP surface). §6b's row marker and card row read the
  same field, so one addition serves all three surfaces.
- `classify` keeps its shape as a pure function over the probe result, so the
  new row is unit-tested with the rest of the state table and needs no daemon.

**Deliberately excluded: keychain-persistence fallback does not turn the
indicator yellow.** On headless Linux and on Windows that fallback is the
expected steady state (§5b), so wiring it here would pin the indicator yellow
forever on those platforms — and an indicator that is always yellow teaches
users to ignore it, which costs more than the warning gains. Persistence state
belongs in `squad status` and `awman squad env`, where it is sought
deliberately. The indicator is reserved for **per-task unmet variables**: a
condition that is always someone's to fix, and that goes away when they fix it.

#### 6d. The full picture — `awman squad env`

The whole feature needs one home, or its state is scattered across three
surfaces and inferable from none:

```
$ awman squad env
Daemon env coverage (persistence: keychain)

  NAME             STATE      SOURCE          SINCE
  GITHUB_TOKEN     ✓ set      this shell      —
  ANTHROPIC_KEY    ✓ set      keychain        —
  AWS_PROFILE      ⚠ unmet    —               3d ago
  NPM_TOKEN        ✓ set      pushed          —

  AWS_PROFILE is required by task "deploy-preview".
  Export it and run `awman squad env --push`.
```

- `SOURCE` distinguishes *this shell has it and the daemon agrees*, *the daemon
  holds it from a previous push*, and *it came back from the keychain at
  startup* — three states that look identical without this view, and that
  determine whether restarting the daemon will lose it.
- `SINCE` is `unmet_since` (§4b), so "just typo'd it" and "broken for three
  days" are distinguishable at a glance.
- `--push` forces a push; `--clear` removes the stored item (§5c). Values are
  never printed — only whether one is present.

#### 6e. After the fact — `squad show`

Unmet names are recorded on the run row at run start and listed in
`squad show <task>` and the TUI run detail, so a run that behaved oddly last
Tuesday can still be explained. This is the only place the information is
retained historically.

#### 6f. What is deliberately *not* done

- **No prompting.** Neither the CLI nor the TUI asks for a value interactively.
  Tasks are created non-interactively too, an unattended daemon can never
  prompt, and a prompt would put a secret in shell history the moment someone
  pasted it into the wrong place.
- **No repetition on a timer.** A task evaluating every 5 minutes produces one
  `warn!` per name, not one per tick (§4b).
- **No blocking of runs.** Covered in §4b with its rationale.

### Rejected alternatives

- **A `daemon.env` file (0600) written by the supervisor and read by the
  daemon.** Simple and portable, and it was the first design considered. Dropped
  because it puts plaintext secrets on disk in a home directory that gets swept
  into Time Machine, Dropbox, and `rsync` backups — and the push model needs no
  file at all.
- **The keychain as the *primary* transport.** It does not protect the point of
  greatest exposure (the long-lived daemon's memory, which any same-user process
  can read), and it cannot work on headless Linux or Windows. (The third
  objection in the first draft — that a macOS ACL prompt would block an
  unattended daemon — was measured and did not hold; see §5.) Kept as the
  default-on *persistence* layer, which is a different job from transport: a
  push is still the only way a value reaches a running daemon, and the keychain
  only carries it across an OS-initiated restart (§5).
- **Adding a `keyring`/`secret-service` crate.** Both link `Security.framework`
  or libsecret/D-Bus, against the single-statically-linked-binary constraint in
  `aspec/architecture/design.md:7`. The shell-out shims already in
  `keychain.rs` are the established answer.
- **Adding `env()` names to `FORWARDED_ENV`.** Keeps secrets in the plist, and
  computing the name union at spawn time would force the supervisor to read the
  task database, which `TaskStore` reserves to the daemon.
- **Threading a resolved env map from `collect_all_overlay_specs` down to
  `build_run_argv`.** Architecturally purer, but it touches call sites across
  three layers and solves nothing: resolution already happens inside the daemon,
  so the daemon must hold the values either way. Delivery is the problem.

## Edge Case Considerations:
- A daemon started with `--dangerously-skip-auth` has no bearer key; the push
  route is still behind `auth_middleware` and follows whatever that mode
  decides, so no new auth surface is introduced.
- Two clients pushing different values for the same name: last *provision*
  wins. An `absent` report never wins, so the order clients happen to run in
  cannot decide whether a task is armed (§4b).
- A client running from a shell with none of the required variables pushes an
  all-`absent` body. That must be a no-op on the overlay, not a wipe — the
  single most damaging way to get §4 wrong.
- A task whose `env(NAME)` was satisfied at creation and whose variable later
  disappears from every client: the value the daemon already holds stays and
  keeps working. It only goes when the last task naming it does.
- A variable rotated to a new value while an evaluation is mid-flight: a run
  snapshots the overlay at start, so a push landing mid-run cannot change the
  environment of a container that is already up.
- A name that no client has *ever* supplied is reported with `unmet_since` set
  at first requirement, not at first push, so a typo'd `env(GTIHUB_TOKEN)` reads
  as long-unmet rather than newly-unmet.
- `required_env` is computed from the task store, so it changes as tasks are
  added and removed; the cold-call round trip in §4 re-reads it on every
  supervisor connection rather than caching it.
- `systemd-run --setenv` with a value containing `=` or whitespace must survive
  intact; forwarded values are paths and mode strings, but the escaping still
  needs a test.
- A `KeychainStore` that times out on `load` at startup must not delay the
  daemon's `listen` — the load is best-effort and happens before the scheduler
  starts, under the same 5s cap.
- Headless Linux and Windows fall back on **every** daemon start. That is the
  expected steady state there, not a fault, so the warning must read as
  information and must not repeat within a lifetime; a user who does not want
  to see it at all sets `envPersistence: "none"`.
- A stored map outliving the tasks that justified it: the item is replaced
  wholesale on each push, so a removed task's variable disappears at the next
  push — but on a daemon that never gets another push, it persists until
  `awman clean` or an explicit opt-out. Documented, not silently mitigated.
- A machine where the keychain is available at daemon start and locked later:
  rule 3 degrades that daemon to `NoStore`; the values it already holds in
  memory are unaffected, so runs keep working and only restart-survival is lost.
- Two daemons sharing one storage root are already prevented by the existing
  mutual-exclusion guard, so a single fixed keychain item cannot be contended.
- `host_var` must fall back to `std::env::var` cleanly in every non-daemon
  process, so CLI and TUI behaviour is bit-for-bit unchanged.

## Test Considerations:
- Data: `FORWARDED_ENV` still excludes `AWMAN_API_KEY`/`AWMAN_SQUAD_KEY` and now
  includes `AWMAN_OVERLAYS`; `forwarded_env()` returns only the names actually
  set, in list order.
- Data: `render_launchd_plist` carries the extended list; a systemd argv builder
  test asserts one `--setenv` per forwarded name and correct escaping.
- Data: `host_var` prefers the overlay, falls back to the process env, and is
  overlay-free (identical to `std::env::var`) when `set_daemon_overlay` was
  never called; `DaemonEnvMap`'s `Debug` prints names and never values.
- Engine: `build_run_argv` emits `-e NAME` (not `NAME=VALUE`) for passthrough,
  still emits nothing for an unset name, and still emits `-e KEY=VALUE` for
  literals — extend `build_run_argv_env_passthrough_only_when_set`
  (`docker.rs:1694`).
- Engine: `dedup_credentials_by_declared_env` behaviour is unchanged when the
  covering value comes from the overlay rather than the process env.
- Frontend: `POST /v1/daemon/env` requires auth, accepts a map, reports accepted
  names only, and the response body contains no values; a rejected request logs
  no values either.
- Command: `ensure_running` pushes on both the spawn and already-running
  branches; the cold-call round trip sends nothing, learns `required_env`, then
  sends the intersection.
- Frontend: **an all-`absent` push leaves every existing value in place.** The
  highest-value test in this work item after the E2E one — it is the regression
  that would silently disarm every scheduled task, and it must fail against a
  whole-map-replace implementation.
- Frontend: a provided value replaces an existing one (rotation works); an
  `absent` for a name the daemon holds does not remove it; a name dropping out
  of `required_env` does remove it, from the overlay and the store alike.
- Frontend: an empty-string value is treated as `absent`, matching
  `options.rs:263`'s set-and-non-empty rule.
- Data: coverage is computed over overlay ∪ keychain-loaded values, so a name
  supplied only by the store at startup does not report as unmet.
- Data: `unmet_since` is stamped when the name first becomes required, not when
  a push first omits it.
- Frontend: an unmet name logs `warn!` once and `debug!` thereafter across many
  evaluation cycles; `squad status` lists it with its `unmet_since`.
- Frontend: `classify` gains rows for `EnvUnmet` in the existing state-table
  test — a task with `unmet_env` non-empty yields `EnvUnmet`; a *failed* task
  with unmet env still yields `Failed`; a *running* task with unmet env yields
  `EnvUnmet`, not `Running` (the precedence change, and the row most likely to
  be got backwards); an unreachable daemon yields `Unreachable` regardless.
- Frontend: `squad_indicator_color(EnvUnmet)` is `Color::Yellow`, and a
  persistence fallback with no unmet task variable leaves the indicator
  `Healthy` — the deliberate exclusion in §6c, asserted so nobody "fixes" it
  into a permanently yellow indicator on headless Linux.
- Frontend: the indicator's probe still makes exactly one `list` call per tick;
  `unmet_env` rides on that response rather than adding a round trip.
- Frontend: no response body from any endpoint contains a payload value —
  assert on the serialised JSON, not on the typed struct.
- Command: creating a task naming an unset variable returns the warning and
  still creates the task.
- Data: `NoStore` round-trips as `None`; a `load` that exceeds the timeout
  returns `None` rather than blocking (drive with an injected slow backend, not
  a real keychain — the shell-out shims stay untested in CI, as they are today).
- Data: `DaemonEnvStore::resolve` returns `KeychainStore` with no
  `FallbackReason` for a config that omits `envPersistence` — the default is
  keychain, and a test asserts that rather than trusting `Default`.
- Data: `resolve` probes rather than assumes — an available backend holding no
  item yet ("no such item") resolves to `KeychainStore`, not to a fallback.
- Data: an absent backend resolves to `NoStore` **with** a `FallbackReason`
  naming it; `envPersistence: "none"` resolves to `NoStore` with **no** reason,
  so the explicit opt-out never produces a warning.
- Frontend: the fallback warning is emitted exactly once per daemon lifetime.
  Drive many pushes against a failing store and assert one `warn!` and the rest
  at `debug!` — the daemon log is what `awman squad logs` prints, and burying it
  would defeat §6's whole purpose.
- Frontend: `GET /v1/status` reports `env_persistence` as `keychain`, `none`,
  and `unavailable(<reason>)` in the three cases.
- Data: a store that probed available and then fails mid-session degrades to
  `NoStore` for the rest of the lifetime and never retries.
- Command: switching `envPersistence` to `"none"` clears an existing item on the
  next daemon start, so opting out removes what opting in stored.
- Data: the stdin text handed to `security -i` is asserted directly — a pure
  string-building function, testable with no keychain. It must be one
  `add-generic-password` line whose `-w` value is the `go-keyring-base64:`
  envelope, containing no quote, space or backslash from the payload, for a map
  whose values deliberately include all three. Round-trip it through
  `decode_go_keyring_payload` to prove the read side unwraps what the write
  side wrapped.
- Data: no code path ever places a payload value in a `Command` argument.
  A grep-level guard in `tools/architecture-lint.sh` is the cheapest form:
  `add-generic-password` may appear only in the `security -i` stdin builder.
- E2E: a squad task with `env(VAR)` run against a live daemon receives the value
  in its container when the launching process had it exported. This is the
  regression test the whole work item exists for; it must fail against `main`.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- §2 overlaps `aspec/work-items/completed/0114-architecture-audit-medium-and-low.md:288`
  (F-47.2), which already calls for `publish_key_to_process_env`'s
  `std::env::set_var` to be replaced by passing the key through
  `SquadServeConfig`. Land whichever comes first and make the other conform; the
  end state is no `set_var` in Layer 2 at all.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to
reflect the current state of the tool:

- `docs/12-squad.md` — how the daemon gets its environment, why a variable
  exported after the daemon started is picked up on the next awman command, what
  the "daemon has no value for `NAME`" warning means and how to clear it, the
  new `awman squad env` command, and the `squad.envPersistence` setting — that
  it defaults to keychain storage, what the fallback warning means, and how to
  remove a stored item. The indicator table in that file gains the yellow
  `EnvUnmet` row, and the squad-tab section notes the card's `Env` line.
- `docs/08-overlays.md` — a note in the `env(VAR_NAME)` section that for squad
  tasks the value is resolved by the daemon, not by the shell that created the
  task.
- `docs/07-configuration.md` — the `squad.envPersistence` key, its `"keychain"`
  default, and the `"none"` opt-out.
