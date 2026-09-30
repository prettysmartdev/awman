# WI 0123 evidence: `builtin-experimental` on Apple Silicon

Final sweep recorded 2026-09-29 on Apple Silicon (macOS 26.6.2, arm64,
`kern.hv_support=1`, Apple Containers 0.12.0). Every log under
[`final/`](final/) starts with the SHA-256 of the WI 0123 code patch against
base `e86ddde0` (WI 0122). All logs in `final/` carry the same hash. The
clean-checkout log also records the commit of the throwaway clone that
contains exactly this tree. The Apple-store bridge code is unchanged since
WI 0122, whose [evidence](../0122-evidence/notes.md) (release matrix, bridge
suite, helper crash, sandboxed export) therefore applies unchanged.

## What changed in this item

| Area | Change |
|---|---|
| Runtime name and gate | `builtin` → `builtin-experimental` (`RuntimeSelection`); plain `builtin` is refused with a hint. `RELEASED_TARGETS = [macos/aarch64]` gates `open()` before any work; Linux stays compiled and unit-tested. Display name carries "(experimental)". `config set` validates runtime values; commands that need no runtime keep working with an invalid stored value. |
| Signing | `packaging/macos/awman.entitlements` (exactly `com.apple.security.hypervisor`); `tools/macos-sign.sh` signs and fails on any entitlement drift; `make build` signs on Apple Silicon; `make sign-macos`; `release.yml` signs the Apple Silicon asset, records unsigned and signed SHA-256, and boot-checks the signed file. The Linux arm64 asset ships without the feature; the Linux builtin release job and Linux hardware CI rows moved to WI 0124. |
| Test harness | `make test-builtin` builds the guest drivers explicitly (they were stale before) and signs awman, the drivers and their `deps/` originals; the hardware gate reports a missing entitlement as BLOCKED. `tools/release-offline-session.sh` is the reference offline-session wrapper; the macOS artifact check enforces (sandbox) instead of tracing when not root, and its early-exit bug is fixed. |
| Defects fixed (with tests) | ACP client EOF never reached the agent (the injector held a stdin sender); squad and `exec workflow --dynamic` tried to build images on the importing runtime; images with no `HOME` were refused where Docker uses `/etc/passwd`; `ready` left re-import rows Running; concurrent stop/exit and stop/remove races reported "stop failed"; the host probe tests needed GNU `timeout`; a flock-inheritance test flake; the SQLite probe no longer matched awman's patched graph. |
| Test fixes | DoH-by-name probe header; IPv6 probes only where the host has IPv6; `only_lo` requires no usable interface and no routes (the kernel's inert `dummy0`); OOM agent exit asserts the agent's real `-1` signal-death contract. |
| New scenario tests | actual-awman workflow (setup, dependent steps, teardown, failure), API dispatch, squad leader dispatch, owner crash plus `clean`, old/new upgrade plus binary replacement, and a registry-pulled image run offline; public-registry normal cases; private-registry normal cases (token-gated); Apple-only corpus builder mode. |

## Results (all at the frozen tree)

| Evidence | Log | Result |
|---|---|---|
| fmt, clippy (default and builtin-runtime), architecture lint, `make pre-push`, full suite `--no-fail-fast`, feature lib/bin tests | [hermetic.log](final/hermetic.log) | PASS (`make pre-push` exit 0, 36/36 targets) |
| Every builtin hardware test (27) with network, pressure, Apple store, public registry and old-artifact opt-ins | [hardware.log](final/hardware.log) | PASS |
| Full suite with the real Apple Containers backend | [apple-backend-regression.log](final/apple-backend-regression.log) | PASS (36/36 targets) |
| Public registries (Docker Hub, GHCR: foreign token realm, CDN redirects, index selection, digest pin, missing tag, cached reuse); real corpus of all nine shipped templates built by Apple Containers | [image-sources.log](final/image-sources.log) | PASS |
| `make build` auto-signing; release artifact check on the signed optimized binary (linkage, provider metadata, kernel hash, no extracted helpers, offline session under a kernel sandbox with network denied and an exec allowlist); size/startup/RSS measurements | [release-artifact.log](final/release-artifact.log) | PASS |
| Clean checkout: `tools/native-builtin-ci.sh aarch64-apple-darwin` on a throwaway clone (fresh `CARGO_HOME` and target, payload fetch and verify, build, sign, hardware suite) | [clean-checkout-ci.log](final/clean-checkout-ci.log) | PASS (27/27 hardware tests in the clean clone) |
| SQLite: genuine old probe rebuilt, one bundled SQLite, cross-driver transactions, 27 catalog migrations, old↔new catalog round trip, 33 store tests | [sqlite.log](final/sqlite.log) | PASS |
| Intel macOS build refuses `builtin-experimental` (run under Rosetta) | [intel-refusal.log](final/intel-refusal.log) | PASS |
| Private GHCR package pulled with environment, Docker `config.json` and macOS keychain credentials; wrong token and anonymous refused; no secret in any error or log | [private-registry.log](final/private-registry.log) | PASS |

## Not run here, and why

- **Windows build.** It can't be built or run on this Mac. CI's
  `existing-backend-unsupported-targets` job covers it.
- **`docker-sbx-experimental` live regression.** It needs a signed-in Docker
  account (`sbx login`). The hermetic sbx tests pass; the live run moves with
  the other Docker gates to WI 0124.
- **Docker-built corpus formats** (`docker save`, buildx OCI). These need
  Docker (WI 0124); the synthetic corpus covers their parsing.
- **Syscall-level trace.** `fs_usage`/`dtruss` need root. Kernel-enforced
  sandbox runs are used instead, with positive controls in WI 0122.
