#!/usr/bin/env bash
# Remaining WI 0143 / 0139–0143 native completion gate. Run from any directory.
# Intentionally all-or-nothing: Docker alone cannot validate Apple/builtin.
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: bash scripts/validate-worker-gate.sh [--help]

Requires an idle Apple Silicon Mac, Hypervisor.framework, Docker and Apple
Containers running, Xcode command-line tools, the repository's Rust toolchain,
a full Git checkout (including history), and network access for images/payloads.
Run as your own user. The hardware suite stops/restarts Apple Containers.

Runs all three worker mount probes, the builtin hardware suite (including
explicit daemon API/squad tests), docs regeneration/projection tests, then
architecture-lint and test-fast. Imports a disposable builtin image/state;
no agent credentials or existing awman configuration are needed.

Optional environment:
  AWMAN_GATE_LOG_DIR       New directory for evidence (default: target/worker-gate-<time>-<pid>)
  AWMAN_GATE_SHELL_IMAGE   POSIX-shell image to pull in Docker and Apple (default: alpine:3.20)
  CARGO_BUILD_JOBS         Build parallelism (default: 2)
  RUST_TEST_THREADS        Test parallelism (default: 4)

Nonzero exit means FAIL/BLOCKED. summary.txt and per-step logs retain evidence.
This script does not change the workflow gate report or declare unrun checks passed.
EOF
}
if [ "$#" -gt 0 ]; then
    if [ "$#" -eq 1 ] && [ "$1" = --help ]; then usage; exit 0; fi
    usage >&2
    exit 2
fi

root=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$root"
logs=${AWMAN_GATE_LOG_DIR:-$root/target/worker-gate-$(date -u +%Y%m%dT%H%M%SZ)-$$}
# Refuse to mix this run's result with stale successful evidence.
[ ! -e "$logs" ] || { echo "Evidence directory already exists: $logs" >&2; exit 2; }
mkdir -p "$logs"
logs=$(cd "$logs" && pwd -P)
scratch=
step=preflight
finish() {
    local status=$?
    trap - EXIT
    if [ "$status" -ne 0 ]; then
        printf 'FAIL/BLOCKED: %s (exit %s)\n' "$step" "$status" | tee -a "$logs/summary.txt"
    fi
    if [ -n "$scratch" ]; then rm -rf "$scratch"; fi
    echo "Evidence: $logs"
    exit "$status"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
fail() { echo "BLOCKED: $*" >&2; exit 2; }
run() {
    step=$1
    shift
    printf '\n==> %s\n' "$step"
    "$@" 2>&1 | tee "$logs/$step.log"
    printf 'PASS: %s\n' "$step" >> "$logs/summary.txt"
}
require_line() {
    grep -Fq "$2" "$logs/$1.log" || fail "$1 did not record: $2"
}

[ "$(uname -s):$(uname -m)" = Darwin:arm64 ] || fail "all checks require an Apple Silicon Mac"
[ "$(id -u)" -ne 0 ] || fail "run as your own user, not root"
for command in cargo rustc git make docker container codesign xcrun; do
    command -v "$command" >/dev/null || fail "missing command: $command"
done
[ "$(sysctl -n kern.hv_support)" = 1 ] || fail "Hypervisor.framework unavailable"
[ "$(rustc -vV | sed -n 's/^host: //p')" = aarch64-apple-darwin ] || fail "use native Apple Silicon Rust"
[ -z "${CARGO_BUILD_TARGET:-}" ] || fail "unset CARGO_BUILD_TARGET for native artifact paths"
git rev-parse --verify HEAD > "$logs/revision.txt"
[ "$(git rev-parse --is-shallow-repository)" = false ] || fail "full Git history is required"
git status --short > "$logs/source-status.txt"
git diff --stat > "$logs/source-diff-stat.txt"
run docker-info docker info
run apple-status container system status
run toolchain rustc -vV
run host sw_vers

export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
export RUST_TEST_THREADS=${RUST_TEST_THREADS:-4}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/target}
mkdir -p "$CARGO_TARGET_DIR"
CARGO_TARGET_DIR=$(cd "$CARGO_TARGET_DIR" && pwd -P)
export CARGO_TARGET_DIR
scratch=$(mktemp -d /tmp/aw-gate.XXXXXX)
export AWMAN_TEST_TMPROOT=$scratch/tests
mkdir -p "$AWMAN_TEST_TMPROOT"
# Do not inherit opt-ins or an artifact from another run. Each step opts in
# explicitly; the final fast suite must remain isolated from native runtimes.
unset AWMAN_TEST_WORKER_NATIVE AWMAN_TEST_WORKER_NATIVE_IMAGE AWMAN_TEST_WORKER_NATIVE_STATE
unset AWMAN_TEST_DOCKER AWMAN_TEST_APPLE_CONTAINER AWMAN_TEST_BUILTIN AWMAN_TEST_SBX
unset AWMAN_TEST_BUILTIN_ARTIFACT AWMAN_TEST_BUILTIN_PREVIOUS
unset AWMAN_TEST_BUILTIN_REQUIRE_HW AWMAN_TEST_BUILTIN_REPORT
unset AWMAN_TEST_BUILTIN_BASE_TAR AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE
shell_image=${AWMAN_GATE_SHELL_IMAGE:-alpine:3.20}
probe=native_worker_workspace_denies_writes_and_output_receipt_requires_guest_mount

