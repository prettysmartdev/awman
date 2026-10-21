#!/usr/bin/env bash
set -euo pipefail

scripts=$(cd "$(dirname "$0")" && pwd -P)
root=$(cd "$scripts/../../.." && pwd -P)
previous=${1:?Usage: bash checks.sh STRICT_BUILD_DIR|auto [MICROSANDBOX_CHECKOUT] [FIXTURE_OCI_TAR]}
microsandbox=${2:-}
fixture=${3:-${SQLITE_SPIKE_FIXTURE_OCI_TAR:-}}
# Hermetic real 27 -> 29 migration/refusal gate precedes native artifact work.
bash "$root/tools/test-msb-catalog-compat.sh"
output_base=${SQLITE_SPIKE_OUTPUT_DIR:-/tmp}
mkdir -p "$output_base"
output=$(mktemp -d "$output_base/awsq.XXXXXX")
mkdir -p "$output/results" "$output/empty-path" "$output/home" "$output/tmp"
printf 'SQLite spike results: %s\nNo production manifests, databases or credentials are modified.\n' "$output"
if [ "$previous" = auto ]; then
    bash "$scripts/build-old-probe.sh" "$(rustc -vV | sed -n 's/^host: //p')" "$output/old-strict-build"
    previous="$output/old-strict-build"
fi
previous=$(cd "$previous" && pwd -P)
if [ -z "$microsandbox" ]; then
    microsandbox="$output/microsandbox"
    git clone --filter=blob:none https://github.com/superradcompany/microsandbox.git "$microsandbox"
    git -C "$microsandbox" checkout --detach 7b7b9dc89e9a1c77801918f7833c3381d1754579
fi
microsandbox=$(cd "$microsandbox" && pwd -P)
test "$(git -C "$microsandbox" rev-parse HEAD)" = 7b7b9dc89e9a1c77801918f7833c3381d1754579
if [ -z "$fixture" ]; then
    fixture_dir="$output/fixtures"
    fixture_arch=amd64
    case "$(rustc -vV | sed -n 's/^host: //p')" in aarch64-*) fixture_arch=arm64;; esac
    AWMAN_SQLITE_FIXTURE_ARCH="$fixture_arch" CARGO_TARGET_DIR="$output/fixture-target" cargo run --locked --manifest-path "$root/tools/oci-runtime-spike/fixture/Cargo.toml" -- --sqlite-minimal "$fixture_dir" > "$output/results/fixture-generation.log" 2>&1
    fixture="$fixture_dir/fixture-oci.tar"
elif [ ! -f "$fixture" ]; then
    echo "fixture OCI archive does not exist: $fixture" >&2
    exit 4
fi
fixture=$(cd "$(dirname "$fixture")" && pwd -P)/$(basename "$fixture")
hash_file() { if command -v sha256sum >/dev/null; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi; }
old="$previous/runtime/awman-probe"
test -x "$old" || old="$previous/target/debug/awman-msb-full-embed-probe"
test -x "$old"
{
    printf 'microsandbox_revision=%s\n' "$(git -C "$microsandbox" rev-parse HEAD)"
    printf 'microsandbox_tree=%s\n' "$(git -C "$microsandbox" rev-parse 'HEAD^{tree}')"
    printf 'old_probe_sha256=%s\n' "$(hash_file "$old")"
    printf 'fixture_sha256=%s\n' "$(hash_file "$fixture")"
} > "$output/results/input-provenance.txt"

export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$output/target}
export SPIKE_KERNEL="$previous/kernel.bin"
test -f "$SPIKE_KERNEL" || export SPIKE_KERNEL="$previous/build-only-kernel.bin"
export SPIKE_KERNEL_METADATA="$previous/kernel.metadata"
export MSB_EMBED_ARTIFACTS_DIR="$previous/artifacts"
export MSB_EMBED_ARTIFACTS_ROOT="$root/third_party/msb-payloads"
test -f "$MSB_EMBED_ARTIFACTS_DIR/agentd" || export MSB_EMBED_ARTIFACTS_DIR="$previous/build-only-artifacts"
test -f "$SPIKE_KERNEL"
test -f "$SPIKE_KERNEL_METADATA"
test -f "$MSB_EMBED_ARTIFACTS_DIR/agentd"

curl --fail --location --retry 3 --max-time 300 \
    https://static.crates.io/crates/sqlx-sqlite/sqlx-sqlite-0.9.0.crate -o "$output/sqlx-sqlite.crate"
printf '%s  %s\n' 488e99c397a62007e4229aec669a179816339afc6d2620ca6fa420dbee2e982c "$output/sqlx-sqlite.crate" | shasum -a 256 -c -
tar xf "$output/sqlx-sqlite.crate" -C "$output"
patch -d "$output/sqlx-sqlite-0.9.0" -p1 < "$root/third_party/sqlx-sqlite-0.9.0/upstream.diff" > "$output/results/patch.log"

