#!/usr/bin/env bash
# Run the carried in-crate managed/user-layer regression without a VM.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
scratch=$(mktemp -d /tmp/awman-isolated-builder.XXXXXX)
trap 'rm -r -- "$scratch"' EXIT
cp -R "$repo/third_party/microsandbox-0.7.8" "$scratch/sdk"
cp "$repo/Cargo.lock" "$scratch/sdk/Cargo.lock"
# Published SDK tests contain one live test using an unpublished test_utils
# macro. Omit only that unrelated hardware test in this disposable copy.
sed 's/#\[test_utils::msb_test\]/#[cfg(any())]/' "$scratch/sdk/lib/backend/local/mod.rs" > "$scratch/mod.rs"
mv "$scratch/mod.rs" "$scratch/sdk/lib/backend/local/mod.rs"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$repo/target}
cargo test --manifest-path "$scratch/sdk/Cargo.toml" --no-default-features --features local,net \
    --config "patch.crates-io.sqlx-sqlite.path=\"$repo/third_party/sqlx-sqlite-0.9.0\"" \
    --config "patch.crates-io.microsandbox-filesystem.path=\"$repo/third_party/microsandbox-filesystem-0.7.8\"" \
    --config "patch.crates-io.microsandbox-network.path=\"$repo/third_party/microsandbox-network-0.7.8\"" \
    --config "patch.crates-io.microsandbox-types.path=\"$repo/third_party/microsandbox-types-0.7.8\"" \
    --lib isolated_builder_ignores_hostile_user_and_managed_files
