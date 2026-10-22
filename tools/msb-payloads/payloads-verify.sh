#!/usr/bin/env bash
# Promote a native payload to status=verified (`make payloads-verify`).
#
# The builtin runtime only builds against a verified manifest record (build.rs,
# the builtin_hw tests), so the gates run against a candidate record: the
# freshly extracted kernel hash and layout with status=verified. The candidate
# is kept only if every gate passes; any failure restores the original
# manifest and provenance. Nothing is committed.
#
# Gates, all on this native host with hardware virtualization:
#   1. fetch.sh + verify.sh: pinned archive/agent hashes, kernel layout.
#   2. make test-builtin with real guests required (no SKIP counts as a pass).
#   3. tools/native-builtin-ci.sh from a clean temporary checkout of HEAD plus
#      the candidate record (fresh CARGO_HOME, actual awman guest boots).
#   4. Optimized release build, signed on macOS, traced through
#      tools/release-artifact-check.sh with the offline fixture session.
#
# Fixtures: AWMAN_TEST_BUILTIN_BASE_TAR (docker-archive alpine base) and
# AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE (the promoted fixture built from it by
# tools/oci-runtime-spike/fixture) are used when set; otherwise the base comes
# from a pinned skopeo image under Docker, or Apple Containers when Docker is
# not running, and the fixture is built from it.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd -P)
target=${1:?usage: payloads-verify.sh TARGET_TRIPLE}
cd "$root"
manifest=third_party/msb-payloads/manifest.toml
provenance=third_party/msb-payloads/$target/provenance.txt
blocked() { echo "BLOCKED: $*" >&2; exit 2; }

case "$target" in
    aarch64-apple-darwin)
        [ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || blocked "Apple Silicon/macOS host required"
        [ "$(sysctl -n kern.hv_support)" = 1 ] || blocked "Hypervisor.framework unavailable"
        ;;
    aarch64-unknown-linux-gnu|x86_64-unknown-linux-gnu)
        [ "$(uname -s)" = Linux ] || blocked "native Linux host required"
        [ -r /dev/kvm ] && [ -w /dev/kvm ] || blocked "writable /dev/kvm is required"
        ;;
    *) echo "unsupported payload target: $target" >&2; exit 2 ;;
esac
# Cargo must not build as root (root-owned target/ and caches); run this as
# yourself, then `sudo make install` afterwards if needed.
[ "$(id -u)" != 0 ] || blocked "run as your own user, not root"
# The promoted change must be exactly the candidate record, and gate 3 checks
# out HEAD: uncommitted changes would be tested by some gates and not others.
[ -z "$(git status --porcelain --untracked-files=all)" ] || blocked "commit or stash local changes first"

work=$(mktemp -d "${TMPDIR:-/tmp}/awman-payloads-verify.XXXXXX")
cp "$manifest" "$work/manifest.orig"
had_provenance=0
if [ -f "$provenance" ]; then cp "$provenance" "$work/provenance.orig"; had_provenance=1; fi
promoted=0
cleanup() {
    if [ "$promoted" != 1 ]; then
        cp "$work/manifest.orig" "$manifest"
        if [ "$had_provenance" = 1 ]; then cp "$work/provenance.orig" "$provenance"; fi
        echo "payloads-verify FAILED: manifest left unverified; evidence kept in $work" >&2
    fi
    if [ -d "$work/checkout" ]; then git worktree remove --force "$work/checkout" || true; fi
}
trap cleanup EXIT
step() { printf '\n==> %s\n' "$*"; }

step "Fetch and extract the $target payload"
rc=0; bash tools/msb-payloads/fetch.sh "$target" || rc=$?
# 0: already verified (re-running the gates is harmless); 4: evidence recorded.
[ "$rc" = 0 ] || [ "$rc" = 4 ] || exit "$rc"

