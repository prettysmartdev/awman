#!/usr/bin/env bash
# Reference offline, cached-session wrapper for tools/release-artifact-check.sh
# and tools/measure-release-artifact.sh (runners may provision their own at
# /opt/awman/release-offline-session.sh).
#
# Usage: release-offline-session.sh [--fixture FIXTURE_OCI_TAR] ARTIFACT STATE_DIR
#   --fixture / AWMAN_RELEASE_FIXTURE   the promoted fixture OCI archive
#       (the argument form survives callers that clear the environment)
#   AWMAN_GUEST_RSS_OUTPUT   optional: file to receive the peak RSS in bytes
#                            of the VM worker processes during the session
#
# It creates a private project and HOME, imports the fixture from a local
# archive (no network), and runs one agent session with network mode "none"
# and a synthetic agent mounted by overlay (no paid agent, no credential).
# Host container helpers (docker, container, sbx) are stubs that fail the run
# if awman ever executes them.
set -euo pipefail
fixture=${AWMAN_RELEASE_FIXTURE:-}
if [ "${1:-}" = --fixture ]; then fixture=${2:?--fixture needs a path}; shift 2; fi
artifact=${1:?usage: release-offline-session.sh [--fixture PATH] ARTIFACT STATE_DIR}
state=${2:?state directory required}
[ -f "$fixture" ] || { echo "the fixture OCI archive is required (--fixture or AWMAN_RELEASE_FIXTURE)" >&2; exit 2; }
work=$(mktemp -d "${TMPDIR:-/tmp}/awman-offline.XXXXXX")
repo="$work/repo"; home="$work/home"; bin="$work/bin"
mkdir -p "$repo/.awman" "$home/.awman" "$bin" "$state"
chmod 700 "$state"   # awman requires a private state directory
git_bin=$(command -v git || true)
[ -n "$git_bin" ] || git_bin=/usr/bin/git
"$git_bin" -C "$repo" init -q
printf 'FROM scratch\n' > "$repo/Dockerfile.dev"
printf 'FROM awman-repo:latest\n' > "$repo/.awman/Dockerfile.claude"
cat > "$work/claude" <<'AGENT'
#!/bin/sh
IFS= read -r prompt || prompt="$*"
case "$prompt" in *sleep=2*) sleep 2;; esac
printf 'OFFLINE_SESSION_UID=%s\n' "$(id -u)"
printf 'OFFLINE_SESSION_PROMPT=%s\n' "$prompt"
exit 0
AGENT
chmod 755 "$work/claude"
cat > "$repo/.awman/config.json" <<JSON
{"agent":"claude","auth":"none","autoAgentAuthAccepted":true,
 "overlays":["dir($work/claude:/usr/local/bin/claude:ro)"]}
JSON
cat > "$home/.awman/config.json" <<JSON
{"runtime":"builtin-experimental","default_agent":"claude","auth":"none","autoAgentAuthAccepted":true,
 "builtin":{"stateDir":"$state","imageSource":{"type":"archive","path":"$fixture"},"network":{"mode":"none"}}}
JSON
marker="$work/helper-called"
for helper in docker container sbx; do
    printf '#!/bin/sh\necho %s >> %s\nexit 91\n' "$helper" "$marker" > "$bin/$helper"
    chmod 755 "$bin/$helper"
done
# `awman ready` pings the host agent once; this stand-in answers it.
printf '#!/bin/sh\necho synthetic-ready-ping\n' > "$bin/claude"
chmod 755 "$bin/claude"

run() {
    (cd "$repo" && env -i HOME="$home" TMPDIR="$work" PATH="$bin:/usr/bin:/bin" \
        AWMAN_CONFIG_HOME="$home/.awman" AWMAN_TEST_ISOLATION=0 "$artifact" "$@")
}
run ready --json > "$work/ready.json"
grep -q '"ready": true' "$work/ready.json" || { cat "$work/ready.json" >&2; exit 1; }

peak=0
if [ -n "${AWMAN_GUEST_RSS_OUTPUT:-}" ]; then
    (
        while :; do
            kb=$(ps -axo rss=,command= | awk -v exe="$artifact" 'index($0, exe" machine") {s += $1} END {print s + 0}')
            echo "$kb"
            sleep 0.2
        done
    ) > "$work/rss.samples" &
    sampler=$!
fi
run exec prompt --non-interactive --agent claude "offline session sleep=2" > "$work/session.out" 2>&1 || {
    cat "$work/session.out" >&2; exit 1; }
if [ -n "${AWMAN_GUEST_RSS_OUTPUT:-}" ]; then
    kill "$sampler" 2>/dev/null || true
    wait "$sampler" 2>/dev/null || true
    peak=$(sort -n "$work/rss.samples" | tail -1)
    echo $((peak * 1024)) > "$AWMAN_GUEST_RSS_OUTPUT"
fi
cat "$work/session.out"
grep -q 'OFFLINE_SESSION_PROMPT=offline session' "$work/session.out"
[ ! -e "$marker" ] || { echo "awman executed a host container helper: $(cat "$marker")" >&2; exit 1; }
echo "PASS offline cached session"
