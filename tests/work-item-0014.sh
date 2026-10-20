#!/usr/bin/env bash
# Tests for work item 0014: prepare for users
# Validates GitHub workflows, Makefile release target, docs, and README.
set -euo pipefail

PASS=0
FAIL=0
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

pass() { PASS=$((PASS + 1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL: $1"; }

check_file_exists() {
    if [ -f "$ROOT/$1" ]; then pass "$1 exists"; else fail "$1 missing"; fi
}

check_file_contains() {
    if grep -q "$2" "$ROOT/$1" 2>/dev/null; then
        pass "$1 contains '$2'"
    else
        fail "$1 does not contain '$2'"
    fi
}

echo "=== GitHub Workflows ==="

# Test workflow: test.yml
check_file_exists ".github/workflows/test.yml"
check_file_contains ".github/workflows/test.yml" "name: Tests"
check_file_contains ".github/workflows/test.yml" 'push:'
check_file_contains ".github/workflows/test.yml" 'pull_request:'
check_file_contains ".github/workflows/test.yml" 'branches:.*\*\*'
check_file_contains ".github/workflows/test.yml" 'make test'
check_file_contains ".github/workflows/test.yml" 'rust-toolchain'
check_file_contains ".github/workflows/test.yml" 'actions/checkout'
check_file_contains ".github/workflows/test.yml" 'actions/cache'

# Test workflow: release.yml
check_file_exists ".github/workflows/release.yml"
check_file_contains ".github/workflows/release.yml" "name: Release"
check_file_contains ".github/workflows/release.yml" 'v\[0-9\]'
check_file_contains ".github/workflows/release.yml" 'x86_64-unknown-linux-gnu'
check_file_contains ".github/workflows/release.yml" 'aarch64-unknown-linux-gnu'
check_file_contains ".github/workflows/release.yml" 'aarch64-apple-darwin'
check_file_contains ".github/workflows/release.yml" 'x86_64-pc-windows-msvc'
check_file_contains ".github/workflows/release.yml" 'amux-linux-amd64'
check_file_contains ".github/workflows/release.yml" 'amux-macos-arm64'
check_file_contains ".github/workflows/release.yml" 'amux-linux-arm64'
check_file_contains ".github/workflows/release.yml" 'amux-windows-amd64.exe'
check_file_contains ".github/workflows/release.yml" 'upload-artifact'
check_file_contains ".github/workflows/release.yml" 'download-artifact'
check_file_contains ".github/workflows/release.yml" 'action-gh-release'
check_file_contains ".github/workflows/release.yml" 'docs/releases/'
check_file_contains ".github/workflows/release.yml" 'contents: write'

echo ""
echo "=== Makefile Release Target ==="

check_file_contains "Makefile" 'release'
check_file_contains "Makefile" 'VERSION'
check_file_contains "Makefile" 'git checkout main'
check_file_contains "Makefile" 'git pull'
check_file_contains "Makefile" 'git status --porcelain'
check_file_contains "Makefile" 'docs/releases'
check_file_contains "Makefile" 'chat'
check_file_contains "Makefile" 'cargo test'
check_file_contains "Makefile" 'git tag'
check_file_contains "Makefile" 'git push'
check_file_contains "Makefile" 'gh release create'

# Test that make release without VERSION fails
echo ""
echo "--- Testing make release without VERSION ---"
RELEASE_OUTPUT="$(cd "$ROOT" && make release 2>&1 || true)"
if echo "$RELEASE_OUTPUT" | grep -q "Usage: make release VERSION="; then
    pass "make release without VERSION shows usage"
else
    fail "make release without VERSION should show usage"
fi

echo ""
echo "=== Documentation ==="

# Getting started guide
check_file_exists "docs/getting-started.md"
check_file_contains "docs/getting-started.md" "Getting Started"
check_file_contains "docs/getting-started.md" "amux init"
check_file_contains "docs/getting-started.md" "amux ready"
check_file_contains "docs/getting-started.md" "amux chat"
check_file_contains "docs/getting-started.md" "amux implement"
check_file_contains "docs/getting-started.md" "usage.md"
check_file_contains "docs/getting-started.md" "Installation"
check_file_contains "docs/getting-started.md" "Docker"
check_file_contains "docs/getting-started.md" "Prerequisites"

# Releases directory exists
if [ -d "$ROOT/docs/releases" ]; then
    pass "docs/releases/ directory exists"
else
    fail "docs/releases/ directory missing"
fi

echo ""
echo "=== README ==="

check_file_exists "README.md"
check_file_contains "README.md" "badge"
check_file_contains "README.md" "test.yml"
check_file_contains "README.md" "spec-driven"
check_file_contains "README.md" "Docker container"
check_file_contains "README.md" "Security"
check_file_contains "README.md" "getting-started.md"
check_file_contains "README.md" "usage.md"
check_file_contains "README.md" "architecture.md"
check_file_contains "README.md" "github.com/cohix/aspec"
check_file_contains "README.md" "GitHub Releases"
check_file_contains "README.md" "amux-linux-amd64"
check_file_contains "README.md" "amux-macos-arm64"
check_file_contains "README.md" "amux-windows-amd64.exe"

echo ""
echo "==============================="
echo "Results: $PASS passed, $FAIL failed"
echo "==============================="

if [ "$FAIL" -gt 0 ]; then exit 1; fi
