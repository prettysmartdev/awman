#!/usr/bin/env bash
set -euo pipefail
test "$(uname -s)" = Darwin && test "$(uname -m)" = arm64 || {
  echo 'apple-prerelease requires native Apple Silicon macOS' >&2
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
test -z "$(git status --porcelain)" || {
  echo 'Prerelease pin commit left a dirty checkout' >&2
  exit 1
}
test "$(git ls-remote origin "refs/heads/$branch" | cut -f 1)" = "$commit" || {
  echo 'Local HEAD must equal the remote release branch' >&2
  exit 1
}
ci_group_id=
ci_up=0
work=
ci_evidence=
started=
ci_runner() {
  AWMAN_RELEASE_RUNNER_ROLE=ci-guest \
    AWMAN_CI_GUEST_RUNNER_GROUP_ID="$ci_group_id" \
    AWMAN_RELEASE_RUNNER_HOME="${HOME:?}/.awman/ci-guest-runner" \
    AWMAN_RELEASE_RUNNER_WORK=/tmp/awci \
    cargo xtask release runner "$@"
}
cleanup() {
  result=$?
  if test "$ci_up" = 1; then ci_runner down || result=1; fi
  if test -n "$work"; then rm -rf -- "$work"; fi
  if test -n "$ci_evidence"; then rm -rf -- "$ci_evidence"; fi
  if test -n "$started"; then rm -f -- "$started"; fi
  exit "$result"
}
trap cleanup EXIT
cargo xtask gates check
cargo xtask ci runner-preflight --platform macos-arm64-ci-guest
cargo xtask ci runner-preflight --platform macos-arm64-release
cargo xtask release runner check --hardware
cargo xtask release runner provision
if ! gh pr view "$branch" --repo "$repo" >/dev/null 2>&1; then
  gh pr create --repo "$repo" --base main --head "$branch" \
    --title "Release v$version" --body-file "docs/releases/v$version.md"
fi
test "$(gh pr view "$branch" --repo "$repo" --json state,baseRefName,headRefOid,isCrossRepository \
  --jq 'select(.state == "OPEN" and .baseRefName == "main" and .isCrossRepository == false) | .headRefOid')" = "$commit" || {
  echo 'The open release PR must target main at the exact prerelease commit' >&2
  exit 1
}
pr_number=$(gh pr view "$branch" --repo "$repo" --json number --jq .number)
ci_group_id=$(bash "$root/tools/prerelease/group-id.sh" "$repo" ci-guest)
AWMAN_RELEASE_RUNNER_GROUP_ID=$(bash "$root/tools/prerelease/group-id.sh" "$repo" release)
export AWMAN_RELEASE_RUNNER_GROUP_ID
gh variable list --repo "$repo" --env release --json name,value \
  --jq '.[] | select(.name == "AWMAN_RELEASE_APP_CLIENT_ID" and .value != "") | .name' \
  | grep -qx AWMAN_RELEASE_APP_CLIENT_ID || {
    echo 'Release environment is missing AWMAN_RELEASE_APP_CLIENT_ID' >&2
    exit 1
  }
gh secret list --repo "$repo" --env release --json name \
  --jq '.[] | select(.name == "AWMAN_RELEASE_APP_PRIVATE_KEY") | .name' \
  | grep -qx AWMAN_RELEASE_APP_PRIVATE_KEY || {
    echo 'Release environment is missing AWMAN_RELEASE_APP_PRIVATE_KEY' >&2
    exit 1
  }
other_ci_runners=$(ci_runner busy "$repo")
test -z "$other_ci_runners" || {
  echo "Other online CI guest runners could take the main gate; stop them before prerelease validation: $other_ci_runners" >&2
  exit 1
}
ci_runner provision
ci_runner up "$repo" "pre-${commit:0:12}"
ci_up=1
before=$(gh run list --repo "$repo" --workflow ci.yml --branch main --limit 1 \
  --json databaseId --jq '.[0].databaseId // 0')
gh workflow run ci.yml --repo "$repo" --ref main -f gate=main -f "ref=$commit"
main_run=
for _ in $(seq 1 60); do
  main_run=$(gh run list --repo "$repo" --workflow ci.yml --branch main --limit 30 \
    --json databaseId,event --jq "[.[] | select(.databaseId > $before and .event == \"workflow_dispatch\") | .databaseId] | max // empty")
  test -z "$main_run" || break
  sleep 5
done
test -n "$main_run" || { echo "No dispatched main CI run found for $commit" >&2; exit 1; }
gh run watch "$main_run" --repo "$repo" --exit-status --interval 60
ci_evidence=$(mktemp -d "$root/target/prerelease/ci.XXXXXXXX")
gh run download "$main_run" --repo "$repo" --name "gate-evidence-main-$commit" \
  --dir "$ci_evidence"
python3 - "$ci_evidence/gate-evidence.json" "$commit" <<'PY'
import json
import sys
gate = json.load(open(sys.argv[1]))
if gate.get("gate") != "main" or gate.get("commit") != sys.argv[2] or gate.get("verdict") != "passed":
    raise SystemExit("The dispatched main gate does not prove this release commit")
PY
ci_name=$(sed -n 's/^name=//p' "${HOME:?}/.awman/ci-guest-runner/state")
test -n "$ci_name" || { echo 'Managed CI guest runner lost its identity' >&2; exit 1; }
guest_job_runners=$(gh api "repos/$repo/actions/runs/$main_run/jobs?per_page=100" --paginate \
  --jq '.jobs[] | select([.labels[]?] | index("awman-ci-guest")) | .runner_name')
test -n "$guest_job_runners" && test -z "$(printf '%s\n' "$guest_job_runners" | grep -Fxv "$ci_name")" || {
  echo 'The main gate did not run all CI guest jobs on this Mac' >&2
  exit 1
}
if test "$ci_up" = 1; then ci_runner down; ci_up=0; fi
started=$(mktemp "$root/target/prerelease/apple-start.XXXXXXXX")
CARGO_TARGET_DIR=target/native-validation-tools cargo xtask native-validation run
latest=
archive_count=0
for archive in target/native-validation/*.tar.gz; do
  test -f "$archive" && test "$archive" -nt "$started" || continue
  latest=$archive
  archive_count=$((archive_count + 1))
done
rm -f -- "$started"
test "$archive_count" -eq 1 || { echo 'Expected exactly one new Apple native packet' >&2; exit 1; }
work=$(mktemp -d "$root/target/prerelease/apple.XXXXXXXX")
python3 "$root/tools/prerelease/extract-packet.py" "$latest" "$work"
cargo xtask native-validation verify "$work/evidence"
pin_for() {
  awk -v table="[fixtures.$1]" '$0 == table {inside=1;next} inside && /^\[/ {exit} inside && /^sha256 = / {gsub(/"/, "", $3); print $3; exit}' ci/gates.toml
}
native_oci=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["fixture_sha256"])' "$work/evidence/packet.json")
native_docker=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["docker_fixture_sha256"])' "$work/evidence/packet.json")
test "$native_oci" = "$(pin_for guest-oci-base)" &&
  test "$native_docker" = "$(pin_for guest-docker-base)" || {
    echo 'Native Apple fixture bytes differ from the published manifest pins' >&2
    exit 1
  }
test "$(git ls-remote origin "refs/heads/$branch" | cut -f 1)" = "$commit" || {
  echo 'The release branch changed during native validation; rerun both prerelease targets' >&2
  exit 1
}
marker="$root/target/prerelease/apple-$commit"
mkdir -p "$marker"
if test -e "$marker/evidence"; then
  mv -- "$marker/evidence" "$marker/evidence.previous.$(date +%s).$$"
fi
mv -- "$work/evidence" "$marker/evidence"
printf '%s\n' "$commit" > "$marker/commit"
printf '%s\n' "$main_run" > "$marker/main-ci-run"
printf '%s\n' "$pr_number" > "$marker/pr-number"
cp "$ci_evidence/gate-evidence.json" "$marker/main-gate-evidence.json"
echo "Apple native acceptance and the full main CI gate passed for $commit."
