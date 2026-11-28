#!/bin/sh
# Fixed SSH payload for the release SBX smoke. The Linux host receives this
# script and the exact asset only: no checkout, toolchain, GitHub token or
# provider key. Every free-text field in a receipt is base64 so the receipt is
# always valid JSON; the controller decodes and verifies it.
#
#   probe
#   stage    <name> <root>
#   run      <name> <nonce> <root> <asset-sha256> <agent> <ready-sandbox> <arch> <min-version> <ready-secs> <step-secs> <create-secs>
#   list     <name> <root>
#   remove   <name> <root> [<sandbox>...]
set -eu
mode=${1:-}
# The SSH account may inherit Git routing through its login environment. The
# smoke repository is private scratch state, so never let Git route elsewhere.
for var in $(env | sed -n 's/^\(GIT_[A-Za-z0-9_]*\)=.*/\1/p'); do unset "$var"; done

safe() { case "$1" in '' | *[!a-z0-9-]*) printf 'invalid argument\n' >&2; exit 2 ;; esac; }
num() { case "$1" in '' | *[!0-9]*) printf 'invalid number\n' >&2; exit 2 ;; esac; }
semver() { case "$1" in '' | *[!0-9.]*) printf 'invalid version\n' >&2; exit 2 ;; esac; }
# The owned root is always <absolute base>/awman-sbx-release-<name>.
owned_path() {
    case "$1" in /*) ;; *) printf 'root must be absolute\n' >&2; exit 2 ;; esac
    case "$1" in *[!A-Za-z0-9/._-]* | *..*) printf 'invalid root\n' >&2; exit 2 ;; esac
    case "$1" in */awman-sbx-release-"$2") ;; *) printf 'root is not owned by this check\n' >&2; exit 2 ;; esac
}
b64() { if test -f "$1"; then printf '"'; base64 < "$1" | tr -d '\n'; printf '"'; else printf null; fi; }
b64s() { printf '"'; printf '%s' "$1" | base64 | tr -d '\n'; printf '"'; }
digest() { d=$(sha256sum "$1"); printf '%s' "${d%% *}"; }
cli_path() {
    p=$(command -v sbx 2>/dev/null) || return 1
    case "$p" in /*) ;; *) return 1 ;; esac
    readlink -f "$p" 2>/dev/null || printf '%s\n' "$p"
}
parse_version() { sed -n 's/^[^0-9]*\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\).*/\1/p' "$1" | head -n 1; }
# ver_ge <have> <want>: numeric X.Y.Z comparison.
ver_ge() {
    h1=${1%%.*}; hr=${1#*.}; h2=${hr%%.*}; h3=${hr#*.}
    w1=${2%%.*}; wr=${2#*.}; w2=${wr%%.*}; w3=${wr#*.}
    test "$h1" -gt "$w1" && return 0; test "$h1" -lt "$w1" && return 1
    test "$h2" -gt "$w2" && return 0; test "$h2" -lt "$w2" && return 1
    test "$h3" -ge "$w3"
}
root_state() {
    if test ! -e "$1" && test ! -L "$1"; then printf absent
    elif test -d "$1" && test ! -L "$1" && test -f "$1/owner" && test "$(cat "$1/owner")" = "$2"; then printf owned
    else printf foreign; fi
}
stop_worker() {
    worker_root=$1; worker_name=$2
    test -f "$worker_root/worker" || return 0
    read -r worker_pid worker_start worker_owner < "$worker_root/worker" || return 1
    case "$worker_pid:$worker_start:$worker_owner" in *[!0-9:a-z-]* | ::*) return 1 ;; esac
    test "$worker_owner" = "$worker_name" || return 1
    worker_alive() {
        kill -0 "$worker_pid" 2>/dev/null || return 1
        test "$(awk '{print $3}' "/proc/$worker_pid/stat" 2>/dev/null)" != Z
    }
    if ! worker_alive; then return 0; fi
    current_start=$(awk '{print $22}' "/proc/$worker_pid/stat" 2>/dev/null || true)
    cmdline=$(tr '\000' ' ' < "/proc/$worker_pid/cmdline" 2>/dev/null || true)
    test "$current_start" = "$worker_start" || return 1
    case "$cmdline" in *"-s -- run $worker_name "*) ;; *) return 1 ;; esac
    kill -TERM -- "-$worker_pid" 2>/dev/null || kill -TERM "$worker_pid" 2>/dev/null || true
    i=0
    while worker_alive && test "$i" -lt 5; do sleep 1; i=$((i + 1)); done
    if worker_alive; then
        kill -KILL -- "-$worker_pid" 2>/dev/null || kill -KILL "$worker_pid" 2>/dev/null || true
        i=0
        while worker_alive && test "$i" -lt 5; do sleep 1; i=$((i + 1)); done
    fi
    ! worker_alive
}
need() {
    for tool in "$@"; do
        command -v "$tool" > /dev/null 2>&1 || { printf 'missing tool on the SSH account PATH: %s\n' "$tool" >&2; exit 2; }
    done
}

