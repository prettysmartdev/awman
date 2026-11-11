# Testing

WI 0144 remains an incomplete shadow migration. Static checks passing do not
certify a working production evidence chain or complete legacy replacement;
see [replacement-ledger.md](replacement-ledger.md) for code blockers and cutover.

How awman's tests are organised, isolated, selected and run. The contract itself
(suites, tiers, capabilities, gates, quarantine) lives in `ci/gates.toml`; this
document explains it and is checked against it by `cargo xtask gates check`.

<!-- gates:begin contract -->
Generated from `ci/gates.toml` (schema 1).

| Gate | Budget (minutes) | Required status |
| --- | --- | --- |
| candidate | 20 | gate |
| main | 20 | gate |
| nightly | 60 | gate |
| pr | 10 | gate |
| promote | 20 | gate |

| Tier | Per-test deadline (seconds) | Guarantees |
| --- | --- | --- |
| artifact | 300 | tempdir, loopback, subprocess, release-artifact |
| boundary | 120 | tempdir, loopback, subprocess, awman-binary, fake-cli, local-git |
| component | 20 | tempdir, loopback, subprocess, awman-binary, fake-cli, local-git |
| guest | 300 | tempdir, loopback, subprocess, awman-binary, hypervisor, builtin-payload, guest-fixtures, previous-release |
| soak | suite-specific | tempdir, loopback, subprocess, awman-binary |
| unit | 1 | tempdir |

Unit, lint, doctest and parity suites always run. Unknown/shared paths select the full PR set. Main and merge-group run the full set. Required skips, missing evidence and expired quarantine fail the gate.
<!-- gates:end contract -->

## Tiers

A tier is defined by what a test is **guaranteed** to have and what it **may**
request. A test never discovers its environment and skips; it asks for a
capability and either gets it or the run is `blocked`.

| Tier | Binary / location | What it is for | Real engines, guests or network |
|---|---|---|---|
| `unit` | inline `#[cfg(test)]` modules and doctests in every crate | pure functions and data; files in a private temp directory only | never: no sockets, no processes |
| `component` | `tests/component/` (package `awman-cli`, target `component`) | commands, engine, daemon, squad, worker and frontends against fakes; runs both the builtin and the daemon backend | never |
| `boundary` | `tests/boundary/` (target `boundary`) | the real Docker Engine, registry, Git transports, Apple Containers and `sbx`, to prove the fakes tell the truth | only those named in `AWMAN_TEST_HOST_RESOURCES` |
| `guest` | `tests/guest/` (target `guest`, feature `guest-drivers`) | real builtin microVM guests: boot, exec, import, network, upgrade, builder, strategy | hypervisor, verified payload, pinned fixtures |
| `artifact` | `tests/artifact/` (target `artifact`) | the exact signed release executables, run as a user would, with no access to library code | the executable under test |
| `soak` | `tests/boundary/soak/`, guest soak | latency, fuzz, public/private registry, long guest runs; nightly only | as the suite declares |

`unit` and `component` are what `make test` runs. `boundary`, `guest` and
`artifact` need a named host resource or a release executable and run on the
matching CI platform (see [cicd.md](cicd.md)).

## Capabilities

The capability ids are defined in `ci/gates.toml` (`[capabilities.*]`) and
implemented as probes in `crates/awman-test-support/src/capability.rs`.

