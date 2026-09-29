#!/bin/bash
set -u
export PATH=/usr/local/cargo/bin:$PATH
export CARGO_BUILD_JOBS=2
out=aspec/review-notes/0121-evidence/raw/final
run() {
  local name=$1
  shift
  "$@" > "$out/$name.log" 2>&1
  local code=$?
  printf '%s\t%s\n' "$name" "$code" >> "$out/command-exits.tsv"
}
run builtin-inventory cargo test --locked --features builtin-runtime --test builtin_runtime -- --list
run builtin-hermetic env AWMAN_TEST_TMPROOT=/var/tmp/test-fixtures bash tools/isolated-test.sh --features builtin-runtime --test builtin_runtime -- --skip builtin_hw --nocapture
run sdk-and-builtin-lib env AWMAN_TEST_TMPROOT=/var/tmp/test-fixtures bash tools/isolated-test.sh --features builtin-runtime --lib engine::container::builtin:: -- --nocapture
run hardware-required env AWMAN_TEST_BUILTIN=1 AWMAN_TEST_BUILTIN_REQUIRE_HW=1 AWMAN_TEST_BUILTIN_NETWORK=1 AWMAN_TEST_BUILTIN_PRESSURE=1 bash tools/isolated-test.sh --features builtin-runtime --test builtin_runtime builtin_hw -- --nocapture
run oci-current bash tools/isolated-test.sh --features builtin-runtime --test oci_import -- --nocapture
run services-required env AWMAN_TEST_REQUIRE_EXTERNAL=1 bash tools/isolated-test.sh --features builtin-runtime --test oci_import real_stores:: -- --nocapture
run corpus-required env AWMAN_TEST_REQUIRE_EXTERNAL=1 bash tools/isolated-test.sh --features builtin-runtime --test oci_import real_corpus_every_shipped_template_archive_validates_and_projects -- --nocapture
run distribution-required env AWMAN_TEST_DISTRIBUTION=1 bash tools/isolated-test.sh --features builtin-runtime --test builtin_runtime builtin_final_artifact_retains_worker_provider -- --nocapture
cat "$out/command-exits.tsv"
run final-feature-clippy cargo clippy --all-targets --features builtin-runtime -- -D warnings
run final-pre-push make pre-push
run final-test-fast make test-fast
run fast-oci-inventory cargo test --test oci_import -- --list --skip real_git --skip real_network --skip builtin_hw
cat "$out/command-exits.tsv"
