#!/usr/bin/env bash
set -euo pipefail

scripts=$(cd "$(dirname "$0")" && pwd -P)
previous=${1:?Usage: bash checks.sh STRICT_BUILD_DIR MICROSANDBOX_CHECKOUT FIXTURE_OCI_TAR}
microsandbox=${2:?Pass the pinned Microsandbox source checkout}
fixture=${3:?Pass the fixture-oci.tar from the original OCI spike}
previous=$(cd "$previous" && pwd -P)
microsandbox=$(cd "$microsandbox" && pwd -P)
test "$(git -C "$microsandbox" rev-parse HEAD)" = 60d4dc8a436fb9365491567ec21d073e924e3c6d
test -f "$fixture"
output=$(mktemp -d /tmp/awsq.XXXXXX)
mkdir -p "$output/results" "$output/empty-path" "$output/home" "$output/tmp"
printf 'SQLite spike results: %s\nNo production manifests, databases or credentials are modified.\n' "$output"

export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$output/target}
export SPIKE_KERNEL="$previous/kernel.bin"
test -f "$SPIKE_KERNEL" || export SPIKE_KERNEL="$previous/build-only-kernel.bin"
export SPIKE_KERNEL_METADATA="$previous/kernel.metadata"
export MSB_EMBED_ARTIFACTS_DIR="$previous/artifacts"
test -f "$MSB_EMBED_ARTIFACTS_DIR/agentd" || export MSB_EMBED_ARTIFACTS_DIR="$previous/build-only-artifacts"
old="$previous/runtime/awman-probe"
test -x "$old" || old="$previous/target/debug/awman-msb-full-embed-probe"
test -x "$old"
test -f "$SPIKE_KERNEL"
test -f "$SPIKE_KERNEL_METADATA"
test -f "$MSB_EMBED_ARTIFACTS_DIR/agentd"
test -f "$previous/msb_krun-0.1.39/Cargo.toml"
if [ "$(uname -s)" = Linux ]; then
    native=${SQLITE_SPIKE_NATIVE_LIB_DIR:-$previous/static-lib}
    test -f "$native/libcap-ng.a"
    export LIBRARY_PATH="$native${LIBRARY_PATH:+:$LIBRARY_PATH}"
fi

curl --fail --location --retry 3 --max-time 300 \
    https://static.crates.io/crates/sqlx-sqlite/sqlx-sqlite-0.9.0.crate -o "$output/sqlx-sqlite.crate"
printf '%s  %s\n' 488e99c397a62007e4229aec669a179816339afc6d2620ca6fa420dbee2e982c "$output/sqlx-sqlite.crate" | shasum -a 256 -c -
tar xf "$output/sqlx-sqlite.crate" -C "$output"
patch -d "$output/sqlx-sqlite-0.9.0" -p1 < "$scripts/sqlx-sqlite-0.38.patch" > "$output/results/patch.log"

base=(--manifest-path "$scripts/probe/Cargo.toml"
    --config "patch.crates-io.microsandbox-cli.path=\"$microsandbox/crates/cli\""
    --config "patch.crates-io.microsandbox-db.path=\"$microsandbox/crates/db\""
    --config "patch.crates-io.microsandbox-migration.path=\"$microsandbox/crates/migration\""
    --config "patch.crates-io.msb_krun.path=\"$previous/msb_krun-0.1.39\"")
if cargo metadata "${base[@]}" --format-version 1 > "$output/unpatched.json" 2> "$output/results/unpatched.stderr"; then
    printf 'Unexpected: unpatched graph resolves; investigate newer dependencies.\n' >&2
    exit 1
fi
grep -q 'links to the native library `sqlite3`' "$output/results/unpatched.stderr"
patched=("${base[@]}" --config "patch.crates-io.sqlx-sqlite.path=\"$output/sqlx-sqlite-0.9.0\"")
printf 'Building the combined awman + strict Microsandbox probe.\n'
cargo build --locked "${patched[@]}" > "$output/results/build.log" 2>&1 || {
    tail -60 "$output/results/build.log"
    exit 1
}
cargo tree --locked "${patched[@]}" -i libsqlite3-sys > "$output/results/sqlite-tree.txt"
cargo tree --locked "${patched[@]}" -e features -i libsqlite3-sys > "$output/results/sqlite-features.txt"
test "$(grep -c '^name = "libsqlite3-sys"$' "$scripts/probe/Cargo.lock")" = 1
new="$CARGO_TARGET_DIR/debug/awman-msb-sqlite-probe"
if [ "$(uname -s)" = Darwin ]; then
    codesign --force --sign - --entitlements "$scripts/../strict-embed/hypervisor.plist" "$new"
    otool -L "$new" > "$output/results/dynamic-dependencies.txt"
else
    ldd "$new" > "$output/results/dynamic-dependencies.txt"
fi
if grep -Ei 'libsqlite3|libcap-ng|libkrun' "$output/results/dynamic-dependencies.txt"; then
    printf 'Unexpected non-system runtime dependency.\n' >&2
    exit 1
fi

run() {
    env -i PATH="$output/empty-path" HOME="$output/home" TMPDIR="$output/tmp" \
        MSB_HOME="$output/catalog" MSB_BACKEND=local "$@"
}
run "$new" sqlite-check > "$output/results/sqlite-check.log" 2>&1
cargo test --locked "${patched[@]}" --test awman_stores > "$output/results/awman-stores.log" 2>&1
run "$old" image load -i "$fixture" -t awman-spike/sqlite:latest > "$output/results/catalog-old-load.log" 2>&1
run "$old" image inspect awman-spike/sqlite:latest > "$output/results/catalog-old-inspect.json"
run "$new" image inspect awman-spike/sqlite:latest > "$output/results/catalog-new-inspect.json"
cmp "$output/results/catalog-old-inspect.json" "$output/results/catalog-new-inspect.json"
run "$new" image load -i "$fixture" -t awman-spike/sqlite-new:latest > "$output/results/catalog-new-load.log" 2>&1
run "$old" image inspect awman-spike/sqlite-new:latest > "$output/results/catalog-old-reopen.json"
printf 'PASS old catalog read by new binary; new image registration read by old binary\n' > "$output/results/catalog-roundtrip.log"
cat "$output/results/sqlite-check.log" "$output/results/catalog-roundtrip.log"
grep 'test result:' "$output/results/awman-stores.log"
tar czf "$output/results.tar.gz" -C "$output" results
printf 'Review and share %s/results.tar.gz\n' "$output"
