#!/usr/bin/env bash
# scripts/release.sh — idempotent release script for awman
# Usage: scripts/release.sh v1.2.3
#
# Each step detects whether it has already been completed, so the script can
# be re-run after a failure without repeating work.

set -euo pipefail

# ── Colours ───────────────────────────────────────────────────────────────────

# ANSI-C quoted so `read -p` prompts show colour too, not a literal \033.
RED=$'\033[0;31m'
GREEN=$'\033[0;32m'
YELLOW=$'\033[1;33m'
BLUE=$'\033[0;34m'
BOLD=$'\033[1m'
NC=$'\033[0m'

step() { echo -e "\n${BLUE}${BOLD}==>${NC}${BOLD} $*${NC}"; }
ok()   { echo -e "  ${GREEN}✓${NC} $*"; }
warn() { echo -e "  ${YELLOW}!${NC} $*"; }
die()  { echo -e "\n${RED}${BOLD}Error:${NC} $*" >&2; exit 1; }

# Ask a yes/no question. Default answer is shown in caps.
# Usage: ask_proceed "message" [y|n]  (default: n)
# Returns 0 if user says yes, exits if no.
ask_proceed() {
  local msg="$1"
  local default="${2:-n}"
  local prompt
  if [[ "$default" == "y" ]]; then
    prompt="[Y/n]"
  else
    prompt="[y/N]"
  fi
  echo ""
  read -r -p "  ${YELLOW}?${NC} $msg $prompt " REPLY < /dev/tty
  REPLY="${REPLY:-$default}"
  if [[ "$REPLY" =~ ^[Yy]$ ]]; then
    return 0
  else
    echo "  Aborting." >&2
    exit 1
  fi
}

# Like ask_proceed, but returns 1 on "no" instead of exiting.
ask_yes_no() {
  local default="${2:-n}" prompt="[y/N]"
  [[ "$default" == "y" ]] && prompt="[Y/n]"
  read -r -p "  ${YELLOW}?${NC} $1 $prompt " REPLY < /dev/tty
  [[ "${REPLY:-$default}" =~ ^[Yy]$ ]]
}

# ── Args ──────────────────────────────────────────────────────────────────────

VERSION="${1:-}"

if [ -z "$VERSION" ]; then
  echo "Usage: $0 <version>   (e.g. $0 v1.2.3)" >&2
  exit 1
fi

# Plain vX.Y.Z only: release.yml runs on tags matching v[0-9]+.[0-9]+.[0-9]+, so
# a pre-release tag (v1.2.3-rc1) would never start the Release workflow.
if [[ ! "$VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  die "Version must match vX.Y.Z (no pre-release suffix), got: $VERSION"
fi

echo -e "\n${BOLD}Releasing awman ${VERSION}${NC}"

# ── PRE-CHECKS ────────────────────────────────────────────────────────────────

step "Pre-checks"

# gh auth
if ! gh auth status &>/dev/null; then
  die "Not logged in to GitHub. Run: gh auth login"
fi
ok "gh: authenticated"

# on main
BRANCH=$(git rev-parse --abbrev-ref HEAD)
if [ "$BRANCH" != "main" ]; then
  warn "Currently on branch '$BRANCH', not 'main'."
  ask_proceed "Proceed with release from '$BRANCH'?" n
else
  ok "Branch: main"
fi

# up to date with origin
git fetch origin main --quiet
LOCAL=$(git rev-parse HEAD)
REMOTE=$(git rev-parse origin/main)
if [ "$LOCAL" != "$REMOTE" ]; then
  warn "Local branch is not in sync with origin/main."
  git status -sb | head -5
  ask_proceed "Proceed anyway (you may be ahead, or behind)?" n
else
  ok "Up to date with origin/main"
fi

# clean working tree — allow only files this script manages
DIRTY=$(git status --porcelain | grep -Ev "^.. (Cargo\.(toml|lock)|docs/releases/)" || true)
if [ -n "$DIRTY" ]; then
  warn "Working tree has unexpected changes:"
  echo "$DIRTY" | sed 's/^/    /'
  ask_proceed "Proceed anyway? (consider committing or stashing these first)" n
else
  ok "Working tree clean (or only release-managed files pending)"
fi

# tag on remote
if git ls-remote --tags origin "refs/tags/$VERSION" | grep -q "$VERSION"; then
  warn "Tag $VERSION already exists on origin."
  ask_proceed "Proceed anyway? (its Release workflow run will be checked, and re-run if it failed)" y
else
  ok "Tag $VERSION not yet on remote"
fi

# tag may already exist locally from a prior interrupted run — that's OK
if git tag -l "$VERSION" | grep -q "$VERSION"; then
  warn "Tag $VERSION already exists locally (prior run). Will push it."
fi

# Release runner. The Release workflow's Apple Silicon job needs a runner with
# Hypervisor.framework, which no hosted runner offers. RELEASE_RUNNER=auto (the
# default) registers this Mac as a temporary runner when no such runner is
# online; local always uses this Mac; none leaves the job to a runner you run.
REPO=$(gh repo view --json nameWithOwner -q .nameWithOwner)
RELEASE_RUNNER="${RELEASE_RUNNER:-auto}"
case "$RELEASE_RUNNER" in
  auto|local|none) ;;
  *) die "RELEASE_RUNNER must be auto, local or none, got: $RELEASE_RUNNER" ;;
