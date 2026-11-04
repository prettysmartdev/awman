#!/usr/bin/env bash
# Exercise tools/release-runner.sh against fakes for gh, uname, sysctl,
# hostname and the actions/runner archive: host checks, checksum refusal,
# registration, the online wait, busy-runner detection and teardown.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
script="$repo/tools/release-runner.sh"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/awman-release-runner.XXXXXX")
trap 'bash "$script" down >/dev/null 2>&1 || true; rm -rf -- "$scratch"' EXIT
failures=0

bin="$scratch/bin"
log="$scratch/calls.log"
opt="$scratch/opt/awman"   # does not exist yet: provision creates it with sudo
mkdir -p "$bin" "$scratch/fixtures"
export PATH="$bin:$PATH"
export AWMAN_RELEASE_RUNNER_HOME="$scratch/home"
export AWMAN_RELEASE_OPT_DIR="$opt"
export AWMAN_RELEASE_RUNNER_WORK="$scratch/work"
export AWMAN_RELEASE_RUNNER_ONLINE_TIMEOUT=10
# Prebuilt guest archives, so provisioning needs no container engine.
echo base > "$scratch/fixtures/base.tar"
echo fixture-v1 > "$scratch/fixtures/fixture-oci.tar"
export AWMAN_TEST_BUILTIN_BASE_TAR="$scratch/fixtures/base.tar"
export AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE="$scratch/fixtures/fixture-oci.tar"
export FAKE_LOG="$log" FAKE_ONLINE="" FAKE_BAD_SHA="" FAKE_NO_SHA="" FAKE_PLAIN_SHA="" FAKE_RUN_EXITS="" FAKE_INSTALL_FAILS=""

# The runner archive: config.sh records its arguments; run.sh announces it
# started and records the signal that stops it.
mkdir -p "$scratch/archive"
cat > "$scratch/archive/config.sh" <<'EOF'
#!/usr/bin/env bash
echo "config.sh $* token=${ACTIONS_RUNNER_INPUT_TOKEN:-}" >> "$FAKE_LOG"
EOF
cat > "$scratch/archive/run.sh" <<'EOF'
#!/usr/bin/env bash
[ -z "$FAKE_RUN_EXITS" ] || exit 3
trap 'echo "run.sh stopped by INT" >> "$FAKE_LOG"; exit 0' INT
touch "$AWMAN_RELEASE_RUNNER_HOME/.fake-online"
while :; do sleep 0.2; done
EOF
chmod +x "$scratch/archive/"*.sh
tar -czf "$scratch/runner.tar.gz" -C "$scratch/archive" .
if command -v shasum >/dev/null 2>&1; then
  FAKE_SHA=$(shasum -a 256 "$scratch/runner.tar.gz" | cut -d' ' -f1)
else
  FAKE_SHA=$(sha256sum "$scratch/runner.tar.gz" | cut -d' ' -f1)
fi
export FAKE_TARBALL="$scratch/runner.tar.gz" FAKE_SHA

cat > "$bin/gh" <<'EOF'
#!/usr/bin/env bash
echo "gh $*" >> "$FAKE_LOG"
args="$*"
case "$args" in
  "api repos/actions/runner/releases/latest -q .tag_name") echo v9.9.9 ;;
  "release download v9.9.9 --repo actions/runner"*)
    dir=$(echo "$args" | sed -n 's/.*--dir \([^ ]*\).*/\1/p')
    cp "$FAKE_TARBALL" "$dir/actions-runner-osx-arm64-9.9.9.tar.gz" ;;
  "api repos/actions/runner/releases/tags/v9.9.9 -q .body")
    sha=${FAKE_BAD_SHA:-$FAKE_SHA}
    printf 'Notes\n## SHA-256 Checksums\n'
    if [ -z "$FAKE_NO_SHA" ]; then
      if [ -n "$FAKE_PLAIN_SHA" ]; then
        printf '  * actions-runner-osx-arm64-9.9.9.tar.gz %s\n' "$sha"
      else
        printf '%s\n' "- actions-runner-osx-arm64-9.9.9.tar.gz <!-- BEGIN SHA osx-arm64 -->${sha}<!-- END SHA osx-arm64 -->"
      fi
    fi ;;
  *registration-token*) echo regtok ;;
  *remove-token*) echo rmtok ;;
  "api repos/o/r/actions/runners --paginate -q "*'index("hypervisor")'*)
    printf '%s' "$FAKE_ONLINE"
    [ -f "$AWMAN_RELEASE_RUNNER_HOME/.fake-online" ] && echo "awman-release-v1.2.3-mac" || true ;;
  "api repos/o/r/actions/runners --paginate -q "*.status*)
    [ -f "$AWMAN_RELEASE_RUNNER_HOME/.fake-online" ] && echo online || echo offline ;;
  "api repos/o/denied/actions/runners"*) echo "gh: Must have admin rights to Repository. (HTTP 403)" >&2; exit 1 ;;
  "api repos/o/broken/actions/runners"*) echo "gh: connection reset" >&2; exit 1 ;;
  *) echo "fake gh: unexpected: $args" >&2; exit 1 ;;
