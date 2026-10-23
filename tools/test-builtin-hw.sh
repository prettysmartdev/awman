#!/usr/bin/env bash
# `make test-builtin-hw`: every builtin runtime test, booting real guests on
# this machine, with no setup or variables. It
#   1. checks this host can boot a guest (Apple Silicon with
#      Hypervisor.framework, or Linux with a writable /dev/kvm);
#   2. fetches and verifies this host's builtin payload when needed;
#   3. prepares the guest fixtures (tools/builtin-hw-fixtures.sh: skopeo under
#      Docker or Apple Containers, cached in target/builtin-hw-fixtures/) and,
#      on macOS, loads the fixture into Apple Containers;
#   4. runs `make test-builtin` with real guests required (a missing
#      prerequisite fails, never skips) and the network, pressure and
#      measurement suites on. That builds the previous-protocol awman for the
#      upgrade test from git history (target/previous-builtin/);
#   5. runs tools/test-msb-catalog-compat.sh (published 0.7.2 -> 0.7.8 catalog,
#      then awman's own upgrade of it).
# The per-test SKIP/BLOCKED/RUN report is left in target/builtin-hw-report.tsv.
# Variables the steps already honour (AWMAN_TEST_BUILTIN_BASE_TAR,
# AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE, AWMAN_TEST_BUILTIN_PREVIOUS, ...) still
# override them, but none is needed.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$root"
. tools/builtin-hw-fixtures.sh
step() { printf '\n==> %s\n' "$*"; }

host=$(rustc -vV | sed -n 's/^host: //p')
case "$(uname -s):$(uname -m)" in
    Darwin:arm64) [ "$(sysctl -n kern.hv_support)" = 1 ] || builtin_hw_blocked "Hypervisor.framework unavailable" ;;
    Linux:*) [ -r /dev/kvm ] && [ -w /dev/kvm ] || builtin_hw_blocked "writable /dev/kvm is required" ;;
    *) builtin_hw_blocked "the builtin runtime needs Apple Silicon or Linux with KVM, not $(uname -s)/$(uname -m)" ;;
esac
# Cargo must not build as root (root-owned target/ and caches).
[ "$(id -u)" != 0 ] || builtin_hw_blocked "run as your own user, not root"

step "Builtin payload for $host"
if ! bash tools/msb-payloads/verify.sh "$host" >/dev/null 2>&1; then
    rc=0; bash tools/msb-payloads/fetch.sh "$host" || rc=$?
    [ "$rc" = 0 ] || [ "$rc" = 4 ] || exit "$rc"
    bash tools/msb-payloads/verify.sh "$host" ||
        builtin_hw_blocked "the $host payload is not verified; promote it with \`make payloads-verify\`"
fi
echo "verified"

step "Guest fixtures"
target_dir=${CARGO_TARGET_DIR:-$root/target}
trap release_builtin_hw_fixtures EXIT
prepare_builtin_hw_fixtures "$target_dir/builtin-hw-fixtures"
echo "fixture: $AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE"

export AWMAN_TEST_BUILTIN=1 AWMAN_TEST_BUILTIN_REQUIRE_HW=1
export AWMAN_TEST_BUILTIN_NETWORK=1 AWMAN_TEST_BUILTIN_PRESSURE=1 AWMAN_TEST_BUILTIN_MEASURE=1
export AWMAN_TEST_BUILTIN_REPORT=$target_dir/builtin-hw-report.tsv
: > "$AWMAN_TEST_BUILTIN_REPORT"

step "Builtin tests with real guests (make test-builtin)"
make --no-print-directory test-builtin

step "Catalog compatibility (published 0.7.2 catalog -> awman upgrade)"
bash tools/test-msb-catalog-compat.sh

step "PASS: builtin runtime on $host"
echo "Per-test report: $AWMAN_TEST_BUILTIN_REPORT"
cat "$AWMAN_TEST_BUILTIN_REPORT"
