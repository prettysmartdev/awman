#!/bin/sh
# Fixed SSH payload: no checkout, Rust toolchain or provider credentials needed.
set -eu
name=$1
nonce=$2
case "$name:$nonce" in *[!a-z0-9:-]*) exit 2 ;; esac
test "$(uname -s)" = Linux || { printf 'LINUX_HOST must run Linux\n' >&2; exit 2; }
arch=$(uname -m)
case "$arch" in x86_64|aarch64) ;; *) printf 'Unsupported Linux architecture: %s\n' "$arch" >&2; exit 2 ;; esac
cli=$(command -v sbx) || { printf 'sbx is not on the SSH account PATH\n' >&2; exit 2; }
scratch=$(mktemp -d "${TMPDIR:-/tmp}/awman-sbx-validation.XXXXXXXX")
chmod 700 "$scratch"
mkdir "$scratch/workspace"
cd "$scratch/workspace"
workspace=$(pwd -P)
created=false
round_trip=false
version=
cli_sha=
: > "$scratch/steps"

step() {
    phase=$1
    shift
    printf 'validate-sbx-linux: %s started\n' "$phase" >&2
    if timeout 300 "$@" > "$scratch/$phase.stdout"; then code=0; else code=$?; fi
    cat "$scratch/$phase.stdout" >&2
    if test -s "$scratch/steps"; then printf ',' >> "$scratch/steps"; fi
    printf '"%s":%s' "$phase" "$code" >> "$scratch/steps"
    if test "$code" -eq 0; then
        printf 'validate-sbx-linux: %s passed\n' "$phase" >&2
    else
        printf 'validate-sbx-linux: %s failed (exit %s)\n' "$phase" "$code" >&2
    fi
    return "$code"
}

finish() {
    result=$?
    trap - EXIT HUP INT TERM
    set +e
    # A failed create must not authorize deleting a pre-existing sandbox.
    if "$created"; then
        step remove "$cli" --no-input rm --force "$name" || result=1
    fi
    step cleanup "$cli" --no-input ls --json || { if "$created"; then result=1; fi; }
    printf '{"schema":1,"target":"linux/%s","sandbox":"%s",' "$arch" "$name"
    printf '"cli_version":"%s","cli_sha256":"%s","exit":%s,' "$version" "$cli_sha" "$result"
    printf '"guest_round_trip":%s,"steps":{' "$round_trip"
    cat "$scratch/steps"
    printf '},"session":'
    if test -s "$scratch/session.stdout"; then cat "$scratch/session.stdout"; else printf null; fi
    printf ',"inspection":'
    if test -s "$scratch/inspect.stdout"; then cat "$scratch/inspect.stdout"; else printf null; fi
    printf ',"cleanup":'
    if test -s "$scratch/cleanup.stdout"; then cat "$scratch/cleanup.stdout"; else printf null; fi
    printf '}\n'
    if test "$result" -eq 0; then rm -rf "$scratch"; else printf 'Remote diagnostic files: %s\n' "$scratch" >&2; fi
    exit "$result"
}
trap finish EXIT
trap 'exit 1' HUP INT TERM

step version "$cli" --version || exit 2
version=$(sed -n 's/^[^0-9]*\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\).*/\1/p' "$scratch/version.stdout")
test -n "$version" || exit 2
major=${version%%.*}
rest=${version#*.}
minor=${rest%%.*}
if test "$major" -eq 0 && test "$minor" -lt 54; then exit 2; fi
cli_sha=$(sha256sum "$cli")
cli_sha=${cli_sha%% *}
step session "$cli" --no-input ls --json || exit 2
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
after=$(sha256sum "$cli")
test "${after%% *}" = "$cli_sha"
round_trip=true
