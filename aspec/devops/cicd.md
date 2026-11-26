# Continuous Integration and Deployment

The manifest test system is the only supported path. See [replacement-ledger.md](replacement-ledger.md) for coverage decisions and evidence. Release publication is one command; see [release.md](release.md).

The Linux x86_64 builtin PR build and release jobs run on Ubuntu 22.04 to
preserve the published GNU executable's glibc 2.35 baseline. Default-feature
Linux PR suites remain on Ubuntu 24.04, and the musl static smoke remains
separate. Release asset production rejects any newer or unrecognized glibc
symbol requirement before publishing its build record.

Platform: GitHub Actions

What CI runs is defined once, in `ci/gates.toml` (the **gate manifest**). The CI
workflows `ci.yml` and `nightly.yml` are **generated** from it by
`cargo xtask gates render`; `cargo xtask gates check` (the first step of every
`ci.yml` run, and the `xtask-gates` suite) fails on any difference between the
manifest, the generated files, the Makefile contract and this document's
contract block. Change the manifest, re-render, never edit a generated workflow
by hand. How tests are written and organised is in [testing.md](testing.md);
the release procedure is in [release.md](release.md).

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

## Gates

A gate is a named set of suites on named build cells. A suite is a set of tests
of one tier; a build cell is one compilation (platform × feature set × profile)
whose test archive is reused by every job that needs it, so nothing is compiled
twice.

| Gate | Runs on | Contents |
|---|---|---|
| `pr` | pull requests and merge groups | every lint (fmt, clippy, architecture, hygiene, shell, workflows, deny, notice), doctests, the web app, the `unit` and `component` tiers on Linux x86_64, macOS arm64 and Windows x86_64, the `boundary` tier on Linux with Docker, the builtin-feature unit tests on Linux arm64, the xtask, release-contract, pipeline and CI-helper suites, and a static musl build smoke |
| `main` | pushes to `main`, or `workflow_dispatch` with `gate=main` | the full hosted `pr` gate; no hardware runner is required |
| `release` | `workflow_dispatch` with `gate=release` and a `key`, started by `make release` | the narrow 39-cell blocking set: Linux x86_64 default unit, component, security, parity, Docker/registry/Git boundary, offline, template and xtask suites, plus the Linux x86_64 builtin unit suite. It inherits no lint, web, Windows, macOS, arm64 builtin or musl work |
| `nightly` | 03:23 UTC schedule | everything in `main`, plus hosted soak suites and the nightly modes below |
| `nightly-artifacts` | nightly | six artifact suites (smoke, entrypoints, switches, dependency/helper scan, upgrade policy, measurement) on each of the four shipping target and feature configurations |
| `nightly-kit` | nightly | the standalone native-kit relocation contract |
| `native-diagnostic` | operator only: `make validate-native` on the host being diagnosed | broad native guest, boundary, soak and Linux replacement/upgrade breadth; no hosted job is rendered for it |

The `release` gate has a 15 minute budget and runs in parallel with the four
asset builds. A previous `pr`, `main` or `nightly` run may satisfy it only when
authenticated evidence proves every required cell at the exact release commit.

**What blocks a release.** (1) The `release` gate. (2) For each of the four
shipping assets, an attested build by `release.yml`, then the three exact-byte
smoke suites (smoke, entrypoints, access reduction) on that asset's hosted OS.
(3) Two native checks on operator machines, narrowed to the three tests named in
the release plan: signed offline boot and persisted-state upgrade of the macOS
asset, and offline KVM boot of the Linux amd64 asset. (4) Anonymous verification
of the published bytes. Nothing else blocks; the evidence is verified by
`cargo xtask gates verify-release-evidence`, never trusted from a summary.

**What moved off the release path, and where it runs now.**