esac
USE_LOCAL_RUNNER=false
if [ "$RELEASE_RUNNER" = none ]; then
  ok "Release runner: not managed (RELEASE_RUNNER=none)"
else
  EXISTING=$(bash tools/release-runner.sh busy "$REPO")
  if [ -n "$EXISTING" ] && [ "$RELEASE_RUNNER" = auto ]; then
    ok "Release runner: online runner(s) will take the Apple Silicon job: $(echo $EXISTING)"
  elif bash tools/release-runner.sh check --hardware 2>/dev/null; then
    # Install or refresh the job's /opt/awman scripts and guest fixture, so a
    # release never depends on remembering to provision this Mac.
    bash tools/release-runner.sh provision ||
      die "Could not set up this Mac's release scripts (see above). Fix that, or use RELEASE_RUNNER=none."
    RUNNER_CHECK=$(bash tools/release-runner.sh check 2>&1) ||
      die "This Mac is still missing release prerequisites:\n$RUNNER_CHECK"
    USE_LOCAL_RUNNER=true
    ok "Release runner: this Mac will run the Apple Silicon job, then deregister"
  else
    RUNNER_CHECK=$(bash tools/release-runner.sh check --hardware 2>&1 || true)
    [ "$RELEASE_RUNNER" != local ] || die "This host cannot run the Apple Silicon release job:\n$RUNNER_CHECK"
    warn "No hypervisor runner is online, and this host cannot be one:"
    echo "$RUNNER_CHECK" | sed 's/^/    /'
    ask_proceed "Continue? (the Apple Silicon job will wait for a runner, and nothing publishes until it passes)" n
  fi
fi

ok "Pre-checks done"

# ── STEP 1: Version bump ──────────────────────────────────────────────────────

# Strip the leading 'v' — Cargo.toml uses bare semver (e.g. 1.2.3).
BARE_VERSION="${VERSION#v}"

step "Version in Cargo.toml"

CURRENT_CARGO_VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*= *"\(.*\)"/\1/')

if [ "$CURRENT_CARGO_VERSION" = "$BARE_VERSION" ]; then
  ok "Cargo.toml already at $BARE_VERSION"
else
  # Sanity-check that `awman --version` still reports the old value (not some
  # newer version that disagrees with Cargo.toml).
  if command -v awman &>/dev/null; then
    BINARY_VERSION=$(awman --version 2>/dev/null | awk '{print $NF}' || true)
    if [ -n "$BINARY_VERSION" ] && [ "$BINARY_VERSION" != "$CURRENT_CARGO_VERSION" ]; then
      warn "Installed awman reports version $BINARY_VERSION but Cargo.toml says $CURRENT_CARGO_VERSION."
    fi
  fi

  # Bump the version field in Cargo.toml (first occurrence only). awk, not
  # sed's GNU-only `0,/re/` address, which BSD sed on macOS rejects.
  awk -v old="version = \"${CURRENT_CARGO_VERSION}\"" -v new="version = \"${BARE_VERSION}\"" \
    '!done && $0 == old { $0 = new; done = 1 } { print }' Cargo.toml > Cargo.toml.tmp
  mv Cargo.toml.tmp Cargo.toml

  # Refresh only awman's own entry in Cargo.lock; leave dependencies pinned.
  cargo update --workspace --quiet

  ok "Bumped Cargo.toml: $CURRENT_CARGO_VERSION → $BARE_VERSION"
fi

# Verify the version is correct in Cargo.toml.
VERIFIED_VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*= *"\(.*\)"/\1/')
if [ "$VERIFIED_VERSION" != "$BARE_VERSION" ]; then
  die "Cargo.toml version is '$VERIFIED_VERSION' but expected '$BARE_VERSION'. Check the file manually."
fi

# ── STEP 2: Release notes ─────────────────────────────────────────────────────

NOTES_FILE="docs/releases/${VERSION}.md"
TEMPLATE_MARKER="_Write release notes here._"

