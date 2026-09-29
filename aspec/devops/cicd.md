# Continuous Integration and Deployment

Platform: GitHub Actions

## Pipelines

### `Tests` (`.github/workflows/test.yml`)

The first three jobs run on every push and pull request; the rest are opt-in
(`workflow_dispatch`/`schedule`, or — for the two unsupported-target jobs —
also `pull_request`) because they need native hardware this repo's hosted
runners don't have:

| Job | Runner | What it runs |
|---|---|---|
| `fast` | `ubuntu-latest` | `make architecture-lint`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `make test-fast`. Hermetic — no Docker, no real git, no real network. Should finish in under two minutes warm. |
| `full-linux-docker` | `ubuntu-latest` | `make test-full` against the runner's Docker daemon (`test-full` sets `AWMAN_TEST_DOCKER=1`; without it the Docker tests see Docker as not installed). Includes the `docker_*`, `real_git_*`, and `real_network_*` integration tests. Depends on `fast`. |
| `build-macos` | `macos-latest` | `cargo build --release` and `make test-fast`. Smoke-tests cross-platform compilation; does not run Docker tests (macOS hosted runners lack Docker). Depends on `fast`. |
| `builtin-build` | Linux ARM64 (`ubuntu-24.04-arm`, hosted) | Fetches and verifies the payload, builds the `builtin-runtime` feature, inspects dynamic dependencies, and runs `make test-builtin` — on a hosted runner with no KVM, so `builtin_hw_*` tests SKIP. A build/hermetic gate, not proof of guest execution. Runs on every push/PR (the only builtin job that does). |
| `builtin-hardware` | self-hosted, per-platform (`linux/arm64/kvm`, `linux/x64/kvm`, `macOS/ARM64/hypervisor`) | Opt-in real-guest matrix (`AWMAN_TEST_BUILTIN=1 AWMAN_TEST_BUILTIN_REQUIRE_HW=1`): checks hardware prerequisites first and **fails** (never skips) if they're missing, builds a disposable fixture image, then runs `make test-builtin` for real — same-executable worker boot, image import, execution. Only on `workflow_dispatch` or the nightly `schedule`. |
| `native-typed-provider` | same self-hosted matrix | Clean-checkout gate: `tools/native-builtin-ci.sh <triple>` builds and boots the typed provider from a fresh isolated `CARGO_HOME`/target dir, then `tools/oci-runtime-spike/sqlite-resolution/checks.sh auto` reruns the SQLite resolution checks on that target. Uploads SQLite and native-payload evidence as artifacts. Only on `workflow_dispatch` or `schedule`. |
| `existing-backend-unsupported-targets` | Windows x86_64 (hosted), Intel Mac (self-hosted) | Builds the default backends with the `builtin-runtime` feature **disabled** and asserts the precise "unsupported target" refusal a `builtin` selection reports there. Runs on `pull_request` too (no native prerequisite beyond the target toolchain). |

Cargo's registry, git cache, and `target/` are cached per-OS to keep warm runs fast.

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
  target (Linux x86_64, macOS x86_64, Windows x86_64) and records
  size/startup/RSS measurements and a dependency/`.msbver`/helper-extraction
  scan of the optimized artifact.
- `release-linux-arm64-builtin` and `release-macos-builtin` build the
  Linux ARM64 and Apple Silicon assets **with** the builtin runtime from a
  verified native payload, on isolated Cargo/target directories. No signing or
  notarization steps, identities, credentials or distribution account are
  required. The macOS job uploads the binary directly.
  Both then trace and boot the exact distributed artifact (not a stand-in
  driver) under an offline session wrapper before it's uploaded.
- `release` assembles the GitHub Release from all uploaded artifacts plus the
  matching `docs/releases/<tag>.md` body, and **depends on both builtin gates
  succeeding** — a release cannot publish while either builtin artifact fails
  its native boot gate.

## Versioning

- Semantic versioning. Major version bumps reserved for incompatible CLI or on-disk format changes.
- The `Cargo.toml` `version` is the source of truth. The release workflow assumes the tag matches it (`v<Cargo version>`).
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
