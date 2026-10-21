#!/usr/bin/env bash
# Supply a directory containing pristine extracted protocol/types crates for
# both 0.7.2 and 0.7.8. No network, guest, host agent, or credentials are needed.
set -euo pipefail
source_root=$(cd "$1" && pwd)
repo=$(cd "$(dirname "$0")/.." && pwd)
scratch=$(mktemp -d /tmp/awman-protocol-compat.XXXXXX)
trap 'rm -r -- "$scratch"' EXIT
export AWMAN_PROTOCOL_FIXTURES="$scratch/fixtures"
if [ -z "$(printenv CARGO_TARGET_DIR || true)" ]; then
    export CARGO_TARGET_DIR="$repo/target"
fi
for version in 0.7.2 0.7.8; do
    cp -R "$source_root/microsandbox-protocol-$version" "$scratch/$version"
    cp "$repo/tools/test-fixtures/msb-protocol-compat.rs" "$scratch/$version/tests/awman_0120.rs"
    cp "$repo/Cargo.lock" "$scratch/$version/Cargo.lock"
    cat >> "$scratch/$version/Cargo.toml" <<'MANIFEST'

[[test]]
name = "awman_0120"
path = "tests/awman_0120.rs"
MANIFEST
done
for phase in write_versioned_protocol_fixtures read_other_version_protocol_fixtures; do
    for version in 0.7.2 0.7.8; do
        types="$source_root/microsandbox-types-$version"
        if [ "$version" = 0.7.8 ]; then types="$repo/third_party/microsandbox-types-0.7.8"; fi
        bash "$repo/tools/isolated-test.sh" --offline --manifest-path "$scratch/$version/Cargo.toml" \
            --config "patch.crates-io.microsandbox-types.path='$types'" --test awman_0120 "$phase" -- --exact
    done
done
# Run the adopted protocol repository's native contract gates as well.
bash "$repo/tools/isolated-test.sh" --offline --manifest-path "$scratch/0.7.8/Cargo.toml" \
    --config "patch.crates-io.microsandbox-types.path='$repo/third_party/microsandbox-types-0.7.8'" \
    --lib --test legacy_json_contract --test control_contract --test schema_snapshot
bash "$repo/tools/isolated-test.sh" --offline --manifest-path "$scratch/0.7.8/Cargo.toml" \
    --config "patch.crates-io.microsandbox-types.path='$repo/third_party/microsandbox-types-0.7.8'" \
    --test awman_0120 -- --list