| Capability | Meaning | Real resource named by |
|---|---|---|
| `tempdir` | private temporary directory | always granted |
| `loopback` | private loopback TCP and Unix sockets | always granted |
| `subprocess` | owned, supervised child processes | always granted |
| `awman-binary` | the immutable debug `awman` built for this run | the build |
| `fake-cli` | an owned fake command-line executable | the build |
| `local-git` | Git in private repositories with a sanitised config | the host `git` |
| `docker` | a private Docker Engine | `docker=<DOCKER_HOST>` |
| `local-registry` | a loopback OCI registry | `local-registry=<ip:port>` |
| `git-transports` | loopback SSH and HTTP Git servers | `git-transports` |
| `apple-containers` | the Apple Containers service | `apple-containers` |
| `docker-sbx` | the Docker sandbox CLI | `docker-sbx` |
| `hypervisor` | Hypervisor.framework or KVM | `hypervisor` |
| `builtin-payload` | the verified builtin payload | `builtin-payload=<dir>` |
| `guest-fixtures` | the pinned guest OCI archive | `AWMAN_TEST_FIXTURES` |
| `previous-release` | the pinned previous release executable | `AWMAN_TEST_FIXTURES` |
| `release-artifact` | a release executable with a known digest | `AWMAN_TEST_ARTIFACT` and `AWMAN_TEST_ARTIFACT_SHA256` |
| `offline` | enforced offline network | `offline` |
| `public-network` | public registry access | `public-network` |
| `registry-credentials` | private registry credentials | `registry-credentials=<file>` |

`sandbox.require::<Capability>()` returns a `Granted<C>` or panics:

- `MISPLACED[<id>]`: the tier neither guarantees nor allows that capability. The
  test is in the wrong tier. Fix the test, never the manifest.
- `BLOCKED[<id>]: <reason>`: the capability is allowed but absent (no Docker
  daemon, no hypervisor, fixture not pinned). The run records the cell as
  `blocked`, which fails the gate. A capability is **never** a reason to skip.

A test whose capability disappears while it runs (Docker restarted, VM lost) is
classified `blocked`, not `failed` and not `passed`: on failure the support
crate re-probes the capabilities the test held and records losses in
`capabilities_lost`.

Real host resources are used only when named. `AWMAN_TEST_HOST_RESOURCES` is a
comma-separated list: `docker=unix:///var/run/docker.sock,git-transports`.
Unset means none, so a bare `cargo test` can never touch a real engine, boot a
guest or reach the network.

## The support crate: `awman-test-support`

Every test in the `component`, `boundary`, `guest` and `artifact` tiers gets its
world from `crates/awman-test-support`. It does not depend on `awman`; tests
import both.

```rust
use awman_test_support::{TestSandbox, TestCase, Tier, OwnedCommand, OwnedChild, Ready, Exit, Stdin,
    EnvMap, Ports, Leak, wait_for, wait_for_with, wait_for_async};
use awman_test_support::capability::*;
use awman_test_support::fixtures::{RepoBuilder, ConfigTree, WorkItemBuilder, WorkflowBuilder,
    OciImageBuilder, CredentialFiles};
use awman_test_support::fakes::{FakeCli, FakeRegistry, FakeWorkServer, FakeGitHttp};
use awman_test_support::{conformance, golden::assert_golden, evidence};
```

### Sandbox

In a tier binary, always create the sandbox through that tier's glue so the right
executables and tier are registered:

```rust
let sandbox = support::sandbox("engine::git::clone_over_ssh");
```

(`support::soak_sandbox` exists in the boundary tier.) Directly, use
`TestSandbox::for_test(TestCase::component(id))` (also `boundary`, `guest`,
`artifact`, `soak`). `TestSandbox::new()` is **unit tier**: files only; calling
`command()`, `pty_command()` or `ports()` on it panics with `MISPLACED[…]`.

A sandbox owns a private directory tree: `root() home() xdg_config() xdg_data()
awman_home() tmp() bin() work() sock_dir()`. `env()` / `env_mut()` is the
**complete** child environment: `HOME`, `XDG_*`, `AWMAN_HOME=$HOME/.awman`,
`TMPDIR`, a `PATH` of `<sandbox>/bin` plus system directories,
`AWMAN_TEST_ISOLATION=1`, a Git config with no hooks, helpers or signing,
`GIT_TERMINAL_PROMPT=0`, `TZ=UTC` and `LC_ALL=C.UTF-8`. Nothing is inherited from
the developer's shell, so no real keychain, clipboard, daemon, home directory or
Git setup is ever reachable.

Other sandbox methods: `register_exe`, `allow_tool`, `install_exe`,
`install_fake`, `fixture(id)`, `note(key, value)`, `redact(secret)`,
`on_finish(name, check)`, `finish()` (also run on drop) and
`support::env_snapshot(&sandbox)` for the library's `EnvSnapshot`.

