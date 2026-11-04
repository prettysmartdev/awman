#!/usr/bin/env bash
# tools/release-runner.sh — a temporary GitHub Actions runner for one release.
#
# The Release workflow's `release-macos-builtin` job needs an Apple Silicon Mac
# with Hypervisor.framework (labels: self-hosted, macOS, ARM64, hypervisor).
# No hosted runner provides that, so `make release` registers the Mac it runs
# on as a runner for the length of the release and removes it afterwards.
#
# Usage:
#   tools/release-runner.sh check [--hardware]  # can this host serve the release job?
#   tools/release-runner.sh provision           # install the /opt/awman release scripts
#   tools/release-runner.sh busy <repo>         # print online runners that already can
#   tools/release-runner.sh verify-archive      # download and checksum-check the runner
#   tools/release-runner.sh up <repo> <tag>     # download, register and start a runner
#   tools/release-runner.sh down                # stop and deregister it (idempotent)
#
# State lives in $AWMAN_RELEASE_RUNNER_HOME (default ~/.awman/release-runner):
# a cached, checksum-verified runner archive, the cached guest fixture and the
# directory of the one runner currently registered. `provision` is the only
# command that writes outside it: the release job's wrappers in /opt/awman.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
RUNNER_HOME="${AWMAN_RELEASE_RUNNER_HOME:-$HOME/.awman/release-runner}"
OPT_DIR="${AWMAN_RELEASE_OPT_DIR:-/opt/awman}"
# The job's state directories hold Unix sockets, whose paths macOS caps at
# 103 bytes, so jobs run under a short work directory rather than RUNNER_HOME.
WORK_DIR="${AWMAN_RELEASE_RUNNER_WORK:-/tmp/awr}"
ONLINE_TIMEOUT="${AWMAN_RELEASE_RUNNER_ONLINE_TIMEOUT:-120}"
STATE="$RUNNER_HOME/state"
RUN_DIR="$RUNNER_HOME/run"
LABEL=hypervisor
MARKER="Provisioned by tools/release-runner.sh"

say() { echo "  $*"; }
die() { echo "release-runner: $*" >&2; exit 1; }

sha256() {
  if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  else sha256sum "$1" | cut -d' ' -f1; fi
}