case "$mode" in
probe)
    need uname sha256sum base64 timeout mktemp
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/awman-sbx-probe.XXXXXXXX")
    trap 'rm -rf "$tmp"' EXIT
    uname -s > "$tmp/os"; uname -m > "$tmp/arch"; uname -r > "$tmp/build"
    host=$(uname -n | sha256sum); host=${host%% *}
    cli=; version=; sha=; session=null
    if cli=$(cli_path); then
        sha=$(digest "$cli")
        if timeout -k 5s 20 "$cli" --version > "$tmp/version" 2> "$tmp/version.stderr"; then version=$(parse_version "$tmp/version"); fi
        if timeout -k 5s 20 "$cli" --no-input ls --json > "$tmp/listing" 2> "$tmp/listing.stderr"; then session=0; else session=$?; fi
    else
        cli=
    fi
    printf '{"schema":1,"os":%s,"arch":%s,"os_build":%s,"hostname_sha256":"%s",' "$(b64 "$tmp/os")" "$(b64 "$tmp/arch")" "$(b64 "$tmp/build")" "$host"
    printf '"cli":%s,"cli_version":"%s","cli_sha256":"%s","session":%s,"listing":%s}\n' "$(b64s "$cli")" "$version" "$sha" "$session" "$(b64 "$tmp/listing")"
    exit 0
    ;;
stage)
    test $# -eq 3 || exit 2
    name=$2; root=$3
    safe "$name"; owned_path "$root" "$name"
    if test -e "$root" || test -L "$root"; then printf 'owned root already exists\n' >&2; exit 3; fi
    umask 077
    mkdir -m 700 "$root"
    printf '%s' "$name" > "$root/owner"
    exit 0
    ;;
list)
    test $# -eq 3 || exit 2
    name=$2; root=$3
    safe "$name"; owned_path "$root" "$name"
    need base64 timeout mktemp
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/awman-sbx-list.XXXXXXXX")
    trap 'rm -rf "$tmp"' EXIT
    code=127
    if cli=$(cli_path); then
        if timeout -k 5s 60 "$cli" --no-input ls --json > "$tmp/listing" 2> "$tmp/listing.stderr"; then code=0; else code=$?; fi
    fi
    test "$code" -eq 0 || rm -f "$tmp/listing"
    printf '{"schema":1,"listing_exit":%s,"listing":%s,"root":"%s"}\n' "$code" "$(b64 "$tmp/listing")" "$(root_state "$root" "$name")"
    exit 0
    ;;
