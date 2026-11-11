# Continuous Integration and Deployment

WI 0144 remains an incomplete shadow migration. Static checks passing do not
certify a working production evidence chain or complete legacy replacement;
see [replacement-ledger.md](replacement-ledger.md) for code blockers and cutover.

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
| guest | 300 | tempdir, loopback, subprocess, awman-binary, hypervisor, builtin-payload, guest-fixtures, previous-release |
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
| `main` | pushes to `main` | everything in `pr`, plus the `guest` tier on the CI Mac (hardware) |
| `nightly` | 03:23 UTC schedule | everything in `main`, plus the Apple Containers and `sbx` boundary suites, the soak suites, and the nightly modes below |
| `candidate` | release candidate validation | everything in `main`, plus the `artifact` tier against each exact release executable, the signed offline boot of the macOS asset, and the `guest` tier on the release Mac |
| `promote` | release promotion | requires a passed `candidate` gate; see [release.md](release.md) |

**Selection.** On a pull request only the suites whose declared `paths` changed
are selected, with the exceptions printed in the manifest's `[selection]` table
(`Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `.cargo/`, `ci/`,
`.github/`, `.config/`, `xtask/`, `crates/awman-test-support/`, `Makefile`,
`tools/`, `third_party/` and anything unknown) which select the full `pr` set.
Unit, lint, doctest and parity suites are always selected. Merge-group and
`main` runs always run the full set. A plan that fails, a missing base commit or
a truncated diff is non-green, never an empty selection.

**Evidence.** Each test job writes `evidence.json` (one outcome per expected
test id, not a total), JUnit and failure records. The `gate` job reconstructs
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
| `candidate.yml` | `workflow_dispatch` or `workflow_call` from `main` only | build once, validate those exact bytes, hand off to promotion |
| `promote.yml` | called by `candidate.yml` | tag, publish the validated draft, verify the published bytes |
| `test.yml` | pull request, push to `main` | status shim keeping the legacy required check names alive until the required check is `gate` |
| `release.yml` | `v[0-9]+.[0-9]+.[0-9]+` tag push | the legacy tag-driven release, kept as the fallback until the pipeline is activated (see [release.md](release.md)) |

### `ci.yml` job graph

`plan` → `lint`, `build-*`, `test-*`, `boundary-linux-docker`, `static-musl`,
`web` → `gate`.

- `plan` checks out the commit, runs `cargo xtask gates check`, resolves the
  gate from the event, selects suites from the changed paths and emits the build
  matrix and shards. It also decides whether hardware cells may run (see
  *Runners and trust*).
- `build-<build>` compiles one build cell, writes a nextest archive, the
  expected-test list and the digests of both. `build-linux-x64-default`,
  `build-macos-arm64-default`, `build-windows-x64-default`,
  `build-linux-arm64-builtin`, `build-macos-arm64-ci-guest-builtin`.
- `test-<tier>-<platform>` downloads the archive, verifies its SHA-256 against
  both the job output and `archive/digests.json`, then runs the shard's suites
  with the pinned nextest. Jobs need only `plan` and their own build.
- `lint` runs fmt, clippy, architecture lint, hygiene, shell lint, workflow lint,
  `cargo deny`, the third-party notice check and doctests.
- `hardware-blocked` is a hosted job that fails visibly when `main` or `nightly`
  selects hardware cells but the CI guest runner is not admitted. A green `main`
  therefore always means the guest tier ran.
- `gate` (job and status name `gate`) aggregates evidence and writes the step
  summary: tier × platform matrix of passed/failed/blocked/skipped/not-selected,
  slowest tests, quarantine and override state, cache statistics and the
  `reproduce` line of every failure.

### `nightly.yml`

`full` (the `nightly` gate through `ci.yml`), plus:

- `flake-hunt`: every tier five times with recorded shuffle seeds under CPU
  contention; any intermittent failure opens a `flaky-test` issue with its
  failure record.
- `seeded-defects`: applies known defects from `ci/seeded-defects/` and the
  pipeline faults (missing target, renamed job, skipped hardware) and asserts
  the right cell turns red. The patch directory is not yet populated, and the job fails closed while it is empty.
- `mutation`: `cargo-mutants` on `src/data` and `src/command`, sampled.
- `coverage`: `cargo-llvm-cov` per layer; diagnostic.
- `repro-build`: a clean-room unsigned build compared with the latest
  candidate's unsigned digests; non-blocking.
- `audit`: `cargo deny check advisories`.
- `rehearsal`: `candidate.yml` with `dry_run: true`, a full dry run of the
  release pipeline that publishes nothing (see [release.md](release.md)).
- `ci-trends`: p50/p95 per gate, queue time, flake rate and cache hit rate
  from the last 30 days of `timing-*` artifacts.

### `test.yml` (status shim)

Four jobs keep the legacy required check names until the required check is
switched to `gate`: `fast` (`make check`, `make test`, `make legacy-test`),
`full-linux-docker` (`make test-boundary` with the runner's Docker, and
`make legacy-test LEGACY_DOCKER=1`), `web` (`make test-web`) and `build-macos`
(`make architecture-lint` and a release build). They call the same Make targets
as a developer and contain no logic of their own. The file and its
`[[legacy_fallback]]` row are deleted at cutover.

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
| `self-hosted, macOS, ARM64, awman-ci-guest` | the `guest` and Apple boundary cells of `main` and `nightly` | a long-lived CI Mac |
| `self-hosted, macOS, ARM64, awman-release` | `candidate.yml` and the verify job of `promote.yml` | `make release`, for one release, or an already-online release runner |
| `self-hosted, macOS, ARM64, hypervisor` | legacy `release.yml` only | the legacy runner script |

The label sets are disjoint, so a CI job cannot take the release runner.

- Hosted VMs cannot boot a guest (macOS guests cannot nest Hypervisor.framework),
  so every guest cell needs bare-metal Apple Silicon.
- Self-hosted jobs run only when the plan admits hardware: the gate is not `pr`,
  the event is a push, schedule or dispatch, the ref is `refs/heads/main`, the
  `ref` input is empty or equal to the triggering commit, and the repository
  variable `AWMAN_CI_GUEST_RUNNER` is `enabled`. Each job repeats the event and
  ref test. A pull request edits its own workflow file, so this is defence in
  depth; the real boundary is the runner group, which must be restricted to this
  repository and the workflow paths `ci.yml` and `nightly.yml` at
  `refs/heads/main`.
- Every self-hosted job starts with `cargo xtask ci runner-preflight` (disk,
  `kern.hv_support`, toolchain, fixture digests, Apple Containers) and exits
  `BLOCKED` with the reason before any expensive step. It ends with
  `cargo xtask ci sweep`, which fails the job if processes, VMs, images or temp
  roots of the run remain.
- Require approval for fork pull-request workflows from all outside
  contributors, and do not send secrets or write tokens to fork pull requests.
- Top-level `permissions: contents: read`; write scopes are granted per job and
  never to a job reachable from a pull request. No `ci.yml` job can write.

## Required checks and repository settings

The required status for `main` is the single check `gate` from `ci.yml`, with
the merge queue enabled; `ci.yml` triggers on `merge_group`. `main` may not be
force-pushed or deleted, and creation, update and deletion of `v*` tags is
restricted to the release promotion identity. The expected values are recorded in
`ci/branch-protection.expected.json`. Until the cutover switches the required
check from the four legacy `test.yml` contexts to `gate`, both run.

`gates check` cannot read repository settings. Settings that must be applied by
a maintainer and confirmed against the repository:

1. Branch protection / ruleset for `main` as above.
2. Runner groups for `awman-ci-guest` and `awman-release`, with
   `AWMAN_CI_GUEST_RUNNER=enabled` once the CI Mac is registered.
3. A `release` environment limited to the `main` branch, the release GitHub App
   (`AWMAN_RELEASE_APP_CLIENT_ID`, `AWMAN_RELEASE_APP_PRIVATE_KEY`) used to create
   the tag and publish, and `AWMAN_RELEASE_REHEARSAL=enabled` to turn on the
   nightly rehearsal.
4. Default `GITHUB_TOKEN` read-only; allow only pinned actions; keep `timing-*`
   artifacts for at least 90 days; create the `flaky-test` issue label.

## Versioning

- Semantic versioning. Major version bumps reserved for incompatible CLI or on-disk format changes.
- The `Cargo.toml` `version` is the source of truth. The release tag is `v<Cargo version>`.
- Release tags are plain `vX.Y.Z`. Pre-release suffixes (`v1.2.3-rc1`) are not supported: both `release.yml` and the release operator refuse any other version.
- `docs/releases/v<version>.md` MUST exist before a release starts. The release workflow publishes it as the GitHub Release body.

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

- The `gate` check from `ci.yml` passes (and, until cutover, the legacy
  `test.yml` checks). It covers lint, hygiene, `unit` and `component` on Linux,
  macOS and Windows, the Docker `boundary` tier and the static build smoke.
- `make architecture-lint` is clean (enforced by the `lint` job).

## Local pre-push parity

Run `make pre-push` before pushing: `make check` (every lint plus the unit tier of
the crates you changed) and `make test` (the `unit` and `component` tiers for
your host, both backends, with the same pinned runner and profile CI uses). The
`boundary`, `guest` and `artifact` tiers need named host resources; see
[localdev.md](localdev.md). A CI failure is reproduced from its failure record,
see [testing.md](testing.md#reproducing-a-ci-failure).

## Known limitations and open work

- The generated workflows and the release pipeline have not yet run on GitHub.
  Hardware cells depend on the CI guest Mac, and the pinned guest fixture and
  previous-release digests are still `pending` in `ci/gates.toml`; until they are
  published, the guest and artifact tiers report `BLOCKED` rather than pass. The
  acceptance matrix ([acceptance-matrix.md](acceptance-matrix.md)) lists every
  cell that is still blocked and its owner.
- Some older integration targets remain until their replacements have shadow
  evidence; they run through `make legacy-test` and are listed with their removal
  conditions as `[[legacy_fallback]]` rows ([replacement-ledger.md](replacement-ledger.md)).
- Coverage and mutation results are diagnostic; they do not block a gate.
- `cargo deny check advisories` runs nightly and is expected to report current
  advisories in the dependency tree until the lockfile is updated.