esac
EOF
printf '#!/bin/sh\ncase "$1" in -s) echo "${FAKE_OS:-Darwin}";; -m) echo "${FAKE_ARCH:-arm64}";; esac\n' > "$bin/uname"
printf '#!/bin/sh\necho "${FAKE_HV:-1}"\n' > "$bin/sysctl"
printf '#!/bin/sh\necho mac\n' > "$bin/hostname"
printf '#!/bin/sh\necho "sudo $*" >> "$FAKE_LOG"\nexec "$@"\n' > "$bin/sudo"
printf '#!/bin/sh\n[ -z "$FAKE_INSTALL_FAILS" ] || exit 1\nexec /usr/bin/install "$@"\n' > "$bin/install"
# `down` reads process groups with `ps -A -o pgid= -o command=`; hosts without
# ps (minimal Linux containers) get the same listing from /proc.
if ! command -v ps >/dev/null 2>&1; then
  cat > "$bin/ps" <<'EOF'
#!/usr/bin/env bash
for d in /proc/[0-9]*; do
  stat=$(cat "$d/stat" 2>/dev/null) || continue
  read -r _ _ pgid _ <<< "${stat##*) }"
  cmd=$(tr '\0' ' ' < "$d/cmdline" 2>/dev/null) || continue
  [ -z "$cmd" ] || echo "$pgid $cmd"
done
EOF
fi
chmod +x "$bin/"*

check() { # name condition...
  local name=$1
  shift
  if "$@"; then echo "ok: $name"; else echo "FAIL: $name" >&2; failures=$((failures + 1)); fi
}
logged() { grep -qF -- "$1" "$log"; }
reset() { : > "$log"; rm -f "$AWMAN_RELEASE_RUNNER_HOME/.fake-online"; }

# ── check ──
check "check fails without the /opt/awman release scripts" \
  bash -c '! bash "$0" check 2>/dev/null' "$script"
check "check --hardware passes before provisioning" bash "$script" check --hardware
check "check --hardware fails without Hypervisor.framework" \
  bash -c '! FAKE_HV=0 bash "$0" check --hardware 2>/dev/null' "$script"
check "check fails on Linux" \
  bash -c '! FAKE_OS=Linux bash "$0" check 2>/dev/null' "$script"
check "provision refuses a host without Hypervisor.framework" \
  bash -c '! FAKE_HV=0 bash "$0" provision 2>/dev/null' "$script"

# ── provision ──
reset
check "provision installs the release scripts" bash -c 'bash "$0" provision >/dev/null' "$script"
check "the missing /opt/awman is created with sudo and handed to the user" \
  bash -c 'grep -q "^sudo install -d -m 755 $0$" "$1" && [ -O "$0" ]' "$opt" "$log"
check "check passes once provisioned" bash "$script" check
check "the fixture is the cached archive" cmp -s "$opt/fixture-oci.tar" "$AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE"
check "the session script is the repo's reference script" \
  cmp -s "$opt/lib/release-offline-session.sh" "$repo/tools/release-offline-session.sh"