# Every prerequisite the release job checks for itself, checked up front so a
# release never pushes its tag only to have the job report BLOCKED.
# --hardware checks only what `provision` cannot fix.
cmd_check() {
  local missing=()
  [ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || missing+=("an Apple Silicon Mac")
  [ "$(sysctl -n kern.hv_support 2>/dev/null || echo 0)" = 1 ] || missing+=("Hypervisor.framework (kern.hv_support=1)")
  if [ "${1:-}" != --hardware ]; then
    [ -x "$OPT_DIR/release-offline-session.sh" ] || missing+=("$OPT_DIR/release-offline-session.sh")
    [ -x "$OPT_DIR/release-workload.sh" ] || missing+=("$OPT_DIR/release-workload.sh")
    [ -s "$OPT_DIR/fixture-oci.tar" ] || missing+=("$OPT_DIR/fixture-oci.tar")
  fi
  if [ ${#missing[@]} -gt 0 ]; then
    printf 'missing: %s\n' "${missing[@]}" >&2
    return 1
  fi
}

# Install one file into OPT_DIR when its content changed. A file there that
# this script did not write is kept as <name>.bak rather than lost.
install_opt() { # src name mode
  local dest="$OPT_DIR/$2"
  cmp -s "$1" "$dest" 2>/dev/null && return 0
  if [ -f "$dest" ] && [ "$2" != fixture-oci.tar ] && ! grep -qF "$MARKER" "$dest"; then
    mv "$dest" "$dest.bak"
    say "Kept your existing $dest as $dest.bak"
  fi
  install -m "$3" "$1" "$dest"
  say "Installed $dest"
}

# The release job runs both wrappers with a cleared environment (the session
# one inside a sandbox that only lets listed binaries execute), so each is a
# fixed command line: the repo's reference session script, copied here, with
# the cached fixture's path baked in. The session script records guest RSS
# when AWMAN_GUEST_RSS_OUTPUT is set, which is all the workload wrapper needs.
cmd_provision() {
  local fixture name
  cmd_check --hardware || die "this host cannot run the Apple Silicon release job"
  fixture=$(
    # shellcheck source=tools/builtin-hw-fixtures.sh
    . "$ROOT/tools/builtin-hw-fixtures.sh"
    build_builtin_hw_fixture_archives "$RUNNER_HOME/fixtures" >&2
    echo "$AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE"
  ) || die "could not build the guest fixture (Docker or Apple Containers must be running the first time)"

  if [ ! -d "$OPT_DIR" ] || [ ! -w "$OPT_DIR" ]; then
    say "Creating $OPT_DIR for the release job's scripts (sudo, once)"
    sudo install -d -m 755 "$OPT_DIR"
    sudo chown "$(id -u):$(id -g)" "$OPT_DIR"
  fi
  mkdir -p "$OPT_DIR/lib"

  # Not local: the EXIT trap removes it however provision ends.
  stage=$(mktemp -d "${TMPDIR:-/tmp}/awman-provision.XXXXXX")
  trap 'rm -rf "$stage"' EXIT
  for name in release-offline-session release-workload; do
    printf '#!/bin/bash\n# %s; re-run on every `make release`, so do not edit.\nexec /bin/bash %q --fixture %q "$@"\n' \
      "$MARKER" "$OPT_DIR/lib/release-offline-session.sh" "$OPT_DIR/fixture-oci.tar" > "$stage/$name.sh"
  done
  install_opt "$ROOT/tools/release-offline-session.sh" lib/release-offline-session.sh 755
  install_opt "$fixture" fixture-oci.tar 644
  install_opt "$stage/release-offline-session.sh" release-offline-session.sh 755
  install_opt "$stage/release-workload.sh" release-workload.sh 755
}

# Online runners in <repo> that carry the hypervisor label, excluding ours.
# Listing (and registering) runners needs repository admin rights.
cmd_busy() {
  local repo=$1 ours="" names
  [ -f "$STATE" ] && ours=$(sed -n 's/^name=//p' "$STATE")
  mkdir -p "$RUNNER_HOME"
  if ! names=$(gh api "repos/$repo/actions/runners" --paginate \
    -q ".runners[] | select(.status == \"online\") | select([.labels[].name] | index(\"$LABEL\")) | .name" \
    2> "$RUNNER_HOME/busy.err"); then
    case "$(cat "$RUNNER_HOME/busy.err")" in
      *"HTTP 403"*|*"HTTP 404"*)
        die "listing $repo's self-hosted runners needs repository admin rights for the gh login. Ask an admin to run the release, or use RELEASE_RUNNER=none with a hypervisor runner someone else runs (RELEASE_RUNNER=local needs admin too, to register this Mac)." ;;
      *) cat "$RUNNER_HOME/busy.err" >&2; die "could not list $repo's self-hosted runners" ;;
    esac
  fi
  [ -z "$names" ] || printf '%s\n' "$names" | awk -v ours="$ours" '$0 != ours'
}

# Download (once) and verify the latest osx-arm64 runner. The GitHub API's
# release body has checksum markers; rendered notes show a plain checksum line.
# An archive with no unique matching checksum or a mismatched hash is refused.
fetch_runner() {
  local tag version asset body want got
  tag=$(gh api repos/actions/runner/releases/latest -q .tag_name)
  version=${tag#v}
  asset="actions-runner-osx-arm64-$version.tar.gz"
  mkdir -p "$RUNNER_HOME/cache"
  if [ ! -f "$RUNNER_HOME/cache/$asset" ]; then
    say "Downloading GitHub Actions runner $version" >&2
    gh release download "$tag" --repo actions/runner --pattern "$asset" \
      --dir "$RUNNER_HOME/cache" --clobber
  fi
  body=$(gh api "repos/actions/runner/releases/tags/$tag" -q .body)
  want=$(printf '%s\n' "$body" | awk -v asset="$asset" \
    '($1 == "*" || $1 == "-") && $2 == asset { print }' |
    sed -n 's/.*<!-- BEGIN SHA osx-arm64 -->\([0-9a-fA-F]\{64\}\)<!-- END SHA osx-arm64 -->.*/\1/p')
  if [ -z "$want" ]; then
    want=$(printf '%s\n' "$body" | awk -v asset="$asset" \
      '($1 == "*" || $1 == "-") && $2 == asset && length($3) == 64 && $3 ~ /^[[:xdigit:]]+$/ && NF == 3 { print tolower($3) }')
  fi
  [ -n "$want" ] && [ "${want#*$'\n'}" = "$want" ] ||
    die "actions/runner $tag publishes no unique osx-arm64 checksum; refusing to run it"
  got=$(sha256 "$RUNNER_HOME/cache/$asset")
  if [ "$got" != "$want" ]; then
    rm -f "$RUNNER_HOME/cache/$asset"
    die "checksum mismatch for $asset (expected $want, got $got)"
  fi
  echo "$RUNNER_HOME/cache/$asset"
}

cmd_verify_archive() {
  local archive
  archive=$(fetch_runner)
  say "Verified $(basename "$archive")"
}

runner_id() { # repo name
  gh api "repos/$1/actions/runners" --paginate -q ".runners[] | select(.name == \"$2\") | .id"
}

cmd_up() {
  local repo=$1 tag=$2 archive name token pid deadline status
  [ -f "$STATE" ] && cmd_down
  archive=$(fetch_runner)
  rm -rf "$RUN_DIR"
  mkdir -p "$RUN_DIR"
  # Only a work directory this script creates is marked, so `down` never
  # deletes one that was already there. It must be private and ours: the
  # default lives in the shared /tmp.
  if [ ! -e "$WORK_DIR" ]; then
    mkdir -p "$(dirname "$WORK_DIR")"
    mkdir -m 700 "$WORK_DIR"
    touch "$WORK_DIR/.awman-release-runner"
  fi
  [ ! -L "$WORK_DIR" ] && [ -d "$WORK_DIR" ] && [ -O "$WORK_DIR" ] ||
    die "$WORK_DIR is not a directory owned by $(id -un)"
  tar -xzf "$archive" -C "$RUN_DIR"
  name="awman-release-$tag-$(hostname -s)"
  token=$(gh api -X POST "repos/$repo/actions/runners/registration-token" -q .token)
  # The runner reads ACTIONS_RUNNER_INPUT_<ARG> as <ARG>, which keeps the
  # token out of the process list (an argument is visible to every user).
  (cd "$RUN_DIR" && ACTIONS_RUNNER_INPUT_TOKEN="$token" ./config.sh --unattended --url "https://github.com/$repo" \
    --name "$name" --labels "$LABEL" --work "$WORK_DIR" --replace --disableupdate) > "$RUNNER_HOME/config.log" 2>&1 ||
    { cat "$RUNNER_HOME/config.log" >&2; die "runner registration failed"; }
  printf 'repo=%s\nname=%s\n' "$repo" "$name" > "$STATE"

  # Job control gives the runner its own process group, so `down` stops the
  # listener and any job it is running together.
  set -m
  (cd "$RUN_DIR" && exec ./run.sh) > "$RUNNER_HOME/runner.log" 2>&1 &
  pid=$!
  set +m
  echo "pid=$pid" >> "$STATE"
  # Keep the Mac awake while the runner lives (no-op where caffeinate is absent).
  if command -v caffeinate >/dev/null 2>&1; then
    caffeinate -i -w "$pid" >/dev/null 2>&1 &
  fi

  deadline=$((SECONDS + ONLINE_TIMEOUT))
  while :; do
    status=$(gh api "repos/$repo/actions/runners" --paginate -q ".runners[] | select(.name == \"$name\") | .status" || true)
    [ "$status" = online ] && break
    kill -0 "$pid" 2>/dev/null || { tail -n 20 "$RUNNER_HOME/runner.log" >&2; cmd_down; die "runner exited before coming online"; }
    [ $SECONDS -lt $deadline ] || { cmd_down; die "runner did not come online within ${ONLINE_TIMEOUT}s"; }
    sleep 3
  done
  say "Runner '$name' is online (log: $RUNNER_HOME/runner.log)"
}

# Is process group <pgid> still the runner `up` started? Its saved pid leads
# that group, but after a reboot the pid may belong to an unrelated program,
# whose group must never be signalled. A pgid stays taken while any member
# lives, so this also finds a runner whose leader has already exited.
is_runner_group() { # pgid
  ps -A -o pgid= -o command= 2>/dev/null |
    awk -v g="$1" '$1 == g && /run\.sh|run-helper\.sh|Runner\.Listener|Runner\.Worker/ { found = 1 } END { exit !found }'
}

cmd_down() {
  [ -f "$STATE" ] || return 0
  local repo name pid id token
  repo=$(sed -n 's/^repo=//p' "$STATE")
  name=$(sed -n 's/^name=//p' "$STATE")
  pid=$(sed -n 's/^pid=//p' "$STATE")
  if [ -n "$pid" ] && is_runner_group "$pid"; then
    # SIGINT is the listener's graceful shutdown; escalate if it lingers.
    kill -INT -- "-$pid" 2>/dev/null || kill -INT "$pid" 2>/dev/null || true
    for _ in $(seq 1 30); do is_runner_group "$pid" || break; sleep 1; done
    kill -KILL -- "-$pid" 2>/dev/null || true
  elif [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    echo "release-runner: pid $pid is no longer the runner; not signalling it" >&2
  fi
  token=$(gh api -X POST "repos/$repo/actions/runners/remove-token" -q .token 2>/dev/null || true)
  if [ -z "$token" ] || ! (cd "$RUN_DIR" && ACTIONS_RUNNER_INPUT_TOKEN="$token" ./config.sh remove) >/dev/null 2>&1; then
    id=$(runner_id "$repo" "$name" 2>/dev/null || true)
    [ -z "$id" ] || gh api -X DELETE "repos/$repo/actions/runners/$id" >/dev/null 2>&1 ||
      echo "release-runner: could not deregister '$name'; remove it under Settings → Actions → Runners" >&2
  fi
  # Only a work directory this script created is removed.
  [ ! -e "$WORK_DIR/.awman-release-runner" ] || rm -rf "$WORK_DIR"
  rm -rf "$RUN_DIR" "$STATE"
  say "Runner '$name' stopped and removed"
}

case "${1:-}" in
  check) cmd_check "${2:-}" ;;
  provision) cmd_provision ;;
  busy) [ $# -eq 2 ] || die "usage: $0 busy <owner/repo>"; cmd_busy "$2" ;;
  verify-archive) [ $# -eq 1 ] || die "usage: $0 verify-archive"; cmd_verify_archive ;;
  up) [ $# -eq 3 ] || die "usage: $0 up <owner/repo> <tag>"; cmd_up "$2" "$3" ;;
  down) cmd_down ;;
  *) die "usage: $0 check [--hardware] | provision | busy <owner/repo> | verify-archive | up <owner/repo> <tag> | down" ;;
esac