run docker-image docker pull "$shell_image"
run worker-docker env AWMAN_TEST_DOCKER=1 AWMAN_TEST_WORKER_NATIVE=docker \
    AWMAN_TEST_WORKER_NATIVE_IMAGE="$shell_image" \
    bash tools/isolated-test.sh -p awman --test worker_native "$probe" -- --exact --nocapture
require_line worker-docker 'PASS: worker mount probe docker'

run apple-image container image pull "$shell_image"
run worker-apple env AWMAN_TEST_APPLE_CONTAINER=1 AWMAN_TEST_WORKER_NATIVE=apple-containers \
    AWMAN_TEST_WORKER_NATIVE_IMAGE="$shell_image" \
    bash tools/isolated-test.sh -p awman --test worker_native "$probe" -- --exact --nocapture
require_line worker-apple 'PASS: worker mount probe apple-containers'

run builtin-hardware make test-builtin-hw
cp "$CARGO_TARGET_DIR/builtin-hw-report.tsv" "$logs/builtin-hw-report.tsv"
if grep -Eq '^(SKIP|BLOCKED)[[:space:]]' "$logs/builtin-hw-report.tsv"; then
    fail "builtin hardware report contains skipped/blocked scenarios"
fi

# Reuse the automatically prepared archive. The driver imports it into fresh
# state and runs the exact same assertions as the Docker/Apple probe. A Cargo
# test harness cannot host the builtin re-exec worker; the signed driver can.
archive=$CARGO_TARGET_DIR/builtin-hw-fixtures/fixture-oci.tar
[ -s "$archive" ] || fail "builtin fixture archive missing"
mkdir -m 700 "$scratch/state"
run worker-builtin "$CARGO_TARGET_DIR/debug/examples/builtin_hw_driver" \
    --scenario worker-mount --state-dir "$scratch/state" --archive "$archive" \
    --tag awman-worker-gate:latest --work "$scratch/probe" \
    --scripts "$root/tools/oci-runtime-spike"
require_line worker-builtin 'PASS: worker mount probe builtin'

# Explicit named reruns provide non-quiet evidence for both migrated daemon
# fixtures. REQUIRE_HW turns missing prerequisites into failure, and an exact
# one-test result prevents a renamed/filtered-out test from passing the gate.
for scenario in api_dispatches_into_a_guest squad_dispatches_a_leader_into_a_guest; do
    test_name=frontends::builtin_hw_actual_awman_$scenario
    run "builtin-$scenario" env AWMAN_TEST_BUILTIN=1 AWMAN_TEST_BUILTIN_REQUIRE_HW=1 \
        AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE="$archive" \
        bash tools/isolated-test.sh -p awman-cli --features builtin-runtime \
        --test builtin_runtime "$test_name" -- --exact --nocapture
    require_line "builtin-$scenario" 'test result: ok. 1 passed; 0 failed; 0 ignored;'
done

run docs-reference make docs-reference
run markdown-projections bash tools/isolated-test.sh -p awman --lib command::dispatch::projections::markdown
for heading in '### `awman worker link`' '## `awman queue`' '### `awman queue pick`'; do
    grep -Fxq "$heading" docs/14-command-reference.md || fail "generated reference lacks $heading"
done
run architecture-lint make architecture-lint
run test-fast make test-fast
printf 'Overall PASS: all remaining native checks and final checks passed.\n' | tee -a "$logs/summary.txt"
