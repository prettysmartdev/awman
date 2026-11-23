#!/usr/bin/env bash
# Run from SSH with stdin carrying one GitHub token and stdout reserved for evidence.
set -euo pipefail
branch=${1:?expected release branch}
commit=${2:?expected commit}
[[ "$branch" =~ ^release/v[0-9]+\.[0-9]+\.[0-9]+$ ]] || exit 2
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || exit 2
test "$(uname -s)" = Linux && test "$(uname -m)" = x86_64 || {
  echo 'Release Linux acceptance requires a native x86_64 Linux KVM host' >&2
  exit 1
}
# SSH uses a noninteractive shell; a login profile may not have put rustup or
# mise on PATH. Resolve the installed user toolchain before accepting a token.
if ! command -v cargo >/dev/null 2>&1; then
  for cargo_bin in "${CARGO_HOME:-$HOME/.cargo}/bin/cargo" \
    "$HOME/.cargo/bin/cargo" "$HOME/.local/share/mise/shims/cargo" \
    "$HOME/.local/bin/cargo"; do
    if test -x "$cargo_bin"; then
      PATH="$(dirname "$cargo_bin"):$PATH"
      export PATH
      break
    fi
  done
fi
command -v cargo >/dev/null 2>&1 || {
  echo 'Cargo is unavailable to noninteractive SSH; install it under ~/.cargo/bin or expose it in the SSH command PATH' >&2
  exit 1
}
work=$(dirname -- "$0")
[[ "$work" =~ ^/tmp/awman-linux-prerelease\.[A-Za-z0-9]+$ ]] || exit 2
trap 'rm -rf -- "$work"' EXIT
trap 'exit 130' HUP INT TERM
umask 077
IFS= read -r token || { echo 'Missing GitHub token on SSH stdin' >&2; exit 1; }
test -n "$token" || exit 1
printf '%s' "$token" > "$work/token"
unset token
cat > "$work/askpass" <<'ASKPASS'
#!/bin/sh
case "$1" in
  *Username*) printf '%s\n' x-access-token ;;
  *Password*) cat "$AWMAN_PRERELEASE_TOKEN_FILE" ;;
  *) exit 1 ;;
esac
ASKPASS
chmod 700 "$work/askpass"
base="${HOME:?}/.cache/awman-prerelease"
test ! -L "$base" || { echo 'Refusing symlinked prerelease cache' >&2; exit 1; }
mkdir -p "$base"
test -O "$base" || { echo 'Prerelease cache must be owned by the SSH user' >&2; exit 1; }
chmod 700 "$base"
repo="$base/${branch//\//-}"
if test -e "$repo"; then
  test ! -L "$repo" && test -d "$repo/.git" && test -O "$repo" || {
    echo 'Existing prerelease checkout is not an owned Git repository' >&2
    exit 1
  }
  test "$(git -C "$repo" remote get-url origin)" = https://github.com/prettysmartdev/awman.git || {
    echo 'Existing prerelease checkout has the wrong origin' >&2
    exit 1
  }
  test -z "$(git -C "$repo" status --porcelain)" || {
    echo 'Existing prerelease checkout has source changes' >&2
    exit 1
  }
  AWMAN_PRERELEASE_TOKEN_FILE="$work/token" GIT_ASKPASS="$work/askpass" \
    GIT_TERMINAL_PROMPT=0 git -C "$repo" -c credential.helper= fetch --quiet origin "$branch" >&2
  rm -f -- "$work/token" "$work/askpass"
  git -C "$repo" switch --quiet "$branch"
  git -C "$repo" merge --ff-only --quiet "origin/$branch" >&2
else
  AWMAN_PRERELEASE_TOKEN_FILE="$work/token" GIT_ASKPASS="$work/askpass" \
    GIT_TERMINAL_PROMPT=0 git -c credential.helper= clone --quiet --no-checkout --branch "$branch" \
    https://github.com/prettysmartdev/awman.git "$repo" >&2
  rm -f -- "$work/token" "$work/askpass"
  git -C "$repo" reset --hard --quiet HEAD
fi
cd "$repo"
test "$(git rev-parse HEAD)" = "$commit" || {
  echo 'Remote release branch differs from the local commit' >&2
  exit 1
}
umask 0002
touch "$work/start"
CARGO_TARGET_DIR=target/native-validation-tools cargo xtask native-validation run --platform linux >&2
mapfile -t archives < <(find target/native-validation -maxdepth 1 -type f -name '*.tar.gz' -newer "$work/start" -print)
test "${#archives[@]}" -eq 1 || {
  echo 'Expected exactly one native evidence archive' >&2
  exit 1
}
cat -- "${archives[0]}"
