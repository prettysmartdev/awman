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
# A post-merge candidate failure can require a follow-up PR from this branch.
# Select only an open PR; a prior merged PR is not the current release review.
open_prs=$(gh pr list --repo "$repo" --state open --base main --head "$branch" --json number --jq length)
if test "$open_prs" -eq 0; then
  gh pr create --repo "$repo" --base main --head "$branch" \
    --title "Release v$version" --body-file "docs/releases/v$version.md"
fi
pr_number=$(gh pr list --repo "$repo" --state open --base main --head "$branch" --json number \
  --jq 'if length == 1 then .[0].number else empty end')
test -n "$pr_number" || { echo 'Expected exactly one open release PR' >&2; exit 1; }
test "$(gh pr view "$pr_number" --repo "$repo" --json state,baseRefName,headRefOid,isCrossRepository \
  --jq 'select(.state == "OPEN" and .baseRefName == "main" and .isCrossRepository == false) | .headRefOid')" = "$commit" || {
  echo 'The open release PR must target main at the exact prerelease commit' >&2
  exit 1
}
marker="$root/target/prerelease/apple-$commit"
if test -f "$marker/evidence/packet.json" &&
  cargo xtask native-validation verify "$marker/evidence"; then
  echo "Reusing verified Apple native packet for $commit" >&2
else
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
  if ! python3 "$root/tools/prerelease/extract-packet.py" "$latest" "$work"; then
    # Packaging metadata must not force another successful native run. The
    # runner keeps the original evidence next to its archive; verify those
    # bytes independently before using them for this exact release commit.
    echo 'Archive extraction failed; checking saved native evidence' >&2
    native_evidence="${latest%.tar.gz}/evidence"
    cargo xtask native-validation verify "$native_evidence"
    rm -rf -- "$work/evidence"
    cp -R "$native_evidence" "$work/evidence"
  fi
  cargo xtask native-validation verify "$work/evidence"
  mkdir -p "$marker"
  if test -e "$marker/evidence"; then
    mv -- "$marker/evidence" "$marker/evidence.previous.$(date +%s).$$"
  fi
  mv -- "$work/evidence" "$marker/evidence"
fi
pin_for() {
  awk -v table="[fixtures.$1]" '$0 == table {inside=1;next} inside && /^\[/ {exit} inside && /^sha256 = / {gsub(/"/, "", $3); print $3; exit}' ci/gates.toml
}
native_oci=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["fixture_sha256"])' "$marker/evidence/packet.json")
native_docker=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["docker_fixture_sha256"])' "$marker/evidence/packet.json")
test "$native_oci" = "$(pin_for guest-oci-base)" &&
  test "$native_docker" = "$(pin_for guest-docker-base)" || {
    echo 'Native Apple fixture bytes differ from the published manifest pins' >&2
    exit 1
  }
test "$(git ls-remote origin "refs/heads/$branch" | cut -f 1)" = "$commit" || {
  echo 'The release branch changed during native validation; rerun both prerelease targets' >&2
  exit 1
}
# Native failures need a fresh source revision; do not make the operator wait
# for a full main gate before discovering one. An existing passing gate is
# still authenticated and reused for this exact commit.
ci_evidence=$(mktemp -d "$root/target/prerelease/ci.XXXXXXXX")
main_run=$(bash "$root/tools/prerelease/main-ci.sh" "$repo" "$commit" "$ci_evidence")
printf '%s\n' "$commit" > "$marker/commit"
printf '%s\n' "$main_run" > "$marker/main-ci-run"
printf '%s\n' "$pr_number" > "$marker/pr-number"
cp "$ci_evidence/gate-evidence.json" "$marker/main-gate-evidence.json"
echo "Apple native acceptance and the full main CI gate passed for $commit."
