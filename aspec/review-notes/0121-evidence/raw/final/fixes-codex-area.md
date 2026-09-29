# fix-codex-area interim handoff — 2026-09-28

The required `/awman/context/workflow/0121/review-codex-work.md` did not exist at step start and was still absent at 16:37 UTC. Its numbered findings cannot yet be dispositioned. Do not treat this as a completed review response; map every finding when the report arrives.

## Concrete owned fixes already applied

- Vendored pinned `microsandbox 0.7.2` with the exact one-method isolated-builder patch; registered the Cargo path patch and refreshed `Cargo.lock`. The feature build failed before with E0599 (`build_lazy_isolated` missing); `cargo check --locked --features builtin-runtime --bin awman` now passes (`sdk-vendor-check.log`). `sdk_isolated_config_ignores_installed_msb_config` passes against valid hostile and malformed ambient configuration (`sdk-isolation-test.log`). Feature lib Clippy passes (`sdk-feature-lib-clippy.log`). Provenance, license and removal gate are in `third_party/microsandbox-0.7.2/PATCH.md`, `third_party/README.md`, and WI 0120 inventory. Native coexistence remains unexecuted.
- Registered `builtin_net_driver` as a feature Cargo example and prepared/signs it in native CI alongside `builtin_hw_driver`. `cargo check --locked --features builtin-runtime --example builtin_net_driver` passes (`net-example-check.log`). CI now explicitly enables the network/pressure guest gates. The test module registry in Claude-owned `tests/builtin_runtime/main.rs` is still pending.
- Fixed feature all-target Clippy's two `field_reassign_with_default` diagnostics in `msb_driver.rs::sdk_accepts_every_compiled_network_policy`. That exact feature test passes (`sdk-network-test.log`); feature lib Clippy passes.
- Fixed `binary::worker_unopened_fd_is_sanitized_error_not_abort`: it previously lacked the SDK's required `--sandbox-id`, so Clap rejected the argv before descriptor validation. The targeted test failed before, passes after (`sdk-unopened-fd-test.log`); all 17 feature `binary::` harness cases pass (`sdk-binary-feature-tests-after.log`). The final-artifact case remains gated without an artifact and is not distribution proof. No descriptor/security check was relaxed.
- Updated the WI 0121 evidence register D-07 from FAIL to BLOCKED for remaining native coexistence, and corrected D-05/D-10 descriptions and current file hashes. Its absolute workflow log links should be copied into durable raw evidence by final verification if authorized; the fix step's explicit extra edit allowlist did not include the raw directory.

## Cross-owner blockers requested

- `REQ-fix-codex-area-001`: Claude-owned `tests/builtin_runtime/gates.rs` rejects new acquisition hardware test, so `make test-fast` fails; Claude-owned `tests/builtin_runtime/hardware.rs::Run::facts_for` is unused, so `cargo clippy --all-targets -- -D warnings` fails. Latest logs: `fix-codex-test-fast.log`, `sdk-feature-clippy.log`. No test was deleted/skipped and no warning was suppressed.
- `REQ-fix-codex-area-002`: Claude-owned `src/data/config/env.rs` needs a typed non-UTF-8-preserving `host_var_os` helper. Current owned `embedded/mod.rs` must detect non-UTF-8 `MSB_*` overrides, but its direct `std::env::var_os` triggers `make architecture-lint`. Do not weaken the security check. Change the owned caller after the helper lands.

## Known external limits

No `/dev/kvm`, x86_64 native host, Apple Silicon/HVF host, genuine old/new binaries or final signed artifact is available here. Hardware and distribution verdicts remain open. The repository `.git` pointer names unavailable host worktree metadata; no commit/full-diff identity can be asserted.