### Processes

`sandbox.command(program)` builds a supervised child: `.arg/.args/.env/.envs/
.env_remove/.current_dir/.stdin(Stdin::…)/.label/.deadline/.ready(Ready::…)/
.max_output`, then `.spawn()` (returns after the readiness condition) or `.run()`
(returns `Output { exit, stdout, stderr }`). The default deadline is the tier's
per-test budget. `Ready` is `Immediately`, `StdoutLine(prefix)`,
`StderrLine(prefix)`, `FileExists(path)`, `TcpAccepts(addr)` or
`UnixAccepts(path)`. `OwnedChild` offers `wait`, `interrupt`, `terminate`,
`kill_and_reap`, `wait_for_stdout` and friends; `sandbox.pty_command(p)` gives a
`PtyChild` for terminal-driven tests. On Unix, `PtyChild::wait(limit)` also
drains terminal output to EOF within that limit. Reading `output_so_far` after
wait includes the final transcript bytes; a descendant holding the terminal open
fails the wait instead of producing a truncated successful transcript. Windows
ConPTY completion/containment still needs P9 evidence.

Program names resolve through the sandbox `PATH` only. These are refused with a
`REFUSED:` panic: agent executables (`claude`, `codex`, `gemini`, `opencode`, …,
also as interpreter arguments), keychain, clipboard and privilege tools
(`security`, `secret-tool`, `pbcopy`, `xclip`, `wl-copy`, `sudo`, …), executables
inside the sandbox that were not installed with `install_fake`/`install_exe`, and
anything outside the system tool directories that was not registered. A fake
installed under an agent's name is allowed: agents are only ever *faked* in
tests, never run on the host.

At the end of a test every owned process, port, socket and directory is checked.
A survivor, an address still accepting connections, or a socket file left in
`sock_dir` is reported as a `LEAK` and fails the test.

### Ports, waiting and time

- `sandbox.ports().tcp_listener()` binds `127.0.0.1:0`; `.unix_socket(name)`
  returns a path in a short socket directory (at most 103 bytes, the macOS
  limit); `.track(addr)` registers an address a child reported.
- `wait_for(what, limit, || Option<T>)`, `wait_for_with(…, diagnostics)` and
  `wait_for_async(what, limit, || async { Option<T> })` poll a condition with a
  deadline. Prefer `Ready`, `wait_for_stdout` and
  `FakeCli::wait_until_held/wait_for_calls`.
- No other sleeping. `sleep` in test code violates the target H3 policy; implementing enforcement is B3.

### Fakes and conformance

- `FakeCli`: `fake.on(&["image","inspect"]).stdout(..).stderr(..).exit(n)
  .hold_until_released().touch(path).read_stdin().spawn_child();` The longest
  argument prefix wins, later rules win ties. `calls()` returns every invocation
  (`argv`, `env`, `cwd`, `stdin`, `exit`), plus `wait_for_calls`,
  `wait_until_held` and `release`. `fakes::docker_cli_baseline(&fake)` installs
  rules that satisfy `conformance::docker_cli`.
- `FakeRegistry::start(&sandbox)`, `FakeGitHttp::start(&sandbox, &dir)` and
  `FakeWorkServer::start(&sandbox).await`. **Await `FakeWorkServer::shutdown()`
  before the test ends**, or its port is reported as a leak.
- `conformance::{docker_cli, registry, work_server}` run the same contract against
  a fake in the `component` tier and against the real service in the `boundary`
  tier, and `ConformanceReport::assert_conforms()` fails on any difference. A fake
  with no conformance check behind it is not trusted.

### Fixtures

- `RepoBuilder::new(&sandbox, "name").file(path, content).branch("main").init()`
  makes a Git repository with a fixed author and dates, so commit SHAs are
  identical on every run. The returned `Repo` has `git(&[..])`, `write`,
  `commit`, `head`, `create_branch`, `checkout`, `worktree`, `bare_remote`, `push`.
