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
run pre-push make pre-push
run feature-clippy cargo clippy --all-targets --features builtin-runtime -- -D warnings
run test-builtin make test-builtin
run focused-local bash tools/isolated-test.sh --features builtin-runtime --lib -- --nocapture engine::container::builtin::paths::tests:: engine::container::builtin::backend::lifecycle_tests:: engine::ready::tests::ready_ engine::oci:: frontend::api::serve::tls_tests::
run focused-oci bash tools/isolated-test.sh --features builtin-runtime --test oci_import -- --nocapture
run hermetic-builtin bash tools/isolated-test.sh --features builtin-runtime --test builtin_runtime -- --skip builtin_hw --nocapture
run hardware-required env AWMAN_TEST_BUILTIN=1 AWMAN_TEST_BUILTIN_REQUIRE_HW=1 AWMAN_TEST_BUILTIN_NETWORK=1 AWMAN_TEST_BUILTIN_PRESSURE=1 bash tools/isolated-test.sh --features builtin-runtime --test builtin_runtime builtin_hw -- --nocapture
run services-required env AWMAN_TEST_REQUIRE_EXTERNAL=1 bash tools/isolated-test.sh --features builtin-runtime --test oci_import real_stores:: -- --nocapture
run corpus-required env AWMAN_TEST_REQUIRE_EXTERNAL=1 bash tools/isolated-test.sh --features builtin-runtime --test oci_import real_corpus_every_shipped_template_archive_validates_and_projects -- --nocapture
run distribution-required env AWMAN_TEST_DISTRIBUTION=1 bash tools/isolated-test.sh --features builtin-runtime --test builtin_runtime builtin_final_artifact_retains_worker_provider -- --nocapture
run native-arm bash tools/native-builtin-ci.sh aarch64-unknown-linux-gnu
run native-x86 bash tools/native-builtin-ci.sh x86_64-unknown-linux-gnu
run native-apple bash tools/native-builtin-ci.sh aarch64-apple-darwin
cat "$out/command-exits.tsv"