step "Release notes"

if [ ! -f "$NOTES_FILE" ]; then
  mkdir -p docs/releases
  cat > "$NOTES_FILE" <<EOF
# Release ${VERSION}

## Changes

${TEMPLATE_MARKER}
EOF
  ok "Created $NOTES_FILE"
fi

# If notes still contain the placeholder, prompt the user to fill them in.
if grep -qF "$TEMPLATE_MARKER" "$NOTES_FILE"; then
  warn "$NOTES_FILE still contains the placeholder."
  echo ""
  echo "  Edit $NOTES_FILE with the release notes, then re-run this script."
  echo ""
  read -r -p "  Launch 'awman chat' to write release notes now? [Y/n] " REPLY < /dev/tty
  REPLY="${REPLY:-Y}"
  if [[ "$REPLY" =~ ^[Yy]$ ]]; then
    awman chat
    # After chat exits, check again
    if grep -qF "$TEMPLATE_MARKER" "$NOTES_FILE"; then
      die "Release notes still contain the placeholder. Edit $NOTES_FILE and re-run."
    fi
  else
    die "Edit $NOTES_FILE and re-run this script."
  fi
fi

ok "Release notes ready"

# ── STEP 3: Tests ─────────────────────────────────────────────────────────────

# Sentinel lives inside .git/ so it is never tracked or committed.
TESTS_SENTINEL=".git/.release-tests-passed-${VERSION}"

step "Tests"

if [ -f "$TESTS_SENTINEL" ]; then
  ok "Tests already passed (sentinel present)"
else
  TEST_LOG=$(mktemp)
  # tee so the user sees output in real-time; pipefail captures cargo's exit code.
  if cargo test 2>&1 | tee "$TEST_LOG"; then
    touch "$TESTS_SENTINEL"
    rm -f "$TEST_LOG"
    ok "All tests passed"
  else
    echo ""
    echo -e "${RED}${BOLD}Tests failed.${NC}"
    echo ""

    # Copy failure output to clipboard.
    CLIPPED=false
    if command -v pbcopy &>/dev/null; then
      cat "$TEST_LOG" | pbcopy && CLIPPED=true
    elif command -v xclip &>/dev/null; then
      cat "$TEST_LOG" | xclip -selection clipboard && CLIPPED=true
    elif command -v xsel &>/dev/null; then
      cat "$TEST_LOG" | xsel --clipboard --input && CLIPPED=true
    fi

    if $CLIPPED; then
      ok "Test output copied to clipboard — paste it into the chat."
    else
      warn "Could not copy to clipboard (install pbcopy, xclip, or xsel)."
    fi

    rm -f "$TEST_LOG"

    echo ""
    read -r -p "  Skip tests and continue the release anyway? [y/N] " REPLY < /dev/tty
    REPLY="${REPLY:-N}"
    if [[ "$REPLY" =~ ^[Yy]$ ]]; then
      warn "Skipping tests at user request — proceeding with release."
      touch "$TESTS_SENTINEL"
    else
      echo ""
      read -r -p "  Launch 'awman chat' to fix the failing tests? [Y/n] " REPLY < /dev/tty
      REPLY="${REPLY:-Y}"
      if [[ "$REPLY" =~ ^[Yy]$ ]]; then
        awman chat
        die "Re-run this script after verifying the tests pass."
      else
        die "Fix the tests and re-run this script."
      fi
    fi
  fi
fi

# ── STEP 4: Commit ────────────────────────────────────────────────────────────

step "Commit"

# Check whether there are any release-related changes that need committing.
PENDING=$(git status --porcelain -- Cargo.toml Cargo.lock "$NOTES_FILE" 2>/dev/null || true)

if [ -z "$PENDING" ]; then
  ok "Nothing to commit (release files already committed)"
else
  git add "$NOTES_FILE" Cargo.toml Cargo.lock
  git commit -m "Add release notes for ${VERSION}"
  ok "Committed: Add release notes for ${VERSION}"
fi

# ── STEP 5: Tag ───────────────────────────────────────────────────────────────

step "Tag"

if git tag -l "$VERSION" | grep -q "$VERSION"; then
  ok "Tag $VERSION already exists locally"
else
  git tag "$VERSION"
  ok "Created tag $VERSION"
fi

# ── STEP 6: Release runner ────────────────────────────────────────────────────
# Started before the tag is pushed, so a runner that cannot register stops the
# release before anything is published. The EXIT trap stops and deregisters it
# however the script ends — success, failure or Ctrl-C.

