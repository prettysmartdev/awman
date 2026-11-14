#!/bin/sh
# Guest-side acceptance for the mounted runtime environment. File mounts are
# required: unsupported capabilities are failures, never omitted scenarios.
set -eu
check() { label=$1; shift; if "$@"; then printf 'PASS\t%s\n' "$label"; else printf 'FAIL\t%s\n' "$label"; exit 1; fi; }
read_is() { test "$(cat "$1")" = "$2"; }
is_readonly() { ! touch "$1/forbidden" 2>/dev/null; }
check linux test "$(uname -s)" = Linux
check directory-read read_is /workspace/input workspace
check directory-write sh -c 'printf guest > /workspace/from-guest'
check directory-readonly is_readonly /readonly
check file-readonly sh -c 'test "$(cat /single-ro)" = file-ro && ! (printf bad > /single-ro) 2>/dev/null'
check file-readwrite sh -c 'printf changed > /single-rw'
check settings-claude-file read_is /agent-home/.claude.json claude-config
check environment-prompt read_is "$AWMAN_GUEST_PROMPT_FILE" prompt
check file-prompt read_is /prompt.md prompt
check nested-mount read_is /workspace/nested/value nested
check nested-readonly is_readonly /workspace/nested
check skill-named read_is /skills/named/SKILL.md skill
check context-global read_is /contexts/global/AGENTS.md global
check context-repo read_is /contexts/repo/AGENTS.md repo
check context-workflow read_is /contexts/workflow/AGENTS.md workflow
check context-readwrite sh -c 'printf context > /contexts/workflow/output'
check settings-direct sh -c 'test "$(cat /agent-home/direct/settings.json)" = direct && printf updated > /agent-home/direct/written'
check settings-claude-directory read_is /agent-home/.claude/settings.json sanitized
check settings-antigravity read_is /agent-home/.gemini/antigravity-cli/token fake-token
check credential-refresh-token-absent sh -c '! grep -q refresh_token /agent-home/.claude/.credentials.json'
check environment sh -c 'test "$AWMAN_GUEST_LITERAL" = "literal with spaces" && test "$AWMAN_GUEST_SECRET" = fake-secret'
check agents-md read_is /workspace/AGENTS.md agents-md
check add-directory read_is /extra/AGENTS.md extra
check working-directory test "$PWD" = /workspace
check symlink-escape sh -c '! cat /workspace/escape/secret 2>/dev/null'
check outside-host-secret test ! -e "$AWMAN_GUEST_HOST_SECRET"
check rename sh -c 'printf rename > /workspace/rename-old && mv /workspace/rename-old /workspace/rename-new && test "$(cat /workspace/rename-new)" = rename'
check hardlink sh -c 'ln /workspace/input /workspace/hardlink && test "$(cat /workspace/hardlink)" = workspace'
check executable-bit sh -c 'printf "#!/bin/sh\nexit 0\n" > /workspace/executable && chmod 755 /workspace/executable && /workspace/executable'
check spaces sh -c 'printf spaces > "/workspace/path with spaces" && test "$(cat "/workspace/path with spaces")" = spaces'
check readonly-remount sh -c 'if mount -o remount,rw /readonly 2>/dev/null; then ! touch /readonly/forbidden 2>/dev/null; fi'
printf ready > /workspace/refresh-ready
attempt=0
while ! read_is /agent-home/.claude/.credentials.json access-token-v2 || ! read_is /single-ro file-ro-v2; do
    attempt=$((attempt + 1))
    if test "$attempt" -ge 150; then printf 'FAIL\tlive-atomic-host-refresh\n'; exit 1; fi
    sleep 0.1
done
check live-atomic-host-refresh read_is /single-ro file-ro-v2
printf 'guest-stdout-marker\nguest-suite-complete\n'
printf 'guest-stderr-marker\n' >&2
