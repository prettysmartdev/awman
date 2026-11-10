# Work Item: Task

Title: Replace the entire Rust test suite, CI/CD, and release process
Issue: issuelink

Depends on: 0143 (completed daemon block).

## Summary:

- **This is a total replacement, not an improvement pass.** Every Rust test, test helper, fixture, harness, test-selection convention, Make test target, shell check, Actions workflow, runner arrangement, prerelease script, and release script in this repository is in scope and is replaced. Nothing is kept because it already exists. An existing test, script, or job survives only by being re-derived from a stated product contract and re-implemented on the new foundations; the work item closes when the replacement ledger (section 9) shows zero legacy entries.
- **Three deliverables, one evidence model.** (1) A new Rust test suite designed from a behavior and risk inventory. (2) A new CI graph generated from a single gate manifest. (3) A build-once, validate, promote release pipeline. All three report results in the same machine-readable evidence format, bound to commit, inputs, and artifact digests, so "green" always means "these named contracts ran against these exact bytes".
- **Why now.** The Apple Silicon prerelease takes 40+ minutes on a high-end Mac Studio (reported during v0.13) and the v0.13 release took a day of trial and error. The history shows recurring hangs, platform failures, false-green skips, cache staleness, and process races. The audit in this document also found coverage that never runs in CI, tests that cannot be selected without substring conventions, a release path that can skip tests interactively, and a published Mac binary that is not the one that was tested.
- **Safety requirements that carry over unchanged.** Untrusted code never reaches a privileged Mac; a required hardware check cannot skip; the signed Apple Silicon executable that ships must be the one that booted a guest; old-to-new state compatibility is proven; publication requires every declared gate. No agent or code assistant runs on a host in any test tier (`aspec/architecture/security.md`).
- **The releasing developer's experience does not change.** `make release VERSION=vX.Y.Z [RELEASE_RUNNER=auto|local|none]` remains the one command that takes a checkout to a published release, with the same steps, prompts, defaults, re-run behavior, and final state as today's `scripts/release.sh`. Everything that script does is retained (section 5.3 is the binding contract); what changes is the pipeline behind it.
- **Speed is an outcome, not a lever.** Targets: required PR path under 10 minutes p95, Apple Silicon candidate validation under 20 minutes p95 warm. These are met by building once, sharing artifacts, and removing duplication, never by narrowing coverage.

## User Stories

### User Story 1:
As a: maintainer

I want to:
replace the whole Rust test suite with a documented, behavior-driven architecture covering every shipped subsystem, mode, runtime, and platform

So I can:
trust that a green result means the product contracts were exercised, and extend the suite without copying brittle legacy tests.

### User Story 2:
As a: developer

I want to:
run one command locally and get the same verdict CI will give, in minutes, with a direct link from a failure to its evidence and a one-line reproduction

So I can:
fix defects without waiting on several full builds or rerunning unrelated suites.

### User Story 3:
As a: release operator

I want to:
keep running `make release VERSION=vX.Y.Z` exactly as I do today, and have it build each target once, validate those exact artifacts, and publish the same bytes

So I can:
ship without repeated prerelease runs, runner cleanup, manual job cancellation, or post-tag surprises.

### User Story 4:
As a: maintainer

I want to:
see queue, build, fixture, test, and release timings, flake rates, and a clear split between product failures and infrastructure failures

So I can:
improve the critical path and stop flakes from becoming routine.

### User Story 5:
As a: user installing awman

I want to:
verify that the binary I downloaded is the one the project built and tested

So I can:
trust a release without trusting the download channel.

## Scope

**In scope (all replaced):** `tests/` (27 registered targets, 2 example drivers, `tests/helpers/`, `tests/fixtures/`), `crates/**/tests/`, every inline `#[cfg(test)]` module in `src/` and `crates/`, doctests, feature- and platform-gated tests, `[[test]]`/`[[example]]` registration, the `test-support` feature, `[profile.test]`, every `make test*`/`pre-push`/`release`/`apple-silicon-prerelease`/`native-builtin-ci`/`payloads-verify` target, every script in `tools/` and `scripts/` that tests, gates, measures, signs, or releases, `.github/workflows/test.yml` and `release.yml`, cache and artifact usage, runner registration, branch and tag protection, `aspec/devops/cicd.md`, `aspec/devops/localdev.md`, and the release operator and install-verification documentation.

**Production code changes allowed:** only what tests need to observe or control behavior through a supported seam: injected clock, environment snapshot, process spawner, port/socket allocation, and filesystem roots. Each seam follows the layer rules of `aspec/architecture/2026-grand-architecture.md`. Test-only switches honored by the shipped binary are inventoried and constrained (section 3.5).

**Out of scope:** new product features; restructuring production crates beyond testability seams; Linux builtin runtime release (WI 0124), though the new matrix must make adding it a manifest change rather than a redesign.

## Evidence and current-state diagnosis

Figures are counts taken from the tree at `e4cae907` with `grep`/`wc`; they size the work and are not a substitute for the phase-0 inventory.

### Size of what is being replaced

| Item | Current state |
|---|---|
| Test annotations | About 6,250 `#[test]`/`#[tokio::test]`: 5,117 in `src/`, 48 in `crates/`, 1,085 in `tests/`. 382 source files carry a `#[cfg(test)]` module. |
| Test code volume | About 57,000 lines under `tests/`; 1,700 lines of shared helpers in `tests/helpers/`. |
| Integration targets | 27 hand-registered `[[test]]` entries in `crates/awman-cli/Cargo.toml` with `autotests = false`, plus 2 `[[example]]` guest drivers. Each links the whole library; with `builtin-runtime` each links the VM stack. |
| Test-control variables | About 90 distinct `AWMAN_TEST_*` names across source, tests, tools, and workflows. Gating, fixture paths, credentials, fault injection, and child re-exec markers share one unstructured namespace. |
| Timing dependence | About 330 `sleep(` call sites (103 under `tests/`), 17 paused-clock uses. A latency benchmark with a 5 ms p95 assertion runs inside `make test`. |
| Process-global mutation | 92 `env::set_var`/`remove_var` call sites; about 55 static locks or serialisation guards exist to cope. |
| Skips | About 146 early-return skip prints in Rust tests; only 3 `#[ignore]`. A skipped test reports as passed to libtest. |
| Shell tooling | 4,750 lines across `tools/*.sh`, `scripts/release.sh`, and the two workflows. |

### Failure classes

