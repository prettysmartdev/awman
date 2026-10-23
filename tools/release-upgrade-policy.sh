#!/usr/bin/env bash
# Builtin state upgrade policy for a release (called by
# tools/release-artifact-check.sh; usage: release-upgrade-policy.sh ROOT VERSION).
#
# When WORKER_PROTOCOL changed since the previous release tag, the previous
# protocol must be listed in UPGRADABLE_FROM, or docs/releases/vVERSION.md must
# contain a line starting with the phrase below, followed by the reason.
#
# The check needs the full history and tags. It fails, rather than skipping,
# when they are missing (a shallow checkout), when git fails, or when the
# previous release's protocol cannot be parsed. It is skipped only when there
# is no earlier release tag, or the earlier release predates
# src/data/builtin_runtime.rs (no builtin worker protocol at all).
set -euo pipefail
root=${1:?usage: release-upgrade-policy.sh ROOT VERSION}
version=${2:?usage: release-upgrade-policy.sh ROOT VERSION}
no_upgrade_phrase='No in-place builtin state upgrade:'
runtime_src=src/data/builtin_runtime.rs

fail() {
  echo "builtin state upgrade check: $*" >&2
  exit 1
}

tools=$(cd "$(dirname "$0")" && pwd -P)
worker_protocol_of() { awk -f "$tools/worker-protocol.awk"; }

current=$(worker_protocol_of < "$root/$runtime_src")
[ -n "$current" ] || fail "cannot parse WORKER_PROTOCOL from $runtime_src"

shallow=$(git -C "$root" rev-parse --is-shallow-repository 2>/dev/null) ||
  fail "$root is not a git checkout; the previous release's protocol cannot be determined"
[ "$shallow" = false ] ||
  fail "shallow checkout: fetch the full history and tags (actions/checkout: fetch-depth: 0, fetch-tags: true)"
tags=$(git -C "$root" tag --list 'v*' --sort=-v:refname) || fail "cannot list release tags"

prev_tag=
for tag in $tags; do
  [ "$tag" != "v$version" ] || continue
  if [ "$(printf '%s\n%s\n' "$tag" "v$version" | sort -V | head -1)" = "$tag" ]; then prev_tag=$tag; break; fi
done
if [ -z "$prev_tag" ]; then
  echo "note: no release tag before v$version; skipping the builtin state upgrade check" >&2
  exit 0
fi
git -C "$root" rev-parse --verify --quiet "$prev_tag^{commit}" > /dev/null ||
  fail "cannot resolve $prev_tag to a commit"
listing=$(git -C "$root" ls-tree --name-only "$prev_tag" -- "$runtime_src") ||
  fail "cannot read the tree of $prev_tag"
if [ -z "$listing" ]; then
  echo "note: $prev_tag predates $runtime_src (no builtin worker protocol); skipping the builtin state upgrade check" >&2
  exit 0
fi
prev_source=$(git -C "$root" show "$prev_tag:$runtime_src") || fail "cannot read $runtime_src at $prev_tag"
prev_protocol=$(printf '%s\n' "$prev_source" | worker_protocol_of)
[ -n "$prev_protocol" ] || fail "cannot parse WORKER_PROTOCOL from $runtime_src at $prev_tag"
[ "$current" != "$prev_protocol" ] || exit 0

listed=$(sed 's://.*$::' "$root/$runtime_src" | awk '/UPGRADABLE_FROM[^;]*=/ {on=1} on {print} on && /;/ {exit}' |
  grep -oE '"[^"]*"' | tr -d '"' | grep -Fx -- "$prev_protocol" || true)
[ -z "$listed" ] || exit 0
notes="$root/docs/releases/v$version.md"
if [ -f "$notes" ] && grep -Eq "^[-*[:space:]]*$no_upgrade_phrase[[:space:]]*[^[:space:]]" "$notes"; then exit 0; fi
{
  echo "builtin worker protocol changed: $prev_protocol ($prev_tag) -> $current"
  echo "Existing builtin state from $prev_tag cannot be upgraded in place. Either:"
  echo "  - add \"$prev_protocol\" to UPGRADABLE_FROM in $runtime_src, or"
  echo "  - state why it cannot be upgraded in docs/releases/v$version.md, in a line starting with"
  echo "    \"$no_upgrade_phrase\" followed by the reason."
} >&2
exit 1