- `ConfigTree::new().global(json).repo(json).write(&sandbox, Some(repo))`.
- `WorkItemBuilder`, `WorkflowBuilder`, `OciImageBuilder` (layers → manifest,
  config, index; `write_layout`, `write_archive`) and `CredentialFiles` (fake
  Claude, Codex and `gh` credential files whose secrets are `test-secret-…` and
  are redacted from failure output).
- `sandbox.fixture(id)` returns a file from the content-addressed fixture store,
  verified against `[fixtures.<id>]` `sha256` in `ci/gates.toml`. A fixture whose
  pin is still `pending` produces `BLOCKED[fixture:<id>]`.

### Evidence and goldens

A failing or blocked test with a sandbox writes `records/<id>.json` to
`$AWMAN_TEST_EVIDENCE_DIR/records/`, or to `<sandbox>/evidence/records/` with the
sandbox kept and its path printed. The record holds the test id, tier, seed,
target, toolchain, fixtures, processes with their output files, capabilities
held and lost, leaks, the panic message with its location and a `reproduce`
command. Call `evidence::note("seed", …)` or `sandbox.note("vm", …)` to add
facts a reader will need.

`assert_golden(path, actual)` compares against a checked-in file. It rewrites
the file only when `INSTA_UPDATE=always` is set and never when `CI` is set.

### Process variables

Test code reads the environment only through `awman_test_support::runtime_env`
(`AWMAN_TEST_TIER`, `AWMAN_TEST_FIXTURES`, `AWMAN_TEST_ARTIFACT`,
`AWMAN_TEST_ARTIFACT_SHA256`, `AWMAN_TEST_RUN_CONFIG`, `AWMAN_TEST_TMPROOT`,
`AWMAN_TEST_EVIDENCE_DIR`, `AWMAN_TEST_HOST_RESOURCES`, plus `INSTA_UPDATE` and
`CI`). These are the `[test_env]` table in `ci/gates.toml`. The only other
`AWMAN_TEST_*` names allowed anywhere are the `[[shipped_switch]]` entries
(`AWMAN_TEST_ISOLATION`, `AWMAN_TEST_DOCKER`, `AWMAN_TEST_APPLE_CONTAINER`,
`AWMAN_TEST_SBX`, `AWMAN_TEST_BUILTIN`), which exist in the shipped binary,
can only *reduce* what awman may do, and are each proven by a test in the
artifact tier. `AWMAN_TEST_DOCKER` and friends are no longer test-selection
switches: whether a test runs is decided by the tier and the capability it
requires.

## Fixture rules

1. **Everything a test touches is created inside its sandbox.** No test reads or
   writes the real `HOME`, `XDG_*`, `AWMAN_HOME`, keychain, clipboard, Git
   config, launchd or `systemd --user`.
2. **No process-global state.** No `env::set_var`/`remove_var` and no
   `set_current_dir` (hygiene H1); pass an environment (`EnvSnapshot`,
   `env_mut`) or a directory explicitly. That is what allows tests to run in
   parallel, and nextest runs each test in its own process on top of it.
3. **Processes and ports are owned.** Spawn through `TestSandbox::command` (H2),
   bind through `Ports` (H4); no fixed ports. Nothing outlives the test.
4. **No sleeping, no polling by hand.** Use `Ready` and `wait_for` (H3).
5. **No agent runs on the host.** Agents are fakes, or run in a real container or
   guest in the boundary and guest tiers (H10).
6. **Time is injected.** `ManualClock` and paused Tokio time are for the unit
   tier only (H8).
7. **Determinism.** Fixed Git identities and dates; seeds recorded with
   `evidence::note`; `TZ=UTC`, `LC_ALL=C.UTF-8`.
8. **Fixtures are pinned.** External inputs (guest OCI archive, payloads,
   previous release) are addressed by SHA-256 in `ci/gates.toml` and fetched with
   `cargo xtask fixtures fetch`. There is no unpinned fallback.
9. **Real resources are named by the caller**, never assumed
   (`AWMAN_TEST_HOST_RESOURCES`).

