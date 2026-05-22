#!/usr/bin/env bash
# scripts/release.sh — idempotent release script for awman
# Usage: scripts/release.sh v1.2.3
#
# Each step detects whether it has already been completed, so the script can
# be re-run after a failure without repeating work.

set -euo pipefail

# ── Colours ───────────────────────────────────────────────────────────────────

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

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

# ── Args ──────────────────────────────────────────────────────────────────────

VERSION="${1:-}"

if [ -z "$VERSION" ]; then
  echo "Usage: $0 <version>   (e.g. $0 v1.2.3)" >&2
  exit 1
fi

if [[ ! "$VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9._-]+)?$ ]]; then
  die "Version must match vX.Y.Z format, got: $VERSION"
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
MANAGED_FILES="Cargo.toml Cargo.lock docs/releases/"
DIRTY=$(git status --porcelain | grep -v "^.. Cargo\.\(toml\|lock\)" | grep -v "^.. docs/releases/" || true)
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
  ask_proceed "Proceed anyway? (GitHub release creation will be skipped if it exists)" y
else
  ok "Tag $VERSION not yet on remote"
fi

# tag may already exist locally from a prior interrupted run — that's OK
if git tag -l "$VERSION" | grep -q "$VERSION"; then
  warn "Tag $VERSION already exists locally (prior run). Will push it."
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

  # Bump the version field in Cargo.toml (first occurrence only).
  sed -i.bak "0,/^version = \"${CURRENT_CARGO_VERSION}\"/{s/^version = \"${CURRENT_CARGO_VERSION}\"/version = \"${BARE_VERSION}\"/}" Cargo.toml
  rm -f Cargo.toml.bak

  # Regenerate Cargo.lock.
  cargo generate-lockfile --quiet

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

# ── STEP 6: Push ──────────────────────────────────────────────────────────────

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

# ── STEP 7: GitHub release ────────────────────────────────────────────────────

step "GitHub release"

if gh release view "$VERSION" &>/dev/null; then
  ok "GitHub release $VERSION already exists"
else
  gh release create "$VERSION" --title "$VERSION" --notes-file "$NOTES_FILE"
  ok "Created GitHub release $VERSION"
fi

# ── Done ──────────────────────────────────────────────────────────────────────

echo ""
echo -e "${GREEN}${BOLD}Release ${VERSION} complete!${NC}"
echo ""
