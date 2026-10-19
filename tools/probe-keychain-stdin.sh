#!/bin/bash
# probe-keychain-stdin.sh — macOS only. Investigation tool for work item
# aspec/work-items/completed/0116-squad-daemon-environment.md, §5.
#
# Question: can `security add-generic-password` take the secret on stdin
# instead of as a `-w <value>` argument? Argv is visible to the same user via
# `ps`, so if the answer is no, the macOS `KeychainStore` backend is dropped
# rather than shipping a design that leaks the secret it exists to protect.
#
# Four tests:
#   1. `-w` with no value, secret piped   — does it read stdin or /dev/tty?
#   2. `security -i`                       — plan B: the whole command line on
#                                            stdin, so the value never enters
#                                            any process's argv.
#   3. both of the above under launchd     — no controlling terminal at all,
#                                            which is the daemon's real case.
#   4. the actual payload shape            — a base64-wrapped JSON map, in both
#                                            contexts, at a realistic size.
#
# A GUI authorization dialog during test 3 is a FAIL even on exit 0: an
# unattended daemon has nobody to answer it.
#
# MEASURED RESULT (2026-09-08, macOS) — the answer is `security -i`:
#   Test 1  FAIL. From a terminal, `security ... -w` ignores the pipe, opens
#           /dev/tty and prompts there.
#   Test 2  PASS. `security -i` wrote and read back the sentinel.
#   Test 3  Under launchd (no tty) the piped form behaves differently again:
#           it reads stdin, demands the value twice, hits EOF on the confirm,
#           reports "passwords don't match" — and still exits 0 having stored
#           nothing. `security -i` passed, with no prompt of any kind and no
#           GUI authorization dialog.
#   Test 4  PASS in both contexts. A 589-byte JSON map containing " \ $ ` ; |
#           & and spaces, base64-wrapped in the go-keyring-base64: envelope,
#           round-tripped byte-identical through `security -i` as an 806-byte
#           item — from a terminal and from the launchd job alike. The envelope
#           is what makes this safe: `security -i` parses stdin as command
#           lines, and base64's alphabet passes that parser untouched.
# Corollary: the item's trusted application is /usr/bin/security, not awman,
# so rebuilding awman does not invalidate ACL trust.
# Consequences are written up in work item 0116 §5. Re-run this probe if a
# macOS release changes `security`'s behaviour.
#
# Self-cleaning: removes the keychain item and the plist it creates.
#
# Usage: bash tools/probe-keychain-stdin.sh
set -u

SERVICE="awman-stdin-probe"
ACCOUNT="probe"
SECRET="sentinel-$$-$RANDOM"
LABEL="io.awman.stdinprobe"
LOG="/tmp/awman-probe.log"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "This probe only means anything on macOS (uname says $(uname -s))." >&2
  exit 2
fi

# BSD base64 decodes with -D; newer builds also accept -d. Settle it once.
B64DEC="-D"
printf 'QQ==' | base64 -D >/dev/null 2>&1 || B64DEC="-d"

wipe() { security delete-generic-password -s "$SERVICE" -a "$ACCOUNT" >/dev/null 2>&1; }
readback() { security find-generic-password -s "$SERVICE" -a "$ACCOUNT" -w 2>/dev/null; }

# Run with a 10s alarm so a prompt on /dev/tty surfaces as exit 142 (SIGALRM)
# rather than hanging the script.
capped() { perl -e 'alarm 10; exec @ARGV or exit 127' "$@"; }

verdict() { # $1 = write exit code
  local got
  got="$(readback)"
  if [ "$got" = "$SECRET" ]; then
    echo "  RESULT: PASS — wrote and read back the sentinel (write exit $1)"
  elif [ "$1" = "142" ]; then
    echo "  RESULT: FAIL — timed out; it wants /dev/tty, not stdin"
  else
    echo "  RESULT: FAIL — write exit $1, read back '${got:0:24}'"
  fi
}

write_piped() { # secret on stdin, -w with no value
  printf '%s' "$SECRET" | capped security add-generic-password -U -s "$SERVICE" -a "$ACCOUNT" -w
}

write_interactive() { # whole command line on stdin; argv is just `security -i`
  printf 'add-generic-password -U -s %s -a %s -w %s\n' "$SERVICE" "$ACCOUNT" "$SECRET" |
    capped security -i
}