remove)
    test $# -ge 3 || exit 2
    name=$2; root=$3
    safe "$name"; owned_path "$root" "$name"
    shift 3
    result=0
    need timeout awk tr sleep
    test "$(root_state "$root" "$name")" = owned || { printf 'owned root does not authenticate cleanup\n' >&2; exit 4; }
    stop_worker "$root" "$name" || { printf 'owned remote worker could not be reconciled\n' >&2; exit 4; }
    if test $# -gt 0; then
        cli=$(cli_path) || { printf 'sbx is not on the SSH account PATH\n' >&2; exit 2; }
        for sandbox in "$@"; do
            safe "$sandbox"
            timeout -k 5s 60 "$cli" --no-input rm --force "$sandbox" || result=1
        done
    fi
    # Only a root whose owner file names this check is removed.
    if test "$(root_state "$root" "$name")" = owned; then rm -rf "$root" || result=1; fi
    exit "$result"
    ;;
run) ;;
*) printf 'unknown mode\n' >&2; exit 2 ;;
esac

test $# -eq 12 || { printf 'wrong argument count\n' >&2; exit 2; }
name=$2; nonce=$3; root=$4; asset_sha=$5; agent=$6; ready_sandbox=$7; want_arch=$8
min=$9; ready_secs=${10}; step_secs=${11}; create_secs=${12}
for value in "$name" "$nonce" "$asset_sha" "$agent" "$ready_sandbox"; do safe "$value"; done
case "$want_arch" in
    x86_64 | aarch64) ;;
    *) printf 'invalid architecture\n' >&2; exit 2 ;;
esac
semver "$min"; num "$ready_secs"; num "$step_secs"; num "$create_secs"
owned_path "$root" "$name"
need uname sha256sum base64 timeout git sed cat cmp setsid awk tr
test "$(uname -s)" = Linux || { printf 'LINUX_HOST must run Linux\n' >&2; exit 2; }
arch=$(uname -m)
test "$arch" = "$want_arch" || { printf 'LINUX_HOST architecture %s is not %s\n' "$arch" "$want_arch" >&2; exit 2; }
test "$(root_state "$root" "$name")" = owned || { printf 'owned root was not staged\n' >&2; exit 2; }
test -f "$root/awman" || { printf 'asset was not staged\n' >&2; exit 2; }
cd "$root"
test "$(pwd -P)" = "$root" || { printf 'owned root resolves elsewhere\n' >&2; exit 2; }
logs=$root/logs
mkdir "$logs" "$root/workspace"
worker_start=$(awk '{print $22}' "/proc/$$/stat")
printf '%s %s %s\n' "$$" "$worker_start" "$name" > "$root/worker"
cd "$root/workspace"
workspace=$(pwd -P)
created=false
round_trip=false
ready_ran=false
ready_exit=null
ready_error=
blocked=
version=
cli=
cli_sha=
cli_after=
uname -r > "$logs/os-build"
host=$(uname -n | sha256sum); host=${host%% *}
: > "$logs/steps"

step() {
    phase=$1
    shift
    limit=$step_secs
    test "$phase" = create && limit=$create_secs
    printf 'release-sbx-linux: %s started\n' "$phase" >&2
    if timeout -k 5s "$limit" "$@" > "$logs/$phase.stdout" 2> "$logs/$phase.stderr"; then code=0; else code=$?; fi
    if test -s "$logs/steps"; then printf ',' >> "$logs/steps"; fi
    printf '"%s":%s' "$phase" "$code" >> "$logs/steps"
    if test "$code" -eq 0; then
        printf 'release-sbx-linux: %s passed\n' "$phase" >&2
    else
        printf 'release-sbx-linux: %s failed (exit %s)\n' "$phase" "$code" >&2
    fi
    return "$code"
}
# A bounded call that is not a lifecycle step.
call() {
    label=$1; limit=$2
    shift 2
    printf 'release-sbx-linux: %s started\n' "$label" >&2
    if timeout -k 5s "$limit" "$@" > "$logs/$label.stdout" 2> "$logs/$label.stderr"; then code=0; else code=$?; fi
    printf 'release-sbx-linux: %s exit %s\n' "$label" "$code" >&2
    return "$code"
}
listed() { grep -q "\"$2\"" "$1"; }

