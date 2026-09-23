#!/usr/bin/env bash
# Run `cargo test` isolated from the developer's own machine.
#
# Usage: tools/isolated-test.sh [cargo test args...]
#
# Every `make test*` target runs the suite through here. The integration tests,
# and the `awman` binaries and daemons they spawn, are ordinary builds rather
# than `cfg(test)` ones, so without this they would read and write the same
# per-user state a real awman installation uses:
#
#   * AWMAN_TEST_ISOLATION=1 — awman itself keeps off per-user OS resources:
#     an in-memory keychain, an in-memory clipboard, daemons started as plain
#     child processes rather than through launchd / `systemd --user`, and no
#     network beyond loopback. The squad daemon's keychain item, launchd label
#     and systemd unit are each one fixed name, so a test daemon would
#     otherwise overwrite, stop or replace the developer's real one.
#   * A throwaway HOME and XDG base directories — nothing reads or writes the
#     real ~/.awman, ~/.claude, ~/.codex, ~/.gemini, ~/Library/LaunchAgents or
#     the gh CLI's login. Cargo and rustup keep their real homes.
#   * No global or system git config — fixture commits never run the
#     developer's signing setup, hooks or credential helpers.
#   * The developer's awman and GitHub variables are cleared, so no test sees
#     their squad key, API key, storage-root overrides or tokens.
#   * The real container and sandbox CLIs are off: awman and the tests see
#     `docker`, Apple's `container` and `sbx` as not installed, because their
#     tests build, run and remove images, containers and sandboxes in the
#     developer's own daemon. Opt back in per CLI with AWMAN_TEST_DOCKER=1,
#     AWMAN_TEST_APPLE_CONTAINER=1 or AWMAN_TEST_SBX=1 (`make test-full` sets
#     the first). With Docker opted in, the docker CLI keeps the real
#     ~/.docker, which is where its daemon context lives.
#   * stdin is /dev/null, as in CI. From a terminal, code that asks "is stdin
#     a terminal?" would take its interactive path, and a test could block on
#     a prompt or put the terminal into raw mode.
#   * TMPDIR is a fresh per-run directory under AWMAN_TEST_TMPROOT, removed on
#     exit, keeping fixtures out of the shared /tmp.
set -euo pipefail

tmproot="${AWMAN_TEST_TMPROOT:-/var/tmp/test-fixtures}"
mkdir -p "$tmproot"
# A failed `mktemp` must stop the run, not fall through with an empty TMPDIR:
# Rust's `std::env::temp_dir()` honours TMPDIR verbatim, so TMPDIR="" would
# make every `tempfile::tempdir()` a *relative* path inside the checkout.
run_dir="$(mktemp -d "$tmproot/test-run.XXXXXX")"
if [ -z "$run_dir" ] || [ ! -d "$run_dir" ]; then
    echo "isolated-test: no fixture directory under $tmproot" >&2
    exit 1
fi
trap 'rm -rf "$run_dir"' EXIT

# Resolve cargo's and rustup's homes from the real HOME before replacing it.
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
case "${AWMAN_TEST_DOCKER:-}" in
    1 | true | yes | on) export DOCKER_CONFIG="${DOCKER_CONFIG:-$HOME/.docker}" ;;
esac

test_home="$run_dir/home"
mkdir -p "$test_home"
export HOME="$test_home"
export XDG_CONFIG_HOME="$test_home/.config"
export XDG_DATA_HOME="$test_home/.local/share"
export XDG_CACHE_HOME="$test_home/.cache"

export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null

unset AWMAN_CONFIG_HOME AWMAN_API_ROOT AWMAN_SQUAD_ROOT AWMAN_ATTACH_DIR \
    AWMAN_OVERLAYS AWMAN_REMOTE_ADDR AWMAN_REMOTE_SESSION AWMAN_API_KEY \
    AWMAN_SQUAD_KEY AWMAN_MAX_CONCURRENT_AGENTS AWMAN_LAUNCH_MODE \
    GITHUB_TOKEN GH_TOKEN

export AWMAN_TEST_ISOLATION=1
export TMPDIR="$run_dir"

cargo test "$@" < /dev/null
