#!/usr/bin/env bash
# Real-payload accessor behavior in an optimized/LTO/stripped executable,
# plus a callback-refusal mutation. Never creates a VM or runs an agent.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
target=$(rustc -vV | sed -n 's/^host: //p')
case "$target" in aarch64-unknown-linux-gnu|aarch64-apple-darwin) ;; *)
    echo 'BLOCKED: copying consumer and verified native ARM64 payload required' >&2; exit 2;;
esac
bash "$repo/tools/msb-payloads/verify.sh" "$target"
target_dir=${CARGO_TARGET_DIR:-$repo/target}
table=$(rg --files --hidden --no-ignore "$target_dir/debug/build" | rg '/out/msb_kernel.rs$' | head -1)
test -n "$table"
scratch=$(mktemp -d /tmp/awman-callback-smoke.XXXXXX)
trap 'rm -r -- "$scratch"' EXIT
cp "$repo/crates/awman-host-shims/src/kernel.rs" "$scratch/kernel.rs"
cat > "$scratch/main.rs" <<'RUST'
#![allow(dead_code)]
mod kernel;
fn main() { kernel::smoke_check().unwrap(); }
RUST
OUT_DIR=$(dirname "$table") rustc --edition=2021 -C opt-level=3 -C lto=fat "$scratch/main.rs" -o "$scratch/smoke"
"$scratch/smoke"
strip "$scratch/smoke"
if [ "$(uname -s)" = Darwin ]; then bash "$repo/tools/macos-sign.sh" "$scratch/smoke"; fi
"$scratch/smoke"
echo 'PASS optimized/fat-LTO/stripped actual callback with real payload'
# This mutation keeps the C symbol linkable but makes every call refuse.
sed '/) -> \*mut c_char {/a\    if std::hint::black_box(true) { return std::ptr::null_mut(); }' \
    "$scratch/kernel.rs" > "$scratch/mutant.rs"
mv "$scratch/mutant.rs" "$scratch/kernel.rs"
OUT_DIR=$(dirname "$table") rustc --edition=2021 -C opt-level=3 -C lto=fat "$scratch/main.rs" -o "$scratch/refusal"
if "$scratch/refusal" > "$scratch/mutation.log" 2>&1; then
    echo 'FAIL: callback refusal escaped artifact smoke check' >&2; exit 1
fi
grep -q 'callback refused' "$scratch/mutation.log"
echo 'PASS callback-refusal mutation fails the artifact smoke check'
