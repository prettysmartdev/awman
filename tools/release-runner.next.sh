#!/usr/bin/env bash
# P5 compatibility entry point; retain the legacy script until verified cutover.
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo xtask release runner "$@"