# ── Test 4: the real payload shape ─────────────────────────────────────────
# What KeychainStore actually stores is a JSON map, so the value ALWAYS
# contains `"`, and may contain spaces and backslashes — and `security -i`
# parses its stdin as command lines. The design wraps the JSON in the existing
# `go-keyring-base64:` envelope so none of that reaches the parser. This test
# proves the wrap survives a real write/read round trip, at a realistic size.

build_payload() { # a JSON map with every character class that could break a parser
  local pad
  pad="$(printf 'A%.0s' $(seq 1 512))"
  # Quoted heredoc: nothing here is expanded by the shell.
  cat <<'PAYLOAD' | tr -d '\n' | sed "s/PADPAD/$pad/"
{"GITHUB_TOKEN":"ghp_a b\"c\\d$e`f;g|h&i","AWS_PROFILE":"has space","PAD":"PADPAD"}
PAYLOAD
}

test_envelope() {
  local payload wrapped rc got decoded
  payload="$(build_payload)"
  wrapped="go-keyring-base64:$(printf '%s' "$payload" | base64 | tr -d '\n')"
  wipe
  printf 'add-generic-password -U -s %s -a %s -w %s\n' "$SERVICE" "$ACCOUNT" "$wrapped" |
    capped security -i
  rc=$?
  got="$(readback)"
  decoded="$(printf '%s' "${got#go-keyring-base64:}" | base64 "$B64DEC" 2>/dev/null)"
  if [ "$decoded" = "$payload" ]; then
    echo "  RESULT: PASS — ${#payload}-byte payload round-tripped intact" \
         "(write exit $rc, ${#wrapped}-byte item)"
  else
    echo "  RESULT: FAIL — write exit $rc; stored ${#got} bytes," \
         "decoded ${#decoded}, expected ${#payload}"
    echo "          expected: ${payload:0:70}"
    echo "          decoded : ${decoded:0:70}"
  fi
}

# ── The launchd leg, re-entered as a child with no controlling terminal ─────
if [ "${1:-}" = "--daemon-leg" ]; then
  SECRET="$2"
  wipe
  write_piped
  echo "piped write:       exit $? / readback '$(readback)'"
  wipe
  write_interactive
  echo "interactive write: exit $? / readback '$(readback)'"
  echo "expected sentinel: '$SECRET'"
  echo "envelope round trip, no controlling terminal:"
  test_envelope
  wipe
  exit 0
fi

echo "Test 1: printf SECRET | security add-generic-password ... -w"
wipe
write_piped
verdict "$?"

echo "Test 2: printf 'add-generic-password ... -w SECRET' | security -i"
wipe
write_interactive
verdict "$?"

echo "Test 3: the same two writes under launchd (no controlling terminal),"
echo "        plus the test-4 envelope round trip in that same context"
SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
mkdir -p "$(dirname "$PLIST")"
cat >"$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>$LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>/bin/bash</string>
        <string>$SELF</string>
        <string>--daemon-leg</string>
        <string>$SECRET</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>StandardOutPath</key><string>$LOG</string>
    <key>StandardErrorPath</key><string>$LOG</string>
</dict>
</plist>
EOF
: >"$LOG"
launchctl bootout "gui/$(id -u)/$LABEL" >/dev/null 2>&1
if launchctl bootstrap "gui/$(id -u)" "$PLIST"; then
  sleep 3
  echo "  --- $LOG ---"
  sed 's/^/  /' "$LOG"
else
  echo "  RESULT: could not bootstrap the probe job; test 3 did not run"
fi
launchctl bootout "gui/$(id -u)/$LABEL" >/dev/null 2>&1
rm -f "$PLIST"

echo "Test 4: base64-wrapped JSON payload through security -i (this terminal)"
test_envelope

wipe
echo
echo "Reading the results:"
echo "  test 1 passes                  -> pipe the secret to \`security ... -w\`"
echo "  test 1 fails, test 2 passes    -> use \`security -i\` (the measured answer)"
echo "  both fail under launchd        -> drop the macOS backend (Linux's"
echo "                                    \`secret-tool store\` reads stdin by design)"
echo "  any GUI prompt during test 3   -> FAIL regardless of exit code"
echo "  test 4 must pass in BOTH       -> the terminal run and the launchd run"
echo "                                    inside the test 3 log; a failure there"
echo "                                    means the envelope needs rethinking,"
echo "                                    not more careful quoting"
