#!/usr/bin/env bash
# Inspect and trace an exact optimized artifact. No thresholds are asserted;
# callers retain the raw output tree as release evidence.
set -euo pipefail
artifact=${1:?usage: release-artifact-check.sh ARTIFACT TARGET OUTPUT [OFFLINE_SESSION_WRAPPER [ARGS...]]}
target=${2:?target triple required}
output=${3:?raw output directory required}
shift 3
root=$(cd "$(dirname "$0")/.." && pwd -P)
artifact=$(cd "$(dirname "$artifact")" && pwd -P)/$(basename "$artifact")
test -x "$artifact" || { echo "artifact is not executable: $artifact" >&2; exit 2; }
mkdir -p "$output"
output=$(cd "$output" && pwd -P)
cp "$artifact" "$output/awman"
artifact="$output/awman"
hash_file() { if command -v sha256sum >/dev/null; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi; }
{
  printf 'target=%s\nartifact_sha256=%s\nartifact_size_bytes=%s\n' "$target" "$(hash_file "$artifact")" "$(wc -c < "$artifact" | tr -d ' ')"
  uname -a
  rustc --version || true
} > "$output/identity.txt"
case "$(uname -s)" in
  Linux)
    ldd "$artifact" > "$output/dynamic-dependencies.txt"
    command -v readelf >/dev/null && readelf -SW "$artifact" > "$output/sections.txt"
    readelf --version-info "$artifact" > "$output/versioned-symbols.txt"
    {
      printf 'runner='; . /etc/os-release; printf '%s %s\n' "${PRETTY_NAME:-unknown}" "$(uname -m)"
      getconf GNU_LIBC_VERSION 2>/dev/null || true
      printf 'maximum_required_GLIBC_symbol='; grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' "$output/versioned-symbols.txt" | sort -Vu | tail -1 || true
      printf 'baseline_policy=system libraries only; inspect dynamic-dependencies.txt and versioned-symbols.txt; no minimum ABI threshold is declared by this measurement harness\n'
    } > "$output/linux-abi-baseline.txt"
    command -v strace >/dev/null || { echo "BLOCKED: strace unavailable; executable/firmware trace required" >&2; exit 4; }
    ;;
  Darwin)
    otool -L "$artifact" > "$output/dynamic-dependencies.txt"
    otool -l "$artifact" > "$output/sections.txt"
    command -v fs_usage >/dev/null || command -v dtruss >/dev/null || { echo "BLOCKED: fs_usage or dtruss unavailable; native Mac trace required" >&2; exit 4; }
    ;;
  *) echo "unsupported artifact inspection host: $(uname -s)" >&2; exit 2 ;;
esac
if grep -Ei 'libsqlite3|libcap-ng|libkrun(fw)?' "$output/dynamic-dependencies.txt"; then
  echo "forbidden dynamic runtime dependency found" >&2; exit 1
fi
strings "$artifact" > "$output/strings.txt"
version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$root/Cargo.toml" | head -1)
grep -Fq "$version" "$output/strings.txt" || { echo "embedded awman version $version not retained" >&2; exit 1; }
grep -q 'msb' "$output/strings.txt" || { echo "embedded provider marker not retained" >&2; exit 1; }
if [ "$(uname -s)" = Linux ]; then grep -q '\.msbver' "$output/sections.txt" || { echo "Linux .msbver section missing" >&2; exit 1; }
else grep -q '__msbver' "$output/sections.txt" || { echo "Mac __msbver section missing" >&2; exit 1; }
fi
mkdir -p "$output/home" "$output/tmp" "$output/state"
env -i PATH=/usr/bin:/bin HOME="$output/home" TMPDIR="$output/tmp" "$artifact" --version > "$output/version.stdout" 2> "$output/version.stderr"
env -i PATH= HOME="$output/home" TMPDIR="$output/tmp" "$artifact" __awman-builtin-info > "$output/provider-info.json" 2> "$output/provider-info.stderr"
for expected in '"msb_version"[[:space:]]*:[[:space:]]*"0\.7\.2"' \
  '"worker_protocol"[[:space:]]*:[[:space:]]*"0\.7\.2/18/awman-1"' \
  '"embedded_kernel"[[:space:]]*:[[:space:]]*true' \
  '"embedded_guest_agent"[[:space:]]*:[[:space:]]*true' \
  '"host_helpers"[[:space:]]*:[[:space:]]*false' \
  '"libkrun_commit"[[:space:]]*:[[:space:]]*"2bd0f84ad0956f3032e0490d3b8512b6851eca12"'; do
  grep -Eq "$expected" "$output/provider-info.json" || { echo "provider metadata mismatch: $expected" >&2; exit 1; }
