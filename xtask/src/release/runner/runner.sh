#!/usr/bin/env bash
# tools/release-runner.sh — a temporary GitHub Actions runner for one release.
#
# The candidate/promote chain needs an Apple Silicon Mac with
# Hypervisor.framework (labels: self-hosted, macOS, ARM64, awman-release).
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
umask 077

ROOT=$(cd "$(dirname "$0")/../../../.." && pwd -P)
RUNNER_HOME="${AWMAN_RELEASE_RUNNER_HOME:-$HOME/.awman/release-runner}"
mkdir -p "$RUNNER_HOME"
RUNNER_HOME=$(cd "$RUNNER_HOME" && pwd -P)
export AWMAN_RELEASE_RUNNER_HOME="$RUNNER_HOME"
OPT_DIR="${AWMAN_RELEASE_OPT_DIR:-/opt/awman}"
# The job's state directories hold Unix sockets, whose paths macOS caps at
# 103 bytes, so jobs run under a short work directory rather than RUNNER_HOME.
WORK_DIR="${AWMAN_RELEASE_RUNNER_WORK:-/tmp/awr}"
ONLINE_TIMEOUT="${AWMAN_RELEASE_RUNNER_ONLINE_TIMEOUT:-120}"
STATE="$RUNNER_HOME/state"
RUN_DIR="$RUNNER_HOME/run"
LABEL=awman-release
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
  mkdir -p "$RUNNER_HOME/fixtures"
  cargo xtask fixtures fetch --fixture guest-oci-base --out "$RUNNER_HOME/fixtures" >&2 ||
    die "could not fetch the pinned guest fixture; verify the manifest digest and registry access"
  fixture="$RUNNER_HOME/fixtures/guest-oci-base.tar"
  [ -s "$fixture" ] || die "pinned guest fixture is missing: $fixture"

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
    # generated wrappers expand their own arguments.
    # shellcheck disable=SC2016
    printf '#!/bin/bash\n# %s; re-run on every `make release`, so do not edit.\nexec /bin/bash %q --fixture %q "$@"\n' \
      "$MARKER" "$OPT_DIR/lib/release-offline-session.sh" "$OPT_DIR/fixture-oci.tar" > "$stage/$name.sh"
  done
  install_opt "$ROOT/tools/release-offline-session.sh" lib/release-offline-session.sh 755
  install_opt "$fixture" fixture-oci.tar 644
  install_opt "$stage/release-offline-session.sh" release-offline-session.sh 755
  install_opt "$stage/release-workload.sh" release-workload.sh 755
}

# Online runners in <repo> that carry the release-only label, excluding ours.
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
  # either missing or ambiguous checksum is fatal.
  # shellcheck disable=SC2015
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