if $USE_LOCAL_RUNNER; then
  step "Release runner"

  # Stale scheduled hardware runs queue for the same labels and would take the
  # runner first, delaying the release job by hours. That is any Tests run,
  # queued or already in progress (its hosted jobs started), with a
  # hypervisor-labelled job still waiting for a runner. One the nightly cron
  # queues later in the release fails its first step on this runner
  # (test.yml refuses `awman-release-*` runners), freeing it in seconds.
  STALE=""
  for status in queued in_progress; do
    for id in $(gh run list --repo "$REPO" --workflow test.yml --status "$status" --limit 50 \
        --json databaseId -q '.[].databaseId'); do
      WAITING=$(gh api "repos/$REPO/actions/runs/$id/jobs" --paginate \
        -q '.jobs[] | select(.status != "completed" and .status != "in_progress") | select(.labels | index("hypervisor")) | .id')
      [ -z "$WAITING" ] || STALE="$STALE $id"
    done
  done
  if [ -n "$STALE" ]; then
    warn "$(echo "$STALE" | wc -w | tr -d ' ') Tests run(s) waiting for a hypervisor runner would take this runner before the release job."
    if ask_yes_no "Cancel them?" y; then
      for id in $STALE; do gh run cancel "$id" --repo "$REPO" >/dev/null; done
      ok "Cancelled those Tests runs"
    fi
  fi

  # A second Ctrl-C must not interrupt the teardown and leave the runner
  # registered, so `down` (and what it runs) ignores INT and TERM.
  trap 'trap "" INT TERM; bash tools/release-runner.sh down' EXIT
  trap 'exit 130' INT TERM
  bash tools/release-runner.sh up "$REPO" "$VERSION"
fi

# ── STEP 7: Push ──────────────────────────────────────────────────────────────

step "Push"

AHEAD=$(git rev-list origin/main..HEAD --count 2>/dev/null || echo 0)
if [ "$AHEAD" -gt 0 ]; then
  git push origin main
  ok "Pushed commits to origin/main"
else
  ok "Commits already on origin/main"
fi

if git ls-remote --tags origin "refs/tags/$VERSION" | grep -q "$VERSION"; then
  ok "Tag $VERSION already on origin"
else
  git push origin "$VERSION"
  ok "Pushed tag $VERSION"
fi

# ── STEP 8: Release workflow ──────────────────────────────────────────────────
# The release workflow (.github/workflows/release.yml) creates the GitHub
# release from $NOTES_FILE once every binary is built and the macOS builtin
# boot gate passes. Creating it here would publish a release with no assets.

step "Release workflow"

if ! $USE_LOCAL_RUNNER; then
  ok "Tag pushed; the Release workflow will publish $VERSION after the build gates pass"
  echo "  Follow it with: gh run watch \$(gh run list --workflow release.yml --limit 1 --json databaseId -q '.[0].databaseId')"
  echo ""
  echo -e "${GREEN}${BOLD}Release ${VERSION} tagged!${NC}"
  echo ""
  exit 0
fi

# A tag push's run reports the tag as its head branch.
RUN_ID=""
for _ in $(seq 1 60); do
  RUN_ID=$(gh run list --repo "$REPO" --workflow release.yml --event push --limit 20 \
    --json databaseId,headBranch -q ".[] | select(.headBranch == \"$VERSION\") | .databaseId" | head -n1)
  [ -n "$RUN_ID" ] && break
  sleep 5
done
[ -n "$RUN_ID" ] || die "No Release workflow run appeared for $VERSION within 5 minutes. Check the repository's Actions tab."

CONCLUSION=$(gh run view "$RUN_ID" --repo "$REPO" --json status,conclusion \
  -q 'if .status == "completed" then .conclusion else "" end')
if [ "$CONCLUSION" = success ]; then
  ok "Release workflow run $RUN_ID already succeeded"
else
  if [ -n "$CONCLUSION" ]; then
    warn "Release workflow run $RUN_ID finished earlier: $CONCLUSION"
    ask_proceed "Re-run its failed jobs on this Mac?" y
    gh run rerun "$RUN_ID" --repo "$REPO" --failed
    sleep 5
  fi
  ok "Watching run $RUN_ID; this Mac serves the Apple Silicon job (up to 3 hours)"
  if ! gh run watch "$RUN_ID" --repo "$REPO" --exit-status --interval 60; then
    die "The Release workflow failed. See: gh run view $RUN_ID --repo $REPO --log-failed\n  Re-run this script to retry the failed jobs."
  fi
fi

# ── Done ──────────────────────────────────────────────────────────────────────
# The EXIT trap now stops and deregisters the runner.

echo ""
echo -e "${GREEN}${BOLD}Release ${VERSION} published!${NC}"
echo ""
