#!/usr/bin/env bash
# Pipeline operator: stable release operator prompts and resume behavior.
# Usage: cargo xtask release run v1.2.3
#
# Each step detects whether it has already been completed, so the script can
# be re-run after a failure without repeating work.

set -Eeuo pipefail
trap 'exit 130' INT TERM

XTASK="${AWMAN_RELEASE_XTASK:?run through cargo xtask release run}"
internal() { "$XTASK" release run "$@"; }
runner() { "$XTASK" release runner "$@"; }
release_cleanup() {
  local release_exit=$?
  trap '' INT TERM
  if ! runner down; then
    [ "$release_exit" -ne 0 ] || release_exit=1
  fi
  exit "$release_exit"
}
answer() {
  internal --internal-answer "$1" "$2"
  if [ -n "${RUN_ID:-}" ]; then
    local journal
    journal=$(internal --internal-journal-payload "$CANDIDATE_COMMIT" "$REPO" "$VERSION")
    gh workflow run candidate.yml --repo "$REPO" --ref main -f "version=$VERSION" -f "commit=$CANDIDATE_COMMIT" -f promote=false -f "operator_record=$journal" > /dev/null || die "Could not record the operator answer on GitHub. Re-run this script."
  fi
}
prompt() {
  printf '%s' "$1" > /dev/tty || die "Cannot open /dev/tty for release prompts."
  IFS= read -r REPLY < /dev/tty || die "Cannot read release answer from /dev/tty (EOF)."
}


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

trap 'die "Release step failed. Fix the error above and re-run this script."' ERR

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
  prompt "  ${YELLOW}?${NC} $msg $prompt "
  REPLY="${REPLY:-$default}"
  answer "$msg" "$REPLY"
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
  prompt "  ${YELLOW}?${NC} $1 $prompt "
  answer "$1" "${REPLY:-$default}"
  [[ "${REPLY:-$default}" =~ ^[Yy]$ ]]
}

# ── Args ──────────────────────────────────────────────────────────────────────

VERSION="${1:-}"

if [ -z "$VERSION" ]; then
  echo "Usage: $0 <version>   (e.g. $0 v1.2.3)" >&2
  exit 1
fi

# Plain vX.Y.Z only, preserving the operator's release version contract.
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

internal --internal-init "${RELEASE_RUNNER:-auto}" "$VERSION"
rm -f "$(git rev-parse --git-path ".release-tests-passed-$VERSION")"

# Read remote identity before prompts so answers on resume (including aborts)
# are retained as record-only dispatches. These do not allocate candidates.
REPO=$(gh repo view --json nameWithOwner -q .nameWithOwner)
STATE_LINE=$(internal --internal-state "$REPO" "$VERSION") || die "Could not read release state from GitHub."
IFS='|' read -r RELEASE_STATE CANDIDATE CANDIDATE_COMMIT RUN_ID TAG_ON_ORIGIN <<< "$STATE_LINE"

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
DIRTY=$(git status --porcelain | grep -Ev "^.. (Cargo\.(toml|lock)$|docs/releases/)" || true)
if [ -n "$DIRTY" ]; then
  warn "Working tree has unexpected changes:"
  # sed expresses multiline indentation clearly.
  # shellcheck disable=SC2001
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
STATE_LINE=$(internal --internal-state "$REPO" "$VERSION") || die "Could not read release state from GitHub."
IFS='|' read -r RELEASE_STATE CANDIDATE CANDIDATE_COMMIT RUN_ID TAG_ON_ORIGIN <<< "$STATE_LINE"
if [ "$RELEASE_STATE" = verified ]; then
  ok "Release $VERSION is already published"
  exit 0
elif [ "$RELEASE_STATE" != none ] && [ "$RELEASE_STATE" != abandoned ]; then
  ok "Candidate $CANDIDATE for $VERSION exists (state $RELEASE_STATE); resuming"
