#!/usr/bin/env bash
# Native prerequisite proof against the exact published VMM source. Cargo cannot
# run registry dependency dev-tests in the awman workspace; use a private copy.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd -P)
work=${1:-$root/target/private-memory/$(date -u +%Y%m%dT%H%M%SZ)-$$}
case "$work" in /*) ;; *) echo 'absolute private work directory required' >&2; exit 2 ;; esac
[ ! -e "$work" ] || { echo "private-memory work directory already exists: $work" >&2; exit 2; }
umask 077
mkdir -p "$work"
version=0.1.41
expected=400ebdfc24faad502a5f0ef7e1e91625873dfb54cf4ed7aa8b3ad6944f46c0bf
locked_field() {
    awk -v key="$1" '$0 == "name = \"msb_krun_vmm\"" {selected=1; next}
        selected && $1 == key {gsub(/"/, "", $3); print $3; exit}' "$root/Cargo.lock"
}
[ "$(locked_field version)" = "$version" ] && [ "$(locked_field checksum)" = "$expected" ] || {
    echo 'VMM verification pin differs from Cargo.lock; review and update the mapping proof' >&2
    exit 1
}
archive="$work/msb_krun_vmm-$version.crate"
curl -fsSL --retry 3 "https://static.crates.io/crates/msb_krun_vmm/msb_krun_vmm-$version.crate" -o "$archive"
printf '%s  %s\n' "$expected" "$archive" | sha256sum --check
tar xf "$archive" -C "$work"
source="$work/msb_krun_vmm-$version"
# Only package ownership changes: prevent Cargo from attaching this standalone
# dev-test invocation to an enclosing checkout. Source and published lock stay intact.
printf '\n[workspace]\n' >> "$source/Cargo.toml"
export CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}
export RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}
export HOME="$work/home" XDG_CONFIG_HOME="$work/config" XDG_DATA_HOME="$work/data"
export XDG_CACHE_HOME="$work/cache" AWMAN_HOME="$work/awman" TMPDIR="$work/tmp"
export CARGO_TARGET_DIR="$work/target" CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0
mkdir -m 700 "$HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$AWMAN_HOME" "$TMPDIR"
cat > "$work/expected-tests" <<'TESTS'
private_memory::tests::fresh_zero_backing_is_sparse_unlinked_and_private
private_memory::tests::preserve_vmm_slot_boundaries_without_losing_coverage
private_memory::tests::reject_truncation_alignment_and_overflow
private_memory::tests::reject_writable_handle_and_wrong_topology
private_memory::tests::siblings_and_file_are_immutable_after_host_write_and_unlink
TESTS
cargo test --locked --manifest-path "$source/Cargo.toml" --lib private_memory::tests:: -- --list > "$work/list.txt"
sed -n 's/: test$//p' "$work/list.txt" | LC_ALL=C sort > "$work/actual-tests"
cmp "$work/expected-tests" "$work/actual-tests"
cargo test --locked --manifest-path "$source/Cargo.toml" --lib private_memory::tests:: -- --test-threads=1
