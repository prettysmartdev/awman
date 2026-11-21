#!/usr/bin/env bash
# Wait for the reviewed release PR, then run protected-main candidate/promotion.
set -euo pipefail
test "$(uname -s)" = Darwin && test "$(uname -m)" = arm64 || {
  echo 'make release runs from the Apple Silicon release Mac' >&2
  exit 1
}
command -v python3 >/dev/null || { echo 'python3 is required on the release Mac' >&2; exit 1; }
root=$(git rev-parse --show-toplevel)
cd "$root"
cargo_version=$(sed -n 's/^version = "\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)"$/\1/p' Cargo.toml | head -1)
version=${VERSION:-v$cargo_version}
[[ "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] && test "$version" = "v$cargo_version" || {
  echo 'Release version must match the version prepared in Cargo.toml' >&2
  exit 2
}
branch="release/$version"
current_branch=$(git branch --show-current)
test -z "$(git status --porcelain)" || {
  echo 'Release checkout must be clean' >&2
  exit 1
}
if test "$current_branch" = main; then
  git show-ref --verify --quiet "refs/heads/$branch" || {
    echo "Local $branch is required to resume the reviewed release" >&2
    exit 1
  }
  branch_tree=$(git rev-parse "refs/heads/$branch^{tree}")
  test "$(git rev-parse 'HEAD^{tree}')" = "$branch_tree" || {
    echo 'Current main differs from the reviewed release tree' >&2
    exit 1
  }
  git switch "$branch"
  current_branch="$branch"
fi
test "$current_branch" = "$branch" || {
  echo "Run make release from the clean $branch branch" >&2
  exit 1
}
commit=$(git rev-parse HEAD)
tree=$(git rev-parse 'HEAD^{tree}')
remote_head=$(git ls-remote origin "refs/heads/$branch" | cut -f 1)
gh auth status >/dev/null
repo=$(gh repo view --json nameWithOwner -q .nameWithOwner)
test "$repo" = prettysmartdev/awman || { echo 'Wrong release repository' >&2; exit 1; }
linux="$root/target/prerelease/linux-$commit/evidence"
apple="$root/target/prerelease/apple-$commit/evidence"
test -f "$linux/packet.json" && test -f "$apple/packet.json" || {
  echo 'Both prerelease targets must pass for the same release branch commit' >&2
  exit 1
}
cargo xtask native-validation verify "$linux"
cargo xtask native-validation verify "$apple"
test "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["target"])' "$linux/packet.json")" = x86_64-unknown-linux-gnu
test "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["target"])' "$apple/packet.json")" = aarch64-apple-darwin
main_run=$(cat "$root/target/prerelease/apple-$commit/main-ci-run")
test "$(gh run view "$main_run" --repo "$repo" --json status,conclusion \
  --jq 'select(.status == "completed") | .conclusion')" = success || {
  echo 'The full main CI gate recorded by apple-prerelease is not green' >&2
  exit 1
}
test "$(python3 -c 'import json,sys; g=json.load(open(sys.argv[1])); print(g.get("commit") == sys.argv[2] and g.get("gate") == "main" and g.get("verdict") == "passed")' \
  "$root/target/prerelease/apple-$commit/main-gate-evidence.json" "$commit")" = True
pr=$(cat "$root/target/prerelease/apple-$commit/pr-number")
[[ "$pr" =~ ^[0-9]+$ ]] || { echo 'Missing release PR identity' >&2; exit 1; }
pr_identity=$(gh pr view "$pr" --repo "$repo" --json state,baseRefName,headRefOid,isCrossRepository \
  --jq 'select((.state == "OPEN" or .state == "MERGED") and .baseRefName == "main" and .isCrossRepository == false) | .headRefOid')
test "$pr_identity" = "$commit" || {
  echo 'The release PR no longer matches the validated branch commit' >&2
  exit 1
}
pr_state=$(gh pr view "$pr" --repo "$repo" --json state --jq .state)
test "$remote_head" = "$commit" || test "$pr_state" = MERGED || {
  echo 'Remote release branch differs from the validated commit' >&2
  exit 1
}
echo "Waiting for every release PR check to finish: https://github.com/$repo/pull/$pr"
while :; do
  checks=$(gh pr checks "$pr" --repo "$repo" --json name,state || true)
  test -n "$checks" || { echo 'Cannot read release PR checks from GitHub' >&2; exit 1; }
  check_state=$(printf '%s' "$checks" | python3 -c '
import json, sys
try:
    checks = json.load(sys.stdin)
except (ValueError, TypeError):
    print("waiting")
    raise SystemExit
states = [item.get("state") for item in checks]
if any(state in ("FAILURE", "ERROR", "CANCELLED", "TIMED_OUT", "ACTION_REQUIRED") for state in states):
    print("failed")
elif any(item.get("name") == "gate" and item.get("state") == "SUCCESS" for item in checks) and all(state in ("SUCCESS", "SKIPPED", "NEUTRAL") for state in states):
    print("green")
else:
    print("waiting")
')
  case "$check_state" in
    green) break ;;
    failed) echo 'Release PR has a failed check; fix it and rerun both prerelease targets' >&2; exit 1 ;;
  esac
  test "$(gh pr view "$pr" --repo "$repo" --json state,headRefOid \
    --jq 'select(.state == "OPEN") | .headRefOid')" = "$commit" || {
    echo 'The release PR changed while checks were running' >&2
    exit 1
  }
  sleep 30
done
echo "All release PR checks are green. Review, approve and click Merge when ready (the queue squashes): https://github.com/$repo/pull/$pr"
while :; do
  state=$(gh pr view "$pr" --repo "$repo" --json state --jq .state)
  case "$state" in
    MERGED) break ;;
    OPEN) ;;
    *) echo "Release PR ended in state $state without merging" >&2; exit 1 ;;
  esac
  test "$(gh pr view "$pr" --repo "$repo" --json headRefOid --jq .headRefOid)" = "$commit" || {
    echo 'The release PR head changed; prerelease evidence is stale' >&2
    exit 1
  }
  sleep 30
done
test -n "$(gh api "repos/$repo/pulls/$pr/reviews?per_page=100" --paginate \
  --jq '.[] | select(.user.type == "User" and .state == "APPROVED") | .id')" || {
  echo 'The release PR has no recorded human approval' >&2
  exit 1
}
git fetch origin main --quiet
test "$(gh pr view "$pr" --repo "$repo" --json mergeCommit --jq .mergeCommit.oid)" = "$(git rev-parse origin/main)" || {
  echo 'main moved after the release PR merged; stop before promotion' >&2
  exit 1
}
test "$(git rev-parse 'origin/main^{tree}')" = "$tree" || {
  echo 'Merged main tree differs from the validated release branch; stop before promotion' >&2
  exit 1
}
git switch main
git merge --ff-only origin/main
AWMAN_RELEASE_RUNNER_GROUP_ID=$(bash "$root/tools/prerelease/group-id.sh" "$repo" release)
export AWMAN_RELEASE_RUNNER_GROUP_ID
RELEASE_RUNNER="${RELEASE_RUNNER:-auto}" cargo xtask release run "$version"