fi
RELEASE_RUNNER="${RELEASE_RUNNER:-auto}"
case "$RELEASE_RUNNER" in
  auto|local|none) ;;
  *) die "RELEASE_RUNNER must be auto, local or none, got: $RELEASE_RUNNER" ;;
esac
USE_LOCAL_RUNNER=false
if [ "$RELEASE_RUNNER" = none ]; then
  ok "Release runner: not managed (RELEASE_RUNNER=none)"
else
  EXISTING=$(runner busy "$REPO")
  if [ -n "$EXISTING" ] && [ "$RELEASE_RUNNER" = auto ]; then
    ok "Release runner: online runner(s) will take the Apple Silicon job: $EXISTING"
  elif runner check --hardware 2>/dev/null; then
    # Fetch and verify manifest fixtures in the private runner directory before
    # registration; every release checks its inputs again.
    runner provision ||
      die "Could not set up this Mac's release scripts (see above). Fix that, or use RELEASE_RUNNER=none."
    RUNNER_CHECK=$(runner check 2>&1) ||
      die "This Mac is still missing release prerequisites:\n$RUNNER_CHECK"
    USE_LOCAL_RUNNER=true
    ok "Release runner: this Mac will run the Apple Silicon job, then deregister"
  else
    RUNNER_CHECK=$(runner check --hardware 2>&1 || true)
    [ "$RELEASE_RUNNER" != local ] || die "This host cannot run the Apple Silicon release job:\n$RUNNER_CHECK"
    warn "No hypervisor runner is online, and this host cannot be one:"
    # sed expresses multiline indentation clearly.
    # shellcheck disable=SC2001
    echo "$RUNNER_CHECK" | sed 's/^/    /'
    ask_proceed "Continue? (the Apple Silicon job will wait for a runner, and nothing publishes until it passes)" n
  fi
fi

ok "Pre-checks done"

# ── STEP 1: Version bump ──────────────────────────────────────────────────────

# Strip the leading 'v' — Cargo.toml uses bare semver (e.g. 1.2.3).
BARE_VERSION="${VERSION#v}"

update_workspace_lock() {
  local LOCK_BACKUP
  LOCK_BACKUP=$(mktemp)
  cp Cargo.lock "$LOCK_BACKUP"
  if ! cargo update --workspace --quiet || ! internal --internal-lock "$LOCK_BACKUP"; then
    cp "$LOCK_BACKUP" Cargo.lock
    rm -f "$LOCK_BACKUP"
    die "Could not update workspace versions without changing dependency pins."
  fi
  rm -f "$LOCK_BACKUP"

}

step "Version in Cargo.toml"

CURRENT_CARGO_VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*= *"\(.*\)"/\1/')

if [ "$CURRENT_CARGO_VERSION" = "$BARE_VERSION" ]; then
  ok "Cargo.toml already at $BARE_VERSION"
  if [ "$(internal --internal-lock-version "$BARE_VERSION")" != ready ]; then update_workspace_lock; fi
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
  update_workspace_lock

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
  PROMPT_ID=release-notes
  prompt "  Launch 'awman chat' to write release notes now? [Y/n] "
  REPLY="${REPLY:-Y}"
  answer "direct:$PROMPT_ID" "$REPLY"
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

# Local record is source-bound and is never treated as pipeline evidence.

step "Tests"

LOCAL_RESULT=$(internal --internal-local query)
REMOTE_TREE_MATCH=$(internal --internal-local remote)
if [ "$CANDIDATE_COMMIT" = "$(git rev-parse HEAD)" ] && [ -n "$RUN_ID" ] && [ "$REMOTE_TREE_MATCH" = same ]; then
  ok "Local preparation already recorded by candidate $CANDIDATE"
elif [ "$LOCAL_RESULT" = passed ]; then
  ok "Tests already passed for this source tree"
elif [ "$LOCAL_RESULT" = 'skipped by operator' ]; then
  warn "Skipping tests at user request — proceeding with release."
