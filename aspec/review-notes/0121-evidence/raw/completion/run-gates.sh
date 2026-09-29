#!/bin/bash
set -u
export PATH=/usr/local/cargo/bin:$PATH
export CARGO_BUILD_JOBS=2
out=/awman/context/workflow/0121/completion
run() {
  local name=$1
  shift
  "$@" > "$out/$name.log" 2>&1
  local code=$?
  printf '%s\t%s\n' "$name" "$code" >> "$out/command-exits.tsv"
}
run pre-push-reviewed make pre-push
run feature-clippy-reviewed cargo clippy --all-targets --features builtin-runtime -- -D warnings
run test-builtin-reviewed make test-builtin
run focused-oci bash tools/isolated-test.sh --features builtin-runtime --test oci_import -- --nocapture
run focused-fixes bash tools/isolated-test.sh --features builtin-runtime --lib -- --nocapture engine::oci:: frontend::api::serve::tls_tests:: engine::container::builtin::msb_driver::tests::sdk_hostname_dns_binding_cannot_authorize_udp_or_quic
run hardware-required env AWMAN_TEST_BUILTIN=1 AWMAN_TEST_BUILTIN_REQUIRE_HW=1 AWMAN_TEST_BUILTIN_NETWORK=1 AWMAN_TEST_BUILTIN_PRESSURE=1 bash tools/isolated-test.sh --features builtin-runtime --test builtin_runtime builtin_hw -- --nocapture
run services-required env AWMAN_TEST_REQUIRE_EXTERNAL=1 bash tools/isolated-test.sh --features builtin-runtime --test oci_import real_stores:: -- --nocapture
run corpus-required env AWMAN_TEST_REQUIRE_EXTERNAL=1 bash tools/isolated-test.sh --features builtin-runtime --test oci_import real_corpus_every_shipped_template_archive_validates_and_projects -- --nocapture
run distribution-required env AWMAN_TEST_DISTRIBUTION=1 bash tools/isolated-test.sh --features builtin-runtime --test builtin_runtime builtin_final_artifact_retains_worker_provider -- --nocapture
cat "$out/command-exits.tsv"
