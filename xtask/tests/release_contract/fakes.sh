#!/usr/bin/env bash
# Stateful transport adapters for a scratch repo. No network command is forwarded.
set -euo pipefail
trap 'exit 130' INT TERM
persona=$(basename "$0")
# keep each recorded argv readable.
# shellcheck disable=SC2129
printf '%s' "$persona" >> "$FAKE_CALLS"
printf ' <%s>' "$@" >> "$FAKE_CALLS"
printf '\n' >> "$FAKE_CALLS"
if [ "${HOLD_AT:-}" = "$persona/${1:-}" ]; then
  if [ "$persona/${1:-}" != git/tag ] || [ "$#" -eq 2 ]; then
    touch "$FAKE_STATE/held"
    echo HOLD_READY
    while :; do /bin/sleep 0.1; done
  fi
fi
case "$persona" in
 git)
  case "$1" in
   fetch) exit 0 ;;
   push)
    [ "${FAIL_GIT:-}" != push ] || exit 7
    if [ "$3" = main ] || [ "$3" = HEAD:main ]; then "$REAL_GIT" update-ref refs/remotes/origin/main HEAD
    else "$REAL_GIT" rev-parse "$3^{commit}" > "$FAKE_STATE/remote-tag"; fi
    exit 0 ;;
   ls-remote)
    if [ -f "$FAKE_STATE/remote-tag" ]; then
      target=$(cat "$FAKE_STATE/remote-tag")
      [ -n "$target" ] || target=$("$REAL_GIT" rev-parse HEAD)
      printf '%s\trefs/tags/v1.2.3\n' "$target"
    fi
    exit 0 ;;
   commit) [ "${FAIL_GIT:-}" != commit ] || exit 7 ;;
   tag) if [ "$#" = 2 ]; then [ "${FAIL_GIT:-}" != tag ] || exit 7; fi ;;
  esac
  exec "$REAL_GIT" "$@" ;;
 gh)
  case "$1 $2" in
   'auth status') exit "${AUTH_FAIL:-0}" ;;
   'repo view') echo o/r ;;
   'workflow run')
    case "$*" in *'"record_only":true'*)
      printf '%s\n' "$*" >> "$FAKE_STATE/journals"; exit 0 ;;
    esac
    [ "${FAIL_DISPATCH:-0}" = 0 ] || exit 7
    echo prepared > "$FAKE_STATE/state"
    "$REAL_GIT" rev-parse HEAD > "$FAKE_STATE/commit"
    for arg in "$@"; do case "$arg" in operator_record=*) printf '%s' "${arg#operator_record=}" > "$FAKE_STATE/operator.json";; esac; done ;;
   'run list')
    case "$*" in
      *--status*queued*) if [ -f "$FAKE_STATE/queued" ]; then cat "$FAKE_STATE/queued"; [ -s "$FAKE_STATE/queued" ] || echo 88; fi ;;
      *--status*) if [ -f "$FAKE_STATE/in-progress" ]; then cat "$FAKE_STATE/in-progress"; fi ;;
      *) echo 99 ;;
    esac ;;
   'api repos/o/r/actions/runs/88/jobs'|'api repos/o/r/actions/runs/89/jobs')
    # 88 waits for release hardware; 89 is a CI job and must be excluded.
    # Model the query, rather than accepting any label and hiding regressions.
    case "$2:$*" in
      *runs/88/jobs:*'index("awman-release")'*) echo 888 ;;
      *runs/89/jobs:*'index("hypervisor")'*) echo 889 ;;
    esac ;;
   'run cancel') touch "$FAKE_STATE/cancelled" ;;
   'run view') cat "$FAKE_STATE/conclusion" 2>/dev/null || true ;;
   'run rerun') rm -f "$FAKE_STATE/conclusion" ;;
   'run watch')
    [ "${WATCH_FAIL:-0}" = 0 ] || exit 7
    [ "${WATCH_HOLD:-0}" = 0 ] || { echo WATCH_READY; while :; do /bin/sleep 0.1; done; }
    echo verified > "$FAKE_STATE/state"
    "$REAL_GIT" rev-parse 'v1.2.3^{commit}' > "$FAKE_STATE/remote-tag"
    mkdir -p "$FAKE_STATE/assets"
    for asset in awman-linux-amd64 awman-linux-arm64 awman-macos-arm64 awman-windows-amd64.exe; do
      printf '%s\n' "$asset" > "$FAKE_STATE/assets/$asset"
    done
    (cd "$FAKE_STATE/assets"; for asset in awman-*; do shasum -a 256 "$asset"; done) > "$FAKE_STATE/assets/SHA256SUMS"
    cp docs/releases/v1.2.3.md "$FAKE_STATE/body" ;;
   *) echo "unexpected fake gh: $*" >&2; exit 127 ;;
  esac ;;
 operator-adapter)
  if [ "$1 $2 $3" = 'release run --internal-state' ]; then
    if [ "${FORCE_NO_STATE:-0}" = 1 ]; then echo 'none||||false'; exit 0; fi
    state=$(cat "$FAKE_STATE/state" 2>/dev/null || echo none)
    if [ "$state" = none ]; then echo 'none||||false'
    else
      commit=$(cat "$FAKE_STATE/commit")
      tag=false; [ ! -f "$FAKE_STATE/remote-tag" ] || tag=true
      printf '%s|1|%s|99|%s\n' "$state" "$commit" "$tag"
    fi
  elif [ "$1 $2" = 'release runner' ]; then
    case "$3" in
     busy) cat "$FAKE_STATE/online" 2>/dev/null || true ;;
     check)
      [ "${HARDWARE:-1}" = 1 ] || { echo 'missing: Hypervisor.framework' >&2; exit 1; }
      [ "${FULL_CHECK_FAIL:-0}" = 0 ] || [ "${4:-}" = --hardware ] || exit 1 ;;
     provision) [ "${PROVISION_FAIL:-0}" = 0 ] || exit 1 ;;
     up) if [ "${HOLD_AT:-}" = runner/up ]; then touch "$FAKE_STATE/held"; echo HOLD_READY; while :; do /bin/sleep 0.1; done; fi; touch "$FAKE_STATE/runner"; [ "${UP_FAIL:-0}" = 0 ] || exit 7 ;;
     down)
      echo TEARDOWN_READY
      if [ "${DOWN_HOLD:-0}" = 1 ]; then
        while [ ! -f "$FAKE_STATE/release-down" ]; do /bin/sleep 0.1; done
      fi
      rm -f "$FAKE_STATE/runner" ;;
     *) exit 127 ;;
    esac
  else exec "$REAL_XTASK" "$@"; fi ;;
 *) echo "unknown fake persona $persona" >&2; exit 127 ;;
esac
