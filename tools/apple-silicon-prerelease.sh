#!/usr/bin/env bash
# Run the local and native Apple Silicon gates before creating a release tag.
# This never registers a runner, pushes a tag, or publishes a release.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$root"

step() { printf '\n==> %s\n' "$*"; }
die() { echo "apple-silicon-prerelease: $*" >&2; exit 1; }

step "Apple Silicon host"
bash tools/release-runner.sh check --hardware || die "an Apple Silicon Mac with Hypervisor.framework is required"
command -v cargo >/dev/null 2>&1 || die "cargo is required"
command -v gh >/dev/null 2>&1 || die "gh is required"
command -v container >/dev/null 2>&1 || die "Apple Containers is required for the real guest suite"
test "${AWMAN_RELEASE_OPT_DIR:-/opt/awman}" = /opt/awman ||
  die "the Release workflow requires /opt/awman; unset AWMAN_RELEASE_OPT_DIR"
for path_var in AWMAN_RELEASE_RUNNER_HOME AWMAN_TEST_BUILTIN_BASE_TAR AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE; do
  path_value=${!path_var:-}
  case "$path_value" in ""|/*) ;; *) die "$path_var must be an absolute path" ;; esac
done

step "Release source and GitHub CI"
test "$(git branch --show-current)" = main || die "run from main"
test -z "$(git status --porcelain --untracked-files=all)" || {
  git status --short >&2
  die "commit or set aside working-tree changes before running this gate"
}
git fetch origin main --tags --quiet
test "$(git rev-parse --is-shallow-repository)" = false ||
  die "the upgrade checks need full Git history; unshallow this checkout first"
head=$(git rev-parse HEAD)
test "$head" = "$(git rev-parse origin/main)" || die "main is not in sync with origin/main"

version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' Cargo.toml | head -n1)
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "Cargo.toml needs a plain X.Y.Z version"
tag="v$version"
notes="docs/releases/$tag.md"
test -s "$notes" || die "$notes is missing or empty"
! grep -qF '_Write release notes here._' "$notes" || die "$notes still has its placeholder"
test -z "$(git tag -l "$tag")" || die "$tag already exists locally"
remote_tag=$(git ls-remote --tags origin "refs/tags/$tag") || die "could not check the remote tag"
test -z "$remote_tag" || die "$tag already exists on origin"

gh auth status || die "gh must be authenticated"
repo=$(gh repo view --json nameWithOwner -q .nameWithOwner) || die "could not identify the GitHub repository"
# Runner listing needs the repository admin access that registration also needs.
online_runners=$(bash tools/release-runner.sh busy "$repo") || die "could not list hypervisor runners"
printf 'Repository: %s\nVersion: %s\nOnline hypervisor runners: %s\n' \
  "$repo" "$tag" "${online_runners:-none}"
bash tools/release-runner.sh verify-archive
ci_run=""
for _ in $(seq 1 60); do
  ci_run=$(gh run list --repo "$repo" --workflow test.yml --commit "$head" \
    --event push --limit 20 --json databaseId --jq '.[0].databaseId // ""') ||
    die "could not find Tests CI for $head"
  [ -z "$ci_run" ] || break
  sleep 5
done
test -n "$ci_run" || die "no Tests push run appeared for $head within 5 minutes"
step "Tests CI run $ci_run"
gh run watch "$ci_run" --repo "$repo" --exit-status --interval 60 ||
  die "Tests CI run $ci_run did not succeed"

evidence=$(mktemp -d /tmp/awman-prerelease.XXXXXX)
printf 'Evidence: %s\n' "$evidence"
source_dir="$evidence/source"
git worktree add --detach "$source_dir" "$head"
cleanup() {
  local result=$?
  trap - EXIT
  if [ -f "$source_dir/target/builtin-hw-report.tsv" ]; then
    cp "$source_dir/target/builtin-hw-report.tsv" "$evidence/" || true
  fi
  if [ -d "$source_dir/target/build-input-audit" ]; then
    cp -R "$source_dir/target/build-input-audit" "$evidence/" || true
  fi
  if ! cd "$root"; then
    echo "apple-silicon-prerelease: could not return to $root for worktree cleanup" >&2
    [ "$result" -ne 0 ] || result=1
    exit "$result"
  fi
  if ! git worktree remove --force "$source_dir"; then
    echo "apple-silicon-prerelease: remove stale worktree $source_dir manually" >&2
    [ "$result" -ne 0 ] || result=1
  fi
  if [ "$result" -eq 0 ]; then
    step "PASS: $tag Apple Silicon prerelease gates"
    printf 'Evidence: %s\n' "$evidence"
  fi
  exit "$result"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
cd "$source_dir"

step "Local pre-push gate"
make --no-print-directory pre-push

step "Release runner provisioning"
container system status >/dev/null 2>&1 || container system start
bash tools/release-runner.sh provision
bash tools/release-runner.sh check
runner_home=${AWMAN_RELEASE_RUNNER_HOME:-$HOME/.awman/release-runner}
base=${AWMAN_TEST_BUILTIN_BASE_TAR:-$runner_home/fixtures/base.tar}
fixture=${AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE:-$runner_home/fixtures/fixture-oci.tar}
test -s "$base" && test -s "$fixture" || die "provisioning did not leave a cached guest fixture"

step "Clean native provider build and guest tests"
mkdir -p "$evidence/native"
RUNNER_TEMP="$evidence/native" AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE="$fixture" \
  bash tools/native-builtin-ci.sh aarch64-apple-darwin

step "Real guest hardware suite"
AWMAN_TEST_BUILTIN_BASE_TAR="$base" AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE="$fixture" \
  make --no-print-directory test-builtin-hw

step "SQLite native compatibility gate"
SQLITE_SPIKE_OUTPUT_DIR="$evidence/sqlite" \
  bash tools/oci-runtime-spike/sqlite-resolution/checks.sh auto

step "Exact Apple Silicon release artifact"
bash tools/msb-payloads/fetch.sh aarch64-apple-darwin
bash tools/msb-payloads/verify.sh aarch64-apple-darwin
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR="$evidence/target" \
  cargo build --workspace --locked --release --features builtin-runtime \
    --target aarch64-apple-darwin --bin awman
artifact="$evidence/awman-macos-arm64"
cp "$evidence/target/aarch64-apple-darwin/release/awman" "$artifact"
shasum -a 256 "$artifact" > "$evidence/awman.unsigned.sha256"
bash tools/macos-sign.sh "$artifact"
shasum -a 256 "$artifact" > "$evidence/awman.sha256"
codesign -d --entitlements - --xml "$artifact" > "$evidence/awman.entitlements.plist"
bash tools/release-artifact-check.sh "$artifact" aarch64-apple-darwin \
  "$evidence/artifact-check" /opt/awman/release-offline-session.sh
bash tools/measure-release-artifact.sh "$artifact" aarch64-apple-darwin \
  "$evidence/measurements" /opt/awman/release-workload.sh
