#!/usr/bin/env bash
set -euo pipefail
login=${REMOTE_LOGIN:-}
[[ "$login" =~ ^[A-Za-z_][A-Za-z0-9_.-]*@[A-Za-z0-9][A-Za-z0-9.-]*$ ]] || {
  echo 'Usage: make linux-prerelease REMOTE_LOGIN=user@host' >&2
  exit 2
}
test "$(uname -s)" = Darwin && test "$(uname -m)" = arm64 || {
  echo 'linux-prerelease runs from the Apple Silicon release Mac' >&2
  exit 1
}
command -v python3 >/dev/null || { echo 'python3 is required on the release Mac' >&2; exit 1; }
# shellcheck source=tools/prerelease/branch.sh
source "$(dirname "$0")/branch.sh"
for variable in AWMAN_CI_GUEST_RUNNER AWMAN_RELEASE_REHEARSAL; do
  current=$(gh variable list --repo "$repo" --json name,value \
    --jq ".[] | select(.name == \"$variable\") | .value")
  if test "$current" != enabled; then
    gh variable set "$variable" --repo "$repo" --body enabled
  fi
done
bash "$root/tools/prerelease/check-policy.sh" "$repo"
bash "$root/tools/prerelease/pin-inputs.sh"
commit=$(git rev-parse HEAD)
test "$(git branch --show-current)" = "$branch" && test -z "$(git status --porcelain)" || {
  echo 'Prerelease input pins must be committed on the clean release branch' >&2
  exit 1
}
test "$(git ls-remote origin "refs/heads/$branch" | cut -f 1)" = "$commit" || {
  echo 'Prerelease input pins are not on the remote release branch' >&2
  exit 1
}
mkdir -p "$root/target/prerelease"
work=$(mktemp -d "$root/target/prerelease/linux.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
remote_work=$(ssh "$login" 'mktemp -d /tmp/awman-linux-prerelease.XXXXXXXX')
[[ "$remote_work" =~ ^/tmp/awman-linux-prerelease\.[A-Za-z0-9]+$ ]] || {
  echo 'Unexpected temporary directory from Linux host' >&2
  exit 1
}
if ! scp -q "$root/tools/prerelease/remote-linux.sh" "$login:$remote_work/run.sh"; then
  ssh "$login" rm -rf -- "$remote_work" || true
  exit 1
fi
gh auth token | ssh "$login" bash "$remote_work/run.sh" "$branch" "$commit" \
  > "$work/archive.tar.gz"
python3 "$root/tools/prerelease/extract-packet.py" "$work/archive.tar.gz" "$work"
cargo xtask native-validation verify "$work/evidence"
destination="$root/target/prerelease/linux-$commit"
if test -e "$destination"; then
  mv -- "$destination" "$destination.previous.$(date +%s).$$"
fi
mv -- "$work" "$destination"
trap - EXIT
printf 'Linux native packet: %s\n' "$destination/evidence"
