#!/usr/bin/env bash
# Publish the reviewed guest recipe, pin both archives and this Mac's OS build.
set -euo pipefail
root=$(git rev-parse --show-toplevel)
cd "$root"
mac_build=$(sw_vers -buildVersion)
expected_image="macos-$mac_build"
actual_image=$(awk '/^\[platforms.macos-arm64-ci-guest\]$/{inside=1;next} inside && /^\[/{exit} inside && /^image = /{gsub(/"/, "", $3); print $3; exit}' ci/gates.toml)
fixture_pending=$(awk '/^\[fixtures.guest-(oci|docker)-base\]$/{inside=1;next} inside && /^\[/{inside=0} inside && /^pending = /{print "yes"; exit}' ci/gates.toml)
gh auth status >/dev/null
repo=$(gh repo view --json nameWithOwner -q .nameWithOwner)
branch=$(git branch --show-current)
[[ "$branch" =~ ^release/v[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo 'Fixture pins must be committed on a release/vX.Y.Z branch' >&2
  exit 1
}
if test -z "$fixture_pending"; then
  source_for() {
    awk -v table="[fixtures.$1]" '$0 == table {inside=1;next} inside && /^\[/ {exit} inside && /^source = / {gsub(/"/, "", $3); print $3; exit}' ci/gates.toml
  }
  oci_source=$(source_for guest-oci-base)
  docker_source=$(source_for guest-docker-base)
  [[ "$oci_source" =~ ^https://github.com/$repo/releases/download/(fixtures-[0-9a-f]{40})/fixture-oci.tar$ ]] || {
    echo 'OCI fixture pin is missing its immutable release source' >&2
    exit 1
  }
  existing_tag=${BASH_REMATCH[1]}
  test "$docker_source" = "https://github.com/$repo/releases/download/$existing_tag/fixture-docker.tar" || {
    echo 'Guest fixture pins come from different releases' >&2
    exit 1
  }
  existing_assets="$root/target/prerelease/$existing_tag"
  mkdir -p "$existing_assets"
  gh release download "$existing_tag" --repo "$repo" --dir "$existing_assets" \
    --pattern base-image.txt --pattern provenance.json --clobber
  if ! cmp -s ci/fixture-base-image.txt "$existing_assets/base-image.txt"; then
    fixture_pending=yes
  fi
  if ! python3 - "$existing_assets/provenance.json" <<'PY'
import hashlib, json, pathlib, sys
record = json.load(open(sys.argv[1]))
for key, path in (
    ("recipe_sha256", "xtask/src/fixtures/recipes/guest_image.rs"),
    ("image_contract_sha256", "tests/guest/support/image-contract.sh"),
):
    if record.get(key) != hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest():
        raise SystemExit(1)
PY
  then
    fixture_pending=yes
  fi
  if test "$actual_image" = "$expected_image" && test -z "$fixture_pending"; then exit 0; fi
fi
source_commit=$(git rev-parse HEAD)
tag="fixtures-$source_commit"
assets="$root/target/prerelease/$tag"
if test -n "$fixture_pending" && ! gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then
  before=$(gh run list --repo "$repo" --workflow fixtures.yml --branch "$branch" --limit 1 --json databaseId --jq '.[0].databaseId // 0')
  gh workflow run fixtures.yml --repo "$repo" --ref "$branch" -f "tag=$tag"
  run_id=
  for _ in $(seq 1 60); do
    run_id=$(gh run list --repo "$repo" --workflow fixtures.yml --branch "$branch" --limit 10 \
      --json databaseId,headSha \
      --jq "[.[] | select(.databaseId > $before and .headSha == \"$source_commit\") | .databaseId] | max // empty")
    test -z "$run_id" || break
    sleep 5
  done
  test -n "$run_id" || { echo 'Fixtures workflow did not appear within five minutes' >&2; exit 1; }
  gh run watch "$run_id" --repo "$repo" --exit-status --interval 60
fi
if test -n "$fixture_pending"; then
  published_commit=$(gh release view "$tag" --repo "$repo" --json targetCommitish --jq .targetCommitish)
  test "$published_commit" = "$source_commit" || {
    echo 'Fixture release was built from a different release-branch commit' >&2
    exit 1
  }
  mkdir -p "$assets"
  gh release download "$tag" --repo "$repo" --dir "$assets" --clobber
  for asset in fixture-oci.tar fixture-docker.tar provenance.json base-image.txt; do
    gh attestation verify "$assets/$asset" --repo "$repo" \
      --signer-workflow "$repo/.github/workflows/fixtures.yml" \
      --source-digest "$source_commit" --source-ref "refs/heads/$branch" \
      --deny-self-hosted-runners >/dev/null
  done
  python3 tools/prerelease/pin-inputs.py "$repo" "$tag" "$assets" "$mac_build"
else
  python3 tools/prerelease/pin-mac-image.py "$mac_build"
fi
cargo xtask gates check
cargo xtask fixtures fetch
cargo xtask fixtures verify
if git diff --quiet -- ci/gates.toml; then
  exit 0
fi
git add -- ci/gates.toml
git commit -m "Pin prerelease guest and Mac inputs"
git push -u origin "$branch"
