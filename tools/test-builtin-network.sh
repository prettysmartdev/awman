#!/usr/bin/env bash
# Exercise the carried SDK networking implementation without a VM or services.
# A dependency crate is not a workspace test target, so use an isolated copy
# with the application's lockfile and the same native/type patches.
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
network_test_source=$(mktemp -d "${TMPDIR:-/tmp}/awman-network-tests.XXXXXX")
trap 'rm -r -- "$network_test_source"' EXIT
cp "$repo/third_party/microsandbox-network-0.7.2/Cargo.toml" "$network_test_source/"
cp "$repo/Cargo.lock" "$network_test_source/"
cp -R "$repo/third_party/microsandbox-network-0.7.2/lib" "$network_test_source/"
cp -R "$repo/third_party/microsandbox-network-0.7.2/tests" "$network_test_source/"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo/target}" \
AWMAN_TEST_DOCKER=0 AWMAN_TEST_APPLE_CONTAINER=0 AWMAN_TEST_SBX=0 \
AWMAN_TEST_IMAGE_STORES=0 AWMAN_TEST_REGISTRY=0 \
bash "$repo/tools/isolated-test.sh" --offline \
    --manifest-path "$network_test_source/Cargo.toml" \
    --config "patch.crates-io.msb_krun.path='$repo/third_party/msb_krun-0.1.39'" \
    --config "patch.crates-io.microsandbox-types.path='$repo/third_party/microsandbox-types-0.7.2'" \
    --lib "$@" -- strict_sni strict_mode_blocks_hostname_allowed_opaque_tls model::policy::types::tests