finish() {
    result=$?
    trap - EXIT HUP INT TERM
    set +e
    # A failed create must not authorize deleting a pre-existing sandbox.
    if "$created"; then
        step remove "$cli" --no-input rm --force "$name" || result=1
    fi
    if test -n "$cli"; then
        step cleanup "$cli" --no-input ls --json || { if "$created"; then result=1; fi; }
    fi
    # The readiness sandbox did not exist before readiness ran, so it is ours.
    if "$ready_ran" && call ready-final "$step_secs" "$cli" --no-input ls --json && listed "$logs/ready-final.stdout" "$ready_sandbox"; then
        call ready-remove "$step_secs" "$cli" --no-input rm --force "$ready_sandbox" || result=1
        call ready-final "$step_secs" "$cli" --no-input ls --json || result=1
    fi
    if test -n "$cli" && test -f "$cli"; then cli_after=$(digest "$cli"); fi
    if test -n "$blocked" && test "$result" -eq 0; then result=2; fi
    # Keep the receipt on an open descriptor while removing its directory.
    # The controller must never receive a completed receipt before the owned
    # root is gone, even when SSH or setsid forks on the remote host.
    {
    printf '{"schema":1,"mode":"release","target":"linux/%s","sandbox":"%s","root":"%s",' "$arch" "$name" "$root"
    printf '"cli":%s,"cli_version":"%s","cli_sha256":"%s","cli_sha256_after":"%s",' "$(b64s "$cli")" "$version" "$cli_sha" "$cli_after"
    printf '"exit":%s,"blocked":"%s","guest_round_trip":%s,"sandbox_claimed":%s,"ready_claimed":%s,"steps":{' "$result" "$blocked" "$round_trip" "$created" "$ready_ran"
    cat "$logs/steps"
    printf '},"session":%s,"inspection":%s,"cleanup":%s,' "$(b64 "$logs/session.stdout")" "$(b64 "$logs/inspect.stdout")" "$(b64 "$logs/cleanup.stdout")"
    printf '"host_probe":%s,"guest_probe":%s,' "$(b64 "$workspace/.host-probe")" "$(b64 "$workspace/.guest-probe")"
    printf '"os_build":%s,"hostname_sha256":"%s","asset_sha256":"%s","asset_version":%s,' "$(b64 "$logs/os-build")" "$host" "$asset_now" "$(b64 "$logs/asset-version.stdout")"
    printf '"ready_sandbox":"%s","ready":{"ran":%s,"exit":%s,"error":"%s",' "$ready_sandbox" "$ready_ran" "$ready_exit" "$ready_error"
    printf '"stdout":%s,"stderr":%s,"pre":%s,"final":%s},"raw_logs":{' "$(b64 "$logs/ready.stdout")" "$(b64 "$logs/ready.stderr")" "$(b64 "$logs/ready-pre.stdout")" "$(b64 "$logs/ready-final.stdout")"
    separator=
    for file in "$logs"/*; do
        test -f "$file" || continue
        key=${file##*/}
        case "$key" in *[!A-Za-z0-9._-]*) continue ;; esac
        printf '%s"%s":%s' "$separator" "$key" "$(b64 "$file")"
        separator=,
    done
    printf '}}\n'
    } > "$root/receipt.json"
    exec 3< "$root/receipt.json"
    cd /
    rm -rf "$root" || { printf 'release-sbx-linux: owned root could not be removed\n' >&2; result=1; }
    cat <&3
    exec 3<&-
    exit "$result"
}
asset_now=
trap finish EXIT
trap 'exit 1' HUP INT TERM