# Swap in a stub to see exactly what each wrapper runs under a cleared env.
printf '#!/bin/bash\necho "$*"\n' > "$opt/lib/release-offline-session.sh"
for wrapper in release-offline-session release-workload; do
  check "$wrapper.sh passes the baked-in fixture under env -i" \
    bash -c '[ "$(env -i /bin/bash "$0/$1.sh" ART STATE)" = "--fixture $0/fixture-oci.tar ART STATE" ]' "$opt" "$wrapper"
done
reset
echo fixture-v2 > "$AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE"
echo "#!/bin/sh" > "$opt/release-workload.sh"
check "re-provisioning refreshes changed files" bash -c 'bash "$0" provision >/dev/null' "$script"
check "without sudo once /opt/awman is the user's" bash -c '! grep -q "^sudo" "$0"' "$log"
check "a changed fixture is reinstalled" grep -qx fixture-v2 "$opt/fixture-oci.tar"
check "the stubbed session script is restored" \
  cmp -s "$opt/lib/release-offline-session.sh" "$repo/tools/release-offline-session.sh"
check "a wrapper this script did not write is kept as .bak" grep -qx "#!/bin/sh" "$opt/release-workload.sh.bak"
check "and replaced by the provisioned one" grep -qF "Provisioned by" "$opt/release-workload.sh"
check "an unchanged file is left alone" \
  bash -c '! grep -q "Installed $0/release-offline-session.sh$" <(bash "$1" provision)' "$opt" "$script"
mkdir -p "$scratch/tmp"
echo fixture-v3 > "$AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE"
check "provision fails when a file cannot be installed" \
  bash -c '! TMPDIR="$1" FAKE_INSTALL_FAILS=1 bash "$0" provision >/dev/null 2>&1' "$script" "$scratch/tmp"
check "and removes its staging directory" bash -c '[ -z "$(ls -A "$0")" ]' "$scratch/tmp"

# ── up refuses an archive whose checksum does not match ──
reset
check "up refuses release notes without an osx-arm64 checksum" \
  bash -c '! FAKE_NO_SHA=1 bash "$0" up o/r v1.2.3 2>/dev/null' "$script"
check "a missing checksum never registers the runner" bash -c '! grep -q registration-token "$0"' "$log"
reset
check "up refuses a mismatched runner checksum" \
  bash -c '! FAKE_BAD_SHA=0000000000000000000000000000000000000000000000000000000000000000 bash "$0" up o/r v1.2.3 2>/dev/null' "$script"
check "a mismatched archive is never registered" bash -c '! grep -q registration-token "$0"' "$log"
check "a mismatched archive is not kept in the cache" \
  bash -c '[ -z "$(ls -A "$0/cache")" ]' "$AWMAN_RELEASE_RUNNER_HOME"

# ── up ──
reset
check "up registers and starts the runner" bash -c 'bash "$0" up o/r v1.2.3 >/dev/null' "$script"
check "registers with the hypervisor label, a release name and the repo URL" \
  logged "config.sh --unattended --url https://github.com/o/r --name awman-release-v1.2.3-mac --labels hypervisor --work $AWMAN_RELEASE_RUNNER_WORK --replace --disableupdate token=regtok"
check "the registration token is never a command-line argument" bash -c '! grep -q -- --token "$0"' "$log"
check "the work directory it creates is private" \
  bash -c '[ "$(ls -ld "$0" | cut -c1-10)" = drwx------ ]' "$AWMAN_RELEASE_RUNNER_WORK"
check "the runner is running" bash -c 'kill -0 "$(sed -n "s/^pid=//p" "$0/state")"' "$AWMAN_RELEASE_RUNNER_HOME"

# ── busy ──
check "busy ignores our own runner" bash -c '[ -z "$(bash "$0" busy o/r)" ]' "$script"
check "busy reports another online hypervisor runner" \
  bash -c '[ "$(FAKE_ONLINE=$'"'"'studio\n'"'"' bash "$0" busy o/r)" = studio ]' "$script"
check "busy explains that listing runners needs admin rights" \
  bash -c 'if bash "$0" busy o/denied 2>"$1"; then exit 1; fi; grep -q "needs repository admin rights" "$1" && ! grep -q "HTTP 403" "$1"' "$script" "$scratch/busy.err"
