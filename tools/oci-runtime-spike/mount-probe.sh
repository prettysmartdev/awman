#!/usr/bin/env bash
set -euo pipefail

candidate=${1:?Usage: mount-probe.sh msb-or-smolvm BINARY IMAGE NEW_OUTPUT_DIRECTORY}
binary=${2:?absolute binary path required}
image=${3:?image archive required: OCI or Docker for msb, Docker for smolvm}
output=${4:?new output directory required}
scripts=$(cd "$(dirname "$0")" && pwd)
case "$candidate" in msb|smolvm) ;; *) exit 2 ;; esac
case "$(uname -s)" in
    Linux) if [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then printf 'BLOCKED: accessible /dev/kvm required\n'; exit 77; fi ;;
    Darwin) if [ "$(uname -m)" != arm64 ]; then printf 'BLOCKED: Apple Silicon required\n'; exit 77; fi ;;
    *) printf 'BLOCKED: unsupported platform\n'; exit 77 ;;
esac
mkdir "$output"
output=$(cd "$output" && pwd)
mkdir -p "$output"/{state,tmp,workspace/nested,readonly,nested,skills/named,contexts/global,contexts/repo,contexts/workflow,direct,claude,gemini/antigravity-cli,extra,outside}
printf workspace > "$output/workspace/input"
printf agents-md > "$output/workspace/AGENTS.md"
printf secret > "$output/outside/secret"
ln -s "$output/outside" "$output/workspace/escape"
printf nested > "$output/nested/value"
printf skill > "$output/skills/named/SKILL.md"
for scope in global repo workflow; do printf '%s' "$scope" > "$output/contexts/$scope/AGENTS.md"; done
printf direct > "$output/direct/settings.json"
printf claude-config > "$output/claude.json"
printf sanitized > "$output/claude/settings.json"
printf access-token-v1 > "$output/claude/.credentials.json"
printf fake-token > "$output/gemini/antigravity-cli/token"
printf extra > "$output/extra/AGENTS.md"
printf prompt > "$output/prompt.md"
printf file-ro > "$output/single-ro"
printf file-rw > "$output/single-rw"
volumes=(-v "$output/workspace:/workspace:rw" -v "$output/readonly:/readonly:ro" -v "$output/nested:/workspace/nested:ro" -v "$output/skills:/skills:ro" -v "$output/contexts/global:/contexts/global:ro" -v "$output/contexts/repo:/contexts/repo:ro" -v "$output/contexts/workflow:/contexts/workflow:rw" -v "$output/direct:/agent-home/direct:rw" -v "$output/claude:/agent-home/.claude:rw" -v "$output/gemini:/agent-home/.gemini:rw" -v "$output/extra:/extra:ro" -v "$scripts:/spike:ro")
files=0
if [ "$candidate" = msb ]; then
    files=1
    volumes+=(-v "$output/single-ro:/single-ro:ro" -v "$output/single-rw:/single-rw:rw" -v "$output/claude.json:/agent-home/.claude.json:rw" -v "$output/prompt.md:/prompt.md:ro")
fi
environment=(-e 'SPIKE_LITERAL=literal with spaces' -e SPIKE_SECRET=fake-secret -e SPIKE_PROMPT_FILE=/prompt.md -e "SPIKE_HOST_SECRET=$output/outside/secret" -e "SPIKE_FILES=$files")
refresh_pid=
cleanup() {
    if [ -n "$refresh_pid" ]; then kill "$refresh_pid" 2>/dev/null || true; wait "$refresh_pid" 2>/dev/null || true; fi
    if [ "$candidate" = msb ]; then
        env -i PATH=/usr/bin:/bin MSB_HOME="$output/state" MSB_BACKEND=local "$binary" remove --force awman-mount-probe > "$output/cleanup.log" 2>&1 || true
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
(
    attempts=0
    until [ -f "$output/workspace/refresh-ready" ]; do
        attempts=$((attempts + 1))
        if [ "$attempts" -ge 1200 ]; then exit 1; fi
        sleep 0.1
    done
    printf access-token-v2 > "$output/claude/.credentials.next"
    mv "$output/claude/.credentials.next" "$output/claude/.credentials.json"
    printf file-ro-v2 > "$output/single-ro.next"
    mv "$output/single-ro.next" "$output/single-ro"
) &
refresh_pid=$!
status=0
if [ "$candidate" = msb ]; then
    env -i PATH=/usr/bin:/bin TMPDIR="$output/tmp" MSB_HOME="$output/state" MSB_BACKEND=local "$binary" image load -i "$image" -t awman-spike/mount:latest > "$output/import.log" 2>&1
    env -i PATH=/usr/bin:/bin TMPDIR="$output/tmp" MSB_HOME="$output/state" MSB_BACKEND=local "$binary" run -n awman-mount-probe --cpus 1 --memory 512M --no-tty --timeout 90s --max-duration 120s --no-net -u 0 -w /workspace --entrypoint /bin/sh "${volumes[@]}" "${environment[@]}" awman-spike/mount:latest -- /spike/guest-checks.sh > "$output/guest.stdout" 2> "$output/guest.stderr" || status=$?
else
    env -i PATH=/usr/bin:/bin TMPDIR="$output/tmp" SMOLVM_DATA_DIR="$output/state" "$binary" machine run --cpus 1 --mem 512 --storage 1 --overlay 1 --timeout 90s -u 0 -w /workspace -I "$image" "${volumes[@]}" "${environment[@]}" -- /bin/sh /spike/guest-checks.sh > "$output/guest.stdout" 2> "$output/guest.stderr" || status=$?
fi
printf 'guest_exit\t%s\n' "$status" | tee "$output/result.tsv"
cat "$output/guest.stdout"
if [ "$status" -eq 0 ]; then
    if ! grep -q '^guest-suite-complete$' "$output/guest.stdout" ||
       ! grep -q '^guest-stderr-marker$' "$output/guest.stderr" ||
       grep -q '^FAIL' "$output/guest.stdout" ||
       [ "$(cat "$output/workspace/from-guest")" != guest ]; then
        printf 'FAIL\tmissing completion, output markers, or host writeback\n' | tee -a "$output/result.tsv"
        exit 1
    fi
fi
exit "$status"
