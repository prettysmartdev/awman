#!/bin/bash
set -u
export PATH=/usr/local/cargo/bin:$PATH CARGO_BUILD_JOBS=2
out=aspec/review-notes/0121-evidence/raw/final
run() {
 local name=$1
 shift
 "$@" > "$out/$name.log" 2>&1
 local code=$?
 printf '%s\t%s\n' "$name" "$code" >> "$out/registry-recheck-exits.tsv"
}
run final-feature-clippy cargo clippy --all-targets --features builtin-runtime -- -D warnings
run test-builtin make test-builtin
run builtin-hermetic bash tools/isolated-test.sh --features builtin-runtime --test builtin_runtime -- --skip builtin_hw --nocapture
run builtin-inventory cargo test --locked --features builtin-runtime --test builtin_runtime -- --list
run final-test-fast make test-fast
cat "$out/registry-recheck-exits.tsv"
