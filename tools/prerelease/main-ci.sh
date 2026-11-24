#!/usr/bin/env bash
set -euo pipefail

repo=$1
commit=$2
evidence_dir=$3
artifact="gate-evidence-main-$commit"

verify_evidence() {
  python3 - "$1" "$commit" <<'PY'
import json
import sys

with open(sys.argv[1]) as file:
    gate = json.load(file)
if gate.get("gate") != "main" or gate.get("commit") != sys.argv[2] or gate.get("verdict") != "passed":
    raise SystemExit("Main CI evidence does not prove this release commit")
PY
}

download_verified() {
  local run=$1 candidate
  candidate=$(mktemp -d "$evidence_dir/candidate.XXXXXXXX") || return 1
  if gh run download "$run" --repo "$repo" --name "$artifact" --dir "$candidate" \
      >/dev/null 2>&1 &&
    test -f "$candidate/gate-evidence.json" &&
    verify_evidence "$candidate/gate-evidence.json" 2>/dev/null; then
    cp "$candidate/gate-evidence.json" "$evidence_dir/gate-evidence.json" || return 1
    return 0
  fi
  return 1
}

# A dispatch's head SHA is main, even when its ref input tests another commit.
# Look up artifacts by exact name, then authenticate their producing run and
# verify the commit and verdict inside the downloaded evidence.
runs=$(gh api --paginate --method GET \
  "repos/$repo/actions/artifacts?name=$artifact&per_page=100" \
  --jq '.artifacts[] | select(.expired == false) | .workflow_run.id // empty')
while IFS= read -r run; do
  test -n "$run" || continue
  trusted=$(gh run view "$run" --repo "$repo" \
    --json status,conclusion,event,headBranch,workflowName \
    --jq 'select(.status == "completed" and .conclusion == "success" and .event == "workflow_dispatch" and .headBranch == "main" and .workflowName == "CI") | true') || continue
  if test "$trusted" = true && download_verified "$run"; then
    echo "Reusing successful main CI run $run for $commit" >&2
    printf '%s\n' "$run"
    exit 0
  fi
done <<< "$runs"

before=$(gh run list --repo "$repo" --workflow ci.yml --branch main --limit 1 \
  --json databaseId --jq '.[0].databaseId // 0')
echo "Dispatching main CI for $commit" >&2
gh workflow run ci.yml --repo "$repo" --ref main -f gate=main -f "ref=$commit" >&2
main_run=
for _ in $(seq 1 60); do
  main_run=$(gh run list --repo "$repo" --workflow ci.yml --branch main --limit 30 \
    --json databaseId,event --jq "[.[] | select(.databaseId > $before and .event == \"workflow_dispatch\") | .databaseId] | max // empty")
  test -z "$main_run" || break
  sleep 5
done
test -n "$main_run" || { echo "No dispatched main CI run found for $commit" >&2; exit 1; }
gh run watch "$main_run" --repo "$repo" --exit-status --interval 60 >&2
download_verified "$main_run" || {
  echo "Main CI run $main_run has no passing evidence for $commit" >&2
  exit 1
}
printf '%s\n' "$main_run"
