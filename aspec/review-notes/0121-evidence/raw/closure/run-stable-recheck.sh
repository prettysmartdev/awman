#!/bin/bash
set -u
export PATH=/usr/local/cargo/bin:$PATH
export CARGO_BUILD_JOBS=2
out=/awman/context/workflow/0121/closure
run() {
  local name=$1
  shift
  "$@" > "$out/$name.log" 2>&1
  local code=$?
  printf '%s\t%s\n' "$name" "$code" >> "$out/command-exits.tsv"
}
run pre-push-stable make pre-push
run test-fast-stable make test-fast
run builtin-sdk-local bash tools/isolated-test.sh --features builtin-runtime --lib engine::container::builtin:: -- --nocapture
run builtin-inventory bash tools/isolated-test.sh --features builtin-runtime --test builtin_runtime -- --list