| Failure class | Repository evidence | Redesign requirement |
|---|---|---|
| Coverage that never runs in CI | `make test` runs the suite with `AWMAN_TEST_BACKEND=builtin` and then reruns the TUI, `binary_smoke`, and `cli_parity` suites with `AWMAN_TEST_BACKEND=daemon`. CI runs `make test-fast` and `make test-full`, which set neither; no workflow sets `AWMAN_TEST_BACKEND`. The daemon-backend passes run only on a developer machine or inside `scripts/release.sh`. Seven tracked checks have no Make or Actions caller: `test-build-previous-builtin-awman.sh`, `test-kernel-callback-smoke.sh`, `test-msb-isolated-builder.sh`, `test-release-provider-metadata.sh`, `test-release-upgrade-policy.sh`, `test-review-build-inputs.sh`, `test-static-krunfw.sh`. Windows PR coverage is one build plus one named unit test. | Every test and check has a tier and a gate in the manifest, or is deleted. Builtin/daemon parity is a matrix dimension of the required PR gate. |
| Silent skips and name-based selection | `test-fast` selects by `--skip real_git --skip real_network --skip builtin_hw --skip latency::`. `tests/helpers/mod.rs` still documents a `--skip docker` filter the Makefile no longer has. Real-store and corpus tests skip unless `AWMAN_TEST_REQUIRE_EXTERNAL=1`, which no ordinary job sets. `autotests = false` means a new file under `tests/` is never built unless someone registers it. | Tier membership is structural. A test declares the capabilities it requires; the tier declares the capabilities it guarantees; a mismatch fails. Skips are reported as skips with a reason. |
| Shared state and unsafe process lifecycle | Re-exec fork bomb (`2110d050`), API tests reaching another test's server (`585680c3`), fork-inherited locks (`e6a2d429`, `e6322996`, `f5b724fc`), executable-busy launches (`39fe0e05`, `72537ed5`, `d5043695`), tests emptying `PATH` (`8b4d5273`), VM-owner collisions (`4aaef99f`). `localdev.md` records that 4× CPU test threads "surfaced fork/exec races in existing fixtures". | Per-test owned HOME, temp root, ports, process group, images, and VM namespace. No process-global mutation. Bounded, reaped subprocess trees. |
| Timing, scheduling, hangs | Hanging tests removed in `776e56d6`, `58a1dadb`, `e1f959c0`; slow-runner failures (`204cea73`); paused Tokio time against real sockets (`4d3ba70f`); stream and lease stalls (`3e8d7e6c`); subscriber attached after sender exposed, with a test relying on a 300 ms delay (`8cf45de1`). No hosted CI job declares `timeout-minutes`. | Readiness and completion signals, virtual time only for virtual resources, per-test wall-clock budgets with forced termination, job-level timeouts everywhere. |
| Platform differences | Windows, Linux builtin, and Docker repairs (`e28d35a1`); libcap-ng native build (`02fe8c41`); macOS/Linux fixes (`9f63d915`); macOS credential-source isolation (`32a8e59a`). Isolation is implemented in a bash wrapper (`tools/isolated-test.sh`), so a plain `cargo test` is not isolated and Windows has no first-class path. Runner images are mixed (`ubuntu-26.04`, `ubuntu-24.04`, `macos-latest`, `windows-2025`, `windows-latest`). | Isolation lives in Rust test support and works identically on every OS and under a bare `cargo test`. One pinned image per platform in the manifest. |
| Build fanout and cache correctness | `[profile.test]` disables debug info and incremental builds because of link volume. Cache keys hash only `Cargo.lock`, with a prefix `restore-keys` fallback; a stale cache had to be busted by hand (`f462fa2e`). `fast` and `build-macos` both compile and run the hermetic suite; `full-linux-docker` recompiles and reruns it with Docker on. Native clean checks discard caches by design. | Build test executables once per platform and feature set, share them as an archive, key caches on every build input, write caches only from trusted refs. |
| CI graph, runner capacity, policy drift | `full-linux-docker`, `build-macos`, and `static-workspace-build` wait on `fast`. No `concurrency` group, so superseded PR runs keep running. CI hardware jobs and the release job share the `hypervisor` label; `scripts/release.sh` hunts for queued `Tests` runs to cancel, and `test.yml` jobs fail themselves when they land on a release runner. `prerelease-ci-gate.sh` matches jobs by display name. `cicd.md` says `pre-push` equals the `fast` job (it runs `make test`, which is a different selection), still describes a removed SQLite gate, and says Windows is built only at release. | One generated job graph, stable gate IDs, dedicated release capacity, PR supersession, documentation checked against configuration. |
| Release integrity and recovery | The local test sentinel `.git/.release-tests-passed-<version>` is keyed by version only, so it survives source changes, and choosing "Skip tests and continue the release anyway?" writes the same sentinel as a pass. Because `release.yml` runs no test suite, that local run (or its skip) is the only suite execution a release gets. The script pushes a lightweight final tag before any release build runs, so a failed build leaves a public tag with no release. The Mac binary validated by `make apple-silicon-prerelease` is discarded; `release.yml` rebuilds, re-signs, and publishes different bytes. Windows and Linux assets get no functional smoke beyond `--version` (Windows: none). `measure-release-artifact.sh` records measurements with no thresholds. Only `vX.Y.Z` tags trigger anything, so there is no candidate stage. No provenance attestation. | Evidence bound to commit, inputs, and digests. Exact distributed bytes tested. Remote final tag created last, by automation. The operator's local-test skip stays available but is recorded as a skip and can never stand in for the pipeline's own gates. Deterministic resume and rollback. |

### v0.13 case study (November 3–5)

- November 3: a speed and flake pass (`9d4fec2b`) was followed by fixes for inherited `PATH`, fork-held private-file locks, PTY close behavior, and cross-test API server contact (`8b4d5273`, `e6a2d429`, `72dff6de`, `585680c3`).
- November 4: release preparation (`146510ec`, `edd9dc9a`), a new prerelease checker (`4a2853d7`), then `824aeda9` replaced waiting on the whole workflow with a hand-maintained job-name list after hosted macOS allocation stalled. The same day needed fixes for a fork-inherited build lock (twice), a late event subscriber, executable-busy races, a fixture missing from the Apple-store gate, daemon-helper readiness, and VMs visible across tests (`e6322996`, `f5b724fc`, `8cf45de1`, `72537ed5`, `f23e75a4`, `4aaef99f`), with two more at 19:15 and 20:27 (`d5043695`, `9eac9cda`). `d0a29ec2` removed a gate that the documentation still describes.
- Every one of these is a symptom of four causes: process-global state, uncontrolled subprocess inheritance, time-based synchronization, and gate membership kept by hand in several places. The redesign removes the causes; it does not add more per-symptom guards.
- Limits of this evidence: commit messages do not prove which CI run failed or how long it took, and `gh` was not authenticated during this review. The 40+ minute figure is user-reported. Phase 0 collects the run logs and timings.

## Implementation Details:

### 1. Target design at a glance

```
            ci/gates.toml  (single source of truth: tiers, suites, platforms, gates)
                  │  validated + rendered by `cargo xtask gates`
     ┌────────────┼──────────────────────────────┐
     ▼            ▼                              ▼
 local `make`   ci.yml (PR / merge queue / main)   candidate.yml ──► promote.yml
 targets        nightly.yml                        (build once)      (same bytes)
     │            │                              │
     └────────────┴──────── evidence.json per gate ┘
                  (commit, input digest, artifact digests, expected/ran/passed/
                   failed/skipped/blocked counts, durations, reproduction command)
```

- **Tiers** describe what a test is allowed to touch. **Gates** describe when a set of tiers must pass. Both are defined once in the manifest.
- **`xtask`** is a small, non-shipped workspace member that owns manifest validation, workflow rendering, evidence aggregation, and release orchestration. Orchestration logic moves out of bash into tested Rust; shell remains only as thin wrappers around platform tools (`codesign`, `otool`, `ldd`).
- **Evidence** is the only thing a gate trusts. No sentinel files, no job display names, no operator memory.

### 2. Phase 0: baseline, inventory, and gate contract

No new suite code lands before these exist and are reviewed.

- **Inventory.** Enumerate every Rust test (by `--list` on every target, feature set, and platform), every inline test module, doctest, shell test, payload/image verification, guest scenario, upgrade check, build smoke, artifact check, measurement, and release step. For each record: contract protected, layer, current caller (or "none"), required environment, side effects, duration, and historical failures. Commit it as `aspec/devops/test-inventory.md` (or generated data next to it).
- **Failure history.** Classify every available Actions failure and rerun by class, OS, runtime, and gate. Capture v0.13 in full, including queue times and each rerun.
- **Timing baseline.** A reproducible Apple Silicon trace with per-step timing (compile, link, cache restore, fixture setup, previous-version build, VM boot, test run, signing, upload), cold and warm, plus p50/p95 for the PR path over enough runs to separate the two. Publish as CI artifacts.
- **Tooling spikes with exit criteria.** (a) `cargo-nextest` as the runner: per-test process isolation, timeouts, test groups for VM concurrency, archive/reuse, JUnit output, on all three OSes including the guest tier. (b) Test-archive reuse across jobs. (c) Candidate storage and same-bytes promotion on GitHub Releases. Each spike ends in a written adopt/reject decision with measurements. The design below assumes adoption; a rejection must name the replacement that still meets the same requirement.
- **Gate manifest v1.** Write `ci/gates.toml` (section 4.1) describing the new target state, then validate the existing pipeline against it to produce the initial gap list.
- **Service levels.** Confirm or restate the PR and candidate targets from the baseline, with separate cold-run and full-suite budgets. If a target is infeasible, record the limiting cost and a staged numeric plan. Coverage is never reduced to meet a number.