`cargo xtask lint hygiene` currently enforces structural H5 in test bodies,
file-local helper calls and explicitly named cross-module assertion helpers.
`make architecture-lint` and the `lint-hygiene` suite invoke that check. The table
below is the required migration contract; H1–H4 and H6–H10 are not yet implemented
(B3 in replacement-ledger.md). A green command does not certify those rules.

| Rule | Forbids |
|---|---|
| H1 | `env::set_var`, `env::remove_var`, `set_current_dir` in test code |
| H2 | `Command::new` in test code outside `awman-test-support` |
| H3 | `sleep` in test code |
| H4 | fixed ports and unowned listeners |
| H5 | tests that can pass without asserting: early return before the first assertion, return after a skip message, assertions only under an `if`/`if let Ok/Some` with no failing alternative, an error discarded with `.ok()`, `#[ignore]` without a `not-applicable:` reason |
| H6 | sockets, processes, `TestSandbox::command` or `require::<…>` in inline unit tests |
| H7 | reading the environment in test code outside `runtime_env`; unreviewed `AWMAN_TEST_*` names |
| H8 | `ManualClock` outside test code; paused Tokio time outside the unit tier |
| H9 | `use awman` in `tests/artifact/`; new users of `tests/helpers` or the `test-support` feature |
| H10 | passing an agent command directly to `TestSandbox::command` |

An exception is a comment on the line above:
`// hygiene-allow(H<n>): <reason>`. Exceptions are counted in the lint output.
There is no shared allow-list.

## Naming

A test's id is `<package>::<target> <module path>::<test name>`, for example
`awman-cli::component engine::git::clone_over_ssh`. Suites select tests by the
**first module segment**, so the file's location decides which suite runs it:

| Where | First segment | Suite |
|---|---|---|
| `tests/component/data/` | `data::` | `component-data` |
| `tests/component/engine/` | `engine::` | `component-engine` |
| `tests/component/command/` | `command::`, `command::builtin::`, `command::daemon::`, `command::differential::` | `component-command*` |
| `tests/component/frontend/`, `daemon/`, `squad/`, `worker/`, `security/`, `tools/` | the directory name | `component-<name>` |
| `tests/component/parity/` | `parity::builtin::`, `parity::daemon::`, `parity::differential::` | `component-parity-*` |
| `tests/boundary/docker/`, `registry/`, `git/`, `apple/`, `sbx/`, `conformance/`, `templates/`, `soak/` | the directory name | `boundary-*`, `soak-*` |
| `tests/guest/boot/`, `exec/`, `import/`, `network/`, `upgrade/`, `builder/`, `strategy/`, `soak/` | the directory name | `guest-*` |
| `tests/artifact/entrypoints/`, `deps_scan/`, `signed_boot/`, `upgrade_policy/`, `smoke/`, `switches/`, `measure/` | the directory name | `artifact-*` |
| inline `#[cfg(test)]` in a crate | the crate's module path | `unit-<crate>` |

Test function names describe the behaviour and its outcome
(`sandbox_refusal_precedes_task_allocation_and_every_runtime_call`), not the
function under test. A test that must pass for a release is registered by id in a
`[[release_critical]]` row of `ci/gates.toml`; renaming it without updating the
row fails `cargo xtask gates check`.

## How to add a test

1. **Pick the lowest tier that can prove it.** Pure logic → inline unit test next
   to the code. A command, engine or frontend behaviour against fakes →
   `component`. A claim about a real engine or service → `boundary` (and a
   conformance check if you are adding a fake). A real guest → `guest`. A claim
   about the shipped executable → `artifact`.
2. **Put it where the suite filter finds it** (table above) in an existing module,
   or add a module and declare it in that tier's `main.rs`. A test file that is
   not reachable from a Cargo target is reported by `gates check` as an
   unregistered test file.
3. **Build the world from the sandbox.** `let sandbox = support::sandbox("<module
   path>::<test name>");`, fixtures from `RepoBuilder`/`ConfigTree`, fakes from
   `FakeCli` and friends, and `sandbox.require::<…>()` for anything real.
4. **Assert something that can fail.** Do not return early, do not wrap
   assertions in `if let`, do not swallow errors. Missing prerequisites are the
   sandbox's job: they panic `BLOCKED[…]`.
