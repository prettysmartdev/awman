# Testing

The manifest test system is the only supported path. See [replacement-ledger.md](replacement-ledger.md) for coverage decisions and evidence. The release path and its residual risks are in [release.md](release.md).

How awman's tests are organised, isolated, selected and run. The contract itself
(suites, tiers, capabilities, gates, quarantine) lives in `ci/gates.toml`; this
document explains it and is checked against it by `cargo xtask gates check`.

<!-- gates:begin contract -->
Generated from `ci/gates.toml` (schema 2).

| Gate | Budget (minutes) | Required status |
| --- | --- | --- |
| main | 20 | gate |
| native-diagnostic | 120 | operator diagnostic |
| nightly | 60 | gate |
| nightly-artifacts | 45 | gate |
| nightly-kit | 10 | gate |
| pr | 10 | gate |
| release | 15 | gate |

| Tier | Per-test deadline (seconds) | Guarantees |
| --- | --- | --- |
| artifact | 300 | tempdir, loopback, subprocess, release-artifact |
| boundary | 120 | tempdir, loopback, subprocess, awman-binary, fake-cli, local-git |
| component | 20 | tempdir, loopback, subprocess, awman-binary, fake-cli, local-git |
| guest | 300 | tempdir, loopback, subprocess, awman-binary, hypervisor, builtin-payload, guest-fixtures |
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
| `unit` | pure contract modules and doctests | pure functions and data; files in a private temp directory only | never: no sockets, no processes |
| `component` | `tests/component/` (package `awman-cli`, target `component`) | commands, engine, daemon, squad, worker and frontends against fakes; runs both the builtin and the daemon backend | never |
| `boundary` | `tests/boundary/` (target `boundary`) | the real Docker Engine, registry, Git transports, Apple Containers and `sbx`, to prove the fakes tell the truth | only those named in `AWMAN_TEST_HOST_RESOURCES` |
| `guest` | `tests/guest/` (target `guest`, feature `guest-drivers`) | real builtin microVM guests: boot, exec, import, network, upgrade, builder, strategy | hypervisor, verified payload, pinned fixtures |
| `artifact` | `tests/artifact/` (target `artifact`) | the exact signed release executables, run as a user would, with no access to library code | the executable under test |
| `soak` | `tests/boundary/soak/`, guest soak | hosted latency, fuzz and registry runs nightly; long guest runs only in the operator-run native diagnostic | as the suite declares |

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
| `sbx-test-identity` | a dedicated Docker Sandboxes identity | the host resource plus `AWMAN_TEST_SBX_HOME`, `AWMAN_TEST_SBX_IDENTITY`, `AWMAN_TEST_SBX_ANTHROPIC_KEY_FILE` |
| `hypervisor` | Hypervisor.framework or KVM | `hypervisor` |
| `builtin-payload` | the verified builtin payload | `builtin-payload=<dir>` |
| `guest-fixtures` | pinned guest OCI and Docker-save archives | `AWMAN_TEST_FIXTURES` |
| `previous-release` | the pinned previous release executable | `AWMAN_TEST_FIXTURES` |
| `release-artifact` | a release executable with a known digest | `AWMAN_TEST_ARTIFACT` and `AWMAN_TEST_ARTIFACT_SHA256` |
| `offline` | enforced offline network | `offline` |
| `public-network` | public registry access | `public-network` |
| `registry-credentials` | private registry credentials | `registry-credentials=<file>` |

`previous-release` is required by published-predecessor upgrade suites, rather
than every guest test. The first Linux builtin release runs separate cached
executable-replacement and offline artifact suites. Once a reviewed Linux
predecessor pin exists, native validation additionally requires the published
upgrade suites; missing or invalid predecessor bytes then block the run.

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