# Labels route jobs; only the service-enforced group authorizes them.
release_group() {
  local repo=$1 org=${1%%/*} group_id=${AWMAN_RELEASE_RUNNER_GROUP_ID:-} group
  case "$repo" in *[!a-zA-Z0-9_./-]*|*..*) die "invalid repository" ;; esac
  case "$group_id" in ''|*[!0-9]*) die "AWMAN_RELEASE_RUNNER_GROUP_ID must name a workflow-restricted organization runner group (P1)" ;; esac
  group=$(gh api "orgs/$org/actions/runner-groups/$group_id" --jq ". | select(.restricted_to_workflows == true) | select((.selected_workflows | sort) == ([\"$repo/.github/workflows/candidate.yml@refs/heads/main\", \"$repo/.github/workflows/promote.yml@refs/heads/main\"] | sort)) | .name") || die "cannot verify release runner group policy"
  [ -n "$group" ] || die "runner group must restrict execution to candidate.yml and promote.yml on protected main"
  printf '%s\n' "$group"
}

cmd_up() {
  local repo=$1 tag=$2 archive name token pid deadline status group owned_work=false
  group=$(release_group "$repo") || return 1
  [ -f "$STATE" ] && cmd_down
  trap 'status=$?; trap "" INT TERM; [ "$status" -eq 0 ] || cmd_down; exit "$status"' EXIT
  trap 'exit 130' INT TERM
  archive=$(fetch_runner)
  # Only a work directory this script creates is marked, so `down` never
  # deletes one that was already there. It must be private and ours: the
  # default lives in the shared /tmp.
  [ ! -L "$WORK_DIR" ] || die "$WORK_DIR is not a directory owned by $(id -un)"
  if [ ! -e "$WORK_DIR" ]; then
    mkdir -p "$(dirname "$WORK_DIR")"
    mkdir -m 700 "$WORK_DIR"
    touch "$WORK_DIR/.awman-release-runner"
    owned_work=true
  fi
  # all three ownership checks are required.
  # shellcheck disable=SC2015
  [ ! -L "$WORK_DIR" ] && [ -d "$WORK_DIR" ] && [ -O "$WORK_DIR" ] ||
    die "$WORK_DIR is not a directory owned by $(id -un)"
  chmod 700 "$WORK_DIR"
  rm -rf "$RUN_DIR"
  mkdir -p "$RUN_DIR"
  name="awman-release-$tag-$(hostname -s)"
  printf 'repo=%s\nname=%s\nwork=%s\nowned_work=%s\nregistered=false\n' "$repo" "$name" "$WORK_DIR" "$owned_work" > "$STATE"
  tar -xzf "$archive" -C "$RUN_DIR"
  token=$(gh api -X POST "orgs/${repo%%/*}/actions/runners/registration-token" -q .token)
  printf 'repo=%s\nname=%s\nwork=%s\nowned_work=%s\nregistered=true\n' "$repo" "$name" "$WORK_DIR" "$owned_work" > "$STATE"
  # The runner reads ACTIONS_RUNNER_INPUT_<ARG> as <ARG>, which keeps the
  # token out of the process list (an argument is visible to every user).
  (cd "$RUN_DIR" && ACTIONS_RUNNER_INPUT_TOKEN="$token" ./config.sh --unattended --url "https://github.com/${repo%%/*}" --runnergroup "$group" \
    --name "$name" --labels "$LABEL" --work "$WORK_DIR" --replace --disableupdate) > "$RUNNER_HOME/config.log" 2>&1 ||
    { cat "$RUNNER_HOME/config.log" >&2; die "runner registration failed"; }
  : # state already durable before registration

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
  trap - EXIT INT TERM
}

# Is process group <pgid> still the runner `up` started? Its saved pid leads
# that group, but after a reboot the pid may belong to an unrelated program,
# whose group must never be signalled. A pgid stays taken while any member
# lives, so this also finds a runner whose leader has already exited.
is_runner_group() { # pgid; names alone are not proof after PID reuse
  local member group command cwd
  while read -r member group command; do
    [ "$group" = "$1" ] || continue
    case "$command" in *run.sh*|*run-helper.sh*|*Runner.Listener*|*Runner.Worker*) ;; *) continue ;; esac
    if [ -d "/proc/$member" ]; then
      cwd=$(readlink "/proc/$member/cwd" 2>/dev/null || true)
    else
      cwd=$(lsof -a -p "$member" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' || true)
    fi
    [ "$cwd" != "$RUN_DIR" ] || return 0
  done < <(ps -A -o pid= -o pgid= -o command= 2>/dev/null)
  return 1
}

cmd_down() {
  [ -f "$STATE" ] || return 0
  trap '' INT TERM
  local repo name pid id token saved_work
  saved_work=$(sed -n 's/^work=//p' "$STATE")
  [ -z "$saved_work" ] || WORK_DIR=$saved_work
  repo=$(sed -n 's/^repo=//p' "$STATE")
  name=$(sed -n 's/^name=//p' "$STATE")
  pid=$(sed -n 's/^pid=//p' "$STATE")
  if [ -n "$pid" ] && is_runner_group "$pid"; then
    # SIGINT is the listener's graceful shutdown; escalate if it lingers.
    kill -INT -- "-$pid" 2>/dev/null || kill -INT "$pid" 2>/dev/null || true
    for _ in $(seq 1 30); do is_runner_group "$pid" || break; sleep 1; done
    if is_runner_group "$pid"; then kill -KILL -- "-$pid" 2>/dev/null || true; fi
  elif [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    die "pid $pid is no longer the runner; not signalling it (state retained)"
  fi
  if [ -n "$pid" ] && is_runner_group "$pid"; then
    die "runner group $pid is still alive (state retained)"
  fi
  if ! grep -qx 'registered=false' "$STATE"; then
  token=$(gh api -X POST "orgs/${repo%%/*}/actions/runners/remove-token" -q .token 2>/dev/null || true)
  if [ -z "$token" ] || ! (cd "$RUN_DIR" && ACTIONS_RUNNER_INPUT_TOKEN="$token" ./config.sh remove) >/dev/null 2>&1; then
    if id=$(runner_id "$repo" "$name" 2>/dev/null); then
      if [ -n "$id" ]; then
        gh api -X DELETE "repos/$repo/actions/runners/$id" >/dev/null 2>&1 ||
          die "could not deregister '$name'; remove it under Settings → Actions → Runners, then retry down (state retained)"
      fi
    else
      die "could not confirm deregistration of '$name'; remove it under Settings → Actions → Runners, then retry down (state retained)"
    fi
  fi
  fi
  # Only a work directory this script created is removed.
  if grep -qx 'owned_work=true' "$STATE" && [ ! -L "$WORK_DIR" ] && [ -O "$WORK_DIR" ] && [ -f "$WORK_DIR/.awman-release-runner" ]; then rm -rf "$WORK_DIR"; fi
  rm -rf "$RUN_DIR" "$STATE"
  say "Runner '$name' stopped and removed"
}

case "${1:-}" in
  check) [ $# -eq 1 ] || { [ $# -eq 2 ] && [ "$2" = --hardware ]; } || die "usage: $0 check [--hardware]"; cmd_check "${2:-}" ;;
  provision) [ $# -eq 1 ] || die "usage: $0 provision"; cmd_provision ;;
  busy) [ $# -eq 2 ] || die "usage: $0 busy <owner/repo>"; cmd_busy "$2" ;;
  verify-archive) [ $# -eq 1 ] || die "usage: $0 verify-archive"; cmd_verify_archive ;;
  up) [ $# -eq 3 ] || die "usage: $0 up <owner/repo> <tag>"; cmd_up "$2" "$3" ;;
  down) [ $# -eq 1 ] || die "usage: $0 down"; cmd_down ;;
  *) die "usage: $0 check [--hardware] | provision | busy <owner/repo> | verify-archive | up <owner/repo> <tag> | down" ;;
esac