5. **Run it.**
   - `cargo nextest run -p awman-cli --test component -E 'test(=engine::git::my_test)'`, or
   - `cargo xtask test run --suite component-engine`.
6. **Run the lints.** `make check` (lints plus the unit tier of the crates you
   changed) then `make test`. If you added a new suite, tier, capability, test
   file location or fixture, edit `ci/gates.toml`, run `cargo xtask gates render`
   and `cargo xtask gates check`; the workflows, `.config/nextest.toml` and the
   documentation blocks are generated from it.
7. **Golden files.** `cargo xtask test update-golden --suite <suite>` reruns the
   suite with `INSTA_UPDATE=always` and rewrites them (`assert_golden` refuses to
   rewrite when `CI` is set); review the diff like source. The command
   reference is one: `make docs-reference` regenerates
   `docs/14-command-reference.md`.

Property tests use `proptest` and snapshot tests use `insta` (pinned in
[localdev.md](localdev.md)). Record the seed of a property test with
`evidence::note("seed", …)` so a failure is reproducible.

## Reproducing a CI failure

Every failed or blocked cell uploads a `failure-<job>-m<index>` artifact. The
`gate` job's summary links each one and prints the cell's `reproduce` line.

1. Download the artifact: `gh run download <run id> -n failure-<job>-m<index>`.
2. Open `records/<id>.json`. It names the test, tier, platform, toolchain, seed,
   fixtures and capabilities, lists each process with its stdout and stderr files
   (copied next to the record), and gives a `reproduce` command, normally:
   `cargo nextest run -p awman-cli --test component -E 'test(=engine::git::clone_over_ssh)'`.
3. Check out the commit named in the run, with the toolchain in `rust-toolchain.toml`.
4. Install the pinned runner once: `cargo xtask test install-runner`.
5. Run the `reproduce` command. For a `boundary`, `guest` or `artifact` test also
   provide what the record's `capabilities` lists: `AWMAN_TEST_HOST_RESOURCES`
   for boundary, `cargo xtask fixtures fetch --tier guest` and a hypervisor for
   guest, `AWMAN_TEST_ARTIFACT` (plus `AWMAN_TEST_ARTIFACT_SHA256`) for artifact.
6. For a flaky test, replay the shuffle seed from the record: `cargo xtask test
   run --suite <suite> --shuffle-seed <N>`.
7. `BLOCKED[…]` means the environment, not the product: fix the runner or the
   missing pin and rerun the job. `LEAK:` lines name the process, port or socket
   the test left behind.

A whole cell can be replayed from a CI build with `cargo xtask test run --archive
<file> --shard <i/N>` against the downloaded `test-archive-<build>` artifact.

## Quarantine and overrides

A flaky or temporarily broken test is quarantined in `ci/gates.toml` with a
reviewed entry:

```toml
[[quarantine]]
test = "awman-cli::component engine::git::example"
owner = "@handle"
issue = "https://github.com/<owner>/<repo>/issues/NNN"
expires = "2026-12-31"
```

- A quarantined test still runs and its result is still recorded; quarantine does
  not hide it from the evidence.
- Every entry needs an owner, an issue and an expiry date. An expired entry fails
  planning, aggregation and promotion; it is never extended silently.
- Quarantine cannot make a **required** case green: release-critical scenarios
  (guest boot, upgrade from the previous release, strategy matrix, signed offline
  session), artifact integrity and required hardware cells stay non-green while
  quarantined, and have no override path.
- An `[[override]]` (gate, cell, reason, expiry, approver) is the only way to
  accept a non-green cell, is limited to cells not marked non-overridable, and
  expires like a quarantine. Evidence expiry for a release is the earliest of its
  parts and of any quarantine or override deadline.
- `#[ignore]` is not quarantine. The only permitted form is
  `#[ignore = "not-applicable: <reason>"]` for a case that cannot apply on a
  platform, and the manifest must agree. Any other ignore is an "unexplained
  skip" and fails the gate.
