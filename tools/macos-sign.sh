#!/usr/bin/env bash
# Ad-hoc sign an awman executable for macOS with exactly the entitlements in
# packaging/macos/awman.entitlements, then verify the result.
#
# macOS only lets a process start a virtual machine when its code signature
# carries com.apple.security.hypervisor; the experimental builtin runtime needs
# it. The signature is ad-hoc (no Developer ID, no notarization): it proves the
# bytes are unmodified, not who built them. See docs/11-runtimes.md.
#
# Usage: tools/macos-sign.sh PATH_TO_AWMAN
set -euo pipefail
binary=${1:?usage: macos-sign.sh PATH_TO_AWMAN}
root=$(cd "$(dirname "$0")/.." && pwd -P)
entitlements="$root/packaging/macos/awman.entitlements"
[ "$(uname -s)" = Darwin ] || { echo "macos-sign.sh: macOS is required" >&2; exit 2; }
[ -f "$binary" ] || { echo "macos-sign.sh: no such file: $binary" >&2; exit 2; }

codesign --force --sign - --identifier awman --entitlements "$entitlements" "$binary"
codesign --verify --strict --verbose=2 "$binary"

# The embedded entitlements must be exactly the committed file: nothing added.
embedded=$(mktemp)
trap 'rm -f "$embedded"' EXIT
codesign -d --entitlements - --xml "$binary" 2>/dev/null > "$embedded"
want=$(plutil -convert json -o - "$entitlements")
got=$(plutil -convert json -o - "$embedded")
if [ "$want" != "$got" ]; then
    echo "macos-sign.sh: embedded entitlements differ from $entitlements" >&2
    echo "  want: $want" >&2
    echo "  got:  $got" >&2
    exit 1
fi
codesign -dvvv "$binary" 2>&1 | grep -E '^(Identifier|Signature|CDHash)='
echo "signed: $binary (ad-hoc, entitlements: $want)"
