#!/usr/bin/env bash
# Find exactly one organization runner group with the required workflow policy.
set -euo pipefail
repo=${1:?repository required}
role=${2:?role required}
[[ "$repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || exit 2
case "$role" in
  ci-guest) first=ci.yml; second=nightly.yml ;;
  release) first=candidate.yml; second=promote.yml ;;
  *) echo "Unknown runner role $role" >&2; exit 2 ;;
esac
org=${repo%%/*}
matches=()
if ! group_ids=$(gh api "orgs/$org/actions/runner-groups" --paginate --jq '.runner_groups[].id'); then
  echo "Cannot inspect $org runner groups; the gh login needs organization runner-group access" >&2
  exit 1
fi
while IFS= read -r id; do
  test -n "$id" || continue
  allowed=$(gh api "orgs/$org/actions/runner-groups/$id" --jq \
    ". | select(.restricted_to_workflows == true) | select((.selected_workflows | sort) == ([\"$repo/.github/workflows/$first@refs/heads/main\", \"$repo/.github/workflows/$second@refs/heads/main\"] | sort)) | .id")
  if test -n "$allowed"; then matches+=("$id"); fi
done <<< "$group_ids"
test "${#matches[@]}" -eq 1 || {
  echo "Expected exactly one workflow-restricted $role runner group for $repo" >&2
  exit 1
}
printf '%s\n' "${matches[0]}"
