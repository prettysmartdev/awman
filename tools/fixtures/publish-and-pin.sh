#!/usr/bin/env bash
# Publish the reviewed guest fixtures once, verify both architectures, and
# commit their pins. Run on a clean branch based on the current origin/main.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd -P)
cd "$root"
repo=prettysmartdev/awman

die() { echo "fixtures-pin: $*" >&2; exit 1; }
for tool in git gh cargo; do
    command -v "$tool" >/dev/null || die "$tool is required"
done
case "$(git remote get-url origin)" in
    git@github.com:prettysmartdev/awman.git|\
    ssh://git@github.com/prettysmartdev/awman.git|\
    https://github.com/prettysmartdev/awman|\
    https://github.com/prettysmartdev/awman.git) ;;
    *) die "origin must be the canonical $repo repository" ;;
esac
[[ -z "$(git status --porcelain)" ]] || die "commit or stash checkout changes first"
git fetch --quiet origin main
main_sha=$(git rev-parse origin/main)
git merge-base --is-ancestor "$main_sha" HEAD || die "merge origin/main into this branch first"
for path in ci/fixture-base-image.txt xtask/src/fixtures/recipes/guest_image.rs tests/guest/support/image-contract.sh; do
    git diff --quiet origin/main -- "$path" || die "$path differs from the fixture workflow's source on main"
done
tag="fixtures-$main_sha"
if [[ "${1:-}" == --check ]]; then
    echo "fixtures-pin: ready to publish or reuse $tag"
    exit 0
fi
[[ $# -eq 0 ]] || die "usage: make fixtures-pin (or bash tools/fixtures/publish-and-pin.sh --check)"

if gh release view "$tag" -R "$repo" --json tagName >/dev/null 2>&1; then
    echo "fixtures-pin: using existing $tag"
else
    before=$(gh run list -R "$repo" --workflow fixtures.yml --event workflow_dispatch --limit 1 \
        --json databaseId --jq '.[0].databaseId // 0')
    gh workflow run fixtures.yml -R "$repo" --ref main -f "tag=$tag"
    run_line=
    for _ in {1..40}; do
        run_line=$(gh run list -R "$repo" --workflow fixtures.yml --event workflow_dispatch --limit 10 \
            --json databaseId,headSha \
            --jq "[.[] | select(.databaseId > $before)] | max_by(.databaseId) | if . == null then empty else [.databaseId,.headSha] | @tsv end")
        [[ -n "$run_line" ]] && break
        sleep 3
    done
    [[ -n "$run_line" ]] || die "the dispatched Fixtures run did not appear; inspect GitHub Actions"
    IFS=$'\t' read -r run_id run_sha <<< "$run_line"
    [[ "$run_sha" == "$main_sha" ]] || die "main moved before dispatch; Fixtures ran at $run_sha, expected $main_sha"
    gh run watch "$run_id" -R "$repo" --compact --exit-status
fi

details=$(gh release view "$tag" -R "$repo" --json targetCommitish,isPrerelease,isDraft \
    --jq '[.targetCommitish,.isPrerelease,.isDraft] | @tsv')
IFS=$'\t' read -r target prerelease draft <<< "$details"
[[ "$target" == "$main_sha" && "$prerelease" == true && "$draft" == false ]] || \
    die "$tag is not a published prerelease for $main_sha"
assets=$(gh release view "$tag" -R "$repo" --json assets --jq '[.assets[].name] | join(" ")')
for name in base-image.txt fixture-oci.tar fixture-docker.tar provenance.json fixture-oci-amd64.tar provenance-amd64.json SHA256SUMS; do
    [[ " $assets " == *" $name "* ]] || die "$tag lacks $name; immutable fixture releases cannot be replaced"
done

temp=$(mktemp -d "${TMPDIR:-/tmp}/awman-fixtures.XXXXXXXX")
pin_started=0
cleanup() {
    status=$?
    if [[ "$status" -ne 0 && "$pin_started" -eq 1 ]]; then
        git restore --staged -- ci/gates.toml 2>/dev/null || true
        cp "$temp/gates.toml" ci/gates.toml || true
    fi
    rm -rf "$temp"
}
trap cleanup EXIT
mkdir "$temp/assets"
gh release download "$tag" -R "$repo" --dir "$temp/assets"
cp ci/gates.toml "$temp/gates.toml"
pin_started=1
for arch in arm64 amd64; do
    cargo xtask fixtures pin --arch "$arch" --repo "$repo" --tag "$tag" --assets "$temp/assets"
done
cargo xtask gates check
git diff --check
changes=$(git diff --name-only)
[[ -z "$changes" || "$changes" == ci/gates.toml ]] || die "fixture pin changed unexpected files: $changes"
if git diff --quiet -- ci/gates.toml; then
    echo "fixtures-pin: pins already match $tag"
else
    git add -- ci/gates.toml
    git commit -m "Pin published guest fixtures from $tag"
fi
echo "fixtures-pin: both architectures are pinned and the checkout is ready for make release-doctor"
