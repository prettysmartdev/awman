#!/usr/bin/env bash
# Sign the executables the builtin hardware tests boot VMs from: the awman
# binary and the example guest drivers in a cargo profile directory. macOS
# refuses Hypervisor.framework to unsigned processes; cargo produces unsigned
# (linker ad-hoc, no entitlements) binaries. No-op off macOS.
#
# Usage: tools/macos-sign-test-artifacts.sh PROFILE_DIR   (e.g. target/debug)
set -euo pipefail
dir=${1:?usage: macos-sign-test-artifacts.sh PROFILE_DIR}
[ "$(uname -s)" = Darwin ] || exit 0
root=$(cd "$(dirname "$0")/.." && pwd -P)
# Cargo links into deps/ (and examples/<name>-<hash>) and re-copies the result
# to the uplifted path on every build, so both copies are signed: signing only
# the uplifted file is undone by the next cargo invocation.
for binary in "$dir/awman" "$dir"/deps/awman-* \
              "$dir"/examples/builtin_*_driver "$dir"/examples/builtin_*_driver-*; do
    [ -f "$binary" ] && [ -x "$binary" ] || continue
    case "$binary" in *.d|*.rlib|*.rmeta|*.o|*.dSYM) continue ;; esac
    file -b "$binary" | grep -q "Mach-O" || continue
    bash "$root/tools/macos-sign.sh" "$binary" >/dev/null 2>&1
    echo "signed for local hardware tests: $binary"
done
