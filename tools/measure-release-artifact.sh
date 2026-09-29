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
mkdir -p "$output/home" "$output/tmp" "$output/state"
output=$(cd "$output" && pwd -P)
printf 'target=%s\nartifact=%s\nbytes=%s\n' "$target" "$artifact" "$(wc -c < "$artifact" | tr -d ' ')" > "$output/identity.txt"
hash_file() { if command -v sha256sum >/dev/null; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi; }
printf 'artifact_sha256=%s\n' "$(hash_file "$artifact")" >> "$output/identity.txt"
run_env=(env -i PATH=/usr/bin:/bin HOME="$output/home" TMPDIR="$output/tmp" XDG_CONFIG_HOME="$output/home/config" XDG_DATA_HOME="$output/home/data" XDG_CACHE_HOME="$output/home/cache")
for n in 1 2; do
  if [ "$(uname -s)" = Linux ]; then
    /usr/bin/time -v -o "$output/startup-$n.time" "${run_env[@]}" "$artifact" --version \
      > "$output/startup-$n.stdout" 2> "$output/startup-$n.stderr"
  else
    /usr/bin/time -l "${run_env[@]}" "$artifact" --version > "$output/startup-$n.stdout" 2> "$output/startup-$n.time"
  fi
done
du -ak "$output/state" > "$output/disk-state.txt"
du -ak "$output/home" >> "$output/disk-state.txt"
if [ "$host_only" -eq 1 ]; then
  echo 'NOT APPLICABLE: builtin guest workload for this existing-backend artifact' > "$output/workload.status"
elif [ "$#" -gt 0 ]; then
  missing_guest_rss=0
  for phase in cold warm; do
    if [ "$(uname -s)" = Linux ]; then
      command -v /usr/bin/time >/dev/null || { echo "BLOCKED: GNU time required for workload RSS" >&2; exit 4; }
      AWMAN_GUEST_RSS_OUTPUT="$output/guest-rss-$phase.txt" /usr/bin/time -v -o "$output/workload-$phase.time" "$@" "$artifact" "$output/state" \
        > "$output/workload-$phase.stdout" 2> "$output/workload-$phase.stderr"
    else
      AWMAN_GUEST_RSS_OUTPUT="$output/guest-rss-$phase.txt" /usr/bin/time -l "$@" "$artifact" "$output/state" \
        > "$output/workload-$phase.stdout" 2> "$output/workload-$phase.time"
    fi
    if [ ! -s "$output/guest-rss-$phase.txt" ]; then
      echo "BLOCKED: workload wrapper did not record guest RSS for $phase run" > "$output/guest-rss-$phase.txt"
      missing_guest_rss=1
    fi
    du -ak "$output/state" > "$output/disk-state-after-$phase.txt"
  done
  if [ "$missing_guest_rss" -ne 0 ]; then echo "BLOCKED: workload wrapper must write guest RSS bytes to AWMAN_GUEST_RSS_OUTPUT for both runs" >&2; exit 4; fi
else
  echo 'BLOCKED: realistic git/build-tools guest workload wrapper not supplied' > "$output/workload.status"
  echo "BLOCKED: provide the realistic guest workload wrapper" >&2
  exit 4
fi
echo "PASS measurements captured without thresholds: $output"
