#!/usr/bin/env bash
# Source from either prerelease target to create or resume one release branch.
root=$(git rev-parse --show-toplevel)
cd "$root" || exit 1
test -z "$(git status --porcelain)" || {
  echo 'Commit source changes before prerelease validation' >&2
  exit 1
}
current_version=$(sed -n 's/^version = "\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)"$/\1/p' Cargo.toml | head -1)
requested_version=${VERSION:-v$current_version}
[[ "$requested_version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo 'Use VERSION=vX.Y.Z for a new release' >&2
  exit 2
}
version=${requested_version#v}
branch="release/$requested_version"
current_branch=$(git branch --show-current)
if test "$current_branch" = main; then
  test "$(git ls-remote origin refs/heads/main | cut -f 1)" = "$(git rev-parse HEAD)" || {
    echo 'Local main must equal origin/main before creating the release branch' >&2
    exit 1
  }
  if git show-ref --verify --quiet "refs/heads/$branch"; then
    git switch "$branch"
  elif test -n "$(git ls-remote origin "refs/heads/$branch")"; then
    git fetch origin "$branch"
    git switch -c "$branch" --track "origin/$branch"
  else
    git switch -c "$branch"
  fi
elif test "$current_branch" != "$branch"; then
  echo "Check out main or $branch for prerelease validation" >&2
  exit 1
fi
current_version=$(sed -n 's/^version = "\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)"$/\1/p' Cargo.toml | head -1)
gh auth status >/dev/null
repo=$(gh repo view --json nameWithOwner -q .nameWithOwner)
test "$repo" = prettysmartdev/awman || {
  echo 'Prerelease must run from the canonical prettysmartdev/awman checkout' >&2
  exit 1
}
if gh release view "$requested_version" --repo "$repo" >/dev/null 2>&1; then
  echo "$requested_version already exists as a GitHub release; prepare the next version" >&2
  exit 1
fi
test "$current_version" = "$version" && test -s "docs/releases/$requested_version.md" && \
  ! grep -q '_Write release notes here._' "docs/releases/$requested_version.md" || {
  echo "Release prep must commit Cargo version $version and finished notes before prerelease validation" >&2
  exit 1
}
git push -u origin "$branch"
