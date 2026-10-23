#!/usr/bin/env bash
# Build, cache and sign the awman that last shipped the builtin worker protocol
# this build upgrades from, for the old-to-new builtin hardware test
# (builtin_hw_actual_awman_boots_after_upgrading_a_072_state_directory).
# `make test-builtin` and tools/native-builtin-ci.sh call it; nobody needs to
# build the older awman by hand.
#
# Usage: tools/build-previous-builtin-awman.sh [--revision-only | --if-hypervisor] [ROOT]
# ROOT is the git checkout to build from (default: this one). With
# --revision-only, prints the chosen commit and builds nothing. Otherwise prints
# the directory holding `awman`, `protocol`, `revision` and `runtime`
# (the global `runtime` value that awman accepts for the builtin runtime).
# With --if-hypervisor, a host that cannot boot a guest (no writable /dev/kvm,
# no Hypervisor.framework) prints nothing and builds nothing: the hardware test
# reports itself BLOCKED there anyway.
#
# The revision is found in git history: the first-parent commit just before
# src/data/builtin_runtime.rs stopped defining the first UPGRADABLE_FROM
# protocol (HEAD itself while it still defines it). Set
# AWMAN_TEST_PREVIOUS_BUILTIN_REV to build another revision; its protocol must
# still be that UPGRADABLE_FROM entry. The source comes from `git archive`, so
# the checkout and its worktrees are untouched. The build uses that revision's
# own Cargo.lock, payload pins, payload fetch script and (on Linux) native
# libcap-ng build script, in its own target
# directory, and is reused until the revision changes.
set -euo pipefail
tools=$(cd "$(dirname "$0")" && pwd -P)
revision_only=false
if [ "${1:-}" = --revision-only ]; then
    revision_only=true
    shift
elif [ "${1:-}" = --if-hypervisor ]; then
    shift
    # The same checks as tests/builtin_runtime/gate.rs (hypervisor).
    case "$(uname -s):$(uname -m)" in
        Linux:*) [ -r /dev/kvm ] && [ -w /dev/kvm ] || exit 0 ;;
        Darwin:arm64) [ "$(sysctl -n kern.hv_support 2>/dev/null)" = 1 ] || exit 0 ;;
        *) exit 0 ;;
    esac
fi
root=$(cd "${1:-$tools/..}" && pwd -P)
runtime_src=src/data/builtin_runtime.rs
protocol_of() { awk -f "$tools/worker-protocol.awk"; }
fail() { echo "build-previous-builtin-awman: $*" >&2; exit 1; }

previous=$(sed 's://.*$::' "$root/$runtime_src" | awk '!done && /UPGRADABLE_FROM[^;]*=/ {on=1} on {print} on && /;/ {on=0; done=1}' |
    grep -oE '"[^"]*"' | tr -d '"' | sed -n 1p)
[ -n "$previous" ] || fail "UPGRADABLE_FROM in $runtime_src is empty: there is no previous protocol to build"

[ "$(git -C "$root" rev-parse --is-shallow-repository 2>/dev/null)" = false ] ||
    fail "$root is not a full git checkout; the previous awman is built from git history (git fetch --unshallow)"
at() { git -C "$root" show "$1:$runtime_src" 2>/dev/null | protocol_of; }

rev=${AWMAN_TEST_PREVIOUS_BUILTIN_REV:-}
if [ -z "$rev" ]; then
    newer=
    for commit in $(git -C "$root" rev-list --first-parent HEAD -- "$runtime_src"); do
        if [ "$(at "$commit")" = "$previous" ]; then
            if [ -z "$newer" ]; then rev=HEAD; else rev="$newer^1"; fi
            break
        fi
        newer=$commit
    done
    [ -n "$rev" ] || fail "no commit in the history of HEAD defines WORKER_PROTOCOL $previous"
fi
rev=$(git -C "$root" rev-parse --verify --quiet "$rev^{commit}") || fail "cannot resolve revision $rev"
[ "$(at "$rev")" = "$previous" ] ||
    fail "revision $rev defines WORKER_PROTOCOL '$(at "$rev")', not $previous"
if $revision_only; then echo "$rev"; exit 0; fi

case "$(uname -s):$(uname -m)" in
    Darwin:arm64) target=aarch64-apple-darwin ;;
    Linux:aarch64) target=aarch64-unknown-linux-gnu ;;
    Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
    *) fail "no builtin runtime on $(uname -s)/$(uname -m)" ;;
esac

out="${CARGO_TARGET_DIR:-$root/target}/previous-builtin/${previous//\//_}"
binary="$out/target/debug/awman"
if [ "$(cat "$out/revision" 2>/dev/null)" != "$rev" ] || [ ! -x "$binary" ]; then
    echo "building the previous builtin awman ($previous) from $rev" >&2
    rm -rf "$out"
    mkdir -p "$out/src"
    git -C "$root" archive "$rev" | tar -x -C "$out/src"
    (
        cd "$out/src"
        if [ -f tools/msb-payloads/fetch.sh ]; then bash tools/msb-payloads/fetch.sh "$target" >&2; fi
        # Trees before WI 0120 link a pinned static libcap-ng on Linux, built
        # by their own script (their `make payloads` ran it).
        if [ "$(uname -s)" = Linux ] && [ -f third_party/native/libcap-ng/build.sh ]; then
            bash third_party/native/libcap-ng/build.sh "$target" >&2
        fi
        CARGO_TARGET_DIR="$out/target" cargo build --locked --features builtin-runtime --bin awman >&2
    )
    # Older awman spelled the builtin runtime `builtin`.
    if grep -rqF '"builtin-experimental"' "$out/src/src"; then runtime=builtin-experimental; else runtime=builtin; fi
    printf '%s' "$runtime" > "$out/runtime"
    printf '%s' "$previous" > "$out/protocol"
    printf '%s' "$rev" > "$out/revision"
fi
# Hypervisor.framework needs the entitlement; cargo relinks unsigned. Signing
# with this checkout's tool and entitlements keeps the old tree's scripts out
# of the trust path.
if [ "$(uname -s)" = Darwin ]; then bash "$tools/macos-sign.sh" "$binary" >&2; fi
ln -sf target/debug/awman "$out/awman"
echo "$out"
