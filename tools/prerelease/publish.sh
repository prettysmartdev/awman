#!/usr/bin/env bash
# Validate the merged release commit, then tag and publish with the gh login.
set -Eeuo pipefail
version=${1:?version required}
commit=${2:?merged commit required}
repo=${3:?repository required}
test "$(git branch --show-current)" = main && test -z "$(git status --porcelain)" || {
  echo 'Publication requires a clean main checkout' >&2; exit 1;
}
test "$(git rev-parse HEAD)" = "$commit" &&
  test "$(git ls-remote origin refs/heads/main | cut -f 1)" = "$commit" || {
  echo 'main moved after the release PR merge; stop before publication' >&2; exit 1;
}
read_state() {
  state_line=$(cargo xtask release run --internal-state "$repo" "$version")
  IFS='|' read -r state candidate recorded_commit run_id _tag_on_origin <<< "$state_line"
}
read_state
if test "$state" = verified; then
  echo "Release $version is already published and verified"
  exit 0
fi
if test -n "$recorded_commit" && test "$recorded_commit" != "$commit" && test "$state" != abandoned; then
  echo "Existing candidate belongs to $recorded_commit, not merged commit $commit" >&2
  exit 1
fi
runner_mode=${RELEASE_RUNNER:-auto}
case "$runner_mode" in auto|local|none) ;; *) echo 'RELEASE_RUNNER must be auto, local or none' >&2; exit 2 ;; esac
local_runner=false
cleanup() {
  result=$?
  trap - EXIT
  if "$local_runner"; then cargo xtask release runner down || result=1; fi
  exit "$result"
}
trap cleanup EXIT
if test "$runner_mode" != none; then
  online=$(cargo xtask release runner busy "$repo")
  if test "$runner_mode" = local || test -z "$online"; then
    cargo xtask release runner provision
    local_runner=true
    cargo xtask release runner up "$repo" "$version"
  else
    echo "Using online release runner: $online"
  fi
fi
if test "$state" = none || test "$state" = abandoned; then
  echo "Starting candidate validation for merged commit $commit"
  gh workflow run candidate.yml --repo "$repo" --ref main \
    -f "version=$version" -f "commit=$commit" -f promote=false -f operator_record='{}'
fi
if test "$state" != promoted; then
  run_id=
  for _ in $(seq 1 60); do
    read_state
    if test "$recorded_commit" = "$commit" && test -n "$run_id"; then break; fi
    sleep 5
  done
  test -n "$run_id" && test "$recorded_commit" = "$commit" || {
    echo 'Candidate workflow did not appear for the merged commit' >&2; exit 1;
  }
  echo "Waiting for candidate validation: https://github.com/$repo/actions/runs/$run_id"
  gh run watch "$run_id" --repo "$repo" --exit-status --interval 60
  read_state
  test "$state" = validated || {
    echo "Candidate ended in state $state; nothing was tagged" >&2; exit 1;
  }
fi
[[ "$candidate" =~ ^[0-9]+$ ]] || { echo 'No validated candidate number' >&2; exit 1; }
out="target/prerelease/promotion-$version"
mkdir -p "$out"
args=(--repo "$repo" --version "$version" --commit "$commit" --candidate "$candidate" --namespace production --main-checkout . --out "$out")
echo "Rechecking candidate $candidate and publishing $version at $commit"
cargo xtask release promote verify-inputs "${args[@]}"
cargo xtask release promote tag "${args[@]}"
cargo xtask release promote publish "${args[@]}"
before=$(gh run list --repo "$repo" --workflow promote.yml --branch main --limit 1 --json databaseId --jq '.[0].databaseId // 0')
gh workflow run promote.yml --repo "$repo" --ref main \
  -f "version=$version" -f "commit=$commit" -f "candidate=$candidate" -f dry_run=false
verify_run=
for _ in $(seq 1 60); do
  verify_run=$(gh run list --repo "$repo" --workflow promote.yml --branch main --limit 30 \
    --json databaseId,displayTitle \
    --jq "[.[] | select(.databaseId > $before and .displayTitle == \"Verify $version candidate $candidate\") | .databaseId] | max // empty")
  test -z "$verify_run" || break
  sleep 5
done
test -n "$verify_run" || { echo 'Published verification workflow did not appear' >&2; exit 1; }
echo "Waiting for published asset verification: https://github.com/$repo/actions/runs/$verify_run"
gh run watch "$verify_run" --repo "$repo" --exit-status --interval 60
read_state
test "$state" = verified || {
  echo 'Published release has no verified receipt' >&2; exit 1;
}
echo "Release $version published and verified at $commit"