step "Write the candidate manifest record"
meta=third_party/msb-payloads/$target/kernel.meta
meta_value() { sed -n "s/^$1=//p" "$meta"; }
if command -v sha256sum >/dev/null; then kernel_hash=$(sha256sum "third_party/msb-payloads/$target/kernel.bin" | awk '{print $1}')
else kernel_hash=$(shasum -a 256 "third_party/msb-payloads/$target/kernel.bin" | awk '{print $1}'); fi
awk -v section="[targets.\"$target\"]" -v hash="$kernel_hash" \
    -v size="$(meta_value size)" -v guest="$(meta_value guest_address)" -v entry="$(meta_value entry_address)" '
    $0 == section {inside=1; print; next}
    /^\[/ {inside=0}
    inside && $1 == "status" {print "status = \"verified\""; next}
    inside && $1 == "kernel_sha256" {print "kernel_sha256 = \"" hash "\""; next}
    inside && $1 == "size" {print "size = " size; next}
    inside && $1 == "guest_address" {print "guest_address = " guest; next}
    inside && $1 == "entry_address" {print "entry_address = " entry; next}
    {print}' "$work/manifest.orig" > "$manifest"
git --no-pager diff -- "$manifest"
bash tools/msb-payloads/verify.sh "$target"

step "Prepare guest fixtures"
# The gates boot the promoted fixture image, which
# tools/oci-runtime-spike/fixture builds from a docker-save alpine base (a
# plain alpine archive is not a valid fixture). skopeo makes that base from a
# digest-pinned image under the local container engine (Docker preferred,
# else Apple Containers), with only an empty output directory mounted.
skopeo_image=quay.io/skopeo/stable:v1.22.3-immutable@sha256:c0ee1f4edca5c01cb8d5611124f92f3cc47196ecab68aee5d0f90834e00574d5
base_tar=${AWMAN_TEST_BUILTIN_BASE_TAR:-}
fixture=${AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE:-}
mkdir "$work/fixtures"
if [ -z "$base_tar" ]; then
    if docker info >/dev/null 2>&1; then engine=(docker run --rm --user "$(id -u):$(id -g)")
    elif container system status >/dev/null 2>&1; then engine=(container run --rm)
    else blocked "start Docker or Apple Containers to build the guest fixture base (or set AWMAN_TEST_BUILTIN_BASE_TAR)"
    fi
    echo "using ${engine[0]} to run skopeo"
    "${engine[@]}" --volume "$work/fixtures:/out" "$skopeo_image" \
        copy --override-os linux --format v2s2 docker://docker.io/library/alpine:3.22 docker-archive:/out/base.tar
    base_tar=$work/fixtures/base.tar
fi
if [ -z "$fixture" ]; then
    cargo run --locked --quiet --manifest-path tools/oci-runtime-spike/fixture/Cargo.toml -- "$base_tar" "$work/fixtures"
    fixture=$work/fixtures/fixture-oci.tar
fi
[ -f "$base_tar" ] && [ -f "$fixture" ] || blocked "guest fixture archives missing"
export AWMAN_TEST_BUILTIN_BASE_TAR=$base_tar AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE=$fixture
export AWMAN_TEST_BUILTIN=1 AWMAN_TEST_BUILTIN_REQUIRE_HW=1
export AWMAN_TEST_BUILTIN_NETWORK=1 AWMAN_TEST_BUILTIN_PRESSURE=1
export AWMAN_TEST_BUILTIN_REPORT=$work/builtin-hw-report.tsv

step "Gate: builtin tests with real guests (make test-builtin)"
make --no-print-directory test-builtin
cat "$AWMAN_TEST_BUILTIN_REPORT" 2>/dev/null || true

step "Gate: clean-checkout native build and guest boots (native-builtin-ci.sh)"
git worktree add --quiet --detach "$work/checkout" HEAD
cp "$manifest" "$work/checkout/$manifest"
cp "$provenance" "$work/checkout/$provenance"
git -C "$work/checkout" -c user.name=awman -c user.email=awman@localhost \
    commit --quiet --no-verify -am "candidate $target payload (payloads-verify, not for publishing)"
RUNNER_TEMP=$work bash "$work/checkout/tools/native-builtin-ci.sh" "$target"

step "Gate: optimized, signed artifact traced through an offline session"
cargo build --locked --release --features builtin-runtime --target "$target" --bin awman
artifact=$work/awman
cp "${CARGO_TARGET_DIR:-target}/$target/release/awman" "$artifact"
if [ "$(uname -s)" = Darwin ]; then bash tools/macos-sign.sh "$artifact"; fi
bash tools/release-artifact-check.sh "$artifact" "$target" "$work/release-evidence" \
    "$root/tools/release-offline-session.sh" --fixture "$fixture"

promoted=1
step "PASS: $target payload verified"
git --no-pager diff --stat
echo "Review and commit $manifest and $provenance. Evidence: $work"
