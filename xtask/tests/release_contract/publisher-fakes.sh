#!/usr/bin/env bash
set -euo pipefail
persona=$(basename "$0")
{ printf '%s' "$persona"; printf ' %s' "$@"; printf '\n'; } >> "$FAKE_STATE/calls"
state=$(cat "$FAKE_STATE/state")
commit=${FAKE_COMMIT:?}
case "$persona:$*" in
  "git:branch --show-current") echo main ;;
  "git:status --porcelain") ;;
  "git:rev-parse HEAD") echo "$commit" ;;
  "git:ls-remote origin refs/heads/main") printf '%s\trefs/heads/main\n' "$commit" ;;
  cargo:xtask\ release\ run\ --internal-state*)
    case "$state" in
      none|abandoned) printf '%s||||false\n' "$state" ;;
      *) recorded=$(cat "$FAKE_STATE/commit" 2>/dev/null || echo "$commit")
         printf '%s|1|%s|44|false\n' "$state" "$recorded" ;;
    esac ;;
  cargo:xtask\ release\ runner*) ;;
  cargo:xtask\ release\ promote\ verify-inputs*) ;;
  cargo:xtask\ release\ promote\ tag*)
    test "$state" = validated || test "$state" = promoted
    touch "$FAKE_STATE/tag" ;;
  cargo:xtask\ release\ promote\ publish*)
    test -f "$FAKE_STATE/tag"
    echo promoted > "$FAKE_STATE/state" ;;
  gh:workflow\ run\ candidate.yml*)
    echo prepared > "$FAKE_STATE/state" ;;
  gh:workflow\ run\ promote.yml*)
    test -f "$FAKE_STATE/tag"
    touch "$FAKE_STATE/verification-dispatched" ;;
  gh:run\ watch\ 44*)
    test "${FAIL_CANDIDATE:-0}" = 0 || exit 1
    echo validated > "$FAKE_STATE/state" ;;
  gh:run\ watch\ 55*)
    test -f "$FAKE_STATE/verification-dispatched"
    echo verified > "$FAKE_STATE/state" ;;
  gh:run\ list*)
    if test -f "$FAKE_STATE/verification-dispatched"; then echo 55; else echo 0; fi ;;
  *) echo "unexpected fake command: $persona $*" >&2; exit 127 ;;
esac