### 3. Rewrite the Rust test suite

#### 3.1 Rules of the rewrite

- Start from the **behavior and risk inventory**, not from existing files. For every existing test record a disposition: `rewritten`, `replaced-by-stronger` (contract, property, or scenario test), `merged`, or `retired` with a written reason. Existing names, helpers, fixtures, assertion style, and layout are not compatibility requirements.
- No old test is kept because it predates the rewrite. No old test is dropped because it is slow or flaky; its protected behavior moves to the correct tier.
- Tests assert externally observable behavior and stable layer contracts. Internal helpers get focused unit tests only where they own a decision. Tests that assert incidental timing or implementation order are not carried forward.
- Every regression behind a commit cited in this document gets a named test in the new suite, tagged with the commit it guards.

#### 3.2 Layout and build shape

- **`crates/awman-test-support`** (dev-only, `publish = false`) replaces `tests/helpers/` and the `test-support` feature. It owns the sandbox, builders, fakes, capability checks, evidence emission, and the process supervisor. Its public API is documented and has its own tests.
- **Few integration binaries, grouped by tier and dependency weight**, replace the 27 targets: `component`, `boundary`, `guest`, and `artifact`, plus guest driver executables. Only `guest` and `artifact` link the VM stack. Modules inside each binary are organised by layer and domain (`data::config`, `engine::git`, `command::workflow`, `frontend::cli`, …). Fewer link steps is the largest single compile-time saving available without touching production crates.
- **Discovery is automatic or verified.** Either restore Cargo autodiscovery, or keep explicit registration and have `cargo xtask gates check` fail when a `.rs` file under a test root is not reachable from a registered target. A test file that is never compiled must be impossible.
- **Inline unit tests** stay next to the code they test and are tier `unit` by definition. A lint (clippy `disallowed-methods` in `clippy.toml` plus an `architecture-lint` rule) forbids them from spawning processes, opening sockets, sleeping on the wall clock, or mutating the process environment or current directory.
- **Doctests** are either compiled and run in the `unit` tier or marked `no_run`/`ignore` with a reason; the manifest records which.

#### 3.3 Tiers

| Tier | May touch | Typical content | Budget per test |
|---|---|---|---|
| `unit` | Memory, injected clock/env, a private temp dir | Parsing, validation, precedence, state machines, pure command logic, rendering to a buffer | 1 s |
| `component` | Loopback sockets, owned subprocesses (the `awman` binary, fake runtime CLIs), local `git` on temp repositories, in-process daemon | Layer 2 commands through builtin and daemon backends, CLI/TUI/API parity, protocol, streams, cancellation, cleanup | 20 s |
| `boundary` | A real Docker Engine, local registry, Git over real transports to local servers, real Apple Containers/sandbox CLIs where the runner provides them | One bounded proof per real boundary plus the shared runtime conformance suite | 120 s |
| `guest` | A real VM on native hardware (Hypervisor.framework now; KVM when WI 0124 lands) | Boot, exec, image import, networking, upgrade from previous state, builder scenarios | 300 s |
| `artifact` | The exact release executable under a cleared environment, offline | Version/info entry points, dependency and helper-extraction scans, signed-binary boot, upgrade policy, platform smoke | 300 s |
| `soak` | Public network, pressure, fault storms, long runs, performance, reproducibility | Registry pulls from public hosts, guest OOM, repeated-run flake hunting, latency benchmarks, clean-room rebuild | declared per suite |

Tier membership is the binary (and for `unit`, the location), selected with runner filter expressions. Substring conventions in test names are removed.

#### 3.4 Capabilities replace environment opt-ins

- A test declares requirements through the support crate, for example `let docker = require::<Docker>()`. The tier declares what it guarantees. Outcomes:
  - capability guaranteed by the tier and present: test runs;
  - guaranteed but absent: **failed/blocked**, never skipped;
  - not guaranteed by the tier: the test does not belong in that binary, which is a compile-time or lint error where possible and a failure otherwise.
- The approximately 90 `AWMAN_TEST_*` variables collapse to: one tier selector set by the runner profile, a small typed set of fixture locations resolved by the support crate from the manifest (not by each script), and nothing else. Credentials for private-registry tests come from the CI environment of the one suite that needs them.
- A genuine skip (platform not applicable) is reported as a skip with a machine-readable reason, using `#[ignore = "..."]` or `cfg`, so the runner counts it. Early-return "skips" that report as passes are banned by lint.
- Each gate compares `expected` (from `--list` under that tier and platform) with `ran`. Any difference not explained by a declared skip fails the gate. Release-critical scenarios (actual-awman guest boot, old-to-new upgrade, guest strategy matrix, signed-artifact offline session) are additionally listed by ID in the manifest, replacing the `grep` checks in `tools/native-builtin-ci.sh`.

#### 3.5 Isolation by construction

- **Sandbox per test.** `TestSandbox::new()` yields a private root with HOME, XDG directories, awman data root, temp dir, short socket directory (macOS `sockaddr_un` limit is handled here once), and a fully constructed environment map. Nothing reads the process environment; awman library code already takes an `Env` snapshot and the rewrite finishes that seam where it is incomplete.
- **Bare `cargo test` is safe.** Isolation no longer depends on `tools/isolated-test.sh`. The support crate refuses to spawn `awman` or a daemon without a sandbox, and spawned children start from an empty environment plus an allowlist. The wrapper script is deleted.
- **No process-global mutation.** `std::env::set_var`/`remove_var` and `set_current_dir` are disallowed in all test code. With per-test processes (nextest) residual global state cannot leak between tests; the lint keeps tests correct under libtest too.
- **Owned processes.** One supervisor API spawns every child in its own process group (job object on Windows), with close-on-exec for all inherited descriptors, a readiness handshake, a deadline, and kill-and-reap on drop, panic, and timeout. Executables used by tests are written once, atomically, before any test starts (no write-then-exec races). A leak check at test end fails the test if any owned process, port, VM, or image remains.
- **Owned network.** Listeners bind port 0 or a Unix socket inside the sandbox and hand the bound address to clients directly. No fixed ports, no discovery by scanning.
- **Time.** Protocol and state-machine logic takes an injected clock and is tested on virtual time. Anything crossing a real socket, pipe, or process uses real time with event-based waits (`wait_for(condition, deadline)` with diagnostics on expiry). `sleep` as synchronization is disallowed by lint; a deliberate real delay requires an annotated reason.
- **Guest isolation.** Each guest test gets a unique VM owner identity and state root. A declared concurrency budget per hypervisor host is enforced by the runner (test groups), not by ad hoc locks.
- **Test switches in the shipped binary.** Any switch the released executable honors (today at least `AWMAN_TEST_ISOLATION`) is listed in the manifest, may only reduce access to host resources, and has an `artifact`-tier test proving that. Fault-injection hooks reachable in release builds are removed in favour of injected fakes in `unit`/`component`.

#### 3.6 Fixtures and test data

- Deterministic builders for repositories, config trees, work items, workflows, image manifests and layers, servers, daemons, clocks, credentials, and guest state. Each fixture has one owner and a defined lifetime.
- **Fakes are verified.** A fake Docker CLI, registry, or work server runs the same conformance suite as the real one in the `boundary` tier, so hermetic tests cannot drift from reality.
- **External fixtures are content-addressed.** OCI base and guest fixture images are built by a dedicated job, stored by digest (registry or release assets), and pinned in the manifest. Tests verify the digest before use. Rebuilds happen when inputs change or in the reproducibility audit, never inside a test run.
- **Previous-release binaries** for upgrade tests are the published release assets, fetched by pinned digest, because that is the binary users actually upgrade from. Building from Git history remains as a fallback for protocols with no published asset and runs outside the hot path with a digest-keyed cache.
- Golden files and snapshots have one update command, and CI fails on unreferenced or stale snapshots.