**Artifact-tier service grants.** The artifact tier allows `docker` and
`apple-containers` for exactly one suite each, `artifact-docker-smoke` and
`artifact-apple-smoke`; the manifest policy rejects either capability on any
other artifact suite. A release native kit grants only what its suite requires
(`docker` or `apple-containers`, no hypervisor or offline grant); the pinned
guest fixtures are validated kit inputs. See
[Required release checks](#required-release-checks).

**The dedicated SBX identity.** The full provider boundary suite and the
provider artifact smoke require the `sbx-test-identity` capability, never a
developer's own login. The developer diagnostic `make validate-sbx` and the
[release SBX smoke](#required-release-checks) instead use the host's existing
logged-in session and run no provider tasks. The full provider suites need three operator inputs: `AWMAN_TEST_SBX_HOME` (an owned, 0700
directory used as `HOME`), `AWMAN_TEST_SBX_IDENTITY` (a non-secret account
label) and `AWMAN_TEST_SBX_ANTHROPIC_KEY_FILE` (an owned, 0600 file). Granting
it sets only `HOME` for the child; `AWMAN_HOME`, XDG and Git configuration stay
private, and the key is redacted from any debug output. An optional
`AWMAN_TEST_SBX_PROVIDER_KEYS_DIR` (a 0700 directory of 0600 files named for the
variable, such as `OPENAI_API_KEY` or `GH_TOKEN`) lets the boundary suite
qualify the Codex and Copilot routes; Cline and Antigravity need a credential
approved in the identity. A route without its credential is BLOCKED.

**BLOCKED versus FAILED.** A missing capability, a host that is not Apple
silicon macOS, an `sbx` older than 0.54.0 (boundary) or 0.53.0 (provider
artifact smoke),
a signed-out identity or an unprovisioned provider credential reports
`BLOCKED[...]` and is never a pass. A wrong product behavior, a failed or
skipped test, or a surviving owned sandbox is FAILED. Both the `boundary-sbx`
suite (`tests/boundary/sbx/`) and the opt-in `artifact-sbx-smoke` suite
(`tests/artifact/sbx_smoke/`) follow this rule. The latter's
`native.macos-arm64-sbx` declaration is optional and excluded from the normal
release plan and every hosted gate. Evidence
files record the CLI version and identity label, never a key.

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
ConPTY execution is validated by the hosted Windows component gate.

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
On Linux, when procfs is available, TCP ownership includes the original socket
inode: an unrelated test reusing a closed address is not a surviving resource.
Without that identity evidence the checker conservatively probes the address.

Release database handles and other file owners before calling `finish()`. For
ordinary scoped tests, declare the sandbox first and let its checked `Drop` run
after the remaining locals; Unix allowing an open file to be unlinked is not
proof that the same cleanup is correct on Windows.

Compile platform-specific component contracts only on the platforms they test
with `#[cfg(...)]`. Keep their applicable platforms covered by the manifest;
an ignored test in a selected suite cannot satisfy the gate. Split independent
protocol or release scenarios into separately scheduled cases when their combined
runtime exceeds the tier deadline. Preserve the assertions and deadline.

### Ports, waiting and time

- `sandbox.ports().tcp_listener()` binds `127.0.0.1:0`; `.unix_socket(name)`
  returns a path in a short socket directory (at most 103 bytes, the macOS
  limit); `.track(addr)` registers an address a child reported.
- `wait_for(what, limit, || Option<T>)`, `wait_for_with(…, diagnostics)` and
  `wait_for_async(what, limit, || async { Option<T> })` poll a condition with a
  deadline. Prefer `Ready`, `wait_for_stdout` and
  `FakeCli::wait_until_held/wait_for_calls`.
- No other sleeping. direct `sleep` calls in test code are rejected by H3.

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
`AWMAN_TEST_EVIDENCE_DIR`, `AWMAN_TEST_HOST_RESOURCES`, the SBX identity inputs below, plus `INSTA_UPDATE` and
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

`cargo xtask lint hygiene` checks direct Rust resource calls (H1–H4, H7, H9 and H10) and structural assertion control flow (H5). The support crate implements the reviewed low-level primitives. H6/H8 tier placement, fixed-port intent and macro/alias indirection also require code review; syntax checks are not a proof of isolation or semantic coverage. The manifest capability boundary enforces resource grants at execution time.

| Rule | Forbids |
|---|---|
| H1 | `env::set_var`, `env::remove_var`, `set_current_dir` in test code |
| H2 | `Command::new` in test code outside `awman-test-support` |
| H3 | `sleep` in test code |
| H4 | fixed ports and unowned listeners |
| H5 | tests that can pass without asserting: early return before the first assertion, return after a skip message, assertions only under an `if`/`if let Ok/Some` with no failing alternative, an error discarded with `.ok()`, `#[ignore]` without a `not-applicable:` reason |
| H6 | sockets, processes, `TestSandbox::command` or `require::<…>` in tests assigned to the unit tier |
| H7 | reading the environment in test code outside `runtime_env`; unreviewed `AWMAN_TEST_*` names |
| H8 | `ManualClock` outside test code; paused Tokio time outside the unit tier |
| H9 | `use awman` in `tests/artifact/`; new users of `tests/helpers` or the `test-support` feature |
| H10 | passing an agent command directly to `TestSandbox::command` |

A reviewed direct-resource exception is a function doc comment:
`/// hygiene-allow(H<n>): <concrete ownership or inspection reason>`.
This is used for the runner sweep fault injection and the leased Unix listener double. Build metadata (`CARGO_MANIFEST_DIR`) may be read to locate committed tools. Do not suppress a real ownership defect.

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
| `tests/artifact/entrypoints/`, `deps_scan/`, `signed_boot/`, `apple_smoke/`, `docker_smoke/`, `sbx_smoke/`, `upgrade_policy/`, `smoke/`, `switches/`, `measure/` | the directory name | `artifact-*` |
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

`ci/acceptance-matrix.toml` records important behavior anchors with named tests and gates. Platform, feature, runtime and complete suite selection come from `ci/gates.toml`; do not duplicate that Cartesian product by hand or demand one test per catalogue permutation. `cargo xtask matrix check` rejects missing behavior slices and nonexistent named tests. `covered` means executable coverage exists, not that it passed on a platform. Only actual run evidence establishes execution. Native evidence comes from the release command's checks on real hosts or the native diagnostic.

The manifest runner accepts `--test-threads N` (a positive integer) to limit
nextest parallelism without changing suite deadlines, retries or selection.

## Execution and evidence

Every selected suite must contain tests. The runner captures compiled inventory, executes with zero retries, clears stale JUnit, and records source hashes and actual results. Missing tests, required skips and changed inputs fail validation. Component contracts in private library modules are selected explicitly by the manifest; placement alone never grants unit-tier privileges.

Tests own their private HOME, Git configuration, processes and sockets through TestSandbox. The runner watches the real Git configuration for changes and checks for run-owned process leaks on Linux. It does not watch the operator's live `.awman` tree: an ordinary session may update its heartbeat or disk while tests run. Child environment and capability checks remain the test escape boundary. An unexpected write to the operator's `.awman` by a test child is no longer caught by the runner's filesystem canary; restore that detection only with ownership attribution, not a snapshot of unrelated user activity. This is test resource ownership, not a sandbox for hostile native code; never run an untrusted PR on a privileged host.

`--shuffle-seed` changes suite invocation order. It does not randomize tests inside a nextest invocation. Record repeats, time and failures accurately; a single clean run is not p95 or a long-term flake-rate measurement.

The Actions controller authenticates service run, attempt, job roles and artifact digests, downloads evidence independently, and compares selected compiled tests with passing JUnit. Native results, SBX smoke records and diagnostic evidence directories are operator evidence and cannot authorize CI publication. The release checks below are separate from the hosted gates.

## Native validation

Required release checks, broad native diagnostics and lightweight experimental
SBX smoke have different purposes.

### Required release checks

`make release` runs four native checks and the selected SBX smokes on the
attested release bytes, on operator machines. The native checks use a prebuilt,
digest-bound kit produced by the hosted `release.yml` build (a standalone
`xtask`, the pinned nextest, the compiled artifact test target and its fake
companion, and a compiled inventory):

| Check | Suite | Host and asset | Grants, fixture |
|---|---|---|---|
| `native.macos-arm64` | `artifact-signed-boot` | release Mac, `awman-macos-arm64`: signed offline boot and persisted-state upgrade | `hypervisor`, `offline`; `guest-oci-base` and the previous release |
| `native.macos-arm64-apple` | `artifact-apple-smoke` | release Mac, `awman-macos-arm64` | `apple-containers`; `guest-oci-base` |
| `native.linux-x64` | `artifact-linux-boot` | Linux x86_64 over SSH, `awman-linux-amd64`: offline KVM boot | `hypervisor`, `offline`; `guest-oci-base-amd64` |
| `native.linux-x64-docker` | `artifact-docker-smoke` | Linux x86_64 over SSH, `awman-linux-amd64` | `docker`; `guest-oci-base-amd64` |

`artifact-apple-smoke` and `artifact-docker-smoke` each hold one release-critical,
non-overridable test,
`exact_asset_runs_a_synthetic_session_in_an_owned_container` (module
`apple_smoke` on macOS arm64, `docker_smoke` on Linux), with a 420-second suite
budget inside a 480-second kit budget. Like `artifact-sbx-smoke`, they appear in
no gate cell. The test loads the pinned guest fixture locally under a reserved
tag (no registry pull), runs `awman ready --prepare-only --json` and one
synthetic `exec prompt` session through the exact asset in private state,
requires the guest output and exit 37, audits the running container's mounts and
removes and verifies its objects. The kit reserves those names before the run,
reconciles them independently afterwards (`cargo xtask test kit-reconcile`), and
voids a result that leaked an object or cannot verify absence. Without its
capability the test reports `BLOCKED[...]`, never a pass or skip.

Nothing is compiled, signed or rebuilt on the native hosts, and the host has no
Cargo or checkout. The runner verifies the kit's digests, runs with private
HOME and Git configuration, deadlines and an exact expected inventory, enforces
offline operation for the two boot checks (seccomp on Linux, the macOS sandbox
on the Mac), and writes `result.json`, JUnit and a runner log. The release
command accepts a result only when the raw inventory, JUnit, log, command exit,
environment contract, fixture and predecessor pins, source, tree, asset and kit
digests all reconcile with the attested kit; the Apple and Docker results also
need their bound service identity and ownership, session and cleanup documents.

The SBX smoke is not a kit, a test suite or a capability. It is declared in
`[release.sbx_smoke.*]` in `ci/gates.toml` (`sbx.macos-arm64` always,
`sbx.linux-x64` only for an explicit `LINUX_HOST`) and implemented in
`xtask/src/sbx_validation/release.rs`, sharing the shell-sandbox lifecycle with
`make validate-sbx` and adding `awman ready --prepare-only` through the exact
asset. It needs `sbx` 0.54.0 or newer. It proves no provider authentication,
effective agent policy or successful provider task. The provider diagnostic
`artifact-sbx-smoke` stays optional (`required = false`) and cannot satisfy it.

A missing, short, skipped, blocked or failed result of any selected check
prevents sealing; there is no waiver. A pending fixture pin, missing capability,
unavailable service or signed-out CLI is `BLOCKED`, never a pass. Results are
operator evidence, not Actions attestations. See [release.md](release.md).

Test coverage of the release logic itself is hermetic: manifest policy, kit
capability and cleanup handling, result verification and the SBX smoke (with
fake `sbx`, `ssh` and `awman` executables, in suite `unit-xtask`), plus the
fault-injected `make release-rehearse`. None of it is evidence that a real Apple
Containers service, Docker Engine or SBX login works; only a run on the real
hosts is.

On macOS the runner allocates a short private root under `/private/tmp` and
checks the canonical guest socket path against the 103-byte limit before
starting guest tests; a long ambient `TMPDIR` does not change this.

### Broad native diagnostic

```sh
make validate-native
```

runs the registered `native-diagnostic` gate on the host being diagnosed (an
Apple Silicon Mac or a Linux KVM machine). It is not part of a release and has
no remote mode: run it on the host itself, from a clean checkout. It installs
the pinned runner, fetches and verifies the builtin payload, previous
executable and architecture-specific fixtures, compiles the native suites,
captures their expected tests, signs the guest executables on macOS and runs the
guest, boundary, soak and artifact breadth (see the `native-diagnostic` gate in
[cicd.md](cicd.md)). It never runs an agent on the host. This broad diagnostic excludes the SBX
platform override; use `make validate-sbx` for Docker Sandboxes. It does not
run the release Apple, Docker or SBX smokes.

It writes an evidence directory under `target/native-validation/`. Check one
with `cargo xtask native-validation verify DIR` from a checkout of its exact
commit: this checks source inputs, every file digest, complete selection,
successful command records and equality between the independently enumerated
tests and passing JUnit. Missing, duplicated, skipped, blocked, failed or
substituted results fail verification, and so does evidence for another
revision. A missing historical Linux predecessor is reported as blocked, never
as a passed upgrade: only the suites that need it are blocked, every
independent registered suite still runs, and the diagnostic as a whole ends
BLOCKED (exit 2; a failed suite takes precedence with exit 1).

The diagnostic is the only route to per-agent guest behavior,
builder and strategy matrices, guest soak, Apple and Linux Docker/registry/Git
boundaries and Linux replacement coverage. Nothing schedules it. A single run
measures execution time; it does **not** establish p95 latency or long-term
flake rate, cannot authorize release publication and does not satisfy the
release command's separate provenance requirements.

SBX cleanup uses strict complete listings: unknown, ambiguous or partial rows
fail verification and retain ownership. A selection whose nonpassing tests
are all BLOCKED returns exit 2; a failure, skip, cleanup leak or unverifiable
cleanup returns exit 1. SBX boundary policy tests provide deeper live coverage;
arithmetic output alone does not qualify policy.
They are not a prerequisite for experimental macOS enablement: a passing
`make validate-sbx` is sufficient validation for that decision.

### Docker Sandboxes developer validation

```sh
make validate-sbx
```

Run from the checkout on an Apple silicon Mac with sbx 0.54 or newer already
logged in. No API keys, environment exports, separate identity, credential
bindings or other pre-setup are required. The target reuses the normal SBX
login and configuration. It does not install or run an agent on the host.

To also validate a remote Linux host, use the same destination variable as
release:

```sh
make validate-sbx LINUX_HOST=user@host
```

An exported `LINUX_HOST` works too. SSH must already connect non-interactively
to that account, with sbx installed on its PATH and logged in there. No remote
checkout, Rust toolchain or provider keys are needed. The command runs both
the local Mac and remote Linux smoke, creates its own remote scratch workspace,
and retains the Linux receipt and SSH log in the report's `linux/` directory.
SSH agent forwarding is disabled. Remote failures fail the whole validation;
an absent or unverifiable Linux result never counts as Linux evidence.

This is a lightweight experimental-runtime diagnostic under grand architecture
Tenet 4. The command itself is not a release gate; `make release` runs its own
[release SBX smoke](#required-release-checks), which shares this lifecycle, and
never consumes a diagnostic report. The diagnostic checks the CLI version and
current session, creates a randomly named disposable shell sandbox, inspects it,
checks Linux guest execution and workspace sharing through both exec and run,
stops it, verifies exec can restart it, then removes it and verifies absence.
Only its private scratch workspace is shared. Existing sandboxes, credentials
and login configuration are not modified. Cleanup is attempted on failure too.

The target prints a result and saves summary.txt, proof.json and command logs
under target/sbx-validation/run-*. Override the report parent with
SBX_VALIDATION_DIR if desired. It verifies its report automatically; there is
no separate verification step to perform. For later offline integrity checks,
`cargo xtask sbx-validation verify DIR` remains available.

The console prints the selected hosts and report path immediately, followed
by each host's step start and pass/fail messages. Long-running commands print
elapsed-time updates every ten seconds. Linux step messages are streamed live
over SSH; full command output is retained in the logs. Without `LINUX_HOST`,
only the local Mac smoke runs and no SSH connection is attempted. When it is
provided, the Mac smoke runs first, followed by the Linux smoke; either failure
fails the command.

Missing host/CLI/sign-in prerequisites report BLOCKED; a failed guest check,
unverifiable cleanup or surviving sandbox reports FAILED. Both make the Make
recipe fail. Failed and blocked reports are retained for diagnosis. Success
means every selected smoke step passed and cleanup was verified.

This proof intentionally covers less than the provider boundary suite: no paid
model tasks, per-agent authentication, credential-store isolation or effective
agent-policy qualification are claimed. Those deeper suites keep their existing
refusals and requirements and remain separately opt-in. A successful
`make validate-sbx` is sufficient validation to enable experimental SBX on
the tested platforms: Apple silicon macOS, plus the remote Linux architecture
when `LINUX_HOST` was set and its smoke passed. No further provider tests,
image-pinning exercise, credential provisioning or manual qualification are prerequisites for that
enablement. The accepted Mac/Linux packet has enabled experimental SBX in
shipping builds for macOS arm64 and Linux x86_64. The developer command makes
no automatic source edits. A cheap exact-asset entrypoint
test verifies normal SBX agent admission without test isolation or provider
credentials; deeper SBX provider smoke remains opt-in.
