# Continuous Integration and Deployment

Platform: GitHub Actions

## Pipelines

### `Tests` (`.github/workflows/test.yml`)

The workflow runs on every pull request and on pushes to `main` only. A
branch push is tested through its pull request, which checks the merge with
`main`; a branch without a PR gets CI by opening one (a draft is fine) or by
`gh workflow run test.yml --ref <branch>`. Pushing to every branch would run
the suite twice per PR update.

The first three jobs run on every such event; the rest are opt-in
(`workflow_dispatch`/`schedule`, or — for the two unsupported-target jobs —
also `pull_request`) because they need native hardware this repo's hosted
runners don't have:

| Job | Runner | What it runs |
|---|---|---|
| `fast` | `ubuntu-26.04` | `make architecture-lint`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `make test-fast`. Hermetic — no Docker, no real git, no real network. Should finish in under two minutes warm. |
| `full-linux-docker` | `ubuntu-26.04` | `make test-full` against the runner's Docker daemon (`test-full` sets `AWMAN_TEST_DOCKER=1`; without it the Docker tests see Docker as not installed). Includes the `docker_*`, `real_git_*`, and `real_network_*` integration tests. Depends on `fast`. |
| `build-macos` | `macos-latest` | `cargo build --release` and `make test-fast`. Smoke-tests cross-platform compilation; does not run Docker tests (macOS hosted runners lack Docker). Depends on `fast`. |
| `builtin-build` | Linux ARM64 (`ubuntu-24.04-arm`, hosted) | Fetches and verifies the payload, builds the `builtin-runtime` feature, inspects dynamic dependencies, and runs `make test-builtin` — on a hosted runner with no KVM, so `builtin_hw_*` tests SKIP. A build/hermetic gate, not proof of guest execution. Runs on every PR and push to `main` (the only builtin job that does). |
| `builtin-hardware` | self-hosted `macOS/ARM64/hypervisor` (the released platform; WI 0124 re-adds `linux/arm64/kvm` and `linux/x64/kvm`) | Opt-in real-guest matrix (`AWMAN_TEST_BUILTIN=1 AWMAN_TEST_BUILTIN_REQUIRE_HW=1`): checks hardware prerequisites first and **fails** (never skips) if they're missing, builds a disposable fixture image, then runs `make test-builtin` for real — same-executable worker boot, image import, execution. `make test-builtin` builds the guest drivers explicitly and ad-hoc signs awman and the drivers with the hypervisor entitlement (`tools/macos-sign-test-artifacts.sh`) between building and running. It also builds the previous-protocol awman from git history for the old-to-new upgrade test (`tools/build-previous-builtin-awman.sh`), so the job checks out full history (`fetch-depth: 0`). Only on `workflow_dispatch` or the nightly `schedule`. |
| `native-typed-provider` | same self-hosted macOS runner | Clean-checkout gate: `tools/native-builtin-ci.sh <triple>` builds and boots the typed provider from a fresh isolated `CARGO_HOME`/target dir (with the previous-protocol awman for the upgrade test, so this job also uses `fetch-depth: 0`), then `tools/oci-runtime-spike/sqlite-resolution/checks.sh auto` reruns the SQLite resolution checks on that target. Uploads SQLite and native-payload evidence as artifacts. Only on `workflow_dispatch` or `schedule`. |
| `existing-backend-unsupported-targets` | Windows x86_64 (hosted) | Builds the default backends with the `builtin-runtime` feature **disabled** and asserts the precise refusal a `builtin-experimental` selection reports there (the runtime is released for Apple Silicon only). Runs on `pull_request` too (no native prerequisite beyond the target toolchain). |

Workspace builds select the root library, `awman-cli`, and `awman-host-shims`;
only `awman-cli` has a binary target, named `awman`. Workspace tests include
the relocated worker and native transport unit tests. The
`static-workspace-build` job builds and smoke-tests the single
`x86_64-unknown-linux-musl` release executable with and without the builtin feature.
The builtin and unsupported-target matrices use the same workspace. The musl
feature build reports builtin as unsupported and excludes the GNU-only SDK dependencies.

Cargo's registry, git cache, and `target/` are cached per-OS to keep warm runs fast.