#### 3.7 Required contract coverage

| Contract area | Required test design |
|---|---|
| Layer 0 data and configuration | Parsing, validation, defaults, precedence (global/repo/override), migration, persistence and corruption recovery, path and identity rules, serialization compatibility across versions, secrets and redaction, boundary values. Property tests for parse/serialize round trips and merge laws. |
| Layer 1 engines and transports | Git, container/sandbox/builtin runtimes, image and OCI handling, networking, authentication and credentials, process and terminal lifecycle, streams, clocks, retries, cancellation, cleanup, resource limits. One shared conformance suite run against every exchangeable implementation, real and fake. |
| Layer 2 commands and workflows | Catalogue completeness, authorization and admission, session and work-item behavior, state transitions, workflow planning/execution/recovery, concurrency and idempotency, failure classification, and every command's observable contract. Model-based tests for workflow and session state machines. |
| Layer 3 frontends and protocol | CLI/TUI/API parity for every catalogue command where available, parse and output/error shape, frontend handoff, terminal/dialog/event behavior, disconnect/reconnect, failure visibility in each presentation path. Builtin and daemon backends run the same assertions. |
| Cross-component and external boundaries | Real Git, Docker, Apple Containers, sandbox, registry, daemon/API, process groups, guest boot, upgrade and state compatibility, artifact verification, each supported OS/architecture. |
| Security and hostile inputs | Credential and environment isolation, mount scope (current directory or Git root only, parent-directory confirmation), untrusted work items and repository content, malformed protocol messages, stale or mismatched artifacts, privilege boundaries, failure cleanup, and proof that no test tier executes an agent on the host. |

- **Acceptance matrix.** Subsystem × mode (CLI/TUI/API) × backend (builtin/daemon) × runtime × platform × feature, each cell `covered` (named tests and gate), `not applicable` (reason), or `blocked` (owner and release impact). A test count never substitutes for a cell. Public APIs, the command catalogue, documented workflows, shipped templates, and release targets are reviewed against it. The matrix is generated from test metadata where possible and checked in CI.
- **Test mix.** Each domain uses example, boundary, negative, property, state-machine, and fault-injection tests as its risks require. Failure and cleanup paths get the same rigor as success paths. Generated cases use recorded seeds and print a shrunk reproduction.

#### 3.8 Failure evidence

A failing test records test ID, tier, seed, target triple, toolchain, fixture digests, sandbox path, captured stdout/stderr and structured logs, owned process and VM IDs, and a single command that reproduces it locally. CI uploads this per failure and links it from the summary.

#### 3.9 Shell and tooling tests

Scripts that survive as thin wrappers get tests in the `component` tier or a `tools` suite listed in the manifest, run on both GNU and BSD userlands. `shellcheck` and `bash -n` cover every tracked script by glob, not by a hand-kept list. Logic that moved into `xtask` is tested as Rust.

#### 3.10 Slice order

Rebuild in domain slices, each landing its new tests, its old-to-new coverage map, and the deletion of the tests it supersedes in one reviewable change: (1) support crate, sandbox, supervisor, runner configuration; (2) Layer 0; (3) Layer 1 hermetic plus conformance; (4) Layer 2 commands and workflows; (5) Layer 3 frontends and parity; (6) daemon, API, squad, worker; (7) real boundaries; (8) guest; (9) artifact. The old suite stays runnable until its slice is replaced; no slice is complete while its legacy tests still exist.

### 4. Replace CI

#### 4.1 Gate manifest

`ci/gates.toml` declares: tiers and their guaranteed capabilities; suites (binary plus filter) and their tier; platforms with pinned runner images and toolchain; feature sets; gates (`pr`, `main`, `nightly`, `candidate`, `promote`) as lists of suite × platform × feature × backend; release-critical scenario IDs; fixture and previous-release digests; per-gate time budgets; the quarantine list. `cargo xtask gates check` fails when workflows, Make targets, runner configuration, branch-protection expectations, or the devops documentation disagree with it. Workflows are either rendered from the manifest or verified against it; hand-edited divergence cannot merge.

#### 4.2 Job graph

`ci.yml` on `pull_request`, `merge_group`, and pushes to `main`:

| Job | Runs | Notes |
|---|---|---|
| `plan` | Manifest check, workflow lint, suite selection with reasons | Seconds. Everything else reads its output. |
| `lint` | `fmt`, `clippy -D warnings`, architecture lint, shell lint, dependency policy | Parallel with builds; no test waits on it. |
| `build-<platform>-<features>` | Compile once, emit a test archive and the debug `awman` binary | One per platform/feature set. |
| `test-<tier>-<platform>` | Download the archive, run the tier, upload JUnit and `evidence.json` | Sharded by measured duration. `unit` and `component` on Linux, macOS, and Windows; `component` in both backends. |
| `boundary-linux-docker` | `boundary` tier against the runner's Docker Engine and a local registry | Replaces `full-linux-docker` without rerunning hermetic tiers. |
| `static-musl`, `builtin-hermetic-linux-arm64` | Static build smoke; builtin feature build and its hermetic tests | Recorded as build gates, never as guest proof. |
| `gate` | Aggregates evidence against the manifest | The only required status check. Fails closed on any missing, skipped, or inconclusive required suite. |

- No job has `needs` on another unless it consumes that job's artifact. Every job has `timeout-minutes`. `concurrency` cancels superseded PR runs and never cancels `main`, candidate, or promotion runs.
- **Selection.** The `unit` tier, architecture lint, and builtin/daemon parity always run. Other suites may be selected from an explicit path-to-suite dependency map with a full-run fallback for shared infrastructure, build scripts, manifests, the support crate, and unknown paths. `plan` prints what was selected and why. The merge queue and `main` always run the full `pr` gate set.
- **Merge queue.** Enable it so the required gate runs on the exact merge result and `main` stays green; a release candidate can then be cut from any `main` commit. The ruleset keeps a bypass for the releasing maintainer's direct push of the release commit (version bump and notes), because `make release` pushes it to `main` as it does today; that commit is then validated by the candidate gate before anything is published.
- `nightly.yml` runs every tier on every platform including `guest` and `soak`, the repeated-run flake hunt, the clean-room reproducibility build, the dependency audit, and a promotion rehearsal (section 5.6).

#### 4.3 Caches and artifacts

- Cache keys include OS image, architecture, toolchain, profile, feature set, `Cargo.lock`, `.cargo/config.toml`, `build.rs` inputs, and payload manifest digest. No prefix-fallback restore across toolchain or lockfile changes.
- Caches are written only from `main`; PR runs restore read-only. Candidate and promotion builds never restore a cache writable by PR code.
- Test archives and binaries pass between jobs as artifacts with recorded digests, verified on download.
- Record hit/miss, size, and time saved per job. Adopt a compiler cache or a persistent trusted Mac build cache only if phase-0 numbers justify it.

#### 4.4 Runners and trust

- Two disjoint self-hosted label sets: one for CI guest hardware, one for release. Workflows that take release labels are restricted to protected refs through a GitHub environment; any approval rule on it must not add a manual step to `make release` for the releasing maintainer. The self-refusing step in `test.yml` is deleted because CI jobs can no longer match the release runner. `make release` keeps its check for queued jobs that would take the release runner first and its offer to cancel them (section 5.3); with separate labels it is expected to find none.
- PR code from forks never reaches self-hosted hardware. Guest tiers run on `main`, nightly, and candidates only.
- The release Mac is, as today, a temporary runner that `make release` registers for one release and removes when it ends (or an already-online release runner under `RELEASE_RUNNER=auto`). Each job starts from a clean work directory. A health preflight (disk, hypervisor support, toolchain, fixture digests, Apple Containers state) reports `blocked` with the reason before any expensive step.
- Workflow `permissions` default to read-only; write scopes are granted per job. Actions stay pinned to commit SHAs; pins are checked by the workflow lint.