# The probe links awman itself, so it must resolve awman's own dependency
# graph: the carried patches from awman's Cargo.toml (WI 0120 inventory) apply
# here too, because [patch] sections do not cross workspaces. The pinned
# upstream checkout above is the provenance check for the published 0.7.8
# crates the registry supplies.
third_party="$root/third_party"
base=(--manifest-path "$scripts/probe/Cargo.toml"
    --config "patch.crates-io.microsandbox.path=\"$third_party/microsandbox-0.7.8\""
    --config "patch.crates-io.microsandbox-filesystem.path=\"$third_party/microsandbox-filesystem-0.7.8\""
    --config "patch.crates-io.microsandbox-network.path=\"$third_party/microsandbox-network-0.7.8\""
    --config "patch.crates-io.microsandbox-types.path=\"$third_party/microsandbox-types-0.7.8\"")
if cargo metadata "${base[@]}" --format-version 1 > "$output/unpatched.json" 2> "$output/results/unpatched.stderr"; then
    printf 'Unexpected: unpatched graph resolves; investigate newer dependencies.\n' >&2
    exit 1
fi
grep -q 'links to the native library `sqlite3`' "$output/results/unpatched.stderr"
# Reconstruct the carried patch as an independent check, then link the owned
# path so repeat runs reuse the same Cargo source identity.
diff -ru --exclude=LICENSE-APACHE --exclude=LICENSE-MIT --exclude=PATCH.md --exclude=upstream.diff \
    "$output/sqlx-sqlite-0.9.0" "$third_party/sqlx-sqlite-0.9.0" > "$output/results/sqlx-reconstruction.diff"
patched=("${base[@]}" --config "patch.crates-io.sqlx-sqlite.path=\"$third_party/sqlx-sqlite-0.9.0\"")
printf 'Building the combined awman + strict Microsandbox probe.\n'
cargo build --locked "${patched[@]}" > "$output/results/build.log" 2>&1 || {
    tail -60 "$output/results/build.log"
    exit 1
}
cargo tree --locked "${patched[@]}" -i libsqlite3-sys > "$output/results/sqlite-tree.txt"
cargo tree --locked "${patched[@]}" -e features -i libsqlite3-sys > "$output/results/sqlite-features.txt"
test "$(grep -c '^name = "libsqlite3-sys"$' "$scripts/probe/Cargo.lock")" = 1
sed -n '/^name = "libsqlite3-sys"$/,/^$/p' "$scripts/probe/Cargo.lock" | grep -q '^version = "0.38.2"$'
sed -n '/^name = "rusqlite"$/,/^$/p' "$scripts/probe/Cargo.lock" | grep -q '^version = "0.40.2"$'
sed -n '/^name = "sqlx-sqlite"$/,/^$/p' "$scripts/probe/Cargo.lock" | grep -q '^version = "0.9.0"$'
test "$(grep -c '^name = "libsqlite3-sys"$' "$output/results/sqlite-tree.txt")" -le 1 || { echo 'multiple bundled SQLite bindings found' >&2; exit 1; }
new="$CARGO_TARGET_DIR/debug/awman-msb-sqlite-probe"
if [ "$(uname -s)" = Darwin ]; then
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
run "$old" image inspect --format json awman-spike/sqlite:latest > "$output/results/catalog-old-inspect.json"
run "$new" image inspect --format json awman-spike/sqlite:latest > "$output/results/catalog-new-inspect.json"
cmp "$output/results/catalog-old-inspect.json" "$output/results/catalog-new-inspect.json"
run "$new" image load -i "$fixture" -t awman-spike/sqlite-new:latest > "$output/results/catalog-new-load.log" 2>&1
if run "$old" image inspect --format json awman-spike/sqlite-new:latest > "$output/results/catalog-old-reopen.json" 2> "$output/results/catalog-old-refusal.log"; then
    echo 'FAIL: old SDK accepted a schema-ahead catalog' >&2; exit 1
fi
grep -q 'database schema is newer' "$output/results/catalog-old-refusal.log"
run "$new" image inspect --format json awman-spike/sqlite:latest > "$output/results/catalog-new-after-refusal.json"
cmp "$output/results/catalog-new-inspect.json" "$output/results/catalog-new-after-refusal.json"
run "$new" image inspect --format json awman-spike/sqlite-new:latest > "$output/results/catalog-new-reopen.json"
printf 'PASS old catalog read by new binary; old reader refuses upgraded catalog; both images survive refusal\n' > "$output/results/catalog-roundtrip.log"
cat "$output/results/sqlite-check.log" "$output/results/catalog-roundtrip.log"
grep 'test result:' "$output/results/awman-stores.log"
tar czf "$output/results.tar.gz" -C "$output" results
printf 'Review and share %s/results.tar.gz\n' "$output"
