#!/usr/bin/env bash
set -euo pipefail

if test "${0##*/}" = python3; then
  test "$1" = -
  grep -Fq '"gate":"main"' "$2"
  grep -Fq "\"commit\":\"$3\"" "$2"
  grep -Fq '"verdict":"passed"' "$2"
  exit 0
fi

printf '%s\n' "$*" >> "$FAKE_STATE/calls"
case "$1 $2" in
  'api --paginate')
    case "$FAKE_CASE" in
      reuse) printf '100\n99\n' ;;
      missing) ;;
      *) printf '100\n' ;;
    esac
    ;;
  'run view')
    if test "$FAKE_CASE" = reuse; then printf 'true\n'; fi
    ;;
  'run download')
    run=$3
    shift 3
    while test "$#" -gt 0; do
      if test "$1" = --dir; then dir=$2; break; fi
      shift
    done
    mkdir -p "$dir"
    if test "$run" = 100; then
      printf '%s\n' '{"gate":"main","commit":"wrong","verdict":"passed"}' > "$dir/gate-evidence.json"
    else
      printf '{"gate":"main","commit":"%s","verdict":"passed"}\n' "$FAKE_COMMIT" > "$dir/gate-evidence.json"
    fi
    ;;
  'run list')
    if test -f "$FAKE_STATE/dispatched"; then printf '101\n'; else printf '100\n'; fi
    ;;
  'workflow run')
    touch "$FAKE_STATE/dispatched"
    ;;
  'run watch')
    test "$3" = 101
    ;;
  *) echo "Unexpected fake gh call: $*" >&2; exit 1 ;;
esac