#### 4.5 Reporting

The `gate` summary shows a tier × platform matrix of passed/failed/blocked/skipped/not-selected, queue versus execution time, slowest tests, flaky detections, cache statistics, and links to failure evidence. Timings and outcomes are appended to a durable store (artifact or branch) so p50/p95 and flake rate are trends, not anecdotes.

### 5. Replace prerelease and release with build, validate, promote

#### 5.1 State machine

```
prepared ──► candidate-built ──► validated ──► promoted ──► verified
   │               │                 │             │
   └── any state can move to `abandoned`; nothing public exists before `promoted`
```

- **prepared.** `make release` bumps the version, obtains release notes, runs the local tests, commits `Add release notes for vX.Y.Z`, and pushes that commit to `main`, exactly as `scripts/release.sh` does now. Landing the same two changes through a PR beforehand also works: the command then finds nothing to bump or commit and moves on.
- **candidate-built.** `candidate.yml` is started by `make release` for the pushed commit and version (it can also be dispatched by hand). It runs cheap preflight first: version matches `Cargo.toml`, notes exist without placeholder, commit is the tip or an ancestor of `main`, target set matches the manifest, runner healthy, fixture and payload digests verified. It then builds each distributable target **exactly once** with `--locked`, signs the Apple Silicon binary once with the shipping entitlement, and records per artifact: SHA-256 (unsigned and signed for macOS), size, toolchain, input digest, and a provenance attestation. Artifacts, `SHA256SUMS`, and the evidence bundle are uploaded to a draft GitHub Release for the candidate.
- **validated.** The `candidate` gate first requires the full `main` gate set for the release commit (reusing its evidence when the push run already produced it, running it otherwise), then runs against those bytes: `artifact` tier on every target on its native OS (Linux x86_64 and arm64, Windows, macOS), and on the release Mac the `guest` tier for the same commit plus offline boot, upgrade from the previous published release, dependency/helper scan, and measurements of the exact signed binary. Evidence references artifact digests.
- **promoted.** Promotion follows validation automatically for a release started by `make release`; there is no second command and no extra confirmation, matching today's "tag it and the workflow publishes". `promote.yml` re-downloads the candidate assets, verifies digests and attestations against the evidence, verifies every required gate ID for that commit and gate-manifest version, creates the final annotated `vX.Y.Z` tag at the candidate commit, and publishes the draft. No compilation or signing step exists in this workflow.
- **verified.** A post-publish job downloads from the public release URL, checks `SHA256SUMS` and attestations, runs each binary's smoke on its platform, and boots the published Mac binary on hardware.

#### 5.2 Evidence rules

- Evidence is keyed by commit, input digest (lockfile, toolchain, payload and fixture digests, signing inputs, entitlement file), artifact digest, and gate-manifest version. Any change invalidates the affected evidence. The version-only sentinel is deleted.
- A failed or blocked suite can be rerun against the same artifacts; passing suites are not rerun. A product failure is fixed by a new commit and a new candidate number.
- The operator's local test step keeps its skip prompt (section 5.3). A skip is recorded in the release evidence as `skipped by operator`, never as a pass, and it affects only that local step. The pipeline's own gates (`main` gate set, `artifact`, `guest`, signed-binary boot) cannot be skipped interactively or by flag. An override of one of those is a committed, reviewed manifest change that names the gate, reason, and expiry, and it cannot apply to artifact integrity, the signed-binary boot, or required hardware coverage.
- Candidate evidence expires after a declared window and is void if the commit is no longer an ancestor of `main`.

#### 5.3 Operator interface

**`make release VERSION=vX.Y.Z [RELEASE_RUNNER=auto|local|none]` is the operator interface and it behaves as it does today.** The table below is the complete behavior of `scripts/release.sh` and `tools/release-runner.sh` at `e4cae907`. Every row is a requirement on the new implementation: same order, same questions, same defaults, same outcome for the person at the terminal. The implementation may stay in bash or move into `xtask`; either way each row gets an automated test with faked `git`/`gh`/`cargo` (the approach `tools/test-release-runner.sh` already uses), and the old and new commands are compared side by side on a scratch repository before cutover. A behavior is not dropped or altered because the new pipeline makes it look redundant.