Every action in `test.yml` and `release.yml` is pinned to a full commit SHA, with the release it was resolved from as a trailing comment (`# v7.0.1`); `dtolnay/rust-toolchain` is pinned to its `stable` branch head and always given an explicit `toolchain:`. Update a pin by resolving the new tag with `git ls-remote` (the peeled `^{}` commit for an annotated tag).

The hardware-gated jobs are separate from `fast` and `full-linux-docker`; those
never boot a guest. Each native job first checks host architecture, verified
payload hashes, and KVM or Apple Hypervisor.framework support (Linux: fails the job if
`/dev/kvm` isn't read/write accessible; macOS: fails if not Apple Silicon with
Hypervisor.framework support) — a missing
prerequisite or unavailable runner is reported as **BLOCKED**, with the reason
recorded, and cannot be reported as a successful execution test. Required
hardware coverage is Apple Silicon macOS and both Linux architectures (ARM64
and x86_64), not cross-builds on another host; only Linux ARM64 currently has
a verified payload record, so the x86_64 and Apple Silicon entries in these
matrices still fail at the payload-verification step until that hardware
records genuine extraction/verification evidence.

Changes to SQLx, rusqlite, or native SQLite rerun the SQLite resolution gate
(`native-typed-provider`) on supported native targets: one bundled
`libsqlite3-sys`, combined link, awman store tests, msb migrations,
cross-driver transactions, and old/new catalog round trips. Missing fixtures
or target hardware block the relevant subcheck. Builtin artifacts also receive
minimal-PATH/offline execution checks, dynamic dependency inspection, and
helper-extraction scans; no runtime step may depend on developer spike scripts
or temporary files.

### `Release` (`.github/workflows/release.yml`)

Triggered on tag pushes matching `v[0-9]+.[0-9]+.[0-9]+`.

- `build` builds the ordinary (non-builtin) release binary for each ordinary
  target (Linux x86_64, Linux arm64, Windows x86_64) and records
  size/startup/RSS measurements and a dependency/`.msbver`/helper-extraction
  scan of the optimized artifact.
- `release-macos-builtin` builds the Apple Silicon asset **with** the
  experimental builtin runtime from a verified native payload, on isolated
  Cargo/target directories, then **ad-hoc signs** it with exactly the
  entitlements in `packaging/macos/awman.entitlements`
  (`com.apple.security.hypervisor`) via `tools/macos-sign.sh`, which fails if
  the embedded entitlements differ from that file. There is no Developer ID,
  notarization, signing secret or distribution account. It records the
  SHA-256 of the unsigned build (for reproducibility) and of the signed asset,
  then traces and boots the exact **signed** artifact (not a stand-in driver)
  under an offline session wrapper (`tools/release-offline-session.sh` is the
  reference implementation) before uploading it. Linux release assets ship
  without the builtin runtime until WI 0124.
- `release` assembles the GitHub Release from all uploaded artifacts plus the
  matching `docs/releases/<tag>.md` body, and **depends on the builtin gate
  succeeding** — a release cannot publish while the signed Apple Silicon
  artifact fails its native boot gate.

### Release runner (`make release`)

No hosted macOS runner can boot a guest (every hosted Mac is itself a VM, and
macOS guests cannot nest Hypervisor.framework), so `release-macos-builtin`
needs a bare-metal Apple Silicon Mac. `make release VERSION=vX.Y.Z` provides
one on demand through `tools/release-runner.sh`:

- **When.** `RELEASE_RUNNER=auto` (default) uses an online runner with the
  `hypervisor` label if one exists, and otherwise registers the Mac running the
  script, provided it is Apple Silicon with `kern.hv_support=1`.
  `RELEASE_RUNNER=local` requires this Mac; `RELEASE_RUNNER=none` never
  registers one.
- **Provisioning, on every release.** `release-runner.sh provision` installs
  or refreshes what the job expects in `/opt/awman` (creating it with `sudo`
  the first time and handing it to the invoking user, so later runs need no
  sudo): a copy of `tools/release-offline-session.sh`, the guest fixture
  (`fixture-oci.tar`, built and cached in `~/.awman/release-runner/fixtures`
  by `tools/builtin-hw-fixtures.sh`; the first build needs Docker or Apple
  Containers running), and the two wrappers the job calls,
  `release-offline-session.sh` and `release-workload.sh`. The job runs them with
  a cleared environment, so each wrapper is a fixed command line with the
  fixture path baked in. Only changed files are rewritten. A file there that
  provisioning did not write is kept as `<name>.bak`. The workload wrapper
  runs the same offline session, which records guest RSS, rather than a
  heavier git/build workload.
