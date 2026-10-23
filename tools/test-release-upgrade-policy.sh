#!/usr/bin/env bash
# Exercise tools/release-upgrade-policy.sh against scratch git repositories:
# full, shallow and non-git checkouts, malformed and pre-protocol previous
# releases, and the allowlist / release-note exceptions.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
policy="$repo/tools/release-upgrade-policy.sh"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/awman-upgrade-policy.XXXXXX")
trap 'rm -rf -- "$scratch"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
git_() { git -c user.name=test -c user.email=test@example.invalid -c init.defaultBranch=main -c protocol.file.allow=always "$@"; }
failures=0

runtime() { # protocol [allowlisted...]
  local protocol=$1 entries="" entry
  shift
  for entry in "$@"; do entries="$entries\"$entry\", "; done
  mkdir -p src/data
  printf 'pub const WORKER_PROTOCOL: &str = "%s";\npub const UPGRADABLE_FROM: &[&str] = &[%s];\n' "$protocol" "$entries" > src/data/builtin_runtime.rs
}

# Builds a repository whose v1.0.0 has WORKER_PROTOCOL $1 (or a file $1 when
# it starts with "file:", or nothing when it is "none"), then commits HEAD.
fixture() {
  local name=$1 previous=$2
  rm -rf "$scratch/$name"
  mkdir -p "$scratch/$name"
  cd "$scratch/$name"
  git_ init -q
  case "$previous" in
    none) echo readme > README ;;
    file:*) mkdir -p src/data; printf '%s\n' "${previous#file:}" > src/data/builtin_runtime.rs ;;
    *) runtime "$previous" ;;
  esac
  git_ add -A && git_ commit -qm v1 && git_ tag v1.0.0
}

commit_head() {
  git_ add -A && git_ commit -qm head
}

expect() { # pass|fail name dir
  local want=$1 name=$2 dir=$3 got=pass
  bash "$policy" "$dir" 2.0.0 > "$scratch/out" 2>&1 || got=fail
  if [ "$got" = "$want" ]; then
    echo "ok: $name"
  else
    echo "FAIL: $name: expected $want, got $got" >&2
    sed 's/^/    /' "$scratch/out" >&2
    failures=$((failures + 1))
  fi
}

fixture changed "0.7.2/18/awman-1"
runtime "0.7.8/18/awman-1"; commit_head
expect fail "a changed protocol without an allowlist entry or a reason" "$PWD"
runtime "0.7.8/18/awman-1" "0.7.2/18/awman-1"; commit_head
expect pass "the previous protocol is allowlisted" "$PWD"
runtime "0.7.8/18/awman-1" "0.7.2/18/awman-1/extra"; commit_head
expect fail "only an exact allowlist entry counts" "$PWD"

mkdir -p docs/releases
runtime "0.7.8/18/awman-1"; printf '# v2\n\n- No in-place builtin state upgrade:\n' > docs/releases/v2.0.0.md; commit_head
expect fail "the release-note exception needs a reason" "$PWD"
printf '# v2\n\n- No in-place builtin state upgrade: the catalog format was replaced.\n' > docs/releases/v2.0.0.md; commit_head
expect pass "the release-note exception with a reason" "$PWD"
rm -r docs; commit_head

git_ clone -q --depth 1 "file://$scratch/changed" "$scratch/shallow"
expect fail "a shallow checkout cannot skip the check" "$scratch/shallow"
mkdir -p "$scratch/plain/src/data"
cp "$scratch/changed/src/data/builtin_runtime.rs" "$scratch/plain/src/data/"
expect fail "a directory that is not a git checkout cannot skip the check" "$scratch/plain"

fixture malformed "file:pub const WORKER_PROTOCOL: &str = concat!(SOMETHING_ELSE);"
runtime "0.7.8/18/awman-1"; commit_head
expect fail "an unparseable previous protocol" "$PWD"

fixture pre-protocol none
runtime "0.7.8/18/awman-1"; commit_head
expect pass "a previous release without a builtin runtime is skipped" "$PWD"

fixture unchanged "0.7.8/18/awman-1"
echo change > CHANGE; commit_head
expect pass "an unchanged protocol" "$PWD"

rm -rf "$scratch/first"; mkdir -p "$scratch/first"; cd "$scratch/first"; git_ init -q
runtime "0.7.8/18/awman-1"; git_ add -A; git_ commit -qm first
expect pass "the first release has nothing to compare against" "$PWD"

[ "$failures" = 0 ] || { echo "$failures release upgrade policy case(s) failed" >&2; exit 1; }