| # | Behavior today | Requirement |
|---|---|---|
| 1 | `make release` without `VERSION` prints `Usage: make release VERSION=vx.y.z [RELEASE_RUNNER=auto\|local\|none]` and exits 1. `RELEASE_RUNNER` defaults to `auto`; any other value than `auto`, `local`, `none` is an error. | Unchanged. |
| 2 | Version must match `vX.Y.Z`; anything else, including pre-release suffixes, is refused. | Unchanged. Candidate numbering is internal and never typed by the operator. |
| 3 | Output is a sequence of coloured `==>` steps with `✓` results, `!` warnings, and a final `Error:` on failure. Step order: Pre-checks, Version in Cargo.toml, Release notes, Tests, Commit, Tag, Release runner, Push, Release workflow. | Same steps, names, and order. |
| 4 | Every step detects that it is already done and says so. Re-running after any failure, interruption, or Ctrl-C resumes without repeating finished work. | Unchanged, and extended to the new pipeline states: a re-run finds the existing candidate, reruns only failed or blocked gates, and resumes an interrupted promotion. |
| 5 | Prompts read from `/dev/tty`, show the default in capitals, and a "no" on a proceed question aborts with `Aborting.` | Unchanged. |
| 6 | Pre-check: `gh` must be authenticated. | Unchanged. |
| 7 | Pre-check: not on `main` → warn and ask to proceed (default no). | Unchanged. |
| 8 | Pre-check: fetches `origin/main`; if local differs → warn, show status, ask to proceed (default no). | Unchanged. |
| 9 | Pre-check: working tree must be clean apart from `Cargo.toml`, `Cargo.lock`, and `docs/releases/`; otherwise list the changes and ask to proceed (default no). | Unchanged. |
| 10 | Pre-check: if the tag already exists on origin → warn and ask to proceed (default yes); its Release run is then checked and re-run if it failed. A tag left locally by an earlier run is noted and reused. | Unchanged. In addition, an existing candidate for this version is reported here and resumed (row 4). A version that is already published reports success. |
| 11 | Runner selection. `none`: runner not managed. `auto`: if a suitable runner is already online, use it; otherwise use this Mac if it passes the hardware check. `local`: require this Mac. When this Mac is used: install or refresh `/opt/awman` (`provision`), then run the full prerequisite check and stop on any gap. If no runner is online and this host cannot be one: show why and ask whether to continue (default no); under `local` this is an error. | Unchanged, against the release runner labels of section 4.4. |
| 12 | Version bump: if `Cargo.toml` is not at the version, rewrite the first `version = "…"` line (portable `awk`, works with BSD tools), run `cargo update --workspace` so only awman's own lock entries change, and verify the result. Warn if the installed `awman --version` disagrees with the old `Cargo.toml` version. Already at the version → say so. | Unchanged. |
| 13 | Release notes: create `docs/releases/vX.Y.Z.md` from the template if missing. If the placeholder `_Write release notes here._` is present, offer to launch `awman chat` (default yes); stop if the placeholder is still there afterwards or the offer is declined. | Unchanged. |
| 14 | Tests: run `make test` with live output. On success record it so a re-run does not repeat it. | Unchanged for the operator: the same local run of the standard isolated suite (the new `make test`). The record is bound to the tested source tree's hash (the tree the release commit then carries) instead of the version, so it is reused on re-run but not after a source change. |
| 15 | Tests failed: copy the full output to the clipboard (`pbcopy`, `xclip`, or `xsel`) and say so; ask `Skip tests and continue the release anyway?` (default no); if declined, offer `awman chat` to fix (default yes) and stop with instructions to re-run. | Unchanged, including the skip. A skip is recorded as a skip (section 5.2); the candidate gates still run in CI against the pushed commit and artifacts. |
| 16 | Commit: if `Cargo.toml`, `Cargo.lock`, or the notes have changes, commit them as `Add release notes for vX.Y.Z`; otherwise say nothing to commit. | Unchanged. |
| 17 | Tag: create the local tag `vX.Y.Z` unless it exists. | Unchanged locally. See the note below on when the tag reaches origin. |
| 18 | Release runner (when this Mac serves the release): look for queued or in-progress runs with a job waiting for the release runner's labels; if any, say how many and ask `Cancel them?` (default yes). Download the latest `actions/runner` osx-arm64 archive once into `~/.awman/release-runner/cache` and refuse it unless its SHA-256 matches the published checksum. Register it with a registration token passed through the environment, never argv, named `awman-release-<tag>-<host>`. Start it under `caffeinate` in its own process group with the short private work directory `/tmp/awr` (mode 0700, refused if a symlink or foreign). Wait until it is online. All of this happens before anything is pushed, so a runner that cannot register stops the release with nothing public. | Unchanged, with the release labels of section 4.4. |
| 19 | Provisioning details: `/opt/awman` created with `sudo` once and handed to the user; only changed files rewritten; a foreign file kept as `<name>.bak`; guest fixture built and cached under `~/.awman/release-runner/fixtures`; wrappers have fixed command lines for a cleared environment. | Unchanged in effect: the Mac is made ready on every release without the operator remembering anything. Fixture content comes from the pinned digest (section 3.6) and is still cached locally. |
| 20 | Push: push commits to `origin/main` if ahead, then push the tag; each reports "already on origin" when done. | `main` is pushed as today. The step then starts the candidate for that commit. The final tag is pushed to origin by promotion (note below) and the step reports it once present. |
| 21 | Release workflow, runner not managed by this command (`none`, or `auto` with an existing runner): print that the workflow will publish after the gates pass, print the `gh run watch …` line to follow it, print `Release vX.Y.Z tagged!`, exit 0. | Same messages and exit; publication still completes without further operator action because promotion is automatic. |
| 22 | Release workflow, this Mac serving: find the run (up to 5 minutes), report if it already succeeded; if it finished unsuccessfully ask `Re-run its failed jobs on this Mac?` (default yes) and re-run only those; otherwise watch it to completion (60 s interval). On failure stop with the `gh run view … --log-failed` hint and "Re-run this script to retry the failed jobs." On success print `Release vX.Y.Z published!` | Unchanged, covering candidate validation through promotion and post-publish verification as one watched run or chain. |
| 23 | Teardown: an `EXIT` trap stops the runner's process group (SIGINT, then SIGKILL) and deregisters it on success, failure, or interrupt; a second Ctrl-C cannot interrupt teardown; the group is signalled only while it still holds the runner's processes; only a work directory the command created is removed. Exit code 130 on interrupt. | Unchanged. |
| 24 | End state: release commit on `origin/main`; tag `vX.Y.Z` on that commit locally and on origin; GitHub Release `vX.Y.Z` with the four binaries (`awman-linux-amd64`, `awman-linux-arm64`, `awman-macos-arm64`, `awman-windows-amd64.exe`), `SHA256SUMS`, and the notes file as its body; temporary runner removed. | Unchanged; asset names and `SHA256SUMS` format are part of the contract. |
| 25 | `tools/release-runner.sh` subcommands `check [--hardware]`, `provision`, `busy <repo>`, `verify-archive`, `up <repo> <tag>`, `down` (idempotent), and the `AWMAN_RELEASE_RUNNER_HOME` override. | The same operations stay available to the operator under the same names (script or `xtask` subcommands with a compatibility wrapper). |

**The one visible difference, and why.** Today the final tag is pushed to origin before the release builds run, so a failed build leaves a public tag without a release. In the new pipeline the remote tag is created by promotion, on the same commit, at the moment the release is published. The operator types nothing different, the local tag is created at the same step, and the end state (row 24) is identical; between "Push" and "published" the tag is simply not yet on origin. The releasing maintainer has accepted this difference; pushing the final tag before validation is not supported.

**Additions that do not change the above.** `make release-status VERSION=…` (read-only: current state, failing gate, evidence links) and `make release-abandon VERSION=…` (delete an unpromoted candidate). Neither is needed for a normal release.

`make apple-silicon-prerelease` is no longer needed before a release because its checks run inside candidate validation against the artifacts that ship; the target remains as a way to run that validation for the current `main` commit without releasing. `tools/prerelease-ci-gate.sh` is removed (evidence replaces job-name matching).

#### 5.4 Functional gates versus measurements

Functional acceptance (boots, upgrades, runs offline, no extracted helpers, correct entitlements) blocks. Size, startup, and RSS are always recorded; a metric becomes blocking only after its variance on the release hardware is characterised and a threshold with margin is committed to the manifest. Until then regressions raise a visible warning in the candidate summary.

#### 5.5 Recovery and rollback

Document and test: interrupted candidate build, runner lost mid-validation, partial asset upload, promotion interrupted between tag and publish (idempotent resume), and a bad release found after publication (mark superseded, publish a fixed patch version; a published tag is never moved or reused). Tag-protection rules restrict `v*` tag creation on origin to the promotion workflow.

#### 5.6 Rehearsal

Nightly runs the whole candidate and promotion path against a throwaway version in dry-run mode (everything except creating the public tag and publishing), so the release machinery is exercised daily rather than once per release.

### 6. Design recommendations

These are part of the design. Each is adopted unless phase 0 produces a written reason to reject it.

#### 6.1 Test stability

- Run each test in its own process with a hard timeout and automatic termination of the whole process tree; a hang becomes a named failure with a stack dump in minutes instead of a stuck job.
- Zero retries in required gates. A nightly job runs every tier repeatedly, in shuffled order and under CPU contention, and files each intermittent failure with its evidence. Flake rate per test is tracked.
- Quarantine is explicit: an entry in the manifest with owner, issue, and expiry. A quarantined test still runs and reports; a quarantined required scenario keeps the gate non-green. Expired entries fail `plan`.
- Replace every wall-clock assertion with an event or a virtual clock. Move the 5 ms latency benchmark out of `make test` into `soak` on designated hardware, removing `AWMAN_TEST_SLOW_HOST`.
- Leak detection at the end of every `component`, `boundary`, and `guest` test (processes, ports, sockets, temp roots, images, VMs), and a job-level sweep that fails if the runner is not clean afterwards.
- Retries are permitted only for infrastructure steps (artifact download, registry fetch by digest, runner registration), bounded, logged, and reported as infrastructure in the summary.
- Pin every runner image and tool version in the manifest; upgrade them by PR so an image change is a visible, revertible commit.

#### 6.2 Completeness

- The acceptance matrix and the expected-versus-ran comparison are the definition of coverage. Line coverage (`cargo-llvm-cov`) is published per layer as a diagnostic and used as a no-regression ratchet on changed code, not as a target percentage.
- Derive tests from sources of truth: every `CommandCatalogue` entry must have parity tests in each frontend and backend; every config key must have default, precedence, and validation tests; every shipped template must build in the `boundary` tier; every documented exit code and JSON output shape must have a contract test. Catalogue or schema additions without tests fail `plan`.
- Require a failing test with every bug-fix PR, tagged with the issue or commit, so the regression list grows mechanically.
- Seed defects on purpose: a maintained set of representative faults per layer (and a missing-target, renamed-job, and skipped-hardware fault for the pipeline) that must each turn the correct gate red. Run it nightly.
- Cover Windows as a real platform: `unit` and `component` tiers plus artifact smoke, not one named test.
- Cover feature combinations deliberately (`default`, `builtin-runtime`, each on supported targets, plus the unsupported-target refusal) from the manifest rather than by scattered jobs.

