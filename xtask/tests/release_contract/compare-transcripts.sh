#!/usr/bin/env bash
# All GitHub and push operations inside the test are fail-closed fakes.
set -euo pipefail
cd "$(dirname "$0")/../../.."
export CARGO_TARGET_DIR=target/wf-release-op
export AWMAN_RELEASE_TRANSCRIPT_OUT="${1:-/awman/context/workflow/handoff/release-transcript-diff.txt}"
cargo test --locked -p xtask --test release_contract transcript_comparison -- --exact --nocapture