- The nightly flake hunt reruns every tier five times with recorded shuffle seeds
  and opens a `flaky-test` issue with the failure record for any intermittent
  failure. Fix or quarantine within the issue's deadline; retries are zero for
  tests.

## Acceptance matrix

`ci/acceptance-matrix.toml` has one row per subsystem × mode × backend × runtime
× platform × feature cell: `covered` (with the exact test ids and the gate that
certifies them), `not-applicable` (with a reason) or `blocked` (with owner and
release impact). `cargo xtask matrix render` writes
[acceptance-matrix.md](acceptance-matrix.md) and `cargo xtask matrix check` fails
if a named test does not exist or a required row is missing. `covered` names
tests; it is not a claim that they passed on that platform, which is what the
gate's evidence shows.

## Legacy targets

Until their replacements have shadow evidence, a number of older integration
targets remain, each listed as a `[[legacy_fallback]]` row in `ci/gates.toml`
with its removal condition (see [replacement-ledger.md](replacement-ledger.md)).
`make legacy-test` runs them through `tools/isolated-test.sh`. New tests never go
in these targets and never use `tests/helpers`.

## WI 0144 review status and current runner behavior

The successor is still in shadow operation. `gates check` verifies the static
contract; it does not certify a passing GitHub run. The CLI evidence aggregator
still refuses unauthenticated transport, and producer emission/controller
integration must be completed before requiring `gate`. The legacy coverage stays
registered while semantic replacement and platform evidence are missing.

`test run` executes each selected suite separately with nextest's
`--no-tests fail`. A nonempty neighboring suite cannot hide an empty filter.
Resource-using retained inline awman tests run in the component tier; the unit
budget remains one second. The generated model suite has its own 120-second
budget for its 64 persisted state-machine traces.

The runner gives retained inline harnesses a private HOME, AWMAN_HOME, XDG and
temp root, retains the Cargo/Rustup installation locations, checks a before/after
HOME canary, and removes its owned temporary tree. The canary reports changed
paths; unrelated applications writing that HOME concurrently can also make it
fail. Use a dedicated canary HOME when testing the runner itself. Direct
`cargo test` still includes retained inline tests that have not all migrated to
`TestSandbox`; the run-level protection is provided by `cargo xtask test run`.

`--shuffle-seed` records and permutes suite invocation order. Nextest runs each
test in its own process; it does not shuffle the tests within an invocation.
The stable libtest fallback also does not shuffle individual tests. These seeds
must not be reported as proof of randomized intra-suite ordering.

Self-hosted jobs initialize a private attempt lease with `ci sweep --init`.
`sweep` refuses a missing or mismatched lease, scans only that attempt's roots,
and never deletes ambient Docker containers by name or processes by workspace
substring. Docker/VM containment and reclamation after a killed job still need
native admission/rehearsal evidence. Linux fallback cleanup recognizes inherited
private roots and validates PID creation time; a hostile descendant that erases
its environment requires an enforced cgroup. Linux supervised commands refuse
`setsid`; this restriction and environment checks are not an OS security sandbox.
macOS containment remains pending P3/P9. Labels and cleared environments are not
host isolation.

The structural hygiene implementation currently covers H5 with branch and
polling-helper handling. H1–H4 and H6–H10 are still implementation obligations;
the support crate's narrower clippy configuration does not enforce those rules
across the whole workspace. Do not use a passing hygiene command to claim that
all ten rules or all historical replacements are complete.


Workflow lint parses all seven YAML workflows, including retained fallbacks. It
checks reviewed write scopes, forbids pull_request_target, requires protected-main
and environment/admission guards on hardware routing, and rejects dependency
edges that consume neither artifacts, outputs nor verdicts unless they have a
named ordering rationale. Artifact names distinguish called gates. CI runs each
lint suite once; the retained Tests status shim is removed only at P2.

The documentation check also reads current aspec/devops and docs Markdown outside
the generated block: executable Make references and explicitly named jobs/suites
must exist. Versioned release notes preserve historical command names. The driver
clears ambient credentials and daemon overrides while retaining the pinned tool
PATH and declared build/test inputs. These protections do not migrate every
retained inline test or complete the missing hygiene rules.