check "busy shows any other gh failure as it is" \
  bash -c 'if bash "$0" busy o/broken 2>"$1"; then exit 1; fi; grep -q "connection reset" "$1"' "$script" "$scratch/busy.err"

# ── down ──
pid=$(sed -n 's/^pid=//p' "$AWMAN_RELEASE_RUNNER_HOME/state")
check "down stops and deregisters the runner" bash -c 'bash "$0" down >/dev/null' "$script"
check "the runner got a graceful SIGINT" logged "run.sh stopped by INT"
check "the runner process is gone" bash -c '! kill -0 "$0" 2>/dev/null' "$pid"
check "deregistered with a removal token" logged "config.sh remove token=rmtok"
check "state, runner and work directories are removed" \
  bash -c '[ ! -e "$0/state" ] && [ ! -e "$0/run" ] && [ ! -e "$1" ]' "$AWMAN_RELEASE_RUNNER_HOME" "$AWMAN_RELEASE_RUNNER_WORK"
check "the verified archive stays cached" \
  bash -c '[ -f "$0/cache/actions-runner-osx-arm64-9.9.9.tar.gz" ]' "$AWMAN_RELEASE_RUNNER_HOME"
check "down is idempotent" bash "$script" down
reset
check "up accepts a plain SHA-256 checksum line" \
  bash -c 'FAKE_PLAIN_SHA=1 bash "$0" up o/r v1.2.3 >/dev/null' "$script"
check "plain-checksum runner deregisters" bash -c 'bash "$0" down >/dev/null' "$script"
mkdir -p "$AWMAN_RELEASE_RUNNER_WORK"
check "a work directory it did not create is never deleted" \
  bash -c 'bash "$0" down >/dev/null; [ -d "$1" ]' "$script" "$AWMAN_RELEASE_RUNNER_WORK"
touch "$AWMAN_RELEASE_RUNNER_WORK/keep"
reset
check "up reuses a work directory that was already there" bash -c 'bash "$0" up o/r v1.2.3 >/dev/null' "$script"
check "down leaves that work directory and its files alone" \
  bash -c 'bash "$0" down >/dev/null; [ -f "$1/keep" ]' "$script" "$AWMAN_RELEASE_RUNNER_WORK"
rm -rf "$AWMAN_RELEASE_RUNNER_WORK"
ln -s "$scratch" "$AWMAN_RELEASE_RUNNER_WORK"
check "up refuses a work directory path that is not a plain directory" \
  bash -c '! bash "$0" up o/r v1.2.3 >/dev/null 2>&1' "$script"
rm -f "$AWMAN_RELEASE_RUNNER_WORK"

# ── a saved pid that is no longer the runner (reused after a reboot) ──
reset
bash "$script" up o/r v1.2.3 >/dev/null
runner=$(sed -n 's/^pid=//p' "$AWMAN_RELEASE_RUNNER_HOME/state")
set -m
sleep 300 &
other=$!
set +m
awk -v p="$other" '/^pid=/ { $0 = "pid=" p } { print }' "$AWMAN_RELEASE_RUNNER_HOME/state" > "$scratch/state"
mv "$scratch/state" "$AWMAN_RELEASE_RUNNER_HOME/state"
check "down refuses to signal a process group that is not the runner" \
  bash -c 'bash "$0" down 2>"$1" >/dev/null; grep -q "no longer the runner" "$1"' "$script" "$scratch/down.err"
check "that unrelated process is left running" kill -0 "$other"
check "the runner is still deregistered" logged "config.sh remove token=rmtok"
{ kill -KILL "$other"; wait "$other"; } 2>/dev/null || true
kill -KILL -- "-$runner" 2>/dev/null || true

# ── a runner that dies before coming online ──
reset
check "up fails when the runner exits before coming online" \
  bash -c '! FAKE_RUN_EXITS=1 bash "$0" up o/r v1.2.3 >/dev/null 2>&1' "$script"
check "the dead runner is deregistered" logged "config.sh remove token=rmtok"
check "no state is left behind" bash -c '[ ! -e "$0/state" ]' "$AWMAN_RELEASE_RUNNER_HOME"

if [ "$failures" -gt 0 ]; then
  echo "$failures release-runner check(s) failed" >&2
  exit 1
fi
