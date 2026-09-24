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

check image-user sh -c '[ "$(id -u)" = 1234 ]'
check image-group sh -c '[ "$(id -g)" = 1234 ]'
check image-workdir sh -c '[ "$PWD" = /image-work ]'
check image-home sh -c '[ "$HOME" = /home/probe ]'
check image-env sh -c '[ "$IMAGE_ENV" = "image value" ]'
check image-file-whiteout test ! -e /compat/remove-me
check image-opaque-whiteout test ! -e /compat/opaque/old
check image-upper-layer sh -c '[ "$(cat /compat/opaque/new)" = visible ]'
check image-executable-mode test -x /compat/mode
if [ "$failures" -eq 0 ]; then printf 'image-contract-pass\n'; fi
exit "$failures"