done
manifest="$root/third_party/msb-payloads/manifest.toml"
kernel_hash=$(awk -v section="[targets.\"$target\"]" '$0 == section {inside=1;next} /^\[/ {inside=0} inside && $1 == "kernel_sha256" {gsub(/"/, "", $3); print $3; exit}' "$manifest")
grep -Eq '"kernel_sha256"[[:space:]]*:[[:space:]]*"'"$kernel_hash"'"' "$output/provider-info.json" || { echo "provider kernel hash differs from verified manifest" >&2; exit 1; }
find "$output/home" "$output/state" -type f \( -name msb -o -name agentd -o -name 'libkrunfw*' \) -print > "$output/helper-scan.txt"
test ! -s "$output/helper-scan.txt" || { cat "$output/helper-scan.txt" >&2; exit 1; }
if [ "$#" -eq 0 ]; then
  echo "BLOCKED: pass an offline, cached-session isolation wrapper and actual awman session argv" >&2; exit 4
fi
case "$(uname -s)" in
 Linux)
  command -v unshare >/dev/null || { echo "BLOCKED: unshare network namespace is required for offline cached execution" >&2; exit 4; }
  unshare --net -- strace -ff -yy -e trace=execve,openat,open,mmap -o "$output/session.trace" \
    env -i PATH= HOME="$output/home" TMPDIR="$output/tmp" "$@" "$artifact" "$output/state" \
    > "$output/session.stdout" 2> "$output/session.stderr"
  grep -Fq "$artifact" "$output"/session.trace* || { echo "BLOCKED: trace did not observe the exact distributed artifact" >&2; exit 4; }
  ;;
 Darwin)
  command -v sandbox-exec >/dev/null || { echo "BLOCKED: sandbox-exec network denial profile is required for offline cached execution" >&2; exit 4; }
  if command -v fs_usage >/dev/null; then
    fs_usage -w -f filesys > "$output/session.trace" 2>&1 & trace_pid=$!
    if sandbox-exec -p '(version 1)(deny network*)(allow default)' env -i PATH= HOME="$output/home" TMPDIR="$output/tmp" "$@" "$artifact" "$output/state" > "$output/session.stdout" 2> "$output/session.stderr"; then status=0; else status=$?; fi
    kill "$trace_pid" 2>/dev/null || true; wait "$trace_pid" 2>/dev/null || true; exit "$status"
  else
    sudo dtruss -f sandbox-exec -p '(version 1)(deny network*)(allow default)' env -i PATH= HOME="$output/home" TMPDIR="$output/tmp" "$@" "$artifact" "$output/state" > "$output/session.stdout" 2> "$output/session.trace"
  fi
  grep -Fq "$artifact" "$output/session.trace" || { echo "BLOCKED: Mac trace did not observe the exact distributed artifact" >&2; exit 4; }
  ;;
esac
find "$output/home" "$output/state" -type f \( -name msb -o -name agentd -o -name 'libkrunfw*' \) -print > "$output/helper-scan-after-session.txt"
test ! -s "$output/helper-scan-after-session.txt" || { cat "$output/helper-scan-after-session.txt" >&2; exit 1; }
printf 'PASS artifact inspection and session trace; raw evidence: %s\n' "$output"
