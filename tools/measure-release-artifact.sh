#!/usr/bin/env bash
# Capture costs without passing/failing invented thresholds. Optional workload
# argv is expected to run a realistic cached guest session on native hardware.
set -euo pipefail
artifact=${1:?usage: measure-release-artifact.sh ARTIFACT TARGET OUTPUT [WORKLOAD_WRAPPER ARGS...]}
target=${2:?target triple required}
output=${3:?raw output directory required}
shift 3
host_only=0
if [ "${1:-}" = --host-only ]; then host_only=1; shift; fi
artifact=$(cd "$(dirname "$artifact")" && pwd -P)/$(basename "$artifact")
test -x "$artifact"
mkdir -p "$output/home" "$output/tmp"
output=$(cd "$output" && pwd -P)
test ! -e "$output/state" || { echo "measurement output already has a state directory: $output/state" >&2; exit 2; }
# The guest worker's control socket adds 56 bytes below the state directory.
# Keep the active path short on macOS, then retain the final state as evidence.
state=$(cd "$(mktemp -d /tmp/awm.XXXXXX)" && pwd -P)
preserve_state() {
  local status=$?
  trap - EXIT
  if [ -d "$state" ] && ! mv "$state" "$output/state"; then
    echo "could not preserve measurement state at $output/state; active state: $state" >&2
    [ "$status" -ne 0 ] || status=1
  fi
  exit "$status"
}
trap preserve_state EXIT
test "${#state}" -le 47 || { echo "BLOCKED: measurement state path is too long for guest sockets: $state" >&2; exit 4; }
printf 'target=%s\nartifact=%s\nbytes=%s\n' "$target" "$artifact" "$(wc -c < "$artifact" | tr -d ' ')" > "$output/identity.txt"
hash_file() { if command -v sha256sum >/dev/null; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi; }
printf 'artifact_sha256=%s\n' "$(hash_file "$artifact")" >> "$output/identity.txt"
run_env=(env -i PATH=/usr/bin:/bin HOME="$output/home" TMPDIR="$output/tmp" XDG_CONFIG_HOME="$output/home/config" XDG_DATA_HOME="$output/home/data" XDG_CACHE_HOME="$output/home/cache")
run_measured() {
  local label=$1 stdout=$2 stderr=$3 status=0
  shift 3
  "$@" > "$stdout" 2> "$stderr" || status=$?
  if [ "$status" -ne 0 ]; then
    echo "$label failed (exit $status):" >&2
    tail -n 40 "$stdout" "$stderr" >&2
    exit "$status"
  fi
}
test -x /usr/bin/time || { echo "BLOCKED: /usr/bin/time is required for release measurements" >&2; exit 4; }
for n in 1 2; do
  if [ "$(uname -s)" = Linux ]; then
    run_measured "startup $n" "$output/startup-$n.stdout" "$output/startup-$n.stderr" \
      /usr/bin/time -v -o "$output/startup-$n.time" "${run_env[@]}" "$artifact" --version
  else
    run_measured "startup $n" "$output/startup-$n.stdout" "$output/startup-$n.time" \
      /usr/bin/time -l "${run_env[@]}" "$artifact" --version
  fi
done
du -ak "$state" > "$output/disk-state.txt"
du -ak "$output/home" >> "$output/disk-state.txt"
if [ "$host_only" -eq 1 ]; then
  echo 'NOT APPLICABLE: builtin guest workload for this existing-backend artifact' > "$output/workload.status"
elif [ "$#" -gt 0 ]; then
  missing_guest_rss=0
  for phase in cold warm; do
    if [ "$(uname -s)" = Linux ]; then
      AWMAN_GUEST_RSS_OUTPUT="$output/guest-rss-$phase.txt" \
        run_measured "workload $phase" "$output/workload-$phase.stdout" "$output/workload-$phase.stderr" \
        /usr/bin/time -v -o "$output/workload-$phase.time" "$@" "$artifact" "$state"
    else
      AWMAN_GUEST_RSS_OUTPUT="$output/guest-rss-$phase.txt" \
        run_measured "workload $phase" "$output/workload-$phase.stdout" "$output/workload-$phase.time" \
        /usr/bin/time -l "$@" "$artifact" "$state"
    fi
    if [ ! -s "$output/guest-rss-$phase.txt" ]; then
      echo "BLOCKED: workload wrapper did not record guest RSS for $phase run" > "$output/guest-rss-$phase.txt"
      missing_guest_rss=1
    fi
    du -ak "$state" > "$output/disk-state-after-$phase.txt"
  done
  if [ "$missing_guest_rss" -ne 0 ]; then echo "BLOCKED: workload wrapper must write guest RSS bytes to AWMAN_GUEST_RSS_OUTPUT for both runs" >&2; exit 4; fi
else
  echo 'BLOCKED: realistic git/build-tools guest workload wrapper not supplied' > "$output/workload.status"
  echo "BLOCKED: provide the realistic guest workload wrapper" >&2
  exit 4
fi
trap - EXIT
if ! mv "$state" "$output/state"; then
  echo "could not preserve measurement state at $output/state; active state: $state" >&2
  exit 1
fi
echo "PASS measurements captured without thresholds: $output"
