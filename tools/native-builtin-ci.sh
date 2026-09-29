#!/usr/bin/env bash
# Required native target gate. This intentionally exits nonzero when a runner
# lacks KVM/HVF; a local SKIP is never reported as a hardware pass.
set -euo pipefail
target=${1:?usage: native-builtin-ci.sh TARGET_TRIPLE}
root=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$root"
case "$target" in
  aarch64-unknown-linux-gnu|x86_64-unknown-linux-gnu)
    [ "$(uname -s)" = Linux ] || { echo "BLOCKED: $target requires native Linux" >&2; exit 2; }
    case "$target:$(uname -m)" in aarch64-unknown-linux-gnu:aarch64|x86_64-unknown-linux-gnu:x86_64) ;; *) echo "BLOCKED: target and runner architecture differ" >&2; exit 2 ;; esac
    [ -r /dev/kvm ] && [ -w /dev/kvm ] || { echo "BLOCKED: writable /dev/kvm is required" >&2; ls -l /dev/kvm || true; exit 2; }
    ;;
  aarch64-apple-darwin)
    [ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || { echo "BLOCKED: Apple Silicon/macOS runner required" >&2; exit 2; }
    [ "$(sysctl -n kern.hv_support)" = 1 ] || { echo "BLOCKED: Hypervisor.framework unavailable" >&2; exit 2; }
    ;;
  *) echo "unsupported native target: $target" >&2; exit 2 ;;
esac
git rev-parse --verify HEAD >/dev/null
[ -z "$(git status --porcelain --untracked-files=all)" ] || { echo "BLOCKED: native build requires clean checkout" >&2; exit 2; }
if [ -n "${RUNNER_TEMP:-}" ]; then bash tools/reproducible-build-audit.sh "$RUNNER_TEMP/awman-build-input-audit"; else bash tools/reproducible-build-audit.sh "$root/target/build-input-audit"; fi
test -f .cargo/config.toml && test -f Cargo.lock || { echo "BLOCKED: tracked Cargo inputs unavailable" >&2; exit 2; }
git ls-files --error-unmatch .cargo/config.toml Cargo.lock >/dev/null
if grep -nE '(/tmp/|\.cargo/registry|/Users/[^/]+/\.cargo)' Cargo.toml .cargo/config.toml build.rs; then
  echo "BLOCKED: build configuration references temporary/developer cache paths" >&2; exit 2
fi
# A persistent self-hosted runner must not build from its mutable Cargo source
# cache. Keep rustup's installed toolchain, but use a fresh Cargo registry/git
# cache for every clean-checkout evidence run.
runner_temp=${RUNNER_TEMP:-${TMPDIR:-/tmp}}
mkdir -p "$runner_temp"
export CARGO_HOME=$(mktemp -d "$runner_temp/awman-cargo-home.XXXXXX")
export CARGO_TARGET_DIR=$(mktemp -d "$runner_temp/awman-target.XXXXXX")
bash tools/msb-payloads/fetch.sh "$target"
bash tools/msb-payloads/verify.sh "$target"
if [ "$(uname -s)" = Linux ]; then bash third_party/native/libcap-ng/build.sh "$target"; fi
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
export AWMAN_TEST_BUILTIN=1 AWMAN_TEST_BUILTIN_REQUIRE_HW=1
export AWMAN_TEST_BUILTIN_NETWORK=1 AWMAN_TEST_BUILTIN_PRESSURE=1
export AWMAN_TEST_BUILTIN_ARTIFACT="$CARGO_TARGET_DIR/$target/debug/awman"
cargo build --locked --features builtin-runtime --target "$target" --bin awman
cargo build --locked --features builtin-runtime --target "$target" --example builtin_hw_driver
cargo build --locked --features builtin-runtime --target "$target" --example builtin_net_driver
cargo test --locked --features builtin-runtime --target "$target" --test builtin_runtime --no-run
test_list=$(cargo test --locked --features builtin-runtime --target "$target" --test builtin_runtime -- --list)
printf '%s\n' "$test_list"
printf '%s\n' "$test_list" | grep -Eq '(^|::)builtin_hw_actual_awman_cli_synthetic_agent_exit_37: test$' || {
  echo "FAIL: native CI requires builtin_hw_actual_awman_cli_synthetic_agent_exit_37 to boot actual awman; driver-only coverage cannot pass D-03" >&2
  exit 1
}
# Actual entrypoint smoke and optimized-independent metadata/provider checks.
printf '%s\n' "$test_list" | grep -Eq '(^|::)builtin_hw_actual_awman_all_agent_settings_prompt_overlay_strategies: test$' || {
  echo "FAIL: native CI requires the actual awman guest strategy matrix" >&2
  exit 1
}
env -i PATH=/usr/bin:/bin HOME="$(mktemp -d)" "$AWMAN_TEST_BUILTIN_ARTIFACT" --version
env -i PATH=/usr/bin:/bin HOME="$(mktemp -d)" "$AWMAN_TEST_BUILTIN_ARTIFACT" __awman-builtin-info
if [ -z "${AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE:-}" ] || [ ! -f "$AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE" ]; then
  echo "BLOCKED: AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE must name a disposable OCI fixture archive" >&2; exit 2
fi
# Hardware suite fails on missing prerequisites due to REQUIRE_HW. It records
# each exercised scenario and includes current typed provider guest coverage.
cargo test --locked --features builtin-runtime --target "$target" --test builtin_runtime builtin_hw -- --nocapture