- **Short work directory.** Jobs run under `/tmp/awr` rather than the runner's
  install directory, because the job's builtin state directories hold Unix
  sockets and macOS caps socket paths at 103 bytes. `up` creates it private
  (mode 0700) and refuses a path that is a symlink or someone else's
  directory; only a directory it created is deleted afterwards.
- **Before the tag is pushed.** The script downloads the latest
  `actions/runner` osx-arm64 archive into `~/.awman/release-runner/cache`,
  refuses it unless its SHA-256 matches the checksum in that runner release's
  notes, registers it with a repository registration token (passed in the
  environment, never on a command line; labels
  `self-hosted, macOS, ARM64, hypervisor`, name `awman-release-<tag>-<host>`),
  starts it under `caffeinate`, and waits for it to come online. `Tests` runs,
  queued or in progress, with a `hypervisor` job still waiting for a runner
  would take it first, so they are listed and offered for cancellation. A
  `Tests` self-hosted job that reaches the release runner later (the nightly
  cron, say) fails its first step on any `awman-release-*` runner, freeing it
  in seconds. A runner that cannot register stops the release before
  anything is pushed.
- **During.** The script waits for the tag's `Release` run, re-running its
  failed jobs if the run already finished unsuccessfully, and follows it with
  `gh run watch` until every job has finished.
- **After.** An `EXIT` trap stops the runner's process group (SIGINT, then
  SIGKILL) and deregisters it, whether the run succeeded, failed or the script
  was interrupted; a second Ctrl-C does not interrupt that teardown. The group
  is signalled only while it still holds the runner's processes, so a saved
  pid reused after a reboot is never signalled. Re-running the script picks up
  where it stopped.

The runner executes jobs as the invoking user on that Mac, and accepts any
job whose labels it matches while it is online. The committed self-hosted
jobs in `test.yml` run only on `workflow_dispatch` and `schedule`, but a pull
request runs the workflow file from its own branch, so a fork PR could target
these labels. For a public repository, set **Settings → Actions → General → fork
pull request workflows** to require approval for every outside contributor
(the strictest option), and don't approve fork workflow runs during a release. Registration and removal need repository
admin rights for the `gh` login (so does listing runners for `RELEASE_RUNNER=auto`;
without it the script says so and suggests `RELEASE_RUNNER=none`).

## Versioning

- Semantic versioning. Major version bumps reserved for incompatible CLI or on-disk format changes.
- The `Cargo.toml` `version` is the source of truth. The release workflow assumes the tag matches it (`v<Cargo version>`).
- Release tags are plain `vX.Y.Z`. Pre-release suffixes (`v1.2.3-rc1`) are not supported: `release.yml` runs only on `v[0-9]+.[0-9]+.[0-9]+` tags, and `scripts/release.sh` refuses any other version.
- `docs/releases/v<version>.md` MUST exist before the tag is pushed. The release workflow inlines it as the GitHub Release body.

## Publishing

- Binaries: GitHub Releases.
- Source: pushed to `main` after PR review.
- No crate is published to crates.io today.

## Required gates before merge

- `Tests / fast` passes (lint + fmt + clippy + hermetic test run).
- `Tests / full-linux-docker` passes (real Docker + real git + real network tests).
- `Tests / build-macos` passes (cross-OS build smoke).
- `make architecture-lint` clean (enforced inside the `fast` job).

`Tests / builtin-build` and `Tests / existing-backend-unsupported-targets` also run on every pull request (see the job table above), but as build/hermetic and unsupported-target-refusal checks, not guest-execution proof — treat a green run there the same way, as a build gate.

## Local pre-push parity

Run `make pre-push` before pushing. It runs the same pre-merge gate as the `fast` CI job: architecture-lint, fmt check, clippy with deny-warnings, and `cargo test`. The `full-linux-docker` and `build-macos` jobs only run in CI.

## Known limitations / future work

- The `full-linux-docker` job does not currently build inside an isolated Docker network — it runs against the host runner's daemon.
- The Windows build is exercised only at release time, not on every PR. A Windows PR job is tracked as a future improvement.
- Coverage reporting is not yet wired up; see `aspec/work-items/0076-deferred-parity-and-e2e-tests.md` for the planned coverage delta.