else
  internal --internal-local begin
  TEST_LOG=$(mktemp)
  # Use the same isolated test target as CI and pre-push, so release checks
  # never touch the developer's keychain, clipboard or per-user daemons.
  # tee keeps output visible; pipefail captures make's exit code.
  if make test 2>&1 | tee "$TEST_LOG"; then
    internal --internal-local passed
    rm -f "$TEST_LOG"
    ok "All tests passed"
  else
    echo ""
    echo -e "${RED}${BOLD}Tests failed.${NC}"
    echo ""

    # Copy failure output to clipboard.
    CLIPPED=false
    if command -v pbcopy &>/dev/null; then
      pbcopy < "$TEST_LOG" && CLIPPED=true
    elif command -v xclip &>/dev/null; then
      xclip -selection clipboard < "$TEST_LOG" && CLIPPED=true
    elif command -v xsel &>/dev/null; then
      xsel --clipboard --input < "$TEST_LOG" && CLIPPED=true
    fi

    if $CLIPPED; then
      ok "Test output copied to clipboard — paste it into the chat."
    else
      warn "Could not copy to clipboard (install pbcopy, xclip, or xsel)."
    fi

    rm -f "$TEST_LOG"

    echo ""
    PROMPT_ID=skip-local-tests
    prompt "  Skip tests and continue the release anyway? [y/N] "
    REPLY="${REPLY:-N}"
  answer "direct:$PROMPT_ID" "$REPLY"
    if [[ "$REPLY" =~ ^[Yy]$ ]]; then
      warn "Skipping tests at user request — proceeding with release."
      internal --internal-local 'skipped by operator'
    else
      echo ""
      PROMPT_ID=fix-local-tests
      prompt "  Launch 'awman chat' to fix the failing tests? [Y/n] "
      REPLY="${REPLY:-Y}"
  answer "direct:$PROMPT_ID" "$REPLY"
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
  internal --internal-commit
  git commit --only -m "Add release notes for ${VERSION}" -- "$NOTES_FILE" Cargo.toml Cargo.lock
  ok "Committed: Add release notes for ${VERSION}"
fi

internal --internal-commit

# ── STEP 5: Tag ───────────────────────────────────────────────────────────────

step "Tag"

if git tag -l "$VERSION" | grep -q "$VERSION"; then
  [ "$(git rev-parse "$VERSION^{commit}")" = "$(git rev-parse HEAD)" ] || die "Local tag $VERSION points to a different commit. Resolve it before retrying."
fi

if git tag -l "$VERSION" | grep -q "$VERSION"; then
  ok "Tag $VERSION already exists locally"
else
  git tag "$VERSION"
  ok "Created tag $VERSION"
fi

# ── STEP 6: Release runner ────────────────────────────────────────────────────
# Started before main is pushed, so a runner that cannot register stops the
# release before anything is published. The EXIT trap stops and deregisters it
# however the script ends — success, failure or Ctrl-C.

if $USE_LOCAL_RUNNER; then
  step "Release runner"

  # Only jobs waiting for the release label can take this runner. CI hardware
  # uses disjoint labels and must never be included in the cancellation offer.
  # Keep the legacy terminal text below, as required by the operator contract.
  STALE=""
  for status in queued in_progress; do
    for id in $(gh run list --repo "$REPO" --status "$status" --limit 50 \
        --json databaseId -q '.[].databaseId'); do
      if [ "$id" = "${RUN_ID:-}" ] && [ "$CANDIDATE_COMMIT" = "$(git rev-parse HEAD)" ]; then continue; fi
      case " $STALE " in *" $id "*) continue ;; esac
      WAITING=$(gh api "repos/$REPO/actions/runs/$id/jobs" --paginate \
        -q '.jobs[] | select(.status != "completed" and .status != "in_progress") | select(.labels | index("awman-release")) | .id')
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
  trap release_cleanup EXIT
  trap 'exit 130' INT TERM
  runner up "$REPO" "$VERSION"
