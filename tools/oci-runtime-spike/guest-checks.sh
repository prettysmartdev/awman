#!/bin/sh
set -u

failures=0
check() {
    check_name=$1
    shift
    if "$@"; then
        printf 'PASS\t%s\n' "$check_name"
    else
        printf 'FAIL\t%s\n' "$check_name"
        failures=$((failures + 1))
    fi
}

check linux sh -c '[ "$(uname -s)" = Linux ]'
check directory-read sh -c '[ "$(cat /workspace/input)" = workspace ]'
check directory-write sh -c 'printf guest > /workspace/from-guest'
check directory-readonly sh -c '! touch /readonly/forbidden 2>/dev/null'
if [ "${SPIKE_FILES:-1}" = 1 ]; then
    check file-readonly sh -c '[ "$(cat /single-ro)" = file-ro ] && ! (printf bad > /single-ro) 2>/dev/null'
    check file-readwrite sh -c 'printf changed > /single-rw'
    check settings-claude-file sh -c '[ "$(cat /agent-home/.claude.json)" = claude-config ]'
    check environment-prompt sh -c '[ "$(cat "$SPIKE_PROMPT_FILE")" = prompt ]'
    check file-prompt sh -c '[ "$(cat /prompt.md)" = prompt ]'
else
    printf 'UNSUPPORTED\tfile mounts, Claude config file, file/env prompts\n'
fi
check nested-mount sh -c '[ "$(cat /workspace/nested/value)" = nested ]'
check nested-readonly sh -c '! touch /workspace/nested/forbidden 2>/dev/null'
check skill-named sh -c '[ "$(cat /skills/named/SKILL.md)" = skill ]'
check context-global sh -c '[ "$(cat /contexts/global/AGENTS.md)" = global ]'
check context-repo sh -c '[ "$(cat /contexts/repo/AGENTS.md)" = repo ]'
check context-workflow sh -c '[ "$(cat /contexts/workflow/AGENTS.md)" = workflow ]'
check context-readwrite sh -c 'printf context > /contexts/workflow/output'
check settings-direct sh -c '[ "$(cat /agent-home/direct/settings.json)" = direct ] && printf updated > /agent-home/direct/written'
check settings-claude-directory sh -c '[ "$(cat /agent-home/.claude/settings.json)" = sanitized ]'
check settings-antigravity sh -c '[ "$(cat /agent-home/.gemini/antigravity-cli/token)" = fake-token ]'
check credential-refresh-token-absent sh -c '! grep -q refresh_token /agent-home/.claude/.credentials.json'
check environment sh -c '[ "$SPIKE_LITERAL" = "literal with spaces" ] && [ "$SPIKE_SECRET" = fake-secret ]'
check agents-md sh -c '[ "$(cat /workspace/AGENTS.md)" = agents-md ]'
check add-directory sh -c '[ "$(cat /extra/AGENTS.md)" = extra ]'
check working-directory sh -c '[ "$PWD" = /workspace ]'
check symlink-escape sh -c '! cat /workspace/escape/secret 2>/dev/null'
check outside-host-secret sh -c '[ ! -e "$SPIKE_HOST_SECRET" ]'
check rename sh -c 'printf rename > /workspace/rename-old && mv /workspace/rename-old /workspace/rename-new && [ "$(cat /workspace/rename-new)" = rename ]'
check hardlink sh -c 'ln /workspace/input /workspace/hardlink && [ "$(cat /workspace/hardlink)" = workspace ]'
check executable-bit sh -c 'printf "#!/bin/sh\nexit 0\n" > /workspace/executable && chmod 755 /workspace/executable && /workspace/executable'
check spaces sh -c 'printf spaces > "/workspace/path with spaces" && [ "$(cat "/workspace/path with spaces")" = spaces ]'
check readonly-remount sh -c 'if mount -o remount,rw /readonly 2>/dev/null; then ! touch /readonly/forbidden 2>/dev/null; else true; fi'

printf ready > /workspace/refresh-ready
refresh_seen=0
refresh_attempts=0
while [ "$refresh_attempts" -lt 150 ]; do
    if [ "$(cat /agent-home/.claude/.credentials.json 2>/dev/null)" = access-token-v2 ] &&
       { [ "${SPIKE_FILES:-1}" = 0 ] || [ "$(cat /single-ro 2>/dev/null)" = file-ro-v2 ]; }; then
        refresh_seen=1
        break
    fi
    refresh_attempts=$((refresh_attempts + 1))
    sleep 0.1
done
check live-atomic-host-refresh test "$refresh_seen" = 1
printf 'guest-stderr-marker\n' >&2
printf 'guest-stdout-marker\n'
printf 'guest-suite-complete\n'
exit "$failures"
