# Continuous Integration and Deployment

The manifest test system is the only supported path. See [replacement-ledger.md](replacement-ledger.md) for coverage decisions and evidence. Native Apple Silicon acceptance remains open until the maintainer's packet is independently reviewed.

The Linux x86_64 builtin PR build and release jobs run on Ubuntu 22.04 to
preserve the published GNU executable's glibc 2.35 baseline. Default-feature
Linux PR suites remain on Ubuntu 24.04, and the musl static smoke remains
separate. Candidate creation rejects any newer or unrecognized glibc symbol
requirement before publishing its build record.

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
| `main` | pushes to `main`, or an authenticated dispatch for an open same-repository `release/vX.Y.Z` PR head | the full hosted `pr` gate; no hardware runner is required |
| `nightly` | 03:23 UTC schedule | everything in `main`, plus hosted soak suites and the nightly modes below |
| `candidate` | release candidate validation | everything in `main`, plus the `artifact` tier against each exact release executable, signed offline boot, and the required guest, Apple Containers, Docker sandbox, guest soak, and builtin unit suites on the release Mac |
| `promote` | release promotion | requires a passed `candidate` gate; see [release.md](release.md) |

**Selection.** On a pull request only the suites whose declared `paths` changed
are selected, with the exceptions printed in the manifest's `[selection]` table
(`Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `.cargo/`, `ci/`,
`.github/`, `.config/`, `xtask/`, `crates/awman-test-support/`, `Makefile`,
`tools/`, `third_party/` and anything unknown) which select the full `pr` set.
Unit, lint, doctest and parity suites are always selected. Merge-group and
`main` runs always run the full set. A plan that fails, a missing base commit or
a truncated diff is non-green, never an empty selection.

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
| `ci.yml` (generated) | pull request, merge group, push to `main`, `workflow_call`, `workflow_dispatch` | the `pr` and `main` gates, and the base of `nightly` and `candidate` |
| `nightly.yml` (generated) | daily schedule, `workflow_dispatch` | the `nightly` gate and its modes |
| `candidate.yml` | `workflow_dispatch` or `workflow_call` from `main` only | build once and validate those exact bytes |
| `promote.yml` | dispatched by `make release` after publication; called by rehearsals | verify the published or rehearsal bytes |
| `fixtures.yml` | `workflow_dispatch` from a release branch | pull the reviewed Alpine digest, derive and attest both guest fixtures, publish an immutable fixture release |

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
- `ci.yml` selects no hardware cells. Release candidate validation requires
  native evidence and fails closed when the release runner is unavailable.
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

`full` (the `nightly` gate through `ci.yml`), plus:

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
- `repro-build`: a clean-room unsigned build compared with the latest
  candidate's unsigned digests; non-blocking.
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
| hosted `ubuntu-24.04`, `ubuntu-24.04-arm`, `macos-15`, `windows-2025` | every hosted cell | GitHub |
| `self-hosted, macOS, ARM64, awman-release` | the native cells of `candidate.yml` and the verify job of `promote.yml` | `make release`, for one release, or an already-online release runner |

Hosted `main` and nightly jobs cannot take the release runner.

- Hosted VMs cannot boot a guest (macOS guests cannot nest Hypervisor.framework),
  so every guest cell needs bare-metal Apple Silicon.
- Self-hosted release jobs run from the `main` candidate workflow.
  The release runner group is restricted to this repository and the workflow
  paths `candidate.yml` and `promote.yml` at `refs/heads/main`.
- Release validation checks the native runner before expensive work (disk,
  `kern.hv_support`, toolchain, pinned macOS build, fixture digests, Apple
  Containers). A missing capability blocks the release gate. Native jobs use
  `cargo xtask ci sweep`, which fails if processes, VMs, images or temp roots
  of the run remain.
- Require approval for fork pull-request workflows from all outside
  contributors, and do not send secrets or write tokens to fork pull requests.
- Top-level `permissions: contents: read`; write scopes are granted per job and
  never to a job reachable from a pull request. No `ci.yml` job can write.

## Repository settings

The release procedure does not require branch protection, PR approval rules,
a merge queue, a tag ruleset, or a promotion App. The operator must squash
merge the release PR, and `make release` checks the merged commit and exact
tree before candidate validation. The candidate gate verifies the same
commit before the operator's `gh` login tags and publishes it.

Keep a workflow-restricted runner group for `awman-release`, registered only
during release validation or supplied by an already-online release Mac. Keep
the `release` environment limited to the `main` branch. The default
`GITHUB_TOKEN` should be read-only; allow only pinned actions; keep `timing-*`
artifacts for at least 90 days; create the `flaky-test` issue label.

## Versioning

- Semantic versioning. Major version bumps reserved for incompatible CLI or on-disk format changes.
- The `Cargo.toml` `version` is the source of truth. The release tag is `v<Cargo version>`.
- Release tags are plain `vX.Y.Z`. Pre-release suffixes (`v1.2.3-rc1`) are not supported: the release operator refuse any other version.
- `docs/releases/v<version>.md` MUST exist before a release starts. `make release` publishes it as the GitHub Release body.

## Publishing

- Binaries: GitHub Releases. Each release has four executables
  (`awman-linux-amd64`, `awman-linux-arm64`, `awman-macos-arm64`,
  `awman-windows-amd64.exe`) and `SHA256SUMS`. Every executable carries a GitHub
  build provenance attestation; users verify it as described in
  `docs/11-runtimes.md`.
- A release is built once, validated as those exact bytes, and published by
  turning the validated draft into a release. A published tag and published assets
  are never moved or replaced. The whole procedure, recovery and rollback are in
  [release.md](release.md).
- Source: pushed to `main` after PR review.
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

Native Apple Silicon acceptance remains open in WI144. The maintainer runs `make validate-native` and returns its packet for independent review. No hosted-Mac compilation or Linux pass is native runtime proof. The global guest fixture remains pending until the reviewed archive is published and pinned. Required native gates stay blocked without those inputs and admitted hardware.

Coverage and mutation results are diagnostics. Performance reports must distinguish cold builds, warm runs and queue time; a single run does not establish p95 or a flake rate. Dependency advisories are reviewed separately from the licenses, bans and sources gate.