| Coverage | Now |
|---|---|
| macOS and Windows host unit/component suites, arm64 builtin units, musl, lint, doctests, web | `pr` and `main`, inherited by `nightly`; advisory for unmerged release source |
| Artifact dependency/helper scan, upgrade policy, measurement on all four assets | `nightly-artifacts` |
| Native guest boot/exec/import/network/builder/strategy, builtin Mac units, guest soak, Apple and Linux Docker, registry and Git boundaries, template, offline and private-memory suites, Linux replacement and published-predecessor upgrade | `native-diagnostic`, run by an operator with no schedule. A missing predecessor or infrastructure is blocked evidence, never a pass |
| Standalone kit relocation | `nightly-kit` |
| Disabled `sbx` boundary | reported BLOCKED by the nightly `sandbox-coverage` job until [issue #86](https://github.com/prettysmartdev/awman/issues/86) |
| Reproduction of the four shipping assets | removed as a release blocker; the nightly `repro-build` observes only Linux x64 default |
| Duplicate main-gate, candidate and promotion gates | removed |

`ci/release-coverage-baseline.json` and `ci/release-coverage-migration.json`
record every moved or deleted cell with its destination; `cargo xtask gates check`
rejects a missing row or a filter narrowed without a recorded decision. The
stated residual risks are in [release.md](release.md#residual-risks).

**Selection.** On a pull request only the suites whose declared `paths` changed
are selected, with the exceptions printed in the manifest's `[selection]` table
(`Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `.cargo/`, `ci/`,
`.github/`, `.config/`, `xtask/`, `crates/awman-test-support/`, `Makefile`,
`tools/`, `third_party/` and anything unknown) which select the full `pr` set.
Unit, lint, doctest and parity suites are always selected. Merge-group and
`main` runs always run the full set. Pull requests require a base commit for
changed-path selection; full-gate dispatches do not need one. A plan that fails,
a missing pull-request base commit or a truncated diff is non-green, never an
empty selection.

**Evidence.** Each test job writes `run.json`, the selected test inventory,
JUnit and failure records. The `gate` job reconstructs
the expected set from the manifest and the commit itself, checks that shard
sets are disjoint and complete, requires every expected job to have succeeded
and every cell to have evidence, and only then reports success. A missing job,
renamed job, empty suite, unexpected skip, duplicate or unknown test id, expired
quarantine or override, or a job that produced no evidence fails the gate.
Capability loss is `blocked`, which also fails the gate; nothing is ever
reported as passed because it was skipped.

## Workflows

| Workflow | Trigger | Purpose |
|---|---|---|
| `ci.yml` (generated) | pull request, merge group, push to `main`, `workflow_call`, `workflow_dispatch` (`gate` is `pr`, `main` or `release`; `ref`, `key`) | the `pr`, `main` and `release` gates, and the base of `nightly` |
| `nightly.yml` (generated) | daily schedule, `workflow_dispatch` | the `nightly` gate and its modes |
| `release.yml` | `workflow_dispatch` (`asset`, `version`, `commit`, `key`) | one asset per run on hosted runners: `produce` builds, signs and attests the exact bytes and the native kit; `validate` verifies the attestation, smoke-tests those bytes and writes the build record |
| `fixtures.yml` | `workflow_dispatch` | pull the reviewed Alpine digest, derive and attest the architecture-specific (arm64 and amd64) guest fixtures, publish an immutable fixture prerelease that is never marked Latest |

Fixture releases use `fixtures-<source commit>` tags and remain downloadable by
their exact tags for digest pinning. They contain test inputs, not awman
executables, so publication marks them as prereleases and explicitly excludes
them from GitHub's Latest release selection. The workflow and `make fixtures-pin`
check `releases/latest` after publication, clear an older fixture release's
Latest designation if necessary, and refuse to finish while any fixture tag is
Latest.

`make fixtures-pin` is the maintainer's one-time preparation command on a clean
branch based on `origin/main`. It dispatches `fixtures.yml` at the current
`main`, waits for the run or reuses its complete immutable fixture release,
downloads the assets, verifies both architectures with `cargo xtask fixtures
pin`, checks the manifest and commits the reviewed pins. It stops if `main`
moves before dispatch or if the branch's fixture inputs differ from `main`.
`make release` only consumes the committed pins and never publishes fixtures.

### `ci.yml` job graph

`plan` → `lint`, `build-*`, `test-*`, `boundary-linux-docker`, `static-musl`,
`web` → `gate`.

- `plan` checks out the commit, runs `cargo xtask gates check`, resolves the
  gate from the event, selects suites from the changed paths and emits the build
  matrix and shards.
- `build-<build>` compiles one build cell, writes a nextest archive, the
  expected-test list and the digests of both. `build-linux-x64-default`,
  `build-macos-arm64-default`, `build-windows-x64-default`, and
  `build-linux-arm64-builtin`.
- `test-<tier>-<platform>` downloads the archive, verifies its SHA-256 against
  both the job output and `archive/digests.json`, then runs the shard's suites
  with the pinned nextest. Jobs need only `plan` and their own build.
- `lint` runs fmt, clippy, architecture lint, hygiene, shell lint, workflow lint,
  `cargo deny`, the third-party notice check and doctests.
- `ci.yml` selects no hardware cells. Native evidence comes from the release
  command's own native checks, never from a workflow runner.
- `gate` (job and status name `gate`) aggregates evidence and writes the step
  summary: tier × platform matrix of passed/failed/blocked/skipped/not-selected,
  slowest tests, quarantine and override state, cache statistics and the
  `reproduce` line of every failure.
  Partial reruns may reuse successful jobs' artifacts from an earlier attempt
  of the same authenticated run. The current attempt must still report each
  required job successful, and source, job, shard and complete passing test
  inventory must match. Zero, future or malformed producer attempts and
  artifacts from another run or source are rejected.

### `nightly.yml`

`full` (the `nightly` gate through `ci.yml`), the `nightly-artifacts` jobs (one
per shipping asset), `kit-relocation` and the blocked `sandbox-coverage`
report, plus:

- `flake-hunt`: hosted tiers five times with recorded shuffle seeds under CPU
  contention; any intermittent failure opens a `flaky-test` issue with its
  failure record.
- `seeded-defects`: runs known defects from `ci/seeded-defects/` against an
  isolated clean-HEAD baseline and patched checkout. Fresh JUnit output detects
  product failures; each run retains `seeded-defects.evidence`. Compiler
  failures, stale inputs, surviving defects, pipeline faults and blocked
  infrastructure are non-passing outcomes, never detected defects.
- `mutation`: `cargo-mutants` on `src/data` and `src/command`, sampled.
- `coverage`: `cargo-llvm-cov` per layer; diagnostic.
- `repro-build`: two clean-room unsigned Linux x86_64 default builds compared
  with each other; non-blocking. The four shipping target and feature
  combinations are not covered by a reproduction check.
- `audit`: `cargo deny check advisories`.
- `ci-trends`: p50/p95 per gate, queue time, flake rate and cache hit rate
  from the last 30 days of `timing-*` artifacts.

Timing history records measured invocation wall time (including runner startup)
from `run.json` and individual outcomes from the inventories and JUnit. Shard
sizing uses complete, successful, disjoint partitions: their durations are
summed and their test IDs combined before calculating a suite median. Duplicate
downloads count once, and attempts and build cells are kept separate. Main-gate
budget history uses one wall-span measurement per run's latest attempt, rather
than adding parallel suite durations. The span runs from the first completed
predecessor's start to the last predecessor's finish; it excludes aggregation.
Queue time is the longest predecessor queue. Unmeasured invocation queue and
cache values remain unknown, rather than being reported as zero or cache misses.
Actions job timestamps are diagnostic only: missing, malformed or inverted
timestamps leave gate queue and execution measurements unavailable (`null`) and
do not affect the verdict. Required job success, provenance, artifacts and exact
test coverage still determine admission. Timing history retains the verdict
without inventing a duration; unavailable measurements do not enter percentiles
and break a sequence of measured main-gate budget overruns.

## Pinned inputs

- Toolchain: `rust-toolchain.toml` (1.94.0), repeated per platform in the manifest.
- Runner images: every platform records the hosted image version it was
  validated on in `ci/gates.toml`.
- Test runner and lint tools (`cargo-nextest`, `actionlint`, `shellcheck`,
  `cargo-deny`, `cargo-llvm-cov`, `cargo-mutants`) are pinned with versions, and
  where a release archive exists with per-platform SHA-256 digests, in the
  manifest's `[tools.*]` tables; `cargo xtask ci install-tool` verifies the digest
  before use. The versions are listed in [localdev.md](localdev.md).
- Every action in every workflow is pinned to a full commit SHA with its release
  as a trailing comment (`# v7.0.1`); `dtolnay/rust-toolchain` is pinned to the
  `stable` branch head and always given an explicit `toolchain:`. The workflow lint
  rejects a tag or branch reference. Update a pin by resolving the new tag to its
  commit and editing the manifest or renderer, not the YAML.
- Caches (`actions/cache`) are restored by an exact key built from the schema,
  build cell, platform image, triple, toolchain, profile, features, packages,
  nextest pin, manifest version, `Cargo.lock`, `rust-toolchain.toml` and
  `.cargo/config.toml`, with no restore-key fallback, and saved only on pushes to
  `main`. Release builds use no cache at all.

## Runners and trust

| Label set | Used by | Registered by |
|---|---|---|
| hosted `ubuntu-24.04`, `ubuntu-24.04-arm`, `ubuntu-22.04`, `macos-15`, `windows-2025` | every workflow job, including `release.yml` | GitHub |

No workflow uses a self-hosted runner, an environment, a release secret or a
local `uses: ./` action in `release.yml`; the workflow lint rejects each.

- Hosted VMs cannot boot a guest (macOS guests cannot nest Hypervisor.framework).
  Guest boot is therefore done by the release command on the operator's Apple
  Silicon Mac and on a Linux KVM host reached over SSH, not by a runner. Those
  hosts execute the already attested bytes from a prebuilt, digest-bound kit; no
  checkout, Cargo or compile happens on them, and no GitHub token is sent to
  the Linux host. Their results are operator evidence, not Actions attestations.
- `release.yml` has top-level `contents: read`. Only the `produce` job holds
  `id-token` and `attestations` write, and `validate` only reads. Neither can
  create a release; the operator's login does that.
- Before a long run the release command checks, for every local workflow call,
  that the callee's permissions are within the caller's grant (the
  permission-closure check), and that the dispatched workflows match the pinned
  plan.
- Require approval for fork pull-request workflows from all outside
  contributors, and do not send secrets or write tokens to fork pull requests.
- Top-level `permissions: contents: read`; write scopes are granted per job and
  never to a job reachable from a pull request. No `ci.yml` job can write.
- Push-triggered `ci.yml` runs only for `main`; branches the release command
  creates (`release/`, `rehearsal/`, `awman-release-state/`) never enqueue push CI.

## Repository settings

The release procedure requires no organization runner group, workflow
allowlist, release environment, branch protection, PR approval rule, merge
queue, tag ruleset or promotion App. It needs a `gh` login with repository
write, which preflight checks against the actual rules. The command lands the
validated release commit itself (fast-forward, or a server-side merge commit
when `main` moved) and creates the tag with that login.

The default `GITHUB_TOKEN` should be read-only; allow only pinned actions; keep
`timing-*` artifacts for at least 90 days; create the `flaky-test` issue label.

## Versioning

- Semantic versioning. Major version bumps reserved for incompatible CLI or on-disk format changes.
- The `Cargo.toml` `version` is the source of truth. The release tag is `v<Cargo version>`.
- Release tags are plain `vX.Y.Z`. Pre-release suffixes (`v1.2.3-rc1`) are not supported: the release command refuses any other version.
- `docs/releases/v<version>.md` MUST exist before a release starts. `make release` publishes it as the GitHub Release body.

## Publishing

- Binaries: GitHub Releases. Each release has four executables
  (`awman-linux-amd64`, `awman-linux-arm64`, `awman-macos-arm64`,
  `awman-windows-amd64.exe`) and `SHA256SUMS`. Every executable carries a GitHub
  build provenance attestation; users verify it as described in
  `docs/11-runtimes.md`.
- A release is built once, validated as those exact bytes, sealed as a draft and
  published by turning that draft into a release after one confirmation. A
  published tag and published assets are never moved or replaced. The whole
  procedure, recovery and rollback are in [release.md](release.md).
- Source: pushed to `main` after PR review, or landed unchanged by `make release`
  from the validated release commit.
- No crate is published to crates.io.

## Required gates before merge

- The `gate` check from `ci.yml` passes . It covers lint, hygiene, `unit` and `component` on Linux,
  macOS and Windows, the Docker `boundary` tier and the static build smoke.
- `make architecture-lint` is clean (enforced by the `lint` job).

## Local pre-push parity

Run `make pre-push` before pushing: `make check` (every lint plus the unit tier of
the crates you changed) and `make test` (the `unit` and `component` tiers for
your host, both backends, with the same pinned runner and profile CI uses). The
`boundary`, `guest` and `artifact` tiers need named host resources; see
[localdev.md](localdev.md). A CI failure is reproduced from its failure record,
see [testing.md](testing.md#reproducing-a-ci-failure).

## Evidence limits

The two required native checks run only through `make release`, on exact attested bytes, and a missing, blocked or short result prevents sealing. No hosted-Mac compilation or Linux pass is native runtime proof. `make validate-native` is a broad operator diagnostic whose evidence directory is checked with `cargo xtask native-validation verify DIR`; it is not part of a release and cannot authorize one. Guest fixtures are pinned to published, architecture-specific archives; a pending pin blocks. Native results are operator evidence, not Actions attestations. Real native boot and upgrade, SSH transport, the Windows firewall and complete publication timing are measured separately in [release-measurements.md](release-measurements.md), not claimed by the local tests.

Coverage and mutation results are diagnostics. Performance reports must distinguish cold builds, warm runs and queue time; a single run does not establish p95 or a flake rate. Dependency advisories are reviewed separately from the licenses, bans and sources gate.
