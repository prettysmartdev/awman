# Continuous Integration and Deployment

Platform: GitHub Actions

## Pipelines

### `Tests` (`.github/workflows/test.yml`)

Runs on every push and pull request. Three jobs:

| Job | Runner | What it runs |
|---|---|---|
| `fast` | `ubuntu-latest` | `make architecture-lint`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `make test-fast`. Hermetic — no Docker, no real git, no real network. Should finish in under two minutes warm. |
| `full-linux-docker` | `ubuntu-latest` | `make test-full` against the runner's Docker daemon (`test-full` sets `AWMAN_TEST_DOCKER=1`; without it the Docker tests see Docker as not installed). Includes the `docker_*`, `real_git_*`, and `real_network_*` integration tests. Depends on `fast`. |
| `build-macos` | `macos-latest` | `cargo build --release` and `make test-fast`. Smoke-tests cross-platform compilation; does not run Docker tests (macOS hosted runners lack Docker). Depends on `fast`. |
| `builtin-build` | native Linux ARM64 runner | Opt-in `builtin-runtime` feature build and hermetic runtime/import tests. The target's kernel payload record must be `verified`; this is a build gate, not proof of guest execution. |
| `builtin-integration` | self-hosted/native Apple Silicon, Linux ARM64, Linux x86_64 | Opt-in guest integration matrix (`AWMAN_TEST_BUILTIN=1`): same-executable worker boot, image import, execution, and artifact helper/dependency scans. Linux runners require accessible KVM; Apple Silicon requires Hypervisor.framework entitlement. The Apple and x86_64 jobs stay disabled/blocked until native kernel extraction and manifest verification succeed. |

Cargo's registry, git cache, and `target/` are cached per-OS to keep warm runs fast.

The builtin integration gate is separate from `test-fast` and `full-linux-docker`;
those jobs never boot a guest by default. Each native job first checks host
architecture, verified payload hashes, and KVM or Apple entitlement access. A
missing prerequisite or unavailable runner is reported as **SKIP** or
**BLOCKED**, with the reason and unblocking condition recorded; it cannot be
reported as a successful execution test. Required hardware coverage is Apple
Silicon macOS and both Linux architectures (ARM64 and x86_64), not cross-builds
on another host.

Changes to SQLx, rusqlite, or native SQLite rerun the SQLite resolution gate on
supported native targets: one bundled `libsqlite3-sys`, combined link, awman
store tests, msb migrations, cross-driver transactions, and old/new catalog
round trips. Missing fixtures or target hardware block the relevant subcheck.
Builtin artifacts also receive minimal-PATH/offline execution checks, dynamic
dependency inspection, and helper-extraction scans; no runtime step may depend
on developer spike scripts or temporary files.

### `Release` (`.github/workflows/release.yml`)

Triggered on tag pushes matching `v[0-9]+.[0-9]+.[0-9]+`. Builds a release binary for each supported target (Linux x86_64, Linux arm64, macOS x86_64, macOS arm64, Windows x86_64), uploads each as an artifact, then assembles a GitHub Release with the matching `docs/releases/<tag>.md` as the body.

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

## Local pre-push parity

Run `make pre-push` before pushing. It runs the same pre-merge gate as the `fast` CI job: architecture-lint, fmt check, clippy with deny-warnings, and `cargo test`. The `full-linux-docker` and `build-macos` jobs only run in CI.

## Known limitations / future work

- The `full-linux-docker` job does not currently build inside an isolated Docker network — it runs against the host runner's daemon.
- The Windows build is exercised only at release time, not on every PR. A Windows PR job is tracked as a future improvement.
- Coverage reporting is not yet wired up; see `aspec/work-items/0076-deferred-parity-and-e2e-tests.md` for the planned coverage delta.