#### 6.3 Performance

- Build test binaries once per platform and feature set and reuse the archive across shards and tiers; never recompile to rerun a subset.
- Consolidate integration targets (section 3.2) and keep the VM stack out of every binary that does not boot a guest.
- Shard by recorded durations and rebalance automatically from the timing store. Fail `plan` when any tier exceeds its budget for several consecutive `main` runs, so slowdowns are fixed when introduced.
- Remove serial `needs` chains and duplicate suite runs across jobs; each test runs once per platform/backend cell per commit.
- Prebuild and pin fixtures and previous-release binaries; keep registry pulls, image builds, and history builds out of test runs.
- Run guest scenarios concurrently up to the measured safe VM budget of the host; share immutable imported images between scenarios while keeping per-test VM state private.
- Give developers a fast inner loop: `make check` (lint plus `unit` for changed crates) and `make test` (`unit` + `component`) with the same runner profile as CI, and a documented way to run one test with its evidence.
- Measure before adopting a faster linker, compiler cache, or larger runners; record the result in the timing report.

#### 6.4 Correctness

- Property and model-based tests for parsers, config merging, path and identity rules, protocol codecs, and workflow/session state machines, with committed regression seeds.
- Differential testing as a standing technique: builtin versus daemon backend, CLI versus API versus TUI outcome traces, fake versus real runtime under the shared conformance suite.
- Mutation testing (`cargo-mutants`) on Layer 0 and Layer 2 in nightly, sampled to fit the budget, with surviving mutants triaged into new tests. This checks that tests detect wrong behavior rather than merely execute code.
- Snapshot tests for CLI text/JSON output and TUI buffers with one reviewed update path; reject unreviewed snapshot churn in CI.
- Fuzz the untrusted-input surfaces (work items, repository config, protocol frames, OCI manifests and archives) in `soak` on a separately pinned toolchain if the fuzzer requires one; keep discovered inputs as regression cases in `unit`.
- Tests assert on typed results and structured events, not on log text or sleep-then-inspect.
- Dependency policy in `lint` (`cargo-deny`: advisories, licenses, sources, duplicate versions) and a check that `NOTICE.third-party.md` matches the lockfile.
- Clippy lints that encode the isolation rules (`disallowed-methods` for environment mutation, wall-clock sleep, fixed-port binds, and unsupervised `Command::spawn` in test code), so the rules hold after this work item closes.

#### 6.5 Release process stability

- Same-bytes promotion with digest and attestation checks at every hop; `gh attestation verify` documented for users next to the existing checksum instructions in `docs/11-runtimes.md`.
- Remote final tag created last, by promotion; `v*` tags and the `release` environment protected.
- Dedicated release capacity with a health preflight, so a release never competes with scheduled tests and no longer depends on manual cancellation.
- Daily rehearsal of candidate and promotion (section 5.6) and a post-publish verification job.
- Keep the operator command and its prompts exactly as they are (section 5.3) and put a contract test behind every row, so the release experience cannot drift when the pipeline changes. Record each operator answer (proceed off `main`, skip local tests, cancel queued runs) in the release evidence.
- Resume from GitHub state rather than only from local files, so a release interrupted on one Mac can be finished from another checkout.
- A written runbook for each failure state in section 5.5, exercised by the rehearsal's fault cases.
- Release notes, version, and target set validated in preflight before any build minutes are spent.
- A clean-room reproducibility build of the unsigned artifacts in nightly and on each candidate, compared with the candidate's unsigned digests. Report differences from the first run; make it blocking once builds are shown to reproduce.
- Keep candidates and their evidence for a declared retention period so any published binary can be traced back to its gate results.

### 7. Migration and cutover

1. Land `xtask`, the manifest, evidence format, and timing instrumentation; run them against the existing pipeline in report-only mode.
2. Land the support crate and runner configuration; rewrite slices in the order of section 3.10. Each slice runs beside the legacy tests it replaces for a bounded shadow period, comparing matrix cells, detected seeded defects, skip counts, and platform results, then deletes them.
3. Bring up `ci.yml` beside `test.yml`; when the `gate` check has matched or exceeded the old required checks for the agreed period, switch branch protection to `gate` and delete `test.yml`.
4. Bring up `candidate.yml`/`promote.yml`; run at least one full rehearsal and one real release with the old path available as a documented fallback; then delete `release.yml` and the superseded script internals. `make release` is switched to the new pipeline only after the section 5.3 contract tests and transcript comparison pass.
5. Rewrite `aspec/devops/cicd.md` and `localdev.md` from the manifest. Close the ledger.

Rollback during cutover is reverting the branch-protection switch and the workflow deletion commit. After step 5 there is no legacy path.

### 8. Deliverables

`crates/awman-test-support`; `xtask`; `ci/gates.toml`; runner configuration; new `component`/`boundary`/`guest`/`artifact` binaries and rewritten inline tests; `clippy.toml` and architecture-lint rules for test hygiene; `ci.yml`, `nightly.yml`, `candidate.yml`, `promote.yml`; new Make targets (`check`, `test`, `test-boundary`, `test-guest`, `release-status`, `release-abandon`) beside the unchanged `make release` interface and its contract tests; the inventory, acceptance matrix, and old-to-new coverage map; baseline and post-migration timing reports; rewritten devops and user documentation.

### 9. Replacement ledger

The work item is complete only when every row is `replaced` or `deleted`, with a link to the change that did it.

| Existing item | Fate |
|---|---|
| `tests/**` (27 targets, helpers, fixtures), `crates/awman-cli/tests/` | Rewritten into the tier binaries; old files deleted |
| Inline `#[cfg(test)]` modules (382 files), doctests | Each reviewed and rewritten or retired per the inventory |
| `test-support` feature, `[[test]]`/`[[example]]` lists, `[profile.test]` | Replaced by the support crate, verified discovery, and measured profile settings |
| `AWMAN_TEST_*` variables | Replaced by tier selector and typed capabilities; remaining shipped-binary switches listed in the manifest |
| `tools/isolated-test.sh` | Deleted; isolation is in the support crate |
| `make test`, `test-fast`, `test-full`, `test-builtin`, `test-builtin-hw`, `pre-push`, `native-builtin-ci` | Replaced by manifest-backed targets |
| `tools/test-builtin-hw.sh`, `native-builtin-ci.sh`, `test-builtin-network.sh`, `builtin-hw-fixtures.sh`, `build-previous-builtin-awman.sh`, `macos-sign-test-artifacts.sh` | Replaced by the `guest` tier, pinned fixtures, and published previous-release assets |
| `tools/release-artifact-check.sh`, `release-offline-session.sh`, `release-upgrade-policy.sh`, `measure-release-artifact.sh`, `reproducible-build-audit.sh` | Replaced by the `artifact` tier, measurement policy, and reproducibility job |
| `tools/test-*.sh` with no caller (seven listed above), `test-msb-protocol-compat.sh`, `test-msb-catalog-compat.sh`, `probe-keychain-stdin.sh`, `tools/oci-runtime-spike/` | Each wired to a manifest suite as a rewritten test, or deleted with a reason |
| `tools/architecture-lint.sh`, `test-architecture-lint.sh` | Kept as a contract (`aspec/devops/architecture-lint.md`), re-homed under the `lint` gate with its fixtures in the manifest; extended with test-hygiene rules |
| `tools/msb-payloads/*`, `make payloads*` | Verification kept as a requirement; invocation and evidence moved under the manifest |
| `scripts/release.sh`, `tools/release-runner.sh` and their tests | Re-implemented behind the unchanged `make release` interface; every behavior in section 5.3 retained and contract-tested |
| `tools/apple-silicon-prerelease.sh`, `prerelease-ci-gate.sh` and their tests | Checks moved into candidate validation; `make apple-silicon-prerelease` kept as a no-release validation run; job-name gate deleted |
| `.github/workflows/test.yml`, `release.yml` | Replaced by `ci.yml`, `nightly.yml`, `candidate.yml`, `promote.yml` |
| `aspec/devops/cicd.md`, `localdev.md` testing sections | Rewritten from, and checked against, the manifest |

