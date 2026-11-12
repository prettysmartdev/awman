#!/bin/sh
# Runs inside the guest image; each failure remains visible to the host runner.
set -eu
check() { label=$1; shift; if "$@"; then printf 'PASS\t%s\n' "$label"; else printf 'FAIL\t%s\n' "$label"; exit 1; fi; }
check image-user test "$(id -u)" = 1234
check image-group test "$(id -g)" = 1234
check image-workdir test "$PWD" = /image-work
check image-home test "$HOME" = /home/probe
check image-env test "$IMAGE_ENV" = 'image value'
check image-file-whiteout test ! -e /compat/remove-me
check image-opaque-whiteout test ! -e /compat/opaque/old
check image-upper-layer test "$(cat /compat/opaque/new)" = visible
check image-executable-mode test -x /compat/mode
printf 'image-contract-pass\n'
