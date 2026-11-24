#!/usr/bin/env bash
set -euo pipefail
test "$(uname -s)" = Darwin && test "$(uname -m)" = arm64 || {
  echo 'apple-prerelease requires native Apple Silicon macOS' >&2
  exit 1
}
command -v python3 >/dev/null || { echo 'python3 is required on the release Mac' >&2; exit 1; }
# shellcheck source=tools/prerelease/branch.sh
source "$(dirname "$0")/branch.sh"
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
work=
ci_evidence=
started=
cleanup() {
  result=$?
  if test -n "$work"; then rm -rf -- "$work"; fi
  if test -n "$ci_evidence"; then rm -rf -- "$ci_evidence"; fi
  if test -n "$started"; then rm -f -- "$started"; fi
  exit "$result"
}
trap cleanup EXIT
cargo xtask gates check
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
ci_evidence=$(mktemp -d "$root/target/prerelease/ci.XXXXXXXX")
main_run=$(bash "$root/tools/prerelease/main-ci.sh" "$repo" "$commit" "$ci_evidence")
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
