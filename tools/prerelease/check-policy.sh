#!/usr/bin/env bash
# Read the live rules that protect release inputs and promotion.
set -euo pipefail
repo=${1:?repository required}
[[ "$repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || {
  echo 'Invalid repository name' >&2
  exit 2
}
test "$(gh api "repos/$repo/branches/main" --jq .protected)" = true || {
  echo 'main is not protected on GitHub' >&2
  exit 1
}
rules="repos/$repo/rules/branches/main"
for kind in merge_queue deletion non_fast_forward; do
  test "$(gh api "$rules" --jq "[.[] | select(.type == \"$kind\")] | length > 0")" = true || {
    echo "main is missing the active $kind rule" >&2
    exit 1
  }
done
test "$(gh api "$rules" --jq '[.[] | select(.type == "merge_queue") | .parameters.merge_method] | index("SQUASH") != null')" = true || {
  echo 'main merge queue must use the squash merge method' >&2
  exit 1
}
test "$(gh api "$rules" --jq '[.[] | select(.type == "required_status_checks") | .parameters.required_status_checks[]?.context] | index("gate") != null')" = true || {
  echo 'main does not require the gate status check' >&2
  exit 1
}
test "$(gh api "$rules" --jq '[.[] | select(.type == "pull_request") | .parameters.required_approving_review_count] | any(. >= 1)')" = true || {
  echo 'main must require at least one human PR approval before merge' >&2
  exit 1
}
tag_protected=false
while IFS= read -r id; do
  test -n "$id" || continue
  detail="repos/$repo/rulesets/$id"
  applies=$(gh api "$detail" --jq '(.conditions.ref_name.include // []) | index("refs/tags/v*") != null')
  restricts=$(gh api "$detail" --jq '[.rules[] | select(.type == "creation" or .type == "update" or .type == "deletion")] | length == 3')
  app=$(gh api "$detail" --jq '[.bypass_actors[]? | select(.actor_type == "Integration")] | length > 0')
  if test "$applies" = true && test "$restricts" = true && test "$app" = true; then
    tag_protected=true
    break
  fi
done < <(gh api "repos/$repo/rulesets?targets=tag" --paginate --jq '.[] | select(.enforcement == "active") | .id')
test "$tag_protected" = true || {
  echo 'v* tags need an active immutable ruleset with the promotion App bypass' >&2
  exit 1
}
bash "$(dirname "$0")/group-id.sh" "$repo" ci-guest >/dev/null
bash "$(dirname "$0")/group-id.sh" "$repo" release >/dev/null
environment="repos/$repo/environments/release"
test "$(gh api "$environment" --jq .deployment_branch_policy.custom_branch_policies)" = true || {
  echo 'Release environment must use a custom main-only branch policy' >&2
  exit 1
}
test "$(gh api "$environment/deployment-branch-policies?per_page=100" --jq '[.branch_policies[].name] == ["main"]')" = true || {
  echo 'Release environment must permit only main' >&2
  exit 1
}
test "$(gh api "repos/$repo/actions/permissions/workflow" --jq .default_workflow_permissions)" = read || {
  echo 'Default GitHub Actions token permissions are not read-only' >&2
  exit 1
}
test "$(gh variable list --repo "$repo" --json name,value --jq '.[] | select(.name == "AWMAN_RELEASE_REHEARSAL") | .value')" = enabled || {
  echo 'AWMAN_RELEASE_REHEARSAL must be enabled' >&2
  exit 1
}
test "$(gh variable list --repo "$repo" --json name,value --jq '.[] | select(.name == "AWMAN_CI_GUEST_RUNNER") | .value')" = enabled || {
  echo 'AWMAN_CI_GUEST_RUNNER must be enabled' >&2
  exit 1
}
