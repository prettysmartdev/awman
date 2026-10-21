#!/usr/bin/env bash
# Graph checks for all supported targets; native links need verified payloads.
# --release-matrix adds optimized/LTO/stripped links. Missing tools/payloads SKIP.
set -euo pipefail
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo"
release_matrix=0
if [ $# -gt 0 ]; then
    [ "$1" = --release-matrix ] || { echo 'usage: test-static-krunfw.sh [--release-matrix]' >&2; exit 2; }
    release_matrix=1
fi
scratch=$(mktemp -d /tmp/awman-static-link.XXXXXX)
trap 'rm -r -- "$scratch"' EXIT
for target in aarch64-apple-darwin aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
    for feature in default none builtin; do
        case "$feature" in
            default) cargo tree --locked --offline -p awman-cli --target "$target" -e normal,features > "$scratch/tree";;
            none) cargo tree --locked --offline -p awman-cli --target "$target" -e normal,features --no-default-features > "$scratch/tree";;
            builtin) cargo tree --locked --offline -p awman-cli --target "$target" -e normal,features --features builtin-runtime > "$scratch/tree";;
        esac
        if [ "$feature" = builtin ]; then
            cargo tree --locked --offline -p awman-cli --target "$target" -e features --features builtin-runtime -i msb_krun > "$scratch/inverse"
            grep -q 'msb_krun feature "static-krunfw"' "$scratch/inverse"
            versions=$(sed -n 's/.*msb_krun v\([^ ]*\).*/\1/p' "$scratch/tree" | sort -u)
            [ "$versions" = 0.1.41 ] || { echo "FAIL: msb_krun versions: $versions" >&2; exit 1; }
        elif grep -Eq 'msb_krun|static-krunfw' "$scratch/tree"; then
            echo "FAIL: builtin dependency leaked into $target/$feature" >&2; exit 1
        fi
        echo "PASS: feature graph $target/$feature"
    done
done
host=$(rustc -vV | sed -n 's/^host: //p')
for target in aarch64-apple-darwin aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
    [ "$target" = "$host" ] || echo "SKIP: native link $target requires a native runner and verified payload"
done
if ! bash tools/msb-payloads/verify.sh "$host" > "$scratch/payload" 2>&1; then
    echo "SKIP: native builtin link: verified $host payload unavailable"
    cat "$scratch/payload"
    exit 0
fi
if ! command -v nm >/dev/null; then echo 'SKIP: symbol check requires nm'; exit 0; fi
build() {
    if ! cargo build --locked --offline -vv -p awman-cli --bin awman "$@" > "$scratch/build.log" 2>&1; then
        cat "$scratch/build.log" >&2; return 1
    fi
    if grep -Eq -- '--export-dynamic|-Wl,-E([[:space:]]|$)' "$scratch/build.log"; then
        echo 'FAIL: export-dynamic in build commands' >&2; exit 1
    fi
}
target_dir=$(cargo metadata --locked --offline --no-deps --format-version 1 | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
# LTO and stripping can erase names; inspect an unstripped non-LTO binary.
CARGO_PROFILE_DEV_LTO=false build --features builtin-runtime
binary="$target_dir/debug/awman"
if [ "$(uname -s)" = Darwin ]; then
    nm -gU "$binary" > "$scratch/symbols"
    grep -Eq '[[:space:]]T[[:space:]]_krunfw_get_kernel$' "$scratch/symbols"
else
    nm -g --defined-only "$binary" > "$scratch/symbols"
    grep -Eq '[[:space:]]T[[:space:]]krunfw_get_kernel$' "$scratch/symbols"
    nm -D "$binary" > "$scratch/dynamic"
    if grep -q krunfw_get_kernel "$scratch/dynamic"; then
        echo 'FAIL: callback dynamically exported' >&2; exit 1
    fi
fi
nm "$binary" > "$scratch/all-symbols"
if grep -Eq '[[:space:]]_?capng_' "$scratch/all-symbols"; then
    echo 'FAIL: obsolete libcap-ng symbols linked' >&2; exit 1
fi
echo "PASS: native $host static callback symbol (no export-dynamic)"
if [ "$release_matrix" != 1 ]; then
    echo 'SKIP: optimized/LTO/stripped links require --release-matrix'; exit 0
fi
# Metadata only, empty environment: never a host agent or guest.
probe() {
    env -i HOME="$scratch" AWMAN_TEST_ISOLATION=1 "$1" __awman-builtin-kernel-check > "$2"
    grep -q '"embedded_kernel":true' "$2"
    grep -q '"msb_version":"0.7.8"' "$2"
}
for lto in false fat; do
    echo "BUILD: optimized native $host lto=$lto"
    CARGO_TARGET_DIR="$scratch/target" CARGO_PROFILE_RELEASE_LTO="$lto" CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_STRIP=none build --release --features builtin-runtime
    binary="$scratch/target/release/awman"
    probe "$binary" "$scratch/release.json"
    echo "PASS: optimized link/callback lto=$lto"
    if command -v strip >/dev/null; then
        cp "$binary" "$scratch/awman-stripped"
        if [ "$(uname -s)" = Darwin ]; then
            strip -x "$scratch/awman-stripped"
            bash tools/macos-sign.sh "$scratch/awman-stripped"
        else
            strip "$scratch/awman-stripped"
        fi
        probe "$scratch/awman-stripped" "$scratch/stripped.json"
        cmp "$scratch/release.json" "$scratch/stripped.json"
        echo "PASS: stripped link/callback lto=$lto"
    else
        echo 'SKIP: stripped artifact check requires strip'
    fi
    # These are this script's disposable artifacts, not the caller's cache.
    rm -r -- "$scratch/target"
done
