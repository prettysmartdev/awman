#!/usr/bin/env bash
# Exercise how tools/build-previous-builtin-awman.sh picks the revision to
# build (--revision-only, so nothing is compiled) against scratch git
# repositories.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
script="$repo/tools/build-previous-builtin-awman.sh"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/awman-previous-builtin.XXXXXX")
trap 'rm -rf -- "$scratch"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
unset AWMAN_TEST_PREVIOUS_BUILTIN_REV
git_() { git -c user.name=test -c user.email=test@example.invalid -c init.defaultBranch=main -c protocol.file.allow=always "$@"; }
failures=0

runtime() { # protocol [allowlisted...]
  local protocol=$1 entries="" entry
  shift
  for entry in "$@"; do entries="$entries\"$entry\", "; done
  mkdir -p src/data
  printf 'pub const WORKER_PROTOCOL: &str = "%s";\npub const UPGRADABLE_FROM: &[&str] = &[%s];\n' "$protocol" "$entries" > src/data/builtin_runtime.rs
}
commit() { git_ add -A && git_ commit -qm "$1" && git_ rev-parse HEAD; }

expect() { # name want(commit|fail) [env...]
  local name=$1 want=$2 got
  shift 2
  got=$(env "$@" bash "$script" --revision-only "$PWD" 2> "$scratch/err") || got=fail
  if [ "$got" = "$want" ]; then
    echo "ok: $name"
  else
    echo "FAIL: $name: expected $want, got $got" >&2
    sed 's/^/    /' "$scratch/err" >&2
    failures=$((failures + 1))
  fi
}

mkdir "$scratch/history"; cd "$scratch/history"; git_ init -q
runtime "0.7.2/18/awman-1"; first=$(commit first)
echo "// unrelated edit" >> src/data/builtin_runtime.rs; commit edit > /dev/null
echo readme > README; last_old=$(commit "last commit on the old protocol")
runtime "0.7.8/18/awman-1" "0.7.2/18/awman-1"; bump=$(commit bump)
echo more >> README; commit after > /dev/null
expect "the commit just before the protocol changed" "$last_old"
expect "an explicit revision on the previous protocol" "$first" AWMAN_TEST_PREVIOUS_BUILTIN_REV="$first"
expect "an explicit revision on another protocol is refused" fail AWMAN_TEST_PREVIOUS_BUILTIN_REV="$bump"

git_ clone -q --depth 1 "file://$scratch/history" "$scratch/shallow"
cd "$scratch/shallow"; expect "a shallow checkout is refused" fail; cd "$scratch/history"

runtime "0.7.8/18/awman-1"; commit "empty allowlist" > /dev/null
expect "an empty UPGRADABLE_FROM is refused" fail

runtime "0.7.8/18/awman-1" "0.6.0/18/awman-1"; commit "never defined" > /dev/null
expect "a protocol no commit defined is refused" fail

[ "$failures" = 0 ] || { echo "$failures previous builtin awman case(s) failed" >&2; exit 1; }