cli=$(cli_path) || { cli=; blocked=sbx-cli-missing; exit 2; }
cli_sha=$(digest "$cli")
step version "$cli" --version || { blocked=sbx-cli-missing; exit 2; }
version=$(parse_version "$logs/version.stdout")
test -n "$version" || { blocked=sbx-cli-too-old; exit 2; }
ver_ge "$version" "$min" || { blocked=sbx-cli-too-old; exit 2; }
step session "$cli" --no-input ls --json || { blocked=sbx-not-signed-in; exit 2; }
if listed "$logs/session.stdout" "$name"; then blocked=sbx-name-occupied; exit 2; fi
printf '%s\n' "$nonce" > .host-probe
# create refuses an occupied name; do not use --force or attach by name here.
step create "$cli" --no-input create --name "$name" shell "$workspace"
created=true
step inspect "$cli" --no-input inspect "$name" --json
guest='set -eu; cd "$2"; test "$(uname -s)" = Linux; test "$(cat .host-probe)" = "$1"; printf "%s\n" "$1" > .guest-probe'
step exec "$cli" --no-input exec -w "$workspace" "$name" sh -c "$guest" awman-validation "$nonce" "$workspace"
cmp .host-probe .guest-probe
rm .guest-probe
step run "$cli" --no-input run --name "$name" -- -c "$guest" awman-validation "$nonce" "$workspace"
cmp .host-probe .guest-probe
step stop "$cli" --no-input stop "$name"
step restart "$cli" --no-input exec -w "$workspace" "$name" sh -c 'test "$(cat .guest-probe)" = "$1"' awman-validation "$nonce"
round_trip=true

# Exact-asset readiness through the production runtime selection, with
# private awman state and no AWMAN_* or provider variables.
asset_now=$(digest "$root/awman")
test "$asset_now" = "$asset_sha" || { ready_error=asset-digest; exit 1; }
chmod 700 "$root/awman"
mkdir "$root/repo" "$root/awman-home"
printf '%s\n' '{"runtime":{"kind":"sbx"},"agents":{"auth":{"mode":"none","refresh":{"enabled":false}}}}' > "$root/awman-home/config.json"
(
    cd "$root/repo"
    for var in $(env | sed -n 's/^\(GIT_[A-Za-z0-9_]*\)=.*/\1/p'); do unset "$var"; done
    export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_SYSTEM=/dev/null GIT_TEMPLATE_DIR=/dev/null
    export GIT_TERMINAL_PROMPT=0
    git init -q
    git -c user.name=awman-smoke -c user.email=smoke@awman.invalid -c commit.gpgsign=false commit -q --allow-empty -m 'awman release smoke'
) > "$logs/repo.log" 2>&1 || { ready_error=repo; exit 1; }
test "$(cd "$root/repo" && pwd -P)" = "$root/repo" || { ready_error=repo-path; exit 1; }
repo_top=$(cd "$root/repo" && git rev-parse --show-toplevel)
test "$repo_top" = "$root/repo" || { ready_error=repo-git-root; exit 1; }
call ready-pre "$step_secs" "$cli" --no-input ls --json || { ready_error=listing; exit 1; }
if listed "$logs/ready-pre.stdout" "$ready_sandbox"; then blocked=sbx-name-occupied; exit 2; fi
for var in $(env | sed -n 's/^\(AWMAN_[A-Za-z0-9_]*\)=.*/\1/p'); do unset "$var"; done
unset ANTHROPIC_API_KEY CLAUDE_CODE_OAUTH_TOKEN OPENAI_API_KEY GEMINI_API_KEY GOOGLE_API_KEY GH_TOKEN GITHUB_TOKEN 2> /dev/null || true
export AWMAN_HOME="$root/awman-home"
call asset-version 20 "$root/awman" --version || true
ready_ran=true
if (cd "$root/repo" && timeout -k 5s "$ready_secs" "$root/awman" ready --agent "$agent" --prepare-only --json > "$logs/ready.stdout" 2> "$logs/ready.stderr"); then ready_exit=0; else ready_exit=$?; fi
unset AWMAN_HOME
test "$(digest "$root/awman")" = "$asset_now" || { ready_error=asset-changed; exit 1; }
test "$ready_exit" -eq 0 || { ready_error=readiness; exit 1; }
