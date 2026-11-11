#!/usr/bin/env bash
# Give the test runner a private HOME so its canary does not include Cargo
# caches or the checked-out workspace while preserving the installed toolchain.
set -euo pipefail
[ "$#" -gt 0 ] || { echo "usage: ci-test-home.sh COMMAND [ARGS...]" >&2; exit 2; }
original_home=${HOME:?HOME is required}
temp_root=${RUNNER_TEMP:-${TMPDIR:-/tmp}}
if [ "${RUNNER_OS:-}" = Windows ]; then
    native_path() { cygpath -w "$1"; }
    temp_root=$(cygpath -u "$temp_root")
else
    native_path() { printf '%s\n' "$1"; }
fi
canary_dir=$(mktemp -d "$temp_root/awman-canary.XXXXXX")
trap 'rm -rf -- "$canary_dir"' EXIT
CARGO_HOME=${CARGO_HOME:-$(native_path "$original_home/.cargo")}
RUSTUP_HOME=${RUSTUP_HOME:-$(native_path "$original_home/.rustup")}
HOME=$(native_path "$canary_dir")
export CARGO_HOME RUSTUP_HOME HOME
"$@"