fi

# ── STEP 7: Push ──────────────────────────────────────────────────────────────

step "Push"

AHEAD=$(git rev-list origin/main..HEAD --count 2>/dev/null || echo 0)
if [ "$AHEAD" -gt 0 ]; then
  if [ "$BRANCH" = main ]; then
    git push origin main
  else
    git push origin HEAD:main
  fi
  ok "Pushed commits to origin/main"
else
  ok "Commits already on origin/main"
fi

COMMIT=$(git rev-parse HEAD)
# query() discovers even a prepared run without an assembled draft. A matching
# run is resumed; the workflow resolves changed inputs/cancelled requests under
# its durable candidate/publication locks before allocating another identity.
STATE_LINE=$(internal --internal-state "$REPO" "$VERSION") || die "Could not read release state from GitHub."
IFS='|' read -r RELEASE_STATE CANDIDATE CANDIDATE_COMMIT RUN_ID TAG_ON_ORIGIN <<< "$STATE_LINE"
if [ "$CANDIDATE_COMMIT" != "$COMMIT" ] || [ -z "$RUN_ID" ] || [ "$RELEASE_STATE" = abandoned ]; then
  OPERATOR_RECORD=$(internal --internal-payload "$COMMIT" "$REPO" "$VERSION")
  gh workflow run candidate.yml --repo "$REPO" --ref main -f "version=$VERSION" -f "commit=$COMMIT" -f "operator_record=$OPERATOR_RECORD" > /dev/null
  RUN_ID=""
fi
if [ "$TAG_ON_ORIGIN" = true ]; then
  ok "Tag $VERSION already on origin"
else
  ok "Started the release candidate for $VERSION; the tag is pushed when it is published"
fi

# ── STEP 8: Release workflow ──────────────────────────────────────────────────
# candidate.yml calls promotion and post-publish verification. Watching that
# parent run therefore covers the entire chain with one terminal session.

step "Release workflow"

if ! $USE_LOCAL_RUNNER; then
  ok "Candidate started; the Release workflow will publish $VERSION after the build gates pass"
  echo "  Follow it with: gh run watch \$(gh run list --workflow candidate.yml --limit 1 --json databaseId -q '.[0].databaseId')"
  echo ""
  echo -e "${GREEN}${BOLD}Release ${VERSION} tagged!${NC}"
  echo ""
  exit 0
fi

# State lookup also discovers prepared dispatches before a draft is assembled.
for _ in $(seq 1 60); do
  STATE_LINE=$(internal --internal-state "$REPO" "$VERSION") || die "Could not read release state from GitHub."
  IFS='|' read -r RELEASE_STATE CANDIDATE CANDIDATE_COMMIT FOUND_RUN TAG_ON_ORIGIN <<< "$STATE_LINE"
  if [ "$CANDIDATE_COMMIT" = "$COMMIT" ] && [ -n "$FOUND_RUN" ]; then
    RUN_ID=$FOUND_RUN
    break
  fi
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

STATE_LINE=$(internal --internal-state "$REPO" "$VERSION") || die "Could not read release state from GitHub."
IFS='|' read -r RELEASE_STATE CANDIDATE CANDIDATE_COMMIT FOUND_RUN TAG_ON_ORIGIN <<< "$STATE_LINE"
if [ "$RELEASE_STATE" != verified ] || [ "$CANDIDATE_COMMIT" != "$COMMIT" ] || [ "$TAG_ON_ORIGIN" != true ]; then die "The Release workflow has not verified the published release. Re-run this script to retry the failed jobs."; fi
ok "Tag $VERSION already on origin"

# ── Done ──────────────────────────────────────────────────────────────────────
# The EXIT trap now stops and deregisters the runner.

echo ""
echo -e "${GREEN}${BOLD}Release ${VERSION} published!${NC}"
echo ""