## Edge Case Considerations:

- First release on a new Mac, cold caches, offline or rate-limited registries, missing Apple Containers or Docker, low disk, interrupted jobs, stale runner registrations, and two operators starting candidates for the same version must each end in an actionable `blocked` or `failed` state with nothing published.
- Candidate evidence is bound to commit, lockfile, toolchain, payload and fixture digests, target, signing inputs, and manifest version. A force-push, changed release notes, changed entitlement file, or manifest edit cannot reuse stale evidence.
- A test whose required capability disappears mid-run (Docker daemon restarts, runner loses the hypervisor) is reported as infrastructure-blocked, not as a product pass or a skip.
- Timeouts terminate child process groups and release image and VM leases, including after a cancelled workflow; the next job on the same runner must start clean.
- Docker, Apple Containers, and Hypervisor.framework guests are separate finite pools with separate concurrency budgets.
- A cross-build or a hosted-Mac build is never recorded as guest proof. Hosted macOS runners are VMs and cannot boot a guest.
- Fork PRs: no secrets, no self-hosted runners, read-only caches; the required gate must still be decidable for them.
- A new platform, runtime, feature, test binary, or shell check that is not in the manifest fails `plan` rather than running ungated.
- Path-based selection must fall back to a full run for any path it cannot map, and must not be able to deselect architecture lint, parity, or the `unit` tier.
- Windows path length, absence of Unix sockets and process groups, and macOS socket-path limits are handled inside the support crate, not per test.
- Tests must never read or change the developer's real HOME, keychain, clipboard, Git configuration, daemons, or container store, including when run with a bare `cargo test`.
- A quarantine or override entry that expires during a candidate's validation invalidates that candidate's affected evidence.

## Test Considerations:

- **Total replacement.** The ledger has no legacy row. No file under the old `tests/` layout, no legacy helper, no legacy Make target, script, or workflow remains. Every inventory entry has a disposition, and reviewers can trace any old test or product contract to its new test and gate.
- **Acceptance matrix.** Every applicable cell has named passing tests or an explicit blocked entry with owner and release impact. Catalogue commands, config keys, templates, and release targets are all represented.
- **Fail-closed behavior.** Deliberately: add an unregistered test file, remove a required suite, rename a job, drop a release-critical scenario ID, remove a capability from a tier's runner, convert a test to an early-return skip, and expire a quarantine entry. Each must turn `plan` or `gate` red with a clear message.
- **Defect detection.** The seeded-defect set fails the correct tier for each layer. Mutation testing results for Layer 0 and Layer 2 are recorded and surviving mutants triaged.
- **Isolation and repeatability.** Run every tier repeatedly (shuffled, parallel, under CPU contention) with zero intermittent failures over the agreed run count, including the historically flaky API, fake Docker CLI, lock, subprocess, daemon, and VM-owner scenarios. After a forced failure, panic, timeout, and job cancellation, no orphan process, port, socket, guest, image, credential, or host-state change remains. A bare `cargo test` on a developer machine leaves real HOME and keychain untouched (verified by a canary).
- **Platform coverage.** `unit` and `component` pass on Linux, macOS, and Windows in both backends; `boundary` on Linux with Docker; `guest` on Apple Silicon; `artifact` on every release target. Discovery counts and per-scenario results, including skips with reasons, are published for each.
- **CI behavior.** Superseded PR runs are cancelled; `main` and release runs are not. Cache keys change with each declared input; a PR cannot write a cache that a release build reads. Hardware unavailability shows as `blocked`. Measured p50/p95 for the PR path meet the stated targets or the documented staged plan.
- **Release behavior.** Build logs show one optimized build per target per candidate. SHA-256 of every asset is identical at build, after validation, at promotion, and when downloaded from the public release; the downloaded Mac binary boots a guest. Attestations verify. Exercise: failed-suite rerun on the same artifacts, changed input invalidating evidence, runner loss, interrupted promotion and resume, concurrent candidates, expired evidence, and an attempted promotion with a missing gate. The nightly rehearsal passes for the agreed number of consecutive days before the first real release on the new path.
- **Operator contract.** One automated test per row of the section 5.3 table, run on Linux and macOS (BSD tools) with faked `git`, `gh`, `cargo`, and `awman`: usage and version validation, each pre-check prompt and its default, version bump, notes template and placeholder handling, local test pass/fail/skip paths including the clipboard copy, commit, tag, runner selection for `auto`/`local`/`none`, queued-run cancellation offer, registration with the token absent from argv, push, watch, failed-job re-run, teardown on success, failure, and double Ctrl-C, and resume from every step. A transcript comparison of the old and new command on a scratch repository shows the same steps, prompts, and end state. The releasing maintainer performs one real release with the new implementation and confirms nothing about the experience changed apart from the tag timing noted in section 5.3.
- **Reporting.** Baseline and post-migration p50/p95, queue time, per-tier failure and flake rates, rebuild counts, cache hit rate, and manual interventions per release are published. Acceptance requires the full rewrite and matrix to be complete, coverage preserved or strengthened, no path by which stale evidence satisfies a gate, and a measured improvement in critical-path time against the v0.13 baseline.

## Codebase Integration:

- `aspec/architecture/2026-grand-architecture.md` and `aspec/architecture/security.md` are acceptance constraints: layer boundaries, CLI/TUI/API and builtin/daemon parity, verified payloads, native hardware coverage, the signed artifact's offline guest check, and no agent execution on a host. Testability seams added to production code follow the layer rules and pass `make architecture-lint`.
- `xtask` and `awman-test-support` are small workspace members excluded from the shipped binary. They are build and test infrastructure and must not become a second source of runtime policy. Keep them readable by an intermediate Rust developer; prefer plain data and straightforward code over macro-heavy frameworks.
- New tooling (`cargo-nextest`, `cargo-llvm-cov`, `cargo-mutants`, `cargo-deny`, a property-testing crate, a snapshot crate) is pinned by version, installed from verified sources in CI, and listed in `aspec/devops/localdev.md`. A tool is adopted for a stated requirement in this document, not for advertised speed.
- Starting points to read and then replace: `.github/workflows/test.yml`, `.github/workflows/release.yml`, `Makefile`, `scripts/release.sh`, everything under `tools/`, `crates/awman-cli/Cargo.toml`, and the suites under `src/`, `crates/`, and `tests/`. Their present shape is not a requirement.
- GitHub features relied on: workflow concurrency, merge queue (`merge_group`), environments with required reviewers, tag and branch rulesets, artifact digests, artifact attestations, draft releases, and cache scoping. Confirm availability for this repository's plan in phase 0 and record a substitute for any that is unavailable.
- Coordinate with WI 0124 (Linux builtin gates) and WI 0129 (code quality): Linux KVM guest coverage and further crate splitting plug into the manifest and tiers defined here.

## Documentation

After implementation:

- Rewrite `aspec/devops/cicd.md` and the testing sections of `aspec/devops/localdev.md` from the gate manifest, and keep them checked against it in CI. Remove every statement about gates, variables, or scripts that no longer exist.
- Add `aspec/devops/testing.md`: tiers, capabilities, the support-crate API, fixture rules, naming, how to add a test, how to reproduce a CI failure, quarantine policy, and the acceptance matrix location.
- Write the release operator runbook in `aspec/devops/`: `make release VERSION=…` as the normal path with its steps and prompts, what a re-run resumes, `release-status`/`release-abandon`, recovery from each failure state, and rollback.
- Update user-facing docs in `docs/` only where users are affected: release verification in `docs/11-runtimes.md` (checksums and attestation verification) and any install instructions that name release assets. Do not add work-item or implementation documents to `docs/`.
